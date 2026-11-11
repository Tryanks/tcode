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
    ApprovalRequest, Attachment, CatalogRefresh, DeltaKind, FileChange, FileChangeKind, GoalStatus,
    ItemContent, ItemStatus, LaunchEnv, ModelSpec, OptionDescriptor, OptionSelection, PlanStep,
    PlanStepStatus, ProviderCommand, ProviderCommandKind, ProviderKind, ResumeCursor, RewindMode,
    SearchHit, SearchScope, SessionCommand, SessionHandle, SessionOptions, ThreadItem, TokenUsage,
    ToolFamily, ToolOutput, TurnStatus, UserInputDelivery, UserInputOption, UserInputQuestion,
    WorktreeAnswer, WorktreeRequest, selection_bool, selection_str,
};

/// The hook callbacks `initialize` registers. With `onFailure: "block"` an error
/// answer fails the worktree tool instead of counting as an empty success, which
/// for `WorktreeRemove` would report a removal that did not happen.
const WORKTREE_CREATE_CALLBACK: &str = "tcode-worktree-create";
const WORKTREE_REMOVE_CALLBACK: &str = "tcode-worktree-remove";
/// The prefix of the worktree name Claude Code asks for an `isolation: "worktree"`
/// subagent, before its agent id (the `task_started` `task_id`).
const SUBAGENT_WORKTREE_PREFIX: &str = "agent-";

/// First Claude Code build whose headless control protocol is verified to
/// expose `rewind_conversation` alongside the SDK's `rewind_files` request.
const NATIVE_REWIND_MIN_VERSION: (u32, u32, u32) = (2, 1, 214);
/// The release whose `rewind_files` `dry_run` was read to restore nothing.
/// An older CLI that ignored the flag would restore files when the panel
/// merely opens, so it is not asked.
const REWIND_PREVIEW_MIN_VERSION: (u32, u32, u32) = (2, 1, 318);
/// The release whose print mode has `--resume-session-at`.
const FORK_AT_MIN_VERSION: (u32, u32, u32) = (2, 1, 223);

/// First Claude Code release with `--append-system-prompt-file` (a hidden
/// option in the published 2.0.33 `cli.js`; 2.0.32 lacks it).
const APPEND_SYSTEM_PROMPT_FILE_MIN_VERSION: (u32, u32, u32) = (2, 0, 33);

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
    let child_env = |key: &str| launch_env.child_path(ProviderKind::ClaudeCode, key);
    child_env("CLAUDE_CONFIG_DIR").or_else(|| child_env("HOME").map(|home| home.join(".claude")))
}

