//! Provider-agnostic agent session layer.
//!
//! Each provider module (`codex`, `claude`) spawns its CLI as a child process,
//! speaks the provider-native protocol over stdio, and normalizes everything
//! into the canonical [`AgentEvent`] stream. The UI only ever sees this module's
//! types; nothing provider-shaped leaks past this crate except [`ResumeCursor`],
//! which is intentionally opaque.

pub mod claude;
pub mod codex;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Codex,
    ClaudeCode,
}

impl ProviderKind {
    pub fn display_name(&self) -> &'static str {
        match self {
            ProviderKind::Codex => "Codex",
            ProviderKind::ClaudeCode => "Claude Code",
        }
    }
}

/// Provider-shaped opaque state needed to resume a session later
/// (Codex: `{"thread_id": ...}`; Claude: `{"session_id": ...}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeCursor(pub serde_json::Value);

#[derive(Debug, Clone)]
pub struct SessionOptions {
    pub cwd: PathBuf,
    /// Provider-native model id; `None` = provider default.
    pub model: Option<String>,
    pub resume: Option<ResumeCursor>,
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
}

/// Connection details for the tcode preview MCP server (a streamable-HTTP
/// endpoint on loopback, guarded by a bearer token). Provider-agnostic; each
/// provider maps it onto its native MCP-server config shape.
#[derive(Debug, Clone)]
pub struct McpRegistration {
    /// Streamable-HTTP endpoint, e.g. `http://127.0.0.1:53211/mcp`.
    pub url: String,
    /// Bearer token presented on every request (`Authorization: Bearer <token>`).
    pub bearer_token: String,
}

impl McpRegistration {
    /// The MCP server name registered with agents (also the tool namespace).
    pub const SERVER_NAME: &'static str = "tcode_preview";

    /// Claude Code `--mcp-config` JSON: a single `mcpServers` map entry for an
    /// HTTP server carrying the bearer token as an `Authorization` header.
    /// Verified shape for `claude` 2.1.x (`.mcp.json` `type: "http"`).
    pub fn claude_mcp_config_json(&self) -> String {
        let value = serde_json::json!({
            "mcpServers": {
                Self::SERVER_NAME: {
                    "type": "http",
                    "url": self.url,
                    "headers": {
                        "Authorization": format!("Bearer {}", self.bearer_token),
                    }
                }
            }
        });
        value.to_string()
    }

    /// Codex `-c` override value: an inline TOML table for a streamable-HTTP MCP
    /// server. Codex rejects a literal `bearer_token` for HTTP, so the token
    /// rides in `http_headers.Authorization` instead (verified against
    /// codex `config/src/mcp_types.rs`). Returns the full `key=value` argument.
    pub fn codex_config_override(&self) -> String {
        // TOML basic strings; our url/token are ASCII with no quotes/backslashes.
        format!(
            "mcp_servers.{name}={{url=\"{url}\",http_headers={{Authorization=\"Bearer {token}\"}}}}",
            name = Self::SERVER_NAME,
            url = self.url,
            token = self.bearer_token,
        )
    }
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
        id: String, // "reasoningEffort" | "contextWindow" | "serviceTier" ...
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
    if default_name.contains(std::path::MAIN_SEPARATOR) {
        return Ok(PathBuf::from(default_name));
    }
    let path_var = std::env::var_os("PATH")
        .ok_or_else(|| AgentError::Spawn(format!("`{default_name}` not found: PATH is unset")))?;
    for dir in std::env::split_paths(&path_var) {
        // Skip unexpanded/relative entries — they are meaningless to us and
        // would resolve against the child's cwd.
        if !dir.is_absolute() {
            continue;
        }
        let candidate = dir.join(default_name);
        if is_executable(&candidate) {
            return Ok(candidate);
        }
    }
    Err(AgentError::Spawn(format!(
        "`{default_name}` not found on PATH (set its path in Settings → Providers)"
    )))
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

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

/// Interaction mode (T3: Build/Plan).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionMode {
    #[default]
    Build,
    Plan,
}

/// Per-turn overrides layered on top of the session's persisted options.
/// Codex applies these per turn; Claude ignores `effort` (launch-time only).
#[derive(Debug, Clone, Default)]
pub struct TurnOptions {
    pub effort: Option<String>,
    pub interaction_mode: Option<InteractionMode>,
}

/// List the provider's models (spawn, query, teardown).
pub async fn list_models(
    provider: ProviderKind,
    binary_path: Option<PathBuf>,
) -> Result<Vec<ModelSpec>, AgentError> {
    match provider {
        ProviderKind::Codex => codex::list_models(binary_path).await,
        ProviderKind::ClaudeCode => claude::list_models(binary_path).await,
    }
}

/// The user-facing permission model, provider-agnostic.
///
/// Providers map this onto their native controls:
/// - Claude Code: `--permission-mode` default / acceptEdits / bypassPermissions
///   (switchable mid-session via the control protocol).
/// - Codex: approval-policy × sandbox-mode combinations on thread start
///   (mid-session switch may require a resume-restart).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    /// Ask before commands and file changes.
    Supervised,
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
        text: String,
        /// Per-turn overrides (Codex applies per turn; Claude ignores
        /// `effort`). `None` = use the session's persisted options.
        options: Option<TurnOptions>,
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
    /// Switch Build/Plan interaction mode. Codex applies it on the next
    /// `turn/start`; Claude sends a `set_permission_mode` control request.
    SetInteractionMode(InteractionMode),
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
    }
}

// ---------------------------------------------------------------------------
// Canonical event model: one normalized stream all providers map into
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
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
    TurnCompleted {
        turn_id: String,
        status: TurnStatus,
        usage: Option<TokenUsage>,
    },
    ItemStarted(ThreadItem),
    ItemUpdated(ThreadItem),
    ItemCompleted(ThreadItem),
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
    Warning(String),
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
    pub content: ItemContent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ItemContent {
    UserMessage {
        text: String,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: String,
    pub turn_id: Option<String>,
    pub kind: ApprovalKind,
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
    FileRead {
        detail: String,
    },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Total context currently in use, if the provider reports it.
    pub used_tokens: Option<u64>,
    pub context_window: Option<u64>,
}

#[cfg(test)]
mod mcp_registration_tests {
    use super::*;

    fn reg() -> McpRegistration {
        McpRegistration {
            url: "http://127.0.0.1:53211/mcp".into(),
            bearer_token: "abc123".into(),
        }
    }

    #[test]
    fn claude_mcp_config_json_shape() {
        let json: serde_json::Value =
            serde_json::from_str(&reg().claude_mcp_config_json()).unwrap();
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
mod resolve_binary_tests {
    use super::*;

    #[test]
    fn explicit_path_is_used_as_given() {
        let explicit = std::path::Path::new("/opt/custom/claude");
        let resolved = resolve_binary(Some(explicit), "claude").unwrap();
        assert_eq!(resolved, explicit);
    }

    #[test]
    fn bare_name_resolves_to_an_absolute_path_on_path() {
        // `sh` is on PATH everywhere we run; the point is that the result is
        // absolute, so a child that sets its own cwd can still exec it.
        let resolved = resolve_binary(None, "sh").expect("sh must resolve");
        assert!(resolved.is_absolute(), "resolved {resolved:?} is not absolute");
        assert!(is_executable(&resolved));
    }

    #[test]
    fn missing_binary_reports_a_helpful_error() {
        let err = resolve_binary(None, "tcode-definitely-not-a-real-binary").unwrap_err();
        assert!(matches!(err, AgentError::Spawn(msg) if msg.contains("not found on PATH")));
    }
}
