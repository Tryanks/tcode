use std::path::PathBuf;

use agent::{ApprovalDecision, ApprovalMode, InteractionMode, ProviderKind, RewindMode};
use gpui::{App, Context, Task};
use tcode_core::{
    acp::AcpAgentPatch,
    git::GitAction,
    session::ReviewComment,
    settings::{
        ChildApprovalMode, ImageMode, OrchestrateChildModel, OrchestratorIdentity,
        ProfileSettingsPatch, SidebarLayout, ThemeMode,
    },
    ui::{TerminalSplitDirection, WorkspaceMode},
};
use tcode_protocol::{Command, CommandResponse, ProtocolError, SettingsPatch};

use super::{StoreChange, TopicKind, WorkspaceStore};

impl WorkspaceStore {
    pub(super) fn dispatch(&mut self, command: Command) {
        if let Err(error) = self.host.dispatch(command) {
            log::error!("failed to dispatch host command: {}", error.message);
        }
    }

    pub(super) fn command(
        &self,
        command: Command,
        cx: &mut App,
    ) -> Task<Result<CommandResponse, ProtocolError>> {
        let host = self.host.clone();
        #[cfg(test)]
        {
            let result = smol::block_on(host.command(command));
            cx.spawn(async move |_| result)
        }
        #[cfg(not(test))]
        {
            cx.spawn(async move |_| host.command(command).await)
        }
    }
}

// Settings intents (27).
impl WorkspaceStore {
    fn patch_settings(&mut self, patch: SettingsPatch) {
        self.dispatch(Command::PatchSettings { patch });
    }

