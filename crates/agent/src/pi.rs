//! Native pi provider: persistent `pi --mode rpc` JSONL transport.
//!
//! RPC records are framed by LF only. Extension confirmation UI is transported generically.

use crate::{LaterHistory, RewindFailure, RewindFailureReason, RewindMode};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use smol::channel::{Receiver, Sender};

use crate::actor::{self, EventSenderExt as _, SessionActor, TransportOutcome};
use crate::process::{ChildOutput, ProcessTree, send_json as write_json, spawn_line_reader};
use crate::{
    AgentError, AgentEvent, ApprovalDecision, ApprovalKind, ApprovalOption, ApprovalOptionKind,
    ApprovalRequest, Attachment, DeltaKind, FileChange, FileChangeKind, ForkPoint, ItemContent,
    ItemStatus, LaunchEnv, ModelSpec, OptionDescriptor, OptionSelection, ProviderCommand,
    ProviderCommandKind, ProviderKind, ResumeCursor, SearchHit, SearchScope, SelectOption,
    SessionCommand, SessionHandle, SessionOptions, ThreadItem, TokenUsage, ToolFamily, ToolOutput,
    UserInputDelivery, UserInputOption, UserInputQuestion,
};

/// The directory pi launched with `launch_env` keeps its settings, sign-in
/// and sessions in: `PI_CODING_AGENT_DIR`, else `~/.pi/agent`.
pub fn agent_dir(launch_env: &LaunchEnv) -> Option<PathBuf> {
    launch_env
        .child_path(ProviderKind::Pi, "PI_CODING_AGENT_DIR")
        .or_else(|| {
            launch_env
                .child_home(ProviderKind::Pi)
                .map(|home| home.join(".pi/agent"))
        })
}

pub async fn start(opts: SessionOptions) -> Result<SessionHandle, AgentError> {
    crate::spawn_session(
        ProviderKind::Pi,
        opts,
        run_actor,
        "pi actor exited before reporting startup status",
    )
    .await
}

pub async fn list_models(
    binary_path: Option<PathBuf>,
    launch_env: LaunchEnv,
    processes: &crate::SessionProcesses,
) -> Result<Vec<ModelSpec>, AgentError> {
    let processes = processes.clone();
    crate::process::unblock(move || {
        list_models_blocking(binary_path.as_deref(), &launch_env, &processes)
    })
    .await
}

fn list_models_blocking(
    binary_path: Option<&Path>,
    launch_env: &LaunchEnv,
    processes: &crate::SessionProcesses,
) -> Result<Vec<ModelSpec>, AgentError> {
    let binary = crate::resolve_binary(binary_path, "pi")?;
    let mut cmd = crate::process::harness_command(&binary);
    cmd.arg("--mode")
        .arg("rpc")
        .arg("--no-session")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in launch_env.pairs(ProviderKind::Pi) {
        cmd.env(key, value);
    }
    let mut child = processes
        .hold(ProcessTree::new(cmd.spawn().map_err(|err| {
            AgentError::Spawn(format!("spawning `{}`: {err}", binary.display()))
        })?));
    let mut stdin = BufWriter::new(
        child
            .stdin
            .take()
            .ok_or_else(|| AgentError::Spawn("pi child stdin missing".into()))?,
    );
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AgentError::Spawn("pi child stdout missing".into()))?;
    send_json(&mut stdin, &json!({"id":"state","type":"get_state"}))?;
    send_json(
        &mut stdin,
        &json!({"id":"models","type":"get_available_models"}),
    )?;

    let mut current = None;
    let mut thinking_level = None;
    let mut catalog = None;
    let mut lines = BufReader::new(stdout).lines();
    while current.is_none() || catalog.is_none() {
        let line = lines.next().transpose()?.ok_or_else(|| {
            AgentError::Protocol("pi closed stdout during model discovery".into())
        })?;
        let message: Value = serde_json::from_str(&line)
            .map_err(|err| AgentError::Protocol(format!("invalid pi model response: {err}")))?;
        match message.get("id").and_then(Value::as_str) {
            Some("state") => {
                ensure_success(&message)?;
                current = Some(model_wire_id(message.pointer("/data/model")));
                thinking_level = message
                    .pointer("/data/thinkingLevel")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            Some("models") => {
                ensure_success(&message)?;
                catalog = Some(
                    message
                        .pointer("/data/models")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default(),
                );
            }
            _ => {}
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    let current = current.flatten();
    Ok(catalog
        .unwrap_or_default()
        .iter()
        .filter_map(|model| map_model(model, current.as_deref(), thinking_level.as_deref()))
        .collect())
}

fn map_model(
    model: &Value,
    current: Option<&str>,
    thinking_level: Option<&str>,
) -> Option<ModelSpec> {
    let id = model.get("id")?.as_str()?;
    let provider = model.get("provider")?.as_str()?;
    let wire_id = format!("{provider}/{id}");
    let mut options = Vec::new();
    if model
        .get("reasoning")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        // Pi uses provider defaults for missing keys; null disables a level.
        // Extended levels require an explicit mapping (getSupportedThinkingLevels).
        let levels: Vec<_> = ["off", "minimal", "low", "medium", "high", "xhigh", "max"]
            .into_iter()
            .filter(|level| {
                let mapped = model
                    .get("thinkingLevelMap")
                    .and_then(|map| map.get(*level));
                if matches!(*level, "xhigh" | "max") {
                    mapped.is_some_and(Value::is_string)
                } else {
                    !mapped.is_some_and(Value::is_null)
                }
            })
            .collect();
        let default_value = thinking_level
            .filter(|level| levels.contains(level))
            .map(str::to_owned);
        options.push(OptionDescriptor::Select {
            id: crate::EFFORT_OPTION_ID.into(),
            label: "Thinking".into(),
            options: levels
                .into_iter()
                .map(|level| SelectOption {
                    value: level.into(),
                    label: thinking_label(level),
                    description: None,
                    unavailable: None,
                    speed: None,
                    summary: None,
                    approval: None,
                })
                .collect(),
            default_value,
            role: crate::OptionRole::Effort,
            apply: crate::ApplyTiming::Live,
            recommended: None,
            permissive: None,
            minimum: None,
        });
    }
    Some(ModelSpec {
        id: wire_id.clone(),
        display_name: model
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(id)
            .to_owned(),
        is_default: current == Some(wire_id.as_str()),
        options,
    })
}

fn thinking_label(level: &str) -> String {
    if level == "xhigh" {
        return "Extra High".into();
    }
    let mut characters = level.chars();
    characters
        .next()
        .map(|first| first.to_uppercase().collect::<String>() + characters.as_str())
        .unwrap_or_default()
}

/// pi's thinking level in effect, as `get_state` and `thinking_level_changed`
/// give it (1.5.2): pi clamps a level the model lacks, and says so only by
/// these.
fn thinking_level(level: Option<&Value>) -> Option<AgentEvent> {
    let level = level?.as_str()?;
    Some(AgentEvent::ProviderOptions {
        descriptors: Vec::new(),
        selections: vec![OptionSelection {
            id: crate::EFFORT_OPTION_ID.into(),
            value: json!(level),
        }],
    })
}

fn model_wire_id(model: Option<&Value>) -> Option<String> {
    let model = model?;
    Some(format!(
        "{}/{}",
        model.get("provider")?.as_str()?,
        model.get("id")?.as_str()?
    ))
}

async fn run_actor(
    opts: SessionOptions,
    commands: Receiver<SessionCommand>,
    events: Sender<AgentEvent>,
    ready: Sender<Result<(), AgentError>>,
) {
    let binary = match crate::resolve_binary(opts.binary_path.as_deref(), "pi") {
        Ok(binary) => binary,
        Err(err) => {
            let _ = ready.send(Err(err)).await;
            return;
        }
    };
    let mut cmd = crate::process::harness_command(&binary);
    cmd.args(&opts.extra_args).arg("--mode").arg("rpc");
    // Which entry to fork before, or `None` for the leaf.
    let fork_at = match &opts.fork {
        Some(ForkPoint::Before { checkpoint_id }) => Some(Some(checkpoint_id.clone())),
        Some(ForkPoint::Tail) => Some(None),
        Some(ForkPoint::Branch { .. }) | None => None,
    };
    // A level set at launch is written to the session opened, which a fork
    // must leave as it found it: it goes to the fork once that is made.
    let thinking = selected_thinking(&opts.option_selections);
    if let Some(thinking) = thinking.filter(|_| fork_at.is_none()) {
        cmd.arg("--thinking").arg(thinking);
    }
    let session = match &opts.fork {
        Some(ForkPoint::Branch { token }) => {
            // pi would start an empty session at a path it does not find.
            if !Path::new(token).is_file() {
                let _ = ready
                    .send(Err(AgentError::Provider(format!(
                        "pi no longer has the session {token}"
                    ))))
                    .await;
                return;
            }
            Some(token.as_str())
        }
        _ => resume_session(&opts.resume),
    };
    if let Some(session) = session {
        cmd.arg("--session").arg(session);
    }
    cmd.current_dir(&opts.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in opts.launch_env.pairs(ProviderKind::Pi) {
        cmd.env(key, value);
    }
    if let Err(err) = crate::process::refuse_batch_line_breaks(cmd.get_program(), cmd.get_args()) {
        let _ = ready.send(Err(err)).await;
        return;
    }
    let mut child = match cmd.spawn() {
        Ok(child) => opts.processes.hold(ProcessTree::new(child)),
        Err(err) => {
            let _ = ready
                .send(Err(AgentError::Spawn(format!(
                    "spawning `{}`: {err}",
                    binary.display()
                ))))
                .await;
            return;
        }
    };
    let stdin = match child.stdin.take() {
        Some(stdin) => BufWriter::new(stdin),
        None => {
            let _ = ready
                .send(Err(AgentError::Spawn("pi child stdin missing".into())))
                .await;
            return;
        }
    };
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = ready
                .send(Err(AgentError::Spawn("pi child stdout missing".into())))
                .await;
            return;
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            let _ = ready
                .send(Err(AgentError::Spawn("pi child stderr missing".into())))
                .await;
            return;
        }
    };
    let (line_rx, _) = spawn_line_reader(stdout, "pi-rpc-stdout", None, true);
    let stderr_tail = crate::process::StderrTail::default();
    let _ = stderr_tail.spawn(stderr, "pi-rpc-stderr", "pi");

    let mut actor = PiActor {
        clock: opts.clock.clone(),
        child,
        stdin,
        lines: line_rx,
        events,
        mapper: PiMapper::new(),
        next_request: 1,
        pending_approvals: HashSet::new(),
        pending_dialogs: HashSet::new(),
        answered_by_pi_at: HashMap::new(),
        early_requests: Vec::new(),
        pending_steers: HashMap::new(),
        requested_model: opts.model.clone(),
        pending_settings: None,
        fork_at: fork_at.zip(Some(thinking.map(str::to_owned))),
        stderr_tail,
        requests: HashMap::new(),
        entries_cursor: None,
        session_file: None,
    };
    let startup = actor.initialize().await;
    if let Err(err) = startup {
        actor.stop();
        let details = actor.stderr_tail.append_to(err.to_string(), "\nstderr:\n");
        let _ = ready.send(Err(AgentError::Provider(details))).await;
        return;
    }
    let unattached = unattached_servers(&opts);
    if !unattached.is_empty() {
        actor.events
            .emit(AgentEvent::Warning {
                message: format!(
                    "pi has no MCP client of its own, so Tcode's {} tools are unavailable in this session",
                    unattached.join(", ")
                ),
            })
            .await;
    }
    if ready.send(Ok(())).await.is_err() {
        actor.stop();
        return;
    }
    for (request, arrived) in std::mem::take(&mut actor.early_requests) {
        actor.handle_extension_ui(&request, arrived).await;
    }

    actor::run(actor, &commands).await;
}

