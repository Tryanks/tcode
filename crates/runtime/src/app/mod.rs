//! Application state: session registry, active session runtime, event pump.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agent::{
    AgentError, AgentEvent, ApprovalDecision, ApprovalMode, Attachment, InteractionMode,
    ItemContent, LaunchEnv, ModelSpec, OptionDescriptor, OptionDescriptors, OptionSelection,
    PlanResolution, ProviderCommand, ProviderKind, RewindMode, SessionCommand, SessionHandle,
    SessionOptions, ThreadItem, TurnOptions, TurnStatus, list_models,
};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::event::{
    GitActionRequest, HostEvent, RuntimeEffect, RuntimeError, RuntimeEvent, RuntimeNotice,
    RuntimeOperationId, RuntimeToast,
};
use crate::host::{HostCx, HostTask};
use crate::terminal::{LocalTerminalRegistry, TerminalContext, TerminalSplit, TerminalWorkspace};
use tcode_core::acp::{AcpAgentPatch, InstalledAcpAgent as InstalledAgent};
use tcode_core::attachments::mime_from_path;
use tcode_core::git::{GitAction, GitStatus, build_commit_prompt, sanitize_commit_message};
use tcode_core::project::{
    AutoArchiveConfig, AutoArchiveExemptions, Project, SessionMeta, WorktreeInfo,
    auto_archive_candidates,
};
use tcode_core::provider_status::ProviderSnapshot;
use tcode_core::relay::{
    RELAY_TRANSCRIPT_MAX_CHARS, assemble_relay_prompt, has_meaningful_history,
    render_relay_transcript,
};
use tcode_core::session::{
    EntryContent, ReviewComment, Timeline, append_review_comments_to_prompt, implement_prompt,
    plan_title,
};
use tcode_core::settings::{
    ChildApprovalMode, EnvVar, OrchestrateSettings, ProfileSettingsPatch, ProviderProfile,
    ProviderSettings, ResolvedProfile, Settings,
};
use tcode_core::ui::{
    ConversationDestination, MAX_TERMINALS_PER_SESSION, TerminalSplitDirection, WorkspaceMode,
};
use tcode_protocol::{
    AcpMarketplaceItem, EventEnvelope, ExternalThread, GitStatusStatus, IndexSnapshot, PathEntry,
    ProviderVersionStatus as ProtocolProviderVersionStatus, ProvidersStatus, QueuedMessageStatus,
    RecentDir, ServerEvent, SessionEventRecord, SessionStatus, TerminalContextStatus,
    TerminalSplitStatus, TerminalStatus, Topic,
};
use tcode_services::acp_registry::{
    Registry, RegistryAgent, cached, install, load, platform_key, resolve_recipe, uninstall,
    visible_agents,
};
use tcode_services::git::{
    CheckoutError, checkout_if_clean, commit_diff_context, create_git_worktree, list_git_branches,
    perform_action, read_git_branch, read_status, remove_git_worktree, run_claude_headless,
    worktree_path_for,
};
use tcode_services::import::{
    ExternalImportUpdate, ExternalRoots, ImportOutcome, existing_external_ids, import_thread,
    scan_recent_dirs,
};
use tcode_services::provider_probe::{
    default_program, probe_provider, run_capture, run_capture_env, run_status,
};
use tcode_services::settings::SettingsStore;
use tcode_services::store::{SessionStore, now_millis, now_secs};
use tcode_services::user_files;
use tcode_services::version_check::{
    InstallSource, detect_install_source, is_update_available, npm_package, parse_version,
    update_command, update_command_string,
};
use tcode_services::workspace::list_workspace;

const TITLE_MAX_CHARS: usize = 40;
const TITLE_SOURCE_MAX_CHARS: usize = 4_000;
const AI_TITLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
/// Anthropic's prompt cache expires after one hour, so a provider kept longer
/// cannot preserve a useful cached conversation prefix.
const RESIDENT_IDLE_GRACE: Duration = Duration::from_secs(60 * 60);
/// Runaway backstop for unusually large resident fleets; the grace reaper is
/// the primary bound, while this comfortably preserves typical orchestrate
/// fleets whose idle children may be re-messaged.
const MAX_IDLE_RESIDENTS: usize = 16;
const NATIVE_PROVIDER_KINDS: [ProviderKind; 4] = [
    ProviderKind::ClaudeCode,
    ProviderKind::Codex,
    ProviderKind::Pi,
    ProviderKind::OpenCode,
];

