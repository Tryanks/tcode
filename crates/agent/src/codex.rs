//! Codex provider: a small client for the newline-delimited JSON protocol used
//! by `codex app-server`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};

use async_channel::{Receiver, Sender};
use futures_lite::future;
use serde_json::{Value, json};

use crate::{
    AgentError, AgentEvent, ApprovalDecision, ApprovalKind, ApprovalMode, ApprovalRequest,
    Attachment, DeltaKind, FileChange, FileChangeKind, InteractionMode, ItemContent, ItemStatus,
    LaunchEnv, ModelSpec, OptionDescriptor, OptionSelection, PlanStep, PlanStepStatus,
    ProviderCommand, ProviderCommandKind, ProviderKind, ResumeCursor, SelectOption, SessionCommand,
    SessionHandle, SessionOptions, ThreadItem, TokenUsage, TurnOptions, TurnStatus,
    UserInputOption, UserInputQuestion,
};

mod developer_instructions;
use developer_instructions::{default_mode_instructions, plan_mode_instructions};

/// Fallback model slug for `collaborationMode.settings.model` when the session
/// has no resolved model yet (mirrors T3's `DEFAULT_MODEL`).
const DEFAULT_MODEL: &str = "gpt-5-codex";

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

// ---------------------------------------------------------------------------
// Model catalog (`model/list`)
// ---------------------------------------------------------------------------

/// Spawn `codex app-server`, page through `model/list`, and tear the process
/// down. Mirrors T3's `requestAllCodexModels` (initial `{}`, then `{cursor}`
/// until `nextCursor` is empty).
pub async fn list_models(
    binary_path: Option<PathBuf>,
    launch_env: LaunchEnv,
) -> Result<Vec<ModelSpec>, AgentError> {
    let (mut child, mut stdin, lines) = spawn_server(binary_path.as_deref(), &[], &launch_env)?;
    let result = collect_models(&mut stdin, &lines).await;
    stop_child(&mut child, stdin);
    result
}

async fn collect_models(
    stdin: &mut BufWriter<ChildStdin>,
    lines: &Receiver<ChildOutput>,
) -> Result<Vec<ModelSpec>, AgentError> {
    send_json(
        stdin,
        &json!({
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": { "name": "tcode", "title": "tcode", "version": env!("CARGO_PKG_VERSION") },
                "capabilities": { "experimentalApi": true }
            }
        }),
    )?;
    wait_for_response(lines, 1).await?;
    send_json(stdin, &json!({ "method": "initialized" }))?;

    let mut models = Vec::new();
    let mut cursor: Option<String> = None;
    let mut id = 2;
    loop {
        let params = match &cursor {
            Some(cursor) => json!({ "cursor": cursor }),
            None => json!({}),
        };
        send_json(
            stdin,
            &json!({ "id": id, "method": "model/list", "params": params }),
        )?;
        let response = wait_for_response(lines, id).await?;
        id += 1;
        if let Some(data) = response.get("data").and_then(Value::as_array) {
            for model in data {
                if let Some(spec) = map_model(model) {
                    models.push(spec);
                }
            }
        }
        cursor = response
            .get("nextCursor")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    Ok(models)
}

/// Map one `model/list` entry to a [`ModelSpec`]; `None` for hidden models.
fn map_model(model: &Value) -> Option<ModelSpec> {
    if model
        .get("hidden")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    let id = model.get("model").and_then(Value::as_str)?.to_owned();
    let display_name = codex_display_name(
        model
            .get("displayName")
            .and_then(Value::as_str)
            .unwrap_or(&id),
    );
    let is_default = model
        .get("isDefault")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut options = Vec::new();

    if let Some(efforts) = model
        .get("supportedReasoningEfforts")
        .and_then(Value::as_array)
    {
        let select_options: Vec<SelectOption> = efforts
            .iter()
            .filter_map(|entry| {
                let value = entry
                    .get("reasoningEffort")
                    .and_then(Value::as_str)?
                    .to_owned();
                let label = reasoning_effort_label(&value);
                let description = entry
                    .get("description")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                Some(SelectOption {
                    value,
                    label,
                    description,
                })
            })
            .collect();
        if !select_options.is_empty() {
            let default_value = model
                .get("defaultReasoningEffort")
                .and_then(Value::as_str)
                .map(str::to_owned);
            options.push(OptionDescriptor::Select {
                id: "reasoningEffort".into(),
                label: "Reasoning".into(),
                options: select_options,
                default_value,
            });
        }
    }

    let tiers = service_tiers(model);
    if !tiers.is_empty() {
        let catalog_default = model
            .get("defaultServiceTier")
            .and_then(Value::as_str)
            .filter(|d| tiers.iter().any(|t| t.value == *d))
            .map(str::to_owned);
        let default_value = catalog_default.unwrap_or_else(|| "default".into());
        let mut select_options = Vec::with_capacity(tiers.len() + 1);
        select_options.push(SelectOption {
            value: "default".into(),
            label: "Standard".into(),
            description: None,
        });
        select_options.extend(tiers);
        options.push(OptionDescriptor::Select {
            id: "serviceTier".into(),
            label: "Service Tier".into(),
            options: select_options,
            default_value: Some(default_value),
        });
    }

    Some(ModelSpec {
        id,
        display_name,
        is_default,
        options,
    })
}

