//! NDJSON in-process client/host pipe with typed endpoint APIs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use tcode_client::HostLink;
#[cfg(test)]
use tcode_client::{HostEventReceiver, HostEventTryRecvError};
use tcode_protocol::{
    ClientMessage, ClientPayload, Command, CommandResponse, HostMessage, ProtocolError, Query,
    QueryResponse, decode_client_line,
};
#[cfg(test)]
use tcode_protocol::{EventEnvelope, ServerEvent, Subscription, Topic};
use tcode_services::import::ExternalImportUpdate;
use tcode_services::store::SessionStore;

use crate::app::{AppState, DomainDiff};
use crate::host::{HostCx, HostEvent, HostMsg};
use crate::terminal::LocalTerminalRegistry;

/// Optional process-local services attached before the host starts accepting
/// client traffic.
#[derive(Default)]
pub struct HostServices {
    /// Run provider catalog, version, and status probes during host startup.
    pub background_startup_probes: bool,
    /// Generate AI-authored titles after the first completed turn.
    pub ai_title_generation: bool,
    /// URL/tokens and the broker receiver stay host-side. Requests reach
    /// subscribed WebViews through the preview reverse-RPC topic.
    pub preview: Option<preview_mcp::PreviewMcpServer>,
    /// Deliberate construction-time local handle. The broker receiver stays
    /// entirely on the host executor; a remote transport must expose the same
    /// operations as correlated RPC instead of moving the channel.
    pub orchestrate: Option<orchestrate_mcp::OrchestrateMcpServer>,
    /// Registration-only startup data (URL and bearer token); this contains no
    /// request receiver or live backend handle.
    pub computer_use: Option<computer_use_mcp::ComputerUseMcpServer>,
}

#[derive(Clone, Default)]
pub struct ImportRoutes(Arc<Mutex<HashMap<u64, smol::channel::Sender<ExternalImportUpdate>>>>);

#[derive(Clone)]
pub struct SpawnedHost {
    pub to_host: async_channel::Sender<String>,
    pub from_host: async_channel::Receiver<String>,
    pub stopped: async_channel::Receiver<()>,
    pub terminals: LocalTerminalRegistry,
    /// Legacy local affordance; new hosts route preview requests through the link.
    pub preview_requests: Option<async_channel::Receiver<preview_mcp::BrokerRequest>>,
    pub import_routes: ImportRoutes,
    link: Arc<OnceLock<HostLink>>,
    #[cfg(any(test, feature = "test-support"))]
    test_mailbox: async_channel::Sender<HostMsg>,
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
            .send(HostMsg::Enqueued(Box::new(move |state, cx| {
                let result = update(state, cx);
                let _ = sender.try_send(result);
            })))
            .await
            .map_err(transport_error)?;
        receiver.recv().await.map_err(transport_error)
    }
}

