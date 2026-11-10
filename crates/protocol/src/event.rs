use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use agent::{
    ApprovalRequest, OptionDescriptor, OptionSelection, PlanStep, ProviderCommand, ProviderKind,
    RewindMode, TokenUsage,
};
use serde::{Deserialize, Serialize};
use tcode_core::{
    git::{GitAction, GitStatus},
    project::{Project, SessionMeta, WorktreeInfo},
    provider_status::ProviderSnapshot,
    session::{LatestActivity, PendingUserInput, ReviewComment, RunningTurn, StoredEvent},
    settings::Settings,
    settlement::{AgentDelivery, AgentExecution, AgentReport},
    ui::{TerminalSplitDirection, WorkspaceMode},
};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum Topic {
    SessionEvents {
        session_id: String,
    },
    SessionStatus {
        session_id: String,
    },
    SessionPlan {
        session_id: String,
    },
    Index,
    SpaceIndex {
        space_id: String,
    },
    Scope,
    Settings,
    Providers,
    GitStatus {
        session_id: String,
    },
    RuntimeEvents,

    Terminal {
        terminal_id: u64,
    },
    /// One command execution's output, rendered by the host at `cols`
    /// (capped at [`crate::MAX_STORED_OUTPUT_COLS`]). Answered on subscribe and
    /// again while the output grows.
    CommandOutput {
        session_id: String,
        item_id: String,
        cols: u16,
    },
    /// One client's preview channel, subscribed while it can render pages:
    /// the threads it renders, and their agents' preview requests.
    Preview {
        client: crate::PreviewClient,
    },
    /// The host-held preview of one thread.
    PreviewSession {
        session_id: String,
    },
    /// The listeners on the host a thread's preview offers to open, most
    /// relevant to the thread first. The host reads them only while
    /// something subscribes.
    DevServers {
        session_id: String,
    },
    ExternalImport {
        project_id: String,
    },
    /// The machine's hosting state, for a full device: answered on subscribe
    /// and pushed after every change but its invitation's countdown and its
    /// Traverse status. The transport that owns hosting answers it, not the
    /// runtime.
    Hosting,
    /// The host's move of an older build's threads into its current storage,
    /// for a full device: answered on subscribe, and pushed while it runs.
    StorageMigration,
    /// What the host's data and its providers' take on its disk, for a full
    /// device: answered on subscribe with the last measurement, and pushed
    /// when one starts or ends. The first subscription measures.
    DiskUsage,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// Present only on subscription replies; mux routes these to their requester.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<u64>,
    pub topic: Topic,
    pub event: ServerEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum ServerEvent {
    /// The host's whole terminal grid, sent on subscribe and after a restart.
    TerminalFrame {
        terminal_id: u64,
        frame: Box<crate::terminal::TerminalFrame>,
    },
    /// One coalesced grid update, continuing from the last frame or delta.
    TerminalDelta {
        terminal_id: u64,
        delta: Box<crate::terminal::TerminalDelta>,
    },
    /// The whole rendered screen of a [`Topic::CommandOutput`], with no cursor
    /// and trailing blank rows trimmed: the answer to subscribing, and a push
    /// when its subscribers may hold different screens.
    CommandOutputReplaced(Box<crate::terminal::TerminalFrame>),
    /// What changed in a [`Topic::CommandOutput`]'s screen since the last
    /// screen or change pushed on it.
    CommandOutputDelta(Box<crate::terminal::TerminalDelta>),
    /// An agent's preview request, sent on the [`Topic::Preview`] of the
    /// one client rendering its thread.
    PreviewRequest {
        request_id: u64,
        session_id: String,
        request: crate::PreviewRequest,
    },
    /// The threads a [`Topic::Preview`]'s client renders, each as it stood
    /// when rendering came to that client: the answer to subscribing, and a
    /// push whenever the set changes. A thread that leaves the set is
    /// rendered elsewhere or nowhere, and its page here is let go.
    PreviewRenderingReplaced(Vec<crate::PreviewSession>),
    /// A [`Topic::PreviewSession`]'s whole state: the answer to subscribing,
    /// and a push after every change.
    PreviewSessionReplaced(crate::PreviewSession),
    /// A [`Topic::DevServers`]'s whole list: the answer to subscribing, and a
    /// push whenever it changes.
    DevServersReplaced(Vec<crate::DevServer>),
    SessionEvent(StoredEvent),
    SessionStatusReplaced(Box<SessionStatus>),
    SessionUsageChanged {
        usage: Option<TokenUsage>,
        context_window: Option<u64>,
    },
    SessionPlanReplaced(SessionPlan),
    ProvidersReplaced(ProvidersStatus),
    /// Account usage and the usage reads in flight, when the rest of the
    /// providers status is unchanged.
    ProviderUsageChanged {
        provider_usage: HashMap<String, tcode_core::usage::ProviderUsage>,
        usage_checking: HashSet<String>,
    },
    GitStatusReplaced(GitStatusStatus),
    /// An unarchived thread was added or changed. Archiving a thread removes
    /// it with [`ServerEvent::IndexRemoveSession`].
    IndexUpsertSession(Box<SessionMeta>),
    IndexUpsertProject(Project),
    IndexSummaryReplaced(IndexSummary),
    IndexActivitiesChanged {
        upserts: HashMap<String, SessionActivity>,
        removed: Vec<String>,
    },
    IndexUnchanged {
        fingerprint: String,
        archived_revision: u64,
    },
    IndexRemoveSession {
        session_id: String,
    },
    IndexRemoveProject {
        project_id: String,
    },
    SettingsReplaced(Settings),
    HostingReplaced(Box<crate::HostingState>),
    /// Visit times that changed while the rest of the settings did not.
    LastVisitedChanged(HashMap<String, u64>),
    Runtime(RuntimeNotification),

    /// The prompt a rewind restored, for the composer of the client that
    /// asked for the rewind; every other client ignores it.
    RewindPrefill {
        session_id: String,
        client_id: String,
        text: String,
        /// Local paths of the images the prompt carried.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<String>,
    },
    /// Records standing for the log cursors `from..end`. A cursor is the
    /// position of a stored row of the log, and a row without a readable
    /// record stands for none; the host may also merge or drop records whose
    /// effect a later record in the log repeats, so `records` can be shorter
    /// than the range. A reply to a subscription ends at `total`: it starts
    /// at the subscriber's cursor when what follows that fits one reply, and
    /// is otherwise the log's tail, as a subscriber without a cursor gets.
    SessionSnapshot {
        from: u64,
        end: u64,
        records: Vec<StoredEvent>,
        total: u64,
        /// Absolute turn count keeps turn-addressed actions correct in a window.
        total_turns: u64,
        /// The byte budget reduced this reply below the requested record count.
        truncated: bool,
    },
    SessionHistoryError(crate::ProtocolError),
    IndexSnapshot(IndexSnapshot),
    ScopeSnapshot(Scope),
    SettingsSnapshot(Settings),
    /// The current (or latest) external-import run for one project. `None`
    /// means no run has ever started, or the project is gone. Only the latest
    /// run is retained, so a subscriber that attaches after a fast completion
    /// still recovers the outcome from the subscription snapshot.
    ExternalImportStatusReplaced {
        project_id: String,
        status: Option<ExternalImportStatus>,
    },
    /// A [`Topic::StorageMigration`]'s state: the answer to subscribing, a
    /// push at most about once a second while the move runs, and `None` once
    /// none runs.
    StorageMigrationReplaced(Option<StorageMigration>),
    /// A [`Topic::DiskUsage`]'s state: the answer to subscribing, and a push
    /// when a measurement starts or ends.
    DiskUsageReplaced(DiskUsageState),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum Scope {
    Full,
    Space {
        space_id: String,
        space_name: String,
        projects: Vec<Project>,
        providers: Vec<ScopedProviderChoice>,
        policy: crate::SpacePolicy,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScopedProviderChoice {
    pub provider: ProviderKind,
    pub profile_id: Option<String>,
    pub name: String,
    pub models: Vec<agent::ModelSpec>,
}

/// Host-owned progress for one external-history import run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalImportStatus {
    /// Host-generated, so a late subscriber can tell a replayed older run from
    /// the run it started.
    pub run_id: u64,
    pub state: ExternalImportState,
}

/// The host moving an older build's threads into its current storage, in
/// the background while it serves them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageMigration {
    /// Stored rows moved so far, of `total` across every thread to move.
    pub moved: u64,
    pub total: u64,
    /// The move is paused until the disk has room for it.
    pub waiting_for_space: bool,
}

/// The host's disk usage: its last measurement, and whether one runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskUsageState {
    /// `None` until the first measurement ends.
    pub last: Option<DiskUsage>,
    pub measuring: bool,
}

/// What the host's data and its providers' took on its disk when measured,
/// as the host's `disk_usage` service counts it: a symbolic link counts as
/// itself, never as what it points at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskUsage {
    /// When the measurement ended, in seconds since the Unix epoch.
    pub measured_at: u64,
    pub tcode: TcodeDiskUsage,
    /// Largest first; a provider with nothing on disk is left out.
    pub providers: Vec<ProviderDiskUsage>,
}

