//! Serialized in-process client/host pipe.
//!
//! Both directions carry real NDJSON strings of `tcode-protocol` wire types.
//! The desktop client therefore exercises the same serde boundary a future
//! socket transport will use.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tcode_protocol::{
    ClientMessage, ClientPayload, Command, CommandResponse, EventEnvelope, HelloAck, HostMessage,
    ProtocolError, Query, QueryResponse, ServerEvent, Subscription, Topic, decode_host_line,
    encode_line,
};
use tcode_services::store::SessionStore;

use crate::app::AppState;
use crate::blocking::unblock_host;
use crate::event::HostEvent;
use crate::host::{HostCx, HostMsg, HostOutput};
use crate::terminal::LocalTerminalRegistry;
use crate::ui_facade::ExternalImportUpdate;

/// Optional process-local services attached before the host starts accepting
/// client traffic.
#[derive(Default)]
pub struct HostServices {
    /// Split at host construction: URL/tokens stay host-side and the
    /// non-serializable WebView broker receiver moves once to `HostHandle`.
    /// A remote transport replaces that receiver with reverse RPC.
    pub preview: Option<preview_mcp::PreviewMcpServer>,
    /// Deliberate construction-time local handle. The broker receiver stays
    /// entirely on the host executor; a remote transport must expose the same
    /// operations as correlated RPC instead of moving the channel.
    pub orchestrate: Option<orchestrate_mcp::OrchestrateMcpServer>,
    /// Registration-only startup data (URL and bearer token); this contains no
    /// request receiver or live backend handle.
    pub computer_use: Option<computer_use_mcp::ComputerUseMcpServer>,
}

/// Messages on the one process-local external-import progress bus installed at
/// host construction. The `StartExternalImport` command and its started result
/// still cross NDJSON; a remote transport replaces this bus with events.
enum LocalImportProgress {
    Update {
        id: u64,
        update: ExternalImportUpdate,
    },
    Closed {
        id: u64,
    },
}

struct HostHandleInner {
    client_tx: async_channel::Sender<String>,
    event_rx: async_channel::Receiver<String>,
    changed_rx: async_channel::Receiver<()>,
    stopped_rx: async_channel::Receiver<()>,
    #[cfg(feature = "test-support")]
    test_mailbox: async_channel::Sender<HostMsg>,
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, async_channel::Sender<HostMessage>>>>,
    import_routes: Arc<Mutex<HashMap<u64, async_channel::Sender<ExternalImportUpdate>>>>,
    preview_requests: Mutex<Option<async_channel::Receiver<preview_mcp::BrokerRequest>>>,
    terminals: LocalTerminalRegistry,
}

/// Client-side endpoint for the serialized host.
///
/// `client_tx` and `event_rx` contain NDJSON lines, never typed shortcuts.
/// The three non-serialized resources are documented at their accessors:
/// preview broker requests, external-import progress, and live terminal
/// handles.
#[derive(Clone)]
pub struct HostHandle {
    inner: Arc<HostHandleInner>,
}