struct PiActor {
    clock: crate::Clock,
    child: ProcessTree<Child>,
    stdin: BufWriter<ChildStdin>,
    lines: Receiver<ChildOutput>,
    events: Sender<AgentEvent>,
    mapper: PiMapper,
    next_request: u64,
    pending_approvals: HashSet<String>,
    pending_dialogs: HashSet<String>,
    /// When pi itself settles an open dialog or confirmation that gave a
    /// `timeout`: it resolves it with its default and tells no client.
    answered_by_pi_at: HashMap<String, Instant>,
    /// Extension requests that arrived during the startup handshake, with
    /// when they did, put to the user once it is done.
    early_requests: Vec<(Value, Instant)>,
    /// Native prompt request id -> canonical steering request id. pi's prompt
    /// response is only an acceptance signal, but it is stronger than merely
    /// acknowledging the stdin write.
    pending_steers: HashMap<String, String>,
    requested_model: Option<String>,
    pending_settings: Option<PendingSettings>,
    /// The entry to fork before at startup, or `None` to clone at the leaf,
    /// and the thinking level to set in the fork.
    fork_at: Option<(Option<String>, Option<String>)>,
    stderr_tail: crate::process::StderrTail,
    /// RPC requests whose responses carry a rewind's or a restore point's
    /// next step, by request id.
    requests: HashMap<String, PiRequest>,
    /// The last session entry `get_entries` returned; the next read starts
    /// after it.
    entries_cursor: Option<String>,
    /// The session file pi works in now: what a fork leaves, to return to.
    session_file: Option<String>,
}

/// What a pi RPC response settles.
/// A live settings change pi has yet to answer in full.
struct PendingSettings {
    model: Option<String>,
    outstanding: HashSet<String>,
    applied: bool,
}

enum PiRequest {
    /// The session's new entries: the first user message after the cursor is
    /// the prompt of `turn`, when one just finished.
    Entries { turn: Option<String> },
    /// `fork` before the user message `checkpoint_id`, leaving `previous`.
    Fork {
        checkpoint_id: String,
        previous: Option<String>,
        /// The fork stops a running turn, which ends interrupted.
        stops_turn: bool,
    },
    /// `get_state` after a fork: the new session the conversation goes on in.
    Forked {
        checkpoint_id: String,
        prefill: Option<String>,
        previous: Option<String>,
    },
    /// `switch_session` back to the branch a fork left.
    Return { checkpoint_id: String },
    /// `get_state` after returning.
    Returned { checkpoint_id: String },
}

impl SessionActor for PiActor {
    type TransportItem = ChildOutput;

    fn transport(&self) -> &Receiver<Self::TransportItem> {
        &self.lines
    }

    fn events(&self) -> &Sender<AgentEvent> {
        &self.events
    }

    fn clock(&self) -> &crate::Clock {
        &self.clock
    }

    fn wake_at(&self) -> Option<Instant> {
        self.answered_by_pi_at
            .iter()
            .filter(|(id, _)| {
                self.pending_dialogs.contains(*id) || self.pending_approvals.contains(*id)
            })
            .map(|(_, at)| *at)
            .min()
    }

    /// pi resolved what timed out with its default (`rpc-mode.js`'s
    /// `createDialogPromise`): no value, or not confirmed.
    async fn wake(&mut self) -> Result<(), String> {
        let now = self.clock.now();
        let due: Vec<String> = self
            .answered_by_pi_at
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(id, _)| id.clone())
            .collect();
        for id in due {
            self.answered_by_pi_at.remove(&id);
            let event = if self.pending_dialogs.remove(&id) {
                AgentEvent::UserInputResolved {
                    request_id: id,
                    answers: serde_json::Map::new(),
                }
            } else if self.pending_approvals.remove(&id) {
                AgentEvent::ApprovalResolved {
                    request_id: id,
                    decision: ApprovalDecision::Cancel,
                }
            } else {
                continue;
            };
            self.events.emit(event).await;
        }
        Ok(())
    }

    fn command_failure_reason(&self) -> &'static str {
        "pi protocol write failed"
    }

    async fn handle_command(&mut self, command: SessionCommand) -> Result<(), String> {
        match command {
            SessionCommand::SendTurn {
                delivery_id,
                text,
                attachments,
                ..
            } => {
                let id = self.request_id();
                let mut request = json!({"id":id,"type":"prompt","message":text});
                if self.mapper.current_turn.is_some() {
                    request["streamingBehavior"] = json!("followUp");
                }
                attach_images(&mut request, attachments);
                send_json(&mut self.stdin, &request).map_err(|err| err.to_string())?;
                self.events
                    .emit(AgentEvent::TurnAccepted { delivery_id })
                    .await;
                Ok(())
            }
            SessionCommand::Steer {
                request_id,
                text,
                attachments,
            } => {
                let native_id = self.request_id();
                let mut request = json!({
                    "id": native_id,
                    "type":"prompt",
                    "message":text,
                    "streamingBehavior":"steer"
                });
                attach_images(&mut request, attachments);
                send_json(&mut self.stdin, &request).map_err(|err| err.to_string())?;
                self.pending_steers.insert(native_id, request_id);
                Ok(())
            }
            SessionCommand::Interrupt => {
                self.mapper.interrupt_pending = true;
                self.cancel_pending_ui().await?;
                send_json(&mut self.stdin, &json!({"type":"abort"})).map_err(|err| err.to_string())
            }
            SessionCommand::RespondApproval {
                request_id,
                decision,
            } => {
                let confirmed =
                    matches!(&decision, ApprovalDecision::Option(id) if id == "confirm");
                if !self.pending_approvals.remove(&request_id) {
                    return Ok(());
                }
                send_json(
                    &mut self.stdin,
                    &json!({"type":"extension_ui_response","id":request_id,"confirmed":confirmed}),
                )
                .map_err(|err| err.to_string())?;
                if decision == ApprovalDecision::Cancel {
                    self.mapper.interrupt_pending = true;
                    self.cancel_pending_ui().await?;
                    send_json(&mut self.stdin, &json!({"type":"abort"}))
                        .map_err(|err| err.to_string())?;
                }
                self.events
                    .emit(AgentEvent::ApprovalResolved {
                        request_id,
                        decision,
                    })
                    .await;
                Ok(())
            }
            SessionCommand::SetOption { id, value } if id == crate::EFFORT_OPTION_ID => {
                let Some(level) = value.as_str() else {
                    return Ok(());
                };
                let request_id = self.request_id();
                send_json(
                    &mut self.stdin,
                    &json!({"id":request_id,"type":"set_thinking_level","level":level}),
                )
                .map_err(|err| err.to_string())
            }
            SessionCommand::RespondUserInput {
                request_id,
                answers,
            } => {
                let answered = answer_dialog(
                    &mut self.stdin,
                    &mut self.pending_dialogs,
                    request_id,
                    answers,
                )
                .map_err(|err| err.to_string())?;
                if let Some(event) = answered {
                    self.events.emit(event).await;
                }
                Ok(())
            }
            SessionCommand::Rewind {
                checkpoint_id,
                mode: RewindMode::Conversation,
            } => {
                // pi's fork aborts a running turn; its end may not be told.
                let stops_turn =
                    self.mapper.current_turn.is_some() && !self.mapper.interrupt_pending;
                if stops_turn {
                    self.mapper.interrupt_pending = true;
                }
                let id = self.request_id();
                send_json(
                    &mut self.stdin,
                    &json!({"id":id,"type":"fork","entryId":checkpoint_id}),
                )
                .map_err(|err| err.to_string())?;
                self.requests.insert(
                    id,
                    PiRequest::Fork {
                        checkpoint_id,
                        previous: self.session_file.clone(),
                        stops_turn,
                    },
                );
                Ok(())
            }
            SessionCommand::UndoRewind {
                checkpoint_id,
                undo_token,
            } => {
                let id = self.request_id();
                send_json(
                    &mut self.stdin,
                    &json!({"id":id,"type":"switch_session","sessionPath":undo_token}),
                )
                .map_err(|err| err.to_string())?;
                self.requests
                    .insert(id, PiRequest::Return { checkpoint_id });
                Ok(())
            }
            command @ (SessionCommand::Rewind { .. } | SessionCommand::PreviewRewind { .. }) => {
                if let Some(event) = command
                    .rewind_refusal(RewindFailure::new(RewindFailureReason::Unsupported, None))
                {
                    self.events.emit(event).await;
                }
                Ok(())
            }
            // Progress arrives as `compaction_start` / `compaction_end`.
            SessionCommand::Compact => send_json(
                &mut self.stdin,
                &json!({"id":"tcode-compact","type":"compact"}),
            )
            .map_err(|err| err.to_string()),
            SessionCommand::ApplySettings {
                model,
                option_selections,
            } => self.apply_settings(model, &option_selections),
            SessionCommand::RetryTurn { .. }
            | SessionCommand::SetOption { .. }
            | SessionCommand::StopBackgroundTask { .. }
            | SessionCommand::MoveToBackground { .. }
            | SessionCommand::MoveTo { .. }
            | SessionCommand::RespondWorktree { .. }
            | SessionCommand::SetGoal { .. }
            | SessionCommand::ClearGoal
            | SessionCommand::ReadAccountUsage { .. }
            | SessionCommand::Shutdown => Ok(()),
        }
    }

    async fn handle_transport(
        &mut self,
        item: Result<ChildOutput, smol::channel::RecvError>,
    ) -> TransportOutcome {
        match item {
            Ok(ChildOutput::Line(line)) => {
                self.handle_line(&line).await;
                TransportOutcome::Continue
            }
            Ok(ChildOutput::Error(err)) => TransportOutcome::Fatal(err),
            Ok(ChildOutput::Eof) | Err(_) => {
                TransportOutcome::Closed("pi RPC process closed stdout".into())
            }
        }
    }

    async fn teardown(mut self, reason: Option<String>) -> Option<String> {
        self.stop();
        reason.map(|base| self.stderr_tail.append_to(base, "\nstderr:\n"))
    }
}

