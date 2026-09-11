use std::path::PathBuf;

use agent::{ApprovalDecision, ApprovalMode, InteractionMode, ProviderKind, RewindMode};
use serde::{Deserialize, Serialize};
use tcode_core::{
    acp::AcpAgentPatch,
    git::GitAction,
    session::ReviewComment,
    settings::ProfileSettingsPatch,
    ui::{TerminalSplitDirection, WorkspaceMode},
};

pub use tcode_core::settings::SettingsPatch;

use crate::ExternalThread;

/// Fallback cell metrics matching the host emulator's own defaults, used when
/// a client resizes without knowing its physical cell size.
fn default_cell_width() -> u16 {
    8
}

fn default_cell_height() -> u16 {
    17
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSelection {
    pub line_start: usize,
    pub line_end: usize,
    pub text: String,
}

/// User-selectable thread export formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadExportFormat {
    /// A tcode metadata record followed by the session's native JSONL event log.
    Jsonl,
    /// A readable, deterministic rendering of the folded conversation timeline.
    Markdown,
}

/// A backend mutation requested by a client.
///
/// Variants correspond to serializable `AppState` mutations used by the UI.
/// UI-only consuming selectors are intentionally absent.
#[allow(clippy::large_enum_variant)] // Wire DTOs preserve direct, typed payload fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum Command {
    TerminalInput {
        terminal_id: u64,
        #[serde(with = "crate::wire::base64_bytes")]
        bytes: Vec<u8>,
    },
    ResizeTerminal {
        terminal_id: u64,
        cols: u16,
        rows: u16,
        /// Physical cell size, which the host needs to answer a program's
        /// pixel-size queries (CSI 14 t).
        #[serde(default = "default_cell_width")]
        cell_width: u16,
        #[serde(default = "default_cell_height")]
        cell_height: u16,
    },
    /// Clear the host grid and its scrollback, keeping the current prompt line.
    ClearTerminal {
        terminal_id: u64,
    },
    PreviewReply {
        request_id: u64,
        response: Result<crate::PreviewResponse, String>,
    },
    /// Consume and apply the restart-continuity marker, returning the Settings
    /// section that the client should open.
    ApplyPendingRelaunch,
    /// Open the newest stored session without exposing the host's index.
    OpenLatestSession,
    /// Shut down every live provider and PTY, then acknowledge only after the
    /// FIFO store-write barrier has drained.
    ShutdownAllAndFlush,
    OrchestrateTurn {
        session_id: String,
        text: String,
        attachment_paths: Vec<PathBuf>,
    },
    ReloadProvider,
    SetProfileSecret {
        profile_id: String,
        name: String,
        value: Option<String>,
    },
    UpdateProfileSettings {
        profile_id: String,
        patch: ProfileSettingsPatch,
    },
    CreateThirdPartyProfile {
        name: String,
        base_url: String,
        model: Option<String>,
        api_key: String,
    },
    DeleteProfile {
        profile_id: String,
    },
    RefreshProviderStatus,
    /// Re-fetch account usage / rate-limit windows for every usage-capable
    /// provider profile (Codex, Claude Code).
    RefreshProviderUsage,
    CheckProviderVersions,
    UpdateProvider {
        provider: ProviderKind,
    },
    SetSidebarCollapsed {
        collapsed: bool,
    },
    RunGitAction {
        session_id: String,
        action: GitAction,
        message: Option<String>,
        included: Option<Vec<String>>,
        feature_branch: Option<String>,
    },
    RefreshAcpRegistry,
    InstallAcpAgent {
        id: String,
    },
    RemoveAcpAgent {
        id: String,
    },
    AddCustomAcpAgent {
        name: String,
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    UpdateAcpAgent {
        id: String,
        patch: AcpAgentPatch,
    },
    SetActiveAcpAgent {
        session_id: String,
        id: String,
    },
    ResetSettings,
    WriteRelaunchMarker {
        session_id: String,
        reopen_settings: String,
    },
    ClearRelaunchMarker,
    SetTerminalHeight {
        session_id: String,
        height: f32,
    },
    ToggleTerminalPanel {
        session_id: String,
    },
    CloseTerminalPanel {
        session_id: String,
    },
    RestartTerminal {
        session_id: String,
    },
    NewTerminal {
        session_id: String,
    },
    SplitTerminal {
        session_id: String,
        direction: TerminalSplitDirection,
    },
    ActivateTerminal {
        session_id: String,
        terminal_id: u64,
    },
    CloseTerminal {
        session_id: String,
        terminal_id: u64,
    },
    CaptureTerminalSelection {
        session_id: String,
        terminal_id: u64,
        /// Client-grid selection; absent retains the local-handle behavior.
        #[serde(default)]
        selection: Option<TerminalSelection>,
    },
    RemoveTerminalContext {
        session_id: String,
        context_id: u64,
    },
    AddReviewComment {
        session_id: String,
        comment: ReviewComment,
    },
    RemoveReviewComment {
        session_id: String,
        index: usize,
    },
    CycleProjectSort,
    /// Register a project rooted at `root`. The host validates the path against
    /// its own filesystem — a client never decides whether a host path is
    /// absolute or exists — and answers `invalid_project_root` when it is not an
    /// absolute, existing directory there.
    CreateProject {
        root: PathBuf,
    },
    /// Start an import run. Progress, completion and the finalized index are
    /// host-owned: subscribe to [`crate::Topic::ExternalImport`] before sending
    /// this, and read the outcome from that replicated status. Returns
    /// [`CommandResponse::ExternalImportStarted(false)`] for an unknown
    /// project, and an `import_in_progress` error for a second concurrent run.
    StartExternalImport {
        project_id: String,
        threads: Vec<ExternalThread>,
    },
    ToggleProjectCollapsed {
        project_id: String,
    },
    PatchSettings {
        patch: SettingsPatch,
    },
    SettleSession {
        session_id: String,
    },
    MakeSessionActive {
        session_id: String,
    },
    ArchiveSession {
        session_id: String,
    },
    UnarchiveSession {
        session_id: String,
    },
    AutoArchiveSweep {
        project_id: String,
    },
    RenameSession {
        session_id: String,
        title: String,
    },
    ForkThread {
        id: String,
    },
    DeleteSession {
        session_id: String,
        remove_worktree: bool,
    },
    MergeWorktree {
        session_id: String,
    },
    DeleteProject {
        project_id: String,
    },
    MarkSessionUnread {
        session_id: String,
    },
    StartDraft {
        project_id: String,
        cwd: PathBuf,
    },
    SetDraftWorkspace {
        session_id: String,
        mode: WorkspaceMode,
    },

    SendTurn {
        session_id: String,
        text: String,
        attachment_paths: Vec<PathBuf>,
    },
    /// Keep a user-authored turn in the session's in-memory queue until the
    /// given Unix timestamp. Scheduled turns deliberately share the ordinary
    /// queue and are not persisted as conversation events before delivery.
    ScheduleTurn {
        session_id: String,
        text: String,
        attachment_paths: Vec<PathBuf>,
        fire_at_unix_secs: u64,
    },
    ConfirmRelayAndSend {
        session_id: String,
        text: String,
        attachment_paths: Vec<PathBuf>,
    },
    Steer {
        session_id: String,
        text: String,
        attachment_paths: Vec<PathBuf>,
    },
    SteerQueued {
        session_id: String,
        id: u64,
    },
    DropQueued {
        session_id: String,
        id: u64,
    },
    Interrupt {
        session_id: String,
    },
    RespondApproval {
        session_id: String,
        request_id: String,
        decision: ApprovalDecision,
    },
    RespondUserInput {
        session_id: String,
        request_id: String,
        answers: serde_json::Map<String, serde_json::Value>,
    },
    SetActiveModel {
        session_id: String,
        provider: ProviderKind,
        model: Option<String>,
        profile_id: Option<String>,
    },
    SetActiveOption {
        session_id: String,
        id: String,
        value: Option<serde_json::Value>,
    },
    SelectUltrathink {
        session_id: String,
    },
    SetInteractionMode {
        session_id: String,
        mode: InteractionMode,
    },
    ToggleInteractionMode {
        session_id: String,
    },
    ImplementPlan {
        session_id: String,
    },
    DismissPlan {
        session_id: String,
    },
    ImplementPlanInNewThread {
        session_id: String,
        title: String,
    },
    CopyPlan {
        markdown: String,
    },
    SavePlanToWorkspace {
        session_id: String,
        markdown: String,
    },
    DownloadPlan {
        session_id: String,
        markdown: String,
        fallback_title: String,
    },
    LoadBranches {
        session_id: String,
    },
    CheckoutBranch {
        session_id: String,
        branch: String,
    },
    SetActiveApprovalMode {
        session_id: String,
        mode: ApprovalMode,
    },
    ToggleFavoriteModel {
        model: String,
    },
    RewindTurn {
        session_id: String,
        turn: usize,
        mode: RewindMode,
    },
}

