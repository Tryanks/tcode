//! The Agent Client Protocol session shared by every ACP-speaking client: the
//! registry client (`acp.rs`) and the native Cursor, Grok and Antigravity
//! clients.
//!
//! One child process per session, JSON-RPC over its stdio, driven by an
//! `agent_client_protocol::Client` connection on a dedicated thread running a
//! `LocalExecutor`. This module owns the protocol machinery: child and
//! connection lifetime, stderr and EOF handling, prompt delivery
//! acknowledgement, permission settlement, the standard fs/terminal client
//! services, MCP and option mapping, the standard `session/update` mapping and
//! cancellation. What an agent family does differently is its [`Dialect`].

use crate::write_journal::WriteJournal;
use crate::{RewindFailure, RewindFailureReason};
use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, Ordering},
};
use std::task::{Context, Poll};
use std::time::Duration;

use agent_client_protocol::{self as sdk, schema::ProtocolVersion, v1::schema as acp};
use serde::Deserialize as _;
use serde_json::{Map, Value, json};
use smol::channel::{Receiver, Sender};
use smol::future;
use smol::io::{AsyncRead, AsyncReadExt as _, AsyncWrite};
use smol::prelude::*;

use crate::{
    AgentError, AgentEvent, ApprovalDecision, ApprovalKind, ApprovalOption, ApprovalOptionKind,
    ApprovalRequest, Attachment, DeltaKind, FileChange, FileChangeKind, ItemContent, ItemStatus,
    McpRegistration, OptionDescriptor, OptionSelection, PlanStep, PlanStepStatus, ProviderCommand,
    ProviderCommandKind, ProviderKind, ResumeCursor, SelectOption, SessionCommand, SessionHandle,
    SessionOptions, ThreadItem, TokenUsage, ToolFamily, ToolName, ToolOutput, TurnStatus,
    UserInputDelivery, UserInputQuestion,
};

/// Option-descriptor ids. An ACP agent's own modes, models and config options
/// reach the composer as option descriptors, so each one needs a stable id
/// that routes back to the right ACP method.
pub(crate) use crate::ACP_MODE_OPTION_ID as MODE_OPTION_ID;
pub(crate) use crate::ACP_MODEL_OPTION_ID as MODEL_OPTION_ID;
use crate::EFFORT_OPTION_ID;
const CONFIG_OPTION_PREFIX: &str = "acp:cfg:";

/// The canonical mode option id for `provider`. Antigravity exposes its live
/// permission modes under `"mode"` (`crate::permission_control`), while generic
/// ACP agents and other dialects use [`MODE_OPTION_ID`].
fn mode_option_id(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Antigravity => "mode",
        _ => MODE_OPTION_ID,
    }
}

/// The descriptor id of the session config option `config_id` in `category`.
fn config_option_id(
    provider: ProviderKind,
    config_id: &str,
    category: Option<&acp::SessionConfigOptionCategory>,
) -> String {
    match category {
        Some(acp::SessionConfigOptionCategory::Mode) => mode_option_id(provider).to_string(),
        Some(acp::SessionConfigOptionCategory::Model) => MODEL_OPTION_ID.to_string(),
        Some(acp::SessionConfigOptionCategory::ThoughtLevel) => EFFORT_OPTION_ID.to_string(),
        _ => format!("{CONFIG_OPTION_PREFIX}{config_id}"),
    }
}

/// What a session config option of `category` is on `provider`: a thought
/// level is the model's reasoning effort, and a mode is what runs without
/// asking when it is the provider's permission control.
fn config_option_role(
    provider: ProviderKind,
    category: Option<&acp::SessionConfigOptionCategory>,
) -> crate::OptionRole {
    match category {
        Some(acp::SessionConfigOptionCategory::ThoughtLevel) => crate::OptionRole::Effort,
        Some(acp::SessionConfigOptionCategory::Mode)
            if provider == ProviderKind::Antigravity
                || crate::permission_control(provider).is_none() =>
        {
            crate::OptionRole::Permission
        }
        _ => crate::OptionRole::Model,
    }
}

/// The persisted string selection for the session config option `config_id` in
/// `category`, as [`SessionOptions::option_selections`] carries it into a new
/// process.
pub(crate) fn config_selection<'a>(
    selections: &'a [OptionSelection],
    config_id: &str,
    category: Option<&acp::SessionConfigOptionCategory>,
) -> Option<&'a str> {
    config_selection_value(ProviderKind::Acp, selections, config_id, category)
        .and_then(Value::as_str)
}

/// The persisted JSON value for the session config option `config_id` in
/// `category`, covering both select and boolean config options.
fn config_selection_value<'a>(
    provider: ProviderKind,
    selections: &'a [OptionSelection],
    config_id: &str,
    category: Option<&acp::SessionConfigOptionCategory>,
) -> Option<&'a Value> {
    let id = config_option_id(provider, config_id, category);
    selections
        .iter()
        .find(|selection| selection.id == id)
        .map(|selection| &selection.value)
}

/// How long a [`query`], which no one can stop, may take. Generous: an `npm
/// exec` recipe may have to fetch the package first, and an agent that never
/// answers `initialize` (cline 3.0.39) would hold its catalog forever.
const QUERY_TIMEOUT: Duration = Duration::from_secs(120);

pub(crate) type Connection = sdk::v1::ConnectionTo<sdk::Agent>;

trait MutexExt<T> {
    fn lock_recover(&self) -> MutexGuard<'_, T>;
}

impl<T> MutexExt<T> for Mutex<T> {
    fn lock_recover(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

macro_rules! request_handler {
    ($client:expr, $request:ty, $method:ident) => {{
        let client = $client.clone();
        async move |args: $request, responder, connection| {
            let client = client.clone();
            connection
                .spawn(async move { responder.respond_with_result(client.$method(args).await) })?;
            Ok(())
        }
    }};
    ($client:expr, $request:ty, $method:ident, connection) => {{
        let client = $client.clone();
        async move |args: $request, responder, connection| {
            let client = client.clone();
            let task_connection = connection.clone();
            connection.spawn(async move {
                responder.respond_with_result(client.$method(args, &task_connection).await)
            })?;
            Ok(())
        }
    }};
}

/// What one ACP agent family does on top of the shared protocol machinery.
///
pub(crate) trait Dialect: Send + Sync + 'static {
    /// The agent's name in messages and logs.
    fn name(&self) -> &str;

    /// The process to spawn and how the client introduces itself to it.
    fn launch(&self, opts: &SessionOptions) -> Result<Launch, AgentError>;

    /// Authenticate when needed, then create, load, resume or fork the session
    /// and apply its initial mode. Runs after `initialize` succeeded.
    async fn establish(&self, setup: &Setup<'_>) -> Result<Established, AgentError>;

    /// One `session/update`, including whether a replayed update is dropped.
    /// [`State::apply_update`] is the standard mapping.
    fn session_update(
        &self,
        state: &mut State,
        notification: acp::SessionNotification,
    ) -> Vec<AgentEvent>;

    /// How the `session/prompt` result ends the turn, with its usage.
    fn turn_end(
        &self,
        state: &mut State,
        result: Result<acp::PromptResponse, sdk::Error>,
    ) -> TurnEnd;

    /// Deliver [`SessionCommand::Steer`] into the running turn.
    async fn steer(
        &self,
        session: &Session,
        session_id: &acp::SessionId,
        request_id: String,
        text: String,
        attachments: Vec<Attachment>,
    );

    /// Whether an agent→client request outside the standard set belongs to
    /// this dialect, by its literal method name.
    fn handles_request(&self, _method: &str) -> bool {
        false
    }

    /// Answer a request [`Self::handles_request`] claimed, with its raw params.
    /// It runs as its own task, so it may wait for the user without holding
    /// up the connection.
    fn request(
        &self,
        _session: Session,
        method: String,
        _params: Value,
    ) -> impl Future<Output = Result<Value, sdk::Error>> + Send {
        async move { Err(sdk::Error::method_not_found().data(method)) }
    }

    /// An agent→client notification other than a `session/update` the schema
    /// knows (a vendor `sessionUpdate` kind arrives here as `session/update`),
    /// by its literal method name with raw params. Runs in arrival order with
    /// the session updates.
    fn notification(&self, _state: &mut State, _method: &str, _params: Value) -> Vec<AgentEvent> {
        Vec::new()
    }

    /// Answer [`SessionCommand::Rewind`], [`SessionCommand::PreviewRewind`]
    /// or [`SessionCommand::UndoRewind`] with the agent's own rewind. It runs
    /// as its own task, so it may wait for an interrupted turn to end. ACP has
    /// no rewind of its own: by default the command is refused.
    fn rewind(
        self: Arc<Self>,
        _session: Session,
        _session_id: acp::SessionId,
        command: SessionCommand,
    ) -> impl Future<Output = Option<AgentEvent>> + Send {
        async move {
            command.rewind_refusal(RewindFailure::new(
                RewindFailureReason::Unsupported,
                "this ACP agent does not advertise a native rewind operation".to_owned(),
            ))
        }
    }

    /// Whether `initialize` offers the client's `fs/*` and `terminal/*`
    /// services. An agent offered them runs its file and shell tools through
    /// tcode instead of its own implementation.
    fn client_services(&self) -> bool {
        true
    }

    /// Whether this agent accepts loopback HTTP `mcpServers` in `session/new`,
    /// `session/load` and `session/resume`. By default follows
    /// `mcpCapabilities.http` (plus known servers that omit the flag).
    fn accepts_http_mcp(&self, init: &acp::InitializeResponse) -> bool {
        init.agent_capabilities.mcp_capabilities.http || accepts_undeclared_http_mcp(init)
    }

    /// Session config options that tcode already controls elsewhere (the
    /// composer's model picker), so they are neither surfaced again in
    /// [`AgentEvent::ProviderOptions`] nor restored from the thread's option
    /// selections. Each is named by its `configId` or by its option id, which
    /// names an agent's model option whatever its `configId` is.
    fn owned_config_options(&self) -> &'static [&'static str] {
        &[]
    }

    /// Called once when the session's command loop ends, before
    /// [`AgentEvent::SessionClosed`], so a dialect can stop any background
    /// tasks it started in [`Self::establish`].
    fn on_close(&self) {}
}

/// Whether the config option `config_id`, whose option id is `option_id`, is
/// one of the dialect's [`Dialect::owned_config_options`].
fn owned(owned: &[&str], config_id: &str, option_id: &str) -> bool {
    owned.contains(&config_id) || owned.contains(&option_id)
}

/// The process a dialect runs and its `initialize` introduction.
pub(crate) struct Launch {
    pub(crate) program: PathBuf,
    /// The complete argument list, including the user's launch arguments.
    pub(crate) args: Vec<String>,
    /// Applied before the profile's configured environment, which wins.
    pub(crate) env: Vec<(String, String)>,
    /// `clientCapabilities._meta` sent with `initialize`.
    pub(crate) client_meta: Option<acp::Meta>,
}

/// The session a dialect settled on.
pub(crate) struct Established {
    pub(crate) session_id: acp::SessionId,
    pub(crate) resume: ResumeCursor,
    /// The session asked for could not be resumed and this is a new one
    /// ([`SessionHandle::started_anew`]).
    pub(crate) anew: bool,
}

/// The canonical end of one prompt turn.
pub(crate) struct TurnEnd {
    pub(crate) status: TurnStatus,
    /// Surfaced as a non-fatal [`AgentEvent::Error`] before the completion.
    pub(crate) message: Option<String>,
    pub(crate) usage: Option<TokenUsage>,
}

/// The modes and config options an earlier session came back with.
pub(crate) type Reattached = (
    Option<acp::SessionModeState>,
    Option<Vec<acp::SessionConfigOption>>,
);

/// What [`Dialect::establish`] works with.
pub(crate) struct Setup<'a> {
    pub(crate) connection: &'a Connection,
    pub(crate) opts: &'a SessionOptions,
    pub(crate) init: &'a acp::InitializeResponse,
    name: &'a str,
    accepts_http_mcp: bool,
    pub(crate) state: &'a Arc<Mutex<State>>,
    pub(crate) events: &'a Sender<AgentEvent>,
}

