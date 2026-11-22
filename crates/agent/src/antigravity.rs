//! Google Antigravity CLI (`agy`): `agy acp [--process-per-session] [flags]`,
//! an ACP agent with Antigravity's `_meta.antigravity` extensions, session
//! resumption, steering, and headless `/usage` quota queries.
//!
//! The protocol machinery is [`crate::acp_session`]; this module is
//! Antigravity's dialect.

pub(crate) mod plugins;
mod record;
mod subagents;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agent_client_protocol::v1::schema as acp;
use serde_json::{Map, Value, json};
use smol::channel::Sender;

use crate::acp_session::{
    self, Connection, Dialect, Established, Launch, Session, Setup, State, TurnEnd, describe,
    is_auth_required, prompt_status,
};
use crate::{
    AgentError, AgentEvent, Attachment, ItemContent, ItemStatus, LaunchEnv, ModelSpec,
    ProviderKind, ResumeCursor, SessionHandle, SessionOptions, SessionProcesses, ThreadItem,
    TokenUsage,
};

/// The directories the Antigravity CLI launched with `launch_env` keeps its
/// configuration and conversation state in: `~/.gemini/antigravity-cli`.
pub fn state_dirs(launch_env: &LaunchEnv) -> Vec<PathBuf> {
    launch_env
        .child_home(ProviderKind::Antigravity)
        .map(|home| home.join(".gemini").join("antigravity-cli"))
        .into_iter()
        .collect()
}

/// The user-level MCP configuration file Antigravity CLI documents:
/// `~/.gemini/config/mcp_config.json`.
pub fn mcp_config_path(launch_env: &LaunchEnv) -> Option<PathBuf> {
    launch_env
        .child_home(ProviderKind::Antigravity)
        .map(|home| home.join(".gemini").join("config").join("mcp_config.json"))
}

/// Start (or resume) an Antigravity session.
pub async fn start(opts: SessionOptions) -> Result<SessionHandle, AgentError> {
    if opts.fork.is_some() {
        return Err(AgentError::Protocol(
            "session fork is not supported for Antigravity".into(),
        ));
    }
    acp_session::start(ProviderKind::Antigravity, Antigravity::default(), opts).await
}

/// The models Antigravity's `session/new` advertises for the signed-in account.
/// Their options (`thought_level`, `mode`) arrive over the wire once a session
/// starts ([`crate::OptionDescriptors::Wire`]).
pub async fn list_models(
    binary_path: Option<PathBuf>,
    launch_env: LaunchEnv,
    processes: &SessionProcesses,
) -> Result<Vec<ModelSpec>, AgentError> {
    outside_a_thread(
        binary_path,
        launch_env,
        processes,
        async move |connection, init, cwd| {
            let new = open_session(connection, init, cwd)
                .await?
                .ok_or_else(signed_out)?;
            Ok(acp_session::catalog(
                ProviderKind::Antigravity,
                new.modes.as_ref(),
                new.config_options.as_deref(),
            )
            .models)
        },
    )
    .await
}

/// Whether the agent opens a session for the account it is launched with.
/// The agent answers at once either way; `agy --print /usage` waits a minute
/// for a sign-in before it says there is none (1.5.14).
pub async fn signed_in(
    binary_path: Option<PathBuf>,
    launch_env: LaunchEnv,
) -> Result<bool, AgentError> {
    outside_a_thread(
        binary_path,
        launch_env,
        &SessionProcesses::default(),
        async move |connection, init, cwd| Ok(open_session(connection, init, cwd).await?.is_some()),
    )
    .await
}

/// Sign the account out with the agent's own `logout`, which forgets the
/// sign-in for every `agy` that shares it.
pub async fn sign_out(
    binary_path: Option<PathBuf>,
    launch_env: LaunchEnv,
) -> Result<(), AgentError> {
    outside_a_thread(
        binary_path,
        launch_env,
        &SessionProcesses::default(),
        async move |connection, init, _cwd| {
            if init.agent_capabilities.auth.logout.is_none() {
                return Err(AgentError::Provider(
                    "This Antigravity version does not sign out over ACP. Run `/logout` in `agy`."
                        .into(),
                ));
            }
            connection
                .send_request(acp::LogoutRequest::new())
                .block_task()
                .await
                .map(|_| ())
                .map_err(|err| {
                    AgentError::Provider(format!(
                        "Antigravity could not sign out: {}",
                        describe(&err)
                    ))
                })
        },
    )
    .await
}

