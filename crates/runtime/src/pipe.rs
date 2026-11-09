//! NDJSON in-process client/host pipe with typed endpoint APIs.

use std::sync::{Arc, OnceLock};

use tcode_client::HostLink;
use tcode_client::attachment::{AttachmentMessage, AttachmentPolicy, AttachmentSender};
use tcode_core::thread_sort::ThreadSection;
use tcode_protocol::{
    ClientMessage, ClientPayload, Command, CommandResponse, HostMessage, Principal, ProtocolError,
    Query, QueryResponse, decode_client_line,
};
#[cfg(test)]
use tcode_protocol::{EventEnvelope, ServerEvent, Subscription, Topic};
use tcode_services::store::SessionStore;

use crate::app::{AppState, DomainDiff};
use crate::host::{HostCx, HostFn};

/// Optional process-local services attached before the host starts accepting
/// client traffic.
#[derive(Default)]
pub struct HostServices {
    pub user_directories: tcode_services::user_files::UserDirectories,
    /// Run provider catalog, version, and status probes, and bring the search
    /// index up to date, during host startup.
    pub background_startup_probes: bool,
    /// Generate AI-authored titles for new threads and explicit regeneration.
    pub ai_title_generation: bool,
    /// URL/tokens and the broker receiver stay host-side. Requests reach the
    /// client rendering the thread's preview on its preview topic.
    pub preview: Option<preview_mcp::PreviewMcpServer>,
    pub pull_requests: Option<pull_request_mcp::PullRequestMcpServer>,
    /// Registration-only startup data (URL and token issuer); this contains no
    /// request receiver or live backend handle.
    pub computer_use: Option<computer_use_mcp::ComputerUseMcpServer>,
    pub control: Option<control_mcp::ControlMcpServer>,
}

impl HostServices {
    /// Starts the MCP servers a host offers its agents on one loopback MCP host, computer use
    /// only where `computer_use`. A host that cannot bind or start runs without any of them.
    /// Preview requests travel to the client rendering the thread's preview, so a host without a
    /// desktop serves preview too.
    pub fn start_mcp_servers(&mut self, computer_use: bool) -> std::io::Result<()> {
        let mut host = mcp_host::Host::bind()?;
        let preview = preview_mcp::start(&mut host);
        let pull_requests = pull_request_mcp::start(&mut host);
        let control = control_mcp::start(&mut host);
        let computer_use = computer_use.then(|| computer_use_mcp::start(&mut host));
        host.start()?;
        self.preview = Some(preview);
        self.pull_requests = Some(pull_requests);
        self.control = Some(control);
        self.computer_use = computer_use;
        Ok(())
    }
}

type OriginalIngress = (
    String,
    async_channel::Sender<Result<tcode_client::original::OriginalSource, ProtocolError>>,
);

#[derive(serde::Serialize, serde::Deserialize)]
struct OriginalAdmission {
    principal: Principal,
    target: tcode_protocol::OriginalTarget,
}

#[derive(Clone)]
pub struct SpawnedHost {
    originals: async_channel::Sender<OriginalIngress>,
    pub attachments: async_channel::Sender<AttachmentMessage>,
    pub to_host: AttachmentSender,
    pub from_host: async_channel::Receiver<String>,
    pub stopped: async_channel::Receiver<()>,
    link: Arc<OnceLock<HostLink>>,
    #[cfg(any(test, feature = "test-support"))]
    test_mailbox: async_channel::Sender<HostFn>,
}

impl tcode_client::original::OriginalBackend for SpawnedHost {
    fn open(
        &self,
        principal: Principal,
        target: tcode_protocol::OriginalTarget,
    ) -> tcode_client::original::OriginalFuture<'static, tcode_client::original::OriginalSource>
    {
        let ingress = self.originals.clone();
        Box::pin(async move {
            let (answer, result) = async_channel::bounded(1);
            struct Cancel(
                async_channel::Receiver<
                    Result<tcode_client::original::OriginalSource, ProtocolError>,
                >,
            );
            impl Drop for Cancel {
                fn drop(&mut self) {
                    self.0.close();
                }
            }
            let cancel = Cancel(result);
            let request = serde_json::to_string(&OriginalAdmission { principal, target })
                .map_err(tcode_client::original::error)?;
            ingress
                .send((request, answer))
                .await
                .map_err(tcode_client::original::error)?;
            cancel
                .0
                .recv()
                .await
                .map_err(tcode_client::original::error)?
        })
    }
}

impl SpawnedHost {
    pub fn link(&self) -> HostLink {
        self.link
            .get_or_init(|| {
                let link = HostLink::new(self.to_host.clone(), self.from_host.clone());
                smol::spawn({
                    let link = link.clone();
                    async move { link.pump().await }
                })
                .detach();
                link
            })
            .clone()
    }

    /// Stop the host for an application that is quitting: its loop ends once
    /// its inbox closes, which, as the process's last host, stops every harness
    /// this process started (see [`RunningHost`]). A quit
    /// that stops waiting first drops this future — gpui gives its quit
    /// handlers a fixed budget, then exits — and the drop stops every harness
    /// still running at once, on the quitting thread, since the host may not
    /// run again before the process is gone.
    pub fn stop(&self) -> impl std::future::Future<Output = ()> + use<> {
        struct Unfinished;
        impl Drop for Unfinished {
            fn drop(&mut self) {
                agent::stop_every_tree();
            }
        }
        // Made outside the future, so a future dropped before its first
        // poll stops them too.
        let unfinished = Unfinished;
        let to_host = self.to_host.clone();
        let stopped = self.stopped.clone();
        async move {
            let unfinished = unfinished;
            to_host.close();
            let _ = stopped.recv().await;
            std::mem::forget(unfinished);
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn shutdown_blocking(&self) -> Result<(), ProtocolError> {
        self.link().shutdown_blocking()?;
        self.stopped.recv_blocking().map_err(transport_error)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn update_state_for_test<R>(
        &self,
        update: impl FnOnce(&mut AppState, &mut HostCx) -> R + Send + 'static,
    ) -> Result<R, ProtocolError>
    where
        R: Send + 'static,
    {
        let (sender, receiver) = smol::channel::bounded(1);
        self.test_mailbox
            .send(Box::new(move |state, cx| {
                let result = update(state, cx);
                let _ = sender.try_send(result);
            }))
            .await
            .map_err(transport_error)?;
        receiver.recv().await.map_err(transport_error)
    }
}

#[cfg(any(test, feature = "test-support"))]
fn transport_error(error: impl std::fmt::Display) -> ProtocolError {
    ProtocolError {
        code: "transport_closed".into(),
        message: error.to_string(),
    }
}

/// A host running in this process. When the last one goes, every harness this
/// process started is stopped, since the process may exit before their actors
/// run again. A host that goes while another runs leaves its harnesses to their
/// actors, which it asked to stop and which the running process still runs.
struct RunningHost;

static RUNNING_HOSTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl RunningHost {
    fn enter() -> Self {
        RUNNING_HOSTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self
    }
}

impl Drop for RunningHost {
    fn drop(&mut self) {
        if RUNNING_HOSTS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) == 1 {
            agent::stop_every_tree();
        }
    }
}

/// Spawn the dedicated host thread and return its serialized transport and local affordances.
pub fn spawn_host(store: SessionStore, mut services: HostServices) -> std::io::Result<SpawnedHost> {
    fn assert_send<T: Send>() {}
    assert_send::<AppState>();

    let (original_tx, original_rx) = async_channel::bounded::<OriginalIngress>(8);
    let (attachment_tx, attachment_rx) = async_channel::unbounded::<AttachmentMessage>();
    let (event_tx, event_rx) = async_channel::unbounded::<String>();
    let (stopped_tx, stopped_rx) = smol::channel::bounded(1);
    let (mailbox_tx, mailbox_rx) = smol::channel::unbounded::<HostFn>();
    // The host owns the broker, including when started without a desktop.
    // Both local and remote WebViews answer the same serialized reverse RPC.
    let (preview_registration, preview_requests) = match services.preview.take() {
        Some(preview_mcp::PreviewMcpServer {
            url,
            tokens,
            requests,
        }) => (Some((url, tokens)), Some(requests)),
        None => (None, None),
    };

    #[cfg(any(test, feature = "test-support"))]
    let test_mailbox = mailbox_tx.clone();

    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("tcode-host".into())
        .spawn(move || {
            // Entered before the host answers that it is ready.
            let running = RunningHost::enter();
            let mut state = match AppState::with_ai_titles(
                store,
                services.ai_title_generation,
                services.user_directories,
            ) {
                Ok(state) => state,
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                    return;
                }
            };
            if let Some((url, tokens)) = preview_registration {
                state.attach_preview_mcp(url, tokens);
            }
            if let Some(server) = services.computer_use.take() {
                state.attach_computer_use_mcp(server.url, server.tokens);
            }
            if let Some(server) = services.control.take() {
                state.attach_control_mcp(server);
            }
            let mut cx = HostCx::new(mailbox_tx, event_tx);
            state.pump_pull_request_requests(services.pull_requests.take(), &mut cx);
            state.start_pull_request_workers(&mut cx);
            state.pump_control_requests(&mut cx);
            state.pump_preview_requests(preview_requests, &mut cx);
            state.resume_undelivered_reports(&mut cx);
            if services.background_startup_probes {
                state.recover_orphaned_worktrees(&mut cx);
                state.refresh_model_catalogs(&mut cx);
                if state.provider_update_checks_enabled() {
                    state.check_provider_versions(&mut cx);
                }
                state.refresh_provider_usage(&mut cx);
                state.refresh_provider_status(&mut cx);
                state.index_session_search(&mut cx);
            }
            state.start_storage_pass(&mut cx);
            state.sync_terminal_handles();
            state.start_activity(&mut cx);
            let _ = ready_tx.send(Ok(()));
            let mut state =
                smol::block_on(host_loop(state, cx, attachment_rx, mailbox_rx, original_rx));
            drop(running);
            if let Err(error) = state.close_store() {
                log::error!("could not close the session store cleanly: {error}");
            }
            let _ = stopped_tx.send_blocking(());
        })?;
    ready_rx.recv().map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            format!("host failed during startup: {error}"),
        )
    })??;

    Ok(SpawnedHost {
        originals: original_tx,
        to_host: AttachmentSender::new(attachment_tx.clone(), AttachmentPolicy::LOCAL),
        attachments: attachment_tx,
        from_host: event_rx,
        stopped: stopped_rx,
        link: Arc::new(OnceLock::new()),
        #[cfg(any(test, feature = "test-support"))]
        test_mailbox,
    })
}