impl Setup<'_> {
    pub(crate) async fn warn(&self, message: String) {
        let _ = self.events.send(AgentEvent::Warning { message }).await;
    }

    /// The `mcpServers` for `session/new`, `load` or `resume`. tcode's MCP
    /// servers are loopback streamable-HTTP endpoints, so they are only
    /// offered to agents that speak MCP over HTTP; the instructions go with
    /// them when the provider delivers instructions per prompt.
    pub(crate) async fn mcp_servers(&self) -> Vec<acp::McpServer> {
        let registrations: Vec<_> = self.opts.mcp_servers.iter().collect();
        let servers = if self.accepts_http_mcp {
            http_mcp_servers(&registrations)
        } else {
            Vec::new()
        };
        {
            let mut state = self.state.lock_recover();
            state.instructions = if !servers.is_empty()
                && state.options.provider.caps().instruction_channel
                    == crate::InstructionChannel::PerPromptUserBlock
            {
                self.opts.instructions.clone()
            } else {
                None
            };
        }
        if !registrations.is_empty() && servers.is_empty() {
            let _ = self.events.send(AgentEvent::McpServersNotAttached).await;
            log::info!(
                "acp[{}]: no mcpCapabilities.http; Tcode MCP servers are not registered",
                self.name
            );
            self.warn(format!(
                "{} does not support HTTP MCP servers; Tcode MCP tools are unavailable in this session",
                self.name
            ))
            .await;
        }
        servers
    }

    /// `session/load`, with [`State::loading`] set until it answers: an agent
    /// may replay the conversation as `session/update`s before it responds.
    pub(crate) async fn load(
        &self,
        request: acp::LoadSessionRequest,
    ) -> Result<acp::LoadSessionResponse, sdk::Error> {
        self.state.lock_recover().loading = true;
        let (loaded_tx, loaded) = smol::channel::bounded(1);
        let state = self.state.clone();
        // Cleared while the response holds the dispatch loop: an update the
        // agent sends right after answering is live, and with `block_task`
        // it could be read before the flag clears and dropped as replay.
        let registered =
            self.connection
                .send_request(request)
                .on_receiving_result(move |result| async move {
                    state.lock_recover().loading = false;
                    let _ = loaded_tx.send(result).await;
                    Ok(())
                });
        let loaded = match registered {
            Ok(()) => loaded.recv().await.unwrap_or_else(|_| {
                Err(sdk::Error::internal_error().data("the agent closed before answering"))
            }),
            Err(err) => Err(err),
        };
        self.state.lock_recover().loading = false;
        loaded
    }

    /// Make the earlier session `session_id` live again: `session/resume`
    /// where the agent advertises it, which replays nothing, and
    /// `session/load` where it does not or where it refused the resume.
    /// `None` when the agent advertises neither; an error is the last
    /// request's.
    pub(crate) async fn reattach(
        &self,
        session_id: &acp::SessionId,
        mcp_servers: &[acp::McpServer],
        meta: Option<&acp::Meta>,
    ) -> Option<Result<Reattached, sdk::Error>> {
        let capabilities = &self.init.agent_capabilities;
        let mut refused = None;
        if capabilities.session_capabilities.resume.is_some() {
            let mut request =
                acp::ResumeSessionRequest::new(session_id.clone(), self.opts.cwd.clone())
                    .mcp_servers(mcp_servers.to_vec());
            if let Some(meta) = meta {
                request = request.meta(meta.clone());
            }
            match self.connection.send_request(request).block_task().await {
                Ok(resumed) => return Some(Ok((resumed.modes, resumed.config_options))),
                Err(err) => refused = Some(err),
            }
        }
        if !capabilities.load_session {
            return refused.map(Err);
        }
        if let Some(err) = &refused {
            log::warn!(
                "acp[{}]: session/resume failed ({}); trying session/load",
                self.name,
                describe(err)
            );
        }
        let mut request = acp::LoadSessionRequest::new(session_id.clone(), self.opts.cwd.clone())
            .mcp_servers(mcp_servers.to_vec());
        if let Some(meta) = meta {
            request = request.meta(meta.clone());
        }
        Some(
            self.load(request)
                .await
                .map(|loaded| (loaded.modes, loaded.config_options)),
        )
    }

    /// Take the modes and config options the session started with.
    pub(crate) fn adopt(
        &self,
        modes: Option<&acp::SessionModeState>,
        config_options: Option<&[acp::SessionConfigOption]>,
    ) {
        let mut state = self.state.lock_recover();
        state.ingest_modes(modes);
        state.options.ingest(None, config_options);
    }

    fn select_mode(&self, mode: acp::SessionModeId) {
        self.state.lock_recover().select_mode(mode);
    }

    /// Apply the composer's selected model when the session exposes a model
    /// config option and is not already on it. A rejected model warns and
    /// carries on so a stale or removed model does not block starting or
    /// resuming a thread.
    pub(crate) async fn select_model(
        &self,
        session_id: &acp::SessionId,
        model: &str,
        config_options: &[acp::SessionConfigOption],
    ) -> Option<Vec<acp::SessionConfigOption>> {
        let option = model_config_option(config_options)?;
        let acp::SessionConfigKind::Select(select) = &option.kind else {
            return None;
        };
        if select.current_value.0.as_ref() == model {
            return None;
        }
        match self
            .connection
            .send_request(acp::SetSessionConfigOptionRequest::new(
                session_id.clone(),
                option.id.clone(),
                acp::SessionConfigOptionValue::value_id(acp::SessionConfigValueId::new(model)),
            ))
            .block_task()
            .await
        {
            Ok(response) => {
                self.adopt(None, Some(&response.config_options));
                Some(response.config_options)
            }
            Err(err) => {
                self.warn(format!(
                    "{} did not accept the model `{model}`: {}",
                    self.name,
                    describe(&err)
                ))
                .await;
                None
            }
        }
    }

    /// Re-apply the session options selected before launch (model, mode,
    /// effort, or agent-defined config options), except those the dialect
    /// owns.
    pub(crate) async fn restore_options(
        &self,
        session_id: &acp::SessionId,
        modes: Option<&acp::SessionModeState>,
        config_options: &[acp::SessionConfigOption],
    ) {
        let (provider, owned_options) = {
            let state = self.state.lock_recover();
            (state.options.provider, state.owned_options)
        };
        let has_mode_config = config_options
            .iter()
            .any(|option| option.category == Some(acp::SessionConfigOptionCategory::Mode));
        for option in config_options {
            if owned(
                owned_options,
                &option.id.0,
                &config_option_id(provider, &option.id.0, option.category.as_ref()),
            ) {
                continue;
            }
            let Some(chosen) = config_selection_value(
                provider,
                &self.opts.option_selections,
                option.id.0.as_ref(),
                option.category.as_ref(),
            ) else {
                continue;
            };
            let option_id = config_option_id(provider, &option.id.0, option.category.as_ref());
            if self
                .state
                .lock_recover()
                .options
                .value_requires_restart(&option_id, chosen)
            {
                continue;
            }
            let wire_value = match (&option.kind, chosen) {
                (acp::SessionConfigKind::Select(select), Value::String(value))
                    if select.current_value.0.as_ref() != value.as_str() =>
                {
                    acp::SessionConfigOptionValue::value_id(acp::SessionConfigValueId::new(
                        value.as_str(),
                    ))
                }
                (acp::SessionConfigKind::Boolean(boolean), Value::Bool(value))
                    if boolean.current_value != *value =>
                {
                    acp::SessionConfigOptionValue::boolean(*value)
                }
                _ => continue,
            };
            match self
                .connection
                .send_request(acp::SetSessionConfigOptionRequest::new(
                    session_id.clone(),
                    option.id.clone(),
                    wire_value,
                ))
                .block_task()
                .await
            {
                Ok(response) => self.adopt(None, Some(&response.config_options)),
                Err(err) => {
                    self.warn(format!(
                        "{} did not restore `{}`: {}",
                        self.name,
                        option.name,
                        describe(&err)
                    ))
                    .await;
                }
            }
        }
        if !has_mode_config
            && let Some(modes) = modes
            && let Some(chosen) = self
                .opts
                .option_selections
                .iter()
                .find(|selection| selection.id == mode_option_id(provider))
                .and_then(|selection| selection.value.as_str())
            && modes.current_mode_id.0.as_ref() != chosen
        {
            match self
                .connection
                .send_request(acp::SetSessionModeRequest::new(
                    session_id.clone(),
                    acp::SessionModeId::new(chosen),
                ))
                .block_task()
                .await
            {
                Ok(_) => self.select_mode(acp::SessionModeId::new(chosen)),
                Err(err) => {
                    self.warn(format!(
                        "{} did not restore mode `{chosen}`: {}",
                        self.name,
                        describe(&err)
                    ))
                    .await;
                }
            }
        }
    }
}

fn model_config_option(
    config_options: &[acp::SessionConfigOption],
) -> Option<&acp::SessionConfigOption> {
    config_options
        .iter()
        .find(|option| option.category == Some(acp::SessionConfigOptionCategory::Model))
        .or_else(|| {
            config_options
                .iter()
                .find(|option| option.id.0.as_ref() == "model")
        })
}

/// A live session as dialect hooks see it.
#[derive(Clone)]
pub(crate) struct Session {
    pub(crate) connection: Connection,
    events: Sender<AgentEvent>,
    state: Arc<Mutex<State>>,
    turn_tx: Sender<TurnOutcome>,
}

impl Session {
    pub(crate) async fn emit(&self, event: AgentEvent) {
        let _ = self.events.send(event).await;
    }

    pub(crate) fn with_state<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut self.state.lock_recover())
    }

    /// Steer the running turn by issuing a replacement `session/prompt` on
    /// `session_id` (for agents such as `agy acp` that supersede the earlier
    /// in-flight prompt request and keep streaming until the new one finishes).
    /// Returns `false` when no turn is currently running.
    pub(crate) fn steer_prompt(
        &self,
        session_id: &acp::SessionId,
        text: &str,
        attachments: &[Attachment],
    ) -> bool {
        let Some((turn_id, generation)) = self.with_state(|state| {
            let turn_id = state.turn.clone()?;
            state.prompt_generation += 1;
            Some((turn_id, state.prompt_generation))
        }) else {
            return false;
        };
        let request =
            acp::PromptRequest::new(session_id.clone(), prompt_blocks(text, attachments, None));
        let pending = self.connection.send_request(request);
        let turn_tx = self.turn_tx.clone();
        self.connection
            .spawn(async move {
                let result = pending.block_task().await;
                let _ = turn_tx
                    .send(TurnOutcome {
                        turn_id,
                        generation,
                        result,
                    })
                    .await;
                Ok(())
            })
            .is_ok()
    }

    /// Cancel the agent's work as [`SessionCommand::Interrupt`] does, whether
    /// or not tcode has a turn open: the agent may be running one tcode has
    /// not seen start. A sent turn ends when the agent answers its
    /// `session/prompt`.
    pub(crate) async fn interrupt(&self, session_id: &acp::SessionId) {
        // Every in-flight permission request must be answered with
        // `cancelled` first: the protocol requires it, and the agent will
        // not settle the turn otherwise.
        cancel_pending(&self.state, &self.events).await;
        let _ = self
            .connection
            .send_notification(acp::CancelNotification::new(session_id.clone()));
    }

    /// Ask for a decision and wait for [`SessionCommand::RespondApproval`]; an
    /// interrupt or shutdown decides [`ApprovalDecision::Cancel`].
    pub(crate) async fn ask_approval(
        &self,
        kind: ApprovalKind,
        options: Vec<ApprovalOption>,
    ) -> ApprovalDecision {
        let (responder, decided) = smol::channel::bounded(1);
        let (id, turn_id) = self.with_state(|state| {
            state.approval_seq += 1;
            let id = format!("acp-approval-{}", state.approval_seq);
            state.approvals.insert(id.clone(), responder);
            (id, state.turn().map(str::to_owned))
        });
        self.emit(AgentEvent::ApprovalRequested(ApprovalRequest {
            id: id.clone(),
            turn_id,
            parent_item_id: None,
            kind,
            reason: None,
            blocked_path: None,
            options,
        }))
        .await;
        let decision = decided.recv().await.unwrap_or(ApprovalDecision::Cancel);
        self.with_state(|state| state.approvals.remove(&id));
        decision
    }

    /// Show the plan a dialect's plan request carries in the timeline, then
    /// ask for its decision among the dialect's own `options`. `params` holds
    /// the plan under `field` and the agent's `toolCallId`.
    pub(crate) async fn ask_plan<const N: usize>(
        &self,
        params: &Value,
        field: &str,
        options: [(&str, &str, ApprovalOptionKind); N],
    ) -> ApprovalDecision {
        let markdown = params
            .get(field)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let item_id = match params.get("toolCallId").and_then(Value::as_str) {
            Some(id) => format!("plan:{id}"),
            None => format!("plan:{}", uuid::Uuid::new_v4()),
        };
        self.emit(AgentEvent::ItemCompleted(ThreadItem {
            id: item_id.clone(),
            parent_item_id: None,
            content: ItemContent::Plan {
                markdown: markdown.clone(),
            },
        }))
        .await;
        let options = options
            .into_iter()
            .map(|(id, label, kind)| ApprovalOption {
                id: id.into(),
                label: label.into(),
                kind,
            })
            .collect();
        self.ask_approval(ApprovalKind::Plan { item_id, markdown }, options)
            .await
    }

    /// Ask the user and wait for [`SessionCommand::RespondUserInput`]. `None`
    /// when the request was cancelled by an interrupt or shutdown.
    pub(crate) async fn ask_user(
        &self,
        questions: Vec<UserInputQuestion>,
    ) -> Option<Map<String, Value>> {
        let (answer, answered) = smol::channel::bounded(1);
        let request_id = self.with_state(|state| {
            state.input_seq += 1;
            let request_id = format!("acp-input-{}", state.input_seq);
            state.inputs.insert(request_id.clone(), answer);
            request_id
        });
        self.emit(AgentEvent::UserInputRequested {
            request_id,
            questions,
            delivery: UserInputDelivery::Blocking,
        })
        .await;
        answered.recv().await.ok()
    }
}

/// Start (or resume) a session with an ACP agent speaking `dialect`.
pub(crate) async fn start<D: Dialect>(
    provider: ProviderKind,
    dialect: D,
    opts: SessionOptions,
) -> Result<SessionHandle, AgentError> {
    let (commands_tx, commands_rx) = smol::channel::unbounded();
    let (events_tx, events_rx) = smol::channel::unbounded();
    let (ready_tx, ready_rx) = smol::channel::bounded(1);
    let dialect = Arc::new(dialect);

    // Keep each ACP connection and all of its callbacks on one dedicated
    // executor thread. The SDK requires Send handlers internally, but the
    // adapter still exposes only channels across this boundary.
    std::thread::Builder::new()
        .name("acp-session".into())
        .spawn(move || {
            let executor = Rc::new(smol::LocalExecutor::new());
            let task = run_actor(
                executor.clone(),
                provider,
                dialect,
                opts,
                commands_rx,
                events_tx,
                ready_tx,
            );
            smol::block_on(executor.run(task));
        })
        .map_err(|err| {
            AgentError::Spawn(format!("could not start the ACP session thread: {err}"))
        })?;

    let started_anew = ready_rx.recv().await.map_err(|_| {
        AgentError::Protocol("ACP actor exited before reporting startup status".into())
    })??;

    Ok(SessionHandle {
        provider,
        commands: commands_tx,
        events: events_rx,
        started_anew,
    })
}

/// Run `query` against a freshly started and initialized agent outside any
/// session, then tear the agent down. Model catalogs use it.
pub(crate) async fn query<D, R>(
    provider: ProviderKind,
    dialect: D,
    opts: SessionOptions,
    query: impl AsyncFnOnce(&Connection, &acp::InitializeResponse) -> Result<R, AgentError>
    + Send
    + 'static,
) -> Result<R, AgentError>
where
    D: Dialect,
    R: Send + 'static,
{
    let (result_tx, result) = smol::channel::bounded(1);
    let processes = opts.processes.clone();
    std::thread::Builder::new()
        .name("acp-query".into())
        .spawn(move || {
            let outcome = smol::block_on(async move {
                let launch = dialect.launch(&opts)?;
                let name = dialect.name().to_string();
                let client_services = dialect.client_services();
                let mut child = spawn_agent(&name, provider, &launch, &opts)?;
                let (Some(stdin), Some(stdout), Some(stderr)) =
                    (child.stdin.take(), child.stdout.take(), child.stderr.take())
                else {
                    let _ = child.kill();
                    return Err(AgentError::Spawn(format!(
                        "ACP agent `{name}` started without piped stdio"
                    )));
                };
                // Drained so a chatty agent never blocks on a full stderr pipe.
                smol::spawn(async move {
                    let mut lines = smol::io::BufReader::new(stderr).lines();
                    while let Some(Ok(_)) = lines.next().await {}
                })
                .detach();
                let outcome = sdk::Client
                    .v1()
                    .name(format!("tcode-acp-query-{name}"))
                    .connect_with(sdk::ByteStreams::new(stdin, stdout), async |connection| {
                        Ok(
                            match initialize(&connection, &name, &launch, client_services).await {
                                Ok(init) => query(&connection, &init).await,
                                Err(err) => Err(err),
                            },
                        )
                    })
                    .await;
                let _ = child.kill();
                let _ = child.status().await;
                outcome.unwrap_or_else(|err| {
                    Err(AgentError::Protocol(format!(
                        "ACP transport error: {}",
                        describe(&err)
                    )))
                })
            });
            let _ = result_tx.send_blocking(outcome);
        })
        .map_err(|err| AgentError::Spawn(format!("could not start the ACP query thread: {err}")))?;
    future::or(
        async {
            result
                .recv()
                .await
                .map_err(|_| AgentError::Protocol("ACP query exited without a result".into()))?
        },
        async {
            smol::Timer::after(QUERY_TIMEOUT).await;
            // Its agent killed, the query's thread ends with it.
            processes.stop();
            Err(AgentError::Protocol(format!(
                "the agent did not answer within {} s; it may not speak ACP over stdio \
                 (check its launch arguments)",
                QUERY_TIMEOUT.as_secs()
            )))
        },
    )
    .await
}

