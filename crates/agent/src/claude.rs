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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use futures_lite::{AsyncBufReadExt, AsyncWriteExt, StreamExt};
use serde_json::{Value, json};
use smol::io::BufReader;
use smol::process::Stdio;

use crate::{
    AgentError, AgentEvent, ApprovalDecision, ApprovalKind, ApprovalMode, ApprovalRequest,
    Attachment, DeltaKind, FileChange, FileChangeKind, InteractionMode, ItemContent, ItemStatus,
    LaunchEnv, ModelSpec, OptionDescriptor, OptionSelection, PlanStep, PlanStepStatus,
    ProviderCommand, ProviderCommandKind, ProviderKind, ResumeCursor, SelectOption, SessionCommand,
    SessionHandle, SessionOptions, ThreadItem, TokenUsage, TurnStatus, UserInputOption,
    UserInputQuestion,
};

/// T3's exact message denied to `ExitPlanMode` once the plan is captured.
const EXIT_PLAN_DENY_MESSAGE: &str = "The client captured your proposed plan. Stop here and wait for the user's feedback or implementation request in a later turn.";

/// Map a canonical [`ApprovalMode`] onto the value Claude's CLI expects for
/// `--permission-mode` (and the `set_permission_mode` control request).
///
/// Verified against `@anthropic-ai/claude-agent-sdk` v0.3.170
/// `SDKControlSetPermissionModeRequest` (`sdk.d.ts`): `'default'` prompts for
/// dangerous operations, `'acceptEdits'` auto-accepts file edits, and
/// `'bypassPermissions'` skips all permission checks.
pub(crate) fn permission_mode_flag(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::Supervised => "default",
        ApprovalMode::AutoAcceptEdits => "acceptEdits",
        ApprovalMode::FullAccess => "bypassPermissions",
    }
}

