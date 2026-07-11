//! Codex provider: a small client for the newline-delimited JSON protocol used
//! by `codex app-server`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::process::{Child, ChildStdin, Command, Stdio};

use async_channel::{Receiver, Sender};
use futures_lite::future;
use serde_json::{Value, json};

use crate::{
    AgentError, AgentEvent, ApprovalDecision, ApprovalKind, ApprovalMode, ApprovalRequest,
    DeltaKind, FileChange, FileChangeKind, ItemContent, ItemStatus, ProviderKind, ResumeCursor,
    SessionCommand, SessionHandle, SessionOptions, ThreadItem, TokenUsage, TurnStatus,
};

/// Map a canonical [`ApprovalMode`] onto Codex's `approvalPolicy` × `sandbox`
/// pair for `thread/start` (and `thread/resume`).
///
/// The wire strings are the kebab-case `AskForApproval` / `SandboxMode`
/// variants from codex `app-server-protocol` v2 (`shared.rs`): approval
/// `untrusted` / `on-request` / `never`, sandbox `read-only` /
/// `workspace-write` / `danger-full-access`. The three-mode assignment mirrors
/// T3's `CodexSessionRuntime.runtimeModeToThreadConfig`:
/// - Supervised (approval-required): everything outside a read-only sandbox is
///   confirmed → asks before commands and file changes.
/// - AutoAcceptEdits: edits inside the workspace-write sandbox proceed;
///   escalations (e.g. commands needing more access) still request approval.
/// - FullAccess: no prompts, unsandboxed.
fn approval_knobs(mode: ApprovalMode) -> (&'static str, &'static str) {
    match mode {
        ApprovalMode::Supervised => ("untrusted", "read-only"),
        ApprovalMode::AutoAcceptEdits => ("on-request", "workspace-write"),
        ApprovalMode::FullAccess => ("never", "danger-full-access"),
    }
}

/// Starts an app-server process and waits until its thread is ready.
pub async fn start(opts: SessionOptions) -> Result<SessionHandle, AgentError> {
    let (commands_tx, commands_rx) = async_channel::unbounded();
    let (events_tx, events_rx) = async_channel::unbounded();
    let (ready_tx, ready_rx) = async_channel::bounded(1);

    smol::spawn(run_actor(opts, commands_rx, events_tx, ready_tx)).detach();
    ready_rx.recv().await.map_err(|_| {
        AgentError::Protocol("codex actor exited before reporting startup status".into())
    })??;

    Ok(SessionHandle {
        provider: ProviderKind::Codex,
        commands: commands_tx,
        events: events_rx,
    })
}

enum ChildOutput {
    Line(String),
    Eof,
    Error(String),
}

#[derive(Clone, Copy)]
enum PendingRequest {
    TurnStart,
    Interrupt,
}

struct Actor {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    lines: Receiver<ChildOutput>,
    events: Sender<AgentEvent>,
    thread_id: String,
    next_id: i64,
    pending_requests: HashMap<i64, PendingRequest>,
    approvals: HashMap<String, Value>,
    items: HashMap<String, ThreadItem>,
    usage_by_turn: HashMap<String, TokenUsage>,
    active_turn: Option<String>,
}