/// Run `ask` against an `agy acp` started for no thread, in a directory of
/// its own that is removed afterwards.
async fn outside_a_thread<R: Send + 'static>(
    binary_path: Option<PathBuf>,
    launch_env: LaunchEnv,
    processes: &SessionProcesses,
    ask: impl AsyncFnOnce(&Connection, &acp::InitializeResponse, &Path) -> Result<R, AgentError>
    + Send
    + 'static,
) -> Result<R, AgentError> {
    let cwd = std::env::temp_dir().join(format!("tcode-agy-probe-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).map_err(|err| {
        AgentError::Spawn(format!(
            "could not make a directory to start Antigravity in: {err}"
        ))
    })?;
    let opts = SessionOptions {
        cwd: cwd.clone(),
        model: None,
        resume: None,
        fork: None,
        binary_path,
        option_selections: Vec::new(),
        mcp_servers: Vec::new(),
        launch_env,
        extra_args: Vec::new(),
        acp: None,
        instructions: None,
        processes: processes.clone(),
        clock: Default::default(),
        worktrees: Vec::new(),
        write_journal: None,
    };
    let session_cwd = cwd.clone();
    let answer = acp_session::query(
        ProviderKind::Antigravity,
        Antigravity::default(),
        opts,
        async move |connection, init| ask(connection, init, &session_cwd).await,
    )
    .await;
    if let Err(err) = crate::process::unblock(move || std::fs::remove_dir_all(&cwd)).await {
        log::warn!("could not remove Antigravity's probe directory: {err}");
    }
    answer
}

/// Open a session in `cwd` and close it again, for what `session/new` says
/// of the account. `None` when the agent asks for credentials it has none of.
async fn open_session(
    connection: &Connection,
    init: &acp::InitializeResponse,
    cwd: &Path,
) -> Result<Option<acp::NewSessionResponse>, AgentError> {
    let new_session = async || {
        connection
            .send_request(acp::NewSessionRequest::new(cwd.to_path_buf()))
            .block_task()
            .await
    };
    let mut new = new_session().await;
    if matches!(&new, Err(err) if is_auth_required(err)) && sign_in_with_api_key(connection).await {
        new = new_session().await;
    }
    let new = match new {
        Ok(new) => new,
        Err(err) if is_auth_required(&err) => return Ok(None),
        Err(err) => {
            return Err(AgentError::Protocol(format!(
                "Antigravity could not open a session: {}",
                describe(&err)
            )));
        }
    };
    if init.agent_capabilities.session_capabilities.close.is_some()
        && let Err(err) = connection
            .send_request(acp::CloseSessionRequest::new(new.session_id.clone()))
            .block_task()
            .await
    {
        log::debug!(
            "antigravity: session/close after a probe failed: {}",
            describe(&err)
        );
    }
    Ok(Some(new))
}

/// Ask a short-lived `agy --print /usage --output-format json` process for
/// account quota windows (handled locally by the CLI with zero model turns).
pub async fn read_usage(
    binary_path: Option<PathBuf>,
    launch_env: LaunchEnv,
) -> Result<Value, AgentError> {
    let binary = crate::resolve_binary(binary_path.as_deref(), "agy")?;
    let binary_display = binary.to_string_lossy().into_owned();
    let mut cmd = crate::process::command(&binary);
    cmd.arg("--print")
        .arg("/usage")
        .arg("--output-format")
        .arg("json")
        .current_dir(std::env::temp_dir());
    for (key, value) in launch_env.pairs(ProviderKind::Antigravity) {
        cmd.env(key, value);
    }
    let output = crate::process::tree_output(cmd)
        .await
        .map_err(|err| AgentError::Spawn(format!("spawning `{binary_display}`: {err}")))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    // A failed command still prints its envelope, whose `error` says why
    // (signed out, 1.5.14: `authentication failed or timed out`, exit 1, with
    // the sign-in page and its prompt on stderr).
    let envelope = serde_json::from_str(&stdout);
    if output.status.success() || envelope.is_ok() {
        return envelope.map_err(|err| {
            AgentError::Protocol(format!(
                "Antigravity usage response was not valid JSON: {err}"
            ))
        });
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(AgentError::Provider(if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        output.status.to_string()
    }))
}

