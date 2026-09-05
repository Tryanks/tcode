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

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use smol::io::{AsyncWrite, BufReader};
use smol::prelude::*;
use smol::process::Stdio;

use crate::pending::{PendingRequests, drain_resolved};
use crate::{
    AgentError, AgentEvent, ApprovalDecision, ApprovalKind, ApprovalMode, ApprovalRequest,
    Attachment, ClassifierCategory, DeltaKind, FileChange, FileChangeKind, InteractionMode,
    ItemContent, ItemStatus, LaunchEnv, ModelSpec, OptionDescriptor, OptionSelection, PlanStep,
    PlanStepStatus, ProviderCommand, ProviderCommandKind, ProviderKind, ResumeCursor, RewindMode,
    SelectOption, SessionCommand, SessionHandle, SessionOptions, ThreadItem, TokenUsage,
    TurnStatus, UserInputOption, UserInputQuestion, selection_bool, selection_str,
};

/// T3's exact message denied to `ExitPlanMode` once the plan is captured.
const EXIT_PLAN_DENY_MESSAGE: &str = "The client captured your proposed plan. Stop here and wait for the user's feedback or implementation request in a later turn.";

/// First Claude Code build whose headless control protocol is verified to
/// expose `rewind_conversation` alongside the SDK's `rewind_files` request.
const NATIVE_REWIND_MIN_VERSION: (u32, u32, u32) = (2, 1, 214);

/// Map a canonical [`ApprovalMode`] onto the value Claude's CLI expects for
/// `--permission-mode` (and the `set_permission_mode` control request).
///
/// Verified against `@anthropic-ai/claude-agent-sdk` v0.3.170
/// `SDKControlSetPermissionModeRequest` (`sdk.d.ts`): `'default'` prompts for
/// dangerous operations, `'acceptEdits'` auto-accepts file edits, and
/// `'bypassPermissions'` skips all permission checks. ReadOnly also launches in
/// default mode; tcode enforces its narrower policy in [`Mapper`].
pub(crate) fn permission_mode_flag(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::Supervised | ApprovalMode::ReadOnly => "default",
        ApprovalMode::AutoAcceptEdits => "acceptEdits",
        ApprovalMode::FullAccess => "bypassPermissions",
    }
}

/// Permission mode placed on the internal launch argv. Persisted Plan sessions
/// must enter the CLI's plan sandbox before their first message, without
/// depending on an unchecked live control request.
fn initial_permission_mode(
    approval_mode: ApprovalMode,
    interaction_mode: InteractionMode,
) -> &'static str {
    match interaction_mode {
        InteractionMode::Plan => "plan",
        InteractionMode::Build => permission_mode_flag(approval_mode),
    }
}

/// Resolve the last permission-mode value on the effective argv. Provider
/// launch arguments intentionally follow internal arguments and may use either
/// CLI spelling, so the mode tracker must honor both.
fn effective_permission_mode(internal: &str, extra_args: &[String]) -> String {
    let mut effective = internal.to_owned();
    let mut args = extra_args.iter();
    while let Some(arg) = args.next() {
        if arg == "--permission-mode" {
            if let Some(value) = args.next() {
                effective.clone_from(value);
            }
        } else if let Some(value) = arg.strip_prefix("--permission-mode=") {
            effective = value.to_owned();
        }
    }
    effective
}

/// Start (or resume) a Claude Code session.
pub async fn start(opts: SessionOptions) -> Result<SessionHandle, AgentError> {
    let native_rewind = version_ge(
        claude_version(opts.binary_path.as_deref(), &opts.launch_env).await,
        NATIVE_REWIND_MIN_VERSION,
    );
    // Absolute path: a bare name would be resolved against the session cwd we
    // set below, which breaks PATH lookup (see `resolve_binary`).
    let binary = crate::resolve_binary(opts.binary_path.as_deref(), "claude")?;
    let binary = binary.to_string_lossy().into_owned();

    // Resolve model-scoped launch options from the persisted selections
    // (effort/context/fast/thinking are launch-time only; mid-session changes
    // ride the resume-restart machinery).
    let launch = ClaudeLaunchOptions::resolve(opts.model.as_deref(), &opts.option_selections);
    let base_permission_mode = permission_mode_flag(opts.approval_mode);
    let launch_permission_mode = initial_permission_mode(opts.approval_mode, opts.interaction_mode);
    // Launch arguments are deliberately appended last, so the tracker must
    // follow their effective value rather than the internal flag they replace.
    let applied_permission_mode =
        effective_permission_mode(launch_permission_mode, &opts.extra_args);

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
        .arg(launch_permission_mode);

    if native_rewind {
        // User-message UUIDs are Claude's native checkpoint ids. SDK file
        // checkpointing is opt-in for print/stream-json hosts even though the
        // interactive CLI enables it automatically.
        cmd.arg("--replay-user-messages")
            .env("CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING", "true");
    }

    if let Some(model) = &launch.model_id {
        cmd.arg("--model").arg(model);
    }
    if let Some(effort) = &launch.effort {
        cmd.arg("--effort").arg(effort);
    }
    if let Some(settings) = &launch.settings_json {
        cmd.arg("--settings").arg(settings);
    }
    for arg in resume_args(&opts.resume, opts.fork) {
        cmd.arg(arg);
    }
    // Register tcode's enabled HTTP MCP servers. Tokens ride in Authorization
    // headers inside the merged `--mcp-config` JSON.
    for arg in mcp_args(&opts.mcp_servers) {
        cmd.arg(arg);
    }
    // Settings → Providers "Launch arguments", appended last so the user can
    // override anything we set above.
    for arg in &opts.extra_args {
        cmd.arg(arg);
    }
    log::debug!(
        "claude spawn args: model={:?} effort={:?} settings={:?} ultrathink={} permission-mode={} effective-permission-mode={}",
        launch.model_id,
        launch.effort,
        launch.settings_json,
        launch.ultrathink,
        launch_permission_mode,
        applied_permission_mode,
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

    let (cmd_tx, cmd_rx) = smol::channel::unbounded::<SessionCommand>();
    let (event_tx, event_rx) = smol::channel::unbounded::<AgentEvent>();

    // Reader task: forward each stdout line (an item = one JSON message) into an
    // internal channel; closing the channel signals stdout EOF.
    let (line_tx, line_rx) = smol::channel::unbounded::<String>();
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

    // Stderr drain: never protocol, but the tail is kept so an unexpected exit
    // can be reported in the CLI's own words (crash stacks land here).
    let stderr_tail = crate::process::StderrTail::default();
    let stderr_task = smol::spawn({
        let tail = stderr_tail.clone();
        async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Some(Ok(line)) = lines.next().await {
                if line.trim().is_empty() {
                    continue;
                }
                log::warn!("claude[stderr]: {line}");
                tail.push(line);
            }
        }
    });

    let session_config = SessionConfig {
        claude_dir: opts
            .launch_env
            .home
            .clone()
            .or_else(|| {
                opts.launch_env
                    .env
                    .iter()
                    .rev()
                    .find(|(key, _)| key == "HOME")
                    .map(|(_, value)| PathBuf::from(value))
            })
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .map(|home| home.join(".claude")),
    };
    smol::spawn(actor_loop(
        child,
        stdin,
        cmd_rx,
        line_rx,
        event_tx,
        session_config,
        launch.ultrathink,
        opts.interaction_mode,
        base_permission_mode,
        applied_permission_mode,
        opts.approval_mode,
        native_rewind,
        opts.model.clone(),
        stderr_tail,
        stderr_task,
    ))
    .detach();

    Ok(SessionHandle {
        provider: ProviderKind::ClaudeCode,
        commands: cmd_tx,
        events: event_rx,
    })
}

/// Ask a short-lived Claude Code process for its subscription usage.
pub async fn read_usage(
    binary_path: Option<PathBuf>,
    launch_env: LaunchEnv,
) -> Result<Value, AgentError> {
    let binary = crate::resolve_binary(binary_path.as_deref(), "claude")?;
    let binary_display = binary.to_string_lossy().into_owned();
    let mut cmd = crate::process::async_command(&binary);
    cmd.arg("--print")
        .arg("--input-format")
        .arg("stream-json")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--verbose")
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT");
    for (key, value) in launch_env.pairs(ProviderKind::ClaudeCode) {
        cmd.env(key, value);
    }

    let mut child = cmd
        .spawn()
        .map_err(|err| AgentError::Spawn(format!("spawning `{binary_display}`: {err}")))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| AgentError::Spawn("child stdin missing".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AgentError::Spawn("child stdout missing".into()))?;

    write_line(
        &mut stdin,
        &json!({
            "type": "control_request",
            "request_id": "usage-1",
            "request": { "subtype": "get_usage" }
        }),
    )
    .await?;

    let read_response = async {
        let mut lines = BufReader::new(stdout).lines();
        while let Some(line) = lines.next().await {
            let line = line.map_err(AgentError::Io)?;
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message.get("type").and_then(Value::as_str) != Some("control_response") {
                continue;
            }
            let Some(response) = message.get("response") else {
                continue;
            };
            if response.get("request_id").and_then(Value::as_str) != Some("usage-1") {
                continue;
            }
            if response.get("subtype").and_then(Value::as_str) == Some("error") {
                let error = response
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| response.get("error").unwrap_or(response).to_string());
                return Err(AgentError::Provider(error));
            }
            return response.get("response").cloned().ok_or_else(|| {
                AgentError::Protocol("Claude usage response omitted response".into())
            });
        }
        Err(AgentError::Protocol(
            "Claude stdout closed before usage response".into(),
        ))
    };

    let result = smol::future::or(async { Some(read_response.await) }, async {
        smol::Timer::after(std::time::Duration::from_secs(20)).await;
        None
    })
    .await
    .unwrap_or_else(|| {
        Err(AgentError::Protocol(
            "Claude usage request timed out".into(),
        ))
    });

    let _ = stdin.close().await;
    let _ = child.kill();
    let _ = child.status().await;
    result
}

fn mcp_args(registrations: &[crate::McpRegistration]) -> Vec<String> {
    if registrations.is_empty() {
        Vec::new()
    } else {
        vec![
            "--mcp-config".into(),
            crate::claude_mcp_config_json(registrations),
        ]
    }
}

/// Model-scoped launch flags resolved from the session's option selections.
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

fn launch_settings_json(
    thinking: Option<bool>,
    fast_mode: bool,
    ultracode: bool,
    auto_compact_window: Option<u64>,
) -> Option<String> {
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
    if let Some(window) = auto_compact_window {
        settings.insert("autoCompactWindow".into(), json!(window));
    }
    (!settings.is_empty())
        .then(|| serde_json::to_string(&Value::Object(settings)).unwrap_or_default())
}

impl ClaudeLaunchOptions {
    fn resolve(model: Option<&str>, selections: &[OptionSelection]) -> Self {
        let spec = model.and_then(model_spec);
        let raw_effort = selection_str(selections, "reasoningEffort");
        let resolved_effort = resolve_claude_effort(spec.as_ref(), raw_effort.as_deref());
        let ultrathink = resolved_effort.as_deref() == Some("ultrathink");
        let ultracode = resolved_effort.as_deref() == Some("ultracode");
        let effort = normalize_claude_cli_effort(resolved_effort.as_deref(), model);

        let window = resolved_context_window(model.unwrap_or_default(), selections);
        let native_window = native_context_window(model.unwrap_or_default());
        let effective_model_window = if native_window == 200_000 && window > native_window {
            1_000_000
        } else {
            native_window
        };
        let model_id = model.map(|m| {
            let base = m.strip_suffix("[1m]").unwrap_or(m);
            if native_window == 200_000 && window > native_window {
                format!("{base}[1m]")
            } else {
                base.to_owned()
            }
        });
        let auto_compact_window = (window < effective_model_window).then_some(window);

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

        let settings_json =
            launch_settings_json(thinking, fast_mode, ultracode, auto_compact_window);

        ClaudeLaunchOptions {
            model_id,
            effort,
            settings_json,
            ultrathink,
        }
    }
}

/// Per-session config threaded into the actor loop / mapper.
struct SessionConfig {
    claude_dir: Option<PathBuf>,
}

fn resume_args(resume: &Option<ResumeCursor>, fork: bool) -> Vec<String> {
    let Some(session_id) = resume
        .as_ref()
        .and_then(|cursor| cursor.str_field(&["session_id"]))
        .map(str::to_owned)
    else {
        return Vec::new();
    };
    let mut args = vec!["--resume".into(), session_id];
    if fork {
        args.push("--fork-session".into());
    }
    args
}