impl PiActor {
    async fn initialize(&mut self) -> Result<(), AgentError> {
        // Before anything that writes to the session open now, the source.
        if let Some((entry_id, thinking)) = self.fork_at.take() {
            let request = match entry_id {
                Some(entry_id) => json!({"id":"tcode-fork","type":"fork","entryId":entry_id}),
                None => json!({"id":"tcode-fork","type":"clone"}),
            };
            send_json(&mut self.stdin, &request)?;
            let forked = self.wait_response("tcode-fork").await?;
            if forked.pointer("/data/cancelled") == Some(&json!(true)) {
                return Err(AgentError::Provider(
                    "an extension cancelled the fork".into(),
                ));
            }
            if let Some(level) = thinking {
                send_json(
                    &mut self.stdin,
                    &json!({"id":"tcode-thinking","type":"set_thinking_level","level":level}),
                )?;
                self.wait_response("tcode-thinking").await?;
            }
        }
        if let Some(model) = self.requested_model.as_deref() {
            let (provider, model_id) = model.split_once('/').ok_or_else(|| {
                AgentError::Protocol(format!(
                    "pi model `{model}` must use the provider/model format"
                ))
            })?;
            send_json(
                &mut self.stdin,
                &json!({
                    "id":"tcode-model",
                    "type":"set_model",
                    "provider":provider,
                    "modelId":model_id
                }),
            )?;
            self.wait_response("tcode-model").await?;
        }
        send_json(
            &mut self.stdin,
            &json!({"id":"tcode-state","type":"get_state"}),
        )?;
        let state = self.wait_response("tcode-state").await?;
        let data = state.get("data").unwrap_or(&Value::Null);
        let session_id = data
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| AgentError::Protocol("pi get_state omitted sessionId".into()))?
            .to_owned();
        let mut cursor = serde_json::Map::new();
        cursor.insert("session_id".into(), Value::String(session_id.clone()));
        if let Some(path) = data.get("sessionFile").and_then(Value::as_str) {
            cursor.insert("session_file".into(), Value::String(path.to_owned()));
        }
        let model = model_wire_id(data.get("model"));
        self.session_file = data
            .get("sessionFile")
            .and_then(Value::as_str)
            .map(str::to_owned);
        self.events
            .emit(AgentEvent::SessionStarted {
                provider_session_id: session_id,
                resume: ResumeCursor(Value::Object(cursor)),
                model,
            })
            .await;
        if let Some(event) = thinking_level(data.get("thinkingLevel")) {
            self.events.emit(event).await;
        }

        // Restore points are the user messages entered from here on.
        send_json(
            &mut self.stdin,
            &json!({"id":"tcode-entries","type":"get_entries"}),
        )?;
        // `wait_response` reports pi refusing a request as
        // `AgentError::Provider`; the session itself has started. Any other
        // error means pi is gone.
        match self.wait_response("tcode-entries").await {
            Ok(response) => self.entries_cursor = last_entry_id(&response),
            Err(AgentError::Provider(err)) => {
                self.events
                    .emit(AgentEvent::Warning {
                        message: format!("pi get_entries failed: {err}"),
                    })
                    .await;
            }
            Err(err) => return Err(err),
        }

