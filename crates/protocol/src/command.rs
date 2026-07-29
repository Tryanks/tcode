use std::path::PathBuf;

use agent::{ApprovalDecision, ApprovalMode, InteractionMode, ProviderKind, RewindMode};
use serde::{Deserialize, Serialize};
use tcode_core::{git::GitAction, settings::Settings};

/// A backend mutation requested by a client.
///
/// Variants correspond to serializable `AppState` mutations used by the UI.
/// UI-only consuming selectors and methods whose signatures contain
/// non-serializable types are intentionally absent.
#[non_exhaustive]
#[allow(clippy::large_enum_variant)] // Wire DTOs preserve direct, typed payload fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum Command {
    OrchestrateTurn {
        text: String,
        attachment_paths: Vec<PathBuf>,
    },
    ReloadProvider {
        provider: ProviderKind,
    },
    SetProfileSecret {
        profile_id: String,
        name: String,
        value: Option<String>,
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
    SetActiveAcpAgent {
        id: String,
    },
    ResetSettings,
    ToggleDiffPanel,
    OpenDiffForTurn {
        turn: usize,
    },
    OpenDiffForFile {
        turn: usize,
        path: String,
    },
    DiscardDiffFocus,
    SetTerminalHeight {
        height: f32,
    },
    ToggleTerminalPanel,
    CloseTerminalPanel,
    RestartTerminal,
    NewTerminal,
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
    CloseDiffPanel,
    RemoveReviewComment {
        index: usize,
    },
    ToggleDiffExpanded,
    CycleProjectSort,
    CreateProject {
        root: PathBuf,
    },
    FinishExternalImport {
        project_id: String,
    },
    ToggleProjectCollapsed {
        project_id: String,
    },
    UpdateSettings {
        settings: Settings,
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
    SelectSession {
        session_id: String,
    },
    SendTurn {
        text: String,
        attachment_paths: Vec<PathBuf>,
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
    TogglePlanPanel,
    TogglePreviewPanel,
    ClosePreviewPanel,
    OpenPreviewPanel,
    OpenPreviewPanelFor {
        session_id: String,
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
