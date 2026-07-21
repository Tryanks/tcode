//! Native pi provider: persistent `pi --mode rpc` JSONL transport.
//!
//! RPC records are framed by LF only. A bundled extension supplies the
//! permission boundary that pi intentionally leaves to hosts and translates
//! its confirmation UI into tcode's canonical approval events.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};

use async_channel::{Receiver, Sender};
use futures_lite::future;
use serde_json::{Value, json};

use crate::{
    AgentError, AgentEvent, ApprovalDecision, ApprovalKind, ApprovalMode, ApprovalRequest,
    Attachment, DeltaKind, FileChange, FileChangeKind, InteractionMode, ItemContent, ItemStatus,
    LaunchEnv, ModelSpec, OptionDescriptor, OptionSelection, ProviderCommand, ProviderCommandKind,
    ProviderKind, ResumeCursor, SelectOption, SessionCommand, SessionHandle, SessionOptions,
    ThreadItem, TokenUsage, TurnStatus,
};

const PERMISSION_EXTENSION: &str = include_str!("../assets/pi/tcode-permissions.ts");
const STDERR_TAIL_LINES: usize = 20;
const SETTLED_MIN_VERSION: (u32, u32, u32) = (0, 80, 4);

pub async fn start(opts: SessionOptions) -> Result<SessionHandle, AgentError> {
    if opts.fork {
        return Err(AgentError::Protocol(
            "session fork is not supported for this provider".into(),
        ));
    }
    let (commands_tx, commands_rx) = async_channel::unbounded();
    let (events_tx, events_rx) = async_channel::unbounded();
    let (ready_tx, ready_rx) = async_channel::bounded(1);
    smol::spawn(run_actor(opts, commands_rx, events_tx, ready_tx)).detach();
    ready_rx.recv().await.map_err(|_| {
        AgentError::Protocol("pi actor exited before reporting startup status".into())
    })??;
    Ok(SessionHandle {
        provider: ProviderKind::Pi,
        commands: commands_tx,
        events: events_rx,
    })
}