        send_json(
            &mut self.stdin,
            &json!({"id":"tcode-commands","type":"get_commands"}),
        )?;
        let event = match self.wait_response("tcode-commands").await {
            Ok(response) => AgentEvent::ProviderCommands {
                commands: map_commands(&response),
            },
            Err(AgentError::Provider(err)) => AgentEvent::Warning {
                message: format!("pi get_commands failed: {err}"),
            },
            Err(err) => return Err(err),
        };
        self.events.emit(event).await;
        Ok(())
    }

    async fn wait_response(&mut self, id: &str) -> Result<Value, AgentError> {
        loop {
            match self.lines.recv().await {
                Ok(ChildOutput::Line(line)) => {
                    let message: Value = serde_json::from_str(&line).map_err(|err| {
                        AgentError::Protocol(format!("invalid pi JSON response: {err}"))
                    })?;
                    if message.get("id").and_then(Value::as_str) == Some(id) {
                        ensure_success(&message)?;
                        return Ok(message);
                    }
                    // Extensions load before this handshake completes, so a
                    // notification such as one explaining a broken extension,
                    // or a question for the user, arrives while we are still
                    // here. A question waits for the handshake to end and is
                    // then the user's to answer.
                    if let Some(warning) = extension_notice(&message) {
                        self.events
                            .emit(AgentEvent::Warning { message: warning })
                            .await;
                    } else if message.get("type").and_then(Value::as_str)
                        == Some("extension_ui_request")
                    {
                        self.early_requests.push((message, self.clock.now()));
                    }
                }
                Ok(ChildOutput::Error(err)) => return Err(AgentError::Protocol(err)),
                Ok(ChildOutput::Eof) | Err(_) => {
                    return Err(AgentError::Protocol(
                        "pi closed stdout during startup handshake".into(),
                    ));
                }
            }
        }
    }

    /// The session pi works in now, from a `get_state` response.
    async fn adopt_session(&mut self, state: &Value) {
        let data = state.get("data").unwrap_or(&Value::Null);
        let Some(session_id) = data.get("sessionId").and_then(Value::as_str) else {
            return;
        };
        let mut cursor = serde_json::Map::new();
        cursor.insert("session_id".into(), Value::String(session_id.to_owned()));
        self.session_file = data
            .get("sessionFile")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Some(path) = &self.session_file {
            cursor.insert("session_file".into(), Value::String(path.clone()));
        }
        self.events
            .emit(AgentEvent::SessionStarted {
                provider_session_id: session_id.to_owned(),
                resume: ResumeCursor(Value::Object(cursor)),
                model: model_wire_id(data.get("model")),
            })
            .await;
        if let Some(event) = thinking_level(data.get("thinkingLevel")) {
            self.events.emit(event).await;
        }
    }

    /// Read the entries after the cursor; `turn` names the turn that just
    /// finished, whose prompt they hold.
    fn read_entries(&mut self, turn: Option<String>) -> Result<(), String> {
        let id = self.request_id();
        let mut request = json!({"id":id,"type":"get_entries"});
        if let Some(cursor) = &self.entries_cursor {
            request["since"] = json!(cursor);
        }
        send_json(&mut self.stdin, &request).map_err(|err| err.to_string())?;
        self.requests.insert(id, PiRequest::Entries { turn });
        Ok(())
    }

    fn request(&mut self, command: &str, request: PiRequest) -> Result<(), String> {
        let id = self.request_id();
        send_json(&mut self.stdin, &json!({"id":id,"type":command}))
            .map_err(|err| err.to_string())?;
        self.requests.insert(id, request);
        Ok(())
    }

    /// A response a rewind, its undo or a restore point waited for.
    async fn settle_request(&mut self, request: PiRequest, message: &Value) {
        let success = message
            .get("success")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let data = message.get("data").unwrap_or(&Value::Null);
        let cancelled = data.get("cancelled").and_then(Value::as_bool) == Some(true);
        let failure = |reason| RewindFailure::new(reason, response_error(message));
        match request {
            PiRequest::Entries { turn } => {
                if !success {
                    self.events
                        .emit(AgentEvent::Warning {
                            message: format!("pi get_entries failed: {}", response_error(message)),
                        })
                        .await;
                    return;
                }
                if let Some(cursor) = last_entry_id(message) {
                    self.entries_cursor = Some(cursor);
                }
                let prompt = data
                    .get("entries")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .find(|entry| {
                        entry.get("type").and_then(Value::as_str) == Some("message")
                            && entry.pointer("/message/role").and_then(Value::as_str)
                                == Some("user")
                    })
                    .and_then(|entry| entry.get("id").and_then(Value::as_str));
                if let (Some(turn_id), Some(checkpoint_id)) = (turn, prompt) {
                    self.events
                        .emit(AgentEvent::TurnCheckpoint {
                            turn_id,
                            checkpoint_id: checkpoint_id.to_owned(),
                        })
                        .await;
                }
            }
            PiRequest::Fork {
                checkpoint_id,
                previous,
                stops_turn,
            } => {
                if !success || cancelled {
                    // Nothing was forked, so a running turn goes on.
                    if stops_turn {
                        self.mapper.interrupt_pending = false;
                    }
                    let reason = if !success && response_error(message).contains("Invalid entry") {
                        RewindFailureReason::TargetNotFound
                    } else {
                        RewindFailureReason::Other
                    };
                    let mut failure = failure(reason);
                    if cancelled {
                        failure.detail = Some("an extension cancelled the fork".into());
                    }
                    self.events
                        .emit(AgentEvent::RewindFailed {
                            checkpoint_id,
                            mode: RewindMode::Conversation,
                            failure,
                        })
                        .await;
                    return;
                }
                self.end_aborted_turn().await;
                let prefill = data.get("text").and_then(Value::as_str).map(str::to_owned);
                if let Err(err) = self.request(
                    "get_state",
                    PiRequest::Forked {
                        checkpoint_id,
                        prefill,
                        previous,
                    },
                ) {
                    log::warn!("pi get_state after a fork not sent: {err}");
                }
            }
            PiRequest::Forked {
                checkpoint_id,
                prefill,
                previous,
            } => {
                self.adopt_session(message).await;
                self.entries_cursor = None;
                let _ = self.read_entries(None);
                self.events
                    .emit(AgentEvent::RewindCompleted {
                        checkpoint_id,
                        mode: RewindMode::Conversation,
                        prefill,
                        prefill_attachments: Vec::new(),
                        later_history: LaterHistory::Branch,
                        files: None,
                        incomplete: None,
                        // The session the fork left stays on disk, to return to.
                        undo_token: previous
                            .filter(|previous| Some(previous) != self.session_file.as_ref()),
                    })
                    .await;
            }
            PiRequest::Return { checkpoint_id } => {
                if !success || cancelled {
                    let mut failure = failure(RewindFailureReason::Other);
                    if cancelled {
                        failure.detail = Some("an extension cancelled the switch".into());
                    }
                    self.events
                        .emit(AgentEvent::RewindUndoFailed {
                            checkpoint_id,
                            failure,
                        })
                        .await;
                    return;
                }
                if let Err(err) = self.request("get_state", PiRequest::Returned { checkpoint_id }) {
                    log::warn!("pi get_state after returning not sent: {err}");
                }
            }
            PiRequest::Returned { checkpoint_id } => {
                self.adopt_session(message).await;
                self.entries_cursor = None;
                let _ = self.read_entries(None);
                self.events
                    .emit(AgentEvent::RewindUndone { checkpoint_id })
                    .await;
            }
        }
    }

    /// A turn the fork aborted ends interrupted, whether or not pi said so.
    async fn end_aborted_turn(&mut self) {
        if self.mapper.current_turn.is_none() {
            return;
        }
        self.mapper.interrupt_pending = true;
        let _ = self.cancel_pending_ui().await;
        for event in self.mapper.complete_turn() {
            self.events.emit(event).await;
        }
    }

    async fn handle_line(&mut self, line: &str) {
        let message: Value = match serde_json::from_str(line) {
            Ok(message) => message,
            Err(err) => {
                self.events
                    .emit(AgentEvent::Warning {
                        message: format!("ignored invalid pi RPC record: {err}"),
                    })
                    .await;
                return;
            }
        };
        if message.get("type").and_then(Value::as_str) == Some("response") {
            let success = message.get("success").and_then(Value::as_bool) == Some(true);
            // `set_model` answers with the model it set (1.5.2).
            if success
                && message.get("command").and_then(Value::as_str) == Some("set_model")
                && let Some(model) = model_wire_id(message.get("data"))
            {
                self.events.emit(AgentEvent::SessionModel { model }).await;
            }
            if let Some(event) = message
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| self.settle_settings(id, success))
            {
                self.events.emit(event).await;
                return;
            }
            if let Some(request) = message
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| self.requests.remove(id))
            {
                self.settle_request(request, &message).await;
                return;
            }
            let steer = message
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| self.pending_steers.remove(id));
            let event = match (success, steer) {
                (true, Some(request_id)) => AgentEvent::SteerAccepted { request_id },
                (true, None) => return,
                (false, Some(request_id)) => AgentEvent::SteerRejected { request_id },
                (false, None) => AgentEvent::Error {
                    message: response_error(&message),
                    fatal: false,
                },
            };
            self.events.emit(event).await;
            return;
        }
        if message.get("type").and_then(Value::as_str) == Some("extension_ui_request") {
            let arrived = self.clock.now();
            self.handle_extension_ui(&message, arrived).await;
            return;
        }
        let events = self.mapper.on_message(&message);
        let turn_completed = events.iter().find_map(|event| match event {
            AgentEvent::TurnCompleted { turn_id, .. } => Some(turn_id.clone()),
            _ => None,
        });
        for event in events {
            self.events.emit(event).await;
        }
        if let Some(turn_id) = turn_completed {
            // Its prompt is now an entry: a point a fork can go back before.
            if let Err(err) = self.read_entries(Some(turn_id)) {
                log::warn!("pi get_entries not sent: {err}");
            }
        }
    }

    /// `arrived` is when pi sent it, which a `timeout` it gives counts from.
    async fn handle_extension_ui(&mut self, message: &Value, arrived: Instant) {
        if let Some(id) = message.get("id").and_then(Value::as_str)
            && let Some(timeout) = message.get("timeout").and_then(Value::as_u64)
        {
            self.answered_by_pi_at
                .insert(id.to_owned(), arrived + Duration::from_millis(timeout));
        }
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        if matches!(method, "select" | "input" | "editor") {
            let Some((id, question)) = extension_dialog_question(message) else {
                self.events
                    .emit(AgentEvent::Warning {
                        message: format!("pi extension `{method}` dialog omitted its id"),
                    })
                    .await;
                return;
            };
            self.pending_dialogs.insert(id.clone());
            self.events
                .emit(AgentEvent::UserInputRequested {
                    request_id: id,
                    questions: vec![question],
                    delivery: UserInputDelivery::Blocking,
                })
                .await;
            return;
        }
        if method != "confirm" {
            if let Some(warning) = extension_notice(message) {
                self.events
                    .emit(AgentEvent::Warning { message: warning })
                    .await;
                return;
            }
            if matches!(
                method,
                "notify" | "setStatus" | "setWidget" | "setTitle" | "set_editor_text"
            ) {
                return;
            }
            let id = message.get("id").and_then(Value::as_str).unwrap_or("");
            let _ = send_json(
                &mut self.stdin,
                &json!({"type":"extension_ui_response","id":id,"cancelled":true}),
            );
            self.events
                .emit(AgentEvent::Warning {
                    message: format!("pi extension requested unsupported UI method `{method}`"),
                })
                .await;
            return;
        }
        let Some(id) = message.get("id").and_then(Value::as_str) else {
            self.events
                .emit(AgentEvent::Warning {
                    message: "pi extension confirmation omitted its id".into(),
                })
                .await;
            return;
        };
        self.pending_approvals.insert(id.to_owned());
        self.events
            .emit(AgentEvent::ApprovalRequested(ApprovalRequest {
                id: id.to_owned(),
                parent_item_id: None,
                turn_id: self.mapper.current_turn.clone(),
                reason: None,
                blocked_path: None,
                kind: ApprovalKind::ToolUse {
                    name: message
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    input: Value::Null,
                    detail: message
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                },
                options: vec![
                    ApprovalOption {
                        id: "confirm".into(),
                        label: "Confirm".into(),
                        kind: ApprovalOptionKind::AllowOnce,
                    },
                    ApprovalOption {
                        id: "decline".into(),
                        label: "Decline".into(),
                        kind: ApprovalOptionKind::RejectOnce,
                    },
                ],
            }))
            .await;
    }

    /// `set_model`, then `set_thinking_level`, which pi clamps to the model:
    /// its RPC answers each in turn, and the last answer settles the change.
    fn apply_settings(
        &mut self,
        model: Option<String>,
        selections: &[OptionSelection],
    ) -> Result<(), String> {
        let mut requests = Vec::new();
        if model != self.requested_model {
            let Some((provider, model_id)) = model.as_deref().and_then(|m| m.split_once('/'))
            else {
                // The default model is chosen at start; a running process
                // cannot be asked for it.
                self.pending_settings = None;
                let _ = self
                    .events
                    .try_send(AgentEvent::SettingsApplied { applied: false });
                return Ok(());
            };
            let id = self.request_id();
            requests.push((
                id.clone(),
                json!({"id":id,"type":"set_model","provider":provider,"modelId":model_id}),
            ));
        }
        if let Some(level) = crate::selection_str(selections, crate::EFFORT_OPTION_ID) {
            let id = self.request_id();
            requests.push((
                id.clone(),
                json!({"id":id,"type":"set_thinking_level","level":level}),
            ));
        }
        if requests.is_empty() {
            let _ = self
                .events
                .try_send(AgentEvent::SettingsApplied { applied: true });
            return Ok(());
        }
        self.pending_settings = Some(PendingSettings {
            model,
            outstanding: requests.iter().map(|(id, _)| id.clone()).collect(),
            applied: true,
        });
        for (_, request) in requests {
            send_json(&mut self.stdin, &request).map_err(|err| err.to_string())?;
        }
        Ok(())
    }

    /// One answer to [`Self::apply_settings`]; `None` when `id` is not one.
    fn settle_settings(&mut self, id: &str, success: bool) -> Option<AgentEvent> {
        let pending = self.pending_settings.as_mut()?;
        if !pending.outstanding.remove(id) {
            return None;
        }
        pending.applied &= success;
        if !pending.outstanding.is_empty() {
            return None;
        }
        let pending = self.pending_settings.take()?;
        if pending.applied {
            self.requested_model = pending.model;
        }
        Some(AgentEvent::SettingsApplied {
            applied: pending.applied,
        })
    }

    fn request_id(&mut self) -> String {
        let id = format!("tcode-{}", self.next_request);
        self.next_request += 1;
        id
    }

    /// Dismiss every open extension prompt, as stopping the turn does.
    async fn cancel_pending_ui(&mut self) -> Result<(), String> {
        let dismissed = dismiss_prompts(
            &mut self.stdin,
            &mut self.pending_approvals,
            &mut self.pending_dialogs,
        )
        .map_err(|err| err.to_string())?;
        for event in dismissed {
            self.events.emit(event).await;
        }
        Ok(())
    }

    fn stop(&mut self) {
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct PiMapper {
    turn_counter: u64,
    current_turn: Option<String>,
    turn_usage: Option<TokenUsage>,
    cumulative_processed: u64,
    interrupt_pending: bool,
    failed: bool,
    tool_items: HashMap<String, PiTool>,
    /// Item ids of this turn's assistant messages, keyed by the message
    /// `timestamp` (the only field present from `message_start` through
    /// `message_end`), with a count so two messages stamped in the same
    /// millisecond still get distinct ids.
    assistant_ids: HashMap<u64, (String, u32)>,
    /// Id of the assistant message being streamed. `message_update` carries
    /// only the delta, so the id is taken from `message_start`.
    streaming_assistant: Option<String>,
}

#[derive(Clone)]
struct PiTool {
    name: String,
    input: Value,
    output: ToolOutput,
    /// The latest `AgentToolResult`.
    result: Option<Value>,
}

impl PiMapper {
    fn new() -> Self {
        Self {
            turn_counter: 0,
            current_turn: None,
            turn_usage: None,
            cumulative_processed: 0,
            interrupt_pending: false,
            failed: false,
            tool_items: HashMap::new(),
            assistant_ids: HashMap::new(),
            streaming_assistant: None,
        }
    }

    fn on_message(&mut self, message: &Value) -> Vec<AgentEvent> {
        match message.get("type").and_then(Value::as_str).unwrap_or("") {
            "agent_start" => self.start_turn(),
            "agent_settled" => self.complete_turn(),
            "message_start" => {
                let message = message.get("message").unwrap_or(&Value::Null);
                if message.get("role").and_then(Value::as_str) == Some("assistant") {
                    let id = self.allocate_assistant_id(message);
                    self.streaming_assistant = Some(id);
                }
                Vec::new()
            }
            "message_update" => self.message_update(message),
            "message_end" => self.message_end(message),
            "tool_execution_start" => self.tool_event(message, ItemStatus::InProgress, false),
            "tool_execution_update" => self.tool_event(message, ItemStatus::InProgress, true),
            "tool_execution_end" => self.tool_event(
                message,
                if message
                    .get("isError")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    ItemStatus::Failed
                } else {
                    ItemStatus::Completed
                },
                true,
            ),
            "compaction_start" => vec![AgentEvent::Warning {
                message: format!(
                    "pi is compacting context ({})",
                    message
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown reason")
                ),
            }],
            "compaction_end" => {
                if message.get("result").is_some_and(|value| !value.is_null()) {
                    vec![AgentEvent::ContextCompacted(Default::default())]
                } else {
                    vec![AgentEvent::Warning {
                        message: format!(
                            "pi context compaction did not complete: {}",
                            message
                                .get("errorMessage")
                                .and_then(Value::as_str)
                                .unwrap_or("aborted")
                        ),
                    }]
                }
            }
            "auto_retry_start" => vec![AgentEvent::Warning {
                message: format!(
                    "pi retry {}/{} in {} ms: {}",
                    crate::json_u64(message.get("attempt")).unwrap_or(0),
                    crate::json_u64(message.get("maxAttempts")).unwrap_or(0),
                    crate::json_u64(message.get("delayMs")).unwrap_or(0),
                    message
                        .get("errorMessage")
                        .and_then(Value::as_str)
                        .unwrap_or("transient provider error")
                ),
            }],
            // pi ends a retry with success once a retried request answers;
            // the failures it retried are behind the turn then.
            "auto_retry_end" if message.get("success").and_then(Value::as_bool) == Some(true) => {
                self.failed = false;
                Vec::new()
            }
            "auto_retry_end" => {
                self.failed = true;
                vec![AgentEvent::Warning {
                    message: format!(
                        "pi retry failed: {}",
                        message
                            .get("finalError")
                            .and_then(Value::as_str)
                            .unwrap_or("provider error")
                    ),
                }]
            }
            "thinking_level_changed" => thinking_level(message.get("level")).into_iter().collect(),
            "extension_error" => vec![AgentEvent::Warning {
                message: format!(
                    "pi extension error in {}: {}",
                    message
                        .get("event")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown event"),
                    message
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                ),
            }],
            _ => Vec::new(),
        }
    }

    fn start_turn(&mut self) -> Vec<AgentEvent> {
        crate::start_mapped_turn(
            "pi",
            &mut self.turn_counter,
            &mut self.current_turn,
            &mut self.turn_usage,
            &mut self.failed,
        )
    }

    fn complete_turn(&mut self) -> Vec<AgentEvent> {
        self.assistant_ids.clear();
        self.streaming_assistant = None;
        crate::complete_mapped_turn(
            &mut self.current_turn,
            &mut self.interrupt_pending,
            self.failed,
            self.turn_usage,
        )
    }

    fn message_update(&mut self, message: &Value) -> Vec<AgentEvent> {
        let event = message.get("assistantMessageEvent").unwrap_or(&Value::Null);
        let kind = match event.get("type").and_then(Value::as_str) {
            Some("text_delta") => DeltaKind::AssistantText,
            Some("thinking_delta") => DeltaKind::ReasoningText,
            Some("toolcall_end") => {
                let tool = event.get("toolCall").unwrap_or(&Value::Null);
                let Some(id) = tool.get("id").and_then(Value::as_str) else {
                    return Vec::new();
                };
                let id = id.to_owned();
                let name = tool
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_owned();
                let input = tool.get("arguments").cloned().unwrap_or(Value::Null);
                let tool = PiTool {
                    name,
                    input,
                    output: ToolOutput::default(),
                    result: None,
                };
                let item = tool_item(&id, &tool, ItemStatus::InProgress);
                let existed = self.tool_items.insert(id, tool).is_some();
                return vec![if existed {
                    AgentEvent::ItemUpdated(item)
                } else {
                    AgentEvent::ItemStarted(item)
                }];
            }
            _ => return Vec::new(),
        };
        let delta = event.get("delta").and_then(Value::as_str).unwrap_or("");
        if delta.is_empty() {
            return Vec::new();
        }
        let Some(message_id) = &self.streaming_assistant else {
            return Vec::new();
        };
        let index = crate::json_u64(event.get("contentIndex")).unwrap_or(0);
        vec![AgentEvent::Delta {
            item_id: format!("{message_id}:{index}"),
            kind,
            text: delta.to_owned(),
        }]
    }

    fn message_end(&mut self, event: &Value) -> Vec<AgentEvent> {
        let message = event.get("message").unwrap_or(&Value::Null);
        match message.get("role").and_then(Value::as_str) {
            // The app records the canonical user message before sending the
            // prompt. pi echoes it back; mapping that echo would duplicate the
            // user bubble, as with Codex/OpenCode.
            Some("user") => Vec::new(),
            Some("custom") if message.get("display").and_then(Value::as_bool) == Some(true) => {
                let summary = match message.get("content") {
                    Some(Value::String(text)) => text.clone(),
                    Some(Value::Array(parts)) => parts
                        .iter()
                        .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => String::new(),
                };
                if summary.trim().is_empty() {
                    return Vec::new();
                }
                let provider_kind = message
                    .get("customType")
                    .and_then(Value::as_str)
                    .filter(|kind| !kind.trim().is_empty())
                    .unwrap_or("pi-extension")
                    .to_owned();
                vec![AgentEvent::ItemCompleted(ThreadItem {
                    id: custom_message_id(message),
                    parent_item_id: None,
                    content: ItemContent::Other {
                        provider_kind,
                        summary,
                    },
                })]
            }
            Some("assistant") => {
                // pi ends a failed provider request with this message, its
                // `errorMessage` saying why; no `message_update` reports it.
                let failure = if message.get("stopReason").and_then(Value::as_str) == Some("error")
                {
                    self.failed = true;
                    message
                        .get("errorMessage")
                        .and_then(Value::as_str)
                        .map(|text| AgentEvent::Error {
                            message: text.to_owned(),
                            fatal: false,
                        })
                } else {
                    None
                };
                let Some(id) = self.assistant_id(message) else {
                    return Vec::new();
                };
                let mut events = Vec::new();
                if let Some(content) = message.get("content").and_then(Value::as_array) {
                    for (index, block) in content.iter().enumerate() {
                        let item = match block.get("type").and_then(Value::as_str) {
                            Some("text") => Some(ItemContent::AssistantMessage {
                                text: block
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_owned(),
                                summarized: false,
                            }),
                            Some("thinking") => Some(ItemContent::Reasoning {
                                text: block
                                    .get("thinking")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_owned(),
                            }),
                            _ => None,
                        };
                        if let Some(content) = item {
                            events.push(AgentEvent::ItemCompleted(ThreadItem {
                                id: format!("{id}:{index}"),
                                parent_item_id: None,
                                content,
                            }));
                        }
                    }
                }
                if let Some(usage) = map_usage(message.get("usage")) {
                    let processed = crate::processed_tokens(usage);
                    self.cumulative_processed = self.cumulative_processed.saturating_add(processed);
                    let usage = TokenUsage {
                        freshness: crate::ContextFreshness::Current,
                        total_processed_tokens: Some(self.cumulative_processed),
                        ..usage
                    };
                    self.turn_usage.get_or_insert_default().merge(usage);
                    events.push(AgentEvent::TokenUsage(usage));
                }
                events.extend(failure);
                events
            }
            _ => Vec::new(),
        }
    }

    /// Names a new assistant message. pi stamps `timestamp` when it creates
    /// the message, before the first delta, and keeps it on the final
    /// message; `responseId` only arrives with the provider response, so it
    /// cannot name the item that was streamed.
    fn allocate_assistant_id(&mut self, message: &Value) -> String {
        let timestamp = crate::json_u64(message.get("timestamp")).unwrap_or(0);
        let (id, count) = self
            .assistant_ids
            .entry(timestamp)
            .or_insert_with(|| (String::new(), 0));
        *count += 1;
        *id = if *count == 1 {
            format!("pi-assistant-{timestamp}")
        } else {
            format!("pi-assistant-{timestamp}-{count}")
        };
        id.clone()
    }

    /// The id the `message_start` of a completed assistant message named.
    fn assistant_id(&self, message: &Value) -> Option<String> {
        let timestamp = crate::json_u64(message.get("timestamp")).unwrap_or(0);
        self.assistant_ids.get(&timestamp).map(|(id, _)| id.clone())
    }

    fn tool_event(
        &mut self,
        event: &Value,
        status: ItemStatus,
        has_result: bool,
    ) -> Vec<AgentEvent> {
        let Some(id) = event.get("toolCallId").and_then(Value::as_str) else {
            return Vec::new();
        };
        let id = id.to_owned();
        let event_name = event.get("toolName").and_then(Value::as_str);
        let event_input = event.get("args").filter(|value| !value.is_null()).cloned();
        let result = if event.get("type").and_then(Value::as_str) == Some("tool_execution_update") {
            event.get("partialResult")
        } else {
            event.get("result")
        };
        let output = result
            .map(|result| ToolOutput::from_content(result.get("content").unwrap_or(&Value::Null)))
            .unwrap_or_default();
        let was_known = self.tool_items.contains_key(&id);
        let tool = self.tool_items.entry(id.clone()).or_insert_with(|| PiTool {
            name: event_name.unwrap_or("tool").to_owned(),
            input: event_input.clone().unwrap_or(Value::Null),
            output: ToolOutput::default(),
            result: None,
        });
        if let Some(name) = event_name {
            tool.name = name.to_owned();
        }
        if let Some(input) = event_input {
            tool.input = input;
        }
        if has_result {
            tool.output = output;
            tool.result = result.cloned();
        }
        let item = tool_item(&id, tool, status);
        vec![match status {
            ItemStatus::InProgress if has_result => AgentEvent::ItemUpdated(item),
            ItemStatus::InProgress if was_known => AgentEvent::ItemUpdated(item),
            ItemStatus::InProgress => AgentEvent::ItemStarted(item),
            _ => AgentEvent::ItemCompleted(item),
        }]
    }
}