/// Tcode's data directory, by part. An extra account's directory inside it
/// counts under its provider instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TcodeDiskUsage {
    /// The data directory, `~` standing for the host's home.
    pub dir: String,
    /// The thread store, its write-ahead files, an older build's store while
    /// it moves, and the images cut out of stored events.
    pub conversations: u64,
    /// Files attached to messages and the previews made of images.
    pub images: u64,
    /// Project copies threads work in.
    pub worktrees: u64,
    pub other: u64,
}

impl TcodeDiskUsage {
    pub fn total(&self) -> u64 {
        self.conversations + self.images + self.worktrees + self.other
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderDiskUsage {
    pub provider: ProviderKind,
    pub bytes: u64,
    /// Each account whose directories hold something, largest first.
    pub accounts: Vec<AccountDiskUsage>,
}

/// The directories one provider account's CLI keeps its state in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountDiskUsage {
    /// The first profile launching with these directories.
    pub profile_id: String,
    /// `~` standing for the host's home.
    pub dirs: Vec<String>,
    pub bytes: u64,
    /// The largest entries directly inside those directories, largest first.
    pub parts: Vec<DiskUsagePart>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskUsagePart {
    /// The entry's file name.
    pub name: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum ExternalImportState {
    Progress {
        done: usize,
        total: usize,
        tool: String,
    },
    Finished {
        imported: usize,
        skipped: usize,
    },
}

/// Full provider/settings-page read projection.
///
/// Provider state changes infrequently and its pieces are consumed together,
/// so hosts replace this single value after any covered mutation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ProvidersStatus {
    pub model_catalogs: HashMap<ProviderKind, Vec<agent::ModelSpec>>,
    pub models_loading: HashMap<ProviderKind, bool>,
    pub provider_versions: HashMap<ProviderKind, ProviderVersionStatus>,
    pub update_run: Option<ProviderUpdateRun>,
    pub tcode_update: TcodeUpdateStatus,
    pub provider_snapshots: HashMap<String, ProviderSnapshot>,
    /// Latest account usage per profile id (Codex / Claude Code only).
    pub provider_usage: HashMap<String, tcode_core::usage::ProviderUsage>,
    /// Profile ids with a usage fetch in flight.
    pub usage_checking: HashSet<String>,
    pub acp_marketplace_items: Vec<AcpMarketplaceItem>,
    pub acp_registry_loading: bool,
    pub acp_registry_error: Option<String>,
    pub acp_installing: HashSet<String>,
    pub providers_checked_at: Option<u64>,
    pub providers_checking: bool,
    /// Environment-variable names present for each profile. Values never cross
    /// the protocol boundary.
    pub secret_names: HashMap<String, HashSet<String>>,
    /// Native plugin catalogs of the profiles a client asked about: for each,
    /// the listing from outside any project and the listing of the one
    /// project last asked about.
    pub plugins: Vec<ProviderPluginCatalog>,
    /// What each harness is, as clients present it: every native one, then
    /// every installed ACP agent.
    pub harnesses: Vec<HarnessDescription>,
}

/// One harness as its host describes it to clients: which sections and
/// fields its settings have, and the native names behind them. A client
/// presents a harness from this alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessDescription {
    pub provider: ProviderKind,
    /// The installed ACP agent this is, for an ACP harness.
    pub acp_agent_id: Option<String>,
    /// The command looked up on `PATH`, for a harness with a binary setting.
    pub binary_name: Option<String>,
    /// How an ACP agent is started.
    pub launch_summary: Option<String>,
    pub install_command: Option<String>,
    pub doctor_command: Option<String>,
    /// What to run in a terminal to sign in, where Tcode does not sign in.
    pub sign_in_command: Option<String>,
    /// The variable naming each account's config directory, when the
    /// harness has one.
    pub config_dir_env: Option<String>,
    pub default_config_dir: Option<String>,
    pub endpoint: Option<EndpointDescription>,
    /// The ids of the harness's native switches.
    pub toggles: Vec<String>,
    /// Tcode runs the harness's account sign-in, so accounts can be added.
    pub account_login: bool,
    pub launch_args: bool,
    /// Its plugins are managed in Settings → Plugins.
    pub plugins: bool,
    pub mcp_servers: bool,
    pub native_computer_use: bool,
    /// Its own configuration can be listed ([`crate::Query::NativeMcpServers`]).
    pub native_mcp_config: bool,
    /// The choice of what a new thread may do without asking, where the
    /// harness has more than one.
    pub permission: Option<HarnessPermission>,
    /// A newer version of an ACP agent in the registry.
    pub registry_update: Option<String>,
}

/// A harness's permission control, as its adapter publishes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessPermission {
    pub id: String,
    pub options: Vec<agent::SelectOption>,
    pub recommended: Option<String>,
}