impl HostHandle {
    fn next_id(&self) -> u64 {
        self.inner.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn send_payload(&self, id: u64, payload: ClientPayload) -> Result<(), ProtocolError> {
        let line = encode_line(&ClientMessage { id, payload })?;
        self.inner
            .client_tx
            .try_send(line)
            .map_err(|error| ProtocolError {
                code: "transport_closed".into(),
                message: error.to_string(),
            })
    }

    async fn request(&self, payload: ClientPayload) -> Result<HostMessage, ProtocolError> {
        let id = self.next_id();
        let (sender, receiver) = async_channel::bounded(1);
        self.inner.pending.lock().unwrap().insert(id, sender);
        if let Err(error) = self.send_payload(id, payload) {
            self.inner.pending.lock().unwrap().remove(&id);
            return Err(error);
        }
        receiver.recv().await.map_err(|error| ProtocolError {
            code: "transport_closed".into(),
            message: error.to_string(),
        })
    }

    /// Fire a mutation through serde/NDJSON. Its correlated ack is intentionally
    /// ignored by this fire-and-forget UI convenience.
    pub fn dispatch(&self, command: Command) -> Result<(), ProtocolError> {
        let id = self.next_id();
        self.send_payload(id, ClientPayload::Command(command))
    }

    pub async fn command(&self, command: Command) -> Result<CommandResponse, ProtocolError> {
        match self.request(ClientPayload::Command(command)).await? {
            HostMessage::Ack { result, .. } => result,
            other => Err(unexpected_response("command ack", &other)),
        }
    }

    pub fn command_blocking(&self, command: Command) -> Result<CommandResponse, ProtocolError> {
        smol::block_on(self.command(command))
    }

    pub async fn query(&self, query: Query) -> Result<QueryResponse, ProtocolError> {
        match self.request(ClientPayload::Query(query)).await? {
            HostMessage::QueryResult { result, .. } => result,
            other => Err(unexpected_response("query result", &other)),
        }
    }

    /// Drain the host's store-write barrier, close the serialized client
    /// endpoint, and wait until the state-owning thread has exited.
    ///
    /// This is the terminal lifecycle operation for the one-client in-process
    /// transport. Closing happens after the correlated command result so the
    /// shutdown request itself always crosses NDJSON.
    pub async fn shutdown(&self) -> Result<(), ProtocolError> {
        let result = self.command(Command::ShutdownAllAndFlush).await;
        self.inner.client_tx.close();
        let stopped = self.inner.stopped_rx.recv().await;
        match result? {
            CommandResponse::Unit => {}
            other => {
                return Err(ProtocolError {
                    code: "unexpected_response".into(),
                    message: format!("expected unit shutdown ack, got {other:?}"),
                });
            }
        }
        stopped.map_err(|error| ProtocolError {
            code: "host_stop_failed".into(),
            message: error.to_string(),
        })
    }

    pub fn shutdown_blocking(&self) -> Result<(), ProtocolError> {
        smol::block_on(self.shutdown())
    }

    pub fn subscribe(&self, subscription: Subscription) -> Result<(), ProtocolError> {
        let id = self.next_id();
        self.send_payload(id, ClientPayload::Subscribe(subscription))
    }

    pub fn hello(&self, app_version: impl Into<String>) -> Result<(), ProtocolError> {
        let id = self.next_id();
        self.send_payload(
            id,
            ClientPayload::Hello(tcode_protocol::Hello {
                protocol_version: tcode_protocol::PROTOCOL_VERSION,
                app_version: app_version.into(),
                capabilities: vec![
                    "ndjson".into(),
                    "local-terminal-byte-streams".into(),
                    "local-preview-reverse-rpc".into(),
                ],
            }),
        )
    }

    /// Serialized event lines consumed by the client-side replica pump.
    pub fn event_receiver(&self) -> async_channel::Receiver<String> {
        self.inner.event_rx.clone()
    }

    /// Coalesced notification-only changes consumed by the client-side pump.
    pub fn changed_receiver(&self) -> async_channel::Receiver<()> {
        self.inner.changed_rx.clone()
    }

    /// Take the preview MCP request receiver once.
    ///
    /// This is a deliberate local-transport affordance because each request
    /// includes a reply sender and a native WebView must answer on the client.
    /// A remote transport must replace it with reverse RPC.
    pub fn take_preview_requests(
        &self,
    ) -> Option<async_channel::Receiver<preview_mcp::BrokerRequest>> {
        self.inner.preview_requests.lock().unwrap().take()
    }

    /// Access the construction-time live-terminal handle registry.
    ///
    /// No bytes or layout metadata use JSON here: terminal bytes stay on the
    /// split term channels, while layout is replicated in `SessionStatus`.
    pub fn terminal_registry(&self) -> LocalTerminalRegistry {
        self.inner.terminals.clone()
    }

    /// Start an import through a serialized command and receive its one allowed
    /// client-local progress receiver, correlated by the command's wire id.
    /// Progress enters the client through the single process-local bus installed
    /// when the host is constructed; no per-command channel handle crosses.
    pub async fn start_external_import(
        &self,
        project_id: String,
        threads: Vec<tcode_protocol::ExternalThread>,
    ) -> Result<Option<async_channel::Receiver<ExternalImportUpdate>>, ProtocolError> {
        let id = self.next_id();
        let (progress_sender, progress_receiver) = async_channel::unbounded();
        self.inner
            .import_routes
            .lock()
            .unwrap()
            .insert(id, progress_sender);
        let (ack_sender, ack_receiver) = async_channel::bounded(1);
        self.inner.pending.lock().unwrap().insert(id, ack_sender);
        if let Err(error) = self.send_payload(
            id,
            ClientPayload::Command(Command::StartExternalImport {
                project_id,
                threads,
            }),
        ) {
            self.inner.pending.lock().unwrap().remove(&id);
            self.inner.import_routes.lock().unwrap().remove(&id);
            return Err(error);
        }
        let started = match ack_receiver.recv().await {
            Ok(HostMessage::Ack {
                result: Ok(CommandResponse::ExternalImportStarted(started)),
                ..
            }) => started,
            Ok(HostMessage::Ack {
                result: Ok(other), ..
            }) => {
                self.inner.import_routes.lock().unwrap().remove(&id);
                return Err(ProtocolError {
                    code: "unexpected_response".into(),
                    message: format!("expected external-import started result, got {other:?}"),
                });
            }
            Ok(HostMessage::Ack {
                result: Err(error), ..
            }) => {
                self.inner.import_routes.lock().unwrap().remove(&id);
                return Err(error);
            }
            Ok(other) => {
                self.inner.import_routes.lock().unwrap().remove(&id);
                return Err(unexpected_response("external-import ack", &other));
            }
            Err(error) => {
                self.inner.import_routes.lock().unwrap().remove(&id);
                return Err(ProtocolError {
                    code: "transport_closed".into(),
                    message: error.to_string(),
                });
            }
        };
        if !started {
            self.inner.import_routes.lock().unwrap().remove(&id);
            return Ok(None);
        }
        Ok(Some(progress_receiver))
    }

    /// Test-fixture ingress for assertions that compare a client replica with
    /// the authoritative host value after an internal mutation.
    ///
    /// This is intentionally absent from production builds. The mutation
    /// enters the real host mailbox, and any emitted events still cross the
    /// NDJSON encoder/decoder before reaching the client.
    #[cfg(feature = "test-support")]
    pub async fn update_state_for_test<R>(
        &self,
        update: impl FnOnce(&mut AppState, &mut HostCx) -> R + Send + 'static,
    ) -> Result<R, ProtocolError>
    where
        R: Send + 'static,
    {
        let (sender, receiver) = async_channel::bounded(1);
        self.inner
            .test_mailbox
            .send(HostMsg::Enqueued(Box::new(move |state, cx| {
                let result = update(state, cx);
                let _ = sender.try_send(result);
            })))
            .await
            .map_err(|error| ProtocolError {
                code: "transport_closed".into(),
                message: error.to_string(),
            })?;
        receiver.recv().await.map_err(|error| ProtocolError {
            code: "transport_closed".into(),
            message: error.to_string(),
        })
    }
}

fn unexpected_response(expected: &str, actual: &HostMessage) -> ProtocolError {
    ProtocolError {
        code: "unexpected_response".into(),
        message: format!("expected {expected}, got {actual:?}"),
    }
}

/// Spawn the dedicated host thread and return its serialized client endpoint.
pub fn spawn_host(store: SessionStore, mut services: HostServices) -> std::io::Result<HostHandle> {
    fn assert_send<T: Send>() {}
    assert_send::<AppState>();

    let (client_tx, client_rx) = async_channel::unbounded::<String>();
    let (raw_host_tx, raw_host_rx) = async_channel::unbounded::<String>();
    let (event_tx, event_rx) = async_channel::unbounded::<String>();
    let (changed_tx, changed_rx) = async_channel::bounded(1);
    let (stopped_tx, stopped_rx) = async_channel::bounded(1);
    let (mailbox_tx, mailbox_rx) = async_channel::unbounded::<HostMsg>();
    let (outgoing_tx, outgoing_rx) = async_channel::unbounded::<HostOutput>();
    // Deliberate construction-time local affordance: one progress bus crosses
    // into the host thread. Per-import commands/results remain serialized, and
    // a future remote transport replaces this bus with correlated events.
    let (import_progress_tx, import_progress_rx) =
        async_channel::unbounded::<LocalImportProgress>();
    let terminals = LocalTerminalRegistry::default();
    // Deliberate local-transport affordance: the WebView broker request
    // receiver cannot cross serde. Move its single consumer exactly once into
    // `HostHandle`; a remote transport must replace it with reverse RPC. The
    // host retains only the URL/token registry used to register providers.
    let (preview_registration, preview_requests) = match services.preview.take() {
        Some(preview_mcp::PreviewMcpServer {
            url,
            tokens,
            requests,
        }) => (Some((url, tokens)), Some(requests)),
        None => (None, None),
    };

    let pending = Arc::new(Mutex::new(HashMap::new()));
    let import_routes = Arc::new(Mutex::new(HashMap::new()));
    let inner = Arc::new(HostHandleInner {
        client_tx,
        event_rx,
        changed_rx,
        stopped_rx,
        #[cfg(feature = "test-support")]
        test_mailbox: mailbox_tx.clone(),
        next_id: AtomicU64::new(1),
        pending: pending.clone(),
        import_routes: import_routes.clone(),
        preview_requests: Mutex::new(preview_requests),
        terminals: terminals.clone(),
    });

    spawn_client_router(raw_host_rx, event_tx, pending)?;
    spawn_local_import_router(import_progress_rx, import_routes)?;
    spawn_decoder(client_rx, mailbox_tx.clone(), outgoing_tx.clone())?;
    spawn_encoder(outgoing_rx, raw_host_tx)?;

    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("tcode-host".into())
        .spawn(move || {
            let mut state = AppState::new_with_terminal_registry(store, terminals);
            if let Some((url, tokens)) = preview_registration {
                state.attach_preview_mcp(url, tokens);
            }
            if let Some(server) = services.orchestrate.take() {
                state.attach_orchestrate_mcp(server);
            }
            if let Some(server) = services.computer_use.take() {
                state.attach_computer_use_mcp(server.url, server.token);
            }
            let mut cx = HostCx::new(mailbox_tx, outgoing_tx, changed_tx);
            state.pump_orchestrate_requests(&mut cx);
            if !cfg!(any(test, feature = "test-support")) {
                state.refresh_model_catalogs(&mut cx);
                if state.provider_update_checks_enabled() {
                    state.check_provider_versions(&mut cx);
                }
                state.refresh_provider_status(&mut cx);
            }
            state.sync_terminal_handles();
            let _ = ready_tx.send(());
            smol::block_on(host_loop(state, cx, mailbox_rx, import_progress_tx));
            let _ = stopped_tx.send_blocking(());
        })?;
    ready_rx.recv().map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            format!("host failed during startup: {error}"),
        )
    })?;

    Ok(HostHandle { inner })
}