    pub fn set_language(&mut self, value: Option<String>) {
        self.patch_settings(SettingsPatch::Language(value));
    }
    pub fn set_theme_mode(&mut self, value: ThemeMode) {
        self.patch_settings(SettingsPatch::ThemeMode(value));
    }
    pub fn set_word_wrap_diffs(&mut self, value: bool) {
        self.patch_settings(SettingsPatch::WordWrapDiffs(value));
    }
    pub fn set_skip_delete_confirmation(&mut self, value: bool) {
        self.patch_settings(SettingsPatch::SkipDeleteConfirmation(value));
    }
    pub fn set_auto_open_task_panel(&mut self, value: bool) {
        self.patch_settings(SettingsPatch::AutoOpenTaskPanel(value));
    }
    pub fn set_live_command_panel_disabled(&mut self, value: bool) {
        self.patch_settings(SettingsPatch::LiveCommandPanelDisabled(value));
    }
    pub fn set_provider_update_checks_disabled(&mut self, value: bool) {
        self.patch_settings(SettingsPatch::ProviderUpdateChecksDisabled(value));
    }
    pub fn set_inactive_frame_throttle_disabled(&mut self, value: bool) {
        self.patch_settings(SettingsPatch::InactiveFrameThrottleDisabled(value));
    }
    pub fn set_auto_archive_disabled(&mut self, value: bool) {
        self.patch_settings(SettingsPatch::AutoArchiveDisabled(value));
    }
    pub fn set_auto_archive_max_idle_days(&mut self, value: u32) {
        self.patch_settings(SettingsPatch::AutoArchiveMaxIdleDays(value));
    }
    pub fn set_auto_archive_keep_count(&mut self, value: usize) {
        self.patch_settings(SettingsPatch::AutoArchiveKeepCount(value));
    }
    pub fn set_auto_archive_notice_shown(&mut self, value: bool) {
        self.patch_settings(SettingsPatch::AutoArchiveNoticeShown(value));
    }
    pub fn set_orchestrate_generic_identity(&mut self, value: String) {
        self.patch_settings(SettingsPatch::OrchestrateGenericIdentity(value));
    }
    pub fn set_orchestrate_model_identities(&mut self, value: Vec<OrchestratorIdentity>) {
        self.patch_settings(SettingsPatch::OrchestrateModelIdentities(value));
    }
    pub fn set_orchestrate_child_models(&mut self, value: Vec<OrchestrateChildModel>) {
        self.patch_settings(SettingsPatch::OrchestrateChildModels(value));
    }
    pub fn set_orchestrate_child_approval(&mut self, value: ChildApprovalMode) {
        self.patch_settings(SettingsPatch::OrchestrateChildApproval(value));
    }
    pub fn set_orchestrate_child_worktrees(&mut self, value: bool) {
        self.patch_settings(SettingsPatch::OrchestrateChildWorktrees(value));
    }
    pub fn set_orchestrate_archive_on_complete(&mut self, value: bool) {
        self.patch_settings(SettingsPatch::OrchestrateArchiveOnComplete(value));
    }
    pub fn set_computer_use_enabled(&mut self, value: bool) {
        self.patch_settings(SettingsPatch::ComputerUseEnabled(value));
    }
    pub fn set_computer_use_image_mode(&mut self, value: ImageMode) {
        self.patch_settings(SettingsPatch::ComputerUseImageMode(value));
    }
    pub fn set_computer_use_allow_input(&mut self, value: bool) {
        self.patch_settings(SettingsPatch::ComputerUseAllowInput(value));
    }
    pub fn set_browser_enabled(&mut self, value: bool) {
        self.patch_settings(SettingsPatch::BrowserEnabled(value));
    }
    pub fn set_browser_home_url(&mut self, value: Option<String>) {
        self.patch_settings(SettingsPatch::BrowserHomeUrl(value));
    }
    pub fn set_browser_allow_evaluate(&mut self, value: bool) {
        self.patch_settings(SettingsPatch::BrowserAllowEvaluate(value));
    }
    pub fn set_title_generation_provider(&mut self, value: ProviderKind) {
        self.patch_settings(SettingsPatch::TitleGenerationProvider(value));
    }
    pub fn set_title_generation_model(&mut self, value: String) {
        self.patch_settings(SettingsPatch::TitleGenerationModel(value));
    }
    pub fn set_title_generation_profile_id(&mut self, value: Option<String>) {
        self.patch_settings(SettingsPatch::TitleGenerationProfileId(value));
    }
    pub fn set_sidebar_layout(&mut self, value: SidebarLayout) {
        self.patch_settings(SettingsPatch::SidebarLayout(value));
    }
    pub fn reset_settings(&mut self) {
        self.dispatch(Command::ResetSettings);
    }
    pub fn write_relaunch_marker(&mut self, reopen_settings: String) {
        self.dispatch(Command::WriteRelaunchMarker { reopen_settings });
    }
    pub fn set_sidebar_collapsed(&mut self, collapsed: bool) {
        self.dispatch(Command::SetSidebarCollapsed { collapsed });
    }
}

