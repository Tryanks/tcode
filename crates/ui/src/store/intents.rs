use std::path::PathBuf;

use agent::{ApprovalDecision, PluginAction, ProviderKind, RewindMode};
use gpui::{App, Context, Task};
use tcode_core::{
    acp::AcpAgentPatch,
    git::GitAction,
    session::ReviewComment,
    settings::{
        ChildApprovalMode, ImageMode, OrchestrateChildModel, ProfileSettingsPatch, SidebarLayout,
    },
    ui::{TerminalSplitDirection, WorkspaceMode},
};
use tcode_protocol::{Command, CommandResponse, ProtocolError, RuntimeOperationId, SettingsPatch};

use super::{ArchivedDeletion, KEPT_THREADS, StoreChange, TopicKind, WorkspaceStore};

impl WorkspaceStore {
    pub(crate) fn local_settings_changed(&self, cx: &mut Context<Self>) {
        if !self.scope.is_full() {
            cx.emit(StoreChange {
                topic: TopicKind::Settings,
            });
            cx.notify();
        }
    }

    fn host_global_command(command: &Command) -> bool {
        matches!(
            command,
            Command::DeleteSession { .. }
                | Command::DeleteProject { .. }
                | Command::CreateProject { .. }
                | Command::CreateNewProject { .. }
                | Command::StartScratchDraft
                | Command::SetProjectIcon { .. }
                | Command::SetProjectRoot { .. }
                | Command::StartExternalImport { .. }
                | Command::OpenLatestSession
                | Command::ShutdownAllAndFlush
                | Command::ApplyPendingRelaunch
                | Command::PatchSettings { .. }
                | Command::ResetSettings
                | Command::SetSidebarCollapsed { .. }
                | Command::CycleProjectSort
                | Command::ToggleFavoriteModel { .. }
                | Command::ToggleProjectCollapsed { .. }
                | Command::SetThreadCollapsed { .. }
                | Command::PreviewReply { .. }
                | Command::ReloadProvider
                | Command::SetProfileSecret { .. }
                | Command::UpdateProfileSettings { .. }
                | Command::CreateThirdPartyProfile { .. }
                | Command::DeleteProfile { .. }
                | Command::RefreshProviderStatus
                | Command::RefreshProviderUsage
                | Command::CheckProviderVersions
                | Command::UpdateProviders { .. }
                | Command::RefreshAcpRegistry
                | Command::InstallAcpAgent { .. }
                | Command::RemoveAcpAgent { .. }
                | Command::AddCustomAcpAgent { .. }
                | Command::UpdateAcpAgent { .. }
                | Command::RefreshProviderPlugins { .. }
                | Command::InstallProviderPlugin { .. }
                | Command::UninstallProviderPlugin { .. }
                | Command::SetProviderPluginEnabled { .. }
                | Command::UpdateProviderPlugin { .. }
                | Command::AddProviderMarketplace { .. }
                | Command::RemoveProviderMarketplace { .. }
                | Command::ResolvePluginChallenge { .. }
        )
    }

