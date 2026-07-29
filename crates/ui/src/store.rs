use std::collections::HashSet;
use std::path::PathBuf;

use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Task, Window};
use tcode_core::{
    git::{GitFileEntry, MenuItem, QuickAction},
    project::{Project, SessionMeta, WorktreeInfo, group_sessions},
    provider_models::{ResolvedModel, picker_models, resolve_models},
    provider_status::ProviderSnapshot,
    session::{ReviewComment, StoredEvent, Timeline},
    settings::{BrowserSettings, ProjectSort, ProviderSettings, ResolvedProfile, Settings},
};
use tcode_protocol::{Command, EventEnvelope, ServerEvent, Topic};
use tcode_runtime::{
    app::{AppState, ProjectGroup, ProviderVersionStatus, QueuedMessage},
    event::{HostEvent, RuntimeEvent},
    terminal::{TerminalContext, TerminalWorkspace},
    ui_facade::{
        AcpMarketplaceItem, ExternalImportUpdate, ExternalThread, GitDiffResult, GitDiffScope,
        PathEntry, RecentDir,
    },
};

use crate::{composer::Composer, terminal_drawer::TerminalDrawer, window_state::WindowState};

/// The client-facing projection and command boundary for workspace state.
///
/// Views observe this entity and use its typed accessors instead of retaining
/// or reading the backend `AppState` entity directly.
pub struct WorkspaceStore {
    app: Entity<AppState>,
    index_replica: (Vec<SessionMeta>, Vec<Project>),
    settings_replica: Settings,
    session_replica: Option<(String, Timeline)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkAvailability {
    Available,
    Unsupported,
    Empty,
    Running,
}

pub(crate) struct ComposerCheckoutState {
    pub branch: String,
    pub branches: Vec<String>,
    pub turn_running: bool,
    pub is_draft: bool,
    pub worktree_base: Option<String>,
    pub worktree: Option<WorktreeInfo>,
}

pub(crate) struct ComposerActiveModel {
    pub provider: agent::ProviderKind,
    pub model: Option<String>,
    pub acp_agent_id: Option<String>,
    pub profile_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiffFocus {
    pub session: String,
    pub turn: usize,
    pub path: String,
}

pub(crate) struct DiffActiveState {
    pub session: String,
    pub cwd: PathBuf,
    pub branches: Vec<String>,
}

impl WorkspaceStore {
    pub fn new(app: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let (index_replica, settings_replica) = {
            let state = app.read(cx);
            (
                (state.sessions.clone(), state.projects.clone()),
                state.settings.clone(),
            )
        };
        cx.observe(&app, |store, app, cx| {
            let active = app
                .read(cx)
                .active
                .as_ref()
                .map(|active| (active.meta.id.as_str(), active.draft));
            let replica_matches = match (active, store.session_replica.as_ref()) {
                (Some((active_id, false)), Some((replica_id, _))) => replica_id == active_id,
                (None, None) => true,
                _ => false,
            };
            if !replica_matches {
                store.session_replica = None;
            }
            cx.notify();
        })
        .detach();
        cx.subscribe(&app, |store, _, event: &HostEvent, cx| {
            match event {
                HostEvent::Runtime(event) => cx.emit(event.clone()),
                HostEvent::Domain(envelope) => store.apply_domain_event(envelope, cx),
            }
            cx.notify();
        })
        .detach();
        Self {
            app,
            index_replica,
            settings_replica,
            session_replica: None,
        }
    }

    fn apply_domain_event(&mut self, envelope: &EventEnvelope, cx: &App) {
        match (&envelope.topic, &envelope.event) {
            (Topic::Index, ServerEvent::IndexUpsertSession(meta)) => {
                match self
                    .index_replica
                    .0
                    .iter_mut()
                    .find(|existing| existing.id == meta.id)
                {
                    Some(existing) => *existing = meta.clone(),
                    None => self.index_replica.0.push(meta.clone()),
                }
                self.index_replica
                    .0
                    .sort_by_key(|meta| std::cmp::Reverse(meta.updated_at));
            }
            (Topic::Index, ServerEvent::IndexUpsertProject(project)) => {
                match self
                    .index_replica
                    .1
                    .iter_mut()
                    .find(|existing| existing.id == project.id)
                {
                    Some(existing) => *existing = project.clone(),
                    None => self.index_replica.1.push(project.clone()),
                }
            }
            (Topic::Index, ServerEvent::IndexRemoveSession { session_id }) => {
                self.index_replica.0.retain(|meta| meta.id != *session_id);
            }
            (Topic::Index, ServerEvent::IndexRemoveProject { project_id }) => {
                self.index_replica
                    .1
                    .retain(|project| project.id != *project_id);
            }
            (Topic::Index, ServerEvent::IndexSnapshot(snapshot)) => {
                self.index_replica = (snapshot.sessions.clone(), snapshot.projects.clone());
            }
            (Topic::Settings, ServerEvent::SettingsReplaced(settings))
            | (Topic::Settings, ServerEvent::SettingsSnapshot(settings)) => {
                self.settings_replica = settings.clone();
            }
            (Topic::SessionEvents { session_id }, ServerEvent::SessionSnapshot(records)) => {
                let mut timeline =
                    Timeline::fold_events(records.iter().map(|record| StoredEvent {
                        ts: record.ts,
                        event: record.event.clone(),
                    }));
                let live_turn_running = self
                    .app
                    .read(cx)
                    .active
                    .as_ref()
                    .filter(|active| active.meta.id == *session_id)
                    .is_some_and(|active| active.is_turn_running());
                if !live_turn_running {
                    timeline.mark_idle();
                }
                self.session_replica = Some((session_id.clone(), timeline));
            }
            (Topic::SessionEvents { session_id }, ServerEvent::SessionEvent(record)) => {
                if let Some((replica_id, timeline)) = self.session_replica.as_mut()
                    && replica_id == session_id
                {
                    timeline.apply_at(record.ts, &record.event);
                }
            }
            _ => {}
        }
    }

    fn all_provider_profiles(&self) -> Vec<ResolvedProfile> {
        let mut profiles = Vec::new();
        for kind in [
            agent::ProviderKind::Codex,
            agent::ProviderKind::ClaudeCode,
            agent::ProviderKind::Pi,
            agent::ProviderKind::OpenCode,
        ] {
            profiles.extend(self.settings_replica.profiles_for_kind(kind));
        }
        profiles
    }

