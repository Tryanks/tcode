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
            let result = futures_lite::future::block_on(host.command(command));
            cx.spawn(async move |_| result)
        }
        #[cfg(not(test))]
        {
            cx.spawn(async move |_| host.command(command).await)
        }
    }
}

// Settings intents (32).
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
    pub fn set_abort_on_model_fallback(&mut self, value: bool) {
        self.patch_settings(SettingsPatch::AbortOnModelFallback(value));
    }
    pub fn set_fallback_review_advisor(&mut self, value: bool) {
        self.patch_settings(SettingsPatch::FallbackReviewAdvisor(value));
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
    pub fn set_computer_use_allow_foreground_fallback(&mut self, value: bool) {
        self.patch_settings(SettingsPatch::ComputerUseAllowForegroundFallback(value));
    }
    pub fn set_computer_use_show_agent_cursor(&mut self, value: bool) {
        self.patch_settings(SettingsPatch::ComputerUseShowAgentCursor(value));
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
    pub fn set_fallback_review_provider(&mut self, value: ProviderKind) {
        self.patch_settings(SettingsPatch::FallbackReviewProvider(value));
    }
    pub fn set_fallback_review_model(&mut self, value: String) {
        self.patch_settings(SettingsPatch::FallbackReviewModel(value));
    }
    pub fn set_fallback_review_profile_id(&mut self, value: Option<String>) {
        self.patch_settings(SettingsPatch::FallbackReviewProfileId(value));
    }
    pub fn set_sidebar_layout(&mut self, value: SidebarLayout) {
        self.patch_settings(SettingsPatch::SidebarLayout(value));
    }
    pub fn set_remote_hosting_enabled(&mut self, value: bool) {
        self.patch_settings(SettingsPatch::RemoteHostingEnabled(value));
    }
    pub fn set_remote_port(&mut self, value: Option<u16>) {
        self.patch_settings(SettingsPatch::RemotePort(value));
    }
    pub fn set_remote_host_name(&mut self, value: Option<String>) {
        self.patch_settings(SettingsPatch::RemoteHostName(value));
    }
    pub fn reset_settings(&mut self) {
        self.dispatch(Command::ResetSettings);
    }
    pub fn write_relaunch_marker(&mut self, reopen_settings: String) {
        self.dispatch(Command::WriteRelaunchMarker {
            session_id: self.active_session_id().unwrap_or_default(),
            reopen_settings,
        });
    }
    pub fn clear_relaunch_marker(&mut self) {
        self.dispatch(Command::ClearRelaunchMarker);
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
    pub fn fork_thread(&mut self, id: String, cx: &mut Context<Self>) {
        self.create_and_select(Command::ForkThread { id }, cx);
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
    pub(super) fn leave_session(&mut self) {
        #[cfg(feature = "terminal")]
        self.clear_terminal_topics();
        if let Some(status) = &self.session_status_replica
            && !status.draft
        {
            self.background_session_flags.insert(
                status.session_id.clone(),
                (
                    status.working,
                    status.pending_approval,
                    status.pending_user_input,
                    Self::status_background_only(status),
                ),
            );
        }
        if let Some(session_id) = self.selected_session_id.take() {
            for topic in [
                tcode_protocol::Topic::SessionEvents {
                    session_id: session_id.clone(),
                },
                tcode_protocol::Topic::SessionStatus {
                    session_id: session_id.clone(),
                },
                #[cfg(feature = "desktop")]
                tcode_protocol::Topic::Preview {
                    session_id: session_id.clone(),
                },
                tcode_protocol::Topic::GitStatus { session_id },
            ] {
                let _ = self
                    .host
                    .unsubscribe(tcode_protocol::Subscription { topic, after: None });
            }
        }
        self.session_status_replica = None;
        self.session_replica = None;
        self.active_destination = None;
        self.git_status_replica = Default::default();
    }

    pub fn select_session(&mut self, session_id: String) {
        if self.selected_session_id.as_ref() == Some(&session_id) {
            return;
        }
        self.leave_session();
        self.selected_session_id = Some(session_id.clone());
        self.session_status_replica = self.session_statuses.get(&session_id).cloned();
        self.git_status_replica = self
            .git_statuses
            .get(&session_id)
            .cloned()
            .unwrap_or_default();
        let records = self.session_records.entry(session_id.clone()).or_default();
        self.session_replica = Some((
            session_id.clone(),
            tcode_core::session::Timeline::fold_events(records.iter().cloned()),
        ));
        let after = Some(records.len() as u64);
        for topic in [
            tcode_protocol::Topic::SessionStatus {
                session_id: session_id.clone(),
            },
            tcode_protocol::Topic::GitStatus {
                session_id: session_id.clone(),
            },
            #[cfg(feature = "desktop")]
            tcode_protocol::Topic::Preview {
                session_id: session_id.clone(),
            },
            tcode_protocol::Topic::SessionEvents { session_id },
        ] {
            let _ = self.host.subscribe(tcode_protocol::Subscription {
                after: if matches!(topic, tcode_protocol::Topic::SessionEvents { .. }) {
                    after
                } else {
                    None
                },
                topic,
            });
        }
        #[cfg(feature = "terminal")]
        self.sync_terminal_topics();
        self.sync_active_conversation_ui();
    }
    pub fn select_session_at_turn(&mut self, session_id: String, turn: usize) {
        self.pending_chat_turn = Some((session_id.clone(), turn));
        self.select_session(session_id);
    }
    pub fn send_turn(&mut self, text: String, attachment_paths: Vec<PathBuf>) {
        self.dispatch(Command::SendTurn {
            session_id: self.active_session_id().unwrap_or_default(),
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
            session_id: self.active_session_id().unwrap_or_default(),
            text,
            attachment_paths,
            fire_at_unix_secs,
        });
    }
    pub fn confirm_relay_and_send(&mut self, text: String, attachment_paths: Vec<PathBuf>) {
        self.dispatch(Command::ConfirmRelayAndSend {
            session_id: self.active_session_id().unwrap_or_default(),
            text,
            attachment_paths,
        });
    }
    pub fn orchestrate_turn(&mut self, text: String, attachment_paths: Vec<PathBuf>) {
        self.dispatch(Command::OrchestrateTurn {
            session_id: self.active_session_id().unwrap_or_default(),
            text,
            attachment_paths,
        });
    }
    pub fn steer(&mut self, text: String, attachment_paths: Vec<PathBuf>) {
        self.dispatch(Command::Steer {
            session_id: self.active_session_id().unwrap_or_default(),
            text,
            attachment_paths,
        });
    }
    pub fn steer_queued(&mut self, id: u64) {
        self.dispatch(Command::SteerQueued {
            session_id: self.active_session_id().unwrap_or_default(),
            id,
        });
    }
    pub fn drop_queued(&mut self, id: u64) {
        self.dispatch(Command::DropQueued {
            session_id: self.active_session_id().unwrap_or_default(),
            id,
        });
    }
    pub fn interrupt(&mut self) {
        self.dispatch(Command::Interrupt {
            session_id: self.active_session_id().unwrap_or_default(),
        });
    }
    pub fn respond_approval(&mut self, request_id: String, decision: ApprovalDecision) {
        self.dispatch(Command::RespondApproval {
            session_id: self.active_session_id().unwrap_or_default(),
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
            session_id: self.active_session_id().unwrap_or_default(),
            request_id,
            answers,
        });
    }
    pub fn rewind_turn(&mut self, turn: usize, mode: RewindMode) {
        self.dispatch(Command::RewindTurn {
            session_id: self.active_session_id().unwrap_or_default(),
            turn,
            mode,
        });
    }
    pub fn add_review_comment(&mut self, comment: ReviewComment) {
        self.dispatch(Command::AddReviewComment {
            session_id: self.active_session_id().unwrap_or_default(),
            comment,
        });
    }
    pub fn remove_review_comment(&mut self, index: usize) {
        self.dispatch(Command::RemoveReviewComment {
            session_id: self.active_session_id().unwrap_or_default(),
            index,
        });
    }
    pub fn implement_plan(&mut self) {
        self.dispatch(Command::ImplementPlan {
            session_id: self.active_session_id().unwrap_or_default(),
        });
    }
    pub fn dismiss_plan(&mut self) {
        self.dispatch(Command::DismissPlan {
            session_id: self.active_session_id().unwrap_or_default(),
        });
    }
    pub fn implement_plan_in_new_thread(&mut self, title: String, cx: &mut Context<Self>) {
        self.create_and_select(
            Command::ImplementPlanInNewThread {
                session_id: self.active_session_id().unwrap_or_default(),
                title,
            },
            cx,
        );
    }
    pub fn copy_plan(&mut self, markdown: String) {
        self.dispatch(Command::CopyPlan { markdown });
    }
    pub fn save_plan_to_workspace(&mut self, markdown: String) {
        self.dispatch(Command::SavePlanToWorkspace {
            session_id: self.active_session_id().unwrap_or_default(),
            markdown,
        });
    }
    pub fn download_plan(&mut self, markdown: String, fallback_title: String) {
        self.dispatch(Command::DownloadPlan {
            session_id: self.active_session_id().unwrap_or_default(),
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
    pub fn start_draft(&mut self, project_id: String, cwd: PathBuf, cx: &mut Context<Self>) {
        self.create_and_select(Command::StartDraft { project_id, cwd }, cx);
    }
    fn create_and_select(&mut self, command: Command, cx: &mut Context<Self>) {
        let request = self.command(command, cx);
        let selected = self.selected_session_id.clone();
        cx.spawn(async move |this, cx| {
            if let Ok(CommandResponse::SessionId(Some(id))) = request.await {
                let _ = this.update(cx, |store, cx| {
                    if store.selected_session_id == selected {
                        store.select_session(id);
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }
    pub fn set_draft_workspace(&mut self, mode: WorkspaceMode) {
        self.dispatch(Command::SetDraftWorkspace {
            session_id: self.active_session_id().unwrap_or_default(),
            mode,
        });
    }
    pub fn run_git_action(
        &mut self,
        action: GitAction,
        message: Option<String>,
        included: Option<Vec<String>>,
        feature_branch: Option<String>,
    ) {
        self.dispatch(Command::RunGitAction {
            session_id: self.active_session_id().unwrap_or_default(),
            action,
            message,
            included,
            feature_branch,
        });
    }
    pub fn retry_git_action(&mut self, request: tcode_protocol::GitActionRequest) {
        self.dispatch(Command::RunGitAction {
            session_id: request.session_id,
            action: request.action,
            message: request.message,
            included: request.included,
            feature_branch: request.feature_branch,
        });
    }
    pub fn load_branches(&mut self) {
        self.dispatch(Command::LoadBranches {
            session_id: self.active_session_id().unwrap_or_default(),
        });
    }
    pub fn checkout_branch(&mut self, branch: String) {
        self.dispatch(Command::CheckoutBranch {
            session_id: self.active_session_id().unwrap_or_default(),
            branch,
        });
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
            self.dispatch(Command::ToggleTerminalPanel {
                session_id: self.active_session_id().unwrap_or_default(),
            });
        } else {
            self.dispatch(Command::CloseTerminalPanel {
                session_id: self.active_session_id().unwrap_or_default(),
            });
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
        self.dispatch(Command::CloseTerminalPanel {
            session_id: self.active_session_id().unwrap_or_default(),
        });
        cx.emit(StoreChange {
            topic: TopicKind::ActiveSession,
        });
        cx.notify();
    }
    pub fn set_terminal_height(&mut self, height: f32, cx: &mut Context<Self>) {
        if let Some(ui) = self.active_conversation_ui_mut() {
            ui.terminal_height = height;
        }
        self.dispatch(Command::SetTerminalHeight {
            session_id: self.active_session_id().unwrap_or_default(),
            height,
        });
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
        self.dispatch(Command::CloseTerminal {
            session_id: self.active_session_id().unwrap_or_default(),
            terminal_id,
        });
        cx.emit(StoreChange {
            topic: TopicKind::ActiveSession,
        });
        cx.notify();
    }
    pub fn restart_terminal(&mut self) {
        self.dispatch(Command::RestartTerminal {
            session_id: self.active_session_id().unwrap_or_default(),
        });
    }
    pub fn new_terminal(&mut self) {
        self.dispatch(Command::NewTerminal {
            session_id: self.active_session_id().unwrap_or_default(),
        });
    }
    pub fn split_terminal(&mut self, direction: TerminalSplitDirection) {
        self.dispatch(Command::SplitTerminal {
            session_id: self.active_session_id().unwrap_or_default(),
            direction,
        });
    }
    pub fn activate_terminal(&mut self, terminal_id: u64) {
        self.dispatch(Command::ActivateTerminal {
            session_id: self.active_session_id().unwrap_or_default(),
            terminal_id,
        });
    }
    pub fn capture_terminal_selection(&mut self, terminal_id: u64) {
        #[cfg(feature = "terminal")]
        let selection = self
            .client_terminal(terminal_id)
            .and_then(|terminal| terminal.selected_text())
            .map(|selection| tcode_protocol::TerminalSelection {
                line_start: selection.line_start,
                line_end: selection.line_end,
                text: selection.text,
            });
        #[cfg(not(feature = "terminal"))]
        let selection = None;
        self.dispatch(Command::CaptureTerminalSelection {
            session_id: self.active_session_id().unwrap_or_default(),
            terminal_id,
            selection,
        });
    }
    pub fn remove_terminal_context(&mut self, context_id: u64) {
        self.dispatch(Command::RemoveTerminalContext {
            session_id: self.active_session_id().unwrap_or_default(),
            context_id,
        });
    }
}

// Provider intents (12).
impl WorkspaceStore {
    pub fn refresh_provider_status(&mut self) {
        self.dispatch(Command::RefreshProviderStatus);
    }
    pub fn refresh_provider_usage(&mut self) {
        self.dispatch(Command::RefreshProviderUsage);
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
            session_id: self.active_session_id().unwrap_or_default(),
            provider,
            model,
            profile_id,
        });
    }
    pub fn toggle_favorite_model(&mut self, model: String) {
        self.dispatch(Command::ToggleFavoriteModel { model });
    }
    pub fn set_active_option(&mut self, id: String, value: Option<serde_json::Value>) {
        self.dispatch(Command::SetActiveOption {
            session_id: self.active_session_id().unwrap_or_default(),
            id,
            value,
        });
    }
    pub fn select_ultrathink(&mut self) {
        self.dispatch(Command::SelectUltrathink {
            session_id: self.active_session_id().unwrap_or_default(),
        });
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
        self.dispatch(Command::SetActiveAcpAgent {
            session_id: self.active_session_id().unwrap_or_default(),
            id,
        });
    }
}

// Composer mode intents (3).
impl WorkspaceStore {
    pub fn set_interaction_mode(&mut self, mode: InteractionMode) {
        self.dispatch(Command::SetInteractionMode {
            session_id: self.active_session_id().unwrap_or_default(),
            mode,
        });
    }
    pub fn toggle_interaction_mode(&mut self) {
        self.dispatch(Command::ToggleInteractionMode {
            session_id: self.active_session_id().unwrap_or_default(),
        });
    }
    pub fn set_active_approval_mode(&mut self, mode: ApprovalMode) {
        self.dispatch(Command::SetActiveApprovalMode {
            session_id: self.active_session_id().unwrap_or_default(),
            mode,
        });
    }
}