fn spawn_agent(
    name: &str,
    provider: ProviderKind,
    launch: &Launch,
    opts: &SessionOptions,
) -> Result<crate::process::ProcessTree<smol::process::Child>, AgentError> {
    let mut cmd = crate::process::async_harness_command(&launch.program);
    cmd.args(&launch.args)
        .current_dir(&opts.cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // The dialect's env first, the user's configured env last (the user always wins).
    for (key, value) in &launch.env {
        cmd.env(key, value);
    }
    for (key, value) in opts.launch_env.pairs(provider) {
        cmd.env(key, value);
    }
    log::debug!(
        "spawning ACP agent {name}: {} {:?}",
        launch.program.display(),
        launch.args
    );
    crate::process::refuse_batch_line_breaks(cmd.get_program(), cmd.get_args())?;
    cmd.spawn()
        .map(|child| opts.processes.hold(crate::process::ProcessTree::new(child)))
        .map_err(|err| {
            AgentError::Spawn(format!(
                "could not launch ACP agent `{name}` ({}): {err}",
                launch.program.display()
            ))
        })
}

#[allow(clippy::too_many_arguments)]
async fn run_actor<D: Dialect>(
    executor: Rc<smol::LocalExecutor<'static>>,
    provider: ProviderKind,
    dialect: Arc<D>,
    opts: SessionOptions,
    commands: Receiver<SessionCommand>,
    events: Sender<AgentEvent>,
    ready: Sender<Result<bool, AgentError>>,
) {
    let launch = match dialect.launch(&opts) {
        Ok(launch) => launch,
        Err(err) => {
            let _ = ready.send(Err(err)).await;
            return;
        }
    };
    let name = dialect.name().to_string();
    let mut child = match spawn_agent(&name, provider, &launch, &opts) {
        Ok(child) => child,
        Err(err) => {
            let _ = ready.send(Err(err)).await;
            return;
        }
    };
    let Some(stdin) = child.stdin.take() else {
        let _ = ready
            .send(Err(AgentError::Spawn(format!(
                "ACP agent `{name}` started without piped stdin"
            ))))
            .await;
        return;
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = ready
            .send(Err(AgentError::Spawn(format!(
                "ACP agent `{name}` started without piped stdout"
            ))))
            .await;
        return;
    };
    let Some(stderr) = child.stderr.take() else {
        let _ = ready
            .send(Err(AgentError::Spawn(format!(
                "ACP agent `{name}` started without piped stderr"
            ))))
            .await;
        return;
    };

    // The agent's stderr is its log channel: keep the tail so a startup failure
    // can be reported in the agent's own words.
    let stderr_tail = crate::process::StderrTail::default();
    executor
        .spawn({
            let tail = stderr_tail.clone();
            let name = name.clone();
            async move {
                let mut lines = smol::io::BufReader::new(stderr).lines();
                while let Some(Ok(line)) = lines.next().await {
                    log::debug!("acp[{name}] stderr: {line}");
                    tail.push(line);
                }
            }
        })
        .detach();

    let mut state = State::new(provider, opts.cwd.clone());
    if provider.caps().relayed_history {
        state.journal = opts
            .write_journal
            .clone()
            .map(|dir| Arc::new(Mutex::new(WriteJournal::open(dir))));
    }
    state.owned_options = dialect.owned_config_options();
    if provider == ProviderKind::Antigravity
        && launch
            .args
            .iter()
            .any(|arg| arg == "--dangerously-skip-permissions")
    {
        state.options.set_launch_override(
            "mode".to_owned(),
            Value::String(crate::ANTIGRAVITY_FULL_ACCESS_MODE.to_owned()),
        );
    }
    let state = Arc::new(Mutex::new(state));
    let (turn_tx, turn_done) = smol::channel::unbounded::<TurnOutcome>();
    let client = Client {
        events: events.clone(),
        state: state.clone(),
        turn_tx: turn_tx.clone(),
    };
    let (io_done_tx, io_done) = smol::channel::bounded::<String>(1);
    let pending_deliveries = Arc::new(Mutex::new(VecDeque::new()));
    let transport = sdk::ByteStreams::new(
        ObservedWriter::new(stdin, pending_deliveries.clone(), events.clone()),
        ObservedReader::new(stdout, io_done_tx),
    );
    let session_started = Arc::new(AtomicBool::new(false));
    let connection_result = sdk::Client
        .v1()
        .name(format!("tcode-acp-{name}"))
        // Untyped, so a `sessionUpdate` kind the schema does not know reaches
        // the dialect instead of failing to parse and being dropped.
        .on_receive_notification(
            {
                let client = client.clone();
                let dialect = dialect.clone();
                async move |notification: sdk::UntypedMessage, _connection| {
                    let (method, params) = notification.into_parts();
                    let update = (method == "session/update")
                        .then(|| acp::SessionNotification::deserialize(&params).ok())
                        .flatten();
                    let events = {
                        let mut state = client.state.lock_recover();
                        match update {
                            Some(update) => dialect.session_update(&mut state, update),
                            None => dialect.notification(&mut state, &method, params),
                        }
                    };
                    client.emit_all(events).await;
                    Ok(())
                }
            },
            sdk::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let client = client.clone();
                async move |args: acp::RequestPermissionRequest, responder, connection| {
                    let client = client.clone();
                    let cancellation = responder.cancellation();
                    connection.spawn(async move {
                        let withdrawn = async move { cancellation.cancelled().await };
                        responder
                            .respond_with_result(client.request_permission(args, withdrawn).await)
                    })?;
                    Ok(())
                }
            },
            sdk::on_receive_request!(),
        )
        .on_receive_request(
            request_handler!(client, acp::ReadTextFileRequest, read_text_file),
            sdk::on_receive_request!(),
        )
        .on_receive_request(
            request_handler!(client, acp::WriteTextFileRequest, write_text_file),
            sdk::on_receive_request!(),
        )
        .on_receive_request(
            request_handler!(
                client,
                acp::CreateTerminalRequest,
                create_terminal,
                connection
            ),
            sdk::on_receive_request!(),
        )
        .on_receive_request(
            request_handler!(client, acp::TerminalOutputRequest, terminal_output),
            sdk::on_receive_request!(),
        )
        .on_receive_request(
            request_handler!(
                client,
                acp::WaitForTerminalExitRequest,
                wait_for_terminal_exit
            ),
            sdk::on_receive_request!(),
        )
        .on_receive_request(
            request_handler!(client, acp::KillTerminalRequest, kill_terminal),
            sdk::on_receive_request!(),
        )
        .on_receive_request(
            request_handler!(client, acp::ReleaseTerminalRequest, release_terminal),
            sdk::on_receive_request!(),
        )
        // Vendor methods are routed by their literal names. The schema's own
        // extension fallback only recognises `_`-prefixed methods and strips
        // the prefix, so it cannot carry Cursor's `cursor/*` methods.
        .on_receive_request(
            {
                let client = client.clone();
                let dialect = dialect.clone();
                async move |request: sdk::UntypedMessage, responder, connection| {
                    if !dialect.handles_request(&request.method) {
                        return Ok(sdk::Handled::No {
                            message: (request, responder),
                            retry: false,
                        });
                    }
                    let session = client.session(connection.clone());
                    let dialect = dialect.clone();
                    let (method, params) = request.into_parts();
                    connection.spawn(async move {
                        responder
                            .respond_with_result(dialect.request(session, method, params).await)
                    })?;
                    Ok(sdk::Handled::Yes)
                }
            },
            sdk::on_receive_request!(),
        )
        .connect_with(transport, {
            let session_started = session_started.clone();
            let stderr_tail = stderr_tail.clone();
            let actor_events = events.clone();
            let actor_ready = ready.clone();
            async move |connection| {
                connected_actor(
                    &executor,
                    provider,
                    connection,
                    &dialect,
                    &launch,
                    &opts,
                    &commands,
                    &actor_events,
                    &actor_ready,
                    &client,
                    &stderr_tail,
                    &io_done,
                    &turn_tx,
                    &turn_done,
                    &session_started,
                    &pending_deliveries,
                )
                .await
            }
        })
        .await;

    // The connection closure owns the protocol shutdown; this is the hard
    // process boundary for both graceful shutdowns and broken transports. A
    // startup failure is reported only once the agent is gone, so a caller
    // that gives up on the error leaves no process behind.
    let _ = child.kill();
    let _ = child.status().await;

    let connection_result = match connection_result {
        Ok(Exit::StartFailed(err)) => {
            let _ = ready.send(Err(err)).await;
            return;
        }
        Ok(Exit::Closed(reason)) => Ok(reason),
        Err(err) => Err(err),
    };
    if !session_started.load(Ordering::Acquire) {
        if let Err(err) = connection_result {
            let message = format!("ACP transport error: {}", describe(&err));
            let message = stderr_tail.append_to(message, "\n");
            let _ = ready.send(Err(AgentError::Protocol(message))).await;
        }
        return;
    }

    let close_reason = match connection_result {
        Ok(reason) => reason,
        Err(err) => Some(format!("ACP transport error: {}", describe(&err))),
    };
    let close_reason = close_reason.map(|reason| stderr_tail.append_to(reason, "\nstderr:\n"));
    let _ = events
        .send(AgentEvent::SessionClosed {
            reason: close_reason,
        })
        .await;
}

enum Exit {
    StartFailed(AgentError),
    /// The session ended, with a reason unless it was asked to.
    Closed(Option<String>),
}

#[allow(clippy::too_many_arguments)]
async fn connected_actor<D: Dialect>(
    executor: &Rc<smol::LocalExecutor<'static>>,
    provider: ProviderKind,
    connection: Connection,
    dialect: &Arc<D>,
    launch: &Launch,
    opts: &SessionOptions,
    commands: &Receiver<SessionCommand>,
    events: &Sender<AgentEvent>,
    ready: &Sender<Result<bool, AgentError>>,
    client: &Client,
    stderr_tail: &crate::process::StderrTail,
    io_done: &Receiver<String>,
    turn_tx: &Sender<TurnOutcome>,
    turn_done: &Receiver<TurnOutcome>,
    session_started: &AtomicBool,
    pending_deliveries: &Arc<Mutex<VecDeque<u64>>>,
) -> Result<Exit, sdk::Error> {
    enum Startup {
        Handshake(Result<(Established, bool), AgentError>),
        Io(String),
    }

    let state = &client.state;
    let startup = future::or(
        async {
            Startup::Handshake(handshake(&connection, dialect, launch, opts, state, events).await)
        },
        async {
            Startup::Io(
                io_done
                    .recv()
                    .await
                    .unwrap_or_else(|_| "the ACP agent closed its stdio".to_string()),
            )
        },
    )
    .await;
    let (established, can_close) = match startup {
        Startup::Handshake(Ok(established)) => established,
        Startup::Handshake(Err(err)) => {
            let err = match err {
                AgentError::Protocol(message) => {
                    AgentError::Protocol(stderr_tail.append_to(message, "\n"))
                }
                other => other,
            };
            return Ok(Exit::StartFailed(err));
        }
        Startup::Io(reason) => {
            let message = stderr_tail.append_to(reason, "\n");
            return Ok(Exit::StartFailed(AgentError::Protocol(message)));
        }
    };
    let session_id = established.session_id;
    let model = {
        let mut state = state.lock_recover();
        if let Some(mut descriptor) = crate::permission_control(provider) {
            let id = descriptor_id(&descriptor).to_owned();
            if state.options.origin(&id).is_none() {
                let mut value = opts
                    .option_selections
                    .iter()
                    .find(|selection| selection.id == id)
                    .map(|selection| selection.value.clone())
                    .or_else(|| match &descriptor {
                        OptionDescriptor::Select { default_value, .. } => {
                            default_value.as_ref().map(|value| json!(value))
                        }
                        OptionDescriptor::Boolean { default_value, .. } => {
                            Some(json!(default_value))
                        }
                    });
                if provider == ProviderKind::Cursor {
                    value = Some(json!("unrestricted"));
                } else if provider == ProviderKind::Grok {
                    let mut arguments = launch.args.iter();
                    while let Some(argument) = arguments.next() {
                        if argument == "--permission-mode" {
                            if let Some(mode) = arguments.next() {
                                value = Some(json!(mode));
                            }
                        } else if let Some(mode) = argument.strip_prefix("--permission-mode=") {
                            value = Some(json!(mode));
                        }
                    }
                }
                if let Some(value) = value {
                    if let (OptionDescriptor::Select { options, .. }, Some(mode)) =
                        (&mut descriptor, value.as_str())
                        && !options.iter().any(|option| option.value == mode)
                    {
                        options.push(SelectOption {
                            value: mode.to_owned(),
                            label: mode.to_owned(),
                            description: None,
                            unavailable: Some("Reported by the launch arguments".into()),
                            speed: None,
                            summary: None,
                            approval: crate::approval_class(provider, &id, mode),
                            apply: None,
                        });
                    }
                    state
                        .options
                        .upsert(descriptor, OptionOrigin::Launch, value);
                }
            }
        }
        state.options.current_model()
    };

    let _ = events
        .send(AgentEvent::SessionStarted {
            provider_session_id: session_id.0.to_string(),
            resume: established.resume,
            model,
        })
        .await;
    emit_provider_options(state, events, false).await;
    if ready.send(Ok(established.anew)).await.is_err() {
        return Ok(Exit::Closed(None));
    }
    session_started.store(true, Ordering::Release);

    let session = client.session(connection.clone());
    let mut turn_seq: u64 = 0;

    let close_reason = loop {
        enum Input {
            Command(Result<SessionCommand, smol::channel::RecvError>),
            Turn(TurnOutcome),
            Io(String),
        }
        let input = future::or(
            future::or(async { Input::Command(commands.recv().await) }, async {
                match turn_done.recv().await {
                    Ok(outcome) => Input::Turn(outcome),
                    // This loop holds the sender, so the channel cannot close.
                    Err(_) => future::pending().await,
                }
            }),
            async {
                match io_done.recv().await {
                    Ok(reason) => Input::Io(reason),
                    Err(_) => future::pending().await,
                }
            },
        )
        .await;

        match input {
            Input::Command(Ok(SessionCommand::Shutdown)) | Input::Command(Err(_)) => break None,
            Input::Command(Ok(command)) => {
                handle_command(
                    command,
                    dialect,
                    executor,
                    &session,
                    &session_id,
                    turn_tx,
                    &mut turn_seq,
                    pending_deliveries,
                )
                .await;
            }
            Input::Turn(outcome) => finish_turn(dialect, state, events, outcome).await,
            Input::Io(reason) => break Some(reason),
        }
    };

    dialect.on_close();

    // Only a live transport can acknowledge a graceful close. A dead stdout
    // has already ended the session and must not leave us awaiting a response.
    if close_reason.is_none() {
        // An agent blocked on one of our answers may not settle the close.
        cancel_pending(state, events).await;
        if can_close {
            let _ = connection
                .send_request(acp::CloseSessionRequest::new(session_id))
                .block_task()
                .await;
        }
    }

    Ok(Exit::Closed(close_reason))
}

struct TurnOutcome {
    turn_id: String,
    generation: u64,
    result: Result<acp::PromptResponse, sdk::Error>,
}