pub async fn start_external_import(
    link: &HostLink,
    routes: &ImportRoutes,
    project_id: String,
    threads: Vec<tcode_protocol::ExternalThread>,
) -> Result<Option<async_channel::Receiver<ExternalImportUpdate>>, ProtocolError> {
    let (id, response) = link.command_with_id(Command::StartExternalImport {
        project_id,
        threads,
    });
    let (sender, receiver) = async_channel::unbounded();
    routes.0.lock().unwrap().insert(id, sender);
    match response.await {
        Ok(CommandResponse::ExternalImportStarted(true)) => Ok(Some(receiver)),
        Ok(CommandResponse::ExternalImportStarted(false)) => {
            routes.0.lock().unwrap().remove(&id);
            Ok(None)
        }
        Ok(other) => {
            routes.0.lock().unwrap().remove(&id);
            Err(ProtocolError {
                code: "unexpected_response".into(),
                message: format!("expected external-import started result, got {other:?}"),
            })
        }
        Err(error) => {
            routes.0.lock().unwrap().remove(&id);
            Err(error)
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
fn transport_error(error: impl std::fmt::Display) -> ProtocolError {
    ProtocolError {
        code: "transport_closed".into(),
        message: error.to_string(),
    }
}

/// Spawn the dedicated host thread and return its serialized transport and local affordances.
pub fn spawn_host(store: SessionStore, mut services: HostServices) -> std::io::Result<SpawnedHost> {
    fn assert_send<T: Send>() {}
    assert_send::<AppState>();

    let (client_tx, client_rx) = async_channel::unbounded::<String>();
    let (event_tx, event_rx) = async_channel::unbounded::<String>();
    let (stopped_tx, stopped_rx) = smol::channel::bounded(1);
    let (mailbox_tx, mailbox_rx) = smol::channel::unbounded::<HostMsg>();
    let terminals = LocalTerminalRegistry::default();
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

    let broker_requests = preview_requests;
    let preview_requests = None;
    let import_routes = ImportRoutes::default();
    let host_terminals = terminals.clone();
    let host_import_routes = import_routes.clone();
    #[cfg(any(test, feature = "test-support"))]
    let test_mailbox = mailbox_tx.clone();

    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("tcode-host".into())
        .spawn(move || {
            let mut state = AppState::new_with_terminal_registry(
                store,
                host_terminals,
                services.ai_title_generation,
            );
            if let Some((url, tokens)) = preview_registration {
                state.attach_preview_mcp(url, tokens);
            }
            if let Some(server) = services.orchestrate.take() {
                state.attach_orchestrate_mcp(server);
            }
            if let Some(server) = services.computer_use.take() {
                state.attach_computer_use_mcp(server.url, server.token);
            }
            let mut cx = HostCx::new(mailbox_tx, event_tx);
            state.pump_orchestrate_requests(&mut cx);
            state.pump_preview_requests(broker_requests, &mut cx);
            if services.background_startup_probes {
                state.recover_orphaned_worktrees(&mut cx);
                state.refresh_model_catalogs(&mut cx);
                if state.provider_update_checks_enabled() {
                    state.check_provider_versions(&mut cx);
                }
                state.refresh_provider_usage(&mut cx);
                state.refresh_provider_status(&mut cx);
            }
            state.sync_terminal_handles();
            let _ = ready_tx.send(());
            smol::block_on(host_loop(
                state,
                cx,
                client_rx,
                mailbox_rx,
                host_import_routes,
            ));
            let _ = stopped_tx.send_blocking(());
        })?;
    ready_rx.recv().map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            format!("host failed during startup: {error}"),
        )
    })?;

    Ok(SpawnedHost {
        to_host: client_tx,
        from_host: event_rx,
        stopped: stopped_rx,
        terminals,
        preview_requests,
        import_routes,
        link: Arc::new(OnceLock::new()),
        #[cfg(any(test, feature = "test-support"))]
        test_mailbox,
    })
}

async fn host_loop(
    mut state: AppState,
    mut cx: HostCx,
    client: smol::channel::Receiver<String>,
    mailbox: smol::channel::Receiver<HostMsg>,
    import_routes: ImportRoutes,
) {
    let mut domain_diff = DomainDiff::new(&state);
    loop {
        enum Input {
            Client(Result<String, smol::channel::RecvError>),
            Internal(Result<HostMsg, smol::channel::RecvError>),
        }
        match smol::future::race(async { Input::Client(client.recv().await) }, async {
            Input::Internal(mailbox.recv().await)
        })
        .await
        {
            Input::Client(message) => match message {
                Ok(line) => match decode_client_line(&line) {
                    Ok(message) => {
                        handle_client_message(&mut state, &mut cx, message, &import_routes)
                    }
                    Err(error) => cx.send_message(HostMessage::Ack {
                        id: malformed_message_id(&line).unwrap_or(0),
                        result: Err(error),
                    }),
                },
                Err(_) => break,
            },
            Input::Internal(message) => match message {
                Ok(HostMsg::Enqueued(operation)) => operation(&mut state, &mut cx),
                Err(_) => break,
            },
        }
        state.sync_terminal_handles();
        state.reap_terminal_output();
        domain_diff.emit_changes(&state, &mut cx);
    }
}

fn malformed_message_id(line: &str) -> Option<u64> {
    serde_json::from_str::<serde_json::Value>(line.trim_end())
        .ok()?
        .get("id")?
        .as_u64()
}