impl HarnessPermission {
    fn of(provider: ProviderKind) -> Option<Self> {
        match agent::permission_control(provider)? {
            agent::OptionDescriptor::Select {
                id,
                options,
                recommended,
                ..
            } if options.len() > 1 => Some(Self {
                id,
                options,
                recommended,
            }),
            _ => None,
        }
    }
}

/// The variables a harness reads to reach a model API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointDescription {
    pub base_url_env: String,
    pub key_env: String,
    pub model_env: Option<String>,
    /// Endpoint profiles can be added for it.
    pub profiles: bool,
    pub presets: Vec<EndpointPresetDescription>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointPresetDescription {
    pub name: String,
    pub base_url: String,
    pub model: Option<String>,
}

impl HarnessDescription {
    /// A native harness as its adapter's facts and capabilities describe it.
    pub fn native(provider: ProviderKind) -> Self {
        let facts = provider.harness();
        let caps = provider.caps();
        let text = |value: Option<&str>| value.map(str::to_owned);
        Self {
            provider,
            acp_agent_id: None,
            binary_name: Some(facts.binary_name.to_owned()).filter(|name| !name.is_empty()),
            launch_summary: None,
            install_command: text(facts.install_command),
            doctor_command: text(facts.doctor_command),
            sign_in_command: text(facts.sign_in_command),
            config_dir_env: text(facts.config_dir_env).filter(|_| caps.home_path),
            default_config_dir: text(facts.default_config_dir),
            endpoint: facts.endpoint.map(|endpoint| EndpointDescription {
                base_url_env: endpoint.base_url_env.to_owned(),
                key_env: endpoint.key_envs[0].to_owned(),
                model_env: text(endpoint.model_env),
                profiles: endpoint.profiles,
                presets: endpoint
                    .presets
                    .iter()
                    .map(|preset| EndpointPresetDescription {
                        name: preset.name.to_owned(),
                        base_url: preset.base_url.to_owned(),
                        model: text(preset.model),
                    })
                    .collect(),
            }),
            toggles: facts.toggles.iter().map(|id| (*id).to_owned()).collect(),
            account_login: caps.account_login,
            launch_args: caps.launch_args,
            plugins: caps.plugin_management.manages(),
            mcp_servers: caps.mcp_servers,
            native_computer_use: caps.native_computer_use,
            native_mcp_config: facts.native_mcp.is_some(),
            permission: HarnessPermission::of(provider),
            registry_update: None,
        }
    }