fn spawn_decoder(
    client_rx: async_channel::Receiver<String>,
    mailbox: async_channel::Sender<HostMsg>,
    outgoing: async_channel::Sender<HostOutput>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("tcode-host-ndjson-decoder".into())
        .spawn(move || {
            while let Ok(line) = client_rx.recv_blocking() {
                match tcode_protocol::decode_client_line(&line) {
                    Ok(message) => {
                        if mailbox
                            .send_blocking(HostMsg::DecodedClient(Box::new(message)))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = outgoing.send_blocking(HostOutput::Message(HostMessage::Ack {
                            id: 0,
                            result: Err(error),
                        }));
                    }
                }
            }
            let _ = mailbox.send_blocking(HostMsg::ClientClosed);
        })?;
    Ok(())
}

fn spawn_encoder(
    outgoing: async_channel::Receiver<HostOutput>,
    raw_host: async_channel::Sender<String>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("tcode-host-ndjson-encoder".into())
        .spawn(move || {
            let mut runtime_seq = 0_u64;
            while let Ok(output) = outgoing.recv_blocking() {
                let message = match output {
                    HostOutput::Message(message) => message,
                    HostOutput::Event(HostEvent::Domain(envelope)) => HostMessage::Event(envelope),
                    HostOutput::Event(HostEvent::Runtime(notification)) => {
                        runtime_seq = runtime_seq.wrapping_add(1);
                        HostMessage::Event(EventEnvelope {
                            topic: Topic::RuntimeEvents,
                            seq: runtime_seq,
                            event: ServerEvent::Runtime(notification),
                        })
                    }
                };
                match encode_line(&message) {
                    Ok(line) => {
                        if raw_host.send_blocking(line).is_err() {
                            break;
                        }
                    }
                    Err(error) => log::error!("failed to encode host NDJSON: {}", error.message),
                }
            }
        })?;
    Ok(())
}

