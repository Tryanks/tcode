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
//! CLI. Before the first message an `initialize` control_request registers hook
//! callbacks for `WorktreeCreate` and `WorktreeRemove`, so that every worktree the
//! session or its subagents work in is asked of the host
//! ([`AgentEvent::WorktreeRequested`]); nothing else rides on the handshake.
//!
//! Everything is normalized into the canonical [`AgentEvent`] stream. An actor
//! task owns the child: it reads stdout lines, receives [`SessionCommand`]s, and
//! writes stream-json lines to stdin. Multiple turns run over one process.

use crate::{
    ForkPoint, LaterHistory, RewindFailure, RewindFailureReason, RewindFile, RewindFiles,
    RewindIncomplete, RewindPreview, RewindPreviewResult,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use smol::io::{AsyncWrite, BufReader};
use smol::prelude::*;
use smol::process::Stdio;

pub use crate::claude_context::{CONTEXT_WINDOW_SUFFIXES, strip_context_window_suffix};
use crate::claude_goal::GoalTranscript;
use crate::claude_manifest::{CatalogModel, ClaudeCatalog};
use crate::{
    AgentError, AgentEvent, ApprovalDecision, ApprovalKind, ApprovalOption, ApprovalOptionKind,
    ApprovalRequest, AskedQuestion, Attachment, CatalogRefresh, DeltaKind, FileChange,
    FileChangeKind, GoalStatus, ItemContent, ItemStatus, LaunchEnv, ModelSpec, OptionDescriptor,
    OptionSelection, PlanStep, PlanStepStatus, ProviderCommand, ProviderCommandKind, ProviderKind,
    ResumeCursor, RewindMode, SearchHit, SearchScope, SessionCommand, SessionHandle,
    SessionOptions, ThreadItem, TokenUsage, ToolFamily, ToolOutput, TurnStatus, UserInputDelivery,
    UserInputOption, UserInputQuestion, WebLink, WorktreeAnswer, WorktreeRequest, selection_bool,
    selection_str,
};

/// The hook callbacks `initialize` registers. With `onFailure: "block"` an error
/// answer fails the worktree tool instead of counting as an empty success, which
/// for `WorktreeRemove` would report a removal that did not happen.
const WORKTREE_CREATE_CALLBACK: &str = "tcode-worktree-create";
const WORKTREE_REMOVE_CALLBACK: &str = "tcode-worktree-remove";
/// The prefix of the worktree name Claude Code asks for an `isolation: "worktree"`
/// subagent, before its agent id (the `task_started` `task_id`).
const SUBAGENT_WORKTREE_PREFIX: &str = "agent-";

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

/// The directory the CLI launched with `launch_env` keeps its settings,
/// sign-in and transcripts in.
pub fn config_dir(launch_env: &LaunchEnv) -> Option<PathBuf> {
    launch_env
        .child_path(ProviderKind::ClaudeCode, "CLAUDE_CONFIG_DIR")
        .or_else(|| {
            launch_env
                .child_home(ProviderKind::ClaudeCode)
                .map(|home| home.join(".claude"))
        })
}

/// The CLI's account file, `.claude.json`: in `CLAUDE_CONFIG_DIR` when that
/// is set, else in the home directory itself rather than in `~/.claude`.
pub fn account_file(launch_env: &LaunchEnv) -> Option<PathBuf> {
    account_file_in(
        launch_env.child_path(ProviderKind::ClaudeCode, "CLAUDE_CONFIG_DIR"),
        launch_env.child_home(ProviderKind::ClaudeCode),
    )
}

fn account_file_in(config_dir: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    config_dir.or(home).map(|dir| dir.join(".claude.json"))
}

// Credentials and per-account state (`.claude.json`, `projects/`) must not
// cross accounts, and `plugins/` records absolute paths into its own config
// directory. The one exception is `transfer_session`: when the user switches a
// thread to another account, that thread's own session files are copied, and
// nothing else of either account.
const INHERITED_CONFIG: [&str; 4] = ["CLAUDE.md", "agents", "commands", "skills"];

/// Copy the user's configuration from one config directory into another.
/// Blocking.
pub fn inherit_config(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut ancestors = HashSet::new();
    INHERITED_CONFIG
        .iter()
        .try_for_each(|name| copy_resolved(&from.join(name), &to.join(name), &mut ancestors))?;
    inherit_settings(&from.join("settings.json"), &to.join("settings.json"))
}

/// Copy the session `resume` names from the config directory `from` launches
/// with into the one `to` launches with: its transcript, the directory beside
/// it (subagent transcripts, stored tool results), its file checkpoints, task
/// list, todo lists, hook-set environment and the plan files its transcript
/// names. What `to` already holds of the session is replaced, since `from`
/// holds the copy the thread last ran. Blocking.
pub fn transfer_session(
    from: &LaunchEnv,
    to: &LaunchEnv,
    resume: &ResumeCursor,
) -> std::io::Result<()> {
    let not_found = |what: String| std::io::Error::new(std::io::ErrorKind::NotFound, what);
    let session_id = resume
        .str_field(&["session_id"])
        .filter(|id| is_file_name(id))
        .ok_or_else(|| not_found("the resume cursor names no session".into()))?;
    let (Some(from), Some(to)) = (config_dir(from), config_dir(to)) else {
        return Err(not_found("a profile has no config directory".into()));
    };
    if let (Ok(source), Ok(target)) = (std::fs::canonicalize(&from), std::fs::canonicalize(&to))
        && source == target
    {
        return Ok(());
    }
    // The project directory's name is the CLI's own encoding of the session's
    // directory, so the one the session is found under is reused rather than
    // derived again.
    let projects = from.join("projects");
    let transcript = format!("{session_id}.jsonl");
    let project = std::fs::read_dir(&projects)
        .map_err(|error| {
            std::io::Error::new(error.kind(), format!("{}: {error}", projects.display()))
        })?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|dir| dir.join(&transcript).is_file())
        .ok_or_else(|| {
            not_found(format!(
                "{} holds no session {session_id}",
                projects.display()
            ))
        })?;
    let target = to
        .join("projects")
        .join(project.file_name().unwrap_or_default());
    if target.join(&transcript).exists() {
        log::info!(
            "claude: replacing session {session_id} in {} with the copy in {}",
            to.display(),
            from.display()
        );
    }
    let mut ancestors = HashSet::new();
    copy_resolved(
        &project.join(session_id),
        &target.join(session_id),
        &mut ancestors,
    )?;
    for dir in ["file-history", "tasks", "session-env"] {
        copy_resolved(
            &from.join(dir).join(session_id),
            &to.join(dir).join(session_id),
            &mut ancestors,
        )?;
    }
    let agent_todos = format!("{session_id}-agent-");
    copy_entries(&from.join("todos"), &to.join("todos"), |name| {
        name.starts_with(&agent_todos) && name.ends_with(".json")
    })?;
    // A plan file is named for the plan's slug, which the transcript records
    // on its entries; a subagent's plan adds its agent id.
    for slug in plan_slugs(&project.join(&transcript))? {
        let agent_plans = format!("{slug}-agent-");
        copy_entries(&from.join("plans"), &to.join("plans"), |name| {
            name.strip_suffix(".md")
                .is_some_and(|stem| stem == slug || stem.starts_with(&agent_plans))
        })?;
    }
    std::fs::create_dir_all(&target)?;
    std::fs::copy(project.join(&transcript), target.join(&transcript))?;
    Ok(())
}

fn is_file_name(name: &str) -> bool {
    Path::new(name).file_name() == Some(std::ffi::OsStr::new(name))
}

/// The plan slugs a session transcript records, in the order they appear.
fn plan_slugs(transcript: &Path) -> std::io::Result<Vec<String>> {
    let mut slugs = Vec::<String>::new();
    for line in std::fs::read(transcript)?.split(|byte| *byte == b'\n') {
        if !line.windows(6).any(|window| window == b"\"slug\"") {
            continue;
        }
        let Ok(entry) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if let Some(slug) = entry.get("slug").and_then(Value::as_str)
            && !slugs.iter().any(|known| known == slug)
        {
            slugs.push(slug.to_owned());
        }
    }
    Ok(slugs)
}

/// Copy the entries of the directory `from` whose names `wanted` accepts into
/// `to`.
fn copy_entries(from: &Path, to: &Path, wanted: impl Fn(&str) -> bool) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(from) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        entries => entries?,
    };
    let mut ancestors = HashSet::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_name().to_str().is_some_and(&wanted) {
            std::fs::create_dir_all(to)?;
            copy_resolved(&entry.path(), &to.join(entry.file_name()), &mut ancestors)?;
        }
    }
    Ok(())
}

/// Copy what `from` names, following symbolic links: a link copied as a link
/// would resolve its relative target under the new directory. A directory
/// that is its own ancestor is skipped.
pub(crate) fn copy_resolved(
    from: &Path,
    to: &Path,
    ancestors: &mut HashSet<PathBuf>,
) -> std::io::Result<()> {
    let metadata = match std::fs::metadata(from) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        metadata => metadata?,
    };
    if metadata.is_dir() {
        let canonical = std::fs::canonicalize(from)?;
        if !ancestors.insert(canonical.clone()) {
            return Ok(());
        }
        std::fs::create_dir_all(to)?;
        let copied = std::fs::read_dir(from)?.try_for_each(|entry| {
            let entry = entry?;
            copy_resolved(&entry.path(), &to.join(entry.file_name()), ancestors)
        });
        ancestors.remove(&canonical);
        copied?;
    } else if metadata.is_file() {
        std::fs::copy(from, to)?;
    }
    Ok(())
}

/// Copy `settings.json` without what would carry the source account's
/// credentials or endpoint into the new one.
fn inherit_settings(from: &Path, to: &Path) -> std::io::Result<()> {
    let text = match std::fs::read(from) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        text => text?,
    };
    let mut settings: serde_json::Value = serde_json::from_slice(&text)
        .map_err(|error| std::io::Error::other(format!("{}: {error}", from.display())))?;
    if let Some(settings) = settings.as_object_mut() {
        settings.remove("apiKeyHelper");
        if let Some(env) = settings
            .get_mut("env")
            .and_then(serde_json::Value::as_object_mut)
        {
            for key in [
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_AUTH_TOKEN",
                "ANTHROPIC_BASE_URL",
            ] {
                env.remove(key);
            }
        }
    }
    std::fs::write(to, serde_json::to_vec_pretty(&settings)?)
}