    /// Whether endpoint profiles can be added to this harness.
    pub fn offers_endpoint_profiles(&self) -> bool {
        self.endpoint
            .as_ref()
            .is_some_and(|endpoint| endpoint.profiles)
    }
}

/// One provider profile's native plugin catalog. The native CLI's own files
/// are the truth; the host only keeps the last listing in memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderPluginCatalog {
    pub profile_id: String,
    /// The project directory the catalog was listed from; `None` lists from
    /// outside any project, so only user scope is offered.
    pub context_cwd: Option<PathBuf>,
    pub marketplaces: Vec<agent::ProviderPluginMarketplace>,
    pub marketplace_actions: Vec<agent::MarketplaceAction>,
    pub entries: Vec<agent::ProviderPluginEntry>,
    /// Native errors for parts of the catalog that could not be read; the
    /// rest of the listing still stands.
    pub errors: Vec<String>,
    pub state: PluginCatalogState,
    pub loading: bool,
    /// Plugin ids or marketplace names with an operation in flight.
    pub pending: Vec<String>,
    pub challenges: Vec<PluginChallenge>,
}

/// How far the entries can be trusted. A failed listing keeps the last
/// known entries rather than clearing them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum PluginCatalogState {
    Fresh,
    Stale { reason: PluginStaleReason },
    Error { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginStaleReason {
    NotLoaded,
    /// Listed for another directory or profile configuration.
    ContextChanged,
    /// A native change went through and has not been listed yet.
    Changed,
}

/// A native confirmation an operation is waiting on. It is answered with
/// [`crate::Command::ResolvePluginChallenge`]; a command the CLI shows again
/// arrives as a new challenge with a new `op_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginChallenge {
    pub op_id: RuntimeOperationId,
    pub profile_id: String,
    /// The plugin id or marketplace name the operation targets.
    pub entry_id: String,
    pub kind: PluginChallengeKind,
    pub native_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum PluginChallengeKind {
    /// Run this marketplace-declared command.
    AcceptCommand {
        command: String,
        sha256: String,
        mode: Option<String>,
    },
    /// The native operation has no confirmation of its own but removes more
    /// than its target: these installed plugins, in every scope.
    ConfirmDestructive { uninstalls: Vec<String> },
}

/// The native operation a plugin toast reports on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum PluginOperationTarget {
    Plugin {
        action: agent::PluginActionKind,
        scope: agent::PluginScope,
        plugin: String,
    },
    AddMarketplace {
        source: String,
    },
    RemoveMarketplace {
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderUpdateRun {
    pub total: usize,
    pub completed: usize,
    pub current: Option<ProviderKind>,
    pub failed: Vec<ProviderKind>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderUpdateAvailable {
    pub provider: ProviderKind,
    pub version: String,
    pub automatic: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProviderVersionStatus {
    pub installed: Option<String>,
    pub latest: Option<String>,
    pub update_available: bool,
    pub checking: bool,
    pub updating: bool,
    pub update_command: Option<String>,
    /// Compact label; `update_command` retains the copyable execution context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_command_summary: Option<String>,
    /// The command needs an interactive terminal (for example, elevation).
    pub update_requires_terminal: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TcodeUpdateStatus {
    pub current: String,
    pub latest: Option<String>,
    pub release_url: Option<String>,
    pub update_available: bool,
    pub checking: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpMarketplaceItem {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub installed: bool,
    pub installing: bool,
    pub supported: bool,
}

/// Full active-workspace Git projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct GitStatusStatus {
    pub status: Option<GitStatus>,
    pub busy: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForkAvailability {
    Available,
    Unsupported,
    Empty,
    Running,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionActivity {
    pub working: bool,
    pub turn_running: bool,
    /// What goes on running in the thread's background.
    #[serde(default, skip_serializing_if = "BackgroundActivity::is_idle")]
    pub background: BackgroundActivity,
    /// Dispatched agents that finished and await this thread's settle.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unsettled: u32,
    /// What wakes the thread later though nothing of it runs.
    #[serde(default, skip_serializing_if = "ThreadWakes::is_empty")]
    pub wakes: ThreadWakes,
    pub waiting_for_approval: bool,
    pub waiting_for_input: bool,
    /// The latest run ended in an error and nothing has run since.
    pub failed: bool,
    pub unread: bool,
    pub fork: ForkAvailability,
    /// A dispatched orchestrate child's execution and delivery; absent for
    /// every other thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentStatus>,
    /// What the thread's running turn last started; absent between turns and
    /// for a provider that has reported no item yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_activity: Option<LatestActivity>,
}

fn is_zero(count: &u32) -> bool {
    *count == 0
}

/// What goes on running in a thread's background, which keeps it from
/// settling.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundActivity {
    /// Pieces of background work its harness runs (see
    /// [`SessionStatus::background_tasks`]).
    pub tasks: u32,
    /// Its agents still running: the dispatched ones, and the harness's own
    /// subagents that run in its turn rather than in its background.
    pub agents: u32,
}

impl BackgroundActivity {
    pub fn is_idle(&self) -> bool {
        self.tasks == 0 && self.agents == 0
    }

    pub fn count(&self) -> u32 {
        self.tasks + self.agents
    }
}

/// What wakes a thread later though nothing of it runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadWakes {
    /// Numbers of the pull requests a watch that is not paused follows.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pull_requests: Vec<u64>,
    /// When its earliest scheduled message is due (unix seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled_at: Option<u64>,
    /// Its `notify_when` watches on other threads.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub notify: u32,
}

impl ThreadWakes {
    pub fn is_empty(&self) -> bool {
        self.pull_requests.is_empty() && self.scheduled_at.is_none() && self.notify == 0
    }
}

/// One piece of a thread's background work as its host lists it: what the
/// harness lists, and calls going on in the background that it does not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundTaskStatus {
    pub task: agent::BackgroundTask,
    /// The harness lists it, so a stop can reach it. One it does not is
    /// known only by its call, and `task.id` is the call's.
    pub listed: bool,
    /// When a stop for it went out (unix ms), while the harness still lists
    /// it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopping_since: Option<u64>,
    /// The harness no longer lists it while its call has not ended: it has
    /// ended, and the harness has yet to say so in the conversation.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ended: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStatus {
    pub execution: AgentExecution,
    pub delivery: AgentDelivery,
    /// Whether the child has reported to its lead (see `AgentReport`).
    pub report: AgentReport,
    /// When its latest run started and ended (unix ms); an end before the
    /// start belongs to an earlier run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_started_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_completed_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionPlan {
    pub session_id: String,
    pub steps: Vec<PlanStep>,
    /// What the agent said of the plan when it last set it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
}

/// Host-owned runtime and full-history facts for one resident session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStatus {
    pub session_id: String,
    pub title: String,
    pub cwd: PathBuf,
    pub attachments_dir: PathBuf,
    pub provider: ProviderKind,
    pub requested_model: Option<String>,
    pub requested_profile_id: Option<String>,
    pub acp_agent_id: Option<String>,
    pub project_id: Option<String>,
    pub queued_messages: Vec<QueuedMessageStatus>,
    /// The thread's background work: what its harness lists, then calls
    /// going on in the background that it does not.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub background_tasks: Vec<BackgroundTaskStatus>,
    /// Backend-owned drafts used by provider-bound send-path assembly.
    pub review_comment_drafts: Vec<ReviewComment>,
    pub terminals: Vec<TerminalStatus>,
    pub active_terminal_id: Option<u64>,
    pub terminal_splits: Vec<TerminalSplitStatus>,
    pub terminal_contexts: Vec<TerminalContextStatus>,
    pub terminal_open: bool,
    pub terminal_height: f32,
    pub delivery_in_flight: Option<u64>,
    pub activity: SessionActivity,
    /// An interrupt was accepted by the provider and its turn has not ended yet.
    pub stopping: bool,
    /// When that first interrupt went out (unix ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopping_since: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_retry: Option<agent::TurnRetry>,
    /// The harness's standing goal (Codex `thread/goal`, Claude Code `/goal`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<agent::Goal>,
    /// What a rewind does in this thread and whether one can start now; none
    /// when its harness, or this thread's session, has no rewind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewind: Option<RewindStatus>,
    /// What a fork from a message does in this thread and whether one can
    /// start now; none when its harness, or this thread's session, has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_at: Option<ForkStatus>,
    /// Queued messages wait, after a rewind, until the user sends the next
    /// message.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub queue_held: bool,
    pub checkout_blocked: bool,
    pub conversation_read_only: bool,
    pub terminal_limit_reached: bool,
    pub terminal_split_available: bool,
    pub usage: Option<TokenUsage>,
    /// Effective capacity the context meter should use, resolved by the host.
    pub context_window: Option<u64>,
    /// The live turn and the requests it waits on. A client's fold of a
    /// history window can place them but cannot decide them: the window may
    /// begin after they opened, and a provider can stop without a record.
    pub running_turn: Option<RunningTurn>,
    pub pending_approvals: Vec<ApprovalRequest>,
    pub pending_user_input: Option<PendingUserInput>,
    #[serde(rename = "supports_steering")]
    pub steering_supported: bool,
    pub provider_option_descriptors: Vec<OptionDescriptor>,
    pub provider_option_selections: Vec<OptionSelection>,
    pub provider_option_requested_selections: Vec<OptionSelection>,
    pub provider_commands: Vec<ProviderCommand>,
    pub git_branch: Option<String>,
    pub branches: Vec<String>,
    pub draft: bool,
    pub draft_workspace: WorkspaceMode,
    pub worktree: Option<WorktreeInfo>,
    pub preparing_worktree: bool,
    pub relay_confirmation: Option<(String, String)>,
    pub model_pending_restart: bool,
    pub options_pending_restart: bool,
    /// The MCP tools offered to this thread's provider, with every text Tcode gives its
    /// model outside the conversation; absent when the provider takes no MCP server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub injected_tools: Option<InjectedTools>,
    /// The thread's `tcode_control` grant and what rests on it; absent when the provider
    /// takes no MCP server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<ControlStatus>,
}

/// A thread's fork through a finished turn, where its harness has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkStatus {
    /// A rewind is being made, and the turns it touches are not settled.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub blocked: bool,
}

/// A thread's rewind, as its harness declares it and the thread refines it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewindStatus {
    /// What a rewind restores, in the order the panel offers them.
    pub modes: Vec<RewindMode>,
    pub first_turn_conversation: bool,
    pub steer_targets: bool,
    pub later_history: agent::LaterHistory,
    pub preview: agent::RewindPreviewKind,
    pub running_turn: agent::RunningTurnPolicy,
    pub restores_command_edits: bool,
    /// Why no rewind can start now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<RewindBlocked>,
    /// The rewind, or undo, the harness is making now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<PendingRewind>,
}

impl RewindStatus {
    /// Whether `mode` can restore turn `turn`, or a steer inside it.
    pub fn offers(&self, mode: RewindMode, turn: usize, steer: bool) -> bool {
        self.modes.contains(&mode)
            && (!steer || (self.steer_targets && mode == RewindMode::Conversation))
            && (turn > 0 || steer || self.first_turn_conversation || !mode.includes_conversation())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewindBlocked {
    /// A rewind or its undo is being made.
    InProgress,
    /// A message is on its way to the harness.
    Delivering,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingRewind {
    pub checkpoint_id: String,
    pub mode: RewindMode,
    /// An undo of the rewind to `checkpoint_id`, not a rewind.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub undo: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlStatus {
    pub grant: tcode_core::control::ControlGrant,
    pub source: tcode_core::control::ControlGrantSource,
    pub pending_requests: Vec<tcode_core::control::ControlGrantRequest>,
    pub references: Vec<ThreadReferenceStatus>,
    pub self_settle: Option<tcode_core::control::SelfSettleGrant>,
    /// Whether the thread's provider is given the `tcode_control` server when it starts.
    pub tools_offered: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadReferenceStatus {
    pub thread_id: String,
    pub title: String,
    /// The referenced thread no longer exists on the host.
    pub deleted: bool,
    pub by: tcode_core::control::ReferenceOrigin,
    pub boundary_notice: bool,
}

pub use tcode_core::control::ControlActivityEntry;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InjectedTools {
    /// Each tool's name and the description the model reads, across every server offered.
    pub tools: Vec<(String, String)>,
    /// Which server offers them: `tools` holds each server's tools together, in this order.
    pub servers: Vec<InjectedServer>,
    /// False once the provider took none of Tcode's MCP servers: its model has neither these
    /// tools nor the linking instructions.
    pub attached: bool,
    /// Verbatim, as delivered.
    pub instructions: Vec<InjectedInstructions>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InjectedServer {
    pub name: String,
    /// How many consecutive entries of `InjectedTools::tools` are this server's.
    pub tools: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InjectedInstructions {
    pub channel: InjectedChannel,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InjectedChannel {
    /// The pull request linking instructions, through the provider's own channel.
    Linking(agent::InstructionChannel),
    /// An MCP server's `instructions`, by server name.
    McpServer(String),
    /// Codex's thread `developerInstructions`.
    DeveloperInstructions,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalStatus {
    pub id: u64,
    pub title: String,
    pub exited: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSplitStatus {
    pub first: u64,
    pub second: u64,
    pub direction: TerminalSplitDirection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalContextStatus {
    pub id: u64,
    pub terminal_label: String,
    pub line_start: usize,
    pub line_end: usize,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedMessageStatus {
    pub id: u64,
    pub editable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_key: Option<String>,
    pub text: String,
    /// Unix timestamp for a scheduled row, or `None` for an ordinary queued
    /// message. Like the queue itself, this status is ephemeral.
    pub fire_at_unix_secs: Option<u64>,
    /// Held because the space it was sent from does not allow the thread's
    /// setup as it stands; it goes once the thread or the space allows it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub space_held: bool,
}

/// The unarchived threads and every project. Archived threads are read on
/// demand with [`crate::Query::ArchivedSessions`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexSnapshot {
    #[serde(flatten)]
    pub summary: IndexSummary,
    pub sessions: Vec<SessionMeta>,
    pub projects: Vec<Project>,
}

/// Index facts that are not one thread's metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexSummary {
    /// Host-authored activity for every unarchived stored session.
    pub activity: HashMap<String, SessionActivity>,
    /// Background title requests, including threads with no live provider.
    pub title_generating: HashSet<String>,
    /// Archived threads per project id.
    pub archived_counts: HashMap<String, usize>,
    pub worktree_shared: HashSet<String>,
    pub archived_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchivedSessions {
    pub sessions: Vec<SessionMeta>,
    pub worktree_shared: HashSet<String>,
    pub revision: u64,
}

/// A transient runtime notification delivered to clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum RuntimeNotification {
    Error(RuntimeError),
    Notice(RuntimeNotice),
    Toast(RuntimeToast),
    Effect(RuntimeEffect),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum RuntimeEffect {
    ApplyLocale { language: Option<String> },
    CopyToClipboard { text: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RuntimeOperationId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitActionRequest {
    pub session_id: String,
    pub action: GitAction,
    pub message: Option<String>,
    pub included: Option<Vec<String>>,
    pub feature_branch: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeWorktreeFailure {
    Missing,
    DirtyWorktree,
    DestinationDetached,
    DirtyDestination,
    DivergedConflict,
    Git,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum RuntimeToast {
    /// A watched pull request woke its thread's agent, or its watch ended with a notice.
    PullRequestWatch {
        session_id: String,
        number: u64,
        notice: tcode_core::pull_request_watch::WatchNotice,
    },
    /// How a write to a native stack ended after its command was answered: a followed merge,
    /// a rebase, or an unconfirmed merge a sync has since read. `session_id` is the thread
    /// `target` is opened from.
    PullRequestStack {
        session_id: String,
        target: tcode_core::pull_request::PullRequestKey,
        stack: u64,
        layers: Vec<u64>,
        base: String,
        result: crate::PullRequestActionResult,
        /// A merge that ended after the host stopped following it.
        late: bool,
    },
    ProviderUpdatesAvailable {
        updates: Vec<ProviderUpdateAvailable>,
    },
    ProviderUpdateStarted {
        operation: RuntimeOperationId,
        run: ProviderUpdateRun,
    },
    ProviderUpdateProgress {
        operation: RuntimeOperationId,
        run: ProviderUpdateRun,
    },
    ProviderUpdateFinished {
        operation: RuntimeOperationId,
        run: ProviderUpdateRun,
    },
    GitBusy,
    GitStarted {
        operation: RuntimeOperationId,
        action: GitAction,
    },
    GitSucceeded {
        operation: RuntimeOperationId,
        action: GitAction,
    },
    GitFailed {
        operation: RuntimeOperationId,
        detail: String,
        retry: GitActionRequest,
    },
    CommitMessageGenerated {
        message: String,
    },
    CommitMessageFailed {
        detail: String,
    },
    AcpInstallStarted {
        operation: RuntimeOperationId,
        name: String,
    },
    AcpInstallSucceeded {
        operation: RuntimeOperationId,
        name: String,
    },
    AcpInstallFailed {
        operation: RuntimeOperationId,
        name: String,
        detail: String,
    },
    PluginOperationStarted {
        operation: RuntimeOperationId,
        profile_id: String,
        target: PluginOperationTarget,
    },
    PluginOperationSucceeded {
        operation: RuntimeOperationId,
        profile_id: String,
        target: PluginOperationTarget,
    },
    PluginOperationFailed {
        operation: RuntimeOperationId,
        profile_id: String,
        target: PluginOperationTarget,
        detail: String,
    },
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum RuntimeError {
    External(String),
    PersistSettings {
        error: String,
    },
    /// settings.json could not be read at start, so the host runs on defaults
    /// and writes no settings over it.
    SettingsUnreadable {
        error: String,
    },
    /// Another file of the host's own (reviews.json, control_activity.json)
    /// could not be read at start, so the host runs without what it holds and
    /// writes nothing over it. `error` names the file.
    FileUnreadable {
        error: String,
    },
    TerminalStart {
        error: String,
    },
    TerminalRestart {
        error: String,
    },
    PersistProject {
        error: String,
    },
    WorktreeRemove {
        error: String,
    },
    DeleteSession {
        error: String,
    },
    DeleteProject {
        error: String,
    },
    PersistEvent {
        error: String,
    },
    WorktreeAdd {
        error: String,
    },
    PersistSession {
        error: String,
    },
    TitleGenerationFailed,
    TitleGenerationEmpty,
    ProcessGone,
    SteerUnsupported {
        agent: String,
    },
    DirtyTree,
    ProviderStart {
        /// The display name of the thread's Harness, with its account.
        harness: String,
        error: String,
    },
    /// The provider's CLI is a Windows batch file Tcode cannot start as what
    /// it runs, and one of its arguments spans lines, which a batch file
    /// cannot be given.
    ProviderBatchLineBreak {
        program: PathBuf,
    },
    /// A provider could not start because the thread's working directory is
    /// gone; `project_id` lets a client offer to re-point or remove the project.
    WorkingDirectoryMissing {
        session_id: String,
        project_id: Option<String>,
        cwd: PathBuf,
    },
    ProviderClosed {
        harness: String,
        reason: Option<String>,
    },
    PersistSessionIndex {
        error: String,
    },
    ProviderMessage(String),
    /// A profile's config directory could not be prepared or removed; the
    /// profile is added or deleted regardless.
    ProfileConfig {
        error: String,
    },
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum RuntimeNotice {
    ProviderMessage(String),
    TcodeUpdateAvailable {
        version: String,
    },
    SwitchedBranch {
        branch: String,
    },
    WorktreeSeeded {
        copied_files: usize,
        skipped: Vec<String>,
    },
    WorktreeMergedFastForward,
    WorktreeMergedCommit,
    WorktreeMergeFailed {
        reason: MergeWorktreeFailure,
        detail: Option<String>,
    },
    /// A thread's model recorded a conversation review.
    ReviewRecorded {
        items: usize,
        coverage: String,
    },
    /// A thread's model referenced a thread in another project or of another provider, so
    /// that thread's content may now reach this thread's model vendor; the user is told once.
    ReferenceBoundaryCrossed {
        thread: String,
        thread_title: String,
        referenced: String,
        title: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeSeverity {
    Success,
    Warning,
}

impl RuntimeNotice {
    pub fn severity(&self) -> NoticeSeverity {
        match self {
            Self::ProviderMessage(_)
            | Self::WorktreeMergeFailed { .. }
            | Self::ReferenceBoundaryCrossed { .. } => NoticeSeverity::Warning,
            Self::TcodeUpdateAvailable { .. }
            | Self::SwitchedBranch { .. }
            | Self::WorktreeSeeded { .. }
            | Self::WorktreeMergedFastForward
            | Self::WorktreeMergedCommit
            | Self::ReviewRecorded { .. } => NoticeSeverity::Success,
        }
    }
}
