//! Claude Code provider: spawns the `claude` CLI as a persistent child process
//! per session and speaks the bidirectional stream-json protocol.
//!
//! The CLI is launched as:
//!
//! ```text
//! claude --print --input-format stream-json --output-format stream-json \
//!        --include-partial-messages --verbose --permission-prompt-tool stdio \
//!        [--model <model>] [--resume <session_id>]
//! ```
//!
//! `--permission-prompt-tool stdio` makes the CLI ask for tool-use permission
//! over the control protocol (`control_request` with subtype `can_use_tool`),
//! which we surface as [`AgentEvent::ApprovalRequested`] and answer with a
//! `control_response`. This flag set (and the control shapes) is lifted from the
//! `@anthropic-ai/claude-agent-sdk` `Query` implementation, which spawns the same
//! CLI. We intentionally do NOT send an `initialize` control_request: the CLI
//! streams `can_use_tool` prompts without it (verified against v2.1.206), so the
//! handshake is unnecessary for our reduced feature set.
//!
//! Everything is normalized into the canonical [`AgentEvent`] stream. An actor
//! task owns the child: it reads stdout lines, receives [`SessionCommand`]s, and
//! writes stream-json lines to stdin. Multiple turns run over one process.

use std::collections::HashMap;

use futures_lite::{AsyncBufReadExt, AsyncWriteExt, StreamExt};
use serde_json::{Value, json};
use smol::io::BufReader;
use smol::process::{Command, Stdio};

use crate::{
    AgentError, AgentEvent, ApprovalDecision, ApprovalKind, ApprovalRequest, DeltaKind, FileChange,
    FileChangeKind, ItemContent, ItemStatus, ProviderKind, ResumeCursor, SessionCommand,
    SessionHandle, SessionOptions, ThreadItem, TokenUsage, TurnStatus,
};

/// Start (or resume) a Claude Code session.
pub async fn start(opts: SessionOptions) -> Result<SessionHandle, AgentError> {
    let binary = opts
        .binary_path
        .clone()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "claude".to_string());

    let mut cmd = Command::new(&binary);
    cmd.arg("--print")
        .arg("--input-format")
        .arg("stream-json")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--include-partial-messages")
        .arg("--verbose")
        .arg("--permission-prompt-tool")
        .arg("stdio");

    if let Some(model) = &opts.model {
        cmd.arg("--model").arg(model);
    }
    if let Some(session_id) = resume_session_id(&opts.resume) {
        cmd.arg("--resume").arg(session_id);
    }

    cmd.current_dir(&opts.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // We are frequently spawned from inside Claude Code itself; strip the
        // markers that tell the CLI it is nested so the child behaves like a
        // top-level invocation.
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT");

    let mut child = cmd
        .spawn()
        .map_err(|e| AgentError::Spawn(format!("spawning `{binary}`: {e}")))?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| AgentError::Spawn("child stdin missing".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AgentError::Spawn("child stdout missing".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AgentError::Spawn("child stderr missing".into()))?;

    let (cmd_tx, cmd_rx) = async_channel::unbounded::<SessionCommand>();
    let (event_tx, event_rx) = async_channel::unbounded::<AgentEvent>();

    // Reader task: forward each stdout line (an item = one JSON message) into an
    // internal channel; closing the channel signals stdout EOF.
    let (line_tx, line_rx) = async_channel::unbounded::<String>();
    smol::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Some(line) = lines.next().await {
            match line {
                Ok(line) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    if line_tx.send(line).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    log::warn!("claude: stdout read error: {e}");
                    break;
                }
            }
        }
        drop(line_tx);
    })
    .detach();

    // Stderr drain: never protocol, just diagnostics.
    smol::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Some(Ok(line)) = lines.next().await {
            if !line.trim().is_empty() {
                log::warn!("claude[stderr]: {line}");
            }
        }
    })
    .detach();

    smol::spawn(actor_loop(child, stdin, cmd_rx, line_rx, event_tx)).detach();

    Ok(SessionHandle {
        provider: ProviderKind::ClaudeCode,
        commands: cmd_tx,
        events: event_rx,
    })
}