/// `initialize`, then the dialect's session establishment. Also reports
/// whether the agent can close sessions.
async fn handshake<D: Dialect>(
    connection: &Connection,
    dialect: &Arc<D>,
    launch: &Launch,
    opts: &SessionOptions,
    state: &Arc<Mutex<State>>,
    events: &Sender<AgentEvent>,
) -> Result<(Established, bool), AgentError> {
    let name = dialect.name();
    let init = initialize(connection, name, launch, dialect.client_services()).await?;
    let setup = Setup {
        connection,
        opts,
        init: &init,
        name,
        accepts_http_mcp: dialect.accepts_http_mcp(&init),
        state,
        events,
    };
    let established = dialect.establish(&setup).await?;
    let can_close = init.agent_capabilities.session_capabilities.close.is_some();
    Ok((established, can_close))
}

async fn initialize(
    connection: &Connection,
    name: &str,
    launch: &Launch,
    client_services: bool,
) -> Result<acp::InitializeResponse, AgentError> {
    let mut client_capabilities = acp::ClientCapabilities::new()
        .fs(acp::FileSystemCapabilities::new()
            .read_text_file(client_services)
            .write_text_file(client_services))
        .terminal(client_services);
    client_capabilities.meta = launch.client_meta.clone();
    // An agent that never answers (cline 3.0.39) leaves the thread starting
    // until the user stops it, which kills the agent and ends this request.
    connection
        .send_request(
            acp::InitializeRequest::new(ProtocolVersion::LATEST)
                .client_capabilities(client_capabilities)
                .client_info(
                    acp::Implementation::new("tcode", env!("CARGO_PKG_VERSION")).title("Tcode"),
                ),
        )
        .block_task()
        .await
        .map_err(|err| {
            AgentError::Protocol(format!("`{name}` failed to initialize: {}", describe(&err)))
        })
}

#[allow(clippy::too_many_arguments)]
async fn handle_command<D: Dialect>(
    command: SessionCommand,
    dialect: &Arc<D>,
    executor: &Rc<smol::LocalExecutor<'static>>,
    session: &Session,
    session_id: &acp::SessionId,
    turn_tx: &Sender<TurnOutcome>,
    turn_seq: &mut u64,
    pending_deliveries: &Arc<Mutex<VecDeque<u64>>>,
) {
    let connection = &session.connection;
    let state = &session.state;
    let events = &session.events;
    match command {
        SessionCommand::Steer {
            request_id,
            text,
            attachments,
        } => {
            dialect
                .steer(session, session_id, request_id, text, attachments)
                .await
        }
        SessionCommand::SendTurn {
            delivery_id,
            text,
            attachments,
            ..
        } => {
            *turn_seq += 1;
            let id = format!("turn-{turn_seq}");
            let (generation, instructions, checkpoint) = {
                let mut state = state.lock_recover();
                state.turn = Some(id.clone());
                state.prompt_generation += 1;
                // Tcode's own restore point, where the harness has none.
                let checkpoint = state
                    .options
                    .provider
                    .caps()
                    .relayed_history
                    .then(|| uuid::Uuid::new_v4().to_string());
                if checkpoint.is_some() {
                    state.checkpoint = checkpoint.clone();
                }
                (
                    state.prompt_generation,
                    state.instructions.clone(),
                    checkpoint,
                )
            };

            let request = acp::PromptRequest::new(
                session_id.clone(),
                prompt_blocks(&text, &attachments, instructions.as_deref()),
            );
            // The observed stdio writer consumes this id when the complete
            // `session/prompt` JSON-RPC line reaches the child.
            pending_deliveries.lock_recover().push_back(delivery_id);
            let request = connection.send_request(request);
            let _ = events
                .send(AgentEvent::TurnStarted {
                    turn_id: id.clone(),
                })
                .await;
            if let Some(checkpoint_id) = checkpoint {
                let _ = events
                    .send(AgentEvent::TurnCheckpoint {
                        turn_id: id.clone(),
                        checkpoint_id,
                    })
                    .await;
            }
            let turn_tx = turn_tx.clone();
            executor
                .spawn(async move {
                    let result = request.block_task().await;
                    let _ = turn_tx
                        .send(TurnOutcome {
                            turn_id: id,
                            generation,
                            result,
                        })
                        .await;
                })
                .detach();
        }
        // The agent still owes us the `session/prompt` response carrying
        // `stopReason: cancelled`; the turn completes when it lands.
        SessionCommand::Interrupt => session.interrupt(session_id).await,
        SessionCommand::RespondApproval {
            request_id,
            decision,
        } => {
            let approval = state.lock_recover().approvals.remove(&request_id);
            let Some(responder) = approval else {
                log::warn!("acp: no pending approval {request_id}");
                return;
            };
            let _ = responder.send(decision.clone()).await;
            let _ = events
                .send(AgentEvent::ApprovalResolved {
                    request_id,
                    decision,
                })
                .await;
        }
        SessionCommand::SetOption { id, value } => {
            let (origin, restart) = {
                let state = state.lock_recover();
                (
                    state.options.origin(&id),
                    state.options.value_requires_restart(&id, &value),
                )
            };
            let Some(origin) = origin else {
                log::warn!("acp: unknown option id `{id}`");
                return;
            };
            if origin == OptionOrigin::Launch || restart {
                return;
            }
            match set_option(connection, session_id, &origin, &value).await {
                Ok(config_options) => {
                    {
                        let mut state = state.lock_recover();
                        // A config option's answer carries every option's
                        // `currentValue`, the one set included; a mode's
                        // carries nothing, and its success is the switch.
                        if let Some(options) = config_options.as_deref() {
                            state.options.ingest(None, Some(options));
                        } else if origin == OptionOrigin::Mode
                            && let Some(mode) = value.as_str()
                        {
                            state.select_mode(acp::SessionModeId::new(mode));
                        }
                    }
                    emit_provider_options(state, events, false).await;
                }
                Err(err) => {
                    let _ = events
                        .send(AgentEvent::Warning {
                            message: format!("could not apply `{id}`: {}", describe(&err)),
                        })
                        .await;
                }
            }
        }
        SessionCommand::RespondUserInput {
            request_id,
            answers,
        } => {
            let Some(answer) = state.lock_recover().inputs.remove(&request_id) else {
                log::warn!("acp: no pending user input {request_id}");
                return;
            };
            let _ = answer.send(answers.clone()).await;
            let _ = events
                .send(AgentEvent::UserInputResolved {
                    request_id,
                    answers,
                })
                .await;
        }
        command @ (SessionCommand::Rewind { .. }
        | SessionCommand::PreviewRewind { .. }
        | SessionCommand::UndoRewind { .. }) => {
            let rewind = dialect
                .clone()
                .rewind(session.clone(), session_id.clone(), command);
            let events = events.clone();
            executor
                .spawn(async move {
                    if let Some(event) = rewind.await {
                        let _ = events.send(event).await;
                    }
                })
                .detach();
        }
        SessionCommand::Compact => {
            let _ = events
                .send(AgentEvent::Warning {
                    message: "this ACP agent does not advertise a native compaction operation"
                        .into(),
                })
                .await;
        }
        SessionCommand::ApplySettings {
            model,
            option_selections,
        } => {
            let mut applied = true;
            if let Some(target_model) = model {
                let (current, origin) = {
                    let state = state.lock_recover();
                    (
                        state.options.current_model(),
                        state.options.origin(MODEL_OPTION_ID),
                    )
                };
                if current.as_deref() != Some(target_model.as_str()) {
                    match origin {
                        Some(origin) if origin != OptionOrigin::Launch => {
                            match set_option(
                                connection,
                                session_id,
                                &origin,
                                &Value::String(target_model.clone()),
                            )
                            .await
                            {
                                Ok(config_options) => {
                                    let confirmed_model = {
                                        let mut state = state.lock_recover();
                                        if let Some(options) = config_options.as_deref() {
                                            state.options.ingest(None, Some(options));
                                        }
                                        state.options.current_model()
                                    };
                                    if let Some(model) = confirmed_model {
                                        let _ =
                                            events.send(AgentEvent::SessionModel { model }).await;
                                    }
                                    emit_provider_options(state, events, false).await;
                                }
                                Err(err) => {
                                    applied = false;
                                    let _ = events
                                        .send(AgentEvent::Warning {
                                            message: format!(
                                                "could not switch model to `{target_model}`: {}",
                                                describe(&err)
                                            ),
                                        })
                                        .await;
                                }
                            }
                        }
                        _ => {
                            applied = false;
                        }
                    }
                }
            }
            if applied {
                for selection in option_selections {
                    if selection.id == MODEL_OPTION_ID {
                        continue;
                    }
                    let (current, origin, restart) = {
                        let state = state.lock_recover();
                        (
                            state.options.value(&selection.id).cloned(),
                            state.options.origin(&selection.id),
                            state
                                .options
                                .value_requires_restart(&selection.id, &selection.value),
                        )
                    };
                    if current.as_ref() == Some(&selection.value) {
                        continue;
                    }
                    if restart {
                        applied = false;
                        break;
                    }
                    let Some(origin) = origin.filter(|origin| *origin != OptionOrigin::Launch)
                    else {
                        continue;
                    };
                    match set_option(connection, session_id, &origin, &selection.value).await {
                        Ok(config_options) => {
                            {
                                let mut state = state.lock_recover();
                                if let Some(options) = config_options.as_deref() {
                                    state.options.ingest(None, Some(options));
                                } else if origin == OptionOrigin::Mode
                                    && let Some(mode) = selection.value.as_str()
                                {
                                    state.select_mode(acp::SessionModeId::new(mode));
                                }
                            }
                            emit_provider_options(state, events, false).await;
                        }
                        Err(err) => {
                            applied = false;
                            let _ = events
                                .send(AgentEvent::Warning {
                                    message: format!(
                                        "could not apply `{}`: {}",
                                        selection.id,
                                        describe(&err)
                                    ),
                                })
                                .await;
                            break;
                        }
                    }
                }
            }
            let _ = events.send(AgentEvent::SettingsApplied { applied }).await;
        }
        SessionCommand::RetryTurn { .. }
        | SessionCommand::StopBackgroundTask { .. }
        | SessionCommand::MoveToBackground { .. }
        | SessionCommand::MoveTo { .. }
        | SessionCommand::RespondWorktree { .. }
        | SessionCommand::SetGoal { .. }
        | SessionCommand::ClearGoal
        | SessionCommand::ReadAccountUsage { .. } => {}
        SessionCommand::Shutdown => unreachable!("handled by the caller"),
    }
}

/// Settle every request waiting on the user: permissions answer `cancelled`
/// and questions resolve with no answers.
async fn cancel_pending(state: &Arc<Mutex<State>>, events: &Sender<AgentEvent>) {
    let (approvals, inputs): (Vec<_>, Vec<_>) = {
        let mut state = state.lock_recover();
        (
            state.approvals.drain().collect(),
            state
                .inputs
                .drain()
                .map(|(request_id, _)| request_id)
                .collect(),
        )
    };
    for (request_id, responder) in approvals {
        let _ = responder.send(ApprovalDecision::Cancel).await;
        let _ = events
            .send(AgentEvent::ApprovalResolved {
                request_id,
                decision: ApprovalDecision::Cancel,
            })
            .await;
    }
    for request_id in inputs {
        let _ = events
            .send(AgentEvent::UserInputResolved {
                request_id,
                answers: Map::new(),
            })
            .await;
    }
}

/// Complete a sent turn with its `session/prompt` result, unless the dialect
/// already completed it from the agent's own signal ([`State::end_turn`]).
async fn finish_turn<D: Dialect>(
    dialect: &Arc<D>,
    state: &Arc<Mutex<State>>,
    events: &Sender<AgentEvent>,
    outcome: TurnOutcome,
) {
    let completion = {
        let mut state = state.lock_recover();
        if state.turn.as_deref() != Some(outcome.turn_id.as_str())
            || state.prompt_generation != outcome.generation
        {
            return;
        }
        let end = dialect.turn_end(&mut state, outcome.result);
        state.complete_turn(&outcome.turn_id, end)
    };
    for event in completion {
        let _ = events.send(event).await;
    }
}

/// The status and message of a `session/prompt` result. A `-32800
/// request_cancelled` error is an agent aborting the prompt outright instead
/// of returning `stopReason: cancelled`.
pub(crate) fn prompt_status(
    result: &Result<acp::PromptResponse, sdk::Error>,
) -> (TurnStatus, Option<String>) {
    match result {
        Ok(response) => stop_reason_status(response.stop_reason),
        Err(err) if i32::from(err.code) == -32800 => (TurnStatus::Interrupted, None),
        Err(err) => (TurnStatus::Failed, Some(describe(err))),
    }
}

/// Reports an agent closing (or breaking) stdout while still handing the bytes
/// to the SDK transport. The SDK's `ByteStreams` treats a clean EOF as a
/// completed input stream, which does not end the command loop; observed here,
/// it closes the session at once.
struct ObservedReader {
    inner: smol::process::ChildStdout,
    done: Sender<String>,
}

impl ObservedReader {
    fn new(inner: smol::process::ChildStdout, done: Sender<String>) -> Self {
        Self { inner, done }
    }
}

impl AsyncRead for ObservedReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(0)) => {
                let _ = self
                    .done
                    .try_send("the ACP agent closed its stdio".to_string());
                Poll::Ready(Ok(0))
            }
            Poll::Ready(Err(err)) => {
                let _ = self.done.try_send(format!("ACP transport error: {err}"));
                Poll::Ready(Err(err))
            }
            other => other,
        }
    }
}

/// Reports a turn only after the SDK has written its complete JSON-RPC prompt
/// line to the child. `ConnectionTo::send_request` merely queues internally and
/// does not expose a synchronous enqueue failure, so it is not a delivery
/// boundary on its own.
struct ObservedWriter<W> {
    inner: W,
    line: Vec<u8>,
    pending_deliveries: Arc<Mutex<VecDeque<u64>>>,
    events: Sender<AgentEvent>,
}

impl<W> ObservedWriter<W> {
    fn new(
        inner: W,
        pending_deliveries: Arc<Mutex<VecDeque<u64>>>,
        events: Sender<AgentEvent>,
    ) -> Self {
        Self {
            inner,
            line: Vec::new(),
            pending_deliveries,
            events,
        }
    }

    fn finish_line(&mut self) {
        let is_prompt = serde_json::from_slice::<Value>(&self.line)
            .ok()
            .is_some_and(|message| {
                message.get("method").and_then(Value::as_str) == Some("session/prompt")
            });
        self.line.clear();
        if !is_prompt {
            return;
        }
        if let Some(delivery_id) = self.pending_deliveries.lock_recover().pop_front() {
            let _ = self
                .events
                .try_send(AgentEvent::TurnAccepted { delivery_id });
        }
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for ObservedWriter<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let written = match Pin::new(&mut self.inner).poll_write(cx, buf) {
            Poll::Ready(Ok(written)) => written,
            other => return other,
        };
        for byte in &buf[..written] {
            if *byte == b'\n' {
                self.finish_line();
            } else {
                self.line.push(*byte);
            }
        }
        Poll::Ready(Ok(written))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_close(cx)
    }
}

/// ACP's `auth_required` error.
pub(crate) fn is_auth_required(err: &sdk::Error) -> bool {
    i32::from(err.code) == -32000
}