/// Correlated result of a [`Command`].
///
/// Most mutations return [`CommandResponse::Unit`]. Keeping the few
/// result-bearing operations on the command plane avoids disguising mutations
/// as queries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum CommandResponse {
    Unit,
    ProjectId(Option<String>),
    SessionId(Option<String>),
    PendingRelaunchSection {
        section: Option<String>,
        session_id: Option<String>,
    },
    ArchivedCount(usize),
    ExternalImportStarted(bool),
}

impl Command {
    /// The thread whose state this command addresses, for delivery navigation.
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::OrchestrateTurn { session_id, .. }
            | Self::RunGitAction { session_id, .. }
            | Self::SetActiveAcpAgent { session_id, .. }
            | Self::SetTerminalHeight { session_id, .. }
            | Self::ToggleTerminalPanel { session_id, .. }
            | Self::CloseTerminalPanel { session_id, .. }
            | Self::RestartTerminal { session_id, .. }
            | Self::NewTerminal { session_id, .. }
            | Self::SplitTerminal { session_id, .. }
            | Self::ActivateTerminal { session_id, .. }
            | Self::CloseTerminal { session_id, .. }
            | Self::CaptureTerminalSelection { session_id, .. }
            | Self::RemoveTerminalContext { session_id, .. }
            | Self::AddReviewComment { session_id, .. }
            | Self::RemoveReviewComment { session_id, .. }
            | Self::SettleSession { session_id, .. }
            | Self::MakeSessionActive { session_id, .. }
            | Self::ArchiveSession { session_id, .. }
            | Self::UnarchiveSession { session_id, .. }
            | Self::RenameSession { session_id, .. }
            | Self::DeleteSession { session_id, .. }
            | Self::MergeWorktree { session_id, .. }
            | Self::MarkSessionUnread { session_id, .. }
            | Self::SetDraftWorkspace { session_id, .. }
            | Self::SendTurn { session_id, .. }
            | Self::ScheduleTurn { session_id, .. }
            | Self::ConfirmRelayAndSend { session_id, .. }
            | Self::Steer { session_id, .. }
            | Self::SteerQueued { session_id, .. }
            | Self::DropQueued { session_id, .. }
            | Self::Interrupt { session_id, .. }
            | Self::RespondApproval { session_id, .. }
            | Self::RespondUserInput { session_id, .. }
            | Self::SetActiveModel { session_id, .. }
            | Self::SetActiveOption { session_id, .. }
            | Self::SelectUltrathink { session_id, .. }
            | Self::SetInteractionMode { session_id, .. }
            | Self::ToggleInteractionMode { session_id, .. }
            | Self::ImplementPlan { session_id, .. }
            | Self::DismissPlan { session_id, .. }
            | Self::ImplementPlanInNewThread { session_id, .. }
            | Self::SavePlanToWorkspace { session_id, .. }
            | Self::DownloadPlan { session_id, .. }
            | Self::LoadBranches { session_id, .. }
            | Self::CheckoutBranch { session_id, .. }
            | Self::SetActiveApprovalMode { session_id, .. }
            | Self::RewindTurn { session_id, .. } => Some(session_id),
            Self::ForkThread { id } => Some(id),
            _ => None,
        }
    }

    /// Idempotent controls and reads do not need retained delivery.
    /// All other variants are retained writes, including settings assignments:
    /// repeating an old assignment after a newer one would undo user intent.
    pub fn requires_delivery_key(&self) -> bool {
        !matches!(
            self,
            Self::ResizeTerminal { .. }
                | Self::PreviewReply { .. }
                | Self::ShutdownAllAndFlush
                | Self::OpenLatestSession
                | Self::RefreshProviderStatus
                | Self::RefreshProviderUsage
                | Self::CheckProviderVersions
                | Self::RefreshAcpRegistry
                | Self::LoadBranches { .. }
                | Self::CopyPlan { .. }
                | Self::DownloadPlan { .. }
        )
    }
}