/// Answer the agent's "Authentication required" with its API-key sign-in,
/// which reads `GEMINI_API_KEY` from the agent's own environment and is
/// refused when there is none. `agy acp` asks for credentials until a client
/// has chosen a sign-in method once, and keeps the choice afterwards (1.5.14:
/// with the key set and no Google login, `session/new` is refused before this
/// request and answered after it, in that process and the next).
async fn sign_in_with_api_key(connection: &Connection) -> bool {
    connection
        .send_request(acp::AuthenticateRequest::new("gemini-api-key"))
        .block_task()
        .await
        .is_ok()
}

fn signed_out() -> AgentError {
    AgentError::Provider(
        "Antigravity is not signed in. Sign in, or set GEMINI_API_KEY, in Settings → Harnesses → Antigravity, then try again."
            .into(),
    )
}

/// Build `agy acp` launch arguments: `--process-per-session` is passed when
/// HTTP MCP servers are attached (as Antigravity's ACP docs specify), and
/// `--dangerously-skip-permissions` when Full access is selected, unless
/// already supplied in `extra_args`.
pub(crate) fn launch_args(
    has_mcp_servers: bool,
    full_access: bool,
    extra_args: &[String],
) -> Vec<String> {
    let mut args = vec!["acp".to_string()];
    if has_mcp_servers && !extra_args.iter().any(|arg| arg == "--process-per-session") {
        args.push("--process-per-session".to_string());
    }
    if full_access
        && !extra_args
            .iter()
            .any(|arg| arg == "--dangerously-skip-permissions")
    {
        args.push("--dangerously-skip-permissions".to_string());
    }
    args.extend(extra_args.iter().cloned());
    args
}

/// Build the `_meta` object for `session/new` and `session/resume` carrying
/// `_meta.antigravity.systemPrompt.append` when the server advertises it.
pub(crate) fn session_meta(
    init: &acp::InitializeResponse,
    instructions: Option<&str>,
) -> Option<acp::Meta> {
    let instructions = instructions.filter(|text| !text.trim().is_empty())?;
    let supports_append = init
        .agent_capabilities
        .meta
        .as_ref()
        .and_then(|meta| meta.get("antigravity"))
        .and_then(|ag| ag.get("systemPrompt"))
        .and_then(|sp| sp.get("append"))
        .and_then(Value::as_bool)
        == Some(true);
    if !supports_append {
        return None;
    }
    let mut meta = Map::new();
    meta.insert(
        "antigravity".to_string(),
        json!({ "systemPrompt": { "append": instructions } }),
    );
    Some(meta)
}

#[derive(Default)]
struct Antigravity {
    /// The agent's own record of the session (`conversations/<id>.db` and
    /// `brain/<id>/.system_generated/logs/transcript.jsonl`).
    record: Mutex<Option<SessionRecord>>,
    /// Tracks the last seen step index and any open autonomous (proactive)
    /// turn so both `session_update` and the background watcher stay in sync.
    proactive: Arc<Mutex<subagents::ProactiveTracker>>,
    /// `invoke_subagent` calls observed on the ACP wire, keyed by `toolCallId`,
    /// so their immediate spawn-ack `completed` updates do not prematurely
    /// close the subagent item.
    spawned_subagents: Mutex<HashMap<String, subagents::SpawnedSubagent>>,
    /// Control channel for the session's background subagent/proactive watcher.
    watcher: Mutex<Option<Sender<subagents::WatcherCommand>>>,
}

#[derive(Clone)]
struct SessionRecord {
    path: PathBuf,
    transcript: PathBuf,
}

impl Antigravity {
    fn send_watcher(&self, command: subagents::WatcherCommand) {
        if let Some(tx) = self
            .watcher
            .lock()
            .unwrap_or_else(|held| held.into_inner())
            .as_ref()
        {
            let _ = tx.try_send(command);
        }
    }

    fn session_record(&self) -> Option<SessionRecord> {
        self.record
            .lock()
            .unwrap_or_else(|held| held.into_inner())
            .clone()
    }

