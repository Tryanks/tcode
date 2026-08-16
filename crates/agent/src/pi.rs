//! Native pi provider: persistent `pi --mode rpc` JSONL transport.
//!
//! RPC records are framed by LF only. A bundled extension supplies the
//! permission boundary that pi intentionally leaves to hosts and translates
//! its confirmation UI into tcode's canonical approval events.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};

use serde_json::{Value, json};
use smol::channel::{Receiver, Sender};

#[cfg(test)]
use crate::TurnStatus;
use crate::actor::{self, EventSenderExt as _, SessionActor, TransportOutcome};
use crate::process::{ChildOutput, send_json as write_json, spawn_line_reader};
use crate::{
    AgentError, AgentEvent, ApprovalDecision, ApprovalKind, ApprovalMode, ApprovalRequest,
    Attachment, DeltaKind, FileChange, FileChangeKind, InteractionMode, ItemContent, ItemStatus,
    LaunchEnv, ModelSpec, OptionDescriptor, OptionSelection, ProviderCommand, ProviderCommandKind,
    ProviderKind, ResumeCursor, SelectOption, SessionCommand, SessionHandle, SessionOptions,
    ThreadItem, TokenUsage, UserInputOption, UserInputQuestion,
};

const PERMISSION_EXTENSION: &str = include_str!("../assets/pi/tcode-permissions.ts");
const PLAN_MODE_WARNING: &str =
    "pi RPC has no native Plan interaction mode; this session is running in Build mode";

pub async fn start(opts: SessionOptions) -> Result<SessionHandle, AgentError> {
    if opts.fork {
        return Err(AgentError::Protocol(
            "session fork is not supported for this provider".into(),
        ));
    }
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
) -> Result<Vec<ModelSpec>, AgentError> {
    crate::process::unblock(move || list_models_blocking(binary_path.as_deref(), &launch_env)).await
}