fn spawn_client_router(
    raw_host: async_channel::Receiver<String>,
    events: async_channel::Sender<String>,
    pending: Arc<Mutex<HashMap<u64, async_channel::Sender<HostMessage>>>>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("tcode-client-ndjson-router".into())
        .spawn(move || {
            while let Ok(line) = raw_host.recv_blocking() {
                let message = match decode_host_line(&line) {
                    Ok(message) => message,
                    Err(error) => {
                        log::error!("failed to decode host NDJSON: {}", error.message);
                        continue;
                    }
                };
                let id = match &message {
                    HostMessage::Ack { id, .. } | HostMessage::QueryResult { id, .. } => Some(*id),
                    _ => None,
                };
                if let Some(id) = id
                    && let Some(waiter) = pending.lock().unwrap().remove(&id)
                {
                    let _ = waiter.send_blocking(message);
                    continue;
                }
                if events.send_blocking(line).is_err() {
                    break;
                }
            }
        })?;
    Ok(())
}

fn spawn_local_import_router(
    progress: async_channel::Receiver<LocalImportProgress>,
    routes: Arc<Mutex<HashMap<u64, async_channel::Sender<ExternalImportUpdate>>>>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("tcode-client-local-import-router".into())
        .spawn(move || {
            while let Ok(progress) = progress.recv_blocking() {
                match progress {
                    LocalImportProgress::Update { id, update } => {
                        let route = routes.lock().unwrap().get(&id).cloned();
                        if let Some(route) = route {
                            let _ = route.send_blocking(update);
                        }
                    }
                    LocalImportProgress::Closed { id } => {
                        routes.lock().unwrap().remove(&id);
                    }
                }
            }
        })?;
    Ok(())
}