    /// Whether `update` begins autonomous (proactive) work when no turn is
    /// open. Late `tool_call_update`s for a tool from an earlier turn (such as
    /// a `schedule` timer completing) stay on that tool and do not open a new
    /// turn.
    fn starts_proactive_turn(&self, state: &State, update: &acp::SessionUpdate) -> bool {
        if state.turn().is_some() {
            return false;
        }
        match update {
            acp::SessionUpdate::AgentMessageChunk(chunk)
            | acp::SessionUpdate::AgentThoughtChunk(chunk) => !chunk_is_empty(&chunk.content),
            acp::SessionUpdate::ToolCall(call) => {
                !state.has_tool(&call.tool_call_id.0)
                    && !self
                        .spawned_subagents
                        .lock()
                        .unwrap_or_else(|held| held.into_inner())
                        .contains_key(call.tool_call_id.0.as_ref())
            }
            acp::SessionUpdate::ToolCallUpdate(update) => {
                !state.has_tool(&update.tool_call_id.0)
                    && !self
                        .spawned_subagents
                        .lock()
                        .unwrap_or_else(|held| held.into_inner())
                        .contains_key(update.tool_call_id.0.as_ref())
            }
            _ => false,
        }
    }
}

fn chunk_is_empty(block: &acp::ContentBlock) -> bool {
    match block {
        acp::ContentBlock::Text(text) => text.text.is_empty(),
        acp::ContentBlock::ResourceLink(link) => link.name.is_empty() && link.uri.is_empty(),
        acp::ContentBlock::Resource(resource) => match &resource.resource {
            acp::EmbeddedResourceResource::TextResourceContents(text) => text.text.is_empty(),
            _ => true,
        },
        _ => true,
    }
}

impl Dialect for Antigravity {
    fn name(&self) -> &str {
        "Antigravity"
    }

    fn launch(&self, opts: &SessionOptions) -> Result<Launch, AgentError> {
        let program = crate::resolve_binary(opts.binary_path.as_deref(), "agy")?;
        let full_access = opts.option_selections.iter().any(|selection| {
            selection.id == "mode"
                && selection.value.as_str() == Some(crate::ANTIGRAVITY_FULL_ACCESS_MODE)
        });
        let args = launch_args(!opts.mcp_servers.is_empty(), full_access, &opts.extra_args);
        Ok(Launch {
            program,
            args,
            env: Vec::new(),
            client_meta: None,
        })
    }

    fn accepts_http_mcp(&self, _init: &acp::InitializeResponse) -> bool {
        // `agy acp` omits `mcpCapabilities.http` from `initialize` yet connects
        // to HTTP `mcpServers` passed in `session/new` and `session/resume`.
        true
    }

    async fn establish(&self, setup: &Setup<'_>) -> Result<Established, AgentError> {
        let opts = setup.opts;
        let mcp_servers = setup.mcp_servers().await;
        let meta = session_meta(setup.init, opts.instructions.as_deref());
        if meta.is_none()
            && opts
                .instructions
                .as_deref()
                .is_some_and(|text| !text.trim().is_empty())
        {
            // The session carries Tcode's instructions nowhere else, and the
            // thread's status lists them as given.
            setup
                .warn(
                    "This Antigravity version takes no appended session instructions, so Tcode's instructions for its tools were not given to the model"
                        .into(),
                )
                .await;
        }
        let resumed_id = opts
            .resume
            .as_ref()
            .and_then(|cursor| cursor.0.get("session_id"))
            .and_then(Value::as_str)
            .map(str::to_string);

        let mut established_session = None;
        let mut anew = false;
        let resumed = resumed_id.is_some();
        if let Some(session_id) = resumed_id {
            let session_id = acp::SessionId::new(session_id);
            let mut reattached = setup
                .reattach(&session_id, &mcp_servers, meta.as_ref())
                .await;
            if matches!(&reattached, Some(Err(err)) if is_auth_required(err))
                && sign_in_with_api_key(setup.connection).await
            {
                reattached = setup
                    .reattach(&session_id, &mcp_servers, meta.as_ref())
                    .await;
            }
            match reattached {
                Some(Ok((modes, config_options))) => {
                    established_session = Some((session_id, modes, config_options));
                }
                Some(Err(err)) => {
                    log::warn!(
                        "antigravity: the session could not be resumed ({}); starting a fresh session",
                        describe(&err)
                    );
                    setup
                        .warn(
                            "Antigravity could not resume the previous conversation; starting a new one"
                                .into(),
                        )
                        .await;
                    anew = true;
                }
                None => {}
            }
        }

        let (session_id, modes, config_options) = match established_session {
            Some(session) => session,
            None => {
                let new_session = async || {
                    let mut request = acp::NewSessionRequest::new(opts.cwd.clone())
                        .mcp_servers(mcp_servers.clone());
                    if let Some(meta) = &meta {
                        request = request.meta(meta.clone());
                    }
                    setup.connection.send_request(request).block_task().await
                };
                let mut new = new_session().await;
                if matches!(&new, Err(err) if is_auth_required(err))
                    && sign_in_with_api_key(setup.connection).await
                {
                    new = new_session().await;
                }
                let new = new.map_err(|err| {
                    if is_auth_required(&err) {
                        signed_out()
                    } else {
                        AgentError::Protocol(format!(
                            "Antigravity could not start a session: {}",
                            describe(&err)
                        ))
                    }
                })?;
                (new.session_id, new.modes, new.config_options)
            }
        };

        setup.adopt(modes.as_ref(), config_options.as_deref());
        let mut config_options = config_options.unwrap_or_default();
        if let Some(model) = &opts.model
            && let Some(selected) = setup
                .select_model(&session_id, model, &config_options)
                .await
        {
            config_options = selected;
        }
        setup
            .restore_options(&session_id, modes.as_ref(), &config_options)
            .await;

        if let Some(state_dir) = state_dirs(&opts.launch_env).into_iter().next() {
            let path = record::path(&state_dir, &session_id.0);
            let transcript = record::transcript_path(&state_dir, &session_id.0);
            // A session that goes on has steps already; a new one has none.
            let seen = if resumed && !anew {
                let path = path.clone();
                crate::process::unblock(move || record::last_step(&path))
                    .await
                    .map(|step| step.index)
            } else {
                Some(-1)
            };
            *self.record.lock().unwrap_or_else(|held| held.into_inner()) =
                Some(SessionRecord { path, transcript });
            self.proactive
                .lock()
                .unwrap_or_else(|held| held.into_inner())
                .seen = seen;

            let (tx, rx) = smol::channel::unbounded();
            *self.watcher.lock().unwrap_or_else(|held| held.into_inner()) = Some(tx);
            let watcher_state = setup.state.clone();
            let watcher_proactive = self.proactive.clone();
            let watcher_events = setup.events.clone();
            let sid = session_id.0.to_string();
            smol::spawn(subagents::run_watcher(
                state_dir,
                sid,
                watcher_state,
                watcher_proactive,
                watcher_events,
                rx,
            ))
            .detach();
        }

        Ok(Established {
            resume: ResumeCursor(json!({
                "session_id": session_id.0.to_string(),
            })),
            session_id,
            anew,
        })
    }