fn list_models_blocking(
    binary_path: Option<&Path>,
    launch_env: &LaunchEnv,
) -> Result<Vec<ModelSpec>, AgentError> {
    let binary = crate::resolve_binary(binary_path, "pi")?;
    let mut cmd = crate::process::command(&binary);
    cmd.arg("--mode")
        .arg("rpc")
        .arg("--no-session")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in launch_env.pairs(ProviderKind::Pi) {
        cmd.env(key, value);
    }
    let mut child = cmd
        .spawn()
        .map_err(|err| AgentError::Spawn(format!("spawning `{}`: {err}", binary.display())))?;
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
        let mut levels = vec!["off", "minimal", "low", "medium", "high", "xhigh"];
        if model
            .pointer("/thinkingLevelMap/max")
            .and_then(Value::as_str)
            .is_some()
        {
            levels.push("max");
        }
        let default_value = thinking_level
            .filter(|level| levels.contains(level))
            .map(str::to_owned);
        options.push(OptionDescriptor::Select {
            id: "reasoningEffort".into(),
            label: "Thinking".into(),
            options: levels
                .into_iter()
                .map(|level| SelectOption {
                    value: level.into(),
                    label: thinking_label(level),
                    description: None,
                })
                .collect(),
            default_value,
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
    let extension = if gate_required(opts.approval_mode) {
        match materialize_permission_extension() {
            Ok(path) => Some(path),
            Err(err) => {
                let _ = ready.send(Err(err)).await;
                return;
            }
        }
    } else {
        None
    };
    let mut cmd = crate::process::command(&binary);
    // Profile arguments are applied first. The transport, optional permission
    // extension, resume target, and read-only tool set are tcode-owned and go
    // last so a profile cannot accidentally override the safety boundary.
    cmd.args(&opts.extra_args).arg("--mode").arg("rpc");
    if let Some(extension) = extension {
        cmd.arg("--extension").arg(extension);
    }
    if let Some(thinking) = selected_thinking(&opts.option_selections) {
        cmd.arg("--thinking").arg(thinking);
    }
    if let Some(session) = resume_session(&opts.resume) {
        cmd.arg("--session").arg(session);
    }
    if opts.approval_mode == ApprovalMode::ReadOnly {
        cmd.arg("--tools").arg("read,grep,find,ls");
    }
    cmd.current_dir(&opts.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in opts.launch_env.pairs(ProviderKind::Pi) {
        cmd.env(key, value);
    }
    if gate_required(opts.approval_mode) {
        cmd.env(
            "TCODE_PI_APPROVAL_MODE",
            pi_approval_mode(opts.approval_mode),
        )
        .env("TCODE_PI_CWD", &opts.cwd);
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
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
        child,
        stdin,
        lines: line_rx,
        events,
        mapper: PiMapper::new(),
        next_request: 1,
        approval_mode: opts.approval_mode,
        pending_approvals: HashMap::new(),
        pending_dialogs: HashSet::new(),
        approved_for_session: HashSet::new(),
        pending_steers: HashMap::new(),
        requested_model: opts.model.clone(),
        stderr_tail,
    };
    let startup = actor.initialize().await;
    if let Err(err) = startup {
        actor.stop();
        let details = actor.stderr_tail.append_to(err.to_string(), "\nstderr:\n");
        let _ = ready.send(Err(AgentError::Provider(details))).await;
        return;
    }
    if opts.interaction_mode == InteractionMode::Plan {
        actor
            .events
            .emit(AgentEvent::Warning {
                message: PLAN_MODE_WARNING.into(),
            })
            .await;
    }
    let unattached = unattached_servers(&opts);
    if !unattached.is_empty() {
        actor.events
            .emit(AgentEvent::Warning {
                message: format!(
                    "pi has no MCP client of its own, so tcode's {} tools are unavailable in this session",
                    unattached.join(", ")
                ),
            })
            .await;
    }
    if ready.send(Ok(())).await.is_err() {
        actor.stop();
        return;
    }

    actor::run(actor, &commands).await;
}

struct PiActor {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    lines: Receiver<ChildOutput>,
    events: Sender<AgentEvent>,
    mapper: PiMapper,
    next_request: u64,
    approval_mode: ApprovalMode,
    pending_approvals: HashMap<String, String>,
    pending_dialogs: HashSet<String>,
    approved_for_session: HashSet<String>,
    /// Native prompt request id -> canonical steering request id. pi's prompt
    /// response is only an acceptance signal, but it is stronger than merely
    /// acknowledging the stdin write.
    pending_steers: HashMap<String, String>,
    requested_model: Option<String>,
    stderr_tail: crate::process::StderrTail,
}

impl SessionActor for PiActor {
    type TransportItem = ChildOutput;

    fn transport(&self) -> &Receiver<Self::TransportItem> {
        &self.lines
    }

    fn events(&self) -> &Sender<AgentEvent> {
        &self.events
    }

    fn command_failure_reason(&self) -> &'static str {
        "pi protocol write failed"
    }

    async fn handle_command(&mut self, command: SessionCommand) -> Result<(), String> {
        PiActor::handle_command(self, command).await
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

    async fn settle_shutdown(&mut self) {
        let _ = self.cancel_pending_ui();
    }

    async fn teardown(mut self, reason: Option<String>) -> Option<String> {
        self.stop();
        reason.map(|base| self.stderr_tail.append_to(base, "\nstderr:\n"))
    }
}

impl PiActor {
    async fn initialize(&mut self) -> Result<(), AgentError> {
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
        self.events
            .emit(AgentEvent::SessionStarted {
                provider_session_id: session_id,
                resume: ResumeCursor(Value::Object(cursor)),
                model,
            })
            .await;

        send_json(
            &mut self.stdin,
            &json!({"id":"tcode-commands","type":"get_commands"}),
        )?;
        let commands = match self.wait_response("tcode-commands").await {
            Ok(response) => map_commands(&response),
            Err(err) => {
                log::debug!("pi get_commands unavailable: {err}");
                Vec::new()
            }
        };
        self.events
            .emit(AgentEvent::ProviderCommands { commands })
            .await;
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
                    // Extensions load before this handshake completes, so the
                    // one notification that explains a broken extension arrives
                    // while we are still here. Everything else that lands early
                    // belongs to the mapper, not to startup.
                    if let Some(warning) = extension_warning(&message) {
                        self.events
                            .emit(AgentEvent::Warning { message: warning })
                            .await;
                    } else if is_extension_dialog(&message)
                        && let Some(request_id) = message.get("id").and_then(Value::as_str)
                    {
                        send_json(
                            &mut self.stdin,
                            &json!({
                                "type":"extension_ui_response",
                                "id":request_id,
                                "cancelled":true
                            }),
                        )?;
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
            let success = message
                .get("success")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if success {
                if let Some(request_id) = message
                    .get("id")
                    .and_then(Value::as_str)
                    .and_then(|id| self.pending_steers.remove(id))
                {
                    self.events
                        .emit(AgentEvent::SteerAccepted { request_id })
                        .await;
                }
            } else {
                self.events
                    .emit(AgentEvent::Error {
                        message: response_error(&message),
                        fatal: false,
                    })
                    .await;
            }
            return;
        }
        if message.get("type").and_then(Value::as_str) == Some("extension_ui_request") {
            self.handle_extension_ui(&message).await;
            return;
        }
        let events = self.mapper.on_message(&message);
        let turn_completed = events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnCompleted { .. }));
        for event in events {
            self.events.emit(event).await;
        }
        if turn_completed {
            let _ = self.cancel_pending_ui();
        }
    }

    async fn handle_extension_ui(&mut self, message: &Value) {
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
                })
                .await;
            return;
        }
        if method != "confirm" {
            if let Some(warning) = extension_warning(message) {
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
        let payload: Value = message
            .get("message")
            .and_then(Value::as_str)
            .and_then(|text| serde_json::from_str(text).ok())
            .unwrap_or(Value::Null);
        let tool_name = payload
            .get("toolName")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        if self.approved_for_session.contains(&tool_name) {
            let _ = send_json(
                &mut self.stdin,
                &json!({"type":"extension_ui_response","id":id,"confirmed":true}),
            );
            return;
        }
        self.pending_approvals
            .insert(id.to_owned(), tool_name.clone());
        self.events
            .emit(AgentEvent::ApprovalRequested(ApprovalRequest {
                id: id.to_owned(),
                turn_id: self.mapper.current_turn.clone(),
                kind: approval_kind(&tool_name, &payload),
                options: Vec::new(),
            }))
            .await;
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
                self.cancel_pending_ui()?;
                send_json(&mut self.stdin, &json!({"type":"abort"})).map_err(|err| err.to_string())
            }
            SessionCommand::RespondApproval {
                request_id,
                decision,
            } => {
                let Some(tool_name) = self.pending_approvals.remove(&request_id) else {
                    return Ok(());
                };
                let confirmed = matches!(
                    decision,
                    ApprovalDecision::Approve | ApprovalDecision::ApproveForSession
                );
                if decision == ApprovalDecision::ApproveForSession {
                    self.approved_for_session.insert(tool_name);
                }
                send_json(
                    &mut self.stdin,
                    &json!({"type":"extension_ui_response","id":request_id,"confirmed":confirmed}),
                )
                .map_err(|err| err.to_string())?;
                if decision == ApprovalDecision::Cancel {
                    self.mapper.interrupt_pending = true;
                    self.cancel_pending_ui()?;
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
            SessionCommand::SetOption { id, value } if id == "reasoningEffort" => {
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
            SessionCommand::SetApprovalMode(mode) => {
                if mode != self.approval_mode {
                    self.events
                        .emit(AgentEvent::Warning {
                            message: "pi permission changes require restarting the session".into(),
                        })
                        .await;
                }
                Ok(())
            }
            SessionCommand::SetInteractionMode(mode) => {
                if mode == InteractionMode::Plan {
                    self.events
                        .emit(AgentEvent::Warning {
                            message: PLAN_MODE_WARNING.into(),
                        })
                        .await;
                }
                Ok(())
            }
            SessionCommand::RespondUserInput {
                request_id,
                answers,
            } => {
                let Some(response) = take_extension_dialog_response(
                    &mut self.pending_dialogs,
                    &request_id,
                    &answers,
                ) else {
                    return Ok(());
                };
                send_json(&mut self.stdin, &response).map_err(|err| err.to_string())?;
                Ok(())
            }
            SessionCommand::Rewind { .. } => {
                self.events
                    .emit(AgentEvent::Warning {
                        message: "pi rewind is not exposed by tcode's native adapter".into(),
                    })
                    .await;
                Ok(())
            }
            SessionCommand::SetOption { .. } | SessionCommand::Shutdown => Ok(()),
        }
    }

    fn request_id(&mut self) -> String {
        let id = format!("tcode-{}", self.next_request);
        self.next_request += 1;
        id
    }

    fn cancel_pending_ui(&mut self) -> Result<(), String> {
        for id in self.pending_approvals.drain().map(|(id, _)| id) {
            send_json(
                &mut self.stdin,
                &json!({"type":"extension_ui_response","id":id,"confirmed":false}),
            )
            .map_err(|err| err.to_string())?;
        }
        cancel_pending_dialogs(&mut self.stdin, &mut self.pending_dialogs)
            .map_err(|err| err.to_string())
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
    finalized_messages: HashSet<String>,
}

#[derive(Clone)]
struct PiTool {
    name: String,
    input: Value,
    output: String,
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
            finalized_messages: HashSet::new(),
        }
    }

    fn on_message(&mut self, message: &Value) -> Vec<AgentEvent> {
        match message.get("type").and_then(Value::as_str).unwrap_or("") {
            "agent_start" => self.start_turn(),
            "agent_settled" => self.complete_turn(),
            "message_update" => self.message_update(message),
            "message_end" => self.message_end(message),
            // Current pi emits both message_end and turn_end. Treat turn_end as
            // a reconciliation fallback; usage is de-duplicated by message id.
            "turn_end" => self.message_end(message),
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
                    vec![AgentEvent::ContextCompacted]
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
            "auto_retry_end"
                if !message
                    .get("success")
                    .and_then(Value::as_bool)
                    .unwrap_or(false) =>
            {
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
                let id = tool
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("pi-tool")
                    .to_owned();
                let name = tool
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_owned();
                let input = tool
                    .get("arguments")
                    .or_else(|| tool.get("input"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let existed = self
                    .tool_items
                    .insert(
                        id.clone(),
                        PiTool {
                            name: name.clone(),
                            input: input.clone(),
                            output: String::new(),
                        },
                    )
                    .is_some();
                let item = tool_item(&id, &name, input, String::new(), ItemStatus::InProgress);
                return vec![if existed {
                    AgentEvent::ItemUpdated(item)
                } else {
                    AgentEvent::ItemStarted(item)
                }];
            }
            Some("error") => {
                self.failed = event.get("reason").and_then(Value::as_str) != Some("aborted");
                return vec![AgentEvent::Error {
                    message: event
                        .get("error")
                        .or_else(|| event.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or("pi assistant stream failed")
                        .to_owned(),
                    fatal: false,
                }];
            }
            _ => return Vec::new(),
        };
        let delta = event.get("delta").and_then(Value::as_str).unwrap_or("");
        if delta.is_empty() {
            return Vec::new();
        }
        let index = crate::json_u64(event.get("contentIndex")).unwrap_or(0);
        let message_id = assistant_message_id(message.get("message").unwrap_or(&Value::Null));
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
                    id: assistant_message_id(message),
                    parent_item_id: None,
                    content: ItemContent::Other {
                        provider_kind,
                        summary,
                    },
                })]
            }
            Some("assistant") => {
                let mut events = Vec::new();
                let id = assistant_message_id(message);
                let first_finalization = self.finalized_messages.insert(id.clone());
                if first_finalization
                    && let Some(content) = message.get("content").and_then(Value::as_array)
                {
                    for (index, block) in content.iter().enumerate() {
                        let item = match block.get("type").and_then(Value::as_str) {
                            Some("text") => Some(ItemContent::AssistantMessage {
                                text: block
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_owned(),
                            }),
                            Some("thinking") => Some(ItemContent::Reasoning {
                                text: block
                                    .get("thinking")
                                    .or_else(|| block.get("text"))
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
                if first_finalization && let Some(usage) = map_usage(message.get("usage")) {
                    let processed = crate::processed_tokens(usage);
                    self.cumulative_processed = self.cumulative_processed.saturating_add(processed);
                    let usage = TokenUsage {
                        total_processed_tokens: Some(self.cumulative_processed),
                        ..usage
                    };
                    self.turn_usage.get_or_insert_default().merge(usage);
                    events.push(AgentEvent::TokenUsage(usage));
                }
                if message.get("stopReason").and_then(Value::as_str) == Some("error") {
                    self.failed = true;
                }
                events
            }
            _ => Vec::new(),
        }
    }

    fn tool_event(
        &mut self,
        event: &Value,
        status: ItemStatus,
        has_result: bool,
    ) -> Vec<AgentEvent> {
        let id = event
            .get("toolCallId")
            .and_then(Value::as_str)
            .unwrap_or("pi-tool")
            .to_owned();
        let event_name = event.get("toolName").and_then(Value::as_str);
        let event_input = event.get("args").filter(|value| !value.is_null()).cloned();
        let result = if event.get("type").and_then(Value::as_str) == Some("tool_execution_update") {
            event.get("partialResult")
        } else {
            event.get("result")
        };
        let output = result.map(result_text).unwrap_or_default();
        let was_known = self.tool_items.contains_key(&id);
        let tool = self.tool_items.entry(id.clone()).or_insert_with(|| PiTool {
            name: event_name.unwrap_or("tool").to_owned(),
            input: event_input.clone().unwrap_or(Value::Null),
            output: String::new(),
        });
        if let Some(name) = event_name {
            tool.name = name.to_owned();
        }
        if let Some(input) = event_input {
            tool.input = input;
        }
        if has_result {
            tool.output = output;
        }
        if status == ItemStatus::Failed {
            self.failed = true;
        }
        let item = tool_item(
            &id,
            &tool.name,
            tool.input.clone(),
            tool.output.clone(),
            status,
        );
        vec![match status {
            ItemStatus::InProgress if has_result => AgentEvent::ItemUpdated(item),
            ItemStatus::InProgress if was_known => AgentEvent::ItemUpdated(item),
            ItemStatus::InProgress => AgentEvent::ItemStarted(item),
            _ => AgentEvent::ItemCompleted(item),
        }]
    }
}

fn tool_item(id: &str, name: &str, input: Value, output: String, status: ItemStatus) -> ThreadItem {
    let content = match name {
        "bash" => ItemContent::CommandExecution {
            command: input
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            output,
            exit_code: None,
            status,
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
                        diff: None,
                    }]
                })
                .unwrap_or_default(),
            status,
        },
        _ => ItemContent::ToolCall {
            name: name.to_owned(),
            input,
            output: (!output.is_empty()).then_some(output),
            status,
        },
    };
    crate::normalize::thread_item(id, content)
}