async fn run_actor(
    opts: SessionOptions,
    commands: Receiver<SessionCommand>,
    events: Sender<AgentEvent>,
    ready: Sender<Result<(), AgentError>>,
) {
    let (mut child, mut stdin, lines) = match spawn_server(&opts) {
        Ok(parts) => parts,
        Err(err) => {
            let _ = ready.send(Err(err)).await;
            return;
        }
    };

    let startup = initialize_and_open_thread(&opts, &mut stdin, &lines).await;
    let (thread_id, model, next_id) = match startup {
        Ok(value) => value,
        Err(err) => {
            let _ = ready.send(Err(err)).await;
            stop_child(&mut child, stdin);
            return;
        }
    };

    let mut actor = Actor {
        child,
        stdin,
        lines,
        events,
        thread_id: thread_id.clone(),
        next_id,
        pending_requests: HashMap::new(),
        approvals: HashMap::new(),
        items: HashMap::new(),
        usage_by_turn: HashMap::new(),
        active_turn: None,
    };

    let started = AgentEvent::SessionStarted {
        provider_session_id: thread_id.clone(),
        resume: ResumeCursor(json!({ "thread_id": thread_id })),
        model,
    };
    if actor.events.send(started).await.is_err() {
        let _ = ready
            .send(Err(AgentError::Protocol("event channel closed".into())))
            .await;
        stop_child(&mut actor.child, actor.stdin);
        return;
    }
    if ready.send(Ok(())).await.is_err() {
        stop_child(&mut actor.child, actor.stdin);
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
                    break Some("protocol write failed".into());
                }
            }
            Input::Output(Ok(ChildOutput::Line(line))) => actor.handle_line(&line).await,
            Input::Output(Ok(ChildOutput::Eof)) | Input::Output(Err(_)) => {
                let status = actor.child.try_wait().ok().flatten();
                break Some(match status {
                    Some(status) => format!("codex app-server exited with {status}"),
                    None => "codex app-server closed stdout".into(),
                });
            }
            Input::Output(Ok(ChildOutput::Error(err))) => {
                actor
                    .emit(AgentEvent::Error {
                        message: err.clone(),
                        fatal: true,
                    })
                    .await;
                break Some(err);
            }
        }
    };

    stop_child(&mut actor.child, actor.stdin);
    actor
        .events
        .send(AgentEvent::SessionClosed {
            reason: close_reason,
        })
        .await
        .ok();
}

fn spawn_server(
    opts: &SessionOptions,
) -> Result<(Child, BufWriter<ChildStdin>, Receiver<ChildOutput>), AgentError> {
    let binary = opts
        .binary_path
        .as_deref()
        .unwrap_or_else(|| std::path::Path::new("codex"));
    let mut child = Command::new(binary)
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| AgentError::Spawn(err.to_string()))?;
    let stdin = BufWriter::new(
        child
            .stdin
            .take()
            .ok_or_else(|| AgentError::Spawn("missing child stdin".into()))?,
    );
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AgentError::Spawn("missing child stdout".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AgentError::Spawn("missing child stderr".into()))?;
    let (tx, rx) = async_channel::unbounded();

    std::thread::Builder::new()
        .name("codex-app-server-stdout".into())
        .spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        if tx.send_blocking(ChildOutput::Line(line)).is_err() {
                            return;
                        }
                    }
                    Err(err) => {
                        let _ = tx.send_blocking(ChildOutput::Error(format!(
                            "failed reading codex stdout: {err}"
                        )));
                        return;
                    }
                }
            }
            let _ = tx.send_blocking(ChildOutput::Eof);
        })
        .map_err(|err| AgentError::Spawn(err.to_string()))?;
    std::thread::Builder::new()
        .name("codex-app-server-stderr".into())
        .spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                log::debug!("codex app-server: {line}");
            }
        })
        .map_err(|err| AgentError::Spawn(err.to_string()))?;
    Ok((child, stdin, rx))
}

async fn initialize_and_open_thread(
    opts: &SessionOptions,
    stdin: &mut BufWriter<ChildStdin>,
    lines: &Receiver<ChildOutput>,
) -> Result<(String, Option<String>, i64), AgentError> {
    send_json(
        stdin,
        &json!({
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": { "name": "tcode", "title": "tcode", "version": env!("CARGO_PKG_VERSION") },
                "capabilities": null
            }
        }),
    )?;
    wait_for_response(lines, 1).await?;
    send_json(stdin, &json!({ "method": "initialized" }))?;

    let cwd = opts.cwd.to_string_lossy();
    let (approval_policy, sandbox) = approval_knobs(opts.approval_mode);
    let (method, mut params) = if let Some(resume) = &opts.resume {
        let thread_id = resume
            .0
            .get("thread_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                AgentError::Protocol(
                    "Codex resume cursor is missing string field `thread_id`".into(),
                )
            })?;
        (
            "thread/resume",
            json!({
                "threadId": thread_id,
                "cwd": cwd,
                "approvalPolicy": approval_policy,
                "sandbox": sandbox
            }),
        )
    } else {
        (
            "thread/start",
            json!({
                "cwd": cwd,
                "approvalPolicy": approval_policy,
                "sandbox": sandbox
            }),
        )
    };
    if let Some(model) = &opts.model {
        params["model"] = json!(model);
    }
    send_json(
        stdin,
        &json!({ "id": 2, "method": method, "params": params }),
    )?;
    let result = wait_for_response(lines, 2).await?;
    let thread_id = result
        .pointer("/thread/id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            AgentError::Protocol(format!("{method} response omitted thread.id: {result}"))
        })?
        .to_owned();
    let model = result
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| opts.model.clone());
    Ok((thread_id, model, 3))
}