    fn session_update(
        &self,
        state: &mut State,
        notification: acp::SessionNotification,
    ) -> Vec<AgentEvent> {
        if state.loading() {
            return Vec::new();
        }

        let mut events = Vec::new();
        if self.starts_proactive_turn(state, &notification.update) {
            let turn_id = format!("proactive-{}", uuid::Uuid::new_v4());
            let mut tracker = self
                .proactive
                .lock()
                .unwrap_or_else(|held| held.into_inner());
            let since = tracker.seen.unwrap_or(-1);
            tracker.active = Some(subagents::ActiveProactive {
                turn_id: turn_id.clone(),
                since,
                usage_seen: false,
            });
            drop(tracker);
            events.extend(state.begin_agent_turn(&turn_id));
        }

        match &notification.update {
            acp::SessionUpdate::ToolCall(call) => {
                if let Some(spawn) = subagents::parse_spawn(call) {
                    events.extend(state.flush_text());
                    if call.status == acp::ToolCallStatus::Failed {
                        events.push(AgentEvent::ItemCompleted(ThreadItem {
                            id: spawn.tool_call_id,
                            parent_item_id: None,
                            content: ItemContent::Subagent {
                                agent_type: spawn.agent_type,
                                description: spawn.description,
                                status: ItemStatus::Failed,
                                summary: None,
                                model: spawn.model,
                                effort: None,
                            },
                        }));
                    } else {
                        events.extend(subagents::spawn_events(&spawn));
                        self.spawned_subagents
                            .lock()
                            .unwrap_or_else(|held| held.into_inner())
                            .insert(spawn.tool_call_id.clone(), spawn.clone());
                        self.send_watcher(subagents::WatcherCommand::Spawned(spawn));
                    }
                    return events;
                }
                if let Some((recipient, message)) = subagents::parse_send_message(call) {
                    self.send_watcher(subagents::WatcherCommand::MessageSent {
                        recipient,
                        message,
                    });
                } else if subagents::is_manage_subagents(call) {
                    self.send_watcher(subagents::WatcherCommand::Wake);
                }
            }
            acp::SessionUpdate::ToolCallUpdate(update) => {
                let id = update.tool_call_id.0.as_ref();
                let spawn = self
                    .spawned_subagents
                    .lock()
                    .unwrap_or_else(|held| held.into_inner())
                    .get(id)
                    .cloned();
                if let Some(spawn) = spawn {
                    if update.fields.status == Some(acp::ToolCallStatus::Failed) {
                        self.spawned_subagents
                            .lock()
                            .unwrap_or_else(|held| held.into_inner())
                            .remove(id);
                        events.push(AgentEvent::ItemCompleted(ThreadItem {
                            id: spawn.tool_call_id,
                            parent_item_id: None,
                            content: ItemContent::Subagent {
                                agent_type: spawn.agent_type,
                                description: spawn.description,
                                status: ItemStatus::Failed,
                                summary: None,
                                model: spawn.model,
                                effort: None,
                            },
                        }));
                    }
                    // The immediate `completed` update on `invoke_subagent`
                    // only acknowledges the async spawn; the watcher settles
                    // the subagent item from its on-disk trajectory.
                    return events;
                }
                if state.tool_title(id) == Some("Run manage_subagents") {
                    self.send_watcher(subagents::WatcherCommand::Wake);
                }
            }
            acp::SessionUpdate::AgentMessageChunk(_) | acp::SessionUpdate::AgentThoughtChunk(_) => {
                if let Some(active) = self
                    .proactive
                    .lock()
                    .unwrap_or_else(|held| held.into_inner())
                    .active
                    .as_mut()
                {
                    active.usage_seen = false;
                }
            }
            _ => {}
        }

        let is_usage_update = matches!(notification.update, acp::SessionUpdate::UsageUpdate(_));
        events.extend(state.apply_update(notification.update));

        if is_usage_update {
            let active_turn = {
                let mut tracker = self
                    .proactive
                    .lock()
                    .unwrap_or_else(|held| held.into_inner());
                tracker.active.as_mut().map(|active| {
                    active.usage_seen = true;
                    (active.turn_id.clone(), active.since)
                })
            };
            if let Some((turn_id, since)) = active_turn
                && let Some(rec) = self.session_record()
            {
                if let Some(end) =
                    record::proactive_turn_end(&rec.path, &rec.transcript, since, state.usage())
                {
                    let mut tracker = self
                        .proactive
                        .lock()
                        .unwrap_or_else(|held| held.into_inner());
                    if tracker
                        .active
                        .as_ref()
                        .is_some_and(|active| active.turn_id == turn_id)
                    {
                        tracker.active = None;
                        tracker.seen = Some(end.step_index);
                        drop(tracker);
                        events.extend(state.end_turn(
                            &turn_id,
                            TurnEnd {
                                status: end.status,
                                message: None,
                                usage: end.usage,
                            },
                        ));
                    }
                } else {
                    self.send_watcher(subagents::WatcherCommand::Wake);
                }
            }
        }

        events
    }