    fn enabled_profiles(&self) -> Vec<ResolvedProfile> {
        self.all_provider_profiles()
            .into_iter()
            .filter(|profile| profile.settings.enabled)
            .collect()
    }

    fn profile_catalog(&self, profile_id: &str, cx: &App) -> Vec<agent::ModelSpec> {
        if Settings::is_builtin_profile_id(profile_id) {
            let kind = self
                .settings_replica
                .resolved_profile(profile_id)
                .map(|profile| profile.kind)
                .unwrap_or(agent::ProviderKind::ClaudeCode);
            self.app.read(cx).models_for(kind).to_vec()
        } else {
            Vec::new()
        }
    }

    /// Dispatch a serializable backend mutation.
    ///
    /// This match deliberately has no wildcard: extending the protocol must
    /// also extend this in-process client/host bridge.
    pub fn dispatch(&mut self, command: Command, cx: &mut Context<Self>) {
        self.app.update(cx, |app, cx| match command {
            Command::OrchestrateTurn {
                text,
                attachment_paths,
            } => app.orchestrate_turn(text, attachment_paths, cx),
            Command::ReloadProvider { provider } => app.reload_provider(provider, cx),
            Command::SetProfileSecret {
                profile_id,
                name,
                value,
            } => app.set_profile_secret(&profile_id, &name, value.as_deref(), cx),
            Command::UpdateProfileSettings { profile_id, patch } => {
                app.update_profile_settings(&profile_id, patch, cx)
            }
            Command::CreateThirdPartyProfile {
                name,
                base_url,
                model,
                api_key,
            } => {
                app.create_third_party_profile(&name, &base_url, model.as_deref(), &api_key, cx);
            }
            Command::DeleteProfile { profile_id } => app.delete_profile(&profile_id, cx),
            Command::RefreshProviderStatus => app.refresh_provider_status(cx),
            Command::CheckProviderVersions => app.check_provider_versions(cx),
            Command::UpdateProvider { provider } => app.update_provider(provider, cx),
            Command::SetSidebarCollapsed { collapsed } => app.set_sidebar_collapsed(collapsed, cx),
            Command::RunGitAction {
                action,
                message,
                included,
                feature_branch,
            } => app.run_git_action(action, message, included, feature_branch, cx),
            Command::RefreshAcpRegistry => app.refresh_acp_registry(cx),
            Command::InstallAcpAgent { id } => app.install_acp_agent(id, cx),
            Command::RemoveAcpAgent { id } => app.remove_acp_agent(&id, cx),
            Command::AddCustomAcpAgent {
                name,
                command,
                args,
                env,
            } => app.add_custom_acp_agent(name, command, args, env, cx),
            Command::UpdateAcpAgent { id, patch } => app.update_acp_agent(&id, patch, cx),
            Command::SetActiveAcpAgent { id } => app.set_active_acp_agent(&id, cx),
            Command::ResetSettings => app.reset_settings(cx),
            Command::WriteRelaunchMarker { reopen_settings } => {
                app.write_relaunch_marker(&reopen_settings)
            }
            Command::ToggleDiffPanel => app.toggle_diff_panel(cx),
            Command::OpenDiffForTurn { turn } => app.open_diff_for_turn(turn, cx),
            Command::OpenDiffForFile { turn, path } => app.open_diff_for_file(turn, path, cx),
            Command::SelectDiffTurn { turn } => app.select_diff_turn(turn, cx),
            Command::DiscardDiffFocus => app.discard_diff_focus(),
            Command::SetTerminalHeight { height } => app.set_terminal_height(height, cx),
            Command::ToggleTerminalPanel => app.toggle_terminal_panel(cx),
            Command::CloseTerminalPanel => app.close_terminal_panel(cx),
            Command::RestartTerminal => app.restart_terminal(cx),
            Command::NewTerminal => app.new_terminal(cx),
            Command::SplitTerminal { direction } => app.split_terminal(direction, cx),
            Command::ActivateTerminal { terminal_id } => app.activate_terminal(terminal_id, cx),
            Command::CloseTerminal { terminal_id } => app.close_terminal(terminal_id, cx),
            Command::CaptureTerminalSelection { terminal_id } => {
                app.capture_terminal_selection(terminal_id, cx)
            }
            Command::RemoveTerminalContext { context_id } => {
                app.remove_terminal_context(context_id, cx)
            }
            Command::CloseDiffPanel => app.close_diff_panel(cx),
            Command::AddReviewComment { comment } => app.add_review_comment(comment, cx),
            Command::RemoveReviewComment { index } => app.remove_review_comment(index, cx),
            Command::ToggleDiffExpanded => app.toggle_diff_expanded(cx),
            Command::CycleProjectSort => app.cycle_project_sort(cx),
            Command::CreateProject { root } => {
                app.create_project(root, cx);
            }
            Command::FinishExternalImport { project_id } => {
                app.finish_external_import(&project_id, cx)
            }
            Command::ToggleProjectCollapsed { project_id } => {
                app.toggle_project_collapsed(&project_id, cx)
            }
            Command::UpdateSettings { settings } => app.update_settings(settings, cx),
            Command::ArchiveSession { session_id } => app.archive_session(&session_id, cx),
            Command::UnarchiveSession { session_id } => app.unarchive_session(&session_id, cx),
            Command::AutoArchiveSweep { project_id } => {
                app.auto_archive_sweep(&project_id, cx);
            }
            Command::RenameSession { session_id, title } => {
                app.rename_session(&session_id, &title, cx)
            }
            Command::ForkThread { id } => app.fork_thread(&id, cx),
            Command::DeleteSession {
                session_id,
                remove_worktree,
            } => app.delete_session(&session_id, remove_worktree, cx),
            Command::DeleteProject { project_id } => app.delete_project(&project_id, cx),
            Command::MarkSessionUnread { session_id } => app.mark_session_unread(&session_id, cx),
            Command::StartDraft { project_id, cwd } => app.start_draft(project_id, cwd, cx),
            Command::SetDraftWorkspace { mode } => app.set_draft_workspace(mode, cx),
            Command::SelectSession { session_id } => app.select_session(&session_id, cx),
            Command::SendTurn {
                text,
                attachment_paths,
            } => app.send_turn(text, attachment_paths, cx),
            Command::ConfirmRelayAndSend {
                text,
                attachment_paths,
            } => app.confirm_relay_and_send(text, attachment_paths, cx),
            Command::Steer {
                text,
                attachment_paths,
            } => app.steer(text, attachment_paths, cx),
            Command::SteerQueued { id } => app.steer_queued(id, cx),
            Command::DropQueued { id } => app.drop_queued(id, cx),
            Command::Interrupt => app.interrupt(cx),
            Command::RespondApproval {
                request_id,
                decision,
            } => app.respond_approval(request_id, decision, cx),
            Command::RespondUserInput {
                request_id,
                answers,
            } => app.respond_user_input(request_id, answers, cx),
            Command::SetActiveModel {
                provider,
                model,
                profile_id,
            } => app.set_active_model(provider, model, profile_id, cx),
            Command::SetActiveOption { id, value } => app.set_active_option(&id, value, cx),
            Command::SelectUltrathink => app.select_ultrathink(cx),
            Command::SetInteractionMode { mode } => app.set_interaction_mode(mode, cx),
            Command::ToggleInteractionMode => app.toggle_interaction_mode(cx),
            Command::ImplementPlan => app.implement_plan(cx),
            Command::DismissPlan => app.dismiss_plan(cx),
            Command::ImplementPlanInNewThread { title } => {
                app.implement_plan_in_new_thread(title, cx)
            }
            Command::CopyPlan { markdown } => app.copy_plan(markdown, cx),
            Command::SavePlanToWorkspace { markdown } => app.save_plan_to_workspace(markdown, cx),
            Command::DownloadPlan {
                markdown,
                fallback_title,
            } => app.download_plan(markdown, fallback_title, cx),
            Command::TogglePlanPanel => app.toggle_plan_panel(cx),
            Command::SetRightTab { tab } => app.set_right_tab(tab, cx),
            Command::TogglePreviewPanel => app.toggle_preview_panel(cx),
            Command::ClosePreviewPanel => app.close_preview_panel(cx),
            Command::OpenPreviewPanel => app.open_preview_panel(cx),
            Command::OpenPreviewPanelFor { session_id } => {
                app.open_preview_panel_for(&session_id, cx)
            }
            Command::LoadBranches => app.load_branches(cx),
            Command::CheckoutBranch { branch } => app.checkout_branch(branch, cx),
            Command::SetActiveApprovalMode { mode } => app.set_active_approval_mode(mode, cx),
            Command::ToggleFavoriteModel { model } => app.toggle_favorite_model(&model, cx),
            Command::RewindTurn { turn, mode } => app.rewind_turn(turn, mode, cx),
        });
    }