/// Start (or resume) a Claude Code session.
pub async fn start(opts: SessionOptions) -> Result<SessionHandle, AgentError> {
    let fork_anchor = match &opts.fork {
        Some(ForkPoint::Before { checkpoint_id }) => Some(fork_anchor(
            &opts.resume,
            config_dir(&opts.launch_env).as_deref(),
            checkpoint_id,
        )?),
        Some(ForkPoint::Branch { .. }) => {
            return Err(AgentError::Provider(
                "Claude Code keeps no branches to go on in".into(),
            ));
        }
        Some(ForkPoint::Tail) | None => None,
    };
    // Absolute path: a bare name would be resolved against the session cwd we
    // set below, which breaks PATH lookup (see `resolve_binary`).
    let binary = crate::resolve_binary(opts.binary_path.as_deref(), "claude")?;
    let binary = binary.to_string_lossy().into_owned();

    // Resolve model-scoped launch options from the persisted selections.
    let launch = ClaudeLaunchOptions::resolve(opts.model.as_deref(), &opts.option_selections);
    let launch_permission_mode = opts
        .option_selections
        .iter()
        .find(|selection| selection.id == "permissionMode")
        .and_then(|selection| selection.value.as_str())
        .unwrap_or("default");
    // Launch arguments are deliberately appended last, so the tracker must
    // follow their effective value rather than the internal flag they replace.
    let applied_permission_mode =
        effective_permission_mode(launch_permission_mode, &opts.extra_args);

    let mut cmd = crate::process::async_harness_command(&binary);
    // SDK file checkpointing is opt-in for print/stream-json hosts even
    // though the interactive CLI enables it automatically.
    cmd.env("CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING", "true");
    // Passed on every launch: the CLI records the rendered system prompt with the conversation
    // and renders it again from the current launch after a compaction.
    // A file keeps the argument on one line for a Windows batch launcher.
    let instructions_file = opts
        .instructions
        .as_deref()
        .map(InstructionsFile::write)
        .transpose()
        .map_err(|err| AgentError::Spawn(format!("writing Tcode's instructions: {err}")))?;
    cmd.args(launch_args(
        launch_permission_mode,
        &launch,
        resume_args(&opts.resume, opts.fork.is_some(), fork_anchor.as_deref()),
        &opts.mcp_servers,
        instructions_file.as_ref().map(|file| file.0.as_path()),
        &opts.extra_args,
    ));
    log::debug!(
        "claude spawn args: model={:?} effort={:?} settings={:?} permission-mode={} effective-permission-mode={}",
        launch.model_id,
        launch.effort,
        launch.settings_json,
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
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .env("CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS", "1");
    for (key, value) in opts.launch_env.pairs(ProviderKind::ClaudeCode) {
        cmd.env(key, value);
    }

    crate::process::refuse_batch_line_breaks(cmd.get_program(), cmd.get_args())?;
    let mut child = opts
        .processes
        .hold(crate::process::ProcessTree::new(cmd.spawn().map_err(
            |e| AgentError::Spawn(format!("spawning `{binary}`: {e}")),
        )?));

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
        // Kept until the CLI's output ends, so it reads the file whenever it likes.
        let _instructions = instructions_file;
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
        cwd: opts.cwd.clone(),
        worktrees: opts.worktrees.clone(),
        claude_dir: config_dir(&opts.launch_env),
        launch,
        resumed: opts.resume.is_some(),
    };
    smol::spawn(actor_loop(
        child,
        stdin,
        cmd_rx,
        line_rx,
        event_tx,
        session_config,
        applied_permission_mode,
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

/// The arguments a session launches Claude Code with; `instructions` is the
/// file holding Tcode's part of the system prompt.
fn launch_args(
    permission_mode: &str,
    launch: &ClaudeLaunchOptions,
    resume: Vec<String>,
    mcp_servers: &[crate::McpRegistration],
    instructions: Option<&Path>,
    extra_args: &[String],
) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = [
        "--print",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--include-partial-messages",
        "--verbose",
        "--permission-prompt-tool",
        "stdio",
        "--permission-mode",
        permission_mode,
        "--allow-dangerously-skip-permissions",
        // User-message UUIDs are Claude's native checkpoint ids.
        "--replay-user-messages",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    for (flag, value) in [
        ("--model", &launch.model_id),
        ("--effort", &launch.effort),
        ("--settings", &launch.settings_json),
    ] {
        if let Some(value) = value {
            args.extend([flag.into(), value.into()]);
        }
    }
    args.extend(resume.into_iter().map(Into::into));
    // Tokens ride in Authorization headers inside the merged `--mcp-config` JSON.
    args.extend(mcp_args(mcp_servers).into_iter().map(Into::into));
    if let Some(path) = instructions {
        args.extend(["--append-system-prompt-file".into(), path.into()]);
    }
    // Settings → Harnesses "Launch arguments", last so the user can override
    // anything Tcode set.
    args.extend(extra_args.iter().map(Into::into));
    args
}

/// Tcode's instructions for `--append-system-prompt-file`, removed when dropped. Text that spans
/// lines cannot be an argument to a Windows batch file, which `claude.cmd` may be.
struct InstructionsFile(PathBuf);

impl InstructionsFile {
    fn write(text: &str) -> std::io::Result<Self> {
        use std::io::Write as _;
        let path = std::env::temp_dir().join(format!(
            "tcode-claude-instructions-{}.md",
            uuid::Uuid::new_v4()
        ));
        // A new file: in a shared temporary directory, a name someone else
        // created first must not be written through.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        let written = Self(path);
        file.write_all(text.as_bytes())?;
        Ok(written)
    }
}

impl Drop for InstructionsFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

const USAGE_EXIT_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// The payload of a `get_usage` control response.
fn usage_response(response: &Value) -> Result<Value, AgentError> {
    if response.get("subtype").and_then(Value::as_str) == Some("error") {
        let error = response
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| response.get("error").unwrap_or(response).to_string());
        return Err(AgentError::Provider(error));
    }
    response
        .get("response")
        .cloned()
        .ok_or_else(|| AgentError::Protocol("Claude usage response omitted response".into()))
}

/// Ask a short-lived Claude Code process for its subscription usage.
pub async fn read_usage(
    binary_path: Option<PathBuf>,
    launch_env: LaunchEnv,
) -> Result<Value, AgentError> {
    let binary = crate::resolve_binary(binary_path.as_deref(), "claude")?;
    let binary_display = binary.to_string_lossy().into_owned();
    let mut cmd = crate::process::async_harness_command(&binary);
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

    let mut child = crate::process::ProcessTree::new(
        cmd.spawn()
            .map_err(|err| AgentError::Spawn(format!("spawning `{binary_display}`: {err}")))?,
    );
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
            return usage_response(response);
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

    // End of input lets the CLI leave: only then is the read in its config
    // directory, where the next read is answered from it if the usage endpoint
    // refuses that one. Killed here, a profile without a long-running session
    // would keep nothing.
    let _ = stdin.close().await;
    drop(stdin);
    let exited = smol::future::or(async { child.status().await.is_ok() }, async {
        smol::Timer::after(USAGE_EXIT_GRACE).await;
        false
    })
    .await;
    if !exited {
        let _ = child.kill();
        let _ = child.status().await;
    }
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
#[derive(Clone, Debug, Default)]
struct ClaudeLaunchOptions {
    /// The model as Tcode named it, which `model_id` launches.
    model: Option<String>,
    /// The effort selection it resolved to, in Tcode's values.
    effort_value: Option<String>,
    /// Model id with the manifest's context-window suffix (e.g. `[1m]`) only
    /// when the selected window exceeds what the bare slug already opens.
    model_id: Option<String>,
    /// `--effort` value after the manifest's `effortMap` (`None` when the
    /// selection maps to no flag).
    effort: Option<String>,
    /// `--settings` JSON string (fastMode / ultracode / alwaysThinkingEnabled).
    settings_json: Option<String>,
    thinking: Option<bool>,
    fast_mode: bool,
    ultracode: bool,
    auto_compact_window: Option<u64>,
    /// The window is the bare slug's own, so `auto_compact_window` adds nothing.
    native_window: bool,
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
        Self::resolve_with(&crate::claude_manifest::current(), model, selections)
    }

    fn resolve_with(
        catalog: &ClaudeCatalog,
        model: Option<&str>,
        selections: &[OptionSelection],
    ) -> Self {
        let entry = model.and_then(|model| catalog.model(model));
        let spec = entry.map(|entry| &entry.spec);
        let raw_effort = selection_str(selections, crate::EFFORT_OPTION_ID);
        let resolved_effort = resolve_claude_effort(spec, raw_effort.as_deref());
        let ultracode = resolved_effort.as_deref() == Some("ultracode");
        let effort = match (entry, resolved_effort.as_deref()) {
            (Some(entry), Some(effort)) => entry.cli_effort(effort),
            _ => None,
        };

        let window = catalog.resolved_context_window(model.unwrap_or_default(), selections);
        // Aliases (`opus`, `claude-opus-5.0`) launch as their canonical slug.
        let model_id = model.map(|model| {
            let base = spec.map_or_else(
                || model.split('[').next().unwrap_or(model),
                |spec| spec.id.as_str(),
            );
            let suffix = entry.map_or("", |entry| entry.context_window_suffix(window));
            format!("{base}{suffix}")
        });
        // Anything below the model's largest window is enforced through
        // `autoCompactWindow`: some bare slugs (e.g. Opus 5.5) already run at
        // 1M, so a smaller selection must be told to compact early.
        let largest = entry
            .and_then(CatalogModel::largest_context_window)
            .unwrap_or(window);
        let auto_compact_window = (window < largest).then_some(window);

        // `--settings` object: only supported/true keys are emitted.
        let fast_supported = spec.is_some_and(|spec| has_boolean_option(spec, "fastMode"));
        let thinking_supported = spec.is_some_and(|spec| has_boolean_option(spec, "thinking"));
        let fast_mode = fast_supported && selection_bool(selections, "fastMode") == Some(true);
        let thinking = if thinking_supported {
            selection_bool(selections, "thinking")
        } else {
            None
        };

        let settings_json =
            launch_settings_json(thinking, fast_mode, ultracode, auto_compact_window);

        ClaudeLaunchOptions {
            model: model.map(str::to_owned),
            effort_value: resolved_effort.clone(),
            model_id,
            effort,
            settings_json,
            thinking,
            fast_mode,
            ultracode,
            auto_compact_window,
            native_window: entry.and_then(CatalogModel::default_context_window) == Some(window),
        }
    }

    /// `apply_flag_settings` keys that differ from `self` in `target`; `None`
    /// clears a key.
    fn flag_changes(&self, target: &Self) -> serde_json::Map<String, Value> {
        let mut flags = serde_json::Map::new();
        if self.effort != target.effort {
            flags.insert("effortLevel".into(), json!(target.effort));
        }
        if self.fast_mode != target.fast_mode {
            flags.insert("fastMode".into(), json!(target.fast_mode.then_some(true)));
        }
        if self.ultracode != target.ultracode {
            flags.insert("ultracode".into(), json!(target.ultracode));
        }
        flags
    }

    /// Whether `target` differs in a setting that only reaches a new process.
    /// `alwaysThinkingEnabled` and `autoCompactWindow` land in the flag layer
    /// but change nothing there (2.1.318: `thinking` stays adaptive, and the
    /// compaction a launch flag brings on never comes).
    fn needs_restart_for(&self, target: &Self) -> bool {
        let window_is_kept = self.auto_compact_window == target.auto_compact_window
            || (self.auto_compact_window.is_none() && target.native_window);
        self.thinking != target.thinking || !window_is_kept
    }
}

/// A live settings change the CLI has yet to answer. `set_model` validates the
/// model against the API first, so the flags wait for its answer.
struct PendingSettings {
    request_id: String,
    target: ClaudeLaunchOptions,
    flags: Option<Value>,
}

/// Per-session config threaded into the actor loop / mapper.
struct SessionConfig {
    /// The directory the process was started in.
    cwd: PathBuf,
    /// [`SessionOptions::worktrees`].
    worktrees: Vec<PathBuf>,
    claude_dir: Option<PathBuf>,
    launch: ClaudeLaunchOptions,
    /// The transcript already holds history this process did not write.
    resumed: bool,
}

/// The CLI appends transcript records shortly after the stdout lines they
/// accompany, so goal records are polled for a while after each line.
const GOAL_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(400);
const GOAL_POLL_WINDOW: std::time::Duration = std::time::Duration::from_secs(3);

/// The entry the prompt `checkpoint_id` answered: where a fork before it ends,
/// which `--resume-session-at` takes. Stream-json does not carry a message's
/// parent, only the session's transcript does. `--resume-drops-turn`, which
/// guards the same flag, is left out: it refuses any range holding a later
/// prompt, so it allows a fork before the last turn alone.
fn fork_anchor(
    resume: &Option<ResumeCursor>,
    claude_dir: Option<&Path>,
    checkpoint_id: &str,
) -> Result<String, AgentError> {
    let session_id = resume
        .as_ref()
        .and_then(|cursor| cursor.str_field(&["session_id"]))
        .ok_or_else(|| AgentError::Protocol("Claude Code fork without a session".into()))?;
    let transcript = claude_dir
        .and_then(|dir| crate::claude_goal::find_session_transcript(dir, session_id))
        .ok_or_else(|| {
            AgentError::Provider(format!("Claude Code has no transcript of {session_id}"))
        })?;
    let file = std::fs::File::open(&transcript)
        .map_err(|err| AgentError::Provider(format!("{}: {err}", transcript.display())))?;
    let entry = std::io::BufRead::lines(std::io::BufReader::new(file))
        .map_while(Result::ok)
        .filter(|line| line.contains(checkpoint_id))
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
        .find(|entry| entry.get("uuid").and_then(Value::as_str) == Some(checkpoint_id))
        .ok_or_else(|| {
            AgentError::Provider(format!(
                "message {checkpoint_id} is not in Claude Code's transcript of {session_id}"
            ))
        })?;
    entry
        .get("parentUuid")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            AgentError::Provider("Claude Code cannot fork before a session's first message".into())
        })
}