async fn host_loop(
    mut state: AppState,
    mut cx: HostCx,
    client: async_channel::Receiver<AttachmentMessage>,
    mailbox: smol::channel::Receiver<HostFn>,
    originals: async_channel::Receiver<OriginalIngress>,
) -> AppState {
    let mut domain_diff = DomainDiff::new(&state);
    loop {
        enum Input {
            Client(Result<AttachmentMessage, smol::channel::RecvError>),
            Internal(Result<HostFn, smol::channel::RecvError>),
            Original(OriginalIngress),
        }
        match smol::future::race(async { Input::Client(client.recv().await) }, async {
            smol::future::race(async { Input::Internal(mailbox.recv().await) }, async {
                match originals.recv().await {
                    Ok(request) => Input::Original(request),
                    Err(_) => std::future::pending().await,
                }
            })
            .await
        })
        .await
        {
            Input::Client(message) => match message {
                Ok(AttachmentMessage { line, policy }) => match decode_client_line(&line) {
                    Ok(message) => handle_client_message(&mut state, &mut cx, message, policy),
                    Err(error) => cx.send_message(HostMessage::Ack {
                        id: malformed_message_id(&line).unwrap_or(0),
                        result: Err(error),
                    }),
                },
                Err(_) => break,
            },
            Input::Original((request, answer)) => {
                match serde_json::from_str::<OriginalAdmission>(&request) {
                    Ok(request) => {
                        state.start_original(request.principal, request.target, answer, &mut cx)
                    }
                    Err(error) => {
                        let _ = answer.try_send(Err(tcode_client::original::error(error)));
                    }
                }
            }
            Input::Internal(message) => match message {
                Ok(operation) => operation(&mut state, &mut cx),
                Err(_) => break,
            },
        }
        state.sync_terminal_handles();
        state.reap_terminal_projections();
        domain_diff.emit_changes(&mut state, &mut cx);
    }
    // Reject late completions and release their captured results.
    mailbox.close();
    while let Ok(operation) = mailbox.try_recv() {
        drop(operation);
    }
    state
}

fn malformed_message_id(line: &str) -> Option<u64> {
    serde_json::from_str::<serde_json::Value>(line.trim_end())
        .ok()?
        .get("id")?
        .as_u64()
}

pub(crate) fn handle_client_message(
    state: &mut AppState,
    cx: &mut HostCx,
    message: ClientMessage,
    policy: AttachmentPolicy,
) {
    let ClientMessage {
        id,
        payload,
        key,
        principal,
    } = message;
    cx.principal = principal.unwrap_or(Principal::Full);
    cx.space = match &cx.principal {
        Principal::Full => None,
        Principal::Space { space_id, .. } => Some(space_id.clone()),
    };
    cx.origin = Some(tcode_core::session::MessageOrigin::Human);
    cx.author = match &cx.principal {
        Principal::Full => None,
        Principal::Space {
            device_id,
            device_name,
            ..
        } => Some(tcode_core::session::Author {
            device_id: device_id.clone(),
            name: device_name.clone(),
        }),
    };
    if !matches!(payload, ClientPayload::Unsubscribe(_)) && state.observe_principal(&cx.principal) {
        state.space_policy_changed(cx);
    }
    if let Err(error) = state.authorize(&cx.principal, &payload, cx) {
        let reply = if matches!(payload, ClientPayload::Query(_)) {
            HostMessage::QueryResult {
                id,
                result: Err(error),
            }
        } else {
            HostMessage::Ack {
                id,
                result: Err(error),
            }
        };
        cx.send_message(reply);
        cx.principal = Principal::Full;
        cx.author = None;
        cx.space = None;
        cx.origin = None;
        return;
    }
    match payload {
        ClientPayload::Command(command) => {
            let scoped_key = key.filter(|_| command.requires_delivery_key()).map(|key| {
                let (device, key) = key.split_once(':').unwrap_or(("local", &key));
                (device.to_owned(), key.to_owned())
            });
            if let Some((device, key)) = &scoped_key {
                let mut completed = cx.completed.lock().unwrap();
                if let Some(cache) = completed.get_mut(device)
                    && let Some(index) = cache.iter().position(|(cached, _)| cached == key)
                {
                    let cached = cache.remove(index).unwrap();
                    let result = cached.1.clone();
                    cache.push_back(cached);
                    cx.send_message(HostMessage::Ack { id, result });
                    cx.principal = Principal::Full;
                    cx.author = None;
                    cx.space = None;
                    cx.origin = None;
                    return;
                }
            }
            cx.delivery_key = scoped_key.as_ref().map(|(_, key)| key.clone());
            let outcome = dispatch_command(state, cx, command);
            cx.delivery_key = None;
            match outcome {
                CommandOutcome::Immediate(result) => {
                    complete_command(cx, id, scoped_key, result);
                }
                CommandOutcome::Pending(task) => {
                    let response_cx = cx.clone();
                    cx.spawn_detached(async move {
                        complete_command(&response_cx, id, scoped_key, task.await);
                    });
                }
                CommandOutcome::StoreBarrier(barrier) => {
                    let response_cx = cx.clone();
                    cx.spawn_detached(async move {
                        let result = match barrier.recv().await {
                            Ok(Ok(())) => Ok(CommandResponse::Unit),
                            Ok(Err(message)) => Err(ProtocolError {
                                code: "store_flush_failed".into(),
                                message,
                            }),
                            Err(error) => Err(ProtocolError {
                                code: "store_barrier_closed".into(),
                                message: error.to_string(),
                            }),
                        };
                        response_cx.send_message(HostMessage::Ack { id, result });
                    });
                }
            }
        }
        ClientPayload::Query(query) => {
            let task = dispatch_query(state, cx, query, policy);
            let response_cx = cx.clone();
            cx.spawn_detached(async move {
                response_cx.send_message(HostMessage::QueryResult {
                    id,
                    result: task.await,
                });
            });
        }
        ClientPayload::Subscribe(subscription) => {
            state.subscribe(&subscription, cx);
            state.reply_to_subscription(Some(id), subscription, cx);
        }
        ClientPayload::Unsubscribe(subscription) => {
            state.unsubscribe(&subscription, cx);
            cx.send_message(HostMessage::Ack {
                id,
                result: Ok(CommandResponse::Unit),
            });
        }
    }
    cx.principal = Principal::Full;
    cx.author = None;
    cx.space = None;
    cx.origin = None;
}

fn complete_command(
    cx: &HostCx,
    id: u64,
    scoped_key: Option<(String, String)>,
    result: Result<CommandResponse, ProtocolError>,
) {
    if let Some((device, key)) = scoped_key {
        let mut completed = cx.completed.lock().unwrap();
        let cache = completed.entry(device).or_default();
        cache.push_back((key, result.clone()));
        while cache.len() > 512 {
            cache.pop_front();
        }
    }
    cx.send_message(HostMessage::Ack { id, result });
}

enum CommandOutcome {
    Immediate(Result<CommandResponse, ProtocolError>),
    StoreBarrier(smol::channel::Receiver<Result<(), String>>),
    Pending(crate::host::HostTask<Result<CommandResponse, ProtocolError>>),
}