#[allow(clippy::too_many_arguments)]
async fn actor_loop(
    mut child: smol::process::Child,
    mut stdin: smol::process::ChildStdin,
    cmd_rx: smol::channel::Receiver<SessionCommand>,
    line_rx: smol::channel::Receiver<String>,
    event_tx: smol::channel::Sender<AgentEvent>,
    config: SessionConfig,
    ultrathink: bool,
    interaction_mode: InteractionMode,
    base_permission_mode: &'static str,
    applied_permission_mode: String,
    approval_mode: ApprovalMode,
    native_rewind: bool,
    expected_model: Option<String>,
    stderr_tail: crate::process::StderrTail,
    stderr_task: smol::Task<()>,
) {
    let mut mapper = Mapper::new_configured(
        ultrathink,
        interaction_mode,
        base_permission_mode,
        applied_permission_mode,
        approval_mode,
        native_rewind,
        expected_model,
    );
    let claude_dir = config.claude_dir.clone();
    let mut tailers = HashMap::new();

    // Set when the child died on its own (stdout EOF): only then do its exit
    // status and stderr tail belong in the close reason.
    let mut provider_exited = false;
    let closed_reason: Option<String> = loop {
        // Race a UI command against the next stdout line. `or` biases toward the
        // command channel, which is fine: both channels make independent progress.
        let sel = smol::future::or(async { Sel::Cmd(cmd_rx.recv().await.ok()) }, async {
            Sel::Line(line_rx.recv().await.ok())
        })
        .await;

        match sel {
            Sel::Cmd(Some(command)) => {
                if let ControlFlow::Break(reason) =
                    handle_command(command, &mut mapper, &mut stdin, &event_tx, &mut child).await
                {
                    break Some(reason.into());
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
                process_tail_requests(
                    mapper.take_tail_requests(),
                    &mut tailers,
                    claude_dir.as_deref(),
                    &event_tx,
                )
                .await;
            }
            Sel::Line(None) => {
                // stdout closed: child is exiting. Status is read before our
                // kill below so the child's own exit code is not masked by it.
                provider_exited = true;
                break Some(match child.try_status().ok().flatten() {
                    Some(status) => format!("claude exited with {status}"),
                    None => "claude closed stdout".into(),
                });
            }
        }
    };

    let _ = stdin.close().await;
    for control in tailers.into_values() {
        let _ = control.send(TailControl::Stop).await;
    }
    let _ = child.kill();
    let _ = child.status().await;
    // The child is gone, so its stderr pipe normally closes and the drain task
    // finishes; the timeout covers a grandchild keeping the pipe open, which
    // would otherwise hang the close event forever.
    smol::future::or(stderr_task, async {
        smol::Timer::after(std::time::Duration::from_millis(500)).await;
    })
    .await;
    let closed_reason = closed_reason.map(|reason| {
        if provider_exited {
            stderr_tail.append_to(reason, "\nstderr:\n")
        } else {
            reason
        }
    });
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

enum TailControl {
    PreferPath(PathBuf),
    Stop,
}

async fn process_tail_requests(
    requests: Vec<TailRequest>,
    tailers: &mut HashMap<String, smol::channel::Sender<TailControl>>,
    claude_dir: Option<&Path>,
    event_tx: &smol::channel::Sender<AgentEvent>,
) {
    for request in requests {
        match request {
            TailRequest::Start {
                parent_id,
                task_id,
                session_id,
            } => {
                if tailers.contains_key(&parent_id) {
                    continue;
                }
                let (control_tx, control_rx) = smol::channel::unbounded();
                tailers.insert(parent_id.clone(), control_tx);
                let claude_dir = claude_dir.map(Path::to_path_buf);
                let events = event_tx.clone();
                smol::spawn(run_subagent_tail(
                    parent_id, task_id, session_id, claude_dir, control_rx, events,
                ))
                .detach();
            }
            TailRequest::PreferPath { parent_id, path } => {
                if let Some(control) = tailers.get(&parent_id) {
                    let _ = control.send(TailControl::PreferPath(path)).await;
                }
            }
            TailRequest::Stop { parent_id } => {
                if let Some(control) = tailers.get(&parent_id) {
                    let _ = control.send(TailControl::Stop).await;
                }
            }
        }
    }
}

async fn run_subagent_tail(
    parent_id: String,
    task_id: String,
    session_id: String,
    claude_dir: Option<PathBuf>,
    controls: smol::channel::Receiver<TailControl>,
    events: smol::channel::Sender<AgentEvent>,
) {
    let mut path = None;
    let mut reader = None;
    let mut stopping = false;
    loop {
        while let Ok(control) = controls.try_recv() {
            match control {
                TailControl::PreferPath(preferred) => {
                    if path.as_ref() != Some(&preferred) {
                        path = Some(preferred.clone());
                        reader = Some(crate::subagent_tail::TailReader::new(
                            preferred,
                            parent_id.clone(),
                        ));
                    }
                }
                TailControl::Stop => stopping = true,
            }
        }
        if path.is_none()
            && let Some(root) = &claude_dir
            && let Some(found) =
                crate::subagent_tail::find_transcript(root, &session_id, &task_id, &parent_id)
        {
            path = Some(found.clone());
            reader = Some(crate::subagent_tail::TailReader::new(
                found,
                parent_id.clone(),
            ));
        }
        if let Some(reader) = &mut reader {
            match reader.read_appended() {
                Ok(mapped) => {
                    for event in mapped {
                        if events.send(event).await.is_err() {
                            return;
                        }
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => log::debug!("claude subagent tail read failed: {err}"),
            }
        }
        if stopping {
            return;
        }
        smol::Timer::after(std::time::Duration::from_millis(400)).await;
    }
}

async fn handle_command(
    command: SessionCommand,
    mapper: &mut Mapper,
    stdin: &mut smol::process::ChildStdin,
    event_tx: &smol::channel::Sender<AgentEvent>,
    child: &mut smol::process::Child,
) -> ControlFlow<&'static str> {
    match command {
        SessionCommand::SendTurn {
            delivery_id,
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
                if write_line(stdin, &req).await.is_err() {
                    let _ = event_tx
                        .send(AgentEvent::Error {
                            message: "failed to write turn mode to provider stdin".into(),
                            fatal: true,
                        })
                        .await;
                    return ControlFlow::Break("provider stdin write failed");
                }
            }

            // `ultrathink` is a prompt-prefix mode, not a `--effort` value.
            let text = turn_text(text, mapper.ultrathink);
            let msg = user_message(&text, &attachments);
            if write_turn_message(stdin, &msg, delivery_id, event_tx)
                .await
                .is_err()
            {
                let _ = event_tx
                    .send(AgentEvent::Error {
                        message: "failed to write turn to provider stdin".into(),
                        fatal: true,
                    })
                    .await;
                return ControlFlow::Break("provider stdin write failed");
            }
            let turn_id = mapper.start_turn();
            let _ = event_tx.send(AgentEvent::TurnStarted { turn_id }).await;
            ControlFlow::Continue(())
        }
        SessionCommand::SetInteractionMode(mode) => {
            // Stored now; the `set_permission_mode` switch is issued before the
            // next `SendTurn` (matching T3's per-message application).
            mapper.interaction_mode = mode;
            ControlFlow::Continue(())
        }
        SessionCommand::Interrupt => {
            let msg = mapper.interrupt_request();
            let _ = write_line(stdin, &msg).await;
            ControlFlow::Continue(())
        }
        SessionCommand::RespondApproval {
            request_id,
            decision,
        } => {
            let response = mapper.build_approval_response(&request_id, decision.clone());
            resolve_response(
                stdin,
                event_tx,
                request_id,
                decision,
                response,
                |request_id, decision| AgentEvent::ApprovalResolved {
                    request_id,
                    decision,
                },
            )
            .await;
            ControlFlow::Continue(())
        }
        SessionCommand::RespondUserInput {
            request_id,
            answers,
        } => {
            let response = mapper.build_user_input_response(&request_id, &answers);
            resolve_response(
                stdin,
                event_tx,
                request_id,
                answers,
                response,
                |request_id, answers| AgentEvent::UserInputResolved {
                    request_id,
                    answers,
                },
            )
            .await;
            ControlFlow::Continue(())
        }
        SessionCommand::SetApprovalMode(mode) => {
            // The CLI's control protocol switches permission mode live via a
            // `set_permission_mode` control_request (same shape the Agent SDK
            // sends). Plan mode is a stricter overlay: approval changes update
            // only the Build mode to restore later.
            let flag = permission_mode_flag(mode);
            mapper.base_permission_mode = flag;
            mapper.approval_mode = mode;
            if mapper.interaction_mode == InteractionMode::Build {
                let msg = mapper.set_permission_mode_request_str(permission_mode_flag(mode));
                if write_line(stdin, &msg).await.is_err() {
                    if let Some(request_id) = msg.get("request_id").and_then(Value::as_str) {
                        mapper.pending_permission_modes.remove(request_id);
                    }
                    let _ = event_tx
                        .send(AgentEvent::Warning {
                            message: format!(
                                "claude: failed to write permission-mode switch for {mode:?}"
                            ),
                        })
                        .await;
                }
            }
            ControlFlow::Continue(())
        }
        SessionCommand::SetOption { id, .. } => {
            log::debug!("claude: ignoring ACP-only SetOption {id}");
            ControlFlow::Continue(())
        }
        SessionCommand::Rewind {
            checkpoint_id,
            mode,
        } => {
            match mapper.begin_rewind(checkpoint_id.clone(), mode) {
                Ok(request) if write_line(stdin, &request).await.is_ok() => {}
                Ok(_) => {
                    let _ = event_tx
                        .send(AgentEvent::RewindFailed {
                            checkpoint_id,
                            mode,
                            error: "failed to write rewind request to provider stdin".into(),
                        })
                        .await;
                }
                Err(error) => {
                    let _ = event_tx
                        .send(AgentEvent::RewindFailed {
                            checkpoint_id,
                            mode,
                            error,
                        })
                        .await;
                }
            }
            ControlFlow::Continue(())
        }
        SessionCommand::Steer {
            request_id,
            text,
            attachments,
        } => {
            // Steering writes the *same* stream-json user-message line as
            // `SendTurn`, but deliberately skips the turn bookkeeping: no
            // `start_turn()`, no `TurnStarted`. The CLI folds the message into
            // the turn that is already running at its next input checkpoint.
            // A successful write only queues the request id; acceptance is
            // emitted when stdout next reports `status: requesting`, the best
            // available signal that the CLI consumed it. There is a small
            // residual race: a steer written microseconds before that status
            // may actually miss the request, but the CLI protocol exposes no
            // stronger acknowledgement, so we accept it at that checkpoint.
            //
            // Verified live (examples/steer_probe.rs): 1 `TurnStarted`,
            // 1 `TurnCompleted` across a steered turn.
            let text = turn_text(text, mapper.ultrathink);
            let msg = user_message(&text, &attachments);
            write_steering_message(stdin, &msg, request_id, mapper, event_tx).await;
            ControlFlow::Continue(())
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
            ControlFlow::Break("shutdown requested")
        }
    }
}

async fn resolve_response<T>(
    stdin: &mut smol::process::ChildStdin,
    event_tx: &smol::channel::Sender<AgentEvent>,
    request_id: String,
    value: T,
    response: Option<Value>,
    resolved: impl FnOnce(String, T) -> AgentEvent,
) {
    if let Some(response) = response {
        let _ = write_line(stdin, &response).await;
        let _ = event_tx.send(resolved(request_id, value)).await;
    } else {
        log::debug!("claude: response for unknown request {request_id}");
    }
}

fn turn_text(text: String, ultrathink: bool) -> String {
    if ultrathink {
        format!("Ultrathink:\n{text}")
    } else {
        text
    }
}

fn control_response(request_id: &str, body: Value) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": body,
        }
    })
}

async fn write_turn_message<W: AsyncWrite + Unpin>(
    stdin: &mut W,
    message: &Value,
    delivery_id: u64,
    event_tx: &smol::channel::Sender<AgentEvent>,
) -> std::io::Result<()> {
    write_line(stdin, message).await?;
    // The complete newline-delimited prompt is now flushed. Emit acceptance
    // before returning to the actor loop, where stdout EOF can win the race.
    let _ = event_tx
        .send(AgentEvent::TurnAccepted { delivery_id })
        .await;
    Ok(())
}

async fn write_steering_message<W: AsyncWrite + Unpin>(
    stdin: &mut W,
    message: &Value,
    request_id: String,
    mapper: &mut Mapper,
    event_tx: &smol::channel::Sender<AgentEvent>,
) {
    if write_line(stdin, message).await.is_err() {
        let _ = event_tx
            .send(AgentEvent::Error {
                message: "failed to write steering message to provider stdin".into(),
                fatal: true,
            })
            .await;
    } else {
        mapper.pending_steers.push_back(request_id);
    }
}