/// The canonical item for a pi tool call. Its `result` carries what the
/// content text leaves out: bash's `structuredContent.exit_code` and edit's
/// `details.patch`, a unified diff.
fn tool_item(id: &str, tool: &PiTool, status: ItemStatus) -> ThreadItem {
    let PiTool {
        name,
        input,
        output,
        result,
    } = tool;
    let result = result.as_ref();
    let content = match name.as_str() {
        "bash" => ItemContent::CommandExecution {
            command: input
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            input: Vec::new(),
            output: output.text.clone(),
            exit_code: result
                .and_then(|result| result.pointer("/structuredContent/exit_code"))
                .and_then(Value::as_i64)
                .and_then(|code| i32::try_from(code).ok()),
            status,
            changes: Vec::new(),
        },
        "edit" | "write" => ItemContent::FileChange {
            changes: input
                .get("path")
                .and_then(Value::as_str)
                .map(|path| {
                    vec![FileChange {
                        path: path.to_owned(),
                        kind: if name == "write" {
                            FileChangeKind::Create
                        } else {
                            FileChangeKind::Modify
                        },
                        diff: if name == "write" {
                            input
                                .get("content")
                                .and_then(Value::as_str)
                                .filter(|content| !content.is_empty())
                                .map(|content| {
                                    content
                                        .lines()
                                        .map(|line| format!("+{line}"))
                                        .collect::<Vec<_>>()
                                        .join("\n")
                                })
                        } else {
                            result
                                .and_then(|result| result.pointer("/details/patch"))
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                        },
                    }]
                })
                .unwrap_or_default(),
            status,
            output: (!output.text.is_empty()).then(|| output.text.clone()),
        },
        _ => {
            let text = (!output.text.is_empty()).then(|| output.text.clone());
            let (family, text) = tool_family(name, input, text, result, status);
            ItemContent::ToolCall {
                image_reads: output.images.clone(),
                name: name.clone(),
                input: input.clone(),
                output: text,
                status,
                family: family.map(Box::new),
            }
        }
    };
    ThreadItem {
        id: id.into(),
        parent_item_id: None,
        content,
    }
}

