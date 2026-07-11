//! Application state: session registry, active session runtime, event pump.

use std::path::PathBuf;

use agent::{
    AgentEvent, ApprovalDecision, ItemContent, ProviderKind, SessionCommand, SessionOptions,
    ThreadItem, TurnStatus, start_session,
};
use gpui::{Context, EventEmitter, Task};

use crate::session::Timeline;
use crate::settings::{ProjectSort, Settings, SettingsStore};
use crate::store::{Project, SessionMeta, SessionStore, now_millis, now_secs};

const TITLE_MAX_CHARS: usize = 40;

/// A project and its sessions, ready for the sidebar (newest activity first).
#[derive(Debug, Clone)]
pub struct ProjectGroup {
    pub project: Project,
    pub sessions: Vec<SessionMeta>,
}

/// Group `sessions` under their `projects`, ordering sessions newest-activity
/// first within each group and groups per `sort`.
pub fn group_sessions(
    projects: &[Project],
    sessions: &[SessionMeta],
    sort: ProjectSort,
) -> Vec<ProjectGroup> {
    let mut groups: Vec<ProjectGroup> = projects
        .iter()
        .map(|project| {
            let mut sessions: Vec<SessionMeta> = sessions
                .iter()
                .filter(|s| s.project_id.as_deref() == Some(project.id.as_str()))
                .cloned()
                .collect();
            sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
            ProjectGroup {
                project: project.clone(),
                sessions,
            }
        })
        .collect();

    match sort {
        // Groups ordered by newest activity (falling back to project creation).
        ProjectSort::RecentActivity => groups.sort_by(|a, b| {
            let activity = |g: &ProjectGroup| {
                g.sessions
                    .iter()
                    .map(|s| s.updated_at)
                    .max()
                    .unwrap_or(g.project.created_at)
            };
            activity(b).cmp(&activity(a))
        }),
        // Groups ordered by project name, case-insensitive A-Z.
        ProjectSort::NameAsc => {
            groups.sort_by(|a, b| {
                a.project
                    .name
                    .to_lowercase()
                    .cmp(&b.project.name.to_lowercase())
            });
        }
    }
    groups
}

/// Events emitted for UI side-effects (notifications need a `Window`).
#[derive(Debug, Clone)]
pub enum AppEvent {
    Error(String),
    /// A success/info notice (e.g. a branch checkout succeeded).
    Notice(String),
}

/// The top-level window route: the chat workspace or the full-page settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Route {
    #[default]
    Chat,
    Settings,
}

/// Provider process state for the active session.
enum Runtime {
    /// Not started yet — stored session opened (replay only) or brand new.
    Idle,
    /// `start_session` is in flight; queued turns flush when it completes.
    Starting { generation: u64 },
    /// Live child process.
    Live(async_channel::Sender<SessionCommand>),
}

pub struct ActiveSession {
    pub meta: SessionMeta,
    pub timeline: Timeline,
    /// Git branch of the session cwd, if it is a git repo (display-only).
    pub git_branch: Option<String>,
    /// Local branches for the checkout-row picker, loaded lazily when the
    /// popover opens (empty until then / when not a git repo).
    pub branches: Vec<String>,
    /// A draft thread: set up (provider/model/cwd) but not yet persisted or
    /// started. Materialized into a real session on the first send.
    pub draft: bool,
    runtime: Runtime,
    /// The model the live provider process was actually started with. When the
    /// user picks a different model we compare against this to decide whether a
    /// restart is needed before the next turn.
    live_model: Option<String>,
    pending_sends: Vec<String>,
    turn_in_flight: bool,
    /// Diff panel UI state (per-session, in-memory only). Open/closed, the
    /// full-width "expand" toggle, and the explicitly-selected turn (None means
    /// "follow the latest turn that has file changes", resolved on demand).
    pub diff_open: bool,
    pub diff_expanded: bool,
    pub diff_selected_turn: Option<usize>,
    /// Bottom terminal drawer state and its lazily-spawned per-session PTY.
    pub terminal_open: bool,
    pub terminal_height: f32,
    pub terminal: Option<term::Terminal>,
    _pump: Option<Task<()>>,
}

impl ActiveSession {
    /// Whether the live provider is running a different model than the one now
    /// selected in `meta.model` (so the next turn must restart the provider).
    fn model_changed_while_live(&self) -> bool {
        matches!(self.runtime, Runtime::Live(_)) && self.meta.model != self.live_model
    }

    /// Tear down the live provider and return to `Idle` so the next
    /// `ensure_started` respawns it (with the current model + resume cursor).
    /// Queued sends are preserved so they flush once the new process is up.
    fn shutdown_to_idle(&mut self) {
        if let Runtime::Live(commands) = &self.runtime {
            let _ = commands.try_send(SessionCommand::Shutdown);
        }
        self.runtime = Runtime::Idle;
        self.turn_in_flight = false;
        self._pump = None;
    }

    /// Dispatch at most one queued send, preserving FIFO order.
    fn dispatch_next_pending(&mut self) -> Result<bool, ()> {
        if self.turn_in_flight {
            return Ok(false);
        }
        let Runtime::Live(commands) = &self.runtime else {
            return Ok(false);
        };
        let Some(text) = self.pending_sends.first().cloned() else {
            return Ok(false);
        };
        commands
            .try_send(SessionCommand::SendTurn { text })
            .map_err(|_| ())?;
        self.pending_sends.remove(0);
        self.turn_in_flight = true;
        Ok(true)
    }

    fn is_starting_generation(&self, generation: u64) -> bool {
        matches!(
            self.runtime,
            Runtime::Starting {
                generation: current
            } if current == generation
        )
    }
}

/// Smoke-mode behavior flags (see `crate::smoke`).
#[derive(Debug, Clone, Copy, Default)]
pub struct SmokeMode {
    pub auto_approve: bool,
}

pub struct AppState {
    store: SessionStore,
    settings_store: SettingsStore,
    pub sessions: Vec<SessionMeta>,
    pub projects: Vec<Project>,
    pub active: Option<ActiveSession>,
    pub settings: Settings,
    pub smoke: Option<SmokeMode>,
    /// Whether the sidebar is collapsed to an icon strip (ephemeral UI state).
    pub sidebar_collapsed: bool,
    /// Current window route (chat vs. settings page).
    pub route: Route,
    /// Whether the command palette (⌘K) overlay is showing.
    pub palette_open: bool,
    next_start_generation: u64,
}