    pub(super) fn dispatch(&mut self, command: Command) {
        if !self.scope.is_full() {
            match &command {
                Command::SetSidebarCollapsed { collapsed } => {
                    self.settings_replica.sidebar_collapsed = *collapsed
                }
                Command::ToggleProjectCollapsed { project_id } => {
                    if self
                        .settings_replica
                        .collapsed_projects
                        .contains(project_id)
                    {
                        self.settings_replica
                            .collapsed_projects
                            .retain(|id| id != project_id);
                    } else {
                        self.settings_replica
                            .collapsed_projects
                            .push(project_id.clone());
                    }
                }
                Command::SetThreadCollapsed {
                    session_id,
                    collapsed,
                } => {
                    self.settings_replica
                        .collapsed_threads
                        .retain(|id| id != session_id);
                    if *collapsed {
                        self.settings_replica
                            .collapsed_threads
                            .push(session_id.clone());
                    }
                }
                Command::CycleProjectSort => {
                    self.settings_replica.project_sort = self.settings_replica.project_sort.next()
                }
                Command::ToggleFavoriteModel { model } => {
                    if self.settings_replica.favorite_models.contains(model) {
                        self.settings_replica
                            .favorite_models
                            .retain(|id| id != model);
                    } else {
                        self.settings_replica.favorite_models.push(model.clone());
                    }
                }
                Command::MarkSessionRead {
                    session_id,
                    through,
                } => {
                    self.settings_replica
                        .last_visited
                        .insert(session_id.clone(), *through);
                }
                Command::MarkSessionUnread { session_id } => {
                    self.settings_replica
                        .last_visited
                        .insert(session_id.clone(), 0);
                }
                _ => {
                    if Self::host_global_command(&command) {
                        return;
                    }
                    if let Err(error) = self.host.dispatch(command) {
                        log::error!("failed to dispatch host command: {}", error.message);
                    }
                    return;
                }
            }
            self.save_member_settings();
            return;
        }
        if !self.baseline_topics.contains(&tcode_protocol::Topic::Scope)
            && Self::host_global_command(&command)
        {
            return;
        }
        if let Err(error) = self.host.dispatch(command) {
            log::error!("failed to dispatch host command: {}", error.message);
        }
    }