/// The CLI's account file, `.claude.json`: in `CLAUDE_CONFIG_DIR` when that
/// is set, else in the home directory itself rather than in `~/.claude`.
pub fn account_file(launch_env: &LaunchEnv) -> Option<PathBuf> {
    let child_env = |key: &str| launch_env.child_path(ProviderKind::ClaudeCode, key);
    account_file_in(child_env("CLAUDE_CONFIG_DIR"), child_env("HOME"))
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
    let version = claude_version(opts.binary_path.as_deref(), &opts.launch_env).await;
    let native_rewind = version.is_some_and(|version| version >= NATIVE_REWIND_MIN_VERSION);
    let rewind_preview = version.is_some_and(|version| version >= REWIND_PREVIEW_MIN_VERSION);
    let fork_anchor = match &opts.fork {
        Some(ForkPoint::Before { checkpoint_id }) => {
            if version.is_some_and(|version| version < FORK_AT_MIN_VERSION) {
                return Err(AgentError::Provider(
                    "forking from a message needs Claude Code 2.1.223 or later".into(),
                ));
            }
            Some(fork_anchor(
                &opts.resume,
                config_dir(&opts.launch_env).as_deref(),
                checkpoint_id,
            )?)
        }
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
    if native_rewind {
        // SDK file checkpointing is opt-in for print/stream-json hosts even
        // though the interactive CLI enables it automatically.
        cmd.env("CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING", "true");
    }
    // Passed on every launch: the CLI records the rendered system prompt with the conversation
    // and renders it again from the current launch after a compaction.
    // A file keeps the argument on one line for a Windows batch launcher; an
    // older CLI, or one whose version is unknown, gets the text itself.
    let file_supported =
        version.is_some_and(|version| version >= APPEND_SYSTEM_PROMPT_FILE_MIN_VERSION);
    let instructions_file = match opts.instructions.as_deref() {
        Some(text) if file_supported => Some(
            InstructionsFile::write(text)
                .map_err(|err| AgentError::Spawn(format!("writing Tcode's instructions: {err}")))?,
        ),
        _ => None,
    };
    let system_prompt = match &instructions_file {
        Some(file) => Some(SystemPrompt::File(&file.0)),
        None => opts.instructions.as_deref().map(SystemPrompt::Text),
    };
    cmd.args(launch_args(
        launch_permission_mode,
        native_rewind,
        &launch,
        resume_args(&opts.resume, opts.fork.is_some(), fork_anchor.as_deref()),
        &opts.mcp_servers,
        system_prompt,
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
        (native_rewind, rewind_preview),
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

/// How Tcode's instructions reach the system prompt.
enum SystemPrompt<'a> {
    /// The text itself, for a CLI without `--append-system-prompt-file`.
    Text(&'a str),
    /// A file holding the text.
    File(&'a Path),
}

/// The arguments a session launches Claude Code with.
fn launch_args(
    permission_mode: &str,
    native_rewind: bool,
    launch: &ClaudeLaunchOptions,
    resume: Vec<String>,
    mcp_servers: &[crate::McpRegistration],
    system_prompt: Option<SystemPrompt<'_>>,
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
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    if native_rewind {
        // User-message UUIDs are Claude's native checkpoint ids.
        args.push("--replay-user-messages".into());
    }
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
    match system_prompt {
        Some(SystemPrompt::Text(text)) => {
            args.extend(["--append-system-prompt".into(), text.into()]);
        }
        Some(SystemPrompt::File(path)) => {
            args.extend(["--append-system-prompt-file".into(), path.into()]);
        }
        None => {}
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
        let raw_effort = selection_str(selections, "reasoningEffort");
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
    (native_rewind, rewind_preview): (bool, bool),
    stderr_tail: crate::process::StderrTail,
    stderr_task: smol::Task<()>,
) {
    let mut mapper = Mapper::new_configured(applied_permission_mode, native_rewind);
    mapper.rewind_preview = rewind_preview;
    mapper.applied_settings = config.launch.clone();
    mapper.launch_cwd = Some(config.cwd.clone());
    mapper.cwd = Some(config.cwd.clone());
    let claude_dir = config.claude_dir.clone();
    // Before any user message, so no worktree is made without the host.
    let initialize = mapper.initialize_request();
    if let Err(error) = write_line(&mut stdin, &initialize).await {
        log::warn!("claude: could not register the worktree hooks: {error}");
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
            let failure =
                if !mapper.native_rewind || (mode.includes_files() && !mapper.rewind_preview) {
                    Some(RewindFailure::new(RewindFailureReason::Unsupported, None))
                } else {
                    match mapper.begin_preview(checkpoint_id.clone(), mode) {
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
            match mapper.begin_rewind(checkpoint_id.clone(), mode) {
                Ok(request) if write_line(stdin, &request).await.is_ok() => {}
                Ok(_) => {
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
                Err(error) => {
                    let _ = event_tx
                        .send(AgentEvent::RewindFailed {
                            checkpoint_id,
                            mode,
                            failure: RewindFailure::new(RewindFailureReason::Unsupported, error),
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
            let mut msg = user_message(&text, &attachments);
            // A steer sent under its own uuid comes back in the replayed
            // echo under it, which makes it a conversation restore point.
            let uuid = mapper
                .native_rewind
                .then(|| uuid::Uuid::new_v4().to_string());
            if let Some(uuid) = &uuid {
                msg["uuid"] = json!(uuid);
            }
            if write_steering_message(stdin, &msg, request_id.clone(), mapper, event_tx).await
                && let Some(uuid) = uuid
            {
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
        let status = if is_error {
            ItemStatus::Failed
        } else {
            ItemStatus::Completed
        };
        let ToolOutput { text, images } =
            ToolOutput::from_content(block.get("content").unwrap_or(&Value::Null));
        match self {
            ToolItem::Command { command, .. } => {
                let (output, exit_code) = match is_error.then(|| exited(&text)).flatten() {
                    Some((code, rest)) => (rest.to_owned(), Some(code)),
                    None => (text, None),
                };
                ItemContent::CommandExecution {
                    command,
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
                ItemContent::FileChange { changes, status }
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
    /// Pending `AskUserQuestion` prompts: control request_id → the original
    /// `questions` array, echoed back verbatim in the allow response.
    pending_user_input: HashMap<String, Value>,
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
    /// Native rewind control requests awaiting Claude's correlated response.
    pending_rewinds: HashMap<String, PendingRewind>,
    /// What the latest dry run of each restore point found: Claude answers a
    /// real `rewind_files` with skipped links alone.
    rewind_previews: HashMap<String, RewindFiles>,
    /// The uuids steers were sent under, to the steer's request id. Their
    /// replayed echo makes them conversation restore points.
    steer_uuids: HashMap<String, String>,
    native_rewind: bool,
    /// The CLI's `rewind_files` `dry_run` is known to restore nothing.
    rewind_preview: bool,
    /// Last assistant model published to the canonical event stream.
    last_served_model: Option<String>,
    /// Reset time reported for a rejected usage window in the active turn.
    limit_resets_at: Option<u64>,
    /// Warning-bearing stop reason already emitted for this occurrence.
    warned_stop_reason: Option<String>,
}

impl Mapper {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::new_configured("default".to_owned(), false)
    }

    fn new_configured(applied_permission_mode: String, native_rewind: bool) -> Self {
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
            pending_rewinds: HashMap::new(),
            rewind_previews: HashMap::new(),
            steer_uuids: HashMap::new(),
            native_rewind,
            rewind_preview: native_rewind,
            last_served_model: None,
            limit_resets_at: None,
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
        self.current_message_id = None;
        self.usage_message_id = None;
        self.request_usage = json!({});
        if self.latest_usage.freshness == crate::ContextFreshness::Current {
            self.latest_usage.freshness = crate::ContextFreshness::LastKnown;
        }
        self.turn_counter += 1;
        let id = format!("turn-{}", self.turn_counter);
        self.current_turn_id = Some(id.clone());
        self.awaiting_turn_checkpoint = self.native_rewind;
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
        let step = if !mode.includes_files() {
            RewindStep::Conversation
        } else if self.current_turn_id.is_some() {
            RewindStep::Interrupt
        } else {
            RewindStep::Files
        };
        Ok(self.rewind_request(PendingRewind {
            checkpoint_id,
            mode,
            step,
            files: None,
            files_failure: None,
        }))
    }

    /// A dry run of the files a rewind would restore; `None` when `mode`
    /// restores none, which needs no preview.
    fn begin_preview(&mut self, checkpoint_id: String, mode: RewindMode) -> Option<Value> {
        mode.includes_files().then(|| {
            self.rewind_request(PendingRewind {
                checkpoint_id,
                mode,
                step: RewindStep::Preview,
                files: None,
                files_failure: None,
            })
        })
    }

    fn rewind_request(&mut self, pending: PendingRewind) -> Value {
        let checkpoint_id = pending.checkpoint_id.as_str();
        if matches!(
            pending.step,
            RewindStep::Interrupt | RewindStep::Conversation
        ) && self.current_turn_id.is_some()
        {
            self.interrupt_pending = true;
        }
        let request = match pending.step {
            RewindStep::Interrupt => json!({ "subtype": "interrupt" }),
            RewindStep::Files => {
                json!({ "subtype": "rewind_files", "user_message_id": checkpoint_id })
            }
            RewindStep::Preview => json!({
                "subtype": "rewind_files",
                "user_message_id": checkpoint_id,
                "dry_run": true,
            }),
            // A target before a compaction boundary resolves to the summary
            // that replaced it, as the native menu allows.
            RewindStep::Conversation => json!({
                "subtype": "rewind_conversation",
                "target_message_uuid": checkpoint_id,
                "interrupt_if_running": true,
                "allow_summarized_target": true,
            }),
        };
        let message = self.control_request(request);
        let request_id = message["request_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        self.pending_rewinds.insert(request_id, pending);
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

    /// One `stop_task` for `task_id`, or for every task the process runs: the
    /// CLI has no way to stop them together.
    fn stop_task_requests(&mut self, task_id: Option<String>) -> Vec<Value> {
        let tasks = match task_id {
            Some(id) => vec![id],
            None => self
                .background_tasks
                .iter()
                .map(|task| task.id.clone())
                .collect(),
        };
        tasks
            .into_iter()
            .map(|task_id| {
                self.control_request(json!({ "subtype": "stop_task", "task_id": task_id }))
            })
            .collect()
    }

    /// Ctrl+B for the call `item_id`: Claude returns it to the model and
    /// its command or agent goes on as a background task.
    fn move_to_background_request(&mut self, item_id: String) -> Value {
        let message =
            self.control_request(json!({ "subtype": "background_tasks", "tool_use_id": item_id }));
        if let Some(request_id) = message.get("request_id").and_then(Value::as_str) {
            self.pending_background_moves.insert(request_id.to_owned());
        }
        message
    }

    /// The list again, once the call is known of a listed task that had
    /// none: Claude lists a task before it says which call started it.
    fn link_background_tasks(&mut self) -> Option<AgentEvent> {
        let mut linked = false;
        for task in &mut self.background_tasks {
            if task.item_id.is_none()
                && let Some(item_id) = self.task_tools.get(&task.id)
            {
                task.item_id = Some(item_id.clone());
                linked = true;
            }
        }
        linked.then(|| AgentEvent::BackgroundTaskList {
            tasks: self.background_tasks.clone(),
        })
    }

    /// The first request bringing the process to `target`, `None` when it runs
    /// that already. `Err` when the change takes a restart.
    fn begin_settings(&mut self, mut target: ClaudeLaunchOptions) -> Result<Option<Value>, ()> {
        if self.applied_settings.needs_restart_for(&target) {
            return Err(());
        }
        target.auto_compact_window = self.applied_settings.auto_compact_window;
        let flags = self.applied_settings.flag_changes(&target);
        let flags = (!flags.is_empty()).then(|| {
            json!({
                "subtype": "apply_flag_settings",
                "settings": flags,
            })
        });
        let (request, flags) = if self.applied_settings.model_id != target.model_id {
            (
                json!({ "subtype": "set_model", "model": target.model_id }),
                flags,
            )
        } else if let Some(flags) = flags {
            (flags, None)
        } else {
            self.applied_settings = target;
            return Ok(None);
        };
        let message = self.control_request(request);
        self.pending_settings = Some(PendingSettings {
            request_id: message["request_id"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            target,
            flags,
        });
        Ok(Some(message))
    }

    /// One step of a live settings change answered: send the flags that waited
    /// for the model, or report the outcome.
    fn on_settings_response(&mut self, response: &Value) -> Vec<AgentEvent> {
        let Some(mut pending) = self.pending_settings.take() else {
            return Vec::new();
        };
        if response.get("subtype").and_then(Value::as_str) != Some("success") {
            log::warn!("claude: live settings change refused: {response}");
            return vec![AgentEvent::SettingsApplied { applied: false }];
        }
        self.applied_settings.model_id = pending.target.model_id.clone();
        if let Some(flags) = pending.flags.take() {
            let message = self.control_request(flags);
            pending.request_id = message["request_id"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            self.outgoing.push(message);
            self.pending_settings = Some(pending);
            return Vec::new();
        }
        self.applied_settings = pending.target;
        vec![AgentEvent::SettingsApplied { applied: true }]
    }

    /// Build the `control_response` answering a pending `can_use_tool` prompt.
    fn build_approval_response(
        &mut self,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> Option<Value> {
        let pending = self.pending_approvals.get(request_id)?;
        let response = match decision {
            ApprovalDecision::Option(id) if id == "allow" => json!({
                "behavior": "allow", "updatedInput": pending.input,
            }),
            ApprovalDecision::Option(id) if id == "deny" => json!({
                "behavior": "deny", "message": "User declined tool execution.",
            }),
            ApprovalDecision::Option(id) => {
                let suggestions = pending.suggestions.get(&id)?;
                json!({ "behavior": "allow", "updatedInput": pending.input, "updatedPermissions": suggestions })
            }
            ApprovalDecision::Feedback(text) => json!({
                "behavior": "deny", "message": text,
            }),
            ApprovalDecision::Cancel => json!({
                "behavior": "deny", "message": "User cancelled tool execution.",
            }),
        };
        self.pending_approvals.remove(request_id);
        Some(control_response(request_id, response))
    }

    /// Build the `control_response` allowing a pending `AskUserQuestion` prompt,
    /// echoing the original `questions` alongside the collected `answers`.
    /// Returns `None` for an unknown request id.
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

    /// The `initialize` control request registering the worktree hooks.
    fn initialize_request(&mut self) -> Value {
        let message = self.control_request(json!({
            "subtype": "initialize",
            "hooks": {
                "WorktreeCreate": [
                    { "hookCallbackIds": [WORKTREE_CREATE_CALLBACK], "onFailure": "block" }
                ],
                "WorktreeRemove": [
                    { "hookCallbackIds": [WORKTREE_REMOVE_CALLBACK], "onFailure": "block" }
                ],
            },
        }));
        self.pending_initialize = message["request_id"].as_str().map(str::to_owned);
        message
    }

    /// A `hook_callback` control request: a worktree hook becomes a request to
    /// the host; any other is answered with no opinion.
    fn on_hook_callback(&mut self, request_id: String, request: &Value) -> Vec<AgentEvent> {
        let input = request.get("input").unwrap_or(&Value::Null);
        let field = |key: &str| input.get(key).and_then(Value::as_str);
        let worktree = match field("hook_event_name") {
            Some("WorktreeCreate") => {
                field("cwd")
                    .zip(field("name"))
                    .map(|(cwd, name)| WorktreeRequest::Create {
                        cwd: PathBuf::from(cwd),
                        name: name.to_owned(),
                    })
            }
            Some("WorktreeRemove") => field("worktree_path").map(|path| WorktreeRequest::Remove {
                path: PathBuf::from(path),
            }),
            _ => None,
        };
        let Some(worktree) = worktree else {
            log::debug!("claude: answering hook_callback {request} with no opinion");
            self.outgoing.push(control_response(&request_id, json!({})));
            return Vec::new();
        };
        self.pending_worktrees
            .insert(request_id.clone(), worktree.clone());
        vec![AgentEvent::WorktreeRequested {
            request_id,
            request: worktree,
        }]
    }

    /// The `control_response` giving a pending worktree hook the host's
    /// `answer`, and the request it answers; `None` for an unknown request id.
    fn worktree_response(
        &mut self,
        request_id: &str,
        answer: &WorktreeAnswer,
    ) -> Option<(Value, WorktreeRequest)> {
        let request = self.pending_worktrees.remove(request_id)?;
        let refusal = |error: &str| {
            json!({
                "type": "control_response",
                "response": { "subtype": "error", "request_id": request_id, "error": error },
            })
        };
        let response = match (&request, answer) {
            (WorktreeRequest::Create { cwd, name }, WorktreeAnswer::Ready { path, .. }) => {
                if let Some(agent_id) = name.strip_prefix(SUBAGENT_WORKTREE_PREFIX) {
                    self.subagent_worktrees
                        .insert(agent_id.to_owned(), (cwd.clone(), path.clone()));
                }
                control_response(
                    request_id,
                    json!({ "hookSpecificOutput": {
                        "hookEventName": "WorktreeCreate",
                        "worktreePath": path,
                    }}),
                )
            }
            (WorktreeRequest::Remove { .. }, WorktreeAnswer::Removed) => {
                control_response(request_id, json!({}))
            }
            (_, WorktreeAnswer::Refused { error }) => refusal(error),
            (_, mismatched) => {
                log::warn!("claude: {mismatched:?} does not answer {request:?}");
                refusal("the host answered another kind of worktree request")
            }
        };
        Some((response, request))
    }

    /// The session works in `to` from now on, as Claude reported it: a move, unless
    /// that is where it works already.
    fn move_session(&mut self, to: &str) -> Option<AgentEvent> {
        let to = PathBuf::from(to);
        let from = self.cwd.as_ref()?;
        if same_directory(from, &to) {
            return None;
        }
        // Claude reports directories resolved; the one Tcode started it in keeps
        // Tcode's spelling.
        let to = match &self.launch_cwd {
            Some(launch) if same_directory(launch, &to) => launch.clone(),
            _ => to,
        };
        let from = self.cwd.replace(to.clone())?;
        Some(AgentEvent::SessionMoved {
            from,
            to,
            parent_item_id: None,
        })
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
        // A resumed session goes on where it last worked, which Claude restores
        // itself: a worktree it had entered rather than the directory it was
        // started in.
        let moved = msg
            .get("cwd")
            .and_then(Value::as_str)
            .and_then(|cwd| self.move_session(cwd));
        if self.session_id.is_some() {
            let mut events = vec![self.permission_options()];
            events.extend(moved);
            return events;
        }
        let session_id = match msg.get("session_id").and_then(Value::as_str) {
            Some(id) => id.to_string(),
            None => return Vec::new(),
        };
        self.session_id = Some(session_id.clone());
        let model = msg.get("model").and_then(Value::as_str).map(str::to_string);
        let mut events = vec![
            AgentEvent::SessionStarted {
                provider_session_id: session_id.clone(),
                resume: ResumeCursor(json!({ "session_id": session_id })),
                model,
            },
            self.permission_options(),
            // A CLI older than the rewind controls has none for this session.
            AgentEvent::RewindAvailability {
                unavailable: (!self.native_rewind).then_some(RewindFailureReason::Unsupported),
            },
        ];
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
        self.observe_served_model(&json!({ "model": actual }))
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
        let mut parent_id = tool_use_id.to_owned();
        if let Some(task_id) = msg.get("task_id").and_then(Value::as_str) {
            parent_id = self
                .task_tools
                .entry(task_id.to_owned())
                .or_insert(parent_id)
                .clone();
            if msg.get("task_type").and_then(Value::as_str) != Some("local_bash") {
                self.tailed.insert(parent_id.clone());
                self.tail_requests.push(TailRequest::Start {
                    parent_id: parent_id.clone(),
                    task_id: task_id.to_owned(),
                    session_id: msg
                        .get("session_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                });
            }
        }
        // An agent a subagent starts is reported here too, naming that
        // subagent's task. Its call is in the subagent's transcript, which
        // may reach us later or never, so the call is known from this event
        // and is the subagent's from the start.
        let spawner = msg
            .get("parent_task_id")
            .and_then(Value::as_str)
            .and_then(|task_id| self.task_tools.get(task_id))
            .filter(|_| msg.get("task_type").and_then(Value::as_str) == Some("local_agent"))
            .cloned();
        if let Some(spawner) = &spawner {
            match self.tool_items.get_mut(&parent_id) {
                Some(ToolItem::Subagent { parent_item_id, .. }) => {
                    *parent_item_id = Some(spawner.clone());
                }
                Some(_) => {}
                None => {
                    if let Some(tool) =
                        ToolItem::subagent(&spawned_subagent(msg), Some(spawner.clone()))
                    {
                        self.tool_items.insert(parent_id.clone(), tool);
                    }
                }
            }
        }
        let mut events = self.update_subagent(&parent_id, ItemStatus::InProgress, None);
        if let Some((from, to)) = msg
            .get("task_id")
            .and_then(Value::as_str)
            .and_then(|task_id| self.subagent_worktrees.remove(task_id))
        {
            events.push(AgentEvent::SessionMoved {
                from,
                to,
                parent_item_id: Some(parent_id.clone()),
            });
        }
        // The launch acknowledgement that returns the call is only in the
        // subagent's transcript, which both feeds replay; this comes once.
        if spawner.is_some() && msg.get("is_backgrounded").and_then(Value::as_bool) == Some(true) {
            events.push(AgentEvent::ItemBackgrounded {
                item_id: parent_id,
                parent_item_id: spawner,
            });
        }
        events.extend(self.link_background_tasks());
        events
    }

    fn on_background_tasks_changed(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let now = crate::now_unix_millis();
        let current = msg
            .get("tasks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|task| {
                let id = task.get("task_id").and_then(Value::as_str)?;
                let known = self.background_tasks.iter().find(|known| known.id == id);
                Some(crate::BackgroundTask {
                    id: id.to_owned(),
                    name: task
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or(id)
                        .to_owned(),
                    kind: match task.get("task_type").and_then(Value::as_str) {
                        Some("local_bash") => crate::BackgroundKind::Command,
                        Some("local_agent" | "remote_agent") => crate::BackgroundKind::Agent,
                        _ => crate::BackgroundKind::Other,
                    },
                    item_id: self.task_tools.get(id).cloned(),
                    started_at: known.map_or(now, |known| known.started_at),
                    started_reported: false,
                })
            })
            .collect::<Vec<_>>();
        let had_background_tasks = !self.background_tasks.is_empty();
        self.background_tasks = current;
        let list = AgentEvent::BackgroundTaskList {
            tasks: self.background_tasks.clone(),
        };
        self.background_drain_withheld = self.background_tasks.is_empty() && had_background_tasks;
        if self.background_drain_withheld {
            // `background_tasks_changed []` arrives next to the final
            // task_notification (recorded traces have it on either side) and
            // before the provider's self-invoked follow-up turn. Keep the last
            // non-zero liveness until that turn's result so the runtime cannot
            // park/restart the process in this handoff window. A task stopped
            // through `stop_task` starts no such turn; the CLI goes idle
            // instead, which `session_state_changed` reports (2.1.318).
            vec![list]
        } else {
            vec![
                list,
                AgentEvent::BackgroundTasksChanged {
                    count: self.background_tasks.len(),
                },
            ]
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
        let status = match status {
            "completed" => ItemStatus::Completed,
            "failed" | "killed" => ItemStatus::Failed,
            _ => ItemStatus::InProgress,
        };
        // The tail is stopped by the task_notification that follows a
        // terminal patch, so the notification's `output_file` reaches it first.
        self.update_subagent(&tool_use_id, status, None)
    }

    fn on_task_notification(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let task_id = msg
            .get("task_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let tool_use_id = task_id
            .as_ref()
            .and_then(|task_id| self.task_tools.get(task_id).cloned())
            .or_else(|| {
                msg.get("tool_use_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
        let Some(tool_use_id) = tool_use_id else {
            return Vec::new();
        };
        // The task stays in `background_tasks` until the CLI's own
        // `background_tasks_changed` drops it: removing it here lets a drain
        // that follows this notification publish zero before the follow-up
        // turn (see `on_background_tasks_changed`).
        // Claude Code sends "" for a task with no output file.
        if let Some(path) = msg
            .get("output_file")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
        {
            self.tail_requests.push(TailRequest::PreferPath {
                parent_id: tool_use_id.clone(),
                path: PathBuf::from(path),
            });
        }
        self.tail_requests.push(TailRequest::Stop {
            parent_id: tool_use_id.clone(),
        });
        // A notification is the task's terminal frame: `completed`, `failed`
        // or `stopped`.
        let status = if msg.get("status").and_then(Value::as_str) == Some("completed") {
            ItemStatus::Completed
        } else {
            ItemStatus::Failed
        };
        let summary = handback_summary(
            msg.get("handback"),
            msg.get("handback_report"),
            msg.get("summary")
                .and_then(Value::as_str)
                .map(one_line_summary),
        );
        // A Command still held here is a background Bash: foreground ones
        // leave `tool_items` with their tool_result.
        if matches!(
            self.tool_items.get(&tool_use_id),
            Some(ToolItem::Command { .. })
        ) {
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
            status: saved_status,
            summary: saved_summary,
            model,
            effort,
            parent_item_id,
        }) = self.tool_items.get_mut(tool_use_id)
        else {
            return Vec::new();
        };
        if summary.is_some() {
            *saved_summary = summary;
        }
        *saved_status = status;
        let event = AgentEvent::ItemUpdated(ThreadItem {
            id: tool_use_id.to_owned(),
            parent_item_id: parent_item_id.clone(),
            content: ItemContent::Subagent {
                agent_type: agent_type.clone(),
                description: description.clone(),
                status,
                summary: saved_summary.clone(),
                model: model.clone(),
                effort: effort.clone(),
            },
        });
        if status == ItemStatus::InProgress {
            vec![event]
        } else {
            self.gate_subagent_terminal(tool_use_id, event)
        }
    }

    /// Hold a terminal Subagent snapshot while its transcript tail is still
    /// flushing; the tail's Stop acknowledgement releases the latest one.
    fn gate_subagent_terminal(&mut self, tool_use_id: &str, event: AgentEvent) -> Vec<AgentEvent> {
        if self.tailed.contains(tool_use_id) {
            self.pending_subagent_terminals
                .insert(tool_use_id.to_owned(), event);
            Vec::new()
        } else {
            vec![event]
        }
    }

    /// Fold one feed's child items into the canonical stream: drop the stage
    /// the other feed already delivered, and give a nested spawn the same
    /// `tool_items` lifecycle as a top-level one so its task events and tail
    /// settle it.
    fn fold_child_events(&mut self, events: Vec<AgentEvent>) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        for event in events {
            if !self.advance_child_item(&event) {
                continue;
            }
            match &event {
                AgentEvent::ItemStarted(item)
                    if matches!(item.content, ItemContent::Subagent { .. }) =>
                {
                    if let Some(tool) =
                        ToolItem::subagent(&item.content, item.parent_item_id.clone())
                    {
                        self.tool_items.entry(item.id.clone()).or_insert(tool);
                    }
                    out.push(event);
                }
                AgentEvent::ItemCompleted(item)
                    if matches!(item.content, ItemContent::Subagent { .. })
                        && matches!(
                            self.tool_items.get(&item.id),
                            Some(ToolItem::Subagent { .. })
                        ) =>
                {
                    let mut item = item.clone();
                    let id = item.id.clone();
                    // A task_notification outranks the tool_result's text
                    // here as at the top level: this record never says
                    // whether that text is a report.
                    if let (
                        Some(ToolItem::Subagent {
                            summary: Some(notified),
                            ..
                        }),
                        ItemContent::Subagent { summary, .. },
                    ) = (self.tool_items.remove(&id), &mut item.content)
                    {
                        *summary = Some(notified);
                    }
                    out.extend(self.gate_subagent_terminal(&id, AgentEvent::ItemCompleted(item)));
                }
                _ => out.push(event),
            }
        }
        out
    }

    /// Record a child item's stage; false when this stage was already delivered.
    fn advance_child_item(&mut self, event: &AgentEvent) -> bool {
        let (item, stage) = match event {
            AgentEvent::ItemStarted(item) => (item, ChildItemStage::Started),
            AgentEvent::ItemUpdated(item) => (item, ChildItemStage::Updated(item.content.clone())),
            AgentEvent::ItemCompleted(item) => (item, ChildItemStage::Completed),
            _ => return true,
        };
        match (self.child_items.get(&item.id), &stage) {
            (Some(ChildItemStage::Completed), _) | (Some(_), ChildItemStage::Started) => false,
            (Some(ChildItemStage::Updated(previous)), ChildItemStage::Updated(content))
                if previous == content =>
            {
                false
            }
            _ => {
                self.child_items.insert(item.id.clone(), stage);
                true
            }
        }
    }

    /// Fold one batch from a subagent transcript tail: its child items, the
    /// model it observed, and — once it has stopped — the terminal Subagent
    /// snapshot that was waiting on its final flush.
    pub(crate) fn on_tail_notice(&mut self, notice: SubagentTailNotice) -> Vec<AgentEvent> {
        let mut events = match notice.model {
            Some((model, effort)) => self.note_subagent_model(&notice.parent_id, model, effort),
            None => Vec::new(),
        };
        events.extend(self.fold_child_events(notice.events));
        if notice.stopped {
            self.tailed.remove(&notice.parent_id);
            events.extend(self.pending_subagent_terminals.remove(&notice.parent_id));
        }
        events
    }

    /// Terminal snapshots whose tail acknowledgement will never arrive
    /// (session teardown).
    fn take_pending_subagent_terminals(&mut self) -> Vec<AgentEvent> {
        self.tailed.clear();
        std::mem::take(&mut self.pending_subagent_terminals)
            .into_values()
            .collect()
    }

    /// Record the model/effort a child transcript reports for its spawn item
    /// and re-emit the item when either changed.
    pub(crate) fn note_subagent_model(
        &mut self,
        tool_use_id: &str,
        model: Option<String>,
        effort: Option<String>,
    ) -> Vec<AgentEvent> {
        let Some(ToolItem::Subagent {
            status,
            model: saved_model,
            effort: saved_effort,
            ..
        }) = self.tool_items.get_mut(tool_use_id)
        else {
            return Vec::new();
        };
        let model = model.or_else(|| saved_model.clone());
        let effort = effort.or_else(|| saved_effort.clone());
        if model == *saved_model && effort == *saved_effort {
            return Vec::new();
        }
        *saved_model = model;
        *saved_effort = effort;
        let status = *status;
        self.update_subagent(tool_use_id, status, None)
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
                let message = &event["message"];
                let mut events = self.observe_served_model(message);
                events.extend(self.observe_request_usage(
                    message.get("id").and_then(Value::as_str),
                    message.get("usage"),
                    true,
                ));
                events
            }
            // Claude 5 models stream a thinking block whose deltas carry no
            // text, so the block's start is the only sign the model is
            // thinking: a placeholder reasoning item renders as "Thinking…"
            // until text or the next block arrives.
            Some("content_block_start")
                if event.pointer("/content_block/type").and_then(Value::as_str)
                    == Some("thinking") =>
            {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                vec![AgentEvent::ItemStarted(ThreadItem {
                    id: self.block_item_id(index),
                    parent_item_id: None,
                    content: ItemContent::Reasoning {
                        text: String::new(),
                    },
                })]
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
                if let Some(usage) = event.get("usage") {
                    let id = self.current_message_id.clone();
                    events.extend(self.observe_request_usage(id.as_deref(), Some(usage), false));
                }
                events
            }
            _ => Vec::new(),
        }
    }

    /// Stream deltas are cumulative fields within one request, never increments.
    /// Assistant messages can repeat an ID for parallel tool blocks.
    fn observe_request_usage(
        &mut self,
        id: Option<&str>,
        usage: Option<&Value>,
        start: bool,
    ) -> Vec<AgentEvent> {
        let Some(id) = id else {
            return Vec::new();
        };
        if self.usage_message_id.as_deref() != Some(id) {
            if !self.usage_message_ids.insert(id.to_owned()) {
                return Vec::new();
            }
            self.usage_message_id = Some(id.to_owned());
            self.request_usage = json!({});
        }
        if let Some(fields) = usage.and_then(Value::as_object) {
            for key in [
                "input_tokens",
                "cache_read_input_tokens",
                "cache_creation_input_tokens",
                "output_tokens",
            ] {
                if let Some(value) = fields.get(key).filter(|v| v.is_u64()) {
                    // Non-streaming assistant output is a placeholder; never regress
                    // the cumulative output already observed in the stream.
                    if key != "output_tokens" || value.as_u64() >= self.request_usage[key].as_u64()
                    {
                        self.request_usage[key] = value.clone();
                    }
                }
            }
        }
        self.latest_usage.turn_processed_tokens = None;
        let mapped = map_usage(&self.request_usage, None);
        self.latest_usage.input_tokens = mapped.input_tokens;
        self.latest_usage.cached_input_tokens = mapped.cached_input_tokens;
        self.latest_usage.output_tokens = mapped.output_tokens;
        // Claude's context observation counts the latest request's input and caches.
        // Output deltas describe generated traffic, not a new input observation.
        self.latest_usage.used_tokens = mapped.input_tokens.map(|input| {
            input
                .saturating_add(mapped.cached_input_tokens.unwrap_or(0))
                .saturating_add(
                    self.request_usage["cache_creation_input_tokens"]
                        .as_u64()
                        .unwrap_or(0),
                )
        });
        self.latest_usage.freshness = if self.latest_usage.used_tokens.is_some() {
            crate::ContextFreshness::Current
        } else {
            crate::ContextFreshness::AwaitingObservation
        };
        if usage.is_some() || start {
            vec![AgentEvent::TokenUsage(self.latest_usage)]
        } else {
            Vec::new()
        }
    }

    fn block_item_id(&self, index: u64) -> String {
        match &self.current_message_id {
            Some(id) => format!("{id}:{index}"),
            None => format!("msg:{index}"),
        }
    }

    fn observe_served_model(&mut self, message: &Value) -> Vec<AgentEvent> {
        let Some(model) = message
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty() && *model != "<synthetic>")
        else {
            return Vec::new();
        };
        if self.last_served_model.as_deref() == Some(model) {
            return Vec::new();
        }
        self.last_served_model = Some(model.to_owned());
        vec![AgentEvent::ServedModel {
            model: model.to_owned(),
            reason: None,
        }]
    }

    fn on_assistant(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let message = match msg.get("message") {
            Some(m) => m,
            None => return Vec::new(),
        };
        let mut out = self.observe_request_usage(
            message.get("id").and_then(Value::as_str),
            message.get("usage"),
            false,
        );
        let msg_id = message
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("msg")
            .to_string();
        let state = self.assistant_messages.entry(msg_id.clone()).or_default();
        let model = message.get("model").and_then(Value::as_str);
        let model_changed = state.model.as_deref() != model;
        if model_changed {
            state.model = model.map(str::to_owned);
        }
        let content = message.get("content").and_then(Value::as_array);
        let first_index = state.blocks_seen;
        state.blocks_seen += content.map_or(0, Vec::len);
        // Split assistant blocks retain the message_start model even after a
        // fallback boundary; observing an unchanged header would undo the switch.
        if model_changed {
            out.extend(self.observe_served_model(message));
        }
        out.extend(self.observe_stop_reason(message.get("stop_reason").and_then(Value::as_str)));
        let Some(content) = content else { return out };
        // Thinking blocks the server tagged as narration carry its summary of
        // the prose the model wrote to the user between tool calls; Claude Code
        // shows them as messages, not as reasoning.
        let narration: Vec<u64> = msg
            .get("narration_block_indexes")
            .and_then(Value::as_array)
            .map(|indexes| indexes.iter().filter_map(Value::as_u64).collect())
            .unwrap_or_default();

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
                            summarized: false,
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
                    let content = if text.trim().is_empty() {
                        None
                    } else if narration.contains(&(offset as u64)) {
                        Some(ItemContent::AssistantMessage {
                            text: text.to_string(),
                            summarized: true,
                        })
                    } else {
                        Some(ItemContent::Reasoning {
                            text: text.to_string(),
                        })
                    };
                    if let Some(content) = content {
                        out.push(AgentEvent::ItemCompleted(ThreadItem {
                            id: format!("{msg_id}:{index}"),
                            parent_item_id: None,
                            content,
                        }));
                    }
                }
                Some("tool_use") => {
                    out.extend(self.on_tool_use(block));
                }
                Some("fallback") => out.extend(self.observe_served_model(&block["to"])),
                _ => {}
            }
        }
        out
    }

    fn observe_stop_reason(&mut self, reason: Option<&str>) -> Vec<AgentEvent> {
        let Some(reason) = reason.filter(|reason| !reason.is_empty()) else {
            return Vec::new();
        };
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

        let (item, content) = if is_agent_tool(&name.to_lowercase()) {
            let content = spawned_subagent(&input);
            let item = ToolItem::subagent(&content, None).expect("a spawn snapshot is a Subagent");
            (item, content)
        } else {
            ToolItem::started(name, input)
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
        if let Some(uuid) = msg.get("uuid").and_then(Value::as_str)
            && let Some(request_id) = self.steer_uuids.remove(uuid)
        {
            out.push(AgentEvent::SteerCheckpoint {
                request_id,
                checkpoint_id: uuid.to_owned(),
            });
        } else if self.awaiting_turn_checkpoint
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
            let launched_in_background = msg.pointer("/tool_use_result/backgroundTaskId").is_some();
            let item = match self.tool_items.remove(&tool_use_id) {
                Some(i) => i,
                None => continue,
            };
            // A background Agent's tool_result only acknowledges the launch;
            // the subagent keeps running until its task_notification.
            if matches!(item, ToolItem::Subagent { .. })
                && msg
                    .pointer("/tool_use_result/status")
                    .and_then(Value::as_str)
                    == Some("async_launched")
            {
                if let Some(agent_id) = msg
                    .pointer("/tool_use_result/agentId")
                    .and_then(Value::as_str)
                {
                    self.task_tools
                        .insert(agent_id.to_owned(), tool_use_id.clone());
                    out.extend(self.link_background_tasks());
                }
                let ToolItem::Subagent { parent_item_id, .. } = &item else {
                    unreachable!("matched as a subagent");
                };
                let parent_item_id = parent_item_id.clone();
                self.tool_items.insert(tool_use_id.clone(), item);
                out.extend(self.update_subagent(&tool_use_id, ItemStatus::InProgress, None));
                out.push(AgentEvent::ItemBackgrounded {
                    item_id: tool_use_id.clone(),
                    parent_item_id,
                });
                continue;
            }
            let is_error = block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            // Where the session works after it entered or left a worktree.
            let moved_to = match &item {
                ToolItem::Tool { name, .. } if !is_error => match name.as_str() {
                    "EnterWorktree" => msg.pointer("/tool_use_result/worktreePath"),
                    "ExitWorktree" => msg.pointer("/tool_use_result/restoredCwd"),
                    _ => None,
                },
                _ => None,
            }
            .and_then(Value::as_str)
            .map(str::to_owned);
            let output =
                ToolOutput::from_content(block.get("content").unwrap_or(&Value::Null)).text;
            let status = if is_error {
                ItemStatus::Failed
            } else {
                ItemStatus::Completed
            };
            let mut parent_item_id = None;
            let backgrounded = launched_in_background && matches!(item, ToolItem::Command { .. });
            if let Some(task_id) = msg
                .pointer("/tool_use_result/backgroundTaskId")
                .and_then(Value::as_str)
                .filter(|_| backgrounded)
            {
                self.task_tools
                    .entry(task_id.to_owned())
                    .or_insert_with(|| tool_use_id.clone());
                out.extend(self.link_background_tasks());
            }
            let content = match item {
                ToolItem::Command { command, .. } if backgrounded => {
                    self.tool_items.insert(
                        tool_use_id.clone(),
                        ToolItem::Command {
                            command: command.clone(),
                            output: output.clone(),
                        },
                    );
                    ItemContent::CommandExecution {
                        command,
                        output,
                        exit_code: None,
                        status: ItemStatus::InProgress,
                    }
                }
                item @ (ToolItem::Command { .. }
                | ToolItem::File { .. }
                | ToolItem::Tool { .. }
                | ToolItem::Plan { .. }) => item.finished(block, msg.get("tool_use_result")),
                ToolItem::Subagent {
                    agent_type,
                    description,
                    summary,
                    model,
                    effort,
                    parent_item_id: parent,
                    ..
                } => {
                    parent_item_id = parent;
                    let result = msg.get("tool_use_result");
                    ItemContent::Subagent {
                        agent_type,
                        description,
                        status,
                        summary: handback_summary(
                            result.and_then(|result| result.get("handback")),
                            result.and_then(|result| result.get("handbackReport")),
                            summary.or_else(|| {
                                (!output.trim().is_empty()).then(|| one_line_summary(&output))
                            }),
                        ),
                        model,
                        effort,
                    }
                }
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
            let is_subagent = matches!(content, ItemContent::Subagent { .. });
            let event = event(ThreadItem {
                id: tool_use_id.clone(),
                parent_item_id,
                content,
            });
            if is_subagent {
                out.extend(self.gate_subagent_terminal(&tool_use_id, event));
            } else {
                out.push(event);
            }
            if backgrounded {
                out.push(AgentEvent::ItemBackgrounded {
                    item_id: tool_use_id.clone(),
                    parent_item_id: None,
                });
            }
            out.extend(moved_to.and_then(|to| self.move_session(&to)));
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
        if let Some(reply) = self.pending_usage_reads.remove(request_id) {
            let _ = reply.try_send(usage_response(response).map_err(|error| error.to_string()));
            return Vec::new();
        }
        if self.pending_initialize.as_deref() == Some(request_id) {
            self.pending_initialize = None;
            if response.get("subtype").and_then(Value::as_str) != Some("success") {
                log::warn!("claude: the worktree hooks were not registered: {response}");
            }
            return Vec::new();
        }
        if self.pending_background_moves.remove(request_id) {
            let moved = response.get("subtype").and_then(Value::as_str) == Some("success")
                && response
                    .pointer("/response/backgrounded")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
            if moved {
                return Vec::new();
            }
            let error = response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("the call is no longer running in the foreground");
            return vec![AgentEvent::Warning {
                message: format!("claude: could not move the call to the background: {error}"),
            }];
        }
        if self
            .pending_settings
            .as_ref()
            .is_some_and(|pending| pending.request_id == request_id)
        {
            return self.on_settings_response(response);
        }
        if let Some(mode) = self.pending_permission_modes.remove(request_id) {
            if response.get("subtype").and_then(Value::as_str) == Some("success") {
                self.applied_permission_mode = response
                    .pointer("/response/permissionMode")
                    .or_else(|| response.pointer("/response/mode"))
                    .and_then(Value::as_str)
                    .unwrap_or(&mode)
                    .to_owned();
                if self.applied_permission_mode == "auto" {
                    self.auto_unavailable = None;
                }
                return vec![self.permission_options()];
            }
            let error = response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("Claude Code rejected the permission-mode request");
            let warning = AgentEvent::Warning {
                message: format!("claude: failed to switch permission mode: {error}"),
            };
            if mode == "auto"
                && response
                    .get("error_code")
                    .and_then(Value::as_str)
                    .is_some_and(|code| code.starts_with("auto_mode_"))
            {
                self.auto_unavailable = Some(error.to_owned());
                return vec![warning, self.permission_options()];
            }
            return vec![warning];
        }
        let Some(pending) = self.pending_rewinds.remove(request_id) else {
            return Vec::new();
        };
        self.on_rewind_response(pending, response)
    }

    /// One step of a rewind answered: send the next, or report the outcome.
    fn on_rewind_response(
        &mut self,
        mut pending: PendingRewind,
        response: &Value,
    ) -> Vec<AgentEvent> {
        let succeeded = response.get("subtype").and_then(Value::as_str) == Some("success");
        let result = response.get("response").unwrap_or(&Value::Null);
        let error = || {
            response
                .get("error")
                .or_else(|| result.get("error"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        match pending.step {
            RewindStep::Interrupt => {
                pending.step = RewindStep::Files;
                let request = self.rewind_request(pending);
                self.outgoing.push(request);
                Vec::new()
            }
            RewindStep::Preview => {
                let result = if succeeded
                    && result.get("canRewind").and_then(Value::as_bool) == Some(true)
                {
                    let files = rewind_files(result);
                    self.rewind_previews
                        .insert(pending.checkpoint_id.clone(), files.clone());
                    RewindPreviewResult::Ready(RewindPreview {
                        files,
                        conflicts: Vec::new(),
                    })
                } else {
                    RewindPreviewResult::Refused(files_failure(error()))
                };
                vec![AgentEvent::RewindPreviewed {
                    checkpoint_id: pending.checkpoint_id,
                    mode: pending.mode,
                    result,
                }]
            }
            RewindStep::Files => {
                if succeeded && result.get("canRewind").and_then(Value::as_bool) != Some(false) {
                    let mut files = self
                        .rewind_previews
                        .remove(&pending.checkpoint_id)
                        .unwrap_or_default();
                    files.skipped_links = result
                        .get("skippedLinks")
                        .and_then(Value::as_u64)
                        .unwrap_or_default();
                    pending.files = Some(files);
                } else {
                    pending.files_failure = Some(files_failure(error()));
                }
                if pending.mode.includes_conversation() {
                    pending.step = RewindStep::Conversation;
                    let request = self.rewind_request(pending);
                    self.outgoing.push(request);
                    return Vec::new();
                }
                vec![match pending.files_failure {
                    Some(failure) => AgentEvent::RewindFailed {
                        checkpoint_id: pending.checkpoint_id,
                        mode: pending.mode,
                        failure,
                    },
                    None => AgentEvent::RewindCompleted {
                        checkpoint_id: pending.checkpoint_id,
                        mode: pending.mode,
                        prefill: None,
                        prefill_attachments: Vec::new(),
                        later_history: LaterHistory::Truncated,
                        files: pending.files,
                        incomplete: None,
                        undo_token: None,
                    },
                }]
            }
            RewindStep::Conversation => {
                let rewound =
                    succeeded && result.get("rewound").and_then(Value::as_bool) == Some(true);
                if rewound {
                    let restored = if pending.files_failure.is_some() {
                        RewindMode::Conversation
                    } else {
                        pending.mode
                    };
                    return vec![AgentEvent::RewindCompleted {
                        checkpoint_id: pending.checkpoint_id,
                        mode: restored,
                        prefill: result
                            .get("prefillText")
                            .and_then(Value::as_str)
                            .filter(|text| !text.is_empty())
                            .map(str::to_owned),
                        prefill_attachments: Vec::new(),
                        later_history: LaterHistory::Truncated,
                        files: pending.files,
                        incomplete: pending.files_failure.map(|failure| RewindIncomplete {
                            requested: pending.mode,
                            failure,
                        }),
                        undo_token: None,
                    }];
                }
                let failure = RewindFailure::new(
                    if succeeded {
                        conversation_refusal(result.get("reason").and_then(Value::as_str))
                    } else {
                        RewindFailureReason::Other
                    },
                    error(),
                );
                if pending.mode.includes_files() && pending.files_failure.is_none() {
                    // The files were restored before the conversation refused.
                    vec![AgentEvent::RewindCompleted {
                        checkpoint_id: pending.checkpoint_id,
                        mode: RewindMode::Files,
                        prefill: None,
                        prefill_attachments: Vec::new(),
                        later_history: LaterHistory::Truncated,
                        files: pending.files,
                        incomplete: Some(RewindIncomplete {
                            requested: pending.mode,
                            failure,
                        }),
                        undo_token: None,
                    }]
                } else {
                    vec![AgentEvent::RewindFailed {
                        checkpoint_id: pending.checkpoint_id,
                        mode: pending.mode,
                        failure,
                    }]
                }
            }
        }
    }

    /// Claude Code withdrew one of its own prompts, as it does for a pending
    /// `can_use_tool` when the turn is interrupted.
    fn on_control_cancel_request(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let Some(request_id) = msg.get("request_id").and_then(Value::as_str) else {
            return Vec::new();
        };
        if self.pending_approvals.remove(request_id).is_some() {
            vec![AgentEvent::ApprovalResolved {
                request_id: request_id.to_owned(),
                decision: ApprovalDecision::Cancel,
            }]
        } else if self.pending_user_input.remove(request_id).is_some() {
            vec![AgentEvent::UserInputResolved {
                request_id: request_id.to_owned(),
                answers: serde_json::Map::new(),
            }]
        } else {
            Vec::new()
        }
    }

    fn on_control_request(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let request = match msg.get("request") {
            Some(r) => r,
            None => return Vec::new(),
        };
        if request.get("subtype").and_then(Value::as_str) == Some("hook_callback")
            && let Some(request_id) = msg.get("request_id").and_then(Value::as_str)
        {
            return self.on_hook_callback(request_id.to_owned(), request);
        }
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
        // Claude Code's consent line; it may carry terminal escapes.
        let text = |key: &str| {
            request
                .get(key)
                .and_then(Value::as_str)
                .map(crate::strip_ansi)
                .filter(|text| !text.trim().is_empty())
        };
        let reason = text("decision_reason");
        let blocked_path = text("blocked_path");

        if tool_name == "AskUserQuestion" {
            let questions_raw = input.get("questions").cloned().unwrap_or_else(|| json!([]));
            let questions = parse_ask_user_questions(&input);
            self.pending_user_input
                .insert(request_id.clone(), questions_raw);
            return vec![AgentEvent::UserInputRequested {
                request_id,
                questions,
                delivery: UserInputDelivery::Blocking,
            }];
        }

        if tool_name == EXIT_PLAN_MODE {
            return self.plan_approval(request_id, request, input);
        }

        let request_type = classify_claude_tool(&tool_name);

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
                description: input
                    .get("description")
                    .and_then(Value::as_str)
                    .filter(|description| !description.trim().is_empty())
                    .map(str::to_string),
            },
            ClaudeRequestType::FileChange => ApprovalKind::FileChange {
                changes: file_changes(&tool_name, &input),
            },
            ClaudeRequestType::ToolUse => ApprovalKind::ToolUse {
                name: tool_name.clone(),
                input: input.clone(),
                detail,
            },
        };

        let mut options = vec![
            ApprovalOption {
                id: "allow".into(),
                label: "Allow".into(),
                kind: ApprovalOptionKind::AllowOnce,
            },
            ApprovalOption {
                id: "deny".into(),
                label: "Deny".into(),
                kind: ApprovalOptionKind::RejectOnce,
            },
        ];
        let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
        for suggestion in request
            .get("permission_suggestions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let scope = suggestion
                .get("destination")
                .and_then(Value::as_str)
                .unwrap_or("session");
            if let Some((_, suggestions)) = groups.iter_mut().find(|(known, _)| known == scope) {
                suggestions.push(suggestion.clone());
            } else {
                groups.push((scope.to_owned(), vec![suggestion.clone()]));
            }
        }
        let mut suggestions = HashMap::new();
        for (index, (scope, group)) in groups.into_iter().enumerate() {
            let id = format!("allow_suggestion:{index}");
            let label = match scope.as_str() {
                "session" => "Allow and remember for this session".to_owned(),
                "userSettings" => "Allow and save to user settings".to_owned(),
                "projectSettings" => "Allow and save to project settings".to_owned(),
                "localSettings" => "Allow and save to local settings".to_owned(),
                _ => format!("Allow and remember in {scope}"),
            };
            options.push(ApprovalOption {
                id: id.clone(),
                label,
                kind: ApprovalOptionKind::AllowAlways,
            });
            suggestions.insert(id, json!(group));
        }
        self.pending_approvals
            .insert(request_id.clone(), PendingApproval { input, suggestions });
        let parent_item_id = request
            .get("agent_id")
            .and_then(Value::as_str)
            .and_then(|agent_id| self.task_tools.get(agent_id))
            .cloned();
        vec![AgentEvent::ApprovalRequested(ApprovalRequest {
            id: request_id,
            turn_id: self.current_turn_id.clone(),
            parent_item_id,
            kind,
            reason,
            blocked_path,
            options,
        })]
    }

    /// `ExitPlanMode`'s permission prompt: the plan item it decides on (from
    /// the tool's own item, emitted here when the prompt arrives first), then
    /// the decision with Claude Code's allow and deny under plan names.
    fn plan_approval(
        &mut self,
        request_id: String,
        request: &Value,
        input: Value,
    ) -> Vec<AgentEvent> {
        let markdown = plan_markdown(&input);
        let item_id = request
            .get("tool_use_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("plan-{request_id}"));
        let mut events = Vec::new();
        let known = match self.tool_items.get(&item_id) {
            Some(ToolItem::Plan { markdown: shown }) => Some(*shown == markdown),
            _ => None,
        };
        if known != Some(true) {
            self.tool_items.insert(
                item_id.clone(),
                ToolItem::Plan {
                    markdown: markdown.clone(),
                },
            );
            let item = ThreadItem {
                id: item_id.clone(),
                parent_item_id: None,
                content: ItemContent::Plan {
                    markdown: markdown.clone(),
                },
            };
            events.push(if known.is_some() {
                AgentEvent::ItemUpdated(item)
            } else {
                AgentEvent::ItemStarted(item)
            });
        }
        self.pending_approvals.insert(
            request_id.clone(),
            PendingApproval {
                input,
                suggestions: HashMap::new(),
            },
        );
        let parent_item_id = request
            .get("agent_id")
            .and_then(Value::as_str)
            .and_then(|agent_id| self.task_tools.get(agent_id))
            .cloned();
        events.push(AgentEvent::ApprovalRequested(ApprovalRequest {
            id: request_id,
            turn_id: self.current_turn_id.clone(),
            parent_item_id,
            kind: ApprovalKind::Plan { item_id, markdown },
            reason: None,
            blocked_path: None,
            options: vec![
                ApprovalOption {
                    id: "allow".into(),
                    label: "Approve".into(),
                    kind: ApprovalOptionKind::AllowOnce,
                },
                ApprovalOption {
                    id: "deny".into(),
                    label: "Keep planning".into(),
                    kind: ApprovalOptionKind::RejectOnce,
                },
            ],
        }));
        events
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
        let turn_id = self
            .current_turn_id
            .take()
            .unwrap_or_else(|| format!("turn-{}", self.turn_counter.max(1)));
        // No message outlives its turn, so the block counters can go.
        self.assistant_messages.clear();
        let mut status = result_status(msg);
        if std::mem::take(&mut self.interrupt_pending) && status != TurnStatus::Completed {
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
/// `max_retries` times, so each attempt is a non-fatal error rather than an end.
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
    AgentEvent::Error {
        message: format!("claude API request failed ({cause}); retry {attempt}/{max} in {delay}"),
        fatal: false,
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
        // the tool result puts before each line.
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
                // An empty read's result is only Claude Code's warning that
                // the file is empty or shorter than the offset.
                return (
                    Some(ToolFamily::Read {
                        path: path.to_owned(),
                        first_line: file.get("startLine").and_then(Value::as_u64),
                        note: output.filter(|text| content.is_empty() && !text.trim().is_empty()),
                    }),
                    Some(content.to_owned()),
                );
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
        _ => return (None, output),
    };
    (Some(family), output)
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

/// A Read tool result's numbered lines (`N\t` before each, 2.1.322's
/// format) as the first line's number and the lines without their numbers.
fn numbered_read(text: &str) -> Option<(u64, String)> {
    let mut first = None;
    let mut lines = Vec::new();
    for line in text.lines() {
        let (number, rest) = line.split_once('\t')?;
        let number: u64 = number.trim_start().parse().ok()?;
        first.get_or_insert(number);
        lines.push(rest);
    }
    Some((first?, lines.join("\n")))
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

pub(crate) fn one_line_summary(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
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

/// Approval categories; tools without a dedicated category use `ToolUse`.
enum ClaudeRequestType {
    FileRead,
    ExecCommand,
    FileChange,
    ToolUse,
}

/// The tools that spawn a subagent: `Agent` and its former name `Task`.
/// Coordination tools such as `ListAgents` and `SendMessage` do not.
pub(crate) fn is_agent_tool(normalized: &str) -> bool {
    matches!(normalized, "agent" | "task")
}

/// The in-progress Subagent snapshot for an Agent tool call. The call only
/// carries a model alias when the caller picked one; the resolved model and
/// effort arrive with the child's first assistant message.
pub(crate) fn spawned_subagent(input: &Value) -> ItemContent {
    ItemContent::Subagent {
        agent_type: input
            .get("subagent_type")
            .and_then(Value::as_str)
            .unwrap_or("subagent")
            .to_owned(),
        description: subagent_description(input),
        status: ItemStatus::InProgress,
        summary: None,
        model: input
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty())
            .map(str::to_owned),
        effort: None,
    }
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

/// The tool Claude Code calls to present its plan and leave plan mode.
const EXIT_PLAN_MODE: &str = "ExitPlanMode";

/// The plan `ExitPlanMode` presents, which Claude Code fills in from the plan
/// file it wrote.
fn plan_markdown(input: &Value) -> String {
    input
        .get("plan")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// The Claude Code tools (2.1.322) whose approval has a category of its own;
/// every other tool, an MCP tool included, asks as itself.
fn classify_claude_tool(name: &str) -> ClaudeRequestType {
    match name {
        "Read" | "Glob" | "Grep" | "WebSearch" => ClaudeRequestType::FileRead,
        "Bash" | "PowerShell" => ClaudeRequestType::ExecCommand,
        "Write" | "Edit" | "NotebookEdit" => ClaudeRequestType::FileChange,
        _ => ClaudeRequestType::ToolUse,
    }
}

/// Summarize a command, subagent brief, or tool input for its approval prompt.
fn approval_detail(tool_name: &str, input: &Value) -> String {
    if let Some(cmd) = input
        .get("command")
        .or_else(|| input.get("cmd"))
        .and_then(Value::as_str)
    {
        let clipped: String = cmd.trim().chars().take(400).collect();
        return format!("{tool_name}: {clipped}");
    }
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
    let json = serde_json::to_string(input).unwrap_or_default();
    if json.chars().count() <= 400 {
        format!("{tool_name}: {json}")
    } else {
        let clipped: String = json.chars().take(397).collect();
        format!("{tool_name}: {clipped}...")
    }
}

/// Parse `AskUserQuestion` tool input into canonical [`UserInputQuestion`]s
/// with the complete question text as `id` (falling back to `q-<index>`);
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

fn result_status(msg: &Value) -> TurnStatus {
    if msg.get("subtype").and_then(Value::as_str) == Some("success")
        && msg.get("is_error").and_then(Value::as_bool) != Some(true)
    {
        TurnStatus::Completed
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
        input_tokens: input,
        cached_input_tokens: cache_read,
        output_tokens: output,
        used_tokens,
        // Session-cumulative total is stamped by the caller (`on_result`); the
        // streaming/partial usage path leaves it unset.
        ..TokenUsage::default()
    }
}

/// Parse Claude system-init `slash_commands` (→ [`ProviderCommandKind::Command`])
/// and `skills` (→ [`ProviderCommandKind::Skill`]) into [`ProviderCommand`]s.
/// Both are arrays of bare name strings; the CLI supplies no descriptions. A
/// skill is listed under both, which is how the runtime knows a `$name`
/// mention of it is sent as the `/name` command the CLI expands.
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

fn is_todo_tool(name: &str) -> bool {
    name.to_lowercase().contains("todowrite")
}

/// Map `TodoWrite` input `{ todos: [{ content, status, activeForm? }] }` to
/// plan steps (content → step, fallback `"Task"`; completed/in_progress →
/// Completed/InProgress, else Pending). `activeForm` is ignored.
fn extract_plan_steps_from_todo(input: &Value) -> Option<Vec<PlanStep>> {
    let todos = input.get("todos").and_then(Value::as_array)?;
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

fn has_boolean_option(spec: &ModelSpec, id: &str) -> bool {
    spec.options
        .iter()
        .any(|o| matches!(o, OptionDescriptor::Boolean { id: oid, .. } if oid == id))
}

/// Resolve the effort selection against the model's `reasoningEffort`
/// descriptor: an accepted listed value wins, else the descriptor default.
/// Returns `None` when the model has no reasoning selector (e.g. Haiku).
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

/// Run `claude --version` and parse the semver triple; `None` on any failure.
async fn claude_version(binary: Option<&Path>, launch_env: &LaunchEnv) -> Option<(u32, u32, u32)> {
    // Resolve through the PATH search (PATHEXT-aware: on Windows the CLI only
    // exists as `claude.cmd`), falling back to the bare name so the failure is
    // reported by the OS exactly as before.
    let bin = crate::resolve_binary(binary, "claude")
        .unwrap_or_else(|_| std::path::PathBuf::from("claude"));
    crate::process::probe_version(&bin, launch_env, ProviderKind::ClaudeCode).await
}

/// List Claude's models: the manifest catalog (refreshed per `refresh`),
/// gated by the installed CLI version.
pub async fn list_models(
    binary_path: Option<PathBuf>,
    launch_env: LaunchEnv,
    refresh: CatalogRefresh,
) -> Result<Vec<ModelSpec>, AgentError> {
    crate::process::unblock(move || {
        crate::claude_manifest::refresh(refresh.cache_dir.as_deref(), refresh.network)
    })
    .await;
    let version = claude_version(binary_path.as_deref(), &launch_env).await;
    Ok(crate::claude_manifest::current().models_for_version(version))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `CLAUDE_CONFIG_DIR` the user set in the profile's environment is
    /// where the CLI keeps its transcripts, whatever `HOME` says.
    #[test]
    fn config_dir_follows_a_configured_claude_config_dir_over_home() {
        let launch_env = LaunchEnv {
            env: [("CLAUDE_CONFIG_DIR", "/configured"), ("HOME", "/home")]
                .into_iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
            home: None,
        };
        assert_eq!(config_dir(&launch_env), Some(PathBuf::from("/configured")));
    }

    /// claude 2.1.318 reads its account file as `join(CLAUDE_CONFIG_DIR ??
    /// home, ".claude.json")`: inside a configured directory, but beside
    /// `~/.claude`, not in it, for the default account.
    #[test]
    fn account_file_sits_in_a_configured_dir_and_beside_dot_claude_otherwise() {
        assert_eq!(
            account_file_in(Some("/configured".into()), Some("/home".into())),
            Some(PathBuf::from("/configured/.claude.json"))
        );
        assert_eq!(
            account_file_in(None, Some("/home".into())),
            Some(PathBuf::from("/home/.claude.json"))
        );
    }

    #[test]
    fn native_permission_switches_publish_only_confirmed_values() {
        let mut mapper = Mapper::new();
        for mode in [
            "default",
            "acceptEdits",
            "auto",
            "dontAsk",
            "bypassPermissions",
        ] {
            let previous = mapper.applied_permission_mode.clone();
            let request = mapper.set_permission_mode_request_str(mode);
            assert_eq!(
                request["request"],
                json!({"subtype":"set_permission_mode", "mode":mode})
            );
            assert_eq!(mapper.applied_permission_mode, previous);
            let events = mapper.on_message(json!({"type":"control_response", "response": {
                "subtype":"success", "request_id":request["request_id"], "response":{}
            }}));
            assert!(
                matches!(events.as_slice(), [AgentEvent::ProviderOptions { selections, .. }]
                if selections.iter().any(|selection| selection.id == "permissionMode" && selection.value == mode))
            );
        }
        let request = mapper.set_permission_mode_request_str("auto");
        let events = mapper.on_message(json!({"type":"control_response", "response": {
            "subtype":"error", "request_id":request["request_id"], "error":"unsupported mode"
        }}));
        assert!(
            matches!(events.as_slice(), [AgentEvent::Warning { message }] if message.contains("unsupported mode"))
        );
        assert_eq!(mapper.applied_permission_mode, "bypassPermissions");

        let request = mapper.set_permission_mode_request_str("auto");
        let events = mapper.on_message(json!({"type":"control_response", "response": {
            "subtype":"success", "request_id":request["request_id"], "response":{"permissionMode":"dontAsk"}
        }}));
        assert!(
            matches!(events.as_slice(), [AgentEvent::ProviderOptions { selections, .. }] if selections[0].value == "dontAsk")
        );

        let request = mapper.set_permission_mode_request_str("auto");
        let reason = "Cannot set permission mode to auto: auto mode unavailable for this model";
        let events = mapper.on_message(json!({"type":"control_response", "response": {
            "subtype":"error", "request_id":request["request_id"],
            "error":reason, "error_code":"auto_mode_model"
        }}));
        assert!(
            events.iter().any(|event| matches!(event,
                AgentEvent::ProviderOptions { descriptors, selections }
                if selections[0].value == "dontAsk" && matches!(&descriptors[0],
                    OptionDescriptor::Select { options, .. }
                    if options.iter().any(|option| option.value == "auto"
                        && option.unavailable.as_deref() == Some(reason))))),
            "the provider's explicit auto unavailability must reach the picker: {events:?}"
        );

        let mut auto = Mapper::new_configured("auto".into(), false);
        let events = auto.on_message(json!({"type":"system", "subtype":"init", "session_id":"native", "permissionMode":"default"}));
        let descriptor = events
            .iter()
            .find_map(|event| match event {
                AgentEvent::ProviderOptions { descriptors, .. } => descriptors.first(),
                _ => None,
            })
            .unwrap();
        assert!(
            matches!(descriptor, OptionDescriptor::Select { options, .. }
            if options.iter().any(|option| option.value == "auto" && option.unavailable.as_deref() == Some("Not available: the session started in another mode")))
        );
        let events = auto.on_message(json!({"type":"system", "subtype":"init", "session_id":"native", "permissionMode":"plan"}));
        assert!(
            matches!(events.as_slice(), [AgentEvent::ProviderOptions { descriptors, selections }]
            if selections[0].value == "plan" && matches!(&descriptors[0], OptionDescriptor::Select { options, .. }
                if options.iter().any(|option| option.value == "plan" && option.unavailable.as_deref() == Some("Entered by the agent"))))
        );

        let mut supported = Mapper::new_configured("auto".into(), false);
        supported.on_message(json!({"type":"system", "subtype":"init", "session_id":"supported", "permissionMode":"auto"}));
        let events = supported.on_message(json!({"type":"system", "subtype":"init", "session_id":"supported", "permissionMode":"plan"}));
        assert!(
            matches!(events.as_slice(), [AgentEvent::ProviderOptions { descriptors, selections }]
            if selections[0].value == "plan" && matches!(&descriptors[0], OptionDescriptor::Select { options, .. }
                if options.iter().any(|option| option.value == "plan" && option.unavailable.as_deref() == Some("Entered by the agent"))
                && options.iter().any(|option| option.value == "auto" && option.unavailable.is_none()))),
            "native plan entry must preserve confirmed auto availability: {events:?}"
        );

        for confirmation in ["control_response", "init"] {
            let mut recovered = Mapper::new_configured("auto".into(), false);
            recovered.on_message(json!({"type":"system", "subtype":"init", "session_id":"recovered", "permissionMode":"default"}));
            let message = if confirmation == "init" {
                json!({"type":"system", "subtype":"init", "session_id":"recovered", "permissionMode":"auto"})
            } else {
                let request = recovered.set_permission_mode_request_str("auto");
                json!({"type":"control_response", "response": {
                    "subtype":"success", "request_id":request["request_id"], "response":{"permissionMode":"auto"}
                }})
            };
            let events = recovered.on_message(message);
            assert!(
                matches!(events.as_slice(), [AgentEvent::ProviderOptions { descriptors, selections }]
                if selections[0].value == "auto" && matches!(&descriptors[0], OptionDescriptor::Select { options, .. }
                    if options.iter().any(|option| option.value == "auto" && option.unavailable.is_none()))),
                "{confirmation} confirmation must restore auto availability: {events:?}"
            );
        }
    }

    #[test]
    fn native_permission_requests_apply_only_the_chosen_suggestion_group() {
        let input = json!({"file_path":"file.rs", "content":"changed"});
        let session =
            json!({"type":"addRules", "rules":[{"toolName":"Write"}], "destination":"session"});
        let user = json!({"type":"setMode", "mode":"acceptEdits", "destination":"userSettings"});
        for name in [
            "Write",
            "EnterPlanMode",
            "mcp__tcode_control__report_result",
        ] {
            for choice in [
                "allow",
                "deny",
                "allow_suggestion:0",
                "allow_suggestion:1",
                "cancel",
            ] {
                let mut mapper = Mapper::new();
                let events = mapper.on_message(
                    json!({"type":"control_request", "request_id":"native", "request": {
                        "subtype":"can_use_tool", "tool_name":name, "input":input,
                        "permission_suggestions":[session, user]
                    }}),
                );
                let [AgentEvent::ApprovalRequested(request)] = events.as_slice() else {
                    panic!("{events:?}")
                };
                assert_eq!(
                    request
                        .options
                        .iter()
                        .map(|option| option.id.as_str())
                        .collect::<Vec<_>>(),
                    ["allow", "deny", "allow_suggestion:0", "allow_suggestion:1"]
                );
                assert_eq!(
                    request.options[2].label,
                    "Allow and remember for this session"
                );
                assert!(mapper.take_outgoing().is_empty());
                let decision = if choice == "cancel" {
                    ApprovalDecision::Cancel
                } else {
                    ApprovalDecision::Option(choice.into())
                };
                let response = mapper.build_approval_response("native", decision).unwrap();
                let body = &response["response"]["response"];
                if matches!(choice, "deny" | "cancel") {
                    assert_eq!(body["behavior"], "deny");
                    assert_eq!(
                        body["message"],
                        if choice == "cancel" {
                            "User cancelled tool execution."
                        } else {
                            "User declined tool execution."
                        }
                    );
                } else {
                    assert_eq!(body["behavior"], "allow");
                    assert_eq!(body["updatedInput"], input);
                }
                match choice {
                    "allow_suggestion:0" => {
                        assert_eq!(body["updatedPermissions"], json!([session]))
                    }
                    "allow_suggestion:1" => assert_eq!(body["updatedPermissions"], json!([user])),
                    _ => assert!(body.get("updatedPermissions").is_none()),
                }
            }
        }
    }

    /// Every worktree goes through the host: the hooks are registered before
    /// any message, and the host's answer to Claude's `WorktreeCreate`
    /// callback reaches Claude as the hook's output.
    #[test]
    fn the_host_answers_claudes_worktree_hooks() {
        let mut mapper = Mapper::new();
        assert_eq!(
            mapper.initialize_request(),
            json!({"type":"control_request","request_id":"tcode-ctrl-1","request":{
                "subtype":"initialize",
                "hooks":{
                    "WorktreeCreate":[{"hookCallbackIds":["tcode-worktree-create"],"onFailure":"block"}],
                    "WorktreeRemove":[{"hookCallbackIds":["tcode-worktree-remove"],"onFailure":"block"}]
                }
            }})
        );
        feed(
            &mut mapper,
            r#"{"type":"control_request","request_id":"hook-1","request":{"subtype":"hook_callback","callback_id":"tcode-worktree-create","input":{"session_id":"native","transcript_path":"/home/me/.claude/projects/-work-repo/native.jsonl","cwd":"/work/repo","prompt_id":"prompt-1","hook_event_name":"WorktreeCreate","name":"feat"},"tool_use_id":"00000000-0000-4000-8000-000000000062"}}"#,
        );
        let answer = WorktreeAnswer::Ready {
            path: PathBuf::from("/data/worktrees/repo-1a2b3c4d/feat"),
            reused: false,
        };
        assert_eq!(
            mapper.worktree_response("hook-1", &answer),
            Some((
                json!({"type":"control_response","response":{
                    "subtype":"success","request_id":"hook-1",
                    "response":{"hookSpecificOutput":{
                        "hookEventName":"WorktreeCreate",
                        "worktreePath":"/data/worktrees/repo-1a2b3c4d/feat"
                    }}
                }}),
                WorktreeRequest::Create {
                    cwd: PathBuf::from("/work/repo"),
                    name: "feat".into(),
                }
            ))
        );
    }

    /// An `isolation: "worktree"` subagent asks for `agent-<agent id>` before
    /// its `task_started`; the worktree it is given is where that subagent
    /// works, while the session stays where it is. Removal and a refusal answer
    /// in the shapes Claude Code 2.1.319 accepts.
    #[test]
    fn a_subagent_works_in_the_worktree_its_hook_was_given() {
        let mut mapper = Mapper::new();
        mapper.cwd = Some(PathBuf::from("/work/repo"));
        feed(
            &mut mapper,
            r#"{"type":"assistant","message":{"id":"msg-1","role":"assistant","content":[{"type":"tool_use","id":"toolu_agent","name":"Agent","input":{"description":"Print working directory","prompt":"Run pwd","subagent_type":"general-purpose","isolation":"worktree"}}]},"parent_tool_use_id":null,"session_id":"native"}"#,
        );
        let events = feed(
            &mut mapper,
            r#"{"type":"control_request","request_id":"hook-1","request":{"subtype":"hook_callback","callback_id":"tcode-worktree-create","input":{"session_id":"native","transcript_path":"/home/me/.claude/projects/-work-repo/native.jsonl","cwd":"/work/repo","prompt_id":"prompt-1","hook_event_name":"WorktreeCreate","name":"agent-a0000000000000010"},"tool_use_id":"00000000-0000-4000-8000-000000000063"}}"#,
        );
        assert_eq!(
            events,
            [AgentEvent::WorktreeRequested {
                request_id: "hook-1".into(),
                request: WorktreeRequest::Create {
                    cwd: PathBuf::from("/work/repo"),
                    name: "agent-a0000000000000010".into(),
                },
            }]
        );
        let worktree = PathBuf::from("/data/worktrees/repo-1a2b3c4d/agent-a0000000000000010");
        mapper
            .worktree_response(
                "hook-1",
                &WorktreeAnswer::Ready {
                    path: worktree.clone(),
                    reused: false,
                },
            )
            .unwrap();
        let events = feed(
            &mut mapper,
            r#"{"type":"system","subtype":"task_started","task_id":"a0000000000000010","tool_use_id":"toolu_agent","description":"Print working directory","subagent_type":"general-purpose","is_backgrounded":true,"spawn_depth":1,"task_type":"local_agent","prompt":"Run pwd","session_id":"native"}"#,
        );
        assert!(
            events.contains(&AgentEvent::SessionMoved {
                from: PathBuf::from("/work/repo"),
                to: worktree.clone(),
                parent_item_id: Some("toolu_agent".into()),
            }),
            "{events:?}"
        );
        assert_eq!(mapper.cwd, Some(PathBuf::from("/work/repo")));

        feed(
            &mut mapper,
            r#"{"type":"control_request","request_id":"hook-2","request":{"subtype":"hook_callback","callback_id":"tcode-worktree-remove","input":{"session_id":"native","cwd":"/work/repo","hook_event_name":"WorktreeRemove","worktree_path":"/data/worktrees/repo-1a2b3c4d/agent-a0000000000000010"}}}"#,
        );
        let (removed, _) = mapper
            .worktree_response("hook-2", &WorktreeAnswer::Removed)
            .unwrap();
        assert_eq!(
            removed,
            json!({"type":"control_response","response":{"subtype":"success","request_id":"hook-2","response":{}}})
        );
        feed(
            &mut mapper,
            r#"{"type":"control_request","request_id":"hook-3","request":{"subtype":"hook_callback","callback_id":"tcode-worktree-create","input":{"session_id":"native","cwd":"/work/repo","hook_event_name":"WorktreeCreate","name":"feat"}}}"#,
        );
        let (refused, request) = mapper
            .worktree_response(
                "hook-3",
                &WorktreeAnswer::Refused {
                    error: "/work/repo is not a Git repository root".into(),
                },
            )
            .unwrap();
        assert_eq!(
            refused,
            json!({"type":"control_response","response":{"subtype":"error","request_id":"hook-3","error":"/work/repo is not a Git repository root"}})
        );
        assert!(matches!(request, WorktreeRequest::Create { name, .. } if name == "feat"));
        assert!(
            mapper
                .worktree_response("hook-3", &WorktreeAnswer::Removed)
                .is_none()
        );
    }

    /// The session's directory follows Claude: into the worktree `EnterWorktree`
    /// made, back to the one `ExitWorktree` restored (in the spelling Tcode
    /// started it with, though Claude reports it resolved), and, on resume, to
    /// the worktree `system/init` says Claude restored. A subagent's own
    /// `EnterWorktree` moves only that subagent.
    #[cfg(unix)]
    #[test]
    fn the_session_directory_follows_claude_in_and_out_of_worktrees() {
        let directory =
            std::env::temp_dir().join(format!("agent-claude-cwd-{}", uuid::Uuid::new_v4()));
        let checkout = directory.join("repo");
        std::fs::create_dir_all(&checkout).unwrap();
        let link = directory.join("link");
        std::os::unix::fs::symlink(&checkout, &link).unwrap();
        let resolved = checkout.canonicalize().unwrap();
        let worktree = PathBuf::from("/data/worktrees/repo-1a2b3c4d/feat");
        let started = |cwd: &Path| {
            let mut mapper = Mapper::new();
            mapper.launch_cwd = Some(link.clone());
            mapper.cwd = Some(link.clone());
            let events = mapper.on_message(json!({"type":"system","subtype":"init","cwd":cwd,"session_id":"native","permissionMode":"default"}));
            (mapper, events)
        };
        let moved = |events: &[AgentEvent]| -> Vec<(PathBuf, PathBuf)> {
            events
                .iter()
                .filter_map(|event| match event {
                    AgentEvent::SessionMoved {
                        from,
                        to,
                        parent_item_id: None,
                    } => Some((from.clone(), to.clone())),
                    _ => None,
                })
                .collect()
        };

        let (mut mapper, events) = started(&resolved);
        assert_eq!(moved(&events), []);
        let tool = |id: &str, name: &str, result: Value| {
            [
                json!({"type":"assistant","message":{"id":format!("msg-{id}"),"role":"assistant","content":[{"type":"tool_use","id":id,"name":name,"input":{}}]},"parent_tool_use_id":null,"session_id":"native"}),
                json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":id,"content":"done"}]},"parent_tool_use_id":null,"session_id":"native","tool_use_result":result}),
            ]
        };
        let mut events = Vec::new();
        for frame in tool(
            "toolu_enter",
            "EnterWorktree",
            json!({"worktreePath":worktree,"message":"Created worktree. The session is now working in the worktree."}),
        ) {
            events.extend(mapper.on_message(frame));
        }
        assert_eq!(moved(&events), [(link.clone(), worktree.clone())]);
        // A subagent's call reaches the session as that subagent's.
        let events = mapper.on_message(json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_child_enter","content":"done"}]},"parent_tool_use_id":"toolu_agent","session_id":"native","tool_use_result":{"worktreePath":"/data/worktrees/repo-1a2b3c4d/child"}}));
        assert_eq!(moved(&events), []);
        let mut events = Vec::new();
        for frame in tool(
            "toolu_exit",
            "ExitWorktree",
            json!({"action":"keep","originalCwd":resolved,"worktreePath":worktree,"restoredCwd":resolved,"originalCwdMissing":false}),
        ) {
            events.extend(mapper.on_message(frame));
        }
        assert_eq!(moved(&events), [(worktree.clone(), link.clone())]);

        let (_, events) = started(&worktree);
        assert_eq!(moved(&events), [(link.clone(), worktree.clone())]);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn fork_flag_requires_both_request_and_resume_cursor() {
        let resume = Some(ResumeCursor(json!({"session_id": "session-source"})));
        assert_eq!(
            resume_args(&resume, true, None),
            ["--resume", "session-source", "--fork-session"]
        );
        assert_eq!(
            resume_args(&resume, false, None),
            ["--resume", "session-source"]
        );
        assert!(resume_args(&None, true, None).is_empty());
    }

    /// A fork from a message ends at the entry the message answered, which
    /// only the transcript names (entries as 2.1.318 writes them), and leaves
    /// out `--resume-drops-turn`, which refuses a range of several turns.
    #[test]
    fn a_fork_from_a_message_ends_where_its_transcript_says_it_was_sent() {
        let dir = std::env::temp_dir().join(format!("agent-claude-fork-{}", uuid::Uuid::new_v4()));
        let project = dir.join("projects/-work");
        std::fs::create_dir_all(&project).unwrap();
        let entry = |kind: &str, uuid: &str, parent: Option<&str>| {
            json!({"type": kind, "uuid": uuid, "parentUuid": parent, "sessionId": "source",
                "message": {"role": kind, "content": [{"type": "text", "text": "text"}]}})
            .to_string()
        };
        std::fs::write(
            project.join("source.jsonl"),
            [
                entry("user", "prompt-1", None),
                entry("assistant", "reply-1", Some("prompt-1")),
                entry("user", "prompt-2", Some("reply-1")),
                entry("assistant", "reply-2", Some("prompt-2")),
                entry("user", "prompt-3", Some("reply-2")),
            ]
            .join("\n"),
        )
        .unwrap();
        let resume = Some(ResumeCursor(json!({"session_id": "source"})));

        let anchor = fork_anchor(&resume, Some(&dir), "prompt-2").unwrap();
        assert_eq!(
            resume_args(&resume, true, Some(&anchor)),
            [
                "--resume",
                "source",
                "--fork-session",
                "--resume-session-at",
                "reply-1"
            ]
        );
        for refused in ["prompt-1", "not-in-the-session"] {
            assert!(
                matches!(
                    fork_anchor(&resume, Some(&dir), refused),
                    Err(AgentError::Provider(_))
                ),
                "{refused}"
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A launch asks for the native permission prompt in the selected mode
    /// with a live switch to bypass allowed, puts Tcode's instructions in the
    /// system prompt (a file, or the text for a CLI without the file option),
    /// and leaves the user's launch arguments last so they win.
    #[test]
    fn a_launch_passes_the_selected_mode_and_leaves_the_users_arguments_last() {
        let base = [
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
        ];
        let os =
            |args: &[&str]| -> Vec<std::ffi::OsString> { args.iter().map(Into::into).collect() };
        let launch = ClaudeLaunchOptions {
            model_id: Some("claude-opus-4-8".into()),
            effort: Some("high".into()),
            settings_json: Some(r#"{"fastMode":true}"#.into()),
            ..ClaudeLaunchOptions::default()
        };
        let mcp = [crate::McpRegistration {
            name: "tcode_pull_requests".into(),
            url: "http://127.0.0.1:1/mcp".into(),
            bearer_token: "token".into(),
        }];
        let mcp_config = mcp_args(&mcp);
        let mut expected = os(&base);
        expected.extend(os(&[
            "auto",
            "--allow-dangerously-skip-permissions",
            "--replay-user-messages",
            "--model",
            "claude-opus-4-8",
            "--effort",
            "high",
            "--settings",
            r#"{"fastMode":true}"#,
            "--resume",
            "native",
            &mcp_config[0],
            &mcp_config[1],
            "--append-system-prompt-file",
            "/tmp/instructions.md",
            "--permission-mode",
            "plan",
        ]));
        assert_eq!(
            launch_args(
                "auto",
                true,
                &launch,
                vec!["--resume".into(), "native".into()],
                &mcp,
                Some(SystemPrompt::File(Path::new("/tmp/instructions.md"))),
                &["--permission-mode".into(), "plan".into()],
            ),
            expected
        );

        let mut expected = os(&base);
        expected.extend(os(&[
            "default",
            "--allow-dangerously-skip-permissions",
            "--append-system-prompt",
            "Link every PR.",
        ]));
        assert_eq!(
            launch_args(
                "default",
                false,
                &ClaudeLaunchOptions::default(),
                Vec::new(),
                &[],
                Some(SystemPrompt::Text("Link every PR.")),
                &[],
            ),
            expected
        );
    }

    #[test]
    fn mcp_builder_handles_both_one_and_none() {
        let registrations = [
            crate::McpRegistration {
                name: "tcode_preview".into(),
                url: "http://p".into(),
                bearer_token: "p".into(),
            },
            crate::McpRegistration {
                name: "tcode_control".into(),
                url: "http://o".into(),
                bearer_token: "o".into(),
            },
            crate::McpRegistration {
                name: "tcode_computer_use".into(),
                url: "http://c".into(),
                bearer_token: "c".into(),
            },
        ];
        for count in [0, 1, 3] {
            let args = mcp_args(&registrations[..count]);
            if count == 0 {
                assert!(args.is_empty());
                continue;
            }
            assert_eq!(args.len(), 2);
            assert_eq!(args[0], "--mcp-config");
            let config: Value = serde_json::from_str(&args[1]).unwrap();
            let servers = config["mcpServers"].as_object().unwrap();
            assert_eq!(servers.len(), count);
            for (name, url, authorization) in [
                ("tcode_preview", "http://p", "Bearer p"),
                ("tcode_control", "http://o", "Bearer o"),
                ("tcode_computer_use", "http://c", "Bearer c"),
            ]
            .into_iter()
            .take(count)
            {
                assert_eq!(
                    servers[name],
                    json!({
                        "type": "http",
                        "url": url,
                        "headers": {"Authorization": authorization},
                    })
                );
            }
        }
    }

    fn feed(mapper: &mut Mapper, line: &str) -> Vec<AgentEvent> {
        let msg: Value = serde_json::from_str(line).expect("valid json fixture line");
        mapper.on_message(msg)
    }

    /// [`feed`] without the list a task change also publishes, for tests of
    /// the liveness count.
    fn feed_count(mapper: &mut Mapper, line: &str) -> Vec<AgentEvent> {
        let mut events = feed(mapper, line);
        events.retain(|event| !matches!(event, AgentEvent::BackgroundTaskList { .. }));
        events
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
    fn mid_stream_fallback_is_published_before_the_response_finishes() {
        // Claude Code 2.1.299 against a local SSE endpoint using the documented
        // fallback boundary; only stream, assistant and result messages retained.
        let fixture = include_str!("../tests/fixtures/claude/mid-stream-fallback.jsonl");
        {
            let mut mapper = Mapper::new();
            mapper.start_turn();
            let mut switched = false;
            let mut completed = false;
            for line in fixture.lines() {
                let message: Value = serde_json::from_str(line).unwrap();
                let boundary = message
                    .pointer("/message/content/0/type")
                    .and_then(Value::as_str)
                    == Some("fallback");
                let events = mapper.on_message(message);
                if boundary {
                    assert!(!completed);
                    let notices = events
                        .iter()
                        .filter(|event| matches!(event, AgentEvent::ServedModel { .. }))
                        .cloned()
                        .collect::<Vec<_>>();
                    assert!(
                        !events
                            .iter()
                            .any(|event| matches!(event, AgentEvent::TurnCompleted { .. }))
                    );
                    assert_eq!(
                        notices,
                        [AgentEvent::ServedModel {
                            model: "claude-opus-4-8".into(),
                            reason: None
                        }]
                    );
                    switched = true;
                } else if switched {
                    assert!(
                        !events
                            .iter()
                            .any(|event| matches!(event, AgentEvent::ServedModel { .. })),
                        "later blocks must not repeat or undo the switch: {events:?}"
                    );
                }
                if events
                    .iter()
                    .any(|event| matches!(event, AgentEvent::TurnCompleted { .. }))
                {
                    assert!(switched, "the switch must be visible before completion");
                    completed = true;
                }
            }
            assert!(switched && completed);
        }
    }

    #[test]
    fn model_refusal_fallback_publishes_served_model_before_completion() {
        let mut mapper = Mapper::new();
        mapper.start_turn();
        let events = feed(
            &mut mapper,
            r#"{"type":"system","subtype":"model_refusal_fallback","trigger":"refusal","direction":"retry","scope":"session","original_model":"claude-fable-5","fallback_model":"claude-opus-4-8","request_id":"req_123","api_refusal_category":"cyber","api_refusal_explanation":null,"refused_user_message_uuid":"00000000-0000-4000-8000-000000000064","content":"Safeguards flagged this message. Switched models.","session_id":"session-1","uuid":"system-1"}"#,
        );

        assert_eq!(
            events,
            [AgentEvent::ServedModel {
                model: "claude-opus-4-8".into(),
                reason: None,
            }]
        );
    }

    #[test]
    fn classifier_refusal_result_fails_the_turn_with_claudes_text() {
        let mut mapper = Mapper::new();
        mapper.start_turn();

        let assistant_events = feed(
            &mut mapper,
            r#"{"type":"assistant","message":{"id":"msg-refusal","model":"<synthetic>","stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber","explanation":"This request triggered cyber content restrictions."},"content":[{"type":"text","text":"API Error: safeguards flagged this message. Details: `[cyber]`"}]},"parent_tool_use_id":null}"#,
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
            AgentEvent::Error { fatal: false, message } if message.contains("[cyber]")
        )));
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

    fn rewind_response(mapper: &mut Mapper, request: &Value, response: Value) -> Vec<AgentEvent> {
        feed(
            mapper,
            &json!({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": request["request_id"],
                    "response": response,
                }
            })
            .to_string(),
        )
    }

    /// Restoring files and conversation during a running turn: the turn is
    /// stopped first, files are restored, and when the files fail the
    /// conversation is still rewound, as Claude Code's own menu does.
    #[test]
    fn a_running_turn_is_stopped_before_files_and_a_files_failure_still_rewinds_the_conversation() {
        let mut mapper = Mapper::new();
        mapper.native_rewind = true;
        mapper.start_turn();
        let interrupt = mapper
            .begin_rewind("checkpoint-2".into(), RewindMode::FilesAndConversation)
            .unwrap();
        assert_eq!(interrupt["request"], json!({ "subtype": "interrupt" }));
        assert!(rewind_response(&mut mapper, &interrupt, json!({})).is_empty());
        let files = mapper.take_outgoing().remove(0);
        assert_eq!(
            files["request"],
            json!({ "subtype": "rewind_files", "user_message_id": "checkpoint-2" })
        );
        let error = "Failed to rewind: No file checkpoint found for this message.";
        let events = feed(
            &mut mapper,
            &json!({
                "type": "control_response",
                "response": {
                    "subtype": "error",
                    "request_id": files["request_id"],
                    "error": error,
                }
            })
            .to_string(),
        );
        assert!(events.is_empty());
        let conversation = mapper.take_outgoing().remove(0);
        assert_eq!(
            conversation["request"],
            json!({
                "subtype": "rewind_conversation",
                "target_message_uuid": "checkpoint-2",
                "interrupt_if_running": true,
                "allow_summarized_target": true,
            })
        );
        let events = rewind_response(
            &mut mapper,
            &conversation,
            json!({ "rewound": true, "prefillText": "original prompt" }),
        );
        assert_eq!(
            serde_json::to_value(&events).unwrap(),
            json!([{
                "type": "rewind_completed",
                "checkpoint_id": "checkpoint-2",
                "mode": "conversation",
                "prefill": "original prompt",
                "later_history": "truncated",
                "incomplete": {
                    "requested": "files_and_conversation",
                    "failure": { "reason": "nothing_to_restore", "detail": error },
                },
            }])
        );
    }

    /// A preview is Claude's dry run of the files a rewind would restore; a
    /// rewind of the conversation alone restores none and asks nothing.
    #[test]
    fn a_rewind_preview_is_a_dry_run_of_the_files() {
        let mut mapper = Mapper::new();
        assert_eq!(
            mapper.begin_preview("checkpoint-2".into(), RewindMode::FilesAndConversation),
            Some(
                json!({"type":"control_request","request_id":"tcode-ctrl-1","request":{
                    "subtype":"rewind_files","user_message_id":"checkpoint-2","dry_run":true
                }})
            )
        );
        assert_eq!(
            mapper.begin_preview("checkpoint-2".into(), RewindMode::Conversation),
            None
        );
    }

    /// `rewind_conversation` stops a running turn itself, so the turn it
    /// stops ends interrupted rather than failed.
    #[test]
    fn a_conversation_rewind_during_a_running_turn_ends_it_interrupted() {
        let mut mapper = Mapper::new();
        mapper.native_rewind = true;
        mapper.start_turn();
        mapper
            .begin_rewind("checkpoint-2".into(), RewindMode::Conversation)
            .unwrap();
        let events = feed(
            &mut mapper,
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"stopped"}"#,
        );
        assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));
        assert!(matches!(
            turn_completed_after_count(&events, 0),
            AgentEvent::TurnCompleted {
                status: TurnStatus::Interrupted,
                ..
            }
        ));
    }

    fn select(id: &str, value: Value) -> OptionSelection {
        OptionSelection {
            id: id.into(),
            value,
        }
    }

    fn settings(launch: &ClaudeLaunchOptions) -> Value {
        launch
            .settings_json
            .as_deref()
            .map(|settings| serde_json::from_str(settings).unwrap())
            .unwrap_or(Value::Null)
    }

    #[test]
    fn effort_map_drives_the_effort_flag() {
        let catalog = crate::claude_manifest::test_catalog();
        let resolve = |model: &str, effort: &str| {
            ClaudeLaunchOptions::resolve_with(
                &catalog,
                Some(model),
                &[select("reasoningEffort", json!(effort))],
            )
        };
        // Mapped to another value: ultracode → xhigh, plus the ultracode setting.
        let launch = resolve("test-wide", "ultracode");
        assert_eq!(launch.effort.as_deref(), Some("xhigh"));
        assert_eq!(settings(&launch)["ultracode"], true);
        // Per-profile downgrade (max → high) and passthrough.
        assert_eq!(
            resolve("test-narrow", "max").effort.as_deref(),
            Some("high")
        );
        assert_eq!(resolve("test-wide", "max").effort.as_deref(), Some("max"));
        assert_eq!(
            resolve("test-fixed", "xhigh").effort.as_deref(),
            Some("max")
        );
        assert_eq!(
            resolve("test-wide", "bogus").effort.as_deref(),
            Some("medium")
        );
        assert_eq!(
            ClaudeLaunchOptions::resolve_with(&catalog, Some("test-wide"), &[])
                .effort
                .as_deref(),
            Some("medium")
        );
        assert_eq!(resolve("test-plain", "low").effort, None);
        // Unknown model: no descriptor to resolve against, so no flag.
        assert_eq!(resolve("test-unknown", "high").effort, None);
    }

    #[test]
    fn parse_semver_from_version_output() {
        assert_eq!(
            crate::parse_semver("2.1.206 (Claude Code)"),
            Some((2, 1, 206))
        );
        assert_eq!(crate::parse_semver("2.1.169"), Some((2, 1, 169)));
        assert_eq!(crate::parse_semver("nonsense"), None);
    }

    #[test]
    fn context_window_launch_semantics() {
        let catalog = crate::claude_manifest::test_catalog();
        let resolve = |model: &str, value: Option<Value>| {
            let selections: Vec<_> = value
                .map(|value| select("contextWindow", value))
                .into_iter()
                .collect();
            ClaudeLaunchOptions::resolve_with(&catalog, Some(model), &selections)
        };
        let auto_compact =
            |launch: &ClaudeLaunchOptions| settings(launch)["autoCompactWindow"].clone();

        // Default window of a 1M-default profile: the bare slug already runs
        // at 1M, so no suffix is sent (and none is needed for `1m` either).
        for value in [None, Some(json!("1m"))] {
            let launch = resolve("test-wide", value);
            assert_eq!(launch.model_id.as_deref(), Some("test-wide"));
            assert!(launch.settings_json.is_none());
        }
        assert_eq!(catalog.resolved_context_window("test-wide", &[]), 1_000_000);
        // A suffixed id still resolves to the same model.
        assert_eq!(
            catalog.resolved_context_window("test-wide[1m]", &[]),
            1_000_000
        );

        // Below the largest window: bare slug plus early compaction.
        let launch = resolve("test-wide", Some(json!("200k")));
        assert_eq!(launch.model_id.as_deref(), Some("test-wide"));
        assert_eq!(auto_compact(&launch), json!(200_000));
        let launch = resolve("test-wide", Some(json!(500_000)));
        assert_eq!(launch.model_id.as_deref(), Some("test-wide"));
        assert_eq!(auto_compact(&launch), json!(500_000));

        // 200k-default profile: only the 1M expansion carries the suffix.
        for value in [Some(json!("200k")), None] {
            let launch = resolve("test-narrow", value);
            assert_eq!(launch.model_id.as_deref(), Some("test-narrow"));
            assert_eq!(auto_compact(&launch), json!(200_000));
        }
        let launch = resolve("test-narrow", Some(json!("1m")));
        assert_eq!(launch.model_id.as_deref(), Some("test-narrow[1m]"));
        assert!(launch.settings_json.is_none());
        let launch = resolve("test-narrow", Some(json!(500_000)));
        assert_eq!(launch.model_id.as_deref(), Some("test-narrow[1m]"));
        assert_eq!(auto_compact(&launch), json!(500_000));

        // Fixed window: no suffix, smaller values compact early.
        assert_eq!(
            catalog.resolved_context_window("test-fixed", &[]),
            1_000_000
        );
        let launch = resolve("test-fixed", Some(json!("1m")));
        assert_eq!(launch.model_id.as_deref(), Some("test-fixed"));
        assert!(launch.settings_json.is_none());
        let launch = resolve("test-fixed", Some(json!(500_000)));
        assert_eq!(launch.model_id.as_deref(), Some("test-fixed"));
        assert_eq!(auto_compact(&launch), json!(500_000));

        // An alias launches as the canonical slug with the same semantics.
        let launch = resolve("WIDE", Some(json!("200k")));
        assert_eq!(launch.model_id.as_deref(), Some("test-wide"));
        assert_eq!(auto_compact(&launch), json!(200_000));
        let launch = resolve("test-wide.0[1m]", None);
        assert_eq!(launch.model_id.as_deref(), Some("test-wide"));

        // No context data at all falls back to 200k and never emits settings.
        assert_eq!(catalog.resolved_context_window("test-plain", &[]), 200_000);
        assert_eq!(
            catalog.resolved_context_window("test-unknown", &[]),
            200_000
        );
        let launch = resolve("test-plain", Some(json!(500_000)));
        assert_eq!(launch.model_id.as_deref(), Some("test-plain"));
        assert!(launch.settings_json.is_none());
    }

    #[test]
    fn boolean_options_reach_settings_only_when_supported() {
        let catalog = crate::claude_manifest::test_catalog();
        let both = [
            select("fastMode", json!(true)),
            select("thinking", json!(true)),
        ];
        let launch = ClaudeLaunchOptions::resolve_with(&catalog, Some("test-fixed"), &both);
        let fixed = settings(&launch);
        assert_eq!(fixed["fastMode"], true);
        assert!(fixed.get("alwaysThinkingEnabled").is_none());

        let launch = ClaudeLaunchOptions::resolve_with(&catalog, Some("test-plain"), &both);
        let plain = settings(&launch);
        assert_eq!(plain["alwaysThinkingEnabled"], true);
        assert!(plain.get("fastMode").is_none());
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

        let cleared = feed(
            &mut m,
            r#"{"type":"assistant","message":{"id":"msg_c","content":[{"type":"tool_use","id":"toolu_clear","name":"TodoWrite","input":{"todos":[]}}]}}"#,
        );
        assert!(matches!(
            cleared.as_slice(),
            [AgentEvent::PlanUpdated { steps, .. }] if steps.is_empty()
        ));
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
        assert!(
            matches!(evs.iter().find(|event| matches!(event, AgentEvent::SessionStarted { .. })),
            Some(AgentEvent::SessionStarted { provider_session_id, resume, model })
            if provider_session_id == "s1" && resume.0["session_id"] == "s1" && model.as_deref() == Some("claude-opus-4-8"))
        );
        let commands = evs
            .iter()
            .find_map(|event| match event {
                AgentEvent::ProviderCommands { commands } => Some(commands),
                _ => None,
            })
            .expect("init must publish native commands and skills");
        assert_eq!(
            commands
                .iter()
                .map(|command| (command.name.as_str(), command.kind))
                .collect::<Vec<_>>(),
            vec![
                ("plan", ProviderCommandKind::Command),
                ("review", ProviderCommandKind::Command),
                ("dataviz", ProviderCommandKind::Skill),
            ]
        );
        let repeated = feed(
            &mut m,
            r#"{"type":"system","subtype":"init","session_id":"s1","model":"claude-opus-4-8"}"#,
        );
        assert!(!repeated.iter().any(|event| matches!(
            event,
            AgentEvent::SessionStarted { .. } | AgentEvent::ProviderCommands { .. }
        )));
    }

    #[test]
    fn recorded_compaction_retains_metadata_without_claiming_post_context() {
        let mut m = Mapper::new();
        let mut events = Vec::new();
        for line in include_str!("../tests/fixtures/claude/compaction_recorded.jsonl").lines() {
            events.extend(feed(&mut m, line));
        }
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], AgentEvent::ContextCompacted(c) if c.in_progress));
        assert!(matches!(&events[1], AgentEvent::ContextCompacted(c)
                if !c.in_progress && c.trigger.as_deref() == Some("manual")
                    && c.pre_tokens == Some(19555) && c.post_tokens == Some(2948)
                    && c.dropped_tokens == Some(16607) && c.duration_ms == Some(24455)));
        assert_eq!(m.latest_usage.used_tokens, None);
        assert_eq!(
            m.latest_usage.freshness,
            crate::ContextFreshness::AwaitingObservation
        );
    }

    #[test]
    fn recorded_multi_request_usage_keeps_latest_context() {
        let mut m = Mapper::new();
        m.start_turn();
        let mut observations = Vec::new();
        let mut completion = None;
        for line in include_str!("../tests/fixtures/claude/usage_recorded.jsonl").lines() {
            for event in feed(&mut m, line) {
                match event {
                    AgentEvent::TokenUsage(u) => observations.push(u.used_tokens),
                    AgentEvent::TurnCompleted { usage, .. } => completion = usage,
                    _ => {}
                }
            }
        }
        assert_eq!(
            observations,
            vec![
                Some(20250),
                Some(20250),
                Some(20250),
                Some(20250),
                Some(20635),
                Some(20635),
                Some(20635),
                Some(20764),
                Some(20764),
                Some(20764)
            ]
        );
        let usage = completion.unwrap();
        assert_eq!(usage.used_tokens, Some(20764));
        assert_eq!(usage.output_tokens, Some(5));
        assert_eq!(usage.turn_processed_tokens, Some(62084));
        assert_eq!(usage.context_window, Some(1000000));
    }

    #[test]
    fn streaming_usage_keeps_request_scope_at_completion() {
        let mut m = Mapper::new();
        m.start_turn();
        let start = feed(
            &mut m,
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"usage-1","usage":{"input_tokens":100,"cache_read_input_tokens":400,"cache_creation_input_tokens":50,"output_tokens":0}}}}"#,
        );
        assert!(
            matches!(start.last(), Some(AgentEvent::TokenUsage(u)) if u.used_tokens == Some(550)),
            "message start must expose the measured request context: {start:?}"
        );
        let delta = feed(
            &mut m,
            r#"{"type":"stream_event","event":{"type":"message_delta","usage":{"output_tokens":20}}}"#,
        );
        assert!(
            matches!(delta.last(), Some(AgentEvent::TokenUsage(u)) if u.used_tokens == Some(550)),
            "output-only delta must retain input and cache: {delta:?}"
        );
        let result = feed(
            &mut m,
            r#"{"type":"result","subtype":"success","usage":{"input_tokens":1000,"cache_read_input_tokens":4000000,"output_tokens":200},"modelUsage":{"claude":{"contextWindow":1000000}}}"#,
        );
        assert!(result.iter().any(|e| matches!(e, AgentEvent::TurnCompleted { usage: Some(u), .. } if u.used_tokens == Some(550) && u.turn_processed_tokens == Some(4001200))), "result accounting must not replace occupancy: {result:?}");
        m.start_turn();
        let missing_start = feed(
            &mut m,
            r#"{"type":"stream_event","event":{"type":"message_delta","usage":{"output_tokens":1}}}"#,
        );
        assert!(
            missing_start.is_empty(),
            "a new turn cannot reuse the previous request identity: {missing_start:?}"
        );
        assert_eq!(m.latest_usage.freshness, crate::ContextFreshness::LastKnown);
        let unknown = feed(
            &mut m,
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"usage-2"}}}"#,
        );
        assert!(
            matches!(unknown.last(), Some(AgentEvent::TokenUsage(u)) if u.used_tokens.is_none())
        );
        let output_only = feed(
            &mut m,
            r#"{"type":"stream_event","event":{"type":"message_delta","usage":{"output_tokens":1}}}"#,
        );
        assert!(
            matches!(output_only.last(), Some(AgentEvent::TokenUsage(u)) if u.used_tokens.is_none())
        );
    }

    #[test]
    fn rejected_rate_limit_is_emitted_after_failed_result_error() {
        let mut m = Mapper::new();
        feed(
            &mut m,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1800000000,"rateLimitType":"five_hour","utilization":1.0}}"#,
        );

        let events = feed(
            &mut m,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"done","session_id":"sess-1"}"#,
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::UsageLimitReached { .. }))
        );

        let mut m = Mapper::new();
        assert!(feed(
            &mut m,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1800000000,"rateLimitType":"five_hour","utilization":1.0}}"#,
        )
        .is_empty());

        let events = feed(
            &mut m,
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"You've hit your limit","session_id":"sess-1"}"#,
        );
        let error_index = events
            .iter()
            .position(|event| matches!(event, AgentEvent::Error { .. }))
            .expect("failed result error");
        assert!(matches!(
            events.get(error_index + 1),
            Some(AgentEvent::UsageLimitReached {
                resets_at: 1_800_000_000
            })
        ));
    }

    #[test]
    fn streamed_deltas_preserve_block_identity_and_content_kind() {
        for (index, delta, expected_kind, expected_text) in [
            (
                0,
                json!({"type":"text_delta","text":"Hi"}),
                DeltaKind::AssistantText,
                "Hi",
            ),
            (
                1,
                json!({"type":"thinking_delta","thinking":"hmm"}),
                DeltaKind::ReasoningText,
                "hmm",
            ),
        ] {
            let mut m = Mapper::new();
            feed(
                &mut m,
                r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_1"}}}"#,
            );
            let events = feed(
                &mut m,
                &json!({"type":"stream_event", "event": {
                    "type":"content_block_delta", "index":index, "delta":delta
                }})
                .to_string(),
            );
            assert!(
                matches!(events.as_slice(), [AgentEvent::Delta { item_id, kind, text }]
                if item_id == &format!("msg_1:{index}") && *kind == expected_kind && text == expected_text)
            );
        }
    }

    /// Captured from Claude Code 2.1.312 with claude-opus-5-5 and thinking on:
    /// the prose the model wrote between two tool calls arrives as a second
    /// thinking block holding the server's summary of it, and only the
    /// `assistant` frame's `narration_block_indexes` tells it apart from
    /// reasoning. It must complete the streamed item as a summarized message,
    /// or the reply stays folded into the work log.
    #[test]
    fn narration_completes_its_streamed_block_as_a_summarized_message() {
        let mut m = Mapper::new();
        for line in [
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"model":"claude-opus-5-5","id":"msg_n","content":[]}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}},"thinking_display":"updates"}"#,
            r#"{"type":"assistant","message":{"model":"claude-opus-5-5","id":"msg_n","content":[{"type":"thinking","thinking":"","signature":"CAQS+QQKEAgTGAI4AUIIdGhpbmtpbmc"}]},"thinking_duration_ms":2214,"thinking_display":"updates"}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":"","signature":""}},"thinking_display":"updates"}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"f.txt contains two lines."}},"thinking_display":"updates"}"#,
        ] {
            feed(&mut m, line);
        }
        let completed = feed(
            &mut m,
            r#"{"type":"assistant","message":{"model":"claude-opus-5-5","id":"msg_n","content":[{"type":"thinking","thinking":"f.txt contains two lines.","signature":"CAQS0gYKEQgTGAI4AUIJbmFycmF0aW9u"}]},"narration_block_indexes":[0],"thinking_duration_ms":2,"thinking_display":"updates"}"#,
        );
        assert!(
            matches!(
                completed.as_slice(),
                [AgentEvent::ItemCompleted(ThreadItem {
                    id,
                    parent_item_id: None,
                    content: ItemContent::AssistantMessage { text, summarized: true },
                })] if id == "msg_n:1" && text == "f.txt contains two lines."
            ),
            "{completed:?}"
        );
    }

    /// Captured from Claude Code 2.1.294 with claude-opus-5-5: the thinking
    /// block opens with `content_block_start`, every `thinking_delta` is empty
    /// and the `assistant` line repeats the block with no text. The block's
    /// start is the one event that can announce the thinking, so it must
    /// become an in-progress reasoning item that the empty deltas and the
    /// empty completion leave alone.
    #[test]
    fn hidden_thinking_announces_itself_from_the_block_start() {
        let mut m = Mapper::new();
        feed(
            &mut m,
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_h"}}}"#,
        );
        let started = feed(
            &mut m,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}}"#,
        );
        assert!(
            matches!(
                started.as_slice(),
                [AgentEvent::ItemStarted(ThreadItem {
                    id,
                    parent_item_id: None,
                    content: ItemContent::Reasoning { text },
                })] if id == "msg_h:0" && text.is_empty()
            ),
            "{started:?}"
        );
        for line in [
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":""}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}}"#,
            r#"{"type":"assistant","message":{"id":"msg_h","content":[{"type":"thinking","thinking":"","signature":"sig"}]}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}}"#,
        ] {
            let events = feed(&mut m, line);
            assert!(events.is_empty(), "{line}: {events:?}");
        }
    }

    /// Captured from a real session: the CLI splits one message across several
    /// `assistant` lines, each carrying a single-element `content` array — a
    /// (redacted, empty) thinking block first, then the text block. Enumerating
    /// each array on its own numbered the text block 0 while its deltas streamed
    /// under index 1, so the timeline rendered the paragraph twice: once live,
    /// once again from the completion. The completed item must land on the
    /// stream's id, and the empty thinking block must not complete an item.
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

    /// The shape Claude Code 2.1.316 prints (`claude -p --output-format
    /// stream-json --verbose --model haiku` in a scratch directory holding an
    /// 8x8 blue PNG and `a.txt`), its ids and paths invented: a Read of the
    /// image, then a subagent that
    /// runs Bash, reads and edits `a.txt`. The image is the result's only
    /// content and reaches `image_reads` alone; the subagent's Bash and Edit
    /// map as they do at the top level.
    #[test]
    fn recorded_image_read_and_subagent_tools_map_like_top_level_calls() {
        let mut mapper = Mapper::new();
        let mut completed = Vec::new();
        for line in include_str!("../tests/fixtures/claude/tool_results_recorded.jsonl").lines() {
            for event in feed(&mut mapper, line) {
                if let AgentEvent::ItemCompleted(item) = event {
                    completed.push(item);
                }
            }
        }
        let item = |id: &str| {
            completed
                .iter()
                .find(|item| item.id == id)
                .unwrap_or_else(|| panic!("{id} never completed: {completed:#?}"))
        };
        assert_eq!(
            item("toolu_010000000000000000000014").content,
            ItemContent::ToolCall {
                image_reads: vec![Attachment {
                    media_type: "image/png".into(),
                    data_base64: "iVBORw0KGgoAAAANSUhEUgAAAAgAAAAICAIAAABLbSncAAAAEElEQVR4nGNgYPiPAw0pCQCpcD/BFMrqcwAAAABJRU5ErkJggg==".into(),
                    source_path: None,
                }],
                name: "Read".into(),
                input: json!({"file_path": "blue.png"}),
                output: Some(String::new()),
                status: ItemStatus::Completed,
                family: Some(Box::new(ToolFamily::Read {
                    path: "blue.png".into(),
                    first_line: None,
                    note: None,
                })),
            }
        );
        let child = |id: &str| {
            let item = item(&format!("toolu_010000000000000000000015:{id}"));
            assert_eq!(
                item.parent_item_id.as_deref(),
                Some("toolu_010000000000000000000015")
            );
            &item.content
        };
        assert_eq!(
            child("toolu_010000000000000000000016"),
            &ItemContent::CommandExecution {
                command: "echo hi".into(),
                output: "hi".into(),
                exit_code: None,
                status: ItemStatus::Completed,
            }
        );
        assert_eq!(
            child("toolu_010000000000000000000018"),
            &ItemContent::FileChange {
                changes: vec![FileChange {
                    path: "/home/user/project/a.txt".into(),
                    kind: FileChangeKind::Modify,
                    diff: Some("-hello\n+bye".into()),
                }],
                status: ItemStatus::Completed,
            }
        );
        // A subagent's tool result comes without the output object, so its
        // read is told by the lines the result numbers.
        assert!(matches!(
            child("toolu_010000000000000000000017"),
            ItemContent::ToolCall { name, output: Some(output), family: Some(family), .. }
                if name == "Read" && output == "hello\nworld\n" && matches!(&**family, ToolFamily::Read { path, first_line: Some(1), .. } if path == "/home/user/project/a.txt")
        ));
    }

    /// A command is a command whichever shell runs it, and one that exited
    /// non-zero carries the code Claude Code wrote before its output
    /// (2.1.322's tool error formatter: `Exit code ${code}`, then what the
    /// command printed); the lines are stream-json `assistant` and `user`
    /// messages with Anthropic `tool_use` and `tool_result` blocks, content
    /// invented.
    #[test]
    fn a_command_carries_the_code_it_exited_with() {
        let mut mapper = Mapper::new();
        let started = feed(
            &mut mapper,
            r#"{"type":"assistant","message":{"id":"msg_ps","content":[{"type":"tool_use","id":"toolu_ps","name":"PowerShell","input":{"command":"Get-ChildItem build"}}]}}"#,
        );
        assert!(
            matches!(
                started.as_slice(),
                [AgentEvent::ItemStarted(ThreadItem { content: ItemContent::CommandExecution { command, .. }, .. })]
                    if command == "Get-ChildItem build"
            ),
            "{started:?}"
        );
        let completed = feed(
            &mut mapper,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_ps","is_error":true,"content":"Exit code 2\nGet-ChildItem: Cannot find path 'build'."}]}}"#,
        );
        let [AgentEvent::ItemCompleted(item)] = completed.as_slice() else {
            panic!("{completed:?}");
        };
        assert_eq!(
            item.content,
            ItemContent::CommandExecution {
                command: "Get-ChildItem build".into(),
                output: "Get-ChildItem: Cannot find path 'build'.".into(),
                exit_code: Some(2),
                status: ItemStatus::Failed,
            }
        );
    }

    /// A Read is a read of the file it names: its lines as the output object
    /// gives them, without the `N\t` numbers of the tool result, from the
    /// line the read began at (`FileReadOutput` of `sdk-tools.d.ts` in
    /// `@anthropic-ai/claude-agent-sdk` 0.3.322, for Claude Code 2.1.322;
    /// content invented).
    #[test]
    fn a_read_is_the_lines_it_read_from_where_it_began() {
        let mut mapper = Mapper::new();
        let started = feed(
            &mut mapper,
            r#"{"type":"assistant","message":{"id":"msg_read","content":[{"type":"tool_use","id":"toolu_read","name":"Read","input":{"file_path":"/work/src/lib.rs","offset":40,"limit":2}}]}}"#,
        );
        assert!(
            matches!(
                started.as_slice(),
                [AgentEvent::ItemStarted(ThreadItem { content: ItemContent::ToolCall { family: Some(family), .. }, .. })]
                    if matches!(&**family, ToolFamily::Read { path, first_line: None, .. } if path == "/work/src/lib.rs")
            ),
            "{started:?}"
        );
        let completed = feed(
            &mut mapper,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_read","content":"40\tfn main() {\n41\t}"}]},"tool_use_result":{"type":"text","file":{"filePath":"/work/src/lib.rs","content":"fn main() {\n}","numLines":2,"startLine":40,"totalLines":90}}}"#,
        );
        let [AgentEvent::ItemCompleted(item)] = completed.as_slice() else {
            panic!("{completed:?}");
        };
        let ItemContent::ToolCall { output, family, .. } = &item.content else {
            panic!("{item:?}");
        };
        assert_eq!(output.as_deref(), Some("fn main() {\n}"));
        assert_eq!(
            family.as_deref(),
            Some(&ToolFamily::Read {
                path: "/work/src/lib.rs".into(),
                first_line: Some(40),
                note: None,
            })
        );
    }

    /// A Grep is a search of what its output object found: in content mode
    /// rg's `path:N:text` lines, context lines left out; a Glob the files in
    /// its `filenames` (`GrepOutput`, `GlobOutput` of `sdk-tools.d.ts` in
    /// `@anthropic-ai/claude-agent-sdk` 0.3.322; content invented).
    #[test]
    fn a_search_is_what_its_output_object_found() {
        let mut mapper = Mapper::new();
        feed(
            &mut mapper,
            r#"{"type":"assistant","message":{"id":"msg_grep","content":[{"type":"tool_use","id":"toolu_grep","name":"Grep","input":{"pattern":"TODO","path":"src","output_mode":"content","-C":1}},{"type":"tool_use","id":"toolu_glob","name":"Glob","input":{"pattern":"**/*.rs"}}]}}"#,
        );
        let completed = feed(
            &mut mapper,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_grep","content":"src/a.rs-2-fn a() {\nsrc/a.rs:3:    // TODO: one\n--\nsrc/b.rs:7:// TODO: two"}]},"tool_use_result":{"mode":"content","numFiles":0,"filenames":[],"content":"src/a.rs-2-fn a() {\nsrc/a.rs:3:    // TODO: one\n--\nsrc/b.rs:7:// TODO: two","numLines":4}}"#,
        );
        let [AgentEvent::ItemCompleted(grep)] = completed.as_slice() else {
            panic!("{completed:?}");
        };
        let ItemContent::ToolCall { family, .. } = &grep.content else {
            panic!("{grep:?}");
        };
        let hit = |path: &str, line, text: &str| SearchHit {
            path: path.into(),
            line: Some(line),
            text: Some(text.into()),
        };
        assert_eq!(
            family.as_deref(),
            Some(&ToolFamily::Search {
                scope: SearchScope::Content,
                pattern: Some("TODO".into()),
                path: Some("src".into()),
                glob: None,
                hits: Some(vec![
                    hit("src/a.rs", 3, "    // TODO: one"),
                    hit("src/b.rs", 7, "// TODO: two"),
                ]),
            })
        );
        let completed = feed(
            &mut mapper,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_glob","content":"src/a.rs\nsrc/b.rs"}]},"tool_use_result":{"filenames":["src/a.rs","src/b.rs"],"durationMs":3,"numFiles":2,"truncated":false}}"#,
        );
        let [AgentEvent::ItemCompleted(glob)] = completed.as_slice() else {
            panic!("{completed:?}");
        };
        let ItemContent::ToolCall { family, .. } = &glob.content else {
            panic!("{glob:?}");
        };
        let Some(ToolFamily::Search { scope, hits, .. }) = family.as_deref() else {
            panic!("{family:?}");
        };
        assert_eq!(*scope, SearchScope::Files);
        assert_eq!(
            hits.as_deref()
                .map(|hits| hits.iter().map(|hit| hit.path.as_str()).collect::<Vec<_>>()),
            Some(vec!["src/a.rs", "src/b.rs"])
        );
    }

    /// A message sent to a resumed session whose background command did not
    /// outlive the previous process: Claude Code first reports the command
    /// stopped and closes that notification with a `result` of its own
    /// (`num_turns: 0`, origin `task-notification`), then runs the message
    /// (2.1.321's shape, its ids invented).
    #[test]
    fn a_resumed_sessions_stopped_task_result_leaves_the_message_turn_open() {
        let mut m = Mapper::new();
        let turn_id = m.start_turn();
        let turn_events: Vec<_> =
            include_str!("../tests/fixtures/claude/resume_stopped_task_trace.jsonl")
                .lines()
                .flat_map(|line| feed(&mut m, line))
                .filter(|event| {
                    matches!(
                        event,
                        AgentEvent::TurnStarted { .. }
                            | AgentEvent::TurnCompleted { .. }
                            | AgentEvent::ItemCompleted(ThreadItem {
                                content: ItemContent::AssistantMessage { .. },
                                ..
                            })
                    )
                })
                .collect();
        assert!(
            matches!(
                turn_events.as_slice(),
                [
                    AgentEvent::ItemCompleted(ThreadItem {
                        content: ItemContent::AssistantMessage { text, .. },
                        ..
                    }),
                    AgentEvent::TurnCompleted {
                        turn_id: completed,
                        status: TurnStatus::Completed,
                        ..
                    },
                ] if text == "PONG" && completed == &turn_id
            ),
            "{turn_events:?}"
        );
    }

    #[test]
    fn reinvocation_after_background_task_synthesizes_turn() {
        let mut m = Mapper::new();
        feed(
            &mut m,
            r#"{"type":"system","subtype":"init","session_id":"session-bg","model":"claude-haiku-4-5-20251001"}"#,
        );
        let first_turn_id = m.start_turn();

        let started = feed(
            &mut m,
            r#"{"type":"assistant","message":{"id":"msg-bg","content":[{"type":"tool_use","id":"toolu-bg","name":"Bash","input":{"command":"sleep 8 && echo BG_DONE","description":"Launch background sleep command","run_in_background":true}}]}}"#,
        );
        assert!(matches!(
            started.as_slice(),
            [AgentEvent::ItemStarted(ThreadItem {
                content: ItemContent::CommandExecution {
                    status: ItemStatus::InProgress,
                    ..
                },
                ..
            })]
        ));
        let count = feed_count(
            &mut m,
            r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"bg-1","task_type":"local_bash","description":"Launch background sleep command"}]}"#,
        );
        assert!(matches!(
            count.as_slice(),
            [AgentEvent::BackgroundTasksChanged { count: 1 }]
        ));
        let task_started = feed(
            &mut m,
            r#"{"type":"system","subtype":"task_started","task_id":"bg-1","tool_use_id":"toolu-bg","description":"Launch background sleep command","task_type":"local_bash"}"#,
        );
        // Claude lists the task before it names the call that started it;
        // the list follows once it has.
        assert!(matches!(
            task_started.as_slice(),
            [AgentEvent::BackgroundTaskList { tasks }]
                if tasks.len() == 1
                    && tasks[0].item_id.as_deref() == Some("toolu-bg")
                    && tasks[0].kind == crate::BackgroundKind::Command
        ));
        let running = feed(
            &mut m,
            r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu-bg","type":"tool_result","content":"Command running in background with ID: bg-1","is_error":false}]},"tool_use_result":{"stdout":"","stderr":"","interrupted":false,"backgroundTaskId":"bg-1"}}"#,
        );
        // The call returned; the command goes on in the background.
        assert!(matches!(
            running.as_slice(),
            [
                AgentEvent::ItemUpdated(ThreadItem {
                    content: ItemContent::CommandExecution {
                        status: ItemStatus::InProgress,
                        exit_code: None,
                        ..
                    },
                    ..
                }),
                AgentEvent::ItemBackgrounded { item_id, parent_item_id: None },
            ] if item_id == "toolu-bg"
        ));
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
            feed_count(
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
        let reinit = feed(
            &mut m,
            r#"{"type":"system","subtype":"init","session_id":"session-bg","model":"claude-haiku-4-5-20251001"}"#,
        );
        assert!(!reinit.iter().any(|event| matches!(
            event,
            AgentEvent::SessionStarted { .. }
                | AgentEvent::TurnStarted { .. }
                | AgentEvent::TurnCompleted { .. }
        )));

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
                content: ItemContent::AssistantMessage { text, .. },
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
        feed_count(
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
            feed_count(
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

    /// A live change sends the model first, because `set_model` validates it
    /// against the API before answering, then the flags; only the last answer
    /// reports it. Answers recorded from Claude Code 2.1.318.
    #[test]
    fn a_live_settings_change_sends_the_model_then_the_flags() {
        let mut m = Mapper::new();
        m.applied_settings = ClaudeLaunchOptions {
            model_id: Some("claude-opus-4-8".into()),
            effort: Some("max".into()),
            ..ClaudeLaunchOptions::default()
        };
        let target = ClaudeLaunchOptions {
            model_id: Some("claude-sonnet-4-6[1m]".into()),
            effort: Some("low".into()),
            fast_mode: true,
            ..ClaudeLaunchOptions::default()
        };

        let model = m.begin_settings(target.clone()).unwrap().unwrap();
        assert_eq!(
            model["request"],
            json!({"subtype": "set_model", "model": "claude-sonnet-4-6[1m]"})
        );
        let answer = format!(
            r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"{}","response":{{"model":"claude-sonnet-4-6"}}}}}}"#,
            model["request_id"].as_str().unwrap()
        );
        assert!(feed(&mut m, &answer).is_empty());

        let flags = m.take_outgoing().remove(0);
        assert_eq!(
            flags["request"],
            json!({
                "subtype": "apply_flag_settings",
                "settings": {"effortLevel": "low", "fastMode": true},
            })
        );
        let answer = format!(
            r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"{}"}}}}"#,
            flags["request_id"].as_str().unwrap()
        );
        assert!(matches!(
            feed(&mut m, &answer).as_slice(),
            [AgentEvent::SettingsApplied { applied: true }]
        ));
        assert_eq!(
            m.begin_settings(target).unwrap(),
            None,
            "a process already there is not asked again"
        );
    }

    /// A model the CLI cannot validate leaves the process on its old one, and
    /// the host restarts for it; a setting the CLI cannot take live is not
    /// sent at all.
    #[test]
    fn a_live_settings_change_it_cannot_make_takes_a_restart() {
        let mut m = Mapper::new();
        m.applied_settings.model_id = Some("claude-opus-4-8".into());
        let request = m
            .begin_settings(ClaudeLaunchOptions {
                model_id: Some("claude-sonnet-4-6".into()),
                ..ClaudeLaunchOptions::default()
            })
            .unwrap()
            .unwrap();
        let answer = format!(
            r#"{{"type":"control_response","response":{{"subtype":"error","request_id":"{}","error":"Unable to validate model","error_code":"check_failed","model":"claude-opus-4-8"}}}}"#,
            request["request_id"].as_str().unwrap()
        );
        assert!(matches!(
            feed(&mut m, &answer).as_slice(),
            [AgentEvent::SettingsApplied { applied: false }]
        ));
        assert_eq!(
            m.applied_settings.model_id.as_deref(),
            Some("claude-opus-4-8")
        );

        for target in [
            ClaudeLaunchOptions {
                thinking: Some(false),
                ..ClaudeLaunchOptions::default()
            },
            ClaudeLaunchOptions {
                auto_compact_window: Some(500_000),
                ..ClaudeLaunchOptions::default()
            },
        ] {
            assert!(m.begin_settings(target).is_err());
        }
    }

    /// A model that opens the selected window by itself needs no compaction
    /// limit, so switching to it does not take a restart; leaving one the
    /// process was launched with does, because only a launch sets that limit.
    #[test]
    fn a_model_with_its_own_window_needs_no_restart_for_it() {
        let native = ClaudeLaunchOptions {
            model_id: Some("claude-sonnet-4-6".into()),
            auto_compact_window: Some(200_000),
            native_window: true,
            ..ClaudeLaunchOptions::default()
        };
        let mut m = Mapper::new();
        m.applied_settings.model_id = Some("claude-opus-4-8".into());
        assert!(m.begin_settings(native.clone()).unwrap().is_some());

        let mut launched = Mapper::new();
        launched.applied_settings = native;
        assert!(
            launched
                .begin_settings(ClaudeLaunchOptions {
                    model_id: Some("claude-opus-4-8".into()),
                    ..ClaudeLaunchOptions::default()
                })
                .is_err()
        );
    }

    /// The list names what runs, keeps when each was first seen, and empties
    /// at once, ahead of the withheld count. `stop_task` takes one id, so all
    /// of them is one request each. Shapes recorded from Claude Code 2.1.318,
    /// a subagent and the shell it started.
    #[test]
    fn background_tasks_are_listed_and_stopped_one_request_each() {
        let tasks = |events: &[AgentEvent]| {
            events
                .iter()
                .find_map(|event| match event {
                    AgentEvent::BackgroundTaskList { tasks } => Some(tasks.clone()),
                    _ => None,
                })
                .expect("a task change publishes the list")
        };
        let mut m = Mapper::new();
        let first = tasks(&feed(
            &mut m,
            r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"a0000000000000011","run_id":"000000002-00000000","task_type":"local_agent","subagent_type":"general-purpose","description":"wait helper"}]}"#,
        ));
        assert_eq!(first[0].id, "a0000000000000011");
        assert_eq!(first[0].name, "wait helper");
        assert_eq!(first[0].kind, crate::BackgroundKind::Agent);
        let second = tasks(&feed(
            &mut m,
            r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"a0000000000000011","run_id":"000000002-00000000","task_type":"local_agent","subagent_type":"general-purpose","description":"wait helper"},{"task_id":"b00000003","run_id":"000000003-00000000","task_type":"local_bash","description":"wait","parent_task_id":"a0000000000000011"}]}"#,
        ));
        assert_eq!(second.len(), 2);
        assert_eq!(second[0], first[0]);

        let requests: Vec<Value> = m
            .stop_task_requests(None)
            .into_iter()
            .map(|message| message["request"].clone())
            .collect();
        assert_eq!(
            requests,
            [
                json!({"subtype": "stop_task", "task_id": "a0000000000000011"}),
                json!({"subtype": "stop_task", "task_id": "b00000003"}),
            ]
        );

        let drained = feed(
            &mut m,
            r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#,
        );
        assert!(
            matches!(drained.as_slice(), [AgentEvent::BackgroundTaskList { tasks }] if tasks.is_empty())
        );
    }

    /// Moving a call to the background is Ctrl+B for that call: the
    /// `background_tasks` control request naming it (Claude Code 2.1.319's
    /// SDK schema). A refusal is the only word of one that did nothing.
    #[test]
    fn a_call_moves_to_the_background_and_a_refusal_says_so() {
        let mut m = Mapper::new();
        let request = m.move_to_background_request("toolu-bash".into());
        assert_eq!(
            request["request"],
            json!({"subtype": "background_tasks", "tool_use_id": "toolu-bash"})
        );
        let id = request["request_id"].as_str().unwrap().to_owned();
        let refused = m.on_message(json!({"type":"control_response","response":{
            "subtype":"success","request_id": id,"response":{"backgrounded":false}}}));
        assert!(
            matches!(refused.as_slice(), [AgentEvent::Warning { .. }]),
            "{refused:?}"
        );

        let request = m.move_to_background_request("toolu-agent".into());
        let id = request["request_id"].as_str().unwrap().to_owned();
        let moved = m.on_message(json!({"type":"control_response","response":{
            "subtype":"success","request_id": id,"response":{"backgrounded":true}}}));
        assert!(moved.is_empty(), "{moved:?}");
    }

    /// A task stopped through `stop_task` starts no follow-up turn: the CLI
    /// empties its list and goes idle, and that idle is the zero's only chance.
    /// Shapes recorded from Claude Code 2.1.318.
    #[test]
    fn a_stopped_task_publishes_zero_when_the_cli_goes_idle() {
        let mut m = Mapper::new();
        feed_count(
            &mut m,
            r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"b00000002","run_id":"000000004-00000000","task_type":"local_bash","description":"wait"}]}"#,
        );
        for line in [
            r#"{"type":"system","subtype":"task_updated","task_id":"b00000002","run_id":"000000004-00000000","patch":{"status":"killed","end_time":1794082805458}}"#,
            r#"{"type":"system","subtype":"task_notification","task_id":"b00000002","run_id":"000000004-00000000","tool_use_id":"toolu_0025","status":"stopped","summary":"wait"}"#,
            r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#,
        ] {
            assert!(feed_count(&mut m, line).is_empty());
        }
        assert!(matches!(
            feed(
                &mut m,
                r#"{"type":"system","subtype":"session_state_changed","state":"idle"}"#,
            )
            .as_slice(),
            [AgentEvent::BackgroundTasksChanged { count: 0 }]
        ));
        assert!(
            feed(
                &mut m,
                r#"{"type":"system","subtype":"session_state_changed","state":"idle"}"#,
            )
            .is_empty(),
            "a turn's own idle says nothing about background work"
        );
    }

    /// Every task the CLI lists keeps the process busy, not only background
    /// Bash: a backgrounded Agent (`task_type: local_agent`, as the CLI
    /// reports it) must hold the count above zero until its notification's
    /// follow-up result, or the runtime judges the session quiescent while
    /// the subagent is still running. Shapes recorded from Claude Code 2.1.301.
    #[test]
    fn non_bash_background_tasks_count_until_their_follow_up_result() {
        let mut m = Mapper::new();
        m.start_turn();
        feed(
            &mut m,
            r#"{"type":"assistant","message":{"id":"msg-spawn","content":[{"type":"tool_use","id":"toolu_agent","name":"Agent","input":{"description":"Run sleep and echo command","subagent_type":"general-purpose","run_in_background":true}}]}}"#,
        );
        let counted = feed_count(
            &mut m,
            r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"a0000000000000012","run_id":"000000005-00000000","task_type":"local_agent","subagent_type":"general-purpose","description":"Run sleep and echo command"}]}"#,
        );
        assert!(matches!(
            counted.as_slice(),
            [AgentEvent::BackgroundTasksChanged { count: 1 }]
        ));
        feed(
            &mut m,
            r#"{"type":"system","subtype":"task_started","task_id":"a0000000000000012","tool_use_id":"toolu_agent","description":"Run sleep and echo command","subagent_type":"general-purpose","is_backgrounded":true,"task_type":"local_agent"}"#,
        );
        feed(
            &mut m,
            r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_agent","type":"tool_result","content":[{"type":"text","text":"Async agent launched successfully."}]}]},"tool_use_result":{"isAsync":true,"status":"async_launched","agentId":"a0000000000000012"}}"#,
        );
        let launched = feed(
            &mut m,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"launched"}"#,
        );
        turn_completed_after_count(&launched, 1);

        // Between turns, the background subagent asks for permission.
        let asked = feed(
            &mut m,
            r#"{"type":"control_request","request_id":"ctrl-bg","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"cp a b"},"tool_use_id":"toolu_cp","agent_id":"a0000000000000012"}}"#,
        );
        let [AgentEvent::ApprovalRequested(request)] = asked.as_slice() else {
            panic!("expected one approval request, got {asked:?}");
        };
        assert_eq!(request.parent_item_id.as_deref(), Some("toolu_agent"));

        let notification = feed(
            &mut m,
            r#"{"type":"system","subtype":"task_notification","task_id":"a0000000000000012","tool_use_id":"toolu_agent","status":"completed","output_file":"/tmp/a0000000000000012.output","summary":"SUB_DONE"}"#,
        );
        assert!(
            !notification
                .iter()
                .any(|event| matches!(event, AgentEvent::ItemCompleted(_))),
            "a subagent task is settled as a Subagent, never as a command card: {notification:?}"
        );
        assert!(
            feed_count(
                &mut m,
                r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#,
            )
            .is_empty()
        );
        m.start_turn();
        let done = feed(
            &mut m,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"DONE","origin":{"kind":"task-notification","producer":"session-task","runId":"000000005-00000000"}}"#,
        );
        turn_completed_after_count(&done, 0);
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

    /// A safety check that bypassPermissions cannot skip still asks; the
    /// request carries Claude Code's consent line, and the agent's own
    /// description of the command stays beside it.
    #[test]
    fn a_permission_request_says_why_claude_code_asks() {
        let mut m = Mapper::new();
        m.start_turn();
        let events = feed(
            &mut m,
            r#"{"type":"control_request","request_id":"ctrl-rm","request":{"subtype":"can_use_tool","tool_name":"Bash","display_name":"Bash","input":{"command":"rm -rf $F/*","description":"Clear the scratch directory"},"description":"Clear the scratch directory","decision_reason":"Dangerous rm operation detected in `rm -rf $F/*`. The target '$F/*' is a shell variable expansion: when $F is unset or empty it becomes `/`, `/*` or a top-level path. This requires explicit approval and cannot be auto-allowed by permission rules.","decision_reason_type":"safetyCheck","classifier_approvable":false,"tool_use_id":"toolu_rm"}}"#,
        );
        let [AgentEvent::ApprovalRequested(request)] = events.as_slice() else {
            panic!("expected one approval request, got {events:?}");
        };
        assert_eq!(
            request.reason.as_deref(),
            Some(
                "Dangerous rm operation detected in `rm -rf $F/*`. The target '$F/*' is a shell variable expansion: when $F is unset or empty it becomes `/`, `/*` or a top-level path. This requires explicit approval and cannot be auto-allowed by permission rules."
            )
        );
        assert_eq!(request.blocked_path, None);
        assert_eq!(
            request.kind,
            ApprovalKind::ExecCommand {
                command: "rm -rf $F/*".into(),
                cwd: None,
                description: Some("Clear the scratch directory".into()),
            }
        );
    }

    #[test]
    fn approval_details_reach_native_permission_requests() {
        let mut m = Mapper::new();
        m.start_turn();
        for (tool, input, expected) in [
            (
                "Task",
                json!({"subagent_type":"explore", "description":"find refs", "prompt":"ignored"}),
                Some("explore: find refs"),
            ),
            (
                "Task",
                json!({"prompt":"find references"}),
                Some("find references"),
            ),
            ("Weird", json!({"a":1}), Some("Weird: {\"a\":1}")),
            (
                "Read",
                json!({"file_path":"/tmp/a.txt"}),
                Some("Read: {\"file_path\":\"/tmp/a.txt\"}"),
            ),
            ("Task", json!({"prompt":"y".repeat(300)}), None),
            ("Weird", json!({"blob":"z".repeat(500)}), None),
        ] {
            let events = m.on_message(json!({
                "type": "control_request",
                "request_id": "detail-request",
                "request": {"subtype":"can_use_tool", "tool_name":tool, "input":input},
            }));
            let [AgentEvent::ApprovalRequested(request)] = events.as_slice() else {
                panic!("expected native permission request, got {events:?}");
            };
            let detail = match &request.kind {
                ApprovalKind::ToolUse {
                    name,
                    input: actual_input,
                    detail,
                } => {
                    assert_eq!(name, tool);
                    assert_eq!(actual_input, &input);
                    detail
                }
                ApprovalKind::FileRead { detail } if tool == "Read" => detail,
                other => panic!("unexpected approval for {tool}: {other:?}"),
            };
            if let Some(expected) = expected {
                assert_eq!(detail, expected);
            } else if tool == "Task" {
                let prompt = input["prompt"].as_str().unwrap();
                assert!(!detail.is_empty());
                assert!(prompt.starts_with(detail.as_str()));
                assert!(
                    detail.len() < prompt.len(),
                    "long task prompts must remain bounded"
                );
            } else {
                assert!(detail.starts_with("Weird: {\"blob\":\""));
                assert!(detail.ends_with("..."));
                assert!(detail.len() < input["blob"].as_str().unwrap().len());
            }
        }
    }

    /// Tools that write no file and run no command ask as themselves, under
    /// their own name and input: Claude Code 2.1.322's task tools (inputs from
    /// their schemas in the bundle) and Tcode's control MCP tools as Claude
    /// Code names them (inputs from `crates/control-mcp/src/params.rs`).
    #[test]
    fn a_tool_that_changes_no_file_asks_as_itself() {
        let mut m = Mapper::new();
        m.start_turn();
        for (tool, input) in [
            (
                "TodoWrite",
                json!({"todos":[{"content":"Wire the parser","status":"pending","activeForm":"Wiring the parser"}]}),
            ),
            (
                "TaskCreate",
                json!({"subject":"Wire the parser","description":"Map the new frames"}),
            ),
            (
                "mcp__tcode_control__create_project",
                json!({"root":"/work/parser"}),
            ),
            (
                "mcp__tcode_control__read_terminal",
                json!({"terminal_id":3}),
            ),
        ] {
            let events = m.on_message(json!({
                "type": "control_request",
                "request_id": "ask-as-itself",
                "request": {"subtype":"can_use_tool", "tool_name":tool, "input":input, "tool_use_id":"toolu_ask"},
            }));
            let [AgentEvent::ApprovalRequested(request)] = events.as_slice() else {
                panic!("expected one approval request for {tool}, got {events:?}");
            };
            assert!(
                matches!(&request.kind, ApprovalKind::ToolUse { name, input: asked, .. }
                    if name == tool && *asked == input),
                "{tool}: {:?}",
                request.kind
            );
        }
    }

    #[test]
    fn exit_plan_mode_shows_its_plan_and_takes_the_decision_natively() {
        let plan = "# Plan\n\n1. Write hello.txt";
        let input = json!({"plan": plan, "planFilePath": "/plans/p.md"});
        for (choice, expected) in [
            (
                ApprovalDecision::Option("allow".into()),
                json!({"behavior": "allow", "updatedInput": input}),
            ),
            (
                ApprovalDecision::Option("deny".into()),
                json!({"behavior": "deny", "message": "User declined tool execution."}),
            ),
            (
                ApprovalDecision::Feedback("Use Python instead".into()),
                json!({"behavior": "deny", "message": "Use Python instead"}),
            ),
        ] {
            let mut mapper = Mapper::new();
            let started = mapper.on_message(json!({"type":"assistant", "message": {"id":"msg_plan",
                "content":[{"type":"tool_use", "id":"toolu_plan", "name":"ExitPlanMode", "input":input}]}}));
            assert!(
                started.iter().any(|event| matches!(event, AgentEvent::ItemStarted(ThreadItem { id, content: ItemContent::Plan { markdown }, .. })
                    if id == "toolu_plan" && markdown == plan)),
                "{started:?}"
            );
            let events = mapper.on_message(json!({"type":"control_request", "request_id":"plan-request",
                "request": {"subtype":"can_use_tool", "tool_name":"ExitPlanMode", "input":input, "tool_use_id":"toolu_plan"}}));
            let [AgentEvent::ApprovalRequested(request)] = events.as_slice() else {
                panic!("the plan item is already shown: {events:?}")
            };
            assert_eq!(
                request.kind,
                ApprovalKind::Plan {
                    item_id: "toolu_plan".into(),
                    markdown: plan.into()
                }
            );
            assert_eq!(
                request
                    .options
                    .iter()
                    .map(|option| (option.id.as_str(), option.label.as_str(), option.kind))
                    .collect::<Vec<_>>(),
                [
                    ("allow", "Approve", ApprovalOptionKind::AllowOnce),
                    ("deny", "Keep planning", ApprovalOptionKind::RejectOnce)
                ]
            );
            let response = mapper
                .build_approval_response("plan-request", choice)
                .unwrap();
            assert_eq!(response["response"]["response"], expected);
            let completed = mapper.on_message(json!({"type":"user", "message": {"content":[
                {"type":"tool_result", "tool_use_id":"toolu_plan", "content":"User has approved your plan."}]}}));
            assert!(
                completed.iter().any(|event| matches!(event, AgentEvent::ItemCompleted(ThreadItem { id, content: ItemContent::Plan { markdown }, .. })
                    if id == "toolu_plan" && markdown == plan)),
                "{completed:?}"
            );
        }

        let mut mapper = Mapper::new();
        let events = mapper.on_message(json!({"type":"control_request", "request_id":"early",
            "request": {"subtype":"can_use_tool", "tool_name":"ExitPlanMode", "input":{"plan": plan}, "tool_use_id":"toolu_early"}}));
        assert!(
            matches!(events.as_slice(), [
                AgentEvent::ItemStarted(ThreadItem { id, content: ItemContent::Plan { markdown }, .. }),
                AgentEvent::ApprovalRequested(ApprovalRequest { kind: ApprovalKind::Plan { item_id, .. }, .. }),
            ] if id == "toolu_early" && item_id == id && markdown == plan),
            "a prompt that arrives before its tool block still shows the plan it decides: {events:?}"
        );
    }

    /// `control_cancel_request {request_id}` as Claude Code 2.1.322's SDK
    /// schema describes it: the sender no longer needs the answer to its own
    /// request, "for example a pending can_use_tool prompt after the turn was
    /// interrupted". Its `sendRequest` writes one when the turn's abort signal
    /// fires.
    #[test]
    fn prompts_claude_code_withdraws_are_reported_resolved() {
        let mut m = Mapper::new();
        m.start_turn();
        feed(
            &mut m,
            r#"{"type":"control_request","request_id":"ctrl-bash","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"cargo test"},"tool_use_id":"toolu_bash"}}"#,
        );
        feed(
            &mut m,
            r#"{"type":"control_request","request_id":"ctrl-ask","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[{"question":"Which DB?","header":"DB"}]},"tool_use_id":"toolu_ask"}}"#,
        );
        let events = feed(
            &mut m,
            r#"{"type":"control_cancel_request","request_id":"ctrl-bash"}"#,
        );
        assert!(
            matches!(events.as_slice(), [AgentEvent::ApprovalResolved { request_id, decision: ApprovalDecision::Cancel }] if request_id == "ctrl-bash"),
            "{events:?}"
        );
        let events = feed(
            &mut m,
            r#"{"type":"control_cancel_request","request_id":"ctrl-ask"}"#,
        );
        assert!(
            matches!(events.as_slice(), [AgentEvent::UserInputResolved { request_id, answers }] if request_id == "ctrl-ask" && answers.is_empty()),
            "{events:?}"
        );
        assert!(
            m.build_approval_response("ctrl-bash", ApprovalDecision::Option("allow".into()))
                .is_none()
        );
        assert!(
            feed(
                &mut m,
                r#"{"type":"control_cancel_request","request_id":"ctrl-bash"}"#
            )
            .is_empty()
        );
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
                ..
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
    fn result_maps_to_turn_completed_with_usage() {
        let mut m = Mapper::new();
        let turn_id = m.start_turn();
        let evs = feed(
            &mut m,
            r#"{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.125,"duration_ms":4321,"usage":{"input_tokens":100,"cache_read_input_tokens":50,"cache_creation_input_tokens":10,"output_tokens":20},"modelUsage":{"claude-opus-4-8[1m]":{"contextWindow":1000000}}}"#,
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
                assert_eq!(usage.input_tokens, None);
                assert_eq!(usage.cached_input_tokens, None);
                assert_eq!(usage.output_tokens, None);
                assert_eq!(usage.used_tokens, None);
                assert_eq!(usage.turn_processed_tokens, Some(180));
                assert_eq!(usage.total_processed_tokens, None);
                assert_eq!(usage.context_window, Some(1_000_000));
                assert_eq!(usage.cost_usd, Some(0.125));
                assert_eq!(usage.duration_ms, Some(4321));
            }
            other => panic!("expected TurnCompleted, got {other:?}"),
        }
        // Each result is that turn's traffic, however long the process ran.
        m.start_turn();
        let evs = feed(
            &mut m,
            r#"{"type":"result","subtype":"success","usage":{"input_tokens":30,"output_tokens":5}}"#,
        );
        assert!(matches!(
            turn_completed_after_count(&evs, 0),
            AgentEvent::TurnCompleted { usage: Some(usage), .. }
                if usage.turn_processed_tokens == Some(35) && usage.total_processed_tokens.is_none()
        ));
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
    }

    #[test]
    fn full_fixture_trace_parses() {
        let trace = include_str!("../tests/fixtures/claude/tool_use_trace.jsonl");
        let mut m = Mapper::new();
        let mut all = Vec::new();
        let mut turn_id = None;
        let mut approvals = 0;
        for line in trace.lines().filter(|line| !line.trim().is_empty()) {
            let msg: Value = serde_json::from_str(line).expect("fixture line is json");
            for event in m.on_message(msg) {
                if let AgentEvent::TurnStarted { turn_id: started } = &event {
                    turn_id = Some(started.clone());
                }
                if let AgentEvent::ApprovalRequested(request) = &event {
                    approvals += 1;
                    assert_eq!(request.id, "00000000-0000-4000-8000-000000000047");
                    assert!(turn_id.is_some());
                    assert_eq!(request.turn_id, turn_id);
                    let ApprovalKind::FileChange { changes } = &request.kind else {
                        panic!("expected Write approval, got {request:?}");
                    };
                    assert_eq!(changes.len(), 1);
                    assert_eq!(changes[0].path, "/home/user/project/hello.txt");
                    assert_eq!(
                        request.reason, None,
                        "the frame says what the tool does, not why it asks"
                    );
                    let response = m
                        .build_approval_response(
                            &request.id,
                            ApprovalDecision::Option("allow".into()),
                        )
                        .expect("response to the recorded request");
                    assert_eq!(
                        response,
                        json!({
                            "type": "control_response",
                            "response": {
                                "subtype": "success",
                                "request_id": "00000000-0000-4000-8000-000000000047",
                                "response": {
                                    "behavior": "allow",
                                    "updatedInput": {
                                        "file_path": "/home/user/project/hello.txt",
                                        "content": "hi\n",
                                    },
                                },
                            },
                        })
                    );
                    assert!(
                        m.build_approval_response(
                            &request.id,
                            ApprovalDecision::Option("allow".into())
                        )
                        .is_none()
                    );
                }
                all.push(event);
            }
        }
        assert!(
            all.iter()
                .any(|event| matches!(event, AgentEvent::SessionStarted { .. })),
            "expected SessionStarted"
        );
        assert_eq!(approvals, 1);

        // Empty structuredPatch in the recorded result must preserve the Write input's diff.
        let changes: Vec<_> = all
            .iter()
            .filter_map(|event| {
                let (phase, item) = match event {
                    AgentEvent::ItemStarted(item) => ("started", item),
                    AgentEvent::ItemCompleted(item) => ("completed", item),
                    _ => return None,
                };
                let ItemContent::FileChange { changes, status } = &item.content else {
                    return None;
                };
                Some((phase, item.id.as_str(), *status, changes.as_slice()))
            })
            .collect();
        let expected = [FileChange {
            path: "/home/user/project/hello.txt".into(),
            kind: FileChangeKind::Create,
            diff: Some("+hi".into()),
        }];
        assert_eq!(
            changes,
            vec![
                (
                    "started",
                    "toolu_010000000000000000000019",
                    ItemStatus::InProgress,
                    expected.as_slice()
                ),
                (
                    "completed",
                    "toolu_010000000000000000000019",
                    ItemStatus::Completed,
                    expected.as_slice()
                ),
            ]
        );
        assert!(
            all.iter().any(|event| matches!(
                event,
                AgentEvent::TurnCompleted {
                    status: TurnStatus::Completed,
                    ..
                }
            )),
            "expected completed TurnCompleted"
        );
    }

    /// The snapshots of the Subagent item `id` in `events`.
    fn subagent_snapshots<'a>(events: &'a [AgentEvent], id: &str) -> Vec<&'a ThreadItem> {
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ItemStarted(item)
                | AgentEvent::ItemUpdated(item)
                | AgentEvent::ItemCompleted(item)
                    if item.id == id && matches!(item.content, ItemContent::Subagent { .. }) =>
                {
                    Some(item)
                }
                _ => None,
            })
            .collect()
    }

    /// A foreground subagent as Claude Code recorded it: the lead waits on
    /// it, its activity is parented to the spawn, the spawn shows the model
    /// the child ran with, and no terminal snapshot reaches the thread before
    /// the transcript tail's last flush.
    #[test]
    fn subagent_fixture_maps_lifecycle_and_parented_activity() {
        const SPAWN: &str = "toolu_010000000000000000000007";
        let trace = include_str!("../tests/fixtures/claude/subagent_handback_trace.jsonl");
        let mut mapper = Mapper::new();
        let mut events = Vec::new();
        for line in trace.lines() {
            events.extend(feed(&mut mapper, line));
        }
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::ItemBackgrounded { .. }))
        );
        assert!(
            subagent_snapshots(&events, SPAWN)
                .iter()
                .all(|item| matches!(
                    item.content,
                    ItemContent::Subagent {
                        status: ItemStatus::InProgress,
                        ..
                    }
                )),
            "{events:?}"
        );
        let children: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ItemStarted(item) | AgentEvent::ItemCompleted(item)
                    if item.parent_item_id.as_deref() == Some(SPAWN) =>
                {
                    Some(&item.content)
                }
                _ => None,
            })
            .collect();
        assert!(
            children.iter().any(|content| matches!(content,
                ItemContent::UserMessage { text, .. } if text.starts_with("Your report is two lines"))),
            "{children:?}"
        );
        assert!(
            children.iter().any(|content| matches!(content,
                ItemContent::ToolCall { name, status: ItemStatus::Completed, .. } if name == "SubagentHandback")),
            "{children:?}"
        );

        let flushed = mapper.on_tail_notice(SubagentTailNotice {
            parent_id: SPAWN.into(),
            events: Vec::new(),
            model: None,
            stopped: true,
        });
        assert!(
            matches!(
                subagent_snapshots(&flushed, SPAWN).last(),
                Some(ThreadItem { parent_item_id: None, content: ItemContent::Subagent { status: ItemStatus::Completed, model: Some(model), .. }, .. })
                    if model == "claude-sonnet-5-5"
            ),
            "{flushed:?}"
        );
        assert!(mapper.take_pending_subagent_terminals().is_empty());
    }

    /// A background subagent as Claude Code recorded it: the launch
    /// acknowledgement returns the call and leaves the Subagent item in
    /// progress; it settles only with the transcript tail's final flush, and
    /// the tail's copy of a child the stdout feed already completed cannot
    /// reopen it.
    #[test]
    fn background_subagent_terminal_waits_for_tail_flush_and_dedupes_feeds() {
        const SPAWN: &str = "toolu_010000000000000000000002";
        let trace =
            include_str!("../tests/fixtures/claude/subagent_handback_background_trace.jsonl");
        let mut mapper = Mapper::new();
        let mut events = Vec::new();
        for line in trace.lines() {
            events.extend(feed(&mut mapper, line));
        }
        assert!(
            subagent_snapshots(&events, SPAWN)
                .iter()
                .all(|item| matches!(
                    item.content,
                    ItemContent::Subagent {
                        status: ItemStatus::InProgress,
                        ..
                    }
                )),
            "launch acknowledgement and task_notification must not settle the item yet: {events:?}"
        );
        assert!(events.iter().any(|event| matches!(event,
            AgentEvent::ItemBackgrounded { item_id, parent_item_id: None } if item_id == SPAWN)));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ItemCompleted(ThreadItem { id, content: ItemContent::ToolCall { status: ItemStatus::Completed, .. }, .. })
                if *id == format!("{SPAWN}:toolu_010000000000000000000003")
        )));

        // The tail maps the same child records a second time.
        let mut tail = crate::subagent_tail::TranscriptMapper::new(SPAWN);
        let child = format!("\"parent_tool_use_id\":\"{SPAWN}\"");
        let mut tail_events = Vec::new();
        for line in trace.lines().filter(|line| line.contains(&child)) {
            tail_events.extend(tail.map_value(&serde_json::from_str(line).unwrap()));
        }
        assert!(!tail_events.is_empty());
        let flushed = mapper.on_tail_notice(SubagentTailNotice {
            parent_id: SPAWN.into(),
            events: tail_events,
            model: None,
            stopped: true,
        });
        assert!(
            flushed
                .iter()
                .all(|event| !matches!(event, AgentEvent::ItemStarted(_))),
            "a completed child must not be restarted by the tail's copy: {flushed:?}"
        );
        assert!(
            matches!(
                flushed.last(),
                Some(AgentEvent::ItemUpdated(ThreadItem { id, content: ItemContent::Subagent { status: ItemStatus::Completed, .. }, .. }))
                    if id == SPAWN
            ),
            "{flushed:?}"
        );
        assert!(mapper.take_pending_subagent_terminals().is_empty());
    }

    #[test]
    fn resumed_subagent_keeps_its_spawn_identity_across_parent_restart() {
        smol::block_on(async {
            let root =
                std::env::temp_dir().join(format!("tcode-resumed-agent-{}", uuid::Uuid::new_v4()));
            let dir = root.join("projects/project/session/subagents");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("agent-child.meta.json"),
                r#"{"agentType":"general-purpose","description":"Audit routing","toolUseId":"spawn","spawnDepth":1,"requestShape":"background","requestNonInteractive":true}"#
            ).unwrap();
            for restart in [true, false] {
                let mut mapper = Mapper::new();
                if !restart {
                    feed(
                        &mut mapper,
                        r#"{"type":"assistant","message":{"id":"initial","content":[{"type":"tool_use","id":"spawn","name":"Agent","input":{"description":"Audit routing","subagent_type":"general-purpose"}}]}}"#,
                    );
                    feed(
                        &mut mapper,
                        r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"spawn","content":"Launched"}]},"tool_use_result":{"status":"async_launched","agentId":"child"}}"#,
                    );
                    feed(
                        &mut mapper,
                        r#"{"type":"system","subtype":"task_notification","task_id":"child","tool_use_id":"spawn","status":"completed"}"#,
                    );
                }
                let mut events = Vec::new();
                for msg in [
                    json!({"type":"assistant","message":{"id":"resume-message","content":[{"type":"tool_use","id":"send","name":"SendMessage","input":{"to":"child","message":"Continue the audit"}}]}}),
                    json!({"type":"user","session_id":"session","message":{"content":[{"type":"tool_result","tool_use_id":"send","content":[{"type":"text","text":"{\"success\":true,\"resumedAgentId\":\"child\"}"}]}]},"tool_use_result":{"success":true,"resumedAgentId":"child"}}),
                ] {
                    events.extend(
                        mapper
                            .on_wire_message(msg, restart.then_some(root.as_path()))
                            .await,
                    );
                }
                events.extend(mapper.on_wire_message(json!({"type":"system","subtype":"task_notification","session_id":"session","task_id":"child","tool_use_id":"send","status":"completed","summary":"Audit complete"}), restart.then_some(root.as_path())).await);

                assert!(events.iter().any(|event| matches!(event,
                    AgentEvent::ItemUpdated(ThreadItem { id, content: ItemContent::Subagent {status: ItemStatus::Completed, ..}, .. }) if id == "spawn"
                )), "completion must settle the original mirror (restart={restart})");
                assert!(events.iter().any(|event| matches!(event,
                    AgentEvent::ItemUpdated(ThreadItem { id, content: ItemContent::Subagent {status: ItemStatus::InProgress, ..}, .. }) if id == "spawn"
                )), "resuming must reopen the original mirror (restart={restart})");
                assert!(events.iter().any(|event| matches!(event,
                    AgentEvent::ItemCompleted(ThreadItem { id, content: ItemContent::ToolCall {status: ItemStatus::Completed, ..}, .. }) if id == "send"
                )), "SendMessage must retain its own tool result");
            }
            std::fs::remove_dir_all(root).unwrap();
        });
    }

    /// A background subagent's own background Agent call is that subagent's.
    /// Claude Code 2.1.319 reports the nested task on the lead's stream with
    /// the launching subagent's task as `parent_task_id`, before the
    /// subagent's transcript tail shows the call; the lead's Agents view
    /// lists only Subagent items without a parent, so the nested one must
    /// never be one, whether or not its metadata file is readable yet.
    #[test]
    fn a_subagents_background_agent_stays_with_that_subagent() {
        smol::block_on(async {
            let root =
                std::env::temp_dir().join(format!("tcode-nested-bg-{}", uuid::Uuid::new_v4()));
            let dir = root.join("projects/project/session/subagents");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("agent-a-child.meta.json"),
                r#"{"agentType":"general-purpose","description":"Check the UI","toolUseId":"toolu_child","spawnDepth":1,"requestShape":"background","requestNonInteractive":true}"#
            ).unwrap();
            std::fs::write(dir.join("agent-a-nested.meta.json"),
                r#"{"agentType":"general-purpose","description":"Visual check: headless screenshots","toolUseId":"toolu_nested","parentAgentId":"a-child","spawnDepth":2,"requestShape":"background","requestNonInteractive":true}"#
            ).unwrap();
            for metadata in [Some(root.as_path()), None] {
                let mut mapper = Mapper::new();
                let mut events = Vec::new();
                for line in [
                    r#"{"type":"assistant","session_id":"session","message":{"id":"msg-spawn","content":[{"type":"tool_use","id":"toolu_child","name":"Agent","input":{"description":"Check the UI","prompt":"Check the UI.","subagent_type":"general-purpose","run_in_background":true}}]}}"#,
                    r#"{"type":"system","subtype":"task_started","session_id":"session","task_id":"a-child","tool_use_id":"toolu_child","description":"Check the UI","subagent_type":"general-purpose","is_backgrounded":true,"spawn_depth":1,"task_type":"local_agent","prompt":"Check the UI."}"#,
                    r#"{"type":"user","session_id":"session","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_child","content":"Async agent launched successfully.\nagentId: a-child"}]},"tool_use_result":{"isAsync":true,"status":"async_launched","agentId":"a-child"}}"#,
                    r#"{"type":"system","subtype":"task_started","session_id":"session","task_id":"a-nested","tool_use_id":"toolu_nested","description":"Visual check: headless screenshots","subagent_type":"general-purpose","is_backgrounded":true,"spawn_depth":2,"parent_task_id":"a-child","task_type":"local_agent","prompt":"Take headless screenshots."}"#,
                    r#"{"type":"system","subtype":"background_tasks_changed","session_id":"session","tasks":[{"task_id":"a-child","task_type":"local_agent","subagent_type":"general-purpose","description":"Check the UI"},{"task_id":"a-nested","task_type":"local_agent","subagent_type":"general-purpose","description":"Visual check: headless screenshots","parent_task_id":"a-child"}]}"#,
                ] {
                    let msg = serde_json::from_str(line).unwrap();
                    events.extend(mapper.on_wire_message(msg, metadata).await);
                }
                // The child's transcript shows the call and its launch only now.
                let mut child = crate::subagent_tail::TranscriptMapper::new("toolu_child");
                let mut transcript = Vec::new();
                for line in [
                    r#"{"type":"assistant","message":{"id":"msg-child","content":[{"type":"tool_use","id":"toolu_nested","name":"Agent","input":{"description":"Visual check: headless screenshots","prompt":"Take headless screenshots.","subagent_type":"general-purpose","run_in_background":true}}]}}"#,
                    r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_nested","content":[{"type":"text","text":"Async agent launched successfully.\nagentId: a-nested"}]}]}}"#,
                ] {
                    transcript.extend(child.map_value(&serde_json::from_str(line).unwrap()));
                }
                events.extend(mapper.on_tail_notice(SubagentTailNotice {
                    parent_id: "toolu_child".into(),
                    events: transcript,
                    model: None,
                    stopped: false,
                }));
                events.extend(mapper.on_wire_message(serde_json::from_str(
                    r#"{"type":"system","subtype":"task_notification","session_id":"session","task_id":"a-nested","tool_use_id":"toolu_nested","status":"completed","output_file":"","summary":"Screenshots taken"}"#,
                ).unwrap(), metadata).await);
                events.extend(mapper.on_tail_notice(SubagentTailNotice {
                    parent_id: "toolu_nested".into(),
                    events: Vec::new(),
                    model: None,
                    stopped: true,
                }));

                let snapshots: Vec<_> = events
                    .iter()
                    .filter_map(|event| match event {
                        AgentEvent::ItemStarted(item)
                        | AgentEvent::ItemUpdated(item)
                        | AgentEvent::ItemCompleted(item)
                            if matches!(item.content, ItemContent::Subagent { .. }) =>
                        {
                            Some(item)
                        }
                        _ => None,
                    })
                    .collect();
                let leads: Vec<_> = snapshots
                    .iter()
                    .filter(|item| item.parent_item_id.is_none())
                    .map(|item| item.id.as_str())
                    .collect();
                assert!(
                    leads.iter().all(|id| *id == "toolu_child"),
                    "only the lead's own call is the lead's agent (metadata={metadata:?}): {snapshots:?}"
                );
                let nested: Vec<_> = snapshots
                    .iter()
                    .filter(|item| item.id == "toolu_nested")
                    .collect();
                assert!(
                    matches!(
                        nested.first(),
                        Some(ThreadItem { parent_item_id: Some(parent), content: ItemContent::Subagent { description, status: ItemStatus::InProgress, .. }, .. })
                            if parent == "toolu_child" && description == "Visual check: headless screenshots"
                    ),
                    "the nested agent is the subagent's from its task on (metadata={metadata:?}): {nested:?}"
                );
                assert!(
                    matches!(
                        nested.last(),
                        Some(ThreadItem { content: ItemContent::Subagent { status: ItemStatus::Completed, summary: Some(summary), .. }, .. })
                            if summary == "Screenshots taken"
                    ),
                    "{nested:?}"
                );
                assert!(
                    events.iter().any(|event| matches!(
                        event,
                        AgentEvent::ItemBackgrounded { item_id, parent_item_id: Some(parent) }
                            if item_id == "toolu_nested" && parent == "toolu_child"
                    )),
                    "the launch returns the subagent's call: {events:?}"
                );
            }
            std::fs::remove_dir_all(root).unwrap();
        });
    }

    /// A nested Agent that runs in the foreground settles with the child's
    /// tool_result; a coordination tool is not a spawn.
    #[test]
    fn nested_foreground_subagent_completes_on_the_childs_tool_result() {
        let mut mapper = Mapper::new();
        let mut events = feed(
            &mut mapper,
            r#"{"type":"assistant","message":{"id":"msg-spawn","content":[{"type":"tool_use","id":"toolu_child","name":"Agent","input":{"description":"Audit","prompt":"Audit routing.","subagent_type":"Explore"}}]}}"#,
        );
        events.extend(feed(
            &mut mapper,
            r#"{"type":"assistant","parent_tool_use_id":"toolu_child","message":{"id":"msg-child-1","content":[{"type":"tool_use","id":"toolu_list","name":"ListAgents","input":{}},{"type":"tool_use","id":"toolu_nested","name":"Agent","input":{"description":"Check tests","prompt":"Check the tests.","subagent_type":"Explore"}}]}}"#,
        ));
        events.extend(feed(
            &mut mapper,
            r#"{"type":"user","parent_tool_use_id":"toolu_child","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_list","content":"none"},{"type":"tool_result","tool_use_id":"toolu_nested","content":[{"type":"text","text":"Tests   are\ngreen."}]}]}}"#,
        ));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ItemStarted(ThreadItem { id, parent_item_id: Some(parent), content: ItemContent::ToolCall { name, .. }, .. })
                if id == "toolu_child:toolu_list" && parent == "toolu_child" && name == "ListAgents"
        )));
        assert!(
            matches!(
                events.last(),
                Some(AgentEvent::ItemCompleted(ThreadItem { id, parent_item_id: Some(parent), content: ItemContent::Subagent { agent_type, status: ItemStatus::Completed, summary: Some(summary), .. }, .. }))
                    if id == "toolu_nested" && parent == "toolu_child" && agent_type == "Explore" && summary == "Tests are green."
            ),
            "{events:?}"
        );
        assert!(mapper.take_pending_subagent_terminals().is_empty());
    }

    /// Claude Code 2.1.322 builds a `task_notification` with
    /// `output_file: outputFile ?? ""`: an empty string is no file, and a
    /// tail told to prefer it would read nothing and stop looking for the
    /// subagent's transcript.
    #[test]
    fn a_notification_without_an_output_file_names_no_path_to_tail() {
        let mut mapper = Mapper::new();
        feed(
            &mut mapper,
            r#"{"type":"system","subtype":"task_notification","session_id":"session","task_id":"a-child","tool_use_id":"toolu_child","status":"completed","output_file":"","summary":"Done"}"#,
        );
        let requests = mapper.take_tail_requests();
        assert!(
            !requests
                .iter()
                .any(|request| matches!(request, TailRequest::PreferPath { .. }))
        );
        assert!(matches!(
            &requests[..],
            [TailRequest::Stop { parent_id }] if parent_id == "toolu_child"
        ));
    }

    /// A recorded stdout trace driven as the actor drives it: every line, then
    /// the acknowledgement of each tail the trace stopped.
    fn replay_subagent_trace<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<AgentEvent> {
        let mut mapper = Mapper::new();
        let mut events = Vec::new();
        for line in lines {
            events.extend(feed(&mut mapper, line));
        }
        for request in mapper.take_tail_requests() {
            if let TailRequest::Stop { parent_id } = request {
                events.extend(mapper.on_tail_notice(SubagentTailNotice {
                    parent_id,
                    events: Vec::new(),
                    model: None,
                    stopped: true,
                }));
            }
        }
        assert!(mapper.take_pending_subagent_terminals().is_empty());
        events
    }

    /// The summary a thread shows for `id` once it has settled: a snapshot
    /// without one leaves the last one standing.
    #[track_caller]
    fn settled_summary(events: &[AgentEvent], id: &str) -> Option<String> {
        let mut shown = None;
        let mut settled = false;
        for event in events {
            if let AgentEvent::ItemUpdated(item) | AgentEvent::ItemCompleted(item) = event
                && item.id == id
                && let ItemContent::Subagent {
                    status, summary, ..
                } = &item.content
            {
                shown = summary.clone().or(shown);
                settled |= *status == ItemStatus::Completed;
            }
        }
        assert!(settled, "{id} never settled: {events:?}");
        shown
    }

    #[track_caller]
    fn assert_no_handback_note(events: &[AgentEvent]) {
        for event in events {
            if let AgentEvent::ItemUpdated(item) | AgentEvent::ItemCompleted(item) = event
                && let ItemContent::Subagent {
                    summary: Some(summary),
                    ..
                } = &item.content
            {
                assert!(!is_handback_note(summary), "{event:?}");
            }
        }
    }

    #[test]
    fn a_foreground_subagent_that_hands_back_shows_its_report_whole() {
        let trace = include_str!("../tests/fixtures/claude/subagent_handback_trace.jsonl");
        let events = replay_subagent_trace(trace.lines());
        assert_eq!(
            settled_summary(&events, "toolu_010000000000000000000007").as_deref(),
            Some("PONG-FG-REPORT\nsecond-line")
        );
        assert_no_handback_note(&events);
    }

    #[test]
    fn a_background_subagent_that_hands_back_shows_its_report() {
        let trace =
            include_str!("../tests/fixtures/claude/subagent_handback_background_trace.jsonl");
        let events = replay_subagent_trace(trace.lines());
        assert_eq!(
            settled_summary(&events, "toolu_010000000000000000000002").as_deref(),
            Some("PONG-BG-REPORT")
        );
        assert_no_handback_note(&events);
    }

    /// The tool_result of a nested Agent carries only the note, so the
    /// nested agent's report is the one its task_notification brought.
    #[test]
    fn a_nested_subagent_that_hands_back_shows_its_report() {
        let trace = include_str!("../tests/fixtures/claude/subagent_handback_nested_trace.jsonl");
        let events = replay_subagent_trace(trace.lines());
        assert_eq!(
            settled_summary(&events, "toolu_010000000000000000000005").as_deref(),
            Some("PONG-INNER-REPORT")
        );
        assert_eq!(
            settled_summary(&events, "toolu_010000000000000000000004").as_deref(),
            Some("PONG-OUTER-REPORT")
        );
        assert_no_handback_note(&events);
    }

    #[test]
    fn a_subagent_that_delivered_no_report_has_no_summary() {
        let trace = include_str!("../tests/fixtures/claude/subagent_withheld_trace.jsonl");
        let events = replay_subagent_trace(trace.lines());
        assert_eq!(
            settled_summary(&events, "toolu_010000000000000000000013"),
            None
        );
        assert_no_handback_note(&events);
    }

    #[test]
    fn a_nested_subagent_that_delivered_no_report_has_no_summary() {
        let trace = include_str!("../tests/fixtures/claude/subagent_withheld_nested_trace.jsonl");
        let events = replay_subagent_trace(trace.lines());
        assert_eq!(
            settled_summary(&events, "toolu_010000000000000000000011"),
            None
        );
        assert_eq!(
            settled_summary(&events, "toolu_010000000000000000000010").as_deref(),
            Some("OUTER-REPORT")
        );
        assert_no_handback_note(&events);
    }

    /// Outside auto mode a subagent has no `SubagentHandback`: it ends with
    /// text, and the task_notification's `summary` is that text.
    #[test]
    fn a_subagent_that_ends_with_text_keeps_it() {
        let trace = include_str!("../tests/fixtures/claude/subagent_text_end_trace.jsonl");
        let events = replay_subagent_trace(trace.lines());
        assert_eq!(
            settled_summary(&events, "toolu_010000000000000000000009").as_deref(),
            Some("PONG-FG-REPORT")
        );
    }

    /// No recording: auto mode has to block the report. The fields are those
    /// of the CLI's own schema, which has a client show `warning` above `text`.
    #[test]
    fn a_flagged_report_keeps_its_warning_above_it() {
        let trace = include_str!("../tests/fixtures/claude/subagent_handback_trace.jsonl")
            .replace("\"handback\":\"send\"", "\"handback\":\"flagged\"")
            .replace(
                "{\"text\":\"PONG-FG-REPORT",
                "{\"warning\":\"SECURITY WARNING\",\"text\":\"PONG-FG-REPORT",
            );
        let events = replay_subagent_trace(trace.lines());
        assert_eq!(
            settled_summary(&events, "toolu_010000000000000000000007").as_deref(),
            Some("SECURITY WARNING\n\nPONG-FG-REPORT\nsecond-line")
        );
    }

    /// The tail keeps the transcript discovery found; the notification's
    /// `output_file` only seeds a reader when discovery found nothing.
    #[test]
    fn subagent_tail_prefer_path_does_not_replay_a_discovered_transcript() {
        let root = std::env::temp_dir().join(format!(
            "tcode-subagent-tail-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let subagents = root.join("projects/session-tail/subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        let transcript = "{\"type\":\"user\",\"message\":{\"content\":\"go\"}}\n{\"type\":\"assistant\",\"message\":{\"id\":\"m1\",\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]}}\n";
        std::fs::write(
            subagents.join("agent-x.meta.json"),
            r#"{"toolUseId":"toolu_tail"}"#,
        )
        .unwrap();
        std::fs::write(subagents.join("agent-x.jsonl"), transcript).unwrap();
        let output_file = root.join("task.output");
        std::fs::write(&output_file, transcript).unwrap();

        let run = |claude_dir: Option<PathBuf>| {
            let (control_tx, control_rx) = smol::channel::unbounded();
            let (notice_tx, notice_rx) = smol::channel::unbounded();
            smol::spawn(run_subagent_tail(
                "toolu_tail".into(),
                "task-tail".into(),
                "session-tail".into(),
                claude_dir,
                control_rx,
                notice_tx,
            ))
            .detach();
            (control_tx, notice_rx)
        };
        smol::block_on(async {
            let (control, notices) = run(Some(root.clone()));
            let first = notices.recv().await.unwrap();
            assert_eq!(first.events.len(), 2);
            assert!(!first.stopped);
            control
                .send(TailControl::PreferPath(output_file.clone()))
                .await
                .unwrap();
            control.send(TailControl::Stop).await.unwrap();
            let mut after = Vec::new();
            loop {
                let notice = notices.recv().await.unwrap();
                after.extend(notice.events);
                if notice.stopped {
                    break;
                }
            }
            assert!(
                after.is_empty(),
                "preferred path must not be re-read: {after:?}"
            );

            let (control, notices) = run(None);
            control
                .send(TailControl::PreferPath(output_file.clone()))
                .await
                .unwrap();
            control.send(TailControl::Stop).await.unwrap();
            let mut fallback = Vec::new();
            loop {
                let notice = notices.recv().await.unwrap();
                fallback.extend(notice.events);
                if notice.stopped {
                    break;
                }
            }
            assert_eq!(
                fallback.len(),
                2,
                "undiscovered tail reads the notified file"
            );
        });
        let _ = std::fs::remove_dir_all(root);
    }

    /// Claude Code 2.1.322 emits a terminal `task_updated` and then the
    /// `task_notification` naming `output_file` (its task-state update emits
    /// one after the other), and the actor hands the tail each line's
    /// requests before reading the next. A tail that discovery could not
    /// point at a transcript still reads the notified file before it stops.
    #[test]
    fn an_undiscovered_subagent_tail_reads_the_notified_output_file() {
        const SPAWN: &str = "toolu_010000000000000000000002";
        const RECORDED_OUTPUT: &str = "/tmp/claude-1000/-home-user-project/00000000-0000-4000-8000-000000000002/tasks/a0000000000000001.output";
        let trace =
            include_str!("../tests/fixtures/claude/subagent_handback_background_trace.jsonl");
        let root =
            std::env::temp_dir().join(format!("tcode-notified-output-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let output_file = root.join("a0000000000000001.output");
        // The transcript holds the child's own records, which stdout also
        // forwards with `parent_tool_use_id`.
        let child = format!("\"parent_tool_use_id\":\"{SPAWN}\"");
        let transcript: String = trace
            .lines()
            .filter(|line| line.contains(&child))
            .map(|line| format!("{line}\n"))
            .collect();
        std::fs::write(&output_file, transcript).unwrap();
        // The path goes into JSON text, where a Windows separator is an escape.
        let escaped = serde_json::to_string(output_file.to_str().unwrap()).unwrap();
        let trace = trace.replace(RECORDED_OUTPUT, escaped.trim_matches('"'));

        smol::block_on(async {
            let mut mapper = Mapper::new();
            let mut tailers = HashMap::new();
            let (tail_tx, tail_rx) = smol::channel::unbounded();
            let mut tailed = Vec::new();
            for line in trace.lines() {
                feed(&mut mapper, line);
                let requests = mapper.take_tail_requests();
                let stops_a_live_tail = tailers.contains_key(SPAWN)
                    && requests
                        .iter()
                        .any(|request| matches!(request, TailRequest::Stop { .. }));
                process_tail_requests(requests, &mut tailers, None, &tail_tx).await;
                if !stops_a_live_tail {
                    continue;
                }
                loop {
                    let notice = tail_rx.recv().await.unwrap();
                    tailed.extend(notice.events.iter().cloned());
                    let stopped = notice.stopped;
                    mapper.on_tail_notice(notice);
                    if stopped {
                        tailers.remove(SPAWN);
                        break;
                    }
                }
            }
            assert!(tailers.is_empty(), "the tail was never stopped");
            assert!(
                tailed.iter().any(|event| matches!(
                    event,
                    AgentEvent::ItemCompleted(ThreadItem { id, .. })
                        if *id == format!("{SPAWN}:toolu_010000000000000000000003")
                )),
                "the tail never read {output_file:?}: {tailed:?}"
            );
        });
        let _ = std::fs::remove_dir_all(root);
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
    fn steer_acceptance_uses_request_boundaries_with_a_legacy_assistant_fallback() {
        let requesting = r#"{"type":"system","subtype":"status","status":"requesting","uuid":"checkpoint","session_id":"session"}"#;
        let assistant = r#"{"type":"assistant","message":{"model":"claude-opus-4-8","id":"msg","content":[{"type":"text","text":"response"}]},"session_id":"session"}"#;
        for current_cli in [false, true] {
            let mut mapper = Mapper::new();
            if current_cli {
                assert!(feed(&mut mapper, requesting).is_empty());
            }
            mapper
                .pending_steers
                .extend(["first".into(), "second".into()]);
            let result = feed(
                &mut mapper,
                r#"{"type":"result","subtype":"success","is_error":false,"session_id":"session"}"#,
            );
            assert!(accepted_request_ids(&result).is_empty());
            assert_eq!(mapper.pending_steers.len(), 2);
            let events = feed(&mut mapper, assistant);
            if current_cli {
                assert!(accepted_request_ids(&events).is_empty());
                assert_eq!(mapper.pending_steers.len(), 2);
                assert_eq!(
                    accepted_request_ids(&feed(&mut mapper, requesting)),
                    ["first", "second"]
                );
            } else {
                assert_eq!(accepted_request_ids(&events), ["first", "second"]);
            }
            assert!(mapper.pending_steers.is_empty());
            assert!(accepted_request_ids(&feed(&mut mapper, requesting)).is_empty());
            assert!(accepted_request_ids(&feed(&mut mapper, assistant)).is_empty());
        }
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
        let changed = feed(
            &mut mapper,
            r#"{"type":"assistant","message":{"model":"claude-opus-4-8","id":"msg-1","content":[]}}"#,
        );
        assert!(
            matches!(changed.as_slice(), [AgentEvent::ServedModel { model, reason: None }] if model == "claude-opus-4-8")
        );
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
}