type ProviderLaunchFuture =
    Pin<Box<dyn Future<Output = Result<SessionHandle, AgentError>> + Send + 'static>>;

/// Internal seam for starting a provider adapter.
///
/// Production uses [`agent::start_session`]; runtime tests install a scripted
/// adapter while exercising the same command and event paths.
#[derive(Clone)]
pub struct ProviderLauncher(
    Arc<dyn Fn(ProviderKind, SessionOptions) -> ProviderLaunchFuture + Send + Sync>,
);

impl ProviderLauncher {
    fn launch(&self, provider: ProviderKind, options: SessionOptions) -> ProviderLaunchFuture {
        (self.0)(provider, options)
    }
}

impl Default for ProviderLauncher {
    fn default() -> Self {
        Self(Arc::new(|provider, options| {
            Box::pin(agent::start_session(provider, options))
        }))
    }
}

/// Test-controlled provider adapter paired with its launcher.
#[cfg(any(test, feature = "test-support"))]
pub struct ScriptedProvider {
    pub launcher: ProviderLauncher,
    pub commands: smol::channel::Receiver<SessionCommand>,
    pub events: smol::channel::Sender<AgentEvent>,
}

/// Build a provider launcher whose command and event channels are owned by the test.
#[cfg(any(test, feature = "test-support"))]
pub fn scripted_provider(provider: ProviderKind) -> ScriptedProvider {
    let (commands_tx, commands) = smol::channel::unbounded();
    let (events, events_rx) = smol::channel::unbounded();
    let launcher = ProviderLauncher(Arc::new(move |requested, _options| {
        let commands = commands_tx.clone();
        let events = events_rx.clone();
        Box::pin(async move {
            if requested != provider {
                return Err(AgentError::Protocol(format!(
                    "scripted provider expected {provider:?}, got {requested:?}"
                )));
            }
            Ok(SessionHandle {
                provider,
                commands,
                events,
            })
        })
    }));
    ScriptedProvider {
        launcher,
        commands,
        events,
    }
}

fn normalize_terminal_context_text(text: &str) -> String {
    text.replace("\r\n", "\n").trim_matches('\n').to_string()
}

fn append_terminal_contexts_to_prompt(prompt: &str, contexts: &[TerminalContext]) -> String {
    let prompt = prompt.trim();
    let mut lines = Vec::new();
    for context in contexts {
        let text = normalize_terminal_context_text(&context.text);
        if text.is_empty() || context.terminal_label.trim().is_empty() {
            continue;
        }
        let range = if context.line_start == context.line_end {
            format!("line {}", context.line_start)
        } else {
            format!("lines {}-{}", context.line_start, context.line_end)
        };
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push(format!("- {} {}:", context.terminal_label.trim(), range));
        lines.extend(
            text.lines()
                .enumerate()
                .map(|(index, line)| format!("  {} | {}", context.line_start + index, line)),
        );
    }
    if lines.is_empty() {
        return prompt.to_string();
    }
    let block = format!(
        "<terminal_context>\n{}\n</terminal_context>",
        lines.join("\n")
    );
    if prompt.is_empty() {
        block
    } else {
        format!("{prompt}\n\n{block}")
    }
}