/// Start (or resume) a Claude Code session.
pub async fn start(opts: SessionOptions) -> Result<SessionHandle, AgentError> {
    // Absolute path: a bare name would be resolved against the session cwd we
    // set below, which breaks PATH lookup (see `resolve_binary`).
    let binary = crate::resolve_binary(opts.binary_path.as_deref(), "claude")?;
    let binary = binary.to_string_lossy().into_owned();

    // Resolve model-scoped launch options from the persisted selections
    // (effort/context/fast/thinking are launch-time only; mid-session changes
    // ride the resume-restart machinery).
    let launch = ClaudeLaunchOptions::resolve(opts.model.as_deref(), &opts.option_selections);

    let mut cmd = crate::process::async_command(&binary);
    cmd.arg("--print")
        .arg("--input-format")
        .arg("stream-json")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--include-partial-messages")
        .arg("--verbose")
        .arg("--permission-prompt-tool")
        .arg("stdio")
        .arg("--permission-mode")
        .arg(permission_mode_flag(opts.approval_mode));

    if let Some(model) = &launch.model_id {
        cmd.arg("--model").arg(model);
    }
    if let Some(effort) = &launch.effort {
        cmd.arg("--effort").arg(effort);
    }
    if let Some(settings) = &launch.settings_json {
        cmd.arg("--settings").arg(settings);
    }
    if let Some(session_id) = resume_session_id(&opts.resume) {
        cmd.arg("--resume").arg(session_id);
    }
    // Register the embedded preview MCP server so the agent can drive the
    // in-app browser. The token rides in an Authorization header (see
    // `McpRegistration::claude_mcp_config_json`).
    if let Some(mcp) = &opts.mcp_server {
        cmd.arg("--mcp-config").arg(mcp.claude_mcp_config_json());
    }
    // Settings → Providers "Launch arguments", appended last so the user can
    // override anything we set above.
    for arg in &opts.extra_args {
        cmd.arg(arg);
    }
    log::debug!(
        "claude spawn args: model={:?} effort={:?} settings={:?} ultrathink={} permission-mode={}",
        launch.model_id,
        launch.effort,
        launch.settings_json,
        launch.ultrathink,
        permission_mode_flag(opts.approval_mode),
    );

    cmd.current_dir(&opts.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // We are frequently spawned from inside Claude Code itself; strip the
        // markers that tell the CLI it is nested so the child behaves like a
        // top-level invocation.
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT");
    // Per-provider environment (Settings → Providers): custom variables and the
    // `HOME` override that relocates `.claude.json` / `.claude`.
    for (key, value) in opts.launch_env.pairs(ProviderKind::ClaudeCode) {
        cmd.env(key, value);
    }

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

    let session_config = SessionConfig {
        ultrathink: launch.ultrathink,
        interaction_mode: opts.interaction_mode,
        base_permission_mode: permission_mode_flag(opts.approval_mode),
        approval_mode: opts.approval_mode,
    };
    smol::spawn(actor_loop(
        child,
        stdin,
        cmd_rx,
        line_rx,
        event_tx,
        session_config,
    ))
    .detach();

    Ok(SessionHandle {
        provider: ProviderKind::ClaudeCode,
        commands: cmd_tx,
        events: event_rx,
    })
}

/// Model-scoped launch flags resolved from the session's option selections.
#[derive(Debug, Default)]
struct ClaudeLaunchOptions {
    /// Model id with a `[1m]` suffix appended for the 1M context window.
    model_id: Option<String>,
    /// `--effort` value after T3's compatibility transforms (`None` when the
    /// selection is `ultrathink`, which is a prompt-prefix mode).
    effort: Option<String>,
    /// `--settings` JSON string (fastMode / ultracode / alwaysThinkingEnabled).
    settings_json: Option<String>,
    /// Whether the effort selection is `ultrathink` (prompt-prefix mode).
    ultrathink: bool,
}

impl ClaudeLaunchOptions {
    fn resolve(model: Option<&str>, selections: &[OptionSelection]) -> Self {
        let spec = model.and_then(model_spec);
        let raw_effort = selection_str(selections, "reasoningEffort");
        let resolved_effort = resolve_claude_effort(spec.as_ref(), raw_effort.as_deref());
        let ultrathink = resolved_effort.as_deref() == Some("ultrathink");
        let ultracode = resolved_effort.as_deref() == Some("ultracode");
        let effort = normalize_claude_cli_effort(resolved_effort.as_deref(), model);

        // Model id: append `[1m]` when the 1M context window is selected.
        let model_id = model.map(|m| {
            if selection_str(selections, "contextWindow").as_deref() == Some("1m") {
                format!("{m}[1m]")
            } else {
                m.to_owned()
            }
        });

        // `--settings` object: only supported/true keys are emitted.
        let fast_supported = spec
            .as_ref()
            .map(|s| has_boolean_option(s, "fastMode"))
            .unwrap_or(false);
        let thinking_supported = spec
            .as_ref()
            .map(|s| has_boolean_option(s, "thinking"))
            .unwrap_or(false);
        let fast_mode = fast_supported && selection_bool(selections, "fastMode") == Some(true);
        let thinking = if thinking_supported {
            selection_bool(selections, "thinking")
        } else {
            None
        };

        let mut settings = serde_json::Map::new();
        if let Some(thinking) = thinking {
            settings.insert("alwaysThinkingEnabled".into(), json!(thinking));
        }
        if fast_mode {
            settings.insert("fastMode".into(), json!(true));
        }
        if ultracode {
            settings.insert("ultracode".into(), json!(true));
        }
        let settings_json = (!settings.is_empty())
            .then(|| serde_json::to_string(&Value::Object(settings)).unwrap_or_default());

        ClaudeLaunchOptions {
            model_id,
            effort,
            settings_json,
            ultrathink,
        }
    }
}

/// Per-session config threaded into the actor loop / mapper.
#[derive(Debug, Clone)]
struct SessionConfig {
    ultrathink: bool,
    interaction_mode: InteractionMode,
    base_permission_mode: &'static str,
    approval_mode: ApprovalMode,
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
    config: SessionConfig,
) {
    let mut mapper = Mapper::new();
    mapper.configure(config);

    let closed_reason: Option<String> = loop {
        // Race a UI command against the next stdout line. `or` biases toward the
        // command channel, which is fine: both channels make independent progress.
        let sel = futures_lite::future::or(async { Sel::Cmd(cmd_rx.recv().await.ok()) }, async {
            Sel::Line(line_rx.recv().await.ok())
        })
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
                // Drain any control responses the mapper needs to write back
                // (e.g. the auto-deny answering an `ExitPlanMode` prompt).
                for write in mapper.take_outgoing() {
                    let _ = write_line(&mut stdin, &write).await;
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
        SessionCommand::SendTurn {
            text,
            options,
            attachments,
        } => {
            // Apply the interaction mode (per-turn override, else session mode)
            // via a `set_permission_mode` control request when it has changed.
            let mode = options
                .as_ref()
                .and_then(|o| o.interaction_mode)
                .unwrap_or(mapper.interaction_mode);
            let desired = match mode {
                InteractionMode::Plan => "plan",
                InteractionMode::Build => mapper.base_permission_mode,
            };
            if desired != mapper.applied_permission_mode {
                let req = mapper.set_permission_mode_request_str(desired);
                let _ = write_line(stdin, &req).await;
                mapper.applied_permission_mode = desired.to_string();
            }

            let turn_id = mapper.start_turn();
            let _ = event_tx
                .send(AgentEvent::TurnStarted {
                    turn_id: turn_id.clone(),
                })
                .await;
            // `ultrathink` is a prompt-prefix mode, not a `--effort` value.
            let text = if mapper.ultrathink {
                format!("Ultrathink:\n{text}")
            } else {
                text
            };
            let msg = user_message(&text, &attachments);
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
        SessionCommand::SetInteractionMode(mode) => {
            // Stored now; the `set_permission_mode` switch is issued before the
            // next `SendTurn` (matching T3's per-message application).
            mapper.interaction_mode = mode;
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
        SessionCommand::RespondUserInput {
            request_id,
            answers,
        } => {
            if let Some(response) = mapper.build_user_input_response(&request_id, &answers) {
                let _ = write_line(stdin, &response).await;
                let _ = event_tx
                    .send(AgentEvent::UserInputResolved {
                        request_id,
                        answers,
                    })
                    .await;
            } else {
                log::debug!("claude: RespondUserInput for unknown request {request_id}");
            }
            Flow::Continue
        }
        SessionCommand::SetApprovalMode(mode) => {
            // The CLI's control protocol switches permission mode live via a
            // `set_permission_mode` control_request (same shape the Agent SDK
            // sends). On success we emit nothing — the UI updated optimistically;
            // only a stdin write failure warrants a Warning.
            let flag = permission_mode_flag(mode);
            mapper.base_permission_mode = flag;
            mapper.full_access = mode == ApprovalMode::FullAccess;
            let msg = mapper.set_permission_mode_request(mode);
            if write_line(stdin, &msg).await.is_err() {
                let _ = event_tx
                    .send(AgentEvent::Warning(format!(
                        "claude: failed to switch permission mode to {mode:?}"
                    )))
                    .await;
            } else {
                mapper.applied_permission_mode = flag.to_string();
            }
            Flow::Continue
        }
        SessionCommand::Shutdown => {
            // Settle any pending AskUserQuestion prompts: deny the callback with
            // T3's cancel message and emit an empty resolution (S2 §4.2).
            for (request_id, response) in mapper.cancel_pending_user_input() {
                let _ = write_line(stdin, &response).await;
                let _ = event_tx
                    .send(AgentEvent::UserInputResolved {
                        request_id,
                        answers: serde_json::Map::new(),
                    })
                    .await;
            }
            let _ = stdin.close().await;
            let _ = child.kill();
            Flow::Break
        }
    }
}

async fn write_line(stdin: &mut smol::process::ChildStdin, value: &Value) -> std::io::Result<()> {
    let mut line = serde_json::to_string(value).unwrap_or_default();
    line.push('\n');
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await
}

/// Build a stream-json user message line. Text comes first, followed by one
/// `image` content block per attachment (`source: {type: "base64", media_type,
/// data}` — the Anthropic content-block shape the CLI accepts).
fn user_message(text: &str, attachments: &[Attachment]) -> Value {
    let mut content = vec![json!({ "type": "text", "text": text })];
    for attachment in attachments {
        content.push(json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": attachment.media_type,
                "data": attachment.data_base64,
            }
        }));
    }
    json!({
        "type": "user",
        "session_id": "",
        "parent_tool_use_id": null,
        "message": {
            "role": "user",
            "content": content,
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
/// (possibly updated) input and, for "approve for session", forward the SDK's
/// `permission_suggestions` verbatim.
#[derive(Debug, Clone)]
struct PendingApproval {
    input: Value,
    /// `permission_suggestions` from the `can_use_tool` control_request,
    /// forwarded unchanged as `updatedPermissions` on `ApproveForSession` when
    /// the SDK supplied a non-empty array (S2 §4.3).
    suggestions: Option<Value>,
}

pub(crate) struct Mapper {
    session_started: bool,
    current_message_id: Option<String>,
    turn_counter: usize,
    current_turn_id: Option<String>,
    control_counter: usize,
    tool_items: HashMap<String, ToolItem>,
    pending_approvals: HashMap<String, PendingApproval>,
    /// Pending `AskUserQuestion` prompts: control request_id → the original
    /// `questions` array, echoed back verbatim in the allow response.
    pending_user_input: HashMap<String, Value>,
    /// Whether the session runs in full-access (bypassPermissions) mode, in
    /// which normal tools that reach `can_use_tool` are auto-allowed with no
    /// approval event (AskUserQuestion / ExitPlanMode are still handled first).
    full_access: bool,
    /// Set when we send an `interrupt` control_request; the next non-success
    /// `result` is then attributed to the interrupt rather than a failure
    /// (the CLI's result carries no reliable interrupt marker).
    interrupt_pending: bool,
    /// Whether the effort selection is `ultrathink` (→ prompt prefix).
    ultrathink: bool,
    /// Session Build/Plan mode (updated by `SetInteractionMode`).
    interaction_mode: InteractionMode,
    /// Permission mode to restore on Build (from the session's ApprovalMode).
    base_permission_mode: &'static str,
    /// Permission mode currently applied on the CLI, so we only switch on change.
    applied_permission_mode: String,
    /// Dedupe keys for captured `ExitPlanMode` plans (tool id, else plan text).
    exit_plan_captures: HashSet<String>,
    /// Control responses to write back (e.g. the auto-deny for `ExitPlanMode`).
    outgoing: Vec<Value>,
    /// Cumulative tokens processed across every completed turn this session
    /// (Claude reports only per-turn usage, so we accumulate it ourselves for
    /// the "Total processed" display).
    cumulative_processed: u64,
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
            pending_user_input: HashMap::new(),
            full_access: false,
            interrupt_pending: false,
            ultrathink: false,
            interaction_mode: InteractionMode::Build,
            base_permission_mode: "default",
            applied_permission_mode: "default".to_string(),
            exit_plan_captures: HashSet::new(),
            outgoing: Vec::new(),
            cumulative_processed: 0,
        }
    }

    fn configure(&mut self, config: SessionConfig) {
        self.ultrathink = config.ultrathink;
        self.interaction_mode = config.interaction_mode;
        self.base_permission_mode = config.base_permission_mode;
        self.applied_permission_mode = config.base_permission_mode.to_string();
        self.full_access = config.approval_mode == ApprovalMode::FullAccess;
    }

    /// Drain queued control-response writes for the actor to send.
    fn take_outgoing(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.outgoing)
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

    /// Client → CLI `set_permission_mode` control request. Wire shape verified
    /// against `@anthropic-ai/claude-agent-sdk` v0.3.170 (`browser-sdk.js`):
    /// `request(e)` wraps the payload as
    /// `{request_id, type:"control_request", request:e}`, and
    /// `setPermissionMode(m)` sends `{subtype:"set_permission_mode", mode:m}`.
    fn set_permission_mode_request(&mut self, mode: ApprovalMode) -> Value {
        self.set_permission_mode_request_str(permission_mode_flag(mode))
    }

    /// `set_permission_mode` with a raw wire mode string (e.g. `"plan"`).
    fn set_permission_mode_request_str(&mut self, mode: &str) -> Value {
        json!({
            "type": "control_request",
            "request_id": self.next_control_id(),
            "request": {
                "subtype": "set_permission_mode",
                "mode": mode,
            }
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
            ApprovalDecision::ApproveForSession => {
                // T3 does not synthesize a rule: it forwards the SDK's
                // `permission_suggestions` verbatim as `updatedPermissions`,
                // and only when they were supplied (S2 §4.3). Absent
                // suggestions, this is wire-equivalent to a one-time allow.
                match &pending.suggestions {
                    Some(suggestions) => json!({
                        "behavior": "allow",
                        "updatedInput": pending.input,
                        "updatedPermissions": suggestions,
                    }),
                    None => json!({
                        "behavior": "allow",
                        "updatedInput": pending.input,
                    }),
                }
            }
            ApprovalDecision::Deny => json!({
                "behavior": "deny",
                "message": "User declined tool execution.",
            }),
            ApprovalDecision::Cancel => json!({
                "behavior": "deny",
                "message": "User cancelled tool execution.",
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

    /// Build the `control_response` allowing a pending `AskUserQuestion` prompt,
    /// echoing the original `questions` alongside the collected `answers`
    /// (S2 §1.2 / §2.3). Returns `None` for an unknown request id.
    fn build_user_input_response(
        &mut self,
        request_id: &str,
        answers: &serde_json::Map<String, Value>,
    ) -> Option<Value> {
        let questions = self.pending_user_input.remove(request_id)?;
        Some(json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": {
                    "behavior": "allow",
                    "updatedInput": {
                        "questions": questions,
                        "answers": answers,
                    }
                }
            }
        }))
    }

    /// Drain every pending `AskUserQuestion`, producing `(request_id, deny
    /// control_response)` pairs with T3's cancel message (S2 §1.2 abort path).
    fn cancel_pending_user_input(&mut self) -> Vec<(String, Value)> {
        let pending: Vec<String> = self.pending_user_input.keys().cloned().collect();
        pending
            .into_iter()
            .map(|request_id| {
                self.pending_user_input.remove(&request_id);
                let response = json!({
                    "type": "control_response",
                    "response": {
                        "subtype": "success",
                        "request_id": request_id,
                        "response": {
                            "behavior": "deny",
                            "message": "User cancelled tool execution.",
                        }
                    }
                });
                (request_id, response)
            })
            .collect()
    }

    /// Emit a [`AgentEvent::ProposedPlan`] for a captured plan, deduping across
    /// the assistant-block and permission-callback capture paths (T3 captures
    /// from both). Returns `None` if this plan was already captured.
    fn capture_proposed_plan(
        &mut self,
        tool_use_id: Option<&str>,
        markdown: String,
    ) -> Option<AgentEvent> {
        let key = match tool_use_id.filter(|id| !id.is_empty()) {
            Some(id) => format!("tool:{id}"),
            None => format!("plan:{markdown}"),
        };
        if !self.exit_plan_captures.insert(key) {
            return None;
        }
        let item_id = tool_use_id
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("plan-{}", self.exit_plan_captures.len()));
        Some(AgentEvent::ProposedPlan { item_id, markdown })
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
        match msg.get("subtype").and_then(Value::as_str) {
            Some("init") => {}
            // Claude compacted its context window (verified shape:
            // `{type:"system", subtype:"compact_boundary", compact_metadata:{…}}`).
            Some("compact_boundary") => return vec![AgentEvent::ContextCompacted],
            other => {
                log::debug!("claude: ignoring system/{other:?}");
                return Vec::new();
            }
        }
        if self.session_started {
            return Vec::new();
        }
        let session_id = match msg.get("session_id").and_then(Value::as_str) {
            Some(id) => id.to_string(),
            None => return Vec::new(),
        };
        self.session_started = true;
        let model = msg.get("model").and_then(Value::as_str).map(str::to_string);
        let mut events = vec![AgentEvent::SessionStarted {
            provider_session_id: session_id.clone(),
            resume: ResumeCursor(json!({ "session_id": session_id })),
            model,
        }];
        // The `slash_commands` (Command) and `skills` (Skill) arrays feed the
        // composer's `/` and `$` menus. Both are arrays of names (no descriptions).
        let commands = parse_provider_commands(msg);
        if !commands.is_empty() {
            events.push(AgentEvent::ProviderCommands { commands });
        }
        events
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

        // TodoWrite drives the structured plan/task sidebar, not the timeline.
        if is_todo_tool(&name) {
            if let Some(steps) = extract_plan_steps_from_todo(&input) {
                return vec![AgentEvent::PlanUpdated {
                    turn_id: self.current_turn_id.clone(),
                    explanation: None,
                    steps,
                }];
            }
            return Vec::new();
        }

        // ExitPlanMode: capture the proposed plan from the assistant block
        // (deduped against the permission-callback capture).
        if name == "ExitPlanMode" {
            if let Some(markdown) = extract_exit_plan_markdown(&input) {
                if let Some(event) = self.capture_proposed_plan(Some(&tool_use_id), markdown) {
                    return vec![event];
                }
            }
            return Vec::new();
        }

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

        // (a) AskUserQuestion → structured user-input flow, in ALL access modes
        // (its branch precedes the full-access allow branch; S2 §1.1/§1.2).
        if tool_name == "AskUserQuestion" {
            let questions_raw = input.get("questions").cloned().unwrap_or_else(|| json!([]));
            let questions = parse_ask_user_questions(&input);
            self.pending_user_input
                .insert(request_id.clone(), questions_raw);
            return vec![AgentEvent::UserInputRequested {
                request_id,
                questions,
            }];
        }

        // (b) ExitPlanMode: capture the plan (deduped against the assistant-block
        // capture via the shared `tool_use_id`), then auto-deny with T3's exact
        // message rather than surfacing an approval to the user.
        if tool_name == "ExitPlanMode" {
            let tool_use_id = request
                .get("tool_use_id")
                .and_then(Value::as_str)
                .unwrap_or(&request_id);
            let mut events = Vec::new();
            if let Some(markdown) = extract_exit_plan_markdown(&input) {
                if let Some(event) = self.capture_proposed_plan(Some(tool_use_id), markdown) {
                    events.push(event);
                }
            }
            self.outgoing.push(json!({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": request_id,
                    "response": {
                        "behavior": "deny",
                        "message": EXIT_PLAN_DENY_MESSAGE,
                    }
                }
            }));
            return events;
        }

        // (c) Full-access: ordinary SDK permission checks are bypassed, but the
        // callback stays installed. Any non-special tool that reaches it is
        // auto-allowed with no approval event (S2 §1.1/§1.2 "full-access allow").
        if self.full_access {
            self.outgoing.push(json!({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": request_id,
                    "response": {
                        "behavior": "allow",
                        "updatedInput": input,
                    }
                }
            }));
            return Vec::new();
        }

        // (d) Classify per the T3 substring matrix (S2 §1.3).
        let detail = approval_detail(&tool_name, &input);
        let kind = match classify_claude_tool(&tool_name) {
            ClaudeRequestType::FileRead => ApprovalKind::FileRead { detail },
            ClaudeRequestType::ExecCommand => ApprovalKind::ExecCommand {
                command: input
                    .get("command")
                    .or_else(|| input.get("cmd"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                cwd: input.get("cwd").and_then(Value::as_str).map(str::to_string),
                reason,
            },
            ClaudeRequestType::FileChange => ApprovalKind::FileChange {
                changes: file_changes(&tool_name, &input),
                reason,
            },
            ClaudeRequestType::ToolUse => ApprovalKind::ToolUse {
                name: tool_name.clone(),
                input: input.clone(),
                detail,
            },
        };

        let suggestions = request
            .get("permission_suggestions")
            .filter(|v| v.as_array().map(|a| !a.is_empty()).unwrap_or(false))
            .cloned();
        self.pending_approvals
            .insert(request_id.clone(), PendingApproval { input, suggestions });

        vec![AgentEvent::ApprovalRequested(ApprovalRequest {
            id: request_id,
            turn_id: self.current_turn_id.clone(),
            kind,
        })]
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
        let usage = msg.get("usage").map(|u| {
            let mut usage = map_usage(u, msg.get("modelUsage"));
            // Accumulate this turn's processed tokens into the session total.
            self.cumulative_processed += usage.used_tokens.unwrap_or(0);
            usage.total_processed_tokens = Some(self.cumulative_processed);
            usage
        });
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

/// The reduced canonical request type our approval kinds distinguish. T3's
/// item classification has more buckets (collab/mcp/web-search/image) but its
/// request conversion collapses everything except read-only, command, and
/// file-change into the dynamic fallback (S2 §1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeRequestType {
    FileRead,
    ExecCommand,
    FileChange,
    ToolUse,
}

/// Whether a tool name classifies as a collab/subagent item (S2 §1.3 rule 1).
fn is_agent_tool(normalized: &str) -> bool {
    normalized.contains("agent") || normalized == "task"
}

/// Classify a tool name into its canonical approval request type using T3's
/// ordered, substring-based matcher (S2 §1.3). The read-only predicate is
/// checked first (so `WebSearch` → `FileRead` via `"search"`), then the ordered
/// item classification; only command and file-change buckets get a dedicated
/// kind — agent / mcp / web-search / image / default all fall through to the
/// dynamic `ToolUse`.
fn classify_claude_tool(name: &str) -> ClaudeRequestType {
    let n = name.to_lowercase();
    if n == "read"
        || n.contains("read file")
        || n.contains("view")
        || n.contains("grep")
        || n.contains("glob")
        || n.contains("search")
    {
        return ClaudeRequestType::FileRead;
    }
    if is_agent_tool(&n) {
        return ClaudeRequestType::ToolUse;
    }
    if n.contains("bash") || n.contains("command") || n.contains("shell") || n.contains("terminal")
    {
        return ClaudeRequestType::ExecCommand;
    }
    if n.contains("edit")
        || n.contains("write")
        || n.contains("file")
        || n.contains("patch")
        || n.contains("replace")
        || n.contains("create")
        || n.contains("delete")
    {
        return ClaudeRequestType::FileChange;
    }
    // "mcp" / "websearch" / "web search" / "image" all resolve to the dynamic
    // fallback after request conversion.
    ClaudeRequestType::ToolUse
}

/// Construct the approval `detail` string per the S2 §1.3 ordered rules.
fn approval_detail(tool_name: &str, input: &Value) -> String {
    // 1. A command string (`command` or `cmd`).
    if let Some(cmd) = input
        .get("command")
        .or_else(|| input.get("cmd"))
        .and_then(Value::as_str)
    {
        let clipped: String = cmd.trim().chars().take(400).collect();
        return format!("{tool_name}: {clipped}");
    }
    // 2. Collab/subagent item: description, else first 200 chars of prompt,
    //    prefixed with `subagent_type: ` when present.
    if is_agent_tool(&tool_name.to_lowercase()) {
        let body = input
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| {
                input
                    .get("prompt")
                    .and_then(Value::as_str)
                    .map(|p| p.chars().take(200).collect())
                    .unwrap_or_default()
            });
        return match input
            .get("subagent_type")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            Some(subtype) => format!("{subtype}: {body}"),
            None => body,
        };
    }
    // 3. Serialize the full input, clipping to 400 chars with an ellipsis.
    let json = serde_json::to_string(input).unwrap_or_default();
    if json.chars().count() <= 400 {
        format!("{tool_name}: {json}")
    } else {
        let clipped: String = json.chars().take(397).collect();
        format!("{tool_name}: {clipped}...")
    }
}

/// Parse `AskUserQuestion` tool input into canonical [`UserInputQuestion`]s
/// (S2 §1.2). `id` is the complete question text (falling back to `q-<index>`);
/// options and empty labels are preserved (the Claude side does not filter).
fn parse_ask_user_questions(input: &Value) -> Vec<UserInputQuestion> {
    let questions = match input.get("questions").and_then(Value::as_array) {
        Some(q) => q,
        None => return Vec::new(),
    };
    questions
        .iter()
        .enumerate()
        .map(|(index, q)| {
            let question_text = q.get("question").and_then(Value::as_str);
            let id = match question_text {
                Some(t) if !t.is_empty() => t.to_owned(),
                _ => format!("q-{index}"),
            };
            let header = q
                .get("header")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("Question {}", index + 1));
            let question = question_text.unwrap_or("").to_owned();
            let options = q
                .get("options")
                .and_then(Value::as_array)
                .map(|opts| {
                    opts.iter()
                        .map(|opt| UserInputOption {
                            label: opt
                                .get("label")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned(),
                            description: opt
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            let multi_select = q
                .get("multiSelect")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            UserInputQuestion {
                id,
                header,
                question,
                options,
                multi_select,
            }
        })
        .collect()
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
        msg.get("subtype")
            .and_then(Value::as_str)
            .unwrap_or_default()
    )
    .to_lowercase();
    if haystack.contains("interrupt") || haystack.contains("abort") || haystack.contains("cancel") {
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

    let context_window = model_usage.and_then(Value::as_object).and_then(|m| {
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
        // Session-cumulative total is stamped by the caller (`on_result`); the
        // streaming/partial usage path leaves it unset.
        total_processed_tokens: None,
    }
}

/// Parse Claude system-init `slash_commands` (→ [`ProviderCommandKind::Command`])
/// and `skills` (→ [`ProviderCommandKind::Skill`]) into [`ProviderCommand`]s.
/// Both are arrays of bare name strings; the CLI supplies no descriptions.
fn parse_provider_commands(init: &Value) -> Vec<ProviderCommand> {
    let mut out = Vec::new();
    let mut push = |field: &str, kind: ProviderCommandKind| {
        if let Some(names) = init.get(field).and_then(Value::as_array) {
            for name in names.iter().filter_map(Value::as_str) {
                let name = name.trim();
                if !name.is_empty() {
                    out.push(ProviderCommand {
                        name: name.to_owned(),
                        description: None,
                        kind,
                    });
                }
            }
        }
    };
    push("slash_commands", ProviderCommandKind::Command);
    push("skills", ProviderCommandKind::Skill);
    out
}

// ---------------------------------------------------------------------------
// Plan / todo extraction
// ---------------------------------------------------------------------------

fn is_todo_tool(name: &str) -> bool {
    name.to_lowercase().contains("todowrite")
}

/// Map `TodoWrite` input `{ todos: [{ content, status, activeForm? }] }` to
/// plan steps (content → step, fallback `"Task"`; completed/in_progress →
/// Completed/InProgress, else Pending). `activeForm` is ignored.
fn extract_plan_steps_from_todo(input: &Value) -> Option<Vec<PlanStep>> {
    let todos = input.get("todos").and_then(Value::as_array)?;
    if todos.is_empty() {
        return None;
    }
    let steps = todos
        .iter()
        .filter(|todo| todo.is_object())
        .map(|todo| {
            let step = todo
                .get("content")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("Task")
                .to_owned();
            let status = match todo.get("status").and_then(Value::as_str) {
                Some("completed") => PlanStepStatus::Completed,
                Some("in_progress") => PlanStepStatus::InProgress,
                _ => PlanStepStatus::Pending,
            };
            PlanStep { step, status }
        })
        .collect();
    Some(steps)
}

/// Extract the plan markdown from an `ExitPlanMode` tool input (`{ plan }`),
/// trimmed and non-empty.
fn extract_exit_plan_markdown(input: &Value) -> Option<String> {
    input
        .get("plan")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

// ---------------------------------------------------------------------------
// Model catalog + effort mapping
// ---------------------------------------------------------------------------

fn selection_str(selections: &[OptionSelection], id: &str) -> Option<String> {
    selections
        .iter()
        .find(|s| s.id == id)
        .and_then(|s| s.value.as_str().map(str::to_owned))
}

fn selection_bool(selections: &[OptionSelection], id: &str) -> Option<bool> {
    selections
        .iter()
        .find(|s| s.id == id)
        .and_then(|s| s.value.as_bool())
}

fn has_boolean_option(spec: &ModelSpec, id: &str) -> bool {
    spec.options
        .iter()
        .any(|o| matches!(o, OptionDescriptor::Boolean { id: oid, .. } if oid == id))
}

/// Resolve the effort selection against the model's `reasoningEffort`
/// descriptor: an accepted listed value wins, else the descriptor default
/// (T3's `resolveClaudeEffort` / `getProviderOptionDescriptors`). `None` when
/// the model has no reasoning selector (e.g. Haiku).
fn resolve_claude_effort(spec: Option<&ModelSpec>, raw: Option<&str>) -> Option<String> {
    let spec = spec?;
    let (options, default_value) = spec.options.iter().find_map(|o| match o {
        OptionDescriptor::Select {
            id,
            options,
            default_value,
            ..
        } if id == "reasoningEffort" => Some((options, default_value)),
        _ => None,
    })?;
    if let Some(raw) = raw {
        if options.iter().any(|o| o.value == raw) {
            return Some(raw.to_owned());
        }
    }
    default_value.clone()
}

/// T3's `normalizeClaudeCliEffort`: `ultrathink` → no flag (prompt prefix);
/// `ultracode` → `xhigh`; `xhigh` → `max` except Fable 5 / Opus 4.8 / Sonnet 5;
/// Sonnet 4.6 `max` → `high`; otherwise passthrough.
fn normalize_claude_cli_effort(effort: Option<&str>, model: Option<&str>) -> Option<String> {
    let effort = effort?;
    if effort == "ultrathink" {
        return None;
    }
    if effort == "ultracode" {
        return Some("xhigh".to_owned());
    }
    if effort == "xhigh"
        && model != Some("claude-fable-5")
        && model != Some("claude-opus-4-8")
        && model != Some("claude-sonnet-5")
    {
        return Some("max".to_owned());
    }
    if effort == "max" && model == Some("claude-sonnet-4-6") {
        return Some("high".to_owned());
    }
    Some(effort.to_owned())
}

fn effort_option(value: &str) -> SelectOption {
    let label = match value {
        "low" => "Low",
        "medium" => "Medium",
        "high" => "High",
        "xhigh" => "Extra High",
        "max" => "Max",
        "ultracode" => "Ultracode",
        "ultrathink" => "Ultrathink",
        other => other,
    };
    SelectOption {
        value: value.to_owned(),
        label: label.to_owned(),
        description: None,
    }
}

fn reasoning(values: &[&str], default: &str) -> OptionDescriptor {
    OptionDescriptor::Select {
        id: "reasoningEffort".to_owned(),
        label: "Reasoning".to_owned(),
        options: values.iter().map(|v| effort_option(v)).collect(),
        default_value: Some(default.to_owned()),
    }
}

fn context_window() -> OptionDescriptor {
    OptionDescriptor::Select {
        id: "contextWindow".to_owned(),
        label: "Context Window".to_owned(),
        options: vec![
            SelectOption {
                value: "200k".to_owned(),
                label: "200k".to_owned(),
                description: None,
            },
            SelectOption {
                value: "1m".to_owned(),
                label: "1M".to_owned(),
                description: None,
            },
        ],
        default_value: Some("200k".to_owned()),
    }
}

fn boolean(id: &str, label: &str) -> OptionDescriptor {
    OptionDescriptor::Boolean {
        id: id.to_owned(),
        label: label.to_owned(),
        default_value: false,
    }
}

fn model(id: &str, display_name: &str, options: Vec<OptionDescriptor>) -> ModelSpec {
    ModelSpec {
        id: id.to_owned(),
        display_name: display_name.to_owned(),
        is_default: false,
        options,
    }
}

/// The full static Claude catalog (unfiltered by version). Mirrors T3's
/// `BUILT_IN_MODELS` (S1 §2).
fn built_in_models() -> Vec<ModelSpec> {
    vec![
        model(
            "claude-fable-5",
            "Claude Fable 5",
            vec![
                reasoning(
                    &[
                        "low",
                        "medium",
                        "high",
                        "xhigh",
                        "max",
                        "ultracode",
                        "ultrathink",
                    ],
                    "high",
                ),
                context_window(),
            ],
        ),
        model(
            "claude-opus-4-8",
            "Claude Opus 4.8",
            vec![
                reasoning(
                    &[
                        "low",
                        "medium",
                        "high",
                        "xhigh",
                        "max",
                        "ultracode",
                        "ultrathink",
                    ],
                    "high",
                ),
                boolean("fastMode", "Fast Mode"),
            ],
        ),
        model(
            "claude-opus-4-7",
            "Claude Opus 4.7",
            vec![
                reasoning(
                    &["low", "medium", "high", "xhigh", "max", "ultrathink"],
                    "xhigh",
                ),
                boolean("fastMode", "Fast Mode"),
            ],
        ),
        model(
            "claude-opus-4-6",
            "Claude Opus 4.6",
            vec![
                reasoning(&["low", "medium", "high", "max", "ultrathink"], "high"),
                boolean("fastMode", "Fast Mode"),
                context_window(),
            ],
        ),
        model(
            "claude-opus-4-5",
            "Claude Opus 4.5",
            vec![
                reasoning(&["low", "medium", "high", "max"], "high"),
                boolean("fastMode", "Fast Mode"),
            ],
        ),
        model(
            "claude-sonnet-5",
            "Claude Sonnet 5",
            vec![
                reasoning(
                    &["low", "medium", "high", "xhigh", "max", "ultrathink"],
                    "high",
                ),
                context_window(),
            ],
        ),
        model(
            "claude-sonnet-4-6",
            "Claude Sonnet 4.6",
            vec![
                reasoning(&["low", "medium", "high", "max", "ultrathink"], "high"),
                context_window(),
            ],
        ),
        model(
            "claude-haiku-4-5",
            "Claude Haiku 4.5",
            vec![boolean("thinking", "Thinking")],
        ),
    ]
}

/// Capabilities for one model id (from the unfiltered catalog).
fn model_spec(id: &str) -> Option<ModelSpec> {
    let id = id.trim();
    built_in_models().into_iter().find(|m| m.id == id)
}

/// Whether a version-gated model is available at the installed Claude version.
fn model_available(id: &str, version: Option<(u32, u32, u32)>) -> bool {
    match id {
        "claude-fable-5" => version_ge(version, (2, 1, 169)),
        "claude-opus-4-8" => version_ge(version, (2, 1, 154)),
        "claude-opus-4-7" => version_ge(version, (2, 1, 111)),
        _ => true,
    }
}

fn version_ge(version: Option<(u32, u32, u32)>, min: (u32, u32, u32)) -> bool {
    version.map(|v| v >= min).unwrap_or(false)
}

/// Parse a `MAJOR.MINOR.PATCH` triple from `claude --version` output
/// (e.g. `"2.1.206 (Claude Code)"`).
fn parse_semver(text: &str) -> Option<(u32, u32, u32)> {
    let token = text.split_whitespace().next()?;
    let mut parts = token.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts
        .next()
        .and_then(|p| p.split('-').next())
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);
    Some((major, minor, patch))
}

/// Run `claude --version` and parse the semver triple; `None` on any failure.
async fn claude_version(binary: Option<&Path>, launch_env: &LaunchEnv) -> Option<(u32, u32, u32)> {
    // Resolve through the PATH search (PATHEXT-aware: on Windows the CLI only
    // exists as `claude.cmd`), falling back to the bare name so the failure is
    // reported by the OS exactly as before.
    let bin = crate::resolve_binary(binary, "claude")
        .unwrap_or_else(|_| std::path::PathBuf::from("claude"));
    let mut cmd = crate::process::async_command(&bin);
    cmd.arg("--version")
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT");
    for (key, value) in launch_env.pairs(ProviderKind::ClaudeCode) {
        cmd.env(key, value);
    }
    let output = cmd.output().await.ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_semver(&stdout)
}

/// List Claude's models: the static catalog, gated by the installed CLI version.
pub async fn list_models(
    binary_path: Option<PathBuf>,
    launch_env: LaunchEnv,
) -> Result<Vec<ModelSpec>, AgentError> {
    let version = claude_version(binary_path.as_deref(), &launch_env).await;
    Ok(built_in_models()
        .into_iter()
        .filter(|m| model_available(&m.id, version))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(mapper: &mut Mapper, line: &str) -> Vec<AgentEvent> {
        let msg: Value = serde_json::from_str(line).expect("valid json fixture line");
        mapper.on_message(msg)
    }

    #[test]
    fn effort_compat_transforms() {
        // ultrathink → no flag (prompt-prefix mode)
        assert_eq!(
            normalize_claude_cli_effort(Some("ultrathink"), Some("claude-opus-4-8")),
            None
        );
        // ultracode → xhigh
        assert_eq!(
            normalize_claude_cli_effort(Some("ultracode"), Some("claude-opus-4-8")).as_deref(),
            Some("xhigh")
        );
        // xhigh → max EXCEPT on fable-5 / opus-4-8 / sonnet-5
        assert_eq!(
            normalize_claude_cli_effort(Some("xhigh"), Some("claude-opus-4-7")).as_deref(),
            Some("max")
        );
        assert_eq!(
            normalize_claude_cli_effort(Some("xhigh"), Some("claude-fable-5")).as_deref(),
            Some("xhigh")
        );
        assert_eq!(
            normalize_claude_cli_effort(Some("xhigh"), Some("claude-opus-4-8")).as_deref(),
            Some("xhigh")
        );
        assert_eq!(
            normalize_claude_cli_effort(Some("xhigh"), Some("claude-sonnet-5")).as_deref(),
            Some("xhigh")
        );
        // sonnet-4-6 max → high
        assert_eq!(
            normalize_claude_cli_effort(Some("max"), Some("claude-sonnet-4-6")).as_deref(),
            Some("high")
        );
        // passthrough
        assert_eq!(
            normalize_claude_cli_effort(Some("low"), Some("claude-opus-4-6")).as_deref(),
            Some("low")
        );
    }

    #[test]
    fn resolve_effort_uses_listed_value_or_default() {
        let fable = model_spec("claude-fable-5");
        // Listed value wins.
        assert_eq!(
            resolve_claude_effort(fable.as_ref(), Some("max")).as_deref(),
            Some("max")
        );
        // Unknown value falls back to the descriptor default (high).
        assert_eq!(
            resolve_claude_effort(fable.as_ref(), Some("bogus")).as_deref(),
            Some("high")
        );
        // No selection → default.
        assert_eq!(
            resolve_claude_effort(fable.as_ref(), None).as_deref(),
            Some("high")
        );
        // Haiku has no reasoning selector.
        let haiku = model_spec("claude-haiku-4-5");
        assert_eq!(resolve_claude_effort(haiku.as_ref(), Some("low")), None);
    }

    #[test]
    fn version_gating_filters_new_models() {
        let ids = |version: Option<(u32, u32, u32)>| -> Vec<String> {
            built_in_models()
                .into_iter()
                .filter(|m| model_available(&m.id, version))
                .map(|m| m.id)
                .collect()
        };
        // Current version exposes everything.
        assert!(ids(Some((2, 1, 206))).contains(&"claude-fable-5".to_string()));
        // Below every gate: fable-5 / opus-4-8 / opus-4-7 hidden, rest visible.
        let old = ids(Some((2, 1, 100)));
        assert!(!old.contains(&"claude-fable-5".to_string()));
        assert!(!old.contains(&"claude-opus-4-8".to_string()));
        assert!(!old.contains(&"claude-opus-4-7".to_string()));
        assert!(old.contains(&"claude-opus-4-6".to_string()));
        assert!(old.contains(&"claude-haiku-4-5".to_string()));
        // Exact boundary is inclusive.
        assert!(ids(Some((2, 1, 154))).contains(&"claude-opus-4-8".to_string()));
        assert!(!ids(Some((2, 1, 153))).contains(&"claude-opus-4-8".to_string()));
        // Unknown version hides gated models.
        assert!(!ids(None).contains(&"claude-fable-5".to_string()));
    }

    #[test]
    fn parse_semver_from_version_output() {
        assert_eq!(parse_semver("2.1.206 (Claude Code)"), Some((2, 1, 206)));
        assert_eq!(parse_semver("2.1.169"), Some((2, 1, 169)));
        assert_eq!(parse_semver("nonsense"), None);
    }

    #[test]
    fn launch_options_resolve_effort_context_and_settings() {
        // 1M context suffix + ultracode → effort xhigh + settings.ultracode.
        let launch = ClaudeLaunchOptions::resolve(
            Some("claude-opus-4-8"),
            &[
                OptionSelection {
                    id: "reasoningEffort".into(),
                    value: json!("ultracode"),
                },
                OptionSelection {
                    id: "fastMode".into(),
                    value: json!(true),
                },
            ],
        );
        assert_eq!(launch.model_id.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(launch.effort.as_deref(), Some("xhigh"));
        assert!(!launch.ultrathink);
        let settings: Value =
            serde_json::from_str(launch.settings_json.as_deref().unwrap()).unwrap();
        assert_eq!(settings["ultracode"], true);
        assert_eq!(settings["fastMode"], true);

        // ultrathink → no --effort, prompt-prefix flag set.
        let launch = ClaudeLaunchOptions::resolve(
            Some("claude-fable-5"),
            &[
                OptionSelection {
                    id: "reasoningEffort".into(),
                    value: json!("ultrathink"),
                },
                OptionSelection {
                    id: "contextWindow".into(),
                    value: json!("1m"),
                },
            ],
        );
        assert_eq!(launch.model_id.as_deref(), Some("claude-fable-5[1m]"));
        assert_eq!(launch.effort, None);
        assert!(launch.ultrathink);
        assert!(launch.settings_json.is_none());

        // Haiku thinking → settings.alwaysThinkingEnabled.
        let launch = ClaudeLaunchOptions::resolve(
            Some("claude-haiku-4-5"),
            &[OptionSelection {
                id: "thinking".into(),
                value: json!(true),
            }],
        );
        let settings: Value =
            serde_json::from_str(launch.settings_json.as_deref().unwrap()).unwrap();
        assert_eq!(settings["alwaysThinkingEnabled"], true);
    }

    #[test]
    fn todo_write_maps_to_plan_updated() {
        let mut m = Mapper::new();
        m.start_turn();
        let evs = feed(
            &mut m,
            r#"{"type":"assistant","message":{"id":"msg_t","content":[{"type":"tool_use","id":"toolu_todo","name":"TodoWrite","input":{"todos":[{"content":"Build board","status":"completed","activeForm":"Building board"},{"content":"","status":"in_progress"},{"content":"Ship","status":"todo"}]}}]}}"#,
        );
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            AgentEvent::PlanUpdated { turn_id, steps, .. } => {
                assert_eq!(turn_id.as_deref(), Some("turn-1"));
                assert_eq!(steps.len(), 3);
                assert_eq!(steps[0].step, "Build board");
                assert_eq!(steps[0].status, PlanStepStatus::Completed);
                assert_eq!(steps[1].step, "Task"); // empty content fallback
                assert_eq!(steps[1].status, PlanStepStatus::InProgress);
                assert_eq!(steps[2].status, PlanStepStatus::Pending);
            }
            other => panic!("expected PlanUpdated, got {other:?}"),
        }
    }

    #[test]
    fn exit_plan_mode_captures_and_denies() {
        let mut m = Mapper::new();
        m.start_turn();
        // Permission-callback path: capture ProposedPlan + queue auto-deny.
        let evs = feed(
            &mut m,
            r##"{"type":"control_request","request_id":"req-plan","request":{"subtype":"can_use_tool","tool_name":"ExitPlanMode","input":{"plan":"# Plan\n- step one"}}}"##,
        );
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            AgentEvent::ProposedPlan { item_id, markdown } => {
                assert_eq!(item_id, "req-plan");
                assert_eq!(markdown, "# Plan\n- step one");
            }
            other => panic!("expected ProposedPlan, got {other:?}"),
        }
        let outgoing = m.take_outgoing();
        assert_eq!(outgoing.len(), 1);
        assert_eq!(outgoing[0]["response"]["subtype"], "success");
        assert_eq!(outgoing[0]["response"]["request_id"], "req-plan");
        assert_eq!(outgoing[0]["response"]["response"]["behavior"], "deny");
        assert_eq!(
            outgoing[0]["response"]["response"]["message"],
            EXIT_PLAN_DENY_MESSAGE
        );

        // Assistant-block path with the SAME tool id is deduped (no second event).
        let evs = feed(
            &mut m,
            r##"{"type":"assistant","message":{"id":"msg_p","content":[{"type":"tool_use","id":"req-plan","name":"ExitPlanMode","input":{"plan":"# Plan\n- step one"}}]}}"##,
        );
        assert!(evs.is_empty(), "duplicate capture should be suppressed");
    }

    #[test]
    fn user_message_carries_image_content_blocks() {
        let attachments = vec![
            Attachment {
                media_type: "image/png".into(),
                data_base64: "AAAA".into(),
            },
            Attachment {
                media_type: "image/jpeg".into(),
                data_base64: "BBBB".into(),
            },
        ];
        let msg = user_message("what color is this?", &attachments);
        let content = msg["message"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "what color is this?");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert_eq!(content[1]["source"]["media_type"], "image/png");
        assert_eq!(content[1]["source"]["data"], "AAAA");
        assert_eq!(content[2]["source"]["media_type"], "image/jpeg");
        // Text-only stays a single text block.
        let plain = user_message("hi", &[]);
        assert_eq!(plain["message"]["content"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn init_parses_slash_commands_and_skills() {
        let mut m = Mapper::new();
        let evs = feed(
            &mut m,
            r#"{"type":"system","subtype":"init","session_id":"s1","model":"claude-opus-4-8","slash_commands":["plan","review",""],"skills":["dataviz"]}"#,
        );
        // SessionStarted then ProviderCommands.
        assert!(matches!(evs[0], AgentEvent::SessionStarted { .. }));
        match &evs[1] {
            AgentEvent::ProviderCommands { commands } => {
                // Empty names dropped; two commands + one skill.
                assert_eq!(commands.len(), 3);
                assert_eq!(commands[0].name, "plan");
                assert_eq!(commands[0].kind, ProviderCommandKind::Command);
                assert_eq!(commands[2].name, "dataviz");
                assert_eq!(commands[2].kind, ProviderCommandKind::Skill);
            }
            other => panic!("expected ProviderCommands, got {other:?}"),
        }
    }

    #[test]
    fn compact_boundary_maps_to_context_compacted() {
        let mut m = Mapper::new();
        let evs = feed(
            &mut m,
            r#"{"type":"system","subtype":"compact_boundary","session_id":"s1","compact_metadata":{"trigger":"manual","pre_tokens":500,"post_tokens":10}}"#,
        );
        assert!(matches!(evs.as_slice(), [AgentEvent::ContextCompacted]));
    }

    #[test]
    fn result_accumulates_total_processed_tokens() {
        let mut m = Mapper::new();
        m.start_turn();
        let evs = feed(
            &mut m,
            r#"{"type":"result","subtype":"success","usage":{"input_tokens":100,"output_tokens":20}}"#,
        );
        let first = match &evs[0] {
            AgentEvent::TurnCompleted { usage, .. } => usage.unwrap(),
            other => panic!("expected TurnCompleted, got {other:?}"),
        };
        assert_eq!(first.total_processed_tokens, Some(120));
        // A second turn accumulates on top of the first.
        m.start_turn();
        let evs = feed(
            &mut m,
            r#"{"type":"result","subtype":"success","usage":{"input_tokens":30,"output_tokens":5}}"#,
        );
        let second = match &evs[0] {
            AgentEvent::TurnCompleted { usage, .. } => usage.unwrap(),
            other => panic!("expected TurnCompleted, got {other:?}"),
        };
        assert_eq!(second.total_processed_tokens, Some(155));
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
    fn deny_cancel_and_session_approval_wire_strings() {
        // Deny → T3's exact "declined" message.
        let mut m = Mapper::new();
        feed(
            &mut m,
            r#"{"type":"control_request","request_id":"req-d","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"rm -rf /"}}}"#,
        );
        let deny = m
            .build_approval_response("req-d", ApprovalDecision::Deny)
            .unwrap();
        assert_eq!(deny["response"]["response"]["behavior"], "deny");
        assert_eq!(
            deny["response"]["response"]["message"],
            "User declined tool execution."
        );

        // Cancel → deny with the exact "cancelled" message (no interrupt).
        let mut mc = Mapper::new();
        feed(
            &mut mc,
            r#"{"type":"control_request","request_id":"req-c","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"}}}"#,
        );
        let cancel = mc
            .build_approval_response("req-c", ApprovalDecision::Cancel)
            .unwrap();
        assert_eq!(cancel["response"]["response"]["behavior"], "deny");
        assert_eq!(
            cancel["response"]["response"]["message"],
            "User cancelled tool execution."
        );

        // ApproveForSession with NO permission_suggestions → plain allow.
        let mut m2 = Mapper::new();
        feed(
            &mut m2,
            r#"{"type":"control_request","request_id":"req-s","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"}}}"#,
        );
        let sess = m2
            .build_approval_response("req-s", ApprovalDecision::ApproveForSession)
            .unwrap();
        assert_eq!(sess["response"]["response"]["behavior"], "allow");
        assert!(
            sess["response"]["response"]
                .get("updatedPermissions")
                .is_none()
        );

        // ApproveForSession WITH suggestions → forwarded verbatim.
        let mut m3 = Mapper::new();
        feed(
            &mut m3,
            r#"{"type":"control_request","request_id":"req-p","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},"permission_suggestions":[{"type":"setMode","mode":"acceptEdits","destination":"session"}]}}"#,
        );
        let sess = m3
            .build_approval_response("req-p", ApprovalDecision::ApproveForSession)
            .unwrap();
        assert_eq!(
            sess["response"]["response"]["updatedPermissions"][0]["type"],
            "setMode"
        );
        assert_eq!(
            sess["response"]["response"]["updatedPermissions"][0]["mode"],
            "acceptEdits"
        );
    }

    #[test]
    fn classification_matrix_covers_t3_substring_quirks() {
        use ClaudeRequestType::*;
        let cases = [
            ("Read", FileRead),       // exact lowercase "read"
            ("Read File", FileRead),  // "read file" substring
            ("ReadFile", FileChange), // no space → "file" classifies it as file_change
            ("View", FileRead),
            ("ViewImage", FileRead), // "view" wins before "image"
            ("Grep", FileRead),
            ("Glob", FileRead),
            ("WebSearch", FileRead), // "search" predicate wins over web_search
            ("codebase_search", FileRead),
            ("WebFetch", ToolUse), // neither search nor read-only recognizes it
            ("Bash", ExecCommand),
            ("run_shell", ExecCommand),
            ("terminal", ExecCommand),
            ("some_command", ExecCommand),
            ("Edit", FileChange),
            ("Write", FileChange),
            ("MultiEdit", FileChange),
            ("delete_thing", FileChange),
            ("TodoWrite", FileChange),  // "write"
            ("TaskCreate", FileChange), // "create"
            ("TaskUpdate", ToolUse),    // no classification substring
            ("TaskList", ToolUse),
            ("Task", ToolUse), // agent item, falls through
            ("some_agent", ToolUse),
            ("subagent_run", ToolUse),
            ("mcp__server__tool", ToolUse),
            ("view_image", FileRead), // "view" still wins
            ("image_tool", ToolUse),  // image → dynamic
            ("MysteryTool", ToolUse),
        ];
        for (name, expected) in cases {
            assert_eq!(classify_claude_tool(name), expected, "classifying {name:?}");
        }
    }

    #[test]
    fn approval_detail_construction_rules() {
        // 1. command → "<tool>: <trimmed, first 400 chars>".
        let d = approval_detail("Bash", &json!({ "command": "  echo hi  " }));
        assert_eq!(d, "Bash: echo hi");
        let long = "x".repeat(500);
        let d = approval_detail("Bash", &json!({ "cmd": long }));
        assert_eq!(d, format!("Bash: {}", "x".repeat(400)));

        // 2. subagent item: description preferred, prefixed with subagent_type.
        let d = approval_detail(
            "Task",
            &json!({ "subagent_type": "explore", "description": "find refs", "prompt": "ignored" }),
        );
        assert_eq!(d, "explore: find refs");
        // prompt fallback (first 200 chars), no subagent_type prefix.
        let d = approval_detail("Task", &json!({ "prompt": "y".repeat(300) }));
        assert_eq!(d, "y".repeat(200));

        // 3. otherwise serialize input; ≤400 keeps full JSON.
        let d = approval_detail("Weird", &json!({ "a": 1 }));
        assert_eq!(d, "Weird: {\"a\":1}");
        // >400 → first 397 chars + "..."
        let big = json!({ "blob": "z".repeat(500) });
        let d = approval_detail("Weird", &big);
        assert!(d.starts_with("Weird: "));
        assert!(d.ends_with("..."));
    }

    #[test]
    fn ask_user_question_parse_and_answer_wire_shape() {
        let mut m = Mapper::new();
        m.start_turn();
        let evs = feed(
            &mut m,
            r#"{"type":"control_request","request_id":"ctrl-9","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[{"question":"Which color?","header":"Color","options":[{"label":"Red","description":"warm"},{"label":"Blue","description":""}],"multiSelect":false},{"header":"Free"}]}}}"#,
        );
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            AgentEvent::UserInputRequested {
                request_id,
                questions,
            } => {
                assert_eq!(request_id, "ctrl-9");
                assert_eq!(questions.len(), 2);
                // id = complete question text.
                assert_eq!(questions[0].id, "Which color?");
                assert_eq!(questions[0].header, "Color");
                assert_eq!(questions[0].options.len(), 2);
                assert_eq!(questions[0].options[0].label, "Red");
                assert_eq!(questions[0].options[1].description, "");
                assert!(!questions[0].multi_select);
                // Missing question text → id fallback q-<index>, header kept.
                assert_eq!(questions[1].id, "q-1");
                assert_eq!(questions[1].header, "Free");
                assert!(questions[1].options.is_empty());
            }
            other => panic!("expected UserInputRequested, got {other:?}"),
        }

        // Answer: allow with {questions: <original>, answers: <provided>}.
        let mut answers = serde_json::Map::new();
        answers.insert("Which color?".into(), json!("Red"));
        let resp = m
            .build_user_input_response("ctrl-9", &answers)
            .expect("response for known request");
        assert_eq!(resp["response"]["subtype"], "success");
        assert_eq!(resp["response"]["request_id"], "ctrl-9");
        assert_eq!(resp["response"]["response"]["behavior"], "allow");
        assert_eq!(
            resp["response"]["response"]["updatedInput"]["answers"]["Which color?"],
            "Red"
        );
        // Original questions echoed back verbatim.
        assert_eq!(
            resp["response"]["response"]["updatedInput"]["questions"][0]["header"],
            "Color"
        );
        // Consumed once.
        assert!(m.build_user_input_response("ctrl-9", &answers).is_none());
    }

    #[test]
    fn ask_user_question_cancel_on_teardown() {
        let mut m = Mapper::new();
        feed(
            &mut m,
            r#"{"type":"control_request","request_id":"ctrl-x","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[{"question":"q?","header":"h"}]}}}"#,
        );
        let cancels = m.cancel_pending_user_input();
        assert_eq!(cancels.len(), 1);
        let (id, resp) = &cancels[0];
        assert_eq!(id, "ctrl-x");
        assert_eq!(resp["response"]["response"]["behavior"], "deny");
        assert_eq!(
            resp["response"]["response"]["message"],
            "User cancelled tool execution."
        );
        // Drained: no longer answerable.
        let empty = serde_json::Map::new();
        assert!(m.build_user_input_response("ctrl-x", &empty).is_none());
    }

    #[test]
    fn full_access_auto_allows_without_event() {
        let mut m = Mapper::new();
        m.full_access = true;
        let evs = feed(
            &mut m,
            r#"{"type":"control_request","request_id":"req-fa","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"}}}"#,
        );
        assert!(evs.is_empty(), "full-access emits no approval event");
        let outgoing = m.take_outgoing();
        assert_eq!(outgoing.len(), 1);
        assert_eq!(outgoing[0]["response"]["request_id"], "req-fa");
        assert_eq!(outgoing[0]["response"]["response"]["behavior"], "allow");
        assert_eq!(
            outgoing[0]["response"]["response"]["updatedInput"]["command"],
            "ls"
        );
        // AskUserQuestion still surfaces even in full-access.
        let evs = feed(
            &mut m,
            r#"{"type":"control_request","request_id":"req-q","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[{"question":"q?","header":"h"}]}}}"#,
        );
        assert!(matches!(evs[0], AgentEvent::UserInputRequested { .. }));
    }

    #[test]
    fn read_tool_maps_to_file_read_kind() {
        let mut m = Mapper::new();
        let evs = feed(
            &mut m,
            r#"{"type":"control_request","request_id":"req-r","request":{"subtype":"can_use_tool","tool_name":"Read","input":{"file_path":"/tmp/a.txt"}}}"#,
        );
        match &evs[0] {
            AgentEvent::ApprovalRequested(req) => match &req.kind {
                ApprovalKind::FileRead { detail } => {
                    assert!(detail.starts_with("Read: "), "detail was {detail:?}")
                }
                other => panic!("expected FileRead, got {other:?}"),
            },
            other => panic!("expected ApprovalRequested, got {other:?}"),
        }
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
    fn permission_mode_flag_maps_all_modes() {
        assert_eq!(permission_mode_flag(ApprovalMode::Supervised), "default");
        assert_eq!(
            permission_mode_flag(ApprovalMode::AutoAcceptEdits),
            "acceptEdits"
        );
        assert_eq!(
            permission_mode_flag(ApprovalMode::FullAccess),
            "bypassPermissions"
        );
    }

    #[test]
    fn set_permission_mode_request_shape() {
        let mut m = Mapper::new();
        let req = m.set_permission_mode_request(ApprovalMode::AutoAcceptEdits);
        assert_eq!(req["type"], "control_request");
        assert!(req["request_id"].is_string());
        assert_eq!(req["request"]["subtype"], "set_permission_mode");
        assert_eq!(req["request"]["mode"], "acceptEdits");

        // FullAccess maps to bypassPermissions on the wire.
        let req = m.set_permission_mode_request(ApprovalMode::FullAccess);
        assert_eq!(req["request"]["mode"], "bypassPermissions");
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