    pub fn grouped_sessions(&self, _cx: &App) -> Vec<ProjectGroup> {
        let visible: Vec<_> = self
            .index_replica
            .0
            .iter()
            .filter(|meta| meta.archived_at.is_none())
            .cloned()
            .collect();
        group_sessions(
            &self.index_replica.1,
            &visible,
            self.settings_replica.project_sort,
        )
    }

    pub fn palette_groups(&self, cx: &App) -> Vec<ProjectGroup> {
        self.grouped_sessions(cx)
    }

    pub fn palette_settings(&self, _cx: &App) -> Settings {
        self.settings_replica.clone()
    }

    pub fn archived_groups(&self, _cx: &App) -> Vec<ProjectGroup> {
        let archived: Vec<_> = self
            .index_replica
            .0
            .iter()
            .filter(|meta| meta.archived_at.is_some())
            .cloned()
            .collect();
        let mut groups = group_sessions(
            &self.index_replica.1,
            &archived,
            self.settings_replica.project_sort,
        );
        for group in &mut groups {
            group
                .sessions
                .sort_by_key(|meta| std::cmp::Reverse(meta.archived_at));
        }
        groups.retain(|group| !group.sessions.is_empty());
        groups
    }

    pub fn project_sort(&self, _cx: &App) -> ProjectSort {
        self.settings_replica.project_sort
    }

    pub fn is_project_collapsed(&self, project_id: &str, _cx: &App) -> bool {
        self.settings_replica
            .collapsed_projects
            .iter()
            .any(|id| id == project_id)
    }

    pub fn active_session_id(&self, cx: &App) -> Option<String> {
        self.app.read(cx).active_session_id().map(str::to_owned)
    }

    pub fn turn_running_for(&self, session_id: &str, cx: &App) -> bool {
        self.app.read(cx).turn_running_for(session_id)
    }

    pub fn session_unread(&self, session_id: &str, cx: &App) -> bool {
        if self.app.read(cx).active_session_id() == Some(session_id) {
            return false;
        }
        let Some(meta) = self
            .index_replica
            .0
            .iter()
            .find(|meta| meta.id == session_id)
        else {
            return false;
        };
        self.settings_replica
            .last_visited
            .get(session_id)
            .is_some_and(|visited| meta.updated_at > *visited)
    }

    pub fn pending_approval_for(&self, session_id: &str, cx: &App) -> bool {
        self.app.read(cx).pending_approval_for(session_id).is_some()
    }

    pub fn fork_availability(&self, session_id: &str, cx: &App) -> ForkAvailability {
        let app = self.app.read(cx);
        let Some(meta) = self
            .index_replica
            .0
            .iter()
            .find(|meta| meta.id == session_id)
        else {
            return ForkAvailability::Available;
        };
        if !meta.provider.supports_fork() {
            ForkAvailability::Unsupported
        } else if meta.resume_cursor.is_none() {
            ForkAvailability::Empty
        } else if app.turn_running_for(session_id) {
            ForkAvailability::Running
        } else {
            ForkAvailability::Available
        }
    }

    pub fn sidebar_sessions(&self, _cx: &App) -> Vec<SessionMeta> {
        self.index_replica.0.clone()
    }

    pub fn sidebar_settings(&self, _cx: &App) -> Settings {
        self.settings_replica.clone()
    }

    pub fn orchestrate_editor_settings(&self, _cx: &App) -> Settings {
        self.settings_replica.clone()
    }

    pub fn settings_page_settings(&self, _cx: &App) -> Settings {
        self.settings_replica.clone()
    }

    pub fn settings_provider_profiles(&self, _cx: &App) -> Vec<ResolvedProfile> {
        self.all_provider_profiles()
    }

    pub fn settings_installed_acp_agents(
        &self,
        _cx: &App,
    ) -> Vec<tcode_core::acp::InstalledAcpAgent> {
        self.settings_replica
            .installed_acp_agents()
            .into_iter()
            .cloned()
            .collect()
    }

    pub fn providers_checked_at(&self, cx: &App) -> Option<u64> {
        self.app.read(cx).providers_checked_at()
    }

    pub fn providers_checking(&self, cx: &App) -> bool {
        self.app.read(cx).providers_checking()
    }

