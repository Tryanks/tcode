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

/// A chosen option value, persisted per session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionSelection {
    pub id: String,
    pub value: serde_json::Value,
} // string or bool

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
    FileChange {
        changes: Vec<FileChange>,
        reason: Option<String>,
    },
    ToolUse {
        name: String,
        input: serde_json::Value,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approve,
    /// Approve and don't ask again for this kind of action in this session.
    ApproveForSession,
    Deny,
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