async fn wait_for_response(lines: &Receiver<ChildOutput>, id: i64) -> Result<Value, AgentError> {
    loop {
        match lines
            .recv()
            .await
            .map_err(|_| AgentError::Protocol("codex stdout closed during startup".into()))?
        {
            ChildOutput::Line(line) => {
                let value: Value = serde_json::from_str(&line).map_err(|err| {
                    AgentError::Protocol(format!("invalid JSON from codex: {err}: {line}"))
                })?;
                if value.get("id").and_then(Value::as_i64) != Some(id) {
                    continue;
                }
                if let Some(error) = value.get("error") {
                    return Err(AgentError::Provider(
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown JSON-RPC error")
                            .into(),
                    ));
                }
                return value
                    .get("result")
                    .cloned()
                    .ok_or_else(|| AgentError::Protocol(format!("response {id} omitted result")));
            }
            ChildOutput::Eof => {
                return Err(AgentError::Protocol("codex exited during startup".into()));
            }
            ChildOutput::Error(err) => return Err(AgentError::Protocol(err)),
        }
    }
}

fn send_json(stdin: &mut BufWriter<ChildStdin>, value: &Value) -> Result<(), AgentError> {
    serde_json::to_writer(&mut *stdin, value)
        .map_err(|err| AgentError::Protocol(err.to_string()))?;
    stdin.write_all(b"\n")?;
    stdin.flush()?;
    Ok(())
}

fn stop_child(child: &mut Child, stdin: BufWriter<ChildStdin>) {
    drop(stdin);
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
}

impl Actor {
    async fn emit(&self, event: AgentEvent) {
        self.events.send(event).await.ok();
    }

    fn request(&mut self, method: &str, params: Value, kind: PendingRequest) -> Result<(), String> {
        let id = self.next_id;
        self.next_id += 1;
        send_json(
            &mut self.stdin,
            &json!({ "id": id, "method": method, "params": params }),
        )
        .map_err(|e| e.to_string())?;
        self.pending_requests.insert(id, kind);
        Ok(())
    }

    async fn handle_command(&mut self, command: SessionCommand) -> Result<(), String> {
        match command {
            SessionCommand::SendTurn { text } => {
                let thread_id = self.thread_id.clone();
                self.request(
                    "turn/start",
                    json!({
                        "threadId": thread_id,
                        "input": [{ "type": "text", "text": text, "text_elements": [] }]
                    }),
                    PendingRequest::TurnStart,
                )
            }
            SessionCommand::Interrupt => {
                let Some(turn_id) = self.active_turn.clone() else {
                    self.emit(AgentEvent::Warning(
                        "cannot interrupt: no active Codex turn".into(),
                    ))
                    .await;
                    return Ok(());
                };
                let thread_id = self.thread_id.clone();
                self.request(
                    "turn/interrupt",
                    json!({ "threadId": thread_id, "turnId": turn_id }),
                    PendingRequest::Interrupt,
                )
            }
            SessionCommand::RespondApproval {
                request_id,
                decision,
            } => {
                let Some(json_rpc_id) = self.approvals.remove(&request_id) else {
                    self.emit(AgentEvent::Warning(format!(
                        "unknown Codex approval request id: {request_id}"
                    )))
                    .await;
                    return Ok(());
                };
                let wire_decision = match decision {
                    ApprovalDecision::Approve => "accept",
                    ApprovalDecision::ApproveForSession => "acceptForSession",
                    ApprovalDecision::Deny => "decline",
                };
                send_json(
                    &mut self.stdin,
                    &json!({ "id": json_rpc_id, "result": { "decision": wire_decision } }),
                )
                .map_err(|e| e.to_string())?;
                self.emit(AgentEvent::ApprovalResolved {
                    request_id,
                    decision,
                })
                .await;
                Ok(())
            }
            SessionCommand::SetApprovalMode(mode) => {
                // The app-server binds approvalPolicy × sandbox at thread
                // start/resume; there is no thread-level permissions-update
                // request. Signal the UI to fall back to a resume-restart (the
                // fresh thread/resume carries the new mode), mirroring the
                // model-switch path.
                self.emit(AgentEvent::Warning(format!(
                    "codex: applying approval mode {mode:?} requires a session restart"
                )))
                .await;
                Ok(())
            }
            SessionCommand::Shutdown => Ok(()),
        }
    }