    fn turn_end(
        &self,
        state: &mut State,
        result: Result<acp::PromptResponse, agent_client_protocol::Error>,
    ) -> TurnEnd {
        let (mut status, message) = prompt_status(&result);
        {
            let mut tracker = self
                .proactive
                .lock()
                .unwrap_or_else(|held| held.into_inner());
            tracker.active = None;
            if status == crate::TurnStatus::Completed
                && let Some(rec) = self.session_record()
                && let Some(last) = record::last_step(&rec.path)
            {
                // The error itself is the message the agent sent with the turn.
                if last.error && tracker.seen.is_some_and(|seen| last.index > seen) {
                    status = crate::TurnStatus::Failed;
                }
                tracker.seen = Some(last.index);
            }
        }
        let context = state.usage();
        let usage = result
            .ok()
            .and_then(|response| response.usage)
            .map(|usage| TokenUsage {
                freshness: crate::ContextFreshness::Current,
                turn_processed_tokens: Some(usage.total_tokens),
                input_tokens: Some(usage.input_tokens),
                cached_input_tokens: usage.cached_read_tokens,
                output_tokens: Some(usage.output_tokens),
                used_tokens: context.and_then(|context| context.used_tokens),
                context_window: context.and_then(|context| context.context_window),
                cost_usd: context.and_then(|context| context.cost_usd),
                ..TokenUsage::default()
            })
            .or(context);
        TurnEnd {
            status,
            message,
            usage,
        }
    }