fn resume_session_id(resume: &Option<ResumeCursor>) -> Option<String> {
    resume
        .as_ref()
        .and_then(|c| c.0.get("session_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

async fn actor_loop(
    mut child: smol::process::Child,
    mut stdin: smol::process::ChildStdin,
    cmd_rx: async_channel::Receiver<SessionCommand>,
    line_rx: async_channel::Receiver<String>,
    event_tx: async_channel::Sender<AgentEvent>,
) {
    let mut mapper = Mapper::new();

    let closed_reason: Option<String> = loop {
        // Race a UI command against the next stdout line. `or` biases toward the
        // command channel, which is fine: both channels make independent progress.
        let sel = futures_lite::future::or(
            async { Sel::Cmd(cmd_rx.recv().await.ok()) },
            async { Sel::Line(line_rx.recv().await.ok()) },
        )
        .await;

        match sel {
            Sel::Cmd(Some(command)) => {
                if handle_command(command, &mut mapper, &mut stdin, &event_tx, &mut child)
                    .await
                    .is_break()
                {
                    break Some("shutdown requested".into());
                }
            }
            Sel::Cmd(None) => {
                // UI dropped the command sender: tear down.
                let _ = stdin.close().await;
                let _ = child.kill();
                break Some("command channel closed".into());
            }
            Sel::Line(Some(line)) => {
                let events = match serde_json::from_str::<Value>(&line) {
                    Ok(msg) => mapper.on_message(msg),
                    Err(e) => {
                        log::debug!("claude: non-JSON stdout line ({e}): {line}");
                        Vec::new()
                    }
                };
                for ev in events {
                    if event_tx.send(ev).await.is_err() {
                        let _ = child.kill();
                        return;
                    }
                }
            }
            Sel::Line(None) => {
                // stdout closed: child is exiting.
                break Some("provider process exited".into());
            }
        }
    };

    let _ = stdin.close().await;
    let _ = child.kill();
    let _ = child.status().await;
    let _ = event_tx
        .send(AgentEvent::SessionClosed {
            reason: closed_reason,
        })
        .await;
}

enum Sel {
    Cmd(Option<SessionCommand>),
    Line(Option<String>),
}

/// Whether the actor loop should stop.
enum Flow {
    Continue,
    Break,
}

impl Flow {
    fn is_break(&self) -> bool {
        matches!(self, Flow::Break)
    }
}

async fn handle_command(
    command: SessionCommand,
    mapper: &mut Mapper,
    stdin: &mut smol::process::ChildStdin,
    event_tx: &async_channel::Sender<AgentEvent>,
    child: &mut smol::process::Child,
) -> Flow {
    match command {
        SessionCommand::SendTurn { text } => {
            let turn_id = mapper.start_turn();
            let _ = event_tx
                .send(AgentEvent::TurnStarted {
                    turn_id: turn_id.clone(),
                })
                .await;
            let msg = user_message(&text);
            if write_line(stdin, &msg).await.is_err() {
                let _ = event_tx
                    .send(AgentEvent::Error {
                        message: "failed to write turn to provider stdin".into(),
                        fatal: true,
                    })
                    .await;
            }
            Flow::Continue
        }
        SessionCommand::Interrupt => {
            let msg = mapper.interrupt_request();
            let _ = write_line(stdin, &msg).await;
            Flow::Continue
        }
        SessionCommand::RespondApproval {
            request_id,
            decision,
        } => {
            if let Some(response) = mapper.build_approval_response(&request_id, decision) {
                let _ = write_line(stdin, &response).await;
                let _ = event_tx
                    .send(AgentEvent::ApprovalResolved {
                        request_id,
                        decision,
                    })
                    .await;
            } else {
                log::debug!("claude: RespondApproval for unknown request {request_id}");
            }
            Flow::Continue
        }
        SessionCommand::SetApprovalMode(mode) => {
            // Live switching lands with the permission-mode milestone; until
            // then the UI falls back to a resume-restart.
            let _ = event_tx
                .send(AgentEvent::Warning(format!(
                    "claude: live approval-mode switch to {mode:?} not implemented yet"
                )))
                .await;
            Flow::Continue
        }
        SessionCommand::Shutdown => {
            let _ = stdin.close().await;
            let _ = child.kill();
            Flow::Break
        }
    }
}

async fn write_line(
    stdin: &mut smol::process::ChildStdin,
    value: &Value,
) -> std::io::Result<()> {
    let mut line = serde_json::to_string(value).unwrap_or_default();
    line.push('\n');
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await
}

/// Build a stream-json user message line.
fn user_message(text: &str) -> Value {
    json!({
        "type": "user",
        "session_id": "",
        "parent_tool_use_id": null,
        "message": {
            "role": "user",
            "content": [{ "type": "text", "text": text }]
        }
    })
}

// ---------------------------------------------------------------------------
// Message mapping (pure, unit-testable)
// ---------------------------------------------------------------------------

/// Remembers what kind of tool-use item a `tool_use_id` refers to, so that when
/// the matching `tool_result` arrives we can emit the right `ItemCompleted`.
#[derive(Debug, Clone)]
enum ToolItem {
    Command { command: String },
    File { changes: Vec<FileChange> },
    Tool { name: String, input: Value },
}

/// A pending permission prompt, kept so `RespondApproval` can echo the tool's
/// (possibly updated) input and, for "approve for session", the tool name.
#[derive(Debug, Clone)]
struct PendingApproval {
    tool_name: String,
    input: Value,
}

pub(crate) struct Mapper {
    session_started: bool,
    current_message_id: Option<String>,
    turn_counter: usize,
    current_turn_id: Option<String>,
    control_counter: usize,
    tool_items: HashMap<String, ToolItem>,
    pending_approvals: HashMap<String, PendingApproval>,
    /// Set when we send an `interrupt` control_request; the next non-success
    /// `result` is then attributed to the interrupt rather than a failure
    /// (the CLI's result carries no reliable interrupt marker).
    interrupt_pending: bool,
}

impl Mapper {
    pub(crate) fn new() -> Self {
        Mapper {
            session_started: false,
            current_message_id: None,
            turn_counter: 0,
            current_turn_id: None,
            control_counter: 0,
            tool_items: HashMap::new(),
            pending_approvals: HashMap::new(),
            interrupt_pending: false,
        }
    }

    /// Allocate the next synthesized turn id and mark it in-flight.
    fn start_turn(&mut self) -> String {
        self.turn_counter += 1;
        let id = format!("turn-{}", self.turn_counter);
        self.current_turn_id = Some(id.clone());
        id
    }

    fn next_control_id(&mut self) -> String {
        self.control_counter += 1;
        format!("tcode-ctrl-{}", self.control_counter)
    }

    /// Client → CLI interrupt control request.
    fn interrupt_request(&mut self) -> Value {
        if self.current_turn_id.is_some() {
            self.interrupt_pending = true;
        }
        json!({
            "type": "control_request",
            "request_id": self.next_control_id(),
            "request": { "subtype": "interrupt" }
        })
    }

    /// Build the `control_response` answering a pending `can_use_tool` prompt.
    fn build_approval_response(
        &mut self,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> Option<Value> {
        let pending = self.pending_approvals.remove(request_id)?;
        let response = match decision {
            ApprovalDecision::Approve => json!({
                "behavior": "allow",
                "updatedInput": pending.input,
            }),
            ApprovalDecision::ApproveForSession => json!({
                "behavior": "allow",
                "updatedInput": pending.input,
                // "always allow" for this tool, scoped to the live session.
                "updatedPermissions": [{
                    "type": "addRules",
                    "rules": [{ "toolName": pending.tool_name }],
                    "behavior": "allow",
                    "destination": "session",
                }],
            }),
            ApprovalDecision::Deny => json!({
                "behavior": "deny",
                "message": "Denied by user.",
            }),
        };
        Some(json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": response,
            }
        }))
    }

    /// Map one CLI stdout message to zero or more outcomes.
    pub(crate) fn on_message(&mut self, msg: Value) -> Vec<AgentEvent> {
        match msg.get("type").and_then(Value::as_str) {
            Some("system") => self.on_system(&msg),
            Some("stream_event") => self.on_stream_event(&msg),
            Some("assistant") => self.on_assistant(&msg),
            Some("user") => self.on_user(&msg),
            Some("control_request") => self.on_control_request(&msg),
            Some("result") => self.on_result(&msg),
            other => {
                log::debug!("claude: ignoring message type {other:?}");
                Vec::new()
            }
        }
    }

    fn on_system(&mut self, msg: &Value) -> Vec<AgentEvent> {
        if msg.get("subtype").and_then(Value::as_str) != Some("init") {
            log::debug!(
                "claude: ignoring system/{:?}",
                msg.get("subtype").and_then(Value::as_str)
            );
            return Vec::new();
        }
        if self.session_started {
            return Vec::new();
        }
        let session_id = match msg.get("session_id").and_then(Value::as_str) {
            Some(id) => id.to_string(),
            None => return Vec::new(),
        };
        self.session_started = true;
        let model = msg
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string);
        vec![AgentEvent::SessionStarted {
            provider_session_id: session_id.clone(),
            resume: ResumeCursor(json!({ "session_id": session_id })),
            model,
        }]
    }

    fn on_stream_event(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let event = match msg.get("event") {
            Some(e) => e,
            None => return Vec::new(),
        };
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                self.current_message_id = event
                    .get("message")
                    .and_then(|m| m.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                Vec::new()
            }
            Some("content_block_delta") => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                let delta = match event.get("delta") {
                    Some(d) => d,
                    None => return Vec::new(),
                };
                let (kind, text) = match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => (
                        DeltaKind::AssistantText,
                        delta.get("text").and_then(Value::as_str),
                    ),
                    Some("thinking_delta") => (
                        DeltaKind::ReasoningText,
                        delta
                            .get("thinking")
                            .and_then(Value::as_str)
                            .or_else(|| delta.get("text").and_then(Value::as_str)),
                    ),
                    // input_json_delta and friends: tool input is reconstructed
                    // from the (complete) `assistant` message instead.
                    _ => return Vec::new(),
                };
                let text = match text {
                    Some(t) if !t.is_empty() => t,
                    _ => return Vec::new(),
                };
                vec![AgentEvent::Delta {
                    item_id: self.block_item_id(index),
                    kind,
                    text: text.to_string(),
                }]
            }
            Some("message_delta") => {
                // Live usage growth; nice-to-have for token display.
                if let Some(usage) = event.get("usage") {
                    let tu = map_usage(usage, None);
                    return vec![AgentEvent::TokenUsage(tu)];
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn block_item_id(&self, index: u64) -> String {
        match &self.current_message_id {
            Some(id) => format!("{id}:{index}"),
            None => format!("msg:{index}"),
        }
    }

    fn on_assistant(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let message = match msg.get("message") {
            Some(m) => m,
            None => return Vec::new(),
        };
        let msg_id = message
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("msg")
            .to_string();
        let content = match message.get("content").and_then(Value::as_array) {
            Some(c) => c,
            None => return Vec::new(),
        };
        let mut out = Vec::new();
        for (index, block) in content.iter().enumerate() {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                    out.push(AgentEvent::ItemCompleted(ThreadItem {
                        id: format!("{msg_id}:{index}"),
                        content: ItemContent::AssistantMessage {
                            text: text.to_string(),
                        },
                    }));
                }
                Some("thinking") => {
                    let text = block
                        .get("thinking")
                        .and_then(Value::as_str)
                        .or_else(|| block.get("text").and_then(Value::as_str))
                        .unwrap_or("");
                    out.push(AgentEvent::ItemCompleted(ThreadItem {
                        id: format!("{msg_id}:{index}"),
                        content: ItemContent::Reasoning {
                            text: text.to_string(),
                        },
                    }));
                }
                Some("tool_use") => {
                    out.extend(self.on_tool_use(block));
                }
                _ => {}
            }
        }
        out
    }

    fn on_tool_use(&mut self, block: &Value) -> Vec<AgentEvent> {
        let tool_use_id = match block.get("id").and_then(Value::as_str) {
            Some(id) => id.to_string(),
            None => return Vec::new(),
        };
        let name = block
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let input = block.get("input").cloned().unwrap_or_else(|| json!({}));

        let (item, content) = if name == "Bash" {
            let command = input
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            (
                ToolItem::Command {
                    command: command.clone(),
                },
                ItemContent::CommandExecution {
                    command,
                    output: String::new(),
                    exit_code: None,
                    status: ItemStatus::InProgress,
                },
            )
        } else if is_file_tool(&name) {
            let changes = file_changes(&name, &input);
            (
                ToolItem::File {
                    changes: changes.clone(),
                },
                ItemContent::FileChange {
                    changes,
                    status: ItemStatus::InProgress,
                },
            )
        } else {
            (
                ToolItem::Tool {
                    name: name.clone(),
                    input: input.clone(),
                },
                ItemContent::ToolCall {
                    name,
                    input,
                    output: None,
                    status: ItemStatus::InProgress,
                },
            )
        };

        self.tool_items.insert(tool_use_id.clone(), item);
        vec![AgentEvent::ItemStarted(ThreadItem {
            id: tool_use_id,
            content,
        })]
    }

    fn on_user(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let content = msg
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array);
        let content = match content {
            Some(c) => c,
            None => return Vec::new(),
        };
        let mut out = Vec::new();
        for block in content {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let tool_use_id = match block.get("tool_use_id").and_then(Value::as_str) {
                Some(id) => id.to_string(),
                None => continue,
            };
            let item = match self.tool_items.remove(&tool_use_id) {
                Some(i) => i,
                None => continue,
            };
            let is_error = block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let output = tool_result_text(block.get("content"));
            let status = if is_error {
                ItemStatus::Failed
            } else {
                ItemStatus::Completed
            };
            let content = match item {
                ToolItem::Command { command } => ItemContent::CommandExecution {
                    command,
                    output,
                    exit_code: if is_error { Some(1) } else { Some(0) },
                    status,
                },
                ToolItem::File { changes } => ItemContent::FileChange { changes, status },
                ToolItem::Tool { name, input } => ItemContent::ToolCall {
                    name,
                    input,
                    output: Some(output),
                    status,
                },
            };
            out.push(AgentEvent::ItemCompleted(ThreadItem {
                id: tool_use_id,
                content,
            }));
        }
        out
    }

    fn on_control_request(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let request = match msg.get("request") {
            Some(r) => r,
            None => return Vec::new(),
        };
        if request.get("subtype").and_then(Value::as_str) != Some("can_use_tool") {
            log::debug!(
                "claude: ignoring control_request/{:?}",
                request.get("subtype").and_then(Value::as_str)
            );
            return Vec::new();
        }
        let request_id = match msg.get("request_id").and_then(Value::as_str) {
            Some(id) => id.to_string(),
            None => return Vec::new(),
        };
        let tool_name = request
            .get("tool_name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let input = request.get("input").cloned().unwrap_or_else(|| json!({}));
        let reason = request
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string);

        let kind = if tool_name == "Bash" {
            ApprovalKind::ExecCommand {
                command: input
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                cwd: input
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                reason,
            }
        } else if is_file_tool(&tool_name) {
            ApprovalKind::FileChange {
                changes: file_changes(&tool_name, &input),
                reason,
            }
        } else {
            ApprovalKind::ToolUse {
                name: tool_name.clone(),
                input: input.clone(),
            }
        };

        self.pending_approvals.insert(
            request_id.clone(),
            PendingApproval {
                tool_name,
                input,
            },
        );

        vec![AgentEvent::ApprovalRequested(
            ApprovalRequest {
                id: request_id,
                turn_id: self.current_turn_id.clone(),
                kind,
            },
        )]
    }

    fn on_result(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let turn_id = self
            .current_turn_id
            .take()
            .unwrap_or_else(|| format!("turn-{}", self.turn_counter.max(1)));
        let mut status = result_status(msg);
        if std::mem::take(&mut self.interrupt_pending) && status != TurnStatus::Completed {
            status = TurnStatus::Interrupted;
        }
        let usage = msg
            .get("usage")
            .map(|u| map_usage(u, msg.get("modelUsage")));
        vec![AgentEvent::TurnCompleted {
            turn_id,
            status,
            usage,
        }]
    }
}