pub async fn list_models(
    binary_path: Option<PathBuf>,
    launch_env: LaunchEnv,
) -> Result<Vec<ModelSpec>, AgentError> {
    let (sender, receiver) = async_channel::bounded(1);
    std::thread::Builder::new()
        .name("pi-model-discovery".into())
        .spawn(move || {
            let result = list_models_blocking(binary_path.as_deref(), &launch_env);
            let _ = sender.send_blocking(result);
        })
        .map_err(|err| AgentError::Spawn(format!("spawning pi model discovery: {err}")))?;
    receiver.recv().await.map_err(|_| {
        AgentError::Protocol("pi model discovery worker exited without a result".into())
    })?
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
    let mut catalog = None;
    let mut reader = BufReader::new(stdout);
    while current.is_none() || catalog.is_none() {
        let line = read_lf_record(&mut reader)?.ok_or_else(|| {
            AgentError::Protocol("pi closed stdout during model discovery".into())
        })?;
        let message: Value = serde_json::from_str(&line)
            .map_err(|err| AgentError::Protocol(format!("invalid pi model response: {err}")))?;
        match message.get("id").and_then(Value::as_str) {
            Some("state") => {
                ensure_success(&message)?;
                current = Some(model_wire_id(message.pointer("/data/model")));
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
        .filter_map(|model| map_model(model, current.as_deref()))
        .collect())
}

fn map_model(model: &Value, current: Option<&str>) -> Option<ModelSpec> {
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
        options.push(OptionDescriptor::Select {
            id: "reasoningEffort".into(),
            label: "Thinking".into(),
            options: levels
                .into_iter()
                .map(|level| SelectOption {
                    value: level.into(),
                    label: thinking_label(level).into(),
                    description: None,
                })
                .collect(),
            default_value: None,
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

fn thinking_label(level: &str) -> &'static str {
    match level {
        "off" => "Off",
        "minimal" => "Minimal",
        "low" => "Low",
        "medium" => "Medium",
        "high" => "High",
        "xhigh" => "Extra High",
        "max" => "Max",
        _ => "Custom",
    }
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
    let extension = match materialize_permission_extension() {
        Ok(path) => path,
        Err(err) => {
            let _ = ready.send(Err(err)).await;
            return;
        }
    };
    let supports_settled =
        pi_version(&binary, &opts.launch_env).is_some_and(|version| version >= SETTLED_MIN_VERSION);
    let mut cmd = crate::process::command(&binary);
    // Profile arguments are applied first. The transport, permission
    // extension, resume target, and read-only tool set are tcode-owned and go
    // last so a profile cannot accidentally override the safety boundary.
    cmd.args(&opts.extra_args)
        .arg("--mode")
        .arg("rpc")
        .arg("--extension")
        .arg(extension);
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
    cmd.env(
        "TCODE_PI_APPROVAL_MODE",
        pi_approval_mode(opts.approval_mode),
    )
    .env("TCODE_PI_CWD", &opts.cwd);
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
    let (line_tx, line_rx) = async_channel::unbounded();
    let _ = std::thread::Builder::new()
        .name("pi-rpc-stdout".into())
        .spawn(move || read_pi_stdout(stdout, line_tx));
    let stderr_tail = spawn_stderr_reader(stderr, "pi-rpc-stderr");

    let mut actor = PiActor {
        child,
        stdin,
        lines: line_rx,
        events,
        mapper: PiMapper::new(supports_settled),
        next_request: 1,
        approval_mode: opts.approval_mode,
        pending_approvals: HashMap::new(),
        approved_for_session: HashSet::new(),
        pending_steers: HashMap::new(),
        requested_model: opts.model.clone(),
        stderr_tail,
    };
    let startup = actor.initialize().await;
    if let Err(err) = startup {
        actor.stop();
        let details = actor.describe_failure(err.to_string());
        let _ = ready.send(Err(AgentError::Provider(details))).await;
        return;
    }
    if opts.interaction_mode == InteractionMode::Plan {
        actor
            .emit(AgentEvent::Warning {
                message:
                "pi RPC has no native Plan interaction mode; this session is running in Build mode"
                    .into(),
             })
            .await;
    }
    if opts.mcp_server.is_some()
        || opts.orchestrate_server.is_some()
        || opts.computer_use_server.is_some()
    {
        actor
            .emit(AgentEvent::Warning {
                message:
                "pi RPC does not expose native MCP registration; configured tcode MCP servers were not attached"
                    .into(),
             })
            .await;
    }
    if ready.send(Ok(())).await.is_err() {
        actor.stop();
        return;
    }

    let close_reason = loop {
        enum Input {
            Command(Result<SessionCommand, async_channel::RecvError>),
            Output(Result<ChildOutput, async_channel::RecvError>),
        }
        let input = future::race(async { Input::Command(commands.recv().await) }, async {
            Input::Output(actor.lines.recv().await)
        })
        .await;
        match input {
            Input::Command(Ok(SessionCommand::Shutdown)) | Input::Command(Err(_)) => break None,
            Input::Command(Ok(command)) => {
                if let Err(err) = actor.handle_command(command).await {
                    actor
                        .emit(AgentEvent::Error {
                            message: err,
                            fatal: true,
                        })
                        .await;
                    break Some("pi protocol write failed".into());
                }
            }
            Input::Output(Ok(ChildOutput::Line(line))) => actor.handle_line(&line).await,
            Input::Output(Ok(ChildOutput::Error(err))) => {
                actor
                    .emit(AgentEvent::Error {
                        message: err.clone(),
                        fatal: true,
                    })
                    .await;
                break Some(err);
            }
            Input::Output(Ok(ChildOutput::Eof)) | Input::Output(Err(_)) => {
                break Some("pi RPC process closed stdout".into());
            }
        }
    };
    actor.stop();
    let reason = close_reason.map(|base| actor.describe_failure(base));
    let _ = actor
        .events
        .send(AgentEvent::SessionClosed { reason })
        .await;
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
    approved_for_session: HashSet<String>,
    /// Native prompt request id -> canonical steering request id. pi's prompt
    /// response is only an acceptance signal, but it is stronger than merely
    /// acknowledging the stdin write.
    pending_steers: HashMap<String, String>,
    requested_model: Option<String>,
    stderr_tail: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
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
        self.emit(AgentEvent::SessionStarted {
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
        self.emit(AgentEvent::ProviderCommands { commands }).await;
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
                self.emit(AgentEvent::Warning {
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
                    self.emit(AgentEvent::SteerAccepted { request_id }).await;
                }
            } else {
                self.emit(AgentEvent::Error {
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
        for event in self.mapper.on_message(&message) {
            self.emit(event).await;
        }
    }

    async fn handle_extension_ui(&mut self, message: &Value) {
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        if method != "confirm" {
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
            self.emit(AgentEvent::Warning {
                message: format!("pi extension requested unsupported UI method `{method}`"),
            })
            .await;
            return;
        }
        let Some(id) = message.get("id").and_then(Value::as_str) else {
            self.emit(AgentEvent::Warning {
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
        self.emit(AgentEvent::ApprovalRequested(ApprovalRequest {
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
                self.emit(AgentEvent::TurnAccepted { delivery_id }).await;
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
                    send_json(&mut self.stdin, &json!({"type":"abort"}))
                        .map_err(|err| err.to_string())?;
                }
                self.emit(AgentEvent::ApprovalResolved {
                    request_id,
                    decision,
                })
                .await;
                Ok(())
            }
            SessionCommand::SetOption { id, value }
                if matches!(id.as_str(), "reasoningEffort" | "thinkingLevel") =>
            {
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
                    self.emit(AgentEvent::Warning {
                        message: "pi permission changes require restarting the session".into(),
                    })
                    .await;
                }
                Ok(())
            }
            SessionCommand::SetInteractionMode(mode) => {
                if mode == InteractionMode::Plan {
                    self.emit(AgentEvent::Warning {
                        message: "pi RPC has no native Plan interaction mode".into(),
                    })
                    .await;
                }
                Ok(())
            }
            SessionCommand::RespondUserInput { .. } => {
                self.emit(AgentEvent::Warning {
                    message: "pi RPC does not expose structured user-input requests".into(),
                })
                .await;
                Ok(())
            }
            SessionCommand::Rewind { .. } => {
                self.emit(AgentEvent::Warning {
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

    async fn emit(&self, event: AgentEvent) {
        let _ = self.events.send(event).await;
    }

    fn stop(&mut self) {
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn describe_failure(&self, base: String) -> String {
        let tail = self.stderr_tail.lock().unwrap().join("\n");
        if tail.trim().is_empty() {
            base
        } else {
            format!("{base}\nstderr:\n{tail}")
        }
    }
}

pub(crate) struct PiMapper {
    supports_settled: bool,
    turn_counter: u64,
    current_turn: Option<String>,
    turn_usage: TokenUsage,
    has_usage: bool,
    cumulative_processed: u64,
    interrupt_pending: bool,
    failed: bool,
    tool_items: HashMap<String, PiTool>,
    usage_messages: HashSet<String>,
    finalized_messages: HashSet<String>,
}

#[derive(Clone)]
struct PiTool {
    name: String,
    input: Value,
    output: String,
}

impl PiMapper {
    pub(crate) fn new(supports_settled: bool) -> Self {
        Self {
            supports_settled,
            turn_counter: 0,
            current_turn: None,
            turn_usage: TokenUsage::default(),
            has_usage: false,
            cumulative_processed: 0,
            interrupt_pending: false,
            failed: false,
            tool_items: HashMap::new(),
            usage_messages: HashSet::new(),
            finalized_messages: HashSet::new(),
        }
    }

    pub(crate) fn on_message(&mut self, message: &Value) -> Vec<AgentEvent> {
        match message.get("type").and_then(Value::as_str).unwrap_or("") {
            "agent_start" => self.start_turn(),
            "agent_settled" => self.complete_turn(),
            "agent_end"
                if !self.supports_settled
                    && !message
                        .get("willRetry")
                        .and_then(Value::as_bool)
                        .unwrap_or(false) =>
            {
                self.complete_turn()
            }
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
                    number(message.get("attempt")).unwrap_or(0),
                    number(message.get("maxAttempts")).unwrap_or(0),
                    number(message.get("delayMs")).unwrap_or(0),
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
        if self.current_turn.is_some() {
            return Vec::new();
        }
        self.turn_counter += 1;
        let turn_id = format!("pi-turn-{}", self.turn_counter);
        self.current_turn = Some(turn_id.clone());
        self.turn_usage = TokenUsage::default();
        self.has_usage = false;
        self.failed = false;
        vec![AgentEvent::TurnStarted { turn_id }]
    }

    fn complete_turn(&mut self) -> Vec<AgentEvent> {
        let Some(turn_id) = self.current_turn.take() else {
            return Vec::new();
        };
        let status = if self.interrupt_pending {
            TurnStatus::Interrupted
        } else if self.failed {
            TurnStatus::Failed
        } else {
            TurnStatus::Completed
        };
        self.interrupt_pending = false;
        vec![AgentEvent::TurnCompleted {
            turn_id,
            status,
            usage: self.has_usage.then_some(self.turn_usage),
        }]
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
        let index = number(event.get("contentIndex")).unwrap_or(0);
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
                if self.usage_messages.insert(id)
                    && let Some(usage) = map_usage(message.get("usage"))
                {
                    let processed = usage.used_tokens.unwrap_or_else(|| {
                        usage
                            .input_tokens
                            .unwrap_or(0)
                            .saturating_add(usage.output_tokens.unwrap_or(0))
                            .saturating_add(usage.cached_input_tokens.unwrap_or(0))
                    });
                    self.cumulative_processed = self.cumulative_processed.saturating_add(processed);
                    let usage = TokenUsage {
                        total_processed_tokens: Some(self.cumulative_processed),
                        ..usage
                    };
                    merge_usage(&mut self.turn_usage, usage);
                    self.has_usage = true;
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
    ThreadItem {
        id: id.to_owned(),
        parent_item_id: None,
        content,
    }
}

fn approval_kind(tool_name: &str, payload: &Value) -> ApprovalKind {
    let input = payload.get("input").cloned().unwrap_or(Value::Null);
    let reason = payload
        .get("reason")
        .and_then(Value::as_str)
        .map(str::to_owned);
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
                number(message.get("timestamp")).unwrap_or(0)
            )
        })
}

fn map_usage(usage: Option<&Value>) -> Option<TokenUsage> {
    let usage = usage?;
    let input = number(usage.get("input"));
    let output = number(usage.get("output"));
    let cache_read = number(usage.get("cacheRead"));
    (input.is_some() || output.is_some() || cache_read.is_some()).then_some(TokenUsage {
        input_tokens: input,
        cached_input_tokens: cache_read,
        output_tokens: output,
        used_tokens: number(usage.get("totalTokens")),
        context_window: None,
        total_processed_tokens: None,
    })
}

fn merge_usage(total: &mut TokenUsage, usage: TokenUsage) {
    total.input_tokens = add_options(total.input_tokens, usage.input_tokens);
    total.cached_input_tokens = add_options(total.cached_input_tokens, usage.cached_input_tokens);
    total.output_tokens = add_options(total.output_tokens, usage.output_tokens);
    total.used_tokens = add_options(total.used_tokens, usage.used_tokens);
    total.context_window = usage.context_window.or(total.context_window);
    total.total_processed_tokens = usage.total_processed_tokens;
}

fn add_options(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (None, None) => None,
        (left, right) => Some(left.unwrap_or(0).saturating_add(right.unwrap_or(0))),
    }
}

fn number(value: Option<&Value>) -> Option<u64> {
    value.and_then(|value| {
        value
            .as_u64()
            .or_else(|| value.as_f64().map(|number| number.max(0.0) as u64))
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
        .find(|selection| matches!(selection.id.as_str(), "reasoningEffort" | "thinkingLevel"))
        .and_then(|selection| selection.value.as_str())
}

fn resume_session(resume: &Option<ResumeCursor>) -> Option<&str> {
    let cursor = resume.as_ref()?;
    cursor
        .0
        .get("session_file")
        .or_else(|| cursor.0.get("session_id"))
        .and_then(Value::as_str)
}

fn pi_approval_mode(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::Supervised => "supervised",
        ApprovalMode::ReadOnly => "read_only",
        ApprovalMode::AutoAcceptEdits => "auto_accept_edits",
        ApprovalMode::FullAccess => "full_access",
    }
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
    let current = std::fs::read_to_string(&path).ok();
    if current.as_deref() != Some(PERMISSION_EXTENSION) {
        std::fs::write(&path, PERMISSION_EXTENSION)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(path)
}

fn pi_version(binary: &Path, launch_env: &LaunchEnv) -> Option<(u32, u32, u32)> {
    let mut command = crate::process::command(binary);
    command.arg("--version");
    for (key, value) in launch_env.pairs(ProviderKind::Pi) {
        command.env(key, value);
    }
    let output = command.output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    parse_version(&text)
}

fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let token = text.split_whitespace().find(|token| token.contains('.'))?;
    let mut parts = token.trim_start_matches('v').split('.');
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts
            .next()
            .and_then(|part| part.split(['-', '+']).next())?
            .parse()
            .ok()?,
    ))
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

fn send_json(writer: &mut BufWriter<ChildStdin>, value: &Value) -> Result<(), AgentError> {
    serde_json::to_writer(&mut *writer, value)
        .map_err(|err| AgentError::Protocol(format!("serializing pi request: {err}")))?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

enum ChildOutput {
    Line(String),
    Eof,
    Error(String),
}

fn read_pi_stdout(stdout: impl std::io::Read, sender: Sender<ChildOutput>) {
    let mut reader = BufReader::new(stdout);
    loop {
        match read_lf_record(&mut reader) {
            Ok(Some(line)) => {
                if !line.trim().is_empty() && sender.send_blocking(ChildOutput::Line(line)).is_err()
                {
                    return;
                }
            }
            Ok(None) => {
                let _ = sender.send_blocking(ChildOutput::Eof);
                return;
            }
            Err(err) => {
                let _ = sender.send_blocking(ChildOutput::Error(err.to_string()));
                return;
            }
        }
    }
}

fn read_lf_record(reader: &mut impl BufRead) -> Result<Option<String>, AgentError> {
    let mut bytes = Vec::new();
    let count = reader.read_until(b'\n', &mut bytes)?;
    if count == 0 {
        return Ok(None);
    }
    if bytes.last() != Some(&b'\n') {
        return Err(AgentError::Protocol(
            "pi stdout ended with an unterminated JSONL record".into(),
        ));
    }
    bytes.pop();
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|err| AgentError::Protocol(format!("pi stdout was not UTF-8: {err}")))
}

fn spawn_stderr_reader(
    stderr: impl std::io::Read + Send + 'static,
    name: &str,
) -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
    let tail: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
    let capture = tail.clone();
    let _ = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                log::debug!("pi: {line}");
                let mut tail = capture.lock().unwrap();
                if tail.len() == STDERR_TAIL_LINES {
                    tail.remove(0);
                }
                tail.push(line);
            }
        });
    tail
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_recorded_rpc_fixture() {
        let mut mapper = PiMapper::new(true);
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
        let mut mapper = PiMapper::new(true);
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
    fn legacy_pi_uses_agent_end_fallback() {
        let mut mapper = PiMapper::new(false);
        mapper.on_message(&json!({"type":"agent_start"}));
        assert!(matches!(
            mapper
                .on_message(&json!({"type":"agent_end","willRetry":false}))
                .as_slice(),
            [AgentEvent::TurnCompleted { .. }]
        ));
    }

    #[test]
    fn lf_reader_preserves_unicode_line_separators() {
        let bytes = b"{\"text\":\"a\xE2\x80\xA8b\"}\n";
        let mut reader = BufReader::new(bytes.as_slice());
        let record = read_lf_record(&mut reader).unwrap().unwrap();
        assert!(record.contains('\u{2028}'));
        assert!(read_lf_record(&mut reader).unwrap().is_none());
    }
}