impl EventEmitter<AppEvent> for AppState {}

impl AppState {
    pub fn new(store: SessionStore) -> Self {
        // Load + migrate once and persist so derived project ids stay stable.
        let file = store.read_file();
        if let Err(err) = store.persist_index(&file) {
            log::warn!("failed to persist migrated session index: {err}");
        }
        let mut sessions = file.sessions;
        sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        let projects = file.projects;
        let settings_store = SettingsStore::new(store.root().clone());
        let settings = settings_store.load();
        log::info!(
            "loaded {} stored session(s) in {} project(s) from {}",
            sessions.len(),
            projects.len(),
            store.root().display()
        );
        Self {
            store,
            settings_store,
            sessions,
            projects,
            active: None,
            settings,
            smoke: None,
            sidebar_collapsed: false,
            route: Route::Chat,
            palette_open: false,
            next_start_generation: 0,
        }
    }

    pub fn toggle_sidebar_collapsed(&mut self, cx: &mut Context<Self>) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        cx.notify();
    }

    // -- routing + palette --------------------------------------------------

    /// Switch to the full-page settings route (closes the palette).
    pub fn open_settings(&mut self, cx: &mut Context<Self>) {
        self.palette_open = false;
        self.route = Route::Settings;
        cx.notify();
    }

    /// Return from settings to the chat workspace.
    pub fn close_settings(&mut self, cx: &mut Context<Self>) {
        self.route = Route::Chat;
        cx.notify();
    }

    pub fn open_palette(&mut self, cx: &mut Context<Self>) {
        self.palette_open = true;
        cx.notify();
    }

    pub fn close_palette(&mut self, cx: &mut Context<Self>) {
        self.palette_open = false;
        cx.notify();
    }

    pub fn toggle_palette(&mut self, cx: &mut Context<Self>) {
        self.palette_open = !self.palette_open;
        cx.notify();
    }

    /// Reset user settings to defaults, preserving the sidebar's per-project
    /// collapsed state and the model favorites (UI state, not page settings).
    /// The theme is reset too; the caller re-applies it to the window.
    pub fn reset_settings(&mut self, cx: &mut Context<Self>) {
        let settings = Settings {
            collapsed_projects: self.settings.collapsed_projects.clone(),
            favorite_models: self.settings.favorite_models.clone(),
            ..Settings::default()
        };
        self.update_settings(settings, cx);
    }

    // -- diff panel (per-session, in-memory) --------------------------------

    /// Turn indices (ascending) whose timeline contains at least one file
    /// change, i.e. the turns the diff panel can display.
    pub fn diff_turns(&self) -> Vec<usize> {
        let Some(active) = self.active.as_ref() else {
            return Vec::new();
        };
        let mut turns: Vec<usize> = Vec::new();
        for entry in &active.timeline.entries {
            if let crate::session::EntryContent::FileChange { changes, .. } = &entry.content {
                if !changes.is_empty() && turns.last() != Some(&entry.turn) {
                    if !turns.contains(&entry.turn) {
                        turns.push(entry.turn);
                    }
                }
            }
        }
        turns.sort_unstable();
        turns
    }

    /// The turn the diff panel currently shows: the explicit selection when it
    /// still has changes, otherwise the latest turn that does.
    pub fn diff_selected_turn(&self) -> Option<usize> {
        let turns = self.diff_turns();
        let explicit = self.active.as_ref().and_then(|a| a.diff_selected_turn);
        match explicit {
            Some(t) if turns.contains(&t) => Some(t),
            _ => turns.last().copied(),
        }
    }

    pub fn diff_panel_open(&self) -> bool {
        self.active.as_ref().is_some_and(|a| a.diff_open)
    }

    pub fn diff_panel_expanded(&self) -> bool {
        self.active.as_ref().is_some_and(|a| a.diff_expanded)
    }

    /// Toggle the diff panel open/closed (header button). Opening with no prior
    /// selection defaults to the latest turn with changes.
    pub fn toggle_diff_panel(&mut self, cx: &mut Context<Self>) {
        if let Some(active) = self.active.as_mut() {
            active.diff_open = !active.diff_open;
            cx.notify();
        }
    }

    /// Open the diff panel and select `turn` (a "View diff" card button).
    pub fn open_diff_for_turn(&mut self, turn: usize, cx: &mut Context<Self>) {
        if let Some(active) = self.active.as_mut() {
            active.diff_open = true;
            active.diff_selected_turn = Some(turn);
            cx.notify();
        }
    }

    /// Open the diff panel on the latest turn with changes (used by
    /// `--open-diff` and as a general "just show me the diffs" entry point).
    pub fn open_diff_panel(&mut self, cx: &mut Context<Self>) {
        if let Some(active) = self.active.as_mut() {
            active.diff_open = true;
            cx.notify();
        }
    }

    // -- terminal drawer (per-session, in-memory) --------------------------

    pub fn terminal_panel_open(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.terminal_open)
    }

    pub fn toggle_terminal_panel(&mut self, cx: &mut Context<Self>) {
        if self.terminal_panel_open() {
            self.close_terminal_panel(cx);
        } else {
            self.open_terminal_panel(cx);
        }
    }

    pub fn open_terminal_panel(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if active.terminal.is_none() {
            match term::Terminal::spawn(&active.meta.cwd) {
                Ok(terminal) => active.terminal = Some(terminal),
                Err(error) => {
                    self.report_error(format!("failed to start terminal: {error}"), cx);
                    return;
                }
            }
        }
        active.terminal_open = true;
        cx.notify();
    }

    pub fn close_terminal_panel(&mut self, cx: &mut Context<Self>) {
        if let Some(active) = self.active.as_mut() {
            active.terminal_open = false;
            cx.notify();
        }
    }

    pub fn restart_terminal(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        match term::Terminal::spawn(&active.meta.cwd) {
            Ok(terminal) => {
                active.terminal = Some(terminal);
                active.terminal_open = true;
                cx.notify();
            }
            Err(error) => self.report_error(format!("failed to restart terminal: {error}"), cx),
        }
    }

    pub fn close_diff_panel(&mut self, cx: &mut Context<Self>) {
        if let Some(active) = self.active.as_mut() {
            active.diff_open = false;
            cx.notify();
        }
    }

    pub fn set_diff_turn(&mut self, turn: usize, cx: &mut Context<Self>) {
        if let Some(active) = self.active.as_mut() {
            active.diff_selected_turn = Some(turn);
            cx.notify();
        }
    }

    pub fn toggle_diff_expanded(&mut self, cx: &mut Context<Self>) {
        if let Some(active) = self.active.as_mut() {
            active.diff_expanded = !active.diff_expanded;
            cx.notify();
        }
    }

    /// Sessions grouped by project for the sidebar.
    pub fn grouped_sessions(&self) -> Vec<ProjectGroup> {
        group_sessions(&self.projects, &self.sessions, self.settings.project_sort)
    }

    /// The current sidebar sort mode (for the sort button tooltip/label).
    pub fn project_sort(&self) -> ProjectSort {
        self.settings.project_sort
    }

    /// Cycle the sidebar PROJECTS ordering and persist it.
    pub fn cycle_project_sort(&mut self, cx: &mut Context<Self>) {
        let mut settings = self.settings.clone();
        settings.project_sort = settings.project_sort.next();
        self.update_settings(settings, cx);
    }

    /// Create a project rooted at `root` (native picker feeds this).
    /// Returns the new project's id, or an existing one if `root` matches.
    pub fn create_project(&mut self, root: PathBuf, cx: &mut Context<Self>) -> Option<String> {
        if let Some(existing) = self.projects.iter().find(|p| p.root == root) {
            return Some(existing.id.clone());
        }
        let project = Project::from_root(root);
        if let Err(err) = self.store.upsert_project(&project) {
            self.report_error(format!("failed to persist project: {err}"), cx);
            return None;
        }
        let id = project.id.clone();
        self.projects = self.store.load_projects();
        cx.notify();
        Some(id)
    }

    /// Toggle a project's collapsed state (persisted in settings).
    pub fn toggle_project_collapsed(&mut self, project_id: &str, cx: &mut Context<Self>) {
        let mut settings = self.settings.clone();
        if let Some(pos) = settings
            .collapsed_projects
            .iter()
            .position(|id| id == project_id)
        {
            settings.collapsed_projects.remove(pos);
        } else {
            settings.collapsed_projects.push(project_id.to_string());
        }
        self.update_settings(settings, cx);
    }

    pub fn is_project_collapsed(&self, project_id: &str) -> bool {
        self.settings
            .collapsed_projects
            .iter()
            .any(|id| id == project_id)
    }

    pub fn active_session_id(&self) -> Option<&str> {
        self.active.as_ref().map(|a| a.meta.id.as_str())
    }

    pub fn update_settings(&mut self, settings: Settings, cx: &mut Context<Self>) {
        if let Err(err) = self.settings_store.save(&settings) {
            self.report_error(format!("failed to persist settings: {err}"), cx);
            return;
        }
        self.settings = settings;
        cx.notify();
    }

    pub fn delete_session(&mut self, session_id: &str, cx: &mut Context<Self>) {
        if self.active_session_id() == Some(session_id) {
            self.shutdown_active();
        }
        if let Err(err) = self.store.remove_session(session_id) {
            self.report_error(format!("failed to delete session: {err}"), cx);
            return;
        }
        self.sessions = self.store.load_index();
        cx.notify();
    }

    /// Create a new session, make it active, and start its provider process.
    pub fn create_session(
        &mut self,
        provider: ProviderKind,
        cwd: PathBuf,
        model: Option<String>,
        project_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let mut meta = SessionMeta::new(provider, cwd, model);
        // Associate with the given project, or derive one from the cwd.
        meta.project_id = match project_id {
            Some(id) if self.projects.iter().any(|p| p.id == id) => Some(id),
            _ => self.create_project(meta.cwd.clone(), cx),
        };
        if let Err(err) = self.store.upsert_meta(&meta) {
            self.report_error(format!("failed to persist session: {err}"), cx);
        }
        self.sessions = self.store.load_index();
        self.shutdown_active();
        let git_branch = read_git_branch(&meta.cwd);
        self.active = Some(ActiveSession {
            meta,
            timeline: Timeline::default(),
            git_branch,
            branches: Vec::new(),
            draft: false,
            runtime: Runtime::Idle,
            live_model: None,
            pending_sends: Vec::new(),
            turn_in_flight: false,
            diff_open: false,
            diff_expanded: false,
            diff_selected_turn: None,
            terminal_open: false,
            terminal_height: 240.,
            terminal: None,
            _pump: None,
        });
        self.ensure_started(cx);
        cx.notify();
    }

    // -- draft threads ------------------------------------------------------

    /// Build a draft `ActiveSession` for `cwd` under `project_id`: set up but
    /// not persisted or started (see `commit_draft`). Pure (no store/cx) so the
    /// draft flow is unit-testable.
    fn build_draft_session(
        project_id: String,
        cwd: PathBuf,
        provider: ProviderKind,
        model: Option<String>,
    ) -> ActiveSession {
        let mut meta = SessionMeta::new(provider, cwd, model);
        meta.project_id = Some(project_id);
        let git_branch = read_git_branch(&meta.cwd);
        ActiveSession {
            meta,
            timeline: Timeline::default(),
            git_branch,
            branches: Vec::new(),
            draft: true,
            runtime: Runtime::Idle,
            live_model: None,
            pending_sends: Vec::new(),
            turn_in_flight: false,
            diff_open: false,
            diff_expanded: false,
            diff_selected_turn: None,
            terminal_open: false,
            terminal_height: 240.,
            terminal: None,
            _pump: None,
        }
    }

    /// The provider + model a new draft should start with: the most recently
    /// used session's, else the Claude default.
    fn draft_defaults(&self) -> (ProviderKind, Option<String>) {
        match self.sessions.first() {
            Some(meta) => (meta.provider, meta.model.clone()),
            None => (ProviderKind::ClaudeCode, None),
        }
    }

    /// Switch the main area into a draft for `project_id` (rooted at `cwd`): an
    /// empty timeline with a focused, functional composer. The session is
    /// created lazily on the first send (see `send_turn`/`commit_draft`).
    pub fn start_draft(&mut self, project_id: String, cwd: PathBuf, cx: &mut Context<Self>) {
        self.shutdown_active();
        let (provider, model) = self.draft_defaults();
        self.active = Some(Self::build_draft_session(project_id, cwd, provider, model));
        cx.notify();
    }

    /// Whether the active thread is an unsent draft.
    pub fn active_is_draft(&self) -> bool {
        self.active.as_ref().is_some_and(|a| a.draft)
    }

    /// Persist the active draft as a real session (no cx; caller notifies).
    /// The session id is preserved, so its already-recorded events line up.
    fn commit_draft(&mut self) -> std::io::Result<()> {
        if let Some(active) = self.active.as_mut() {
            if active.draft {
                active.draft = false;
                let meta = active.meta.clone();
                self.store.upsert_meta(&meta)?;
                self.sessions = self.store.load_index();
            }
        }
        Ok(())
    }

    /// Make a stored session active: replay its JSONL log only. The provider
    /// process starts lazily on the next send (with the stored resume cursor).
    pub fn select_session(&mut self, session_id: &str, cx: &mut Context<Self>) {
        if self.active_session_id() == Some(session_id) {
            return;
        }
        let Some(meta) = self.sessions.iter().find(|m| m.id == session_id).cloned() else {
            return;
        };
        self.shutdown_active();
        let events = self.store.read_events(&meta.id);
        let mut timeline = Timeline::fold_events(events);
        // The provider process is gone; stale approvals / running turns can't
        // continue, so drop them.
        timeline.mark_idle();
        log::info!(
            "opened session {} ({} timeline entries, resume cursor: {})",
            meta.id,
            timeline.entries.len(),
            meta.resume_cursor.is_some()
        );
        let git_branch = read_git_branch(&meta.cwd);
        self.active = Some(ActiveSession {
            meta,
            timeline,
            git_branch,
            branches: Vec::new(),
            draft: false,
            runtime: Runtime::Idle,
            live_model: None,
            pending_sends: Vec::new(),
            turn_in_flight: false,
            diff_open: false,
            diff_expanded: false,
            diff_selected_turn: None,
            terminal_open: false,
            terminal_height: 240.,
            terminal: None,
            _pump: None,
        });
        cx.notify();
    }

    /// Open the most recently updated stored session (replay only). Used by the
    /// hidden `--open-latest` launch flag. No-op when there are no sessions.
    pub fn open_latest_session(&mut self, cx: &mut Context<Self>) {
        // `sessions` is kept sorted newest-first by `load_index`.
        if let Some(id) = self.sessions.first().map(|m| m.id.clone()) {
            self.select_session(&id, cx);
        }
    }

    /// Submit a user turn. Starts the provider lazily if needed.
    pub fn send_turn(&mut self, text: String, cx: &mut Context<Self>) {
        // The first send on a draft materializes it into a real (persisted)
        // session so the sidebar row appears; the provider then starts below.
        if self.active_is_draft() {
            if let Err(err) = self.commit_draft() {
                self.report_error(format!("failed to persist session: {err}"), cx);
                return;
            }
        }

        let Some(active) = self.active.as_mut() else {
            return;
        };
        let session_id = active.meta.id.clone();

        // Record the user message as a synthetic canonical event so replay
        // renders it identically (providers don't echo user input).
        let user_event = AgentEvent::ItemCompleted(ThreadItem {
            id: format!("local-user-{}", uuid::Uuid::new_v4()),
            content: ItemContent::UserMessage { text: text.clone() },
        });
        self.record_event(&session_id, &user_event, cx);

        self.maybe_adopt_title(cx);

        let Some(active) = self.active.as_mut() else {
            return;
        };
        active.pending_sends.push(text);
        // If the user switched models while the provider is live, restart it
        // first: the queued turn then flushes on the fresh (correct-model)
        // process, resumed from the stored cursor.
        if active.model_changed_while_live() {
            log::info!(
                "model changed to {:?} while live; restarting provider before next turn",
                active.meta.model
            );
            active.shutdown_to_idle();
        }
        let should_start = matches!(active.runtime, Runtime::Idle);
        let dispatch_failed = active.dispatch_next_pending().is_err();
        if should_start {
            self.ensure_started(cx);
        }
        if dispatch_failed {
            self.report_error("session process is gone; try reopening".into(), cx);
        }
        cx.notify();
    }

    pub fn interrupt(&mut self, cx: &mut Context<Self>) {
        if let Some(ActiveSession {
            runtime: Runtime::Live(commands),
            ..
        }) = &self.active
        {
            let _ = commands.try_send(SessionCommand::Interrupt);
        }
        cx.notify();
    }

    pub fn respond_approval(
        &mut self,
        request_id: String,
        decision: ApprovalDecision,
        cx: &mut Context<Self>,
    ) {
        if let Some(ActiveSession {
            runtime: Runtime::Live(commands),
            ..
        }) = &self.active
        {
            let _ = commands.try_send(SessionCommand::RespondApproval {
                request_id,
                decision,
            });
        }
        cx.notify();
    }

    /// Select `model` (None = provider default) for the active session and
    /// persist it. Takes effect on the next provider (re)start; if a provider
    /// is currently live, the next `send_turn` restarts it (see `send_turn`).
    pub fn set_active_model(&mut self, model: Option<String>, cx: &mut Context<Self>) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        // In a draft the model picker also selects the provider (the picker is
        // the provider selection): infer it from the chosen model. The draft is
        // in-memory only, so update it without persisting to the index.
        if active.draft {
            if active.meta.model == model {
                return;
            }
            if let Some(provider) = provider_for_model(model.as_deref()) {
                active.meta.provider = provider;
            }
            active.meta.model = model;
            cx.notify();
            return;
        }
        if active.meta.model == model {
            return;
        }
        active.meta.model = model;
        active.meta.updated_at = now_secs();
        let meta = active.meta.clone();
        self.persist_meta(&meta, cx);
    }

    // -- git branch picker (checkout row) -----------------------------------

    /// Load the local branches for the active session's cwd in the background
    /// (called when the checkout-row popover opens).
    pub fn load_branches(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        let cwd = active.meta.cwd.clone();
        let session_id = active.meta.id.clone();
        cx.spawn(async move |this, cx| {
            let branches = smol::unblock(move || list_git_branches(&cwd)).await;
            let _ = this.update(cx, |state, cx| {
                if let Some(active) = state.active.as_mut() {
                    if active.meta.id == session_id {
                        active.branches = branches;
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// Check out `branch` in the active session's cwd, if the working tree is
    /// clean. Runs git off the main thread; reports success/failure as an
    /// `AppEvent` the chat view turns into a notification.
    pub fn checkout_branch(&mut self, branch: String, cx: &mut Context<Self>) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        if active.timeline.turn_running {
            return;
        }
        let cwd = active.meta.cwd.clone();
        let session_id = active.meta.id.clone();
        let branch_for_task = branch.clone();
        cx.spawn(async move |this, cx| {
            let result = smol::unblock(move || checkout_if_clean(&cwd, &branch_for_task)).await;
            let _ = this.update(cx, |state, cx| {
                match result {
                    Ok(()) => {
                        if let Some(active) = state.active.as_mut() {
                            if active.meta.id == session_id {
                                active.git_branch = read_git_branch(&active.meta.cwd);
                            }
                        }
                        cx.emit(AppEvent::Notice(format!("Switched to {branch}")));
                    }
                    Err(CheckoutError::Dirty) => {
                        cx.emit(AppEvent::Error(
                            "Working tree has uncommitted changes".into(),
                        ));
                    }
                    Err(CheckoutError::Git(message)) => cx.emit(AppEvent::Error(message)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Whether the live provider is running a different model than the one now
    /// selected — used by the model picker to show the "restart" footer note.
    pub fn model_pending_restart(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(ActiveSession::model_changed_while_live)
    }

    /// Toggle a model id in the persisted favorites list.
    pub fn toggle_favorite_model(&mut self, model: &str, cx: &mut Context<Self>) {
        let mut settings = self.settings.clone();
        if let Some(pos) = settings.favorite_models.iter().position(|m| m == model) {
            settings.favorite_models.remove(pos);
        } else {
            settings.favorite_models.push(model.to_string());
        }
        self.update_settings(settings, cx);
    }

    pub fn is_favorite_model(&self, model: &str) -> bool {
        self.settings.favorite_models.iter().any(|m| m == model)
    }

    /// Spawn the provider process for the active session if it isn't running.
    fn ensure_started(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        if !matches!(active.runtime, Runtime::Idle) {
            return;
        }
        self.next_start_generation = self
            .next_start_generation
            .checked_add(1)
            .expect("provider start generation overflow");
        let generation = self.next_start_generation;
        let active = self.active.as_mut().unwrap();
        active.runtime = Runtime::Starting { generation };
        // Remember the model this process is being launched with so a later
        // model switch can detect the mismatch and restart.
        active.live_model = active.meta.model.clone();

        let meta = active.meta.clone();
        let settings = self.settings.clone();
        let session_id = meta.id.clone();
        if let Some(cursor) = &meta.resume_cursor {
            log::info!(
                "starting provider {:?} with resume cursor: {}",
                meta.provider,
                cursor.0
            );
        } else {
            log::info!("starting provider {:?} (fresh session)", meta.provider);
        }

        cx.spawn(async move |this, cx| {
            let opts = session_options(&meta, &settings);
            let result = start_session(meta.provider, opts).await;
            let _ = this.update(cx, |state, cx| {
                let matches_attempt = state.active.as_ref().is_some_and(|active| {
                    active.meta.id == session_id && active.is_starting_generation(generation)
                });
                match result {
                    Ok(handle) => {
                        if !matches_attempt {
                            // User switched away or a newer start superseded this one.
                            let _ = handle.commands.try_send(SessionCommand::Shutdown);
                            return;
                        }
                        let commands = handle.commands.clone();
                        let events = handle.events.clone();
                        let pump_session = session_id.clone();
                        let pump = cx.spawn(async move |this, cx| {
                            while let Ok(event) = events.recv().await {
                                if this
                                    .update(cx, |state, cx| {
                                        state.on_event(&pump_session, event, cx)
                                    })
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        });
                        let active = state.active.as_mut().unwrap();
                        active.runtime = Runtime::Live(commands.clone());
                        active._pump = Some(pump);
                        if active.dispatch_next_pending().is_err() {
                            state.report_error(
                                "session process exited before the queued turn was sent".into(),
                                cx,
                            );
                        }
                        cx.notify();
                    }
                    Err(err) => {
                        if matches_attempt {
                            if let Some(active) = state.active.as_mut() {
                                active.runtime = Runtime::Idle;
                                active.pending_sends.clear();
                                active.turn_in_flight = false;
                            }
                            let message = format!("failed to start provider: {err}");
                            let error_event = AgentEvent::Error {
                                message: message.clone(),
                                fatal: true,
                            };
                            state.record_event(&session_id, &error_event, cx);
                            state.report_error(message, cx);
                            cx.notify();
                        }
                    }
                }
            });
        })
        .detach();
    }

    /// Handle one canonical event from the live provider.
    fn on_event(&mut self, session_id: &str, event: AgentEvent, cx: &mut Context<Self>) {
        if self.smoke.is_some() {
            log::info!(
                "event: {}",
                serde_json::to_string(&event).unwrap_or_else(|_| "<unserializable>".into())
            );
        } else {
            log::debug!(
                "event: {}",
                serde_json::to_string(&event).unwrap_or_else(|_| "<unserializable>".into())
            );
        }

        if let AgentEvent::SessionClosed { reason } = &event {
            let is_active = self.active_session_id() == Some(session_id);
            if !is_active {
                // User-requested shutdowns remove the active runtime before the
                // provider acknowledges them, so their close event stays silent.
                return;
            }

            self.record_event(session_id, &event, cx);
            if let Some(active) = self.active.as_mut() {
                active.runtime = Runtime::Idle;
                active.turn_in_flight = false;
                active._pump = None;
            }
            let message = match reason {
                Some(reason) => format!("provider session closed unexpectedly: {reason}"),
                None => "provider session closed unexpectedly".to_string(),
            };
            self.report_error(message, cx);
            cx.notify();
            return;
        }

        // Session bookkeeping side effects.
        match &event {
            AgentEvent::SessionStarted { resume, model, .. } => {
                if let Some(meta) = self.meta_mut(session_id) {
                    meta.resume_cursor = Some(resume.clone());
                    if meta.model.is_none() {
                        meta.model = model.clone();
                    }
                    meta.updated_at = now_secs();
                    let meta = meta.clone();
                    self.persist_meta(&meta, cx);
                }
            }
            AgentEvent::TurnCompleted { .. } => {
                if let Some(meta) = self.meta_mut(session_id) {
                    meta.updated_at = now_secs();
                    let meta = meta.clone();
                    self.persist_meta(&meta, cx);
                }
                // The turn may have switched branches (checkout) or made the
                // first commit; refresh the display-only branch label.
                if let Some(active) = self.active.as_mut() {
                    if active.meta.id == session_id {
                        active.git_branch = read_git_branch(&active.meta.cwd);
                    }
                }
            }
            AgentEvent::Error { message, .. } => {
                cx.emit(AppEvent::Error(message.clone()));
            }
            _ => {}
        }

        self.record_event(session_id, &event, cx);

        if matches!(event, AgentEvent::TurnCompleted { .. }) {
            let dispatch_failed = self
                .active
                .as_mut()
                .filter(|active| active.meta.id == session_id)
                .is_some_and(|active| {
                    active.turn_in_flight = false;
                    active.dispatch_next_pending().is_err()
                });
            if dispatch_failed {
                self.report_error("session process is gone; try reopening".into(), cx);
            }
        }

        // Smoke-mode automation.
        if let Some(smoke) = self.smoke {
            match &event {
                AgentEvent::ApprovalRequested(request) if smoke.auto_approve => {
                    log::info!("smoke: auto-approving request {}", request.id);
                    self.respond_approval(request.id.clone(), ApprovalDecision::Approve, cx);
                }
                AgentEvent::TurnCompleted { status, .. } => {
                    let code = match status {
                        TurnStatus::Completed => 0,
                        TurnStatus::Failed | TurnStatus::Interrupted => 1,
                    };
                    log::info!("smoke: turn completed with status {status:?}; exiting {code}");
                    std::process::exit(code);
                }
                AgentEvent::Error {
                    fatal: true,
                    message,
                } => {
                    log::error!("smoke: fatal provider error: {message}");
                    std::process::exit(1);
                }
                _ => {}
            }
        }

        cx.notify();
    }

    /// Append to JSONL + fold into the active timeline (if it's this session).
    /// The same wall-clock timestamp is persisted and folded so the on-disk
    /// log and the live timeline agree.
    fn record_event(&mut self, session_id: &str, event: &AgentEvent, cx: &mut Context<Self>) {
        let ts = now_millis();
        if let Err(err) = self.store.append_event(session_id, ts, event) {
            self.report_error(format!("failed to persist event: {err}"), cx);
        }
        if let Some(active) = self.active.as_mut() {
            if active.meta.id == session_id {
                active.timeline.apply_at(Some(ts), event);
            }
        }
    }

    /// Set the session title from the first user message, once.
    fn maybe_adopt_title(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if !active.meta.title.starts_with("New ") {
            return;
        }
        let Some(first) = active.timeline.first_user_message() else {
            return;
        };
        let title = truncate_title(first);
        if title.is_empty() {
            return;
        }
        active.meta.title = title;
        active.meta.updated_at = now_secs();
        let meta = active.meta.clone();
        self.persist_meta(&meta, cx);
    }

    fn meta_mut(&mut self, session_id: &str) -> Option<&mut SessionMeta> {
        self.active
            .as_mut()
            .map(|a| &mut a.meta)
            .filter(|m| m.id == session_id)
    }

    fn persist_meta(&mut self, meta: &SessionMeta, cx: &mut Context<Self>) {
        if let Err(err) = self.store.upsert_meta(meta) {
            self.report_error(format!("failed to persist session index: {err}"), cx);
        }
        self.sessions = self.store.load_index();
        cx.notify();
    }

    pub fn shutdown_active(&mut self) {
        if let Some(active) = self.active.take() {
            if let Runtime::Live(commands) = active.runtime {
                let _ = commands.try_send(SessionCommand::Shutdown);
            }
        }
    }

    fn report_error(&mut self, message: String, cx: &mut Context<Self>) {
        log::error!("{message}");
        cx.emit(AppEvent::Error(message));
    }
}

/// Read the current git branch (or short detached-HEAD sha) for `cwd`, if it is
/// a git repository. Reads `.git/HEAD` directly (no git process); returns None
/// when `cwd` is not a repo. Worktrees/submodules (`.git` is a file) are treated
/// as non-repos here — the below-card branch row simply hides.
fn read_git_branch(cwd: &std::path::Path) -> Option<String> {
    let head = std::fs::read_to_string(cwd.join(".git").join("HEAD")).ok()?;
    let head = head.trim();
    if let Some(reference) = head.strip_prefix("ref: ") {
        // e.g. "refs/heads/feature/x" -> "feature/x"
        let name = reference.strip_prefix("refs/heads/").unwrap_or(reference);
        (!name.is_empty()).then(|| name.to_string())
    } else if !head.is_empty() {
        // Detached HEAD: show the short commit sha.
        Some(head.chars().take(7).collect())
    } else {
        None
    }
}

/// Map a model id to its provider, for the draft model-picker → provider link.
/// `None` (the "Default" row, a Claude entry) and the Claude model ids map to
/// Claude; the `gpt-*` ids to Codex. Unknown custom ids leave it unchanged.
fn provider_for_model(model: Option<&str>) -> Option<ProviderKind> {
    match model {
        None => Some(ProviderKind::ClaudeCode),
        Some("opus" | "sonnet" | "haiku") => Some(ProviderKind::ClaudeCode),
        Some(m) if m.starts_with("gpt") => Some(ProviderKind::Codex),
        Some(_) => None,
    }
}

/// Parse `git for-each-ref` output into a list of branch names (blank lines
/// dropped, whitespace trimmed).
fn parse_branch_list(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// List local git branches for `cwd` (empty when not a repo / git fails).
fn list_git_branches(cwd: &std::path::Path) -> Vec<String> {
    let output = std::process::Command::new("git")
        .args(["for-each-ref", "refs/heads", "--format=%(refname:short)"])
        .current_dir(cwd)
        .output();
    match output {
        Ok(out) if out.status.success() => parse_branch_list(&String::from_utf8_lossy(&out.stdout)),
        _ => Vec::new(),
    }
}

/// Why a `git checkout` was refused.
enum CheckoutError {
    /// The working tree has uncommitted changes.
    Dirty,
    /// git failed (spawn error or non-zero checkout).
    Git(String),
}

/// Check out `branch` in `cwd` iff the working tree is clean.
fn checkout_if_clean(cwd: &std::path::Path, branch: &str) -> Result<(), CheckoutError> {
    let status = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(cwd)
        .output()
        .map_err(|e| CheckoutError::Git(format!("git status failed: {e}")))?;
    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr);
        return Err(CheckoutError::Git(format!(
            "git status failed: {}",
            stderr.trim()
        )));
    }
    if !status.stdout.is_empty() {
        return Err(CheckoutError::Dirty);
    }
    let checkout = std::process::Command::new("git")
        .args(["checkout", branch])
        .current_dir(cwd)
        .output()
        .map_err(|e| CheckoutError::Git(format!("git checkout failed: {e}")))?;
    if !checkout.status.success() {
        let stderr = String::from_utf8_lossy(&checkout.stderr);
        return Err(CheckoutError::Git(format!(
            "git checkout failed: {}",
            stderr.trim()
        )));
    }
    Ok(())
}

fn session_options(meta: &SessionMeta, settings: &Settings) -> SessionOptions {
    let binary_path = match meta.provider {
        ProviderKind::Codex => settings.codex_binary.clone(),
        ProviderKind::ClaudeCode => settings.claude_binary.clone(),
    };
    SessionOptions {
        cwd: meta.cwd.clone(),
        model: meta.model.clone(),
        resume: meta.resume_cursor.clone(),
        binary_path,
        approval_mode: agent::ApprovalMode::default(),
    }
}

fn truncate_title(text: &str) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= TITLE_MAX_CHARS {
        return normalized;
    }
    let mut title: String = normalized.chars().take(TITLE_MAX_CHARS).collect();
    title.push('…');
    title
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_in(project_id: &str, updated_at: u64) -> SessionMeta {
        let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/x"), None);
        meta.project_id = Some(project_id.to_string());
        meta.updated_at = updated_at;
        meta
    }

    #[test]
    fn group_sessions_orders_by_activity() {
        let projects = vec![
            Project {
                id: "p-old".into(),
                name: "Old".into(),
                root: PathBuf::from("/old"),
                created_at: 1,
            },
            Project {
                id: "p-new".into(),
                name: "New".into(),
                root: PathBuf::from("/new"),
                created_at: 2,
            },
            Project {
                id: "p-empty".into(),
                name: "Empty".into(),
                root: PathBuf::from("/empty"),
                created_at: 15,
            },
        ];
        let sessions = vec![
            session_in("p-old", 10),
            session_in("p-new", 100),
            session_in("p-new", 50),
            session_in("p-old", 20),
        ];

        let groups = group_sessions(&projects, &sessions, ProjectSort::RecentActivity);
        // p-new (activity 100), p-old (activity 20), p-empty (created_at 15, no sessions).
        assert_eq!(groups[0].project.id, "p-new");
        assert_eq!(groups[1].project.id, "p-old");
        assert_eq!(groups[2].project.id, "p-empty");
        // Within a group, newest session first.
        assert_eq!(groups[0].sessions[0].updated_at, 100);
        assert_eq!(groups[0].sessions[1].updated_at, 50);
        assert!(groups[2].sessions.is_empty());

        // Name A-Z ordering ignores activity: Empty, New, Old (case-insensitive).
        let by_name = group_sessions(&projects, &sessions, ProjectSort::NameAsc);
        assert_eq!(by_name[0].project.name, "Empty");
        assert_eq!(by_name[1].project.name, "New");
        assert_eq!(by_name[2].project.name, "Old");
    }

    #[test]
    fn draft_send_creates_session_with_project_cwd() {
        let root = std::env::temp_dir().join(format!("tcode-draft-test-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::open_at(root.clone()).unwrap();
        let project = Project::from_root(PathBuf::from("/tmp/tcode-draft-proj"));
        // Persist the project so the draft's project_id survives index migration.
        store.upsert_project(&project).unwrap();
        let mut state = AppState::new(store);
        // A draft is set up (cwd = project root) but not yet persisted.
        let draft = AppState::build_draft_session(
            project.id.clone(),
            project.root.clone(),
            ProviderKind::ClaudeCode,
            None,
        );
        assert!(draft.draft);
        assert_eq!(draft.meta.cwd, project.root);
        assert_eq!(draft.meta.project_id.as_deref(), Some(project.id.as_str()));
        assert!(matches!(draft.runtime, Runtime::Idle));
        let draft_id = draft.meta.id.clone();
        state.active = Some(draft);
        // Not in the index until the first send materializes it.
        assert!(!state.sessions.iter().any(|m| m.id == draft_id));

        // The first send commits the draft: it becomes a real session whose
        // cwd is the project root and shows up in the sidebar index.
        state.commit_draft().unwrap();
        assert!(!state.active.as_ref().unwrap().draft);
        let created = state.sessions.iter().find(|m| m.id == draft_id).unwrap();
        assert_eq!(created.cwd, project.root);
        assert_eq!(created.project_id.as_deref(), Some(project.id.as_str()));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn branch_list_parser_filters_blank_lines() {
        let out = "main\nfeature/x\n\n  \nrelease-1.0\n";
        assert_eq!(
            parse_branch_list(out),
            vec![
                "main".to_string(),
                "feature/x".to_string(),
                "release-1.0".to_string()
            ]
        );
    }

    #[test]
    fn configured_binary_reaches_session_options() {
        let codex = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/project"), None);
        let claude = SessionMeta::new(
            ProviderKind::ClaudeCode,
            PathBuf::from("/tmp/project"),
            None,
        );
        let settings = Settings {
            codex_binary: Some(PathBuf::from("/custom/codex")),
            claude_binary: Some(PathBuf::from("/custom/claude")),
            ..Settings::default()
        };

        let codex_options = session_options(&codex, &settings);
        let claude_options = session_options(&claude, &settings);

        assert_eq!(codex_options.binary_path, settings.codex_binary);
        assert_eq!(claude_options.binary_path, settings.claude_binary);
    }

    #[test]
    fn shutdown_active_notifies_live_provider() {
        let root = std::env::temp_dir().join(format!("tcode-app-test-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::open_at(root.clone()).unwrap();
        let mut state = AppState::new(store);
        let (commands, receiver) = async_channel::unbounded();
        state.active = Some(ActiveSession {
            meta: SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/project"), None),
            timeline: Timeline::default(),
            git_branch: None,
            branches: Vec::new(),
            draft: false,
            runtime: Runtime::Live(commands),
            live_model: None,
            pending_sends: Vec::new(),
            turn_in_flight: false,
            diff_open: false,
            diff_expanded: false,
            diff_selected_turn: None,
            terminal_open: false,
            terminal_height: 240.,
            terminal: None,
            _pump: None,
        });

        state.shutdown_active();

        assert!(matches!(receiver.try_recv(), Ok(SessionCommand::Shutdown)));
        assert!(state.active.is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn queued_sends_dispatch_one_per_completed_turn() {
        let (commands, receiver) = async_channel::unbounded();
        let mut active = ActiveSession {
            meta: SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/project"), None),
            timeline: Timeline::default(),
            git_branch: None,
            branches: Vec::new(),
            draft: false,
            runtime: Runtime::Live(commands),
            live_model: None,
            pending_sends: vec!["first".into(), "second".into()],
            turn_in_flight: false,
            diff_open: false,
            diff_expanded: false,
            diff_selected_turn: None,
            terminal_open: false,
            terminal_height: 240.,
            terminal: None,
            _pump: None,
        };

        assert_eq!(active.dispatch_next_pending(), Ok(true));
        assert!(matches!(
            receiver.try_recv(),
            Ok(SessionCommand::SendTurn { text }) if text == "first"
        ));
        assert_eq!(active.dispatch_next_pending(), Ok(false));
        assert!(receiver.try_recv().is_err());
        assert_eq!(active.pending_sends, ["second"]);

        active.turn_in_flight = false;
        assert_eq!(active.dispatch_next_pending(), Ok(true));
        assert!(matches!(
            receiver.try_recv(),
            Ok(SessionCommand::SendTurn { text }) if text == "second"
        ));
        assert!(active.pending_sends.is_empty());
    }

    #[test]
    fn startup_generation_rejects_stale_same_session_attempt() {
        let meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/project"), None);
        let mut active = ActiveSession {
            meta,
            timeline: Timeline::default(),
            git_branch: None,
            branches: Vec::new(),
            draft: false,
            runtime: Runtime::Starting { generation: 2 },
            live_model: None,
            pending_sends: Vec::new(),
            turn_in_flight: false,
            diff_open: false,
            diff_expanded: false,
            diff_selected_turn: None,
            terminal_open: false,
            terminal_height: 240.,
            terminal: None,
            _pump: None,
        };

        assert!(!active.is_starting_generation(1));
        assert!(active.is_starting_generation(2));
        active.runtime = Runtime::Live(async_channel::unbounded().0);
        assert!(!active.is_starting_generation(2));
    }

    #[test]
    fn model_switch_restarts_live_provider() {
        let (commands, receiver) = async_channel::unbounded();
        let mut meta = SessionMeta::new(ProviderKind::ClaudeCode, PathBuf::from("/tmp/project"), None);
        meta.model = Some("sonnet".into());
        let mut active = ActiveSession {
            meta,
            timeline: Timeline::default(),
            git_branch: None,
            branches: Vec::new(),
            draft: false,
            runtime: Runtime::Live(commands),
            // Process was started on "opus"; the user has since picked "sonnet".
            live_model: Some("opus".into()),
            pending_sends: vec!["do it".into()],
            turn_in_flight: false,
            diff_open: false,
            diff_expanded: false,
            diff_selected_turn: None,
            terminal_open: false,
            terminal_height: 240.,
            terminal: None,
            _pump: None,
        };

        assert!(active.model_changed_while_live());
        active.shutdown_to_idle();

        // Live provider is told to shut down and the runtime is back to Idle,
        // while the queued turn is preserved for the restarted process.
        assert!(matches!(receiver.try_recv(), Ok(SessionCommand::Shutdown)));
        assert!(matches!(active.runtime, Runtime::Idle));
        assert_eq!(active.pending_sends, ["do it"]);
        assert!(!active.model_changed_while_live());

        // No restart when the selected model matches the live one.
        active.runtime = Runtime::Live(async_channel::unbounded().0);
        active.live_model = active.meta.model.clone();
        assert!(!active.model_changed_while_live());
    }

    #[test]
    fn read_git_branch_reads_head() {
        let root = std::env::temp_dir().join(format!("tcode-branch-test-{}", uuid::Uuid::new_v4()));
        let git = root.join(".git");
        std::fs::create_dir_all(&git).unwrap();

        // A .git dir with no HEAD file yet is treated as no branch.
        assert_eq!(read_git_branch(&root), None);

        // Symbolic ref -> short branch name.
        std::fs::write(git.join("HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        assert_eq!(read_git_branch(&root), Some("feature/x".into()));

        // Detached HEAD -> short sha.
        std::fs::write(git.join("HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(read_git_branch(&root), Some("0123456".into()));

        // Non-repo directory.
        let plain = std::env::temp_dir().join(format!("tcode-plain-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(read_git_branch(&plain), None);

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(plain);
    }
}