async fn host_loop(
    mut state: AppState,
    mut cx: HostCx,
    mailbox: async_channel::Receiver<HostMsg>,
    import_progress: async_channel::Sender<LocalImportProgress>,
) {
    let mut active_seq = 0_u64;
    let mut last_active: Option<Option<tcode_protocol::SessionStatus>> = None;
    while let Ok(message) = mailbox.recv().await {
        match message {
            HostMsg::DecodedClient(message) => {
                handle_client_message(&mut state, &mut cx, *message, &import_progress)
            }
            HostMsg::Enqueued(operation) => operation(&mut state, &mut cx),
            HostMsg::ClientClosed => break,
        }
        state.sync_terminal_handles();
        let active = state
            .active_session_id()
            .and_then(|id| state.session_status_snapshot(id));
        if last_active.as_ref() != Some(&active) {
            active_seq = active_seq.wrapping_add(1);
            last_active = Some(active.clone());
            cx.emit(HostEvent::Domain(EventEnvelope {
                topic: Topic::ActiveSession,
                seq: active_seq,
                event: ServerEvent::ActiveSessionReplaced(active),
            }));
        }
    }
}

fn handle_client_message(
    state: &mut AppState,
    cx: &mut HostCx,
    message: ClientMessage,
    import_progress: &async_channel::Sender<LocalImportProgress>,
) {
    let ClientMessage { id, payload } = message;
    match payload {
        ClientPayload::Command(command) => {
            let outcome = dispatch_command(state, cx, id, command, import_progress);
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
            if let Some(snapshot) = state.subscription_snapshot(&subscription.topic) {
                cx.emit(HostEvent::Domain(snapshot));
            }
            cx.send_message(HostMessage::Ack {
                id,
                result: Ok(CommandResponse::Unit),
            });
        }
        ClientPayload::Unsubscribe(_) => {
            cx.send_message(HostMessage::Ack {
                id,
                result: Ok(CommandResponse::Unit),
            });
        }
        ClientPayload::Hello(hello) => {
            cx.send_message(HostMessage::HelloAck(HelloAck {
                protocol_version: tcode_protocol::PROTOCOL_VERSION,
                app_version: env!("CARGO_PKG_VERSION").into(),
                capabilities: hello.capabilities,
            }));
        }
        _ => {
            cx.send_message(HostMessage::Ack {
                id,
                result: Err(ProtocolError {
                    code: "unsupported_message".into(),
                    message: "client message is not supported by this host".into(),
                }),
            });
        }
    }
}