fn dispatch_command(app: &mut AppState, cx: &mut HostCx, command: Command) -> CommandOutcome {
    if let Err(error) = app.validate_command_target(&command) {
        return CommandOutcome::Immediate(Err(error));
    }
    if matches!(cx.principal, Principal::Space { .. })
        && matches!(
            command,
            Command::MarkSessionRead { .. } | Command::MarkSessionUnread { .. }
        )
    {
        return CommandOutcome::Immediate(Ok(CommandResponse::Unit));
    }
    let mut response = CommandResponse::Unit;
    match command {
        Command::TerminalInput { terminal_id, bytes } => {
            app.write_terminal_input(terminal_id, bytes, cx)
        }
        Command::ResizeTerminal {
            terminal_id,
            cols,
            rows,
            cell_width,
            cell_height,
        } => app.resize_terminal(terminal_id, cols, rows, cell_width, cell_height, cx),
        Command::ClearTerminal { terminal_id } => app.clear_terminal(terminal_id, cx),
        Command::PreviewReply {
            request_id,
            response,
        } => app.resolve_preview(request_id, response, cx),
        Command::ShowPreviewHere {
            session_id,
            client_id,
            take_over,
        } => {
            if let Err(error) = app.show_preview_here(&session_id, &client_id, take_over, cx) {
                return CommandOutcome::Immediate(Err(error));
            }
        }
        Command::ReportPreviewTabs {
            session_id,
            client_id,
            tabs,
        } => app.report_preview_tabs(&session_id, &client_id, tabs, cx),
        Command::ReportPreviewInput {
            session_id,
            client_id,
        } => app.report_preview_input(&session_id, &client_id, cx),
        Command::ApplyPendingRelaunch => {
            let (section, session_id) = app.apply_pending_relaunch();
            response = CommandResponse::PendingRelaunchSection {
                section,
                session_id,
            };
        }
        Command::OpenLatestSession => {
            response = CommandResponse::SessionId(app.sessions.first().map(|m| m.id.clone()));
        }
        Command::ShutdownAllAndFlush => {
            app.shutdown_all(cx);
            return CommandOutcome::StoreBarrier(app.store_write_barrier(cx));
        }
        Command::ReloadProvider => app.reload_provider(cx),
        Command::LinkPullRequest {
            session_id,
            reference,
        } => {
            return CommandOutcome::Pending(app.link_pull_request(
                &session_id,
                reference,
                tcode_core::pull_request::PullRequestSource::Manual,
                cx,
            ));
        }
        Command::RefreshPullRequest { session_id, key } => {
            return CommandOutcome::Immediate(
                app.refresh_pull_request(&session_id, key, cx)
                    .map(|()| CommandResponse::Unit),
            );
        }
        Command::SetPullRequestFilesViewed {
            session_id,
            key,
            paths,
            viewed,
        } => {
            return CommandOutcome::Pending(app.set_pull_request_files_viewed(
                &session_id,
                key,
                paths,
                viewed,
                cx,
            ));
        }
        Command::RunPullRequestAction {
            session_id,
            key,
            action,
        } => {
            return CommandOutcome::Pending(app.run_pull_request_action(
                &session_id,
                key,
                action,
                cx,
            ));
        }
        Command::EditPullRequestReviewDraft {
            session_id,
            key,
            edit,
        } => {
            return CommandOutcome::Pending(app.edit_pull_request_review_draft(
                &session_id,
                key,
                edit,
                cx,
            ));
        }
        Command::UnlinkPullRequest { session_id, key } => {
            app.unlink_pull_request(&session_id, &key, cx);
        }
        Command::WatchPullRequest {
            session_id,
            key,
            watching,
        } => {
            return CommandOutcome::Immediate(app.watch_pull_request(
                &session_id,
                &key,
                watching,
                cx,
            ));
        }
        Command::SetPullRequestWatchesPaused { session_id, paused } => {
            app.set_pull_request_watches_paused(&session_id, paused, cx);
        }
        Command::RefreshHostCredentials => app.identify_host_credentials(cx),
        Command::SetHostToken { host, token, email } => {
            return CommandOutcome::Pending(app.set_host_token(host, token, email, cx));
        }
        Command::VerifyHostToken { host, token, email } => {
            return CommandOutcome::Pending(app.verify_host_token(host, token, email, cx));
        }
        Command::SetProfileSecret {
            profile_id,
            name,
            value,
        } => app.set_profile_secret(&profile_id, &name, value.as_deref(), cx),
        Command::UpdateProfileSettings { profile_id, patch } => {
            app.update_profile_settings(&profile_id, patch, cx)
        }
        Command::CreateThirdPartyProfile {
            provider,
            name,
            base_url,
            model,
            api_key,
            inherit_config,
        } => app.create_third_party_profile(
            provider,
            crate::app::NewEndpointProfile {
                name: &name,
                base_url: &base_url,
                model: model.as_deref(),
                api_key: &api_key,
            },
            inherit_config,
            cx,
        ),
        Command::CreateAccountProfile {
            provider,
            name,
            inherit_config,
        } => {
            let task = app.create_account_profile(provider, &name, inherit_config, cx);
            return CommandOutcome::Pending(
                cx.spawn_background(async move { task.await.map(CommandResponse::ProfileId) }),
            );
        }
        Command::DeleteProfile { profile_id } => app.delete_profile(&profile_id, cx),
        Command::StartProviderLogin { profile_id } => app.start_provider_login(&profile_id, cx),
        Command::SubmitProviderLoginCode { profile_id, code } => {
            app.submit_provider_login_code(&profile_id, &code)
        }
        Command::CancelProviderLogin { profile_id } => app.cancel_provider_login(&profile_id),
        Command::LogoutProvider { profile_id } => app.logout_provider(&profile_id, cx),
        Command::RefreshProviderStatus => app.refresh_provider_status(cx),
        Command::RefreshProviderUsage => app.refresh_provider_usage(cx),
        Command::CheckProviderVersions => app.check_provider_versions(cx),
        Command::MeasureDiskUsage => app.measure_disk_usage(cx),
        Command::UpdateProviders { providers } => app.update_providers(providers, cx),
        Command::SetSidebarCollapsed { collapsed } => app.set_sidebar_collapsed(collapsed, cx),
        Command::RunGitAction {
            session_id,
            action,
            message,
            included,
            feature_branch,
        } => {
            app.run_git_action(&session_id, action, message, included, feature_branch, cx);
        }
        Command::RefreshAcpRegistry => app.refresh_acp_registry(cx),
        Command::InstallAcpAgent { id } => app.install_acp_agent(id, cx),
        Command::RemoveAcpAgent { id } => app.remove_acp_agent(&id, cx),
        Command::AddCustomAcpAgent {
            name,
            command,
            args,
        } => app.add_custom_acp_agent(name, command, args, cx),
        Command::UpdateAcpAgent { id, patch } => app.update_acp_agent(&id, patch, cx),
        Command::SetActiveAcpAgent { session_id, id } => {
            app.set_active_acp_agent(&session_id, &id, cx);
            app.release_policy_holds(&session_id, cx);
        }
        Command::RefreshProviderPlugins { profile_id, cwd } => {
            app.refresh_provider_plugins(&profile_id, cwd, cx)
        }
        Command::InstallProviderPlugin {
            profile_id,
            entry_id,
            scope,
            cwd,
        } => app.start_plugin_operation(
            &profile_id,
            cwd,
            agent::PluginOp::Install {
                id: entry_id,
                scope,
                accept_command: None,
            },
            cx,
        ),
        Command::UninstallProviderPlugin {
            profile_id,
            entry_id,
            scope,
            cwd,
        } => app.start_plugin_operation(
            &profile_id,
            cwd,
            agent::PluginOp::Uninstall {
                id: entry_id,
                scope,
            },
            cx,
        ),
        Command::SetProviderPluginEnabled {
            profile_id,
            entry_id,
            scope,
            enabled,
            cwd,
        } => app.start_plugin_operation(
            &profile_id,
            cwd,
            agent::PluginOp::SetEnabled {
                id: entry_id,
                scope,
                enabled,
            },
            cx,
        ),
        Command::UpdateProviderPlugin {
            profile_id,
            entry_id,
            scope,
            cwd,
        } => app.start_plugin_operation(
            &profile_id,
            cwd,
            agent::PluginOp::Update {
                id: entry_id,
                scope,
                accept_command: None,
            },
            cx,
        ),
        Command::AddProviderMarketplace {
            profile_id,
            source,
            cwd,
        } => app.start_plugin_operation(
            &profile_id,
            cwd,
            agent::PluginOp::AddMarketplace {
                source: source.trim().to_string(),
            },
            cx,
        ),
        Command::RemoveProviderMarketplace {
            profile_id,
            name,
            cwd,
        } => app.start_plugin_operation(
            &profile_id,
            cwd,
            agent::PluginOp::RemoveMarketplace { name },
            cx,
        ),
        Command::ResolvePluginChallenge { op_id, accept } => {
            return CommandOutcome::Immediate(
                app.resolve_plugin_challenge(op_id, accept, cx)
                    .map(|()| CommandResponse::Unit),
            );
        }
        Command::ResetSettings => app.reset_settings(cx),
        Command::WriteRelaunchMarker {
            session_id,
            reopen_settings,
        } => app.write_relaunch_marker(&session_id, &reopen_settings),
        Command::ClearRelaunchMarker => app.clear_relaunch_marker(),
        Command::SetTerminalHeight { session_id, height } => {
            app.set_terminal_height(&session_id, height, cx)
        }
        Command::ToggleTerminalPanel { session_id } => app.toggle_terminal_panel(&session_id, cx),
        Command::CloseTerminalPanel { session_id } => app.close_terminal_panel(&session_id, cx),
        Command::RestartTerminal { session_id } => app.restart_terminal(&session_id, cx),
        Command::NewTerminal { session_id } => app.new_terminal(&session_id, cx),
        Command::SplitTerminal {
            session_id,
            direction,
        } => app.split_terminal(&session_id, direction, cx),
        Command::ActivateTerminal {
            session_id,
            terminal_id,
        } => app.activate_terminal(&session_id, terminal_id, cx),
        Command::CloseTerminal {
            session_id,
            terminal_id,
        } => app.close_terminal(&session_id, terminal_id, cx),
        Command::CaptureTerminalSelection {
            session_id,
            terminal_id,
            selection,
        } => app.capture_terminal_selection(&session_id, terminal_id, selection, cx),
        Command::RemoveTerminalContext {
            session_id,
            context_id,
        } => app.remove_terminal_context(&session_id, context_id, cx),
        Command::AddReviewComment {
            session_id,
            comment,
        } => app.add_review_comment(&session_id, comment, cx),
        Command::RemoveReviewComment { session_id, index } => {
            app.remove_review_comment(&session_id, index, cx)
        }
        Command::CycleProjectSort => app.cycle_project_sort(cx),
        Command::SetProjectIcon { project_id, png } => {
            if let Err(error) = app.set_project_icon(&project_id, png, cx) {
                return CommandOutcome::Immediate(Err(io_protocol_error(error)));
            }
        }
        Command::CreateProject { root } => match app.create_project(root, cx) {
            Ok(project_id) => response = CommandResponse::ProjectId(Some(project_id)),
            Err(error) => return CommandOutcome::Immediate(Err(error)),
        },
        Command::CreateNewProject { name, parent } => {
            let task = app.create_new_project(name, parent, cx);
            return CommandOutcome::Pending(cx.spawn_background(async move {
                task.await.map(|id| CommandResponse::ProjectId(Some(id)))
            }));
        }
        Command::SetProjectRoot {
            project_id,
            root,
            move_files,
        } => {
            let task = app.set_project_root(&project_id, root, move_files, cx);
            return CommandOutcome::Pending(
                cx.spawn_background(async move { task.await.map(|()| CommandResponse::Unit) }),
            );
        }
        Command::StartScratchDraft => {
            let task = app.start_scratch_draft(cx);
            return CommandOutcome::Pending(cx.spawn_background(async move {
                task.await.map(|id| CommandResponse::SessionId(Some(id)))
            }));
        }
        Command::StartExternalImport {
            project_id,
            threads,
        } => match app.start_external_import(&project_id, threads, cx) {
            Ok(started) => response = CommandResponse::ExternalImportStarted(started),
            Err(error) => return CommandOutcome::Immediate(Err(error)),
        },
        Command::ToggleProjectCollapsed { project_id } => {
            app.toggle_project_collapsed(&project_id, cx)
        }
        Command::PatchSettings { patch } => {
            app.patch_settings(patch, cx);
            // A default model or an enabled profile is part of what a held
            // message waits on.
            app.space_policy_changed(cx);
        }
        Command::SettleChildren { session_id } => {
            response = CommandResponse::ChildrenSettled(app.settle_children(&session_id, cx));
        }
        Command::SettleSession { session_id } => {
            app.settle_session(&session_id, cx);
        }
        Command::SetControlGrant { session_id, grant } => {
            app.set_control_grant(&session_id, grant, cx)
        }
        Command::RespondControlGrant {
            session_id,
            request_id,
            approve,
        } => {
            return CommandOutcome::Immediate(
                app.respond_control_grant(&session_id, &request_id, approve, cx)
                    .map(|()| CommandResponse::Unit),
            );
        }
        Command::AddThreadReference {
            session_id,
            thread_id,
        } => {
            return CommandOutcome::Immediate(
                app.add_thread_reference(&session_id, &thread_id, cx)
                    .map(|boundary_notice| CommandResponse::ThreadReferenceAdded {
                        boundary_notice,
                    }),
            );
        }
        Command::RemoveThreadReference {
            session_id,
            thread_id,
        } => app.remove_thread_reference(&session_id, &thread_id, cx),
        Command::SetFollowUp {
            session_id,
            follow_up,
        } => app.set_follow_up(&session_id, follow_up, cx),
        Command::SetParked { session_id, parked } => app.set_parked(&session_id, parked, cx),
        Command::DismissSettleSuggestion { session_id } => {
            app.dismiss_settle_suggestion(&session_id, cx)
        }
        Command::RevokeSelfSettle { session_id } => app.revoke_self_settle(&session_id, cx),
        Command::CancelAgent { session_id } => app.cancel_child(&session_id, cx),
        Command::SetAutoSettle {
            session_id,
            enabled,
        } => app.set_auto_settle(&session_id, enabled, cx),
        Command::UnsettleSession { session_id } => app.unsettle_session(&session_id, cx),
        Command::PinSession {
            session_id,
            order_key,
        } => app.pin_session(&session_id, order_key, cx),
        Command::UnpinSession { session_id } => app.unpin_session(&session_id, cx),
        Command::ReorderPinned {
            session_id,
            order_key,
        } => app.reorder_session(&session_id, ThreadSection::Pinned, order_key, cx),
        Command::ReorderActive {
            session_id,
            order_key,
        } => app.reorder_session(&session_id, ThreadSection::Active, order_key, cx),
        Command::ArchiveSession { session_id } => app.archive_session(&session_id, cx),
        Command::UnarchiveSession { session_id } => app.unarchive_session(&session_id, cx),
        Command::RenameSession { session_id, title } => app.rename_session(&session_id, &title, cx),
        Command::RegenerateSessionTitle { session_id } => {
            app.regenerate_session_title(&session_id, cx)
        }
        Command::ForkThread { id, at: None } => {
            response = CommandResponse::SessionId(app.fork_thread(&id, cx));
        }
        Command::ForkThread { id, at: Some(at) } => {
            let Some(forked) = app.fork_thread_at(&id, &at, cx) else {
                return CommandOutcome::Immediate(Err(ProtocolError {
                    code: "fork_unavailable".into(),
                    message: "This thread cannot be forked through that turn now.".into(),
                }));
            };
            response = CommandResponse::SessionId(Some(forked));
        }
        Command::MoveSession {
            session_id,
            project_id,
            cwd,
        } => {
            let moved = app.move_session(session_id, project_id, cwd, cx);
            return CommandOutcome::Pending(
                cx.spawn_background(async move { moved.await.map(|()| CommandResponse::Unit) }),
            );
        }
        Command::MergeWorktree { session_id } => {
            app.merge_worktree(&session_id, cx);
        }
        Command::DeleteSession {
            session_id,
            remove_worktree,
        } => app.delete_session(&session_id, remove_worktree, cx),
        Command::DeleteProject { project_id } => app.delete_project(&project_id, cx),
        Command::MarkSessionUnread { session_id } => app.mark_session_unread(&session_id, cx),
        Command::MarkSessionRead {
            session_id,
            through,
        } => app.mark_session_read(&session_id, through, cx),
        Command::StartDraft { project_id, cwd } => {
            let id = app.start_draft(project_id, cwd, cx);
            app.fit_draft_to_space(&id, cx);
            response = CommandResponse::SessionId(Some(id));
        }
        Command::SetDraftWorkspace { session_id, mode } => {
            app.set_draft_workspace(&session_id, mode, cx)
        }
        Command::SendTurn {
            session_id,
            text,
            attachment_paths,
        } => app.send_turn(&session_id, text, attachment_paths, cx),
        Command::ScheduleTurn {
            session_id,
            text,
            attachment_paths,
            fire_at_unix_secs,
        } => app.schedule_turn(&session_id, text, attachment_paths, fire_at_unix_secs, cx),
        Command::ConfirmRelayAndSend {
            session_id,
            text,
            attachment_paths,
        } => app.confirm_relay_and_send(&session_id, text, attachment_paths, cx),
        Command::Steer {
            session_id,
            text,
            attachment_paths,
        } => app.steer(&session_id, text, attachment_paths, cx),
        Command::SteerQueued { session_id, id } => app.steer_queued(&session_id, id, cx),
        Command::DropQueued { session_id, id } => app.drop_queued(&session_id, id, cx),
        Command::MoveQueued {
            session_id,
            id,
            before,
        } => app.move_queued(&session_id, id, before),
        Command::RetryTurn {
            session_id,
            turn_id,
        } => {
            return CommandOutcome::Immediate(
                app.retry_turn(&session_id, turn_id, cx)
                    .map(|()| CommandResponse::Unit),
            );
        }
        Command::Interrupt { session_id } => {
            return CommandOutcome::Immediate(
                app.interrupt(&session_id, cx)
                    .map(|()| CommandResponse::Unit),
            );
        }
        Command::ForceTerminate { session_id } => {
            return CommandOutcome::Immediate(
                app.force_terminate(&session_id, cx)
                    .map(|()| CommandResponse::Unit),
            );
        }
        Command::MoveToBackground {
            session_id,
            item_id,
        } => {
            return CommandOutcome::Immediate(
                app.move_to_background(&session_id, item_id)
                    .map(|()| CommandResponse::Unit),
            );
        }
        Command::StopBackgroundTask {
            session_id,
            task_id,
        } => {
            return CommandOutcome::Immediate(
                app.stop_background_task(&session_id, task_id)
                    .map(|()| CommandResponse::Unit),
            );
        }
        Command::CompactContext { session_id } => {
            return CommandOutcome::Immediate(
                app.compact_context(&session_id, cx)
                    .map(|()| CommandResponse::Unit),
            );
        }
        Command::SetGoal {
            session_id,
            objective,
            status,
        } => {
            return CommandOutcome::Immediate(
                app.change_goal(
                    &session_id,
                    agent::SessionCommand::SetGoal { objective, status },
                    cx,
                )
                .map(|()| CommandResponse::Unit),
            );
        }
        Command::ClearGoal { session_id } => {
            return CommandOutcome::Immediate(
                app.change_goal(&session_id, agent::SessionCommand::ClearGoal, cx)
                    .map(|()| CommandResponse::Unit),
            );
        }
        Command::RespondApproval {
            session_id,
            request_id,
            decision,
        } => {
            return CommandOutcome::Immediate(
                app.respond_approval(&session_id, request_id, decision, cx)
                    .map(|()| CommandResponse::Unit),
            );
        }
        Command::RespondUserInput {
            session_id,
            request_id,
            answers,
        } => {
            return CommandOutcome::Immediate(
                app.respond_user_input(&session_id, request_id, answers, cx)
                    .map(|()| CommandResponse::Unit),
            );
        }
        Command::SetActiveModel {
            session_id,
            provider,
            model,
            profile_id,
        } => {
            app.set_active_model(&session_id, provider, model, profile_id, cx);
            app.release_policy_holds(&session_id, cx);
        }
        Command::SetActiveOption {
            session_id,
            id,
            value,
        } => {
            app.set_active_option(&session_id, &id, value, cx);
            app.release_policy_holds(&session_id, cx);
        }
        Command::LoadBranches { session_id } => app.load_branches(&session_id, cx),
        Command::CheckoutBranch { session_id, branch } => {
            app.checkout_branch(&session_id, branch, cx);
        }
        Command::ToggleFavoriteModel {
            provider,
            profile_id,
            model,
        } => app.toggle_favorite_model(provider, profile_id, model, cx),
        Command::RewindTurn {
            session_id,
            turn,
            steer,
            checkpoint_id,
            mode,
            client_id,
        } => {
            let at = crate::app::RewindAt {
                turn,
                steer: steer.as_deref(),
                checkpoint: checkpoint_id.as_deref(),
            };
            app.rewind_turn(&session_id, at, mode, client_id, cx);
        }
        Command::PreviewRewind {
            session_id,
            turn,
            steer,
            checkpoint_id,
            mode,
        } => {
            let at = crate::app::RewindAt {
                turn,
                steer: steer.as_deref(),
                checkpoint: checkpoint_id.as_deref(),
            };
            let answer = app.preview_rewind(&session_id, at, mode, cx);
            return CommandOutcome::Pending(cx.spawn_background(async move {
                answer
                    .recv()
                    .await
                    .map(CommandResponse::RewindPreview)
                    .map_err(|error| ProtocolError {
                        code: "rewind_preview_unanswered".into(),
                        message: error.to_string(),
                    })
            }));
        }
        Command::UndoRewind {
            session_id,
            checkpoint_id,
        } => {
            app.undo_rewind(&session_id, &checkpoint_id, cx);
        }
        Command::OpenRewoundBranch {
            session_id,
            turn,
            checkpoint_id,
        } => {
            response = CommandResponse::SessionId(app.open_rewound_branch(
                &session_id,
                turn,
                &checkpoint_id,
                cx,
            ));
        }
    }
    CommandOutcome::Immediate(Ok(response))
}

