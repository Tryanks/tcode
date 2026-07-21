//! Provider-agnostic agent session layer.
//!
//! Each native provider module spawns its CLI as a child process,
//! speaks the provider-native transport, and normalizes everything
//! into the canonical [`AgentEvent`] stream. The UI only ever sees this module's
//! types; nothing provider-shaped leaks past this crate except [`ResumeCursor`],
//! which is intentionally opaque.

pub mod acp;
pub mod claude;
pub mod codex;
pub mod opencode;
pub mod pi;
mod process;
mod subagent_tail;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Codex,
    ClaudeCode,
    Pi,
    OpenCode,
    /// Any agent speaking the Agent Client Protocol. Which one is carried
    /// separately ([`SessionOptions::acp`]) so this stays `Copy`: Codex and
    /// Claude Code keep their richer native clients, and ACP covers the rest
    /// of the ecosystem.
    Acp,
}

impl ProviderKind {
    /// Whether the provider accepts [`SessionCommand::Steer`] — a message
    /// injected into a turn that is already running. Queueing works everywhere
    /// (the app holds the message until the turn ends); steering does not.
    ///
    /// ACP has no steering method at all (`session/prompt` is one request per
    /// turn; only `session/cancel` interrupts), so ACP sessions must fall back
    /// to queueing.
    pub fn supports_steering(&self) -> bool {
        match self {
            ProviderKind::ClaudeCode | ProviderKind::Codex | ProviderKind::Pi => true,
            ProviderKind::OpenCode | ProviderKind::Acp => false,
        }
    }

    /// Whether the provider can duplicate a resumed native session into a new
    /// provider-owned session rather than continuing the original in place.
    pub fn supports_fork(&self) -> bool {
        matches!(self, ProviderKind::ClaudeCode | ProviderKind::Codex)
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            ProviderKind::Codex => "Codex",
            ProviderKind::ClaudeCode => "Claude Code",
            ProviderKind::Pi => "pi",
            ProviderKind::OpenCode => "OpenCode",
            ProviderKind::Acp => "ACP agent",
        }
    }
}

/// Registry agents we never surface because they duplicate richer native
/// integrations.
pub const HIDDEN_ACP_AGENT_IDS: [&str; 4] = ["claude-acp", "codex-acp", "pi-acp", "opencode"];

/// One ACP agent a session can run: its registry identity plus how to launch it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AcpAgent {
    /// Registry id (`"gemini"`, `"cursor"`, …), or a user-chosen id for a custom agent.
    pub id: String,
    pub name: String,
    pub launch: AcpLaunch,
}

/// How to start an ACP agent process (ACP is JSON-RPC over the child's stdio).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AcpLaunch {
    /// Registry `npx` distribution: `npm exec --yes -- <package> <args…>`.
    Npx {
        package: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
    },
    /// Registry `binary` distribution, already downloaded and extracted.
    Binary {
        command: PathBuf,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
    },
    /// A user-defined command (the escape hatch for agents not in the registry).
    Custom {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
    },
}

/// Provider-shaped opaque state needed to resume a session later
/// (Codex: `{"thread_id": ...}`; Claude/pi/OpenCode use their native ids).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeCursor(pub serde_json::Value);

#[derive(Debug, Clone)]
pub struct SessionOptions {
    pub cwd: PathBuf,
    /// Provider-native model id; `None` = provider default.
    pub model: Option<String>,
    pub resume: Option<ResumeCursor>,
    /// Fork the resumed provider session instead of continuing it in place.
    /// This is meaningful only when `resume` is present.
    pub fork: bool,
    /// Override for the CLI binary; `None` = resolve from PATH.
    pub binary_path: Option<PathBuf>,
    /// How much the agent may do without asking (mirrors the three-mode
    /// model of the UI; each provider maps it onto its native knobs).
    pub approval_mode: ApprovalMode,
    /// Chosen values for the model's [`OptionDescriptor`]s (reasoning effort,
    /// context window, service tier, fast mode, thinking, …). Each provider
    /// reads the ids it understands and ignores the rest.
    pub option_selections: Vec<OptionSelection>,
    /// Build (default) vs Plan interaction mode. Codex applies this per turn via
    /// `collaborationMode`; Claude via `--permission-mode plan` / restore.
    pub interaction_mode: InteractionMode,
    /// The in-process preview MCP server to register with this session, if it
    /// came up. Each provider injects it at spawn time (Claude: `--mcp-config`;
    /// Codex: `-c mcp_servers.tcode_preview=…`) so the agent can drive the
    /// embedded preview browser. `None` = don't register any preview tooling.
    pub mcp_server: Option<McpRegistration>,
    /// The tcode orchestrator MCP server, scoped to this parent session by its
    /// bearer token. Only orchestrate-enabled sessions receive it.
    pub orchestrate_server: Option<McpRegistration>,
    /// The process-wide computer-use MCP server. The runtime supplies it only
    /// when the global computer-use setting is enabled.
    pub computer_use_server: Option<McpRegistration>,
    /// Per-provider environment (Settings → Providers): extra variables merged
    /// into the child's environment, plus the home-directory override. See
    /// [`LaunchEnv`].
    pub launch_env: LaunchEnv,
    /// Extra CLI arguments appended at spawn for providers whose settings card
    /// exposes "Launch arguments".
    pub extra_args: Vec<String>,
    /// Which ACP agent to launch. Required when `provider == ProviderKind::Acp`,
    /// ignored (and `None`) for native providers.
    pub acp: Option<AcpAgent>,
}

/// The per-provider environment configured in Settings → Providers, applied to
/// every child process we spawn for that provider (sessions, `list_models`, and
/// version/auth probes alike).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchEnv {
    /// Extra `KEY=VALUE` pairs merged into the child's environment. Later
    /// entries win, and these override anything inherited from the parent.
    pub env: Vec<(String, String)>,
    /// Home-directory override. Provider-specific: Claude gets `HOME`, Codex
    /// gets `CODEX_HOME`, and pi gets `PI_CODING_AGENT_DIR`. OpenCode has no
    /// supported single-directory override, so its value is ignored.
    pub home: Option<PathBuf>,
}

