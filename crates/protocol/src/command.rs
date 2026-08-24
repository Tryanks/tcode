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
    /// Consume and apply the restart-continuity marker, returning the Settings
    /// section that the client should open.
    ApplyPendingRelaunch,
    /// Open the newest stored session without exposing the host's index.
    OpenLatestSession,
    /// Shut down every live provider and PTY, then acknowledge only after the
    /// FIFO store-write barrier has drained.
    ShutdownAllAndFlush,
    OrchestrateTurn {
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
    CheckProviderVersions,
    UpdateProvider {
        provider: ProviderKind,
    },
    SetSidebarCollapsed {
        collapsed: bool,
    },
    RunGitAction {
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
        id: String,
    },
    ResetSettings,
    WriteRelaunchMarker {
        reopen_settings: String,
    },
    SetTerminalHeight {
        height: f32,
    },
    ToggleTerminalPanel,
    CloseTerminalPanel,
    RestartTerminal,
    NewTerminal,
    SplitTerminal {
        direction: TerminalSplitDirection,
    },
    ActivateTerminal {
        terminal_id: u64,
    },
    CloseTerminal {
        terminal_id: u64,
    },
    CaptureTerminalSelection {
        terminal_id: u64,
    },
    RemoveTerminalContext {
        context_id: u64,
    },
    AddReviewComment {
        comment: ReviewComment,
    },
    RemoveReviewComment {
        index: usize,
    },
    CycleProjectSort,
    CreateProject {
        root: PathBuf,
    },
    /// The command itself is ordinary serialized protocol traffic. Its
    /// progress is routed by request id over the one local bus installed at
    /// host construction; a remote transport must replace it with events.
    StartExternalImport {
        project_id: String,
        threads: Vec<ExternalThread>,
    },
    FinishExternalImport {
        project_id: String,
    },
    ExportThread {
        session_id: String,
        destination: PathBuf,
        format: ThreadExportFormat,
    },
    ToggleProjectCollapsed {
        project_id: String,
    },
    PatchSettings {
        patch: SettingsPatch,
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
        mode: WorkspaceMode,
    },
    SelectSession {
        session_id: String,
    },
    SendTurn {
        text: String,
        attachment_paths: Vec<PathBuf>,
    },
    /// Keep a user-authored turn in the session's in-memory queue until the
    /// given Unix timestamp. Scheduled turns deliberately share the ordinary
    /// queue and are not persisted as conversation events before delivery.
    ScheduleTurn {
        text: String,
        attachment_paths: Vec<PathBuf>,
        fire_at_unix_secs: u64,
    },
    ConfirmRelayAndSend {
        text: String,
        attachment_paths: Vec<PathBuf>,
    },
    Steer {
        text: String,
        attachment_paths: Vec<PathBuf>,
    },
    SteerQueued {
        id: u64,
    },
    DropQueued {
        id: u64,
    },
    Interrupt,
    RespondApproval {
        request_id: String,
        decision: ApprovalDecision,
    },
    RespondUserInput {
        request_id: String,
        answers: serde_json::Map<String, serde_json::Value>,
    },
    SetActiveModel {
        provider: ProviderKind,
        model: Option<String>,
        profile_id: Option<String>,
    },
    SetActiveOption {
        id: String,
        value: Option<serde_json::Value>,
    },
    SelectUltrathink,
    SetInteractionMode {
        mode: InteractionMode,
    },
    ToggleInteractionMode,
    ImplementPlan,
    DismissPlan,
    ImplementPlanInNewThread {
        title: String,
    },
    CopyPlan {
        markdown: String,
    },
    SavePlanToWorkspace {
        markdown: String,
    },
    DownloadPlan {
        markdown: String,
        fallback_title: String,
    },
    LoadBranches,
    CheckoutBranch {
        branch: String,
    },
    SetActiveApprovalMode {
        mode: ApprovalMode,
    },
    ToggleFavoriteModel {
        model: String,
    },
    RewindTurn {
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
    PendingRelaunchSection(Option<String>),
    ArchivedCount(usize),
    ExternalImportStarted(bool),
}