    async fn steer(
        &self,
        session: &Session,
        session_id: &acp::SessionId,
        request_id: String,
        text: String,
        attachments: Vec<Attachment>,
    ) {
        // `agy acp` steers a running turn when a second `session/prompt` arrives
        // on the same session: it immediately answers the superseded prompt with
        // `end_turn` and keeps streaming until the steering prompt completes.
        if session.steer_prompt(session_id, &text, &attachments) {
            session.emit(AgentEvent::SteerAccepted { request_id }).await;
        } else {
            session.emit(AgentEvent::SteerRejected { request_id }).await;
        }
    }

    fn owned_config_options(&self) -> &'static [&'static str] {
        &[acp_session::MODEL_OPTION_ID]
    }

    fn on_close(&self) {
        if let Some(tx) = self
            .watcher
            .lock()
            .unwrap_or_else(|held| held.into_inner())
            .take()
        {
            let _ = tx.try_send(subagents::WatcherCommand::Close);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notification(update: Value) -> acp::SessionNotification {
        serde_json::from_value(json!({
            "sessionId": "session-1",
            "update": update,
        }))
        .unwrap()
    }

    #[test]
    fn launch_args_pass_process_per_session_and_full_access_without_duplicates() {
        assert_eq!(launch_args(false, false, &[]), vec!["acp"]);
        assert_eq!(
            launch_args(true, false, &[]),
            vec!["acp", "--process-per-session"]
        );
        assert_eq!(
            launch_args(false, true, &[]),
            vec!["acp", "--dangerously-skip-permissions"]
        );
        assert_eq!(
            launch_args(true, true, &["--sandbox".into()]),
            vec![
                "acp",
                "--process-per-session",
                "--dangerously-skip-permissions",
                "--sandbox"
            ]
        );
        assert_eq!(
            launch_args(
                true,
                true,
                &[
                    "--process-per-session".into(),
                    "--dangerously-skip-permissions".into(),
                    "--sandbox".into()
                ]
            ),
            vec![
                "acp",
                "--process-per-session",
                "--dangerously-skip-permissions",
                "--sandbox"
            ]
        );
    }

    /// `_meta.antigravity.systemPrompt.append` is sent only when `initialize`
    /// advertises it (observed in `agy acp` 1.5.13).
    #[test]
    fn session_meta_carries_system_prompt_append_only_when_advertised() {
        let advertised: acp::InitializeResponse = serde_json::from_value(json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "_meta": {
                    "antigravity": {
                        "tier": true,
                        "systemPrompt": { "append": true }
                    }
                }
            }
        }))
        .unwrap();
        let unadvertised: acp::InitializeResponse = serde_json::from_value(json!({
            "protocolVersion": 1,
            "agentCapabilities": {}
        }))
        .unwrap();

        let meta = session_meta(&advertised, Some("Use project conventions.")).unwrap();
        assert_eq!(
            meta.get("antigravity"),
            Some(&json!({ "systemPrompt": { "append": "Use project conventions." } }))
        );
        assert!(session_meta(&advertised, Some("   ")).is_none());
        assert!(session_meta(&unadvertised, Some("Use project conventions.")).is_none());
    }

    /// `session/prompt.usage.totalTokens` is per-turn, not cumulative: leaving
    /// `total_processed_tokens` empty lets `Timeline` accumulate across turns.
    #[test]
    fn turn_end_reports_per_turn_processed_tokens() {
        let dialect = Antigravity::default();
        let mut state = State::new(ProviderKind::Antigravity, PathBuf::from("/repo"));
        let _ = dialect.session_update(
            &mut state,
            notification(json!({
                "sessionUpdate": "usage_update",
                "used": 14836,
                "size": 1048576
            })),
        );
        let response: acp::PromptResponse = serde_json::from_value(json!({
            "stopReason": "end_turn",
            "usage": {
                "inputTokens": 2236,
                "cachedReadTokens": 12568,
                "outputTokens": 32,
                "thoughtTokens": 28,
                "totalTokens": 14836
            }
        }))
        .unwrap();

        let end = dialect.turn_end(&mut state, Ok(response));
        let usage = end.usage.expect("turn usage");
        assert_eq!(usage.turn_processed_tokens, Some(14836));
        assert_eq!(usage.total_processed_tokens, None);
        assert_eq!(usage.input_tokens, Some(2236));
        assert_eq!(usage.cached_input_tokens, Some(12568));
        assert_eq!(usage.output_tokens, Some(32));
        assert_eq!(usage.used_tokens, Some(14836));
        assert_eq!(usage.context_window, Some(1_048_576));
    }

    /// Autonomous updates after a prompt returns open a proactive turn and
    /// close it once the terminal planner step completes, while a late
    /// `tool_call_update` on a previous turn's `schedule` tool does not open a
    /// turn of its own.
    #[test]
    fn proactive_reply_opens_and_closes_an_agent_turn() {
        let dir =
            std::env::temp_dir().join(format!("tcode-agy-proactive-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("session.db");
        let transcript = dir.join("transcript.jsonl");
        let dialect = Antigravity::default();
        *dialect.record.lock().unwrap() = Some(SessionRecord {
            path: db.clone(),
            transcript: transcript.clone(),
        });
        dialect.proactive.lock().unwrap().seen = Some(3);

        let mut state = State::new(ProviderKind::Antigravity, PathBuf::from("/repo"));
        // Seed a `schedule` tool from turn 1 that stayed in_progress.
        let _ = state.begin_agent_turn("turn-1");
        let _ = dialect.session_update(
            &mut state,
            notification(json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call_sched",
                "title": "Run schedule",
                "kind": "other",
                "status": "in_progress"
            })),
        );
        let _ = state.end_turn(
            "turn-1",
            TurnEnd {
                status: crate::TurnStatus::Completed,
                message: None,
                usage: None,
            },
        );
        assert_eq!(state.turn(), None);

        // Late `schedule` completion does not open a proactive turn.
        let sched_done = dialect.session_update(
            &mut state,
            notification(json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_sched",
                "status": "completed"
            })),
        );
        assert!(matches!(
            sched_done.as_slice(),
            [AgentEvent::ItemCompleted(_)]
        ));
        assert_eq!(state.turn(), None);

        // Autonomous reply text opens a proactive turn.
        let opened = dialect.session_update(
            &mut state,
            notification(json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "Timer fired!" }
            })),
        );
        assert!(matches!(
            opened.as_slice(),
            [AgentEvent::TurnStarted { .. }, AgentEvent::Delta { .. }]
        ));
        assert!(state.turn().is_some());

        // Write the completed terminal planner step to transcript.jsonl, then
        // deliver `usage_update`.
        std::fs::write(
            &transcript,
            concat!(
                r#"{"step_index":5,"type":"PLANNER_RESPONSE","status":"DONE","input_tokens":100,"cache_read_tokens":50,"output_tokens":12,"content":"Timer fired!"}"#,
                "\n",
            ),
        )
        .unwrap();
        let closed = dialect.session_update(
            &mut state,
            notification(json!({
                "sessionUpdate": "usage_update",
                "used": 162,
                "size": 1048576
            })),
        );
        assert!(closed.iter().any(|event| matches!(
            event,
            AgentEvent::TurnCompleted {
                status: crate::TurnStatus::Completed,
                ..
            }
        )));
        assert_eq!(state.turn(), None);

        std::fs::remove_dir_all(dir).unwrap();
    }

    /// `invoke_subagent` emits a `Subagent` item in progress and ignores the
    /// immediate `completed` spawn-ack `tool_call_update`.
    #[test]
    fn invoke_subagent_maps_to_subagent_item_and_suppresses_spawn_ack() {
        let dialect = Antigravity::default();
        let mut state = State::new(ProviderKind::Antigravity, PathBuf::from("/repo"));
        let _ = state.begin_agent_turn("turn-1");

        let started = dialect.session_update(
            &mut state,
            notification(json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call_sub",
                "title": "Run invoke_subagent",
                "kind": "other",
                "status": "in_progress",
                "rawInput": {
                    "TypeName": "research",
                    "Role": "Explorer",
                    "Prompt": "Check README.md"
                }
            })),
        );
        assert!(matches!(
            started.as_slice(),
            [
                AgentEvent::ItemStarted(ThreadItem {
                    id,
                    parent_item_id: None,
                    content: ItemContent::Subagent {
                        agent_type,
                        description,
                        status: ItemStatus::InProgress,
                        ..
                    }
                }),
                AgentEvent::ItemCompleted(ThreadItem {
                    parent_item_id: Some(parent),
                    content: ItemContent::UserMessage { text, .. },
                    ..
                })
            ] if id == "call_sub" && agent_type == "research" && description == "Explorer"
                && parent == "call_sub" && text == "Check README.md"
        ));

        let ack = dialect.session_update(
            &mut state,
            notification(json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_sub",
                "status": "completed"
            })),
        );
        assert!(ack.is_empty());
    }
}