/// Derive service-tier options from `serviceTiers` (preferred) or, absent that,
/// `additionalSpeedTiers` (`fast` → `Fast`), matching T3's mapping.
fn service_tiers(model: &Value) -> Vec<SelectOption> {
    if let Some(tiers) = model.get("serviceTiers").and_then(Value::as_array) {
        if !tiers.is_empty() {
            return tiers
                .iter()
                .filter_map(|tier| {
                    let value = tier.get("id").and_then(Value::as_str)?.to_owned();
                    let label = tier
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or(&value)
                        .to_owned();
                    let description = tier
                        .get("description")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned);
                    Some(SelectOption {
                        value,
                        label,
                        description,
                    })
                })
                .collect();
        }
    }
    if let Some(speed) = model.get("additionalSpeedTiers").and_then(Value::as_array) {
        return speed
            .iter()
            .filter_map(|tier| {
                let value = tier.as_str()?.to_owned();
                let label = if value == "fast" {
                    "Fast".to_owned()
                } else {
                    value.clone()
                };
                Some(SelectOption {
                    value,
                    label,
                    description: None,
                })
            })
            .collect();
    }
    Vec::new()
}

/// `gpt…` → `GPT…`, and capitalize the letter after each hyphen (T3 transform).
fn codex_display_name(raw: &str) -> String {
    let base = if raw
        .get(..3)
        .map(|p| p.eq_ignore_ascii_case("gpt"))
        .unwrap_or(false)
    {
        format!("GPT{}", &raw[3..])
    } else {
        raw.to_owned()
    };
    let mut out = String::with_capacity(base.len());
    let mut after_hyphen = false;
    for c in base.chars() {
        if after_hyphen && c.is_ascii_lowercase() {
            out.push(c.to_ascii_uppercase());
        } else {
            out.push(c);
        }
        after_hyphen = c == '-';
    }
    out
}

fn reasoning_effort_label(effort: &str) -> String {
    match effort {
        "none" => "None",
        "minimal" => "Minimal",
        "low" => "Low",
        "medium" => "Medium",
        "high" => "High",
        "xhigh" => "Extra High",
        "max" => "Max",
        "ultra" => "Ultra",
        other => return other.to_owned(),
    }
    .to_owned()
}

/// Read a string option value from the session's selections.
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

/// Selected reasoning effort (option id `reasoningEffort`).
fn codex_effort(selections: &[OptionSelection]) -> Option<String> {
    selection_str(selections, "reasoningEffort")
}

