use std::collections::HashSet;
use std::path::PathBuf;

use gpui::{App, AppContext as _, Context, Entity, Task, Window};
use tcode_core::{
    git::{GitFileEntry, MenuItem, QuickAction},
    project::{SessionMeta, WorktreeInfo},
    provider_models::ResolvedModel,
    provider_status::ProviderSnapshot,
    session::{ReviewComment, Timeline},
    settings::{ProjectSort, ProviderSettings, ResolvedProfile, Settings},
};
use tcode_protocol::Command;
use tcode_runtime::{
    app::{AppState, ProjectGroup, ProviderVersionStatus, QueuedMessage},
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
        cx.observe(&app, |_, _, cx| cx.notify()).detach();
        Self { app }
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

    pub fn grouped_sessions(&self, cx: &App) -> Vec<ProjectGroup> {
        self.app.read(cx).grouped_sessions()
    }

    pub fn palette_groups(&self, cx: &App) -> Vec<ProjectGroup> {
        self.app.read(cx).grouped_sessions()
    }

    pub fn palette_settings(&self, cx: &App) -> Settings {
        self.app.read(cx).settings.clone()
    }

    pub fn archived_groups(&self, cx: &App) -> Vec<ProjectGroup> {
        self.app.read(cx).archived_groups()
    }

    pub fn project_sort(&self, cx: &App) -> ProjectSort {
        self.app.read(cx).project_sort()
    }

    pub fn is_project_collapsed(&self, project_id: &str, cx: &App) -> bool {
        self.app.read(cx).is_project_collapsed(project_id)
    }

    pub fn active_session_id(&self, cx: &App) -> Option<String> {
        self.app.read(cx).active_session_id().map(str::to_owned)
    }

    pub fn turn_running_for(&self, session_id: &str, cx: &App) -> bool {
        self.app.read(cx).turn_running_for(session_id)
    }

    pub fn session_unread(&self, session_id: &str, cx: &App) -> bool {
        self.app.read(cx).session_unread(session_id)
    }

    pub fn pending_approval_for(&self, session_id: &str, cx: &App) -> bool {
        self.app.read(cx).pending_approval_for(session_id).is_some()
    }

    pub fn fork_availability(&self, session_id: &str, cx: &App) -> ForkAvailability {
        let app = self.app.read(cx);
        let Some(meta) = app.sessions.iter().find(|meta| meta.id == session_id) else {
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

    pub fn sidebar_sessions(&self, cx: &App) -> Vec<SessionMeta> {
        self.app.read(cx).sessions.clone()
    }

    pub fn sidebar_settings(&self, cx: &App) -> Settings {
        self.app.read(cx).settings.clone()
    }

    pub fn orchestrate_editor_settings(&self, cx: &App) -> Settings {
        self.app.read(cx).settings.clone()
    }

    pub fn settings_page_settings(&self, cx: &App) -> Settings {
        self.app.read(cx).settings.clone()
    }

    pub fn settings_provider_profiles(&self, cx: &App) -> Vec<ResolvedProfile> {
        self.app.read(cx).all_profiles()
    }

    pub fn settings_installed_acp_agents(
        &self,
        cx: &App,
    ) -> Vec<tcode_core::acp::InstalledAcpAgent> {
        self.app
            .read(cx)
            .settings
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

    pub fn enabled_provider_profiles(&self, cx: &App) -> Vec<ResolvedProfile> {
        self.app.read(cx).enabled_profiles()
    }

    pub fn provider_profile_kind(&self, profile_id: &str, cx: &App) -> agent::ProviderKind {
        self.app.read(cx).profile_kind(profile_id)
    }

    pub fn provider_profile_settings(&self, profile_id: &str, cx: &App) -> ProviderSettings {
        self.app.read(cx).profile_settings(profile_id)
    }

    pub fn provider_model_catalog(
        &self,
        provider: agent::ProviderKind,
        cx: &App,
    ) -> Vec<agent::ModelSpec> {
        self.app.read(cx).models_for(provider).to_vec()
    }

    pub fn picker_models_for_profile(&self, profile_id: &str, cx: &App) -> Vec<ResolvedModel> {
        self.app.read(cx).picker_models_for_profile(profile_id)
    }

    pub fn provider_profile_display_name(&self, profile_id: &str, cx: &App) -> String {
        self.app.read(cx).profile_display_name(profile_id)
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

    pub fn provider_profile_accent(&self, profile_id: &str, cx: &App) -> Option<u32> {
        self.app.read(cx).profile_accent(profile_id)
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
        self.app.read(cx).profile_catalog(profile_id)
    }

    pub fn provider_dialog_models(
        &self,
        profile_id: &str,
        custom_models: &[String],
        hidden_models: &[String],
        cx: &App,
    ) -> Vec<ResolvedModel> {
        self.app
            .read(cx)
            .draft_models_for_profile(profile_id, custom_models, hidden_models)
    }

    pub fn installed_acp_agent(
        &self,
        agent_id: &str,
        cx: &App,
    ) -> Option<tcode_core::acp::InstalledAcpAgent> {
        self.app.read(cx).settings.acp_agent(agent_id).cloned()
    }

    pub fn acp_marketplace_items(&self, cx: &App) -> Vec<AcpMarketplaceItem> {
        self.app.read(cx).acp_marketplace_items()
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

    pub fn project_ids(&self, cx: &App) -> Vec<String> {
        self.app
            .read(cx)
            .projects
            .iter()
            .map(|project| project.id.clone())
            .collect()
    }

    pub fn project_summary(&self, project_id: &str, cx: &App) -> Option<(String, usize)> {
        let app = self.app.read(cx);
        let project = app
            .projects
            .iter()
            .find(|project| project.id == project_id)?;
        let count = app
            .sessions
            .iter()
            .filter(|meta| meta.project_id.as_deref() == Some(project_id))
            .count();
        Some((project.name.clone(), count))
    }

    pub fn project_root(&self, project_id: &str, cx: &App) -> Option<PathBuf> {
        self.app
            .read(cx)
            .projects
            .iter()
            .find(|project| project.id == project_id)
            .map(|project| project.root.clone())
    }

    pub fn project_id_for_root(&self, root: &std::path::Path, cx: &App) -> Option<String> {
        self.app
            .read(cx)
            .projects
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
        self.app.read(cx).diff_turns()
    }

    pub fn diff_selected_turn(&self, cx: &App) -> Option<usize> {
        self.app.read(cx).diff_selected_turn()
    }

    pub fn with_diff_turn_changes<R>(
        &self,
        turn: usize,
        cx: &App,
        read: impl FnOnce(&[agent::FileChange], agent::ChangeCompleteness) -> R,
    ) -> Option<R> {
        let app = self.app.read(cx);
        let active = app.active.as_ref()?;
        let changes = active.timeline.turns.get(turn)?.changes.as_ref()?;
        Some(read(&changes.changes, changes.completeness))
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

    pub fn diff_word_wrap(&self, cx: &App) -> bool {
        self.app.read(cx).settings.word_wrap_diffs
    }

    pub fn diff_panel_chrome_state(
        &self,
        cx: &App,
    ) -> (bool, bool, tcode_core::ui::RightTab, bool) {
        let app = self.app.read(cx);
        (
            app.diff_panel_open(),
            app.diff_panel_expanded(),
            app.right_tab(),
            app.plan_tab_active_label(),
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
        cx: &App,
        read: impl FnOnce(&Timeline) -> R,
    ) -> Option<R> {
        self.app
            .read(cx)
            .active
            .as_ref()
            .map(|active| read(&active.timeline))
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
        self.app
            .read(cx)
            .active
            .as_ref()
            .map_or((None, None), |active| {
                (active.timeline.usage, Some(active.meta.provider))
            })
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
        self.app.read(cx).plan_ready_markdown()
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
            turn_running: active.timeline.turn_running,
            is_draft: active.draft,
            worktree_base,
            worktree: active.meta.worktree.clone(),
        })
    }

    pub fn composer_render_state(&self, cx: &App) -> (bool, Option<agent::ApprovalRequest>, usize) {
        self.with_active_timeline(cx, |timeline| {
            (
                timeline.turn_running,
                timeline.pending_approvals.first().cloned(),
                timeline.pending_approvals.len(),
            )
        })
        .unwrap_or((false, None, 0))
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
        let app = self.app.read(cx);
        (
            app.turn_file_changes(turn).unwrap_or_default(),
            app.turn_change_completeness(turn)
                .unwrap_or(agent::ChangeCompleteness::Partial),
        )
    }

    pub fn chat_native_rewind_state(&self, turn: usize, cx: &App) -> Option<(bool, bool)> {
        let app = self.app.read(cx);
        let active = app.active.as_ref()?;
        Some((
            active.meta.provider == agent::ProviderKind::ClaudeCode
                && active
                    .timeline
                    .turns
                    .get(turn)
                    .and_then(|turn| turn.provider_checkpoint_id.as_ref())
                    .is_some(),
            active.is_turn_running()
                || active.timeline.turn_running
                || !active.queued().is_empty()
                || app.native_rewind_pending(),
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
        let app = self.app.read(cx);
        (app.proposed_plan_markdown(), app.plan_steps())
    }

    pub fn archived_session_count(&self, project_id: Option<&str>, cx: &App) -> usize {
        self.app
            .read(cx)
            .sessions
            .iter()
            .filter(|meta| {
                meta.archived_at.is_some()
                    && project_id.is_none_or(|id| meta.project_id.as_deref() == Some(id))
            })
            .count()
    }

    pub fn worktree_orphaned_by_delete(&self, session_id: &str, cx: &App) -> Option<WorktreeInfo> {
        self.app.read(cx).worktree_orphaned_by_delete(session_id)
    }

    pub(crate) fn open_add_project_dialog(store: Entity<Self>, window: &mut Window, cx: &mut App) {
        crate::add_project_dialog::open(store, window, cx);
    }
}