impl LaunchEnv {
    /// The complete list of variables to set on a child of `provider`: the
    /// configured pairs, plus the provider's home variable when an override is
    /// set (appended last, so it wins over an equivalent hand-written pair).
    pub fn pairs(&self, provider: ProviderKind) -> Vec<(String, String)> {
        let mut pairs = self.env.clone();
        if let Some(home) = &self.home {
            let key = match provider {
                ProviderKind::ClaudeCode => Some("HOME"),
                ProviderKind::Codex => Some("CODEX_HOME"),
                ProviderKind::Pi => Some("PI_CODING_AGENT_DIR"),
                ProviderKind::OpenCode => None,
                // ACP agents carry their own env in the launch recipe; there is
                // no protocol-level home concept to override.
                ProviderKind::Acp => None,
            };
            if let Some(key) = key {
                pairs.push((key.to_string(), home.to_string_lossy().into_owned()));
            }
        }
        pairs
    }
}

/// Connection details for the tcode preview MCP server (a streamable-HTTP
/// endpoint on loopback, guarded by a bearer token). Provider-agnostic; each
/// provider maps it onto its native MCP-server config shape.
#[derive(Debug, Clone)]
pub struct McpRegistration {
    /// MCP server name (and tool namespace).
    pub name: String,
    /// Streamable-HTTP endpoint, e.g. `http://127.0.0.1:53211/mcp`.
    pub url: String,
    /// Bearer token presented on every request (`Authorization: Bearer <token>`).
    pub bearer_token: String,
}

impl McpRegistration {
    pub const SERVER_NAME_PREVIEW: &'static str = "tcode_preview";
    pub const SERVER_NAME_ORCHESTRATE: &'static str = "tcode_orchestrate";
    pub const SERVER_NAME_COMPUTER_USE: &'static str = "tcode_computer_use";

    /// Claude Code `--mcp-config` JSON: a single `mcpServers` map entry for an
    /// HTTP server carrying the bearer token as an `Authorization` header.
    /// Verified shape for `claude` 2.1.x (`.mcp.json` `type: "http"`).
    pub fn claude_mcp_entry(&self) -> serde_json::Value {
        serde_json::json!({
                    "type": "http",
                    "url": self.url,
                    "headers": {
                        "Authorization": format!("Bearer {}", self.bearer_token),
                    }
        })
    }

    /// Codex `-c` override value: an inline TOML table for a streamable-HTTP MCP
    /// server. Codex rejects a literal `bearer_token` for HTTP, so the token
    /// rides in `http_headers.Authorization` instead (verified against
    /// codex `config/src/mcp_types.rs`). Returns the full `key=value` argument.
    pub fn codex_config_override(&self) -> String {
        // TOML basic strings; our url/token are ASCII with no quotes/backslashes.
        format!(
            "mcp_servers.{name}={{url=\"{url}\",http_headers={{Authorization=\"Bearer {token}\"}}}}",
            name = self.name,
            url = self.url,
            token = self.bearer_token,
        )
    }
}

/// Claude Code `--mcp-config` JSON containing every supplied registration.
pub fn claude_mcp_config_json<'a>(
    registrations: impl IntoIterator<Item = &'a McpRegistration>,
) -> String {
    let servers: serde_json::Map<String, serde_json::Value> = registrations
        .into_iter()
        .map(|registration| (registration.name.clone(), registration.claude_mcp_entry()))
        .collect();
    serde_json::json!({ "mcpServers": servers }).to_string()
}