pub(crate) fn describe(err: &sdk::Error) -> String {
    match &err.data {
        Some(data) => format!("{} ({data})", err.message),
        None => err.message.clone(),
    }
}

/// `agy acp` (Antigravity ACP Server, observed in 1.5.12 and 1.5.13) omits
/// `mcpCapabilities.http` from its `initialize` response yet connects to HTTP
/// `mcpServers` passed in `session/new` and `session/load`.
fn accepts_undeclared_http_mcp(init: &acp::InitializeResponse) -> bool {
    init.agent_info
        .as_ref()
        .is_some_and(|info| info.name == "Antigravity ACP Server")
}

/// The `mcpServers` array for `session/new`, `session/load` and `session/resume`.
fn http_mcp_servers(registrations: &[&McpRegistration]) -> Vec<acp::McpServer> {
    registrations
        .iter()
        .map(|mcp| {
            acp::McpServer::Http(
                acp::McpServerHttp::new(mcp.name.clone(), mcp.url.clone()).headers(vec![
                    acp::HttpHeader::new("Authorization", format!("Bearer {}", mcp.bearer_token)),
                ]),
            )
        })
        .collect()
}

async fn set_option(
    connection: &Connection,
    session_id: &acp::SessionId,
    origin: &OptionOrigin,
    value: &Value,
) -> Result<Option<Vec<acp::SessionConfigOption>>, sdk::Error> {
    match origin {
        OptionOrigin::Launch => Ok(None),
        OptionOrigin::Mode => {
            let Some(mode) = value.as_str() else {
                return Err(sdk::Error::invalid_params());
            };
            connection
                .send_request(acp::SetSessionModeRequest::new(
                    session_id.clone(),
                    acp::SessionModeId::new(mode),
                ))
                .block_task()
                .await?;
            Ok(None)
        }
        OptionOrigin::Config(config_id) => {
            let value = match value {
                Value::Bool(value) => acp::SessionConfigOptionValue::boolean(*value),
                Value::String(value) => acp::SessionConfigOptionValue::value_id(
                    acp::SessionConfigValueId::new(value.as_str()),
                ),
                _ => return Err(sdk::Error::invalid_params()),
            };
            let response = connection
                .send_request(acp::SetSessionConfigOptionRequest::new(
                    session_id.clone(),
                    config_id.clone(),
                    value,
                ))
                .block_task()
                .await?;
            Ok(Some(response.config_options))
        }
    }
}

fn approval_outcome(
    decision: &ApprovalDecision,
    options: &[ApprovalOption],
) -> Option<acp::RequestPermissionOutcome> {
    let selected = match decision {
        ApprovalDecision::Cancel => return Some(acp::RequestPermissionOutcome::Cancelled),
        ApprovalDecision::Option(id) => Some(id.clone()),
        ApprovalDecision::Feedback(_) => options
            .iter()
            .find(|option| option.kind == ApprovalOptionKind::RejectOnce)
            .map(|option| option.id.clone()),
    }?;
    Some(acp::RequestPermissionOutcome::Selected(
        acp::SelectedPermissionOutcome::new(acp::PermissionOptionId::new(selected)),
    ))
}

/// `stopReason` → canonical turn status, plus the message to surface (if any).
pub(crate) fn stop_reason_status(reason: acp::StopReason) -> (TurnStatus, Option<String>) {
    match reason {
        acp::StopReason::EndTurn => (TurnStatus::Completed, None),
        acp::StopReason::Cancelled => (TurnStatus::Interrupted, None),
        acp::StopReason::Refusal => (
            TurnStatus::Failed,
            Some("The agent refused to continue this turn.".into()),
        ),
        acp::StopReason::MaxTokens => (
            TurnStatus::Failed,
            Some("The agent stopped: token limit reached.".into()),
        ),
        acp::StopReason::MaxTurnRequests => (
            TurnStatus::Failed,
            Some("The agent stopped: too many model requests in one turn.".into()),
        ),
        other => (
            TurnStatus::Failed,
            Some(format!("The agent stopped: {other:?}")),
        ),
    }
}

/// The user's text and images, then the instructions as a text block of their own. A native
/// `/command` goes alone: the agent reads the prompt as the command's arguments.
fn prompt_blocks(
    text: &str,
    attachments: &[Attachment],
    instructions: Option<&str>,
) -> Vec<acp::ContentBlock> {
    let mut blocks = Vec::with_capacity(2 + attachments.len());
    // A message of pictures alone carries no text block: the schema asks for
    // none, and an ACP client sends a pasted picture as an image block alone.
    if !text.is_empty() || attachments.is_empty() {
        blocks.push(acp::ContentBlock::Text(acp::TextContent::new(
            text.to_string(),
        )));
    }
    for attachment in attachments {
        blocks.push(acp::ContentBlock::Image(acp::ImageContent::new(
            attachment.data_base64.clone(),
            attachment.media_type.clone(),
        )));
    }
    if let Some(instructions) = instructions.filter(|_| !text.starts_with('/')) {
        blocks.push(acp::ContentBlock::Text(acp::TextContent::new(
            instructions.to_owned(),
        )));
    }
    blocks
}

async fn emit_provider_options(
    state: &Arc<Mutex<State>>,
    events: &Sender<AgentEvent>,
    emit_empty: bool,
) {
    let event = state.lock_recover().provider_options();
    if matches!(
        &event,
        AgentEvent::ProviderOptions { descriptors, .. } if descriptors.is_empty()
    ) && !emit_empty
    {
        return;
    }
    let _ = events.send(event).await;
}

/// Where a canonical option id came from, so `SetOption` routes to the right
/// ACP method.

#[derive(Debug, Clone, PartialEq)]
enum OptionOrigin {
    Launch,
    Mode,
    Config(acp::SessionConfigId),
}

/// The agent's self-described modes and config options, mapped onto our
/// [`OptionDescriptor`]s, which the composer places by their role.
struct OptionRegistry {
    provider: ProviderKind,
    records: Vec<OptionRecord>,
    /// Launch-flag option override (e.g. Antigravity's `--dangerously-skip-permissions`
    /// pinning `mode` to `full-access` regardless of what wire modes report).
    launch_override: Option<(String, Value)>,
}

struct OptionRecord {
    descriptor: OptionDescriptor,
    selection: OptionSelection,
    origin: OptionOrigin,
}

impl OptionRegistry {
    fn new(provider: ProviderKind) -> Self {
        Self {
            provider,
            records: Vec::new(),
            launch_override: None,
        }
    }

    fn set_launch_override(&mut self, id: String, value: Value) {
        self.launch_override = Some((id, value));
    }

    fn value_requires_restart(&self, id: &str, value: &Value) -> bool {
        self.records
            .iter()
            .find(|record| record.selection.id == id)
            .is_some_and(|record| {
                record.descriptor.value_apply(Some(value)) == crate::ApplyTiming::Restart
                    || record.descriptor.value_apply(Some(&record.selection.value))
                        == crate::ApplyTiming::Restart
            })
    }

    fn override_for(&self, id: &str) -> Option<Value> {
        self.launch_override
            .as_ref()
            .filter(|(override_id, _)| override_id == id)
            .map(|(_, value)| value.clone())
    }

    fn ingest(
        &mut self,
        modes: Option<&acp::SessionModeState>,
        config: Option<&[acp::SessionConfigOption]>,
    ) {
        let permission_meta =
            crate::permission_control(self.provider).and_then(|descriptor| match descriptor {
                OptionDescriptor::Select {
                    id,
                    label,
                    options,
                    recommended,
                    permissive,
                    ..
                } => {
                    let launch_choices: Vec<SelectOption> = options
                        .into_iter()
                        .filter(|option| option.apply == Some(crate::ApplyTiming::Restart))
                        .collect();
                    Some((id, label, recommended, permissive, launch_choices))
                }
                OptionDescriptor::Boolean { .. } => None,
            });
        if let Some(modes) = modes {
            let id = mode_option_id(self.provider).to_string();
            let (label, recommended, permissive, launch_choices) = permission_meta
                .as_ref()
                .filter(|(perm_id, ..)| *perm_id == id)
                .map(|(_, label, rec, perm, extra)| {
                    (label.clone(), rec.clone(), perm.clone(), extra.as_slice())
                })
                .unwrap_or_else(|| ("Mode".to_string(), None, None, &[]));
            let mut options: Vec<SelectOption> = modes
                .available_modes
                .iter()
                .map(|mode| SelectOption {
                    value: mode.id.0.to_string(),
                    label: mode.name.clone(),
                    description: mode.description.clone(),
                    unavailable: None,
                    speed: None,
                    summary: None,
                    approval: crate::approval_class(self.provider, &id, &mode.id.0),
                    apply: None,
                })
                .collect();
            for extra in launch_choices {
                if !options.iter().any(|option| option.value == extra.value) {
                    options.push(extra.clone());
                }
            }
            let selected = self
                .override_for(&id)
                .unwrap_or_else(|| Value::String(modes.current_mode_id.0.to_string()));
            self.upsert(
                OptionDescriptor::Select {
                    id: id.clone(),
                    label,
                    options,
                    default_value: Some(modes.current_mode_id.0.to_string()),
                    role: config_option_role(
                        self.provider,
                        Some(&acp::SessionConfigOptionCategory::Mode),
                    ),
                    apply: crate::ApplyTiming::Live,
                    recommended,
                    permissive,
                    minimum: None,
                },
                OptionOrigin::Mode,
                selected,
            );
        }
        if let Some(config) = config {
            self.records.retain(|record| match &record.origin {
                OptionOrigin::Config(id) => config.iter().any(|option| &option.id == id),
                OptionOrigin::Mode | OptionOrigin::Launch => true,
            });
        }
        for option in config.unwrap_or_default() {
            // The mode, model and thought-level categories take tcode's
            // canonical ids; every option, whatever its category, is set
            // through `session/set_config_option`.
            let id = config_option_id(self.provider, &option.id.0, option.category.as_ref());
            let approval = |value: &str| crate::approval_class(self.provider, &id, value);
            match &option.kind {
                acp::SessionConfigKind::Select(select) => {
                    let (label, recommended, permissive, launch_choices) = permission_meta
                        .as_ref()
                        .filter(|(perm_id, ..)| *perm_id == id)
                        .map(|(_, label, rec, perm, extra)| {
                            (label.clone(), rec.clone(), perm.clone(), extra.as_slice())
                        })
                        .unwrap_or_else(|| (option.name.clone(), None, None, &[]));
                    let mut options: Vec<SelectOption> = match &select.options {
                        acp::SessionConfigSelectOptions::Ungrouped(flat) => flat
                            .iter()
                            .map(|option| SelectOption {
                                value: option.value.0.to_string(),
                                label: option.name.clone(),
                                description: option.description.clone(),
                                unavailable: None,
                                speed: None,
                                summary: None,
                                approval: approval(&option.value.0),
                                apply: None,
                            })
                            .collect(),
                        acp::SessionConfigSelectOptions::Grouped(groups) => groups
                            .iter()
                            .flat_map(|group| {
                                group.options.iter().map(move |option| SelectOption {
                                    value: option.value.0.to_string(),
                                    label: format!("{} · {}", group.name, option.name),
                                    description: option.description.clone(),
                                    unavailable: None,
                                    speed: None,
                                    summary: None,
                                    approval: approval(&option.value.0),
                                    apply: None,
                                })
                            })
                            .collect(),
                        _ => Vec::new(),
                    };
                    for extra in launch_choices {
                        if !options.iter().any(|option| option.value == extra.value) {
                            options.push(extra.clone());
                        }
                    }
                    let selected = self
                        .override_for(&id)
                        .unwrap_or_else(|| Value::String(select.current_value.0.to_string()));
                    self.upsert(
                        OptionDescriptor::Select {
                            id,
                            label,
                            options,
                            default_value: Some(select.current_value.0.to_string()),
                            role: config_option_role(self.provider, option.category.as_ref()),
                            apply: crate::ApplyTiming::Live,
                            recommended,
                            permissive,
                            minimum: None,
                        },
                        OptionOrigin::Config(option.id.clone()),
                        selected,
                    );
                }
                acp::SessionConfigKind::Boolean(boolean) => self.upsert(
                    OptionDescriptor::Boolean {
                        id,
                        label: option.name.clone(),
                        default_value: boolean.current_value,
                        role: config_option_role(self.provider, option.category.as_ref()),
                        apply: crate::ApplyTiming::Live,
                        recommended: None,
                        permissive: None,
                    },
                    OptionOrigin::Config(option.id.clone()),
                    Value::Bool(boolean.current_value),
                ),
                _ => log::warn!("acp: unsupported config-option kind for `{}`", option.id.0),
            }
        }
    }

    fn upsert(&mut self, descriptor: OptionDescriptor, origin: OptionOrigin, value: Value) {
        let id = descriptor_id(&descriptor).to_string();
        match self
            .records
            .iter_mut()
            .find(|record| record.selection.id == id)
        {
            Some(record) => {
                record.descriptor = descriptor;
                record.selection.value = value;
                record.origin = origin;
            }
            None => self.records.push(OptionRecord {
                descriptor,
                selection: OptionSelection { id, value },
                origin,
            }),
        }
    }

    fn select(&mut self, id: &str, value: Value) {
        let value = self.override_for(id).unwrap_or(value);
        if let Some(record) = self
            .records
            .iter_mut()
            .find(|record| record.selection.id == id)
        {
            record.selection.value = value;
        }
    }

    fn origin(&self, id: &str) -> Option<OptionOrigin> {
        self.records
            .iter()
            .find(|record| record.selection.id == id)
            .map(|record| record.origin.clone())
    }

    fn value(&self, id: &str) -> Option<&Value> {
        self.records
            .iter()
            .find(|record| record.selection.id == id)
            .map(|record| &record.selection.value)
    }

    fn current_model(&self) -> Option<String> {
        self.records
            .iter()
            .find(|record| record.selection.id == MODEL_OPTION_ID)
            .and_then(|record| record.selection.value.as_str().map(str::to_string))
    }
}

/// What `session/new` offered an ACP agent's sessions on `provider`: the
/// choices of its model option become the models, everything else the options.
pub(crate) fn catalog(
    provider: ProviderKind,
    modes: Option<&acp::SessionModeState>,
    config_options: Option<&[acp::SessionConfigOption]>,
) -> crate::AcpCatalog {
    let mut registry = OptionRegistry::new(provider);
    registry.ingest(modes, config_options);
    let mut catalog = crate::AcpCatalog::default();
    for record in registry.records {
        match record.descriptor {
            OptionDescriptor::Select {
                options,
                default_value,
                ..
            } if record.selection.id == MODEL_OPTION_ID => {
                catalog.models = options
                    .into_iter()
                    .map(|option| crate::ModelSpec {
                        is_default: default_value.as_deref() == Some(option.value.as_str()),
                        id: option.value,
                        display_name: option.label,
                        options: Vec::new(),
                    })
                    .collect();
            }
            descriptor => catalog.options.push(descriptor),
        }
    }
    catalog
}

fn descriptor_id(descriptor: &OptionDescriptor) -> &str {
    match descriptor {
        OptionDescriptor::Select { id, .. } | OptionDescriptor::Boolean { id, .. } => id,
    }
}