    pub fn window_caption_state(&self, cx: &App) -> (bool, tcode_core::ui::RightTab) {
        let app = self.app.read(cx);
        (app.diff_panel_open(), app.right_tab())
    }

    pub fn shell_window_title(&self, cx: &App) -> String {
        match self.app.read(cx).active.as_ref() {
            Some(active) if active.draft => tcode_i18n::tr!("chat.new_thread").into_owned(),
            Some(active) => active.meta.title.clone(),
            None => "tcode".to_string(),
        }
    }

    pub fn shell_panel_state(&self, cx: &App) -> (bool, tcode_core::ui::RightTab, bool) {
        let app = self.app.read(cx);
        (
            app.diff_panel_open(),
            app.right_tab(),
            app.diff_panel_expanded(),
        )
    }

    pub fn preview_active_identity(&self, cx: &App) -> Option<(String, String)> {
        let app = self.app.read(cx);
        app.active_session_id().and_then(|session_id| {
            app.active_conversation_ui_key()
                .map(|key| (session_id.to_string(), key))
        })
    }

    pub fn preview_active_session_id(&self, cx: &App) -> Option<String> {
        self.app.read(cx).active_session_id().map(str::to_owned)
    }

    pub fn preview_panel_showing(&self, cx: &App) -> bool {
        self.app.read(cx).preview_panel_showing()
    }

    pub fn preview_browser_settings(&self, _cx: &App) -> BrowserSettings {
        self.settings_replica.browser.clone()
    }

    pub fn take_pending_preview_url(&mut self, cx: &mut Context<Self>) -> Option<String> {
        self.app.update(cx, |app, _| app.take_pending_preview_url())
    }