    pub(crate) fn command(
        &self,
        command: Command,
        cx: &mut App,
    ) -> Task<Result<CommandResponse, ProtocolError>> {
        if (!self.scope.is_full() || !self.baseline_topics.contains(&tcode_protocol::Topic::Scope))
            && Self::host_global_command(&command)
        {
            return cx.spawn(async |_| {
                Err(ProtocolError::out_of_scope("command is outside this space"))
            });
        }
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

impl WorkspaceStore {
    fn patch_settings(&mut self, patch: SettingsPatch) {
        if !self.scope.is_full() {
            match patch {
                SettingsPatch::LastProject(id) => self.settings_replica.last_project_id = id,
                SettingsPatch::SidebarLayout(layout) => {
                    self.settings_replica.sidebar_layout = layout
                }
                SettingsPatch::WordWrapDiffs(value) => {
                    self.settings_replica.word_wrap_diffs = value
                }
                SettingsPatch::AutoOpenTaskPanel(value) => {
                    self.settings_replica.auto_open_task_panel = value
                }
                SettingsPatch::LiveCommandPanelDisabled(value) => {
                    self.settings_replica.live_command_panel_disabled = value
                }
                SettingsPatch::SidebarProviderMarks(value) => {
                    self.settings_replica.sidebar_provider_marks = value
                }
                SettingsPatch::InactiveFrameThrottleDisabled(value) => {
                    self.settings_replica.inactive_frame_throttle_disabled = value
                }
                _ => return,
            }
            self.save_member_settings();
            return;
        }
        self.dispatch(Command::PatchSettings { patch });
    }

    /// Record the project the user navigated into. Called only from user
    /// navigation (opening a thread, starting a draft), because the empty
    /// workspace returns here: background activity must not move it.
    fn remember_project(&mut self, project_id: Option<String>) {
        let Some(project_id) = project_id else {
            return;
        };
        if self.settings_replica.last_project_id.as_deref() == Some(project_id.as_str()) {
            return;
        }
        self.settings_replica.last_project_id = Some(project_id.clone());
        self.patch_settings(SettingsPatch::LastProject(Some(project_id)));
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
    pub fn set_sidebar_provider_marks(&mut self, value: bool) {
        self.patch_settings(SettingsPatch::SidebarProviderMarks(value));
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
    pub fn set_resume_on_limit_reset(&mut self, value: bool) {
        self.patch_settings(SettingsPatch::ResumeOnLimitReset(value));
    }
    pub fn set_fallback_review_advisor(&mut self, value: bool) {
        self.patch_settings(SettingsPatch::FallbackReviewAdvisor(value));
    }
    pub fn set_orchestrate_decision_models(&mut self, value: Vec<OrchestrateChildModel>) {
        self.patch_settings(SettingsPatch::OrchestrateDecisionModels(value));
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
    pub fn set_sidebar_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        self.dispatch(Command::SetSidebarCollapsed { collapsed });
        self.local_settings_changed(cx);
    }
}

impl WorkspaceStore {
    pub fn settle_session(&mut self, session_id: String) {
        self.dispatch(Command::SettleSession { session_id });
    }
    pub fn set_auto_settle(&mut self, session_id: String, enabled: bool) {
        self.dispatch(Command::SetAutoSettle {
            session_id,
            enabled,
        });
    }
    pub fn set_auto_settle_after_days(&mut self, days: Option<f64>) {
        self.patch_settings(SettingsPatch::AutoSettleAfterDays(days));
    }
    pub fn set_project_settlement(
        &mut self,
        project_id: String,
        value: Option<tcode_core::settings::ProjectSettlementSettings>,
    ) {
        self.patch_settings(SettingsPatch::ProjectSettlement { project_id, value });
    }
    pub fn make_session_active(&mut self, session_id: String) {
        self.dispatch(Command::UnsettleSession { session_id });
    }
    pub fn archive_session(&mut self, session_id: String) {
        self.dispatch(Command::ArchiveSession { session_id });
    }
    pub fn unarchive_session(&mut self, session_id: String) {
        self.dispatch(Command::UnarchiveSession { session_id });
    }

    pub fn rename_session(&mut self, session_id: String, title: String) {
        self.dispatch(Command::RenameSession { session_id, title });
    }

    pub fn regenerate_session_title(&mut self, session_id: String) {
        self.dispatch(Command::RegenerateSessionTitle { session_id });
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
    /// Deletes what [`WorkspaceStore::archived_deletion`] counted.
    pub fn delete_archived(&mut self, deletion: ArchivedDeletion) {
        // ponytail: one DeleteSession command per tree; add a bulk command if thousands feel slow.
        for session_id in deletion.roots {
            self.delete_session(session_id, false);
        }
    }
    pub fn mark_session_unread(&mut self, session_id: String, cx: &mut Context<Self>) {
        self.dispatch(Command::MarkSessionUnread { session_id });
        self.local_settings_changed(cx);
    }
    pub(crate) fn leave_session(&mut self) {
        self.selection_generation = self.selection_generation.wrapping_add(1);
        self.history_task = None;
        self.history_trim = None;
        self.history_error = None;
        self.history_pages_fetched = 0;
        self.history_logged_records = None;
        self.session_turn_offset = 0;
        self.session_catching_up = false;
        self.clear_terminal_topics();
        if let Some(session_id) = self.selected_session_id.take() {
            if let Some(thread) = self.threads.get_mut(&session_id) {
                thread.left_at = self.selection_generation;
                // The thread reopens at its tail.
                if let Some(held) = &mut thread.history {
                    held.drop_pages_above_tail();
                }
            }
            self.release_left_threads();
            for topic in [
                tcode_protocol::Topic::SessionEvents {
                    session_id: session_id.clone(),
                },
                tcode_protocol::Topic::SessionStatus {
                    session_id: session_id.clone(),
                },
                tcode_protocol::Topic::SessionPlan {
                    session_id: session_id.clone(),
                },
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
        self.read_acknowledged = None;
        self.active_destination = None;
        self.git_status_replica = Default::default();
    }

    /// Keep the replicas of the [`KEPT_THREADS`] threads left most recently.
    fn release_left_threads(&mut self) {
        if self.threads.len() <= KEPT_THREADS {
            return;
        }
        let mut left: Vec<(u64, String)> = self
            .threads
            .iter()
            .map(|(id, thread)| (thread.left_at, id.clone()))
            .collect();
        left.sort_unstable_by(|a, b| b.cmp(a));
        for (_, id) in &left[KEPT_THREADS..] {
            self.threads.remove(id);
        }
    }

    pub fn select_session(&mut self, session_id: String) {
        if self.selected_session_id.as_ref() == Some(&session_id) {
            return;
        }
        let project_id = self
            .index_replica
            .0
            .iter()
            .find(|meta| meta.id == session_id)
            .and_then(|meta| meta.project_id.clone());
        self.remember_project(project_id);
        self.leave_session();
        self.selected_session_id = Some(session_id.clone());
        self.baseline_topics
            .remove(&tcode_protocol::Topic::SessionStatus {
                session_id: session_id.clone(),
            });
        self.baseline_topics
            .remove(&tcode_protocol::Topic::SessionPlan {
                session_id: session_id.clone(),
            });
        self.baseline_topics
            .remove(&tcode_protocol::Topic::SessionEvents {
                session_id: session_id.clone(),
            });
        let thread = self.threads.entry(session_id.clone()).or_default();
        self.session_status_replica = thread.status.clone();
        self.git_status_replica = thread.git.clone().unwrap_or_default();
        self.session_replica = None;
        let after = thread.history.as_ref().map(|held| held.end);
        if !self.baseline_topics.contains(&tcode_protocol::Topic::Scope) {
            return;
        }
        for topic in [
            tcode_protocol::Topic::SessionStatus {
                session_id: session_id.clone(),
            },
            tcode_protocol::Topic::SessionPlan {
                session_id: session_id.clone(),
            },
            tcode_protocol::Topic::GitStatus {
                session_id: session_id.clone(),
            },
            tcode_protocol::Topic::Preview {
                session_id: session_id.clone(),
            },
            tcode_protocol::Topic::SessionEvents { session_id },
        ] {
            // A client with no preview backend must not become a competing
            // owner of the session's preview: it would win requests it can only
            // refuse. It still answers `unsupported` for anything that reaches
            // it through an already-open subscription.
            if matches!(topic, tcode_protocol::Topic::Preview { .. })
                && (!crate::preview_panel::PREVIEW_BACKEND || !self.scope.is_full())
            {
                continue;
            }
            let _ = self.host.subscribe(tcode_protocol::Subscription {
                after: if matches!(topic, tcode_protocol::Topic::SessionEvents { .. }) {
                    after
                } else {
                    None
                },
                topic,
            });
        }
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
        if self.approval_delivery_pending(&request_id) {
            return;
        }
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
            turn: self.absolute_turn(turn),
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
}

impl WorkspaceStore {
    pub fn create_project(
        &self,
        root: PathBuf,
        cx: &mut App,
    ) -> Task<Result<CommandResponse, ProtocolError>> {
        self.command(Command::CreateProject { root }, cx)
    }
    pub fn create_new_project(
        &self,
        name: String,
        cx: &mut App,
    ) -> Task<Result<CommandResponse, ProtocolError>> {
        self.command(Command::CreateNewProject { name }, cx)
    }
    pub fn set_project_root(
        &self,
        project_id: String,
        root: PathBuf,
        move_files: bool,
        cx: &mut App,
    ) -> Task<Result<CommandResponse, ProtocolError>> {
        self.command(
            Command::SetProjectRoot {
                project_id,
                root,
                move_files,
            },
            cx,
        )
    }
    pub fn toggle_project_collapsed(&mut self, project_id: String, cx: &mut Context<Self>) {
        self.dispatch(Command::ToggleProjectCollapsed { project_id });
        self.local_settings_changed(cx);
    }
    pub fn set_thread_collapsed(
        &mut self,
        session_id: String,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) {
        self.dispatch(Command::SetThreadCollapsed {
            session_id,
            collapsed,
        });
        self.local_settings_changed(cx);
    }
    pub fn delete_project(&mut self, project_id: String) {
        self.dispatch(Command::DeleteProject { project_id });
    }
    pub fn start_draft(&mut self, project_id: String, cwd: PathBuf, cx: &mut Context<Self>) {
        self.remember_project(Some(project_id.clone()));
        self.create_and_select(Command::StartDraft { project_id, cwd }, cx);
    }
    pub fn start_scratch_draft(&mut self, cx: &mut Context<Self>) {
        self.create_and_select(Command::StartScratchDraft, cx);
    }
    fn create_and_select(&mut self, command: Command, cx: &mut Context<Self>) {
        self.draft_fallback_pending = true;
        let request = self.command(command, cx);
        let selected = self.selected_session_id.clone();
        cx.spawn(async move |this, cx| {
            let response = request.await;
            let _ = this.update(cx, |store, _| store.draft_fallback_pending = false);
            if let Ok(CommandResponse::SessionId(Some(id))) = response {
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
    pub fn cycle_project_sort(&mut self, cx: &mut Context<Self>) {
        self.dispatch(Command::CycleProjectSort);
        self.local_settings_changed(cx);
    }
}

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
        let selection = self
            .client_terminal(terminal_id)
            .and_then(|terminal| terminal.selected_text())
            .map(|selection| tcode_protocol::TerminalSelection {
                line_start: selection.line_start,
                line_end: selection.line_end,
                text: selection.text,
            });
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
    pub fn refresh_github_credentials(&mut self) {
        self.dispatch(Command::RefreshGitHubCredentials);
    }
    pub fn set_github_token(&mut self, host: String, token: Option<String>) {
        self.dispatch(Command::SetGitHubToken { host, token });
    }
    pub fn patch_github_host(
        &mut self,
        host: String,
        enabled: Option<bool>,
        account: Option<Option<String>>,
    ) {
        self.patch_settings(SettingsPatch::GitHubHost {
            host,
            enabled,
            account,
        });
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
    pub fn update_providers(&mut self, providers: Vec<ProviderKind>) {
        self.dispatch(Command::UpdateProviders { providers });
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
    pub fn toggle_favorite_model(&mut self, model: String, cx: &mut Context<Self>) {
        self.dispatch(Command::ToggleFavoriteModel { model });
        self.local_settings_changed(cx);
    }
    pub fn set_active_option(&mut self, id: String, value: Option<serde_json::Value>) {
        self.dispatch(Command::SetActiveOption {
            session_id: self.active_session_id().unwrap_or_default(),
            id,
            value,
        });
    }
}

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

impl WorkspaceStore {
    pub fn set_plugin_management_enabled(&mut self, enabled: bool) {
        self.patch_settings(SettingsPatch::PluginManagementEnabled(enabled));
    }
    pub fn set_provider_plugin_management(&mut self, provider: ProviderKind, enabled: bool) {
        self.patch_settings(SettingsPatch::PluginManagementProvider { provider, enabled });
    }
    pub fn refresh_provider_plugins(&mut self, profile_id: String, cwd: Option<PathBuf>) {
        self.dispatch(Command::RefreshProviderPlugins { profile_id, cwd });
    }
    /// Run one action the host offered for `entry_id` in the catalog listed
    /// from `cwd`.
    pub fn run_provider_plugin_action(
        &mut self,
        profile_id: String,
        entry_id: String,
        action: PluginAction,
        cwd: Option<PathBuf>,
    ) {
        let scope = action.scope();
        self.dispatch(match action {
            PluginAction::Install { .. } => Command::InstallProviderPlugin {
                profile_id,
                entry_id,
                scope,
                cwd,
            },
            PluginAction::Uninstall { .. } => Command::UninstallProviderPlugin {
                profile_id,
                entry_id,
                scope,
                cwd,
            },
            PluginAction::Enable { .. } | PluginAction::Disable { .. } => {
                Command::SetProviderPluginEnabled {
                    profile_id,
                    entry_id,
                    scope,
                    enabled: matches!(action, PluginAction::Enable { .. }),
                    cwd,
                }
            }
            PluginAction::Update { .. } => Command::UpdateProviderPlugin {
                profile_id,
                entry_id,
                scope,
                cwd,
            },
        });
    }
    pub fn add_provider_marketplace(
        &mut self,
        profile_id: String,
        source: String,
        cwd: Option<PathBuf>,
    ) {
        self.dispatch(Command::AddProviderMarketplace {
            profile_id,
            source,
            cwd,
        });
    }
    pub fn remove_provider_marketplace(
        &mut self,
        profile_id: String,
        name: String,
        cwd: Option<PathBuf>,
    ) {
        self.dispatch(Command::RemoveProviderMarketplace {
            profile_id,
            name,
            cwd,
        });
    }
    pub fn resolve_plugin_challenge(&mut self, op_id: RuntimeOperationId, accept: bool) {
        self.dispatch(Command::ResolvePluginChallenge { op_id, accept });
    }
}