/// One model a provider offers, with its selectable options (T3-style descriptors).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSpec {
    pub id: String, // provider-native id sent on the wire
    pub display_name: String,
    pub is_default: bool,
    pub options: Vec<OptionDescriptor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OptionDescriptor {
    Select {
        id: String,    // "reasoningEffort" | "contextWindow" | "serviceTier" ...
        label: String, // "Reasoning" | "Context Window" | "Service Tier"
        options: Vec<SelectOption>,
        default_value: Option<String>,
    },
    Boolean {
        id: String,    // "fastMode" | "thinking"
        label: String, // "Fast Mode" | "Thinking"
        default_value: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectOption {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

/// Resolve a provider binary to an absolute path before spawning.
///
/// Every provider spawn sets `current_dir(cwd)`, and a bare program name plus a
/// working-directory change makes PATH resolution unreliable (it fails outright
/// when PATH holds unexpanded entries such as `~/.dotnet/tools`). Resolving the
/// binary ourselves against the parent's PATH keeps the lookup deterministic.
pub(crate) fn resolve_binary(
    binary_path: Option<&std::path::Path>,
    default_name: &str,
) -> Result<PathBuf, AgentError> {
    if let Some(path) = binary_path {
        return Ok(path.to_path_buf());
    }
    // An explicit path component means "use this as given" (no PATH search).
    // Both separators are checked: Windows accepts `/` in paths too.
    if default_name.contains(['/', '\\']) {
        return Ok(PathBuf::from(default_name));
    }
    if std::env::var_os("PATH").is_none() {
        return Err(AgentError::Spawn(format!(
            "`{default_name}` not found: PATH is unset"
        )));
    }
    find_on_path(default_name).ok_or_else(|| {
        AgentError::Spawn(format!(
            "`{default_name}` not found on PATH (set its path in Settings → Providers)"
        ))
    })
}

/// Locate `name` on the parent's `PATH`, returning its absolute path.
///
/// On Windows this is `PATHEXT`-aware: the provider CLIs (and npm/pnpm/bun) are
/// installed as `claude.cmd` / `codex.exe` shims and *never* exist under their
/// bare name, so a plain `dir.join(name)` probe finds nothing. Each PATH entry
/// is tried with every `PATHEXT` extension, in `PATHEXT` order, before the bare
/// name. Elsewhere it is the classic "first executable file on PATH" search.
///
/// Shared with the app crate (`crate::process`), which routes every bare-name
/// spawn (npm, pnpm, bun, git, …) through it on Windows.
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    find_in_dirs(
        std::env::split_paths(&path_var),
        name,
        &path_extensions(),
        is_executable,
    )
}

/// The executable extensions to try for a bare name: `PATHEXT` on Windows
/// (falling back to its documented default), and nothing anywhere else.
fn path_extensions() -> Vec<String> {
    if !cfg!(windows) {
        return Vec::new();
    }
    std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
        .split(';')
        .map(str::trim)
        .filter(|ext| !ext.is_empty())
        .map(str::to_string)
        .collect()
}

/// The file names to probe for `name` in one PATH directory, in priority order.
///
/// A name that already carries an extension (`claude.cmd`, `codex.exe`) is used
/// as given. A bare name gets each `pathext` entry appended, with the
/// extensionless name tried last (so a Unix-style extensionless binary dropped
/// on a Windows PATH still resolves). With an empty `pathext` (every non-Windows
/// target) this is just `[name]`.
fn candidate_names(name: &str, pathext: &[String]) -> Vec<String> {
    if std::path::Path::new(name).extension().is_some() {
        return vec![name.to_string()];
    }
    let mut names: Vec<String> = pathext.iter().map(|ext| format!("{name}{ext}")).collect();
    names.push(name.to_string());
    names
}

/// The PATH walk itself, parameterized on the directories, the extension list
/// and the "is this executable" predicate so both platform branches are testable
/// from any host (see `resolve_binary_tests`).
fn find_in_dirs(
    dirs: impl IntoIterator<Item = PathBuf>,
    name: &str,
    pathext: &[String],
    is_exec: impl Fn(&std::path::Path) -> bool,
) -> Option<PathBuf> {
    let candidates = candidate_names(name, pathext);
    for dir in dirs {
        // Skip unexpanded/relative entries — they are meaningless to us and
        // would resolve against the child's cwd.
        if !dir.is_absolute() {
            continue;
        }
        for candidate in &candidates {
            let path = dir.join(candidate);
            if is_exec(&path) {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Windows has no exec bit: a regular file whose name matched a `PATHEXT`
/// candidate *is* the executable.
#[cfg(not(unix))]
fn is_executable(path: &std::path::Path) -> bool {
    path.is_file()
}

/// A chosen option value, persisted per session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionSelection {
    pub id: String,
    pub value: serde_json::Value,
} // string or bool

/// A structured question the agent asks the user (Claude `AskUserQuestion`,
/// Codex `item/tool/requestUserInput`). Rendered as a multiple-choice (or
/// free-text) prompt; answers ride back through [`SessionCommand::RespondUserInput`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInputQuestion {
    /// The answer key. Claude: the complete question text (the SDK indexes
    /// answers by question text). Codex: the native question id.
    pub id: String,
    pub header: String,
    pub question: String,
    pub options: Vec<UserInputOption>,
    pub multi_select: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInputOption {
    pub label: String,
    pub description: String,
}

/// An image attachment sent alongside a turn's text. `data_base64` is the raw
/// image bytes, standard-base64 encoded (no data-URL prefix); each provider maps
/// it onto its native content-block shape (Claude `image`/`base64` source; Codex
/// `image` input with a `data:` URL).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// MIME type, e.g. `image/png`.
    pub media_type: String,
    /// Standard-base64 of the raw bytes (no `data:...;base64,` prefix).
    pub data_base64: String,
    /// Local path of the persisted copy backing this attachment, when one
    /// exists. Never sent to providers (their payloads are built field by
    /// field); the runtime threads it into the recorded user-message event so
    /// the timeline can render the image without re-encoding the base64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
}

/// A provider-native command or skill surfaced to the composer's `/` and `$`
/// menus. Each native provider contributes what its command/skill discovery
/// surface exposes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCommand {
    pub name: String,
    pub description: Option<String>,
    pub kind: ProviderCommandKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCommandKind {
    /// A `/`-command (Claude slash command).
    Command,
    /// A `$`-skill (Claude skill / Codex skill).
    Skill,
}

/// Interaction mode (T3: Build/Plan).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionMode {
    #[default]
    Build,
    Plan,
}

/// Per-turn overrides layered on top of the session's persisted options.
/// Codex and OpenCode apply effort/variant per turn; Claude and pi use their
/// session-level option selection.
#[derive(Debug, Clone, Default)]
pub struct TurnOptions {
    pub effort: Option<String>,
    pub interaction_mode: Option<InteractionMode>,
}

/// List the provider's models (spawn, query, teardown). `launch_env` carries the
/// provider's configured environment/home so the catalog reflects the same CLI
/// (and account) a session would actually run against.
pub async fn list_models(
    provider: ProviderKind,
    binary_path: Option<PathBuf>,
    launch_env: LaunchEnv,
) -> Result<Vec<ModelSpec>, AgentError> {
    match provider {
        ProviderKind::Codex => codex::list_models(binary_path, launch_env).await,
        ProviderKind::ClaudeCode => claude::list_models(binary_path, launch_env).await,
        ProviderKind::Pi => pi::list_models(binary_path, launch_env).await,
        ProviderKind::OpenCode => opencode::list_models(binary_path, launch_env).await,
        // ACP agents advertise their models over the wire at session start
        // (`AgentEvent::ProviderOptions`), so there is no catalog to pre-fetch.
        ProviderKind::Acp => Ok(Vec::new()),
    }
}

/// The user-facing permission model, provider-agnostic.
///
/// Providers map this onto their native controls:
/// - Claude Code: `--permission-mode` default / acceptEdits / bypassPermissions,
///   plus tcode-side ReadOnly filtering (switchable mid-session).
/// - Codex: approval-policy × sandbox-mode combinations on thread start
///   (mid-session switch may require a resume-restart).
/// - pi: a bundled fail-closed tool-call extension in front of RPC extension UI.
/// - OpenCode: `OPENCODE_PERMISSION` rules plus permission reply endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    /// Ask before commands and file changes.
    Supervised,
    /// Read-only actions run without prompts; anything that mutates pauses for approval.
    ReadOnly,
    /// Auto-approve edits, ask before other actions.
    AutoAcceptEdits,
    /// Allow commands and edits without prompts.
    ///
    /// This is the default, mirroring T3 Code (S1 §4). Smoke mode overrides it
    /// back to `Supervised` so the approval path stays exercised.
    #[default]
    FullAccess,
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("failed to spawn provider process: {0}")]
    Spawn(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("provider reported error: {0}")]
    Provider(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Commands the UI sends into a live session's actor loop.
#[derive(Debug, Clone)]
pub enum SessionCommand {
    SendTurn {
        /// Runtime queue id echoed by [`AgentEvent::TurnAccepted`] after the
        /// adapter has crossed its provider-specific submission boundary.
        delivery_id: u64,
        text: String,
        /// Per-turn overrides (Codex/OpenCode apply effort per turn; Claude/pi
        /// use session options). `None` = use the persisted options.
        options: Option<TurnOptions>,
        /// Image attachments carried alongside the text (empty for text-only
        /// turns). Each provider maps these onto its native content blocks.
        attachments: Vec<Attachment>,
    },
    Interrupt,
    RespondApproval {
        request_id: String,
        decision: ApprovalDecision,
    },
    /// Answer a pending user-input request (Claude `AskUserQuestion`, Codex
    /// `item/tool/requestUserInput`). Each value is a string (single-select /
    /// free text) or an array of strings (multi-select), keyed by the matching
    /// [`UserInputQuestion::id`].
    RespondUserInput {
        request_id: String,
        answers: serde_json::Map<String, serde_json::Value>,
    },
    /// Switch the permission model mid-session. Providers that cannot switch
    /// live emit `AgentEvent::Warning` and keep the old mode; the UI then
    /// falls back to a resume-restart.
    SetApprovalMode(ApprovalMode),
    /// Inject a message into the turn that is ALREADY running, so the model
    /// picks it up at its next opportunity to accept input (typically the next
    /// tool call). Distinct from queueing, which is an app-level concept: a
    /// queued message is held and sent as an ordinary [`Self::SendTurn`] once
    /// the current turn completes.
    ///
    /// Only providers whose [`ProviderKind::supports_steering`] is true accept
    /// this; the others log and ignore it (the UI must not offer it there).
    Steer {
        /// Stable id correlating the persisted request with provider acceptance.
        request_id: String,
        text: String,
        attachments: Vec<Attachment>,
    },
    /// Switch Build/Plan interaction mode. Codex/OpenCode apply it on the next
    /// turn; Claude sends a `set_permission_mode` control request. pi has no
    /// native Plan mode.
    SetInteractionMode(InteractionMode),
    /// Set one of the agent's self-described options (see
    /// [`AgentEvent::ProviderOptions`]). ACP routes it to `session/set_mode`,
    /// `session/set_model` or `session/set_config_option` by the descriptor's
    /// origin; the native providers ignore ids they do not know.
    SetOption {
        id: String,
        value: serde_json::Value,
    },
    /// Ask the provider to restore one of its own message checkpoints. The
    /// provider remains the sole owner of both the checkpoint and the restore
    /// semantics; adapters without a native implementation reject this.
    Rewind {
        checkpoint_id: String,
        mode: RewindMode,
    },
    Shutdown,
}

/// A live provider session: send commands in, read canonical events out.
/// Dropping both channels (or sending `Shutdown`) tears the child process down.
pub struct SessionHandle {
    pub provider: ProviderKind,
    pub commands: async_channel::Sender<SessionCommand>,
    pub events: async_channel::Receiver<AgentEvent>,
}

/// Start a new (or resumed) session with the given provider.
pub async fn start_session(
    provider: ProviderKind,
    opts: SessionOptions,
) -> Result<SessionHandle, AgentError> {
    match provider {
        ProviderKind::Codex => codex::start(opts).await,
        ProviderKind::ClaudeCode => claude::start(opts).await,
        ProviderKind::Pi => pi::start(opts).await,
        ProviderKind::OpenCode => opencode::start(opts).await,
        ProviderKind::Acp => acp::start(opts).await,
    }
}

// ---------------------------------------------------------------------------
// Canonical event model: one normalized stream all providers map into
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// A tcode-level handoff between providers. This is never emitted by an
    /// adapter; the runtime persists it before the first relayed user turn.
    ProviderRelay {
        from_provider: ProviderKind,
        from_model: Option<String>,
        to_provider: ProviderKind,
        to_model: Option<String>,
    },
    /// The agent's self-described options (ACP `modes` / `models` /
    /// `configOptions`), pushed at session start and on every change. Reuses
    /// [`OptionDescriptor`] so the composer's existing picker renders them
    /// verbatim; answer with [`SessionCommand::SetOption`].
    ProviderOptions {
        descriptors: Vec<OptionDescriptor>,
        selections: Vec<OptionSelection>,
    },
    /// Emitted once the provider session is ready. `resume` must round-trip
    /// through `SessionOptions::resume` to continue this thread later.
    SessionStarted {
        provider_session_id: String,
        resume: ResumeCursor,
        model: Option<String>,
    },
    TurnStarted {
        turn_id: String,
    },
    /// Transient delivery acknowledgement for [`SessionCommand::SendTurn`].
    /// The runtime consumes this before persistence: it is control-plane state,
    /// not part of the canonical conversation history.
    TurnAccepted {
        delivery_id: u64,
    },
    /// Claude-owned background work which must keep its provider process alive
    /// after the model turn itself has completed. Other adapters currently do
    /// not emit this transient liveness signal.
    BackgroundTasksChanged {
        count: usize,
    },
    /// The provider's current net file changes for one turn. Unlike the
    /// per-tool [`ItemContent::FileChange`] stream, this is a replacement
    /// snapshot: later events with the same `turn_id` supersede earlier ones.
    /// Providers that supply the exact net result of their tracked mutations
    /// mark it [`ChangeCompleteness::Exact`]; adapters built from structured
    /// edit fragments use [`ChangeCompleteness::Partial`].
    TurnChangesUpdated {
        turn_id: String,
        changes: Vec<FileChange>,
        completeness: ChangeCompleteness,
    },
    /// A provider-owned restore point attached to one user turn. The id is
    /// opaque and must only be returned to the provider that emitted it.
    TurnCheckpoint {
        turn_id: String,
        checkpoint_id: String,
    },
    /// The provider successfully applied a native restore operation. A
    /// conversation restore also returns the selected prompt for composer
    /// prefill, matching the provider's own edit-after-rewind behavior.
    RewindCompleted {
        checkpoint_id: String,
        mode: RewindMode,
        prefill: Option<String>,
    },
    /// A native restore request was rejected without changing tcode's local
    /// conversation state.
    RewindFailed {
        checkpoint_id: String,
        mode: RewindMode,
        error: String,
    },
    TurnCompleted {
        turn_id: String,
        status: TurnStatus,
        usage: Option<TokenUsage>,
    },
    ItemStarted(ThreadItem),
    ItemUpdated(ThreadItem),
    ItemCompleted(ThreadItem),
    /// A runtime-synthesized steering request, persisted before provider delivery.
    SteerRequested {
        request_id: String,
        text: String,
        /// Local paths of the image attachments steered with the text (see
        /// [`ItemContent::UserMessage::attachments`]).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<String>,
    },
    /// Emitted only after the provider has consumed the correlated steer into
    /// its model context (or provided the strongest available consumption
    /// signal when its protocol has no explicit acknowledgement). Claude uses
    /// its next `status: requesting` checkpoint; a steer written microseconds
    /// before that status can still miss the request, but this is the best
    /// available signal without CLI protocol support.
    SteerAccepted {
        request_id: String,
    },
    /// Streaming text growth for an in-progress item. The item may not have
    /// been announced via `ItemStarted` yet (providers differ); the UI creates
    /// the item lazily on first delta.
    Delta {
        item_id: String,
        kind: DeltaKind,
        text: String,
    },
    ApprovalRequested(ApprovalRequest),
    ApprovalResolved {
        request_id: String,
        decision: ApprovalDecision,
    },
    /// The agent is asking the user one or more structured questions and is
    /// blocked until a matching [`SessionCommand::RespondUserInput`] arrives.
    UserInputRequested {
        request_id: String,
        questions: Vec<UserInputQuestion>,
    },
    /// A pending user-input request has been settled (answered, or cancelled on
    /// teardown — in which case `answers` is empty).
    UserInputResolved {
        request_id: String,
        answers: serde_json::Map<String, serde_json::Value>,
    },
    TokenUsage(TokenUsage),
    /// Provider-native commands / skills discovered for this session. The
    /// composer's `/` and `$` menus consume these. Session metadata — not folded
    /// into the timeline / persisted to the JSONL log.
    ProviderCommands {
        commands: Vec<ProviderCommand>,
    },
    /// The provider compacted its context window (Claude `system/compact_boundary`;
    /// Codex `contextCompaction` item). Rendered as a "Context compacted" work-log row.
    ContextCompacted,
    /// Structured plan / task list for the sidebar (Codex `turn/plan/updated`,
    /// Claude `TodoWrite`). Replaces the current turn's plan wholesale.
    PlanUpdated {
        turn_id: Option<String>,
        explanation: Option<String>,
        steps: Vec<PlanStep>,
    },
    /// Streaming growth of a proposed-plan block (Codex `item/plan/delta`).
    ProposedPlanDelta {
        item_id: String,
        text: String,
    },
    /// A completed proposed plan (Codex plan item; Claude `ExitPlanMode`).
    ProposedPlan {
        item_id: String,
        markdown: String,
    },
    Warning {
        message: String,
    },
    /// A runtime-synthesized fatal provider startup failure persisted for replay.
    #[rustfmt::skip]
    ProviderStartFailed { error: String },
    Error {
        message: String,
        fatal: bool,
    },
    SessionClosed {
        reason: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStep {
    pub step: String,
    pub status: PlanStepStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlanStepStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Completed,
    Failed,
    Interrupted,
}

/// Provider-native restore scopes. These describe what the upstream agent
/// changes; tcode never emulates a missing scope with Git or transcript edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewindMode {
    Files,
    Conversation,
    FilesAndConversation,
}

impl RewindMode {
    pub fn includes_files(self) -> bool {
        matches!(self, Self::Files | Self::FilesAndConversation)
    }

    pub fn includes_conversation(self) -> bool {
        matches!(self, Self::Conversation | Self::FilesAndConversation)
    }
}

/// How strongly a provider can attribute a turn's reported file changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeCompleteness {
    /// The provider supplied the exact turn-scoped net result of the file
    /// mutations it tracks, rather than inferring it from the ambient working
    /// tree. Uninstrumented command writes are outside that claim.
    Exact,
    /// The listed paths came from structured edit operations, but arbitrary
    /// commands or external writers may have changed additional files.
    Partial,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeltaKind {
    AssistantText,
    ReasoningText,
    CommandOutput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    InProgress,
    Completed,
    Failed,
    Declined,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadItem {
    /// Provider-scoped stable id; deltas and later lifecycle events reference it.
    pub id: String,
    /// Spawn item that owns this child activity, when the item came from a subagent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_item_id: Option<String>,
    pub content: ItemContent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ItemContent {
    UserMessage {
        text: String,
        /// Byte length of an injected context prefix folded into `text` — the
        /// orchestrate guidance + configuration that `/orchestrate` composes
        /// ahead of the user's own words. The provider still receives the whole
        /// `text`; the UI uses this offset to render the prefix as a collapsed
        /// disclosure row and keep the bubble to `text[context_len..]`. Absent on
        /// ordinary messages and on every log written before this field existed,
        /// which therefore render as a plain bubble exactly as before.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_len: Option<usize>,
        /// Local paths of the image attachments sent with this message, so the
        /// timeline (live and replayed) can render them. Empty on text-only
        /// messages and on logs written before this field existed.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<String>,
    },
    AssistantMessage {
        text: String,
    },
    Reasoning {
        text: String,
    },
    CommandExecution {
        command: String,
        output: String,
        exit_code: Option<i32>,
        status: ItemStatus,
    },
    FileChange {
        changes: Vec<FileChange>,
        status: ItemStatus,
    },
    ToolCall {
        name: String,
        input: serde_json::Value,
        output: Option<String>,
        status: ItemStatus,
    },
    Subagent {
        agent_type: String,
        description: String,
        status: ItemStatus,
        /// Final one-line summary when finished.
        summary: Option<String>,
    },
    WebSearch {
        query: String,
    },
    /// Anything canonicalization doesn't model yet; `summary` is displayable.
    Other {
        provider_kind: String,
        summary: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub kind: FileChangeKind,
    /// Unified diff for this file when the provider supplies one.
    pub diff: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    Create,
    Modify,
    Delete,
    Rename,
}

/// Split a provider-supplied, multi-file Git-style unified diff into canonical
/// file changes. This parses only file boundaries and operation kinds; each
/// returned change retains its complete diff section for rendering.
pub fn file_changes_from_unified_diff(diff: &str) -> Result<Vec<FileChange>, String> {
    if diff.trim().is_empty() {
        return Ok(Vec::new());
    }

    let mut sections = Vec::new();
    let mut current = String::new();
    for line in diff.lines() {
        if line.starts_with("diff --git ") && !current.is_empty() {
            sections.push(std::mem::take(&mut current));
        }
        if !current.is_empty() || line.starts_with("diff --git ") {
            current.push_str(line);
            current.push('\n');
        }
    }
    if !current.is_empty() {
        sections.push(current);
    }
    if sections.is_empty() {
        return Err("turn diff contains no `diff --git` file sections".into());
    }

    sections
        .into_iter()
        .map(|section| {
            let mut old_path = None;
            let mut new_path = None;
            for line in section.lines() {
                if let Some(path) = line.strip_prefix("rename from ") {
                    old_path = Some(path.to_string());
                } else if let Some(path) = line.strip_prefix("rename to ") {
                    new_path = Some(path.to_string());
                } else if let Some(path) = line.strip_prefix("--- a/") {
                    old_path.get_or_insert_with(|| path.to_string());
                } else if let Some(path) = line.strip_prefix("+++ b/") {
                    new_path.get_or_insert_with(|| path.to_string());
                }
            }
            if old_path.is_none() || new_path.is_none() {
                let header = section
                    .lines()
                    .next()
                    .and_then(|line| line.strip_prefix("diff --git a/"))
                    .ok_or_else(|| "turn diff section has no valid header".to_string())?;
                if let Some((old, new)) = header.rsplit_once(" b/") {
                    old_path.get_or_insert_with(|| old.to_string());
                    new_path.get_or_insert_with(|| new.to_string());
                }
            }

            let creates = section
                .lines()
                .any(|line| line.starts_with("new file mode ") || line == "--- /dev/null");
            let deletes = section
                .lines()
                .any(|line| line.starts_with("deleted file mode ") || line == "+++ /dev/null");
            let kind = if creates {
                FileChangeKind::Create
            } else if deletes {
                FileChangeKind::Delete
            } else if old_path != new_path {
                FileChangeKind::Rename
            } else {
                FileChangeKind::Modify
            };
            let path = match kind {
                FileChangeKind::Delete => old_path,
                _ => new_path.or(old_path),
            }
            .filter(|path| !path.is_empty())
            .ok_or_else(|| "turn diff section has no file path".to_string())?;

            Ok(FileChange {
                path,
                kind,
                diff: Some(section),
            })
        })
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: String,
    pub turn_id: Option<String>,
    pub kind: ApprovalKind,
    /// Agent-supplied choices. ACP agents send their own option list
    /// (`session/request_permission`), so the UI renders exactly those buttons.
    /// Empty for the native providers, whose four fixed decisions
    /// ([`ApprovalDecision`]) apply instead.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<ApprovalOption>,
}

/// One choice offered by an ACP agent's permission request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalOption {
    /// Opaque id echoed back in [`ApprovalDecision::Option`].
    pub id: String,
    pub label: String,
    pub kind: ApprovalOptionKind,
}

/// ACP's `PermissionOptionKind`: lets the UI style/order the buttons sanely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalKind {
    ExecCommand {
        command: String,
        cwd: Option<String>,
        reason: Option<String>,
    },
    /// A read-only file/search operation (Claude's `file_read_approval` family:
    /// Read/View/Grep/Glob/…/WebSearch). `detail` is the pre-rendered summary
    /// (see the S2 §1.3 "Approval detail" rules).
    FileRead { detail: String },
    FileChange {
        changes: Vec<FileChange>,
        reason: Option<String>,
    },
    /// Dynamic fallback for any tool that doesn't classify as command / file
    /// read / file change (agent, mcp, image, …). `detail` is the pre-rendered
    /// summary per the S2 §1.3 rules.
    ToolUse {
        name: String,
        input: serde_json::Value,
        detail: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approve,
    /// Approve and don't ask again for this kind of action in this session.
    ApproveForSession,
    Deny,
    /// Deny and cancel the turn. Claude maps this to a permission denial with
    /// `"User cancelled tool execution."` (no interrupt); Codex maps it to the
    /// protocol `{decision:"cancel"}` (deny + immediate turn interruption).
    Cancel,
    /// Pick one of the agent's own [`ApprovalOption`]s (ACP only).
    Option(String),
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Total context currently in use, if the provider reports it.
    pub used_tokens: Option<u64>,
    pub context_window: Option<u64>,
    /// Cumulative tokens processed over the session's lifetime, if known (Codex
    /// `thread/tokenUsage` running total; Claude accumulated per-turn usage).
    /// Shown as "Total processed" in the context-meter popover.
    pub total_processed_tokens: Option<u64>,
}

#[cfg(test)]
mod mcp_registration_tests {
    use super::*;

    fn reg() -> McpRegistration {
        McpRegistration {
            name: McpRegistration::SERVER_NAME_PREVIEW.into(),
            url: "http://127.0.0.1:53211/mcp".into(),
            bearer_token: "abc123".into(),
        }
    }

    #[test]
    fn claude_mcp_config_json_shape() {
        let registration = reg();
        let json: serde_json::Value =
            serde_json::from_str(&claude_mcp_config_json([&registration])).unwrap();
        let server = &json["mcpServers"]["tcode_preview"];
        assert_eq!(server["type"], "http");
        assert_eq!(server["url"], "http://127.0.0.1:53211/mcp");
        assert_eq!(server["headers"]["Authorization"], "Bearer abc123");
    }

    #[test]
    fn codex_config_override_is_valid_toml_streamable_http() {
        let arg = reg().codex_config_override();
        let (key, value) = arg.split_once('=').unwrap();
        assert_eq!(key, "mcp_servers.tcode_preview");
        // The value must parse as a TOML inline table with url + auth header,
        // and must NOT use a literal bearer_token (codex rejects that for HTTP).
        let doc: toml::Value = toml::from_str(&format!("v = {value}")).unwrap();
        let table = &doc["v"];
        assert_eq!(table["url"].as_str(), Some("http://127.0.0.1:53211/mcp"));
        assert_eq!(
            table["http_headers"]["Authorization"].as_str(),
            Some("Bearer abc123")
        );
        assert!(table.get("bearer_token").is_none());
    }
}

#[cfg(test)]
mod launch_env_tests {
    use super::*;

    #[test]
    fn home_maps_to_the_providers_own_variable() {
        let env = LaunchEnv {
            env: vec![("FOO".into(), "bar".into())],
            home: Some(PathBuf::from("/tmp/home")),
        };
        assert_eq!(
            env.pairs(ProviderKind::ClaudeCode),
            vec![
                ("FOO".to_string(), "bar".to_string()),
                ("HOME".to_string(), "/tmp/home".to_string()),
            ]
        );
        assert_eq!(
            env.pairs(ProviderKind::Codex),
            vec![
                ("FOO".to_string(), "bar".to_string()),
                ("CODEX_HOME".to_string(), "/tmp/home".to_string()),
            ]
        );
    }

    #[test]
    fn no_home_override_leaves_only_the_configured_pairs() {
        let env = LaunchEnv {
            env: vec![("ANTHROPIC_BASE_URL".into(), "https://x".into())],
            home: None,
        };
        assert_eq!(
            env.pairs(ProviderKind::ClaudeCode),
            vec![("ANTHROPIC_BASE_URL".to_string(), "https://x".to_string())]
        );
        assert!(LaunchEnv::default().pairs(ProviderKind::Codex).is_empty());
    }
}

#[cfg(test)]
mod resolve_binary_tests {
    use super::*;

    /// The documented Windows default, used when `PATHEXT` is unset.
    fn windows_pathext() -> Vec<String> {
        [".COM", ".EXE", ".BAT", ".CMD"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    /// Windows' "is executable": a regular file (no exec bit exists there).
    #[cfg(windows)]
    fn is_file(path: &std::path::Path) -> bool {
        path.is_file()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tcode-resolve-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn explicit_path_is_used_as_given() {
        let explicit = std::path::Path::new("/opt/custom/claude");
        let resolved = resolve_binary(Some(explicit), "claude").unwrap();
        assert_eq!(resolved, explicit);
    }

    /// A `default_name` that carries a path component is passed through — with
    /// either separator, since Windows accepts `/` as well as `\`.
    #[test]
    fn a_name_with_any_separator_skips_the_path_search() {
        assert_eq!(
            resolve_binary(None, "/opt/custom/claude").unwrap(),
            PathBuf::from("/opt/custom/claude")
        );
        assert_eq!(
            resolve_binary(None, r"C:\tools\claude.cmd").unwrap(),
            PathBuf::from(r"C:\tools\claude.cmd")
        );
        assert_eq!(
            resolve_binary(None, "C:/tools/claude.cmd").unwrap(),
            PathBuf::from("C:/tools/claude.cmd")
        );
    }

    #[test]
    fn missing_binary_reports_a_helpful_error() {
        let err = resolve_binary(None, "tcode-definitely-not-a-real-binary").unwrap_err();
        assert!(matches!(err, AgentError::Spawn(msg) if msg.contains("not found on PATH")));
    }

    // ---- the Unix branch ----------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn unix_resolves_an_extensionless_file_with_the_exec_bit() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = temp_dir("unix");
        let bin = dir.join("foo");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A same-named non-executable file must not win.
        std::fs::write(dir.join("bar"), "not executable").unwrap();

        assert_eq!(
            find_in_dirs([dir.clone()], "foo", &[], is_executable),
            Some(bin)
        );
        assert_eq!(find_in_dirs([dir], "bar", &[], is_executable), None);
    }

    #[cfg(unix)]
    #[test]
    fn unix_bare_name_resolves_to_an_absolute_path_on_path() {
        // `sh` is on PATH everywhere we run; the point is that the result is
        // absolute, so a child that sets its own cwd can still exec it.
        let resolved = resolve_binary(None, "sh").expect("sh must resolve");
        assert!(
            resolved.is_absolute(),
            "resolved {resolved:?} is not absolute"
        );
        assert!(is_executable(&resolved));
    }

    #[cfg(unix)]
    #[test]
    fn unix_tries_only_the_bare_name() {
        assert_eq!(path_extensions(), Vec::<String>::new());
        assert_eq!(candidate_names("claude", &[]), vec!["claude".to_string()]);
    }

    // ---- the Windows branch (exercised from any host) ------------------------

    /// The resolved file name, lowercased: the extension comes from `PATHEXT`
    /// (conventionally uppercase: `.EXE`), while the file on disk is usually
    /// lowercase. Both Windows and macOS resolve that case-insensitively, so the
    /// assertions below compare names case-insensitively too.
    #[cfg(windows)]
    fn resolved_name(path: Option<PathBuf>) -> Option<String> {
        Some(path?.file_name()?.to_string_lossy().to_lowercase())
    }

    /// The real Windows shape: `claude` only ever exists as `claude.cmd`, so the
    /// bare join the old resolver did found nothing.
    // Relies on the Windows filesystem being case-insensitive: PATHEXT is
    // upper-case while npm writes `claude.cmd`. On a case-sensitive FS the
    // join cannot match, so this is a Windows-only behavior test. The
    // case-independent ordering/fallback logic is covered below on every OS.
    #[cfg(windows)]
    #[test]
    fn windows_resolves_a_cmd_shim_for_a_bare_name() {
        let dir = temp_dir("win-cmd");
        std::fs::write(dir.join("claude.cmd"), "@echo off\n").unwrap();

        let found = find_in_dirs([dir], "claude", &windows_pathext(), is_file);
        assert_eq!(resolved_name(found), Some("claude.cmd".to_string()));
    }

    /// PATHEXT order decides: `.EXE` beats `.CMD` when both exist.
    #[cfg(windows)]
    #[test]
    fn windows_honors_pathext_order_and_falls_back_to_the_bare_name() {
        let dir = temp_dir("win-order");
        std::fs::write(dir.join("codex.cmd"), "").unwrap();
        std::fs::write(dir.join("codex.exe"), "").unwrap();
        let found = find_in_dirs([dir], "codex", &windows_pathext(), is_file);
        assert_eq!(resolved_name(found), Some("codex.exe".to_string()));

        // An extensionless file is still found — last, after every PATHEXT try.
        let bare = temp_dir("win-bare");
        std::fs::write(bare.join("codex"), "").unwrap();
        let found = find_in_dirs([bare], "codex", &windows_pathext(), is_file);
        assert_eq!(resolved_name(found), Some("codex".to_string()));
    }

    /// A name that already has an extension is used verbatim (no PATHEXT loop):
    /// `claude.cmd` must not be probed as `claude.cmd.EXE`.
    #[test]
    fn windows_uses_an_explicit_extension_as_given() {
        assert_eq!(
            candidate_names("claude.cmd", &windows_pathext()),
            vec!["claude.cmd".to_string()]
        );
        assert_eq!(
            candidate_names("npm", &windows_pathext()),
            vec![
                "npm.COM".to_string(),
                "npm.EXE".to_string(),
                "npm.BAT".to_string(),
                "npm.CMD".to_string(),
                "npm".to_string(),
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_pathext_defaults_when_unset() {
        // On a real Windows host PATHEXT is always set, but the default must be
        // the documented one when it is not.
        assert!(
            path_extensions()
                .iter()
                .any(|ext| ext.eq_ignore_ascii_case(".EXE"))
        );
        assert!(
            path_extensions()
                .iter()
                .any(|ext| ext.eq_ignore_ascii_case(".CMD"))
        );
    }
}

#[cfg(test)]
mod pathext_logic_tests {
    use super::*;
    use std::path::PathBuf;

    /// The PATHEXT candidate order (and the bare-name fallback) is pure logic:
    /// assert it on every OS, independent of filesystem case rules.
    #[test]
    fn pathext_candidates_are_tried_in_order_then_the_bare_name() {
        let pathext: Vec<String> = [".COM", ".EXE", ".BAT", ".CMD"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            candidate_names("codex", &pathext),
            vec![
                "codex.COM".to_string(),
                "codex.EXE".to_string(),
                "codex.BAT".to_string(),
                "codex.CMD".to_string(),
                "codex".to_string(),
            ]
        );
    }

    /// A name that already carries an extension is used as-is (no PATHEXT sweep).
    #[test]
    fn an_explicit_extension_is_not_expanded() {
        let pathext: Vec<String> = vec![".EXE".to_string(), ".CMD".to_string()];
        assert_eq!(
            candidate_names("claude.cmd", &pathext),
            vec!["claude.cmd".to_string()]
        );
    }

    /// Exact-case fixtures, so the search behaves identically on a
    /// case-sensitive filesystem: the first PATHEXT hit wins.
    #[test]
    fn first_matching_candidate_wins_on_any_filesystem() {
        let dir = std::env::temp_dir().join(format!("tcode-pathext-{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tool.CMD"), "").unwrap();
        std::fs::write(dir.join("tool.EXE"), "").unwrap();
        let pathext: Vec<String> = vec![".EXE".to_string(), ".CMD".to_string()];
        let found = find_in_dirs([dir.clone()], "tool", &pathext, |p: &std::path::Path| {
            p.is_file()
        });
        assert_eq!(
            found.and_then(|p: PathBuf| p.file_name().map(|n| n.to_string_lossy().into_owned())),
            Some("tool.EXE".to_string())
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    fn uuid_like() -> u128 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }
}

#[cfg(test)]
mod turn_diff_tests {
    use super::*;

    #[test]
    fn splits_multi_file_turn_diff_and_classifies_operations() {
        let diff = concat!(
            "diff --git a/src/lib.rs b/src/lib.rs\n",
            "index 1111111..2222222 100644\n",
            "--- a/src/lib.rs\n",
            "+++ b/src/lib.rs\n",
            "@@ -1 +1 @@\n",
            "-old\n",
            "+new\n",
            "diff --git a/new.txt b/new.txt\n",
            "new file mode 100644\n",
            "--- /dev/null\n",
            "+++ b/new.txt\n",
            "@@ -0,0 +1 @@\n",
            "+hello\n",
            "diff --git a/old.txt b/old.txt\n",
            "deleted file mode 100644\n",
            "--- a/old.txt\n",
            "+++ /dev/null\n",
            "@@ -1 +0,0 @@\n",
            "-gone\n",
            "diff --git a/before.rs b/after.rs\n",
            "similarity index 100%\n",
            "rename from before.rs\n",
            "rename to after.rs\n",
        );

        let changes = file_changes_from_unified_diff(diff).unwrap();
        assert_eq!(changes.len(), 4);
        assert_eq!(changes[0].path, "src/lib.rs");
        assert_eq!(changes[0].kind, FileChangeKind::Modify);
        assert_eq!(changes[1].path, "new.txt");
        assert_eq!(changes[1].kind, FileChangeKind::Create);
        assert_eq!(changes[2].path, "old.txt");
        assert_eq!(changes[2].kind, FileChangeKind::Delete);
        assert_eq!(changes[3].path, "after.rs");
        assert_eq!(changes[3].kind, FileChangeKind::Rename);
        assert!(changes[0].diff.as_deref().unwrap().contains("-old\n+new"));
        assert!(!changes[0].diff.as_deref().unwrap().contains("new.txt"));
    }

    #[test]
    fn empty_diff_is_an_exact_empty_snapshot_but_malformed_diff_is_rejected() {
        assert!(file_changes_from_unified_diff("\n").unwrap().is_empty());
        assert!(file_changes_from_unified_diff("--- a/file\n+++ b/file\n").is_err());
    }

    #[test]
    fn warning_event_round_trips() {
        let event = AgentEvent::Warning {
            message: "boom".into(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "warning");
        assert_eq!(json["message"], "boom");
        let decoded: AgentEvent = serde_json::from_value(json).unwrap();
        assert!(matches!(
            decoded,
            AgentEvent::Warning { message } if message == "boom"
        ));
    }

    #[test]
    fn turn_change_event_round_trips() {
        let event = AgentEvent::TurnChangesUpdated {
            turn_id: "turn-7".into(),
            changes: vec![FileChange {
                path: "src/main.rs".into(),
                kind: FileChangeKind::Modify,
                diff: Some("@@ -1 +1 @@\n-old\n+new\n".into()),
            }],
            completeness: ChangeCompleteness::Exact,
        };
        let json = serde_json::to_string(&event).unwrap();
        let decoded: AgentEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            AgentEvent::TurnChangesUpdated {
                turn_id,
                completeness: ChangeCompleteness::Exact,
                changes,
            } if turn_id == "turn-7" && changes[0].path == "src/main.rs"
        ));
    }
}

#[cfg(test)]
#[test]
fn provider_start_failed_event_round_trips() {
    let event = AgentEvent::ProviderStartFailed {
        error: "spawn failed".into(),
    };
    let json = serde_json::to_string(&event).unwrap();
    assert_eq!(
        json,
        r#"{"type":"provider_start_failed","error":"spawn failed"}"#
    );
    let decoded: AgentEvent = serde_json::from_str(&json).unwrap();
    assert!(matches!(
        decoded,
        AgentEvent::ProviderStartFailed { error } if error == "spawn failed"
    ));

    let legacy = r#"{"type":"error","message":"boom","fatal":true}"#;
    let decoded: AgentEvent = serde_json::from_str(legacy).unwrap();
    assert!(matches!(
        &decoded,
        AgentEvent::Error { message, fatal: true } if message == "boom"
    ));
    assert_eq!(serde_json::to_string(&decoded).unwrap(), legacy);
}

#[cfg(test)]
mod thread_item_serde_tests {
    use super::*;

    #[test]
    fn parent_item_id_round_trips_and_legacy_items_default_to_none() {
        let event = AgentEvent::ItemCompleted(ThreadItem {
            id: "child".into(),
            parent_item_id: Some("spawn".into()),
            content: ItemContent::AssistantMessage {
                text: "working".into(),
            },
        });
        let json = serde_json::to_string(&event).unwrap();
        let decoded: AgentEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            AgentEvent::ItemCompleted(ThreadItem { parent_item_id: Some(parent), .. })
                if parent == "spawn"
        ));

        let legacy = r#"{"type":"item_completed","id":"old","content":{"kind":"user_message","text":"hello"}}"#;
        let decoded: AgentEvent = serde_json::from_str(legacy).unwrap();
        assert!(matches!(
            decoded,
            AgentEvent::ItemCompleted(ThreadItem {
                parent_item_id: None,
                ..
            })
        ));
    }
}