fn approval_kind(tool_name: &str, payload: &Value) -> ApprovalKind {
    let input = payload.get("input").cloned().unwrap_or(Value::Null);
    let reason = payload
        .get("reason")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if payload.get("source").and_then(Value::as_str) == Some("extension") {
        let path = payload
            .get("extensionPath")
            .and_then(Value::as_str)
            .unwrap_or("(unknown path)");
        let mut source = format!("extension tool from {path}");
        if payload
            .get("shadowsBuiltin")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            source.push_str(&format!(" (overrides builtin {tool_name})"));
        }
        let detail = match reason {
            Some(reason) => format!("{reason}; {source}"),
            None => source,
        };
        return ApprovalKind::ToolUse {
            name: tool_name.to_owned(),
            input,
            detail,
        };
    }
    match tool_name {
        "bash" => ApprovalKind::ExecCommand {
            command: input
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            cwd: payload
                .get("cwd")
                .and_then(Value::as_str)
                .map(str::to_owned),
            reason,
        },
        "edit" | "write" => ApprovalKind::FileChange {
            changes: input
                .get("path")
                .and_then(Value::as_str)
                .map(|path| {
                    vec![FileChange {
                        path: path.to_owned(),
                        kind: if tool_name == "write" {
                            FileChangeKind::Create
                        } else {
                            FileChangeKind::Modify
                        },
                        diff: None,
                    }]
                })
                .unwrap_or_default(),
            reason,
        },
        _ => ApprovalKind::ToolUse {
            name: tool_name.to_owned(),
            detail: reason.unwrap_or_else(|| input.to_string()),
            input,
        },
    }
}