fn dispatch_query(
    app: &mut AppState,
    cx: &mut HostCx,
    query: Query,
    policy: AttachmentPolicy,
) -> crate::host::HostTask<Result<QueryResponse, ProtocolError>> {
    match query {
        Query::SessionHistoryPage {
            session_id,
            before,
            limit,
        } => app.session_history_page(&session_id, before, limit, cx),
        Query::Hosting { .. } => cx.spawn_background(async {
            Err(ProtocolError {
                code: "unsupported".into(),
                message: "this host has no remote hosting controls".into(),
            })
        }),
        Query::ComputerUsePermissions => cx.spawn_background(async {
            Ok(QueryResponse::ComputerUsePermissions(
                computer_use_mcp::permissions::host_status(),
            ))
        }),
        Query::Ping => cx.spawn_background(async { Ok(QueryResponse::Pong) }),
        Query::ListActiveWorkspace { session_id } => {
            let cwd = app
                .resident(&session_id)
                .map(|active| active.meta.cwd.clone());
            let task = app.list_workspace_at(cwd, cx);
            cx.spawn_background(async move { Ok(QueryResponse::ActiveWorkspace(task.await)) })
        }
        Query::ScanExternalHistory => {
            let task = app.scan_external_history(cx);
            cx.spawn_background(async move { Ok(QueryResponse::ExternalHistory(task.await)) })
        }
        Query::GenerateCommitMessage {
            session_id,
            included,
        } => {
            let task = app.generate_commit_message(&session_id, included, cx);
            cx.spawn_background(async move {
                task.await
                    .map(QueryResponse::CommitMessage)
                    .map_err(|message| ProtocolError {
                        code: "commit_message_failed".into(),
                        message,
                    })
            })
        }
        Query::LoadGitDiff {
            cwd,
            scope,
            base,
            ignore_whitespace,
        } => {
            if !matches!(
                scope,
                tcode_protocol::GitDiffScope::WorkingTree | tcode_protocol::GitDiffScope::Branch
            ) {
                return cx.spawn_background(async {
                    Err(ProtocolError {
                        code: "unsupported_scope".into(),
                        message: "unknown git diff scope".into(),
                    })
                });
            }
            let task = cx.unblock(move || {
                tcode_services::git::load_git_diff(&cwd, scope, base.as_deref(), ignore_whitespace)
            });
            cx.spawn_background(async move { Ok(QueryResponse::GitDiff(task.await)) })
        }
        Query::BrowseIconImages { directory } => {
            let task = cx.unblock(move || tcode_services::project_icons::browse(&directory));
            cx.spawn_background(async move { task.await.map_err(io_protocol_error) })
        }
        Query::BrowseDirectories { path } => {
            let directories = app.user_directories().clone();
            let task = cx.unblock(move || directories.browse_directories(&path));
            cx.spawn_background(async move {
                task.await
                    .map(QueryResponse::Directories)
                    .map_err(io_protocol_error)
            })
        }
        Query::ReadIconImage { path } => {
            let task = cx.unblock(move || tcode_services::project_icons::thumbnail(&path));
            cx.spawn_background(async move {
                task.await
                    .map(QueryResponse::FileBytes)
                    .map_err(io_protocol_error)
            })
        }
        Query::ReadImagePreview {
            path,
            pixels,
            cover,
        } => app.image_preview(path, pixels, cover, cx),
        Query::ReadProjectIcon { project_id, pixels } => {
            let project = app.projects.iter().find(|p| p.id == project_id).cloned();
            let task = cx.unblock(move || {
                let project = project.ok_or_else(|| std::io::Error::other("unknown project"))?;
                tcode_services::project_icons::read_project_icon(&project, pixels)
            });
            cx.spawn_background(async move {
                task.await
                    .map(QueryResponse::FileBytes)
                    .map_err(io_protocol_error)
            })
        }
        Query::ReadFilePreview { path } => {
            let task = cx.unblock(move || {
                tcode_services::user_files::read_bounded(
                    &path,
                    policy
                        .read_limit(Some(tcode_services::git::MAX_FILE_TEXT_BYTES))
                        .unwrap(),
                )
            });
            cx.spawn_background(async move {
                task.await
                    .map(QueryResponse::FileBytes)
                    .map_err(io_protocol_error)
            })
        }
        Query::ReadFileBytes { path, max_bytes } => {
            let max_bytes = policy.read_limit(max_bytes);
            let task = cx.unblock(move || match max_bytes {
                Some(limit) => tcode_services::user_files::read_bounded(&path, limit),
                None => std::fs::read(path),
            });
            cx.spawn_background(async move {
                task.await.map(QueryResponse::FileBytes).map_err(|error| {
                    if error.kind() == std::io::ErrorKind::FileTooLarge {
                        ProtocolError {
                            code: "file_too_large".into(),
                            message: error.to_string(),
                        }
                    } else {
                        io_protocol_error(error)
                    }
                })
            })
        }
        Query::SaveAttachment { dir, bytes, ext } => {
            if bytes.len() as u64 > tcode_core::attachments::MAX_BYTES
                || !app.accepts_attachment(&dir, &ext)
            {
                return cx.spawn_background(async {
                    Err(ProtocolError {
                        code: "invalid_attachment".into(),
                        message: "attachment too large or outside the host's attachments directory"
                            .into(),
                    })
                });
            }
            let task = cx.unblock(move || AppState::save_attachment_to_dir(&dir, &bytes, &ext));
            cx.spawn_background(async move {
                task.await
                    .map(QueryResponse::SavedAttachment)
                    .map_err(io_protocol_error)
            })
        }
        Query::RemoveUserFile { path } => {
            let task = cx.unblock(move || std::fs::remove_file(&path));
            cx.spawn_background(async move {
                task.await
                    .map(|()| QueryResponse::UserFileRemoved)
                    .map_err(io_protocol_error)
            })
        }
        Query::BeginUpload {
            session_id,
            name,
            size,
        } => {
            if size > tcode_core::attachments::MAX_UPLOAD_BYTES {
                return cx.spawn_background(async { Err(upload_refused("file too large")) });
            }
            if app.find_meta(&session_id).is_none() {
                return cx.spawn_background(async { Err(upload_refused("unknown thread")) });
            }
            let directories = app.user_directories().clone();
            let task = cx.unblock(move || directories.reserve_upload(&session_id, &name));
            cx.spawn_background(async move {
                task.await
                    .map(QueryResponse::UploadBegun)
                    .map_err(io_protocol_error)
            })
        }
        Query::WriteUpload {
            session_id,
            path,
            offset,
            bytes,
        } => {
            let end = offset.saturating_add(bytes.len() as u64);
            if end > tcode_core::attachments::MAX_UPLOAD_BYTES
                || !app.user_directories().is_upload_of(&session_id, &path)
            {
                return cx.spawn_background(async { Err(upload_refused("not an open upload")) });
            }
            let task = cx
                .unblock(move || tcode_services::user_files::append_upload(&path, offset, &bytes));
            cx.spawn_background(async move {
                task.await
                    .map(|_| QueryResponse::UploadWritten)
                    .map_err(io_protocol_error)
            })
        }
        Query::DiscardUpload { session_id, path } => {
            if !app.user_directories().is_upload_of(&session_id, &path) {
                return cx.spawn_background(async { Err(upload_refused("not an upload")) });
            }
            let task = cx.unblock(move || tcode_services::user_files::discard_upload(&path));
            cx.spawn_background(async move {
                task.await
                    .map(|()| QueryResponse::UploadDiscarded)
                    .map_err(io_protocol_error)
            })
        }
        Query::RenderThreadExport { session_id, format } => {
            app.render_thread_export(&session_id, format, cx)
        }
        Query::SearchSessionContent { query, limit } => {
            let task = app.search_session_content(query, limit, cx);
            cx.spawn_background(async move { Ok(QueryResponse::SessionContentHits(task.await)) })
        }
        Query::ReadItemOutput {
            session_id,
            item_id,
        } => app.item_output(&session_id, item_id, cx),
        Query::ThreadAnchorTurn {
            session_id,
            position,
        } => app.thread_anchor_turn(&session_id, position, cx),
        Query::ReadItemImage {
            session_id,
            item_id,
            image_index,
            max_bytes,
        } => app.item_image(
            &session_id,
            item_id,
            image_index,
            policy.read_limit(max_bytes),
            cx,
        ),
        Query::ReadItemImagePreview {
            session_id,
            item_id,
            image_index,
            width,
            height,
        } => app.item_image_preview(&session_id, item_id, image_index, width, height, cx),
        Query::PullRequest {
            session_id,
            key,
            read,
        } => app.read_pull_request(&session_id, key, read, cx),
        Query::ArchivedSessions => {
            let archived = app.scoped_archived_sessions(&cx.principal);
            cx.spawn_background(async move { Ok(QueryResponse::ArchivedSessions(archived)) })
        }
        Query::ControlActivity { limit, filter } => {
            let activity = app.control_activity(&filter, limit as usize);
            cx.spawn_background(async move { Ok(QueryResponse::ControlActivity(activity)) })
        }
        Query::NativeMcpServers { profile_id } => {
            let servers = app.native_mcp_servers(profile_id, cx);
            cx.spawn_background(async move {
                servers
                    .await
                    .map(QueryResponse::NativeMcpServers)
                    .map_err(|message| ProtocolError {
                        code: "native_mcp_unreadable".into(),
                        message,
                    })
            })
        }
        Query::ReviewRecords { limit } => {
            let records = app.review_records(limit as usize);
            cx.spawn_background(async move { Ok(QueryResponse::ReviewRecords(records)) })
        }
    }
}

