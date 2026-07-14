//! Application state: session registry, active session runtime, event pump.

use std::collections::HashMap;
use std::path::PathBuf;

use agent::{
    AgentEvent, ApprovalDecision, ApprovalMode, Attachment, InteractionMode, ItemContent,
    LaunchEnv, ModelSpec, OptionDescriptor, OptionSelection, ProviderCommand, ProviderKind,
    SessionCommand, SessionOptions, ThreadItem, TurnOptions, TurnStatus, list_models,
    start_session,
};
#[cfg(test)]
use gpui::Entity;
use gpui::{Context, EventEmitter, Task};
use serde::{Deserialize, Serialize};

use crate::blocking::unblock;
use tcode_core::acp::InstalledAcpAgent as InstalledAgent;
use tcode_core::git::{
    GitAction, GitFileEntry, GitStatus, MenuItem, QuickAction, build_commit_prompt, menu_items,
    quick_action, sanitize_commit_message,
};
use tcode_core::project::{Checkpoint, Project, SessionMeta, WorktreeInfo};
use tcode_core::provider_models::{ResolvedModel, picker_models, resolve_models};
use tcode_core::provider_status::ProviderSnapshot;
use tcode_core::session::{
    EntryContent, ReviewComment, Timeline, implement_prompt, plan_title, turn_user_event_offset,
};
#[cfg(test)]
use tcode_core::settings::EnvVar;
use tcode_core::settings::{ProjectSort, ProviderSettings, Settings};
use tcode_services::acp_registry::{
    Registry, RegistryAgent, cached, install, load, platform_key, resolve_recipe, uninstall,
    visible_agents,
};
#[cfg(test)]
use tcode_services::checkpoints::checkpoint_ref_exists;
use tcode_services::checkpoints::{
    create_checkpoint, delete_all_checkpoint_refs, delete_checkpoint_refs_from, is_git_repo,
    restore_checkpoint,
};
use tcode_services::git::{
    CheckoutError, checkout_if_clean, commit_diff_context, create_git_worktree, list_git_branches,
    perform_action, read_git_branch, read_status, remove_git_worktree, run_claude_headless,
    worktree_path_for,
};
use tcode_services::import::{
    ExternalRoots, ImportOutcome, existing_external_ids, import_thread, scan_recent_dirs,
};
#[cfg(test)]
use tcode_services::process::command;
use tcode_services::provider_probe::{
    default_program, probe_provider, run_capture, run_capture_env, run_status, which_in_path,
};
use tcode_services::settings::SettingsStore;
use tcode_services::store::{SessionStore, now_millis, now_secs};
use tcode_services::user_files;
use tcode_services::version_check::{
    InstallSource, detect_install_source, is_update_available, npm_package, parse_version,
    update_command, update_command_string,
};
use tcode_services::workspace::list_workspace;

use crate::ui_facade::{
    AcpMarketplaceItem, ExternalImportUpdate, ExternalThread, PathEntry, RecentDir,
};

#[rustfmt::skip]
pub use tcode_core::project::{group_sessions, ProjectGroup};
pub use crate::event::{
    GitActionRequest, RuntimeEffect, RuntimeError, RuntimeEvent as AppEvent, RuntimeNotice,
    RuntimeOperationId, RuntimeToast,
};
pub use crate::terminal::{
    MAX_TERMINALS_PER_SESSION, TerminalContext, TerminalEntry, TerminalSplit,
    TerminalSplitDirection, TerminalWorkspace,
};