    async fn handle_line(&mut self, line: &str) {
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(err) => {
                self.emit(AgentEvent::Error {
                    message: format!("invalid JSON from codex: {err}: {line}"),
                    fatal: false,
                })
                .await;
                return;
            }
        };
        if value.get("method").is_some() && value.get("id").is_some() {
            self.handle_server_request(&value).await;
        } else if let Some(method) = value.get("method").and_then(Value::as_str) {
            self.handle_notification(method, value.get("params").unwrap_or(&Value::Null))
                .await;
        } else if let Some(id) = value.get("id").and_then(Value::as_i64) {
            if let Some(error) = value.get("error") {
                let method = self.pending_requests.remove(&id);
                self.emit(AgentEvent::Error {
                    message: format!(
                        "Codex request {} failed: {}",
                        pending_name(method),
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown error")
                    ),
                    fatal: false,
                })
                .await;
            } else if matches!(
                self.pending_requests.remove(&id),
                Some(PendingRequest::TurnStart)
            ) {
                if let Some(turn_id) = value.pointer("/result/turn/id").and_then(Value::as_str) {
                    self.active_turn.get_or_insert_with(|| turn_id.to_owned());
                }
            }
        }
    }

    async fn handle_server_request(&mut self, value: &Value) {
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let id = value.get("id").cloned().unwrap_or(Value::Null);
        let key = request_id_string(&id);
        let params = value.get("params").unwrap_or(&Value::Null);
        let turn_id = params
            .get("turnId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let item_id = params
            .get("itemId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let kind = match method {
            "item/commandExecution/requestApproval" | "execCommandApproval" => {
                ApprovalKind::ExecCommand {
                    command: params
                        .get("command")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                    cwd: params.get("cwd").and_then(Value::as_str).map(str::to_owned),
                    reason: params
                        .get("reason")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                }
            }
            "item/fileChange/requestApproval" | "applyPatchApproval" => ApprovalKind::FileChange {
                changes: self
                    .items
                    .get(item_id)
                    .and_then(|item| match &item.content {
                        ItemContent::FileChange { changes, .. } => Some(changes.clone()),
                        _ => None,
                    })
                    .unwrap_or_default(),
                reason: params
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            },
            _ => {
                let _ = send_json(
                    &mut self.stdin,
                    &json!({ "id": id, "error": { "code": -32601, "message": format!("unsupported server request: {method}") } }),
                );
                self.emit(AgentEvent::Warning(format!(
                    "unsupported Codex server request: {method}"
                )))
                .await;
                return;
            }
        };
        self.approvals.insert(key.clone(), id);
        self.emit(AgentEvent::ApprovalRequested(ApprovalRequest {
            id: key,
            turn_id,
            kind,
        }))
        .await;
    }

    async fn handle_notification(&mut self, method: &str, params: &Value) {
        match method {
            "turn/started" => {
                if let Some(id) = params.pointer("/turn/id").and_then(Value::as_str) {
                    self.active_turn = Some(id.into());
                    self.emit(AgentEvent::TurnStarted { turn_id: id.into() })
                        .await;
                }
            }
            "turn/completed" => {
                let turn = params.get("turn").unwrap_or(&Value::Null);
                let id = turn
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let status = match turn.get("status").and_then(Value::as_str) {
                    Some("interrupted") => TurnStatus::Interrupted,
                    Some("failed") => TurnStatus::Failed,
                    _ => TurnStatus::Completed,
                };
                self.active_turn = None;
                let usage = self.usage_by_turn.remove(&id);
                self.emit(AgentEvent::TurnCompleted {
                    turn_id: id,
                    status,
                    usage,
                })
                .await;
            }
            "item/started" | "item/updated" | "item/completed" => {
                if let Some(item) = params.get("item").and_then(map_item) {
                    self.items.insert(item.id.clone(), item.clone());
                    let event = match method {
                        "item/started" => AgentEvent::ItemStarted(item),
                        "item/updated" => AgentEvent::ItemUpdated(item),
                        _ => AgentEvent::ItemCompleted(item),
                    };
                    self.emit(event).await;
                }
            }
            "item/agentMessage/delta" => self.emit_delta(params, DeltaKind::AssistantText).await,
            "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
                self.emit_delta(params, DeltaKind::ReasoningText).await
            }
            "item/commandExecution/outputDelta" | "command/exec/outputDelta" => {
                self.emit_delta(params, DeltaKind::CommandOutput).await
            }
            "thread/tokenUsage/updated" => {
                if let Some(usage) = map_usage(params.get("tokenUsage").unwrap_or(&Value::Null)) {
                    if let Some(turn_id) = params.get("turnId").and_then(Value::as_str) {
                        self.usage_by_turn.insert(turn_id.into(), usage);
                    }
                    self.emit(AgentEvent::TokenUsage(usage)).await;
                }
            }
            "error" => {
                let message = params
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("Codex reported an unknown error")
                    .to_owned();
                let fatal = !params
                    .get("willRetry")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                self.emit(AgentEvent::Error { message, fatal }).await;
            }
            "warning" | "configWarning" | "deprecationNotice" => {
                if let Some(message) = params.get("message").and_then(Value::as_str) {
                    self.emit(AgentEvent::Warning(message.into())).await;
                }
            }
            "thread/closed" => {
                self.emit(AgentEvent::Warning(
                    "Codex thread was closed by the server".into(),
                ))
                .await;
            }
            _ => log::trace!("ignored Codex notification {method}: {params}"),
        }
    }

    async fn emit_delta(&self, params: &Value, kind: DeltaKind) {
        if let (Some(item_id), Some(text)) = (
            params.get("itemId").and_then(Value::as_str),
            params.get("delta").and_then(Value::as_str),
        ) {
            self.emit(AgentEvent::Delta {
                item_id: item_id.into(),
                kind,
                text: text.into(),
            })
            .await;
        }
    }
}