// Session and turn intents (27).
impl WorkspaceStore {
    pub fn archive_session(&mut self, session_id: String) {
        self.dispatch(Command::ArchiveSession { session_id });
    }
    pub fn unarchive_session(&mut self, session_id: String) {
        self.dispatch(Command::UnarchiveSession { session_id });
    }
    pub fn auto_archive_sweep(
        &self,
        project_id: String,
        cx: &mut App,
    ) -> Task<Result<CommandResponse, ProtocolError>> {
        self.command(Command::AutoArchiveSweep { project_id }, cx)
    }
    pub fn rename_session(&mut self, session_id: String, title: String) {
        self.dispatch(Command::RenameSession { session_id, title });
    }
    pub fn fork_thread(&mut self, id: String) {
        self.dispatch(Command::ForkThread { id });
    }
    pub fn merge_worktree(&mut self, session_id: String) {
        self.dispatch(Command::MergeWorktree { session_id });
    }
    pub fn delete_session(&mut self, session_id: String, remove_worktree: bool) {
        self.dispatch(Command::DeleteSession {
            session_id,
            remove_worktree,
        });
    }
    pub fn mark_session_unread(&mut self, session_id: String) {
        self.dispatch(Command::MarkSessionUnread { session_id });
    }
    pub fn select_session(&mut self, session_id: String) {
        self.dispatch(Command::SelectSession { session_id });
    }
    pub fn select_session_at_turn(&mut self, session_id: String, turn: usize) {
        self.pending_chat_turn = Some((session_id.clone(), turn));
        self.dispatch(Command::SelectSession { session_id });
    }
    pub fn send_turn(&mut self, text: String, attachment_paths: Vec<PathBuf>) {
        self.dispatch(Command::SendTurn {
            text,
            attachment_paths,
        });
    }
    pub fn schedule_turn(
        &mut self,
        text: String,
        attachment_paths: Vec<PathBuf>,
        fire_at_unix_secs: u64,
    ) {
        self.dispatch(Command::ScheduleTurn {
            text,
            attachment_paths,
            fire_at_unix_secs,
        });
    }
    pub fn confirm_relay_and_send(&mut self, text: String, attachment_paths: Vec<PathBuf>) {
        self.dispatch(Command::ConfirmRelayAndSend {
            text,
            attachment_paths,
        });
    }
    pub fn orchestrate_turn(&mut self, text: String, attachment_paths: Vec<PathBuf>) {
        self.dispatch(Command::OrchestrateTurn {
            text,
            attachment_paths,
        });
    }
    pub fn steer(&mut self, text: String, attachment_paths: Vec<PathBuf>) {
        self.dispatch(Command::Steer {
            text,
            attachment_paths,
        });
    }
    pub fn steer_queued(&mut self, id: u64) {
        self.dispatch(Command::SteerQueued { id });
    }
    pub fn drop_queued(&mut self, id: u64) {
        self.dispatch(Command::DropQueued { id });
    }
    pub fn interrupt(&mut self) {
        self.dispatch(Command::Interrupt);
    }
    pub fn respond_approval(&mut self, request_id: String, decision: ApprovalDecision) {
        self.dispatch(Command::RespondApproval {
            request_id,
            decision,
        });
    }
    pub fn respond_user_input(
        &mut self,
        request_id: String,
        answers: serde_json::Map<String, serde_json::Value>,
    ) {
        self.dispatch(Command::RespondUserInput {
            request_id,
            answers,
        });
    }
    pub fn rewind_turn(&mut self, turn: usize, mode: RewindMode) {
        self.dispatch(Command::RewindTurn { turn, mode });
    }
    pub fn add_review_comment(&mut self, comment: ReviewComment) {
        self.dispatch(Command::AddReviewComment { comment });
    }
    pub fn remove_review_comment(&mut self, index: usize) {
        self.dispatch(Command::RemoveReviewComment { index });
    }
    pub fn implement_plan(&mut self) {
        self.dispatch(Command::ImplementPlan);
    }
    pub fn dismiss_plan(&mut self) {
        self.dispatch(Command::DismissPlan);
    }
    pub fn implement_plan_in_new_thread(&mut self, title: String) {
        self.dispatch(Command::ImplementPlanInNewThread { title });
    }
    pub fn copy_plan(&mut self, markdown: String) {
        self.dispatch(Command::CopyPlan { markdown });
    }
    pub fn save_plan_to_workspace(&mut self, markdown: String) {
        self.dispatch(Command::SavePlanToWorkspace { markdown });
    }
    pub fn download_plan(&mut self, markdown: String, fallback_title: String) {
        self.dispatch(Command::DownloadPlan {
            markdown,
            fallback_title,
        });
    }
}