fn resume_args(
    resume: &Option<ResumeCursor>,
    fork: bool,
    fork_anchor: Option<&str>,
) -> Vec<String> {
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
    if let Some(anchor) = fork_anchor {
        args.extend(["--resume-session-at".into(), anchor.to_owned()]);
    }
    args
}

#[allow(clippy::too_many_arguments)]
async fn actor_loop(
    mut child: crate::process::ProcessTree<smol::process::Child>,
    mut stdin: smol::process::ChildStdin,
    cmd_rx: smol::channel::Receiver<SessionCommand>,
    line_rx: smol::channel::Receiver<String>,
    event_tx: smol::channel::Sender<AgentEvent>,
    config: SessionConfig,
    applied_permission_mode: String,
    stderr_tail: crate::process::StderrTail,
    stderr_task: smol::Task<()>,
) {
    let mut mapper = Mapper::new_configured(applied_permission_mode);
    mapper.applied_settings = config.launch.clone();
    mapper.launch_cwd = Some(config.cwd.clone());
    mapper.cwd = Some(config.cwd.clone());
    mapper.worktrees = config.worktrees.clone();
    let claude_dir = config.claude_dir.clone();
    // Before any user message, so no worktree is made without the host.
    let initialize = mapper.initialize_request();
    if let Err(error) = write_line(&mut stdin, &initialize).await {
        log::warn!("claude: could not register the worktree hooks: {error}");
    }
    let settings_read = mapper.settings_read_request();
    if let Err(error) = write_line(&mut stdin, &settings_read).await {
        log::warn!("claude: could not ask for the settings in effect: {error}");
    }
    let mut tailers = HashMap::new();
    let (tail_tx, tail_rx) = smol::channel::unbounded::<SubagentTailNotice>();
    let mut goal: Option<GoalTranscript> = None;
    // (next read, last read) while goal records may still be arriving.
    let mut goal_poll: Option<(std::time::Instant, std::time::Instant)> = None;

    // Set when the child died on its own (stdout EOF): only then do its exit
    // status and stderr tail belong in the close reason.
    let mut provider_exited = false;
    let closed_reason: Option<String> = loop {
        // Race a UI command against the next stdout line. `or` biases toward the
        // command channel, which is fine: both channels make independent progress.
        let next_goal_read = goal_poll.map(|(next, _)| next);
        let sel = smol::future::or(
            async { Sel::Cmd(cmd_rx.recv().await.ok()) },
            smol::future::or(
                async { Sel::Line(line_rx.recv().await.ok()) },
                smol::future::or(async { Sel::Tail(tail_rx.recv().await.ok()) }, async {
                    match next_goal_read {
                        Some(at) => {
                            smol::Timer::at(at).await;
                            Sel::GoalRead
                        }
                        None => std::future::pending().await,
                    }
                }),
            ),
        )
        .await;

        match sel {
            Sel::GoalRead => {
                if let Some(mut reader) = goal.take() {
                    let (reader, changed) = crate::process::unblock(move || {
                        let changed = reader.read_appended();
                        (reader, changed)
                    })
                    .await;
                    goal = Some(reader);
                    if let Some(goal) = changed
                        && event_tx
                            .send(AgentEvent::GoalUpdated { goal })
                            .await
                            .is_err()
                    {
                        let _ = child.kill();
                        return;
                    }
                }
                let now = std::time::Instant::now();
                goal_poll = goal_poll
                    .filter(|(_, until)| now < *until)
                    .map(|(_, until)| (now + GOAL_POLL_INTERVAL, until));
            }
            Sel::Tail(notice) => {
                // The sender side is owned by this loop, so it can't close.
                let Some(notice) = notice else { continue };
                if notice.stopped {
                    tailers.remove(&notice.parent_id);
                }
                for ev in mapper.on_tail_notice(notice) {
                    if event_tx.send(ev).await.is_err() {
                        let _ = child.kill();
                        return;
                    }
                }
            }
            Sel::Cmd(Some(command)) => {
                if let ControlFlow::Break(reason) = handle_command(
                    command,
                    &mut mapper,
                    goal.as_mut(),
                    &mut stdin,
                    &event_tx,
                    &mut child,
                )
                .await
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
                    Ok(msg) => mapper.on_wire_message(msg, claude_dir.as_deref()).await,
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
                for write in mapper.take_outgoing() {
                    let _ = write_line(&mut stdin, &write).await;
                }
                if let (Some(root), Some(session_id)) = (&claude_dir, mapper.session_id.as_deref())
                {
                    if goal
                        .as_ref()
                        .is_none_or(|reader| reader.session_id() != session_id)
                    {
                        goal = Some(GoalTranscript::new(
                            root.clone(),
                            session_id.to_owned(),
                            config.resumed,
                        ));
                    }
                    let now = std::time::Instant::now();
                    let next = goal_poll.map_or(now, |(next, _)| next);
                    goal_poll = Some((next, now + GOAL_POLL_WINDOW));
                }
                process_tail_requests(
                    mapper.take_tail_requests(),
                    &mut tailers,
                    claude_dir.as_deref(),
                    &tail_tx,
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
    // Tail acknowledgements no longer reach this loop; release the held
    // terminal snapshots so the parent rows settle before the close.
    for ev in mapper.take_pending_subagent_terminals() {
        let _ = event_tx.send(ev).await;
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
    GoalRead,
    Cmd(Option<SessionCommand>),
    Line(Option<String>),
    Tail(Option<SubagentTailNotice>),
}

/// One read of a tailed subagent transcript, handed back to the actor so the
/// mapper stays the single writer of the canonical stream: it dedupes the
/// child items against the stdout feed and, on `stopped`, releases the
/// Subagent item's terminal snapshot after them.
pub(crate) struct SubagentTailNotice {
    pub(crate) parent_id: String,
    pub(crate) events: Vec<AgentEvent>,
    pub(crate) model: Option<(Option<String>, Option<String>)>,
    /// Final batch: the tail acknowledged Stop (or lost its actor).
    pub(crate) stopped: bool,
}

pub(crate) enum TailControl {
    PreferPath(PathBuf),
    Stop,
}

async fn process_tail_requests(
    requests: Vec<TailRequest>,
    tailers: &mut HashMap<String, smol::channel::Sender<TailControl>>,
    claude_dir: Option<&Path>,
    tail_tx: &smol::channel::Sender<SubagentTailNotice>,
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
                let notices = tail_tx.clone();
                smol::spawn(run_subagent_tail(
                    parent_id, task_id, session_id, claude_dir, control_rx, notices,
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

/// Poll a subagent's transcript until Stop; the final read is flushed in the
/// notice that carries `stopped`, so the actor sees every child item before
/// it releases the Subagent item's terminal snapshot.
pub(crate) async fn run_subagent_tail(
    parent_id: String,
    task_id: String,
    session_id: String,
    claude_dir: Option<PathBuf>,
    controls: smol::channel::Receiver<TailControl>,
    notices: smol::channel::Sender<SubagentTailNotice>,
) {
    let mut reader: Option<crate::subagent_tail::TailReader> = None;
    let mut stopping = false;
    let mut woken = None;
    loop {
        for control in woken
            .take()
            .into_iter()
            .chain(std::iter::from_fn(|| controls.try_recv().ok()))
        {
            match control {
                // The notification's output file duplicates a transcript that
                // discovery already tails; switching would replay it from byte 0.
                TailControl::PreferPath(preferred) if reader.is_none() => {
                    reader = Some(crate::subagent_tail::TailReader::new(
                        preferred,
                        parent_id.clone(),
                    ));
                }
                TailControl::PreferPath(_) => {}
                TailControl::Stop => stopping = true,
            }
        }
        if reader.is_none()
            && let Some(root) = &claude_dir
            && let Some(found) =
                crate::subagent_tail::find_transcript(root, &session_id, &task_id, &parent_id)
        {
            reader = Some(crate::subagent_tail::TailReader::new(
                found,
                parent_id.clone(),
            ));
        }
        let mut notice = SubagentTailNotice {
            parent_id: parent_id.clone(),
            events: Vec::new(),
            model: None,
            stopped: stopping,
        };
        if let Some(reader) = &mut reader {
            match reader.read_appended() {
                Ok(mapped) => {
                    notice.events = mapped;
                    notice.model = reader.take_model();
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => log::debug!("claude subagent tail read failed: {err}"),
            }
        }
        if (stopping || !notice.events.is_empty() || notice.model.is_some())
            && notices.send(notice).await.is_err()
        {
            return;
        }
        if stopping {
            return;
        }
        // Wake on the next control so a Stop flushes immediately instead of
        // after a poll interval; a closed channel means the actor is gone.
        woken = smol::future::or(
            async { Some(controls.recv().await.unwrap_or(TailControl::Stop)) },
            async {
                smol::Timer::after(std::time::Duration::from_millis(400)).await;
                None
            },
        )
        .await;
    }
}

/// Run one of the CLI's slash commands the way stream-json input does: as a
/// user message that opens a turn.
async fn slash_command_turn(
    command: &str,
    mapper: &mut Mapper,
    stdin: &mut smol::process::ChildStdin,
    event_tx: &smol::channel::Sender<AgentEvent>,
) -> ControlFlow<&'static str> {
    if write_line(stdin, &user_message(command, &[]))
        .await
        .is_err()
    {
        let _ = event_tx
            .send(AgentEvent::Error {
                message: format!("failed to write {command} to provider stdin"),
                fatal: true,
            })
            .await;
        return ControlFlow::Break("provider stdin write failed");
    }
    let turn_id = mapper.start_turn();
    let _ = event_tx.send(AgentEvent::TurnStarted { turn_id }).await;
    ControlFlow::Continue(())
}

async fn handle_command(
    command: SessionCommand,
    mapper: &mut Mapper,
    goal: Option<&mut GoalTranscript>,
    stdin: &mut smol::process::ChildStdin,
    event_tx: &smol::channel::Sender<AgentEvent>,
    child: &mut crate::process::ProcessTree<smol::process::Child>,
) -> ControlFlow<&'static str> {
    match command {
        SessionCommand::SendTurn {
            delivery_id,
            text,
            options: _,
            attachments,
        } => {
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
        // The CLI reads its stdin in order: an interrupt written while it is
        // still starting reaches it after the turn's message, once it is up.
        SessionCommand::Interrupt => {
            let msg = mapper.interrupt_request();
            let _ = write_line(stdin, &msg).await;
            ControlFlow::Continue(())
        }
        // The CLI runs its `/compact` slash command from a stream-json user
        // message as a turn of its own: `status: compacting`, then
        // `compact_boundary` (trigger `manual`) and a `result`.
        SessionCommand::Compact => slash_command_turn("/compact", mapper, stdin, event_tx).await,
        // `/goal` runs as a turn of its own too; with a new goal the CLI asks
        // the model to start on it in that turn.
        SessionCommand::SetGoal {
            objective: Some(objective),
            status: None | Some(GoalStatus::Active),
        } => slash_command_turn(&format!("/goal {objective}"), mapper, stdin, event_tx).await,
        // Pausing is not offered: `Caps::goals` has no pause for Claude Code.
        SessionCommand::SetGoal { .. } => ControlFlow::Continue(()),
        // A met or impossible goal is already gone from the CLI; only the
        // record of it is left to dismiss.
        SessionCommand::ClearGoal
            if goal
                .as_ref()
                .and_then(|reader| reader.goal())
                .is_some_and(|goal| {
                    matches!(goal.status, GoalStatus::Complete | GoalStatus::Blocked)
                }) =>
        {
            if let Some(reader) = goal {
                reader.dismiss();
            }
            let _ = event_tx.send(AgentEvent::GoalUpdated { goal: None }).await;
            ControlFlow::Continue(())
        }
        SessionCommand::ClearGoal => {
            slash_command_turn("/goal clear", mapper, stdin, event_tx).await
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
        SessionCommand::SetOption { id, value } if id == "permissionMode" => {
            let Some(mode) = value.as_str() else {
                return ControlFlow::Continue(());
            };
            let msg = mapper.set_permission_mode_request_str(mode);
            if write_line(stdin, &msg).await.is_err() {
                if let Some(request_id) = msg.get("request_id").and_then(Value::as_str) {
                    mapper.pending_permission_modes.remove(request_id);
                }
                let _ = event_tx
                    .send(AgentEvent::Warning {
                        message: format!(
                            "claude: failed to write permission-mode switch for {mode}"
                        ),
                    })
                    .await;
            }
            ControlFlow::Continue(())
        }
        SessionCommand::SetOption { .. } => ControlFlow::Continue(()),
        SessionCommand::StopBackgroundTask { task_id } => {
            for request in mapper.stop_task_requests(task_id) {
                let _ = write_line(stdin, &request).await;
            }
            ControlFlow::Continue(())
        }
        SessionCommand::MoveToBackground { item_id } => {
            let request = mapper.move_to_background_request(item_id);
            let _ = write_line(stdin, &request).await;
            ControlFlow::Continue(())
        }
        SessionCommand::ApplySettings {
            model,
            option_selections,
        } => {
            let target = ClaudeLaunchOptions::resolve(model.as_deref(), &option_selections);
            let answer = match mapper.begin_settings(target) {
                Ok(Some(request)) if write_line(stdin, &request).await.is_err() => {
                    mapper.pending_settings = None;
                    Some(false)
                }
                Ok(Some(_)) => None,
                Ok(None) => Some(true),
                Err(()) => Some(false),
            };
            if let Some(applied) = answer {
                let _ = event_tx.send(AgentEvent::SettingsApplied { applied }).await;
            }
            ControlFlow::Continue(())
        }
        // Claude Code keeps no rewind undoable.
        command @ SessionCommand::UndoRewind { .. } => {
            if let Some(event) =
                command.rewind_refusal(RewindFailure::new(RewindFailureReason::Unsupported, None))
            {
                let _ = event_tx.send(event).await;
            }
            ControlFlow::Continue(())
        }
        SessionCommand::PreviewRewind {
            checkpoint_id,
            mode,
        } => {
            let failure = match mapper.begin_preview(checkpoint_id.clone(), mode) {
                Some(request) => write_line(stdin, &request).await.err().map(|error| {
                    RewindFailure::new(RewindFailureReason::ProcessGone, error.to_string())
                }),
                // Restoring the conversation alone changes no file.
                None => {
                    let _ = event_tx
                        .send(AgentEvent::RewindPreviewed {
                            checkpoint_id: checkpoint_id.clone(),
                            mode,
                            result: RewindPreviewResult::Ready(RewindPreview::default()),
                        })
                        .await;
                    None
                }
            };
            if let Some(failure) = failure {
                let _ = event_tx
                    .send(AgentEvent::RewindPreviewed {
                        checkpoint_id,
                        mode,
                        result: RewindPreviewResult::Refused(failure),
                    })
                    .await;
            }
            ControlFlow::Continue(())
        }
        SessionCommand::Rewind {
            checkpoint_id,
            mode,
        } => {
            let request = mapper.begin_rewind(checkpoint_id.clone(), mode);
            if write_line(stdin, &request).await.is_err() {
                let _ = event_tx
                    .send(AgentEvent::RewindFailed {
                        checkpoint_id,
                        mode,
                        failure: RewindFailure::new(
                            RewindFailureReason::ProcessGone,
                            "failed to write rewind request to provider stdin".to_owned(),
                        ),
                    })
                    .await;
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
            let mut msg = user_message(&text, &attachments);
            // A steer sent under its own uuid comes back in the replayed
            // echo under it, which makes it a conversation restore point.
            let uuid = uuid::Uuid::new_v4().to_string();
            msg["uuid"] = json!(uuid);
            if write_steering_message(stdin, &msg, request_id.clone(), mapper, event_tx).await {
                mapper.steer_uuids.insert(uuid, request_id);
            }
            ControlFlow::Continue(())
        }
        SessionCommand::RetryTurn { .. } | SessionCommand::MoveTo { .. } => {
            ControlFlow::Continue(())
        }
        SessionCommand::RespondWorktree { request_id, answer } => {
            match mapper.worktree_response(&request_id, &answer) {
                Some((response, request)) => {
                    if write_line(stdin, &response).await.is_ok() {
                        let _ = event_tx
                            .send(AgentEvent::WorktreeResolved {
                                request_id,
                                request,
                                answer,
                            })
                            .await;
                    }
                }
                None => log::debug!("claude: worktree answer for unknown request {request_id}"),
            }
            ControlFlow::Continue(())
        }
        // The same `get_usage` control request [`read_usage`] sends, asked
        // of the running CLI instead of a process started for it.
        SessionCommand::ReadAccountUsage { reply } => {
            let msg = mapper.control_request(json!({ "subtype": "get_usage" }));
            if let Some(request_id) = msg["request_id"].as_str() {
                mapper
                    .pending_usage_reads
                    .insert(request_id.to_owned(), reply);
            }
            if write_line(stdin, &msg).await.is_err()
                && let Some(request_id) = msg["request_id"].as_str()
            {
                mapper.pending_usage_reads.remove(request_id);
            }
            ControlFlow::Continue(())
        }
        SessionCommand::Shutdown => {
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

/// Whether `left` and `right` name one directory, however each is spelled.
fn same_directory(left: &Path, right: &Path) -> bool {
    left == right
        || matches!(
            (left.canonicalize(), right.canonicalize()),
            (Ok(left), Ok(right)) if left == right
        )
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

fn control_error(request_id: &str, error: &str) -> Value {
    json!({
        "type": "control_response",
        "response": { "subtype": "error", "request_id": request_id, "error": error },
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
) -> bool {
    if write_line(stdin, message).await.is_err() {
        let _ = event_tx
            .send(AgentEvent::Error {
                message: "failed to write steering message to provider stdin".into(),
                fatal: true,
            })
            .await;
        false
    } else {
        mapper.pending_steers.push_back(request_id);
        true
    }
}

/// The files a `rewind_files` answer names (`filesChanged` is withheld from
/// some clients; the line totals are across all files).
fn rewind_files(result: &Value) -> RewindFiles {
    RewindFiles {
        files: result
            .get("filesChanged")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(RewindFile::path)
            .collect(),
        insertions: result.get("insertions").and_then(Value::as_u64),
        deletions: result.get("deletions").and_then(Value::as_u64),
        skipped_links: result
            .get("skippedLinks")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
    }
}

/// Why `rewind_files` could not restore, from the words Claude Code answers
/// with: it gives no reason code for files.
fn files_failure(error: Option<String>) -> RewindFailure {
    let reason = match error.as_deref() {
        Some(error) if error.contains("No file checkpoint") => {
            RewindFailureReason::NothingToRestore
        }
        Some(error) if error.contains("not enabled") => RewindFailureReason::Unsupported,
        _ => RewindFailureReason::Other,
    };
    RewindFailure::new(reason, error)
}

/// `rewind_conversation`'s refusal `reason`, in the canonical terms.
fn conversation_refusal(reason: Option<&str>) -> RewindFailureReason {
    match reason {
        Some("commands_queued" | "prompt_pending") => RewindFailureReason::Busy,
        Some("turn_running") => RewindFailureReason::TurnRunning,
        Some("target_not_found") => RewindFailureReason::TargetNotFound,
        Some("stale_target" | "unseen_later_turn" | "state_changed") => {
            RewindFailureReason::ConversationChanged
        }
        Some(
            "poll_tool_result_target"
            | "target_splits_tool_call"
            | "delivered_poll_events_in_range",
        ) => RewindFailureReason::CannotCutHere,
        Some("persist_failed") => RewindFailureReason::SaveFailed,
        Some("usage_limited") => RewindFailureReason::UsageLimited,
        Some("expired") => RewindFailureReason::Expired,
        _ => RewindFailureReason::Other,
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
/// The texts Claude Code writes in a message of the user's role for a
/// request, an agent or a tool call it cut off, which it reads as no one's
/// words: the entries of its marker list that say so, matched as it matches
/// them, by prefix (`Vxe` and `uh` in Claude Code 2.1.322).
const INTERRUPTION_MARKERS: [&str; 7] = [
    "[Request interrupted by user]",
    "[Request interrupted: the session ended before this agent finished. Nobody asked it to stop.]",
    "[Agent call interrupted: the process running this conversation stopped before this agent finished, so it returned no result. Nobody asked it to stop.]",
    "[Tool call did not complete: the turn was ended to deliver the message that follows. Nothing refused it; re-run it if still needed.]",
    "[Tool call interrupted: the session ended before this call's result was recorded, so its outcome is unknown. Check whether it took effect before relying on it or running it again.]",
    "[Tool call skipped: the turn was stopped before this call ran, by the check whose denial is on another call in this batch. Nothing refused this call and it had no effects; re-run it if still needed.]",
    "[Tool call skipped: the turn ended to deliver the message that follows before this call ran. Nothing refused it; re-run it if still needed.]",
];

/// Whether `text` is Claude Code's own mark that it cut something off, not
/// anything the user said or a tool answered.
pub fn is_interruption_marker(text: &str) -> bool {
    INTERRUPTION_MARKERS
        .iter()
        .any(|marker| text.starts_with(marker))
}

/// How a call ended, from its `tool_result`: cut off when Claude Code says it
/// cut it off, failed when it is an error otherwise.
fn tool_result_status(is_error: bool, output: &str) -> ItemStatus {
    match is_error {
        true if is_interruption_marker(output) => ItemStatus::Interrupted,
        true => ItemStatus::Failed,
        false => ItemStatus::Completed,
    }
}

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

/// Remembers what kind of tool-use item a `tool_use_id` refers to, so that when
/// the matching `tool_result` arrives we can emit the right `ItemCompleted`.
pub(crate) enum ToolItem {
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
    Plan {
        markdown: String,
    },
    Subagent {
        agent_type: String,
        description: String,
        status: ItemStatus,
        summary: Option<String>,
        model: Option<String>,
        effort: Option<String>,
        /// The subagent this one was spawned from, for nested spawns.
        parent_item_id: Option<String>,
    },
}

impl ToolItem {
    /// The in-progress item for a call that is not an `Agent` spawn or a
    /// `TodoWrite`, and the state its result completes. Calls made at the
    /// top level and inside a subagent map alike.
    pub(crate) fn started(name: String, input: Value) -> (Self, ItemContent) {
        if name == EXIT_PLAN_MODE {
            let markdown = plan_markdown(&input);
            (
                ToolItem::Plan {
                    markdown: markdown.clone(),
                },
                ItemContent::Plan { markdown },
            )
        } else if is_command_tool(&name) {
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
                    input: Vec::new(),
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
                    output: None,
                    command: None,
                },
            )
        } else {
            let family = tool_family(&name, &input, None).0;
            (
                ToolItem::Tool {
                    name: name.clone(),
                    input: input.clone(),
                },
                ItemContent::ToolCall {
                    image_reads: Vec::new(),
                    name,
                    input,
                    output: None,
                    status: ItemStatus::InProgress,
                    family: family.map(Box::new),
                },
            )
        }
    }

    /// The finished item for a call from its `tool_result` block and the
    /// message's `tool_use_result` (`toolUseResult` in transcript files).
    pub(crate) fn finished(self, block: &Value, tool_use_result: Option<&Value>) -> ItemContent {
        let is_error = block
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let ToolOutput { text, images } =
            ToolOutput::from_content(block.get("content").unwrap_or(&Value::Null));
        let status = tool_result_status(is_error, &text);
        match self {
            ToolItem::Command { command, .. } => {
                let (output, exit_code) = match is_error.then(|| exited(&text)).flatten() {
                    Some((code, rest)) => (rest.to_owned(), Some(code)),
                    None => (text, None),
                };
                ItemContent::CommandExecution {
                    command,
                    input: Vec::new(),
                    output,
                    exit_code,
                    status,
                }
            }
            ToolItem::File { mut changes } => {
                if let Some(structured_patch) =
                    tool_use_result.and_then(|result| result.get("structuredPatch"))
                {
                    let diff = render_structured_patch(structured_patch);
                    let path = tool_use_result
                        .and_then(|result| result.get("filePath"))
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
                ItemContent::FileChange {
                    changes,
                    status,
                    output: (!text.is_empty()).then_some(text),
                    command: None,
                }
            }
            ToolItem::Tool { name, input } => {
                let (family, output) = tool_family(&name, &input, Some((text, tool_use_result)));
                ItemContent::ToolCall {
                    image_reads: images,
                    name,
                    input,
                    output,
                    status,
                    family: family.map(Box::new),
                }
            }
            ToolItem::Plan { markdown } => ItemContent::Plan { markdown },
            ToolItem::Subagent {
                agent_type,
                description,
                summary,
                model,
                effort,
                ..
            } => ItemContent::Subagent {
                agent_type,
                description,
                status,
                summary,
                model,
                effort,
            },
        }
    }

    fn subagent(content: &ItemContent, parent_item_id: Option<String>) -> Option<Self> {
        let ItemContent::Subagent {
            agent_type,
            description,
            status,
            summary,
            model,
            effort,
        } = content
        else {
            return None;
        };
        Some(ToolItem::Subagent {
            agent_type: agent_type.clone(),
            description: description.clone(),
            status: *status,
            summary: summary.clone(),
            model: model.clone(),
            effort: effort.clone(),
            parent_item_id,
        })
    }
}

/// A call a transcript records, mapped as a live session maps the same call:
/// a command as a command, an edit as a file change, a proposed plan as the
/// plan, anything else as the tool call it is. The importer reads
/// transcripts through this.
pub struct TranscriptCall(ToolItem);

impl TranscriptCall {
    /// The call as it starts, and its item until its result arrives.
    pub fn start(name: String, input: Value) -> (Self, ItemContent) {
        let (item, content) = ToolItem::started(name, input);
        (Self(item), content)
    }

    /// The finished item, from the `tool_result` block and the line's
    /// `toolUseResult`.
    pub fn finish(self, block: &Value, tool_use_result: Option<&Value>) -> ItemContent {
        self.0.finished(block, tool_use_result)
    }
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

struct PendingApproval {
    input: Value,
    suggestions: HashMap<String, Value>,
}

/// Where a child item's lifecycle stands, as far as the canonical stream has
/// been told. An update keeps its snapshot so only a changed one passes.
enum ChildItemStage {
    Started,
    Updated(ItemContent),
    Completed,
}

/// A rewind, or a preview of one, made of the control requests Claude Code's
/// own rewind menu makes, one at a time.
struct PendingRewind {
    checkpoint_id: String,
    mode: RewindMode,
    step: RewindStep,
    /// What the files step restored, once it has.
    files: Option<RewindFiles>,
    /// Why the files step failed; the conversation is still rewound after it,
    /// as the native menu does.
    files_failure: Option<RewindFailure>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RewindStep {
    /// `rewind_files` has no `interrupt_if_running`: a running turn is
    /// stopped first so the restore does not race the agent's own edits.
    Interrupt,
    Files,
    Conversation,
    Preview,
}

#[derive(Default)]
struct AssistantMessageState {
    blocks_seen: usize,
    model: Option<String>,
}

pub(crate) struct Mapper {
    session_id: Option<String>,
    current_message_id: Option<String>,
    /// How many content blocks of each streamed message we have already seen in
    /// an `assistant` line. The CLI splits one message across several `assistant`
    /// lines — one per block, each carrying a *one-element* `content` array — so
    /// enumerating that array gives 0 every time, while the `content_block_delta`
    /// stream numbers the blocks 0, 1, 2… The completed item has to reuse the
    /// stream's numbering or the timeline shows the same text twice: once from
    /// the deltas (`msg:1`) and once from the completion (`msg:0`).
    assistant_messages: HashMap<String, AssistantMessageState>,
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
    /// Subagents whose transcript tail has not yet acknowledged its Stop. A
    /// terminal Subagent snapshot is held in `pending_subagent_terminals` until
    /// then, so the mirror's turn closes after the tail's final child items.
    tailed: HashSet<String>,
    pending_subagent_terminals: HashMap<String, AgentEvent>,
    /// The last lifecycle stage each child item reached through either feed
    /// (stdout `parent_tool_use_id` lines or the transcript tail); the other
    /// feed's copy of the same stage is dropped and a stale start cannot
    /// reopen a completed item.
    child_items: HashMap<String, ChildItemStage>,
    pending_approvals: HashMap<String, PendingApproval>,
    /// The questions the user has open, by control request_id.
    pending_user_input: HashMap<String, PendingUserInput>,
    /// Set when we send an `interrupt` control_request; the next non-success
    /// `result` is then attributed to the interrupt rather than a failure
    /// (the CLI's result carries no reliable interrupt marker).
    interrupt_pending: bool,
    /// Permission mode currently applied on the CLI, so we only switch on change.
    applied_permission_mode: String,
    auto_unavailable: Option<String>,
    /// Live permission-mode requests awaiting Claude's correlated response.
    /// A successful stdin write is not proof that the CLI applied the mode.
    pending_permission_modes: HashMap<String, String>,
    /// `background_tasks` requests awaiting Claude's answer, by control
    /// request_id: a refusal is the only word of one that did nothing.
    pending_background_moves: HashSet<String>,
    /// [`SessionCommand::ReadAccountUsage`] askers by control request_id.
    pending_usage_reads: HashMap<String, smol::channel::Sender<Result<Value, String>>>,
    /// The `initialize` request awaiting Claude's answer.
    pending_initialize: Option<String>,
    /// Worktree hook callbacks awaiting the host, by control request_id.
    pending_worktrees: HashMap<String, WorktreeRequest>,
    /// Worktrees given to `isolation: "worktree"` subagents whose
    /// `task_started` has not come yet, by agent id: (the directory that
    /// asked, the worktree).
    subagent_worktrees: HashMap<String, (PathBuf, PathBuf)>,
    /// The directory the process was started in, and the one the session works
    /// in now, as far as Claude has said. `None` in fixtures that start nowhere.
    launch_cwd: Option<PathBuf>,
    cwd: Option<PathBuf>,
    /// The worktrees Tcode made for this thread: those it had when the
    /// process started and each the host has given since.
    worktrees: Vec<PathBuf>,
    /// Where the session worked before `EnterWorktree` took it into a
    /// worktree, which `ExitWorktree` returns it to.
    before_worktree: Option<PathBuf>,
    outgoing: Vec<Value>,
    request_usage: Value,
    latest_usage: TokenUsage,
    usage_message_id: Option<String>,
    usage_message_ids: HashSet<String>,
    result_ids: HashSet<String>,
    /// Successfully written steers awaiting the CLI's next input checkpoint.
    pending_steers: VecDeque<String>,
    /// Once the CLI exposes request checkpoints, never use the legacy fallback.
    saw_requesting: bool,
    /// Tasks still owned by this Claude process, whatever their `task_type`:
    /// a Monitor watch or a backgrounded subagent keeps the process busy
    /// exactly like a background Bash command. When the list becomes empty
    /// Claude normally re-invokes itself with the completion notification, so
    /// zero is not published until that follow-up result, or until the CLI
    /// goes idle without one, as it does for a stopped task.
    background_tasks: Vec<crate::BackgroundTask>,
    /// The list emptied and its zero has not been published.
    background_drain_withheld: bool,
    /// What the process runs now.
    applied_settings: ClaudeLaunchOptions,
    pending_settings: Option<PendingSettings>,
    /// The `get_settings` read awaiting its answer.
    pending_settings_read: Option<String>,
    /// The model the session is set to as Claude last reported it.
    reported_model: Option<String>,
    /// Fast mode as Claude last reported it.
    reported_fast_mode: Option<bool>,
    /// Native rewind control requests awaiting Claude's correlated response.
    pending_rewinds: HashMap<String, PendingRewind>,
    /// What the latest dry run of each restore point found: Claude answers a
    /// real `rewind_files` with skipped links alone.
    rewind_previews: HashMap<String, RewindFiles>,
    /// The uuids steers were sent under, to the steer's request id. Their
    /// replayed echo makes them conversation restore points.
    steer_uuids: HashMap<String, String>,
    /// Last assistant model published to the canonical event stream.
    last_served_model: Option<String>,
    /// Reset time reported for a rejected usage window in the active turn.
    limit_resets_at: Option<u64>,
    /// Warning-bearing stop reason already emitted for this occurrence.
    warned_stop_reason: Option<String>,
    /// The `apply_flag_settings` turning on `bashEditDiffEnabled`, awaiting
    /// its answer.
    pending_edit_diff: Option<String>,
}

impl Mapper {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::new_configured("default".to_owned())
    }

    fn new_configured(applied_permission_mode: String) -> Self {
        Mapper {
            session_id: None,
            current_message_id: None,
            assistant_messages: HashMap::new(),
            turn_counter: 0,
            current_turn_id: None,
            awaiting_turn_checkpoint: false,
            control_counter: 0,
            tool_items: HashMap::new(),
            task_tools: HashMap::new(),
            child_mappers: HashMap::new(),
            tail_requests: Vec::new(),
            tailed: HashSet::new(),
            pending_subagent_terminals: HashMap::new(),
            child_items: HashMap::new(),
            pending_approvals: HashMap::new(),
            pending_user_input: HashMap::new(),
            interrupt_pending: false,
            applied_permission_mode,
            auto_unavailable: None,
            pending_permission_modes: HashMap::new(),
            pending_background_moves: HashSet::new(),
            pending_usage_reads: HashMap::new(),
            pending_initialize: None,
            pending_worktrees: HashMap::new(),
            subagent_worktrees: HashMap::new(),
            launch_cwd: None,
            cwd: None,
            worktrees: Vec::new(),
            before_worktree: None,
            outgoing: Vec::new(),
            request_usage: json!({}),
            latest_usage: TokenUsage::default(),
            usage_message_id: None,
            usage_message_ids: HashSet::new(),
            result_ids: HashSet::new(),
            pending_steers: VecDeque::new(),
            saw_requesting: false,
            background_tasks: Vec::new(),
            background_drain_withheld: false,
            applied_settings: ClaudeLaunchOptions::default(),
            pending_settings: None,
            pending_settings_read: None,
            reported_model: None,
            reported_fast_mode: None,
            pending_rewinds: HashMap::new(),
            rewind_previews: HashMap::new(),
            steer_uuids: HashMap::new(),
            last_served_model: None,
            limit_resets_at: None,
            warned_stop_reason: None,
            pending_edit_diff: None,
        }
    }

    /// Drain queued control-response writes for the actor to send.
    fn take_outgoing(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.outgoing)
    }

    fn take_tail_requests(&mut self) -> Vec<TailRequest> {
        std::mem::take(&mut self.tail_requests)
    }

    async fn on_wire_message(&mut self, msg: Value, claude_dir: Option<&Path>) -> Vec<AgentEvent> {
        let resumed = msg
            .pointer("/tool_use_result/resumedAgentId")
            .and_then(Value::as_str);
        let task_id = resumed.or_else(|| {
            (msg.get("type").and_then(Value::as_str) == Some("system"))
                .then(|| msg.get("task_id").and_then(Value::as_str))
                .flatten()
        });
        let session_id = msg
            .get("session_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .or(self.session_id.as_deref())
            .unwrap_or_default()
            .to_owned();
        if let Some(task_id) = task_id
            && self.task_tools.get(task_id).is_none_or(|id| {
                !matches!(self.tool_items.get(id), Some(ToolItem::Subagent { .. }))
            })
            && let Some(root) = claude_dir
            && !session_id.is_empty()
        {
            // A resumed child's notification names SendMessage, while its
            // transcript still names the original Agent call, even after a
            // parent process restart. Claude's metadata preserves that identity.
            let root = root.to_owned();
            let session = session_id.clone();
            let task = task_id.to_owned();
            if let Some(item) = crate::process::unblock(move || {
                crate::subagent_tail::read_subagent_spawn(&root, &session, &task)
            })
            .await
            {
                self.task_tools.insert(task_id.to_owned(), item.id.clone());
                if let Some(tool) = ToolItem::subagent(&item.content, item.parent_item_id) {
                    self.tool_items.entry(item.id).or_insert(tool);
                }
            }
        }
        let resumed = resumed.and_then(|task_id| self.task_tools.get(task_id).cloned());
        let mut events = self.on_message(msg);
        if let Some(parent_id) = resumed {
            events.extend(self.update_subagent(&parent_id, ItemStatus::InProgress, None));
        }
        events
    }

    /// Map one CLI stdout message to zero or more outcomes.
    pub(crate) fn on_message(&mut self, msg: Value) -> Vec<AgentEvent> {
        if let Some(parent_id) = msg
            .get("parent_tool_use_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            let mut events = match crate::subagent_tail::model_and_effort(&msg) {
                Some((model, effort)) => self.note_subagent_model(parent_id, model, effort),
                None => Vec::new(),
            };
            let child = self
                .child_mappers
                .entry(parent_id.to_owned())
                .or_insert_with(|| crate::subagent_tail::TranscriptMapper::new(parent_id))
                .map_value(&msg);
            events.extend(self.fold_child_events(child));
            return events;
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
            Some("control_cancel_request") => self.on_control_cancel_request(&msg),
            Some("control_response") => self.on_control_response(&msg),
            Some("rate_limit_event") => self.on_rate_limit_event(&msg),
            Some("result") => self.on_result(&msg),
            other => {
                log::debug!("claude: ignoring message type {other:?}");
                Vec::new()
            }
        }
    }

    fn permission_options(&self) -> AgentEvent {
        let mut descriptor =
            crate::permission_control(ProviderKind::ClaudeCode).expect("Claude permission control");
        if let OptionDescriptor::Select { options, .. } = &mut descriptor {
            if let Some(auto) = options.iter_mut().find(|option| option.value == "auto") {
                auto.unavailable.clone_from(&self.auto_unavailable);
            }
            if !options
                .iter()
                .any(|option| option.value == self.applied_permission_mode)
            {
                options.push(crate::SelectOption {
                    value: self.applied_permission_mode.clone(),
                    label: self.applied_permission_mode.clone(),
                    description: None,
                    unavailable: Some(if self.applied_permission_mode == "plan" {
                        "Entered by the agent".into()
                    } else {
                        "Reported by Claude".into()
                    }),
                    speed: None,
                    summary: None,
                    approval: None,
                });
            }
        }
        AgentEvent::ProviderOptions {
            descriptors: vec![descriptor],
            selections: vec![OptionSelection {
                id: "permissionMode".into(),
                value: json!(self.applied_permission_mode),
            }],
        }
    }

    fn on_rate_limit_event(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let info = msg.get("rate_limit_info");
        self.limit_resets_at = info
            .filter(|info| info.get("status").and_then(Value::as_str) == Some("rejected"))
            .and_then(|info| info.get("resetsAt"))
            .and_then(Value::as_u64);
        Vec::new()
    }

    fn synthesized_turn_started(&mut self) -> Vec<AgentEvent> {
        if self.session_id.is_some() && self.current_turn_id.is_none() {
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
            Some("compact_boundary") => {
                self.request_usage = json!({});
                self.usage_message_id = None;
                self.current_message_id = None;
                self.latest_usage.used_tokens = None;
                self.latest_usage.input_tokens = None;
                self.latest_usage.cached_input_tokens = None;
                self.latest_usage.output_tokens = None;
                self.latest_usage.freshness = crate::ContextFreshness::AwaitingObservation;
                return vec![AgentEvent::ContextCompacted(crate::Compaction {
                    in_progress: false,
                    trigger: msg
                        .pointer("/compact_metadata/trigger")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    pre_tokens: msg
                        .pointer("/compact_metadata/pre_tokens")
                        .and_then(Value::as_u64),
                    post_tokens: msg
                        .pointer("/compact_metadata/post_tokens")
                        .and_then(Value::as_u64),
                    dropped_tokens: msg
                        .pointer("/compact_metadata/cumulative_dropped_tokens")
                        .and_then(Value::as_u64),
                    duration_ms: msg
                        .pointer("/compact_metadata/duration_ms")
                        .and_then(Value::as_u64),
                })];
            }
            Some("status") if msg.get("status").and_then(Value::as_str) == Some("compacting") => {
                self.latest_usage.freshness = crate::ContextFreshness::Compacting;
                return vec![AgentEvent::ContextCompacted(crate::Compaction {
                    in_progress: true,
                    trigger: None,
                    pre_tokens: None,
                    ..Default::default()
                })];
            }
            Some("task_started") => return self.on_task_started(msg),
            Some("task_updated") => return self.on_task_updated(msg),
            Some("task_notification") => return self.on_task_notification(msg),
            Some("background_tasks_changed") => return self.on_background_tasks_changed(msg),
            Some("session_state_changed")
                if msg.get("state").and_then(Value::as_str) == Some("idle")
                    && self.background_drain_withheld =>
            {
                self.background_drain_withheld = false;
                return vec![AgentEvent::BackgroundTasksChanged { count: 0 }];
            }
            // `{status: null, permissionMode}` on every change of the mode,
            // the model's own included (2.1.322).
            Some("status") => {
                let Some(mode) = msg.get("permissionMode").and_then(Value::as_str) else {
                    log::debug!("claude: ignoring system/status {:?}", msg.get("status"));
                    return Vec::new();
                };
                if mode == "auto" {
                    self.auto_unavailable = None;
                }
                self.applied_permission_mode = mode.to_owned();
                return vec![self.permission_options()];
            }
            Some("model_refusal_fallback") => return self.on_model_refusal_fallback(msg),
            Some("api_retry") => return vec![api_retry_notice(msg)],
            other => {
                log::debug!("claude: ignoring system/{other:?}");
                return Vec::new();
            }
        }
        if let Some(mode) = msg.get("permissionMode").and_then(Value::as_str) {
            if mode == "auto" {
                self.auto_unavailable = None;
            } else if self.session_id.is_none() && self.applied_permission_mode == "auto" {
                self.auto_unavailable =
                    Some("Not available: the session started in another mode".into());
            }
            self.applied_permission_mode = mode.to_owned();
        }
        // A resumed session goes on in the worktree it had entered, which
        // Claude restores itself (2.1.322) wherever it was started. Any other
        // directory here is where the Bash tool's shell last cd'd to, which
        // Claude carries from turn to turn and does not restore on resume:
        // that is not the session moving.
        let moved = msg
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|cwd| {
                self.worktrees
                    .iter()
                    .any(|worktree| same_directory(worktree, Path::new(cwd)))
            })
            .and_then(|cwd| self.move_session(PathBuf::from(cwd)));
        // Each turn's init names the model the turn runs and fast mode's state.
        let fast_mode = self.observe_fast_mode(msg.get("fast_mode_state"));
        if self.session_id.is_some() {
            let mut events = vec![self.permission_options()];
            events.extend(
                msg.get("model")
                    .and_then(Value::as_str)
                    .and_then(|model| self.observe_model(model)),
            );
            events.extend(fast_mode);
            events.extend(moved);
            return events;
        }
        let session_id = match msg.get("session_id").and_then(Value::as_str) {
            Some(id) => id.to_string(),
            None => return fast_mode.into_iter().collect(),
        };
        self.session_id = Some(session_id.clone());
        let model = msg
            .get("model")
            .and_then(Value::as_str)
            .map(|model| self.tcode_model(model));
        self.reported_model.clone_from(&model);
        let mut events = vec![
            AgentEvent::SessionStarted {
                provider_session_id: session_id.clone(),
                resume: ResumeCursor(json!({ "session_id": session_id })),
                model,
            },
            self.permission_options(),
            // A thread an older CLI ran was recorded as having no rewind
            // controls; this session has them.
            AgentEvent::RewindAvailability { unavailable: None },
        ];
        events.extend(fast_mode);
        // The `slash_commands` (Command) and `skills` (Skill) arrays feed the
        // composer's `/` and `$` menus. Both are arrays of names (no descriptions).
        let commands = parse_provider_commands(msg);
        if !commands.is_empty() {
            events.push(AgentEvent::ProviderCommands { commands });
        }
        events.extend(moved);
        events
    }

    fn on_model_refusal_fallback(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let Some(actual) = msg.get("fallback_model").and_then(Value::as_str) else {
            log::debug!("claude: ignoring malformed model_refusal_fallback");
            return Vec::new();
        };
        self.observe_served_model(&json!({ "model": actual }), msg.get("original_model"))
    }

    fn accept_pending_steers(&mut self) -> Vec<AgentEvent> {
        self.pending_steers
            .drain(..)
            .map(|request_id| AgentEvent::SteerAccepted { request_id })
            .collect()
    }

    fn on_result(&mut self, msg: &Value) -> Vec<AgentEvent> {
        if let Some(id) = msg.get("uuid").and_then(Value::as_str)
            && !self.result_ids.insert(id.to_owned())
        {
            return Vec::new();
        }
        // A task notification the CLI closes without asking the model ran no
        // turn, so it ends none: a resumed session reports its stopped
        // background work this way before it takes up the message just sent.
        if self.current_turn_id.is_some()
            && msg.pointer("/origin/kind").and_then(Value::as_str) == Some("task-notification")
            && msg.get("num_turns").and_then(Value::as_u64) == Some(0)
        {
            self.background_drain_withheld = false;
            return vec![AgentEvent::BackgroundTasksChanged {
                count: self.background_tasks.len(),
            }];
        }
        self.awaiting_turn_checkpoint = false;
        let mut events = self.observe_stop_reason(msg.get("stop_reason").and_then(Value::as_str));
        events.extend(self.observe_fast_mode(msg.get("fast_mode_state")));
        let turn_id = self
            .current_turn_id
            .take()
            .unwrap_or_else(|| format!("turn-{}", self.turn_counter.max(1)));
        // A turn stopped before the model wrote anything: the CLI may close
        // it as a success, with nothing in it.
        let answered = !self.assistant_messages.is_empty();
        // No message outlives its turn, so the block counters can go.
        self.assistant_messages.clear();
        let mut status = result_status(msg);
        if std::mem::take(&mut self.interrupt_pending)
            && (status != TurnStatus::Completed || !answered)
        {
            status = TurnStatus::Interrupted;
        }
        let usage = msg.get("usage").map(|u| {
            let aggregate = map_usage(u, msg.get("modelUsage"));
            let processed = [
                "input_tokens",
                "cache_read_input_tokens",
                "cache_creation_input_tokens",
                "output_tokens",
            ]
            .into_iter()
            .any(|key| u.get(key).and_then(Value::as_u64).is_some())
            .then(|| crate::processed_tokens(aggregate));
            let mut usage = self.latest_usage;
            // Select capacity for the main model; a child's larger window is irrelevant.
            usage.context_window = self
                .last_served_model
                .as_deref()
                .and_then(|model| {
                    msg.get("modelUsage")?
                        .as_object()?
                        .iter()
                        .find(|(key, value)| {
                            key.split('[').next() == Some(model)
                                || value.get("canonicalModel").and_then(Value::as_str)
                                    == Some(model)
                        })?
                        .1
                        .get("contextWindow")?
                        .as_u64()
                })
                .or_else(|| {
                    msg.get("modelUsage")?
                        .as_object()
                        .filter(|m| m.len() == 1)?
                        .values()
                        .next()?
                        .get("contextWindow")?
                        .as_u64()
                })
                .or(usage.context_window);
            usage.turn_processed_tokens = processed;
            // The timeline owns lifetime accumulation, including restored history.
            usage.total_processed_tokens = None;
            usage.cost_usd = msg.get("total_cost_usd").and_then(Value::as_f64);
            usage.duration_ms = msg.get("duration_ms").and_then(Value::as_u64);
            self.latest_usage = usage;
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
            if let Some(resets_at) = self.limit_resets_at.take() {
                events.push(AgentEvent::UsageLimitReached { resets_at });
            }
        }
        self.limit_resets_at = None;
        // Publish the current background-task liveness with every result, not
        // only task-notification-origin ones. `background_tasks_changed []`
        // deliberately withholds the zero (see `on_background_tasks_changed`),
        // so a result is the only place the runtime can learn the tasks are
        // gone — and gating that on the CLI's `origin.kind` shape left the
        // count pinned above zero (session stuck "Working") whenever the
        // follow-up result arrived with a different origin. Re-publishing an
        // unchanged count is harmless; a parked runtime still observes zero
        // only after all completion output has landed.
        self.background_drain_withheld = false;
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

/// Claude Code keeps the turn open while it retries a failed API request, up to
/// `max_retries` times.
fn api_retry_notice(msg: &Value) -> AgentEvent {
    let count = |key| msg.get(key).and_then(Value::as_u64);
    let mut cause = msg
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    if let Some(status) = count("error_status") {
        cause.push_str(&format!(", HTTP {status}"));
    }
    if let Some(waited) = msg
        .pointer("/no_response/waited_ms")
        .and_then(Value::as_u64)
    {
        cause.push_str(&format!(", no response in {}s", waited / 1000));
    }
    let attempt = count("attempt").map_or("?".into(), |n| n.to_string());
    let max = count("max_retries").map_or("?".into(), |n| n.to_string());
    let delay =
        count("retry_delay_ms").map_or("?".into(), |ms| format!("{:.1}s", ms as f64 / 1000.0));
    AgentEvent::RequestRetry {
        message: format!("claude API request failed ({cause}); retry {attempt}/{max} in {delay}"),
    }
}

/// What Claude Code's own tool `name` is, from its input and, once it has
/// finished, its result text and `tool_use_result` (the tool's output
/// object; a string when the call failed). Returns the family, and the
/// output the call shows: the result text, or the part of it the family
/// leaves once its fields are read out. Shapes are 2.1.322's
/// (`sdk-tools.d.ts` of `@anthropic-ai/claude-agent-sdk` 0.3.322).
fn tool_family(
    name: &str,
    input: &Value,
    result: Option<(String, Option<&Value>)>,
) -> (Option<ToolFamily>, Option<String>) {
    let text = |key: &str| input.get(key).and_then(Value::as_str);
    let (output, structured) = match result {
        Some((text, structured)) => (Some(text), structured.filter(|value| value.is_object())),
        None => (None, None),
    };
    let family = match name {
        // `FileReadOutput`: a text read is `{type: "text", file: {filePath,
        // content, startLine, ..}}`, the content without the `N\t` numbers
        // the tool result puts before each line. The content stands for the
        // result only while the result is those lines and nothing else.
        "Read" => {
            let Some(path) = text("file_path") else {
                return (None, output);
            };
            let file = structured
                .filter(|result| result.get("type").and_then(Value::as_str) == Some("text"))
                .and_then(|result| result.get("file"));
            if let Some(file) = file
                && let Some(content) = file.get("content").and_then(Value::as_str)
            {
                let first_line = file.get("startLine").and_then(Value::as_u64);
                // An empty read's result is only Claude Code's warning that
                // the file is empty or shorter than the offset.
                if content.is_empty() {
                    return (
                        Some(ToolFamily::Read {
                            path: path.to_owned(),
                            first_line,
                            note: output.filter(|text| !text.trim().is_empty()),
                        }),
                        Some(String::new()),
                    );
                }
                if let Some(first_line) = first_line
                    && output.as_deref().and_then(numbered_read)
                        == Some((first_line, content.to_owned()))
                {
                    return (
                        Some(ToolFamily::Read {
                            path: path.to_owned(),
                            first_line: Some(first_line),
                            note: None,
                        }),
                        Some(content.to_owned()),
                    );
                }
            }
            // A subagent's transcript can leave the output object out; its
            // tool result numbers the lines it read.
            if structured.is_none()
                && let Some((first_line, content)) = output.as_deref().and_then(numbered_read)
            {
                return (
                    Some(ToolFamily::Read {
                        path: path.to_owned(),
                        first_line: Some(first_line),
                        note: None,
                    }),
                    Some(content),
                );
            }
            ToolFamily::Read {
                path: path.to_owned(),
                first_line: None,
                note: None,
            }
        }
        // `GrepOutput`: `filenames` in `files_with_matches` mode (relative
        // to the working directory), `content` in `content` mode, rg's
        // `path:N:text` lines (`N:text` when it searched one file).
        "Grep" => {
            let path = text("path").map(str::to_owned);
            let hits = structured.and_then(|result| {
                match text("output_mode").unwrap_or("files_with_matches") {
                    "files_with_matches" => filenames(result),
                    "content" => result
                        .get("content")
                        .and_then(Value::as_str)
                        .map(|content| grep_lines(content, path.as_deref())),
                    _ => None,
                }
            });
            ToolFamily::Search {
                scope: SearchScope::Content,
                pattern: text("pattern").map(str::to_owned),
                path,
                glob: text("glob").map(str::to_owned),
                hits,
            }
        }
        // `GlobOutput`: the files it found, as `filenames`.
        "Glob" => ToolFamily::Search {
            scope: SearchScope::Files,
            pattern: text("pattern").map(str::to_owned),
            path: text("path").map(str::to_owned),
            glob: None,
            hits: structured.and_then(filenames),
        },
        "WebFetch" => {
            let Some(url) = text("url") else {
                return (None, output);
            };
            ToolFamily::Fetch {
                url: url.to_owned(),
                prompt: text("prompt").map(str::to_owned),
            }
        }
        // `WebSearchOutput.results`: the model's own words, or a search's
        // `{tool_use_id, content: [{title, url}]}`.
        "WebSearch" => {
            let Some(query) = text("query") else {
                return (None, output);
            };
            ToolFamily::WebSearch {
                query: query.to_owned(),
                links: structured
                    .and_then(|result| result.get("results"))
                    .and_then(Value::as_array)
                    .map(|results| {
                        results
                            .iter()
                            .filter_map(|result| result.get("content")?.as_array())
                            .flatten()
                            .filter_map(|link| {
                                Some(WebLink {
                                    title: link
                                        .get("title")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_owned(),
                                    url: link.get("url")?.as_str()?.to_owned(),
                                })
                            })
                            .collect()
                    }),
            }
        }
        // Not in `sdk-tools.d.ts`; 2.1.322's own schema: `{query}`, and
        // `{matches, query, total_deferred_tools}` once it loaded them.
        "ToolSearch" => {
            let Some(query) = text("query") else {
                return (None, output);
            };
            ToolFamily::LoadTools {
                query: query.to_owned(),
                tools: structured
                    .and_then(|result| result.get("matches"))
                    .and_then(Value::as_array)
                    .map(|matches| {
                        matches
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    }),
            }
        }
        // 2.1.322's own schema: `{skill, args?}`.
        "Skill" => match text("skill") {
            Some(name) => ToolFamily::Skill {
                name: name.to_owned(),
            },
            None => return (None, output),
        },
        // `AskUserQuestionInput`'s questions, and `AskUserQuestionOutput`'s
        // `answers`, keyed by question text: one string, a multi-select's
        // labels joined with ", " (a record written before 2.1.3xx joined
        // them holds the list).
        "AskUserQuestion" => {
            let Some(asked) = input.get("questions").and_then(Value::as_array) else {
                return (None, output);
            };
            let answers = structured.and_then(|result| result.get("answers"));
            ToolFamily::Question {
                questions: asked
                    .iter()
                    .filter_map(|question| {
                        let text = question.get("question")?.as_str()?;
                        let options: Vec<UserInputOption> = question
                            .get("options")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(|option| {
                                Some(UserInputOption {
                                    label: option.get("label")?.as_str()?.to_owned(),
                                    description: option
                                        .get("description")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_owned(),
                                })
                            })
                            .collect();
                        let answers = match answers.and_then(|answers| answers.get(text)) {
                            Some(Value::String(answer)) => chosen(answer, &options),
                            Some(Value::Array(answers)) => answers
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect(),
                            _ => Vec::new(),
                        };
                        Some(AskedQuestion {
                            question: text.to_owned(),
                            options,
                            answers,
                        })
                    })
                    .collect(),
            }
        }
        _ => return (None, output),
    };
    (Some(family), output)
}

/// The options an AskUserQuestion answer names, in order. Claude Code joins
/// a multi-select's labels with ", " (JSON-quoting a label that holds ", "
/// or a quote when its own picker joined them), so the answer is read as
/// the options it is made of rather than split at every comma; what no
/// option begins is the user's own words.
fn chosen(answer: &str, options: &[UserInputOption]) -> Vec<String> {
    let mut picked = Vec::new();
    let mut rest = answer;
    while !rest.is_empty() {
        let next = options
            .iter()
            .flat_map(|option| {
                let quoted = serde_json::to_string(&option.label).unwrap_or_default();
                [option.label.len(), quoted.len()]
                    .into_iter()
                    .zip([option.label.clone(), quoted])
                    .map(move |(length, token)| (length, token, option.label.clone()))
            })
            .filter(|(length, token, _)| {
                rest.starts_with(token.as_str())
                    && (rest.len() == *length || rest[*length..].starts_with(", "))
            })
            .max_by_key(|(length, _, _)| *length);
        let Some((length, _, label)) = next else {
            picked.push(rest.to_owned());
            break;
        };
        picked.push(label);
        rest = rest[length..].strip_prefix(", ").unwrap_or_default();
    }
    picked
}

/// A Grep or Glob output's `filenames`, each a file it found.
fn filenames(result: &Value) -> Option<Vec<SearchHit>> {
    let names = result.get("filenames")?.as_array()?;
    Some(
        names
            .iter()
            .filter_map(Value::as_str)
            .map(|path| SearchHit {
                path: path.to_owned(),
                line: None,
                text: None,
            })
            .collect(),
    )
}

/// The matches among a content-mode Grep's lines: `path:N:text`, or `N:text`
/// when it searched the one file `searched`. Context lines (`path-N-text`)
/// and group separators (`--`) are not matches.
fn grep_lines(content: &str, searched: Option<&str>) -> Vec<SearchHit> {
    fn numbered(text: &str) -> Option<(u64, &str)> {
        let (number, rest) = text.split_once(':')?;
        Some((number.parse().ok()?, rest))
    }
    content
        .lines()
        .filter_map(|line| {
            let (path, (number, text)) = match numbered(line) {
                Some(found) => (searched?, found),
                None => line
                    .match_indices(':')
                    .find_map(|(at, _)| Some((&line[..at], numbered(&line[at + 1..])?)))?,
            };
            Some(SearchHit {
                path: path.to_owned(),
                line: Some(number),
                text: Some(text.to_owned()),
            })
        })
        .collect()
}

/// A Read tool result's numbered lines (`N\t` before each, numbered on from
/// the first, 2.1.322's format) as the first line's number and the lines
/// without their numbers; `None` for a result that holds anything else.
fn numbered_read(text: &str) -> Option<(u64, String)> {
    let mut rows = text.split('\n');
    let (first, line) = rows.next()?.split_once('\t')?;
    let first: u64 = first.parse().ok()?;
    let mut lines = vec![line];
    for (row, number) in rows.zip(first + 1..) {
        lines.push(row.strip_prefix(&format!("{number}\t"))?);
    }
    Some((first, lines.join("\n")))
}

fn is_file_tool(name: &str) -> bool {
    matches!(name, "Write" | "Edit" | "NotebookEdit")
}

fn is_command_tool(name: &str) -> bool {
    matches!(name, "Bash" | "PowerShell")
}

/// A command's error result as Claude Code 2.1.322 words it (its tool error
/// formatter starts with `Exit code ${code}` for a command that exited
/// non-zero): the code, and what follows it. Any other error is not an
/// exit, and a success says no code.
fn exited(text: &str) -> Option<(i32, &str)> {
    let (first, rest) = text.split_once('\n').unwrap_or((text, ""));
    let code = first.strip_prefix("Exit code ")?.trim().parse().ok()?;
    Some((code, rest))
}

/// What a finished subagent's record leaves to show. A `handback` outcome
/// means its text is a note to the model and the report comes beside it, the
/// warning auto mode raised above the report's text; `withheld` has no report.
/// With no outcome the agent did not report through `SubagentHandback`, and
/// `text` is what it ended with.
fn handback_summary(
    handback: Option<&Value>,
    report: Option<&Value>,
    text: Option<String>,
) -> Option<String> {
    if handback.is_none() {
        return text.filter(|text| !is_handback_note(text));
    }
    let report = report?;
    let text = report
        .get("text")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())?;
    Some(match report.get("warning").and_then(Value::as_str) {
        Some(warning) => format!("{warning}\n\n{text}"),
        None => text.to_owned(),
    })
}

/// Whether a subagent's result is Claude Code's note to the model about a
/// `SubagentHandback` rather than a report. Only the sentence tells: an Agent
/// tool_result inside a subagent has no `tool_use_result`, and a
/// task_notification for an agent waiting on its own work has no `handback`
/// (`subagent_handback_nested_trace.jsonl`, `subagent_withheld_nested_trace.jsonl`;
/// the waiting sentence is from the 2.1.317 bundle).
pub(crate) fn is_handback_note(text: &str) -> bool {
    let text = text.trim_start();
    [
        "This agent's report was delivered to you",
        "This agent has not reported yet",
        "The subagent ended without delivering a report",
    ]
    .iter()
    .any(|note| text.starts_with(note))
}

/// Run `claude --version` and parse the semver triple; `None` on any failure.
async fn claude_version(
    binary: Option<&Path>,
    launch_env: &LaunchEnv,
    processes: &crate::SessionProcesses,
) -> Option<(u32, u32, u32)> {
    // Resolve through the PATH search (PATHEXT-aware: on Windows the CLI only
    // exists as `claude.cmd`), falling back to the bare name so the failure is
    // reported by the OS exactly as before.
    let bin = crate::resolve_binary(binary, "claude")
        .unwrap_or_else(|_| std::path::PathBuf::from("claude"));
    crate::process::probe_version(&bin, launch_env, ProviderKind::ClaudeCode, processes).await
}

/// List Claude's models: the manifest catalog (refreshed per `refresh`),
/// gated by the installed CLI version, and what of it could not be read.
pub async fn list_models(
    binary_path: Option<PathBuf>,
    launch_env: LaunchEnv,
    refresh: CatalogRefresh,
    processes: &crate::SessionProcesses,
) -> Result<crate::ModelListing, AgentError> {
    crate::process::unblock(move || {
        crate::claude_manifest::refresh(refresh.cache_dir.as_deref(), refresh.network)
    })
    .await;
    let version = claude_version(binary_path.as_deref(), &launch_env, processes).await;
    Ok(crate::ModelListing {
        models: crate::claude_manifest::current().models_for_version(version),
        problems: crate::claude_manifest::problems(),
    })
}

mod control;
mod parse;
mod requests;
mod stream;
mod tasks;
pub use parse::*;

#[cfg(test)]
mod tests;