/// The text of a warning-or-worse `notify` from a pi extension.
///
/// pi's fire-and-forget UI methods expect no response, so an extension that
/// reports trouble this way is otherwise invisible to the user — including a
/// user's own installed extension explaining why it could not start.
fn extension_warning(message: &Value) -> Option<String> {
    if message.get("type").and_then(Value::as_str) != Some("extension_ui_request")
        || message.get("method").and_then(Value::as_str) != Some("notify")
    {
        return None;
    }
    let level = message
        .get("notifyType")
        .and_then(Value::as_str)
        .unwrap_or("info");
    if !matches!(level, "warning" | "error") {
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

fn take_extension_dialog_response(
    pending_dialogs: &mut HashSet<String>,
    request_id: &str,
    answers: &serde_json::Map<String, Value>,
) -> Option<Value> {
    if !pending_dialogs.remove(request_id) {
        return None;
    }
    Some(match answers.get(request_id).and_then(Value::as_str) {
        Some(value) => json!({"type":"extension_ui_response","id":request_id,"value":value}),
        None => json!({"type":"extension_ui_response","id":request_id,"cancelled":true}),
    })
}

fn cancel_pending_dialogs(
    writer: &mut impl Write,
    pending_dialogs: &mut HashSet<String>,
) -> Result<(), AgentError> {
    for id in pending_dialogs.drain() {
        send_json(
            writer,
            &json!({"type":"extension_ui_response","id":id,"cancelled":true}),
        )?;
    }
    Ok(())
}

fn result_text(result: &Value) -> String {
    result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

fn assistant_message_id(message: &Value) -> String {
    message
        .get("responseId")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| {
            format!(
                "pi-assistant-{}",
                crate::json_u64(message.get("timestamp")).unwrap_or(0)
            )
        })
}

fn map_usage(usage: Option<&Value>) -> Option<TokenUsage> {
    let usage = usage?;
    let input = crate::json_u64(usage.get("input"));
    let output = crate::json_u64(usage.get("output"));
    let cache_read = crate::json_u64(usage.get("cacheRead"));
    (input.is_some() || output.is_some() || cache_read.is_some()).then_some(
        crate::normalize::token_usage(
            input,
            cache_read,
            output,
            crate::json_u64(usage.get("totalTokens")),
        ),
    )
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
        .find(|selection| selection.id == "reasoningEffort")
        .and_then(|selection| selection.value.as_str())
}

fn resume_session(resume: &Option<ResumeCursor>) -> Option<&str> {
    resume.as_ref()?.str_field(&["session_file", "session_id"])
}

fn pi_approval_mode(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::Supervised => "supervised",
        ApprovalMode::ReadOnly => "read_only",
        ApprovalMode::AutoAcceptEdits => "auto_accept_edits",
        ApprovalMode::FullAccess => "full_access",
    }
}

fn gate_required(mode: ApprovalMode) -> bool {
    matches!(
        mode,
        ApprovalMode::Supervised | ApprovalMode::AutoAcceptEdits
    )
}

/// The tcode tool families this session does without, named the way the user
/// knows them from Settings.
///
/// pi has no MCP client (its README: "No MCP"), and its RPC protocol exposes no
/// registration hook, so explicitly enabled tcode MCP servers stay behind.
/// Naming them beats a blanket notice: the person reading it wants to know
/// which opted-in tools went missing, not that a protocol lacks a feature.
fn unattached_servers(opts: &SessionOptions) -> Vec<&'static str> {
    [
        (
            crate::McpRegistration::SERVER_NAME_ORCHESTRATE,
            "orchestration",
        ),
        (
            crate::McpRegistration::SERVER_NAME_COMPUTER_USE,
            "computer-use",
        ),
    ]
    .into_iter()
    .filter_map(|(server_name, label)| {
        opts.mcp_servers
            .iter()
            .any(|server| server.name == server_name)
            .then_some(label)
    })
    .collect()
}