    pub fn take_preview_requests(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<async_channel::Receiver<preview_mcp::BrokerRequest>> {
        self.app.update(cx, |app, _| app.take_preview_requests())
    }

    pub fn pump_orchestrate_requests(&mut self, cx: &mut Context<Self>) {
        self.app
            .update(cx, |app, cx| app.pump_orchestrate_requests(cx));
    }

    pub fn enabled_provider_profiles(&self, _cx: &App) -> Vec<ResolvedProfile> {
        self.enabled_profiles()
    }

    pub fn provider_profile_kind(&self, profile_id: &str, _cx: &App) -> agent::ProviderKind {
        self.settings_replica
            .resolved_profile(profile_id)
            .map(|profile| profile.kind)
            .unwrap_or(agent::ProviderKind::ClaudeCode)
    }

    pub fn provider_profile_settings(&self, profile_id: &str, _cx: &App) -> ProviderSettings {
        self.settings_replica
            .resolved_profile(profile_id)
            .map(|profile| profile.settings)
            .unwrap_or_default()
    }

    pub fn provider_model_catalog(
        &self,
        provider: agent::ProviderKind,
        cx: &App,
    ) -> Vec<agent::ModelSpec> {
        self.app.read(cx).models_for(provider).to_vec()
    }

    pub fn picker_models_for_profile(&self, profile_id: &str, cx: &App) -> Vec<ResolvedModel> {
        picker_models(
            &self.profile_catalog(profile_id, cx),
            &self.provider_profile_settings(profile_id, cx),
            &self.settings_replica.favorite_models,
        )
    }

    pub fn provider_profile_display_name(&self, profile_id: &str, _cx: &App) -> String {
        self.settings_replica.profile_display_name(profile_id)
    }

    pub fn provider_profile_snapshot(
        &self,
        profile_id: &str,
        cx: &App,
    ) -> Option<ProviderSnapshot> {
        self.app.read(cx).profile_snapshot(profile_id).cloned()
    }

    pub fn provider_version_status(
        &self,
        provider: agent::ProviderKind,
        cx: &App,
    ) -> Option<ProviderVersionStatus> {
        self.app.read(cx).provider_version(provider).cloned()
    }

    pub fn provider_profile_accent(&self, profile_id: &str, _cx: &App) -> Option<u32> {
        let raw = self
            .settings_replica
            .resolved_profile(profile_id)?
            .settings
            .accent_color?;
        let hex = raw.trim().trim_start_matches('#');
        (hex.len() == 6 && hex.chars().all(|ch| ch.is_ascii_hexdigit()))
            .then(|| u32::from_str_radix(hex, 16).ok())
            .flatten()
    }

    pub fn provider_update_command(
        &self,
        provider: agent::ProviderKind,
        cx: &App,
    ) -> Option<String> {
        self.app.read(cx).provider_update_command(provider)
    }

    pub fn provider_profile_stored_secret_names(
        &self,
        profile_id: &str,
        cx: &App,
    ) -> HashSet<String> {
        self.app
            .read(cx)
            .launch_env_for_profile(profile_id)
            .env
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    pub fn provider_profile_model_catalog(
        &self,
        profile_id: &str,
        cx: &App,
    ) -> Vec<agent::ModelSpec> {
        self.profile_catalog(profile_id, cx)
    }

    pub fn provider_dialog_models(
        &self,
        profile_id: &str,
        custom_models: &[String],
        hidden_models: &[String],
        cx: &App,
    ) -> Vec<ResolvedModel> {
        let mut settings = self.provider_profile_settings(profile_id, cx);
        settings.custom_models = custom_models.to_vec();
        settings.hidden_models = hidden_models.to_vec();
        resolve_models(
            &self.profile_catalog(profile_id, cx),
            &settings,
            &self.settings_replica.favorite_models,
        )
    }

    pub fn installed_acp_agent(
        &self,
        agent_id: &str,
        _cx: &App,
    ) -> Option<tcode_core::acp::InstalledAcpAgent> {
        self.settings_replica.acp_agent(agent_id).cloned()
    }

    pub fn acp_marketplace_items(&self, cx: &App) -> Vec<AcpMarketplaceItem> {
        let mut items = self.app.read(cx).acp_marketplace_items();
        for item in &mut items {
            item.installed = self.settings_replica.acp_agents.contains_key(&item.id);
        }
        items
    }

    pub fn acp_registry_loading(&self, cx: &App) -> bool {
        self.app.read(cx).acp_registry_loading
    }

    pub fn acp_registry_error(&self, cx: &App) -> Option<String> {
        self.app.read(cx).acp_registry_error.clone()
    }

    pub fn acp_installing(&self, agent_id: &str, cx: &App) -> bool {
        self.app.read(cx).acp_installing.contains(agent_id)
    }

    pub fn project_ids(&self, _cx: &App) -> Vec<String> {
        self.index_replica
            .1
            .iter()
            .map(|project| project.id.clone())
            .collect()
    }

    pub fn project_summary(&self, project_id: &str, _cx: &App) -> Option<(String, usize)> {
        let project = self
            .index_replica
            .1
            .iter()
            .find(|project| project.id == project_id)?;
        let count = self
            .index_replica
            .0
            .iter()
            .filter(|meta| meta.project_id.as_deref() == Some(project_id))
            .count();
        Some((project.name.clone(), count))
    }

    pub fn project_root(&self, project_id: &str, _cx: &App) -> Option<PathBuf> {
        self.index_replica
            .1
            .iter()
            .find(|project| project.id == project_id)
            .map(|project| project.root.clone())
    }

    pub fn project_id_for_root(&self, root: &std::path::Path, _cx: &App) -> Option<String> {
        self.index_replica
            .1
            .iter()
            .find(|project| project.root == root)
            .map(|project| project.id.clone())
    }

    pub fn scan_external_history(&self, cx: &App) -> Task<Vec<RecentDir>> {
        self.app
            .read(cx)
            .scan_external_history(cx.background_executor())
    }

    pub fn start_external_import(
        &self,
        project_id: &str,
        threads: Vec<ExternalThread>,
        cx: &App,
    ) -> Option<async_channel::Receiver<ExternalImportUpdate>> {
        self.app
            .read(cx)
            .start_external_import(project_id, threads, cx.background_executor())
    }

    pub fn commit_dialog_state(&self, cx: &App) -> (Vec<GitFileEntry>, Option<String>, bool) {
        let app = self.app.read(cx);
        (
            app.git_changed_files(),
            app.git_branch_name(),
            app.git_on_default_branch(),
        )
    }

    pub(crate) fn diff_active_state(&self, cx: &App) -> Option<DiffActiveState> {
        self.app
            .read(cx)
            .active
            .as_ref()
            .map(|active| DiffActiveState {
                session: active.meta.id.clone(),
                cwd: active.meta.cwd.clone(),
                branches: active.branches.clone(),
            })
    }

    pub fn diff_turns(&self, cx: &App) -> Vec<usize> {
        self.with_active_timeline(cx, |timeline| {
            timeline
                .turns
                .iter()
                .enumerate()
                .filter_map(|(turn, meta)| {
                    meta.changes
                        .as_ref()
                        .is_some_and(|changes| !changes.changes.is_empty())
                        .then_some(turn)
                })
                .collect()
        })
        .unwrap_or_default()
    }

    pub fn diff_selected_turn(&self, cx: &App) -> Option<usize> {
        let turns = self.diff_turns(cx);
        let explicit = self
            .app
            .read(cx)
            .active
            .as_ref()
            .and_then(|active| active.diff_selected_turn);
        match explicit {
            Some(turn) if turns.contains(&turn) => Some(turn),
            _ => turns.last().copied(),
        }
    }

    pub fn with_diff_turn_changes<R>(
        &self,
        turn: usize,
        cx: &App,
        read: impl FnOnce(&[agent::FileChange], agent::ChangeCompleteness) -> R,
    ) -> Option<R> {
        self.with_active_timeline(cx, |timeline| {
            let changes = timeline.turns.get(turn)?.changes.as_ref()?;
            Some(read(&changes.changes, changes.completeness))
        })
        .flatten()
    }

    pub(crate) fn pending_diff_focus(&self, cx: &App) -> Option<DiffFocus> {
        self.app
            .read(cx)
            .pending_diff_focus()
            .map(|request| DiffFocus {
                session: request.session.clone(),
                turn: request.turn,
                path: request.path.clone(),
            })
    }

    pub(crate) fn take_diff_focus(
        &mut self,
        session: &str,
        turn: usize,
        cx: &mut Context<Self>,
    ) -> Option<DiffFocus> {
        self.app
            .update(cx, |app, _| app.take_diff_focus(session, turn))
            .map(|request| DiffFocus {
                session: request.session,
                turn: request.turn,
                path: request.path,
            })
    }

    pub fn diff_refresh_generation(&self, cx: &App) -> u64 {
        self.app.read(cx).diff_refresh_generation
    }

    pub fn diff_word_wrap(&self, _cx: &App) -> bool {
        self.settings_replica.word_wrap_diffs
    }

    pub fn diff_panel_chrome_state(
        &self,
        cx: &App,
    ) -> (bool, bool, tcode_core::ui::RightTab, bool) {
        let app = self.app.read(cx);
        let plan_tab_active_label = self
            .with_active_timeline(cx, |timeline| timeline.proposed_plan.is_some())
            .unwrap_or(false)
            || app.active_interaction_mode() == agent::InteractionMode::Plan;
        (
            app.diff_panel_open(),
            app.diff_panel_expanded(),
            app.right_tab(),
            plan_tab_active_label,
        )
    }

    pub fn diff_review_comments(&self, cx: &App) -> Vec<ReviewComment> {
        self.app.read(cx).review_comments().to_vec()
    }

    pub fn with_diff_review_comments<R>(
        &self,
        cx: &App,
        read: impl FnOnce(&[ReviewComment]) -> R,
    ) -> R {
        read(self.app.read(cx).review_comments())
    }

    pub fn load_git_diff(
        cwd: &std::path::Path,
        scope: GitDiffScope,
        base: Option<&str>,
        ignore_whitespace: bool,
    ) -> GitDiffResult {
        tcode_runtime::ui_facade::load_git_diff_opts(cwd, scope, base, ignore_whitespace)
    }

    pub fn read_diff_working_tree_file(path: &std::path::Path) -> Option<String> {
        const MAX_BYTES: u64 = 512 * 1024;
        let metadata = std::fs::metadata(path).ok()?;
        if metadata.len() > MAX_BYTES {
            return None;
        }
        let text = std::fs::read_to_string(path).ok()?;
        (text.len() as u64 <= MAX_BYTES).then_some(text)
    }

    pub fn with_active_timeline<R>(
        &self,
        _cx: &App,
        read: impl FnOnce(&Timeline) -> R,
    ) -> Option<R> {
        self.session_replica
            .as_ref()
            .map(|(_, timeline)| read(timeline))
    }

    #[cfg(test)]
    pub(crate) fn set_session_replica_for_test(&mut self, session_id: String, timeline: Timeline) {
        self.session_replica = Some((session_id, timeline));
    }

    pub fn with_composer_destination<R>(
        &self,
        cx: &App,
        read: impl FnOnce(bool, &str, Option<&str>) -> R,
    ) -> Option<R> {
        self.app.read(cx).active.as_ref().map(|active| {
            read(
                active.draft,
                &active.meta.id,
                active.meta.project_id.as_deref(),
            )
        })
    }

    pub fn composer_has_active_session(&self, cx: &App) -> bool {
        self.app.read(cx).active.is_some()
    }

    pub fn take_native_rewind_prefill(&mut self, cx: &mut Context<Self>) -> Option<String> {
        self.app
            .update(cx, |app, _| app.take_native_rewind_prefill())
    }

    pub fn composer_terminal_contexts(&self, cx: &App) -> Vec<TerminalContext> {
        self.app
            .read(cx)
            .active
            .as_ref()
            .map(|active| active.terminal_workspace.contexts.clone())
            .unwrap_or_default()
    }

    /// Borrows the live terminal workspace for terminal emulation and PTY I/O.
    ///
    /// Terminal lifecycle and preference mutations still cross [`Command`];
    /// the drawer uses this only for operations on the live `term::Terminal`
    /// objects, whose APIs mutate through shared references.
    pub fn with_terminal_workspace<R>(
        &self,
        cx: &App,
        read: impl FnOnce(&TerminalWorkspace) -> R,
    ) -> Option<R> {
        self.app
            .read(cx)
            .active
            .as_ref()
            .map(|active| read(&active.terminal_workspace))
    }

    pub fn composer_review_comments(&self, cx: &App) -> Vec<ReviewComment> {
        self.app.read(cx).review_comments().to_vec()
    }

    pub fn composer_relay_confirmation(&self, cx: &App) -> Option<(String, String)> {
        self.app.read(cx).relay_confirmation()
    }

    pub fn composer_active_cwd(&self, cx: &App) -> Option<PathBuf> {
        self.app.read(cx).active_cwd()
    }

    pub fn list_active_workspace(&self, cx: &App) -> Task<Vec<PathEntry>> {
        self.app
            .read(cx)
            .list_active_workspace(cx.background_executor())
    }

    pub fn composer_provider_commands(&self, cx: &App) -> Vec<agent::ProviderCommand> {
        self.app.read(cx).active_provider_commands().to_vec()
    }

    pub fn composer_attachments_dir(&self, cx: &App) -> Option<PathBuf> {
        self.app.read(cx).attachments_dir()
    }

    pub fn save_attachment_to_dir(
        dir: &std::path::Path,
        bytes: &[u8],
        ext: &str,
    ) -> std::io::Result<PathBuf> {
        AppState::save_attachment_to_dir(dir, bytes, ext)
    }

    pub fn remove_user_file(&self, path: &std::path::Path) -> std::io::Result<()> {
        tcode_runtime::ui_facade::remove_user_file(path)
    }

    pub fn composer_pending_user_input(
        &self,
        cx: &App,
    ) -> Option<(String, Vec<agent::UserInputQuestion>)> {
        self.with_active_timeline(cx, |timeline| timeline.pending_user_input.clone())
            .flatten()
    }

    pub(crate) fn composer_active_model(&self, cx: &App) -> Option<ComposerActiveModel> {
        self.app
            .read(cx)
            .active
            .as_ref()
            .map(|active| ComposerActiveModel {
                provider: active.meta.provider,
                model: active.meta.model.clone(),
                acp_agent_id: active.meta.acp_agent_id.clone(),
                profile_id: active.meta.profile_id.clone(),
            })
    }

    pub fn composer_picker_models(
        &self,
        provider: agent::ProviderKind,
        cx: &App,
    ) -> Vec<ResolvedModel> {
        self.app.read(cx).picker_models(provider)
    }

    pub fn composer_models_loading(&self, provider: agent::ProviderKind, cx: &App) -> bool {
        self.app.read(cx).models_loading(provider)
    }

    pub fn composer_model_pending_restart(&self, cx: &App) -> bool {
        self.app.read(cx).model_pending_restart()
    }

    pub fn composer_active_model_spec(&self, cx: &App) -> Option<agent::ModelSpec> {
        self.app.read(cx).active_model_spec()
    }

    pub fn composer_active_option_descriptors(&self, cx: &App) -> Vec<agent::OptionDescriptor> {
        self.app.read(cx).active_option_descriptors()
    }

    pub fn composer_active_option_selections(&self, cx: &App) -> Vec<agent::OptionSelection> {
        self.app.read(cx).active_option_selections()
    }

    pub fn composer_ultrathink_armed(&self, cx: &App) -> bool {
        self.app.read(cx).ultrathink_armed()
    }

    pub fn composer_options_pending_restart(&self, cx: &App) -> bool {
        self.app.read(cx).options_pending_restart()
    }

    pub fn composer_interaction_mode(&self, cx: &App) -> agent::InteractionMode {
        self.app.read(cx).active_interaction_mode()
    }

    pub fn composer_context(
        &self,
        cx: &App,
    ) -> (Option<agent::TokenUsage>, Option<agent::ProviderKind>) {
        let provider = self
            .app
            .read(cx)
            .active
            .as_ref()
            .map(|active| active.meta.provider);
        (
            self.with_active_timeline(cx, |timeline| timeline.usage)
                .flatten(),
            provider,
        )
    }

    pub fn composer_approval_mode(&self, cx: &App) -> agent::ApprovalMode {
        self.app.read(cx).active_approval_mode()
    }

    pub fn composer_approval_pending_restart(&self, cx: &App) -> bool {
        self.app.read(cx).approval_pending_restart()
    }

    pub fn composer_queue(&self, cx: &App) -> Option<(Vec<QueuedMessage>, bool, &'static str)> {
        self.app.read(cx).active.as_ref().map(|active| {
            (
                active.queued().to_vec(),
                active.supports_steering(),
                active.meta.provider.display_name(),
            )
        })
    }

    pub fn composer_supports_steering(&self, cx: &App) -> bool {
        self.app
            .read(cx)
            .active
            .as_ref()
            .is_some_and(|active| active.supports_steering())
    }

    pub fn composer_preparing_worktree(&self, cx: &App) -> bool {
        self.app.read(cx).preparing_worktree()
    }

    pub fn composer_plan_ready_markdown(&self, cx: &App) -> Option<String> {
        self.with_active_timeline(cx, |timeline| {
            timeline.plan_ready().map(|plan| plan.markdown.clone())
        })
        .flatten()
    }

    pub(crate) fn composer_checkout_state(&self, cx: &App) -> Option<ComposerCheckoutState> {
        let app = self.app.read(cx);
        let active = app.active.as_ref()?;
        let branch = active.git_branch.clone().or_else(|| {
            active
                .meta
                .worktree
                .as_ref()
                .map(|worktree| worktree.branch.clone())
        })?;
        let worktree_base = match app.draft_workspace_mode() {
            Some(tcode_core::ui::WorkspaceMode::NewWorktree { base }) => Some(base),
            _ => None,
        };
        Some(ComposerCheckoutState {
            branch,
            branches: active.branches.clone(),
            turn_running: active.is_turn_running(),
            is_draft: active.draft,
            worktree_base,
            worktree: active.meta.worktree.clone(),
        })
    }

    pub fn composer_render_state(&self, cx: &App) -> (bool, Option<agent::ApprovalRequest>, usize) {
        let turn_running = self
            .app
            .read(cx)
            .active
            .as_ref()
            .is_some_and(|active| active.is_turn_running());
        self.with_active_timeline(cx, |timeline| {
            (
                turn_running,
                timeline.pending_approvals.first().cloned(),
                timeline.pending_approvals.len(),
            )
        })
        .unwrap_or((turn_running, None, 0))
    }

    pub fn composer_is_favorite_model(&self, model: &str, cx: &App) -> bool {
        self.app.read(cx).is_favorite_model(model)
    }

    pub fn chat_active_session(&self, cx: &App) -> Option<(String, PathBuf, bool)> {
        self.app.read(cx).active.as_ref().map(|active| {
            (
                active.meta.title.clone(),
                active.meta.cwd.clone(),
                active.draft,
            )
        })
    }

    pub fn chat_requested_model(&self, cx: &App) -> Option<String> {
        self.app
            .read(cx)
            .active
            .as_ref()
            .and_then(|active| active.meta.model.clone())
    }

    pub fn chat_turn_changes(
        &self,
        turn: usize,
        cx: &App,
    ) -> (Vec<agent::FileChange>, agent::ChangeCompleteness) {
        self.with_active_timeline(cx, |timeline| {
            timeline
                .turns
                .get(turn)
                .and_then(|turn| turn.changes.as_ref())
                .map(|changes| (changes.changes.clone(), changes.completeness))
        })
        .flatten()
        .unwrap_or((Vec::new(), agent::ChangeCompleteness::Partial))
    }

    pub fn chat_native_rewind_state(&self, turn: usize, cx: &App) -> Option<(bool, bool)> {
        let app = self.app.read(cx);
        let active = app.active.as_ref()?;
        let has_checkpoint = self
            .with_active_timeline(cx, |timeline| {
                timeline
                    .turns
                    .get(turn)
                    .and_then(|turn| turn.provider_checkpoint_id.as_ref())
                    .is_some()
            })
            .unwrap_or(false);
        Some((
            active.meta.provider == agent::ProviderKind::ClaudeCode && has_checkpoint,
            active.is_turn_running() || !active.queued().is_empty() || app.native_rewind_pending(),
        ))
    }

    pub fn chat_panel_state(
        &self,
        cx: &App,
    ) -> (bool, tcode_core::ui::RightTab, bool, bool, bool, f32) {
        let app = self.app.read(cx);
        (
            app.diff_panel_open(),
            app.right_tab(),
            app.plan_panel_showing(),
            app.preview_panel_showing(),
            app.terminal_panel_open(),
            app.active
                .as_ref()
                .map(|active| active.terminal_workspace.height)
                .unwrap_or(240.),
        )
    }

    pub fn chat_git_controls(&self, cx: &App) -> Option<(QuickAction, Vec<MenuItem>)> {
        let app = self.app.read(cx);
        app.git_quick_action()
            .map(|quick| (quick, app.git_menu_items()))
    }

    pub fn chat_git_status_loaded(&self, cx: &App) -> bool {
        self.app.read(cx).git_status.is_some()
    }

    pub(crate) fn new_composer(
        store: Entity<Self>,
        window_state: Entity<WindowState>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Composer> {
        cx.new(|cx| Composer::new(store, window_state, window, cx))
    }

    pub(crate) fn new_terminal_drawer(store: Entity<Self>, cx: &mut App) -> Entity<TerminalDrawer> {
        cx.new(|cx| TerminalDrawer::new(store, cx))
    }

    pub fn generate_commit_message(
        &self,
        included: Option<Vec<String>>,
        cx: &App,
    ) -> Task<Result<String, String>> {
        self.app.read(cx).generate_commit_message(included, cx)
    }

    pub fn plan_panel_state(&self, cx: &App) -> (Option<String>, Vec<agent::PlanStep>) {
        self.with_active_timeline(cx, |timeline| {
            (
                timeline
                    .proposed_plan
                    .as_ref()
                    .map(|plan| plan.markdown.clone()),
                timeline.plan_steps.clone(),
            )
        })
        .unwrap_or_default()
    }

    pub fn archived_session_count(&self, project_id: Option<&str>, _cx: &App) -> usize {
        self.index_replica
            .0
            .iter()
            .filter(|meta| {
                meta.archived_at.is_some()
                    && project_id.is_none_or(|id| meta.project_id.as_deref() == Some(id))
            })
            .count()
    }

    pub fn worktree_orphaned_by_delete(&self, session_id: &str, _cx: &App) -> Option<WorktreeInfo> {
        let meta = self
            .index_replica
            .0
            .iter()
            .find(|meta| meta.id == session_id)?;
        let worktree = meta.worktree.clone()?;
        let shared = self.index_replica.0.iter().any(|meta| {
            meta.id != session_id
                && meta
                    .worktree
                    .as_ref()
                    .is_some_and(|other| other.branch == worktree.branch)
        });
        (!shared).then_some(worktree)
    }

    pub(crate) fn open_add_project_dialog(store: Entity<Self>, window: &mut Window, cx: &mut App) {
        crate::add_project_dialog::open(store, window, cx);
    }
}

impl EventEmitter<RuntimeEvent> for WorkspaceStore {}

#[cfg(test)]
mod tests {
    use agent::{AgentEvent, ItemContent, ProviderKind, ThreadItem, TurnStatus};
    use gpui::{AppContext as _, TestAppContext};
    use tcode_core::project::{Project, SessionMeta};
    use tcode_protocol::{Command, EventEnvelope, ServerEvent, SessionEventRecord, Topic};
    use tcode_runtime::{app::AppState, event::HostEvent};
    use tcode_services::store::SessionStore;

    use super::WorkspaceStore;

    #[gpui::test]
    fn session_replica_matches_live_timeline_for_synthetic_turn(cx: &mut TestAppContext) {
        let root = std::env::temp_dir().join(format!(
            "tcode-session-replica-consistency-test-{}",
            tcode_services::store::now_millis()
        ));
        let session_store = SessionStore::open_at(root.clone()).expect("open test store");
        let meta = SessionMeta::new(ProviderKind::Codex, root.join("worktree"), None);
        let session_id = meta.id.clone();
        session_store.upsert_meta(&meta).expect("persist session");
        let events = [
            AgentEvent::ItemCompleted(ThreadItem {
                id: "user-1".into(),
                parent_item_id: None,
                content: ItemContent::UserMessage {
                    text: "replicate this turn".into(),
                    context_len: None,
                    attachments: Vec::new(),
                },
            }),
            AgentEvent::TurnStarted {
                turn_id: "turn-1".into(),
            },
            AgentEvent::ItemCompleted(ThreadItem {
                id: "assistant-1".into(),
                parent_item_id: None,
                content: ItemContent::AssistantMessage {
                    text: "replicated".into(),
                },
            }),
            AgentEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
        ];
        for (offset, event) in events.iter().enumerate() {
            session_store
                .append_event(&session_id, 100 + offset as u64, event)
                .expect("persist synthetic event");
        }

        let app = cx.new(|_| AppState::new(session_store));
        let workspace = cx.new(|cx| WorkspaceStore::new(app.clone(), cx));
        workspace.update(cx, |store, cx| {
            store.dispatch(
                Command::SelectSession {
                    session_id: session_id.clone(),
                },
                cx,
            );
        });
        cx.run_until_parked();

        let live_events = [
            AgentEvent::ItemCompleted(ThreadItem {
                id: "user-2".into(),
                parent_item_id: None,
                content: ItemContent::UserMessage {
                    text: "apply incrementally".into(),
                    context_len: None,
                    attachments: Vec::new(),
                },
            }),
            AgentEvent::TurnStarted {
                turn_id: "turn-2".into(),
            },
            AgentEvent::ItemCompleted(ThreadItem {
                id: "assistant-2".into(),
                parent_item_id: None,
                content: ItemContent::AssistantMessage {
                    text: "incremental replica".into(),
                },
            }),
            AgentEvent::TurnCompleted {
                turn_id: "turn-2".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
        ];
        for (offset, event) in live_events.into_iter().enumerate() {
            app.update(cx, |state, cx| {
                state
                    .active
                    .as_mut()
                    .expect("active session")
                    .timeline
                    .apply_at(Some(200 + offset as u64), &event);
                cx.emit(HostEvent::Domain(EventEnvelope {
                    topic: Topic::SessionEvents {
                        session_id: session_id.clone(),
                    },
                    seq: 10 + offset as u64,
                    event: ServerEvent::SessionEvent(SessionEventRecord {
                        ts: Some(200 + offset as u64),
                        event,
                    }),
                }));
            });
        }

        let live = app.read_with(cx, |state, _| {
            let timeline = &state.active.as_ref().expect("active session").timeline;
            (
                timeline
                    .entries
                    .iter()
                    .map(|entry| (entry.id.clone(), entry.turn, format!("{:?}", entry.content)))
                    .collect::<Vec<_>>(),
                timeline.turns.len(),
            )
        });
        let replica = workspace.read_with(cx, |store, _| {
            let (id, timeline) = store.session_replica.as_ref().expect("session replica");
            assert_eq!(id, &session_id);
            (
                timeline
                    .entries
                    .iter()
                    .map(|entry| (entry.id.clone(), entry.turn, format!("{:?}", entry.content)))
                    .collect::<Vec<_>>(),
                timeline.turns.len(),
            )
        });
        assert_eq!(replica, live);

        std::fs::remove_dir_all(root).expect("remove test data");
    }

    #[gpui::test]
    fn index_and_settings_replicas_follow_representative_commands(cx: &mut TestAppContext) {
        let root = std::env::temp_dir().join(format!(
            "tcode-replica-consistency-test-{}",
            tcode_services::store::now_millis()
        ));
        let session_store = SessionStore::open_at(root.clone()).expect("open test store");
        let app = cx.new(|_| AppState::new(session_store));
        let seed_project = Project::from_root(root.join("seed"));
        let mut seed_session =
            SessionMeta::new(ProviderKind::Codex, seed_project.root.clone(), None);
        seed_session.project_id = Some(seed_project.id.clone());
        let seed_session_id = seed_session.id.clone();
        app.update(cx, |state, _| {
            let AppState {
                projects, sessions, ..
            } = state;
            projects.push(seed_project);
            sessions.push(seed_session);
        });
        let workspace = cx.new(|cx| WorkspaceStore::new(app.clone(), cx));

        workspace.update(cx, |store, cx| {
            store.dispatch(
                Command::CreateProject {
                    root: root.join("created"),
                },
                cx,
            );
            store.dispatch(
                Command::ArchiveSession {
                    session_id: seed_session_id,
                },
                cx,
            );
            let mut settings = store.settings_page_settings(cx);
            settings.word_wrap_diffs = !settings.word_wrap_diffs;
            store.dispatch(Command::UpdateSettings { settings }, cx);
        });

        let live_index = app.read_with(cx, |state, _| {
            let AppState {
                sessions, projects, ..
            } = state;
            (
                serde_json::to_value(sessions).unwrap(),
                serde_json::to_value(projects).unwrap(),
            )
        });
        let live_settings = app.read_with(cx, |state, _| {
            serde_json::to_value(&state.settings).unwrap()
        });
        let replica_index = workspace.read_with(cx, |store, _| {
            (
                serde_json::to_value(&store.index_replica.0).unwrap(),
                serde_json::to_value(&store.index_replica.1).unwrap(),
            )
        });
        let replica_settings = workspace.read_with(cx, |store, _| {
            serde_json::to_value(&store.settings_replica).unwrap()
        });
        assert_eq!(
            replica_index.0, live_index.0,
            "session replica diverged from live state"
        );
        assert_eq!(
            replica_index.1, live_index.1,
            "project replica diverged from live state"
        );
        assert_eq!(
            replica_settings, live_settings,
            "settings replica diverged from live state"
        );

        std::fs::remove_dir_all(root).expect("remove test data");
    }
}
