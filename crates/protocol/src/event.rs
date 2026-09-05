use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use agent::{
    ApprovalMode, InteractionMode, OptionDescriptor, OptionSelection, ProviderCommand,
    ProviderKind, RewindMode,
};
use serde::{Deserialize, Serialize};
use tcode_core::{
    git::{GitAction, GitStatus},
    project::{Project, SessionMeta, WorktreeInfo},
    provider_status::ProviderSnapshot,
    session::{ReviewComment, StoredEvent},
    settings::Settings,
    ui::{TerminalSplitDirection, WorkspaceMode},
};

/// Backward-compatible name for the core event record now used directly on
/// the protocol wire.
pub type SessionEventRecord = StoredEvent;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum Topic {
    SessionEvents { session_id: String },
    SessionStatus { session_id: String },
    Index,
    Settings,
    Providers,
    GitStatus { session_id: String },
    RuntimeEvents,

    Terminal { terminal_id: u64 },
    Preview { session_id: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// Present only on subscription replies; mux routes these to their requester.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<u64>,
    pub topic: Topic,
    pub event: ServerEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum ServerEvent {
    TerminalOutput {
        terminal_id: u64,
        #[serde(with = "crate::wire::base64_bytes")]
        bytes: Vec<u8>,
        /// A replay (or restarted PTY) replaces the client emulator.
        reset: bool,
        cols: u16,
        rows: u16,
    },
    PreviewRequest {
        request_id: u64,
        session_id: String,
        request: crate::PreviewRequest,
    },
    SessionEvent(StoredEvent),
    SessionStatusReplaced(SessionStatus),
    ProvidersReplaced(ProvidersStatus),
    GitStatusReplaced(GitStatusStatus),
    IndexUpsertSession(SessionMeta),
    IndexUpsertProject(Project),
    IndexRemoveSession {
        session_id: String,
    },
    IndexRemoveProject {
        project_id: String,
    },
    SettingsReplaced(Settings),
    Runtime(RuntimeNotification),

    /// A provider-native rewind prompt is delivered once over the serialized
    /// event stream. The client store owns consumption after receipt.
    NativeRewindPrefill {
        session_id: String,
        text: String,
    },
    /// A turn was stopped because Claude Code's safety classifier fired: the request
    /// was blocked (fallback prevented) or the model silently fell back and tcode
    /// interrupted it. Drives the recovery card. Transient; never persisted.
    ModelFallbackBlocked {
        session_id: String,
        category: Option<agent::ClassifierCategory>,
        /// The selected model that refused / was expected.
        model: Option<String>,
        /// The model Claude rerouted to, when a silent fallback actually occurred
        /// (tcode interrupted it). None when the request was blocked outright.
        fallback_model: Option<String>,
        /// Claude's own explanation text when available (empty otherwise).
        detail: String,
    },
    /// A secondary model's review of a classifier-blocked turn: a user-facing
    /// assessment plus a DRAFT clarification the user reviews and sends. Advisory
    /// only; never auto-sent. Transient.
    FallbackReviewReady {
        session_id: String,
        assessment: String,
        /// Suggested first-person clarification, prefilled but editable. Empty when
        /// the reviewer judged the flag not a false positive.
        draft: String,
    },
    SessionSnapshot {
        from: u64,
        records: Vec<StoredEvent>,
    },
    IndexSnapshot(IndexSnapshot),
    SettingsSnapshot(Settings),
}

/// Full provider/settings-page read projection.
///
/// Provider state changes infrequently and its pieces are consumed together,
/// so hosts replace this single value after any covered mutation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ProvidersStatus {
    pub model_catalogs: HashMap<ProviderKind, Vec<agent::ModelSpec>>,
    pub models_loading: HashMap<ProviderKind, bool>,
    pub provider_versions: HashMap<ProviderKind, ProviderVersionStatus>,
    pub tcode_update: TcodeUpdateStatus,
    pub provider_snapshots: HashMap<String, ProviderSnapshot>,
    /// Latest account usage per profile id (Codex / Claude Code only).
    #[serde(default)]
    pub provider_usage: HashMap<String, tcode_core::usage::ProviderUsage>,
    /// Profile ids with a usage fetch in flight.
    #[serde(default)]
    pub usage_checking: HashSet<String>,
    pub acp_marketplace_items: Vec<AcpMarketplaceItem>,
    pub acp_registry_loading: bool,
    pub acp_registry_error: Option<String>,
    pub acp_installing: HashSet<String>,
    pub providers_checked_at: Option<u64>,
    pub providers_checking: bool,
    /// Environment-variable names present for each profile. Values never cross
    /// the protocol boundary.
    pub secret_names: HashMap<String, HashSet<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProviderVersionStatus {
    pub installed: Option<String>,
    pub latest: Option<String>,
    pub update_available: bool,
    pub checking: bool,
    pub updating: bool,
    pub update_command: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TcodeUpdateStatus {
    pub current: String,
    pub latest: Option<String>,
    pub release_url: Option<String>,
    pub update_available: bool,
    pub checking: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpMarketplaceItem {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub installed: bool,
    pub installing: bool,
    pub supported: bool,
}

/// Full active-workspace Git projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct GitStatusStatus {
    pub status: Option<GitStatus>,
    pub busy: bool,
}

/// Full, ephemeral runtime status for one session.
///
/// Unlike [`StoredEvent`], these values are not derivable by folding the
/// persisted agent event stream. Hosts replace the whole value whenever one of
/// its fields changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStatus {
    pub session_id: String,
    pub title: String,
    pub cwd: PathBuf,
    pub attachments_dir: PathBuf,
    pub provider: ProviderKind,
    pub requested_model: Option<String>,
    pub requested_profile_id: Option<String>,
    pub acp_agent_id: Option<String>,
    pub project_id: Option<String>,
    pub approval_mode: ApprovalMode,
    pub interaction_mode: InteractionMode,
    pub queued_messages: Vec<QueuedMessageStatus>,
    /// Backend-owned drafts used by provider-bound send-path assembly.
    pub review_comment_drafts: Vec<ReviewComment>,
    pub terminals: Vec<TerminalStatus>,
    pub active_terminal_id: Option<u64>,
    pub terminal_splits: Vec<TerminalSplitStatus>,
    pub terminal_contexts: Vec<TerminalContextStatus>,
    pub terminal_open: bool,
    pub terminal_height: f32,
    pub delivery_in_flight: Option<u64>,
    pub turn_running: bool,
    pub working: bool,
    pub pending_approval: bool,
    pub pending_user_input: bool,
    #[serde(rename = "supports_steering")]
    pub steering_supported: bool,
    pub provider_option_descriptors: Vec<OptionDescriptor>,
    pub provider_option_selections: Vec<OptionSelection>,
    pub provider_commands: Vec<ProviderCommand>,
    pub git_branch: Option<String>,
    pub branches: Vec<String>,
    pub draft: bool,
    pub draft_workspace: WorkspaceMode,
    pub worktree: Option<WorktreeInfo>,
    pub preparing_worktree: bool,
    pub relay_confirmation: Option<(String, String)>,
    pub native_rewind_pending: bool,
    pub native_rewind_prefill_available: bool,
    pub model_pending_restart: bool,
    pub options_pending_restart: bool,
    pub approval_pending_restart: bool,
    pub ultrathink_armed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalStatus {
    pub id: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub exited: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSplitStatus {
    pub first: u64,
    pub second: u64,
    pub direction: TerminalSplitDirection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalContextStatus {
    pub id: u64,
    pub terminal_label: String,
    pub line_start: usize,
    pub line_end: usize,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedMessageStatus {
    pub id: u64,
    pub text: String,
    /// Unix timestamp for a scheduled row, or `None` for an ordinary queued
    /// message. Like the queue itself, this status is ephemeral.
    #[serde(default)]
    pub fire_at_unix_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexSnapshot {
    /// Working, approval, user-input and background-only flags for sidebar rows.
    #[serde(default)]
    pub activity: HashMap<String, (bool, bool, bool, bool)>,
    pub sessions: Vec<SessionMeta>,
    pub projects: Vec<Project>,
}

/// A transient runtime notification delivered to clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum RuntimeNotification {
    Error(RuntimeError),
    Notice(RuntimeNotice),
    Toast(RuntimeToast),
    Effect(RuntimeEffect),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum RuntimeEffect {
    ApplyLocale { language: Option<String> },
    CopyToClipboard { text: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RuntimeOperationId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitActionRequest {
    pub session_id: String,
    pub action: GitAction,
    pub message: Option<String>,
    pub included: Option<Vec<String>>,
    pub feature_branch: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeWorktreeFailure {
    Missing,
    DirtyWorktree,
    DestinationDetached,
    DirtyDestination,
    DivergedConflict,
    Git,
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum RuntimeToast {
    GitBusy,
    GitStarted {
        operation: RuntimeOperationId,
        action: GitAction,
    },
    GitSucceeded {
        operation: RuntimeOperationId,
        action: GitAction,
    },
    GitFailed {
        operation: RuntimeOperationId,
        detail: String,
        retry: GitActionRequest,
    },
    CommitMessageGenerated {
        message: String,
    },
    CommitMessageFailed {
        detail: String,
    },
    AcpInstallStarted {
        operation: RuntimeOperationId,
        name: String,
    },
    AcpInstallSucceeded {
        operation: RuntimeOperationId,
        name: String,
    },
    AcpInstallFailed {
        operation: RuntimeOperationId,
        name: String,
        detail: String,
    },
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum RuntimeError {
    External(String),
    PersistSettings { error: String },
    UpdateUnknown { provider: ProviderKind },
    UpdateFailed { provider: ProviderKind },
    TerminalStart { error: String },
    TerminalRestart { error: String },
    PersistProject { error: String },
    WorktreeRemove { error: String },
    DeleteSession { error: String },
    DeleteProject { error: String },
    NativeRewindBlocked,
    PersistEvent { error: String },
    WorktreeAdd { error: String },
    PersistSession { error: String },
    ProcessGone,
    SteerUnsupported { agent: String },
    DirtyTree,
    ProviderStart { error: String },
    ProviderClosed { reason: Option<String> },
    PersistSessionIndex { error: String },
    ProviderMessage(String),
    ExportThread { error: String },
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum RuntimeNotice {
    ProviderMessage(String),
    UpdateAvailable {
        provider: ProviderKind,
        version: String,
    },
    TcodeUpdateAvailable {
        version: String,
    },
    UpdatingProvider {
        provider: ProviderKind,
    },
    UpdateDone {
        provider: ProviderKind,
    },
    NativeRewindCompleted {
        mode: RewindMode,
    },
    PlanSaved {
        file: String,
    },
    SwitchedBranch {
        branch: String,
    },
    ThreadExported {
        file: String,
    },
    WorktreeSeeded {
        copied_files: usize,
        skipped: Vec<String>,
        limit_reached: bool,
    },
    WorktreeMergedFastForward,
    WorktreeMergedCommit,
    WorktreeMergeFailed {
        reason: MergeWorktreeFailure,
        detail: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeSeverity {
    Success,
    Warning,
}

impl RuntimeNotice {
    pub fn severity(&self) -> NoticeSeverity {
        match self {
            Self::ProviderMessage(_) | Self::WorktreeMergeFailed { .. } => NoticeSeverity::Warning,
            Self::UpdateAvailable { .. }
            | Self::TcodeUpdateAvailable { .. }
            | Self::UpdatingProvider { .. }
            | Self::UpdateDone { .. }
            | Self::NativeRewindCompleted { .. }
            | Self::PlanSaved { .. }
            | Self::SwitchedBranch { .. }
            | Self::ThreadExported { .. }
            | Self::WorktreeSeeded { .. }
            | Self::WorktreeMergedFastForward
            | Self::WorktreeMergedCommit => NoticeSeverity::Success,
        }
    }
}