// Project and git intents (10).
impl WorkspaceStore {
    pub fn create_project(
        &self,
        root: PathBuf,
        cx: &mut App,
    ) -> Task<Result<CommandResponse, ProtocolError>> {
        self.command(Command::CreateProject { root }, cx)
    }
    pub fn finish_external_import(&mut self, project_id: String) {
        self.dispatch(Command::FinishExternalImport { project_id });
    }
    pub fn export_thread(
        &mut self,
        session_id: String,
        destination: PathBuf,
        format: tcode_protocol::ThreadExportFormat,
    ) {
        self.dispatch(Command::ExportThread {
            session_id,
            destination,
            format,
        });
    }
    pub fn toggle_project_collapsed(&mut self, project_id: String) {
        self.dispatch(Command::ToggleProjectCollapsed { project_id });
    }
    pub fn delete_project(&mut self, project_id: String) {
        self.dispatch(Command::DeleteProject { project_id });
    }
    pub fn start_draft(&mut self, project_id: String, cwd: PathBuf) {
        self.dispatch(Command::StartDraft { project_id, cwd });
    }
    pub fn set_draft_workspace(&mut self, mode: WorkspaceMode) {
        self.dispatch(Command::SetDraftWorkspace { mode });
    }
    pub fn run_git_action(
        &mut self,
        action: GitAction,
        message: Option<String>,
        included: Option<Vec<String>>,
        feature_branch: Option<String>,
    ) {
        self.dispatch(Command::RunGitAction {
            action,
            message,
            included,
            feature_branch,
        });
    }
    pub fn load_branches(&mut self) {
        self.dispatch(Command::LoadBranches);
    }
    pub fn checkout_branch(&mut self, branch: String) {
        self.dispatch(Command::CheckoutBranch { branch });
    }
    pub fn cycle_project_sort(&mut self) {
        self.dispatch(Command::CycleProjectSort);
    }
}

// Terminal intents (10).
impl WorkspaceStore {
    pub fn toggle_terminal_panel(&mut self, cx: &mut Context<Self>) {
        let opening = !self
            .active_conversation_ui()
            .is_some_and(|ui| ui.terminal_open);
        if let Some(ui) = self.active_conversation_ui_mut() {
            ui.terminal_open = opening;
        }
        if opening {
            self.dispatch(Command::ToggleTerminalPanel);
        } else {
            self.dispatch(Command::CloseTerminalPanel);
        }
        cx.emit(StoreChange {
            topic: TopicKind::ActiveSession,
        });
        cx.notify();
    }
    pub fn close_terminal_panel(&mut self, cx: &mut Context<Self>) {
        if let Some(ui) = self.active_conversation_ui_mut() {
            ui.terminal_open = false;
        }
        self.dispatch(Command::CloseTerminalPanel);
        cx.emit(StoreChange {
            topic: TopicKind::ActiveSession,
        });
        cx.notify();
    }
    pub fn set_terminal_height(&mut self, height: f32, cx: &mut Context<Self>) {
        if let Some(ui) = self.active_conversation_ui_mut() {
            ui.terminal_height = height;
        }
        self.dispatch(Command::SetTerminalHeight { height });
        cx.emit(StoreChange {
            topic: TopicKind::ActiveSession,
        });
        cx.notify();
    }
    pub fn close_terminal(&mut self, terminal_id: u64, cx: &mut Context<Self>) {
        let closes_drawer = self
            .session_status_replica
            .as_ref()
            .is_some_and(|status| status.terminals.len() <= 1);
        if closes_drawer && let Some(ui) = self.active_conversation_ui_mut() {
            ui.terminal_open = false;
        }
        self.dispatch(Command::CloseTerminal { terminal_id });
        cx.emit(StoreChange {
            topic: TopicKind::ActiveSession,
        });
        cx.notify();
    }
    pub fn restart_terminal(&mut self) {
        self.dispatch(Command::RestartTerminal);
    }
    pub fn new_terminal(&mut self) {
        self.dispatch(Command::NewTerminal);
    }
    pub fn split_terminal(&mut self, direction: TerminalSplitDirection) {
        self.dispatch(Command::SplitTerminal { direction });
    }
    pub fn activate_terminal(&mut self, terminal_id: u64) {
        self.dispatch(Command::ActivateTerminal { terminal_id });
    }
    pub fn capture_terminal_selection(&mut self, terminal_id: u64) {
        self.dispatch(Command::CaptureTerminalSelection { terminal_id });
    }
    pub fn remove_terminal_context(&mut self, context_id: u64) {
        self.dispatch(Command::RemoveTerminalContext { context_id });
    }
}