fn handle_client_message(
    state: &mut AppState,
    cx: &mut HostCx,
    message: ClientMessage,
    import_routes: &ImportRoutes,
) {
    let ClientMessage { id, payload } = message;
    match payload {
        ClientPayload::Command(command) => {
            let outcome = dispatch_command(state, cx, id, command, import_routes);
            match outcome {
                CommandOutcome::Immediate(result) => {
                    cx.send_message(HostMessage::Ack { id, result })
                }
                CommandOutcome::StoreBarrier(barrier) => {
                    let response_cx = cx.clone();
                    cx.spawn_detached(async move {
                        let result = barrier
                            .recv()
                            .await
                            .map(|()| CommandResponse::Unit)
                            .map_err(|error| ProtocolError {
                                code: "store_barrier_closed".into(),
                                message: error.to_string(),
                            });
                        response_cx.send_message(HostMessage::Ack { id, result });
                    });
                }
            }
        }
        ClientPayload::Query(query) => {
            let task = dispatch_query(state, cx, query);
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
            if let Some(mut snapshot) = state.subscription_snapshot(&subscription) {
                snapshot.request_id = Some(id);
                cx.emit(HostEvent::Domain(snapshot));
            }
            cx.send_message(HostMessage::Ack {
                id,
                result: Ok(CommandResponse::Unit),
            });
        }
        ClientPayload::Unsubscribe(subscription) => {
            state.unsubscribe(&subscription, cx);
            cx.send_message(HostMessage::Ack {
                id,
                result: Ok(CommandResponse::Unit),
            });
        }
    }
}

#[cfg(test)]
pub(crate) fn handle_client_message_for_test(
    state: &mut AppState,
    cx: &mut HostCx,
    message: ClientMessage,
) {
    handle_client_message(state, cx, message, &ImportRoutes::default());
}

enum CommandOutcome {
    Immediate(Result<CommandResponse, ProtocolError>),
    StoreBarrier(smol::channel::Receiver<()>),
}