/// A tool call as last known. `tool_call_update` is a partial patch, so the
/// merged state lives here and every update re-renders the whole item.
#[derive(Debug, Clone, Default)]
struct ToolState {
    /// The tool's own name when the wire carries one; `title` is the agent's
    /// human description of the call.
    name: Option<String>,
    title: String,
    kind: acp::ToolKind,
    status: acp::ToolCallStatus,
    content: Vec<acp::ToolCallContent>,
    locations: Vec<acp::ToolCallLocation>,
    raw_input: Option<Value>,
    raw_output: Option<Value>,
    announced: bool,
    /// Whether this call's whole-file `Diff` content has been recorded in
    /// [`State::journal`] for relayed rewind.
    journalled: bool,
    /// The turn the agent last spoke of this call in. A call a turn left
    /// unfinished (an interrupt, a rejected permission) keeps its status.
    turn: Option<String>,
    /// The files Tcode wrote for this call through `fs/write_text_file`.
    writes: Vec<FileWrite>,
}

/// A file Tcode wrote for the agent: what it held before, `None` when the
/// write created it, and what it holds now.
#[derive(Debug, Clone)]
struct FileWrite {
    path: PathBuf,
    old_text: Option<String>,
    new_text: String,
}

/// The text block currently streaming (assistant prose or thinking).
struct TextStream {
    id: String,
    kind: DeltaKind,
    text: String,
}

/// One live session's protocol state: the standard mapping's tool, text and
/// option state, the agent's terminals and the requests waiting on the user.
pub(crate) struct State {
    cwd: PathBuf,
    turn: Option<String>,
    /// Incremented on every `session/prompt` issued for the active turn (initial
    /// send or steering replacement) so superseded prompt responses are ignored.
    prompt_generation: u64,
    /// True while `session/load` answers, which may replay history first.
    loading: bool,
    tools: HashMap<String, ToolState>,
    text: Option<TextStream>,
    approvals: HashMap<String, Sender<ApprovalDecision>>,
    approval_seq: u64,
    inputs: HashMap<String, Sender<Map<String, Value>>>,
    input_seq: u64,
    terminal_seq: u64,
    terminals: HashMap<String, Arc<Terminal>>,
    usage: Option<TokenUsage>,
    options: OptionRegistry,
    /// [`Dialect::owned_config_options`].
    owned_options: &'static [&'static str],
    /// A turn the agent started while another was still open.
    next_turn: Option<String>,
    modes: Option<acp::SessionModeState>,
    /// [`SessionOptions::instructions`] while the session attached Tcode's MCP servers.
    instructions: Option<String>,
    /// The thread's record of what its sessions write through Tcode, and the
    /// restore point of the turn whose writes it takes: the latest sent, which
    /// what the agent does on its own afterwards still belongs to
    /// ([`crate::Caps::relayed_history`]).
    journal: Option<Arc<Mutex<WriteJournal>>>,
    checkpoint: Option<String>,
}

impl State {
    pub(crate) fn new(provider: ProviderKind, cwd: PathBuf) -> Self {
        Self {
            cwd,
            turn: None,
            prompt_generation: 0,
            loading: false,
            tools: HashMap::new(),
            text: None,
            approvals: HashMap::new(),
            approval_seq: 0,
            inputs: HashMap::new(),
            input_seq: 0,
            terminal_seq: 0,
            terminals: HashMap::new(),
            usage: None,
            options: OptionRegistry::new(provider),
            owned_options: &[],
            next_turn: None,
            modes: None,
            instructions: None,
            journal: None,
            checkpoint: None,
        }
    }

    /// Whether `session/load` is still answering.
    pub(crate) fn loading(&self) -> bool {
        self.loading
    }

    /// The running turn's id.
    pub(crate) fn turn(&self) -> Option<&str> {
        self.turn.as_deref()
    }

    /// The latest `usage_update` context figure.
    pub(crate) fn usage(&self) -> Option<TokenUsage> {
        self.usage
    }

    /// Whether a tool call with `id` has already been seen in this session.
    pub(crate) fn has_tool(&self, id: &str) -> bool {
        self.tools.contains_key(id)
    }

    /// The title last seen for tool call `id`, if any.
    pub(crate) fn tool_title(&self, id: &str) -> Option<&str> {
        self.tools.get(id).map(|tool| tool.title.as_str())
    }

    /// Record every whole-file `Diff` carried by a completed tool call in the
    /// thread's [`WriteJournal`] when the agent edited files itself rather
    /// than through `fs/write_text_file`.
    fn journal_diffs(&mut self, id: &str) {
        let Some((journal, checkpoint)) = self.journal.clone().zip(self.checkpoint.clone()) else {
            return;
        };
        let Some(tool) = self.tools.get_mut(id) else {
            return;
        };
        if tool.journalled || tool.status != acp::ToolCallStatus::Completed {
            return;
        }
        let entries: Vec<(PathBuf, Option<Vec<u8>>, Vec<u8>)> = tool
            .content
            .iter()
            .filter_map(|content| match content {
                acp::ToolCallContent::Diff(diff) if !is_hunk_diff(diff.meta.as_ref()) => Some((
                    self.cwd.join(&diff.path),
                    diff.old_text.as_ref().map(|text| text.as_bytes().to_vec()),
                    diff.new_text.as_bytes().to_vec(),
                )),
                _ => None,
            })
            .collect();
        if entries.is_empty() {
            return;
        }
        tool.journalled = true;
        let mut journal = journal.lock_recover();
        for (path, before, after) in entries {
            if let Err(err) = journal.record_write(&checkpoint, &path, before.as_deref(), &after) {
                log::warn!(
                    "acp: could not keep what a write to {} replaces: {err}",
                    path.display()
                );
            }
        }
    }

    /// Open a turn the agent started on its own, which no `session/prompt`
    /// answers. While another turn is still open it opens as soon as that one
    /// completes.
    pub(crate) fn begin_agent_turn(&mut self, turn_id: &str) -> Vec<AgentEvent> {
        if self.turn.is_some() {
            self.next_turn = Some(turn_id.to_string());
            return Vec::new();
        }
        self.turn = Some(turn_id.to_string());
        vec![AgentEvent::TurnStarted {
            turn_id: turn_id.to_string(),
        }]
    }

    /// Complete `turn_id` from the agent's own signal if it is still the open
    /// turn. A sent turn completed this way ignores its later `session/prompt`
    /// result. A turn the client sent while an agent-started one ran has taken
    /// its place and completes as sent turns do; only the ended turn's streamed
    /// text is closed.
    pub(crate) fn end_turn(&mut self, turn_id: &str, end: TurnEnd) -> Vec<AgentEvent> {
        if self.next_turn.as_deref() == Some(turn_id) {
            self.next_turn = None;
        }
        if self.turn.as_deref() != Some(turn_id) {
            return self.flush_text();
        }
        self.complete_turn(turn_id, end)
    }

    /// Close the open turn: its streaming text, the end's message and its
    /// completion, then open the agent-started turn waiting behind it.
    fn complete_turn(&mut self, turn_id: &str, end: TurnEnd) -> Vec<AgentEvent> {
        let mut events = self.flush_text();
        self.turn = None;
        events.extend(end.message.map(|message| AgentEvent::Error {
            message,
            fatal: false,
        }));
        events.push(AgentEvent::TurnCompleted {
            turn_id: turn_id.to_string(),
            status: end.status,
            usage: end.usage,
        });
        if let Some(next) = self.next_turn.take() {
            events.extend(self.begin_agent_turn(&next));
        }
        events
    }

    fn ingest_modes(&mut self, modes: Option<&acp::SessionModeState>) {
        let Some(modes) = modes else {
            return;
        };
        self.modes = Some(modes.clone());
        self.select_mode(modes.current_mode_id.clone());
        self.options.ingest(Some(modes), None);
    }

    fn select_mode(&mut self, mode: acp::SessionModeId) {
        if let Some(modes) = self.modes.as_mut() {
            modes.current_mode_id = mode.clone();
        }
        let id = mode_option_id(self.options.provider);
        self.options.select(id, Value::String(mode.0.to_string()));
    }

    fn provider_options(&self) -> AgentEvent {
        let surfaced = || {
            self.options.records.iter().filter(|record| {
                !matches!(&record.origin, OptionOrigin::Config(id)
                    if owned(self.owned_options, &id.0, &record.selection.id))
            })
        };
        AgentEvent::ProviderOptions {
            descriptors: surfaced().map(|record| record.descriptor.clone()).collect(),
            selections: surfaced().map(|record| record.selection.clone()).collect(),
        }
    }

    /// Close the open text block, emitting its final `ItemCompleted`.
    pub(crate) fn flush_text(&mut self) -> Vec<AgentEvent> {
        let Some(stream) = self.text.take() else {
            return Vec::new();
        };
        if stream.text.is_empty() {
            return Vec::new();
        }
        let content = match stream.kind {
            DeltaKind::ReasoningText => ItemContent::Reasoning { text: stream.text },
            _ => ItemContent::AssistantMessage {
                text: stream.text,
                summarized: false,
            },
        };
        vec![AgentEvent::ItemCompleted(ThreadItem {
            id: stream.id,
            parent_item_id: None,
            content,
        })]
    }

    /// Append a streaming chunk, opening a new item when the block changes.
    fn push_text(&mut self, kind: DeltaKind, text: String) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        let same_block = self.text.as_ref().is_some_and(|stream| stream.kind == kind);
        if !same_block {
            events.extend(self.flush_text());
            let prefix = match kind {
                DeltaKind::ReasoningText => "thought",
                _ => "msg",
            };
            // Random ids rather than a per-process counter so a restarted ACP
            // session never collides with an earlier turn's entries.
            self.text = Some(TextStream {
                id: format!("{prefix}-{}", uuid::Uuid::new_v4()),
                kind,
                text: String::new(),
            });
        }
        let stream = self.text.as_mut().expect("stream just opened");
        stream.text.push_str(&text);
        events.push(AgentEvent::Delta {
            item_id: stream.id.clone(),
            kind,
            text,
        });
        events
    }

    /// Map one `session/update` onto canonical events, merging the tool-call and
    /// option state along the way.
    pub(crate) fn apply_update(&mut self, update: acp::SessionUpdate) -> Vec<AgentEvent> {
        match update {
            // The app synthesizes the canonical user message at send time;
            // rendering the agent's echo of it would double it.
            acp::SessionUpdate::UserMessageChunk(_) => Vec::new(),
            acp::SessionUpdate::AgentMessageChunk(chunk) => {
                let text = content_text(&chunk.content);
                if text.is_empty() {
                    return Vec::new();
                }
                self.push_text(DeltaKind::AssistantText, text)
            }
            acp::SessionUpdate::AgentThoughtChunk(chunk) => {
                let text = content_text(&chunk.content);
                if text.is_empty() {
                    return Vec::new();
                }
                self.push_text(DeltaKind::ReasoningText, text)
            }
            acp::SessionUpdate::ToolCall(call) => {
                let mut events = self.flush_text();
                let id = call.tool_call_id.0.to_string();
                let previous = self.tools.get_mut(&id);
                let journalled = previous.as_ref().is_some_and(|tool| tool.journalled);
                let writes = match previous {
                    Some(previous) => std::mem::take(&mut previous.writes),
                    None => Vec::new(),
                };
                let tool = ToolState {
                    name: tool_name(call.meta.as_ref(), call.raw_input.as_ref()),
                    title: call.title,
                    kind: call.kind,
                    status: call.status,
                    content: call.content,
                    locations: call.locations,
                    raw_input: call.raw_input,
                    raw_output: call.raw_output,
                    announced: true,
                    journalled,
                    turn: self.turn.clone(),
                    writes,
                };
                let terminal = matches!(
                    tool.status,
                    acp::ToolCallStatus::Completed | acp::ToolCallStatus::Failed
                );
                let item = self.tool_item(&id, &tool);
                let existed = self
                    .tools
                    .insert(id.clone(), tool)
                    .is_some_and(|previous| previous.announced);
                self.journal_diffs(&id);
                events.push(if terminal {
                    AgentEvent::ItemCompleted(item)
                } else if existed {
                    AgentEvent::ItemUpdated(item)
                } else {
                    AgentEvent::ItemStarted(item)
                });
                events
            }
            acp::SessionUpdate::ToolCallUpdate(update) => {
                let mut events = self.flush_text();
                let id = update.tool_call_id.0.to_string();
                let entry = self.tools.entry(id.clone()).or_default();
                entry.turn = self.turn.clone();
                let fields = update.fields;
                if let Some(title) = fields.title {
                    entry.title = title;
                }
                if let Some(kind) = fields.kind {
                    entry.kind = kind;
                }
                if let Some(status) = fields.status {
                    entry.status = status;
                }
                // `content` and `locations` are whole-array replacements.
                if let Some(content) = fields.content {
                    entry.content = content;
                }
                if let Some(locations) = fields.locations {
                    entry.locations = locations;
                }
                if let Some(raw_input) = fields.raw_input {
                    entry.raw_input = Some(raw_input);
                }
                if let Some(raw_output) = fields.raw_output {
                    entry.raw_output = Some(raw_output);
                }
                if let Some(name) = tool_name(update.meta.as_ref(), entry.raw_input.as_ref()) {
                    entry.name = Some(name);
                }
                let announced = std::mem::replace(&mut entry.announced, true);
                let status = entry.status;
                let tool = entry.clone();
                self.journal_diffs(&id);
                let item = self.tool_item(&id, &tool);
                let terminal = matches!(
                    status,
                    acp::ToolCallStatus::Completed | acp::ToolCallStatus::Failed
                );
                events.push(if terminal {
                    AgentEvent::ItemCompleted(item)
                } else if announced {
                    AgentEvent::ItemUpdated(item)
                } else {
                    AgentEvent::ItemStarted(item)
                });
                events
            }
            acp::SessionUpdate::Plan(plan) => vec![AgentEvent::PlanUpdated {
                turn_id: self.turn().map(str::to_owned),
                explanation: None,
                steps: plan.entries.iter().map(plan_step).collect(),
            }],
            acp::SessionUpdate::AvailableCommandsUpdate(update) => {
                vec![AgentEvent::ProviderCommands {
                    commands: update
                        .available_commands
                        .iter()
                        .map(|command| ProviderCommand {
                            name: command.name.clone(),
                            description: Some(command.description.clone()),
                            kind: ProviderCommandKind::Command,
                        })
                        .collect(),
                }]
            }
            acp::SessionUpdate::CurrentModeUpdate(update) => {
                self.select_mode(update.current_mode_id.clone());
                vec![self.provider_options()]
            }
            acp::SessionUpdate::ConfigOptionUpdate(update) => {
                let previous_model = self.options.current_model();
                self.options.ingest(None, Some(&update.config_options));
                let mut events = vec![self.provider_options()];
                if let Some(model) = self.options.current_model()
                    && Some(model.as_str()) != previous_model.as_deref()
                {
                    events.push(AgentEvent::SessionModel { model });
                }
                events
            }
            acp::SessionUpdate::UsageUpdate(usage) => {
                let usage = TokenUsage {
                    freshness: crate::ContextFreshness::Current,
                    used_tokens: Some(usage.used),
                    context_window: Some(usage.size),
                    cost_usd: usage
                        .cost
                        .as_ref()
                        .filter(|cost| cost.currency.eq_ignore_ascii_case("USD"))
                        .map(|cost| cost.amount),
                    ..Default::default()
                };
                self.usage = Some(usage);
                vec![AgentEvent::TokenUsage(usage)]
            }
            // Session titles are ours (the sidebar names sessions from the first
            // user message), and unknown variants degrade to nothing.
            _ => Vec::new(),
        }
    }

    /// Records a file Tcode wrote for the agent on the call that asked for
    /// it, and returns that call's item as it now stands. `fs/write_text_file`
    /// names no call: the one of this turn in flight that locates the file is
    /// it, and a write no single call locates is recorded on none.
    fn record_write(&mut self, write: FileWrite) -> Option<AgentEvent> {
        let cwd = &self.cwd;
        let turn = &self.turn;
        let mut asking = self.tools.iter_mut().filter(|(_, tool)| {
            tool.turn == *turn
                && matches!(
                    tool.status,
                    acp::ToolCallStatus::Pending | acp::ToolCallStatus::InProgress
                )
                && tool
                    .locations
                    .iter()
                    .any(|location| cwd.join(&location.path) == write.path)
        });
        let (id, tool) = asking.next()?;
        if asking.next().is_some() {
            return None;
        }
        let id = id.clone();
        match tool
            .writes
            .iter_mut()
            .find(|known| known.path == write.path)
        {
            Some(known) => known.new_text = write.new_text,
            None => tool.writes.push(write),
        }
        let tool = tool.clone();
        tool.announced
            .then(|| AgentEvent::ItemUpdated(self.tool_item(&id, &tool)))
    }

    /// The canonical item for a tool call, keyed by [`acp::ToolKind`]. A call
    /// that shows diffs is a file change whatever its kind, which ACP has for
    /// choosing an icon and defaults to `other`; a command and a thought keep
    /// their own shape and show theirs as text.
    fn tool_item(&self, id: &str, tool: &ToolState) -> ThreadItem {
        let status = map_status(tool.status);
        let changes = file_changes(tool);
        let content = match tool.kind {
            acp::ToolKind::Execute => ItemContent::CommandExecution {
                command: command_of(tool),
                input: Vec::new(),
                output: self.tool_output(tool, true),
                exit_code: self.exit_code_of(tool),
                status,
            },
            acp::ToolKind::Think => {
                let text = self.tool_output(tool, true);
                ItemContent::Reasoning {
                    text: if text.is_empty() {
                        tool.title.clone()
                    } else {
                        text
                    },
                }
            }
            _ if !changes.is_empty() => {
                let output = self.tool_output(tool, false);
                ItemContent::FileChange {
                    changes,
                    status,
                    output: (!output.is_empty()).then_some(output),
                    command: None,
                }
            }
            _ => tool_call_content(tool, status, self.tool_output(tool, false)),
        };
        ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content,
        }
    }

    /// Everything the tool produced, as display text: its content blocks plus
    /// the live output of any terminal it embedded, and `with_diffs` its
    /// diffs, for an item that shows them nowhere else.
    fn tool_output(&self, tool: &ToolState, with_diffs: bool) -> String {
        let mut parts: Vec<String> = Vec::new();
        for content in &tool.content {
            match content {
                acp::ToolCallContent::Content(block) => {
                    if matches!(block.content, acp::ContentBlock::Image(_)) {
                        continue;
                    }
                    let text = content_text(&block.content);
                    if !text.is_empty() {
                        parts.push(text);
                    }
                }
                acp::ToolCallContent::Terminal(terminal) => {
                    if let Some(terminal) = self.terminals.get(terminal.terminal_id.0.as_ref()) {
                        let output = terminal.output.lock_recover();
                        if !output.is_empty() {
                            parts.push(output.clone());
                        }
                    }
                }
                _ => {}
            }
        }
        if parts.is_empty()
            && let Some(raw) = &tool.raw_output
        {
            match raw.as_str() {
                Some(text) => parts.push(text.to_string()),
                None if !raw.is_null() => parts.push(raw.to_string()),
                None => {}
            }
        }
        if with_diffs {
            parts.extend(diffs(tool).map(|(path, old_text, new_text, start)| {
                unified_diff(&path, old_text.unwrap_or(""), new_text, start)
            }));
        }
        parts.join("\n")
    }

    fn exit_code_of(&self, tool: &ToolState) -> Option<i32> {
        for content in &tool.content {
            if let acp::ToolCallContent::Terminal(terminal) = content
                && let Some(terminal) = self.terminals.get(terminal.terminal_id.0.as_ref())
                && let Some(status) = terminal.exit.lock_recover().as_ref()
            {
                return status.exit_code.map(|code| code as i32);
            }
        }
        tool.raw_output
            .as_ref()
            .and_then(|raw| raw.get("exitCode").or_else(|| raw.get("exit_code")))
            .and_then(Value::as_i64)
            .map(|code| code as i32)
    }
}