enum CommandOutcome {
    Immediate(Result<CommandResponse, ProtocolError>),
    StoreBarrier(async_channel::Receiver<()>),
}

fn dispatch_command(
    app: &mut AppState,
    cx: &mut HostCx,
    request_id: u64,
    command: Command,
    import_progress: &async_channel::Sender<LocalImportProgress>,
) -> CommandOutcome {
    let mut response = CommandResponse::Unit;
    match command {
        Command::CreateSession {
            provider,
            cwd,
            model,
            project_id,
            acp_agent_id,
            profile_id,
        } => app.create_session(
            provider,
            cwd,
            model,
            project_id,
            acp_agent_id,
            profile_id,
            cx,
        ),
        Command::ApplyPendingRelaunch => {
            response = CommandResponse::PendingRelaunchSection(app.apply_pending_relaunch(cx));
        }
        Command::OpenLatestSession => app.open_latest_session(cx),
        Command::OpenTerminalPanel => app.open_terminal_panel(cx),
        Command::ShutdownAllAndFlush => {
            app.shutdown_all(cx);
            return CommandOutcome::StoreBarrier(app.store_write_barrier(cx));
        }
        Command::OrchestrateTurn {
            text,
            attachment_paths,
        } => app.orchestrate_turn(text, attachment_paths, cx),
        Command::ReloadProvider { provider } => app.reload_provider(provider, cx),
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
        Command::CheckProviderVersions => app.check_provider_versions(cx),
        Command::UpdateProvider { provider } => app.update_provider(provider, cx),
        Command::SetSidebarCollapsed { collapsed } => app.set_sidebar_collapsed(collapsed, cx),
        Command::RunGitAction {
            action,
            message,
            included,
            feature_branch,
        } => app.run_git_action(action, message, included, feature_branch, cx),
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
        Command::SetActiveAcpAgent { id } => app.set_active_acp_agent(&id, cx),
        Command::ResetSettings => app.reset_settings(cx),
        Command::WriteRelaunchMarker { reopen_settings } => {
            app.write_relaunch_marker(&reopen_settings)
        }
        Command::SetTerminalHeight { height } => app.set_terminal_height(height, cx),
        Command::ToggleTerminalPanel => app.toggle_terminal_panel(cx),
        Command::CloseTerminalPanel => app.close_terminal_panel(cx),
        Command::RestartTerminal => app.restart_terminal(cx),
        Command::NewTerminal => app.new_terminal(cx),
        Command::SplitTerminal { direction } => app.split_terminal(direction, cx),
        Command::ActivateTerminal { terminal_id } => app.activate_terminal(terminal_id, cx),
        Command::CloseTerminal { terminal_id } => app.close_terminal(terminal_id, cx),
        Command::CaptureTerminalSelection { terminal_id } => {
            app.capture_terminal_selection(terminal_id, cx)
        }
        Command::RemoveTerminalContext { context_id } => {
            app.remove_terminal_context(context_id, cx)
        }
        Command::AddReviewComment { comment } => app.add_review_comment(comment, cx),
        Command::RemoveReviewComment { index } => app.remove_review_comment(index, cx),
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
                let progress = import_progress.clone();
                cx.spawn_detached(async move {
                    while let Ok(update) = receiver.recv().await {
                        if progress
                            .send(LocalImportProgress::Update {
                                id: request_id,
                                update,
                            })
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    let _ = progress
                        .send(LocalImportProgress::Closed { id: request_id })
                        .await;
                });
            }
        }
        Command::FinishExternalImport { project_id } => app.finish_external_import(&project_id, cx),
        Command::ToggleProjectCollapsed { project_id } => {
            app.toggle_project_collapsed(&project_id, cx)
        }
        Command::UpdateSettings { settings } => app.update_settings(settings, cx),
        Command::ArchiveSession { session_id } => app.archive_session(&session_id, cx),
        Command::UnarchiveSession { session_id } => app.unarchive_session(&session_id, cx),
        Command::AutoArchiveSweep { project_id } => {
            response = CommandResponse::ArchivedCount(app.auto_archive_sweep(&project_id, cx));
        }
        Command::RenameSession { session_id, title } => app.rename_session(&session_id, &title, cx),
        Command::ForkThread { id } => app.fork_thread(&id, cx),
        Command::DeleteSession {
            session_id,
            remove_worktree,
        } => app.delete_session(&session_id, remove_worktree, cx),
        Command::DeleteProject { project_id } => app.delete_project(&project_id, cx),
        Command::MarkSessionUnread { session_id } => app.mark_session_unread(&session_id, cx),
        Command::StartDraft { project_id, cwd } => app.start_draft(project_id, cwd, cx),
        Command::SetDraftWorkspace { mode } => app.set_draft_workspace(mode, cx),
        Command::SelectSession { session_id } => app.select_session(&session_id, cx),
        Command::SendTurn {
            text,
            attachment_paths,
        } => app.send_turn(text, attachment_paths, cx),
        Command::ConfirmRelayAndSend {
            text,
            attachment_paths,
        } => app.confirm_relay_and_send(text, attachment_paths, cx),
        Command::Steer {
            text,
            attachment_paths,
        } => app.steer(text, attachment_paths, cx),
        Command::SteerQueued { id } => app.steer_queued(id, cx),
        Command::DropQueued { id } => app.drop_queued(id, cx),
        Command::Interrupt => app.interrupt(cx),
        Command::RespondApproval {
            request_id,
            decision,
        } => app.respond_approval(request_id, decision, cx),
        Command::RespondUserInput {
            request_id,
            answers,
        } => app.respond_user_input(request_id, answers, cx),
        Command::SetActiveModel {
            provider,
            model,
            profile_id,
        } => app.set_active_model(provider, model, profile_id, cx),
        Command::SetActiveOption { id, value } => app.set_active_option(&id, value, cx),
        Command::SelectUltrathink => app.select_ultrathink(cx),
        Command::SetInteractionMode { mode } => app.set_interaction_mode(mode, cx),
        Command::ToggleInteractionMode => app.toggle_interaction_mode(cx),
        Command::ImplementPlan => app.implement_plan(cx),
        Command::DismissPlan => app.dismiss_plan(cx),
        Command::ImplementPlanInNewThread { title } => app.implement_plan_in_new_thread(title, cx),
        Command::CopyPlan { markdown } => app.copy_plan(markdown, cx),
        Command::SavePlanToWorkspace { markdown } => app.save_plan_to_workspace(markdown, cx),
        Command::DownloadPlan {
            markdown,
            fallback_title,
        } => app.download_plan(markdown, fallback_title, cx),
        Command::LoadBranches => app.load_branches(cx),
        Command::CheckoutBranch { branch } => app.checkout_branch(branch, cx),
        Command::SetActiveApprovalMode { mode } => app.set_active_approval_mode(mode, cx),
        Command::ToggleFavoriteModel { model } => app.toggle_favorite_model(&model, cx),
        Command::RewindTurn { turn, mode } => app.rewind_turn(turn, mode, cx),
    }
    CommandOutcome::Immediate(Ok(response))
}