// Provider intents (12).
impl WorkspaceStore {
    pub fn refresh_provider_status(&mut self) {
        self.dispatch(Command::RefreshProviderStatus);
    }
    pub fn check_provider_versions(&mut self) {
        self.dispatch(Command::CheckProviderVersions);
    }
    pub fn reload_provider(&mut self) {
        self.dispatch(Command::ReloadProvider);
    }
    pub fn set_profile_secret(&mut self, profile_id: String, name: String, value: Option<String>) {
        self.dispatch(Command::SetProfileSecret {
            profile_id,
            name,
            value,
        });
    }
    pub fn update_profile_settings(&mut self, profile_id: String, patch: ProfileSettingsPatch) {
        self.dispatch(Command::UpdateProfileSettings { profile_id, patch });
    }
    pub fn create_third_party_profile(
        &mut self,
        name: String,
        base_url: String,
        model: Option<String>,
        api_key: String,
    ) {
        self.dispatch(Command::CreateThirdPartyProfile {
            name,
            base_url,
            model,
            api_key,
        });
    }
    pub fn delete_profile(&mut self, profile_id: String) {
        self.dispatch(Command::DeleteProfile { profile_id });
    }
    pub fn update_provider(&mut self, provider: ProviderKind) {
        self.dispatch(Command::UpdateProvider { provider });
    }
    pub fn set_active_model(
        &mut self,
        provider: ProviderKind,
        model: Option<String>,
        profile_id: Option<String>,
    ) {
        self.dispatch(Command::SetActiveModel {
            provider,
            model,
            profile_id,
        });
    }
    pub fn toggle_favorite_model(&mut self, model: String) {
        self.dispatch(Command::ToggleFavoriteModel { model });
    }
    pub fn set_active_option(&mut self, id: String, value: Option<serde_json::Value>) {
        self.dispatch(Command::SetActiveOption { id, value });
    }
    pub fn select_ultrathink(&mut self) {
        self.dispatch(Command::SelectUltrathink);
    }
}

// ACP intents (6).
impl WorkspaceStore {
    pub fn refresh_acp_registry(&mut self) {
        self.dispatch(Command::RefreshAcpRegistry);
    }
    pub fn install_acp_agent(&mut self, id: String) {
        self.dispatch(Command::InstallAcpAgent { id });
    }
    pub fn remove_acp_agent(&mut self, id: String) {
        self.dispatch(Command::RemoveAcpAgent { id });
    }
    pub fn add_custom_acp_agent(
        &mut self,
        name: String,
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    ) {
        self.dispatch(Command::AddCustomAcpAgent {
            name,
            command,
            args,
            env,
        });
    }
    pub fn update_acp_agent(&mut self, id: String, patch: AcpAgentPatch) {
        self.dispatch(Command::UpdateAcpAgent { id, patch });
    }
    pub fn set_active_acp_agent(&mut self, id: String) {
        self.dispatch(Command::SetActiveAcpAgent { id });
    }
}

// Composer mode intents (3).
impl WorkspaceStore {
    pub fn set_interaction_mode(&mut self, mode: InteractionMode) {
        self.dispatch(Command::SetInteractionMode { mode });
    }
    pub fn toggle_interaction_mode(&mut self) {
        self.dispatch(Command::ToggleInteractionMode);
    }
    pub fn set_active_approval_mode(&mut self, mode: ApprovalMode) {
        self.dispatch(Command::SetActiveApprovalMode { mode });
    }
}