/// What pi's own tool `name` is, from its input and, once it succeeded, its
/// text and `result` (`{content, details?}`). Returns the family and the
/// output the call shows: the text, or what is left of it once the family's
/// fields are read out. Formats are 1.5.2's (`dist/core/tools/`).
fn tool_family(
    name: &str,
    input: &Value,
    text: Option<String>,
    result: Option<&Value>,
    status: ItemStatus,
) -> (Option<ToolFamily>, Option<String>) {
    let field = |key: &str| input.get(key).and_then(Value::as_str).map(str::to_owned);
    match name {
        // `read.js`: the lines read as they are, from `offset` (1-based);
        // when the file goes on, a blank line and a `[…]` notice after them.
        "read" => {
            let Some(path) = field("path") else {
                return (None, text);
            };
            let first_line = Some(
                input
                    .get("offset")
                    .and_then(Value::as_u64)
                    .unwrap_or(1)
                    .max(1),
            );
            let noticed = result
                .is_some_and(|result| result.pointer("/details/truncation").is_some())
                || input.get("limit").is_some();
            let (lines, note) = match text {
                Some(text) if status == ItemStatus::Completed && noticed => {
                    match text.rsplit_once("\n\n[") {
                        Some((lines, note)) if note.ends_with(']') && !note.contains('\n') => {
                            (Some(lines.to_owned()), Some(format!("[{note}")))
                        }
                        _ => (Some(text), None),
                    }
                }
                text => (text, None),
            };
            // An image's result is the `Read image file [type]` line beside
            // the image, not lines of the file.
            let image = lines
                .as_deref()
                .is_some_and(|text| text.starts_with("Read image file ["));
            (
                Some(ToolFamily::Read {
                    path,
                    first_line: first_line.filter(|_| status == ItemStatus::Completed && !image),
                    note,
                }),
                lines,
            )
        }
        // The tool_search extension (`extensions/tool-search/tool.js`):
        // `{query}`, and `details.loaded`, the tools it loaded.
        "tool_search" => match field("query") {
            Some(query) => (
                Some(ToolFamily::LoadTools {
                    query,
                    tools: result
                        .and_then(|result| result.pointer("/details/loaded"))
                        .and_then(Value::as_array)
                        .map(|loaded| {
                            loaded
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect()
                        }),
                }),
                text,
            ),
            None => (None, text),
        },
        // `grep.js`, `find.js`, `ls.js`: one line each, relative to the
        // directory searched, before any `[…]` notice after a blank line.
        "grep" | "find" | "ls" => {
            let path = field("path");
            let hits = text
                .as_deref()
                .filter(|_| status == ItemStatus::Completed)
                .map(|text| found(name, path.as_deref(), text));
            (
                Some(ToolFamily::Search {
                    scope: match name {
                        "grep" => SearchScope::Content,
                        "find" => SearchScope::Files,
                        _ => SearchScope::Entries,
                    },
                    pattern: field("pattern"),
                    path,
                    glob: field("glob"),
                    hits,
                }),
                text,
            )
        }
        _ => (None, text),
    }
}

/// What a finished grep, find or ls returned. grep's matches are
/// `path:N: text` (`path-N- text` is context), its path the searched file's
/// name when it searched one file; find's and ls's lines are paths; an empty
/// result is one line of pi's words.
fn found(tool: &str, searched: Option<&str>, text: &str) -> Vec<SearchHit> {
    let lines = match text.rsplit_once("\n\n[") {
        Some((lines, notice)) if notice.ends_with(']') => lines,
        _ => text,
    };
    let base = searched.map(Path::new);
    let resolve = |relative: &str| match base {
        Some(base) if base.file_name().and_then(|name| name.to_str()) == Some(relative) => {
            base.to_string_lossy().into_owned()
        }
        Some(base) => base.join(relative).to_string_lossy().into_owned(),
        None => relative.to_owned(),
    };
    let empty = [
        "No matches found",
        "No files found matching pattern",
        "(empty directory)",
    ];
    if empty.contains(&lines.trim()) {
        return Vec::new();
    }
    lines
        .lines()
        .filter_map(|line| {
            if tool != "grep" {
                return Some(SearchHit {
                    path: match base {
                        Some(base) => base.join(line).to_string_lossy().into_owned(),
                        None => line.to_owned(),
                    },
                    line: None,
                    text: None,
                });
            }
            let (path, number, text) = line.match_indices(':').find_map(|(at, _)| {
                let (number, text) = line[at + 1..].split_once(": ")?;
                Some((&line[..at], number.parse().ok()?, text))
            })?;
            Some(SearchHit {
                path: resolve(path),
                line: Some(number),
                text: Some(text.to_owned()),
            })
        })
        .collect()
}

/// The text of a `notify` from a pi extension, at any of its levels.
///
/// pi's fire-and-forget UI methods expect no response, so an extension that
/// tells the user something this way is otherwise invisible to them.
fn extension_notice(message: &Value) -> Option<String> {
    if message.get("type").and_then(Value::as_str) != Some("extension_ui_request")
        || message.get("method").and_then(Value::as_str) != Some("notify")
    {
        return None;
    }
    let text = message.get("message").and_then(Value::as_str)?.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn is_extension_dialog(message: &Value) -> bool {
    message.get("type").and_then(Value::as_str) == Some("extension_ui_request")
        && matches!(
            message.get("method").and_then(Value::as_str),
            Some("select" | "input" | "editor")
        )
}

fn extension_dialog_question(message: &Value) -> Option<(String, UserInputQuestion)> {
    if !is_extension_dialog(message) {
        return None;
    }
    let id = message.get("id")?.as_str()?.to_owned();
    let method = message.get("method")?.as_str()?;
    let title = message
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (question, options, prefill) = match method {
        "select" => (
            title.to_owned(),
            message
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|label| UserInputOption {
                    label: label.to_owned(),
                    description: String::new(),
                })
                .collect(),
            None,
        ),
        "input" => {
            let question = match message.get("placeholder").and_then(Value::as_str) {
                Some(placeholder) => format!("{title} ({placeholder})"),
                None => title.to_owned(),
            };
            (question, Vec::new(), None)
        }
        "editor" => (
            title.to_owned(),
            Vec::new(),
            message
                .get("prefill")
                .and_then(Value::as_str)
                .map(str::to_owned),
        ),
        _ => return None,
    };
    Some((
        id.clone(),
        UserInputQuestion {
            id,
            header: "pi".into(),
            question,
            options,
            multi_select: false,
            prefill,
        },
    ))
}

/// Answer an open extension dialog with the user's value, or dismiss it when
/// the answer holds none; `None` when `request_id` is not an open dialog.
fn answer_dialog(
    writer: &mut impl Write,
    pending_dialogs: &mut HashSet<String>,
    request_id: String,
    answers: serde_json::Map<String, Value>,
) -> Result<Option<AgentEvent>, AgentError> {
    if !pending_dialogs.remove(&request_id) {
        return Ok(None);
    }
    let response = match answers.get(&request_id).and_then(Value::as_str) {
        Some(value) => json!({"type":"extension_ui_response","id":request_id,"value":value}),
        None => json!({"type":"extension_ui_response","id":request_id,"cancelled":true}),
    };
    send_json(writer, &response)?;
    Ok(Some(AgentEvent::UserInputResolved {
        request_id,
        answers,
    }))
}

/// Decline every open confirmation and dismiss every open dialog, reporting
/// each one resolved.
fn dismiss_prompts(
    writer: &mut impl Write,
    pending_approvals: &mut HashSet<String>,
    pending_dialogs: &mut HashSet<String>,
) -> Result<Vec<AgentEvent>, AgentError> {
    let mut dismissed = Vec::new();
    for request_id in pending_approvals.drain() {
        send_json(
            writer,
            &json!({"type":"extension_ui_response","id":request_id,"confirmed":false}),
        )?;
        dismissed.push(AgentEvent::ApprovalResolved {
            request_id,
            decision: ApprovalDecision::Cancel,
        });
    }
    for request_id in pending_dialogs.drain() {
        send_json(
            writer,
            &json!({"type":"extension_ui_response","id":request_id,"cancelled":true}),
        )?;
        dismissed.push(AgentEvent::UserInputResolved {
            request_id,
            answers: serde_json::Map::new(),
        });
    }
    Ok(dismissed)
}

fn custom_message_id(message: &Value) -> String {
    format!(
        "pi-custom-{}",
        crate::json_u64(message.get("timestamp")).unwrap_or(0)
    )
}

fn map_usage(usage: Option<&Value>) -> Option<TokenUsage> {
    let usage = usage?;
    let input = crate::json_u64(usage.get("input"));
    let output = crate::json_u64(usage.get("output"));
    let cache_read = crate::json_u64(usage.get("cacheRead"));
    (input.is_some() || output.is_some() || cache_read.is_some()).then_some(TokenUsage {
        freshness: crate::ContextFreshness::Current,
        input_tokens: input,
        cached_input_tokens: cache_read,
        output_tokens: output,
        used_tokens: crate::json_u64(usage.get("totalTokens")),
        ..TokenUsage::default()
    })
}

fn attach_images(request: &mut Value, attachments: Vec<Attachment>) {
    if attachments.is_empty() {
        return;
    }
    request["images"] = Value::Array(
        attachments
            .into_iter()
            .map(|attachment| {
                json!({
                    "type":"image",
                    "data":attachment.data_base64,
                    "mimeType":attachment.media_type
                })
            })
            .collect(),
    );
}

fn map_commands(response: &Value) -> Vec<ProviderCommand> {
    response
        .pointer("/data/commands")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|command| {
            let raw_name = command.get("name")?.as_str()?;
            let source = command.get("source").and_then(Value::as_str).unwrap_or("");
            let (name, kind) = if source == "skill" {
                (
                    raw_name.strip_prefix("skill:").unwrap_or(raw_name),
                    ProviderCommandKind::Skill,
                )
            } else {
                (raw_name, ProviderCommandKind::Command)
            };
            (!name.trim().is_empty()).then(|| ProviderCommand {
                name: name.to_owned(),
                description: command
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                kind,
            })
        })
        .collect()
}

fn selected_thinking(selections: &[OptionSelection]) -> Option<&str> {
    selections
        .iter()
        .find(|selection| selection.id == crate::EFFORT_OPTION_ID)
        .and_then(|selection| selection.value.as_str())
}

fn resume_session(resume: &Option<ResumeCursor>) -> Option<&str> {
    resume.as_ref()?.str_field(&["session_file", "session_id"])
}

/// The tcode tool families this session does without, named the way the user
/// knows them from Settings.
///
/// pi has no MCP client (its README: "No MCP"), and its RPC protocol exposes no
/// registration hook, so explicitly enabled tcode MCP servers stay behind.
/// Naming them beats a blanket notice: the person reading it wants to know
/// which opted-in tools went missing, not that a protocol lacks a feature.
fn unattached_servers(opts: &SessionOptions) -> Vec<&'static str> {
    [(
        crate::McpRegistration::SERVER_NAME_COMPUTER_USE,
        "computer-use",
    )]
    .into_iter()
    .filter_map(|(server_name, label)| {
        opts.mcp_servers
            .iter()
            .any(|server| server.name == server_name)
            .then_some(label)
    })
    .collect()
}

/// The id of the last entry a `get_entries` response holds.
fn last_entry_id(response: &Value) -> Option<String> {
    response
        .pointer("/data/entries")?
        .as_array()?
        .last()?
        .get("id")?
        .as_str()
        .map(str::to_owned)
}