#[derive(Debug, Clone, Copy)]
enum TimelineLoadTarget {
    Active {
        mark_idle: bool,
        read_git_branch: bool,
    },
    Background {
        mark_idle: bool,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct TerminalPreferences {
    open: bool,
    height: f32,
    count: usize,
}

#[derive(Debug, Clone, Copy)]
enum TerminalSpawnAction {
    Open {
        split_after: Option<TerminalSplitDirection>,
    },
    Restart {
        terminal_id: Option<u64>,
    },
    New,
    Split {
        first: u64,
        direction: TerminalSplitDirection,
    },
}

mod acp;
mod active_session;
mod approvals;
mod events;
mod git;
mod lifecycle;
mod options;
mod orchestrate;
mod providers;
mod send;
mod sessions;
mod snapshots;
mod store_write;
mod terminals;

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

pub use active_session::{ActiveSession, QueuedMessage};
use active_session::{
    PendingRelay, Runtime, SendRouting, attachment_paths, conversation_destination,
    wire_text_with_placeholder,
};
use orchestrate::McpWiring;
pub use providers::ProviderCatalog;
use providers::{
    effort_selection, normalized_selections, provider_secret_names, session_launch_env,
    session_options,
};
pub use sessions::ResidentSessions;
pub(crate) use snapshots::DomainDiff;
use store_write::{StoreWrite, run_store_write};

/// The result of a provider version check (Group C / s3 §6).
#[derive(Debug, Clone, Default)]
pub struct ProviderVersionState {
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

pub struct AppState {
    store: SessionStore,
    settings_store: SettingsStore,
    store_writes: smol::channel::Sender<StoreWrite>,
    store_write_receiver: Option<smol::channel::Receiver<StoreWrite>>,
    store_write_failures: smol::channel::Sender<Result<RuntimeError, String>>,
    store_write_failure_receiver: Option<smol::channel::Receiver<Result<RuntimeError, String>>>,
    pub sessions: Vec<SessionMeta>,
    pub projects: Vec<Project>,
    pub residents: ResidentSessions,
    /// Terminal resources parked by conversation destination. Drawer chrome is
    /// client-owned; this map retains only PTYs, tabs, splits, and contexts.
    terminal_workspaces: HashMap<ConversationDestination, TerminalWorkspace>,
    /// Construction-time local transport registry for opaque live terminal
    /// objects. All serializable terminal metadata remains in SessionStatus.
    terminal_registry: LocalTerminalRegistry,
    /// Provider-native rewind requested while a session is live or starting.
    /// Kept here (rather than in persisted session metadata) because the
    /// provider response is the only authority that can complete it.
    pending_native_rewinds: HashMap<String, (String, RewindMode)>,
    pub settings: Settings,
    pub providers: ProviderCatalog,
    terminal_preferences_path: PathBuf,
    terminal_preferences: HashMap<String, TerminalPreferences>,
    next_terminal_spawn_id: u64,
    pending_terminal_spawns: HashMap<String, HashMap<u64, TerminalSpawnAction>>,
    next_start_generation: u64,
    /// Invalidates detached scheduled-wake tasks whenever the earliest deadline
    /// changes; stale timers must never fire or reschedule superseded work.
    scheduler_generation: u64,
    resident_idle_grace: Duration,
    /// Kept off in unit tests so dispatching a synthetic turn never launches a
    /// real provider process. Production titles are generated in the background.
    ai_title_generation_enabled: bool,
    provider_launcher: ProviderLauncher,
    /// The ACP agent marketplace: the registry index (from the CDN, cached on
    /// disk with a one-hour TTL), whether a refresh is in flight, and the last
    /// failure to show when there is nothing cached to fall back on.
    pub acp_registry: Option<Registry>,
    pub acp_registry_loading: bool,
    pub acp_registry_error: Option<String>,
    /// Registry ids currently downloading (their marketplace row shows a spinner).
    pub acp_installing: std::collections::HashSet<String>,
    mcp: McpWiring,
    callback_last_turn: HashMap<String, usize>,
    callback_approval_requests: HashSet<(String, String)>,
    /// Live provider approvals for every resident session. This is the sole
    /// host-side authority; persisted timeline approvals remain client state.
    approvals: HashMap<String, Vec<agent::ApprovalRequest>>,
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
    /// Monotonic watermark for JSONL appends. Timeline loads retry when this
    /// changes while their background read is in flight.
    store_append_generation: u64,
    /// Per-session token used to discard superseded timeline loads.
    timeline_load_generations: HashMap<String, u64>,
    /// Composer-draft review notes, keyed by session id (in-memory only).
    review_comment_drafts: HashMap<String, Vec<ReviewComment>>,
    /// A restart-continuity marker taken at launch (see `tcode_services::relaunch`).
    /// Present only after an app-relaunch triggered by a permission grant; applied
    /// once by [`AppState::apply_pending_relaunch`] and then cleared.
    pending_relaunch: Option<tcode_services::relaunch::RelaunchMarker>,
}

fn emit_runtime(cx: &mut HostCx, event: RuntimeEvent) {
    cx.emit(HostEvent::Runtime(event));
}

impl AppState {
    pub fn new(store: SessionStore) -> Self {
        Self::new_with_terminal_registry(store, LocalTerminalRegistry::default(), false)
    }

    pub(crate) fn new_with_terminal_registry(
        store: SessionStore,
        terminal_registry: LocalTerminalRegistry,
        ai_title_generation_enabled: bool,
    ) -> Self {
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
        let provider_secret_names = provider_secret_names(&settings, &settings_store);
        // Push the loaded computer-use config to the (already-running) MCP layer
        // so the tools honor the persisted image-mode / allow-input choices from
        // the first call, not just after a settings change.
        computer_use_mcp::config::set(settings.computer_use.clone());
        // Consume any restart-continuity marker left by a permission grant.
        let pending_relaunch = tcode_services::relaunch::take(store.root());
        let terminal_preferences_path = store.root().join("terminal-ui.json");
        let terminal_preferences = std::fs::read(&terminal_preferences_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        // Seed the model picker from the persisted cache so it is instant and
        // works offline; a background refresh (see `refresh_model_catalogs`)
        // updates it once the providers respond.
        let mut model_catalogs = HashMap::new();
        for provider in NATIVE_PROVIDER_KINDS {
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
        let (store_writes, store_write_receiver) = smol::channel::unbounded();
        let (store_write_failures, store_write_failure_receiver) = smol::channel::unbounded();
        Self {
            store,
            settings_store,
            store_writes,
            store_write_receiver: Some(store_write_receiver),
            store_write_failures,
            store_write_failure_receiver: Some(store_write_failure_receiver),
            sessions,
            projects,
            residents: ResidentSessions::default(),
            terminal_workspaces: HashMap::new(),
            terminal_registry,
            pending_native_rewinds: HashMap::new(),
            settings,
            providers: ProviderCatalog::new(model_catalogs, provider_secret_names),
            terminal_preferences_path,
            terminal_preferences,
            next_terminal_spawn_id: 0,
            pending_terminal_spawns: HashMap::new(),
            next_start_generation: 0,
            scheduler_generation: 0,
            resident_idle_grace: RESIDENT_IDLE_GRACE,
            ai_title_generation_enabled,
            provider_launcher: ProviderLauncher::default(),
            acp_registry: None,
            acp_registry_loading: false,
            acp_registry_error: None,
            acp_installing: std::collections::HashSet::new(),
            mcp: McpWiring::default(),
            callback_last_turn: HashMap::new(),
            callback_approval_requests: HashSet::new(),
            approvals: HashMap::new(),
            git_status: None,
            git_busy: false,
            next_operation_id: 1,
            git_status_generation: 0,
            store_append_generation: 0,
            timeline_load_generations: HashMap::new(),
            review_comment_drafts: HashMap::new(),
            pending_relaunch,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_provider_launcher_for_test(&mut self, launcher: ProviderLauncher) {
        self.provider_launcher = launcher;
    }

    fn start_store_writer(&mut self, cx: &mut HostCx) {
        if let Some(writes) = self.store_write_receiver.take() {
            let store = self.store.clone();
            let settings_store = self.settings_store.clone();
            let terminal_preferences_path = self.terminal_preferences_path.clone();
            let failures = self.store_write_failures.clone();
            HostCx::spawn_detached(cx, async move {
                while let Ok(write) = writes.recv().await {
                    if let Some(failure) =
                        run_store_write(&store, &settings_store, &terminal_preferences_path, write)
                    {
                        let _ = failures.send(failure).await;
                    }
                }
            });
        }
        if let Some(failures) = self.store_write_failure_receiver.take() {
            let host_cx = cx.clone();
            HostCx::spawn_detached(cx, async move {
                while let Ok(failure) = failures.recv().await {
                    host_cx.enqueue(move |state, cx| match failure {
                        Ok(error) => state.report_error(error, cx),
                        Err(message) => log::warn!("{message}"),
                    });
                }
            });
        }
    }

    fn enqueue_store_write(&mut self, write: StoreWrite, cx: &mut HostCx) {
        self.start_store_writer(cx);
        if self.store_writes.try_send(write).is_err() {
            log::error!("session store writer stopped before accepting a write");
        }
    }

    fn enqueue_settings(&mut self, settings: &Settings, cx: &mut HostCx) {
        match serde_json::to_vec_pretty(settings) {
            Ok(bytes) => self.enqueue_store_write(StoreWrite::WriteSettings(bytes), cx),
            Err(err) => self.report_error(
                RuntimeError::PersistSettings {
                    error: err.to_string(),
                },
                cx,
            ),
        }
    }

    fn persist_settings(&mut self, cx: &mut HostCx) {
        let settings = self.settings.clone();
        self.enqueue_settings(&settings, cx);
    }

    fn emit_domain(&self, topic: Topic, event: ServerEvent, cx: &mut HostCx) {
        cx.emit(HostEvent::Domain(EventEnvelope { topic, event }));
    }
}