fn dispatch_command(
    app: &mut AppState,
    cx: &mut HostCx,
    request_id: u64,
    command: Command,
    import_routes: &ImportRoutes,
) -> CommandOutcome {
    let mut response = CommandResponse::Unit;
    match command {
        Command::TerminalInput { terminal_id, bytes } => {
            if let Some(terminal) = app.terminal_handle(terminal_id) {
                terminal.write_input(bytes);
            }
        }
        Command::ResizeTerminal {
            terminal_id,
            cols,
            rows,
        } => {
            if let Some(terminal) = app.terminal_handle(terminal_id) {
                terminal.resize(
                    usize::from(cols.clamp(2, 1000)),
                    usize::from(rows.clamp(2, 1000)),
                );
            }
        }
        Command::PreviewReply {
            request_id,
            response,
        } => app.resolve_preview(request_id, response),
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
        Command::OrchestrateTurn {
            session_id,
            text,
            attachment_paths,
        } => app.orchestrate_turn(&session_id, text, attachment_paths, cx),
        Command::ReloadProvider => app.reload_provider(cx),
        Command::SetProfileSecret {
            profile_id,
            name,
            value,
        } => app.set_profile_secret(&profile_id, &name, value.as_deref(), cx),
        Command::UpdateProfileSettings { profile_id, patch } => {
            app.update_profile_settings(&profile_id, patch, cx)
        }
        Command::CreateThirdPartyProfile {
            name,
            base_url,
            model,
            api_key,
        } => {
            app.create_third_party_profile(&name, &base_url, model.as_deref(), &api_key, cx);
        }
        Command::DeleteProfile { profile_id } => app.delete_profile(&profile_id, cx),
        Command::RefreshProviderStatus => app.refresh_provider_status(cx),
        Command::RefreshProviderUsage => app.refresh_provider_usage(cx),
        Command::CheckProviderVersions => app.check_provider_versions(cx),
        Command::UpdateProvider { provider } => app.update_provider(provider, cx),
        Command::SetSidebarCollapsed { collapsed } => app.set_sidebar_collapsed(collapsed, cx),
        Command::RunGitAction {
            session_id,
            action,
            message,
            included,
            feature_branch,
        } => app.run_git_action(&session_id, action, message, included, feature_branch, cx),
        Command::RefreshAcpRegistry => app.refresh_acp_registry(cx),
        Command::InstallAcpAgent { id } => app.install_acp_agent(id, cx),
        Command::RemoveAcpAgent { id } => app.remove_acp_agent(&id, cx),
        Command::AddCustomAcpAgent {
            name,
            command,
            args,
            env,
        } => app.add_custom_acp_agent(name, command, args, env, cx),
        Command::UpdateAcpAgent { id, patch } => app.update_acp_agent(&id, patch, cx),
        Command::SetActiveAcpAgent { session_id, id } => {
            app.set_active_acp_agent(&session_id, &id, cx)
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
        Command::CreateProject { root } => {
            response = CommandResponse::ProjectId(app.create_project(root, cx));
        }
        Command::StartExternalImport {
            project_id,
            threads,
        } => {
            let receiver = app.start_external_import(&project_id, threads, cx);
            response = CommandResponse::ExternalImportStarted(receiver.is_some());
            if let Some(receiver) = receiver {
                let route = import_routes.0.lock().unwrap().get(&request_id).cloned();
                let import_routes = import_routes.clone();
                cx.spawn_detached(async move {
                    if let Some(route) = route {
                        while let Ok(update) = receiver.recv().await {
                            if route.send(update).await.is_err() {
                                break;
                            }
                        }
                    }
                    import_routes.0.lock().unwrap().remove(&request_id);
                });
            } else {
                import_routes.0.lock().unwrap().remove(&request_id);
            }
        }
        Command::FinishExternalImport { project_id } => app.finish_external_import(&project_id, cx),
        Command::ExportThread {
            session_id,
            destination,
            format,
        } => app.export_thread(&session_id, destination, format, cx),
        Command::ToggleProjectCollapsed { project_id } => {
            app.toggle_project_collapsed(&project_id, cx)
        }
        Command::PatchSettings { patch } => app.patch_settings(patch, cx),
        Command::ArchiveSession { session_id } => app.archive_session(&session_id, cx),
        Command::UnarchiveSession { session_id } => app.unarchive_session(&session_id, cx),
        Command::AutoArchiveSweep { project_id } => {
            response = CommandResponse::ArchivedCount(app.auto_archive_sweep(&project_id, cx));
        }
        Command::RenameSession { session_id, title } => app.rename_session(&session_id, &title, cx),
        Command::ForkThread { id } => {
            response = CommandResponse::SessionId(app.fork_thread(&id, cx));
        }
        Command::MergeWorktree { session_id } => app.merge_worktree(&session_id, cx),
        Command::DeleteSession {
            session_id,
            remove_worktree,
        } => app.delete_session(&session_id, remove_worktree, cx),
        Command::DeleteProject { project_id } => app.delete_project(&project_id, cx),
        Command::MarkSessionUnread { session_id } => app.mark_session_unread(&session_id, cx),
        Command::StartDraft { project_id, cwd } => {
            response = CommandResponse::SessionId(Some(app.start_draft(project_id, cwd, cx)));
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
        Command::Interrupt { session_id } => app.interrupt(&session_id, cx),
        Command::RespondApproval {
            session_id,
            request_id,
            decision,
        } => app.respond_approval(&session_id, request_id, decision, cx),
        Command::RespondUserInput {
            session_id,
            request_id,
            answers,
        } => app.respond_user_input(&session_id, request_id, answers, cx),
        Command::SetActiveModel {
            session_id,
            provider,
            model,
            profile_id,
        } => app.set_active_model(&session_id, provider, model, profile_id, cx),
        Command::SetActiveOption {
            session_id,
            id,
            value,
        } => app.set_active_option(&session_id, &id, value, cx),
        Command::SelectUltrathink { session_id } => app.select_ultrathink(&session_id, cx),
        Command::SetInteractionMode { session_id, mode } => {
            app.set_interaction_mode(&session_id, mode, cx)
        }
        Command::ToggleInteractionMode { session_id } => {
            app.toggle_interaction_mode(&session_id, cx)
        }
        Command::ImplementPlan { session_id } => app.implement_plan(&session_id, cx),
        Command::DismissPlan { session_id } => app.dismiss_plan(&session_id, cx),
        Command::ImplementPlanInNewThread { session_id, title } => {
            response = CommandResponse::SessionId(app.implement_plan_in_new_thread(
                &session_id,
                title,
                cx,
            ));
        }
        Command::CopyPlan { markdown } => app.copy_plan(markdown, cx),
        Command::SavePlanToWorkspace {
            session_id,
            markdown,
        } => app.save_plan_to_workspace(&session_id, markdown, cx),
        Command::DownloadPlan {
            session_id,
            markdown,
            fallback_title,
        } => app.download_plan(&session_id, markdown, fallback_title, cx),
        Command::LoadBranches { session_id } => app.load_branches(&session_id, cx),
        Command::CheckoutBranch { session_id, branch } => {
            app.checkout_branch(&session_id, branch, cx)
        }
        Command::SetActiveApprovalMode { session_id, mode } => {
            app.set_active_approval_mode(&session_id, mode, cx)
        }
        Command::ToggleFavoriteModel { model } => app.toggle_favorite_model(&model, cx),
        Command::RewindTurn {
            session_id,
            turn,
            mode,
        } => app.rewind_turn(&session_id, turn, mode, cx),
    }
    CommandOutcome::Immediate(Ok(response))
}

fn dispatch_query(
    app: &AppState,
    cx: &HostCx,
    query: Query,
) -> crate::host::HostTask<Result<QueryResponse, ProtocolError>> {
    match query {
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
        Query::ReadFileBytes { path } => {
            let task = cx.unblock(move || std::fs::read(&path));
            cx.spawn_background(async move {
                task.await
                    .map(QueryResponse::FileBytes)
                    .map_err(io_protocol_error)
            })
        }
        Query::SaveAttachment { dir, bytes, ext } => {
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
        Query::IsDirectory { path } => {
            let task = cx.unblock(move || path.is_dir());
            cx.spawn_background(async move { Ok(QueryResponse::IsDirectory(task.await)) })
        }
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

    pub(super) fn next_event(
        events: &HostEventReceiver,
        ready: impl Fn(&EventEnvelope) -> bool,
    ) -> EventEnvelope {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match events.try_recv() {
                Ok(envelope) => {
                    if ready(&envelope) {
                        return envelope;
                    }
                }
                Err(HostEventTryRecvError::Empty) => {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(HostEventTryRecvError::Closed) => {
                    panic!("host event stream closed before the expected event")
                }
            }
        }
        panic!("timed out waiting for host event");
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
        let events = link.events();

        link.subscribe(Subscription {
            after: None,
            topic: Topic::Index,
        })
        .expect("send subscription");
        let snapshot = next_event(&events, |event| {
            event.topic == Topic::Index && matches!(&event.event, ServerEvent::IndexSnapshot(_))
        });
        assert!(matches!(
            snapshot.event,
            ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot { activity: _,
                ref sessions,
                ref projects,
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
        let replacement = next_event(&events, |event| {
            event.topic == Topic::Index
                && matches!(
                    &event.event,
                    ServerEvent::IndexSnapshot(snapshot)
                        if snapshot.projects.iter().any(|project| project.id == project_id)
                )
        });
        let ServerEvent::IndexSnapshot(snapshot) = replacement.event else {
            unreachable!("filtered to index snapshots")
        };
        assert!(
            snapshot
                .projects
                .iter()
                .any(|project| project.id == project_id && project.root == project_root)
        );

        let import_progress = smol::block_on(start_external_import(
            &link,
            &host.import_routes,
            project_id.clone(),
            Vec::new(),
        ))
        .expect("start import over command")
        .expect("known project starts an import");
        assert_eq!(
            import_progress
                .recv_blocking()
                .expect("receive construction-bus progress"),
            ExternalImportUpdate::Finished {
                imported: 0,
                skipped: 0,
            }
        );
        assert!(
            smol::block_on(start_external_import(
                &link,
                &host.import_routes,
                "missing".into(),
                Vec::new(),
            ))
            .expect("unknown import command response")
            .is_none()
        );

        assert_eq!(
            smol::block_on(link.query(Query::IsDirectory {
                path: project_root.clone(),
            }))
            .expect("query directory over pipe"),
            QueryResponse::IsDirectory(true)
        );
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
mod p4b_tests;