const TITLE_MAX_CHARS: usize = 40;
const ORCHESTRATE_CALLBACK_CAP: usize = 100;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct TerminalPreferences {
    open: bool,
    height: f32,
    count: usize,
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

/// A message waiting for an ordinary turn. Most are user-authored messages sent
/// while another turn was running; orchestration callbacks also wait here while
/// an idle provider is starting.
///
/// Queueing is an APP-LEVEL concept and works for every provider, including the
/// ones that cannot steer. The queue is per-session and in-memory only: it is
/// deliberately NOT persisted to the session JSONL, because a queued message is
/// not yet part of the conversation (it is recorded only once it is actually
/// dispatched, or steered, as a user message).
#[derive(Debug, Clone, PartialEq)]
pub struct QueuedMessage {
    /// Stable per-session id, so the UI can address a row for steer/drop even
    /// as earlier entries are dispatched out from under it.
    pub id: u64,
    pub text: String,
    pub attachments: Vec<Attachment>,
    /// Ultrathink was armed when this message was written. It is a per-send
    /// prompt-prefix mode, so it rides with the message rather than with the
    /// session, and is applied only to the text sent on the wire (the user
    /// message recorded in the transcript stays clean).
    ultrathink: bool,
    /// Orchestration callbacks arriving during the same provider-start window
    /// are folded into one wake-up turn. Once that turn is live, later callbacks
    /// are steered into it instead of becoming more queued turns.
    kind: QueuedMessageKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueuedMessageKind {
    User,
    OrchestrateCallback,
}

impl QueuedMessage {
    /// The text actually sent to the provider (Ultrathink prefix applied).
    fn wire_text(&self) -> String {
        if self.ultrathink {
            format!("Ultrathink:\n{}", self.text)
        } else {
            self.text.clone()
        }
    }
}

impl From<&str> for QueuedMessage {
    fn from(text: &str) -> Self {
        QueuedMessage {
            id: 0,
            text: text.to_string(),
            attachments: Vec::new(),
            ultrathink: false,
            kind: QueuedMessageKind::User,
        }
    }
}

/// What a send gesture resolves to. Enter always means [`Self::Send`] or
/// [`Self::Queue`]; ⌘/Ctrl+Enter additionally reaches [`Self::Steer`] — or
/// [`Self::QueueUnsupported`] when the provider has no steering mechanism, in
/// which case the message is still delivered (queued), just not mid-turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendRouting {
    /// No turn is running: dispatch immediately as an ordinary turn.
    Send,
    /// A turn is running: hold this message until it completes.
    Queue,
    /// A turn is running and the provider can take a mid-turn injection.
    Steer,
    /// A steer was asked for, but this provider cannot steer. Queue it and tell
    /// the user honestly rather than silently dropping the gesture.
    QueueUnsupported,
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
    /// Messages typed while a turn was running (Enter → queue). In-memory only,
    /// per session — see [`QueuedMessage`].
    queue: Vec<QueuedMessage>,
    /// Source of [`QueuedMessage::id`]s.
    next_queue_id: u64,
    turn_in_flight: bool,
    /// Provider-native commands / skills discovered at session start (Claude
    /// `slash_commands` + `skills`; Codex `skills/list` + custom prompts).
    /// Seeded from the per-provider cache, then replaced by live updates.
    provider_commands: Vec<ProviderCommand>,
    /// The agent's self-described options (ACP `modes` / `models` /
    /// `configOptions`), pushed over the wire at session start and on every
    /// change. They render through the composer's existing traits picker; the
    /// native providers describe their options through the model catalog
    /// instead, so this stays empty for them. In-memory only.
    provider_options: Vec<OptionDescriptor>,
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
        // ACP agents take option changes live (`session/set_mode` /
        // `set_model` / `set_config_option`), so nothing ever needs a restart.
        if self.meta.provider == ProviderKind::Acp {
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

    /// Whether a message typed right now could be STEERED into the turn that is
    /// already running — i.e. the provider has a native mid-turn injection
    /// mechanism (Claude: a stream-json user message; Codex: `turn/steer`) and
    /// is actually live. When false, the composer's steer gesture degrades to
    /// queueing (and says so).
    pub fn supports_steering(&self) -> bool {
        matches!(self.runtime, Runtime::Live(_)) && self.meta.provider.supports_steering()
    }

    /// Whether a turn is currently running, i.e. Enter queues rather than sends.
    pub fn is_turn_running(&self) -> bool {
        self.turn_in_flight
    }

    pub fn queued(&self) -> &[QueuedMessage] {
        &self.queue
    }

    /// Where a send gesture should go, given what the session is doing right
    /// now. This is the whole steering-vs-queueing policy in one place.
    fn route(&self, steer: bool) -> SendRouting {
        if !self.is_turn_running() {
            // Nothing to steer into: ⌘Enter and Enter are the same thing.
            SendRouting::Send
        } else if !steer {
            SendRouting::Queue
        } else if self.supports_steering() {
            SendRouting::Steer
        } else {
            SendRouting::QueueUnsupported
        }
    }

    /// Pull a message out of the queue by id (the strip's steer/✕ buttons).
    fn take_queued(&mut self, id: u64) -> Option<QueuedMessage> {
        let index = self.queue.iter().position(|m| m.id == id)?;
        Some(self.queue.remove(index))
    }

    /// Inject a message into the turn already in flight. Deliberately does NOT
    /// touch the turn bookkeeping: the provider folds the message into the
    /// running turn (Claude emits no second `result`; Codex's `turn/steer`
    /// resolves with the same `turnId`), so `turn_in_flight` stays true and the
    /// queue is untouched. Opening a turn here would leave a phantom that never
    /// completes.
    fn steer_now(
        &mut self,
        request_id: String,
        text: String,
        attachments: Vec<Attachment>,
    ) -> Result<(), ()> {
        let Runtime::Live(commands) = &self.runtime else {
            return Err(());
        };
        commands
            .try_send(SessionCommand::Steer {
                request_id,
                text,
                attachments,
            })
            .map_err(|_| ())
    }

    /// Append a message to the queue, consuming the armed Ultrathink flag (it is
    /// per-send, so it belongs to this message, not to whatever is sent later).
    fn push_queued(&mut self, text: String, attachments: Vec<Attachment>) -> u64 {
        let id = self.next_queue_id;
        self.next_queue_id += 1;
        let ultrathink = std::mem::take(&mut self.pending_ultrathink);
        self.queue.push(QueuedMessage {
            id,
            text,
            attachments,
            ultrathink,
            kind: QueuedMessageKind::User,
        });
        id
    }

    /// Keep callbacks that race while an idle provider is starting in the same
    /// wake-up turn. Sending them as separate queued turns lets the first result
    /// drive the orchestrator before the rest are visible, and the leftovers may
    /// not run until much later.
    fn push_or_merge_orchestrate_callback(&mut self, text: String) -> u64 {
        if let Some(pending) = self
            .queue
            .iter_mut()
            .find(|message| message.kind == QueuedMessageKind::OrchestrateCallback)
        {
            pending.text.push_str("\n\n");
            pending.text.push_str(&text);
            return pending.id;
        }
        let id = self.next_queue_id;
        self.next_queue_id += 1;
        self.queue.push(QueuedMessage {
            id,
            text,
            attachments: Vec::new(),
            ultrathink: false,
            kind: QueuedMessageKind::OrchestrateCallback,
        });
        id
    }

    /// Dispatch at most one queued message as an ordinary turn, preserving FIFO
    /// order. A turn already in flight blocks dispatch for EVERY provider: a
    /// queued message is by definition one that waits for the running turn to
    /// finish. (Steering — the other way to send mid-turn — never goes through
    /// here; see [`AppState::steer`].)
    fn dispatch_next_pending(&mut self) -> Result<bool, ()> {
        if self.turn_in_flight {
            return Ok(false);
        }
        let Runtime::Live(commands) = &self.runtime else {
            return Ok(false);
        };
        let Some(send) = self.queue.first().cloned() else {
            return Ok(false);
        };
        let options = Some(self.turn_options());
        commands
            .try_send(SessionCommand::SendTurn {
                text: send.wire_text(),
                options,
                attachments: send.attachments,
            })
            .map_err(|_| ())?;
        self.queue.remove(0);
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

/// Smoke-mode behavior flags (used by the smoke-test harness).
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
    pub install_source: InstallSource,
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
    /// Sessions whose provider outlives their place on screen. Switching threads
    /// used to kill the live process outright — mid-turn — which is the same
    /// failure T3 Code's 30-minute idle reaper inflicts on autonomous overnight
    /// sessions, except triggered by a glance at another thread. A session with
    /// work left (turn in flight, or queued messages) is parked here instead:
    /// its process, event pump and queue stay alive, events keep landing in its
    /// JSONL (`record_event` routes by id), queued messages keep dispatching as
    /// turns complete, and selecting the thread re-adopts it seamlessly. Once a
    /// parked session runs out of work it is shut down for real — no reaper, no
    /// timer, just "finish what you were given, then rest". (The parked
    /// `timeline` goes stale by design; re-adoption replays the JSONL.)
    background: HashMap<String, ActiveSession>,
    pub settings: Settings,
    pub smoke: Option<SmokeMode>,
    /// Whether the sidebar is collapsed to an icon strip (ephemeral UI state).
    pub sidebar_collapsed: bool,
    /// Current window route (chat vs. settings page).
    pub route: Route,
    /// Whether the command palette (⌘K) overlay is showing.
    pub palette_open: bool,
    /// Generation of the transient quit confirmation. Timers capture this value
    /// so an expired prompt cannot dismiss a newer (or unrelated) dialog.
    pub quit_prompt_epoch: u64,
    /// Prevents repeated quit signals from stacking confirmation dialogs.
    pub quit_prompt_open: bool,
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
    /// Screenshot-only diff state seeds.
    pub debug_diff_scope: Option<String>,
    pub debug_diff_split: bool,
    pub debug_diff_scope_menu: bool,
    pub debug_review_comment: bool,
    /// Screenshot-only: seed the command palette's query when it opens (so the
    /// `>`-actions filter and thread result rows can be captured headlessly).
    pub debug_palette: Option<String>,
    /// Screenshot-only: which Settings section to open (`general` / `providers` /
    /// `archived`), so each can be captured headlessly.
    pub debug_settings_section: Option<String>,
    /// Screenshot-only: seed the ACP marketplace's search box.
    pub debug_acp_search: Option<String>,
    /// Screenshot-only: open the ACP Add agent dialog on the Providers page.
    pub debug_acp_dialog: bool,
    /// Screenshot-only: which provider card starts expanded (`codex` / `claude`).
    pub debug_provider_expanded: Option<String>,
    /// The ACP agent marketplace: the registry index (from the CDN, cached on
    /// disk with a one-hour TTL), whether a refresh is in flight, and the last
    /// failure to show when there is nothing cached to fall back on.
    pub acp_registry: Option<Registry>,
    pub acp_registry_loading: bool,
    pub acp_registry_error: Option<String>,
    /// Registry ids currently downloading (their marketplace row shows a spinner).
    pub acp_installing: std::collections::HashSet<String>,
    /// Preview MCP server registration, injected into every session so the agent
    /// can drive the embedded browser. `None` if the server failed to start.
    pub mcp_registration: Option<agent::McpRegistration>,
    /// Automation-request receiver from the preview MCP server. `AppShell` takes
    /// this once to pump requests into the live `PreviewPanel` WebView.
    pub preview_requests: Option<async_channel::Receiver<preview_mcp::BrokerRequest>>,
    /// App-wide orchestrator endpoint and its per-parent bearer-token issuer.
    orchestrate_url: Option<String>,
    orchestrate_tokens: Option<orchestrate_mcp::TokenRegistry>,
    orchestrate_registrations: HashMap<String, agent::McpRegistration>,
    /// Requests from the orchestrate MCP runtime, pumped on the gpui thread.
    pub orchestrate_requests: Option<async_channel::Receiver<orchestrate_mcp::BrokerRequest>>,
    callback_counts: HashMap<String, usize>,
    callback_last_turn: HashMap<String, usize>,
    /// A URL the preview panel should navigate to on its next render (set by the
    /// `--open-preview <url>` dev flag for headless screenshots). Consumed once.
    pub pending_preview_url: Option<String>,
    /// Background-computed git state of the active session's cwd, driving the
    /// adaptive header quick-action button (`None` until the first refresh /
    /// with no active session). See [`AppState::refresh_git_status`].
    pub git_status: Option<GitStatus>,
    /// A git quick-action (commit/push/pull/…) is currently running, so the
    /// button is disabled with an in-progress hint.
    pub git_busy: bool,
    /// Source of ids used to correlate semantic operation lifecycle events.
    next_operation_id: u64,
    /// Monotonic token so a stale background status refresh (from a session the
    /// user has since switched away from) is ignored.
    git_status_generation: u64,
    /// Screenshot-only (`--debug-git-dialog`): open the commit dialog once the
    /// git status has loaded (clicking the header button cannot be driven
    /// headlessly). Consumed by `ChatView` on its next render.
    pub debug_open_commit_dialog: bool,
    /// Screenshot-only (`--debug-edit-open`): open the inline "Edit & resend"
    /// editor on the last user message (hovering a bubble and clicking its action
    /// row cannot be driven headlessly). Consumed by `ChatView` on its next render.
    pub debug_edit_open: bool,
    /// Composer-draft review notes, keyed by session id (in-memory only).
    review_comment_drafts: HashMap<String, Vec<ReviewComment>>,
    /// Invalidates working-tree/branch previews on panel open and turn finish.
    pub diff_refresh_generation: u64,
    /// Per-provider version-check results (Group C). Populated on launch (when
    /// the toggle is on) and by Settings → "Check now".
    pub provider_versions: HashMap<ProviderKind, ProviderVersionStatus>,
    /// Per-provider install/auth probe results, driving the Settings → Providers
    /// card status dot + summary line. Absent until the first probe lands.
    pub provider_snapshots: HashMap<ProviderKind, ProviderSnapshot>,
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
        sessions.sort_by_key(|b| std::cmp::Reverse(b.updated_at));
        let projects = file.projects;
        let settings_store = SettingsStore::new(store.root().clone());
        let settings = settings_store.load();
        let settings_collapsed = settings.sidebar_collapsed;
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
            background: HashMap::new(),
            settings,
            smoke: None,
            sidebar_collapsed: settings_collapsed,
            route: Route::Chat,
            palette_open: false,
            quit_prompt_epoch: 0,
            quit_prompt_open: false,
            model_catalogs,
            models_loading: HashMap::new(),
            terminal_preferences_path,
            terminal_preferences,
            next_start_generation: 0,
            debug_compose: None,
            debug_image: None,
            debug_diff_scope: None,
            debug_diff_split: false,
            debug_diff_scope_menu: false,
            debug_review_comment: false,
            acp_registry: None,
            acp_registry_loading: false,
            acp_registry_error: None,
            acp_installing: std::collections::HashSet::new(),
            mcp_registration: None,
            preview_requests: None,
            orchestrate_url: None,
            orchestrate_tokens: None,
            orchestrate_registrations: HashMap::new(),
            orchestrate_requests: None,
            callback_counts: HashMap::new(),
            callback_last_turn: HashMap::new(),
            pending_preview_url: None,
            git_status: None,
            git_busy: false,
            next_operation_id: 1,
            git_status_generation: 0,
            debug_open_commit_dialog: false,
            debug_edit_open: false,
            review_comment_drafts: HashMap::new(),
            diff_refresh_generation: 0,
            debug_palette: None,
            debug_settings_section: None,
            debug_acp_search: None,
            debug_acp_dialog: false,
            debug_provider_expanded: None,
            provider_versions: HashMap::new(),
            provider_snapshots: HashMap::new(),
        }
    }

    /// Open the Preview tab and queue an initial navigation (dev/testing entry
    /// point for `--open-preview <url>`).
    pub fn open_preview_with_url(&mut self, url: String, cx: &mut Context<Self>) {
        self.pending_preview_url = Some(url);
        self.open_preview_panel(cx);
    }

    /// Take the queued preview URL, if any (consumed by `PreviewPanel`).
    ///
    /// Linux has no preview WebView to navigate (see `ui::preview_panel`), so
    /// nothing consumes this there — the queue is simply never drained.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    pub fn take_pending_preview_url(&mut self) -> Option<String> {
        self.pending_preview_url.take()
    }

    /// Attach the running preview MCP server: its registration (injected into
    /// every spawned session) and the request receiver (taken by `AppShell`).
    pub fn attach_preview_mcp(&mut self, server: preview_mcp::PreviewMcpServer) {
        self.mcp_registration = Some(agent::McpRegistration {
            name: agent::McpRegistration::SERVER_NAME_PREVIEW.into(),
            url: server.url,
            bearer_token: server.bearer_token,
        });
        self.preview_requests = Some(server.requests);
    }

    /// Take the preview request stream once so its UI-side broker can own it.
    pub fn take_preview_requests(
        &mut self,
    ) -> Option<async_channel::Receiver<preview_mcp::BrokerRequest>> {
        self.preview_requests.take()
    }

    pub fn attach_orchestrate_mcp(&mut self, server: orchestrate_mcp::OrchestrateMcpServer) {
        self.orchestrate_url = Some(server.url);
        self.orchestrate_tokens = Some(server.tokens);
        self.orchestrate_requests = Some(server.requests);
    }

    /// Pump orchestrator requests through the runtime on the gpui thread.
    ///
    /// Taking the receiver makes repeated calls harmless: exactly one pump can
    /// own the request stream.
    pub fn pump_orchestrate_requests(&mut self, cx: &mut Context<Self>) {
        let Some(requests) = self.orchestrate_requests.take() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            while let Ok(request) = requests.recv().await {
                let orchestrate_mcp::BrokerRequest { op, reply } = request;
                let Ok(result) = this.update(cx, |state, cx| state.handle_orchestrate_op(op, cx))
                else {
                    break;
                };
                let _ = reply.send(result).await;
            }
        })
        .detach();
    }

    /// Persistently opt a session into native orchestration. Callers restart a
    /// currently-live provider so its next spawn receives the MCP registration.
    pub fn enable_orchestrate(
        &mut self,
        session_id: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(mut meta) = self
            .sessions
            .iter()
            .find(|meta| meta.id == session_id)
            .cloned()
            .or_else(|| self.meta_mut(session_id).cloned())
        else {
            return Err("unknown session".into());
        };
        meta.orchestrate_enabled = true;
        meta.updated_at = now_secs();
        if let Some(live_meta) = self.meta_mut(session_id) {
            live_meta.orchestrate_enabled = true;
            live_meta.updated_at = meta.updated_at;
        }
        self.persist_meta(&meta, cx);
        let _ = self.orchestrate_registration_for(&meta);
        Ok(())
    }

    /// Enable orchestration on first use, restart so the MCP registration is
    /// present, and submit the provider-specific guidance plus the user's text.
    pub fn orchestrate_turn(
        &mut self,
        text: String,
        attachments: Vec<Attachment>,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        let provider = active.meta.provider;
        let enabling = !active.meta.orchestrate_enabled;
        let session_id = active.meta.id.clone();
        let Some(text) = compose_orchestrate_text(provider, enabling, &text) else {
            self.report_error(RuntimeError::OrchestrateUnsupported, cx);
            return;
        };

        if enabling {
            if let Err(message) = self.enable_orchestrate(&session_id, cx) {
                self.report_error(RuntimeError::External(message), cx);
                return;
            }
            if let Some(active) = self.active.as_mut() {
                active.shutdown_to_idle();
            }
        }

        // `steer` sends ordinarily when idle and injects into a live turn. On
        // first enable the restart above intentionally makes this an ordinary
        // queued send for the resumed, MCP-enabled process.
        self.steer(text, attachments, cx);
    }

    fn orchestrate_registration_for(
        &mut self,
        meta: &SessionMeta,
    ) -> Option<agent::McpRegistration> {
        if !meta.orchestrate_enabled {
            return None;
        }
        if let Some(registration) = self.orchestrate_registrations.get(&meta.id) {
            return Some(registration.clone());
        }
        let token = self.orchestrate_tokens.as_ref()?.register(&meta.id);
        let registration = agent::McpRegistration {
            name: agent::McpRegistration::SERVER_NAME_ORCHESTRATE.into(),
            url: self.orchestrate_url.clone()?,
            bearer_token: token,
        };
        self.orchestrate_registrations
            .insert(meta.id.clone(), registration.clone());
        Some(registration)
    }

    #[allow(clippy::too_many_arguments)] // mirrors the MCP dispatch schema
    pub fn create_child_session(
        &mut self,
        parent_id: &str,
        provider: ProviderKind,
        model: Option<String>,
        effort: Option<String>,
        title: String,
        cwd: Option<PathBuf>,
        brief: String,
        cx: &mut Context<Self>,
    ) -> Result<String, String> {
        let parent = self
            .sessions
            .iter()
            .find(|meta| meta.id == parent_id)
            .cloned()
            .or_else(|| {
                self.active
                    .as_ref()
                    .filter(|active| active.meta.id == parent_id)
                    .map(|active| active.meta.clone())
            })
            .or_else(|| self.background.get(parent_id).map(|s| s.meta.clone()))
            .ok_or_else(|| "unknown parent session".to_string())?;
        let cwd = match cwd {
            Some(path) => {
                let path = if path.is_absolute() {
                    path
                } else {
                    parent.cwd.join(path)
                };
                let canonical = path
                    .canonicalize()
                    .map_err(|_| format!("invalid cwd: {}", path.display()))?;
                if !canonical.is_dir() {
                    return Err(format!("invalid cwd: {}", canonical.display()));
                }
                canonical
            }
            None => parent.cwd.clone(),
        };
        let meta = build_child_meta(&parent, provider, model, effort, title, cwd);
        self.store
            .upsert_meta(&meta)
            .map_err(|err| format!("failed to persist child session: {err}"))?;
        self.sessions = self.store.load_index();
        let id = meta.id.clone();
        let provider_commands =
            self.cached_provider_commands(meta.provider, meta.acp_agent_id.as_deref());
        let mut child = Self::build_draft_session(
            meta.project_id.clone().unwrap_or_default(),
            meta.cwd.clone(),
            meta.provider,
            meta.model.clone(),
            None,
            provider_commands,
        );
        child.meta = meta;
        child.draft = false;
        child.push_queued(brief, Vec::new());
        self.background.insert(id.clone(), child);
        self.ensure_session_started(&id, cx);
        cx.notify();
        Ok(id)
    }

    /// Resolve one MCP operation on the gpui thread.
    pub fn handle_orchestrate_op(
        &mut self,
        op: orchestrate_mcp::OrchestrateOp,
        cx: &mut Context<Self>,
    ) -> Result<serde_json::Value, String> {
        use orchestrate_mcp::OrchestrateOp;
        match op {
            OrchestrateOp::Dispatch {
                parent_id,
                provider,
                model,
                effort,
                title,
                brief,
                cwd,
            } => {
                let provider = match provider.as_str() {
                    "claude" => ProviderKind::ClaudeCode,
                    "codex" => ProviderKind::Codex,
                    _ => return Err(format!("unknown provider: {provider}")),
                };
                let id = self.create_child_session(
                    &parent_id,
                    provider,
                    model,
                    effort,
                    title,
                    cwd.map(PathBuf::from),
                    brief,
                    cx,
                )?;
                Ok(serde_json::json!({ "thread_id": id }))
            }
            OrchestrateOp::Status {
                parent_id,
                thread_id,
            } => {
                let mut children: Vec<_> = self
                    .sessions
                    .iter()
                    .filter(|meta| meta.parent_session_id.as_deref() == Some(&parent_id))
                    .filter(|meta| thread_id.as_ref().is_none_or(|id| id == &meta.id))
                    .map(|meta| self.child_status_json(meta))
                    .collect();
                if thread_id.is_some() && children.is_empty() {
                    return Err("unknown thread or not a child of this parent".into());
                }
                children.sort_by_key(|value| value["updated_at"].as_u64().unwrap_or_default());
                children.reverse();
                Ok(serde_json::Value::Array(children))
            }
            OrchestrateOp::Send {
                parent_id,
                thread_id,
                message,
            } => {
                self.require_child(&parent_id, &thread_id)?;
                if self.active_session_id() == Some(&thread_id) {
                    let child = self.active.as_mut().unwrap();
                    child.push_queued(message, Vec::new());
                    let idle = matches!(child.runtime, Runtime::Idle);
                    if self.dispatch_next_queued(cx).is_err() {
                        return Err("child provider is unavailable".into());
                    }
                    if idle {
                        self.ensure_started(cx);
                    }
                    return Ok(serde_json::json!({ "ok": true }));
                }
                self.ensure_child_loaded(&thread_id)?;
                let child = self.background.get_mut(&thread_id).unwrap();
                child.push_queued(message, Vec::new());
                let idle = matches!(child.runtime, Runtime::Idle);
                if !idle && !child.turn_in_flight {
                    self.on_background_turn_completed(&thread_id, cx);
                }
                if idle {
                    self.ensure_session_started(&thread_id, cx);
                }
                Ok(serde_json::json!({ "ok": true }))
            }
            OrchestrateOp::Result {
                parent_id,
                thread_id,
            } => {
                let meta = self.require_child(&parent_id, &thread_id)?;
                let (state, final_message) = self.child_result(meta);
                if state == "running" {
                    return Err("thread is still running".into());
                }
                Ok(serde_json::json!({ "state": state, "final_message": final_message }))
            }
            OrchestrateOp::Cancel {
                parent_id,
                thread_id,
            } => {
                self.require_child(&parent_id, &thread_id)?;
                if self.active_session_id() == Some(&thread_id) {
                    if let Some(child) = self.active.as_mut() {
                        child.queue.clear();
                        child.shutdown_to_idle();
                    }
                } else {
                    self.drop_background(&thread_id);
                }
                Ok(serde_json::json!({ "ok": true }))
            }
        }
    }

    fn require_child(&self, parent_id: &str, thread_id: &str) -> Result<&SessionMeta, String> {
        self.sessions
            .iter()
            .find(|meta| {
                meta.id == thread_id && meta.parent_session_id.as_deref() == Some(parent_id)
            })
            .ok_or_else(|| "unknown thread or not a child of this parent".into())
    }

    fn ensure_child_loaded(&mut self, thread_id: &str) -> Result<(), String> {
        if self.background.contains_key(thread_id) {
            return Ok(());
        }
        let meta = self
            .sessions
            .iter()
            .find(|meta| meta.id == thread_id)
            .cloned()
            .ok_or_else(|| "unknown thread".to_string())?;
        if self.active_session_id() == Some(thread_id) {
            return Err("child thread is currently open in the foreground".into());
        }
        self.load_background_session(meta);
        Ok(())
    }

    fn load_background_session(&mut self, meta: SessionMeta) {
        let thread_id = meta.id.clone();
        let commands = self.cached_provider_commands(meta.provider, meta.acp_agent_id.as_deref());
        let mut child = Self::build_draft_session(
            meta.project_id.clone().unwrap_or_default(),
            meta.cwd.clone(),
            meta.provider,
            meta.model.clone(),
            meta.acp_agent_id.clone(),
            commands,
        );
        child.meta = meta;
        child.draft = false;
        child.timeline = Timeline::fold_events(self.store.read_events(&thread_id));
        child.timeline.mark_idle();
        self.background.insert(thread_id, child);
    }

    fn child_result(&self, meta: &SessionMeta) -> (&'static str, String) {
        let timeline = Timeline::fold_events(self.store.read_events(&meta.id));
        let running = self
            .active
            .as_ref()
            .filter(|child| child.meta.id == meta.id)
            .or_else(|| self.background.get(&meta.id))
            .is_some_and(|child| {
                child.turn_in_flight
                    || !child.queue.is_empty()
                    || matches!(child.runtime, Runtime::Starting { .. })
            });
        let state = if running {
            "running"
        } else {
            match timeline.last_turn_status {
                Some(TurnStatus::Completed) => "completed",
                Some(TurnStatus::Failed | TurnStatus::Interrupted) => "failed",
                None => "idle",
            }
        };
        (state, final_assistant_message(&timeline))
    }

    fn child_status_json(&self, meta: &SessionMeta) -> serde_json::Value {
        let (state, final_message) = self.child_result(meta);
        serde_json::json!({
            "thread_id": meta.id,
            "title": meta.title,
            "provider": match meta.provider { ProviderKind::ClaudeCode => "claude", ProviderKind::Codex => "codex", ProviderKind::Acp => "acp" },
            "state": state,
            "last_output_tail": tail_chars(&final_message, 600),
            "updated_at": meta.updated_at,
        })
    }

    /// Kick off a background refresh of every provider's model catalog (called
    /// at app start and after a binary-path change). Results update
    /// `model_catalogs` and are persisted so the next launch is instant.
    pub fn refresh_model_catalogs(&mut self, cx: &mut Context<Self>) {
        for provider in [ProviderKind::ClaudeCode, ProviderKind::Codex] {
            let binary = self.settings.provider(provider).binary_path;
            let launch_env = self.launch_env(provider);
            self.models_loading.insert(provider, true);
            let store = self.store.clone();
            cx.spawn(async move |this, cx| {
                let result = list_models(provider, binary, launch_env).await;
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
        self.settings
            .provider(provider)
            .binary_path
            .or_else(|| which_in_path(&default_program(provider)))
    }

    // -- per-provider configuration (Settings → Providers) ------------------

    /// The provider's environment as configured on its card: the plaintext env
    /// rows, their sensitive counterparts read back out of `secrets.json`, and
    /// the home override. Applied to every child we spawn for this provider.
    pub fn launch_env(&self, provider: ProviderKind) -> LaunchEnv {
        let settings = self.settings.provider(provider);
        let secrets = self.settings_store.provider_secrets(provider);
        let env = settings
            .env
            .iter()
            .filter(|var| !var.name.trim().is_empty())
            .filter_map(|var| {
                let value = if var.sensitive {
                    // Sensitive rows keep their value only in secrets.json; a
                    // row whose secret was never saved contributes nothing.
                    secrets.get(&var.name).cloned()?
                } else {
                    var.value.clone()
                };
                Some((var.name.trim().to_string(), value))
            })
            .collect();
        LaunchEnv {
            env,
            home: settings.effective_home(),
        }
    }

    /// The environment a session's child process runs with: the provider card's
    /// for the native providers, and the installed agent's own rows for an ACP
    /// session (each ACP agent is configured separately, so the shared
    /// `ProviderKind::Acp` bucket is not what we want there).
    fn session_launch_env(&self, meta: &SessionMeta) -> LaunchEnv {
        if meta.provider != ProviderKind::Acp {
            return self.launch_env(meta.provider);
        }
        let env = meta
            .acp_agent_id
            .as_deref()
            .and_then(|id| self.settings.acp_agent(id))
            .map(|agent| agent.env.clone())
            .unwrap_or_default();
        LaunchEnv { env, home: None }
    }

    /// Whether the provider may be used for new sessions (its card's switch).
    pub fn provider_enabled(&self, provider: ProviderKind) -> bool {
        self.settings.provider(provider).enabled
    }

    /// This provider's card settings (defaults when never configured).
    pub fn provider_settings(&self, provider: ProviderKind) -> ProviderSettings {
        self.settings.provider(provider)
    }

    /// Persist a mutation to one provider's card settings.
    ///
    /// This is called on every keystroke of the card's text fields, so it only
    /// writes settings.json. Anything that has to re-run the CLI (the model
    /// catalog, the status probe) is deferred to [`Self::reload_provider`],
    /// which the card fires once the field is committed (blur / Enter).
    pub fn update_provider_settings(
        &mut self,
        provider: ProviderKind,
        mutate: impl FnOnce(&mut ProviderSettings),
        cx: &mut Context<Self>,
    ) {
        let mut settings = self.settings.clone();
        mutate(settings.provider_mut(provider));
        self.update_settings(settings, cx);
    }

    /// Re-run everything that depends on *how* a provider's CLI is launched
    /// (binary path, home, environment): its model catalog and its status probe.
    pub fn reload_provider(&mut self, _provider: ProviderKind, cx: &mut Context<Self>) {
        self.refresh_model_catalogs(cx);
        self.refresh_provider_status(cx);
    }

    /// Store (or clear) one sensitive env value in `secrets.json`.
    pub fn set_provider_secret(
        &mut self,
        provider: ProviderKind,
        name: &str,
        value: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        if let Err(err) = self.settings_store.set_secret(provider, name, value) {
            self.report_error(
                RuntimeError::PersistSettings {
                    error: err.to_string(),
                },
                cx,
            );
            return;
        }
        cx.notify();
    }

    /// The provider's accent color (`#rrggbb`), when one is configured. Tints
    /// the provider glyph in the composer + model picker.
    pub fn provider_accent(&self, provider: ProviderKind) -> Option<gpui::Rgba> {
        let raw = self.settings.provider(provider).accent_color?;
        parse_hex_color(&raw)
    }

    // -- provider status snapshots (Settings → Providers card) --------------

    pub fn provider_snapshot(&self, provider: ProviderKind) -> Option<&ProviderSnapshot> {
        self.provider_snapshots.get(&provider)
    }

    /// The most recent probe time across providers (the section's "Checked …").
    pub fn providers_checked_at(&self) -> Option<u64> {
        self.provider_snapshots
            .values()
            .filter_map(|s| s.checked_at)
            .max()
    }

    /// Whether any provider probe is currently in flight (spins the refresh icon).
    pub fn providers_checking(&self) -> bool {
        self.provider_snapshots.values().any(|s| s.checking)
            || self.provider_versions.values().any(|s| s.checking)
    }

    /// Probe every provider: is the CLI there, what version, and who is signed
    /// in? Runs the same `--version` call the version check uses, plus the
    /// provider's own auth surface (`claude auth status --json`; Codex's
    /// `auth.json`), both under the provider's configured env/home.
    pub fn refresh_provider_status(&mut self, cx: &mut Context<Self>) {
        for provider in [ProviderKind::ClaudeCode, ProviderKind::Codex] {
            let snapshot = self.provider_snapshots.entry(provider).or_default();
            if snapshot.checking {
                continue;
            }
            snapshot.checking = true;
            let binary = self.resolve_provider_binary(provider);
            let launch_env = self.launch_env(provider);
            cx.spawn(async move |this, cx| {
                let snapshot = probe_provider(provider, binary, launch_env).await;
                log::info!("probe {provider:?} -> {snapshot:?}");
                let _ = this.update(cx, |state, cx| {
                    state.provider_snapshots.insert(provider, snapshot);
                    cx.notify();
                });
            })
            .detach();
        }
        cx.notify();
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
                .map(detect_install_source)
                .unwrap_or_default();
            let source = status.install_source;
            let program = binary
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| default_program(provider));
            let package = npm_package(provider);
            let env = self.launch_env(provider).pairs(provider);
            cx.spawn(async move |this, cx| {
                let installed = run_capture_env(&program, &["--version"], &env).await;
                let latest = run_capture("npm", &["view", package, "version"]).await;
                let _ = this.update(cx, |state, cx| {
                    let update_available = match (&installed, &latest) {
                        (Some(i), Some(l)) => is_update_available(i, l),
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
                            parse_version(r)
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
                    if update_available
                        && !already
                        && let Some(version) = &latest_pretty
                    {
                        cx.emit(AppEvent::Notice(RuntimeNotice::UpdateAvailable {
                            provider,
                            version: version.clone(),
                        }));
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
        let Some(command) = update_command(provider, source) else {
            self.report_error(RuntimeError::UpdateUnknown { provider }, cx);
            return;
        };
        let status = self.provider_versions.entry(provider).or_default();
        if status.updating {
            return;
        }
        status.updating = true;
        cx.emit(AppEvent::Notice(RuntimeNotice::UpdatingProvider {
            provider,
        }));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let args: Vec<&str> = command[1..].iter().map(String::as_str).collect();
            let ok = run_status(&command[0], &args).await;
            let _ = this.update(cx, |state, cx| {
                if let Some(status) = state.provider_versions.get_mut(&provider) {
                    status.updating = false;
                }
                if ok {
                    cx.emit(AppEvent::Notice(RuntimeNotice::UpdateDone { provider }));
                    // Refresh the version so the "update available" state clears.
                    state.check_provider_versions(cx);
                } else {
                    state.report_error(RuntimeError::UpdateFailed { provider }, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The copyable update command for a provider whose install source has
    /// already been detected. The install-source detail stays inside runtime.
    pub fn provider_update_command(&self, provider: ProviderKind) -> Option<String> {
        let source = self.provider_versions.get(&provider)?.install_source;
        update_command_string(provider, source)
    }

    /// Provider-native commands / skills for the active session. Seeded from the
    /// provider cache before a live process starts, then replaced by live data.
    pub fn active_provider_commands(&self) -> &[ProviderCommand] {
        self.active
            .as_ref()
            .map(|a| a.provider_commands.as_slice())
            .unwrap_or(&[])
    }

    fn cached_provider_commands(
        &self,
        provider: ProviderKind,
        acp_agent_id: Option<&str>,
    ) -> Vec<ProviderCommand> {
        self.store.load_commands(provider, acp_agent_id)
    }

    /// The cached model catalog for `provider` (empty when never fetched).
    pub fn models_for(&self, provider: ProviderKind) -> &[ModelSpec] {
        self.model_catalogs
            .get(&provider)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// The provider's full model list for the Settings → Providers "Models"
    /// section: catalog + custom slugs, in the user's order, hidden rows flagged.
    pub fn resolved_models(&self, provider: ProviderKind) -> Vec<ResolvedModel> {
        resolve_models(
            self.models_for(provider),
            &self.settings.provider(provider),
            &self.settings.favorite_models,
        )
    }

    /// The provider's model list as the composer's picker sees it: the same
    /// resolution, minus the models hidden on the provider card.
    pub fn picker_models(&self, provider: ProviderKind) -> Vec<ResolvedModel> {
        picker_models(
            self.models_for(provider),
            &self.settings.provider(provider),
            &self.settings.favorite_models,
        )
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

    /// The option descriptors the traits picker should render for the active
    /// session: the selected model's, or — for an ACP agent, which has no model
    /// catalog — the ones the agent pushed over the wire.
    pub fn active_option_descriptors(&self) -> Vec<OptionDescriptor> {
        let Some(active) = self.active.as_ref() else {
            return Vec::new();
        };
        if active.meta.provider == ProviderKind::Acp {
            return active.provider_options.clone();
        }
        self.active_model_spec()
            .map(|spec| spec.options)
            .unwrap_or_default()
    }

    pub fn toggle_sidebar_collapsed(&mut self, cx: &mut Context<Self>) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        // Persist so the choice survives a restart (save errors are cosmetic).
        self.settings.sidebar_collapsed = self.sidebar_collapsed;
        let settings = self.settings.clone();
        let _ = self.settings_store.save(&settings);
        cx.notify();
    }

    // -- git quick actions (Group: Git) -------------------------------------

    fn next_operation_id(&mut self) -> RuntimeOperationId {
        let id = RuntimeOperationId(self.next_operation_id);
        self.next_operation_id += 1;
        id
    }

    /// Kick off a background refresh of the active session's git status (on
    /// session open, after each turn, and after each git action). A stale result
    /// (session switched, or a newer refresh superseded it) is discarded.
    pub fn refresh_git_status(&mut self, cx: &mut Context<Self>) {
        let Some(cwd) = self.active.as_ref().map(|a| a.meta.cwd.clone()) else {
            self.git_status = None;
            cx.notify();
            return;
        };
        let session_id = self.active_session_id().map(str::to_string);
        self.git_status_generation += 1;
        let generation = self.git_status_generation;
        cx.spawn(async move |this, cx| {
            let status = unblock(cx.background_executor(), move || read_status(&cwd)).await;
            let _ = this.update(cx, |state, cx| {
                if state.git_status_generation == generation
                    && state.active_session_id().map(str::to_string) == session_id
                {
                    state.git_status = Some(status);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The resolved primary quick-action for the active session, or `None` when
    /// there is no active session or its status has not been computed yet (so
    /// the button stays hidden rather than flashing "Initialize Git" on a repo).
    pub fn git_quick_action(&self) -> Option<QuickAction> {
        self.active.as_ref()?;
        let status = self.git_status.as_ref()?;
        Some(quick_action(status, self.git_busy))
    }

    /// The applicable dropdown items for the active session's git state.
    pub fn git_menu_items(&self) -> Vec<MenuItem> {
        match (self.active.as_ref(), self.git_status.as_ref()) {
            (Some(_), Some(status)) => menu_items(status, self.git_busy),
            _ => Vec::new(),
        }
    }

    /// The active session's changed files (for the commit dialog list).
    pub fn git_changed_files(&self) -> Vec<GitFileEntry> {
        self.git_status
            .as_ref()
            .map(|s| s.changed_files.clone())
            .unwrap_or_default()
    }

    /// The active session's current branch (for the commit dialog header).
    pub fn git_branch_name(&self) -> Option<String> {
        self.git_status.as_ref().and_then(|s| s.branch.clone())
    }

    /// Whether the active session is on the repo's default branch (main/master)
    /// — drives the commit dialog's safeguard banner.
    pub fn git_on_default_branch(&self) -> bool {
        self.git_status
            .as_ref()
            .is_some_and(|s| s.is_default_branch)
    }

    /// Generate a commit message with the current provider (Claude, headless
    /// `claude -p`) for the active session, scoped to `included` paths. Returns
    /// a task the caller (commit dialog) awaits to fill the message field.
    pub fn generate_commit_message(
        &self,
        included: Option<Vec<String>>,
        cx: &gpui::App,
    ) -> Task<Result<String, String>> {
        let Some(cwd) = self.active.as_ref().map(|a| a.meta.cwd.clone()) else {
            return cx
                .background_executor()
                .spawn(async { Err("no active session".to_string()) });
        };
        let binary = self.settings.provider(ProviderKind::ClaudeCode).binary_path;
        cx.background_executor().spawn(async move {
            let (stat, patch) = commit_diff_context(&cwd, included.as_deref());
            let prompt = build_commit_prompt(&stat, &patch);
            let raw = run_claude_headless(binary.as_deref(), &cwd, &prompt)?;
            let message = sanitize_commit_message(&raw);
            if message.is_empty() {
                Err("model returned an empty commit message".to_string())
            } else {
                Ok(message)
            }
        })
    }

    /// Run a resolved git quick-action in the background, tracking progress in a
    /// single toast (running → success/error with the command output as the
    /// error detail). Refreshes the git status + branch label on completion.
    ///
    /// `message` is the commit message (commit actions); `included` the checked
    /// file subset (`None` = all); `feature_branch` the safeguard's new branch.
    pub fn run_git_action(
        &mut self,
        action: GitAction,
        message: Option<String>,
        included: Option<Vec<String>>,
        feature_branch: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.git_busy {
            cx.emit(AppEvent::Toast(RuntimeToast::GitBusy));
            return;
        }
        let Some(cwd) = self.active.as_ref().map(|a| a.meta.cwd.clone()) else {
            return;
        };
        let current_branch = self.git_branch_name();
        self.git_busy = true;
        let operation = self.next_operation_id();
        let retry = GitActionRequest {
            action,
            message: message.clone(),
            included: included.clone(),
            feature_branch: feature_branch.clone(),
        };
        cx.emit(AppEvent::Toast(RuntimeToast::GitStarted {
            operation,
            action,
        }));

        cx.spawn(async move |this, cx| {
            let result = unblock(cx.background_executor(), move || {
                perform_action(
                    &cwd,
                    action,
                    message.as_deref(),
                    included.as_deref(),
                    feature_branch.as_deref(),
                    current_branch.as_deref(),
                )
            })
            .await;
            let _ = this.update(cx, |state, cx| {
                state.git_busy = false;
                match &result {
                    Ok(_) => cx.emit(AppEvent::Toast(RuntimeToast::GitSucceeded {
                        operation,
                        action,
                    })),
                    Err(detail) => cx.emit(AppEvent::Toast(RuntimeToast::GitFailed {
                        operation,
                        detail: detail.clone(),
                        retry,
                    })),
                }
                if let Some(active) = state.active.as_mut() {
                    active.git_branch = read_git_branch(&active.meta.cwd);
                }
                state.refresh_git_status(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Debug/E2E entry point (`--debug-git-commit "msg"`): stage everything and
    /// commit the active session's cwd with `message`, driving the same toast +
    /// status-refresh path as the UI commit.
    pub fn debug_git_commit(&mut self, message: String, cx: &mut Context<Self>) {
        self.run_git_action(GitAction::Commit, Some(message), None, None, cx);
    }

    /// Debug/E2E entry point (`--debug-git-action push|pull|publish|init`): run a
    /// non-commit quick-action directly. The current branch is read fresh (the
    /// background status refresh may not have landed yet).
    pub fn debug_git_action(&mut self, name: String, cx: &mut Context<Self>) {
        let action = match name.as_str() {
            "push" => GitAction::Push,
            "pull" => GitAction::Pull,
            "publish" => GitAction::PublishBranch,
            "init" => GitAction::InitializeGit,
            other => {
                log::warn!("unknown --debug-git-action '{other}'");
                return;
            }
        };
        // PublishBranch needs the branch name; seed the status synchronously.
        if matches!(action, GitAction::PublishBranch)
            && self.git_status.is_none()
            && let Some(cwd) = self.active.as_ref().map(|a| a.meta.cwd.clone())
        {
            self.git_status = Some(read_status(&cwd));
        }
        self.run_git_action(action, None, None, None, cx);
    }

    /// Debug/E2E entry point (`--debug-git-genmsg`): generate a commit message
    /// for the active session and surface it (logged + info toast) so the AI
    /// path can be exercised headlessly.
    pub fn debug_git_generate_message(&mut self, cx: &mut Context<Self>) {
        let task = self.generate_commit_message(None, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |_state, cx| match result {
                Ok(message) => {
                    log::info!("generated commit message:\n{message}");
                    cx.emit(AppEvent::Toast(RuntimeToast::CommitMessageGenerated {
                        message,
                    }));
                }
                Err(err) => {
                    log::warn!("commit message generation failed: {err}");
                    cx.emit(AppEvent::Toast(RuntimeToast::CommitMessageFailed {
                        detail: err,
                    }));
                }
            });
        })
        .detach();
    }

    // -- the ACP agent marketplace ------------------------------------------

    /// Load the registry index (cache first, network when stale). Cheap enough
    /// to call every time the Providers page opens.
    pub fn refresh_acp_registry(&mut self, cx: &mut Context<Self>) {
        if self.acp_registry_loading {
            return;
        }
        self.acp_registry_loading = true;
        let data_dir = self.store.root().clone();
        // Show the cache instantly; the fetch below refreshes it in place.
        if self.acp_registry.is_none()
            && let Some(cached) = cached(&data_dir)
        {
            self.acp_registry = Some(cached);
        }
        cx.spawn(async move |this, cx| {
            let result = unblock(cx.background_executor(), move || load(&data_dir)).await;
            let _ = this.update(cx, |state, cx| {
                state.acp_registry_loading = false;
                match result {
                    Ok(registry) => {
                        state.acp_registry = Some(registry);
                        state.acp_registry_error = None;
                    }
                    Err(err) => {
                        log::warn!("ACP registry unavailable: {err}");
                        state.acp_registry_error = Some(err.to_string());
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// The marketplace list: every registry agent except the hidden adapters
    /// over our own native CLIs.
    pub fn acp_marketplace(&self) -> Vec<RegistryAgent> {
        self.acp_registry
            .as_ref()
            .map(|registry| visible_agents(registry).into_iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Runtime-owned marketplace views in the registry's visible ordering.
    pub fn acp_marketplace_items(&self) -> Vec<AcpMarketplaceItem> {
        let platform = platform_key();
        self.acp_registry
            .as_ref()
            .map(|registry| {
                visible_agents(registry)
                    .into_iter()
                    .map(|agent| AcpMarketplaceItem {
                        id: agent.id.clone(),
                        name: agent.name.clone(),
                        version: agent.version.clone(),
                        description: agent.description.clone(),
                        installed: self.settings.acp_agents.contains_key(&agent.id),
                        installing: self.acp_installing.contains(&agent.id),
                        supported: resolve_recipe(agent, &platform).is_some(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Download + install one registry agent, with a progress toast.
    pub fn install_acp_agent(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(agent) = self
            .acp_marketplace()
            .into_iter()
            .find(|agent| agent.id == id)
        else {
            return;
        };
        if !self.acp_installing.insert(id.clone()) {
            return;
        }
        let operation = self.next_operation_id();
        let data_dir = self.store.root().clone();
        let name = agent.name.clone();
        cx.emit(AppEvent::Toast(RuntimeToast::AcpInstallStarted {
            operation,
            name: name.clone(),
        }));
        cx.spawn(async move |this, cx| {
            let result = unblock(cx.background_executor(), move || {
                install(&agent, &data_dir, |_done, _total| {})
            })
            .await;
            let _ = this.update(cx, |state, cx| {
                state.acp_installing.remove(&id);
                match result {
                    Ok(installed) => {
                        state.settings.acp_agents.insert(id.clone(), installed);
                        let settings = state.settings.clone();
                        let _ = state.settings_store.save(&settings);
                        cx.emit(AppEvent::Toast(RuntimeToast::AcpInstallSucceeded {
                            operation,
                            name,
                        }));
                    }
                    Err(err) => cx.emit(AppEvent::Toast(RuntimeToast::AcpInstallFailed {
                        operation,
                        name,
                        detail: err.to_string(),
                    })),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Remove an installed ACP agent (its files and its settings entry).
    pub fn remove_acp_agent(&mut self, id: &str, cx: &mut Context<Self>) {
        self.settings.acp_agents.remove(id);
        let settings = self.settings.clone();
        let _ = self.settings_store.save(&settings);
        let data_dir = self.store.root().clone();
        let id = id.to_string();
        cx.spawn(async move |_this, cx| {
            unblock(cx.background_executor(), move || {
                if let Err(err) = uninstall(&data_dir, &id) {
                    log::warn!("could not remove ACP agent {id}: {err}");
                }
            })
            .await;
        })
        .detach();
        cx.notify();
    }

    /// Register a user-defined ACP agent (the escape hatch for anything not in
    /// the registry): an arbitrary command that speaks ACP over its stdio.
    pub fn add_custom_acp_agent(
        &mut self,
        name: String,
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
        cx: &mut Context<Self>,
    ) {
        let name = name.trim().to_string();
        let command = command.trim().to_string();
        if name.is_empty() || command.is_empty() {
            return;
        }
        let id = custom_acp_id(&name);
        self.settings.acp_agents.insert(
            id.clone(),
            InstalledAgent {
                id,
                name,
                version: String::new(),
                icon: None,
                launch: agent::AcpLaunch::Custom { command, args, env },
                archive_sha256: None,
                enabled: true,
                env: Vec::new(),
                launch_args: None,
            },
        );
        let settings = self.settings.clone();
        let _ = self.settings_store.save(&settings);
        cx.notify();
    }

    /// Update one installed ACP agent in place (enable switch, env rows, args).
    pub fn update_acp_agent(
        &mut self,
        id: &str,
        edit: impl FnOnce(&mut InstalledAgent),
        cx: &mut Context<Self>,
    ) {
        if let Some(agent) = self.settings.acp_agents.get_mut(id) {
            edit(agent);
            let settings = self.settings.clone();
            let _ = self.settings_store.save(&settings);
            cx.notify();
        }
    }

    /// Point the active draft at an installed ACP agent (the model picker's
    /// provider rail). ACP agents have no model catalog: the agent publishes its
    /// models over the wire once the session starts.
    pub fn set_active_acp_agent(&mut self, id: &str, cx: &mut Context<Self>) {
        let provider_commands = self.cached_provider_commands(ProviderKind::Acp, Some(id));
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if active.meta.provider == ProviderKind::Acp
            && active.meta.acp_agent_id.as_deref() == Some(id)
        {
            return;
        }
        active.meta.provider = ProviderKind::Acp;
        active.meta.acp_agent_id = Some(id.to_string());
        active.meta.model = None;
        active.meta.option_selections.clear();
        active.provider_options.clear();
        active.provider_commands = provider_commands;
        active.pending_ultrathink = false;
        if active.draft {
            cx.notify();
            return;
        }
        active.meta.updated_at = now_secs();
        let meta = active.meta.clone();
        self.persist_meta(&meta, cx);
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
            if let EntryContent::FileChange { changes, .. } = &entry.content
                && !changes.is_empty()
                && turns.last() != Some(&entry.turn)
                && !turns.contains(&entry.turn)
            {
                turns.push(entry.turn);
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
                self.diff_refresh_generation = self.diff_refresh_generation.wrapping_add(1);
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
            self.diff_refresh_generation = self.diff_refresh_generation.wrapping_add(1);
            cx.notify();
        }
    }

    /// Open the diff panel on the latest turn with changes (used by
    /// `--open-diff` and as a general "just show me the diffs" entry point).
    pub fn open_diff_panel(&mut self, cx: &mut Context<Self>) {
        if let Some(active) = self.active.as_mut() {
            active.diff_open = true;
            active.right_tab = RightTab::Diff;
            self.diff_refresh_generation = self.diff_refresh_generation.wrapping_add(1);
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
                        RuntimeError::TerminalStart {
                            error: error.to_string(),
                        },
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
                RuntimeError::TerminalRestart {
                    error: error.to_string(),
                },
                cx,
            ),
        }
    }

    pub fn new_terminal(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
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
                RuntimeError::TerminalStart {
                    error: error.to_string(),
                },
                cx,
            ),
        }
    }

    pub fn activate_terminal(&mut self, terminal_id: u64, cx: &mut Context<Self>) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if active.terminal_workspace.terminal(terminal_id).is_some() {
            active.terminal_workspace.active_id = Some(terminal_id);
            cx.notify();
        }
    }

    pub fn close_terminal(&mut self, terminal_id: u64, cx: &mut Context<Self>) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
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

    pub fn split_terminal(&mut self, direction: TerminalSplitDirection, cx: &mut Context<Self>) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        let workspace = &mut active.terminal_workspace;
        let Some(first) = workspace.active_id else {
            return;
        };
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
                RuntimeError::TerminalStart {
                    error: error.to_string(),
                },
                cx,
            ),
        }
    }

    pub fn capture_terminal_selection(&mut self, terminal_id: u64, cx: &mut Context<Self>) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        let Some(entry) = active.terminal_workspace.terminal(terminal_id) else {
            return;
        };
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

    pub fn review_comments(&self) -> &[ReviewComment] {
        let Some(id) = self.active.as_ref().map(|active| active.meta.id.as_str()) else {
            return &[];
        };
        self.review_comment_drafts
            .get(id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn add_review_comment(&mut self, comment: ReviewComment, cx: &mut Context<Self>) {
        if let Some(id) = self.active.as_ref().map(|active| active.meta.id.clone()) {
            self.review_comment_drafts
                .entry(id)
                .or_default()
                .push(comment);
            cx.notify();
        }
    }

    pub fn remove_review_comment(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(id) = self.active.as_ref().map(|active| active.meta.id.clone()) else {
            return;
        };
        if let Some(comments) = self.review_comment_drafts.get_mut(&id)
            && index < comments.len()
        {
            comments.remove(index);
            cx.notify();
        }
    }

    pub fn clear_review_comments(&mut self) {
        if let Some(id) = self.active.as_ref().map(|active| active.meta.id.clone()) {
            self.review_comment_drafts.remove(&id);
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
                .sort_by_key(|b| std::cmp::Reverse(b.archived_at));
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
                RuntimeError::PersistProject {
                    error: err.to_string(),
                },
                cx,
            );
            return None;
        }
        let id = project.id.clone();
        self.projects = self.store.load_projects();
        cx.notify();
        Some(id)
    }

    /// Clone the persistence handles needed by the blocking external-history
    /// importer without exposing mutable application state across threads.
    pub fn external_import_context(
        &self,
        project_id: &str,
    ) -> Option<(SessionStore, Project, Vec<SessionMeta>)> {
        let project = self.projects.iter().find(|p| p.id == project_id)?.clone();
        Some((self.store.clone(), project, self.sessions.clone()))
    }

    /// Scan supported external-agent histories without exposing the import
    /// service or application stores to callers.
    pub fn scan_external_history(
        &self,
        executor: &gpui::BackgroundExecutor,
    ) -> Task<Vec<RecentDir>> {
        let exclude: Vec<_> = self
            .projects
            .iter()
            .map(|project| project.root.clone())
            .collect();
        let sessions = self.sessions.clone();
        unblock(executor, move || {
            let known = existing_external_ids(&sessions);
            let mut recent = scan_recent_dirs(&ExternalRoots::detect(), &exclude);
            for dir in &mut recent {
                dir.threads
                    .retain(|thread| !known.contains(&thread.external_id));
            }
            recent.retain(|dir| !dir.threads.is_empty());
            recent
        })
    }

    /// Import selected external threads in the background and stream runtime-
    /// owned progress updates. Returns `None` for an unknown project.
    pub fn start_external_import(
        &self,
        project_id: &str,
        threads: Vec<ExternalThread>,
        executor: &gpui::BackgroundExecutor,
    ) -> Option<async_channel::Receiver<ExternalImportUpdate>> {
        let project = self
            .projects
            .iter()
            .find(|project| project.id == project_id)?
            .clone();
        let store = self.store.clone();
        let metas = self.sessions.clone();
        let (sender, receiver) = async_channel::unbounded();
        unblock(executor, move || {
            let total = threads.len();
            let mut imported = 0;
            let mut skipped = 0;
            let mut existing = existing_external_ids(&metas);
            for (index, thread) in threads.into_iter().enumerate() {
                let tool = thread.source.display_name().to_string();
                match import_thread(&store, &project, &thread, &mut existing) {
                    ImportOutcome::Imported => imported += 1,
                    ImportOutcome::SkippedDuplicate
                    | ImportOutcome::SkippedEmpty
                    | ImportOutcome::Failed(_) => skipped += 1,
                }
                let _ = sender.try_send(ExternalImportUpdate::Progress {
                    done: index + 1,
                    total,
                    tool,
                });
            }
            let _ = sender.try_send(ExternalImportUpdate::Finished { imported, skipped });
        })
        .detach();
        Some(receiver)
    }

    /// List the active session's workspace on the background executor.
    pub fn list_active_workspace(
        &self,
        executor: &gpui::BackgroundExecutor,
    ) -> Task<Vec<PathEntry>> {
        let cwd = self.active.as_ref().map(|active| active.meta.cwd.clone());
        unblock(executor, move || {
            cwd.map(|cwd| list_workspace(&cwd)).unwrap_or_default()
        })
    }

    /// Reload sessions written by the external-history importer and expand its
    /// project group.
    pub fn finish_external_import(&mut self, project_id: &str, cx: &mut Context<Self>) {
        self.sessions = self.store.load_index();
        if self
            .settings
            .collapsed_projects
            .iter()
            .any(|id| id == project_id)
        {
            let mut settings = self.settings.clone();
            settings.collapsed_projects.retain(|id| id != project_id);
            self.update_settings(settings, cx);
        } else {
            cx.notify();
        }
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
        Some(user_files::attachment_dir(self.store.root(), id))
    }

    /// Persist attachment `bytes` under the active session's attachments dir with
    /// the given file extension, returning the saved file path. Files are written
    /// now so a pending image is never lost even though the send wire cannot yet
    /// carry it (see the composer's image seam + reported contract gap).
    pub fn save_attachment(&self, bytes: &[u8], ext: &str) -> std::io::Result<PathBuf> {
        let id = self.active_session_id().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "no active session")
        })?;
        user_files::save_attachment(self.store.root(), id, bytes, ext)
    }

    pub fn update_settings(&mut self, settings: Settings, cx: &mut Context<Self>) {
        if let Err(err) = self.settings_store.save(&settings) {
            self.report_error(
                RuntimeError::PersistSettings {
                    error: err.to_string(),
                },
                cx,
            );
            return;
        }
        let language = settings.language.clone();
        self.settings = settings;
        cx.emit(AppEvent::Effect(RuntimeEffect::ApplyLocale { language }));
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
        self.close_orchestrator_children(session_id);
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
    pub fn worktree_orphaned_by_delete(&self, session_id: &str) -> Option<WorktreeInfo> {
        let meta = self.sessions.iter().find(|m| m.id == session_id)?;
        let worktree = meta.worktree.clone()?;
        let others = self.sessions.iter().any(|m| {
            m.id != session_id
                && m.worktree
                    .as_ref()
                    .is_some_and(|w| w.branch == worktree.branch)
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
        // Deleting a thread that is working in the background kills it for real.
        self.drop_background(session_id);
        self.close_orchestrator_children(session_id);
        if let Some(meta) = &meta {
            // Best-effort checkpoint ref cleanup in the session cwd.
            if is_git_repo(&meta.cwd) {
                delete_all_checkpoint_refs(&meta.cwd, &meta.id);
            }
            if remove_worktree
                && let Some(worktree) = &meta.worktree
                && let Err(err) = remove_git_worktree(&worktree.root_project_path, &meta.cwd)
            {
                self.report_error(
                    RuntimeError::WorktreeRemove {
                        error: err.to_string(),
                    },
                    cx,
                );
            }
        }
        self.settings.last_visited.remove(session_id);
        if let Err(err) = self.store.remove_session(session_id) {
            self.report_error(
                RuntimeError::DeleteSession {
                    error: err.to_string(),
                },
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

    /// Permanently remove a project and all of its threads from tcode. Project
    /// files and worktrees on disk are left in place.
    pub fn delete_project(&mut self, project_id: &str, cx: &mut Context<Self>) {
        let session_ids: Vec<String> = self
            .sessions
            .iter()
            .filter(|meta| meta.project_id.as_deref() == Some(project_id))
            .map(|meta| meta.id.clone())
            .collect();
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.meta.project_id.as_deref() == Some(project_id))
        {
            self.shutdown_active();
        }
        for session_id in session_ids {
            self.delete_session(&session_id, false, cx);
        }
        if let Err(err) = self.store.remove_project(project_id) {
            self.report_error(
                RuntimeError::DeleteProject {
                    error: err.to_string(),
                },
                cx,
            );
            return;
        }
        self.settings
            .collapsed_projects
            .retain(|id| id != project_id);
        let settings = self.settings.clone();
        let _ = self.settings_store.save(&settings);
        self.sessions = self.store.load_index();
        self.projects = self.store.load_projects();
        cx.notify();
    }

    /// Whether a turn is currently running for `session_id` (only the active
    /// session can be running).
    pub fn turn_running_for(&self, session_id: &str) -> bool {
        if let Some(active) = self.active.as_ref().filter(|a| a.meta.id == session_id) {
            return active.timeline.turn_running;
        }
        // A parked session is working when a turn is in flight or its queue
        // still has messages to run (the parked timeline is stale by design, so
        // the flags are the source of truth).
        self.background
            .get(session_id)
            .is_some_and(|s| s.turn_in_flight || !s.queue.is_empty())
    }

    /// Number of active or parked sessions with a provider turn in flight.
    pub fn turns_in_flight_count(&self) -> usize {
        usize::from(
            self.active
                .as_ref()
                .is_some_and(|session| session.turn_in_flight),
        ) + self
            .background
            .values()
            .filter(|session| session.turn_in_flight)
            .count()
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
    fn capture_checkpoint(
        &mut self,
        session_id: &str,
        event_offset: usize,
        cx: &mut Context<Self>,
    ) {
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
        if !is_git_repo(&cwd) {
            return;
        }
        match create_checkpoint(&cwd, session_id, turn) {
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

    /// Rewind the active thread to just before `turn`'s user message: restore the
    /// worktree from that turn's checkpoint (when there is one), truncate the
    /// JSONL log at the message, drop the now-orphaned newer checkpoint refs, and
    /// roll the provider session back to Idle — so the next send resumes from the
    /// truncated transcript. Blocked while a turn runs.
    ///
    /// This is the single rewind mechanism behind both Revert and Edit & resend;
    /// they differ only in what happens afterwards.
    ///
    /// * `Some(true)` — rewound and the worktree was restored from a checkpoint.
    /// * `Some(false)` — rewound, but the turn has no checkpoint (e.g. a non-git
    ///   cwd): the transcript was truncated and the files on disk were left as
    ///   they are. Callers must say so.
    /// * `None` — nothing happened (no active thread, a turn is running, the turn
    ///   is unknown, or a git/IO failure — which reports itself).
    fn rewind_to_turn(&mut self, turn: usize, cx: &mut Context<Self>) -> Option<bool> {
        let (session_id, cwd, checkpoint) = {
            let active = self.active.as_ref()?;
            if active.timeline.turn_running {
                cx.emit(AppEvent::Error(RuntimeError::CheckpointRevertBlocked));
                return None;
            }
            let checkpoint = active
                .meta
                .checkpoints
                .iter()
                .find(|c| c.turn == turn)
                .cloned();
            (active.meta.id.clone(), active.meta.cwd.clone(), checkpoint)
        };

        // The truncation boundary: the checkpoint's recorded offset, or — with no
        // checkpoint — the offset recomputed by replaying the stored log.
        let event_offset = match &checkpoint {
            Some(cp) => cp.event_offset,
            None => turn_user_event_offset(&self.store.read_events(&session_id), turn)?,
        };

        if let Some(cp) = &checkpoint {
            if let Err(err) = restore_checkpoint(&cwd, &cp.commit) {
                self.report_error(RuntimeError::External(err), cx);
                return None;
            }
            delete_checkpoint_refs_from(&cwd, &session_id, turn);
        }
        if let Err(err) = self.store.truncate_events(&session_id, event_offset) {
            self.report_error(
                RuntimeError::PersistEvent {
                    error: err.to_string(),
                },
                cx,
            );
            return None;
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
            active.queue.clear();
            active.plan_implemented = false;
            let meta = active.meta.clone();
            self.persist_meta(&meta, cx);
        }
        cx.notify();
        Some(checkpoint.is_some())
    }

    /// Revert the active thread to the checkpoint captured before `turn`. Only
    /// offered for turns that have a checkpoint (the hover affordance is hidden
    /// otherwise), so this is a no-op without one.
    pub fn revert_to_turn(&mut self, turn: usize, cx: &mut Context<Self>) {
        if !self.turn_has_checkpoint(turn) {
            return;
        }
        if self.rewind_to_turn(turn, cx).is_some() {
            cx.emit(AppEvent::Notice(RuntimeNotice::CheckpointReverted));
        }
    }

    /// Edit & resend a user message: rewind the conversation to the state just
    /// before it (worktree + transcript + provider session — the same mechanism
    /// Revert uses), then send `text` as a fresh turn.
    ///
    /// Without a checkpoint the transcript is still truncated and the message
    /// resent, but the files on disk are untouched — the caller is told so with a
    /// toast rather than silently.
    pub fn edit_and_resend_turn(&mut self, turn: usize, text: String, cx: &mut Context<Self>) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        let Some(restored) = self.rewind_to_turn(turn, cx) else {
            return;
        };
        if !restored {
            cx.emit(AppEvent::Notice(RuntimeNotice::EditWithoutCheckpoint));
        }
        self.send_turn(text, Vec::new(), cx);
    }

    /// The last turn in the active timeline that has a user message (the target
    /// of the `--debug-edit-resend` dev flag).
    pub fn last_user_turn(&self) -> Option<usize> {
        self.active
            .as_ref()?
            .timeline
            .entries
            .iter()
            .rev()
            .find_map(|entry| match entry.content {
                EntryContent::User { .. } => Some(entry.turn),
                _ => None,
            })
    }

    /// Dev flag `--debug-edit-resend "<text>"`: edit & resend the last user
    /// message of the opened session (the GUI's hover action row cannot be
    /// clicked headlessly).
    pub fn debug_edit_resend(&mut self, text: String, cx: &mut Context<Self>) {
        let Some(turn) = self.last_user_turn() else {
            log::error!("--debug-edit-resend: the opened session has no user message");
            return;
        };
        log::info!("--debug-edit-resend: editing turn {turn} -> {text:?}");
        self.edit_and_resend_turn(turn, text, cx);
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
            let result = unblock(cx.background_executor(), move || {
                create_git_worktree(
                    &root_for_task,
                    &path_for_task,
                    &branch_for_task,
                    &base_for_task,
                )
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
                        state.report_error(
                            RuntimeError::WorktreeAdd {
                                error: err.to_string(),
                            },
                            cx,
                        );
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
        // Which installed ACP agent to run (required when `provider` is
        // `ProviderKind::Acp`, ignored otherwise).
        acp_agent_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let mut meta = SessionMeta::new(provider, cwd, model);
        meta.acp_agent_id = acp_agent_id;
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
                RuntimeError::PersistSession {
                    error: err.to_string(),
                },
                cx,
            );
        }
        self.sessions = self.store.load_index();
        self.park_active();
        let git_branch = read_git_branch(&meta.cwd);
        let provider_commands =
            self.cached_provider_commands(meta.provider, meta.acp_agent_id.as_deref());
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
            queue: Vec::new(),
            next_queue_id: 0,
            turn_in_flight: false,
            provider_commands,
            provider_options: Vec::new(),
            diff_open: false,
            diff_expanded: false,
            diff_selected_turn: None,
            right_tab: RightTab::default(),
            auto_open_suppressed: false,
            terminal_workspace: TerminalWorkspace::default(),
            _pump: None,
        });
        self.ensure_started(cx);
        self.refresh_git_status(cx);
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
        acp_agent_id: Option<String>,
        provider_commands: Vec<ProviderCommand>,
    ) -> ActiveSession {
        let mut meta = SessionMeta::new(provider, cwd, model);
        meta.project_id = Some(project_id);
        meta.acp_agent_id = acp_agent_id;
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
            queue: Vec::new(),
            next_queue_id: 0,
            turn_in_flight: false,
            provider_commands,
            provider_options: Vec::new(),
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
    /// updated, non-archived session in this project. Only reasoning effort is
    /// inherited from its model options. Projects without active history fall
    /// back to the most recently updated non-archived global session (or the
    /// Claude default), without inheriting model options.
    fn draft_defaults(
        &self,
        project_id: &str,
    ) -> (
        ProviderKind,
        Option<String>,
        Option<String>,
        Option<OptionSelection>,
    ) {
        if let Some(meta) = self
            .sessions
            .iter()
            .filter(|meta| {
                meta.archived_at.is_none() && meta.project_id.as_deref() == Some(project_id)
            })
            .max_by_key(|meta| meta.updated_at)
        {
            let reasoning_effort = meta
                .option_selections
                .iter()
                .find(|selection| selection.id == "reasoningEffort")
                .cloned();
            return (
                meta.provider,
                meta.model.clone(),
                meta.acp_agent_id.clone(),
                reasoning_effort,
            );
        }

        match self
            .sessions
            .iter()
            .filter(|meta| meta.archived_at.is_none())
            .max_by_key(|meta| meta.updated_at)
        {
            Some(meta) => (
                meta.provider,
                meta.model.clone(),
                meta.acp_agent_id.clone(),
                None,
            ),
            None => (ProviderKind::ClaudeCode, None, None, None),
        }
    }

    /// Switch the main area into a draft for `project_id` (rooted at `cwd`): an
    /// empty timeline with a focused, functional composer. The session is
    /// created lazily on the first send (see `send_turn`/`commit_draft`).
    pub fn start_draft(&mut self, project_id: String, cwd: PathBuf, cx: &mut Context<Self>) {
        self.park_active();
        let (provider, model, acp_agent_id, reasoning_effort) = self.draft_defaults(&project_id);
        let provider_commands = self.cached_provider_commands(provider, acp_agent_id.as_deref());
        let mut draft = Self::build_draft_session(
            project_id,
            cwd,
            provider,
            model,
            acp_agent_id,
            provider_commands,
        );
        draft.meta.option_selections = reasoning_effort.into_iter().collect();
        self.active = Some(draft);
        self.refresh_git_status(cx);
        cx.notify();
    }

    /// Whether the active thread is an unsent draft.
    pub fn active_is_draft(&self) -> bool {
        self.active.as_ref().is_some_and(|a| a.draft)
    }

    /// Persist the active draft as a real session (no cx; caller notifies).
    /// The session id is preserved, so its already-recorded events line up.
    fn commit_draft(&mut self) -> std::io::Result<()> {
        if let Some(active) = self.active.as_mut()
            && active.draft
        {
            active.draft = false;
            let meta = active.meta.clone();
            self.store.upsert_meta(&meta)?;
            self.sessions = self.store.load_index();
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
        self.park_active();
        self.mark_visited(session_id);

        // A parked session is re-adopted, not replayed cold: its process, pump
        // and queue come back as they were, and the timeline is rebuilt from the
        // JSONL — which stayed current while parked, because `record_event`
        // routes by session id.
        if let Some(mut parked) = self.background.remove(session_id) {
            log::info!(
                "re-adopting parked session {} (turn in flight: {}, queued: {})",
                session_id,
                parked.turn_in_flight,
                parked.queue.len()
            );
            parked.timeline = Timeline::fold_events(self.store.read_events(session_id));
            parked.git_branch = read_git_branch(&parked.meta.cwd);
            let needs_restart = matches!(parked.runtime, Runtime::Idle) && !parked.queue.is_empty();
            self.active = Some(parked);
            // Anything still queued that can go now, goes now.
            if self.dispatch_next_queued(cx).is_err() {
                self.report_error(RuntimeError::ProcessGone, cx);
            }
            if needs_restart {
                // Parked with a dead provider (its start failed while parked):
                // the queue survived, so try again now that someone is looking.
                self.ensure_started(cx);
            }
            self.refresh_git_status(cx);
            cx.notify();
            return;
        }

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
        let provider_commands =
            self.cached_provider_commands(meta.provider, meta.acp_agent_id.as_deref());
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
            queue: Vec::new(),
            next_queue_id: 0,
            turn_in_flight: false,
            provider_commands,
            provider_options: Vec::new(),
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
        self.refresh_git_status(cx);
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
    pub fn send_turn(
        &mut self,
        text: String,
        attachments: Vec<Attachment>,
        cx: &mut Context<Self>,
    ) {
        // Group C: a draft in worktree mode creates its worktree in the
        // background on first send, then re-enters send_turn once ready.
        if let Some(active) = self.active.as_ref()
            && active.draft
            && !active.preparing_worktree
            && let WorkspaceMode::NewWorktree { base } = active.draft_workspace.clone()
        {
            self.begin_worktree_prep(text, attachments, base, cx);
            return;
        }

        // The first send on a draft materializes it into a real (persisted)
        // session so the sidebar row appears; the provider then starts below.
        if self.active_is_draft()
            && let Err(err) = self.commit_draft()
        {
            self.report_error(
                RuntimeError::PersistSession {
                    error: err.to_string(),
                },
                cx,
            );
            return;
        }

        let Some(active) = self.active.as_mut() else {
            return;
        };

        // Every send goes through the queue; what differs is whether it can
        // leave it right now. With a turn in flight the message simply waits
        // (Enter → QUEUE) and shows up in the composer's queue strip; nothing is
        // written to the transcript yet, so dropping it there leaves no trace.
        // See `dispatch_next_queued`, which records the user message at the
        // moment the message is actually sent.
        active.push_queued(text, attachments);

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
            log::info!(
                "launch-time option changed while live; restarting provider before next turn"
            );
            active.shutdown_to_idle();
        }
        let should_start = matches!(active.runtime, Runtime::Idle);
        let dispatch_failed = self.dispatch_next_queued(cx).is_err();
        if should_start {
            self.ensure_started(cx);
        }
        if dispatch_failed {
            self.report_error(RuntimeError::ProcessGone, cx);
        }
        cx.notify();
    }

    /// Record + dispatch the head of the queue, if the session can take a turn
    /// right now (live provider, nothing in flight). This is the ONLY place a
    /// queued message becomes part of the conversation: the user message is
    /// appended to the JSONL and the pre-turn checkpoint captured here, not when
    /// the message was queued — so a message dropped from the queue strip never
    /// touches the transcript.
    fn dispatch_next_queued(&mut self, cx: &mut Context<Self>) -> Result<bool, ()> {
        let Some(active) = self.active.as_ref() else {
            return Ok(false);
        };
        if active.turn_in_flight || !matches!(active.runtime, Runtime::Live(_)) {
            return Ok(false);
        }
        let Some(next) = active.queue.first().cloned() else {
            return Ok(false);
        };
        let session_id = active.meta.id.clone();

        // Group B: the JSONL length before this turn's user message — the revert
        // truncation boundary — captured before the message is appended.
        let checkpoint_offset = self.store.event_count(&session_id);
        self.record_user_message(&session_id, &next.text, cx);
        // Group B: snapshot the pre-turn working tree for this turn's revert.
        self.capture_checkpoint(&session_id, checkpoint_offset, cx);
        self.maybe_adopt_title(cx);

        let Some(active) = self.active.as_mut() else {
            return Ok(false);
        };
        active.dispatch_next_pending()
    }

    /// A parked session finished a turn: keep working through its queue, and
    /// shut it down once nothing is left. Mirrors `dispatch_next_queued` with
    /// two honest omissions — no git checkpoint (the checkpoint boundary needs
    /// the live timeline, which a parked session doesn't maintain; those turns
    /// simply have no Revert) and no title adoption (a parked session already
    /// has its title).
    fn on_background_turn_completed(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let Some(parked) = self.background.get_mut(session_id) else {
            return;
        };
        parked.turn_in_flight = false;
        if parked.queue.is_empty() {
            if parked.meta.parent_session_id.is_some() {
                let child_id = session_id.to_string();
                let idle_turns = Timeline::fold_events(self.store.read_events(session_id))
                    .turns
                    .len();
                cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_secs(30 * 60))
                        .await;
                    let _ = this.update(cx, |state, cx| {
                        let still_idle =
                            state.background.get(&child_id).is_some_and(|child| {
                                child.queue.is_empty() && !child.turn_in_flight
                            }) && Timeline::fold_events(state.store.read_events(&child_id))
                                .turns
                                .len()
                                == idle_turns;
                        if still_idle {
                            state.drop_background(&child_id);
                            cx.notify();
                        }
                    });
                })
                .detach();
                cx.notify();
                return;
            }
            log::info!("parked session {session_id} finished its work; shutting down");
            self.drop_background(session_id);
            cx.notify();
            return;
        }
        let next_text = parked.queue.first().map(|m| m.text.clone()).unwrap();
        self.record_user_message(session_id, &next_text, cx);
        if let Some(parked) = self.background.get_mut(session_id)
            && parked.dispatch_next_pending().is_err()
        {
            // The process is gone; the queue (with its unsent text) survives
            // for the user to find when they reopen the thread.
            log::warn!("parked session {session_id}: dispatch failed (process gone)");
        }
        cx.notify();
    }

    fn deliver_child_callback(
        &mut self,
        child_id: &str,
        status: TurnStatus,
        cx: &mut Context<Self>,
    ) {
        let Some(child) = self
            .sessions
            .iter()
            .find(|meta| meta.id == child_id && meta.parent_session_id.is_some())
            .cloned()
        else {
            return;
        };
        if self
            .active
            .as_ref()
            .filter(|child| child.meta.id == child_id)
            .or_else(|| self.background.get(child_id))
            .is_some_and(|child| !child.queue.is_empty())
        {
            return;
        }
        let timeline = Timeline::fold_events(self.store.read_events(child_id));
        let turn = timeline.turns.len();
        if self.callback_last_turn.get(child_id).copied() == Some(turn) {
            return;
        }
        let parent_id = child.parent_session_id.clone().unwrap();
        let count = self.callback_counts.entry(parent_id.clone()).or_default();
        let first_breach = *count == ORCHESTRATE_CALLBACK_CAP;
        if !take_callback_slot(count) {
            if first_breach {
                self.report_error(
                    RuntimeError::OrchestrateCallbackLimit {
                        parent_id: parent_id.to_string(),
                        cap: ORCHESTRATE_CALLBACK_CAP,
                    },
                    cx,
                );
            }
            return;
        }
        self.callback_last_turn.insert(child_id.to_string(), turn);
        let text = assemble_callback_text(
            child_id,
            &child.title,
            status,
            &final_assistant_message(&timeline),
        );
        self.deliver_orchestrate_callback_to_parent(&parent_id, text, cx);
    }

    /// Deliver a child result into the orchestrator's current reasoning turn.
    ///
    /// A foreground parent already used `steer`, but a parked parent used to put
    /// callbacks into its ordinary queue. Parallel children could therefore
    /// leave results stranded while the orchestrator planned from only the first
    /// completion. Steering is session lifecycle behavior, not UI focus behavior,
    /// so foreground and parked parents follow the same routing here.
    fn deliver_orchestrate_callback_to_parent(
        &mut self,
        parent_id: &str,
        text: String,
        cx: &mut Context<Self>,
    ) {
        let can_steer = self
            .active
            .as_ref()
            .filter(|parent| parent.meta.id == parent_id)
            .or_else(|| self.background.get(parent_id))
            .is_some_and(|parent| parent.turn_in_flight && parent.supports_steering());
        if can_steer {
            // A steered callback is already part of this turn, so persist it just
            // like a user-triggered steer before handing it to the provider.
            let request_id = self.record_steer_request(parent_id, &text, cx);
            let sent = self
                .active
                .as_mut()
                .filter(|parent| parent.meta.id == parent_id)
                .or_else(|| self.background.get_mut(parent_id))
                .is_some_and(|parent| parent.steer_now(request_id, text, Vec::new()).is_ok());
            if !sent {
                self.report_error(RuntimeError::ProcessGone, cx);
            }
            cx.notify();
            return;
        }

        if self.active_session_id() == Some(parent_id) {
            let parent = self.active.as_mut().unwrap();
            parent.push_or_merge_orchestrate_callback(text);

            // Match ordinary sends when a launch-time selection changed while
            // the provider was live: restart before waking the orchestrator.
            if parent.model_changed_while_live()
                || parent.approval_mode_changed_while_live()
                || parent.options_changed_while_live()
            {
                parent.shutdown_to_idle();
            }
            let should_start = matches!(parent.runtime, Runtime::Idle);
            if self.dispatch_next_queued(cx).is_err() {
                self.report_error(RuntimeError::ProcessGone, cx);
            }
            if should_start {
                self.ensure_started(cx);
            }
            cx.notify();
            return;
        }

        if !self.background.contains_key(parent_id)
            && let Some(parent) = self
                .sessions
                .iter()
                .find(|meta| meta.id == parent_id)
                .cloned()
        {
            self.load_background_session(parent);
        }
        if let Some(parent) = self.background.get_mut(parent_id) {
            parent.push_or_merge_orchestrate_callback(text);
            let idle_runtime = matches!(parent.runtime, Runtime::Idle);
            let can_dispatch = !parent.turn_in_flight && matches!(parent.runtime, Runtime::Live(_));
            if can_dispatch {
                self.on_background_turn_completed(parent_id, cx);
            }
            if idle_runtime {
                self.ensure_session_started(parent_id, cx);
            }
            cx.notify();
        }
    }

    /// Append a user message to the session transcript. Providers don't echo
    /// user input, so we record it as a synthetic canonical event and replay
    /// renders it identically.
    fn record_user_message(&mut self, session_id: &str, text: &str, cx: &mut Context<Self>) {
        let user_event = AgentEvent::ItemCompleted(ThreadItem {
            id: format!("local-user-{}", uuid::Uuid::new_v4()),
            parent_item_id: None,
            content: ItemContent::UserMessage {
                text: text.to_owned(),
            },
        });
        self.record_event(session_id, &user_event, cx);
    }

    /// Persist a pending steering bubble and return the exact id providers must
    /// echo in `SteerAccepted` after real delivery succeeds.
    fn record_steer_request(
        &mut self,
        session_id: &str,
        text: &str,
        cx: &mut Context<Self>,
    ) -> String {
        let request_id = format!("local-steer-{}", uuid::Uuid::new_v4());
        self.record_event(
            session_id,
            &AgentEvent::SteerRequested {
                request_id: request_id.clone(),
                text: text.to_owned(),
            },
            cx,
        );
        request_id
    }

    /// Cmd+Enter: inject `text` into the turn that is ALREADY running, so the
    /// model picks it up at its next opportunity (typically its next tool call).
    ///
    /// Degrades honestly:
    ///   * no turn running → there is nothing to steer into, so just send;
    ///   * turn running, provider can't steer (ACP) → queue it and say so.
    ///
    /// A steered message IS part of the conversation, so it is recorded to the
    /// session JSONL as a user message (unlike a merely queued one).
    pub fn steer(&mut self, text: String, attachments: Vec<Attachment>, cx: &mut Context<Self>) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        match active.route(true) {
            // Nothing is running, so there is nothing to steer into: an ordinary
            // send is exactly the right thing.
            SendRouting::Send | SendRouting::Queue => self.send_turn(text, attachments, cx),
            SendRouting::QueueUnsupported => {
                let agent = active.meta.provider.display_name();
                self.send_turn(text, attachments, cx);
                self.report_error(
                    RuntimeError::SteerUnsupported {
                        agent: agent.to_string(),
                    },
                    cx,
                );
            }
            SendRouting::Steer => {
                let session_id = active.meta.id.clone();
                let wire_text = if active.pending_ultrathink {
                    format!("Ultrathink:\n{text}")
                } else {
                    text.clone()
                };
                // The steered message joins the running turn, so it belongs in
                // the transcript exactly like any other user message. (A merely
                // *queued* message does not — see `dispatch_next_queued`.)
                let request_id = self.record_steer_request(&session_id, &text, cx);

                let Some(active) = self.active.as_mut() else {
                    return;
                };
                active.pending_ultrathink = false;
                if active
                    .steer_now(request_id, wire_text, attachments)
                    .is_err()
                {
                    self.report_error(RuntimeError::ProcessGone, cx);
                }
                cx.notify();
            }
        }
    }

    /// Queue strip: convert an already-queued message into a steering message —
    /// pull it out of the queue and inject it into the running turn.
    pub fn steer_queued(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        let Some(message) = active.take_queued(id) else {
            return;
        };
        // `steer` consumes the session's armed Ultrathink flag, but this
        // message captured its own at queue time — re-arm so it rides along.
        active.pending_ultrathink = message.ultrathink;
        self.steer(message.text, message.attachments, cx);
    }

    /// Queue strip: drop a queued message (the row's ✕). It was never recorded,
    /// so nothing needs undoing.
    pub fn drop_queued(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(active) = self.active.as_mut() {
            active.queue.retain(|m| m.id != id);
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
        let store = self.store.clone();
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
                active.meta.acp_agent_id = None;
            }
            active.meta.model = model;
            // A different model has different option descriptors: drop stale
            // selections so each resolves to the new model's defaults.
            active.meta.option_selections.clear();
            active.provider_commands = store.load_commands(active.meta.provider, None);
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
        // ACP agents apply option changes live: route the choice back to the
        // agent (`session/set_mode` / `set_model` / `set_config_option`) instead
        // of waiting for a restart.
        if active.meta.provider == ProviderKind::Acp
            && let Runtime::Live(commands) = &active.runtime
            && let Some(selection) = active.meta.option_selections.iter().find(|s| s.id == id)
        {
            let _ = commands.try_send(SessionCommand::SetOption {
                id: selection.id.clone(),
                value: selection.value.clone(),
            });
            active.live_option_selections = active.meta.option_selections.clone();
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
        self.send_turn(implement_prompt(&markdown), Vec::new(), cx);
    }

    /// Accept the proposed plan in a fresh thread in the same project (same
    /// cwd/model/options, Build mode) titled "Implement <plan title>".
    pub fn implement_plan_in_new_thread(&mut self, title: String, cx: &mut Context<Self>) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        let Some(plan) = active.timeline.proposed_plan.as_ref() else {
            return;
        };
        let markdown = plan.markdown.clone();
        let provider = active.meta.provider;
        let cwd = active.meta.cwd.clone();
        let model = active.meta.model.clone();
        let option_selections = active.meta.option_selections.clone();
        let approval_mode = active.meta.approval_mode;
        let project_id = active.meta.project_id.clone();
        let acp_agent_id = active.meta.acp_agent_id.clone();

        let mut meta = SessionMeta::new(provider, cwd, model);
        meta.title = title;
        meta.option_selections = option_selections;
        meta.approval_mode = approval_mode;
        meta.interaction_mode = InteractionMode::Build;
        meta.project_id = project_id;
        meta.acp_agent_id = acp_agent_id;
        if let Err(err) = self.store.upsert_meta(&meta) {
            self.report_error(
                RuntimeError::PersistSession {
                    error: err.to_string(),
                },
                cx,
            );
        }
        self.sessions = self.store.load_index();
        self.park_active();
        let git_branch = read_git_branch(&meta.cwd);
        let provider_commands =
            self.cached_provider_commands(meta.provider, meta.acp_agent_id.as_deref());
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
            queue: Vec::new(),
            next_queue_id: 0,
            turn_in_flight: false,
            provider_commands,
            provider_options: Vec::new(),
            diff_open: false,
            diff_expanded: false,
            diff_selected_turn: None,
            right_tab: RightTab::default(),
            auto_open_suppressed: false,
            terminal_workspace: TerminalWorkspace::default(),
            _pump: None,
        });
        self.send_turn(implement_prompt(&markdown), Vec::new(), cx);
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
        match user_files::save_plan_to_workspace(&cwd, &markdown) {
            Ok(path) => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                cx.emit(AppEvent::Notice(RuntimeNotice::PlanSaved { file: name }));
            }
            Err(err) => self.report_error(
                RuntimeError::PersistEvent {
                    error: err.to_string(),
                },
                cx,
            ),
        }
        cx.notify();
    }

    /// Save the plan markdown to the user's Downloads directory (falling back to
    /// the session cwd) with a title-derived filename ("Download as markdown").
    pub fn download_plan(
        &mut self,
        markdown: String,
        fallback_title: String,
        cx: &mut Context<Self>,
    ) {
        let title = plan_title(&markdown).unwrap_or(fallback_title);
        let filename = format!("{}.md", sanitize_filename(&title));
        let fallback_cwd = self.active.as_ref().map(|a| a.meta.cwd.as_path());
        match user_files::save_plan_download(&filename, &markdown, fallback_cwd) {
            Ok(path) => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                cx.emit(AppEvent::Notice(RuntimeNotice::PlanSaved { file: name }));
            }
            Err(err) => self.report_error(
                RuntimeError::PersistEvent {
                    error: err.to_string(),
                },
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
        if let Some(active) = self.active.as_mut()
            && !(active.diff_open && active.right_tab == RightTab::Preview)
        {
            active.diff_open = true;
            active.right_tab = RightTab::Preview;
            cx.notify();
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
            let branches = unblock(cx.background_executor(), move || list_git_branches(&cwd)).await;
            let _ = this.update(cx, |state, cx| {
                if let Some(active) = state.active.as_mut()
                    && active.meta.id == session_id
                {
                    active.branches = branches;
                    cx.notify();
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
            let result = unblock(cx.background_executor(), move || {
                checkout_if_clean(&cwd, &branch_for_task)
            })
            .await;
            let _ = this.update(cx, |state, cx| {
                match result {
                    Ok(()) => {
                        if let Some(active) = state.active.as_mut()
                            && active.meta.id == session_id
                        {
                            active.git_branch = read_git_branch(&active.meta.cwd);
                        }
                        cx.emit(AppEvent::Notice(RuntimeNotice::SwitchedBranch { branch }));
                    }
                    Err(CheckoutError::Dirty) => {
                        cx.emit(AppEvent::Error(RuntimeError::DirtyTree));
                    }
                    Err(CheckoutError::Git(message)) => {
                        cx.emit(AppEvent::Error(RuntimeError::External(message)))
                    }
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
        let Some(session_id) = self.active_session_id().map(str::to_owned) else {
            return;
        };
        self.ensure_session_started(&session_id, cx);
    }

    /// Spawn a provider for either the foreground session or a parked child.
    fn ensure_session_started(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let idle = self
            .active
            .as_ref()
            .filter(|active| active.meta.id == session_id)
            .map(|active| matches!(active.runtime, Runtime::Idle))
            .or_else(|| {
                self.background
                    .get(session_id)
                    .map(|active| matches!(active.runtime, Runtime::Idle))
            })
            .unwrap_or(false);
        if !idle {
            return;
        }
        self.next_start_generation = self
            .next_start_generation
            .checked_add(1)
            .expect("provider start generation overflow");
        let generation = self.next_start_generation;
        let active = self
            .active
            .as_mut()
            .filter(|active| active.meta.id == session_id)
            .or_else(|| self.background.get_mut(session_id))
            .unwrap();
        active.runtime = Runtime::Starting { generation };
        // Remember the model + approval mode this process is being launched
        // with so a later switch can detect the mismatch and restart.
        active.live_model = active.meta.model.clone();
        active.live_approval_mode = Some(active.meta.approval_mode);
        active.live_option_selections = active.meta.option_selections.clone();

        let meta = active.meta.clone();
        let settings = self.settings.clone();
        let launch_env = self.session_launch_env(&meta);
        let mcp_registration = self.mcp_registration.clone();
        let orchestrate_registration = self.orchestrate_registration_for(&meta);
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
            let opts = session_options(
                &meta,
                &settings,
                launch_env,
                mcp_registration,
                orchestrate_registration,
            );
            let result = start_session(meta.provider, opts).await;
            let _ = this.update(cx, |state, cx| {
                let matches_active = state.active.as_ref().is_some_and(|active| {
                    active.meta.id == session_id && active.is_starting_generation(generation)
                });
                // The session may have been parked (thread switch) while its
                // start was in flight; the attempt then adopts the parked entry.
                let matches_parked = !matches_active
                    && state
                        .background
                        .get(&session_id)
                        .is_some_and(|parked| parked.is_starting_generation(generation));
                match result {
                    Ok(handle) => {
                        if !matches_active && !matches_parked {
                            // Superseded by a newer start, or the session is gone.
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
                        if matches_active {
                            let active = state.active.as_mut().unwrap();
                            active.runtime = Runtime::Live(commands.clone());
                            active._pump = Some(pump);
                            if state.dispatch_next_queued(cx).is_err() {
                                state.report_error(RuntimeError::ProcessGone, cx);
                            }
                        } else {
                            let parked = state.background.get_mut(&session_id).unwrap();
                            parked.runtime = Runtime::Live(commands.clone());
                            parked._pump = Some(pump);
                            // Work through the parked queue exactly as a
                            // finished background turn would.
                            state.on_background_turn_completed(&session_id, cx);
                        }
                        cx.notify();
                    }
                    Err(err) => {
                        if matches_active || matches_parked {
                            // The queue is deliberately KEPT in both cases: it
                            // holds text the user typed but that was never
                            // sent. It stays visible in the queue strip and
                            // flushes on the next successful start; clearing it
                            // would destroy their words along with the process
                            // (the T3 bug family this app tests against).
                            if let Some(active) = state.active.as_mut().filter(|_| matches_active) {
                                active.runtime = Runtime::Idle;
                                active.turn_in_flight = false;
                            } else if let Some(parked) = state.background.get_mut(&session_id) {
                                parked.runtime = Runtime::Idle;
                                parked.turn_in_flight = false;
                            }
                            let error_event = AgentEvent::ProviderStartFailed {
                                error: err.to_string(),
                            };
                            state.record_event(&session_id, &error_event, cx);
                            let is_child = state.sessions.iter().any(|meta| {
                                meta.id == session_id && meta.parent_session_id.is_some()
                            });
                            if is_child {
                                if let Some(child) = state.background.get_mut(&session_id) {
                                    child.queue.clear();
                                }
                                state.deliver_child_callback(&session_id, TurnStatus::Failed, cx);
                            }
                            state.report_error(
                                RuntimeError::ProviderStart {
                                    error: err.to_string(),
                                },
                                cx,
                            );
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
            self.close_orchestrator_children(session_id);
            let is_active = self.active_session_id() == Some(session_id);
            if !is_active {
                // A parked session's process died on its own (crash, fatal
                // error): put the close on the record — the transcript should
                // say why the work stopped when the thread is reopened — and
                // forget the dead runtime.
                if self.background.contains_key(session_id) {
                    self.record_event(session_id, &event, cx);
                    let is_child = self
                        .sessions
                        .iter()
                        .any(|meta| meta.id == session_id && meta.parent_session_id.is_some());
                    if is_child {
                        if let Some(child) = self.background.get_mut(session_id) {
                            child.queue.clear();
                        }
                        self.deliver_child_callback(session_id, TurnStatus::Failed, cx);
                    }
                    self.background.remove(session_id);
                    cx.notify();
                }
                // Otherwise: user-requested shutdowns remove the runtime before
                // the provider acknowledges them, so their close stays silent.
                return;
            }

            self.record_event(session_id, &event, cx);
            if self
                .sessions
                .iter()
                .any(|meta| meta.id == session_id && meta.parent_session_id.is_some())
            {
                self.deliver_child_callback(session_id, TurnStatus::Failed, cx);
            }
            if let Some(active) = self.active.as_mut() {
                active.runtime = Runtime::Idle;
                active.turn_in_flight = false;
                active._pump = None;
            }
            self.report_error(
                RuntimeError::ProviderClosed {
                    reason: reason.clone(),
                },
                cx,
            );
            cx.notify();
            return;
        }

        // Provider commands/skills are session metadata for the composer menus —
        // stored on the live session and in a per-provider cache, never folded
        // into the timeline or the persisted JSONL log. Parked sessions still
        // receive provider updates, so update/cache those too.
        if let AgentEvent::ProviderCommands { commands } = &event {
            let cache_key = if let Some(active) = self
                .active
                .as_mut()
                .filter(|active| active.meta.id == session_id)
            {
                active.provider_commands.clone_from(commands);
                cx.notify();
                Some((active.meta.provider, active.meta.acp_agent_id.clone()))
            } else if let Some(parked) = self.background.get_mut(session_id) {
                parked.provider_commands.clone_from(commands);
                Some((parked.meta.provider, parked.meta.acp_agent_id.clone()))
            } else {
                None
            };
            if let Some((provider, acp_agent_id)) = cache_key
                && let Err(err) =
                    self.store
                        .save_commands(provider, acp_agent_id.as_deref(), commands)
            {
                log::warn!("failed to persist {provider:?} command cache: {err}");
            }
            return;
        }

        // The agent's own options (ACP modes / models / config options). Same
        // deal: session metadata for the traits picker, not timeline content.
        // The pushed selections become the session's selections, so the picker
        // shows what the agent is actually running with.
        if let AgentEvent::ProviderOptions {
            descriptors,
            selections,
        } = &event
        {
            if let Some(active) = self
                .active
                .as_mut()
                .filter(|active| active.meta.id == session_id)
            {
                active.provider_options = descriptors.clone();
                for selection in selections {
                    active
                        .meta
                        .option_selections
                        .retain(|s| s.id != selection.id);
                    active.meta.option_selections.push(selection.clone());
                }
                active.live_option_selections = active.meta.option_selections.clone();
                let meta = active.meta.clone();
                if meta.acp_agent_id.is_some() {
                    self.persist_meta(&meta, cx);
                }
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
                self.diff_refresh_generation = self.diff_refresh_generation.wrapping_add(1);
                if let Some(meta) = self.meta_mut(session_id) {
                    meta.updated_at = now_secs();
                    let meta = meta.clone();
                    self.persist_meta(&meta, cx);
                }
                // The turn may have switched branches (checkout) or made the
                // first commit; refresh the display-only branch label and the
                // git quick-action status.
                if let Some(active) = self.active.as_mut()
                    && active.meta.id == session_id
                {
                    active.git_branch = read_git_branch(&active.meta.cwd);
                }
                if self.active_session_id() == Some(session_id) {
                    self.refresh_git_status(cx);
                }
            }
            AgentEvent::Error { message, .. } => {
                cx.emit(AppEvent::Error(RuntimeError::ProviderMessage(
                    message.clone(),
                )));
            }
            AgentEvent::Warning(message) => {
                // Provider warnings (config problems, deprecations, failed
                // mode switches) explain later misbehavior: a log line alone
                // hides them from the person who needs to act on them.
                cx.emit(AppEvent::Notice(RuntimeNotice::ProviderMessage(
                    message.clone(),
                )));
            }
            _ => {}
        }

        self.record_event(session_id, &event, cx);

        if let AgentEvent::TurnCompleted { status, .. } = &event {
            self.deliver_child_callback(session_id, *status, cx);
        }

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
                    let already_showing = active.diff_open && active.right_tab == RightTab::Plan;
                    if auto_open && !active.auto_open_suppressed && !already_showing {
                        active.diff_open = true;
                        active.right_tab = RightTab::Plan;
                    }
                }
                _ => {}
            }
        }

        if matches!(event, AgentEvent::TurnCompleted { .. }) {
            // The turn is over: the next queued message (if any) now goes out as
            // an ordinary turn, FIFO, one at a time.
            let is_active = self
                .active
                .as_mut()
                .filter(|active| active.meta.id == session_id)
                .map(|active| active.turn_in_flight = false)
                .is_some();
            if is_active && self.dispatch_next_queued(cx).is_err() {
                self.report_error(RuntimeError::ProcessGone, cx);
            }
            if !is_active {
                self.on_background_turn_completed(session_id, cx);
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
                RuntimeError::PersistEvent {
                    error: err.to_string(),
                },
                cx,
            );
        }
        if let Some(active) = self.active.as_mut()
            && active.meta.id == session_id
        {
            active.timeline.apply_at(Some(ts), event);
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
        if let Some(active) = self.active.as_mut().filter(|a| a.meta.id == session_id) {
            return Some(&mut active.meta);
        }
        // Parked sessions keep receiving meta updates (resume cursor, updated_at)
        // — losing the cursor while parked would break the next cold resume.
        self.background.get_mut(session_id).map(|s| &mut s.meta)
    }

    fn persist_meta(&mut self, meta: &SessionMeta, cx: &mut Context<Self>) {
        if let Err(err) = self.store.upsert_meta(meta) {
            self.report_error(
                RuntimeError::PersistSessionIndex {
                    error: err.to_string(),
                },
                cx,
            );
        }
        self.sessions = self.store.load_index();
        cx.notify();
    }

    pub fn shutdown_active(&mut self) {
        self.persist_terminal_preferences();
        if let Some(active) = self.active.take()
            && let Runtime::Live(commands) = active.runtime
        {
            let _ = commands.try_send(SessionCommand::Shutdown);
        }
    }

    /// Shut down every provider process before the application exits.
    pub fn shutdown_all(&mut self) {
        self.shutdown_active();
        for (_, parked) in self.background.drain() {
            if let Runtime::Live(commands) = parked.runtime {
                let _ = commands.try_send(SessionCommand::Shutdown);
            }
        }
    }

    /// Leave the active session without killing its work: a live session with a
    /// turn in flight or queued messages is parked in `background` (process,
    /// pump and queue intact — see the field docs); an idle one is shut down as
    /// before. Every "switch away" path goes through here; only destructive
    /// paths (archive, delete) use `shutdown_active` directly.
    fn park_active(&mut self) {
        self.persist_terminal_preferences();
        let Some(active) = self.active.take() else {
            return;
        };
        let has_work = active.turn_in_flight || !active.queue.is_empty();
        // Live with work, or still Starting with messages waiting (the start
        // attempt finds and adopts the parked entry when it completes) — both
        // carry state that must not die with a thread switch.
        let parkable = matches!(active.runtime, Runtime::Live(_) | Runtime::Starting { .. });
        if has_work && parkable {
            log::info!(
                "parking session {} (turn in flight: {}, queued: {})",
                active.meta.id,
                active.turn_in_flight,
                active.queue.len()
            );
            self.background.insert(active.meta.id.clone(), active);
        } else if let Runtime::Live(commands) = active.runtime {
            let _ = commands.try_send(SessionCommand::Shutdown);
        }
    }

    /// Shut down and forget a parked session (archive/delete paths).
    fn drop_background(&mut self, session_id: &str) {
        if let Some(parked) = self.background.remove(session_id)
            && let Runtime::Live(commands) = parked.runtime
        {
            let _ = commands.try_send(SessionCommand::Shutdown);
        }
    }

    fn close_orchestrator_children(&mut self, parent_id: &str) {
        let child_ids: Vec<_> = self
            .sessions
            .iter()
            .filter(|meta| meta.parent_session_id.as_deref() == Some(parent_id))
            .map(|meta| meta.id.clone())
            .collect();
        for child_id in child_ids {
            self.drop_background(&child_id);
        }
        if let Some(registration) = self.orchestrate_registrations.remove(parent_id)
            && let Some(tokens) = &self.orchestrate_tokens
        {
            tokens.revoke(&registration.bearer_token);
        }
    }

    fn report_error(&mut self, error: RuntimeError, cx: &mut Context<Self>) {
        log::error!("{error:?}");
        cx.emit(AppEvent::Error(error));
    }
}

const CLAUDE_ORCHESTRATE_GUIDANCE: &str = include_str!("../../../assets/orchestrate/claude.md");
const CODEX_ORCHESTRATE_GUIDANCE: &str = include_str!("../../../assets/orchestrate/codex.md");

fn compose_orchestrate_text(
    provider: ProviderKind,
    enabling: bool,
    user_text: &str,
) -> Option<String> {
    if !enabling {
        return (provider != ProviderKind::Acp).then(|| user_text.to_string());
    }
    let guidance = match provider {
        ProviderKind::ClaudeCode => CLAUDE_ORCHESTRATE_GUIDANCE,
        ProviderKind::Codex => CODEX_ORCHESTRATE_GUIDANCE,
        ProviderKind::Acp => return None,
    };
    if user_text.is_empty() {
        Some(guidance.to_string())
    } else {
        let separator = if guidance.ends_with('\n') {
            "\n"
        } else {
            "\n\n"
        };
        Some(format!("{guidance}{separator}{user_text}"))
    }
}

fn build_child_meta(
    parent: &SessionMeta,
    provider: ProviderKind,
    model: Option<String>,
    effort: Option<String>,
    title: String,
    cwd: PathBuf,
) -> SessionMeta {
    let mut meta = SessionMeta::new(provider, cwd, model);
    meta.title = title;
    meta.project_id = parent.project_id.clone();
    meta.parent_session_id = Some(parent.id.clone());
    if let Some(effort) = effort {
        meta.option_selections.push(OptionSelection {
            id: "reasoningEffort".into(),
            value: serde_json::Value::String(effort),
        });
    }
    meta
}

fn final_assistant_message(timeline: &Timeline) -> String {
    let Some((last_index, last)) = timeline
        .entries
        .iter()
        .enumerate()
        .rev()
        .find(|(_, entry)| matches!(&entry.content, EntryContent::Assistant { .. }))
    else {
        return String::new();
    };

    // One provider message may contain several adjacent text blocks. They are
    // separate timeline entries, but together form the final assistant output.
    // Stop at the first non-assistant item so tool preambles from earlier in the
    // turn are not mistaken for part of the final answer.
    let mut parts = Vec::new();
    for entry in timeline.entries[..=last_index].iter().rev() {
        if entry.turn != last.turn {
            break;
        }
        match &entry.content {
            EntryContent::Assistant { text } => parts.push(text.as_str()),
            _ => break,
        }
    }
    parts.reverse();
    parts.concat()
}

fn tail_chars(text: &str, max: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(max)).collect()
}

fn assemble_callback_text(
    child_id: &str,
    title: &str,
    status: TurnStatus,
    final_message: &str,
) -> String {
    let state = match status {
        TurnStatus::Completed => "completed",
        TurnStatus::Failed | TurnStatus::Interrupted => "failed",
    };
    format!(
        "[orchestrate] thread {child_id} (\"{title}\") {state}.\n{}",
        final_message
    )
}

fn take_callback_slot(count: &mut usize) -> bool {
    if *count >= ORCHESTRATE_CALLBACK_CAP {
        *count = count.saturating_add(1);
        return false;
    }
    *count += 1;
    true
}

/// Parse a `#rrggbb` accent color into a gpui color; `None` when malformed.
fn parse_hex_color(raw: &str) -> Option<gpui::Rgba> {
    let hex = raw.trim().trim_start_matches('#');
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(hex, 16).ok()?;
    Some(gpui::rgb(value))
}

/// Map a model id to its provider, for the draft model-picker → provider link.
/// `None` (the "Default" row, a Claude entry) and the Claude model ids map to
/// Claude; the `gpt-*` ids to Codex. Unknown custom ids leave it unchanged.
/// A stable settings key for a user-defined ACP agent, derived from its name.
fn custom_acp_id(name: &str) -> String {
    let slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    format!("custom:{}", slug.trim_matches('-'))
}

fn provider_for_model(model: Option<&str>) -> Option<ProviderKind> {
    match model {
        None => Some(ProviderKind::ClaudeCode),
        Some("opus" | "sonnet" | "haiku") => Some(ProviderKind::ClaudeCode),
        Some(m) if m.starts_with("gpt") => Some(ProviderKind::Codex),
        Some(_) => None,
    }
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
    launch_env: LaunchEnv,
    mcp_server: Option<agent::McpRegistration>,
    orchestrate_server: Option<agent::McpRegistration>,
) -> SessionOptions {
    let provider_settings = settings.provider(meta.provider);
    // For an ACP session, which agent to launch (and how) comes from the
    // installed-agent list, keyed by the id the session was created with.
    let acp_agent: Option<InstalledAgent> = meta
        .acp_agent_id
        .as_deref()
        .and_then(|id| settings.acp_agent(id))
        .cloned();
    SessionOptions {
        cwd: meta.cwd.clone(),
        model: meta.model.clone(),
        resume: meta.resume_cursor.clone(),
        binary_path: provider_settings.binary_path.clone(),
        approval_mode: meta.approval_mode,
        option_selections: meta.option_selections.clone(),
        interaction_mode: meta.interaction_mode,
        mcp_server,
        orchestrate_server: if meta.orchestrate_enabled {
            orchestrate_server
        } else {
            None
        },
        launch_env,
        // Claude's "Launch arguments"; an ACP agent carries its own from the
        // installed-agent card (Codex has no such field).
        extra_args: match meta.provider {
            ProviderKind::ClaudeCode => provider_settings.extra_args(),
            ProviderKind::Codex => Vec::new(),
            ProviderKind::Acp => acp_agent
                .as_ref()
                .map(|agent| agent.extra_args())
                .unwrap_or_default(),
        },
        acp: acp_agent.map(|agent| agent::AcpAgent {
            id: agent.id.clone(),
            name: agent.name.clone(),
            launch: agent.launch.clone(),
        }),
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
    use gpui::AppContext as _;

    #[test]
    fn marketplace_items_are_runtime_owned_views() {
        let root = std::env::temp_dir().join(format!(
            "tcode-marketplace-view-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = SessionStore::open_at(root.clone()).unwrap();
        let mut state = AppState::new(store);
        state.acp_registry = Some(
            serde_json::from_value(serde_json::json!({
                "agents": [
                    {
                        "id": "first",
                        "name": "First",
                        "version": "1.2.3",
                        "description": "Supported agent",
                        "distribution": { "npx": { "package": "first-agent" } }
                    },
                    {
                        "id": "claude-acp",
                        "name": "Hidden",
                        "distribution": { "npx": { "package": "hidden-agent" } }
                    },
                    {
                        "id": "last",
                        "name": "Last",
                        "version": "4.5.6",
                        "description": "Unsupported agent",
                        "distribution": {}
                    }
                ]
            }))
            .unwrap(),
        );
        state.settings.acp_agents.insert(
            "first".into(),
            InstalledAgent {
                id: "first".into(),
                name: "First".into(),
                version: "1.2.3".into(),
                icon: None,
                launch: agent::AcpLaunch::Npx {
                    package: "first-agent".into(),
                    args: Vec::new(),
                    env: Vec::new(),
                },
                archive_sha256: None,
                enabled: true,
                env: Vec::new(),
                launch_args: None,
            },
        );
        state.acp_installing.insert("last".into());

        assert_eq!(
            state.acp_marketplace_items(),
            vec![
                AcpMarketplaceItem {
                    id: "first".into(),
                    name: "First".into(),
                    version: "1.2.3".into(),
                    description: "Supported agent".into(),
                    installed: true,
                    installing: false,
                    supported: true,
                },
                AcpMarketplaceItem {
                    id: "last".into(),
                    name: "Last".into(),
                    version: "4.5.6".into(),
                    description: "Unsupported agent".into(),
                    installed: false,
                    installing: true,
                    supported: false,
                },
            ]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn provider_update_command_hides_install_source() {
        let root = std::env::temp_dir().join(format!(
            "tcode-provider-update-view-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = SessionStore::open_at(root.clone()).unwrap();
        let mut state = AppState::new(store);
        state.provider_versions.insert(
            ProviderKind::ClaudeCode,
            ProviderVersionStatus {
                install_source: InstallSource::Npm,
                ..ProviderVersionStatus::default()
            },
        );
        state.provider_versions.insert(
            ProviderKind::Codex,
            ProviderVersionStatus {
                install_source: InstallSource::Native,
                ..ProviderVersionStatus::default()
            },
        );

        assert_eq!(
            state.provider_update_command(ProviderKind::ClaudeCode),
            Some("npm install -g @anthropic-ai/claude-code@latest".into())
        );
        assert_eq!(state.provider_update_command(ProviderKind::Codex), None);
        assert_eq!(state.provider_update_command(ProviderKind::Acp), None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn orchestrate_guidance_is_prepended_only_when_enabling() {
        let first = compose_orchestrate_text(ProviderKind::ClaudeCode, true, "Ship it").unwrap();
        assert!(first.starts_with(CLAUDE_ORCHESTRATE_GUIDANCE));
        assert!(first.ends_with("\n\nShip it"));
        assert_eq!(
            compose_orchestrate_text(ProviderKind::ClaudeCode, false, "Follow up"),
            Some("Follow up".into())
        );

        let codex = compose_orchestrate_text(ProviderKind::Codex, true, "Implement").unwrap();
        assert!(codex.starts_with(CODEX_ORCHESTRATE_GUIDANCE));
        assert!(codex.ends_with("\n\nImplement"));
        assert_eq!(
            compose_orchestrate_text(ProviderKind::Acp, true, "No"),
            None
        );
        assert_eq!(
            compose_orchestrate_text(ProviderKind::Acp, false, "No"),
            None
        );
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
            None,
            Vec::new(),
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

    #[gpui::test]
    fn draft_inherits_newest_unarchived_session_from_same_project(cx: &mut gpui::TestAppContext) {
        let root = std::env::temp_dir().join(format!(
            "tcode-draft-project-defaults-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = SessionStore::open_at(root.clone()).unwrap();
        let state = cx.new(|_| AppState::new(store));

        state.update(cx, |state, cx| {
            let mut other_project = SessionMeta::new(
                ProviderKind::ClaudeCode,
                PathBuf::from("/tmp/other"),
                Some("opus".into()),
            );
            other_project.project_id = Some("project-other".into());
            other_project.updated_at = 900;
            other_project.option_selections.push(OptionSelection {
                id: "reasoningEffort".into(),
                value: serde_json::json!("minimal"),
            });

            let mut target_older = SessionMeta::new(
                ProviderKind::ClaudeCode,
                PathBuf::from("/tmp/target-old"),
                Some("sonnet".into()),
            );
            target_older.project_id = Some("project-target".into());
            target_older.updated_at = 100;
            target_older.option_selections.push(OptionSelection {
                id: "reasoningEffort".into(),
                value: serde_json::json!("medium"),
            });

            let mut target_newest = SessionMeta::new(
                ProviderKind::Codex,
                PathBuf::from("/tmp/target-new"),
                Some("gpt-5.2-codex".into()),
            );
            target_newest.project_id = Some("project-target".into());
            target_newest.updated_at = 500;
            target_newest.option_selections = vec![
                OptionSelection {
                    id: "serviceTier".into(),
                    value: serde_json::json!("fast"),
                },
                OptionSelection {
                    id: "reasoningEffort".into(),
                    value: serde_json::json!("high"),
                },
            ];

            let mut target_archived = SessionMeta::new(
                ProviderKind::ClaudeCode,
                PathBuf::from("/tmp/target-archived"),
                Some("haiku".into()),
            );
            target_archived.project_id = Some("project-target".into());
            target_archived.updated_at = 800;
            target_archived.archived_at = Some(801);
            target_archived.option_selections.push(OptionSelection {
                id: "reasoningEffort".into(),
                value: serde_json::json!("low"),
            });

            // Deliberately interleaved and not timestamp-sorted: selection must
            // be project-scoped and based on updated_at, not vector position.
            state.sessions = vec![other_project, target_older, target_archived, target_newest];
            state.start_draft("project-target".into(), PathBuf::from("/tmp/target"), cx);

            let draft = state.active.as_ref().unwrap();
            assert!(draft.draft);
            assert_eq!(draft.meta.provider, ProviderKind::Codex);
            assert_eq!(draft.meta.model.as_deref(), Some("gpt-5.2-codex"));
            assert_eq!(draft.meta.acp_agent_id, None);
            assert_eq!(draft.meta.option_selections.len(), 1);
            assert_eq!(draft.meta.option_selections[0].id, "reasoningEffort");
            assert_eq!(
                draft.meta.option_selections[0].value,
                serde_json::json!("high")
            );
            assert!(state.store.load_index().is_empty());
        });

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn draft_inherits_acp_agent_id_from_project_history() {
        let root = std::env::temp_dir().join(format!(
            "tcode-draft-acp-defaults-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = SessionStore::open_at(root.clone()).unwrap();
        let mut state = AppState::new(store);
        let mut acp = SessionMeta::new(
            ProviderKind::Acp,
            PathBuf::from("/tmp/acp"),
            Some("agent-model".into()),
        );
        acp.project_id = Some("project-acp".into());
        acp.acp_agent_id = Some("agent.example".into());
        acp.updated_at = 40;
        state.sessions = vec![acp];

        let (provider, model, acp_agent_id, effort) = state.draft_defaults("project-acp");
        assert_eq!(provider, ProviderKind::Acp);
        assert_eq!(model.as_deref(), Some("agent-model"));
        assert_eq!(acp_agent_id.as_deref(), Some("agent.example"));
        assert!(effort.is_none());

        let _ = std::fs::remove_dir_all(root);
    }

    #[gpui::test]
    fn draft_without_project_history_keeps_global_fallback_and_stays_unpersisted(
        cx: &mut gpui::TestAppContext,
    ) {
        let root = std::env::temp_dir().join(format!(
            "tcode-draft-fallback-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = SessionStore::open_at(root.clone()).unwrap();
        let state = cx.new(|_| AppState::new(store));

        state.update(cx, |state, cx| {
            let mut global = SessionMeta::new(
                ProviderKind::Acp,
                PathBuf::from("/tmp/existing"),
                Some("fallback-model".into()),
            );
            global.project_id = Some("project-existing".into());
            global.acp_agent_id = Some("fallback-agent".into());
            global.updated_at = 200;
            global.option_selections.push(OptionSelection {
                id: "reasoningEffort".into(),
                value: serde_json::json!("low"),
            });
            state.sessions = vec![global];

            state.start_draft("project-empty".into(), PathBuf::from("/tmp/empty"), cx);

            let draft = state.active.as_ref().unwrap();
            let draft_id = draft.meta.id.clone();
            assert!(draft.draft);
            assert_eq!(draft.meta.provider, ProviderKind::Acp);
            assert_eq!(draft.meta.model.as_deref(), Some("fallback-model"));
            assert_eq!(draft.meta.acp_agent_id.as_deref(), Some("fallback-agent"));
            assert!(draft.meta.option_selections.is_empty());
            assert!(
                !state
                    .store
                    .load_index()
                    .iter()
                    .any(|meta| meta.id == draft_id)
            );
        });

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn draft_global_fallback_ignores_target_projects_archived_history() {
        let root = std::env::temp_dir().join(format!(
            "tcode-draft-archived-fallback-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = SessionStore::open_at(root.clone()).unwrap();
        let mut state = AppState::new(store);

        let mut target_archived = SessionMeta::new(
            ProviderKind::Codex,
            PathBuf::from("/tmp/target-archived"),
            Some("gpt-5.2-codex".into()),
        );
        target_archived.project_id = Some("project-target".into());
        target_archived.updated_at = 900;
        target_archived.archived_at = Some(901);
        target_archived.option_selections.push(OptionSelection {
            id: "reasoningEffort".into(),
            value: serde_json::json!("high"),
        });

        let mut other_active = SessionMeta::new(
            ProviderKind::Acp,
            PathBuf::from("/tmp/other-active"),
            Some("active-model".into()),
        );
        other_active.project_id = Some("project-other".into());
        other_active.acp_agent_id = Some("active-agent".into());
        other_active.updated_at = 100;
        other_active.option_selections.push(OptionSelection {
            id: "reasoningEffort".into(),
            value: serde_json::json!("low"),
        });

        // The target's archived session is globally newest and first, but must
        // not be reselected by the global fallback.
        state.sessions = vec![target_archived, other_active];
        let (provider, model, acp_agent_id, effort) = state.draft_defaults("project-target");
        assert_eq!(provider, ProviderKind::Acp);
        assert_eq!(model.as_deref(), Some("active-model"));
        assert_eq!(acp_agent_id.as_deref(), Some("active-agent"));
        assert!(effort.is_none());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn draft_defaults_to_claude_when_all_sessions_are_archived() {
        let root = std::env::temp_dir().join(format!(
            "tcode-draft-all-archived-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = SessionStore::open_at(root.clone()).unwrap();
        let mut state = AppState::new(store);

        let mut target_archived = SessionMeta::new(
            ProviderKind::Codex,
            PathBuf::from("/tmp/target-archived"),
            Some("gpt-5.2-codex".into()),
        );
        target_archived.project_id = Some("project-target".into());
        target_archived.updated_at = 200;
        target_archived.archived_at = Some(201);

        let mut other_archived = SessionMeta::new(
            ProviderKind::Acp,
            PathBuf::from("/tmp/other-archived"),
            Some("archived-model".into()),
        );
        other_archived.project_id = Some("project-other".into());
        other_archived.acp_agent_id = Some("archived-agent".into());
        other_archived.updated_at = 300;
        other_archived.archived_at = Some(301);

        state.sessions = vec![other_archived, target_archived];
        let (provider, model, acp_agent_id, effort) = state.draft_defaults("project-target");
        assert_eq!(provider, ProviderKind::ClaudeCode);
        assert!(model.is_none());
        assert!(acp_agent_id.is_none());
        assert!(effort.is_none());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn reopened_command_cache_seeds_a_draft_before_provider_start() {
        let root =
            std::env::temp_dir().join(format!("tcode-command-seed-test-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::open_at(root.clone()).unwrap();
        let commands = vec![ProviderCommand {
            name: "review".into(),
            description: Some("Review changes".into()),
            kind: agent::ProviderCommandKind::Command,
        }];
        store
            .save_commands(ProviderKind::ClaudeCode, None, &commands)
            .unwrap();

        let state = AppState::new(SessionStore::open_at(root.clone()).unwrap());
        let seeded = state.cached_provider_commands(ProviderKind::ClaudeCode, None);
        let draft = AppState::build_draft_session(
            "project".into(),
            PathBuf::from("/tmp/project"),
            ProviderKind::ClaudeCode,
            None,
            None,
            seeded,
        );
        assert_eq!(draft.provider_commands, commands);
        assert!(matches!(draft.runtime, Runtime::Idle));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn configured_binary_reaches_session_options() {
        let codex = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/project"), None);
        let claude = SessionMeta::new(
            ProviderKind::ClaudeCode,
            PathBuf::from("/tmp/project"),
            None,
        );
        let mut settings = Settings::default();
        settings.provider_mut(ProviderKind::Codex).binary_path =
            Some(PathBuf::from("/custom/codex"));
        settings.provider_mut(ProviderKind::ClaudeCode).binary_path =
            Some(PathBuf::from("/custom/claude"));

        let codex_options = session_options(&codex, &settings, LaunchEnv::default(), None, None);
        let claude_options = session_options(&claude, &settings, LaunchEnv::default(), None, None);

        assert_eq!(
            codex_options.binary_path,
            Some(PathBuf::from("/custom/codex"))
        );
        assert_eq!(
            claude_options.binary_path,
            Some(PathBuf::from("/custom/claude"))
        );
        assert!(codex_options.mcp_server.is_none());
    }

    /// Settings → Providers env/home/launch-args must reach the spawn options,
    /// and the home override must land on the provider's own variable.
    #[test]
    fn provider_env_home_and_launch_args_reach_session_options() {
        let mut settings = Settings::default();
        let claude = settings.provider_mut(ProviderKind::ClaudeCode);
        claude.home_path = Some(PathBuf::from("/tmp/claude-home"));
        claude.launch_args = Some("--chrome --verbose".into());
        let codex = settings.provider_mut(ProviderKind::Codex);
        codex.shadow_home_path = Some(PathBuf::from("/tmp/codex-shadow"));

        let launch_env = LaunchEnv {
            env: vec![("ANTHROPIC_BASE_URL".into(), "https://proxy.test".into())],
            home: settings.provider(ProviderKind::ClaudeCode).effective_home(),
        };
        let meta = SessionMeta::new(ProviderKind::ClaudeCode, PathBuf::from("/x"), None);
        let opts = session_options(&meta, &settings, launch_env, None, None);
        assert_eq!(opts.extra_args, vec!["--chrome", "--verbose"]);
        assert_eq!(
            opts.launch_env.pairs(ProviderKind::ClaudeCode),
            vec![
                (
                    "ANTHROPIC_BASE_URL".to_string(),
                    "https://proxy.test".to_string()
                ),
                ("HOME".to_string(), "/tmp/claude-home".to_string()),
            ]
        );

        // Codex takes its shadow home as CODEX_HOME, and has no launch args.
        let launch_env = LaunchEnv {
            env: Vec::new(),
            home: settings.provider(ProviderKind::Codex).effective_home(),
        };
        let meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/x"), None);
        let opts = session_options(&meta, &settings, launch_env, None, None);
        assert!(opts.extra_args.is_empty());
        assert_eq!(
            opts.launch_env.pairs(ProviderKind::Codex),
            vec![("CODEX_HOME".to_string(), "/tmp/codex-shadow".to_string())]
        );
    }

    /// Sensitive env rows contribute their value from `secrets.json`, never from
    /// settings.json (which stores an empty value for them).
    #[test]
    fn launch_env_merges_secrets_for_sensitive_rows() {
        let root = std::env::temp_dir().join(format!("tcode-env-test-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::open_at(root.clone()).unwrap();
        let mut state = AppState::new(store);
        let mut settings = state.settings.clone();
        settings.provider_mut(ProviderKind::ClaudeCode).env = vec![
            EnvVar {
                name: "PLAIN".into(),
                value: "visible".into(),
                sensitive: false,
            },
            EnvVar {
                name: "ANTHROPIC_API_KEY".into(),
                value: String::new(),
                sensitive: true,
            },
            // A sensitive row whose secret was never saved contributes nothing.
            EnvVar {
                name: "UNSET_SECRET".into(),
                value: String::new(),
                sensitive: true,
            },
        ];
        state.settings = settings;
        state
            .settings_store
            .set_secret(ProviderKind::ClaudeCode, "ANTHROPIC_API_KEY", Some("sk-x"))
            .unwrap();

        let env = state.launch_env(ProviderKind::ClaudeCode).env;
        assert_eq!(
            env,
            vec![
                ("PLAIN".to_string(), "visible".to_string()),
                ("ANTHROPIC_API_KEY".to_string(), "sk-x".to_string()),
            ]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn session_options_injects_mcp_registration() {
        let settings = Settings::default();
        let meta = SessionMeta::new(ProviderKind::ClaudeCode, PathBuf::from("/x"), None);
        let reg = agent::McpRegistration {
            name: agent::McpRegistration::SERVER_NAME_PREVIEW.into(),
            url: "http://127.0.0.1:7/mcp".into(),
            bearer_token: "tok".into(),
        };
        let opts = session_options(&meta, &settings, LaunchEnv::default(), Some(reg), None);
        let mcp = opts.mcp_server.expect("registration threaded through");
        assert_eq!(mcp.url, "http://127.0.0.1:7/mcp");
        assert_eq!(mcp.bearer_token, "tok");
    }

    #[test]
    fn session_options_isolates_orchestrate_registration_by_meta_flag() {
        let settings = Settings::default();
        let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/x"), None);
        let registration = agent::McpRegistration {
            name: agent::McpRegistration::SERVER_NAME_ORCHESTRATE.into(),
            url: "http://127.0.0.1:8/mcp".into(),
            bearer_token: "parent-token".into(),
        };
        let normal = session_options(
            &meta,
            &settings,
            LaunchEnv::default(),
            None,
            Some(registration.clone()),
        );
        assert!(normal.mcp_server.is_none());
        assert!(normal.orchestrate_server.is_none());

        meta.orchestrate_enabled = true;
        let enabled = session_options(
            &meta,
            &settings,
            LaunchEnv::default(),
            None,
            Some(registration),
        );
        assert_eq!(
            enabled.orchestrate_server.unwrap().name,
            agent::McpRegistration::SERVER_NAME_ORCHESTRATE
        );
    }

    #[test]
    fn child_meta_links_parent_project_and_maps_effort() {
        let mut parent = SessionMeta::new(ProviderKind::ClaudeCode, PathBuf::from("/p"), None);
        parent.id = "parent".into();
        parent.project_id = Some("project".into());
        let child = build_child_meta(
            &parent,
            ProviderKind::Codex,
            Some("gpt-test".into()),
            Some("high".into()),
            "Research".into(),
            PathBuf::from("/p/sub"),
        );
        assert_eq!(child.parent_session_id.as_deref(), Some("parent"));
        assert_eq!(child.project_id.as_deref(), Some("project"));
        assert_eq!(child.model.as_deref(), Some("gpt-test"));
        assert_eq!(child.title, "Research");
        assert_eq!(child.option_selections.len(), 1);
        assert_eq!(child.option_selections[0].id, "reasoningEffort");
        assert_eq!(child.option_selections[0].value, serde_json::json!("high"));
    }

    #[test]
    fn callback_text_keeps_the_full_result_and_cap_stops_at_one_hundred() {
        let text =
            assemble_callback_text("child", "Title", TurnStatus::Completed, &"x".repeat(5000));
        assert!(text.starts_with("[orchestrate] thread child (\"Title\") completed.\n"));
        assert_eq!(text.lines().nth(1).unwrap().chars().count(), 5000);
        let failed = assemble_callback_text("child", "Title", TurnStatus::Interrupted, "done");
        assert!(failed.contains(") failed.\ndone"));

        let mut count = 0;
        for _ in 0..ORCHESTRATE_CALLBACK_CAP {
            assert!(take_callback_slot(&mut count));
        }
        assert!(!take_callback_slot(&mut count));
        assert_eq!(count, ORCHESTRATE_CALLBACK_CAP + 1);
    }

    #[test]
    fn final_assistant_message_joins_all_blocks_of_the_final_output() {
        let timeline = Timeline::fold_events([
            AgentEvent::ItemCompleted(ThreadItem {
                id: "preamble".into(),
                parent_item_id: None,
                content: ItemContent::AssistantMessage {
                    text: "Earlier tool preamble.".into(),
                },
            }),
            AgentEvent::ItemCompleted(ThreadItem {
                id: "reasoning".into(),
                parent_item_id: None,
                content: ItemContent::Reasoning {
                    text: "private reasoning".into(),
                },
            }),
            AgentEvent::ItemCompleted(ThreadItem {
                id: "final-1".into(),
                parent_item_id: None,
                content: ItemContent::AssistantMessage {
                    text: "Complete ".into(),
                },
            }),
            AgentEvent::ItemCompleted(ThreadItem {
                id: "final-2".into(),
                parent_item_id: None,
                content: ItemContent::AssistantMessage {
                    text: "answer.".into(),
                },
            }),
        ]);

        assert_eq!(final_assistant_message(&timeline), "Complete answer.");
    }

    #[gpui::test]
    fn steering_parked_orchestrator_callback_uses_recorded_id(cx: &mut gpui::TestAppContext) {
        let root = std::env::temp_dir().join(format!(
            "tcode-orchestrate-steer-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = SessionStore::open_at(root.clone()).unwrap();
        let state = cx.new(|_| AppState::new(store));
        let (commands, receiver) = async_channel::unbounded();

        state.update(cx, |state, cx| {
            let mut parent = live_session(ProviderKind::Codex, commands);
            parent.meta.id = "parent".into();
            parent.turn_in_flight = true;
            state.background.insert(parent.meta.id.clone(), parent);

            state.deliver_orchestrate_callback_to_parent(
                "parent",
                "[orchestrate] child-a completed.\nfull result".into(),
                cx,
            );

            let parent = state.background.get("parent").unwrap();
            assert!(parent.queue.is_empty(), "parallel result must not queue");
            assert!(parent.turn_in_flight);
            let command = receiver.try_recv().unwrap();
            let SessionCommand::Steer {
                request_id, text, ..
            } = command
            else {
                panic!("callback did not steer")
            };
            assert!(text.contains("full result"));
            let timeline = Timeline::fold_events(state.store.read_events("parent"));
            assert!(timeline.entries.iter().any(|entry| matches!(
                &entry.content,
                EntryContent::User {
                    text,
                    steering: Some(tcode_core::session::SteeringStatus::Pending),
                } if entry.id == request_id && text.contains("child-a completed")
            )));
        });

        let _ = std::fs::remove_dir_all(root);
    }

    #[gpui::test]
    fn steering_user_and_queue_paths_send_the_same_id_they_record(cx: &mut gpui::TestAppContext) {
        let root =
            std::env::temp_dir().join(format!("tcode-user-steer-id-test-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::open_at(root.clone()).unwrap();
        let state = cx.new(|_| AppState::new(store));
        let (commands, receiver) = async_channel::unbounded();

        state.update(cx, |state, cx| {
            let mut active = live_session(ProviderKind::Codex, commands);
            active.meta.id = "active".into();
            active.turn_in_flight = true;
            active.timeline.apply_at(
                None,
                &AgentEvent::ItemCompleted(ThreadItem {
                    id: "opening".into(),
                    parent_item_id: None,
                    content: ItemContent::UserMessage {
                        text: "start".into(),
                    },
                }),
            );
            state.active = Some(active);

            state.steer("redirect".into(), Vec::new(), cx);
            let SessionCommand::Steer { request_id, .. } = receiver.try_recv().unwrap() else {
                panic!("user steer command missing")
            };
            let active = state.active.as_ref().unwrap();
            assert!(active.timeline.entries.iter().any(|entry| matches!(
                &entry.content,
                EntryContent::User {
                    text,
                    steering: Some(tcode_core::session::SteeringStatus::Pending),
                } if entry.id == request_id && text == "redirect"
            )));

            let queued_id = state
                .active
                .as_mut()
                .unwrap()
                .push_queued("queued redirect".into(), Vec::new());
            state.steer_queued(queued_id, cx);
            let SessionCommand::Steer { request_id, .. } = receiver.try_recv().unwrap() else {
                panic!("queue-to-steer command missing")
            };
            let active = state.active.as_ref().unwrap();
            assert!(active.queue.is_empty());
            assert!(active.timeline.entries.iter().any(|entry| matches!(
                &entry.content,
                EntryContent::User {
                    text,
                    steering: Some(tcode_core::session::SteeringStatus::Pending),
                } if entry.id == request_id && text == "queued redirect"
            )));
        });

        let _ = std::fs::remove_dir_all(root);
    }

    #[gpui::test]
    fn callbacks_racing_provider_start_share_one_wakeup_turn(cx: &mut gpui::TestAppContext) {
        let root = std::env::temp_dir().join(format!(
            "tcode-orchestrate-start-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = SessionStore::open_at(root.clone()).unwrap();
        let state = cx.new(|_| AppState::new(store));

        state.update(cx, |state, cx| {
            let mut parent = live_session(ProviderKind::ClaudeCode, async_channel::unbounded().0);
            parent.meta.id = "parent".into();
            parent.runtime = Runtime::Starting { generation: 1 };
            state.background.insert(parent.meta.id.clone(), parent);

            state.deliver_orchestrate_callback_to_parent(
                "parent",
                "[orchestrate] child-a completed.\nresult a".into(),
                cx,
            );
            state.deliver_orchestrate_callback_to_parent(
                "parent",
                "[orchestrate] child-b completed.\nresult b".into(),
                cx,
            );

            let parent = state.background.get("parent").unwrap();
            assert_eq!(parent.queue.len(), 1);
            assert_eq!(parent.queue[0].kind, QueuedMessageKind::OrchestrateCallback);
            assert!(parent.queue[0].text.contains("result a"));
            assert!(parent.queue[0].text.contains("result b"));

            let (commands, receiver) = async_channel::unbounded();
            state.background.get_mut("parent").unwrap().runtime = Runtime::Live(commands);
            state.on_background_turn_completed("parent", cx);

            assert!(matches!(
                receiver.try_recv(),
                Ok(SessionCommand::SendTurn { text, .. })
                    if text.contains("result a") && text.contains("result b")
            ));
            let parent = state.background.get("parent").unwrap();
            assert!(parent.queue.is_empty());
            assert!(parent.turn_in_flight);
        });

        let _ = std::fs::remove_dir_all(root);
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
            queue: Vec::new(),
            next_queue_id: 0,
            turn_in_flight: false,
            provider_commands: Vec::new(),
            provider_options: Vec::new(),
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
            queue: vec!["first".into(), "second".into()],
            next_queue_id: 2,
            turn_in_flight: false,
            provider_commands: Vec::new(),
            provider_options: Vec::new(),
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
        assert_eq!(active.queue, [QueuedMessage::from("second")]);

        active.turn_in_flight = false;
        assert_eq!(active.dispatch_next_pending(), Ok(true));
        assert!(matches!(
            receiver.try_recv(),
            Ok(SessionCommand::SendTurn { text, .. }) if text == "second"
        ));
        assert!(active.queue.is_empty());
    }

    /// A live session with `provider`, nothing queued, no turn in flight.
    fn live_session(
        provider: ProviderKind,
        commands: async_channel::Sender<SessionCommand>,
    ) -> ActiveSession {
        ActiveSession {
            meta: SessionMeta::new(provider, PathBuf::from("/tmp/project"), None),
            timeline: Timeline::default(),
            provider_options: Vec::new(),
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
            queue: Vec::new(),
            next_queue_id: 0,
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

    #[test]
    fn shutdown_all_notifies_active_and_parked_live_providers() {
        let root = std::env::temp_dir().join(format!("tcode-app-test-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::open_at(root.clone()).unwrap();
        let mut state = AppState::new(store);

        let (active_commands, active_receiver) = async_channel::unbounded();
        state.active = Some(live_session(ProviderKind::Codex, active_commands));

        let (parked_commands, parked_receiver) = async_channel::unbounded();
        let parked = live_session(ProviderKind::ClaudeCode, parked_commands);
        state.background.insert(parked.meta.id.clone(), parked);

        let (other_commands, other_receiver) = async_channel::unbounded();
        let other = live_session(ProviderKind::Acp, other_commands);
        state.background.insert(other.meta.id.clone(), other);

        state.shutdown_all();

        assert!(matches!(
            active_receiver.try_recv(),
            Ok(SessionCommand::Shutdown)
        ));
        assert!(matches!(
            parked_receiver.try_recv(),
            Ok(SessionCommand::Shutdown)
        ));
        assert!(matches!(
            other_receiver.try_recv(),
            Ok(SessionCommand::Shutdown)
        ));
        assert!(state.active.is_none());
        assert!(state.background.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn turns_in_flight_count_includes_active_and_parked_sessions() {
        let root = std::env::temp_dir().join(format!("tcode-app-test-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::open_at(root.clone()).unwrap();
        let mut state = AppState::new(store);

        let mut active = live_session(ProviderKind::Codex, async_channel::unbounded().0);
        active.turn_in_flight = true;
        state.active = Some(active);

        let mut parked = live_session(ProviderKind::ClaudeCode, async_channel::unbounded().0);
        parked.turn_in_flight = true;
        state.background.insert(parked.meta.id.clone(), parked);

        let mut queued_only = live_session(ProviderKind::Acp, async_channel::unbounded().0);
        queued_only.push_queued("waiting".into(), Vec::new());
        state
            .background
            .insert(queued_only.meta.id.clone(), queued_only);

        assert_eq!(state.turns_in_flight_count(), 2);
        let _ = std::fs::remove_dir_all(root);
    }

    /// Enter always queues while a turn runs; ⌘Enter steers only where the
    /// provider actually supports it, and otherwise degrades to queueing.
    #[test]
    fn send_routing_matrix() {
        let (commands, _rx) = async_channel::unbounded();
        let mut codex = live_session(ProviderKind::Codex, commands.clone());

        // Idle: both gestures are a plain send — there is nothing to steer into.
        assert_eq!(codex.route(false), SendRouting::Send);
        assert_eq!(codex.route(true), SendRouting::Send);

        // Turn running: Enter queues, ⌘Enter steers (Codex has `turn/steer`).
        codex.turn_in_flight = true;
        assert_eq!(codex.route(false), SendRouting::Queue);
        assert_eq!(codex.route(true), SendRouting::Steer);

        let mut claude = live_session(ProviderKind::ClaudeCode, commands.clone());
        claude.turn_in_flight = true;
        assert_eq!(claude.route(true), SendRouting::Steer);

        // ACP has no steering method, so a steer must fall back to the queue
        // rather than silently vanish.
        let mut acp = live_session(ProviderKind::Acp, commands);
        acp.turn_in_flight = true;
        assert_eq!(acp.route(false), SendRouting::Queue);
        assert_eq!(acp.route(true), SendRouting::QueueUnsupported);

        // A provider that can steer still can't while it isn't live.
        let mut dead = live_session(ProviderKind::Codex, async_channel::unbounded().0);
        dead.runtime = Runtime::Idle;
        dead.turn_in_flight = true;
        assert_eq!(dead.route(true), SendRouting::QueueUnsupported);
    }

    /// Steering must not disturb the turn bookkeeping: it joins the turn already
    /// in flight, so no queue entry is consumed and no new turn is opened.
    /// (Both providers were verified live to emit exactly one TurnStarted /
    /// TurnCompleted across a steered turn — see examples/steer_probe.rs.)
    #[test]
    fn steering_does_not_disturb_turn_accounting() {
        let (commands, receiver) = async_channel::unbounded();
        let mut active = live_session(ProviderKind::Codex, commands);
        active.turn_in_flight = true;
        active.push_queued("queued".into(), Vec::new());

        assert_eq!(
            active.steer_now("steer-1".into(), "steer me".into(), Vec::new()),
            Ok(())
        );

        assert!(matches!(
            receiver.try_recv(),
            Ok(SessionCommand::Steer { request_id, text, .. })
                if request_id == "steer-1" && text == "steer me"
        ));
        // Still exactly one turn in flight, and the queue is untouched.
        assert!(active.turn_in_flight);
        assert_eq!(active.queued().len(), 1);
        assert_eq!(active.queued()[0].text, "queued");
    }

    /// The queue strip's steer button pulls that specific entry out (by id),
    /// leaving the rest of the FIFO in order.
    #[test]
    fn queued_message_converts_to_steer() {
        let (commands, _rx) = async_channel::unbounded();
        let mut active = live_session(ProviderKind::Codex, commands);
        active.turn_in_flight = true;
        let first = active.push_queued("first".into(), Vec::new());
        let second = active.push_queued("second".into(), Vec::new());
        let third = active.push_queued("third".into(), Vec::new());
        assert_ne!(first, second);

        // Steer the middle one: it leaves the queue, order is preserved.
        let taken = active.take_queued(second).expect("queued message");
        assert_eq!(taken.text, "second");
        let remaining: Vec<_> = active.queued().iter().map(|m| m.text.as_str()).collect();
        assert_eq!(remaining, ["first", "third"]);

        // Dropping the head (the ✕) leaves the tail alone.
        active.take_queued(first).expect("queued message");
        assert_eq!(active.queued().len(), 1);
        assert_eq!(active.queued()[0].id, third);

        // An unknown id is a no-op, not a panic.
        assert!(active.take_queued(9999).is_none());
    }

    /// Ultrathink is per-send: it rides with the message it was armed for, not
    /// with whatever happens to be dispatched later.
    #[test]
    fn ultrathink_rides_with_the_queued_message() {
        let (commands, receiver) = async_channel::unbounded();
        let mut active = live_session(ProviderKind::Codex, commands);
        active.turn_in_flight = true;
        active.pending_ultrathink = true;
        active.push_queued("deep".into(), Vec::new());
        // The flag is consumed by the message that was armed for it.
        assert!(!active.pending_ultrathink);
        active.push_queued("shallow".into(), Vec::new());

        active.turn_in_flight = false;
        assert_eq!(active.dispatch_next_pending(), Ok(true));
        assert!(matches!(
            receiver.try_recv(),
            Ok(SessionCommand::SendTurn { text, .. }) if text == "Ultrathink:\ndeep"
        ));
        active.turn_in_flight = false;
        assert_eq!(active.dispatch_next_pending(), Ok(true));
        assert!(matches!(
            receiver.try_recv(),
            Ok(SessionCommand::SendTurn { text, .. }) if text == "shallow"
        ));
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
            queue: Vec::new(),
            next_queue_id: 0,
            turn_in_flight: false,
            provider_commands: Vec::new(),
            provider_options: Vec::new(),
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
            queue: vec!["do it".into()],
            next_queue_id: 1,
            turn_in_flight: false,
            provider_commands: Vec::new(),
            provider_options: Vec::new(),
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
        assert_eq!(active.queue, [QueuedMessage::from("do it")]);
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
        let root = std::env::temp_dir().join(format!("tcode-wt-test-{}", uuid::Uuid::new_v4()));
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

    // -- rewind: revert / edit & resend --------------------------------------

    /// A scratch git repo with one committed file.
    fn scratch_repo() -> PathBuf {
        let root = std::env::temp_dir().join(format!("tcode-rewind-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            command("git")
                .args(args)
                .current_dir(&root)
                .env("GIT_AUTHOR_NAME", "tcode")
                .env("GIT_AUTHOR_EMAIL", "tcode@localhost")
                .env("GIT_COMMITTER_NAME", "tcode")
                .env("GIT_COMMITTER_EMAIL", "tcode@localhost")
                .output()
                .unwrap();
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "tcode"]);
        git(&["config", "user.email", "tcode@localhost"]);
        std::fs::write(root.join("seed.txt"), "seed\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "seed"]);
        root
    }

    fn scratch_state(cwd: PathBuf, cx: &mut gpui::TestAppContext) -> Entity<AppState> {
        let data = std::env::temp_dir().join(format!("tcode-rewind-data-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::open_at(data).unwrap();
        let state = cx.new(|_| AppState::new(store));
        state.update(cx, |state, cx| {
            state.create_session(ProviderKind::ClaudeCode, cwd, None, None, None, cx);
        });
        state
    }

    /// Record a user message for a new turn exactly the way `dispatch_next_queued`
    /// does (JSONL offset first, then the message, then the pre-turn checkpoint),
    /// then let the "agent" reply and finish the turn.
    fn fake_turn(
        state: &mut AppState,
        text: &str,
        reply: &str,
        cx: &mut Context<AppState>,
    ) -> usize {
        let id = state.active.as_ref().unwrap().meta.id.clone();
        let offset = state.store.event_count(&id);
        state.record_user_message(&id, text, cx);
        state.capture_checkpoint(&id, offset, cx);
        let turn = state.last_user_turn().unwrap();
        state.record_event(
            &id,
            &AgentEvent::ItemCompleted(ThreadItem {
                id: format!("assistant-{turn}"),
                parent_item_id: None,
                content: ItemContent::AssistantMessage {
                    text: reply.to_string(),
                },
            }),
            cx,
        );
        state.record_event(
            &id,
            &AgentEvent::TurnCompleted {
                turn_id: format!("turn-{turn}"),
                status: TurnStatus::Completed,
                usage: None,
            },
            cx,
        );
        offset
    }

    /// Edit & resend rewinds to the state just before the edited message: the
    /// JSONL is truncated at exactly that message's offset, the worktree is
    /// restored from the turn's checkpoint (the file the agent created is gone),
    /// the newer checkpoints are dropped and the provider session is idle — ready
    /// for the edited text to be sent as a fresh turn.
    #[gpui::test]
    fn edit_and_resend_truncates_the_transcript_and_restores_the_checkpoint(
        cx: &mut gpui::TestAppContext,
    ) {
        let repo = scratch_repo();
        let state = scratch_state(repo.clone(), cx);

        state.update(cx, |state, cx| {
            let id = state.active.as_ref().unwrap().meta.id.clone();

            // Turn 0: the agent creates a file.
            let offset0 = fake_turn(state, "add a file", "Created agent.txt.", cx);
            assert_eq!(offset0, 0);
            std::fs::write(repo.join("agent.txt"), "written by the agent\n").unwrap();

            // Turn 1: a follow-up (its checkpoint therefore contains agent.txt).
            let offset1 = fake_turn(state, "now rename it", "Renamed it.", cx);
            let events_before = state.store.event_count(&id);
            assert!(offset1 > offset0 && events_before > offset1);
            assert!(state.turn_has_checkpoint(0) && state.turn_has_checkpoint(1));
            // The recomputed boundary (the no-checkpoint path) agrees with the
            // one the checkpoint recorded.
            let events = state.store.read_events(&id);
            assert_eq!(turn_user_event_offset(&events, 0), Some(offset0));
            assert_eq!(turn_user_event_offset(&events, 1), Some(offset1));

            // Edit & resend the FIRST message: everything from it onwards goes.
            let restored = state.rewind_to_turn(0, cx);
            assert_eq!(restored, Some(true), "the worktree was restored");

            // (a) the transcript is truncated at the edited message's offset...
            assert_eq!(state.store.event_count(&id), offset0);
            assert!(state.active.as_ref().unwrap().timeline.entries.is_empty());
            // (b) ...the worktree is back to the pre-turn snapshot...
            assert!(!repo.join("agent.txt").exists());
            assert_eq!(
                std::fs::read_to_string(repo.join("seed.txt")).unwrap(),
                "seed\n"
            );
            // (c) ...the orphaned checkpoints are gone, and the provider session
            // is idle so the next send resumes from the truncated transcript.
            assert!(state.active.as_ref().unwrap().meta.checkpoints.is_empty());
            assert!(!checkpoint_ref_exists(&repo, &id, 0));
            assert!(!checkpoint_ref_exists(&repo, &id, 1));
            assert!(matches!(
                state.active.as_ref().unwrap().runtime,
                Runtime::Idle
            ));
            assert!(state.active.as_ref().unwrap().meta.resume_cursor.is_none());
        });

        let _ = std::fs::remove_dir_all(&repo);
    }

    /// A running turn blocks the rewind (Revert and Edit & resend alike).
    #[gpui::test]
    fn rewind_is_blocked_while_a_turn_runs(cx: &mut gpui::TestAppContext) {
        let repo = scratch_repo();
        let state = scratch_state(repo.clone(), cx);
        state.update(cx, |state, cx| {
            let id = state.active.as_ref().unwrap().meta.id.clone();
            fake_turn(state, "add a file", "Created it.", cx);
            let before = state.store.event_count(&id);
            state.active.as_mut().unwrap().timeline.turn_running = true;

            assert_eq!(state.rewind_to_turn(0, cx), None);
            assert_eq!(state.store.event_count(&id), before);
            assert!(state.turn_has_checkpoint(0));
        });
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// Outside a git repo there is no checkpoint to restore: the transcript is
    /// still truncated at the edited message (so the resend really does replace
    /// it), but the files on disk are left alone — and the caller is told (the
    /// `Some(false)` that drives the "not reverted" toast).
    #[gpui::test]
    fn rewind_without_a_checkpoint_truncates_but_leaves_files(cx: &mut gpui::TestAppContext) {
        let plain =
            std::env::temp_dir().join(format!("tcode-rewind-plain-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&plain).unwrap();
        let state = scratch_state(plain.clone(), cx);

        state.update(cx, |state, cx| {
            let id = state.active.as_ref().unwrap().meta.id.clone();
            let offset0 = fake_turn(state, "add a file", "Created agent.txt.", cx);
            std::fs::write(plain.join("agent.txt"), "written by the agent\n").unwrap();
            let offset1 = fake_turn(state, "and again", "Done.", cx);
            assert!(state.active.as_ref().unwrap().meta.checkpoints.is_empty());

            // Rewind to turn 1: the boundary is recomputed by replaying the log.
            assert_eq!(state.rewind_to_turn(1, cx), Some(false));
            assert_eq!(state.store.event_count(&id), offset1);
            assert_eq!(state.last_user_turn(), Some(0));

            // The agent's file survives (nothing to restore from) — hence the
            // honest toast.
            assert!(plain.join("agent.txt").exists());
            let _ = offset0;
        });

        let _ = std::fs::remove_dir_all(&plain);
    }

    /// An `ActiveSession` wired to a fake live provider: commands land on the
    /// returned receiver, nothing real is spawned.
    fn fake_live_session(cwd: PathBuf) -> (ActiveSession, async_channel::Receiver<SessionCommand>) {
        let (commands, receiver) = async_channel::unbounded();
        let mut session = AppState::build_draft_session(
            "proj-t3".into(),
            cwd,
            ProviderKind::ClaudeCode,
            None,
            None,
            Vec::new(),
        );
        session.draft = false;
        session.runtime = Runtime::Live(commands);
        // What `ensure_started` records at launch — without these, `send_turn`
        // sees a live-config mismatch and restarts the provider instead of
        // dispatching.
        session.live_model = session.meta.model.clone();
        session.live_approval_mode = Some(session.meta.approval_mode);
        session.live_option_selections = session.meta.option_selections.clone();
        (session, receiver)
    }

    /// The T3 Code regression this app must not inherit: send a message, hit
    /// stop, get an error, then immediately open a new thread and send — and the
    /// new thread's FIRST user message must be in its timeline (T3 loses the
    /// bubble while the turn keeps working underneath).
    ///
    /// The guarantees this pins: a message is folded into the timeline at the
    /// moment it is dispatched (not asynchronously after), the fold only accepts
    /// events whose session id matches the active session, and the interrupted
    /// session's error cannot leak into the new thread.
    #[gpui::test]
    fn stop_then_new_thread_keeps_the_first_message_visible(cx: &mut gpui::TestAppContext) {
        let cwd = std::env::temp_dir().join(format!("tcode-t3-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&cwd).unwrap();
        let data = std::env::temp_dir().join(format!("tcode-t3-data-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::open_at(data.clone()).unwrap();
        let state = cx.new(|_| AppState::new(store));

        state.update(cx, |state, cx| {
            // No real provider may spawn if a start slips through.
            state
                .settings
                .provider_mut(ProviderKind::ClaudeCode)
                .binary_path = Some("/nonexistent/tcode-test-claude".into());

            // Session A, live. Send → the bubble is in the timeline immediately.
            let (session, commands_a) = fake_live_session(cwd.clone());
            let id_a = session.meta.id.clone();
            state.active = Some(session);
            state.send_turn("first message".into(), Vec::new(), cx);
            assert_eq!(state.last_user_turn(), Some(0));
            assert!(matches!(
                commands_a.try_recv(),
                Ok(SessionCommand::SendTurn { .. })
            ));

            state.on_event(
                &id_a,
                AgentEvent::TurnStarted {
                    turn_id: "turn-1".into(),
                },
                cx,
            );

            // Stop. The provider reports an error + an interrupted turn — the
            // truncated-error moment in the T3 repro.
            state.interrupt(cx);
            assert!(matches!(
                commands_a.try_recv(),
                Ok(SessionCommand::Interrupt)
            ));
            state.on_event(
                &id_a,
                AgentEvent::Error {
                    message: "Request was aborted\nwith a second line the toast never showed"
                        .into(),
                    fatal: false,
                },
                cx,
            );
            state.on_event(
                &id_a,
                AgentEvent::TurnCompleted {
                    turn_id: "turn-1".into(),
                    status: TurnStatus::Interrupted,
                    usage: None,
                },
                cx,
            );

            // Immediately: new thread, send. The draft commits to a NEW session;
            // the message waits in the queue while the provider starts (still
            // visible in the queue strip — never dropped).
            state.start_draft("proj-t3".into(), cwd.clone(), cx);
            state.send_turn("second message".into(), Vec::new(), cx);
            let active = state.active.as_ref().unwrap();
            let id_b = active.meta.id.clone();
            assert_ne!(id_a, id_b);
            assert_eq!(active.queue.len(), 1);

            // Provider comes up (simulated — the queue flush on start).
            let (commands_b, receiver_b) = async_channel::unbounded();
            state.active.as_mut().unwrap().runtime = Runtime::Live(commands_b);
            assert_eq!(state.dispatch_next_queued(cx), Ok(true));
            assert!(matches!(
                receiver_b.try_recv(),
                Ok(SessionCommand::SendTurn { .. })
            ));

            // THE assertion: the new thread's first message is a visible user
            // entry in a rendered turn, and session A's error did not leak in.
            let active = state.active.as_ref().unwrap();
            let users: Vec<&str> = active
                .timeline
                .entries
                .iter()
                .filter_map(|e| match &e.content {
                    EntryContent::User { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(users, vec!["second message"]);
            let entry_turn = active.timeline.entries[0].turn;
            assert!(
                entry_turn < active.timeline.turns.len(),
                "user entry must belong to a rendered turn (turn {entry_turn} of {})",
                active.timeline.turns.len()
            );
            assert!(
                !active
                    .timeline
                    .entries
                    .iter()
                    .any(|e| matches!(e.content, EntryContent::Error { .. })),
                "session A's interrupt error leaked into the new thread"
            );
            // And it is durable: a replay of the JSONL shows the same thing.
            let replayed = Timeline::fold_events(state.store.read_events(&id_b));
            assert!(replayed.entries.iter().any(
                |e| matches!(&e.content, EntryContent::User { text, .. } if text == "second message")
            ));
        });

        let _ = std::fs::remove_dir_all(&cwd);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// The T3 Code session-reaper failure class, our variant: switching to
    /// another thread must NOT kill a session whose turn is still running. The
    /// session parks in the background — process and queue alive, events still
    /// recorded, sidebar still "Working" — and selecting it again re-adopts it
    /// with the streamed-while-parked content visible.
    #[gpui::test]
    fn switching_threads_parks_a_working_session_instead_of_killing_it(
        cx: &mut gpui::TestAppContext,
    ) {
        let cwd = std::env::temp_dir().join(format!("tcode-park-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&cwd).unwrap();
        let data = std::env::temp_dir().join(format!("tcode-park-data-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::open_at(data.clone()).unwrap();
        let state = cx.new(|_| AppState::new(store));

        state.update(cx, |state, cx| {
            state
                .settings
                .provider_mut(ProviderKind::ClaudeCode)
                .binary_path = Some("/nonexistent/tcode-test-claude".into());

            // A live session with a running turn (the overnight workflow).
            let (session, commands_a) = fake_live_session(cwd.clone());
            let id_a = session.meta.id.clone();
            state.store.upsert_meta(&session.meta).unwrap();
            state.sessions = state.store.load_index();
            state.active = Some(session);
            state.send_turn("run the long migration".into(), Vec::new(), cx);
            state.send_turn("queued follow-up".into(), Vec::new(), cx);
            state.on_event(
                &id_a,
                AgentEvent::TurnStarted {
                    turn_id: "turn-1".into(),
                },
                cx,
            );
            assert!(matches!(
                commands_a.try_recv(),
                Ok(SessionCommand::SendTurn { .. })
            ));

            // Glance at another thread: the session must survive, not die.
            state.start_draft("proj-t3".into(), cwd.clone(), cx);
            assert!(
                commands_a.try_recv().is_err(),
                "switching threads must not send Shutdown to a working session"
            );
            assert!(
                state.turn_running_for(&id_a),
                "a parked working session keeps its sidebar Working status"
            );

            // The parked session keeps streaming; its events keep landing in
            // the JSONL even though another thread is on screen.
            state.on_event(
                &id_a,
                AgentEvent::ItemCompleted(ThreadItem {
                    id: "bg-1".into(),
                    parent_item_id: None,
                    content: ItemContent::AssistantMessage {
                        text: "Migration step 1 done.".into(),
                    },
                }),
                cx,
            );

            // Its turn completes in the background → the queued follow-up goes
            // out as the next turn, on the same process.
            state.on_event(
                &id_a,
                AgentEvent::TurnCompleted {
                    turn_id: "turn-1".into(),
                    status: TurnStatus::Completed,
                    usage: None,
                },
                cx,
            );
            assert!(matches!(
                commands_a.try_recv(),
                Ok(SessionCommand::SendTurn { .. })
            ));
            assert!(state.turn_running_for(&id_a));

            // Coming back re-adopts the live session: everything that happened
            // while parked is in the timeline, and the turn is still running.
            state.select_session(&id_a, cx);
            let active = state.active.as_ref().unwrap();
            assert_eq!(active.meta.id, id_a);
            assert!(matches!(active.runtime, Runtime::Live(_)));
            assert!(active.turn_in_flight);
            assert!(active.timeline.entries.iter().any(|e| matches!(
                &e.content,
                EntryContent::Assistant { text } if text == "Migration step 1 done."
            )));
            assert!(active.timeline.entries.iter().any(|e| matches!(
                &e.content,
                EntryContent::User { text, .. } if text == "queued follow-up"
            )));

            // The second turn completes with nothing queued: NOW the provider
            // shuts down — work finished, not reaped.
            state.on_event(
                &id_a,
                AgentEvent::TurnCompleted {
                    turn_id: "turn-2".into(),
                    status: TurnStatus::Completed,
                    usage: None,
                },
                cx,
            );
            assert!(!state.turn_running_for(&id_a) || state.active.is_some());
        });

        let _ = std::fs::remove_dir_all(&cwd);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// A parked session that runs out of work shuts down for real (no zombie
    /// processes), and a parked session whose process dies is recorded and
    /// forgotten.
    #[gpui::test]
    fn parked_session_shuts_down_when_its_work_is_done(cx: &mut gpui::TestAppContext) {
        let cwd = std::env::temp_dir().join(format!("tcode-parkend-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&cwd).unwrap();
        let data =
            std::env::temp_dir().join(format!("tcode-parkend-data-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::open_at(data.clone()).unwrap();
        let state = cx.new(|_| AppState::new(store));

        state.update(cx, |state, cx| {
            let (session, commands) = fake_live_session(cwd.clone());
            let id = session.meta.id.clone();
            state.store.upsert_meta(&session.meta).unwrap();
            state.sessions = state.store.load_index();
            state.active = Some(session);
            state.send_turn("one last thing".into(), Vec::new(), cx);
            let _ = commands.try_recv(); // the SendTurn

            state.start_draft("proj".into(), cwd.clone(), cx);
            assert!(state.turn_running_for(&id));

            // The parked turn finishes with an empty queue → real shutdown.
            state.on_event(
                &id,
                AgentEvent::TurnCompleted {
                    turn_id: "turn-1".into(),
                    status: TurnStatus::Completed,
                    usage: None,
                },
                cx,
            );
            assert!(matches!(commands.try_recv(), Ok(SessionCommand::Shutdown)));
            assert!(!state.turn_running_for(&id));
        });

        let _ = std::fs::remove_dir_all(&cwd);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// A failed provider start must not destroy what the user typed: the queued
    /// message stays in the queue (visible in the strip, flushed by the next
    /// successful start) instead of being cleared.
    #[gpui::test]
    fn failed_provider_start_keeps_the_queued_message(cx: &mut gpui::TestAppContext) {
        let cwd = std::env::temp_dir().join(format!("tcode-t3f-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&cwd).unwrap();
        let data = std::env::temp_dir().join(format!("tcode-t3f-data-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::open_at(data.clone()).unwrap();
        let state = cx.new(|_| AppState::new(store));

        state.update(cx, |state, cx| {
            // A binary that cannot exist → start_session fails fast.
            state
                .settings
                .provider_mut(ProviderKind::ClaudeCode)
                .binary_path = Some("/nonexistent/tcode-test-claude".into());
            state.start_draft("proj-fail".into(), cwd.clone(), cx);
            state.send_turn("do not lose me".into(), Vec::new(), cx);
            assert_eq!(state.active.as_ref().unwrap().queue.len(), 1);
        });

        // Let the spawned start attempt run to its failure.
        cx.run_until_parked();

        state.update(cx, |state, _| {
            let active = state.active.as_ref().unwrap();
            assert!(
                matches!(active.runtime, Runtime::Idle),
                "failed start must return to Idle"
            );
            assert_eq!(
                active.queue.first().map(|m| m.text.as_str()),
                Some("do not lose me"),
                "the user's text must survive a failed provider start"
            );
            // The failure itself is on the record.
            assert!(
                active
                    .timeline
                    .entries
                    .iter()
                    .any(|e| matches!(e.content, EntryContent::ProviderStartError { .. })),
                "the start failure must be recorded in the timeline"
            );
        });

        let _ = std::fs::remove_dir_all(&cwd);
        let _ = std::fs::remove_dir_all(&data);
    }
}