async fn write_line<W: AsyncWrite + Unpin>(stdin: &mut W, value: &Value) -> std::io::Result<()> {
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
enum ToolItem {
    Command {
        command: String,
        output: String,
    },
    File {
        changes: Vec<FileChange>,
    },
    Tool {
        name: String,
        input: Value,
    },
    Subagent {
        agent_type: String,
        description: String,
        summary: Option<String>,
    },
}

enum TailRequest {
    Start {
        parent_id: String,
        task_id: String,
        session_id: String,
    },
    PreferPath {
        parent_id: String,
        path: PathBuf,
    },
    Stop {
        parent_id: String,
    },
}

/// A pending permission prompt, kept so `RespondApproval` can echo the tool's
/// (possibly updated) input and, for "approve for session", forward the SDK's
/// `permission_suggestions` verbatim.
struct PendingApproval {
    input: Value,
    /// `permission_suggestions` from the `can_use_tool` control_request,
    /// forwarded unchanged as `updatedPermissions` on `ApproveForSession` when
    /// the SDK supplied a non-empty array (S2 §4.3).
    suggestions: Option<Value>,
}

struct PendingRewind {
    checkpoint_id: String,
    mode: RewindMode,
    conversation: bool,
}

fn served_model_is_fallback(expected: &str, served: &str) -> bool {
    (expected.contains("fable") && served.contains("opus"))
        || (served.contains("opus-4-8")
            && expected.contains("opus")
            && !expected.contains("opus-4-8"))
}

pub(crate) struct Mapper {
    session_started: bool,
    current_message_id: Option<String>,
    /// How many content blocks of each streamed message we have already seen in
    /// an `assistant` line. The CLI splits one message across several `assistant`
    /// lines — one per block, each carrying a *one-element* `content` array — so
    /// enumerating that array gives 0 every time, while the `content_block_delta`
    /// stream numbers the blocks 0, 1, 2… The completed item has to reuse the
    /// stream's numbering or the timeline shows the same text twice: once from
    /// the deltas (`msg:1`) and once from the completion (`msg:0`).
    assistant_blocks_seen: HashMap<String, usize>,
    turn_counter: usize,
    current_turn_id: Option<String>,
    /// The next replayed top-level user message carries this turn's provider
    /// checkpoint UUID. Steering messages do not arm this flag.
    awaiting_turn_checkpoint: bool,
    control_counter: usize,
    tool_items: HashMap<String, ToolItem>,
    task_tools: HashMap<String, String>,
    child_mappers: HashMap<String, crate::subagent_tail::TranscriptMapper>,
    tail_requests: Vec<TailRequest>,
    pending_approvals: HashMap<String, PendingApproval>,
    /// Pending `AskUserQuestion` prompts: control request_id → the original
    /// `questions` array, echoed back verbatim in the allow response.
    pending_user_input: PendingRequests<String, Value>,
    /// Canonical session access policy. FullAccess auto-allows every ordinary
    /// tool; ReadOnly auto-allows only classified file reads. Special tools are
    /// handled before either policy.
    approval_mode: ApprovalMode,
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
    /// Live permission-mode requests awaiting Claude's correlated response.
    /// A successful stdin write is not proof that the CLI applied the mode.
    pending_permission_modes: HashMap<String, String>,
    /// Whether an `ExitPlanMode` plan has already been captured this turn.
    exit_plan_captured: bool,
    /// Control responses to write back (e.g. the auto-deny for `ExitPlanMode`).
    outgoing: Vec<Value>,
    /// Cumulative tokens processed across every completed turn this session
    /// (Claude reports only per-turn usage, so we accumulate it ourselves for
    /// the "Total processed" display).
    cumulative_processed: u64,
    /// Successfully written steers awaiting the CLI's next input checkpoint.
    pending_steers: VecDeque<String>,
    /// Once the CLI exposes request checkpoints, never use the legacy fallback.
    saw_requesting: bool,
    /// Background Bash tasks still owned by this Claude process. When the list
    /// becomes empty Claude immediately re-invokes itself with the completion
    /// notification, so zero is not published until that follow-up result.
    background_tasks: HashSet<String>,
    /// Bash tasks which were observed in `background_tasks_changed`, retained
    /// until their final notification so the command card can be completed.
    background_task_history: HashSet<String>,
    /// Native rewind control requests awaiting Claude's correlated response.
    pending_rewinds: HashMap<String, PendingRewind>,
    native_rewind: bool,
    /// Last assistant model published to the canonical event stream.
    last_served_model: Option<String>,
    /// Model selected for this session, used when Claude reports a synthetic refusal message.
    expected_model: Option<String>,
    /// Whether a served-model mismatch was already reported for the active turn.
    fallback_detected: bool,
    /// Latest structured stop reason for the active turn.
    stop_reason: Option<String>,
    /// Classifier category captured from the active turn's structured refusal details.
    pending_refusal_category: Option<ClassifierCategory>,
    /// Warning-bearing stop reason already emitted for this occurrence.
    warned_stop_reason: Option<String>,
}

impl Mapper {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::new_configured(
            false,
            InteractionMode::Build,
            "default",
            "default".to_owned(),
            ApprovalMode::Supervised,
            false,
            None,
        )
    }

    fn new_configured(
        ultrathink: bool,
        interaction_mode: InteractionMode,
        base_permission_mode: &'static str,
        applied_permission_mode: String,
        approval_mode: ApprovalMode,
        native_rewind: bool,
        expected_model: Option<String>,
    ) -> Self {
        Mapper {
            session_started: false,
            current_message_id: None,
            assistant_blocks_seen: HashMap::new(),
            turn_counter: 0,
            current_turn_id: None,
            awaiting_turn_checkpoint: false,
            control_counter: 0,
            tool_items: HashMap::new(),
            task_tools: HashMap::new(),
            child_mappers: HashMap::new(),
            tail_requests: Vec::new(),
            pending_approvals: HashMap::new(),
            pending_user_input: HashMap::new(),
            approval_mode,
            interrupt_pending: false,
            ultrathink,
            interaction_mode,
            base_permission_mode,
            applied_permission_mode,
            pending_permission_modes: HashMap::new(),
            exit_plan_captured: false,
            outgoing: Vec::new(),
            cumulative_processed: 0,
            pending_steers: VecDeque::new(),
            saw_requesting: false,
            background_tasks: HashSet::new(),
            background_task_history: HashSet::new(),
            pending_rewinds: HashMap::new(),
            native_rewind,
            last_served_model: None,
            expected_model,
            fallback_detected: false,
            stop_reason: None,
            pending_refusal_category: None,
            warned_stop_reason: None,
        }
    }

    /// Drain queued control-response writes for the actor to send.
    fn take_outgoing(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.outgoing)
    }

    fn take_tail_requests(&mut self) -> Vec<TailRequest> {
        std::mem::take(&mut self.tail_requests)
    }

    /// Allocate the next synthesized turn id and mark it in-flight.
    fn start_turn(&mut self) -> String {
        self.turn_counter += 1;
        let id = format!("turn-{}", self.turn_counter);
        self.current_turn_id = Some(id.clone());
        self.awaiting_turn_checkpoint = self.native_rewind;
        self.exit_plan_captured = false;
        self.fallback_detected = false;
        self.stop_reason = None;
        self.pending_refusal_category = None;
        self.warned_stop_reason = None;
        id
    }

    fn next_control_id(&mut self) -> String {
        self.control_counter += 1;
        format!("tcode-ctrl-{}", self.control_counter)
    }

    fn begin_rewind(&mut self, checkpoint_id: String, mode: RewindMode) -> Result<Value, String> {
        if !self.native_rewind {
            return Err("this Claude Code version does not expose native rewind controls".into());
        }
        Ok(self.rewind_request(checkpoint_id, mode, !mode.includes_files()))
    }

    fn rewind_request(
        &mut self,
        checkpoint_id: String,
        mode: RewindMode,
        conversation: bool,
    ) -> Value {
        let request = if conversation {
            json!({
                "subtype": "rewind_conversation",
                "target_message_uuid": checkpoint_id,
                "interrupt_if_running": false,
            })
        } else {
            json!({ "subtype": "rewind_files", "user_message_id": checkpoint_id })
        };
        let message = self.control_request(request);
        let request_id = message["request_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        self.pending_rewinds.insert(
            request_id,
            PendingRewind {
                checkpoint_id,
                mode,
                conversation,
            },
        );
        message
    }

    fn control_request(&mut self, request: Value) -> Value {
        json!({
            "type": "control_request",
            "request_id": self.next_control_id(),
            "request": request,
        })
    }

    /// Client → CLI interrupt control request.
    fn interrupt_request(&mut self) -> Value {
        if self.current_turn_id.is_some() {
            self.interrupt_pending = true;
        }
        self.control_request(json!({ "subtype": "interrupt" }))
    }

    /// Client → CLI `set_permission_mode` control request. Wire shape verified
    /// against `@anthropic-ai/claude-agent-sdk` v0.3.170 (`browser-sdk.js`):
    /// `request(e)` wraps the payload as
    /// `{request_id, type:"control_request", request:e}`, and
    /// `setPermissionMode(m)` sends `{subtype:"set_permission_mode", mode:m}`.
    /// `set_permission_mode` with a raw wire mode string (e.g. `"plan"`).
    fn set_permission_mode_request_str(&mut self, mode: &str) -> Value {
        let message = self.control_request(json!({
            "subtype": "set_permission_mode",
            "mode": mode,
        }));
        let request_id = message["request_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        self.pending_permission_modes
            .insert(request_id, mode.to_owned());
        message
    }

    /// Build the `control_response` answering a pending `can_use_tool` prompt.
    fn build_approval_response(
        &mut self,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> Option<Value> {
        let pending = self.pending_approvals.remove(request_id)?;
        let response = match decision {
            // Agent-supplied option ids are an ACP concept; Claude's approvals
            // are the fixed four. Deny rather than leave the turn hanging.
            ApprovalDecision::Option(ref id) => {
                log::warn!("claude: unexpected ACP option decision {id}; denying");
                json!({ "behavior": "deny", "message": "User declined tool execution." })
            }
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
        Some(control_response(request_id, response))
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
        Some(control_response(
            request_id,
            json!({
                "behavior": "allow",
                "updatedInput": { "questions": questions, "answers": answers }
            }),
        ))
    }

    /// Drain every pending `AskUserQuestion`, producing `(request_id, deny
    /// control_response)` pairs with T3's cancel message (S2 §1.2 abort path).
    fn cancel_pending_user_input(&mut self) -> Vec<(String, Value)> {
        drain_resolved(&mut self.pending_user_input)
            .into_iter()
            .map(|(request_id, _questions, _event)| {
                let response = control_response(
                    &request_id,
                    json!({
                        "behavior": "deny",
                        "message": "User cancelled tool execution.",
                    }),
                );
                (request_id, response)
            })
            .collect()
    }

    /// Emit at most one [`AgentEvent::ProposedPlan`] per turn. Claude can retry
    /// `ExitPlanMode` after tcode's auto-deny with a fresh tool id; deduping by
    /// id therefore re-arms the plan UI for the same turn.
    fn capture_proposed_plan(
        &mut self,
        tool_use_id: Option<&str>,
        markdown: String,
    ) -> Option<AgentEvent> {
        if self.exit_plan_captured {
            return None;
        }
        self.exit_plan_captured = true;
        let item_id = tool_use_id
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("plan-{}", self.turn_counter));
        Some(AgentEvent::ProposedPlan { item_id, markdown })
    }

    /// Map one CLI stdout message to zero or more outcomes.
    pub(crate) fn on_message(&mut self, msg: Value) -> Vec<AgentEvent> {
        if let Some(parent_id) = msg
            .get("parent_tool_use_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            return self
                .child_mappers
                .entry(parent_id.to_owned())
                .or_insert_with(|| crate::subagent_tail::TranscriptMapper::new(parent_id))
                .map_value(&msg);
        }
        match msg.get("type").and_then(Value::as_str) {
            Some("system") => self.on_system(&msg),
            Some("stream_event") => {
                let mut events = self.synthesized_turn_started();
                events.extend(self.on_stream_event(&msg));
                events
            }
            Some("assistant") => {
                let mut events = self.synthesized_turn_started();
                let steer_events = if self.saw_requesting {
                    Vec::new()
                } else {
                    self.accept_pending_steers()
                };
                events.extend(steer_events);
                events.extend(self.on_assistant(&msg));
                events
            }
            Some("user") => self.on_user(&msg),
            Some("control_request") => self.on_control_request(&msg),
            Some("control_response") => self.on_control_response(&msg),
            Some("result") => self.on_result(&msg),
            other => {
                log::debug!("claude: ignoring message type {other:?}");
                Vec::new()
            }
        }
    }

    fn synthesized_turn_started(&mut self) -> Vec<AgentEvent> {
        if self.session_started && self.current_turn_id.is_none() {
            let turn_id = self.start_turn();
            self.awaiting_turn_checkpoint = false;
            vec![AgentEvent::TurnStarted { turn_id }]
        } else {
            Vec::new()
        }
    }

    fn on_system(&mut self, msg: &Value) -> Vec<AgentEvent> {
        if msg.get("subtype").and_then(Value::as_str) == Some("status")
            && msg.get("status").and_then(Value::as_str) == Some("requesting")
        {
            self.saw_requesting = true;
            return self.accept_pending_steers();
        }
        match msg.get("subtype").and_then(Value::as_str) {
            Some("init") => {}
            // Claude compacted its context window (verified shape:
            // `{type:"system", subtype:"compact_boundary", compact_metadata:{…}}`).
            Some("compact_boundary") => return vec![AgentEvent::ContextCompacted],
            Some("task_started") => return self.on_task_started(msg),
            Some("task_updated") => return self.on_task_updated(msg),
            Some("task_notification") => return self.on_task_notification(msg),
            Some("background_tasks_changed") => return self.on_background_tasks_changed(msg),
            Some("model_refusal_fallback") => return self.on_model_refusal_fallback(msg),
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

    fn on_model_refusal_fallback(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let (Some(expected), Some(actual)) = (
            msg.get("original_model").and_then(Value::as_str),
            msg.get("fallback_model").and_then(Value::as_str),
        ) else {
            log::debug!("claude: ignoring malformed model_refusal_fallback");
            return Vec::new();
        };
        self.fallback_detected = true;
        vec![AgentEvent::ModelFallbackDetected {
            expected: expected.to_owned(),
            actual: actual.to_owned(),
            category: msg
                .get("api_refusal_category")
                .and_then(Value::as_str)
                .map(ClassifierCategory::parse),
            checkpoint_id: msg
                .get("refused_user_message_uuid")
                .and_then(Value::as_str)
                .map(str::to_owned),
            parent_tool_use_id: msg
                .get("parent_tool_use_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }]
    }

    fn accept_pending_steers(&mut self) -> Vec<AgentEvent> {
        self.pending_steers
            .drain(..)
            .map(|request_id| AgentEvent::SteerAccepted { request_id })
            .collect()
    }

    fn on_task_started(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let Some(tool_use_id) = msg.get("tool_use_id").and_then(Value::as_str) else {
            return Vec::new();
        };
        if let Some(task_id) = msg.get("task_id").and_then(Value::as_str) {
            self.task_tools
                .insert(task_id.to_owned(), tool_use_id.to_owned());
            if msg.get("task_type").and_then(Value::as_str) != Some("local_bash") {
                self.tail_requests.push(TailRequest::Start {
                    parent_id: tool_use_id.to_owned(),
                    task_id: task_id.to_owned(),
                    session_id: msg
                        .get("session_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                });
            }
        }
        self.update_subagent(tool_use_id, ItemStatus::InProgress, None)
    }

    fn on_background_tasks_changed(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let current = msg
            .get("tasks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|task| task.get("task_type").and_then(Value::as_str) == Some("local_bash"))
            .filter_map(|task| task.get("task_id").and_then(Value::as_str))
            .map(str::to_owned)
            .collect::<HashSet<_>>();
        self.background_task_history.extend(current.iter().cloned());
        let had_background_tasks = !self.background_tasks.is_empty();
        self.background_tasks = current;
        if self.background_tasks.is_empty() && had_background_tasks {
            // Recorded Claude traces put `background_tasks_changed []` before
            // task_notification and the provider's self-invoked follow-up turn.
            // Keep the last non-zero liveness until that turn's result so the
            // runtime cannot park/restart the process in this handoff window.
            Vec::new()
        } else {
            vec![AgentEvent::BackgroundTasksChanged {
                count: self.background_tasks.len(),
            }]
        }
    }

    fn on_task_updated(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let Some(task_id) = msg.get("task_id").and_then(Value::as_str) else {
            return Vec::new();
        };
        let Some(tool_use_id) = self.task_tools.get(task_id).cloned() else {
            return Vec::new();
        };
        let Some(status) = msg.pointer("/patch/status").and_then(Value::as_str) else {
            return Vec::new();
        };
        let status = subagent_status(status);
        if status != ItemStatus::InProgress {
            self.tail_requests.push(TailRequest::Stop {
                parent_id: tool_use_id.clone(),
            });
        }
        self.update_subagent(&tool_use_id, status, None)
    }

    fn on_task_notification(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let task_id = msg
            .get("task_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let tool_use_id = msg
            .get("tool_use_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                msg.get("task_id")
                    .and_then(Value::as_str)
                    .and_then(|task_id| self.task_tools.get(task_id).cloned())
            });
        let Some(tool_use_id) = tool_use_id else {
            return Vec::new();
        };
        let is_background_bash = task_id
            .as_ref()
            .is_some_and(|id| self.background_task_history.remove(id));
        if let Some(task_id) = &task_id {
            self.background_tasks.remove(task_id);
        }
        if let Some(path) = msg.get("output_file").and_then(Value::as_str) {
            self.tail_requests.push(TailRequest::PreferPath {
                parent_id: tool_use_id.clone(),
                path: PathBuf::from(path),
            });
        }
        self.tail_requests.push(TailRequest::Stop {
            parent_id: tool_use_id.clone(),
        });
        let status = subagent_status(
            msg.get("status")
                .and_then(Value::as_str)
                .unwrap_or("completed"),
        );
        let summary = msg
            .get("summary")
            .and_then(Value::as_str)
            .map(one_line_summary);
        if is_background_bash {
            self.complete_background_command(&tool_use_id, status, summary)
        } else {
            self.update_subagent(&tool_use_id, status, summary)
        }
    }

    fn complete_background_command(
        &mut self,
        tool_use_id: &str,
        status: ItemStatus,
        summary: Option<String>,
    ) -> Vec<AgentEvent> {
        let Some(ToolItem::Command {
            command,
            mut output,
        }) = self.tool_items.remove(tool_use_id)
        else {
            return Vec::new();
        };
        if output.trim().is_empty()
            && let Some(summary) = summary
        {
            output = summary;
        }
        vec![AgentEvent::ItemCompleted(ThreadItem {
            id: tool_use_id.to_owned(),
            parent_item_id: None,
            content: ItemContent::CommandExecution {
                command,
                output,
                exit_code: (status == ItemStatus::Completed).then_some(0),
                status,
            },
        })]
    }

    fn update_subagent(
        &mut self,
        tool_use_id: &str,
        status: ItemStatus,
        summary: Option<String>,
    ) -> Vec<AgentEvent> {
        let Some(ToolItem::Subagent {
            agent_type,
            description,
            summary: saved_summary,
        }) = self.tool_items.get_mut(tool_use_id)
        else {
            return Vec::new();
        };
        if summary.is_some() {
            *saved_summary = summary;
        }
        vec![AgentEvent::ItemUpdated(ThreadItem {
            id: tool_use_id.to_owned(),
            parent_item_id: None,
            content: ItemContent::Subagent {
                agent_type: agent_type.clone(),
                description: description.clone(),
                status,
                summary: saved_summary.clone(),
            },
        })]
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
                let mut events = self.observe_stop_reason(
                    event.pointer("/delta/stop_reason").and_then(Value::as_str),
                );
                // Live usage growth; nice-to-have for token display.
                if let Some(usage) = event.get("usage") {
                    let tu = map_usage(usage, None);
                    events.push(AgentEvent::TokenUsage(tu));
                }
                events
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
        let mut out = Vec::new();
        if message
            .pointer("/stop_details/type")
            .and_then(Value::as_str)
            == Some("refusal")
        {
            self.pending_refusal_category = message
                .pointer("/stop_details/category")
                .and_then(Value::as_str)
                .map(ClassifierCategory::parse);
        }
        if let Some(model) = message.get("model").and_then(Value::as_str) {
            if !self.fallback_detected
                && let Some(expected) = self.expected_model.as_deref()
            {
                let normalized_expected = expected
                    .split('[')
                    .next()
                    .unwrap_or(expected)
                    .to_ascii_lowercase();
                let normalized_served = model
                    .split('[')
                    .next()
                    .unwrap_or(model)
                    .to_ascii_lowercase();
                if served_model_is_fallback(&normalized_expected, &normalized_served) {
                    self.fallback_detected = true;
                    out.push(AgentEvent::ModelFallbackDetected {
                        expected: expected.to_owned(),
                        actual: model.to_owned(),
                        category: None,
                        checkpoint_id: None,
                        parent_tool_use_id: msg
                            .get("parent_tool_use_id")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    });
                }
            }
            if self.last_served_model.as_deref() != Some(model) {
                self.last_served_model = Some(model.to_owned());
                out.push(AgentEvent::ServedModel {
                    model: model.to_owned(),
                    reason: None,
                });
            }
        }
        out.extend(self.observe_stop_reason(message.get("stop_reason").and_then(Value::as_str)));
        let msg_id = message
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("msg")
            .to_string();
        let content = match message.get("content").and_then(Value::as_array) {
            Some(c) => c,
            None => return out,
        };
        // Continue the stream's block numbering across the CLI's split
        // `assistant` lines (see `assistant_blocks_seen`).
        let seen = self
            .assistant_blocks_seen
            .entry(msg_id.clone())
            .or_default();
        let first_index = *seen;
        *seen += content.len();

        for (offset, block) in content.iter().enumerate() {
            let index = first_index + offset;
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                    out.push(AgentEvent::ItemCompleted(ThreadItem {
                        id: format!("{msg_id}:{index}"),
                        parent_item_id: None,
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
                    // The CLI redacts thinking content in this line even when it
                    // streamed the deltas; completing with "" would blank the
                    // reasoning the user just watched arrive.
                    if !text.is_empty() {
                        out.push(AgentEvent::ItemCompleted(ThreadItem {
                            id: format!("{msg_id}:{index}"),
                            parent_item_id: None,
                            content: ItemContent::Reasoning {
                                text: text.to_string(),
                            },
                        }));
                    }
                }
                Some("tool_use") => {
                    out.extend(self.on_tool_use(block));
                }
                _ => {}
            }
        }
        out
    }

    fn observe_stop_reason(&mut self, reason: Option<&str>) -> Vec<AgentEvent> {
        let Some(reason) = reason.filter(|reason| !reason.is_empty()) else {
            return Vec::new();
        };
        self.stop_reason = Some(reason.to_owned());
        let message = match reason {
            "refusal" => Some("Request declined by safety classifiers (stop_reason: refusal)"),
            "max_tokens" => Some("Response truncated: max_tokens limit reached"),
            _ => None,
        };
        if message.is_none() || self.warned_stop_reason.as_deref() == Some(reason) {
            return Vec::new();
        }
        self.warned_stop_reason = Some(reason.to_owned());
        vec![AgentEvent::Warning {
            message: message.unwrap().to_owned(),
        }]
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
            if let Some(markdown) = extract_exit_plan_markdown(&input)
                && let Some(event) = self.capture_proposed_plan(Some(&tool_use_id), markdown)
            {
                return vec![event];
            }
            return Vec::new();
        }

        let (item, content) = if is_agent_tool(&name.to_lowercase()) {
            let agent_type = input
                .get("subagent_type")
                .and_then(Value::as_str)
                .unwrap_or("subagent")
                .to_owned();
            let description = subagent_description(&input);
            (
                ToolItem::Subagent {
                    agent_type: agent_type.clone(),
                    description: description.clone(),
                    summary: None,
                },
                ItemContent::Subagent {
                    agent_type,
                    description,
                    status: ItemStatus::InProgress,
                    summary: None,
                },
            )
        } else if name == "Bash" {
            let command = input
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            (
                ToolItem::Command {
                    command: command.clone(),
                    output: String::new(),
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
            parent_item_id: None,
            content,
        })]
    }

    fn on_user(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let content = msg
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array);
        let mut out = Vec::new();
        let has_tool_result = content.is_some_and(|content| {
            content
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        });
        if self.awaiting_turn_checkpoint
            && !has_tool_result
            && let (Some(turn_id), Some(checkpoint_id)) = (
                self.current_turn_id.clone(),
                msg.get("uuid").and_then(Value::as_str),
            )
        {
            self.awaiting_turn_checkpoint = false;
            out.push(AgentEvent::TurnCheckpoint {
                turn_id,
                checkpoint_id: checkpoint_id.to_owned(),
            });
        }
        for block in content.into_iter().flatten() {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let tool_use_id = match block.get("tool_use_id").and_then(Value::as_str) {
                Some(id) => id.to_string(),
                None => continue,
            };
            let background_task_id = msg
                .pointer("/tool_use_result/backgroundTaskId")
                .and_then(Value::as_str)
                .map(str::to_owned);
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
                ToolItem::Command { command, .. } if background_task_id.is_some() => {
                    self.tool_items.insert(
                        tool_use_id.clone(),
                        ToolItem::Command {
                            command: command.clone(),
                            output: output.clone(),
                        },
                    );
                    if let Some(task_id) = background_task_id {
                        self.background_task_history.insert(task_id);
                    }
                    ItemContent::CommandExecution {
                        command,
                        output,
                        exit_code: None,
                        status: ItemStatus::InProgress,
                    }
                }
                ToolItem::Command { command, .. } => ItemContent::CommandExecution {
                    command,
                    output,
                    exit_code: if is_error { Some(1) } else { Some(0) },
                    status,
                },
                ToolItem::File { mut changes } => {
                    if let Some(structured_patch) = msg.pointer("/tool_use_result/structuredPatch")
                    {
                        let diff = render_structured_patch(structured_patch);
                        let path = msg
                            .pointer("/tool_use_result/filePath")
                            .and_then(Value::as_str);
                        for change in &mut changes {
                            if let Some(path) = path {
                                change.path = path.to_owned();
                            }
                            // Successful writes can report `structuredPatch: []`;
                            // that omission must not erase the input-derived diff.
                            if let Some(diff) = &diff {
                                change.diff = Some(diff.clone());
                            }
                        }
                    }
                    ItemContent::FileChange { changes, status }
                }
                ToolItem::Tool { name, input } => ItemContent::ToolCall {
                    name,
                    input,
                    output: Some(output),
                    status,
                },
                ToolItem::Subagent {
                    agent_type,
                    description,
                    summary,
                } => ItemContent::Subagent {
                    agent_type,
                    description,
                    status,
                    summary: summary
                        .or_else(|| (!output.trim().is_empty()).then(|| one_line_summary(&output))),
                },
            };
            let event = if matches!(
                &content,
                ItemContent::CommandExecution {
                    status: ItemStatus::InProgress,
                    ..
                }
            ) {
                AgentEvent::ItemUpdated
            } else {
                AgentEvent::ItemCompleted
            };
            out.push(event(ThreadItem {
                id: tool_use_id,
                parent_item_id: None,
                content,
            }));
        }
        out
    }

    fn on_control_response(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let Some(response) = msg.get("response") else {
            return Vec::new();
        };
        let Some(request_id) = response.get("request_id").and_then(Value::as_str) else {
            return Vec::new();
        };
        if let Some(mode) = self.pending_permission_modes.remove(request_id) {
            if response.get("subtype").and_then(Value::as_str) == Some("success") {
                self.applied_permission_mode = mode;
                return Vec::new();
            }
            let error = response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("Claude Code rejected the permission-mode request");
            return vec![AgentEvent::Warning {
                message: format!("claude: failed to switch permission mode: {error}"),
            }];
        }
        let Some(pending) = self.pending_rewinds.remove(request_id) else {
            return Vec::new();
        };
        let checkpoint_id = pending.checkpoint_id.clone();
        let mode = pending.mode;
        if response.get("subtype").and_then(Value::as_str) != Some("success") {
            let error = response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("Claude Code rejected the rewind request")
                .to_owned();
            return vec![AgentEvent::RewindFailed {
                checkpoint_id,
                mode,
                error,
            }];
        }
        let result = response.get("response").unwrap_or(&Value::Null);
        if !pending.conversation {
            if result.get("canRewind").and_then(Value::as_bool) == Some(false) {
                return vec![AgentEvent::RewindFailed {
                    checkpoint_id,
                    mode,
                    error: result
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("Claude Code could not restore files")
                        .to_owned(),
                }];
            }
            if mode == RewindMode::FilesAndConversation {
                let request = self.rewind_request(checkpoint_id, mode, true);
                self.outgoing.push(request);
                return Vec::new();
            }
            return vec![AgentEvent::RewindCompleted {
                checkpoint_id,
                mode,
                prefill: None,
            }];
        }
        if result.get("rewound").and_then(Value::as_bool) != Some(true) {
            vec![AgentEvent::RewindFailed {
                checkpoint_id,
                mode,
                error: result
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("Claude Code could not restore the conversation")
                    .to_owned(),
            }]
        } else {
            vec![AgentEvent::RewindCompleted {
                checkpoint_id,
                mode,
                prefill: result
                    .get("prefillText")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            }]
        }
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
            if let Some(markdown) = extract_exit_plan_markdown(&input)
                && let Some(event) = self.capture_proposed_plan(Some(tool_use_id), markdown)
            {
                events.push(event);
            }
            self.outgoing.push(control_response(
                &request_id,
                json!({ "behavior": "deny", "message": EXIT_PLAN_DENY_MESSAGE }),
            ));
            return events;
        }

        // (c) Classify per the T3 substring matrix (S2 §1.3).
        let request_type = classify_claude_tool(&tool_name);

        // (d) Full-access allows ordinary tools; read-only allows native reads.
        let read_only_allow = self.approval_mode == ApprovalMode::ReadOnly
            && request_type == ClaudeRequestType::FileRead;
        if self.approval_mode == ApprovalMode::FullAccess || read_only_allow {
            self.outgoing.push(control_response(
                &request_id,
                json!({ "behavior": "allow", "updatedInput": input }),
            ));
            return Vec::new();
        }

        // (e) Everything else becomes a user-visible approval request.
        let detail = approval_detail(&tool_name, &input);
        let kind = match request_type {
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
            .filter(|v| v.as_array().is_some_and(|a| !a.is_empty()))
            .cloned();
        self.pending_approvals
            .insert(request_id.clone(), PendingApproval { input, suggestions });

        vec![AgentEvent::ApprovalRequested(ApprovalRequest {
            id: request_id,
            turn_id: self.current_turn_id.clone(),
            kind,
            // Native approvals use the fixed four decisions.
            options: Vec::new(),
        })]
    }

    fn on_result(&mut self, msg: &Value) -> Vec<AgentEvent> {
        self.awaiting_turn_checkpoint = false;
        let mut events = self.observe_stop_reason(msg.get("stop_reason").and_then(Value::as_str));
        let turn_id = self
            .current_turn_id
            .take()
            .unwrap_or_else(|| format!("turn-{}", self.turn_counter.max(1)));
        // No message outlives its turn, so the block counters can go.
        self.assistant_blocks_seen.clear();
        let mut status = result_status(msg, self.stop_reason.as_deref());
        if std::mem::take(&mut self.interrupt_pending) && status != TurnStatus::Completed {
            status = TurnStatus::Interrupted;
        }
        let usage = msg.get("usage").map(|u| {
            let mut usage = map_usage(u, msg.get("modelUsage"));
            // Accumulate this turn's processed tokens into the session total.
            self.cumulative_processed += crate::processed_tokens(usage);
            usage.total_processed_tokens = Some(self.cumulative_processed);
            usage.cost_usd = msg.get("total_cost_usd").and_then(Value::as_f64);
            usage.duration_ms = msg.get("duration_ms").and_then(Value::as_u64);
            usage
        });
        if status == TurnStatus::Failed {
            // The `result` field carries the CLI's own error text (API errors,
            // crashes); a bare "failed" turn marker would discard it.
            let detail = msg
                .get("result")
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| msg.to_string());
            let subtype = msg
                .get("subtype")
                .and_then(Value::as_str)
                .unwrap_or("error");
            events.push(AgentEvent::Error {
                message: format!("claude turn failed ({subtype}): {detail}"),
                fatal: false,
            });
        }
        if self.stop_reason.as_deref() == Some("refusal") {
            let detail = msg
                .get("result")
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty())
                .unwrap_or("Claude blocked this turn after a safety classifier refusal.")
                .to_owned();
            events.push(AgentEvent::TurnBlocked {
                category: self.pending_refusal_category.take(),
                model: self.expected_model.clone(),
                detail,
            });
        }
        // Publish the current background-task liveness with every result, not
        // only task-notification-origin ones. `background_tasks_changed []`
        // deliberately withholds the zero (see `on_background_tasks_changed`),
        // so a result is the only place the runtime can learn the tasks are
        // gone — and gating that on the CLI's `origin.kind` shape left the
        // count pinned above zero (session stuck "Working") whenever the
        // follow-up result arrived with a different origin. Re-publishing an
        // unchanged count is harmless; a parked runtime still observes zero
        // only after all completion output has landed.
        events.push(AgentEvent::BackgroundTasksChanged {
            count: self.background_tasks.len(),
        });
        events.push(AgentEvent::TurnCompleted {
            turn_id,
            status,
            usage,
        });
        events
    }
}

fn is_file_tool(name: &str) -> bool {
    matches!(name, "Write" | "Edit" | "MultiEdit" | "NotebookEdit")
}

fn subagent_status(status: &str) -> ItemStatus {
    match status {
        "completed" | "done" | "succeeded" => ItemStatus::Completed,
        "failed" | "error" | "cancelled" | "canceled" | "stopped" | "killed" | "interrupted" => {
            ItemStatus::Failed
        }
        _ => ItemStatus::InProgress,
    }
}

fn one_line_summary(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
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

fn subagent_description(input: &Value) -> String {
    input
        .get("description")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            input
                .get("prompt")
                .and_then(Value::as_str)
                .map(|prompt| prompt.chars().take(200).collect())
                .unwrap_or_default()
        })
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
        let body = subagent_description(input);
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
                prefill: None,
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

/// Render Claude's structured edit hunks as a unified diff body.
fn render_structured_patch(value: &Value) -> Option<String> {
    let hunks = value.as_array()?;
    let mut rendered = Vec::new();
    for hunk in hunks {
        let old_start = hunk.get("oldStart").and_then(Value::as_u64)?;
        let old_lines = hunk.get("oldLines").and_then(Value::as_u64)?;
        let new_start = hunk.get("newStart").and_then(Value::as_u64)?;
        let new_lines = hunk.get("newLines").and_then(Value::as_u64)?;
        rendered.push(format!(
            "@@ -{old_start},{old_lines} +{new_start},{new_lines} @@"
        ));
        rendered.extend(
            hunk.get("lines")
                .and_then(Value::as_array)?
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned),
        );
    }
    (!rendered.is_empty()).then(|| rendered.join("\n"))
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

fn result_status(msg: &Value, stop_reason: Option<&str>) -> TurnStatus {
    let subtype = msg.get("subtype").and_then(Value::as_str);
    let is_error = msg.get("is_error").and_then(Value::as_bool);
    if subtype == Some("success") && is_error != Some(true) {
        return TurnStatus::Completed;
    }
    if matches!(
        subtype,
        Some("interrupted" | "interrupt" | "cancelled" | "canceled" | "aborted")
    ) || matches!(
        stop_reason,
        Some("interrupted" | "interrupt" | "cancelled" | "canceled" | "aborted")
    ) {
        return TurnStatus::Interrupted;
    }
    // Older CLI error results have no structured interrupt marker. Restrict the
    // legacy prose heuristic to those non-success results so ordinary answers
    // mentioning cancellation cannot be misclassified.
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
        context_window,
        // Session-cumulative total is stamped by the caller (`on_result`); the
        // streaming/partial usage path leaves it unset.
        ..crate::normalize::token_usage(input, cache_read, output, used_tokens)
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
    if let Some(raw) = raw
        && options.iter().any(|o| o.value == raw)
    {
        return Some(raw.to_owned());
    }
    default_value.clone()
}

/// T3's `normalizeClaudeCliEffort`: `ultrathink` → no flag (prompt prefix);
/// `ultracode` → `xhigh`; `xhigh` → `max` except Fable 5.x / Opus 5 /
/// Opus 4.8 / Sonnet 5; Sonnet 4.6 `max` → `high`; otherwise passthrough.
fn normalize_claude_cli_effort(effort: Option<&str>, model: Option<&str>) -> Option<String> {
    let effort = effort?;
    if effort == "ultrathink" {
        return None;
    }
    if effort == "ultracode" {
        return Some("xhigh".to_owned());
    }
    if effort == "xhigh"
        && model != Some("claude-fable-5-1")
        && model != Some("claude-fable-5")
        && model != Some("claude-opus-5")
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

fn context_window(default: &str) -> OptionDescriptor {
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
        default_value: Some(default.to_owned()),
    }
}

/// Parse a Claude context-window selection into a validated token count.
pub fn parse_context_window_tokens(value: &Value) -> Option<u64> {
    let tokens = match value {
        Value::Number(number) => number.as_u64()?,
        Value::String(value) => {
            let value = value.trim().to_ascii_lowercase();
            if let Some(value) = value.strip_suffix('k') {
                value.parse::<u64>().ok()?.checked_mul(1_000)?
            } else if let Some(value) = value.strip_suffix('m') {
                value.parse::<u64>().ok()?.checked_mul(1_000_000)?
            } else {
                let value = value.parse::<u64>().ok()?;
                if value < 1_000 {
                    value.checked_mul(1_000)?
                } else {
                    value
                }
            }
        }
        _ => return None,
    };
    (100_000..=1_000_000).contains(&tokens).then_some(tokens)
}

/// Return the model's native context-window size in tokens.
pub fn native_context_window(model_id: &str) -> u64 {
    match model_id.strip_suffix("[1m]").unwrap_or(model_id) {
        "claude-fable-5" | "claude-fable-5-1" | "claude-opus-5" | "claude-sonnet-5"
        | "claude-opus-4-7" | "claude-opus-4-8" => 1_000_000,
        _ => 200_000,
    }
}

/// Format a context-window token count for display.
pub fn format_context_window(tokens: u64) -> String {
    if tokens == 1_000_000 {
        "1M".to_owned()
    } else {
        format!("{}k", tokens / 1_000)
    }
}

/// Resolve the selected context window, falling back to the model's native size.
pub fn resolved_context_window(model_id: &str, selections: &[OptionSelection]) -> u64 {
    selections
        .iter()
        .find(|selection| selection.id == "contextWindow")
        .and_then(|selection| parse_context_window_tokens(&selection.value))
        .unwrap_or_else(|| native_context_window(model_id))
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
            "claude-fable-5-1",
            "Claude Fable 5.1",
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
                context_window("1m"),
            ],
        ),
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
                context_window("1m"),
            ],
        ),
        model(
            "claude-opus-5",
            "Claude Opus 5",
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
                context_window("1m"),
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
                context_window("1m"),
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
                context_window("1m"),
            ],
        ),
        model(
            "claude-opus-4-6",
            "Claude Opus 4.6",
            vec![
                reasoning(&["low", "medium", "high", "max", "ultrathink"], "high"),
                context_window("200k"),
            ],
        ),
        model(
            "claude-opus-4-5",
            "Claude Opus 4.5",
            vec![reasoning(&["low", "medium", "high", "max"], "high")],
        ),
        model(
            "claude-sonnet-5",
            "Claude Sonnet 5",
            vec![
                reasoning(
                    &["low", "medium", "high", "xhigh", "max", "ultrathink"],
                    "high",
                ),
                context_window("1m"),
            ],
        ),
        model(
            "claude-sonnet-4-6",
            "Claude Sonnet 4.6",
            vec![
                reasoning(&["low", "medium", "high", "max", "ultrathink"], "high"),
                context_window("200k"),
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
        "claude-fable-5-1" => version_ge(version, (2, 1, 257)),
        "claude-opus-5" => version_ge(version, (2, 1, 219)),
        "claude-fable-5" => version_ge(version, (2, 1, 169)),
        "claude-opus-4-8" => version_ge(version, (2, 1, 154)),
        "claude-opus-4-7" => version_ge(version, (2, 1, 111)),
        _ => true,
    }
}

fn version_ge(version: Option<(u32, u32, u32)>, min: (u32, u32, u32)) -> bool {
    version.is_some_and(|v| v >= min)
}

/// Parse a `MAJOR.MINOR.PATCH` triple from `claude --version` output
/// (e.g. `"2.1.206 (Claude Code)"`).
/// Run `claude --version` and parse the semver triple; `None` on any failure.
async fn claude_version(binary: Option<&Path>, launch_env: &LaunchEnv) -> Option<(u32, u32, u32)> {
    // Resolve through the PATH search (PATHEXT-aware: on Windows the CLI only
    // exists as `claude.cmd`), falling back to the bare name so the failure is
    // reported by the OS exactly as before.
    let bin = crate::resolve_binary(binary, "claude")
        .unwrap_or_else(|_| std::path::PathBuf::from("claude"));
    crate::process::probe_version(&bin, launch_env, ProviderKind::ClaudeCode).await
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

    #[test]
    fn fork_flag_requires_both_request_and_resume_cursor() {
        let resume = Some(ResumeCursor(json!({"session_id": "session-source"})));
        assert_eq!(
            resume_args(&resume, true),
            ["--resume", "session-source", "--fork-session"]
        );
        assert_eq!(resume_args(&resume, false), ["--resume", "session-source"]);
        assert!(resume_args(&None, true).is_empty());
    }

    #[test]
    fn mcp_builder_handles_both_one_and_none() {
        let preview = crate::McpRegistration {
            name: "tcode_preview".into(),
            url: "http://p".into(),
            bearer_token: "p".into(),
        };
        let orchestrate = crate::McpRegistration {
            name: "tcode_orchestrate".into(),
            url: "http://o".into(),
            bearer_token: "o".into(),
        };
        let computer_use = crate::McpRegistration {
            name: "tcode_computer_use".into(),
            url: "http://c".into(),
            bearer_token: "c".into(),
        };
        assert!(mcp_args(&[]).is_empty());
        let one = mcp_args(std::slice::from_ref(&preview));
        assert_eq!(one[0], "--mcp-config");
        let one_json: Value = serde_json::from_str(&one[1]).unwrap();
        assert!(one_json["mcpServers"].get("tcode_preview").is_some());
        assert!(one_json["mcpServers"].get("tcode_orchestrate").is_none());
        let all_json: Value =
            serde_json::from_str(&mcp_args(&[preview, orchestrate, computer_use])[1]).unwrap();
        assert!(all_json["mcpServers"].get("tcode_preview").is_some());
        assert!(all_json["mcpServers"].get("tcode_orchestrate").is_some());
        assert!(all_json["mcpServers"].get("tcode_computer_use").is_some());
    }

    fn feed(mapper: &mut Mapper, line: &str) -> Vec<AgentEvent> {
        let msg: Value = serde_json::from_str(line).expect("valid json fixture line");
        mapper.on_message(msg)
    }

    /// Every `result` publishes the current background-task count immediately
    /// before its `TurnCompleted` (the runtime's only reliable zero signal).
    /// Assert that contract and return the completion event.
    #[track_caller]
    fn turn_completed_after_count(events: &[AgentEvent], count: usize) -> &AgentEvent {
        let at = events
            .iter()
            .position(|event| matches!(event, AgentEvent::TurnCompleted { .. }))
            .unwrap_or_else(|| panic!("no TurnCompleted in {events:?}"));
        assert!(
            at > 0
                && matches!(
                    &events[at - 1],
                    AgentEvent::BackgroundTasksChanged { count: published } if *published == count
                ),
            "TurnCompleted must follow BackgroundTasksChanged {{ count: {count} }}, got {events:?}"
        );
        &events[at]
    }

    #[test]
    fn model_refusal_fallback_emits_model_fallback_detected() {
        let mut mapper = Mapper::new();
        let events = feed(
            &mut mapper,
            r#"{"type":"system","subtype":"model_refusal_fallback","trigger":"refusal","direction":"retry","scope":"session","original_model":"claude-fable-5","fallback_model":"claude-opus-4-8","request_id":"req_123","api_refusal_category":"cyber","api_refusal_explanation":null,"refused_user_message_uuid":"cf7703ac-420b-4038-9f32-582077b27352","content":"Safeguards flagged this message. Switched models.","session_id":"session-1","uuid":"system-1"}"#,
        );

        assert_eq!(
            events,
            [AgentEvent::ModelFallbackDetected {
                expected: "claude-fable-5".into(),
                actual: "claude-opus-4-8".into(),
                category: Some(ClassifierCategory::Cyber),
                checkpoint_id: Some("cf7703ac-420b-4038-9f32-582077b27352".into()),
                parent_tool_use_id: None,
            }]
        );
    }

    #[test]
    fn served_model_fallback_family_rule() {
        let cases = [
            ("claude-fable-5", "claude-opus-4-8", true),
            ("claude-fable-5", "claude-opus-5", true),
            ("claude-opus-5", "claude-opus-4-8", true),
            ("claude-opus-4-8", "claude-opus-4-8", false),
            ("claude-fable-5", "claude-fable-5", false),
            ("claude-fable-5", "claude-sonnet-4-5", false),
            ("claude-fable-5", "claude-haiku-4-5", false),
            ("anything", "<synthetic>", false),
        ];
        for (expected, served, is_fallback) in cases {
            assert_eq!(
                served_model_is_fallback(expected, served),
                is_fallback,
                "expected={expected}, served={served}"
            );
        }

        let expected = "claude-opus-5[1m]";
        let normalized_expected = expected.split('[').next().unwrap_or(expected);
        assert!(served_model_is_fallback(
            normalized_expected,
            "claude-opus-4-8"
        ));
    }

    #[test]
    fn assistant_model_mismatch_emits_one_fallback_per_turn() {
        let mut mapper = Mapper::new_configured(
            false,
            InteractionMode::Build,
            "default",
            "default".into(),
            ApprovalMode::Supervised,
            false,
            Some("claude-fable-5".into()),
        );
        mapper.start_turn();

        let first = feed(
            &mut mapper,
            r#"{"type":"assistant","message":{"id":"msg-fallback-1","model":"claude-opus-4-8","content":[]},"parent_tool_use_id":null}"#,
        );
        assert_eq!(
            first
                .iter()
                .filter(|event| matches!(event, AgentEvent::ModelFallbackDetected { .. }))
                .count(),
            1
        );
        assert!(first.iter().any(|event| matches!(
            event,
            AgentEvent::ModelFallbackDetected {
                expected,
                actual,
                category: None,
                checkpoint_id: None,
                parent_tool_use_id: None,
            } if expected == "claude-fable-5"
                && actual == "claude-opus-4-8"
        )));

        let second = feed(
            &mut mapper,
            r#"{"type":"assistant","message":{"id":"msg-fallback-2","model":"claude-opus-4-8","content":[]},"parent_tool_use_id":null}"#,
        );
        assert!(
            !second
                .iter()
                .any(|event| matches!(event, AgentEvent::ModelFallbackDetected { .. }))
        );
    }

    #[test]
    fn classifier_refusal_result_emits_turn_blocked() {
        let mut mapper = Mapper::new_configured(
            false,
            InteractionMode::Build,
            "default",
            "default".into(),
            ApprovalMode::Supervised,
            false,
            Some("claude-fable-5".into()),
        );
        mapper.start_turn();

        let assistant_events = feed(
            &mut mapper,
            r#"{"type":"assistant","message":{"id":"msg-refusal","model":"<synthetic>","stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber","explanation":"This request triggered cyber content restrictions."},"content":[{"type":"text","text":"API Error: safeguards flagged this message. Details: `[cyber]`"}]},"parent_tool_use_id":null}"#,
        );
        assert_eq!(
            mapper.pending_refusal_category,
            Some(ClassifierCategory::Cyber)
        );
        assert!(
            assistant_events
                .iter()
                .any(|event| matches!(event, AgentEvent::Warning { .. }))
        );

        let events = feed(
            &mut mapper,
            r#"{"type":"result","subtype":"success","is_error":true,"stop_reason":"refusal","result":"API Error: safeguards flagged this message. Details: `[cyber]`","terminal_reason":"api_error","modelUsage":{"claude-fable-5":{"inputTokens":1,"outputTokens":0}}}"#,
        );

        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::TurnBlocked {
                category: Some(ClassifierCategory::Cyber),
                model: Some(model),
                detail,
            } if model == "claude-fable-5" && detail.contains("[cyber]")
        )));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::Error { fatal: false, .. }))
        );
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::TurnCompleted {
                status: TurnStatus::Failed,
                ..
            }
        )));
    }

    #[test]
    fn replayed_user_message_exposes_native_turn_checkpoint() {
        let mut mapper = Mapper::new();
        mapper.native_rewind = true;
        let turn_id = mapper.start_turn();
        let events = feed(
            &mut mapper,
            r#"{"type":"user","uuid":"checkpoint-1","message":{"content":[{"type":"text","text":"hello"}]}}"#,
        );
        assert!(matches!(
            events.as_slice(),
            [AgentEvent::TurnCheckpoint {
                turn_id: actual_turn,
                checkpoint_id,
            }] if actual_turn == &turn_id && checkpoint_id == "checkpoint-1"
        ));

        // Tool-result user envelopes and later steering echoes cannot replace
        // the checkpoint associated with the opening prompt.
        assert!(feed(
            &mut mapper,
            r#"{"type":"user","uuid":"tool-result","message":{"content":[{"type":"tool_result","tool_use_id":"missing","content":"ok"}]}}"#,
        )
        .is_empty());
    }

    #[test]
    fn combined_native_rewind_sequences_files_then_conversation() {
        let mut mapper = Mapper::new();
        mapper.native_rewind = true;
        let files = mapper
            .begin_rewind("checkpoint-2".into(), RewindMode::FilesAndConversation)
            .unwrap();
        assert_eq!(files["request"]["subtype"], "rewind_files");
        let files_id = files["request_id"].as_str().unwrap();
        let events = feed(
            &mut mapper,
            &json!({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": files_id,
                    "response": { "canRewind": true }
                }
            })
            .to_string(),
        );
        assert!(events.is_empty());
        let outgoing = mapper.take_outgoing();
        assert_eq!(outgoing.len(), 1);
        assert_eq!(outgoing[0]["request"]["subtype"], "rewind_conversation");
        assert_eq!(
            outgoing[0]["request"]["target_message_uuid"],
            "checkpoint-2"
        );

        let conversation_id = outgoing[0]["request_id"].as_str().unwrap();
        let events = feed(
            &mut mapper,
            &json!({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": conversation_id,
                    "response": {
                        "rewound": true,
                        "prefillText": "original prompt"
                    }
                }
            })
            .to_string(),
        );
        assert!(matches!(
            events.as_slice(),
            [AgentEvent::RewindCompleted {
                checkpoint_id,
                mode: RewindMode::FilesAndConversation,
                prefill: Some(prefill),
            }] if checkpoint_id == "checkpoint-2" && prefill == "original prompt"
        ));
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
            normalize_claude_cli_effort(Some("xhigh"), Some("claude-fable-5-1")).as_deref(),
            Some("xhigh")
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
        assert!(ids(Some((2, 1, 219))).contains(&"claude-opus-5".to_string()));
        assert!(ids(Some((2, 1, 206))).contains(&"claude-fable-5".to_string()));
        // Below every gate: opus-5 / fable-5 / opus-4-8 / opus-4-7 hidden, rest visible.
        let old = ids(Some((2, 1, 100)));
        assert!(!old.contains(&"claude-opus-5".to_string()));
        assert!(!old.contains(&"claude-fable-5".to_string()));
        assert!(!old.contains(&"claude-opus-4-8".to_string()));
        assert!(!old.contains(&"claude-opus-4-7".to_string()));
        assert!(old.contains(&"claude-opus-4-6".to_string()));
        assert!(old.contains(&"claude-haiku-4-5".to_string()));
        // Exact boundary is inclusive.
        assert!(ids(Some((2, 1, 257))).contains(&"claude-fable-5-1".to_string()));
        assert!(!ids(Some((2, 1, 256))).contains(&"claude-fable-5-1".to_string()));
        assert!(ids(Some((2, 1, 154))).contains(&"claude-opus-4-8".to_string()));
        assert!(!ids(Some((2, 1, 153))).contains(&"claude-opus-4-8".to_string()));
        assert!(ids(Some((2, 1, 219))).contains(&"claude-opus-5".to_string()));
        assert!(!ids(Some((2, 1, 218))).contains(&"claude-opus-5".to_string()));
        // Unknown version hides gated models.
        assert!(!ids(None).contains(&"claude-fable-5".to_string()));
    }

    #[test]
    fn fast_mode_models_match_supported_opus_versions() {
        let ids: Vec<String> = built_in_models()
            .into_iter()
            .filter(|model| {
                model.options.iter().any(
                    |option| matches!(option, OptionDescriptor::Boolean { id, .. } if id == "fastMode"),
                )
            })
            .map(|model| model.id)
            .collect();

        assert_eq!(ids, ["claude-opus-5", "claude-opus-4-8"]);
    }

    #[test]
    fn parse_semver_from_version_output() {
        assert_eq!(
            crate::process::parse_semver("2.1.206 (Claude Code)"),
            Some((2, 1, 206))
        );
        assert_eq!(crate::process::parse_semver("2.1.169"), Some((2, 1, 169)));
        assert_eq!(crate::process::parse_semver("nonsense"), None);
    }

    #[test]
    fn parse_context_window_values() {
        assert_eq!(parse_context_window_tokens(&json!("200k")), Some(200_000));
        assert_eq!(parse_context_window_tokens(&json!("1m")), Some(1_000_000));
        assert_eq!(parse_context_window_tokens(&json!("1M")), Some(1_000_000));
        assert_eq!(parse_context_window_tokens(&json!("500k")), Some(500_000));
        assert_eq!(parse_context_window_tokens(&json!("500000")), Some(500_000));
        assert_eq!(parse_context_window_tokens(&json!("500")), Some(500_000));
        assert_eq!(parse_context_window_tokens(&json!(750_000)), Some(750_000));
        assert_eq!(parse_context_window_tokens(&json!(99_999)), None);
        assert_eq!(parse_context_window_tokens(&json!(1_000_001)), None);
        assert_eq!(parse_context_window_tokens(&json!("99k")), None);
        assert_eq!(parse_context_window_tokens(&json!("1001k")), None);
        assert_eq!(parse_context_window_tokens(&json!("garbage")), None);
        assert_eq!(parse_context_window_tokens(&json!(-200_000)), None);
        assert_eq!(parse_context_window_tokens(&json!(null)), None);
        assert_eq!(native_context_window("claude-opus-5[1m]"), 1_000_000);
        assert_eq!(native_context_window("claude-sonnet-4-6[1m]"), 200_000);
        assert_eq!(format_context_window(200_000), "200k");
        assert_eq!(format_context_window(750_000), "750k");
        assert_eq!(format_context_window(1_000_000), "1M");
    }

    #[test]
    fn catalog_context_window_defaults_match_native_windows() {
        let default = |model_id: &str| {
            model_spec(model_id)
                .unwrap()
                .options
                .into_iter()
                .find_map(|option| match option {
                    OptionDescriptor::Select {
                        id, default_value, ..
                    } if id == "contextWindow" => default_value,
                    _ => None,
                })
        };

        for model_id in [
            "claude-fable-5",
            "claude-fable-5-1",
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-opus-4-7",
            "claude-opus-4-8",
        ] {
            assert_eq!(default(model_id).as_deref(), Some("1m"));
        }
        for model_id in ["claude-sonnet-4-6", "claude-opus-4-6"] {
            assert_eq!(default(model_id).as_deref(), Some("200k"));
        }
        assert_eq!(default("claude-haiku-4-5"), None);
        assert_eq!(default("claude-opus-4-5"), None);
    }

    #[test]
    fn context_window_launch_semantics() {
        let resolve = |model, value: Option<Value>| {
            let selections = value
                .map(|value| {
                    vec![OptionSelection {
                        id: "contextWindow".into(),
                        value,
                    }]
                })
                .unwrap_or_default();
            ClaudeLaunchOptions::resolve(Some(model), &selections)
        };
        let auto_compact = |launch: &ClaudeLaunchOptions| {
            launch.settings_json.as_deref().map(|settings| {
                serde_json::from_str::<Value>(settings).unwrap()["autoCompactWindow"].clone()
            })
        };

        let launch = resolve("claude-opus-5", Some(json!("200k")));
        assert_eq!(launch.model_id.as_deref(), Some("claude-opus-5"));
        assert_eq!(auto_compact(&launch), Some(json!(200_000)));

        let launch = resolve("claude-opus-5", Some(json!("1m")));
        assert_eq!(launch.model_id.as_deref(), Some("claude-opus-5"));
        assert!(launch.settings_json.is_none());

        let launch = resolve("claude-opus-5", Some(json!(500_000)));
        assert_eq!(launch.model_id.as_deref(), Some("claude-opus-5"));
        assert_eq!(auto_compact(&launch), Some(json!(500_000)));

        let launch = resolve("claude-sonnet-4-6", Some(json!("1m")));
        assert_eq!(launch.model_id.as_deref(), Some("claude-sonnet-4-6[1m]"));
        assert!(launch.settings_json.is_none());

        let launch = resolve("claude-sonnet-4-6", Some(json!(500_000)));
        assert_eq!(launch.model_id.as_deref(), Some("claude-sonnet-4-6[1m]"));
        assert_eq!(auto_compact(&launch), Some(json!(500_000)));

        for value in [Some(json!("200k")), None] {
            let launch = resolve("claude-sonnet-4-6", value);
            assert_eq!(launch.model_id.as_deref(), Some("claude-sonnet-4-6"));
            assert!(launch.settings_json.is_none());
        }

        let launch = resolve("claude-fable-5", Some(json!("1m")));
        assert_eq!(launch.model_id.as_deref(), Some("claude-fable-5"));
        assert!(launch.settings_json.is_none());
    }

    #[test]
    fn launch_options_resolve_effort_context_and_settings() {
        // Ultracode → effort xhigh + settings.ultracode.
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
        assert_eq!(launch.model_id.as_deref(), Some("claude-fable-5"));
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
    fn plan_session_launches_with_plan_permission_mode() {
        let launch_mode = initial_permission_mode(ApprovalMode::Supervised, InteractionMode::Plan);
        assert_eq!(launch_mode, "plan");

        let m = Mapper::new_configured(
            false,
            InteractionMode::Plan,
            "default",
            launch_mode.into(),
            ApprovalMode::Supervised,
            false,
            None,
        );

        assert_eq!(m.applied_permission_mode, "plan");
    }

    #[test]
    fn set_approval_mode_while_in_plan_keeps_plan_permission_mode() {
        smol::block_on(async {
            let mut child = crate::process::async_command("cat")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut stdin = child.stdin.take().unwrap();
            let (event_tx, _event_rx) = smol::channel::unbounded();
            let mut m = Mapper::new();
            m.interaction_mode = InteractionMode::Plan;
            m.applied_permission_mode = "plan".into();

            let flow = handle_command(
                SessionCommand::SetApprovalMode(ApprovalMode::FullAccess),
                &mut m,
                &mut stdin,
                &event_tx,
                &mut child,
            )
            .await;

            assert!(matches!(flow, ControlFlow::Continue(())));
            assert_eq!(m.base_permission_mode, "bypassPermissions");
            assert_eq!(m.applied_permission_mode, "plan");
            assert!(m.pending_permission_modes.is_empty());
            stdin.close().await.unwrap();
            child.kill().unwrap();
            child.status().await.unwrap();
        });
    }

    #[test]
    fn extra_permission_mode_arg_updates_launch_tracker() {
        let extra_args = vec!["--permission-mode".into(), "plan".into()];
        assert_eq!(
            effective_permission_mode("default", &extra_args),
            "plan",
            "the last provider launch argument overrides the internal flag"
        );

        let extra_args = vec!["--permission-mode=bypassPermissions".into()];
        assert_eq!(
            effective_permission_mode("plan", &extra_args),
            "bypassPermissions"
        );
    }

    #[test]
    fn exit_plan_mode_captures_once_per_turn() {
        let mut m = Mapper::new();
        m.start_turn();
        let first = feed(
            &mut m,
            r##"{"type":"control_request","request_id":"req-plan-1","request":{"subtype":"can_use_tool","tool_name":"ExitPlanMode","input":{"plan":"# First plan"}}}"##,
        );
        assert!(matches!(
            first.as_slice(),
            [AgentEvent::ProposedPlan { .. }]
        ));
        assert_eq!(m.take_outgoing().len(), 1);

        let second = feed(
            &mut m,
            r##"{"type":"control_request","request_id":"req-plan-2","request":{"subtype":"can_use_tool","tool_name":"ExitPlanMode","input":{"plan":"# Retried plan"}}}"##,
        );
        assert!(
            second.is_empty(),
            "a retry in the same turn must not emit another ProposedPlan"
        );
        assert_eq!(m.take_outgoing().len(), 1, "the retry is still denied");

        m.start_turn();
        let next_turn = feed(
            &mut m,
            r##"{"type":"control_request","request_id":"req-plan-3","request":{"subtype":"can_use_tool","tool_name":"ExitPlanMode","input":{"plan":"# New turn plan"}}}"##,
        );
        assert!(matches!(
            next_turn.as_slice(),
            [AgentEvent::ProposedPlan { .. }]
        ));
        assert_eq!(m.take_outgoing().len(), 1);
    }

    #[test]
    fn user_message_carries_image_content_blocks() {
        let attachments = vec![
            Attachment {
                media_type: "image/png".into(),
                data_base64: "AAAA".into(),
                source_path: None,
            },
            Attachment {
                media_type: "image/jpeg".into(),
                data_base64: "BBBB".into(),
                source_path: None,
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
        let first = match turn_completed_after_count(&evs, 0) {
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
        let second = match turn_completed_after_count(&evs, 0) {
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

    /// Captured from a real session: the CLI splits one message across several
    /// `assistant` lines, each carrying a single-element `content` array — a
    /// (redacted, empty) thinking block first, then the text block. Enumerating
    /// each array on its own numbered the text block 0 while its deltas streamed
    /// under index 1, so the timeline rendered the paragraph twice: once live,
    /// once again from the completion. The completed item must land on the
    /// stream's id, and the empty thinking block must not become an item at all.
    #[test]
    fn split_assistant_lines_keep_the_streams_block_numbering() {
        let mut m = Mapper::new();
        feed(
            &mut m,
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_s"}}}"#,
        );
        let streamed = feed(
            &mut m,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Added percent_off."}}}"#,
        );
        let streamed_id = match &streamed[0] {
            AgentEvent::Delta { item_id, .. } => item_id.clone(),
            other => panic!("expected Delta, got {other:?}"),
        };
        assert_eq!(streamed_id, "msg_s:1");

        // First `assistant` line: the thinking block alone, with no content.
        let thinking = feed(
            &mut m,
            r#"{"type":"assistant","message":{"id":"msg_s","content":[{"type":"thinking","thinking":""}]}}"#,
        );
        assert!(
            thinking.is_empty(),
            "an empty thinking block must not blank the streamed reasoning: {thinking:?}"
        );

        // Second `assistant` line: the text block, which the stream called 1.
        let completed = feed(
            &mut m,
            r#"{"type":"assistant","message":{"id":"msg_s","content":[{"type":"text","text":"Added percent_off."}]}}"#,
        );
        match &completed[0] {
            AgentEvent::ItemCompleted(item) => {
                assert_eq!(
                    item.id, streamed_id,
                    "completion must reuse the streamed item id, or the text renders twice"
                );
            }
            other => panic!("expected ItemCompleted, got {other:?}"),
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
    fn background_bash_result_completes_turn_immediately() {
        let mut m = Mapper::new();
        let turn_id = m.start_turn();

        let started = feed(
            &mut m,
            r#"{"type":"assistant","message":{"id":"msg-bg","content":[{"type":"tool_use","id":"toolu-bg","name":"Bash","input":{"command":"sleep 30","run_in_background":true}}]}}"#,
        );
        assert!(matches!(
            &started[0],
            AgentEvent::ItemStarted(ThreadItem {
                content: ItemContent::CommandExecution {
                    status: ItemStatus::InProgress,
                    ..
                },
                ..
            })
        ));

        assert!(matches!(
            feed(
                &mut m,
                r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"bg-1","task_type":"local_bash","description":"sleep"}]}"#,
            )
            .as_slice(),
            [AgentEvent::BackgroundTasksChanged { count: 1 }]
        ));
        assert!(
            feed(
                &mut m,
                r#"{"type":"system","subtype":"task_started","task_id":"bg-1","tool_use_id":"toolu-bg","task_type":"local_bash"}"#,
            )
            .is_empty()
        );
        let running = feed(
            &mut m,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu-bg","content":"Command running in background with ID: bg-1"}]},"tool_use_result":{"backgroundTaskId":"bg-1"}}"#,
        );
        assert!(matches!(
            &running[0],
            AgentEvent::ItemUpdated(ThreadItem {
                content: ItemContent::CommandExecution {
                    status: ItemStatus::InProgress,
                    exit_code: None,
                    ..
                },
                ..
            })
        ));

        // Claude ends its model turn as soon as it launches the process. A
        // background command must not hold the canonical turn open.
        let result = feed(
            &mut m,
            r#"{"type":"result","subtype":"success","is_error":false}"#,
        );
        assert!(matches!(
            turn_completed_after_count(&result, 1),
            AgentEvent::TurnCompleted {
                turn_id: completed,
                status: TurnStatus::Completed,
                ..
            } if completed == &turn_id
        ));
    }

    #[test]
    fn background_bash_notification_completes_command_card() {
        let mut m = Mapper::new();
        feed(
            &mut m,
            r#"{"type":"assistant","message":{"id":"msg-bg","content":[{"type":"tool_use","id":"toolu-bg","name":"Bash","input":{"command":"sleep 30","run_in_background":true}}]}}"#,
        );
        feed(
            &mut m,
            r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"bg-1","task_type":"local_bash"}]}"#,
        );
        feed(
            &mut m,
            r#"{"type":"system","subtype":"task_started","task_id":"bg-1","tool_use_id":"toolu-bg","task_type":"local_bash"}"#,
        );
        feed(
            &mut m,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu-bg","content":"Command running in background with ID: bg-1"}]},"tool_use_result":{"backgroundTaskId":"bg-1"}}"#,
        );
        feed(
            &mut m,
            r#"{"type":"system","subtype":"task_updated","task_id":"bg-1","patch":{"status":"completed"}}"#,
        );

        let finished = feed(
            &mut m,
            r#"{"type":"system","subtype":"task_notification","task_id":"bg-1","tool_use_id":"toolu-bg","status":"completed","summary":"sleep finished"}"#,
        );
        assert!(matches!(
            &finished[0],
            AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::CommandExecution {
                    status: ItemStatus::Completed,
                    exit_code: Some(0),
                    ..
                },
                ..
            })
        ));
    }

    #[test]
    fn reinvocation_after_background_task_synthesizes_turn() {
        let mut m = Mapper::new();
        feed(
            &mut m,
            r#"{"type":"system","subtype":"init","session_id":"session-bg","model":"claude-haiku-4-5-20251001"}"#,
        );
        let first_turn_id = m.start_turn();

        feed(
            &mut m,
            r#"{"type":"assistant","message":{"id":"msg-bg","content":[{"type":"tool_use","id":"toolu-bg","name":"Bash","input":{"command":"sleep 8 && echo BG_DONE","description":"Launch background sleep command","run_in_background":true}}]}}"#,
        );
        feed(
            &mut m,
            r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"bg-1","task_type":"local_bash","description":"Launch background sleep command"}]}"#,
        );
        feed(
            &mut m,
            r#"{"type":"system","subtype":"task_started","task_id":"bg-1","tool_use_id":"toolu-bg","description":"Launch background sleep command","task_type":"local_bash"}"#,
        );
        feed(
            &mut m,
            r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu-bg","type":"tool_result","content":"Command running in background with ID: bg-1","is_error":false}]},"tool_use_result":{"stdout":"","stderr":"","interrupted":false,"backgroundTaskId":"bg-1"}}"#,
        );
        let first_result = feed(
            &mut m,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Background command launched."}"#,
        );
        assert!(matches!(
            turn_completed_after_count(&first_result, 1),
            AgentEvent::TurnCompleted {
                turn_id,
                status: TurnStatus::Completed,
                ..
            } if turn_id == &first_turn_id
        ));

        assert!(
            feed(
                &mut m,
                r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#,
            )
            .is_empty()
        );
        assert!(
            feed(
                &mut m,
                r#"{"type":"system","subtype":"task_updated","task_id":"bg-1","patch":{"status":"completed","end_time":1784396058581}}"#,
            )
            .is_empty()
        );
        let notification = feed(
            &mut m,
            r#"{"type":"system","subtype":"task_notification","task_id":"bg-1","tool_use_id":"toolu-bg","status":"completed","output_file":"/tmp/bg-1.output","summary":"Background command completed (exit code 0)"}"#,
        );
        assert_eq!(notification.len(), 1);
        assert!(matches!(
            &notification[0],
            AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::CommandExecution {
                    status: ItemStatus::Completed,
                    exit_code: Some(0),
                    ..
                },
                ..
            })
        ));
        assert!(
            feed(
                &mut m,
                r#"{"type":"system","subtype":"init","session_id":"session-bg","model":"claude-haiku-4-5-20251001"}"#,
            )
            .is_empty()
        );

        let reinvoked = feed(
            &mut m,
            r#"{"type":"assistant","message":{"id":"msg-reinvoked","content":[{"type":"text","text":"Background command completed successfully."}]}}"#,
        );
        let synthesized_turn_id = match &reinvoked[0] {
            AgentEvent::TurnStarted { turn_id } => turn_id.clone(),
            other => panic!("expected synthesized TurnStarted first, got {other:?}"),
        };
        assert_ne!(synthesized_turn_id, first_turn_id);
        assert!(matches!(
            &reinvoked[1],
            AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::AssistantMessage { text },
                ..
            }) if text == "Background command completed successfully."
        ));

        let second_result = feed(
            &mut m,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Background command completed successfully.","origin":{"kind":"task-notification"}}"#,
        );
        assert!(matches!(
            &second_result[0],
            AgentEvent::BackgroundTasksChanged { count: 0 }
        ));
        assert!(matches!(
            &second_result[1],
            AgentEvent::TurnCompleted {
                turn_id,
                status: TurnStatus::Completed,
                ..
            } if turn_id == &synthesized_turn_id
        ));
    }

    /// The zero-count publication must not depend on the CLI's `origin.kind`
    /// shape: after `background_tasks_changed []` (whose zero is deliberately
    /// withheld), ANY later result must tell the runtime the tasks are gone —
    /// otherwise the session sits at "Working" forever on a live idle process.
    #[test]
    fn any_result_publishes_zero_after_background_tasks_drain() {
        let mut m = Mapper::new();
        m.start_turn();
        feed(
            &mut m,
            r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"bg-1","task_type":"local_bash","description":"sleep"}]}"#,
        );
        let first = feed(
            &mut m,
            r#"{"type":"result","subtype":"success","is_error":false}"#,
        );
        turn_completed_after_count(&first, 1);

        // The drain event withholds its zero (parking guard)...
        assert!(
            feed(
                &mut m,
                r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#,
            )
            .is_empty()
        );
        // ...so a later ordinary result — no `origin` at all — must publish it.
        m.start_turn();
        let second = feed(
            &mut m,
            r#"{"type":"result","subtype":"success","is_error":false}"#,
        );
        turn_completed_after_count(&second, 0);
    }

    #[test]
    fn unknown_task_notification_without_tool_use_id_does_not_wedge_turn() {
        let mut m = Mapper::new();
        let turn_id = m.start_turn();

        assert!(
            feed(
                &mut m,
                r#"{"type":"system","subtype":"task_notification","task_id":"unknown-task","status":"completed"}"#,
            )
            .is_empty()
        );
        let result = feed(
            &mut m,
            r#"{"type":"result","subtype":"success","is_error":false}"#,
        );
        assert!(matches!(
            turn_completed_after_count(&result, 0),
            AgentEvent::TurnCompleted {
                turn_id: completed,
                status: TurnStatus::Completed,
                ..
            } if completed == &turn_id
        ));
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
    fn empty_structured_patch_preserves_external_file_diff() {
        let mut mapper = Mapper::new();
        let started = feed(
            &mut mapper,
            r#"{"type":"assistant","message":{"id":"msg-external","content":[{"type":"tool_use","id":"toolu-external","name":"Write","input":{"file_path":"/tmp/tcode-outside-workspace.txt","content":"visible diff\n"}}]}}"#,
        );
        assert!(matches!(
            &started[0],
            AgentEvent::ItemStarted(ThreadItem {
                content: ItemContent::FileChange { changes, .. },
                ..
            }) if changes[0].diff.as_deref() == Some("+visible diff")
        ));

        let completed = feed(
            &mut mapper,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu-external","content":"File created successfully"}]},"tool_use_result":{"type":"create","filePath":"/tmp/tcode-outside-workspace.txt","content":"visible diff\n","structuredPatch":[]}}"#,
        );

        assert!(matches!(
            &completed[0],
            AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::FileChange { changes, status },
                ..
            }) if *status == ItemStatus::Completed
                && changes[0].path == "/tmp/tcode-outside-workspace.txt"
                && changes[0].diff.as_deref() == Some("+visible diff")
        ));
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
        m.approval_mode = ApprovalMode::FullAccess;
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
    fn read_only_auto_allows_file_reads_without_approval() {
        let mut m = Mapper::new();
        m.approval_mode = ApprovalMode::ReadOnly;
        let evs = feed(
            &mut m,
            r#"{"type":"control_request","request_id":"req-ro-read","request":{"subtype":"can_use_tool","tool_name":"Read","input":{"file_path":"/tmp/a.txt"}}}"#,
        );
        assert!(evs.is_empty(), "read-only file read emitted an approval");
        assert!(m.pending_approvals.is_empty());
        let outgoing = m.take_outgoing();
        assert_eq!(outgoing.len(), 1);
        assert_eq!(outgoing[0]["response"]["request_id"], "req-ro-read");
        assert_eq!(outgoing[0]["response"]["response"]["behavior"], "allow");
        assert_eq!(
            outgoing[0]["response"]["response"]["updatedInput"]["file_path"],
            "/tmp/a.txt"
        );
    }

    #[test]
    fn read_only_file_write_still_requests_approval() {
        let mut m = Mapper::new();
        m.approval_mode = ApprovalMode::ReadOnly;
        let evs = feed(
            &mut m,
            r#"{"type":"control_request","request_id":"req-ro-write","request":{"subtype":"can_use_tool","tool_name":"Write","input":{"file_path":"/tmp/a.txt","content":"changed"}}}"#,
        );
        assert!(matches!(
            evs.as_slice(),
            [AgentEvent::ApprovalRequested(ApprovalRequest {
                id,
                kind: ApprovalKind::FileChange { .. },
                ..
            })] if id == "req-ro-write"
        ));
        assert!(m.take_outgoing().is_empty());
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
        match turn_completed_after_count(&evs, 0) {
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
        // A failed turn discloses the CLI's raw result text before the marker.
        assert!(matches!(
            &idle_result[0],
            AgentEvent::Error { message, fatal: false }
                if message == "claude turn failed (error_during_execution): provider failed"
        ));
        assert!(matches!(
            turn_completed_after_count(&idle_result, 0),
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
            turn_completed_after_count(&attributed, 0),
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
        match turn_completed_after_count(&evs, 0) {
            AgentEvent::TurnCompleted { status, .. } => {
                assert_eq!(*status, TurnStatus::Interrupted)
            }
            other => panic!("expected TurnCompleted, got {other:?}"),
        }
    }

    #[test]
    fn permission_mode_flag_maps_all_modes() {
        assert_eq!(permission_mode_flag(ApprovalMode::Supervised), "default");
        assert_eq!(permission_mode_flag(ApprovalMode::ReadOnly), "default");
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
        let req =
            m.set_permission_mode_request_str(permission_mode_flag(ApprovalMode::AutoAcceptEdits));
        let request_id = req["request_id"].as_str().unwrap().to_owned();
        assert_eq!(req["type"], "control_request");
        assert!(req["request_id"].is_string());
        assert_eq!(req["request"]["subtype"], "set_permission_mode");
        assert_eq!(req["request"]["mode"], "acceptEdits");
        assert_eq!(m.applied_permission_mode, "default");

        let events = m.on_message(json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": {},
            }
        }));
        assert!(events.is_empty());
        assert_eq!(m.applied_permission_mode, "acceptEdits");

        // FullAccess maps to bypassPermissions on the wire.
        let req = m.set_permission_mode_request_str(permission_mode_flag(ApprovalMode::FullAccess));
        assert_eq!(req["request"]["mode"], "bypassPermissions");
        let events = m.on_message(json!({
            "type": "control_response",
            "response": {
                "subtype": "error",
                "request_id": req["request_id"],
                "error": "unsupported mode",
            }
        }));
        assert!(matches!(
            events.as_slice(),
            [AgentEvent::Warning { message }]
                if message.contains("unsupported mode")
        ));
        assert_eq!(m.applied_permission_mode, "acceptEdits");
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

    #[test]
    fn subagent_fixture_maps_lifecycle_and_parented_activity() {
        let trace = include_str!("../tests/fixtures/claude/subagent_trace.jsonl");
        let mut mapper = Mapper::new();
        let mut events = Vec::new();
        for line in trace.lines() {
            events.extend(feed(&mut mapper, line));
        }

        let spawn_events: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ItemStarted(item)
                | AgentEvent::ItemUpdated(item)
                | AgentEvent::ItemCompleted(item)
                    if item.id == "toolu_spawn_1" =>
                {
                    Some(item)
                }
                _ => None,
            })
            .collect();
        assert_eq!(spawn_events.len(), 5);
        assert!(matches!(
            &spawn_events[0].content,
            ItemContent::Subagent { agent_type, description, status: ItemStatus::InProgress, summary: None }
                if agent_type == "general-purpose" && description == "Ping test"
        ));
        assert!(spawn_events.iter().any(|item| matches!(
            &item.content,
            ItemContent::Subagent { status: ItemStatus::Completed, summary: Some(summary), .. }
                if summary == "pong"
        )));
        assert!(matches!(
            &spawn_events.last().unwrap().content,
            ItemContent::Subagent { status: ItemStatus::Completed, summary: Some(summary), .. }
                if summary == "pong"
        ));

        let children: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ItemStarted(item)
                | AgentEvent::ItemUpdated(item)
                | AgentEvent::ItemCompleted(item)
                    if item.parent_item_id.as_deref() == Some("toolu_spawn_1") =>
                {
                    Some(item)
                }
                _ => None,
            })
            .collect();
        assert_eq!(children.len(), 1);
        assert!(matches!(
            &children[0].content,
            ItemContent::UserMessage { text, .. } if text.contains("Reply with pong")
        ));
        assert!(events.iter().all(|event| match event {
            AgentEvent::ItemStarted(item)
            | AgentEvent::ItemUpdated(item)
            | AgentEvent::ItemCompleted(item) => {
                item.id == "toolu_spawn_1" || item.parent_item_id.is_some()
            }
            _ => true,
        }));
    }

    #[test]
    fn turn_acceptance_requires_a_flushed_write_and_steer_failures_remain_fatal() {
        smol::block_on(async {
            use std::io;
            use std::pin::Pin;
            use std::task::{Context, Poll};

            #[derive(Clone, Copy, PartialEq, Eq)]
            enum FailurePoint {
                Write,
                Flush,
            }

            #[derive(Default)]
            struct DeterministicWriter {
                failure: Option<FailurePoint>,
                bytes: Vec<u8>,
            }

            impl AsyncWrite for DeterministicWriter {
                fn poll_write(
                    mut self: Pin<&mut Self>,
                    _cx: &mut Context<'_>,
                    bytes: &[u8],
                ) -> Poll<io::Result<usize>> {
                    if self.failure == Some(FailurePoint::Write) {
                        Poll::Ready(Err(io::Error::other("deterministic write failure")))
                    } else {
                        self.bytes.extend_from_slice(bytes);
                        Poll::Ready(Ok(bytes.len()))
                    }
                }

                fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                    if self.failure == Some(FailurePoint::Flush) {
                        Poll::Ready(Err(io::Error::other("deterministic flush failure")))
                    } else {
                        Poll::Ready(Ok(()))
                    }
                }

                fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                    Poll::Ready(Ok(()))
                }
            }

            let message = user_message("deliver", &[]);
            let (event_tx, event_rx) = smol::channel::unbounded();
            let mut writer = DeterministicWriter::default();
            write_turn_message(&mut writer, &message, 42, &event_tx)
                .await
                .unwrap();
            assert!(matches!(
                event_rx.recv().await.unwrap(),
                AgentEvent::TurnAccepted { delivery_id: 42 }
            ));
            assert_eq!(writer.bytes.last(), Some(&b'\n'));

            let (event_tx, event_rx) = smol::channel::unbounded();
            let mut writer = DeterministicWriter {
                failure: Some(FailurePoint::Flush),
                ..Default::default()
            };
            assert!(
                write_turn_message(&mut writer, &message, 43, &event_tx)
                    .await
                    .is_err()
            );
            assert!(event_rx.try_recv().is_err(), "failed flush must not ack");

            let (event_tx, event_rx) = smol::channel::unbounded();
            let mut writer = DeterministicWriter::default();
            let mut mapper = Mapper::new();
            let message = user_message("redirect", &[]);
            write_steering_message(
                &mut writer,
                &message,
                "steer-ok".into(),
                &mut mapper,
                &event_tx,
            )
            .await;
            assert!(event_rx.try_recv().is_err());
            assert_eq!(mapper.pending_steers, VecDeque::from(["steer-ok".into()]));
            assert_eq!(writer.bytes.last(), Some(&b'\n'));

            let (event_tx, event_rx) = smol::channel::unbounded();
            let mut mapper = Mapper::new();
            let mut writer = DeterministicWriter {
                failure: Some(FailurePoint::Write),
                ..Default::default()
            };
            write_steering_message(
                &mut writer,
                &message,
                "steer-write-failed".into(),
                &mut mapper,
                &event_tx,
            )
            .await;
            assert!(matches!(
                event_rx.recv().await.unwrap(),
                AgentEvent::Error { ref message, fatal: true }
                    if message == "failed to write steering message to provider stdin"
            ));
            assert!(
                event_rx.try_recv().is_err(),
                "stdin write failures must not accept a steer"
            );

            let (event_tx, event_rx) = smol::channel::unbounded();
            let mut mapper = Mapper::new();
            let mut writer = DeterministicWriter {
                failure: Some(FailurePoint::Flush),
                ..Default::default()
            };
            write_steering_message(
                &mut writer,
                &message,
                "steer-flush-failed".into(),
                &mut mapper,
                &event_tx,
            )
            .await;
            assert!(matches!(
                event_rx.recv().await.unwrap(),
                AgentEvent::Error { ref message, fatal: true }
                    if message == "failed to write steering message to provider stdin"
            ));
            assert!(
                event_rx.try_recv().is_err(),
                "stdin flush failures must not accept a steer"
            );
        });
    }

    fn accepted_request_ids(events: &[AgentEvent]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::SteerAccepted { request_id } => Some(request_id.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn pending_steer_waits_for_requesting_checkpoint() {
        let mut mapper = Mapper::new();
        mapper.pending_steers.push_back("steer-1".into());

        assert_eq!(mapper.pending_steers.len(), 1);
        let events = feed(
            &mut mapper,
            r#"{"type":"system","subtype":"status","status":"requesting","uuid":"checkpoint-1","session_id":"session-1"}"#,
        );
        assert_eq!(accepted_request_ids(&events), ["steer-1"]);
        assert!(mapper.pending_steers.is_empty());
    }

    #[test]
    fn requesting_accepts_multiple_pending_steers_in_fifo_order() {
        let mut mapper = Mapper::new();
        mapper.pending_steers.push_back("steer-first".into());
        mapper.pending_steers.push_back("steer-second".into());

        let events = feed(
            &mut mapper,
            r#"{"type":"system","subtype":"status","status":"requesting","uuid":"checkpoint-2","session_id":"session-1"}"#,
        );
        assert_eq!(
            accepted_request_ids(&events),
            ["steer-first", "steer-second"]
        );
    }

    #[test]
    fn result_keeps_pending_steer_for_follow_up_requesting() {
        let mut mapper = Mapper::new();
        mapper.pending_steers.push_back("steer-late".into());

        let result_events = feed(
            &mut mapper,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"first response","session_id":"session-1","usage":{"input_tokens":2,"output_tokens":3}}"#,
        );
        assert!(accepted_request_ids(&result_events).is_empty());
        assert_eq!(mapper.pending_steers.len(), 1);

        let requesting_events = feed(
            &mut mapper,
            r#"{"type":"system","subtype":"status","status":"requesting","uuid":"follow-up-checkpoint","session_id":"session-1"}"#,
        );
        assert_eq!(accepted_request_ids(&requesting_events), ["steer-late"]);
    }

    #[test]
    fn assistant_accepts_pending_steer_for_legacy_cli_fallback() {
        let mut mapper = Mapper::new();
        mapper.pending_steers.push_back("steer-legacy".into());

        let events = feed(
            &mut mapper,
            r#"{"type":"assistant","message":{"model":"claude-opus-4-8","id":"msg_legacy","type":"message","role":"assistant","content":[{"type":"text","text":"consumed"}]},"session_id":"session-legacy","request_id":"req_legacy"}"#,
        );
        assert_eq!(accepted_request_ids(&events), ["steer-legacy"]);
    }

    #[test]
    fn assistant_does_not_fallback_after_requesting_was_observed() {
        let mut mapper = Mapper::new();
        assert!(
            feed(
                &mut mapper,
                r#"{"type":"system","subtype":"status","status":"requesting","uuid":"checkpoint-early","session_id":"session-1"}"#,
            )
            .is_empty()
        );
        mapper
            .pending_steers
            .push_back("steer-after-checkpoint".into());

        let events = feed(
            &mut mapper,
            r#"{"type":"assistant","message":{"model":"claude-opus-4-8","id":"msg_current","type":"message","role":"assistant","content":[{"type":"text","text":"current response"}]},"session_id":"session-1","request_id":"req_current"}"#,
        );
        assert!(accepted_request_ids(&events).is_empty());
        assert_eq!(mapper.pending_steers.len(), 1);
    }

    #[test]
    fn assistant_served_model_is_emitted_once_per_change() {
        let mut mapper = Mapper::new();
        let line = r#"{"type":"assistant","message":{"model":"claude-sonnet-4","id":"msg-1","content":[]}}"#;
        let first = feed(&mut mapper, line);
        let repeat = feed(&mut mapper, line);
        assert!(matches!(
            first.as_slice(),
            [AgentEvent::ServedModel { model, reason: None }] if model == "claude-sonnet-4"
        ));
        assert!(repeat.is_empty());
    }

    #[test]
    fn refusal_stop_reason_warns_once_across_delta_and_assistant() {
        let mut mapper = Mapper::new();
        let delta = feed(
            &mut mapper,
            r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"refusal"}}}"#,
        );
        let assistant = feed(
            &mut mapper,
            r#"{"type":"assistant","message":{"id":"msg-1","stop_reason":"refusal","content":[]}}"#,
        );
        assert!(matches!(
            delta.as_slice(),
            [AgentEvent::Warning { message }]
                if message == "Request declined by safety classifiers (stop_reason: refusal)"
        ));
        assert!(assistant.is_empty());
    }

    #[test]
    fn successful_result_that_mentions_cancel_is_completed() {
        let mut mapper = Mapper::new();
        mapper.start_turn();
        let events = feed(
            &mut mapper,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"You can cancel the operation from Settings."}"#,
        );
        assert!(matches!(
            turn_completed_after_count(&events, 0),
            AgentEvent::TurnCompleted {
                status: TurnStatus::Completed,
                ..
            }
        ));
    }

    #[test]
    fn structured_patch_renders_unified_diff() {
        let patch = json!([{
            "oldStart": 3,
            "oldLines": 2,
            "newStart": 3,
            "newLines": 2,
            "lines": [" keep", "-old", "+new"]
        }]);
        assert_eq!(
            render_structured_patch(&patch).as_deref(),
            Some("@@ -3,2 +3,2 @@\n keep\n-old\n+new")
        );
    }

    #[test]
    fn result_cost_and_duration_land_in_completed_usage() {
        let mut mapper = Mapper::new();
        mapper.start_turn();
        let events = feed(
            &mut mapper,
            r#"{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.125,"duration_ms":4321,"usage":{"input_tokens":10,"output_tokens":2}}"#,
        );
        assert!(matches!(
            turn_completed_after_count(&events, 0),
            AgentEvent::TurnCompleted {
                usage: Some(TokenUsage {
                    cost_usd: Some(cost),
                    duration_ms: Some(4321),
                    ..
                }),
                ..
            } if (*cost - 0.125).abs() < f64::EPSILON
        ));
    }
}