/// Selected service tier (option id `serviceTier`), with the legacy
/// `fastMode: true` → `fast` fallback (T3 `getCodexServiceTierOptionValue`).
fn codex_service_tier(selections: &[OptionSelection]) -> Option<String> {
    selection_str(selections, "serviceTier").or_else(|| {
        (selection_bool(selections, "fastMode") == Some(true)).then(|| "fast".to_owned())
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
    /// Resolved model slug; used for `collaborationMode.settings.model`.
    model: Option<String>,
    /// Session's Build/Plan mode; applied on the next `turn/start`.
    interaction_mode: InteractionMode,
    /// Session reasoning effort (`reasoningEffort` selection), if any.
    effort: Option<String>,
    /// Session service tier (`serviceTier` selection / legacy fastMode), if any.
    service_tier: Option<String>,
    next_id: i64,
    pending_requests: HashMap<i64, PendingRequest>,
    approvals: HashMap<String, Value>,
    /// Pending `item/tool/requestUserInput` requests: canonical request_id → the
    /// server-to-client JSON-RPC id we must reply to.
    user_inputs: HashMap<String, Value>,
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
    // Register the embedded preview MCP server (streamable HTTP) via a `-c`
    // config override so the agent can drive the in-app browser.
    let mut extra_args: Vec<String> = match &opts.mcp_server {
        Some(mcp) => vec!["-c".to_string(), mcp.codex_config_override()],
        None => Vec::new(),
    };
    // Any additional launch arguments configured for this provider.
    extra_args.extend(opts.extra_args.iter().cloned());
    let (mut child, mut stdin, lines) =
        match spawn_server(opts.binary_path.as_deref(), &extra_args, &opts.launch_env) {
            Ok(parts) => parts,
            Err(err) => {
                let _ = ready.send(Err(err)).await;
                return;
            }
        };

    let startup = initialize_and_open_thread(&opts, &mut stdin, &lines).await;
    let (thread_id, model, next_id, provider_commands) = match startup {
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
        model: model.clone(),
        interaction_mode: opts.interaction_mode,
        effort: codex_effort(&opts.option_selections),
        service_tier: codex_service_tier(&opts.option_selections),
        next_id,
        pending_requests: HashMap::new(),
        approvals: HashMap::new(),
        user_inputs: HashMap::new(),
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
    // Feed the composer's `$`-skill menu with the session's discovered skills.
    if !provider_commands.is_empty() {
        actor
            .emit(AgentEvent::ProviderCommands {
                commands: provider_commands,
            })
            .await;
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
            Input::Command(Ok(SessionCommand::Shutdown)) | Input::Command(Err(_)) => {
                actor.settle_pending_user_inputs_on_shutdown().await;
                break None;
            }
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
    binary_path: Option<&Path>,
    extra_args: &[String],
    launch_env: &LaunchEnv,
) -> Result<(Child, BufWriter<ChildStdin>, Receiver<ChildOutput>), AgentError> {
    // Absolute path: bare names break once a child sets its own cwd.
    let binary = crate::resolve_binary(binary_path, "codex")?;
    let mut cmd = crate::process::command(&binary);
    cmd.arg("app-server")
        .args(extra_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Per-provider environment (Settings → Providers): custom variables and the
    // `CODEX_HOME` override (the account-scoped "shadow home", when set).
    for (key, value) in launch_env.pairs(ProviderKind::Codex) {
        cmd.env(key, value);
    }
    if let Some(home) = &launch_env.home {
        // Codex refuses to start against a CODEX_HOME that does not exist.
        if let Err(err) = std::fs::create_dir_all(home) {
            log::warn!("could not create CODEX_HOME {}: {err}", home.display());
        }
    }
    let mut child = cmd
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
) -> Result<(String, Option<String>, i64, Vec<ProviderCommand>), AgentError> {
    send_json(
        stdin,
        &json!({
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": { "name": "tcode", "title": "tcode", "version": env!("CARGO_PKG_VERSION") },
                "capabilities": { "experimentalApi": true }
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
    if let Some(tier) = codex_service_tier(&opts.option_selections) {
        params["serviceTier"] = json!(tier);
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

    // Discover the session's skills for the composer's `$` menu. Supported since
    // codex 0.144.1 (verified live). A failure/omission is non-fatal: we log and
    // return an empty list so sessions still start on older builds.
    let mut next_id = 3;
    let provider_commands = match request_codex_skills(&opts.cwd, stdin, lines, next_id).await {
        Ok(commands) => {
            next_id += 1;
            commands
        }
        Err(err) => {
            log::debug!("codex skills/list unavailable: {err}");
            Vec::new()
        }
    };
    Ok((thread_id, model, next_id, provider_commands))
}

/// Query `skills/list` for `cwd` and map the entries into `Skill`-kind
/// [`ProviderCommand`]s. The response shape (verified against codex 0.144.1) is
/// `{data: [{cwd, skills: [{name, description, interface:{…}}]}]}`.
async fn request_codex_skills(
    cwd: &Path,
    stdin: &mut BufWriter<ChildStdin>,
    lines: &Receiver<ChildOutput>,
    id: i64,
) -> Result<Vec<ProviderCommand>, AgentError> {
    send_json(
        stdin,
        &json!({ "id": id, "method": "skills/list", "params": { "cwds": [cwd.to_string_lossy()] } }),
    )?;
    let result = wait_for_response(lines, id).await?;
    Ok(parse_codex_skills(&result))
}

/// Flatten a `skills/list` response into `Skill`-kind [`ProviderCommand`]s
/// (deduped by name, empty names dropped).
fn parse_codex_skills(result: &Value) -> Vec<ProviderCommand> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let Some(entries) = result.get("data").and_then(Value::as_array) else {
        return out;
    };
    for entry in entries {
        let Some(skills) = entry.get("skills").and_then(Value::as_array) else {
            continue;
        };
        for skill in skills {
            let Some(name) = skill.get("name").and_then(Value::as_str) else {
                continue;
            };
            let name = name.trim();
            if name.is_empty() || !seen.insert(name.to_owned()) {
                continue;
            }
            let description = skill
                .get("description")
                .and_then(Value::as_str)
                .or_else(|| {
                    skill
                        .pointer("/interface/shortDescription")
                        .and_then(Value::as_str)
                })
                .map(str::to_owned)
                .filter(|s| !s.is_empty());
            out.push(ProviderCommand {
                name: name.to_owned(),
                description,
                kind: ProviderCommandKind::Skill,
            });
        }
    }
    out
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

    /// Build `turn/start` params, applying per-turn overrides on top of the
    /// session's persisted effort / service tier / interaction mode. Mirrors
    /// T3's `buildTurnStartParams` + `buildCodexCollaborationMode`.
    fn build_turn_params(
        &self,
        text: &str,
        options: Option<&TurnOptions>,
        attachments: &[Attachment],
    ) -> Value {
        let effort = options
            .and_then(|o| o.effort.clone())
            .or_else(|| self.effort.clone());
        let mode = options
            .and_then(|o| o.interaction_mode)
            .unwrap_or(self.interaction_mode);

        // Text first, then one `image` input entry per attachment carrying a
        // `data:<mime>;base64,<data>` URL (the Codex app-server image-input shape).
        let mut input = vec![json!({ "type": "text", "text": text, "text_elements": [] })];
        for attachment in attachments {
            input.push(json!({
                "type": "image",
                "url": format!("data:{};base64,{}", attachment.media_type, attachment.data_base64),
            }));
        }
        let mut params = json!({
            "threadId": self.thread_id,
            "input": input,
        });
        if let Some(effort) = &effort {
            params["effort"] = json!(effort);
        }
        if let Some(tier) = &self.service_tier {
            params["serviceTier"] = json!(tier);
        }

        // Interaction mode is always present in our session model, so Codex
        // always carries `collaborationMode` (T3 sends it whenever the toggle
        // is exposed, which it is for Codex).
        let mode_str = match mode {
            InteractionMode::Build => "default",
            InteractionMode::Plan => "plan",
        };
        let developer_instructions = match mode {
            InteractionMode::Plan => plan_mode_instructions(),
            InteractionMode::Build => default_mode_instructions(),
        };
        let model = self
            .model
            .clone()
            .unwrap_or_else(|| DEFAULT_MODEL.to_owned());
        params["collaborationMode"] = json!({
            "mode": mode_str,
            "settings": {
                "model": model,
                "reasoning_effort": effort.clone().unwrap_or_else(|| "medium".to_owned()),
                "developer_instructions": developer_instructions,
            }
        });

        log::debug!(
            "codex turn/start: effort={:?} mode={} serviceTier={:?}",
            effort,
            mode_str,
            self.service_tier
        );
        params
    }

    async fn handle_command(&mut self, command: SessionCommand) -> Result<(), String> {
        match command {
            SessionCommand::SendTurn {
                text,
                options,
                attachments,
            } => {
                let params = self.build_turn_params(&text, options.as_ref(), &attachments);
                self.request("turn/start", params, PendingRequest::TurnStart)
            }
            SessionCommand::SetInteractionMode(mode) => {
                // Turn-scoped in the protocol: store it; it applies on the next
                // `turn/start.collaborationMode`.
                self.interaction_mode = mode;
                Ok(())
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
                // `cancel` is protocol-defined as deny + immediate turn
                // interruption (S2 §4.2); the others map 1:1.
                let wire_decision = match decision {
                    ApprovalDecision::Approve => "accept",
                    ApprovalDecision::ApproveForSession => "acceptForSession",
                    ApprovalDecision::Deny => "decline",
                    ApprovalDecision::Cancel => "cancel",
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
            SessionCommand::RespondUserInput {
                request_id,
                answers,
            } => {
                let Some(json_rpc_id) = self.user_inputs.remove(&request_id) else {
                    self.emit(AgentEvent::Warning(format!(
                        "unknown Codex user-input request id: {request_id}"
                    )))
                    .await;
                    return Ok(());
                };
                // Native result shape: `{answers: {<qid>: {answers: [<strings>]}}}`
                // — a single string is wrapped into a 1-element array (S2 §3.2).
                let mut wire_answers = serde_json::Map::new();
                for (qid, value) in &answers {
                    wire_answers.insert(qid.clone(), json!({ "answers": answer_strings(value) }));
                }
                send_json(
                    &mut self.stdin,
                    &json!({ "id": json_rpc_id, "result": { "answers": wire_answers } }),
                )
                .map_err(|e| e.to_string())?;
                self.emit(AgentEvent::UserInputResolved {
                    request_id,
                    answers,
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

        // Structured user-input request: map to a canonical UserInputRequested
        // and remember the JSON-RPC id so RespondUserInput can reply (S2 §3).
        if method == "item/tool/requestUserInput" {
            let questions = parse_codex_user_input(params);
            self.user_inputs.insert(key.clone(), id);
            self.emit(AgentEvent::UserInputRequested {
                request_id: key,
                questions,
            })
            .await;
            return;
        }

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
                let item_value = params.get("item");
                // Proposed-plan item (`ThreadItem::Plan { id, text }`): a
                // completed plan item is the finalized `<proposed_plan>` block.
                if item_value
                    .and_then(|i| i.get("type"))
                    .and_then(Value::as_str)
                    == Some("plan")
                {
                    if method == "item/completed" {
                        if let Some(item) = item_value {
                            let markdown = string_field(item, "text");
                            let item_id = string_field(item, "id");
                            self.emit(AgentEvent::ProposedPlan { item_id, markdown })
                                .await;
                        }
                    }
                    return;
                }
                // Context-compaction marker item (`type: "contextCompaction"`):
                // surface the "Context compacted" work-log row once, on completion.
                if item_value
                    .and_then(|i| i.get("type"))
                    .and_then(Value::as_str)
                    == Some("contextCompaction")
                {
                    if method == "item/completed" {
                        self.emit(AgentEvent::ContextCompacted).await;
                    }
                    return;
                }
                if let Some(item) = item_value.and_then(map_item) {
                    self.items.insert(item.id.clone(), item.clone());
                    let event = match method {
                        "item/started" => AgentEvent::ItemStarted(item),
                        "item/updated" => AgentEvent::ItemUpdated(item),
                        _ => AgentEvent::ItemCompleted(item),
                    };
                    self.emit(event).await;
                }
            }
            "turn/plan/updated" => {
                let turn_id = params
                    .get("turnId")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| self.active_turn.clone());
                let explanation = params
                    .get("explanation")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                let steps = params
                    .get("plan")
                    .and_then(Value::as_array)
                    .map(|steps| steps.iter().map(map_plan_step).collect())
                    .unwrap_or_default();
                self.emit(AgentEvent::PlanUpdated {
                    turn_id,
                    explanation,
                    steps,
                })
                .await;
            }
            "item/plan/delta" => {
                if let (Some(item_id), Some(text)) = (
                    params.get("itemId").and_then(Value::as_str),
                    params
                        .get("delta")
                        .and_then(Value::as_str)
                        .filter(|d| !d.is_empty()),
                ) {
                    self.emit(AgentEvent::ProposedPlanDelta {
                        item_id: item_id.to_owned(),
                        text: text.to_owned(),
                    })
                    .await;
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

    /// Settle every outstanding `requestUserInput` on teardown: reply with an
    /// empty `{answers: {}}` result and emit an empty resolution (S2 §4.2).
    async fn settle_pending_user_inputs_on_shutdown(&mut self) {
        let pending: Vec<(String, Value)> = self.user_inputs.drain().collect();
        for (request_id, rpc_id) in pending {
            let _ = send_json(
                &mut self.stdin,
                &json!({ "id": rpc_id, "result": { "answers": {} } }),
            );
            self.emit(AgentEvent::UserInputResolved {
                request_id,
                answers: serde_json::Map::new(),
            })
            .await;
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

/// Normalize a canonical answer value into the wire array shape. A single
/// string becomes a 1-element array; an array of strings is kept; anything
/// else yields an empty array (S2 §3.2).
fn answer_strings(value: &Value) -> Vec<String> {
    match value {
        Value::String(s) => vec![s.clone()],
        Value::Array(items) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

/// Map an `item/tool/requestUserInput` params object into canonical
/// [`UserInputQuestion`]s (S2 §3.1). Questions missing a non-empty
/// `id`/`header`/`question` are dropped, as are options with an empty label.
/// Unlike T3, questions with zero options are KEPT (with `options: []`) so a
/// free-text-only question still renders — a deliberate fix for T3's
/// dropped-question bug (S2 §2.2 limitations). `multiSelect` is forced false.
fn parse_codex_user_input(params: &Value) -> Vec<UserInputQuestion> {
    let questions = match params.get("questions").and_then(Value::as_array) {
        Some(q) => q,
        None => return Vec::new(),
    };
    questions
        .iter()
        .filter_map(|q| {
            let id = q
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())?;
            let header = q
                .get("header")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())?;
            let question = q
                .get("question")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())?;
            let options = q
                .get("options")
                .and_then(Value::as_array)
                .map(|opts| {
                    opts.iter()
                        .filter_map(|opt| {
                            let label = opt
                                .get("label")
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())?;
                            let description = opt
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            Some(UserInputOption {
                                label: label.to_owned(),
                                description: description.to_owned(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(UserInputQuestion {
                id: id.to_owned(),
                header: header.to_owned(),
                question: question.to_owned(),
                options,
                multi_select: false,
            })
        })
        .collect()
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

/// Map one `turn/plan/updated` step (status fallback `pending`, step text
/// fallback `"step"`), mirroring T3's CodexAdapter plan mapping.
fn map_plan_step(step: &Value) -> PlanStep {
    let text = step
        .get("step")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("step")
        .to_owned();
    let status = match step.get("status").and_then(Value::as_str) {
        Some("completed") => PlanStepStatus::Completed,
        Some("inProgress") => PlanStepStatus::InProgress,
        _ => PlanStepStatus::Pending,
    };
    PlanStep { step: text, status }
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
    // The session-cumulative running total lives in a sibling `total` object.
    let total_processed_tokens = value.pointer("/total/totalTokens").and_then(Value::as_u64);
    Some(TokenUsage {
        input_tokens: last.get("inputTokens").and_then(Value::as_u64),
        cached_input_tokens: last.get("cachedInputTokens").and_then(Value::as_u64),
        output_tokens: last.get("outputTokens").and_then(Value::as_u64),
        used_tokens: last.get("totalTokens").and_then(Value::as_u64),
        context_window: value.get("modelContextWindow").and_then(Value::as_u64),
        total_processed_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_actor() -> (Actor, Receiver<AgentEvent>) {
        let mut child = crate::process::command("cat")
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
                model: Some("gpt-5-codex".into()),
                interaction_mode: InteractionMode::Build,
                effort: None,
                service_tier: None,
                next_id: 1,
                pending_requests: HashMap::new(),
                approvals: HashMap::new(),
                user_inputs: HashMap::new(),
                items: HashMap::new(),
                usage_by_turn: HashMap::new(),
                active_turn: None,
            },
            event_rx,
        )
    }

    #[test]
    fn maps_model_list_entry_to_spec() {
        let spec = map_model(&json!({
            "model": "gpt-5-codex",
            "displayName": "gpt-5-codex",
            "isDefault": true,
            "supportedReasoningEfforts": [
                {"reasoningEffort": "low", "description": "fast"},
                {"reasoningEffort": "medium"},
                {"reasoningEffort": "xhigh"}
            ],
            "defaultReasoningEffort": "medium",
            "serviceTiers": [{"id": "flex", "name": "Flex", "description": "cheap"}],
            "defaultServiceTier": "flex"
        }))
        .expect("visible model maps");

        assert_eq!(spec.id, "gpt-5-codex");
        assert_eq!(spec.display_name, "GPT-5-Codex");
        assert!(spec.is_default);
        assert_eq!(spec.options.len(), 2);

        match &spec.options[0] {
            OptionDescriptor::Select {
                id,
                label,
                options,
                default_value,
            } => {
                assert_eq!(id, "reasoningEffort");
                assert_eq!(label, "Reasoning");
                assert_eq!(default_value.as_deref(), Some("medium"));
                assert_eq!(options[0].label, "Low");
                assert_eq!(options[0].description.as_deref(), Some("fast"));
                assert_eq!(options[2].label, "Extra High");
            }
            other => panic!("expected reasoning Select, got {other:?}"),
        }
        match &spec.options[1] {
            OptionDescriptor::Select {
                id,
                options,
                default_value,
                ..
            } => {
                assert_eq!(id, "serviceTier");
                assert_eq!(default_value.as_deref(), Some("flex"));
                assert_eq!(options[0].value, "default");
                assert_eq!(options[0].label, "Standard");
                assert_eq!(options[1].value, "flex");
                assert_eq!(options[1].label, "Flex");
            }
            other => panic!("expected serviceTier Select, got {other:?}"),
        }
    }

    #[test]
    fn hidden_model_is_skipped_and_speed_tiers_adapt() {
        assert!(
            map_model(&json!({"model": "secret", "displayName": "secret", "hidden": true}))
                .is_none()
        );

        // No serviceTiers → adapt additionalSpeedTiers (`fast` → `Fast`).
        let spec = map_model(&json!({
            "model": "gpt-x",
            "displayName": "gpt-x",
            "supportedReasoningEfforts": [],
            "additionalSpeedTiers": ["fast", "priority"]
        }))
        .unwrap();
        // Empty reasoning efforts → no reasoning descriptor, only serviceTier.
        assert_eq!(spec.options.len(), 1);
        match &spec.options[0] {
            OptionDescriptor::Select { id, options, .. } => {
                assert_eq!(id, "serviceTier");
                assert_eq!(options[1].value, "fast");
                assert_eq!(options[1].label, "Fast");
                assert_eq!(options[2].value, "priority");
                assert_eq!(options[2].label, "priority");
            }
            other => panic!("expected serviceTier Select, got {other:?}"),
        }
    }

    #[test]
    fn collaboration_mode_payload_shape() {
        let (mut actor, _events) = test_actor();
        actor.interaction_mode = InteractionMode::Plan;
        actor.effort = Some("high".into());
        actor.service_tier = Some("flex".into());
        actor.model = Some("gpt-5-codex".into());

        let params = actor.build_turn_params("hi", None, &[]);
        assert_eq!(params["effort"], "high");
        assert_eq!(params["serviceTier"], "flex");
        let collab = &params["collaborationMode"];
        assert_eq!(collab["mode"], "plan");
        assert_eq!(collab["settings"]["model"], "gpt-5-codex");
        assert_eq!(collab["settings"]["reasoning_effort"], "high");
        let instructions = collab["settings"]["developer_instructions"]
            .as_str()
            .unwrap();
        assert!(instructions.contains("# Plan Mode (Conversational)"));
        assert!(instructions.contains("<proposed_plan>"));
        assert!(instructions.trim_end().ends_with("</collaboration_mode>"));

        // Per-turn override to Build with no effort → default instructions and
        // the `medium` reasoning fallback.
        actor.effort = None;
        let opts = TurnOptions {
            effort: None,
            interaction_mode: Some(InteractionMode::Build),
        };
        let params = actor.build_turn_params("hi", Some(&opts), &[]);
        assert!(params.get("effort").is_none());
        assert_eq!(params["collaborationMode"]["mode"], "default");
        assert_eq!(
            params["collaborationMode"]["settings"]["reasoning_effort"],
            "medium"
        );
        assert!(
            params["collaborationMode"]["settings"]["developer_instructions"]
                .as_str()
                .unwrap()
                .contains("# Collaboration Mode: Default")
        );

        let _ = actor.child.kill();
        let _ = actor.child.wait();
    }

    #[test]
    fn turn_input_carries_image_entries() {
        let (mut actor, _events) = test_actor();
        actor.model = Some("gpt-5-codex".into());
        let attachments = vec![Attachment {
            media_type: "image/png".into(),
            data_base64: "AAAA".into(),
        }];
        let params = actor.build_turn_params("what color?", None, &attachments);
        let input = params["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["type"], "text");
        assert_eq!(input[0]["text"], "what color?");
        assert_eq!(input[1]["type"], "image");
        assert_eq!(input[1]["url"], "data:image/png;base64,AAAA");
        let _ = actor.child.kill();
        let _ = actor.child.wait();
    }

    #[test]
    fn skills_list_response_parses_to_provider_commands() {
        let result = json!({
            "data": [
                {
                    "cwd": "/tmp",
                    "skills": [
                        {"name": "browser:control", "description": "drive the browser", "interface": {"displayName": "Browser"}},
                        {"name": "", "description": "dropped"},
                        {"name": "dataviz", "interface": {"shortDescription": "charts"}}
                    ]
                },
                {"cwd": "/tmp", "skills": [{"name": "browser:control", "description": "dup dropped"}]}
            ]
        });
        let commands = parse_codex_skills(&result);
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].name, "browser:control");
        assert_eq!(commands[0].kind, ProviderCommandKind::Skill);
        assert_eq!(
            commands[0].description.as_deref(),
            Some("drive the browser")
        );
        // Falls back to interface.shortDescription when `description` is absent.
        assert_eq!(commands[1].name, "dataviz");
        assert_eq!(commands[1].description.as_deref(), Some("charts"));
    }

    #[test]
    fn token_usage_reads_total_processed() {
        let usage = map_usage(&json!({
            "last": {"inputTokens": 100, "outputTokens": 20, "totalTokens": 120},
            "total": {"totalTokens": 5000},
            "modelContextWindow": 200000
        }))
        .unwrap();
        assert_eq!(usage.used_tokens, Some(120));
        assert_eq!(usage.total_processed_tokens, Some(5000));
        assert_eq!(usage.context_window, Some(200000));
    }

    #[test]
    fn plan_notifications_map_to_events() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            actor.active_turn = Some("turn-9".into());

            actor
                .handle_notification(
                    "turn/plan/updated",
                    &json!({
                        "explanation": "  building  ",
                        "plan": [
                            {"step": "explore", "status": "completed"},
                            {"step": "  ", "status": "inProgress"},
                            {"status": "weird"}
                        ]
                    }),
                )
                .await;
            match events.recv().await.unwrap() {
                AgentEvent::PlanUpdated {
                    turn_id,
                    explanation,
                    steps,
                } => {
                    assert_eq!(turn_id.as_deref(), Some("turn-9"));
                    assert_eq!(explanation.as_deref(), Some("building"));
                    assert_eq!(steps[0].status, PlanStepStatus::Completed);
                    assert_eq!(steps[1].step, "step");
                    assert_eq!(steps[1].status, PlanStepStatus::InProgress);
                    assert_eq!(steps[2].step, "step");
                    assert_eq!(steps[2].status, PlanStepStatus::Pending);
                }
                other => panic!("expected PlanUpdated, got {other:?}"),
            }

            actor
                .handle_notification(
                    "item/plan/delta",
                    &json!({"itemId": "plan-1", "delta": "## Title"}),
                )
                .await;
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::ProposedPlanDelta { ref item_id, ref text } if item_id == "plan-1" && text == "## Title"
            ));

            actor
                .handle_notification(
                    "item/completed",
                    &json!({"item": {"type": "plan", "id": "plan-1", "text": "# Final plan"}}),
                )
                .await;
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::ProposedPlan { ref item_id, ref markdown } if item_id == "plan-1" && markdown == "# Final plan"
            ));

            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn service_tier_falls_back_to_fast_for_legacy_fast_mode() {
        let selections = vec![OptionSelection {
            id: "fastMode".into(),
            value: json!(true),
        }];
        assert_eq!(codex_service_tier(&selections).as_deref(), Some("fast"));
        let explicit = vec![OptionSelection {
            id: "serviceTier".into(),
            value: json!("flex"),
        }];
        assert_eq!(codex_service_tier(&explicit).as_deref(), Some("flex"));
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
    fn request_user_input_maps_questions_and_answer_wire_shape() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            // isOther/isSecret dropped; question missing header dropped; option
            // with empty label dropped; zero-option question KEPT (options: []).
            actor
                .handle_line(
                    &json!({
                        "jsonrpc": "2.0",
                        "id": 55,
                        "method": "item/tool/requestUserInput",
                        "params": {
                            "threadId": "t", "turnId": "turn-1", "itemId": "item-1",
                            "questions": [
                                {
                                    "id": "os",
                                    "header": "Target",
                                    "question": "macOS or Linux?",
                                    "options": [
                                        {"label": "macOS", "description": "apple"},
                                        {"label": "", "description": "skip me"}
                                    ],
                                    "isOther": true,
                                    "isSecret": false
                                },
                                { "id": "free", "header": "Notes", "question": "Any notes?" },
                                { "header": "no id", "question": "dropped?" }
                            ]
                        }
                    })
                    .to_string(),
                )
                .await;

            match events.recv().await.unwrap() {
                AgentEvent::UserInputRequested {
                    request_id,
                    questions,
                } => {
                    assert_eq!(request_id, "55");
                    assert_eq!(questions.len(), 2, "question missing id is dropped");
                    assert_eq!(questions[0].id, "os");
                    assert_eq!(questions[0].options.len(), 1, "empty-label option dropped");
                    assert_eq!(questions[0].options[0].label, "macOS");
                    assert!(!questions[0].multi_select);
                    // Free-text-only question kept with empty options (T3 bug fix).
                    assert_eq!(questions[1].id, "free");
                    assert!(questions[1].options.is_empty());
                }
                other => panic!("expected UserInputRequested, got {other:?}"),
            }

            // Answer: single string wraps into a 1-element array under {answers}.
            let mut answers = serde_json::Map::new();
            answers.insert("os".into(), json!("macOS"));
            answers.insert("free".into(), json!(["a", "b"]));
            actor
                .handle_command(SessionCommand::RespondUserInput {
                    request_id: "55".into(),
                    answers,
                })
                .await
                .unwrap();

            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::UserInputResolved { ref request_id, .. } if request_id == "55"
            ));
            let ChildOutput::Line(response) = actor.lines.recv().await.unwrap() else {
                panic!("expected echoed response")
            };
            let response: Value = serde_json::from_str(&response).unwrap();
            assert_eq!(response["id"], 55);
            assert_eq!(
                response["result"]["answers"]["os"]["answers"],
                json!(["macOS"])
            );
            assert_eq!(
                response["result"]["answers"]["free"]["answers"],
                json!(["a", "b"])
            );

            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn cancel_decision_maps_to_cancel_wire_string() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            actor.approvals.insert("41".into(), json!(41));
            actor
                .handle_command(SessionCommand::RespondApproval {
                    request_id: "41".into(),
                    decision: ApprovalDecision::Cancel,
                })
                .await
                .unwrap();
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::ApprovalResolved {
                    decision: ApprovalDecision::Cancel,
                    ..
                }
            ));
            let ChildOutput::Line(response) = actor.lines.recv().await.unwrap() else {
                panic!("expected echoed response")
            };
            let response: Value = serde_json::from_str(&response).unwrap();
            assert_eq!(
                response,
                json!({"id": 41, "result": {"decision": "cancel"}})
            );

            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn shutdown_settles_pending_user_input_empty() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            actor.user_inputs.insert("77".into(), json!(77));
            actor.settle_pending_user_inputs_on_shutdown().await;

            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::UserInputResolved { ref request_id, ref answers }
                    if request_id == "77" && answers.is_empty()
            ));
            let ChildOutput::Line(response) = actor.lines.recv().await.unwrap() else {
                panic!("expected echoed response")
            };
            let response: Value = serde_json::from_str(&response).unwrap();
            assert_eq!(response, json!({"id": 77, "result": {"answers": {}}}));
            assert!(actor.user_inputs.is_empty());

            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
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