/// The shell command behind an `execute` tool call: the underlying tool's own
/// arguments when it exposed them, else the agent's title.
fn command_of(tool: &ToolState) -> String {
    tool.raw_input
        .as_ref()
        .and_then(|input| input.get("command"))
        .and_then(|command| match command {
            Value::String(command) => Some(command.clone()),
            Value::Array(parts) => Some(
                parts
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            _ => None,
        })
        .unwrap_or_else(|| tool.title.clone())
}

fn tool_call_content(tool: &ToolState, status: ItemStatus, output: String) -> ItemContent {
    let mut image_reads: Vec<_> = tool
        .content
        .iter()
        .filter_map(|content| match content {
            acp::ToolCallContent::Content(block) => match &block.content {
                acp::ContentBlock::Image(image) => {
                    crate::tool_output::image(Some(&image.mime_type), &image.data)
                }
                _ => None,
            },
            _ => None,
        })
        .collect();
    if image_reads.is_empty()
        && let Some(raw) = &tool.raw_output
    {
        image_reads = ToolOutput::from_content(raw.get("content").unwrap_or(raw)).images;
    }
    ItemContent::ToolCall {
        image_reads,
        name: tool.name.clone().unwrap_or_else(|| tool.title.clone()),
        input: match tool.raw_input.as_ref() {
            Some(raw_input) => cursor_mcp_call(raw_input)
                .map_or(raw_input, |(_, args)| args)
                .clone(),
            None => Value::Null,
        },
        output: (!output.is_empty()).then_some(output),
        status,
        family: tool_family(tool).map(Box::new),
    }
}

/// What a call is, as its ACP `kind` and `locations` say: a read of the
/// file it locates, from the line it locates.
fn tool_family(tool: &ToolState) -> Option<ToolFamily> {
    let location = tool.locations.first();
    match tool.kind {
        acp::ToolKind::Read => location.map(|location| ToolFamily::Read {
            path: location.path.to_string_lossy().into_owned(),
            first_line: location.line.map(u64::from),
            note: None,
        }),
        _ => None,
    }
}

/// The tool's own name, where the wire has one: Grok's `_meta["x.ai/tool"]`
/// (whose `use_tool` dispatches the MCP tool named in its input, named here
/// `server/tool`), Cursor's `rawInput._toolName`.
fn tool_name(meta: Option<&acp::Meta>, raw_input: Option<&Value>) -> Option<String> {
    let input = |key: &str| {
        raw_input
            .and_then(|input| input.get(key))
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
    };
    let name = match meta
        .and_then(|meta| meta.get("x.ai/tool"))
        .and_then(|tool| tool.get("name"))
        .and_then(Value::as_str)
    {
        Some("use_tool") => {
            return Some(match input("tool_name") {
                Some(name) => ToolName::slashed(name).unwrap_or_else(|| name.to_owned()),
                None => "use_tool".to_owned(),
            });
        }
        Some(name) => Some(name),
        None => {
            if let Some((name, _)) = raw_input.and_then(cursor_mcp_call) {
                return Some(name);
            }
            input("_toolName")
        }
    };
    name.map(str::to_owned)
}

/// A Cursor call of an MCP server's tool: `rawInput {providerIdentifier,
/// toolName, args}` (cursor-agent 2026.10.01's `3351.index.js`, see the
/// cursor fixtures' PROVENANCE.md), named `server/tool` with `args` as its
/// input.
fn cursor_mcp_call(raw_input: &Value) -> Option<(String, &Value)> {
    let field = |key: &str| {
        raw_input
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
    };
    let name = format!("{}/{}", field("providerIdentifier")?, field("toolName")?);
    Some((name, raw_input.get("args").unwrap_or(&Value::Null)))
}

/// The diffs a call shows, each a path, the text before and after, and where
/// the texts begin: the agent's own `diff` content, and the files Tcode wrote
/// for it that no `diff` names. An agent that reports its own diff of a file
/// has said what the write did, so the write is not a second change to it.
fn diffs(tool: &ToolState) -> impl Iterator<Item = (String, Option<&str>, &str, (usize, usize))> {
    let own = || {
        tool.content.iter().filter_map(|content| match content {
            acp::ToolCallContent::Diff(diff) => Some(diff),
            _ => None,
        })
    };
    own()
        .map(|diff| {
            (
                diff.path.to_string_lossy().into_owned(),
                diff.old_text.as_deref(),
                diff.new_text.as_str(),
                hunk_start(diff.meta.as_ref()),
            )
        })
        .chain(
            tool.writes
                .iter()
                .filter(move |write| !own().any(|diff| diff.path == write.path))
                .map(|write| {
                    (
                        write.path.to_string_lossy().into_owned(),
                        write.old_text.as_deref(),
                        write.new_text.as_str(),
                        (1, 1),
                    )
                }),
        )
}

/// A call's [`diffs`] → canonical [`FileChange`]s (with a unified diff, so
/// the diff panel renders them like every other provider's edits).
fn file_changes(tool: &ToolState) -> Vec<FileChange> {
    let mut changes: Vec<FileChange> = diffs(tool)
        .map(|(path, old_text, new_text, start)| FileChange {
            kind: match (tool.kind, old_text) {
                (acp::ToolKind::Delete, _) => FileChangeKind::Delete,
                (acp::ToolKind::Move, _) => FileChangeKind::Rename,
                (_, None | Some("")) => FileChangeKind::Create,
                _ => FileChangeKind::Modify,
            },
            diff: Some(unified_diff(&path, old_text.unwrap_or(""), new_text, start)),
            path,
        })
        .collect();
    if changes.is_empty()
        && matches!(
            tool.kind,
            acp::ToolKind::Edit | acp::ToolKind::Delete | acp::ToolKind::Move
        )
    {
        // Deletes and renames carry no diff, and an edit none until it is
        // applied (Cursor's arrives with its completion): the locations are
        // the files it changes.
        changes = tool
            .locations
            .iter()
            .map(|location| FileChange {
                path: location.path.to_string_lossy().into_owned(),
                kind: match tool.kind {
                    acp::ToolKind::Delete => FileChangeKind::Delete,
                    acp::ToolKind::Move => FileChangeKind::Rename,
                    _ => FileChangeKind::Modify,
                },
                diff: None,
            })
            .collect();
    }
    changes
}

/// Whether `_meta` marks a hunk-sized diff rather than a whole-file ACP diff.
fn is_hunk_diff(meta: Option<&acp::Meta>) -> bool {
    meta.is_some_and(|meta| {
        meta.contains_key("details")
            || meta.contains_key("old_line")
            || meta.contains_key("new_line")
    })
}

/// Where a diff's texts begin in the old and new file, when the agent says;
/// ACP's own diff is whole-file text. Grok sends hunk-sized texts with
/// `_meta.old_line` / `new_line` while asking, and with one
/// `_meta.details[]` entry per replacement once applied.
fn hunk_start(meta: Option<&acp::Meta>) -> (usize, usize) {
    let lines = meta.and_then(|meta| match meta.get("details").and_then(Value::as_array) {
        Some(details) => match details.as_slice() {
            [only] => only.as_object(),
            _ => None,
        },
        None => Some(meta),
    });
    let line = |key: &str| {
        lines
            .and_then(|lines| lines.get(key))
            .and_then(Value::as_u64)
            .and_then(|line| usize::try_from(line).ok())
            .filter(|line| *line > 0)
            .unwrap_or(1)
    };
    (line("old_line"), line("new_line"))
}

/// A unified diff of one before/after pair. ACP hands us texts rather than a
/// patch, and the diff panel wants `@@` hunks; a single hunk covering the pair
/// is exactly what the panel's full-file path already renders.
fn unified_diff(
    path: &str,
    old: &str,
    new: &str,
    (old_start, new_start): (usize, usize),
) -> String {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let mut out = String::new();
    if old_lines.is_empty() {
        out.push_str("--- /dev/null\n");
    } else {
        let _ = writeln!(out, "--- a/{path}");
    }
    if new_lines.is_empty() {
        out.push_str("+++ /dev/null\n");
    } else {
        let _ = writeln!(out, "+++ b/{path}");
    }
    let _ = writeln!(
        out,
        "@@ -{},{} +{},{} @@",
        if old_lines.is_empty() { 0 } else { old_start },
        old_lines.len(),
        if new_lines.is_empty() { 0 } else { new_start },
        new_lines.len()
    );
    // Trim the common prefix/suffix so the (very common) single-line edit does
    // not render as a whole-file rewrite.
    let prefix = old_lines
        .iter()
        .zip(new_lines.iter())
        .take_while(|(old, new)| old == new)
        .count();
    let suffix = old_lines
        .iter()
        .rev()
        .zip(new_lines.iter().rev())
        .take_while(|(old, new)| old == new)
        .count()
        .min(old_lines.len() - prefix)
        .min(new_lines.len() - prefix);
    for line in &old_lines[..prefix] {
        let _ = writeln!(out, " {line}");
    }
    for line in &old_lines[prefix..old_lines.len() - suffix] {
        let _ = writeln!(out, "-{line}");
    }
    for line in &new_lines[prefix..new_lines.len() - suffix] {
        let _ = writeln!(out, "+{line}");
    }
    for line in &old_lines[old_lines.len() - suffix..] {
        let _ = writeln!(out, " {line}");
    }
    out
}

fn map_status(status: acp::ToolCallStatus) -> ItemStatus {
    match status {
        acp::ToolCallStatus::Pending | acp::ToolCallStatus::InProgress => ItemStatus::InProgress,
        acp::ToolCallStatus::Completed => ItemStatus::Completed,
        acp::ToolCallStatus::Failed => ItemStatus::Failed,
        _ => ItemStatus::InProgress,
    }
}

fn plan_step(entry: &acp::PlanEntry) -> PlanStep {
    PlanStep {
        step: entry.content.clone(),
        status: match entry.status {
            acp::PlanEntryStatus::InProgress => PlanStepStatus::InProgress,
            acp::PlanEntryStatus::Completed => PlanStepStatus::Completed,
            _ => PlanStepStatus::Pending,
        },
    }
}

/// Displayable text for a content block (images/audio degrade to a marker).
fn content_text(block: &acp::ContentBlock) -> String {
    match block {
        acp::ContentBlock::Text(text) => text.text.clone(),
        acp::ContentBlock::ResourceLink(link) => link.uri.clone(),
        acp::ContentBlock::Resource(resource) => match &resource.resource {
            acp::EmbeddedResourceResource::TextResourceContents(text) => text.text.clone(),
            _ => String::new(),
        },
        acp::ContentBlock::Image(_) => "[image]".to_string(),
        acp::ContentBlock::Audio(_) => "[audio]".to_string(),
        _ => String::new(),
    }
}

/// The approval an agent's `session/request_permission` becomes.
fn approval_request(
    id: String,
    turn_id: Option<String>,
    tool: &acp::ToolCallUpdate,
    options: &[acp::PermissionOption],
) -> ApprovalRequest {
    let fields = &tool.fields;
    let title = fields.title.clone().unwrap_or_default();
    let kind = fields.kind.unwrap_or_default();
    let raw_input = fields.raw_input.clone().unwrap_or(Value::Null);
    let changes = file_changes(&ToolState {
        kind,
        content: fields.content.clone().unwrap_or_default(),
        locations: fields.locations.clone().unwrap_or_default(),
        ..Default::default()
    });
    let approval_kind = match kind {
        acp::ToolKind::Execute => ApprovalKind::ExecCommand {
            command: command_of(&ToolState {
                title: title.clone(),
                raw_input: fields.raw_input.clone(),
                ..Default::default()
            }),
            cwd: None,
            description: None,
        },
        _ if !changes.is_empty() => ApprovalKind::FileChange { changes },
        acp::ToolKind::Edit | acp::ToolKind::Delete | acp::ToolKind::Move => {
            ApprovalKind::ToolUse {
                name: title.clone(),
                input: raw_input,
                detail: title,
            }
        }
        acp::ToolKind::Read | acp::ToolKind::Search | acp::ToolKind::Fetch => {
            ApprovalKind::FileRead { detail: title }
        }
        _ => ApprovalKind::ToolUse {
            name: tool_name(tool.meta.as_ref(), fields.raw_input.as_ref())
                .unwrap_or_else(|| title.clone()),
            input: raw_input,
            detail: title,
        },
    };
    ApprovalRequest {
        id,
        parent_item_id: None,
        turn_id,
        kind: approval_kind,
        reason: None,
        blocked_path: None,
        options: options
            .iter()
            .map(|option| ApprovalOption {
                id: option.option_id.0.to_string(),
                label: option.name.clone(),
                kind: match option.kind {
                    acp::PermissionOptionKind::AllowOnce => ApprovalOptionKind::AllowOnce,
                    acp::PermissionOptionKind::AllowAlways => ApprovalOptionKind::AllowAlways,
                    acp::PermissionOptionKind::RejectAlways => ApprovalOptionKind::RejectAlways,
                    _ => ApprovalOptionKind::RejectOnce,
                },
            })
            .collect(),
    }
}

/// The standard client services the agent calls into.
#[derive(Clone)]
struct Client {
    events: Sender<AgentEvent>,
    state: Arc<Mutex<State>>,
    turn_tx: Sender<TurnOutcome>,
}

impl Client {
    fn session(&self, connection: Connection) -> Session {
        Session {
            connection,
            events: self.events.clone(),
            state: self.state.clone(),
            turn_tx: self.turn_tx.clone(),
        }
    }

    async fn emit_all(&self, events: Vec<AgentEvent>) {
        for event in events {
            let _ = self.events.send(event).await;
        }
    }

    /// `withdrawn` completes when the agent sends `$/cancel_request` for this
    /// request.
    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
        withdrawn: impl Future<Output = ()>,
    ) -> Result<acp::RequestPermissionResponse, sdk::Error> {
        let (request_id, turn) = {
            let mut state = self.state.lock_recover();
            state.approval_seq += 1;
            (
                format!("acp-approval-{}", state.approval_seq),
                state.turn().map(str::to_owned),
            )
        };
        let request = approval_request(request_id.clone(), turn, &args.tool_call, &args.options);
        let options = request.options.clone();
        let (responder, decided) = smol::channel::bounded(1);
        {
            let mut state = self.state.lock_recover();
            state.approvals.insert(request_id.clone(), responder);
            // Keep the tool card in step with what we are asking about.
            let id = args.tool_call.tool_call_id.0.to_string();
            let entry = state.tools.entry(id).or_default();
            if let Some(title) = args.tool_call.fields.title.clone() {
                entry.title = title;
            }
            if let Some(kind) = args.tool_call.fields.kind {
                entry.kind = kind;
            }
            if let Some(raw_input) = args.tool_call.fields.raw_input.clone() {
                entry.raw_input = Some(raw_input);
            }
        }
        let _ = self
            .events
            .send(AgentEvent::ApprovalRequested(request))
            .await;

        let decision = future::or(
            async { Some(decided.recv().await.unwrap_or(ApprovalDecision::Cancel)) },
            async {
                withdrawn.await;
                None
            },
        )
        .await;
        let open = self
            .state
            .lock_recover()
            .approvals
            .remove(&request_id)
            .is_some();
        let Some(decision) = decision else {
            if open {
                let _ = self
                    .events
                    .send(AgentEvent::ApprovalResolved {
                        request_id,
                        decision: ApprovalDecision::Cancel,
                    })
                    .await;
            }
            return Err(sdk::Error::request_cancelled());
        };
        let outcome = match approval_outcome(&decision, &options) {
            Some(outcome) => outcome,
            None => {
                let _ = self
                    .events
                    .send(AgentEvent::Warning {
                        message:
                            "the agent offered no matching permission option; cancelling instead"
                                .into(),
                    })
                    .await;
                acp::RequestPermissionOutcome::Cancelled
            }
        };
        Ok(acp::RequestPermissionResponse::new(outcome))
    }

    async fn read_text_file(
        &self,
        args: acp::ReadTextFileRequest,
    ) -> Result<acp::ReadTextFileResponse, sdk::Error> {
        let path = self.state.lock_recover().cwd.join(&args.path);
        let content = smol::fs::read_to_string(&path).await.map_err(|err| {
            sdk::Error::new(-32603, format!("could not read {}: {err}", path.display()))
        })?;
        // `line` is 1-based; `limit` counts lines from there.
        let content = match (args.line, args.limit) {
            (None, None) => content,
            (line, limit) => {
                let start = line.unwrap_or(1).saturating_sub(1) as usize;
                let lines: Vec<&str> = content.lines().skip(start).collect();
                let end = limit.map_or(lines.len(), |limit| lines.len().min(limit as usize));
                lines[..end].join("\n")
            }
        };
        Ok(acp::ReadTextFileResponse::new(content))
    }

    async fn write_text_file(
        &self,
        args: acp::WriteTextFileRequest,
    ) -> Result<acp::WriteTextFileResponse, sdk::Error> {
        let path = self.state.lock_recover().cwd.join(&args.path);
        if let Some(parent) = path.parent() {
            smol::fs::create_dir_all(parent).await.map_err(|err| {
                sdk::Error::new(
                    -32603,
                    format!("could not create {}: {err}", parent.display()),
                )
            })?;
        }
        let kept = {
            let state = self.state.lock_recover();
            state.journal.clone().zip(state.checkpoint.clone())
        };
        if let Some((journal, checkpoint)) = kept {
            let (path, content) = (path.clone(), args.content.clone());
            let kept = crate::process::unblock(move || {
                journal
                    .lock_recover()
                    .before_write(&checkpoint, &path, content.as_bytes())
            })
            .await;
            // The write is the agent's to make either way; one the journal
            // could not take is one a rewind leaves as it is.
            if let Err(err) = kept {
                log::warn!(
                    "acp: could not keep what a write to {} replaces: {err}",
                    self.state.lock_recover().cwd.join(&args.path).display()
                );
            }
        }
        // What the file held decides whether the write created or modified
        // it. One that cannot be read as text is left to the call's own
        // account of it.
        let old_text = match smol::fs::read_to_string(&path).await {
            Ok(text) => Some(Some(text)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Some(None),
            Err(_) => None,
        };
        smol::fs::write(&path, &args.content).await.map_err(|err| {
            sdk::Error::new(-32603, format!("could not write {}: {err}", path.display()))
        })?;
        let update = old_text.and_then(|old_text| {
            self.state.lock_recover().record_write(FileWrite {
                path,
                old_text,
                new_text: args.content,
            })
        });
        if let Some(update) = update {
            let _ = self.events.send(update).await;
        }
        Ok(acp::WriteTextFileResponse::new())
    }

    async fn create_terminal(
        &self,
        args: acp::CreateTerminalRequest,
        connection: &Connection,
    ) -> Result<acp::CreateTerminalResponse, sdk::Error> {
        let cwd = match &args.cwd {
            Some(cwd) => self.state.lock_recover().cwd.join(cwd),
            None => self.state.lock_recover().cwd.clone(),
        };
        let terminal = Terminal::spawn(
            connection,
            &args.command,
            &args.args,
            &args.env,
            &cwd,
            args.output_byte_limit,
        )?;
        let id = {
            let mut state = self.state.lock_recover();
            state.terminal_seq += 1;
            let id = format!("term-{}", state.terminal_seq);
            state.terminals.insert(id.clone(), terminal);
            id
        };
        Ok(acp::CreateTerminalResponse::new(acp::TerminalId::new(id)))
    }

    async fn terminal_output(
        &self,
        args: acp::TerminalOutputRequest,
    ) -> Result<acp::TerminalOutputResponse, sdk::Error> {
        let terminal = self.terminal(&args.terminal_id)?;
        let output = terminal.output.lock_recover().clone();
        let truncated = *terminal.truncated.lock_recover();
        let exit_status = terminal.exit.lock_recover().clone();
        Ok(acp::TerminalOutputResponse::new(output, truncated).exit_status(exit_status))
    }

    async fn wait_for_terminal_exit(
        &self,
        args: acp::WaitForTerminalExitRequest,
    ) -> Result<acp::WaitForTerminalExitResponse, sdk::Error> {
        let terminal = self.terminal(&args.terminal_id)?;
        // The sender is dropped once the process exits, closing the channel.
        let _ = terminal.done.recv().await;
        let exit_status = terminal
            .exit
            .lock_recover()
            .clone()
            .unwrap_or_else(acp::TerminalExitStatus::new);
        Ok(acp::WaitForTerminalExitResponse::new(exit_status))
    }

    async fn kill_terminal(
        &self,
        args: acp::KillTerminalRequest,
    ) -> Result<acp::KillTerminalResponse, sdk::Error> {
        self.terminal(&args.terminal_id)?.kill();
        Ok(acp::KillTerminalResponse::new())
    }

    async fn release_terminal(
        &self,
        args: acp::ReleaseTerminalRequest,
    ) -> Result<acp::ReleaseTerminalResponse, sdk::Error> {
        let terminal = self
            .state
            .lock_recover()
            .terminals
            .remove(args.terminal_id.0.as_ref());
        if let Some(terminal) = terminal {
            terminal.kill();
        }
        Ok(acp::ReleaseTerminalResponse::new())
    }
    fn terminal(&self, id: &acp::TerminalId) -> Result<Arc<Terminal>, sdk::Error> {
        self.state
            .lock_recover()
            .terminals
            .get(id.0.as_ref())
            .cloned()
            .ok_or_else(|| sdk::Error::new(-32002, format!("unknown terminal `{}`", id.0)))
    }
}

/// A command the agent asked us to run. Headless: we capture the output and
/// serve `terminal/output` / `terminal/wait_for_exit` from it, and the text is
/// folded into the owning tool card (`ToolCallContent::Terminal`). Wiring these
/// into the terminal drawer needs a canonical event the contract does not have.
struct Terminal {
    child: Mutex<crate::process::ProcessTree<smol::process::Child>>,
    output: Mutex<String>,
    truncated: Mutex<bool>,
    exit: Mutex<Option<acp::TerminalExitStatus>>,
    /// Closed (never sent on) once the process has exited.
    done: Receiver<()>,
}

impl Terminal {
    fn spawn(
        connection: &Connection,
        command: &str,
        args: &[String],
        env: &[acp::EnvVariable],
        cwd: &Path,
        limit: Option<u64>,
    ) -> Result<Arc<Self>, sdk::Error> {
        let program = crate::resolve_binary(None, command)
            .map_err(|err| sdk::Error::new(-32603, err.to_string()))?;
        let mut cmd = crate::process::async_harness_command(&program);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        for var in env {
            cmd.env(&var.name, &var.value);
        }
        let mut child =
            crate::process::ProcessTree::new(cmd.spawn().map_err(|err| {
                sdk::Error::new(-32603, format!("could not run `{command}`: {err}"))
            })?);
        let stdout = child.stdout.take().ok_or_else(|| {
            sdk::Error::new(-32603, format!("`{command}` started without piped stdout"))
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            sdk::Error::new(-32603, format!("`{command}` started without piped stderr"))
        })?;
        let (done_tx, done) = smol::channel::bounded::<()>(1);

        let terminal = Arc::new(Terminal {
            child: Mutex::new(child),
            output: Mutex::new(String::new()),
            truncated: Mutex::new(false),
            exit: Mutex::new(None),
            done,
        });

        // stdout and stderr interleave into one buffer, as they do in a terminal.
        let streams: [Box<dyn smol::io::AsyncRead + Unpin + Send>; 2] =
            [Box::new(stdout), Box::new(stderr)];
        for stream in streams {
            connection.spawn({
                let terminal = terminal.clone();
                async move {
                    let mut stream = stream;
                    let mut buf = [0u8; 4096];
                    loop {
                        match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(read) => terminal.append(&buf[..read], limit),
                        }
                    }
                    Ok(())
                }
            })?;
        }

        connection.spawn({
            let terminal = terminal.clone();
            async move {
                loop {
                    let status = terminal.child.lock_recover().try_status();
                    match status {
                        Ok(Some(status)) => {
                            *terminal.exit.lock_recover() = Some(exit_status(&status));
                            break;
                        }
                        Err(err) => {
                            log::warn!("acp terminal: {err}");
                            break;
                        }
                        Ok(None) => smol::Timer::after(Duration::from_millis(25)).await,
                    };
                }
                // Closing the channel wakes every `wait_for_exit`.
                drop(done_tx);
                Ok(())
            }
        })?;

        Ok(terminal)
    }

    fn append(&self, bytes: &[u8], limit: Option<u64>) {
        let mut output = self.output.lock_recover();
        output.push_str(&String::from_utf8_lossy(bytes));
        if let Some(limit) = limit.map(|limit| limit as usize)
            && output.len() > limit
        {
            // Keep the tail (what ACP asks for), cutting on a char boundary.
            let mut cut = output.len() - limit;
            while !output.is_char_boundary(cut) {
                cut += 1;
            }
            output.drain(..cut);
            *self.truncated.lock_recover() = true;
        }
    }

    fn kill(&self) {
        let _ = self.child.lock_recover().kill();
    }
}

fn exit_status(status: &std::process::ExitStatus) -> acp::TerminalExitStatus {
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt as _;
        status.signal().map(|signal| signal.to_string())
    };
    #[cfg(not(unix))]
    let signal: Option<String> = None;
    acp::TerminalExitStatus::new()
        .exit_code(status.code().map(|code| code as u32))
        .signal(signal)
}

/// The updates written out here take their shapes from the ACP schema
/// (`agent-client-protocol-schema` 1.18.0, `v1::schema`), whose types parse
/// each one; their contents are made up.
#[cfg(test)]
mod tests;