fn dispatch_query(
    app: &AppState,
    cx: &HostCx,
    query: Query,
) -> crate::host::HostTask<Result<QueryResponse, ProtocolError>> {
    match query {
        Query::ListActiveWorkspace => {
            let cwd = app.active.as_ref().map(|active| active.meta.cwd.clone());
            let task = app.list_workspace_at(cwd, cx);
            cx.spawn_background(async move { Ok(QueryResponse::ActiveWorkspace(task.await)) })
        }
        Query::ScanExternalHistory => {
            let task = app.scan_external_history(cx);
            cx.spawn_background(async move { Ok(QueryResponse::ExternalHistory(task.await)) })
        }
        Query::GenerateCommitMessage { included } => {
            let task = app.generate_commit_message(included, cx);
            cx.spawn_background(async move {
                task.await
                    .map(QueryResponse::CommitMessage)
                    .map_err(|message| ProtocolError {
                        code: "commit_message_failed".into(),
                        message,
                    })
            })
        }
        Query::SecretPresence { profile_id, name } => {
            let present = app.profile_secret_present(&profile_id, &name);
            cx.spawn_background(async move { Ok(QueryResponse::SecretPresence(present)) })
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
            let task = unblock_host(cx, move || {
                tcode_services::git::load_git_diff(&cwd, scope, base.as_deref(), ignore_whitespace)
            });
            cx.spawn_background(async move { Ok(QueryResponse::GitDiff(task.await)) })
        }
        Query::ReadFileBytes { path } => {
            let task = unblock_host(cx, move || tcode_services::user_files::read_bytes(&path));
            cx.spawn_background(async move {
                task.await
                    .map(QueryResponse::FileBytes)
                    .map_err(io_protocol_error)
            })
        }
        Query::SaveAttachment { dir, bytes, ext } => {
            let task = unblock_host(cx, move || {
                AppState::save_attachment_to_dir(&dir, &bytes, &ext)
            });
            cx.spawn_background(async move {
                task.await
                    .map(QueryResponse::SavedAttachment)
                    .map_err(io_protocol_error)
            })
        }
        Query::RemoveUserFile { path } => {
            let task = unblock_host(cx, move || tcode_services::user_files::remove_file(&path));
            cx.spawn_background(async move {
                task.await
                    .map(|()| QueryResponse::UserFileRemoved)
                    .map_err(io_protocol_error)
            })
        }
        Query::IsDirectory { path } => {
            let task = unblock_host(cx, move || tcode_services::user_files::is_directory(&path));
            cx.spawn_background(async move { Ok(QueryResponse::IsDirectory(task.await)) })
        }
        Query::RelativizeToWorkspace { path, cwd } => cx.spawn_background(async move {
            Ok(QueryResponse::RelativePath(
                tcode_services::user_files::relativize_to_workspace(&path, &cwd),
            ))
        }),
        _ => cx.spawn_background(async {
            Err(ProtocolError {
                code: "unsupported_query".into(),
                message: "query is not supported by this host".into(),
            })
        }),
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

    fn next_event(
        events: &async_channel::Receiver<String>,
        ready: impl Fn(&EventEnvelope) -> bool,
    ) -> EventEnvelope {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match events.try_recv() {
                Ok(line) => {
                    assert!(line.ends_with('\n'), "host transport must emit NDJSON");
                    if let HostMessage::Event(envelope) =
                        decode_host_line(&line).expect("decode host NDJSON")
                        && ready(&envelope)
                    {
                        return envelope;
                    }
                }
                Err(async_channel::TryRecvError::Empty) => {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(async_channel::TryRecvError::Closed) => {
                    panic!("host event stream closed before the expected event")
                }
            }
        }
        panic!("timed out waiting for serialized host event");
    }

    #[test]
    fn dedicated_host_round_trips_commands_queries_events_and_quit_barrier() {
        let data_root =
            std::env::temp_dir().join(format!("tcode-host-pipe-test-{}", uuid::Uuid::new_v4()));
        let project_root = data_root.join("project");
        std::fs::create_dir_all(&project_root).expect("create project root");
        let store = SessionStore::open_at(data_root.clone()).expect("open session store");
        let host = spawn_host(store, HostServices::default()).expect("spawn host");
        let events = host.event_receiver();

        host.subscribe(Subscription {
            topic: Topic::Index,
            after_seq: None,
        })
        .expect("serialize subscription");
        let snapshot = next_event(&events, |event| {
            event.topic == Topic::Index && matches!(&event.event, ServerEvent::IndexSnapshot(_))
        });
        assert!(matches!(
            snapshot.event,
            ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                ref sessions,
                ref projects,
            }) if sessions.is_empty() && projects.is_empty()
        ));

        let project_id = match host
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
        assert!(matches!(
            upsert.event,
            ServerEvent::IndexUpsertProject(project) if project.root == project_root
        ));

        let import_progress =
            smol::block_on(host.start_external_import(project_id.clone(), Vec::new()))
                .expect("start import over serialized command")
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
            smol::block_on(host.start_external_import("missing".into(), Vec::new()))
                .expect("unknown import command response")
                .is_none()
        );

        assert_eq!(
            smol::block_on(host.query(Query::IsDirectory {
                path: project_root.clone(),
            }))
            .expect("query directory over pipe"),
            QueryResponse::IsDirectory(true)
        );
        assert_eq!(
            smol::block_on(host.query(Query::ListActiveWorkspace))
                .expect("query inactive workspace over pipe"),
            QueryResponse::ActiveWorkspace(Vec::new())
        );

        host.shutdown_blocking()
            .expect("drain quit barrier and stop host");
        drop(host);
        std::fs::remove_dir_all(data_root).expect("remove test data");
    }
}
