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
    SessionEvents {
        session_id: String,
    },
    SessionStatus {
        session_id: String,
    },
    Index,
    Settings,
    Providers,
    GitStatus,
    RuntimeEvents,
    /// The client's currently selected session/draft, including ephemeral
    /// status and serialized terminal layout metadata.
    ActiveSession,
    Terminal {
        terminal_id: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub topic: Topic,
    pub event: ServerEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum ServerEvent {
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
    ActiveSessionReplaced(Option<SessionStatus>),
    /// A provider-native rewind prompt is delivered once over the serialized
    /// event stream. The client store owns consumption after receipt.
    NativeRewindPrefill {
        session_id: String,
        text: String,
    },
    SessionSnapshot(Vec<StoredEvent>),
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
    pub sessions: Vec<SessionMeta>,
    pub projects: Vec<Project>,
}

/// Protocol-owned mirror of `tcode_runtime::event::RuntimeEvent`.
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