fn pending_name(request: Option<PendingRequest>) -> &'static str {
    match request {
        Some(PendingRequest::TurnStart) => "turn/start",
        Some(PendingRequest::Interrupt) => "turn/interrupt",
        None => "unknown",
    }
}

fn request_id_string(id: &Value) -> String {
    id.as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| id.to_string())
}

fn map_item(item: &Value) -> Option<ThreadItem> {
    let id = item.get("id").and_then(Value::as_str)?.to_owned();
    let provider_kind = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    // The app synthesizes a canonical `UserMessage` event at send time (needed
    // for universal replay across providers). Codex echoes the same user input
    // back as a `userMessage` item; emitting it too would render a duplicate
    // user bubble, so swallow the provider echo here.
    if provider_kind == "userMessage" {
        log::debug!("suppressing provider-echoed userMessage item {id}");
        return None;
    }
    let content = match provider_kind {
        "agentMessage" => ItemContent::AssistantMessage {
            text: string_field(item, "text"),
        },
        "reasoning" => {
            let mut parts = strings(item.get("summary"));
            parts.extend(strings(item.get("content")));
            ItemContent::Reasoning {
                text: parts.join("\n"),
            }
        }
        "commandExecution" => ItemContent::CommandExecution {
            command: string_field(item, "command"),
            output: string_field(item, "aggregatedOutput"),
            exit_code: item
                .get("exitCode")
                .and_then(Value::as_i64)
                .and_then(|n| i32::try_from(n).ok()),
            status: map_status(item.get("status").and_then(Value::as_str)),
        },
        "fileChange" => ItemContent::FileChange {
            changes: item
                .get("changes")
                .and_then(Value::as_array)
                .map(|changes| changes.iter().filter_map(map_file_change).collect())
                .unwrap_or_default(),
            status: map_status(item.get("status").and_then(Value::as_str)),
        },
        "mcpToolCall" => ItemContent::ToolCall {
            name: format!(
                "{}/{}",
                string_field(item, "server"),
                string_field(item, "tool")
            ),
            input: item.get("arguments").cloned().unwrap_or(Value::Null),
            output: tool_output(item),
            status: map_status(item.get("status").and_then(Value::as_str)),
        },
        "dynamicToolCall" => ItemContent::ToolCall {
            name: string_field(item, "tool"),
            input: item.get("arguments").cloned().unwrap_or(Value::Null),
            output: item
                .get("contentItems")
                .filter(|v| !v.is_null())
                .map(Value::to_string),
            status: map_status(item.get("status").and_then(Value::as_str)),
        },
        "webSearch" => ItemContent::WebSearch {
            query: string_field(item, "query"),
        },
        _ => ItemContent::Other {
            provider_kind: provider_kind.into(),
            summary: serde_json::to_string(item).unwrap_or_else(|_| provider_kind.into()),
        },
    };
    Some(ThreadItem { id, content })
}