fn is_file_tool(name: &str) -> bool {
    matches!(name, "Write" | "Edit" | "MultiEdit" | "NotebookEdit")
}

/// Derive canonical [`FileChange`]s from a file-editing tool's input.
fn file_changes(name: &str, input: &Value) -> Vec<FileChange> {
    let path = input
        .get("file_path")
        .or_else(|| input.get("notebook_path"))
        .or_else(|| input.get("path"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    match name {
        "Write" => {
            let content = input.get("content").and_then(Value::as_str).unwrap_or("");
            vec![FileChange {
                path,
                kind: FileChangeKind::Create,
                diff: (!content.is_empty()).then(|| {
                    content
                        .lines()
                        .map(|l| format!("+{l}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                }),
            }]
        }
        "Edit" => {
            let old = input
                .get("old_string")
                .and_then(Value::as_str)
                .unwrap_or("");
            let new = input
                .get("new_string")
                .and_then(Value::as_str)
                .unwrap_or("");
            let mut diff = String::new();
            for l in old.lines() {
                diff.push('-');
                diff.push_str(l);
                diff.push('\n');
            }
            for l in new.lines() {
                diff.push('+');
                diff.push_str(l);
                diff.push('\n');
            }
            vec![FileChange {
                path,
                kind: FileChangeKind::Modify,
                diff: (!diff.is_empty()).then(|| diff.trim_end().to_string()),
            }]
        }
        _ => vec![FileChange {
            path,
            kind: FileChangeKind::Modify,
            diff: None,
        }],
    }
}

/// Flatten a `tool_result` content field (string or block array) into text.
fn tool_result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => {
            let mut parts = Vec::new();
            for block in blocks {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    parts.push(text.to_string());
                } else if block.get("type").and_then(Value::as_str) == Some("text") {
                    // no-op: handled above
                } else {
                    parts.push(block.to_string());
                }
            }
            parts.join("\n")
        }
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn result_status(msg: &Value) -> TurnStatus {
    if msg.get("subtype").and_then(Value::as_str) == Some("success") {
        return TurnStatus::Completed;
    }
    // Distinguish user-triggered interrupts from genuine failures by scanning the
    // result text / error markers, matching the SDK's interrupted heuristic.
    let haystack = format!(
        "{} {}",
        msg.get("result")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        msg.get("subtype").and_then(Value::as_str).unwrap_or_default()
    )
    .to_lowercase();
    if haystack.contains("interrupt") || haystack.contains("abort") || haystack.contains("cancel")
    {
        TurnStatus::Interrupted
    } else {
        TurnStatus::Failed
    }
}

fn map_usage(usage: &Value, model_usage: Option<&Value>) -> TokenUsage {
    let get = |k: &str| usage.get(k).and_then(Value::as_u64);
    let input = get("input_tokens");
    let cache_read = get("cache_read_input_tokens");
    let cache_creation = get("cache_creation_input_tokens");
    let output = get("output_tokens");

    let used = [input, cache_read, cache_creation, output]
        .into_iter()
        .flatten()
        .sum::<u64>();
    let used_tokens = (used > 0).then_some(used);

    let context_window = model_usage
        .and_then(Value::as_object)
        .and_then(|m| {
            m.values()
                .filter_map(|v| v.get("contextWindow").and_then(Value::as_u64))
                .max()
        });

    TokenUsage {
        input_tokens: input,
        cached_input_tokens: cache_read,
        output_tokens: output,
        used_tokens,
        context_window,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(mapper: &mut Mapper, line: &str) -> Vec<AgentEvent> {
        let msg: Value = serde_json::from_str(line).expect("valid json fixture line");
        mapper.on_message(msg)
    }

    #[test]
    fn init_emits_session_started_once() {
        let mut m = Mapper::new();
        let line = r#"{"type":"system","subtype":"init","session_id":"sess-1","model":"claude-opus-4-8","cwd":"/tmp"}"#;
        let evs = feed(&mut m, line);
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            AgentEvent::SessionStarted {
                provider_session_id,
                resume,
                model,
            } => {
                assert_eq!(provider_session_id, "sess-1");
                assert_eq!(resume.0.get("session_id").unwrap(), "sess-1");
                assert_eq!(model.as_deref(), Some("claude-opus-4-8"));
            }
            other => panic!("expected SessionStarted, got {other:?}"),
        }
        // Second init is ignored.
        assert!(feed(&mut m, line).is_empty());
    }

    #[test]
    fn text_delta_maps_to_assistant_delta() {
        let mut m = Mapper::new();
        feed(
            &mut m,
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_1"}}}"#,
        );
        let evs = feed(
            &mut m,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}}"#,
        );
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            AgentEvent::Delta {
                item_id,
                kind,
                text,
            } => {
                assert_eq!(item_id, "msg_1:0");
                assert_eq!(*kind, DeltaKind::AssistantText);
                assert_eq!(text, "Hi");
            }
            other => panic!("expected Delta, got {other:?}"),
        }
    }

    #[test]
    fn thinking_delta_maps_to_reasoning() {
        let mut m = Mapper::new();
        feed(
            &mut m,
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_r"}}}"#,
        );
        let evs = feed(
            &mut m,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"hmm"}}}"#,
        );
        match &evs[0] {
            AgentEvent::Delta {
                item_id,
                kind,
                text,
            } => {
                assert_eq!(item_id, "msg_r:1");
                assert_eq!(*kind, DeltaKind::ReasoningText);
                assert_eq!(text, "hmm");
            }
            other => panic!("expected reasoning Delta, got {other:?}"),
        }
    }

    #[test]
    fn assistant_text_block_completes_item() {
        let mut m = Mapper::new();
        let evs = feed(
            &mut m,
            r#"{"type":"assistant","message":{"id":"msg_2","content":[{"type":"text","text":"Hello there"}]}}"#,
        );
        match &evs[0] {
            AgentEvent::ItemCompleted(item) => {
                assert_eq!(item.id, "msg_2:0");
                match &item.content {
                    ItemContent::AssistantMessage { text } => assert_eq!(text, "Hello there"),
                    other => panic!("expected AssistantMessage, got {other:?}"),
                }
            }
            other => panic!("expected ItemCompleted, got {other:?}"),
        }
    }

    #[test]
    fn bash_tool_use_then_result_roundtrip() {
        let mut m = Mapper::new();
        let started = feed(
            &mut m,
            r#"{"type":"assistant","message":{"id":"msg_3","content":[{"type":"tool_use","id":"toolu_bash","name":"Bash","input":{"command":"echo hi"}}]}}"#,
        );
        match &started[0] {
            AgentEvent::ItemStarted(item) => {
                assert_eq!(item.id, "toolu_bash");
                match &item.content {
                    ItemContent::CommandExecution {
                        command, status, ..
                    } => {
                        assert_eq!(command, "echo hi");
                        assert_eq!(*status, ItemStatus::InProgress);
                    }
                    other => panic!("expected CommandExecution, got {other:?}"),
                }
            }
            other => panic!("expected ItemStarted, got {other:?}"),
        }

        let done = feed(
            &mut m,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_bash","content":"hi\n"}]}}"#,
        );
        match &done[0] {
            AgentEvent::ItemCompleted(item) => match &item.content {
                ItemContent::CommandExecution {
                    output,
                    status,
                    exit_code,
                    ..
                } => {
                    assert_eq!(output, "hi\n");
                    assert_eq!(*status, ItemStatus::Completed);
                    assert_eq!(*exit_code, Some(0));
                }
                other => panic!("expected CommandExecution, got {other:?}"),
            },
            other => panic!("expected ItemCompleted, got {other:?}"),
        }
    }

    #[test]
    fn write_tool_maps_to_file_change() {
        let mut m = Mapper::new();
        let evs = feed(
            &mut m,
            r#"{"type":"assistant","message":{"id":"msg_4","content":[{"type":"tool_use","id":"toolu_w","name":"Write","input":{"file_path":"/tmp/x.txt","content":"hi\n"}}]}}"#,
        );
        match &evs[0] {
            AgentEvent::ItemStarted(item) => match &item.content {
                ItemContent::FileChange { changes, status } => {
                    assert_eq!(changes.len(), 1);
                    assert_eq!(changes[0].path, "/tmp/x.txt");
                    assert_eq!(changes[0].kind, FileChangeKind::Create);
                    assert_eq!(*status, ItemStatus::InProgress);
                }
                other => panic!("expected FileChange, got {other:?}"),
            },
            other => panic!("expected ItemStarted, got {other:?}"),
        }
    }

    #[test]
    fn can_use_tool_maps_to_approval_and_response() {
        let mut m = Mapper::new();
        m.start_turn();
        let evs = feed(
            &mut m,
            r#"{"type":"control_request","request_id":"req-1","request":{"subtype":"can_use_tool","tool_name":"Write","input":{"file_path":"/tmp/h.txt","content":"hi\n"},"description":"h.txt"}}"#,
        );
        let request_id = match &evs[0] {
            AgentEvent::ApprovalRequested(req) => {
                assert_eq!(req.id, "req-1");
                assert_eq!(req.turn_id.as_deref(), Some("turn-1"));
                match &req.kind {
                    ApprovalKind::FileChange { changes, reason } => {
                        assert_eq!(changes[0].path, "/tmp/h.txt");
                        assert_eq!(reason.as_deref(), Some("h.txt"));
                    }
                    other => panic!("expected FileChange approval, got {other:?}"),
                }
                req.id.clone()
            }
            other => panic!("expected ApprovalRequested, got {other:?}"),
        };

        let resp = m
            .build_approval_response(&request_id, ApprovalDecision::Approve)
            .expect("response for known request");
        assert_eq!(resp["type"], "control_response");
        assert_eq!(resp["response"]["subtype"], "success");
        assert_eq!(resp["response"]["request_id"], "req-1");
        assert_eq!(resp["response"]["response"]["behavior"], "allow");
        assert_eq!(
            resp["response"]["response"]["updatedInput"]["file_path"],
            "/tmp/h.txt"
        );
        // Consumed: a second response is not produced.
        assert!(
            m.build_approval_response(&request_id, ApprovalDecision::Approve)
                .is_none()
        );
    }

    #[test]
    fn deny_and_session_approval_shapes() {
        let mut m = Mapper::new();
        feed(
            &mut m,
            r#"{"type":"control_request","request_id":"req-d","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"rm -rf /"}}}"#,
        );
        let deny = m
            .build_approval_response("req-d", ApprovalDecision::Deny)
            .unwrap();
        assert_eq!(deny["response"]["response"]["behavior"], "deny");

        let mut m2 = Mapper::new();
        feed(
            &mut m2,
            r#"{"type":"control_request","request_id":"req-s","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"}}}"#,
        );
        let sess = m2
            .build_approval_response("req-s", ApprovalDecision::ApproveForSession)
            .unwrap();
        assert_eq!(sess["response"]["response"]["behavior"], "allow");
        assert_eq!(
            sess["response"]["response"]["updatedPermissions"][0]["type"],
            "addRules"
        );
        assert_eq!(
            sess["response"]["response"]["updatedPermissions"][0]["rules"][0]["toolName"],
            "Bash"
        );
    }

    #[test]
    fn bash_approval_maps_to_exec_command() {
        let mut m = Mapper::new();
        let evs = feed(
            &mut m,
            r#"{"type":"control_request","request_id":"req-b","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"echo hi"}}}"#,
        );
        match &evs[0] {
            AgentEvent::ApprovalRequested(req) => match &req.kind {
                ApprovalKind::ExecCommand { command, .. } => assert_eq!(command, "echo hi"),
                other => panic!("expected ExecCommand, got {other:?}"),
            },
            other => panic!("expected ApprovalRequested, got {other:?}"),
        }
    }

    #[test]
    fn result_maps_to_turn_completed_with_usage() {
        let mut m = Mapper::new();
        let turn_id = m.start_turn();
        let evs = feed(
            &mut m,
            r#"{"type":"result","subtype":"success","is_error":false,"usage":{"input_tokens":100,"cache_read_input_tokens":50,"cache_creation_input_tokens":10,"output_tokens":20},"modelUsage":{"claude-opus-4-8[1m]":{"contextWindow":1000000}}}"#,
        );
        match &evs[0] {
            AgentEvent::TurnCompleted {
                turn_id: tid,
                status,
                usage,
            } => {
                assert_eq!(tid, &turn_id);
                assert_eq!(*status, TurnStatus::Completed);
                let usage = usage.as_ref().expect("usage present");
                assert_eq!(usage.input_tokens, Some(100));
                assert_eq!(usage.cached_input_tokens, Some(50));
                assert_eq!(usage.output_tokens, Some(20));
                assert_eq!(usage.used_tokens, Some(180));
                assert_eq!(usage.context_window, Some(1_000_000));
            }
            other => panic!("expected TurnCompleted, got {other:?}"),
        }
    }

    #[test]
    fn interrupted_result_status() {
        let mut idle = Mapper::new();
        idle.interrupt_request();
        assert!(!idle.interrupt_pending);
        let idle_result = feed(
            &mut idle,
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"provider failed"}"#,
        );
        assert!(matches!(
            idle_result[0],
            AgentEvent::TurnCompleted {
                status: TurnStatus::Failed,
                ..
            }
        ));

        let mut m = Mapper::new();
        m.start_turn();
        m.interrupt_request();
        assert!(m.interrupt_pending);
        let attributed = feed(
            &mut m,
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"provider failed"}"#,
        );
        assert!(matches!(
            attributed[0],
            AgentEvent::TurnCompleted {
                status: TurnStatus::Interrupted,
                ..
            }
        ));

        m.start_turn();
        let evs = feed(
            &mut m,
            r#"{"type":"result","subtype":"error_during_execution","is_error":false,"result":"Request was aborted"}"#,
        );
        match &evs[0] {
            AgentEvent::TurnCompleted { status, .. } => {
                assert_eq!(*status, TurnStatus::Interrupted)
            }
            other => panic!("expected TurnCompleted, got {other:?}"),
        }
    }

    #[test]
    fn turn_ids_increment() {
        let mut m = Mapper::new();
        assert_eq!(m.start_turn(), "turn-1");
        assert_eq!(m.start_turn(), "turn-2");
    }

    #[test]
    fn full_fixture_trace_parses() {
        // Replay a captured real trace; assert the key canonical events appear.
        let trace = include_str!("../tests/fixtures/claude/tool_use_trace.jsonl");
        let mut m = Mapper::new();
        let mut all = Vec::new();
        for line in trace.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let msg: Value = serde_json::from_str(line).expect("fixture line is json");
            all.extend(m.on_message(msg));
        }
        assert!(
            all.iter()
                .any(|e| matches!(e, AgentEvent::SessionStarted { .. })),
            "expected SessionStarted"
        );
        assert!(
            all.iter()
                .any(|e| matches!(e, AgentEvent::ApprovalRequested(_))),
            "expected ApprovalRequested"
        );
        assert!(
            all.iter().any(|e| matches!(
                e,
                AgentEvent::ItemStarted(ThreadItem {
                    content: ItemContent::FileChange { .. },
                    ..
                })
            )),
            "expected FileChange ItemStarted"
        );
        assert!(
            all.iter().any(|e| matches!(
                e,
                AgentEvent::TurnCompleted {
                    status: TurnStatus::Completed,
                    ..
                }
            )),
            "expected completed TurnCompleted"
        );
    }
}