fn materialize_permission_extension() -> Result<PathBuf, AgentError> {
    let directory = std::env::temp_dir().join(format!("tcode-pi-extension-{}", std::process::id()));
    std::fs::create_dir_all(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    }
    let path = directory.join(format!(
        "tcode-permissions-{}.ts",
        env!("CARGO_PKG_VERSION")
    ));
    std::fs::write(&path, PERMISSION_EXTENSION)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(path)
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
        .and_then(|error| {
            error
                .as_str()
                .map(str::to_owned)
                .or_else(|| error.get("message")?.as_str().map(str::to_owned))
        })
        .or_else(|| message.get("message")?.as_str().map(str::to_owned))
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

    #[test]
    fn permission_gate_is_only_required_for_supervised_modes() {
        assert!(gate_required(ApprovalMode::Supervised));
        assert!(gate_required(ApprovalMode::AutoAcceptEdits));
        assert!(!gate_required(ApprovalMode::FullAccess));
        assert!(!gate_required(ApprovalMode::ReadOnly));
    }

    #[test]
    fn extension_bash_approval_is_tool_use() {
        let approval = approval_kind(
            "bash",
            &json!({
                "toolName": "bash",
                "source": "extension",
                "extensionPath": "/extensions/bash.ts",
                "input": { "command": "x" }
            }),
        );
        assert!(matches!(
            approval,
            ApprovalKind::ToolUse { name, input, .. }
                if name == "bash" && input == json!({ "command": "x" })
        ));
    }

    #[test]
    fn builtin_bash_approval_remains_exec_command() {
        let approval = approval_kind(
            "bash",
            &json!({
                "toolName": "bash",
                "input": { "command": "x" },
                "cwd": "/project"
            }),
        );
        assert!(matches!(
            approval,
            ApprovalKind::ExecCommand { command, cwd, .. }
                if command == "x" && cwd.as_deref() == Some("/project")
        ));
    }

    #[test]
    fn extension_tool_approval_detail_contains_path() {
        let approval = approval_kind(
            "hello_world",
            &json!({
                "toolName": "hello_world",
                "source": "extension",
                "extensionPath": "/extensions/hello.ts",
                "input": {}
            }),
        );
        assert!(matches!(
            approval,
            ApprovalKind::ToolUse { name, detail, .. }
                if name == "hello_world" && detail.contains("/extensions/hello.ts")
        ));
    }

    #[test]
    fn shadowed_builtin_is_noted_in_extension_approval_detail() {
        let approval = approval_kind(
            "bash",
            &json!({
                "toolName": "bash",
                "source": "extension",
                "extensionPath": "/extensions/bash.ts",
                "shadowsBuiltin": true,
                "input": { "command": "x" },
                "reason": "requires confirmation"
            }),
        );
        assert!(matches!(
            approval,
            ApprovalKind::ToolUse { detail, .. }
                if detail.contains("requires confirmation")
                    && detail.contains("/extensions/bash.ts")
                    && detail.contains("overrides builtin bash")
        ));
    }

    #[test]
    fn only_warning_notifications_become_session_warnings() {
        let notify = |level: &str| {
            json!({
                "type":"extension_ui_request",
                "id":"1",
                "method":"notify",
                "message":"my-extension could not start",
                "notifyType":level
            })
        };
        assert_eq!(
            extension_warning(&notify("warning")).as_deref(),
            Some("my-extension could not start")
        );
        assert_eq!(
            extension_warning(&notify("error")).as_deref(),
            Some("my-extension could not start")
        );
        assert!(extension_warning(&notify("info")).is_none());
        assert!(
            extension_warning(&json!({
                "type":"extension_ui_request","id":"1","method":"setStatus","statusText":"busy"
            }))
            .is_none()
        );
        assert!(extension_warning(&json!({"type":"agent_start"})).is_none());
    }

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

    #[test]
    fn responds_to_pending_extension_dialog_with_value_or_cancellation() {
        let mut pending = HashSet::from(["dialog".to_owned()]);
        let answers = serde_json::Map::from_iter([("dialog".into(), json!("custom answer"))]);
        assert_eq!(
            take_extension_dialog_response(&mut pending, "dialog", &answers),
            Some(json!({
                "type":"extension_ui_response",
                "id":"dialog",
                "value":"custom answer"
            }))
        );
        assert!(pending.is_empty());

        for answers in [
            serde_json::Map::new(),
            serde_json::Map::from_iter([("dialog".into(), json!(["not", "a string"]))]),
        ] {
            pending.insert("dialog".into());
            assert_eq!(
                take_extension_dialog_response(&mut pending, "dialog", &answers),
                Some(json!({
                    "type":"extension_ui_response",
                    "id":"dialog",
                    "cancelled":true
                }))
            );
            assert!(pending.is_empty());
        }
    }

    #[test]
    fn cleanup_cancels_pending_extension_dialogs() {
        let mut pending = HashSet::from(["dialog".to_owned()]);
        let mut output = Vec::new();
        cancel_pending_dialogs(&mut output, &mut pending).unwrap();
        assert!(pending.is_empty());
        let response: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(
            response,
            json!({
                "type":"extension_ui_response",
                "id":"dialog",
                "cancelled":true
            })
        );
    }

    #[test]
    fn map_model_uses_supported_state_thinking_level_as_default() {
        let model = json!({
            "id": "gpt-test",
            "provider": "openai",
            "reasoning": true
        });
        let mapped = map_model(&model, None, Some("xhigh")).unwrap();
        assert!(matches!(
            mapped.options.as_slice(),
            [OptionDescriptor::Select {
                default_value: Some(level),
                ..
            }] if level == "xhigh"
        ));

        let mapped = map_model(&model, None, Some("unsupported")).unwrap();
        assert!(matches!(
            mapped.options.as_slice(),
            [OptionDescriptor::Select {
                default_value: None,
                ..
            }]
        ));
    }

    #[test]
    fn maps_recorded_rpc_fixture() {
        let mut mapper = PiMapper::new();
        let mut events = Vec::new();
        for line in include_str!("../tests/fixtures/pi/rpc_events.jsonl").lines() {
            let message: Value = serde_json::from_str(line).unwrap();
            events.extend(mapper.on_message(&message));
        }
        assert!(matches!(events[0], AgentEvent::TurnStarted { .. }));
        assert!(!events.iter().any(|event| matches!(
            event,
            AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::UserMessage { text, .. },
                ..
            }) if text == "DO NOT ECHO"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::Delta { kind: DeltaKind::AssistantText, text, .. } if text == "PONG"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::Delta { kind: DeltaKind::ReasoningText, text, .. } if text == "Checking"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ItemUpdated(ThreadItem { content: ItemContent::CommandExecution { output, .. }, .. }) if output == "accumulated\n"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ItemCompleted(ThreadItem { content: ItemContent::CommandExecution { command, output, .. }, .. })
                if command == "printf ok" && output == "accumulated\n"
        )));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    AgentEvent::ItemCompleted(ThreadItem {
                        content: ItemContent::AssistantMessage { text },
                        ..
                    }) if text == "PONG"
                ))
                .count(),
            1,
            "message_end and turn_end must reconcile the same assistant message"
        );
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::TokenUsage(TokenUsage {
                input_tokens: Some(12),
                output_tokens: Some(3),
                ..
            })
        )));
        assert!(matches!(
            events.last(),
            Some(AgentEvent::TurnCompleted {
                status: TurnStatus::Completed,
                ..
            })
        ));
    }

    #[test]
    fn multi_message_turn_sums_used_tokens() {
        let mut mapper = PiMapper::new();
        mapper.on_message(&json!({"type":"agent_start"}));
        mapper.on_message(&json!({
            "type":"message_end",
            "message":{"role":"assistant","responseId":"one","usage":{"input":10,"output":2,"totalTokens":12}}
        }));
        mapper.on_message(&json!({
            "type":"message_end",
            "message":{"role":"assistant","responseId":"two","usage":{"input":20,"output":3,"totalTokens":23}}
        }));
        let completed = mapper.on_message(&json!({"type":"agent_settled"}));
        assert!(matches!(
            completed.as_slice(),
            [AgentEvent::TurnCompleted {
                usage: Some(TokenUsage {
                    used_tokens: Some(35),
                    total_processed_tokens: Some(35),
                    ..
                }),
                ..
            }]
        ));
    }

    #[test]
    fn displayed_custom_messages_become_work_log_items() {
        let mut mapper = PiMapper::new();
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
            })] if provider_kind == "pi-extension" && summary == "first\nsecond"
        ));
    }

    #[test]
    fn hidden_or_empty_custom_messages_are_ignored() {
        let mut mapper = PiMapper::new();
        for message in [
            json!({"role":"custom","content":"hidden","display":false}),
            json!({"role":"custom","content":"implicit hidden"}),
            json!({"role":"custom","content":[{"type":"image","data":"ignored"}],"display":true}),
        ] {
            assert!(
                mapper
                    .on_message(&json!({"type":"message_end","message":message}))
                    .is_empty()
            );
        }
    }

    #[test]
    fn lf_reader_preserves_unicode_line_separators() {
        let bytes = b"{\"text\":\"a\xE2\x80\xA8b\"}\n";
        let mut lines = BufReader::new(bytes.as_slice()).lines();
        let record = lines.next().unwrap().unwrap();
        assert!(record.contains('\u{2028}'));
        assert!(lines.next().is_none());
    }
}