fn strings(value: Option<&Value>) -> Vec<&str> {
    value
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn string_field(value: &Value, field: &str) -> String {
    value
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn tool_output(item: &Value) -> Option<String> {
    if let Some(message) = item.pointer("/error/message").and_then(Value::as_str) {
        return Some(message.into());
    }
    item.get("result")
        .filter(|v| !v.is_null())
        .map(Value::to_string)
}

fn map_status(status: Option<&str>) -> ItemStatus {
    match status {
        Some("completed") => ItemStatus::Completed,
        Some("failed") => ItemStatus::Failed,
        Some("declined") => ItemStatus::Declined,
        _ => ItemStatus::InProgress,
    }
}

fn map_file_change(change: &Value) -> Option<FileChange> {
    let kind_value = change.get("kind").unwrap_or(&Value::Null);
    let kind_type = kind_value
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| kind_value.as_str())
        .unwrap_or("update");
    let kind = match kind_type {
        "add" | "create" => FileChangeKind::Create,
        "delete" => FileChangeKind::Delete,
        "update"
            if kind_value
                .get("move_path")
                .or_else(|| kind_value.get("movePath"))
                .is_some_and(|v| !v.is_null()) =>
        {
            FileChangeKind::Rename
        }
        _ => FileChangeKind::Modify,
    };
    Some(FileChange {
        path: change.get("path").and_then(Value::as_str)?.into(),
        kind,
        diff: change
            .get("diff")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn map_usage(value: &Value) -> Option<TokenUsage> {
    let last = value.get("last")?;
    Some(TokenUsage {
        input_tokens: last.get("inputTokens").and_then(Value::as_u64),
        cached_input_tokens: last.get("cachedInputTokens").and_then(Value::as_u64),
        output_tokens: last.get("outputTokens").and_then(Value::as_u64),
        used_tokens: last.get("totalTokens").and_then(Value::as_u64),
        context_window: value.get("modelContextWindow").and_then(Value::as_u64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_actor() -> (Actor, Receiver<AgentEvent>) {
        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = BufWriter::new(child.stdin.take().unwrap());
        let stdout = child.stdout.take().unwrap();
        let (line_tx, line_rx) = async_channel::unbounded();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if line_tx.send_blocking(ChildOutput::Line(line)).is_err() {
                    return;
                }
            }
        });
        let (event_tx, event_rx) = async_channel::unbounded();
        (
            Actor {
                child,
                stdin,
                lines: line_rx,
                events: event_tx,
                thread_id: "thread-1".into(),
                next_id: 1,
                pending_requests: HashMap::new(),
                approvals: HashMap::new(),
                items: HashMap::new(),
                usage_by_turn: HashMap::new(),
                active_turn: None,
            },
            event_rx,
        )
    }

    #[test]
    fn approval_knobs_map_all_modes() {
        assert_eq!(
            approval_knobs(ApprovalMode::Supervised),
            ("untrusted", "read-only")
        );
        assert_eq!(
            approval_knobs(ApprovalMode::AutoAcceptEdits),
            ("on-request", "workspace-write")
        );
        assert_eq!(
            approval_knobs(ApprovalMode::FullAccess),
            ("never", "danger-full-access")
        );
    }

    #[test]
    fn maps_core_item_kinds() {
        let command = map_item(&json!({"type":"commandExecution","id":"cmd-1","command":"pwd","aggregatedOutput":"/tmp\n","exitCode":0,"status":"completed"})).unwrap();
        assert!(matches!(
            command.content,
            ItemContent::CommandExecution {
                exit_code: Some(0),
                status: ItemStatus::Completed,
                ..
            }
        ));

        let file = map_item(&json!({"type":"fileChange","id":"patch-1","status":"completed","changes":[{"path":"hello.txt","kind":{"type":"add"},"diff":"+hi"}]})).unwrap();
        assert!(
            matches!(&file.content, ItemContent::FileChange { changes, .. } if changes[0].kind == FileChangeKind::Create)
        );

        let unknown = map_item(&json!({"type":"sleep","id":"sleep-1","durationMs":10})).unwrap();
        assert!(
            matches!(unknown.content, ItemContent::Other { ref provider_kind, .. } if provider_kind == "sleep")
        );
    }

    #[test]
    fn maps_token_usage_from_last_and_total() {
        let usage = map_usage(&json!({
            "total":{"totalTokens":123,"inputTokens":100,"cachedInputTokens":20,"outputTokens":23,"reasoningOutputTokens":3},
            "last":{"totalTokens":12,"inputTokens":8,"cachedInputTokens":2,"outputTokens":4,"reasoningOutputTokens":1},
            "modelContextWindow":200000
        })).unwrap();
        assert_eq!(usage.input_tokens, Some(8));
        assert_eq!(usage.cached_input_tokens, Some(2));
        assert_eq!(usage.output_tokens, Some(4));
        assert_eq!(usage.used_tokens, Some(12));
        assert_eq!(usage.context_window, Some(200000));
    }

    #[test]
    fn maps_reasoning_and_user_text() {
        let reasoning = map_item(
            &json!({"type":"reasoning","id":"r1","summary":["summary"],"content":["detail"]}),
        )
        .unwrap();
        assert!(
            matches!(reasoning.content, ItemContent::Reasoning { ref text } if text == "summary\ndetail")
        );
        // Provider-echoed user messages are suppressed: the app synthesizes the
        // canonical UserMessage at send time, so mapping one here yields None
        // (no duplicate user bubble on replay/live).
        assert!(
            map_item(&json!({"type":"userMessage","id":"u1","content":[{"type":"text","text":"hello"},{"type":"image","url":"x"}]}))
                .is_none()
        );
    }

    #[test]
    fn maps_fixture_envelopes_and_approval_response() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            for line in include_str!("../tests/fixtures/codex/v2_messages.jsonl").lines() {
                actor.handle_line(line).await;
            }

            assert!(
                matches!(events.recv().await.unwrap(), AgentEvent::TurnStarted { ref turn_id } if turn_id == "turn-1")
            );
            assert!(
                matches!(events.recv().await.unwrap(), AgentEvent::ItemStarted(ThreadItem { content: ItemContent::FileChange { ref changes, status: ItemStatus::InProgress }, .. }) if changes[0].kind == FileChangeKind::Create)
            );
            assert!(
                matches!(events.recv().await.unwrap(), AgentEvent::ApprovalRequested(ApprovalRequest { ref id, kind: ApprovalKind::FileChange { ref changes, .. }, .. }) if id == "41" && changes.len() == 1)
            );
            assert!(
                matches!(events.recv().await.unwrap(), AgentEvent::Delta { kind: DeltaKind::AssistantText, ref text, .. } if text == "PONG")
            );
            assert!(
                matches!(events.recv().await.unwrap(), AgentEvent::Delta { kind: DeltaKind::ReasoningText, ref text, .. } if text == "Checking")
            );
            assert!(
                matches!(events.recv().await.unwrap(), AgentEvent::Delta { kind: DeltaKind::CommandOutput, ref text, .. } if text == "ok\n")
            );
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::TokenUsage(TokenUsage {
                    input_tokens: Some(8),
                    ..
                })
            ));
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::ItemCompleted(ThreadItem {
                    content: ItemContent::FileChange {
                        status: ItemStatus::Completed,
                        ..
                    },
                    ..
                })
            ));
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::TurnCompleted {
                    status: TurnStatus::Completed,
                    usage: Some(TokenUsage {
                        output_tokens: Some(4),
                        ..
                    }),
                    ..
                }
            ));

            actor
                .handle_command(SessionCommand::RespondApproval {
                    request_id: "41".into(),
                    decision: ApprovalDecision::ApproveForSession,
                })
                .await
                .unwrap();
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::ApprovalResolved {
                    decision: ApprovalDecision::ApproveForSession,
                    ..
                }
            ));
            let ChildOutput::Line(response) = actor.lines.recv().await.unwrap() else {
                panic!("expected echoed response")
            };
            let response: Value = serde_json::from_str(&response).unwrap();
            assert_eq!(
                response,
                json!({"id": 41, "result": {"decision": "acceptForSession"}})
            );

            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }
}
