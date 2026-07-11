//! Application state: session registry, active session runtime, event pump.

use std::collections::HashMap;
use std::path::PathBuf;

use agent::{
    AgentEvent, ApprovalDecision, ApprovalMode, Attachment, InteractionMode, ItemContent,
    ModelSpec, OptionSelection, ProviderCommand, ProviderKind, SessionCommand, SessionOptions,
    ThreadItem, TurnOptions, TurnStatus, list_models, start_session,
};
use gpui::{Context, EventEmitter, Task};
use serde::{Deserialize, Serialize};

use crate::session::Timeline;
use crate::settings::{ProjectSort, Settings, SettingsStore};
use crate::store::{Checkpoint, Project, SessionMeta, SessionStore, WorktreeInfo, now_millis, now_secs};

const TITLE_MAX_CHARS: usize = 40;
pub const MAX_TERMINALS_PER_SESSION: usize = 6;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct TerminalPreferences {
    open: bool,
    height: f32,
    count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalSplitDirection {
    Horizontal,
    Vertical,
}

pub struct TerminalEntry {
    pub id: u64,
    pub terminal: term::Terminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSplit {
    pub first: u64,
    pub second: u64,
    pub direction: TerminalSplitDirection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalContext {
    pub id: u64,
    pub terminal_label: String,
    pub line_start: usize,
    pub line_end: usize,
    pub text: String,
}

pub struct TerminalWorkspace {
    pub open: bool,
    pub height: f32,
    pub terminals: Vec<TerminalEntry>,
    pub active_id: Option<u64>,
    pub splits: Vec<TerminalSplit>,
    pub contexts: Vec<TerminalContext>,
    next_id: u64,
    next_context_id: u64,
}

impl Default for TerminalWorkspace {
    fn default() -> Self {
        Self {
            open: false,
            height: 240.,
            terminals: Vec::new(),
            active_id: None,
            splits: Vec::new(),
            contexts: Vec::new(),
            next_id: 1,
            next_context_id: 1,
        }
    }
}

impl TerminalWorkspace {
    pub fn active(&self) -> Option<&TerminalEntry> {
        let id = self.active_id?;
        self.terminals.iter().find(|entry| entry.id == id)
    }

    pub fn terminal(&self, id: u64) -> Option<&TerminalEntry> {
        self.terminals.iter().find(|entry| entry.id == id)
    }

    fn push(&mut self, terminal: term::Terminal) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.terminals.push(TerminalEntry { id, terminal });
        self.active_id = Some(id);
        id
    }

    pub fn split_for(&self, terminal_id: u64) -> Option<TerminalSplit> {
        self.splits
            .iter()
            .copied()
            .find(|split| split.first == terminal_id || split.second == terminal_id)
    }

    pub fn add_context(&mut self, label: String, selection: term::SelectedText) {
        let id = self.next_context_id;
        self.next_context_id += 1;
        self.contexts.push(TerminalContext {
            id,
            terminal_label: label,
            line_start: selection.line_start,
            line_end: selection.line_end,
            text: selection.text,
        });
    }
}

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

/// Which tab the right-side panel shows (it hosts the diff view and the
/// plan/task view). Persisted per active session, in memory only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RightTab {
    #[default]
    Diff,
    Plan,
    Preview,
}

/// A queued turn: its text and any image attachments, awaiting dispatch to the
/// live provider.
#[derive(Debug, Clone, PartialEq)]
struct PendingSend {
    text: String,
    attachments: Vec<Attachment>,
}

impl From<&str> for PendingSend {
    fn from(text: &str) -> Self {
        PendingSend {
            text: text.to_string(),
            attachments: Vec::new(),
        }
    }
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
    /// The approval mode the live provider process is actually running under.
    /// Claude switches live (this is updated in lockstep, so no restart);
    /// Codex binds the mode at thread start, so a mid-session change leaves this
    /// stale and forces a resume-restart before the next turn.
    live_approval_mode: Option<ApprovalMode>,
    /// The option selections the live provider was started with (reasoning
    /// effort, context window, fast mode, …). A mid-session change to a
    /// launch-time option forces a resume-restart before the next turn; Codex's
    /// reasoning effort is the exception (it applies per turn, see `send_turn`).
    live_option_selections: Vec<OptionSelection>,
    /// A transient "the next send should be an Ultrathink turn" flag, set when
    /// the user picks Ultrathink in the traits picker. It is never persisted
    /// (T3: Ultrathink is a prompt-prefix mode, not an option) and is cleared
    /// after one send.
    pending_ultrathink: bool,
    /// Whether the current proposed plan has been accepted for implementation
    /// (drives the composer back out of its plan-ready state).
    plan_implemented: bool,
    /// Draft-only (Group C): run in the current checkout or a new dedicated
    /// worktree. Chosen in the checkout row before the first send; locked after.
    pub draft_workspace: WorkspaceMode,
    /// Group C: set while the first send is creating a worktree in the
    /// background (drives the composer's "Preparing worktree…" action).
    preparing_worktree: bool,
    pending_sends: Vec<PendingSend>,
    turn_in_flight: bool,
    /// Provider-native commands / skills discovered at session start (Claude
    /// `slash_commands` + `skills`; Codex `skills/list`). Feeds the composer's
    /// `/` and `$` menus. In-memory only.
    provider_commands: Vec<ProviderCommand>,
    /// Diff panel UI state (per-session, in-memory only). `diff_open` is the
    /// right-panel open/closed flag; `right_tab` selects which tab it shows.
    pub diff_open: bool,
    pub diff_expanded: bool,
    pub diff_selected_turn: Option<usize>,
    /// Which tab the right panel shows (Diff or Plan/Tasks).
    pub right_tab: RightTab,
    /// Set when the user manually closed the right panel during the current
    /// turn, so `auto_open_task_panel` doesn't re-open it until the next turn.
    auto_open_suppressed: bool,
    /// Bottom terminal drawer state and its lazily-spawned per-session PTYs.
    pub terminal_workspace: TerminalWorkspace,
    _pump: Option<Task<()>>,
}

impl ActiveSession {
    /// Whether the live provider is running a different model than the one now
    /// selected in `meta.model` (so the next turn must restart the provider).
    fn model_changed_while_live(&self) -> bool {
        matches!(self.runtime, Runtime::Live(_)) && self.meta.model != self.live_model
    }

    /// Whether the live provider is running a different approval mode than the
    /// one now selected in `meta.approval_mode`. Only providers that cannot
    /// switch live (Codex) reach this state: Claude updates `live_approval_mode`
    /// in lockstep when it applies the switch on the wire.
    fn approval_mode_changed_while_live(&self) -> bool {
        matches!(self.runtime, Runtime::Live(_))
            && Some(self.meta.approval_mode) != self.live_approval_mode
    }

    /// Whether a launch-time option (reasoning effort for Claude, context
    /// window, fast mode, thinking, …) changed while the provider is live, so
    /// the next turn must restart it. Codex's reasoning effort is excluded: it
    /// is applied per turn via [`TurnOptions`] and needs no restart.
    fn options_changed_while_live(&self) -> bool {
        if !matches!(self.runtime, Runtime::Live(_)) {
            return false;
        }
        let ignore_effort = self.meta.provider == ProviderKind::Codex;
        normalized_selections(&self.meta.option_selections, ignore_effort)
            != normalized_selections(&self.live_option_selections, ignore_effort)
    }

    /// Per-turn overrides derived from the session's persisted state: Codex
    /// reasoning effort (applied per turn) and the Build/Plan interaction mode.
    fn turn_options(&self) -> TurnOptions {
        let effort = if self.meta.provider == ProviderKind::Codex {
            codex_effort_selection(&self.meta.option_selections)
        } else {
            None
        };
        TurnOptions {
            effort,
            interaction_mode: Some(self.meta.interaction_mode),
        }
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

    /// Whether this session steers (sends mid-turn) instead of queueing. Group B:
    /// the Claude CLI accepts mid-turn user messages as steering, so we
    /// deliberately exceed T3 here and dispatch immediately even while a turn is
    /// in flight. Codex keeps the strict one-turn-at-a-time queue.
    fn supports_steering(&self) -> bool {
        matches!(self.runtime, Runtime::Live(_))
            && self.meta.provider == ProviderKind::ClaudeCode
    }

    /// Dispatch at most one queued send, preserving FIFO order. A turn already in
    /// flight blocks dispatch for queueing providers (Codex); steering providers
    /// (Claude) dispatch immediately (see [`Self::supports_steering`]).
    fn dispatch_next_pending(&mut self) -> Result<bool, ()> {
        if self.turn_in_flight && !self.supports_steering() {
            return Ok(false);
        }
        let Runtime::Live(commands) = &self.runtime else {
            return Ok(false);
        };
        let Some(send) = self.pending_sends.first().cloned() else {
            return Ok(false);
        };
        let options = Some(self.turn_options());
        commands
            .try_send(SessionCommand::SendTurn {
                text: send.text,
                options,
                attachments: send.attachments,
            })
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

/// The result of a provider version check (Group C / s3 §6).
#[derive(Debug, Clone, Default)]
pub struct ProviderVersionStatus {
    /// Installed version (raw string, e.g. `"2.1.206"`); `None` if `--version` failed.
    pub installed: Option<String>,
    /// Latest published version from npm; `None` if the lookup failed.
    pub latest: Option<String>,
    /// Whether `latest` is strictly newer than `installed`.
    pub update_available: bool,
    /// Whether a version check is currently running.
    pub checking: bool,
    /// Whether a self-update command is currently running.
    pub updating: bool,
    /// How the binary was installed (drives the update command).
    pub install_source: crate::version_check::InstallSource,
}

/// The workspace a draft thread will run in (Group C): the project checkout, or
/// a new dedicated git worktree branched from `base`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum WorkspaceMode {
    #[default]
    LocalCheckout,
    NewWorktree {
        base: String,
    },
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
    /// Per-provider model catalog (from `agent::list_models`): loaded instantly
    /// from the persisted cache, then refreshed in the background at start and
    /// whenever a binary path changes. Absent entry = never fetched.
    pub model_catalogs: HashMap<ProviderKind, Vec<ModelSpec>>,
    /// Providers whose catalog is currently being fetched (drives the picker's
    /// "Loading models…" row when the cache is also empty).
    pub models_loading: HashMap<ProviderKind, bool>,
    terminal_preferences_path: PathBuf,
    terminal_preferences: HashMap<String, TerminalPreferences>,
    next_start_generation: u64,
    /// Screenshot-only: seed the composer text on first render (drives `@`/`/`/`$`
    /// trigger menus headlessly, as `--open-diff` does for the diff panel).
    pub debug_compose: Option<String>,
    /// Screenshot-only: inject a pending image attachment on first render (paste
    /// / drag-drop cannot be driven headlessly).
    pub debug_image: Option<PathBuf>,
    /// Screenshot-only: seed the command palette's query when it opens (so the
    /// `>`-actions filter and thread result rows can be captured headlessly).
    pub debug_palette: Option<String>,
    /// Screenshot-only: which Settings section to open (`general` / `providers` /
    /// `archived`), so each can be captured headlessly.
    pub debug_settings_section: Option<String>,
    /// Preview MCP server registration, injected into every session so the agent
    /// can drive the embedded browser. `None` if the server failed to start.
    pub mcp_registration: Option<agent::McpRegistration>,
    /// Automation-request receiver from the preview MCP server. `AppShell` takes
    /// this once to pump requests into the live `PreviewPanel` WebView.
    pub preview_requests: Option<async_channel::Receiver<preview_mcp::BrokerRequest>>,
    /// A URL the preview panel should navigate to on its next render (set by the
    /// `--open-preview <url>` dev flag for headless screenshots). Consumed once.
    pub pending_preview_url: Option<String>,
    /// Per-provider version-check results (Group C). Populated on launch (when
    /// the toggle is on) and by Settings → "Check now".
    pub provider_versions: HashMap<ProviderKind, ProviderVersionStatus>,
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
        let terminal_preferences_path = store.root().join("terminal-ui.json");
        let terminal_preferences = std::fs::read(&terminal_preferences_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        // Seed the model picker from the persisted cache so it is instant and
        // works offline; a background refresh (see `refresh_model_catalogs`)
        // updates it once the providers respond.
        let mut model_catalogs = HashMap::new();
        for provider in [ProviderKind::ClaudeCode, ProviderKind::Codex] {
            let cached = store.load_models(provider);
            if !cached.is_empty() {
                model_catalogs.insert(provider, cached);
            }
        }
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
            model_catalogs,
            models_loading: HashMap::new(),
            terminal_preferences_path,
            terminal_preferences,
            next_start_generation: 0,
            debug_compose: None,
            debug_image: None,
            debug_palette: None,
            debug_settings_section: None,
            mcp_registration: None,
            preview_requests: None,
            pending_preview_url: None,
            provider_versions: HashMap::new(),
        }
    }

    /// Open the Preview tab and queue an initial navigation (dev/testing entry
    /// point for `--open-preview <url>`).
    pub fn open_preview_with_url(&mut self, url: String, cx: &mut Context<Self>) {
        self.pending_preview_url = Some(url);
        self.open_preview_panel(cx);
    }

    /// Take the queued preview URL, if any (consumed by `PreviewPanel`).
    pub fn take_pending_preview_url(&mut self) -> Option<String> {
        self.pending_preview_url.take()
    }

    /// Attach the running preview MCP server: its registration (injected into
    /// every spawned session) and the request receiver (taken by `AppShell`).
    pub fn attach_preview_mcp(&mut self, server: preview_mcp::PreviewMcpServer) {
        self.mcp_registration = Some(agent::McpRegistration {
            url: server.url,
            bearer_token: server.bearer_token,
        });
        self.preview_requests = Some(server.requests);
    }

    /// Kick off a background refresh of every provider's model catalog (called
    /// at app start and after a binary-path change). Results update
    /// `model_catalogs` and are persisted so the next launch is instant.
    pub fn refresh_model_catalogs(&mut self, cx: &mut Context<Self>) {
        for provider in [ProviderKind::ClaudeCode, ProviderKind::Codex] {
            let binary = match provider {
                ProviderKind::Codex => self.settings.codex_binary.clone(),
                ProviderKind::ClaudeCode => self.settings.claude_binary.clone(),
            };
            self.models_loading.insert(provider, true);
            let store = self.store.clone();
            cx.spawn(async move |this, cx| {
                let result = list_models(provider, binary).await;
                let _ = this.update(cx, |state, cx| {
                    state.models_loading.insert(provider, false);
                    match result {
                        Ok(models) if !models.is_empty() => {
                            if let Err(err) = store.save_models(provider, &models) {
                                log::warn!("failed to persist {provider:?} model catalog: {err}");
                            }
                            state.model_catalogs.insert(provider, models);
                        }
                        Ok(_) => log::info!("{provider:?} returned an empty model catalog"),
                        Err(err) => log::warn!("failed to list {provider:?} models: {err}"),
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        cx.notify();
    }

    /// Screenshot/dev only (`--debug-live`): start the active (non-draft)
    /// session's provider process without sending a turn, so provider-supplied
    /// state (the `/` + `$` command feed) is reachable headlessly.
    pub fn debug_start_provider(&mut self, cx: &mut Context<Self>) {
        if self.active.as_ref().is_some_and(|a| !a.draft) {
            self.ensure_started(cx);
        }
    }

    // -- provider version checks (Group C / s3 §6) --------------------------

    /// Whether the on-launch provider version check is enabled (default on).
    pub fn provider_update_checks_enabled(&self) -> bool {
        !self.settings.provider_update_checks_disabled
    }

    /// The last known version-check result for `provider`.
    pub fn provider_version(&self, provider: ProviderKind) -> Option<&ProviderVersionStatus> {
        self.provider_versions.get(&provider)
    }

    /// Resolve the binary path for a provider: the settings override, else a
    /// PATH lookup of the bare command name.
    fn resolve_provider_binary(&self, provider: ProviderKind) -> Option<PathBuf> {
        let (over, name) = match provider {
            ProviderKind::Codex => (self.settings.codex_binary.clone(), "codex"),
            ProviderKind::ClaudeCode => (self.settings.claude_binary.clone(), "claude"),
        };
        over.or_else(|| which_in_path(name))
    }

    /// Check every provider's installed vs. latest version in the background,
    /// storing results in `provider_versions` and toasting once per provider
    /// that has an update available.
    pub fn check_provider_versions(&mut self, cx: &mut Context<Self>) {
        for provider in [ProviderKind::ClaudeCode, ProviderKind::Codex] {
            let binary = self.resolve_provider_binary(provider);
            let status = self.provider_versions.entry(provider).or_default();
            if status.checking {
                continue;
            }
            status.checking = true;
            status.install_source = binary
                .as_deref()
                .map(crate::version_check::detect_install_source)
                .unwrap_or_default();
            let source = status.install_source;
            let program = binary
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| default_program(provider));
            let package = crate::version_check::npm_package(provider);
            cx.spawn(async move |this, cx| {
                let installed = run_capture(&program, &["--version"]).await;
                let latest = run_capture("npm", &["view", package, "version"]).await;
                let _ = this.update(cx, |state, cx| {
                    let update_available = match (&installed, &latest) {
                        (Some(i), Some(l)) => crate::version_check::is_update_available(i, l),
                        _ => false,
                    };
                    let already = state
                        .provider_versions
                        .get(&provider)
                        .map(|s| s.update_available)
                        .unwrap_or(false);
                    // Normalize both to `a.b.c` for display (raw `--version`
                    // output can carry a program name / suffix).
                    let pretty = |raw: &Option<String>| {
                        raw.as_ref().map(|r| {
                            crate::version_check::parse_version(r)
                                .map(|(a, b, c)| format!("{a}.{b}.{c}"))
                                .unwrap_or_else(|| r.clone())
                        })
                    };
                    let installed_pretty = pretty(&installed);
                    let latest_pretty = pretty(&latest);
                    let status = state.provider_versions.entry(provider).or_default();
                    status.checking = false;
                    status.install_source = source;
                    status.installed = installed_pretty;
                    status.latest = latest_pretty.clone();
                    status.update_available = update_available;
                    // Toast once when an update becomes newly available.
                    if update_available && !already {
                        if let Some(version) = &latest_pretty {
                            cx.emit(AppEvent::Notice(
                                rust_i18n::t!(
                                    "notice.update_available",
                                    provider = provider.display_name(),
                                    version = version
                                )
                                .into_owned(),
                            ));
                        }
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        cx.notify();
    }

    /// Run the provider's self-update command (per its detected install source),
    /// showing an "updating" toast, then re-check its version.
    pub fn update_provider(&mut self, provider: ProviderKind, cx: &mut Context<Self>) {
        let source = self
            .provider_versions
            .get(&provider)
            .map(|s| s.install_source)
            .unwrap_or_default();
        let Some(command) = crate::version_check::update_command(provider, source) else {
            self.report_error(
                rust_i18n::t!("errors.update_unknown", provider = provider.display_name())
                    .into_owned(),
                cx,
            );
            return;
        };
        let status = self.provider_versions.entry(provider).or_default();
        if status.updating {
            return;
        }
        status.updating = true;
        cx.emit(AppEvent::Notice(
            rust_i18n::t!("notice.updating_provider", provider = provider.display_name())
                .into_owned(),
        ));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let args: Vec<&str> = command[1..].iter().map(String::as_str).collect();
            let ok = run_status(&command[0], &args).await;
            let _ = this.update(cx, |state, cx| {
                if let Some(status) = state.provider_versions.get_mut(&provider) {
                    status.updating = false;
                }
                if ok {
                    cx.emit(AppEvent::Notice(
                        rust_i18n::t!("notice.update_done", provider = provider.display_name())
                            .into_owned(),
                    ));
                    // Refresh the version so the "update available" state clears.
                    state.check_provider_versions(cx);
                } else {
                    state.report_error(
                        rust_i18n::t!(
                            "errors.update_failed",
                            provider = provider.display_name()
                        )
                        .into_owned(),
                        cx,
                    );
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Provider-native commands / skills for the active session (empty until the
    /// session's provider reports them). Feeds the composer's `/` and `$` menus.
    pub fn active_provider_commands(&self) -> &[ProviderCommand] {
        self.active
            .as_ref()
            .map(|a| a.provider_commands.as_slice())
            .unwrap_or(&[])
    }

    /// The cached model catalog for `provider` (empty when never fetched).
    pub fn models_for(&self, provider: ProviderKind) -> &[ModelSpec] {
        self.model_catalogs
            .get(&provider)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Whether `provider`'s catalog is being fetched and no cache exists yet
    /// (so the picker should show the "Loading models…" placeholder).
    pub fn models_loading(&self, provider: ProviderKind) -> bool {
        self.models_loading.get(&provider).copied().unwrap_or(false)
            && self.models_for(provider).is_empty()
    }

    /// The [`ModelSpec`] for the active session's selected model, if the catalog
    /// contains it (drives the traits picker's descriptors).
    pub fn active_model_spec(&self) -> Option<ModelSpec> {
        let active = self.active.as_ref()?;
        let model = active.meta.model.as_deref()?;
        self.models_for(active.meta.provider)
            .iter()
            .find(|m| m.id == model)
            .cloned()
    }

    /// The active session's persisted option selections (empty for none).
    pub fn active_option_selections(&self) -> Vec<OptionSelection> {
        self.active
            .as_ref()
            .map(|a| a.meta.option_selections.clone())
            .unwrap_or_default()
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
            // The header diff button focuses the Diff tab: opening (or switching
            // tabs while already open) shows diffs; a second click closes.
            if active.diff_open && active.right_tab == RightTab::Diff {
                active.diff_open = false;
                if active.timeline.turn_running {
                    active.auto_open_suppressed = true;
                }
            } else {
                active.diff_open = true;
                active.right_tab = RightTab::Diff;
            }
            cx.notify();
        }
    }

    /// Open the diff panel and select `turn` (a "View diff" card button).
    pub fn open_diff_for_turn(&mut self, turn: usize, cx: &mut Context<Self>) {
        if let Some(active) = self.active.as_mut() {
            active.diff_open = true;
            active.right_tab = RightTab::Diff;
            active.diff_selected_turn = Some(turn);
            cx.notify();
        }
    }

    /// Open the diff panel on the latest turn with changes (used by
    /// `--open-diff` and as a general "just show me the diffs" entry point).
    pub fn open_diff_panel(&mut self, cx: &mut Context<Self>) {
        if let Some(active) = self.active.as_mut() {
            active.diff_open = true;
            active.right_tab = RightTab::Diff;
            cx.notify();
        }
    }

    // -- terminal drawer (per-session, in-memory) --------------------------

    fn persist_terminal_preferences(&mut self) {
        if let Some(active) = self.active.as_ref() {
            let workspace = &active.terminal_workspace;
            self.terminal_preferences.insert(
                active.meta.id.clone(),
                TerminalPreferences {
                    open: workspace.open,
                    height: workspace.height,
                    count: workspace.terminals.len(),
                },
            );
        }
        match serde_json::to_vec_pretty(&self.terminal_preferences) {
            Ok(bytes) => {
                if let Err(error) = std::fs::write(&self.terminal_preferences_path, bytes) {
                    log::warn!("failed to persist terminal UI state: {error}");
                }
            }
            Err(error) => log::warn!("failed to encode terminal UI state: {error}"),
        }
    }

    pub fn set_terminal_height(&mut self, height: f32) {
        if let Some(active) = self.active.as_mut() {
            active.terminal_workspace.height = height;
            self.persist_terminal_preferences();
        }
    }

    pub fn terminal_panel_open(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.terminal_workspace.open)
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
        if active.terminal_workspace.terminals.is_empty() {
            match term::Terminal::spawn(&active.meta.cwd) {
                Ok(terminal) => {
                    active.terminal_workspace.push(terminal);
                }
                Err(error) => {
                    self.report_error(
                        rust_i18n::t!("errors.terminal_start", error = error).into_owned(),
                        cx,
                    );
                    return;
                }
            }
        }
        active.terminal_workspace.open = true;
        self.persist_terminal_preferences();
        cx.notify();
    }

    pub fn close_terminal_panel(&mut self, cx: &mut Context<Self>) {
        if let Some(active) = self.active.as_mut() {
            active.terminal_workspace.open = false;
            self.persist_terminal_preferences();
            cx.notify();
        }
    }

    pub fn restart_terminal(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        let active_id = active.terminal_workspace.active_id;
        match term::Terminal::spawn(&active.meta.cwd) {
            Ok(terminal) => {
                if let Some(entry) = active
                    .terminal_workspace
                    .terminals
                    .iter_mut()
                    .find(|entry| Some(entry.id) == active_id)
                {
                    entry.terminal = terminal;
                } else {
                    active.terminal_workspace.push(terminal);
                }
                active.terminal_workspace.open = true;
                self.persist_terminal_preferences();
                cx.notify();
            }
            Err(error) => self.report_error(
                rust_i18n::t!("errors.terminal_restart", error = error).into_owned(),
                cx,
            ),
        }
    }

    pub fn new_terminal(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.active.as_mut() else { return };
        if active.terminal_workspace.terminals.len() >= MAX_TERMINALS_PER_SESSION {
            return;
        }
        match term::Terminal::spawn(&active.meta.cwd) {
            Ok(terminal) => {
                active.terminal_workspace.push(terminal);
                active.terminal_workspace.open = true;
                self.persist_terminal_preferences();
                cx.notify();
            }
            Err(error) => self.report_error(
                rust_i18n::t!("errors.terminal_start", error = error).into_owned(),
                cx,
            ),
        }
    }

    pub fn activate_terminal(&mut self, terminal_id: u64, cx: &mut Context<Self>) {
        let Some(active) = self.active.as_mut() else { return };
        if active.terminal_workspace.terminal(terminal_id).is_some() {
            active.terminal_workspace.active_id = Some(terminal_id);
            cx.notify();
        }
    }

    pub fn close_terminal(&mut self, terminal_id: u64, cx: &mut Context<Self>) {
        let Some(active) = self.active.as_mut() else { return };
        let workspace = &mut active.terminal_workspace;
        workspace.terminals.retain(|entry| entry.id != terminal_id);
        workspace
            .splits
            .retain(|split| split.first != terminal_id && split.second != terminal_id);
        if workspace.active_id == Some(terminal_id) {
            workspace.active_id = workspace.terminals.last().map(|entry| entry.id);
        }
        if workspace.terminals.is_empty() {
            workspace.open = false;
        }
        self.persist_terminal_preferences();
        cx.notify();
    }

    pub fn split_terminal(
        &mut self,
        direction: TerminalSplitDirection,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.active.as_mut() else { return };
        let workspace = &mut active.terminal_workspace;
        let Some(first) = workspace.active_id else { return };
        if workspace.terminals.len() >= MAX_TERMINALS_PER_SESSION
            || workspace.split_for(first).is_some()
        {
            return;
        }
        match term::Terminal::spawn(&active.meta.cwd) {
            Ok(terminal) => {
                let second = workspace.push(terminal);
                workspace.splits.push(TerminalSplit {
                    first,
                    second,
                    direction,
                });
                self.persist_terminal_preferences();
                cx.notify();
            }
            Err(error) => self.report_error(
                rust_i18n::t!("errors.terminal_start", error = error).into_owned(),
                cx,
            ),
        }
    }

    pub fn capture_terminal_selection(&mut self, terminal_id: u64, cx: &mut Context<Self>) {
        let Some(active) = self.active.as_mut() else { return };
        let Some(entry) = active.terminal_workspace.terminal(terminal_id) else { return };
        let label = entry.terminal.label();
        let selection = entry.terminal.selected_text();
        if let Some(selection) = selection {
            active.terminal_workspace.add_context(label, selection);
            cx.notify();
        }
    }

    /// Hidden visual-QA fixture: two live PTYs in a split plus a captured chip.
    pub fn open_terminal_demo(&mut self, cx: &mut Context<Self>) {
        self.open_terminal_panel(cx);
        self.split_terminal(TerminalSplitDirection::Horizontal, cx);
        let Some((first, second)) = self.active.as_ref().and_then(|active| {
            let split = active.terminal_workspace.splits.first()?;
            Some((split.first, split.second))
        }) else {
            return;
        };
        if let Some(active) = self.active.as_ref() {
            if let Some(entry) = active.terminal_workspace.terminal(first) {
                entry
                    .terminal
                    .write_input(b"printf 'terminal one ready\\nselect this output\\n'\r".to_vec());
            }
            if let Some(entry) = active.terminal_workspace.terminal(second) {
                entry
                    .terminal
                    .write_input(b"printf 'terminal two ready\\n'\r".to_vec());
            }
        }
        cx.spawn(async move |this, cx| {
            smol::Timer::after(std::time::Duration::from_millis(700)).await;
            let _ = this.update(cx, |state, cx| {
                let selected = state.active.as_ref().and_then(|active| {
                    let entry = active.terminal_workspace.terminal(first)?;
                    let snapshot = entry.terminal.snapshot();
                    let row = snapshot
                        .text()
                        .lines()
                        .position(|line| line.contains("select this output"))?;
                    entry.terminal.select((row, 0), (row, 17));
                    Some(())
                });
                if selected.is_some() {
                    state.capture_terminal_selection(first, cx);
                }
            });
        })
        .detach();
    }

    pub fn remove_terminal_context(&mut self, context_id: u64, cx: &mut Context<Self>) {
        if let Some(active) = self.active.as_mut() {
            active
                .terminal_workspace
                .contexts
                .retain(|context| context.id != context_id);
            cx.notify();
        }
    }

    pub fn close_diff_panel(&mut self, cx: &mut Context<Self>) {
        if let Some(active) = self.active.as_mut() {
            active.diff_open = false;
            // Closing during a turn suppresses auto-open for the rest of it.
            if active.timeline.turn_running {
                active.auto_open_suppressed = true;
            }
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

    /// Sessions grouped by project for the sidebar (archived threads excluded).
    pub fn grouped_sessions(&self) -> Vec<ProjectGroup> {
        let visible: Vec<SessionMeta> = self
            .sessions
            .iter()
            .filter(|m| m.archived_at.is_none())
            .cloned()
            .collect();
        group_sessions(&self.projects, &visible, self.settings.project_sort)
    }

    /// Archived sessions grouped by project (for Settings → Archived Threads),
    /// newest archive time first within each group. Empty groups are dropped.
    pub fn archived_groups(&self) -> Vec<ProjectGroup> {
        let archived: Vec<SessionMeta> = self
            .sessions
            .iter()
            .filter(|m| m.archived_at.is_some())
            .cloned()
            .collect();
        let mut groups = group_sessions(&self.projects, &archived, self.settings.project_sort);
        for group in &mut groups {
            group
                .sessions
                .sort_by(|a, b| b.archived_at.cmp(&a.archived_at));
        }
        groups.retain(|g| !g.sessions.is_empty());
        groups
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
            self.report_error(
                rust_i18n::t!("errors.persist_project", error = err).into_owned(),
                cx,
            );
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

    /// The session cwd of the active session (for the `@`-mention workspace walk).
    pub fn active_cwd(&self) -> Option<PathBuf> {
        self.active.as_ref().map(|a| a.meta.cwd.clone())
    }

    /// Directory where the active session's image attachments are persisted
    /// (`<store root>/attachments/<session id>/`). `None` with no active session.
    pub fn attachments_dir(&self) -> Option<PathBuf> {
        let id = self.active_session_id()?;
        Some(self.store.root().join("attachments").join(id))
    }

    /// Persist attachment `bytes` under the active session's attachments dir with
    /// the given file extension, returning the saved file path. Files are written
    /// now so a pending image is never lost even though the send wire cannot yet
    /// carry it (see the composer's image seam + reported contract gap).
    pub fn save_attachment(&self, bytes: &[u8], ext: &str) -> std::io::Result<PathBuf> {
        let dir = self.attachments_dir().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "no active session")
        })?;
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.{ext}", uuid::Uuid::new_v4()));
        std::fs::write(&path, bytes)?;
        Ok(path)
    }

    pub fn update_settings(&mut self, settings: Settings, cx: &mut Context<Self>) {
        if let Err(err) = self.settings_store.save(&settings) {
            self.report_error(
                rust_i18n::t!("errors.persist_settings", error = err).into_owned(),
                cx,
            );
            return;
        }
        crate::settings::apply_locale(settings.language.as_deref());
        self.settings = settings;
        cx.notify();
    }

    // -- archive / delete / rename / unread (Group A) -----------------------

    /// Archive a thread (reversible; it vanishes from the sidebar). Blocked while
    /// its turn is running (returns without changing anything so the caller's
    /// tooltip stands). The active thread is closed back to the empty state.
    pub fn archive_session(&mut self, session_id: &str, cx: &mut Context<Self>) {
        if self.turn_running_for(session_id) {
            return;
        }
        if self.active_session_id() == Some(session_id) {
            self.shutdown_active();
        }
        if let Some(meta) = self.sessions.iter_mut().find(|m| m.id == session_id) {
            meta.archived_at = Some(now_secs());
            let meta = meta.clone();
            self.persist_meta(&meta, cx);
        }
    }

    /// Restore an archived thread (Settings → Archived Threads → Unarchive).
    pub fn unarchive_session(&mut self, session_id: &str, cx: &mut Context<Self>) {
        if let Some(meta) = self.sessions.iter_mut().find(|m| m.id == session_id) {
            meta.archived_at = None;
            let meta = meta.clone();
            self.persist_meta(&meta, cx);
        }
    }

    /// Rename a thread (context-menu inline edit). Empty titles are rejected.
    pub fn rename_session(&mut self, session_id: &str, title: &str, cx: &mut Context<Self>) {
        let title = title.trim();
        if title.is_empty() {
            return;
        }
        if let Some(active) = self.active.as_mut().filter(|a| a.meta.id == session_id) {
            active.meta.title = title.to_string();
        }
        if let Some(meta) = self.sessions.iter_mut().find(|m| m.id == session_id) {
            meta.title = title.to_string();
            meta.updated_at = now_secs();
            let meta = meta.clone();
            self.persist_meta(&meta, cx);
        }
    }

    /// The worktree that deleting `session_id` would orphan (i.e. it is the only
    /// remaining session bound to that worktree), if any — drives the "also
    /// remove the worktree?" confirmation.
    pub fn worktree_orphaned_by_delete(&self, session_id: &str) -> Option<crate::store::WorktreeInfo> {
        let meta = self.sessions.iter().find(|m| m.id == session_id)?;
        let worktree = meta.worktree.clone()?;
        let others = self.sessions.iter().any(|m| {
            m.id != session_id && m.worktree.as_ref().is_some_and(|w| w.branch == worktree.branch)
        });
        (!others).then_some(worktree)
    }

    /// Permanently delete a thread: stop the provider, close its terminal, remove
    /// its checkpoint refs, delete meta + JSONL, and (when `remove_worktree`)
    /// remove the git worktree it was the last user of.
    pub fn delete_session(
        &mut self,
        session_id: &str,
        remove_worktree: bool,
        cx: &mut Context<Self>,
    ) {
        let meta = self.sessions.iter().find(|m| m.id == session_id).cloned();
        if self.active_session_id() == Some(session_id) {
            // shutdown_active drops the ActiveSession (and its terminal PTY).
            self.shutdown_active();
        }
        if let Some(meta) = &meta {
            // Best-effort checkpoint ref cleanup in the session cwd.
            if crate::checkpoints::is_git_repo(&meta.cwd) {
                crate::checkpoints::delete_all_checkpoint_refs(&meta.cwd, &meta.id);
            }
            if remove_worktree {
                if let Some(worktree) = &meta.worktree {
                    if let Err(err) = remove_git_worktree(&worktree.root_project_path, &meta.cwd) {
                        self.report_error(err, cx);
                    }
                }
            }
        }
        self.settings.last_visited.remove(session_id);
        if let Err(err) = self.store.remove_session(session_id) {
            self.report_error(
                rust_i18n::t!("errors.delete_session", error = err).into_owned(),
                cx,
            );
            return;
        }
        // Persist the pruned last-visited map (ignore save errors — cosmetic).
        let settings = self.settings.clone();
        let _ = self.settings_store.save(&settings);
        self.sessions = self.store.load_index();
        cx.notify();
    }

    /// Whether a turn is currently running for `session_id` (only the active
    /// session can be running).
    pub fn turn_running_for(&self, session_id: &str) -> bool {
        self.active
            .as_ref()
            .is_some_and(|a| a.meta.id == session_id && a.timeline.turn_running)
    }

    /// Record that a thread has been visited now (clears its unread dot).
    fn mark_visited(&mut self, session_id: &str) {
        self.settings
            .last_visited
            .insert(session_id.to_string(), now_secs());
        let settings = self.settings.clone();
        let _ = self.settings_store.save(&settings);
    }

    /// Mark a thread unread (context menu): set its last-visited just below its
    /// update time so the dot reappears.
    pub fn mark_session_unread(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let updated = self
            .sessions
            .iter()
            .find(|m| m.id == session_id)
            .map(|m| m.updated_at)
            .unwrap_or(0);
        self.settings
            .last_visited
            .insert(session_id.to_string(), updated.saturating_sub(1));
        let settings = self.settings.clone();
        let _ = self.settings_store.save(&settings);
        cx.notify();
    }

    /// Whether a thread shows an unread dot: it has been visited before, its
    /// update time is newer than that visit, and it is not the active thread.
    pub fn session_unread(&self, session_id: &str) -> bool {
        if self.active_session_id() == Some(session_id) {
            return false;
        }
        let Some(meta) = self.sessions.iter().find(|m| m.id == session_id) else {
            return false;
        };
        self.settings
            .last_visited
            .get(session_id)
            .is_some_and(|&visited| meta.updated_at > visited)
    }

    /// Whether any non-archived thread in `project_id` is unread (project dot).
    pub fn project_has_unread(&self, project_id: &str) -> bool {
        self.sessions.iter().any(|m| {
            m.archived_at.is_none()
                && m.project_id.as_deref() == Some(project_id)
                && self.session_unread(&m.id)
        })
    }

    // -- checkpoints + revert (Group B) -------------------------------------

    /// Snapshot the pre-turn working tree into a hidden git ref so the turn the
    /// just-recorded user message opened can later be reverted. `event_offset`
    /// is the JSONL length before that message, used as the revert truncation
    /// boundary. Runs synchronously so the snapshot precedes any file edits.
    fn capture_checkpoint(&mut self, session_id: &str, event_offset: usize, cx: &mut Context<Self>) {
        let Some(active) = self.active.as_ref().filter(|a| a.meta.id == session_id) else {
            return;
        };
        let cwd = active.meta.cwd.clone();
        let Some(turn) = active.timeline.entries.last().map(|e| e.turn) else {
            return;
        };
        // A second queued send can land in the same open turn — checkpoint once.
        if active.meta.checkpoints.iter().any(|c| c.turn == turn) {
            return;
        }
        if !crate::checkpoints::is_git_repo(&cwd) {
            return;
        }
        match crate::checkpoints::create_checkpoint(&cwd, session_id, turn) {
            Ok(commit) => {
                if let Some(active) = self.active.as_mut().filter(|a| a.meta.id == session_id) {
                    active.meta.checkpoints.push(Checkpoint {
                        turn,
                        commit,
                        event_offset,
                    });
                    let meta = active.meta.clone();
                    self.persist_meta(&meta, cx);
                }
            }
            Err(err) => log::warn!("failed to create checkpoint: {err}"),
        }
    }

    /// Whether the given timeline turn has a checkpoint (its user bubble then
    /// gets the hover "revert" affordance).
    pub fn turn_has_checkpoint(&self, turn: usize) -> bool {
        self.active
            .as_ref()
            .is_some_and(|a| a.meta.checkpoints.iter().any(|c| c.turn == turn))
    }

    /// Revert the active thread to the checkpoint captured before `turn`:
    /// restore the worktree, discard newer messages/turns from the log, drop the
    /// newer checkpoint refs, and roll the provider session back to idle. Blocked
    /// while a turn runs.
    pub fn revert_to_turn(&mut self, turn: usize, cx: &mut Context<Self>) {
        let (session_id, cwd, checkpoint) = {
            let Some(active) = self.active.as_ref() else {
                return;
            };
            if active.timeline.turn_running {
                cx.emit(AppEvent::Error(
                    rust_i18n::t!("checkpoint.revert_blocked").into_owned(),
                ));
                return;
            }
            let Some(cp) = active.meta.checkpoints.iter().find(|c| c.turn == turn).cloned() else {
                return;
            };
            (active.meta.id.clone(), active.meta.cwd.clone(), cp)
        };

        if let Err(err) = crate::checkpoints::restore_checkpoint(&cwd, &checkpoint.commit) {
            self.report_error(err, cx);
            return;
        }
        crate::checkpoints::delete_checkpoint_refs_from(&cwd, &session_id, turn);
        if let Err(err) = self.store.truncate_events(&session_id, checkpoint.event_offset) {
            self.report_error(
                rust_i18n::t!("errors.persist_event", error = err).into_owned(),
                cx,
            );
            return;
        }

        // Re-fold the truncated timeline and roll the session back to idle.
        let events = self.store.read_events(&session_id);
        let mut timeline = Timeline::fold_events(events);
        timeline.mark_idle();
        if let Some(active) = self.active.as_mut().filter(|a| a.meta.id == session_id) {
            active.shutdown_to_idle();
            active.meta.checkpoints.retain(|c| c.turn < turn);
            active.meta.resume_cursor = None;
            active.meta.updated_at = now_secs();
            active.timeline = timeline;
            active.git_branch = read_git_branch(&active.meta.cwd);
            active.pending_sends.clear();
            active.plan_implemented = false;
            let meta = active.meta.clone();
            self.persist_meta(&meta, cx);
        }
        cx.emit(AppEvent::Notice(
            rust_i18n::t!("checkpoint.reverted").into_owned(),
        ));
        cx.notify();
    }

    // -- worktree mode (Group C) --------------------------------------------

    /// The active draft's workspace mode, or `None` when there is no draft (a
    /// started session's workspace is locked).
    pub fn draft_workspace_mode(&self) -> Option<WorkspaceMode> {
        self.active
            .as_ref()
            .filter(|a| a.draft)
            .map(|a| a.draft_workspace.clone())
    }

    /// Whether a worktree is being prepared for the active thread's first send.
    pub fn preparing_worktree(&self) -> bool {
        self.active.as_ref().is_some_and(|a| a.preparing_worktree)
    }

    /// Choose the draft's workspace mode (checkout-row picker). No-op unless the
    /// active thread is an unstarted draft.
    pub fn set_draft_workspace(&mut self, mode: WorkspaceMode, cx: &mut Context<Self>) {
        if let Some(active) = self.active.as_mut().filter(|a| a.draft) {
            active.draft_workspace = mode;
            cx.notify();
        }
    }

    /// Kick off background worktree creation for a draft's first send, then send
    /// the queued text once it is ready. Sets the "Preparing worktree…" state.
    fn begin_worktree_prep(
        &mut self,
        text: String,
        attachments: Vec<Attachment>,
        base: String,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        active.preparing_worktree = true;
        let session_id = active.meta.id.clone();
        let root = active.meta.cwd.clone();
        cx.notify();

        let path = worktree_path_for(&session_id);
        let branch = format!("tcode/{session_id}");
        let base_for_task = base.clone();
        let root_for_task = root.clone();
        let path_for_task = path.clone();
        let branch_for_task = branch.clone();
        cx.spawn(async move |this, cx| {
            let result = smol::unblock(move || {
                create_git_worktree(&root_for_task, &path_for_task, &branch_for_task, &base_for_task)
            })
            .await;
            let _ = this.update(cx, |state, cx| {
                let Some(active) = state
                    .active
                    .as_mut()
                    .filter(|a| a.meta.id == session_id && a.draft)
                else {
                    return;
                };
                active.preparing_worktree = false;
                match result {
                    Ok(worktree_path) => {
                        active.meta.cwd = worktree_path.clone();
                        active.meta.worktree = Some(WorktreeInfo {
                            root_project_path: root,
                            base,
                            branch,
                        });
                        active.draft_workspace = WorkspaceMode::LocalCheckout;
                        active.git_branch = read_git_branch(&worktree_path);
                        // Now that the worktree exists, run the deferred send.
                        state.send_turn(text, attachments, cx);
                    }
                    Err(err) => {
                        active.draft_workspace = WorkspaceMode::LocalCheckout;
                        state.report_error(err, cx);
                    }
                }
            });
        })
        .detach();
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
        // Smoke mode forces Supervised so the approval path stays exercised even
        // though the app-wide default is now FullAccess (T3 parity). Must be set
        // before `ensure_started` spawns the provider with these launch flags.
        if self.smoke.is_some() {
            meta.approval_mode = ApprovalMode::Supervised;
        }
        // Associate with the given project, or derive one from the cwd.
        meta.project_id = match project_id {
            Some(id) if self.projects.iter().any(|p| p.id == id) => Some(id),
            _ => self.create_project(meta.cwd.clone(), cx),
        };
        if let Err(err) = self.store.upsert_meta(&meta) {
            self.report_error(
                rust_i18n::t!("errors.persist_session", error = err).into_owned(),
                cx,
            );
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
            live_approval_mode: None,
            live_option_selections: Vec::new(),
            pending_ultrathink: false,
            plan_implemented: false,
            draft_workspace: WorkspaceMode::LocalCheckout,
            preparing_worktree: false,
            pending_sends: Vec::new(),
            turn_in_flight: false,
            provider_commands: Vec::new(),
            diff_open: false,
            diff_expanded: false,
            diff_selected_turn: None,
            right_tab: RightTab::default(),
            auto_open_suppressed: false,
            terminal_workspace: TerminalWorkspace::default(),
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
            live_approval_mode: None,
            live_option_selections: Vec::new(),
            pending_ultrathink: false,
            plan_implemented: false,
            draft_workspace: WorkspaceMode::LocalCheckout,
            preparing_worktree: false,
            pending_sends: Vec::new(),
            turn_in_flight: false,
            provider_commands: Vec::new(),
            diff_open: false,
            diff_expanded: false,
            diff_selected_turn: None,
            right_tab: RightTab::default(),
            auto_open_suppressed: false,
            terminal_workspace: TerminalWorkspace::default(),
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
        self.mark_visited(session_id);
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
        let terminal_preferences = self.terminal_preferences.get(&meta.id).copied();
        let mut terminal_workspace = TerminalWorkspace::default();
        if let Some(preferences) = terminal_preferences {
            terminal_workspace.height = preferences.height.clamp(120., 600.);
        }
        let git_branch = read_git_branch(&meta.cwd);
        self.active = Some(ActiveSession {
            meta,
            timeline,
            git_branch,
            branches: Vec::new(),
            draft: false,
            runtime: Runtime::Idle,
            live_model: None,
            live_approval_mode: None,
            live_option_selections: Vec::new(),
            pending_ultrathink: false,
            plan_implemented: false,
            draft_workspace: WorkspaceMode::LocalCheckout,
            preparing_worktree: false,
            pending_sends: Vec::new(),
            turn_in_flight: false,
            provider_commands: Vec::new(),
            diff_open: false,
            diff_expanded: false,
            diff_selected_turn: None,
            right_tab: RightTab::default(),
            auto_open_suppressed: false,
            terminal_workspace,
            _pump: None,
        });
        if terminal_preferences.is_some_and(|preferences| preferences.open) {
            self.open_terminal_panel(cx);
            let count = terminal_preferences
                .map(|preferences| preferences.count.clamp(1, MAX_TERMINALS_PER_SESSION))
                .unwrap_or(1);
            for _ in 1..count {
                self.new_terminal(cx);
            }
        }
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
    pub fn send_turn(&mut self, text: String, attachments: Vec<Attachment>, cx: &mut Context<Self>) {
        // Group C: a draft in worktree mode creates its worktree in the
        // background on first send, then re-enters send_turn once ready.
        if let Some(active) = self.active.as_ref() {
            if active.draft && !active.preparing_worktree {
                if let WorkspaceMode::NewWorktree { base } = active.draft_workspace.clone() {
                    self.begin_worktree_prep(text, attachments, base, cx);
                    return;
                }
            }
        }

        // The first send on a draft materializes it into a real (persisted)
        // session so the sidebar row appears; the provider then starts below.
        if self.active_is_draft() {
            if let Err(err) = self.commit_draft() {
                self.report_error(
                    rust_i18n::t!("errors.persist_session", error = err).into_owned(),
                    cx,
                );
                return;
            }
        }

        let Some(active) = self.active.as_mut() else {
            return;
        };
        let session_id = active.meta.id.clone();

        // Group B: the JSONL length before this turn's user message — the revert
        // truncation boundary — captured before the message is appended.
        let checkpoint_offset = self.store.event_count(&session_id);

        // Record the user message as a synthetic canonical event so replay
        // renders it identically (providers don't echo user input).
        let user_event = AgentEvent::ItemCompleted(ThreadItem {
            id: format!("local-user-{}", uuid::Uuid::new_v4()),
            content: ItemContent::UserMessage { text: text.clone() },
        });
        self.record_event(&session_id, &user_event, cx);

        // Group B: snapshot the pre-turn working tree for this turn's revert.
        self.capture_checkpoint(&session_id, checkpoint_offset, cx);

        self.maybe_adopt_title(cx);

        let Some(active) = self.active.as_mut() else {
            return;
        };
        // Ultrathink is a per-send prompt-prefix mode (never persisted): prepend
        // it to the text sent to the provider (the recorded user message above
        // stays clean) and disarm it.
        let sent_text = if active.pending_ultrathink {
            active.pending_ultrathink = false;
            format!("Ultrathink:\n{text}")
        } else {
            text
        };
        active.pending_sends.push(PendingSend {
            text: sent_text,
            attachments,
        });
        // If the user switched models — or a provider that can't switch its
        // approval mode live (Codex) had its mode changed, or a launch-time
        // option changed — while the provider is live, restart it first: the
        // queued turn then flushes on the fresh process, resumed from the stored
        // cursor with the current model + options + mode.
        if active.model_changed_while_live() {
            log::info!(
                "model changed to {:?} while live; restarting provider before next turn",
                active.meta.model
            );
            active.shutdown_to_idle();
        } else if active.approval_mode_changed_while_live() {
            log::info!(
                "approval mode changed to {:?} while live; restarting provider before next turn",
                active.meta.approval_mode
            );
            active.shutdown_to_idle();
        } else if active.options_changed_while_live() {
            log::info!("launch-time option changed while live; restarting provider before next turn");
            active.shutdown_to_idle();
        }
        let should_start = matches!(active.runtime, Runtime::Idle);
        let dispatch_failed = active.dispatch_next_pending().is_err();
        if should_start {
            self.ensure_started(cx);
        }
        if dispatch_failed {
            self.report_error(rust_i18n::t!("errors.process_gone").into_owned(), cx);
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

    /// Answer a pending user-input request (Claude `AskUserQuestion` / Codex
    /// `requestUserInput`). `answers` is keyed by [`UserInputQuestion::id`] with
    /// string (single-select / free text) or string-array (multi-select) values.
    pub fn respond_user_input(
        &mut self,
        request_id: String,
        answers: serde_json::Map<String, serde_json::Value>,
        cx: &mut Context<Self>,
    ) {
        if let Some(ActiveSession {
            runtime: Runtime::Live(commands),
            ..
        }) = &self.active
        {
            let _ = commands.try_send(SessionCommand::RespondUserInput {
                request_id,
                answers,
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
            // A different model has different option descriptors: drop stale
            // selections so each resolves to the new model's defaults.
            active.meta.option_selections.clear();
            active.pending_ultrathink = false;
            cx.notify();
            return;
        }
        if active.meta.model == model {
            return;
        }
        active.meta.model = model;
        active.meta.option_selections.clear();
        active.pending_ultrathink = false;
        active.meta.updated_at = now_secs();
        let meta = active.meta.clone();
        self.persist_meta(&meta, cx);
    }

    // -- traits (option selections) -----------------------------------------

    /// Set (or clear) the persisted value of one option descriptor for the
    /// active session. `value` is a string (select) or bool (boolean); passing
    /// `None` removes the selection so it resolves back to its default. Takes
    /// effect per the restart machinery (see `send_turn`).
    pub fn set_active_option(
        &mut self,
        id: &str,
        value: Option<serde_json::Value>,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        active.meta.option_selections.retain(|s| s.id != id);
        if let Some(value) = value {
            active.meta.option_selections.push(OptionSelection {
                id: id.to_string(),
                value,
            });
        }
        // Selecting a real reasoning effort supersedes a pending Ultrathink.
        if id == "reasoningEffort" {
            active.pending_ultrathink = false;
        }
        if active.draft {
            cx.notify();
            return;
        }
        active.meta.updated_at = now_secs();
        let meta = active.meta.clone();
        self.persist_meta(&meta, cx);
    }

    /// Arm an Ultrathink turn: the next send is prefixed with `Ultrathink:\n`.
    /// T3 does not persist this as an option (it resolves back to the default),
    /// so it lives as a transient per-send flag.
    pub fn select_ultrathink(&mut self, cx: &mut Context<Self>) {
        if let Some(active) = self.active.as_mut() {
            active.pending_ultrathink = true;
            cx.notify();
        }
    }

    /// Whether an Ultrathink turn is currently armed for the active session.
    pub fn ultrathink_armed(&self) -> bool {
        self.active.as_ref().is_some_and(|a| a.pending_ultrathink)
    }

    /// Whether a live launch-time option change will restart the provider on the
    /// next turn (for the traits popover's "restart" note).
    pub fn options_pending_restart(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(ActiveSession::options_changed_while_live)
    }

    // -- interaction mode (Build / Plan) ------------------------------------

    /// The active session's Build/Plan interaction mode (`Build` when none).
    pub fn active_interaction_mode(&self) -> InteractionMode {
        self.active
            .as_ref()
            .map(|a| a.meta.interaction_mode)
            .unwrap_or_default()
    }

    /// Set the Build/Plan interaction mode for the active session. Both
    /// providers switch live (Codex per turn, Claude via a control request), so
    /// no restart is scheduled.
    pub fn set_interaction_mode(&mut self, mode: InteractionMode, cx: &mut Context<Self>) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if active.meta.interaction_mode == mode {
            return;
        }
        active.meta.interaction_mode = mode;
        if let Runtime::Live(commands) = &active.runtime {
            let _ = commands.try_send(SessionCommand::SetInteractionMode(mode));
        }
        if active.draft {
            cx.notify();
            return;
        }
        active.meta.updated_at = now_secs();
        let meta = active.meta.clone();
        self.persist_meta(&meta, cx);
    }

    /// Toggle Build ↔ Plan (the chip click and Shift+Tab).
    pub fn toggle_interaction_mode(&mut self, cx: &mut Context<Self>) {
        let next = match self.active_interaction_mode() {
            InteractionMode::Build => InteractionMode::Plan,
            InteractionMode::Plan => InteractionMode::Build,
        };
        self.set_interaction_mode(next, cx);
    }

    // -- proposed-plan flow -------------------------------------------------

    /// The active session's captured proposed plan (markdown), if it is in the
    /// composer's "plan ready" state (present and not yet implemented).
    pub fn plan_ready_markdown(&self) -> Option<String> {
        let active = self.active.as_ref()?;
        if active.plan_implemented {
            return None;
        }
        active
            .timeline
            .proposed_plan
            .as_ref()
            .map(|p| p.markdown.clone())
    }

    /// Accept the proposed plan: send the verbatim implementation prompt, switch
    /// to Build mode, and clear the composer's plan-ready state.
    pub fn implement_plan(&mut self, cx: &mut Context<Self>) {
        let Some(markdown) = self.plan_ready_markdown() else {
            return;
        };
        if let Some(active) = self.active.as_mut() {
            active.plan_implemented = true;
        }
        self.set_interaction_mode(InteractionMode::Build, cx);
        self.send_turn(crate::session::implement_prompt(&markdown), Vec::new(), cx);
    }

    /// Accept the proposed plan in a fresh thread in the same project (same
    /// cwd/model/options, Build mode) titled "Implement <plan title>".
    pub fn implement_plan_in_new_thread(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        let Some(plan) = active.timeline.proposed_plan.as_ref() else {
            return;
        };
        let markdown = plan.markdown.clone();
        let title = match crate::session::plan_title(&markdown) {
            Some(t) => rust_i18n::t!("plan.implement_titled", title = t).into_owned(),
            None => rust_i18n::t!("plan.implement_untitled").into_owned(),
        };
        let provider = active.meta.provider;
        let cwd = active.meta.cwd.clone();
        let model = active.meta.model.clone();
        let option_selections = active.meta.option_selections.clone();
        let approval_mode = active.meta.approval_mode;
        let project_id = active.meta.project_id.clone();

        let mut meta = SessionMeta::new(provider, cwd, model);
        meta.title = title;
        meta.option_selections = option_selections;
        meta.approval_mode = approval_mode;
        meta.interaction_mode = InteractionMode::Build;
        meta.project_id = project_id;
        if let Err(err) = self.store.upsert_meta(&meta) {
            self.report_error(
                rust_i18n::t!("errors.persist_session", error = err).into_owned(),
                cx,
            );
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
            live_approval_mode: None,
            live_option_selections: Vec::new(),
            pending_ultrathink: false,
            plan_implemented: false,
            draft_workspace: WorkspaceMode::LocalCheckout,
            preparing_worktree: false,
            pending_sends: Vec::new(),
            turn_in_flight: false,
            provider_commands: Vec::new(),
            diff_open: false,
            diff_expanded: false,
            diff_selected_turn: None,
            right_tab: RightTab::default(),
            auto_open_suppressed: false,
            terminal_workspace: TerminalWorkspace::default(),
            _pump: None,
        });
        self.send_turn(crate::session::implement_prompt(&markdown), Vec::new(), cx);
        cx.notify();
    }

    /// Copy plan markdown to the clipboard (the "Copy to clipboard" action).
    pub fn copy_plan(&mut self, markdown: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(markdown));
    }

    /// Write the plan markdown to `PLAN-<n>.md` in the session cwd, choosing the
    /// lowest unused index ("Save to workspace"). Emits a success/error notice.
    pub fn save_plan_to_workspace(&mut self, markdown: String, cx: &mut Context<Self>) {
        let Some(cwd) = self.active.as_ref().map(|a| a.meta.cwd.clone()) else {
            return;
        };
        let mut n = 1;
        let path = loop {
            let candidate = cwd.join(format!("PLAN-{n}.md"));
            if !candidate.exists() {
                break candidate;
            }
            n += 1;
            if n > 9999 {
                break cwd.join("PLAN.md");
            }
        };
        match std::fs::write(&path, markdown) {
            Ok(()) => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                cx.emit(AppEvent::Notice(
                    rust_i18n::t!("plan.saved_workspace", file = name).into_owned(),
                ));
            }
            Err(err) => self.report_error(
                rust_i18n::t!("errors.persist_event", error = err).into_owned(),
                cx,
            ),
        }
        cx.notify();
    }

    /// Save the plan markdown to the user's Downloads directory (falling back to
    /// the session cwd) with a title-derived filename ("Download as markdown").
    pub fn download_plan(&mut self, markdown: String, cx: &mut Context<Self>) {
        let title = crate::session::plan_title(&markdown)
            .unwrap_or_else(|| rust_i18n::t!("plan.proposed_plan").into_owned());
        let filename = format!("{}.md", sanitize_filename(&title));
        let dir = dirs::download_dir()
            .or_else(|| self.active.as_ref().map(|a| a.meta.cwd.clone()))
            .unwrap_or_else(|| PathBuf::from("."));
        let path = dir.join(filename);
        match std::fs::write(&path, markdown) {
            Ok(()) => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                cx.emit(AppEvent::Notice(
                    rust_i18n::t!("plan.saved_workspace", file = name).into_owned(),
                ));
            }
            Err(err) => self.report_error(
                rust_i18n::t!("errors.persist_event", error = err).into_owned(),
                cx,
            ),
        }
        cx.notify();
    }

    /// The active session's latest structured plan steps and proposed plan (for
    /// the plan/task panel).
    pub fn plan_steps(&self) -> Vec<agent::PlanStep> {
        self.active
            .as_ref()
            .map(|a| a.timeline.plan_steps.clone())
            .unwrap_or_default()
    }

    pub fn proposed_plan_markdown(&self) -> Option<String> {
        self.active
            .as_ref()
            .and_then(|a| a.timeline.proposed_plan.as_ref())
            .map(|p| p.markdown.clone())
    }

    // -- right panel tabs ---------------------------------------------------

    pub fn right_tab(&self) -> RightTab {
        self.active
            .as_ref()
            .map(|a| a.right_tab)
            .unwrap_or_default()
    }

    /// Whether the right panel should offer a "Plan" tab (a proposed plan
    /// exists or the session is in Plan mode); otherwise the tab is "Tasks".
    pub fn plan_tab_active_label(&self) -> bool {
        self.active.as_ref().is_some_and(|a| {
            a.timeline.proposed_plan.is_some() || a.meta.interaction_mode == InteractionMode::Plan
        })
    }

    /// Switch the right panel's tab without changing its open state.
    pub fn set_right_tab(&mut self, tab: RightTab, cx: &mut Context<Self>) {
        if let Some(active) = self.active.as_mut() {
            active.right_tab = tab;
            cx.notify();
        }
    }

    /// The header plan/task toggle: open the Plan tab, switch to it if already
    /// open on another tab, else close it.
    pub fn toggle_plan_panel(&mut self, cx: &mut Context<Self>) {
        if let Some(active) = self.active.as_mut() {
            if active.diff_open && active.right_tab == RightTab::Plan {
                active.diff_open = false;
                if active.timeline.turn_running {
                    active.auto_open_suppressed = true;
                }
            } else {
                active.diff_open = true;
                active.right_tab = RightTab::Plan;
            }
            cx.notify();
        }
    }

    /// The header preview toggle: open the Preview tab, switch to it if the
    /// panel is already open on another tab, else close it.
    pub fn toggle_preview_panel(&mut self, cx: &mut Context<Self>) {
        if let Some(active) = self.active.as_mut() {
            if active.diff_open && active.right_tab == RightTab::Preview {
                active.diff_open = false;
                if active.timeline.turn_running {
                    active.auto_open_suppressed = true;
                }
            } else {
                active.diff_open = true;
                active.right_tab = RightTab::Preview;
            }
            cx.notify();
        }
    }

    /// Open the right panel on the Preview tab (used when the agent drives the
    /// preview so the webview surfaces without a manual toggle).
    pub fn open_preview_panel(&mut self, cx: &mut Context<Self>) {
        if let Some(active) = self.active.as_mut() {
            if !(active.diff_open && active.right_tab == RightTab::Preview) {
                active.diff_open = true;
                active.right_tab = RightTab::Preview;
                cx.notify();
            }
        }
    }

    /// Whether the right panel is open on the Preview tab (header button state).
    pub fn preview_panel_showing(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|a| a.diff_open && a.right_tab == RightTab::Preview)
    }

    /// Whether the right panel is open on the Plan tab (header button state).
    pub fn plan_panel_showing(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|a| a.diff_open && a.right_tab == RightTab::Plan)
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
                        cx.emit(AppEvent::Notice(
                            rust_i18n::t!("notice.switched_branch", branch = branch).into_owned(),
                        ));
                    }
                    Err(CheckoutError::Dirty) => {
                        cx.emit(AppEvent::Error(
                            rust_i18n::t!("notice.dirty_tree").into_owned(),
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

    /// The active session's selected approval mode (`ApprovalMode::default()` —
    /// now FullAccess — for a draft with no active session, matching a fresh
    /// `SessionMeta`).
    pub fn active_approval_mode(&self) -> ApprovalMode {
        self.active
            .as_ref()
            .map(|a| a.meta.approval_mode)
            .unwrap_or_default()
    }

    /// Select `mode` for the active session and persist it. Claude applies the
    /// switch live over the control protocol; Codex (which binds the mode at
    /// thread start) instead restarts via the resume cursor on the next turn.
    pub fn set_active_approval_mode(&mut self, mode: ApprovalMode, cx: &mut Context<Self>) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if active.meta.approval_mode == mode {
            return;
        }
        active.meta.approval_mode = mode;
        active.meta.updated_at = now_secs();

        if let Runtime::Live(commands) = &active.runtime {
            let _ = commands.try_send(SessionCommand::SetApprovalMode(mode));
            // Claude applies the switch live: keep `live_approval_mode` in sync so
            // no restart is scheduled. Codex can't, so leave it stale — the next
            // `send_turn` sees the mismatch and restarts from the resume cursor.
            if active.meta.provider == ProviderKind::ClaudeCode {
                active.live_approval_mode = Some(mode);
            }
        }

        let meta = active.meta.clone();
        self.persist_meta(&meta, cx);
    }

    /// Whether changing approval mode will restart the live provider on the next
    /// turn (Codex) — used by the permission picker to show a "restart" note.
    pub fn approval_pending_restart(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(ActiveSession::approval_mode_changed_while_live)
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
        // Remember the model + approval mode this process is being launched
        // with so a later switch can detect the mismatch and restart.
        active.live_model = active.meta.model.clone();
        active.live_approval_mode = Some(active.meta.approval_mode);
        active.live_option_selections = active.meta.option_selections.clone();

        let meta = active.meta.clone();
        let settings = self.settings.clone();
        let mcp_registration = self.mcp_registration.clone();
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
            let opts = session_options(&meta, &settings, mcp_registration);
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
                            let message =
                                rust_i18n::t!("errors.provider_start", error = err).into_owned();
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
                Some(reason) => {
                    rust_i18n::t!("errors.provider_closed_reason", reason = reason).into_owned()
                }
                None => rust_i18n::t!("errors.provider_closed").into_owned(),
            };
            self.report_error(message, cx);
            cx.notify();
            return;
        }

        // Provider commands/skills are session metadata for the composer menus —
        // stored on the active session, never folded into the timeline or the
        // persisted JSONL log.
        if let AgentEvent::ProviderCommands { commands } = &event {
            if let Some(active) = self
                .active
                .as_mut()
                .filter(|active| active.meta.id == session_id)
            {
                active.provider_commands = commands.clone();
                cx.notify();
            }
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

        // Plan surfaces: a fresh proposed plan re-arms the composer's plan-ready
        // state; a new turn clears the per-turn auto-open suppression; the first
        // structured plan update of a turn may auto-open the task panel.
        let auto_open = self.settings.auto_open_task_panel;
        if let Some(active) = self
            .active
            .as_mut()
            .filter(|active| active.meta.id == session_id)
        {
            match &event {
                AgentEvent::TurnStarted { .. } => active.auto_open_suppressed = false,
                AgentEvent::ProposedPlan { .. } | AgentEvent::ProposedPlanDelta { .. } => {
                    active.plan_implemented = false;
                }
                AgentEvent::PlanUpdated { .. } => {
                    let already_showing =
                        active.diff_open && active.right_tab == RightTab::Plan;
                    if auto_open && !active.auto_open_suppressed && !already_showing {
                        active.diff_open = true;
                        active.right_tab = RightTab::Plan;
                    }
                }
                _ => {}
            }
        }

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
                self.report_error(rust_i18n::t!("errors.process_gone").into_owned(), cx);
            }
        }

        // Smoke-mode automation.
        if let Some(smoke) = self.smoke {
            match &event {
                AgentEvent::ApprovalRequested(request) if smoke.auto_approve => {
                    log::info!("smoke: auto-approving request {}", request.id);
                    self.respond_approval(request.id.clone(), ApprovalDecision::Approve, cx);
                }
                AgentEvent::UserInputRequested {
                    request_id,
                    questions,
                } if smoke.auto_approve => {
                    // Keep smokes deterministic: answer each question with its
                    // first option's label (or empty string when the question is
                    // free-text-only).
                    let mut answers = serde_json::Map::new();
                    for question in questions {
                        let answer = question
                            .options
                            .first()
                            .map(|o| o.label.clone())
                            .unwrap_or_default();
                        log::info!(
                            "smoke: auto-answering user-input {} / {:?} -> {:?}",
                            request_id,
                            question.id,
                            answer
                        );
                        answers.insert(question.id.clone(), serde_json::Value::String(answer));
                    }
                    self.respond_user_input(request_id.clone(), answers, cx);
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
            self.report_error(
                rust_i18n::t!("errors.persist_event", error = err).into_owned(),
                cx,
            );
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
            self.report_error(
                rust_i18n::t!("errors.persist_session_index", error = err).into_owned(),
                cx,
            );
        }
        self.sessions = self.store.load_index();
        cx.notify();
    }

    pub fn shutdown_active(&mut self) {
        self.persist_terminal_preferences();
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

/// The bare command name for a provider (fallback when no path resolves).
fn default_program(provider: ProviderKind) -> String {
    match provider {
        ProviderKind::Codex => "codex".into(),
        ProviderKind::ClaudeCode => "claude".into(),
    }
}

/// A minimal PATH lookup for `name` (first executable match). Used to locate the
/// provider binary for install-source detection when no override is set.
fn which_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Spawn `program args…` and return its trimmed stdout, or `None` on any
/// failure. The nested-Claude markers are stripped so `claude --version` behaves
/// like a top-level invocation.
async fn run_capture(program: &str, args: &[&str]) -> Option<String> {
    let output = smol::process::Command::new(program)
        .args(args)
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Spawn `program args…` for a side effect (e.g. an update command) and report
/// whether it exited successfully.
async fn run_status(program: &str, args: &[&str]) -> bool {
    smol::process::Command::new(program)
        .args(args)
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
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

/// The path a session's dedicated worktree lives at (`~/.tcode/worktrees/<id>`),
/// falling back to a temp dir when the home directory is unknown.
fn worktree_path_for(session_id: &str) -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".tcode")
        .join("worktrees")
        .join(session_id)
}

/// Create a dedicated worktree at `path` branching `branch` from `base`, run
/// from the project checkout `root`. Returns the created worktree path.
fn create_git_worktree(
    root: &std::path::Path,
    path: &std::path::Path,
    branch: &str,
    base: &str,
) -> Result<PathBuf, String> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let out = std::process::Command::new("git")
        .current_dir(root)
        .args([
            "worktree",
            "add",
            "-b",
            branch,
            &path.to_string_lossy(),
            base,
        ])
        .output()
        .map_err(|e| rust_i18n::t!("errors.worktree_add", error = e).into_owned())?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(rust_i18n::t!("errors.worktree_add", error = stderr.trim()).into_owned());
    }
    Ok(path.to_path_buf())
}

/// Remove the worktree at `path` (force), run from the project checkout `root`.
fn remove_git_worktree(root: &std::path::Path, path: &std::path::Path) -> Result<(), String> {
    let out = std::process::Command::new("git")
        .current_dir(root)
        .args(["worktree", "remove", "--force", &path.to_string_lossy()])
        .output()
        .map_err(|e| rust_i18n::t!("errors.worktree_remove", error = e).into_owned())?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(rust_i18n::t!("errors.worktree_remove", error = stderr.trim()).into_owned());
    }
    Ok(())
}

/// A filesystem-safe filename fragment: replace path separators and control
/// characters with `-`, collapse runs, and cap the length.
fn sanitize_filename(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '-'
            } else {
                c
            }
        })
        .collect();
    out = out.trim().trim_matches('-').to_string();
    if out.is_empty() {
        out = "plan".to_string();
    }
    out.chars().take(80).collect()
}

/// The reasoning-effort selection value, if any (Codex applies it per turn).
fn codex_effort_selection(selections: &[OptionSelection]) -> Option<String> {
    selections
        .iter()
        .find(|s| s.id == "reasoningEffort")
        .and_then(|s| s.value.as_str().map(str::to_string))
}

/// Selections sorted by id for order-independent comparison, optionally dropping
/// the reasoning-effort entry (which, for Codex, applies per turn and never
/// forces a restart).
fn normalized_selections(
    selections: &[OptionSelection],
    ignore_effort: bool,
) -> Vec<(String, serde_json::Value)> {
    let mut out: Vec<(String, serde_json::Value)> = selections
        .iter()
        .filter(|s| !(ignore_effort && s.id == "reasoningEffort"))
        .map(|s| (s.id.clone(), s.value.clone()))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn session_options(
    meta: &SessionMeta,
    settings: &Settings,
    mcp_server: Option<agent::McpRegistration>,
) -> SessionOptions {
    let binary_path = match meta.provider {
        ProviderKind::Codex => settings.codex_binary.clone(),
        ProviderKind::ClaudeCode => settings.claude_binary.clone(),
    };
    SessionOptions {
        cwd: meta.cwd.clone(),
        model: meta.model.clone(),
        resume: meta.resume_cursor.clone(),
        binary_path,
        approval_mode: meta.approval_mode,
        option_selections: meta.option_selections.clone(),
        interaction_mode: meta.interaction_mode,
        mcp_server,
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

        let codex_options = session_options(&codex, &settings, None);
        let claude_options = session_options(&claude, &settings, None);

        assert_eq!(codex_options.binary_path, settings.codex_binary);
        assert_eq!(claude_options.binary_path, settings.claude_binary);
        assert!(codex_options.mcp_server.is_none());
    }

    #[test]
    fn session_options_injects_mcp_registration() {
        let settings = Settings::default();
        let meta = SessionMeta::new(ProviderKind::ClaudeCode, PathBuf::from("/x"), None);
        let reg = agent::McpRegistration {
            url: "http://127.0.0.1:7/mcp".into(),
            bearer_token: "tok".into(),
        };
        let opts = session_options(&meta, &settings, Some(reg));
        let mcp = opts.mcp_server.expect("registration threaded through");
        assert_eq!(mcp.url, "http://127.0.0.1:7/mcp");
        assert_eq!(mcp.bearer_token, "tok");
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
            live_approval_mode: None,
            live_option_selections: Vec::new(),
            pending_ultrathink: false,
            plan_implemented: false,
            draft_workspace: WorkspaceMode::LocalCheckout,
            preparing_worktree: false,
            pending_sends: Vec::new(),
            turn_in_flight: false,
            provider_commands: Vec::new(),
            diff_open: false,
            diff_expanded: false,
            diff_selected_turn: None,
            right_tab: RightTab::default(),
            auto_open_suppressed: false,
            terminal_workspace: TerminalWorkspace::default(),
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
            live_approval_mode: None,
            live_option_selections: Vec::new(),
            pending_ultrathink: false,
            plan_implemented: false,
            draft_workspace: WorkspaceMode::LocalCheckout,
            preparing_worktree: false,
            pending_sends: vec!["first".into(), "second".into()],
            turn_in_flight: false,
            provider_commands: Vec::new(),
            diff_open: false,
            diff_expanded: false,
            diff_selected_turn: None,
            right_tab: RightTab::default(),
            auto_open_suppressed: false,
            terminal_workspace: TerminalWorkspace::default(),
            _pump: None,
        };

        assert_eq!(active.dispatch_next_pending(), Ok(true));
        assert!(matches!(
            receiver.try_recv(),
            Ok(SessionCommand::SendTurn { text, .. }) if text == "first"
        ));
        assert_eq!(active.dispatch_next_pending(), Ok(false));
        assert!(receiver.try_recv().is_err());
        assert_eq!(active.pending_sends, [PendingSend::from("second")]);

        active.turn_in_flight = false;
        assert_eq!(active.dispatch_next_pending(), Ok(true));
        assert!(matches!(
            receiver.try_recv(),
            Ok(SessionCommand::SendTurn { text, .. }) if text == "second"
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
            live_approval_mode: None,
            live_option_selections: Vec::new(),
            pending_ultrathink: false,
            plan_implemented: false,
            draft_workspace: WorkspaceMode::LocalCheckout,
            preparing_worktree: false,
            pending_sends: Vec::new(),
            turn_in_flight: false,
            provider_commands: Vec::new(),
            diff_open: false,
            diff_expanded: false,
            diff_selected_turn: None,
            right_tab: RightTab::default(),
            auto_open_suppressed: false,
            terminal_workspace: TerminalWorkspace::default(),
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
        let mut meta = SessionMeta::new(
            ProviderKind::ClaudeCode,
            PathBuf::from("/tmp/project"),
            None,
        );
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
            live_approval_mode: None,
            live_option_selections: Vec::new(),
            pending_ultrathink: false,
            plan_implemented: false,
            draft_workspace: WorkspaceMode::LocalCheckout,
            preparing_worktree: false,
            pending_sends: vec!["do it".into()],
            turn_in_flight: false,
            provider_commands: Vec::new(),
            diff_open: false,
            diff_expanded: false,
            diff_selected_turn: None,
            right_tab: RightTab::default(),
            auto_open_suppressed: false,
            terminal_workspace: TerminalWorkspace::default(),
            _pump: None,
        };

        assert!(active.model_changed_while_live());
        active.shutdown_to_idle();

        // Live provider is told to shut down and the runtime is back to Idle,
        // while the queued turn is preserved for the restarted process.
        assert!(matches!(receiver.try_recv(), Ok(SessionCommand::Shutdown)));
        assert!(matches!(active.runtime, Runtime::Idle));
        assert_eq!(active.pending_sends, [PendingSend::from("do it")]);
        assert!(!active.model_changed_while_live());

        // No restart when the selected model matches the live one.
        active.runtime = Runtime::Live(async_channel::unbounded().0);
        active.live_model = active.meta.model.clone();
        assert!(!active.model_changed_while_live());
    }

    #[test]
    fn archived_hidden_from_sidebar_and_unread_logic() {
        let root =
            std::env::temp_dir().join(format!("tcode-archive-test-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::open_at(root.clone()).unwrap();
        let mut state = AppState::new(store);
        let project = Project {
            id: "p1".into(),
            name: "Proj".into(),
            root: PathBuf::from("/p"),
            created_at: 1,
        };
        state.projects = vec![project.clone()];
        let mut visible = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/p"), None);
        visible.project_id = Some(project.id.clone());
        visible.updated_at = 100;
        let mut archived = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/p"), None);
        archived.project_id = Some(project.id.clone());
        archived.updated_at = 100;
        archived.archived_at = Some(50);
        state.sessions = vec![visible.clone(), archived.clone()];

        // Sidebar groups exclude archived; the Archived view includes only it.
        let groups = state.grouped_sessions();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].sessions.len(), 1);
        assert_eq!(groups[0].sessions[0].id, visible.id);
        let arch = state.archived_groups();
        assert_eq!(arch.len(), 1);
        assert_eq!(arch[0].sessions.len(), 1);
        assert_eq!(arch[0].sessions[0].id, archived.id);

        // Unread: never-visited is not unread; visited-before-update is unread;
        // visited-at-or-after-update clears it.
        assert!(!state.session_unread(&visible.id));
        state.settings.last_visited.insert(visible.id.clone(), 50);
        assert!(state.session_unread(&visible.id));
        assert!(state.project_has_unread(&project.id));
        state.settings.last_visited.insert(visible.id.clone(), 100);
        assert!(!state.session_unread(&visible.id));
        assert!(!state.project_has_unread(&project.id));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn worktree_orphan_detected_only_for_last_session() {
        let root =
            std::env::temp_dir().join(format!("tcode-wt-test-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::open_at(root.clone()).unwrap();
        let mut state = AppState::new(store);
        let worktree = WorktreeInfo {
            root_project_path: PathBuf::from("/proj"),
            base: "main".into(),
            branch: "tcode/shared".into(),
        };
        let mut a = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/wt"), None);
        a.worktree = Some(worktree.clone());
        let mut b = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/wt"), None);
        b.worktree = Some(worktree.clone());
        let solo = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/plain"), None);

        // Two sessions share the worktree: deleting one does not orphan it.
        state.sessions = vec![a.clone(), b.clone(), solo.clone()];
        assert!(state.worktree_orphaned_by_delete(&a.id).is_none());
        // A session with no worktree never reports an orphan.
        assert!(state.worktree_orphaned_by_delete(&solo.id).is_none());
        // Once it's the last session on the worktree, deleting it orphans it.
        state.sessions = vec![a.clone(), solo];
        assert_eq!(
            state.worktree_orphaned_by_delete(&a.id).map(|w| w.branch),
            Some("tcode/shared".to_string())
        );

        let _ = std::fs::remove_dir_all(root);
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