fn ensure_success(message: &Value) -> Result<(), AgentError> {
    if message
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        Ok(())
    } else {
        Err(AgentError::Provider(response_error(message)))
    }
}

fn response_error(message: &Value) -> String {
    message
        .get("error")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| {
            format!(
                "pi rejected {}",
                message.get("command").unwrap_or(&Value::Null)
            )
        })
}

fn send_json(writer: &mut impl Write, value: &Value) -> Result<(), AgentError> {
    write_json(writer, value, Some("serializing pi request"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TurnStatus;

    #[test]
    fn responds_to_pending_extension_dialog_with_value_or_cancellation() {
        let mut approvals = HashSet::from(["confirm".to_owned()]);
        let mut dialogs = HashSet::from(["dialog".to_owned()]);
        let mut output = Vec::new();
        let dismissed = dismiss_prompts(&mut output, &mut approvals, &mut dialogs).unwrap();
        assert!(approvals.is_empty() && dialogs.is_empty());
        let responses: Vec<Value> = serde_json::Deserializer::from_slice(&output)
            .into_iter()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            responses,
            [
                json!({"type":"extension_ui_response","id":"confirm","confirmed":false}),
                json!({"type":"extension_ui_response","id":"dialog","cancelled":true}),
            ]
        );
        assert!(
            matches!(dismissed.as_slice(), [
                AgentEvent::ApprovalResolved { request_id: approval, decision: ApprovalDecision::Cancel },
                AgentEvent::UserInputResolved { request_id: dialog, answers },
            ] if approval == "confirm" && dialog == "dialog" && answers.is_empty()),
            "{dismissed:?}"
        );

        for (answer, response) in [
            (
                Some(json!("custom answer")),
                json!({"type":"extension_ui_response","id":"dialog","value":"custom answer"}),
            ),
            (
                None,
                json!({"type":"extension_ui_response","id":"dialog","cancelled":true}),
            ),
        ] {
            let mut pending = HashSet::from(["dialog".to_owned()]);
            let answers =
                serde_json::Map::from_iter(answer.map(|answer| ("dialog".into(), answer)));
            let mut output = Vec::new();
            let resolved =
                answer_dialog(&mut output, &mut pending, "dialog".into(), answers.clone()).unwrap();
            assert_eq!(serde_json::from_slice::<Value>(&output).unwrap(), response);
            assert!(
                matches!(resolved, Some(AgentEvent::UserInputResolved { ref request_id, answers: ref resolved })
                    if request_id == "dialog" && *resolved == answers),
                "{resolved:?}"
            );
            assert!(pending.is_empty());
        }
        let mut output = Vec::new();
        assert!(
            answer_dialog(
                &mut output,
                &mut HashSet::new(),
                "dialog".into(),
                serde_json::Map::new()
            )
            .unwrap()
            .is_none()
        );
        assert!(output.is_empty());
    }

    /// The `notify` request of pi 1.5.1's `docs/rpc-extension-ui.md`, whose
    /// `notifyType` is "info", "warning" or "error", "info" when omitted.
    #[test]
    fn a_notification_reaches_the_user() {
        let notify = json!({
            "type":"extension_ui_request",
            "id":"1",
            "method":"notify",
            "message":"my-extension synced 3 files"
        });
        assert_eq!(
            extension_notice(&notify).as_deref(),
            Some("my-extension synced 3 files")
        );
        assert!(
            extension_notice(&json!({
                "type":"extension_ui_request","id":"1","method":"setStatus","statusKey":"sync","statusText":"busy"
            }))
            .is_none()
        );
    }

    /// The `select`, `input` and `editor` requests of pi 1.5.1's
    /// `docs/rpc-extension-ui.md`.
    #[test]
    fn maps_extension_dialog_requests_to_native_user_input() {
        let (id, select) = extension_dialog_question(&json!({
            "type":"extension_ui_request",
            "id":"select-id",
            "method":"select",
            "title":"Choose",
            "options":["a","b"],
            "timeout":10000
        }))
        .unwrap();
        assert_eq!(id, "select-id");
        assert_eq!(select.id, "select-id");
        assert_eq!(select.header, "pi");
        assert_eq!(select.question, "Choose");
        assert!(!select.multi_select);
        assert_eq!(select.prefill, None);
        assert_eq!(
            select
                .options
                .iter()
                .map(|option| (option.label.clone(), option.description.clone()))
                .collect::<Vec<_>>(),
            vec![("a".into(), String::new()), ("b".into(), String::new())]
        );

        let (_, input) = extension_dialog_question(&json!({
            "type":"extension_ui_request",
            "id":"input-id",
            "method":"input",
            "title":"Name",
            "placeholder":"type here"
        }))
        .unwrap();
        assert_eq!(input.question, "Name (type here)");
        assert!(input.options.is_empty());
        assert_eq!(input.prefill, None);

        let (_, editor) = extension_dialog_question(&json!({
            "type":"extension_ui_request",
            "id":"editor-id",
            "method":"editor",
            "title":"Edit",
            "prefill":"initial text"
        }))
        .unwrap();
        assert_eq!(editor.question, "Edit");
        assert!(editor.options.is_empty());
        assert_eq!(editor.prefill.as_deref(), Some("initial text"));
    }

    /// A model as pi 1.5.1's `docs/models-configuration.md` defines it, with
    /// its `thinkingLevelMap`.
    #[test]
    fn model_catalog_only_offers_and_defaults_to_supported_thinking_levels() {
        let model = json!({
            "id": "test-model",
            "provider": "test-provider",
            "reasoning": false,
            "thinkingLevelMap": {"xhigh": "xhigh", "max": "max"}
        });
        assert!(map_model(&model, None, None).unwrap().options.is_empty());

        // Pi's thinkingLevelMap contract: omitted keys use provider defaults,
        // null disables a level, and xhigh/max require explicit support.
        for (mapping, expected) in [
            (json!(null), vec!["off", "minimal", "low", "medium", "high"]),
            (
                json!({"low": "low"}),
                vec!["off", "minimal", "low", "medium", "high"],
            ),
            (
                json!({"minimal": null, "medium": null, "xhigh": null, "max": "max"}),
                vec!["off", "low", "high", "max"],
            ),
            (
                json!({"off": null, "xhigh": "xhigh", "max": null}),
                vec!["minimal", "low", "medium", "high", "xhigh"],
            ),
        ] {
            let mut model = json!({
                "id": "test-model",
                "provider": "test-provider",
                "reasoning": true
            });
            if !mapping.is_null() {
                model["thinkingLevelMap"] = mapping;
            }
            for selected in ["medium", "xhigh", "unsupported"] {
                let mapped = map_model(&model, None, Some(selected)).unwrap();
                let [
                    OptionDescriptor::Select {
                        options,
                        default_value,
                        ..
                    },
                ] = mapped.options.as_slice()
                else {
                    panic!("reasoning model must expose its supported thinking levels");
                };
                assert_eq!(
                    options
                        .iter()
                        .map(|option| option.value.as_str())
                        .collect::<Vec<_>>(),
                    expected,
                    "{model}"
                );
                assert_eq!(
                    default_value.as_deref(),
                    expected.contains(&selected).then_some(selected)
                );
            }
        }
    }

    /// `CustomMessage` of pi 1.5.1's `docs/message-types.md`: `customType`,
    /// `content` (text or text and image parts) and `display` are always set.
    #[test]
    fn displayed_custom_messages_become_work_log_items() {
        let mut mapper = PiMapper::new();
        for message in [
            json!({"role":"custom","customType":"my-extension","content":"hidden","display":false}),
            json!({"role":"custom","customType":"my-extension","content":[{"type":"image","data":"ignored"}],"display":true}),
        ] {
            assert!(
                mapper
                    .on_message(&json!({"type":"message_end","message":message}))
                    .is_empty()
            );
        }

        let plain = mapper.on_message(&json!({
            "type":"message_end",
            "message":{
                "role":"custom",
                "customType":"my-extension",
                "content":"plain text",
                "display":true,
                "timestamp":1785400000000_u64
            }
        }));
        assert!(matches!(
            plain.as_slice(),
            [AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::Other { provider_kind, summary },
                ..
            })] if provider_kind == "my-extension" && summary == "plain text"
        ));

        let parts = mapper.on_message(&json!({
            "type":"message_end",
            "message":{
                "role":"custom",
                "customType":"my-extension",
                "content":[
                    {"type":"text","text":"first"},
                    {"type":"image","data":"ignored"},
                    {"type":"text","text":"second"}
                ],
                "display":true,
                "timestamp":1785400000001_u64
            }
        }));
        assert!(matches!(
            parts.as_slice(),
            [AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::Other { provider_kind, summary },
                ..
            })] if provider_kind == "my-extension" && summary == "first\nsecond"
        ));
    }

    /// A provider request that failed, as pi 1.5.1 reports it: pi-agent-core's
    /// `streamAssistantResponse` turns the stream's `error` event into the
    /// `message_end` of an `AssistantMessage` (pi-ai `dist/types.d.ts`) whose
    /// `stopReason` is "error" and whose `errorMessage` the provider set
    /// (e.g. pi-ai `dist/api/openai-responses.js`), and the run ends.
    #[test]
    fn a_failed_provider_request_reaches_the_user_with_its_reason() {
        let mut mapper = PiMapper::new();
        let message = json!({
            "role":"assistant",
            "content":[],
            "api":"openai-responses",
            "provider":"openai",
            "model":"gpt-test",
            "usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,
                     "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},
            "stopReason":"error",
            "errorMessage":"429 rate limit reached for gpt-test",
            "timestamp":1785400000000_u64
        });
        let mut events = Vec::new();
        for line in [
            json!({"type":"agent_start"}),
            json!({"type":"message_start","message":message}),
            json!({"type":"message_end","message":message}),
            json!({"type":"turn_end","message":message,"toolResults":[]}),
            json!({"type":"agent_end","messages":[message]}),
            json!({"type":"agent_settled","aborted":false}),
        ] {
            events.extend(mapper.on_message(&line));
        }
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::Error { message, fatal: false }
                if message == "429 rate limit reached for gpt-test"
        )));
        assert!(matches!(
            events.last(),
            Some(AgentEvent::TurnCompleted {
                status: TurnStatus::Failed,
                ..
            })
        ));
    }

    /// A failed request pi retried and then got an answer for, as pi 1.5.2
    /// reports it (`dist/core/agent-session.js`): the failed attempt's
    /// `message_end`, `auto_retry_start` before it continues the run, and
    /// `auto_retry_end` with `success: true` once an assistant message ends
    /// without an error. The turn completed.
    #[test]
    fn a_request_that_succeeds_on_retry_completes_its_turn() {
        let mut mapper = PiMapper::new();
        let usage = json!({"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,
                           "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}});
        let failed = json!({
            "role":"assistant","content":[],"api":"openai-responses","provider":"openai",
            "model":"gpt-test","usage":usage,"stopReason":"error",
            "errorMessage":"429 rate limit reached for gpt-test","timestamp":1785400000000_u64
        });
        let answered = json!({
            "role":"assistant","content":[{"type":"text","text":"Done."}],
            "api":"openai-responses","provider":"openai","model":"gpt-test","usage":usage,
            "stopReason":"stop","timestamp":1785400002000_u64
        });
        let mut events = Vec::new();
        for line in [
            json!({"type":"agent_start"}),
            json!({"type":"message_start","message":failed}),
            json!({"type":"message_end","message":failed}),
            json!({"type":"turn_end","message":failed,"toolResults":[]}),
            json!({"type":"agent_end","messages":[failed]}),
            json!({"type":"auto_retry_start","attempt":1,"maxAttempts":3,"delayMs":2000,
                   "errorMessage":"429 rate limit reached for gpt-test"}),
            json!({"type":"agent_start"}),
            json!({"type":"message_start","message":answered}),
            json!({"type":"message_end","message":answered}),
            json!({"type":"auto_retry_end","success":true,"attempt":1}),
            json!({"type":"turn_end","message":answered,"toolResults":[]}),
            json!({"type":"agent_end","messages":[answered]}),
            json!({"type":"agent_settled","aborted":false}),
        ] {
            events.extend(mapper.on_message(&line));
        }
        assert!(matches!(
            events.last(),
            Some(AgentEvent::TurnCompleted {
                status: TurnStatus::Completed,
                ..
            })
        ));
    }

    /// Replays `pi --mode rpc` output recorded from pi 0.85.1 with a scripted
    /// provider: one prompt, an assistant message that thinks and calls
    /// `bash`, then a second assistant message that thinks and answers.
    fn recorded_rpc_events() -> Vec<Value> {
        include_str!("../tests/fixtures/pi/rpc_events.jsonl")
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn streamed_text(events: &[AgentEvent], kind: DeltaKind) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Delta {
                    kind: delta_kind,
                    text,
                    ..
                } if *delta_kind == kind => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Each `result` is what pi 1.4.0's own `bash`, `edit`, `write` and
    /// `read` tools returned in a scratch directory (`createToolDefinition(..)
    /// .execute`), in the `tool_execution_*` events `pi --mode rpc` forwards
    /// verbatim. The items keep bash's exit code, edit's unified patch, the
    /// written text and the read image; the failed bash fails its own item,
    /// not the turn pi finished.
    #[test]
    fn tool_results_keep_exit_code_patch_and_images() {
        let mut mapper = PiMapper::new();
        let mut completed = Vec::new();
        mapper.on_message(&json!({"type":"agent_start"}));
        for line in include_str!("../tests/fixtures/pi/tool_results.jsonl").lines() {
            for event in mapper.on_message(&serde_json::from_str(line).unwrap()) {
                if let AgentEvent::ItemCompleted(item) = event {
                    completed.push(item.content);
                }
            }
        }
        assert!(matches!(
            mapper
                .on_message(&json!({"type":"agent_settled","aborted":false}))
                .as_slice(),
            [AgentEvent::TurnCompleted {
                status: TurnStatus::Completed,
                ..
            }]
        ));
        assert_eq!(
            completed,
            [
                ItemContent::CommandExecution {
                    command: "echo hi; exit 3".into(),
                    input: Vec::new(),
                    output: "hi\n\n\nCommand exited with code 3".into(),
                    exit_code: Some(3),
                    status: ItemStatus::Failed,
                    changes: Vec::new(),
                },
                ItemContent::FileChange {
                    changes: vec![FileChange {
                        path: "a.txt".into(),
                        kind: FileChangeKind::Modify,
                        diff: Some(
                            "--- a.txt\n+++ a.txt\n@@ -1,2 +1,2 @@\n-hello\n+bye\n world\n".into()
                        ),
                    }],
                    status: ItemStatus::Completed,
                    output: Some("Successfully replaced 1 block(s) in a.txt.".into()),
                },
                ItemContent::FileChange {
                    changes: vec![FileChange {
                        path: "new.txt".into(),
                        kind: FileChangeKind::Create,
                        diff: Some("+fresh".into()),
                    }],
                    status: ItemStatus::Completed,
                    output: Some("Successfully wrote to new.txt".into()),
                },
                ItemContent::ToolCall {
                    image_reads: vec![Attachment {
                        media_type: "image/png".into(),
                        data_base64: "iVBORw0KGgoAAAANSUhEUgAAAAgAAAAICAIAAABLbSncAAAAEElEQVR4nGNgYPiPAw0pCQCpcD/BFMrqcwAAAABJRU5ErkJggg==".into(),
                        source_path: None,
                    }],
                    name: "read".into(),
                    input: json!({"path": "blue.png"}),
                    output: Some("Read image file [image/png]".into()),
                    status: ItemStatus::Completed,
                    family: Some(Box::new(ToolFamily::Read {
                        path: "blue.png".into(),
                        first_line: None,
                        note: None,
                    })),
                },
            ]
        );
    }

    /// `read` returns the lines as they are, from its `offset`, and when
    /// its `limit` stopped short of the file's end, a blank line and a notice
    /// of what is left (1.5.2's `dist/core/tools/read.js`; content invented).
    #[test]
    fn a_read_is_the_lines_it_read_from_its_offset() {
        assert_eq!(
            tool_family(
                "read",
                &json!({"path": "src/lib.rs", "offset": 40, "limit": 2}),
                Some(
                    "fn main() {\n}\n\n[48 more lines in file. Use offset=42 to continue.]".into()
                ),
                None,
                ItemStatus::Completed,
            ),
            (
                Some(ToolFamily::Read {
                    path: "src/lib.rs".into(),
                    first_line: Some(40),
                    note: Some("[48 more lines in file. Use offset=42 to continue.]".into()),
                }),
                Some("fn main() {\n}".into()),
            )
        );
    }

    /// grep names a match `path:N: text` under the directory it searched,
    /// a context line `path-N- text`, and its limits in a `[…]` notice after
    /// a blank line; ls lists the names in its directory (1.5.2's
    /// `dist/core/tools/grep.js` and `ls.js`; content invented).
    // The fixture's paths are as the harness prints them on a POSIX machine.
    #[cfg(unix)]
    #[test]
    fn a_search_is_what_it_found_under_where_it_looked() {
        let (family, _) = tool_family(
            "grep",
            &json!({"pattern": "TODO", "path": "src"}),
            Some("a.rs-2- fn a() {\na.rs:3: // TODO: one\n\n[100 matches limit reached. Use limit=200 for more, or refine pattern]".into()),
            None,
            ItemStatus::Completed,
        );
        assert_eq!(
            family,
            Some(ToolFamily::Search {
                scope: SearchScope::Content,
                pattern: Some("TODO".into()),
                path: Some("src".into()),
                glob: None,
                hits: Some(vec![SearchHit {
                    path: "src/a.rs".into(),
                    line: Some(3),
                    text: Some("// TODO: one".into()),
                }]),
            })
        );
        let (family, _) = tool_family(
            "ls",
            &json!({"path": "src"}),
            Some("(empty directory)".into()),
            None,
            ItemStatus::Completed,
        );
        assert!(matches!(
            family,
            Some(ToolFamily::Search { scope: SearchScope::Entries, hits: Some(hits), .. }) if hits.is_empty()
        ));
    }

    /// The tool_search extension names the tools it loaded in
    /// `details.loaded` (1.5.2's `dist/extensions/tool-search/tool.js`;
    /// content invented).
    #[test]
    fn a_tool_search_is_the_tools_it_loaded() {
        let (family, _) = tool_family(
            "tool_search",
            &json!({"query": "browser"}),
            Some("Loaded 1 tool. They are available from your next call:\n- browser_open: Open a page".into()),
            Some(&json!({"content": [], "details": {"loaded": ["browser_open"]}})),
            ItemStatus::Completed,
        );
        assert_eq!(
            family,
            Some(ToolFamily::LoadTools {
                query: "browser".into(),
                tools: Some(vec!["browser_open".into()]),
            })
        );
    }

    #[test]
    fn maps_recorded_rpc_fixture() {
        let mut mapper = PiMapper::new();
        let mut events = Vec::new();
        for message in recorded_rpc_events() {
            events.extend(mapper.on_message(&message));
        }
        let mut streamed: Vec<(String, DeltaKind)> = Vec::new();
        for event in &events {
            if let AgentEvent::Delta { item_id, kind, .. } = event
                && !streamed.iter().any(|(id, _)| id == item_id)
            {
                streamed.push((item_id.clone(), *kind));
            }
        }
        let streamed_ids: Vec<&str> = streamed.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            streamed_ids,
            [
                "pi-assistant-1789592281572:0",
                "pi-assistant-1789592281595:0",
                "pi-assistant-1789592281595:1"
            ],
            "each assistant message streams under its own id"
        );
        for (item_id, kind) in &streamed {
            let completed: Vec<&ItemContent> = events
                .iter()
                .filter_map(|event| match event {
                    AgentEvent::ItemCompleted(item) if item.id == *item_id => Some(&item.content),
                    _ => None,
                })
                .collect();
            assert_eq!(completed.len(), 1, "{item_id} completes exactly once");
            assert!(
                matches!(
                    (completed[0], kind),
                    (ItemContent::Reasoning { .. }, DeltaKind::ReasoningText)
                        | (
                            ItemContent::AssistantMessage { .. },
                            DeltaKind::AssistantText
                        )
                ),
                "{item_id} completes as the kind it streamed"
            );
        }
        assert!(
            !events.iter().any(|event| matches!(
                event,
                AgentEvent::ItemCompleted(ThreadItem { id, content: ItemContent::Reasoning { .. } | ItemContent::AssistantMessage { .. }, .. })
                    if !streamed_ids.contains(&id.as_str())
            )),
            "no assistant part completes under an id that was never streamed"
        );

        assert!(matches!(events[0], AgentEvent::TurnStarted { .. }));
        assert!(!events.iter().any(|event| matches!(
            event,
            AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::UserMessage { text, .. },
                ..
            }) if text == "DO NOT ECHO"
        )));
        assert_eq!(streamed_text(&events, DeltaKind::AssistantText), "PONG");
        assert_eq!(
            streamed_text(&events, DeltaKind::ReasoningText),
            "CheckingTool done"
        );
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ItemUpdated(ThreadItem { content: ItemContent::CommandExecution { output, .. }, .. }) if output == "ok"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ItemCompleted(ThreadItem { content: ItemContent::CommandExecution { command, output, .. }, .. })
                if command == "printf ok" && output == "ok"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::TokenUsage(TokenUsage {
                input_tokens: Some(12),
                output_tokens: Some(3),
                ..
            })
        )));
        // The turn's two assistant messages used 19 and 27 tokens; each is
        // reported by its `message_end` and again by its `turn_end`.
        assert!(matches!(
            events.last(),
            Some(AgentEvent::TurnCompleted {
                status: TurnStatus::Completed,
                usage: Some(TokenUsage {
                    used_tokens: Some(46),
                    total_processed_tokens: Some(46),
                    ..
                }),
                ..
            })
        ));
    }
}