fn upload_refused(reason: &str) -> ProtocolError {
    ProtocolError {
        code: "invalid_upload".into(),
        message: reason.into(),
    }
}

fn io_protocol_error(error: std::io::Error) -> ProtocolError {
    ProtocolError {
        code: "io_error".into(),
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A quit stops waiting for the host after a fixed budget (gpui gives
    /// its quit handlers 200 ms, then the process exits). With the host busy
    /// past it and a harness that ignores everything but SIGKILL, the harness
    /// is still gone by the time the quit gives up.
    #[cfg(unix)]
    #[test]
    fn a_quit_that_stops_waiting_for_the_host_still_ends_every_harness() {
        use std::os::unix::fs::PermissionsExt as _;
        let root =
            std::env::temp_dir().join(format!("tcode-abandoned-stop-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pi = root.join("pi");
        let pid_file = root.join("pi.pid");
        std::fs::write(
            &pi,
            format!(
                "#!/bin/sh\ntrap '' HUP INT TERM\necho $$ > '{pid}.tmp'\nmv '{pid}.tmp' '{pid}'\nexec sleep 120\n",
                pid = pid_file.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&pi, std::fs::Permissions::from_mode(0o755)).unwrap();
        let host = spawn_host(
            SessionStore::open_at(root.join("store")).unwrap(),
            HostServices::default(),
        )
        .unwrap();

        // A model catalog query the harness never answers.
        let catalog = std::thread::spawn(move || {
            smol::block_on(agent::pi::list_models(Some(pi), Default::default()))
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let harness: libc::pid_t = loop {
            if let Some(pid) = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|pid| pid.trim().parse().ok())
            {
                break pid;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the harness never ran"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };

        let (entered, busy) = async_channel::bounded(1);
        let (release, released) = async_channel::bounded::<()>(1);
        let working = std::thread::spawn({
            let host = host.clone();
            move || {
                smol::block_on(host.update_state_for_test(move |_, _| {
                    entered.send_blocking(()).unwrap();
                    let _ = released.recv_blocking();
                }))
            }
        });
        busy.recv_blocking().unwrap();

        let stopped = smol::block_on(smol::future::or(
            async {
                host.stop().await;
                true
            },
            async {
                smol::Timer::after(std::time::Duration::from_millis(200)).await;
                false
            },
        ));
        assert!(!stopped, "the host was still busy");
        let exited = |pid: libc::pid_t| {
            // SAFETY: `info` is a valid, writable siginfo; WNOWAIT leaves the
            // exit status for the thread that owns the child.
            unsafe {
                let mut info: libc::siginfo_t = std::mem::zeroed();
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                ) != 0
                    || info.si_pid() == pid
            }
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !exited(harness) {
            assert!(
                std::time::Instant::now() < deadline,
                "the harness {harness} outlived the quit"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        release.send_blocking(()).unwrap();
        working.join().unwrap().unwrap();
        assert!(catalog.join().unwrap().is_err());
        host.stopped.recv_blocking().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    pub(super) fn next_event(
        events: &async_channel::Receiver<EventEnvelope>,
        ready: impl Fn(&EventEnvelope) -> bool,
    ) -> EventEnvelope {
        smol::block_on(smol::future::race(
            async {
                loop {
                    let envelope = events
                        .recv()
                        .await
                        .expect("host event stream closed before the expected event");
                    if ready(&envelope) {
                        return envelope;
                    }
                }
            },
            async {
                smol::Timer::after(std::time::Duration::from_secs(60)).await;
                panic!("timed out waiting for host event");
            },
        ))
    }

    #[test]
    fn stale_conversation_actions_are_rejected_over_the_pipe() {
        let root =
            std::env::temp_dir().join(format!("tcode-stale-actions-{}", uuid::Uuid::new_v4()));
        let host = spawn_host(
            SessionStore::open_at(root.clone()).unwrap(),
            HostServices::default(),
        )
        .unwrap();
        let link = host.link();
        let CommandResponse::SessionId(Some(id)) = link
            .command_blocking(Command::StartDraft {
                project_id: "fixture".into(),
                cwd: root.clone(),
            })
            .unwrap()
        else {
            panic!("draft id")
        };
        for (command, code) in [
            (
                Command::DeleteProfile {
                    profile_id: "codex".into(),
                },
                "builtin_profile",
            ),
            (
                Command::InstallAcpAgent {
                    id: "nonexistent-registry-agent".into(),
                },
                "unknown_acp_agent",
            ),
            (
                Command::MergeWorktree {
                    session_id: id.clone(),
                },
                "no_worktree",
            ),
            (
                Command::SplitTerminal {
                    session_id: id.clone(),
                    direction: tcode_core::ui::TerminalSplitDirection::Horizontal,
                },
                "terminal_split_unavailable",
            ),
            (
                Command::CaptureTerminalSelection {
                    session_id: id.clone(),
                    terminal_id: 999,
                    selection: None,
                },
                "no_selection",
            ),
            (
                Command::RewindTurn {
                    session_id: id.clone(),
                    turn: 999,
                    steer: None,
                    checkpoint_id: None,
                    mode: agent::RewindMode::Files,
                    client_id: None,
                },
                "rewind_unavailable",
            ),
            (
                Command::RespondApproval {
                    session_id: id.clone(),
                    request_id: "gone".into(),
                    decision: agent::ApprovalDecision::Option("accept".into()),
                },
                "unknown_approval",
            ),
            (
                Command::RespondApproval {
                    session_id: id.clone(),
                    request_id: "gone".into(),
                    decision: agent::ApprovalDecision::Cancel,
                },
                "unknown_approval",
            ),
            (
                Command::RespondUserInput {
                    session_id: id.clone(),
                    request_id: "gone".into(),
                    answers: Default::default(),
                },
                "unknown_user_input",
            ),
            (
                Command::SteerQueued {
                    session_id: id.clone(),
                    id: 42,
                },
                "unknown_queued_message",
            ),
            (
                Command::DropQueued {
                    session_id: id.clone(),
                    id: 42,
                },
                "unknown_queued_message",
            ),
            (
                Command::MoveQueued {
                    session_id: id.clone(),
                    id: 42,
                    before: None,
                },
                "unknown_queued_message",
            ),
            (
                Command::Interrupt {
                    session_id: id.clone(),
                },
                "no_running_turn",
            ),
            (
                Command::ClearTerminal { terminal_id: 999 },
                "unknown_terminal",
            ),
            (
                Command::ActivateTerminal {
                    session_id: id.clone(),
                    terminal_id: 999,
                },
                "unknown_terminal",
            ),
            (
                Command::RemoveTerminalContext {
                    session_id: id.clone(),
                    context_id: 999,
                },
                "unknown_terminal_context",
            ),
            (
                Command::RemoveReviewComment {
                    session_id: id.clone(),
                    index: 0,
                },
                "unknown_review_comment",
            ),
            (
                Command::DeleteProject {
                    project_id: "gone".into(),
                },
                "unknown_project",
            ),
            (
                Command::DeleteProfile {
                    profile_id: "gone".into(),
                },
                "unknown_profile",
            ),
            (
                Command::RenameSession {
                    session_id: "gone".into(),
                    title: "lost".into(),
                },
                "unknown_session",
            ),
        ] {
            let error = link
                .command_blocking(command.clone())
                .expect_err("stale action must reject");
            assert_eq!(error.code, code, "{command:?}");
        }
        for command in [
            Command::SendTurn {
                session_id: "gone".into(),
                text: "must not disappear".into(),
                attachment_paths: Vec::new(),
            },
            Command::ScheduleTurn {
                session_id: "gone".into(),
                text: "hi".into(),
                attachment_paths: Vec::new(),
                fire_at_unix_secs: 1,
            },
            Command::Steer {
                session_id: "gone".into(),
                text: "hi".into(),
                attachment_paths: Vec::new(),
            },
            Command::ConfirmRelayAndSend {
                session_id: "gone".into(),
                text: "hi".into(),
                attachment_paths: Vec::new(),
            },
        ] {
            assert_eq!(
                link.command_blocking(command).unwrap_err().code,
                "unknown_session"
            );
        }
        drop(link);
        host.shutdown_blocking().unwrap();
        // Windows may still hold the draft's files for a moment after shutdown;
        // a leftover temp dir is not a test failure.
        let _ = std::fs::remove_dir_all(root);
    }

    /// A recorded image opened from a thread a client is reading streams from
    /// the thread's history already in memory: neither its bytes nor the check
    /// that they are still current read the stored log again. Its size is
    /// known before its bytes, for the viewer to lay out its preview, and its
    /// bytes arrive in full 64 KiB blocks: each block is a hand-over to the
    /// viewer, and the 768-byte reads of the history's base64 once made a
    /// screenshot hundreds of them.
    #[test]
    fn originals_of_a_subscribed_session_reuse_its_history() {
        use base64::Engine as _;
        use tcode_client::original::OriginalBackend as _;
        let data_root =
            std::env::temp_dir().join(format!("tcode-original-pipe-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::open_at(data_root.clone()).expect("open session store");
        let mut meta = tcode_core::project::SessionMeta::new(
            agent::ProviderKind::Codex,
            data_root.join("workspace"),
            None,
        );
        meta.id = "pictured".into();
        store.upsert_meta(&meta).expect("write meta");
        let bytes = include_bytes!("../../../assets/icons/app/tcode.png").to_vec();
        store
            .append_event(
                &meta.id,
                1,
                &agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                    id: "recorded-image".into(),
                    parent_item_id: None,
                    content: agent::ItemContent::ImageRead {
                        path: "screen.png".into(),
                        image: Some(agent::Attachment {
                            media_type: "image/png".into(),
                            data_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
                            source_path: None,
                        }),
                    },
                }),
            )
            .expect("append image");
        let reader = store.clone();
        let host = spawn_host(store, HostServices::default()).expect("spawn host");
        let link = host.link();
        link.subscribe(Subscription {
            known_index: None,
            after: None,
            topic: Topic::SessionEvents {
                session_id: meta.id.clone(),
            },
        })
        .expect("subscribe to the thread");
        // The subscription is answered from the log's tail while its one read
        // of the whole log runs on; a user opens the image after that read.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while reader.event_reads() == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the subscription never read the thread's log"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let source = smol::block_on(host.open(
            tcode_protocol::Principal::Full,
            tcode_protocol::OriginalTarget::Item {
                session_id: meta.id.clone(),
                item_id: "recorded-image".into(),
                image_index: 0,
            },
        ))
        .expect("open the original");
        assert_eq!(source.metadata.len, bytes.len() as u64);
        assert_eq!(source.metadata.dimensions, Some((1024, 1024)));
        let mut received = Vec::new();
        let mut block_lens = Vec::new();
        loop {
            let block = source
                .blocks
                .recv_blocking()
                .expect("original stream")
                .expect("original block");
            if block.is_empty() {
                break;
            }
            block_lens.push(block.len());
            received.extend(block);
        }
        assert_eq!(received, bytes);
        assert!(
            block_lens[..block_lens.len() - 1]
                .iter()
                .all(|&len| len == 64 * 1024),
            "every block but the last is full: {block_lens:?}"
        );
        assert_eq!(
            reader.event_reads(),
            1,
            "the original was served from the thread's history"
        );

        drop(source);
        link.shutdown_blocking().expect("stop host");
        host.stopped.recv_blocking().expect("host thread stopped");
        drop(host);
        std::fs::remove_dir_all(data_root).expect("remove test data");
    }

    /// Command output is rendered by the host at the width a subscriber asked
    /// for: a finished command is answered once per width, and a running one
    /// is pushed again, coalesced, while its output grows, until the
    /// subscriber leaves.
    #[test]
    fn command_output_is_rendered_per_width_and_pushed_while_it_grows() {
        use tcode_protocol::terminal::{CellFlags, TerminalColor, TerminalFrame};

        let data_root =
            std::env::temp_dir().join(format!("tcode-command-output-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::open_at(data_root.clone()).expect("open session store");
        let mut meta = tcode_core::project::SessionMeta::new(
            agent::ProviderKind::ClaudeCode,
            data_root.join("workspace"),
            Some("opus".into()),
        );
        meta.id = "command-output-session".into();
        store
            .append_event(
                &meta.id,
                1,
                &agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                    id: "cmd-1".into(),
                    parent_item_id: None,
                    content: agent::ItemContent::CommandExecution {
                        command: "echo red".into(),
                        // Bold red, 25 characters: it fits one 40-column row and
                        // wraps onto a second at 20.
                        output: "\u{1b}[1;31mabcdefghijklmnopqrstuvwxy\u{1b}[0m\nplain\r\nend\n"
                            .into(),
                        exit_code: Some(0),
                        status: agent::ItemStatus::Completed,
                    },
                }),
            )
            .expect("append event");
        store
            .append_event(
                &meta.id,
                2,
                &agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                    id: "empty-output".into(),
                    parent_item_id: None,
                    content: agent::ItemContent::CommandExecution {
                        command: "true".into(),
                        output: String::new(),
                        exit_code: Some(0),
                        status: agent::ItemStatus::Completed,
                    },
                }),
            )
            .expect("append empty command output");
        store.upsert_meta(&meta).expect("write meta");

        let host = spawn_host(store, HostServices::default()).expect("spawn host");
        let link = host.link();
        let events = super::p4b_tests::observed_events(&link);
        let session_topic = Topic::SessionEvents {
            session_id: meta.id.clone(),
        };
        link.subscribe(tcode_protocol::Subscription {
            known_index: None,
            topic: session_topic.clone(),
            after: None,
        })
        .expect("subscribe to the session");
        next_event(&events, |event| {
            event.topic == session_topic
                && matches!(event.event, ServerEvent::SessionSnapshot { .. })
        });

        let topic = |item_id: &str, cols: u16| Topic::CommandOutput {
            session_id: meta.id.clone(),
            item_id: item_id.into(),
            cols,
        };
        let subscribe = |topic: &Topic| {
            link.subscribe(tcode_protocol::Subscription {
                known_index: None,
                topic: topic.clone(),
                after: None,
            })
            .expect("subscribe to command output");
        };
        let frame_of = |event: EventEnvelope| match event.event {
            ServerEvent::CommandOutputReplaced(frame) => *frame,
            other => panic!("unexpected command output event: {other:?}"),
        };
        let reply = |topic: &Topic| {
            subscribe(topic);
            frame_of(next_event(&events, |event| {
                event.topic == *topic && event.request_id.is_some()
            }))
        };
        let text = |frame: &TerminalFrame, row: usize| {
            frame.visible[row]
                .cells
                .iter()
                .map(|cell| cell.text.as_str())
                .collect::<String>()
        };

        for (requested, cols, first_line_rows) in [
            (0, 20, 2),
            (4, 20, 2),
            (20, 20, 2),
            (40, 40, 1),
            (4000, 400, 1),
        ] {
            let rendered = reply(&topic("cmd-1", requested));
            assert_eq!(rendered.cols, cols);
            assert_eq!(usize::from(rendered.rows), first_line_rows + 2);
            if cols == 20 {
                assert_eq!(text(&rendered, 0), "abcdefghijklmnopqrst");
                assert_eq!(text(&rendered, 1), "uvwxy");
            } else {
                assert_eq!(text(&rendered, 0), "abcdefghijklmnopqrstuvwxy");
            }
            assert_eq!(text(&rendered, first_line_rows), "plain");
            assert_eq!(text(&rendered, first_line_rows + 1), "end");
            for row in &rendered.visible[..first_line_rows] {
                for cell in &row.cells {
                    let style = rendered.style(cell);
                    assert_eq!(style.fg, TerminalColor::Indexed(1));
                    assert!(style.flags().contains(CellFlags::BOLD));
                }
            }
            assert!(
                !rendered
                    .style(&rendered.visible[first_line_rows].cells[0])
                    .flags()
                    .contains(CellFlags::BOLD)
            );
            assert!(rendered.history.is_empty() && rendered.cursor.is_none());
        }
        assert_eq!(reply(&topic("empty-output", 40)).rows, 0);

        let session_id = meta.id.clone();
        let record = |events: Vec<agent::AgentEvent>| {
            let session_id = session_id.clone();
            smol::block_on(host.update_state_for_test(move |state, cx| {
                for (ts, event) in events.iter().enumerate() {
                    state.record_event_for_replica_test(&session_id, 10 + ts as u64, event, cx);
                }
            }))
            .expect("record provider events");
        };
        let delta = |text: &str| agent::AgentEvent::Delta {
            item_id: "run-1".into(),
            kind: agent::DeltaKind::CommandOutput,
            text: text.into(),
        };
        record(vec![agent::AgentEvent::ItemStarted(agent::ThreadItem {
            id: "run-1".into(),
            parent_item_id: None,
            content: agent::ItemContent::CommandExecution {
                command: "build".into(),
                output: String::new(),
                exit_code: None,
                status: agent::ItemStatus::InProgress,
            },
        })]);
        let running = topic("run-1", 40);
        let mut held = reply(&running);
        assert_eq!(held.rows, 0);

        // A progress bar redrawn in place, then a green verdict.
        let mut growth = (1..=9)
            .map(|step| delta(&format!("{}%\r", step * 10)))
            .collect::<Vec<_>>();
        growth.push(delta("100%\n\u{1b}[32mok\u{1b}[0m\n"));
        let deltas = growth.len();
        record(growth);
        // The subscriber holds the answer, so each push is what changed.
        let mut pushes = 0;
        let latest = loop {
            match next_event(&events, |event| event.topic == running).event {
                ServerEvent::CommandOutputDelta(delta) => held.apply(&delta),
                other => panic!("a push after the answer sent {other:?}"),
            }
            pushes += 1;
            if held.rows == 2 {
                break held;
            }
        };
        assert!(pushes < deltas, "{pushes} pushes for {deltas} deltas");
        assert_eq!(text(&latest, 0), "100%");
        assert_eq!(text(&latest, 1), "ok");
        assert_eq!(
            latest.style(&latest.visible[1].cells[0]).fg,
            TerminalColor::Indexed(2)
        );

        link.unsubscribe(tcode_protocol::Subscription {
            known_index: None,
            topic: running.clone(),
            after: None,
        })
        .expect("unsubscribe");
        record(vec![delta("unseen\n")]);
        // Nothing reaches a topic nobody holds: the next event on it is the
        // answer to subscribing again, already holding the line it missed.
        subscribe(&running);
        let resubscribed = next_event(&events, |event| event.topic == running);
        assert!(
            resubscribed.request_id.is_some(),
            "pushed after unsubscribe"
        );
        assert_eq!(text(&frame_of(resubscribed), 2), "unseen");

        link.shutdown_blocking().expect("stop host");
        host.stopped.recv_blocking().expect("host thread stopped");
        drop(host);
        std::fs::remove_dir_all(data_root).expect("remove test data");
    }

    #[test]
    fn scope_and_space_index_subscriptions_reply_over_the_host_pipe() {
        let root = std::env::temp_dir().join(format!("tcode-scope-{}", uuid::Uuid::new_v4()));
        let host = spawn_host(
            SessionStore::open_at(root.clone()).unwrap(),
            HostServices::default(),
        )
        .unwrap();
        let link = host.link();
        let events = super::p4b_tests::observed_events(&link);
        for (topic, expected) in [
            (
                Topic::Scope,
                ServerEvent::ScopeSnapshot(tcode_protocol::Scope::Full),
            ),
            (
                Topic::SpaceIndex {
                    space_id: "shared".into(),
                },
                ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                    summary: tcode_protocol::IndexSummary::default(),
                    projects: vec![],
                    sessions: vec![],
                }),
            ),
        ] {
            link.subscribe(Subscription {
                known_index: None,
                topic: topic.clone(),
                after: None,
            })
            .unwrap();
            let snapshot = next_event(&events, |event| event.topic == topic);
            assert_eq!(snapshot.event, expected);
            assert!(snapshot.request_id.is_some());
        }
        link.shutdown_blocking().unwrap();
        host.stopped.recv_blocking().unwrap();
        drop(host);
        std::fs::remove_dir_all(root).unwrap();
    }

    const LEGACY_START_DIR: &str = "TCODE_LEGACY_START_DIR";
    const LEGACY_START_REPORT: &str = "legacy start: ";

    fn legacy_events() -> [(Option<u64>, agent::AgentEvent); 5] {
        use agent::{AgentEvent, ItemContent, ThreadItem, TurnStatus};

        let item = |id: &str, content| {
            AgentEvent::ItemCompleted(ThreadItem {
                id: id.into(),
                parent_item_id: None,
                content,
            })
        };
        [
            (
                None,
                AgentEvent::TurnStarted {
                    turn_id: "t1".into(),
                },
            ),
            (
                None,
                item(
                    "user",
                    ItemContent::UserMessage {
                        text: "What changed?".into(),
                        context_len: None,
                        attachments: vec![],
                    },
                ),
            ),
            (
                Some(1_000),
                item(
                    "assistant",
                    ItemContent::AssistantMessage {
                        text: "The parser.".into(),
                        summarized: false,
                    },
                ),
            ),
            (
                Some(2_000),
                AgentEvent::TurnCompleted {
                    turn_id: "t1".into(),
                    status: TurnStatus::Completed,
                    usage: None,
                },
            ),
            (
                Some(3_000),
                AgentEvent::Warning {
                    message: "Reconnecting.".into(),
                },
            ),
        ]
    }

    /// A host started on a data dir an older build wrote moves the JSON index
    /// and the JSONL logs into its database once, and serves every thread's
    /// timeline exactly as the logs fold, before and after a restart.
    ///
    /// Each start is its own process, as the app's are. A restart inside one
    /// process can find the data dir still locked: a stopped host's detached
    /// work (here a git status refresh) may be inside a spawn, and the child's
    /// copy of the process's descriptors holds the lock until it execs.
    #[test]
    fn legacy_files_migrated_at_startup_serve_the_same_timelines() {
        let data_root =
            std::env::temp_dir().join(format!("tcode-host-migration-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&data_root).unwrap();
        let events = legacy_events();
        let line = |(ts, event): &(Option<u64>, agent::AgentEvent)| match ts {
            None => serde_json::to_string(event).unwrap(),
            Some(ts) => serde_json::json!({"ts": ts, "event": event}).to_string(),
        };
        // Legacy bare events, a CRLF line, a blank and an unparseable line,
        // and a last line without its newline.
        let log = format!(
            "{}\n{}\n\n{{torn\n{}\r\n{}\n{}",
            line(&events[0]),
            line(&events[1]),
            line(&events[2]),
            line(&events[3]),
            line(&events[4]),
        );
        std::fs::write(data_root.join("legacy-thread.jsonl"), &log).unwrap();
        std::fs::write(
            data_root.join("sessions.json"),
            serde_json::json!({
                "projects": [{"id": "p", "name": "Project", "root": "/work", "created_at": 1}],
                "sessions": [{
                    "id": "legacy-thread", "title": "Legacy", "provider": "codex",
                    "cwd": "/work", "project_id": "p", "created_at": 1, "updated_at": 2
                }]
            })
            .to_string(),
        )
        .unwrap();

        for start in ["migrating", "restarted"] {
            let output = tcode_services::process::command(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "pipe::tests::legacy_files_start_process",
                    "--ignored",
                    "--nocapture",
                ])
                .env(LEGACY_START_DIR, &data_root)
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success(),
                "{start}: {stdout}{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                stdout.contains(&format!("{LEGACY_START_REPORT}{start}")),
                "{start}: {stdout}"
            );
            assert!(!data_root.join("sessions.json").exists(), "{start}");
            assert!(!data_root.join("legacy-thread.jsonl").exists(), "{start}");
        }
        std::fs::remove_dir_all(data_root).unwrap();
    }

    /// One start of the app: migrate if an older build's files are there,
    /// serve the legacy thread's timeline, and stop.
    #[test]
    #[ignore = "a start of legacy_files_migrated_at_startup_serve_the_same_timelines, which runs it"]
    fn legacy_files_start_process() {
        use tcode_core::session::{StoredEvent, Timeline};

        let data_root = std::path::PathBuf::from(std::env::var_os(LEGACY_START_DIR).unwrap());
        let store = SessionStore::open_at(data_root).unwrap();
        let migrating = store.needs_migration().unwrap();
        if migrating {
            store
                .migrate(|_| {}, &std::sync::atomic::AtomicBool::new(false))
                .unwrap();
        }
        let host = spawn_host(store, HostServices::default()).unwrap();
        let link = host.link();
        let stream = super::p4b_tests::observed_events(&link);
        let topic = Topic::SessionEvents {
            session_id: "legacy-thread".into(),
        };
        link.subscribe(Subscription {
            known_index: None,
            after: None,
            topic: topic.clone(),
        })
        .unwrap();
        let snapshot = next_event(&stream, |event| {
            event.topic == topic && matches!(event.event, ServerEvent::SessionSnapshot { .. })
        });
        let ServerEvent::SessionSnapshot {
            records,
            end,
            total,
            ..
        } = snapshot.event
        else {
            unreachable!("filtered to snapshots")
        };
        assert_eq!(end, total, "the window reaches the end of the log");
        assert_eq!(
            format!("{:?}", Timeline::fold_events(records)),
            format!(
                "{:?}",
                Timeline::fold_events(legacy_events().into_iter().map(|(ts, event)| {
                    StoredEvent {
                        origin: None,
                        author: None,
                        ts,
                        event,
                        elided: None,
                    }
                }))
            )
        );
        host.shutdown_blocking().unwrap();
        let start = if migrating { "migrating" } else { "restarted" };
        println!("{LEGACY_START_REPORT}{start}");
    }

    #[test]
    fn dedicated_host_round_trips_commands_queries_events_and_quit_barrier() {
        let data_root =
            std::env::temp_dir().join(format!("tcode-host-pipe-test-{}", uuid::Uuid::new_v4()));
        let project_root = data_root.join("project");
        std::fs::create_dir_all(&project_root).expect("create project root");
        let store = SessionStore::open_at(data_root.clone()).expect("open session store");
        let host = spawn_host(store, HostServices::default()).expect("spawn host");

        host.to_host
            .try_send("{not valid ndjson}\n".into())
            .expect("send malformed client line");
        let decode_error = loop {
            if let HostMessage::Ack {
                id: 0,
                result: Err(error),
            } = tcode_protocol::decode_host_line(
                &host
                    .from_host
                    .recv_blocking()
                    .expect("receive protocol-error response"),
            )
            .expect("decode protocol-error response")
            {
                break error;
            }
        };
        assert_eq!(decode_error.code, "decode_error");
        let link = host.link();
        let events = super::p4b_tests::observed_events(&link);

        link.subscribe(Subscription {
            known_index: None,
            after: None,
            topic: Topic::Index,
        })
        .expect("send subscription");
        let snapshot = next_event(&events, |event| {
            event.topic == Topic::Index && matches!(&event.event, ServerEvent::IndexSnapshot(_))
        });
        assert!(matches!(
            snapshot.event,
            ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                ref sessions,
                ref projects,
                ..
            }) if sessions.is_empty() && projects.is_empty()
        ));

        let project_id = match link
            .command_blocking(Command::CreateProject {
                root: project_root.clone(),
            })
            .expect("create project over command pipe")
        {
            CommandResponse::ProjectId(Some(project_id)) => project_id,
            other => panic!("unexpected create-project response: {other:?}"),
        };
        let upsert = next_event(&events, |event| {
            event.topic == Topic::Index
                && matches!(
                    &event.event,
                    ServerEvent::IndexUpsertProject(project) if project.id == project_id
                )
        });
        let ServerEvent::IndexUpsertProject(project) = upsert.event else {
            unreachable!("filtered to project upserts")
        };
        assert_eq!(project.root, project_root);

        // A path the client believes in but the host cannot resolve is refused
        // by the host, not by whatever OS the client happens to run.
        let rejected = link
            .command_blocking(Command::CreateProject {
                root: project_root.join("does-not-exist"),
            })
            .expect_err("missing project root must be refused");
        assert_eq!(rejected.code, "invalid_project_root");
        assert_eq!(
            smol::block_on(link.query(Query::ListActiveWorkspace {
                session_id: "missing".into()
            }))
            .expect("query inactive workspace over pipe"),
            QueryResponse::ActiveWorkspace(Vec::new())
        );

        link.shutdown_blocking()
            .expect("drain quit barrier and stop host");
        host.stopped
            .recv_blocking()
            .expect("wait for host thread to stop");
        drop(host);
        std::fs::remove_dir_all(data_root).expect("remove test data");
    }
}

#[cfg(test)]
#[path = "pipe_p4b_tests.rs"]
pub(crate) mod p4b_tests;

#[cfg(test)]
#[path = "terminal_replication_tests.rs"]
#[cfg(unix)]
mod terminal_replication_tests;

#[cfg(test)]
#[path = "pipe_spaces_tests.rs"]
mod spaces_tests;

#[cfg(test)]
#[path = "pipe_project_creation_tests.rs"]
mod project_creation_tests;

#[cfg(test)]
#[path = "pipe_project_root_tests.rs"]
mod project_root_tests;
