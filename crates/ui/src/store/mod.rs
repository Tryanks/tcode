use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
#[cfg(not(target_family = "wasm"))]
use std::time::Instant;

use gpui::{App, Context, Entity, EventEmitter, Subscription as GpuiSubscription, Task};
use tcode_client::{
    ConnectionState, HostLink,
    host::{ClientHost, LiveHost},
};
use tcode_core::{
    git::{GitFileEntry, MenuItem, QuickAction, menu_items, quick_action},
    project::{
        Project, ProjectGroup, SessionMeta, WorktreeInfo, descendant_session_ids, group_sessions,
    },
    provider_models::{ResolvedModel, picker_models, resolve_models},
    provider_status::ProviderSnapshot,
    session::{ReviewComment, StoredEvent, Timeline},
    settings::{BrowserSettings, ProjectSort, ProviderSettings, ResolvedProfile, Settings},
    ui::{ConversationDestination, RightTab},
};
use tcode_protocol::{AcpMarketplaceItem, RuntimeNotification as RuntimeEvent};
use tcode_protocol::{
    ArchivedSessions, Command, CommandResponse, DiskUsageState, EventEnvelope,
    ExternalImportStatus, ExternalThread, GitDiffResult, GitDiffScope, GitStatusStatus,
    IndexSummary, ProtocolError, ProviderVersionStatus, ProvidersStatus, Query, QueryResponse,
    Scope, ScopedProviderChoice, ServerEvent, SessionContentHits, SessionPlan, SessionStatus,
    StorageMigration, Subscription, Topic,
};
#[cfg(target_family = "wasm")]
use web_time::Instant;
pub(crate) mod terminal;
pub(crate) use terminal::ClientTerminal;
use terminal::TerminalWorkspace;

use crate::conversation_ui::{ConversationUiState, DiffFocus};

mod apply;
mod chat_queries;
mod command_output;
mod deletion;
mod history;
mod providers;
pub(crate) use providers::AgentChoice;
pub(crate) mod images;
mod intents;
mod preview;
mod right_panel;
pub(crate) use preview::{PreviewPlace, PreviewReleased};
mod thread;
pub(crate) use images::{
    ImageHost, MAX_PREVIEW_PIXELS, MediaState, image_preview, item_image_preview,
    pull_request_media, pull_request_media_state, remote_image, retry_failed_pull_request_media,
};
use thread::{Part, ThreadReplica};
mod snapshots;

pub(crate) use snapshots::PanelState;
pub use snapshots::{ComposerCheckoutState, ComposerQueue, ComposerState};
pub use tcode_protocol::ForkAvailability;

/// Payload-free topic discriminant used by views to subscribe only to the
/// store projections they render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopicKind {
    SessionEvents,
    SessionStatus,
    SessionPlan,
    Index,
    SpaceIndex,
    Scope,
    Settings,
    Providers,
    GitStatus,
    RuntimeEvents,
    Terminal,
    CommandOutput,
    Preview,
    ExternalImport,
    Hosting,
    StorageMigration,
    DiskUsage,
}

impl From<&Topic> for TopicKind {
    fn from(topic: &Topic) -> Self {
        match topic {
            Topic::SessionEvents { .. } => Self::SessionEvents,
            Topic::SessionStatus { .. } => Self::SessionStatus,
            Topic::SessionPlan { .. } => Self::SessionPlan,
            Topic::SpaceIndex { .. } => Self::Index,
            Topic::Scope => Self::Index,
            Topic::Index => Self::Index,
            Topic::Settings => Self::Settings,
            Topic::Providers => Self::Providers,
            Topic::GitStatus { .. } => Self::GitStatus,
            Topic::RuntimeEvents => Self::RuntimeEvents,
            Topic::Terminal { .. } => Self::Terminal,
            Topic::CommandOutput { .. } => Self::CommandOutput,
            Topic::Preview { .. } | Topic::PreviewSession { .. } | Topic::DevServers { .. } => {
                Self::Preview
            }
            Topic::ExternalImport { .. } => Self::ExternalImport,
            Topic::Hosting => Self::Hosting,
            Topic::StorageMigration => Self::StorageMigration,
            Topic::DiskUsage => Self::DiskUsage,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreChange {
    pub topic: TopicKind,
}

/// A terminal's grid changed: its bell and clipboard requests, if any, arrived
/// with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TerminalChange {
    pub terminal_id: u64,
}

/// A thread is to be shown; the shell picks the pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShowThread {
    pub session_id: String,
    /// The thread the control asking shows, for its pane to take this one.
    pub replacing: Option<String>,
    /// The user asked for it, rather than the workspace falling back on it.
    pub asked: bool,
    /// The thread it opens beside, in a pane of its own to that one's right
    /// where the layout has room for one.
    pub beside: Option<String>,
}

/// Observe selected store domains while keeping topic filtering out of views.
pub(crate) fn observe_store_topics<V: 'static>(
    store: &Entity<WorkspaceStore>,
    topics: &'static [TopicKind],
    cx: &mut Context<V>,
) -> GpuiSubscription {
    cx.subscribe(store, move |_, _, change: &StoreChange, cx| {
        if topics.contains(&change.topic) {
            cx.notify();
        }
    })
}

/// Identity fixed for one workspace attachment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceAttachment {
    Local,
    Remote { host_id: String, host_name: String },
}

/// Which host a client names: the one in this process, or a remote one by
/// its id. Written as `"local"` or the id, the names the client's outboxes
/// are stored under.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(from = "String", into = "String")]
pub enum HostKey {
    Local,
    Remote(String),
}

impl From<String> for HostKey {
    fn from(name: String) -> Self {
        if name == "local" {
            Self::Local
        } else {
            Self::Remote(name)
        }
    }
}

impl HostKey {
    /// How this client writes the host down: `"local"` or the host's id.
    pub fn name(&self) -> &str {
        match self {
            Self::Local => "local",
            Self::Remote(host_id) => host_id,
        }
    }
}

impl From<HostKey> for String {
    fn from(host: HostKey) -> Self {
        match host {
            HostKey::Local => "local".into(),
            HostKey::Remote(host_id) => host_id,
        }
    }
}

/// A thread as a client names it: a session id is unique only on its host.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ThreadKey {
    pub host: HostKey,
    pub session_id: String,
}

/// Where a remote attachment's Preview goes: the paired machine and the
/// tunnels the attachment's transport opens to it.
#[cfg(preview_backend)]
pub(crate) type PreviewTarget = (
    tcode_client::pairing::PairedHost,
    std::sync::Arc<dyn tcode_client::host::TunnelOpener>,
);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum WorkspaceScope {
    #[default]
    Full,
    Space {
        space_id: String,
        space_name: String,
    },
}

impl WorkspaceScope {
    pub fn is_full(&self) -> bool {
        matches!(self, Self::Full)
    }
}

/// The client-facing projection and command boundary for workspace state.
///
/// Views observe this entity and use its typed accessors instead of retaining
/// or reading the backend `AppState` entity directly.
pub struct WorkspaceStore {
    host: HostLink,
    scope: WorkspaceScope,
    /// What the space this workspace is a member of allows; `None` on a
    /// full workspace, which may do everything.
    space_policy: Option<tcode_protocol::SpacePolicy>,
    scoped_providers: Vec<ScopedProviderChoice>,
    attachment: WorkspaceAttachment,
    client_host: Option<Rc<dyn ClientHost>>,
    /// The transport's live view of the attached machine: its authenticated
    /// pairing and how the connection is carried. `None` locally and in a
    /// browser.
    #[cfg_attr(not(preview_backend), allow(dead_code))]
    current_host: Option<LiveHost>,
    image_host: images::ImageHost,
    attachment_tasks: Vec<Task<()>>,
    /// Renames this client on its preview channel when the device is renamed.
    _device_name: GpuiSubscription,
    /// Replicated terminal grids, keyed by the host's terminal id.
    terminals: HashMap<u64, std::rc::Rc<ClientTerminal>>,
    /// The agents' preview requests for the threads this client renders.
    remote_preview: (
        async_channel::Sender<EventEnvelope>,
        async_channel::Receiver<EventEnvelope>,
    ),
    preview: preview::PreviewReplica,
    /// Latest host-published import status per project, replicated from
    /// [`Topic::ExternalImport`]. The dialog renders this rather than owning a
    /// second events consumer.
    import_statuses: HashMap<String, Option<ExternalImportStatus>>,
    /// The host's move of an older build's threads, replicated from
    /// [`Topic::StorageMigration`] while a view holds it.
    storage_migration: Option<StorageMigration>,
    storage_migration_holds: usize,
    /// The host's disk usage, replicated from [`Topic::DiskUsage`] while a
    /// view holds it.
    disk_usage: DiskUsageState,
    disk_usage_holds: usize,
    connection_state: ConnectionState,
    /// How far the host's wall clock is ahead of this client's, in
    /// milliseconds, as its last scope snapshot said; until one lands, none.
    host_clock_offset_ms: i64,
    index_replica: (Vec<SessionMeta>, Vec<Project>),
    /// Each thread's place in `index_replica.0`, rebuilt by
    /// [`Self::reindex_sessions`] whenever that list changes.
    index_positions: HashMap<String, usize>,
    index_summary: IndexSummary,
    /// Moves on with every change to what the thread lists are derived
    /// from: the index and its summary, the settings and the scope.
    thread_list_revision: u64,
    /// Archived threads, which the index leaves out; loaded while a view
    /// holds them.
    archived_replica: Option<ArchivedSessions>,
    /// Why the last load of the archived threads failed, and the archived
    /// revision it was asked at: the same revision is not asked for again.
    archived_failure: Option<(u64, String)>,
    archived_holds: usize,
    archived_task: Option<Task<()>>,
    /// The thread the index event being applied removed, so the destination
    /// can still follow it to its parent.
    removed_session: Option<SessionMeta>,
    settings_replica: Settings,
    /// The settings this device keeps for the space it is a member of could
    /// not be read: the workspace runs on defaults and writes none over them.
    member_settings_unreadable: bool,
    /// Whether `settings_replica` is the host's settings or still the local
    /// defaults it was constructed with. Views that copy a setting into an
    /// editable input must not treat the defaults as the host's answer.
    settings_hydrated: bool,
    baseline_topics: HashSet<Topic>,
    applied_scope_request: Option<u64>,
    index_hydrated: bool,
    /// Whether the shell shows a conversation, not a compact window's
    /// thread list, which keeps the thread open behind it.
    conversation_on_screen: bool,
    /// The open threads' replicas, and those of the threads left most
    /// recently ([`KEPT_THREADS`]).
    threads: HashMap<String, ThreadReplica>,
    /// Threads a surface still draws after they close, by how many hold
    /// them: a closed thread keeps its timeline while any does.
    timeline_leases: HashMap<String, usize>,
    selection_generation: u64,
    providers_replica: ProvidersStatus,
    /// One-shot turn navigation requested by a content-search hit or an
    /// anchor link.
    pending_chat_turn: Option<(String, usize)>,
    /// The host resolving an anchor link's position to the turn it opens at.
    anchor_turn_task: Option<Task<()>>,
    native_rewind_prefills: HashMap<String, RewindPrefill>,
    /// The parent of each thread that left the index, for the pane left
    /// without it.
    left_parents: HashMap<String, String>,
    /// Text a pull request action put into a thread's composer for the user to edit and send.
    composer_appends: HashMap<String, String>,
    conversation_ui: HashMap<ConversationDestination, ConversationUiState>,
    /// The pull request each thread has open, in this run only.
    open_pull_requests: HashMap<String, tcode_core::pull_request::PullRequestKey>,
    /// A project-draft fallback is in flight, so the reconcile step does not
    /// ask for one more draft per index event while it resolves.
    draft_fallback_pending: bool,
    creation_request_generation: u64,
    /// Where an Ack the host refused goes to be shown, and the executor that
    /// waits for it.
    refused: async_channel::Sender<ProtocolError>,
    /// A space's settings on their way to the device's preferences, under
    /// the key that names the space.
    member_settings: async_channel::Sender<(String, serde_json::Value)>,
    background: gpui::BackgroundExecutor,
}

/// What an envelope changed, and so whom applying it tells.
enum Reach {
    /// What the store's readers at large may read: they are all told.
    Store,
    /// Only these parts of one thread's replica: only their readers are.
    Thread(String, &'static [Part]),
    /// Only one terminal's grid: only what draws it, and what drains its
    /// bell and clipboard, is told.
    Terminal(u64),
    /// Only these threads' previews: only their readers are.
    Preview(Vec<String>),
}

/// Where a window of a thread's log sits in the whole log.
struct SnapshotWindow {
    from: u64,
    end: u64,
    total: u64,
    total_turns: usize,
}

/// How many threads the user left keep their replicas besides the open
/// ones. Reopening a kept thread sends its cursor and gets only what it
/// missed; any other thread gets a baseline, as on first open. A thread
/// receives nothing while it is not open, so keeping one longer saves no
/// more than that baseline: the bound is on how many are kept, not for how
/// long.
const KEPT_THREADS: usize = 4;

/// A message the thread can be rewound to, as the rewind panel presents it.
#[derive(Debug, Clone, PartialEq)]
pub struct RewindTarget {
    pub checkpoint_id: String,
    /// The scopes the Harness restores this message in, in its order.
    pub modes: Vec<agent::RewindMode>,
    pub rewind: tcode_protocol::RewindStatus,
    /// The message ran in a session a handoff left: the rewind takes the
    /// thread back to `harness`, in that session.
    pub returns: bool,
    /// This rewind is being made now.
    pub in_progress: bool,
    /// Turns the cut takes out of the conversation, this one included; for a
    /// steer, the turns after its own.
    pub later_turns: usize,
    /// Delegated threads started from this thread after the message.
    pub delegated_after: usize,
    pub queued: usize,
    pub turn_running: bool,
    /// The thread works in a worktree of its own, which files are restored in.
    pub worktree: bool,
    /// The product name of the thread's Harness.
    pub harness: String,
}

/// A finished turn the thread can be forked through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkTarget {
    /// Counted from the thread's first.
    pub turn: usize,
    pub checkpoint_id: Option<String>,
    pub blocked: bool,
}

/// A prompt a rewind gave back for this client's composer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindPrefill {
    pub text: String,
    pub attachments: Vec<String>,
}

/// A rendered thread export as it arrives from the host: complete bytes, a file
/// name that is legal on any client OS, and the type to hand a download or
/// share sheet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadExportArtifact {
    pub bytes: Vec<u8>,
    pub suggested_name: String,
    pub mime: String,
}

/// Settings → Archived Threads → Delete all: the listed archived threads, the
/// threads under them that are not archived and go with them, and the threads
/// whose deletion removes exactly that set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivedDeletion {
    pub archived: usize,
    pub unarchived: usize,
    roots: Vec<String>,
}

pub(crate) struct DiffActiveState {
    pub session: String,
    pub cwd: PathBuf,
    pub branches: Vec<String>,
}

pub(crate) struct CommitDialogState {
    pub files: Vec<GitFileEntry>,
    pub branch: Option<String>,
    pub on_default_branch: bool,
    /// git's own error when it could not read the status.
    pub error: Option<String>,
}

fn protocol_io_error(message: impl Into<String>) -> std::io::Error {
    std::io::Error::other(message.into())
}

impl WorkspaceStore {
    fn destination(status: &SessionStatus) -> ConversationDestination {
        if status.draft
            && let Some(project_id) = status.project_id.clone()
        {
            ConversationDestination::ProjectDraft(project_id)
        } else {
            ConversationDestination::Thread(status.session_id.clone())
        }
    }

    /// Construct the complete projection for exactly one client link. It
    /// renders at once and again as the host's snapshots land.
    pub fn new_attached(
        host: HostLink,
        attachment: WorkspaceAttachment,
        client_host: Option<Rc<dyn ClientHost>>,
        current_host: Option<LiveHost>,
        cx: &mut Context<Self>,
    ) -> Self {
        let image_host = images::HostImages::open(host.clone(), cx);
        let device = crate::device::DevicePreferences::entity(cx);
        let device_name = cx.observe(&device, |store, _, cx| {
            store.rename_preview_client(crate::device::DevicePreferences::device_name(cx))
        });
        let remote = matches!(attachment, WorkspaceAttachment::Remote { .. });
        let (refused, refusals) = async_channel::unbounded();
        let (member_settings, member_settings_changes) = async_channel::unbounded();
        let mut store = Self {
            host: host.clone(),
            scope: WorkspaceScope::Full,
            space_policy: None,
            scoped_providers: Vec::new(),
            attachment,
            client_host,
            current_host,
            image_host,
            attachment_tasks: Vec::new(),
            _device_name: device_name,
            terminals: HashMap::new(),
            remote_preview: async_channel::unbounded(),
            preview: preview::PreviewReplica::new(
                crate::device::DevicePreferences::device_name(cx),
                !remote,
            ),
            import_statuses: HashMap::new(),
            connection_state: if remote {
                host.connection_state()
            } else {
                ConnectionState::Connected { path: None }
            },
            host_clock_offset_ms: 0,
            index_replica: (Vec::new(), Vec::new()),
            index_positions: HashMap::new(),
            index_summary: IndexSummary::default(),
            thread_list_revision: 0,
            archived_replica: None,
            archived_failure: None,
            archived_holds: 0,
            archived_task: None,
            storage_migration: None,
            storage_migration_holds: 0,
            disk_usage: DiskUsageState::default(),
            disk_usage_holds: 0,
            removed_session: None,
            settings_replica: Settings::default(),
            member_settings_unreadable: false,
            settings_hydrated: false,
            baseline_topics: HashSet::new(),
            applied_scope_request: None,
            index_hydrated: false,
            conversation_on_screen: false,
            threads: HashMap::new(),
            timeline_leases: HashMap::new(),
            selection_generation: 0,
            providers_replica: ProvidersStatus::default(),
            pending_chat_turn: None,
            left_parents: HashMap::new(),
            anchor_turn_task: None,
            native_rewind_prefills: HashMap::new(),
            composer_appends: HashMap::new(),
            conversation_ui: HashMap::new(),
            open_pull_requests: HashMap::new(),
            draft_fallback_pending: false,
            creation_request_generation: 0,
            refused,
            member_settings,
            background: cx.background_executor().clone(),
        };
        store.attachment_tasks.push(cx.spawn(async move |this, cx| {
            while let Ok((key, settings)) = member_settings_changes.recv().await {
                if this
                    .update(cx, |_, cx| {
                        crate::device::DevicePreferences::set_member_settings(&key, settings, cx)
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
        store.attachment_tasks.push(cx.spawn(async move |this, cx| {
            while let Ok(error) = refusals.recv().await {
                if this
                    .update(cx, |_, cx| {
                        cx.emit(RuntimeEvent::Error(tcode_protocol::RuntimeError::External(
                            error.message,
                        )))
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
        let _ = host.subscribe(Subscription {
            known_index: None,
            topic: Topic::Scope,
            after: None,
        });
        {
            let event_messages = host.events();
            store.attachment_tasks.push(cx.spawn(async move |this, cx| {
                while let Ok(envelope) = event_messages.recv().await {
                    if this
                        .update(cx, |store, cx| store.receive(envelope, cx))
                        .is_err()
                    {
                        break;
                    }
                }
            }));
        }

        {
            let delivery_changes = host.delivery_changes();
            store.attachment_tasks.push(cx.spawn(async move |this, cx| {
                while delivery_changes.recv().await.is_ok() {
                    if this
                        .update(cx, |_, cx| {
                            cx.emit(StoreChange {
                                topic: TopicKind::SessionEvents,
                            });
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }));
        }

        if remote {
            let changes = host.connection_state_changes();
            store.attachment_tasks.push(cx.spawn(async move |this, cx| {
                while let Ok(state) = changes.recv().await {
                    if this
                        .update(cx, |store, cx| {
                            cx.emit(state.clone());
                            store.apply_connection_state(state);
                            cx.emit(StoreChange {
                                topic: TopicKind::Index,
                            });
                            cx.emit(StoreChange {
                                topic: TopicKind::SessionStatus,
                            });
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }));
        }

        store
    }

    #[cfg(preview_backend)]
    pub(crate) fn preview_reply(
        &mut self,
        request_id: u64,
        response: Result<tcode_protocol::PreviewResponse, String>,
    ) {
        self.dispatch(tcode_protocol::Command::PreviewReply {
            request_id,
            response,
        });
    }

    #[cfg(preview_backend)]
    pub(crate) fn remote_preview_requests(&self) -> async_channel::Receiver<EventEnvelope> {
        self.remote_preview.1.clone()
    }

    /// The paired machine Preview reaches and the tunnels that reach it.
    #[cfg(preview_backend)]
    pub(crate) fn preview_proxy(&self) -> Result<Option<PreviewTarget>, String> {
        if !self.is_remote() {
            return Ok(None);
        }
        self.machine_tunnels()
            .map(Some)
            .ok_or_else(|| "remote preview requires a paired machine connection".into())
    }

    /// The paired machine this store is attached to and the tunnels its
    /// transport opens to it; `None` for the host in this process or a
    /// transport that carries no tunnels.
    pub(crate) fn machine_tunnels(
        &self,
    ) -> Option<(
        tcode_client::pairing::PairedHost,
        std::sync::Arc<dyn tcode_client::host::TunnelOpener>,
    )> {
        self.current_host
            .as_ref()
            .filter(|live| Some(live.snapshot().host_id.as_str()) == self.remote_host_id())
            .and_then(|live| Some((live.snapshot(), live.tunnels()?)))
    }

    /// Now by the host's clock, in unix milliseconds. How long ago or how
    /// soon a time the host gave is, whether it stamped it or passed it on
    /// from a harness or a forge, is told against this, not against this
    /// client's clock.
    pub(crate) fn host_now_millis(&self, cx: &App) -> u64 {
        crate::time::now_millis(cx).saturating_add_signed(self.host_clock_offset_ms)
    }

    /// [`Self::host_now_millis`] in unix seconds.
    pub(crate) fn host_now_secs(&self, cx: &App) -> u64 {
        self.host_now_millis(cx) / 1000
    }

    /// The moment the host's clock reads `host_ms` (unix milliseconds) on
    /// this client's clock, for a [`crate::widgets::clock::Clock`], which
    /// ticks on this client's.
    pub(crate) fn host_moment_here(&self, host_ms: u64) -> u64 {
        host_ms.saturating_add_signed(-self.host_clock_offset_ms)
    }

    /// [`Self::host_moment_here`] in unix seconds.
    pub(crate) fn host_moment_here_secs(&self, host_secs: u64) -> u64 {
        self.host_moment_here(host_secs.saturating_mul(1000)) / 1000
    }

    pub(crate) fn can_manage_host(&self) -> bool {
        self.scope.is_full() && self.baseline_topics.contains(&Topic::Scope)
    }

    /// Whether this workspace may do what `allowed` asks of its space's
    /// policy: always on a full workspace.
    pub fn space_allows(&self, allowed: impl FnOnce(&tcode_protocol::SpacePolicy) -> bool) -> bool {
        self.space_policy.as_ref().is_none_or(allowed)
    }

    /// The space policy this workspace works under, when it is a member's.
    pub fn space_policy(&self) -> Option<&tcode_protocol::SpacePolicy> {
        self.space_policy.as_ref()
    }

    pub fn scope(&self) -> &WorkspaceScope {
        &self.scope
    }

    /// Whether this client shows the Preview tab at all: only where it can
    /// embed a browser. A browser tab already reaches the machine by address,
    /// so a dev server listening on the LAN opens in a new tab there instead.
    pub(crate) fn offers_preview(&self) -> bool {
        self.scope.is_full() && crate::preview_backend()
    }

    fn index_topic(&self) -> Topic {
        match &self.scope {
            WorkspaceScope::Full => Topic::Index,
            WorkspaceScope::Space { space_id, .. } => Topic::SpaceIndex {
                space_id: space_id.clone(),
            },
        }
    }

    /// The name the host goes by wherever the window shows several.
    pub(crate) fn host_title(&self) -> gpui::SharedString {
        match self.host_key() {
            HostKey::Local => crate::tr!("hosts.this_computer").into_owned().into(),
            HostKey::Remote(_) => self.machine_label().into(),
        }
    }

    pub fn machine_label(&self) -> String {
        let machine = self.remote_host_name().unwrap_or_default();
        match &self.scope {
            WorkspaceScope::Full => machine.to_owned(),
            WorkspaceScope::Space { space_name, .. } => {
                crate::tr!("member.connection", space = space_name, machine = machine).into_owned()
            }
        }
    }

    pub fn space_label(&self) -> Option<String> {
        match &self.scope {
            WorkspaceScope::Full => None,
            WorkspaceScope::Space { space_name, .. } => {
                Some(crate::tr!("member.space", space = space_name).into_owned())
            }
        }
    }

    fn member_settings_key(&self) -> String {
        match &self.scope {
            WorkspaceScope::Space { space_id, .. } => {
                format!("{}:{space_id}", self.remote_host_id().unwrap_or_default())
            }
            WorkspaceScope::Full => String::new(),
        }
    }

    /// Hand this space's settings to the device's preferences, which the
    /// next turn of the executor saves: the changes come from places that
    /// hold no context.
    fn save_member_settings(&self) {
        if self.member_settings_unreadable {
            return;
        }
        let _ = self.member_settings.try_send((
            self.member_settings_key(),
            serde_json::to_value(&self.settings_replica).unwrap(),
        ));
    }

    fn apply_scope(&mut self, scope: &Scope, cx: &mut Context<Self>) {
        let next = match scope {
            Scope::Full => WorkspaceScope::Full,
            Scope::Space {
                space_id,
                space_name,
                ..
            } => WorkspaceScope::Space {
                space_id: space_id.clone(),
                space_name: space_name.clone(),
            },
        };
        let changed = self.scope != next;
        let needs_baseline = changed || self.applied_scope_request.is_none();
        if changed {
            for subscription in self.host.subscriptions() {
                if subscription.topic != Topic::Scope {
                    let _ = self.host.unsubscribe(subscription);
                }
            }
            self.selection_moved_on();
            for session_id in self.open_threads() {
                self.close_thread(&session_id, cx);
            }
            self.threads.clear();
            self.conversation_ui.clear();
            self.index_replica = Default::default();
            self.reindex_sessions();
            self.index_summary = Default::default();
            self.draft_fallback_pending = false;
        }
        if needs_baseline {
            self.archived_replica = None;
            self.archived_failure = None;
            self.archived_task = None;
            self.baseline_topics.clear();
            self.index_hydrated = false;
            self.settings_hydrated = false;
            self.providers_replica = Default::default();
            self.scoped_providers.clear();
        }
        self.baseline_topics.insert(Topic::Scope);
        self.scope = next;
        self.space_policy = match scope {
            Scope::Full => None,
            Scope::Space { policy, .. } => Some(policy.clone()),
        };
        match scope {
            Scope::Full if needs_baseline => {
                for topic in [
                    Topic::Settings,
                    Topic::Index,
                    Topic::Providers,
                    Topic::RuntimeEvents,
                ] {
                    let _ = self.host.subscribe(Subscription {
                        known_index: (topic == Topic::Index)
                            .then(|| self.cached_index_fingerprint())
                            .flatten(),
                        topic,
                        after: None,
                    });
                }
                // Only a client that can render pages offers to: the host
                // hands an agent's requests to a client that renders them.
                if crate::preview_backend() {
                    self.offer_preview();
                }
                for session_id in self.open_threads() {
                    self.reopen_thread(&session_id, cx);
                }
            }
            Scope::Full => {}
            Scope::Space {
                space_name,
                projects,
                providers,
                ..
            } => {
                if changed {
                    let saved = crate::device::DevicePreferences::member_settings(
                        &self.member_settings_key(),
                        cx,
                    );
                    self.member_settings_unreadable = false;
                    self.settings_replica = match saved.map(serde_json::from_value) {
                        None => Settings::default(),
                        Some(Ok(settings)) => settings,
                        Some(Err(error)) => {
                            self.member_settings_unreadable = true;
                            cx.emit(RuntimeEvent::Error(
                                tcode_protocol::RuntimeError::FileUnreadable {
                                    error: format!(
                                        "this device's settings for {space_name}: {error}"
                                    ),
                                },
                            ));
                            Settings::default()
                        }
                    };
                }
                self.settings_hydrated = true;
                self.index_replica.1 = projects.clone();
                self.scoped_providers = providers.clone();
                self.providers_replica = Default::default();
                for choice in providers {
                    self.providers_replica
                        .model_catalogs
                        .entry(choice.provider)
                        .or_default()
                        .extend(choice.models.clone());
                }
                if needs_baseline {
                    let _ = self.host.subscribe(Subscription {
                        known_index: self.cached_index_fingerprint(),
                        topic: self.index_topic(),
                        after: None,
                    });
                }
            }
        }
        for topic in [TopicKind::Settings, TopicKind::Providers, TopicKind::Index] {
            cx.emit(StoreChange { topic });
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self.attachment, WorkspaceAttachment::Remote { .. })
    }

    pub fn host_key(&self) -> HostKey {
        match &self.attachment {
            WorkspaceAttachment::Local => HostKey::Local,
            WorkspaceAttachment::Remote { host_id, .. } => HostKey::Remote(host_id.clone()),
        }
    }

    /// This host's thread `session_id`.
    pub(crate) fn thread_key(&self, session_id: &str) -> ThreadKey {
        ThreadKey {
            host: self.host_key(),
            session_id: session_id.to_owned(),
        }
    }

    /// `thread`'s session id, when it is one of this host's threads.
    pub(crate) fn session_of(&self, thread: ThreadKey) -> Option<String> {
        (thread.host == self.host_key()).then_some(thread.session_id)
    }

    pub fn remote_host_name(&self) -> Option<&str> {
        match &self.attachment {
            WorkspaceAttachment::Local => None,
            WorkspaceAttachment::Remote { host_name, .. } => Some(host_name),
        }
    }

    /// The host this store's pictures are read from.
    pub(crate) fn image_host(&self) -> images::ImageHost {
        self.image_host
    }

    pub fn remote_host_id(&self) -> Option<&str> {
        match &self.attachment {
            WorkspaceAttachment::Local => None,
            WorkspaceAttachment::Remote { host_id, .. } => Some(host_id),
        }
    }

    /// Connected only once the baseline is in: the transport's `Connected`
    /// says the host answers, the replayed snapshots say the screen is current.
    /// Either way the path is the transport's.
    pub fn connection_state(&self) -> ConnectionState {
        match &self.connection_state {
            ConnectionState::Connected { path } if !self.baseline_ready() => {
                ConnectionState::Syncing { path: path.clone() }
            }
            ConnectionState::Syncing { path } if self.baseline_ready() => {
                ConnectionState::Connected { path: path.clone() }
            }
            state => state.clone(),
        }
    }

    /// A `Syncing` that only renames the path of a sync already under way
    /// keeps the baseline collected so far.
    fn apply_connection_state(&mut self, state: ConnectionState) {
        let restarts = match &state {
            ConnectionState::Syncing { .. } => {
                !matches!(self.connection_state, ConnectionState::Syncing { .. })
            }
            ConnectionState::Reconnecting { .. } | ConnectionState::Offline { .. } => true,
            ConnectionState::Connected { .. } => matches!(
                self.connection_state,
                ConnectionState::Reconnecting { .. } | ConnectionState::Offline { .. }
            ),
        };
        // State notifications and events use separate queues; a delayed disconnect
        // must not clear baselines already applied for the current Scope request.
        let scope_is_current = self.applied_scope_request.is_some()
            && self.applied_scope_request == self.host.subscription_request_id(&Topic::Scope);
        let disconnected = matches!(
            self.host.connection_state(),
            ConnectionState::Reconnecting { .. } | ConnectionState::Offline { .. }
        );
        if restarts && (!scope_is_current || disconnected) {
            self.baseline_topics.clear();
            self.archived_task = None;
        }
        self.connection_state = state;
    }

    /// The threads a message is still on its way to, with its text: what
    /// the window opens and lists while the host does not have it. Other
    /// writes naming a thread, such as the read receipt a phone leaves
    /// behind when it is suspended, are not messages and lead nowhere.
    pub(crate) fn pending_sessions(&self) -> Vec<(String, String)> {
        let mut sessions = Vec::new();
        for (_, command) in self.host.pending_commands() {
            let (id, preview) = match command {
                Command::SendTurn {
                    session_id, text, ..
                }
                | Command::ScheduleTurn {
                    session_id, text, ..
                }
                | Command::Steer {
                    session_id, text, ..
                }
                | Command::ConfirmRelayAndSend {
                    session_id, text, ..
                } => (session_id, text),
                _ => continue,
            };
            if !sessions.iter().any(|(existing, _)| *existing == id) {
                sessions.push((id, preview));
            }
        }
        sessions
    }

    /// The messages sent to `session_id`'s thread that its timeline does
    /// not hold yet: key, text, failure, and whether the host has it.
    pub(crate) fn delivery_messages_for(
        &self,
        session_id: Option<&str>,
        cx: &App,
    ) -> Vec<(String, String, Option<String>, bool)> {
        let active = session_id.unwrap_or_default();
        let message = |command: &Command| match command {
            Command::SendTurn {
                session_id, text, ..
            }
            | Command::ScheduleTurn {
                session_id, text, ..
            }
            | Command::Steer {
                session_id, text, ..
            }
            | Command::ConfirmRelayAndSend {
                session_id, text, ..
            } if session_id == active => Some(text.clone()),
            _ => None,
        };
        self.host
            .pending_commands()
            .into_iter()
            .filter_map(|(key, command)| message(&command).map(|text| (key, text, None, false)))
            .chain(
                self.host
                    .failed_commands()
                    .into_iter()
                    .filter_map(|(entry, error)| {
                        message(&entry.command)
                            .map(|text| (entry.key, text, Some(error.message), false))
                    }),
            )
            .chain(
                self.host
                    .acknowledged_messages()
                    .into_iter()
                    .filter_map(|entry| {
                        let text = message(&entry.command)?;
                        // It stops showing once its thread's status queues it
                        // or its log records it.
                        let thread = self.watched(Some(active), Part::Timeline, cx);
                        if let Some(thread) = thread {
                            thread.watch(Part::Status, cx);
                        }
                        let queued = thread
                            .and_then(|thread| thread.status.as_ref())
                            .is_some_and(|status| {
                                status.queued_messages.iter().any(|message| {
                                    message.delivery_key.as_deref() == Some(entry.key.as_str())
                                })
                            });
                        let recorded = thread
                            .and_then(|thread| thread.history.as_ref())
                            .is_some_and(|held| {
                                held.records.iter().any(|record| {
                                    Self::record_delivery_key(record) == Some(entry.key.as_str())
                                })
                            });
                        if queued || recorded {
                            // Once adopted by the host replica, a later rewind must not
                            // resurrect the acknowledged placeholder.
                            self.host.retire_acknowledged_message(&entry.key);
                            None
                        } else {
                            Some((entry.key, text, None, true))
                        }
                    }),
            )
            .collect()
    }

    fn record_delivery_key(record: &StoredEvent) -> Option<&str> {
        let id = match &record.event {
            agent::AgentEvent::SteerRequested { request_id, .. } => request_id.as_str(),
            agent::AgentEvent::ItemCompleted(item) | agent::AgentEvent::ItemStarted(item) => {
                item.id.as_str()
            }
            _ => return None,
        };
        id.strip_prefix("local-user-")
            .or_else(|| id.strip_prefix("local-steer-"))
    }

    fn retire_record_delivery(&self, session_id: &str, record: &StoredEvent) {
        if let Some(key) = Self::record_delivery_key(record) {
            self.retire_delivery_for(session_id, key);
        }
    }

    fn retire_delivery_for(&self, session_id: &str, key: &str) {
        let matches_session = self
            .host
            .acknowledged_messages()
            .iter()
            .any(|entry| entry.key == key && entry.command.session_id() == Some(session_id))
            || self
                .host
                .pending_commands()
                .iter()
                .any(|(pending_key, command)| {
                    pending_key == key && command.session_id() == Some(session_id)
                });
        if matches_session {
            self.host.retire_acknowledged_message(key);
        }
    }

    pub(crate) fn approval_delivery_pending_for(
        &self,
        session: Option<&str>,
        request: &str,
    ) -> bool {
        self.host.pending_commands().iter().any(|(_, command)| {
            matches!(command,
            Command::RespondApproval { request_id, session_id, .. }
            if request_id == request && Some(session_id.as_str()) == session)
        })
    }

    pub(crate) fn user_input_delivery_pending_for(&self, session: &str, request: &str) -> bool {
        self.host.pending_commands().iter().any(|(_, command)| {
            matches!(command,
            Command::RespondUserInput { request_id, session_id, .. }
            if request_id == request && session_id == session)
        })
    }

    pub(crate) fn retry_delivery(&self, key: &str, cx: &mut Context<Self>) {
        if let Err(error) = self.host.retry_failed(key) {
            cx.emit(RuntimeEvent::Error(tcode_protocol::RuntimeError::External(
                error.message,
            )));
        }
    }

    pub(crate) fn discard_delivery(&self, key: &str) {
        self.host.discard_failed(key);
    }

    pub fn queued_outgoing(&self) -> usize {
        self.host.queued_outgoing()
    }

    /// The workspace's baselines have arrived, and those of every open
    /// thread.
    pub fn baseline_ready(&self) -> bool {
        self.baseline_ready_for(None)
            && self
                .open_threads()
                .iter()
                .all(|id| self.baseline_ready_for(Some(id)))
    }

    /// The workspace's baselines have arrived, and those of `session_id`'s
    /// thread when there is one.
    pub fn baseline_ready_for(&self, session_id: Option<&str>) -> bool {
        self.baseline_topics.contains(&Topic::Scope)
            && self.baseline_topics.contains(&self.index_topic())
            && (!self.scope.is_full() || self.baseline_topics.contains(&Topic::Settings))
            && session_id.is_none_or(|id| {
                self.baseline_topics.contains(&Topic::SessionStatus {
                    session_id: id.to_owned(),
                }) && self.baseline_topics.contains(&Topic::SessionPlan {
                    session_id: id.to_owned(),
                }) && self.baseline_topics.contains(&Topic::SessionEvents {
                    session_id: id.to_owned(),
                })
            })
    }

    /// Cached content remains readable while a new baseline is replayed.
    pub fn threads_loading(&self) -> bool {
        !self.index_hydrated
            || !self.settings_hydrated
            || (!self.connection_state().is_connected()
                && self.index_replica.0.is_empty()
                && self.index_replica.1.is_empty())
    }

    /// Whether a conversation showing `session_id`, or no thread, is still
    /// waiting for what it draws.
    pub fn chat_loading_for(&self, session_id: Option<&str>, cx: &App) -> bool {
        if !self.delivery_messages_for(session_id, cx).is_empty() {
            return false;
        }
        if !self.index_hydrated || !self.settings_hydrated {
            return true;
        }
        match session_id {
            Some(id) => self.session_loading_for(id),
            None => self.threads_loading(),
        }
    }

    /// End this store's one-link lifetime before its views are replaced.
    pub fn detach(&mut self, cx: &mut App) {
        for thread in self.threads.values_mut() {
            thread.history_task = None;
            thread.history_trim = None;
        }
        self.attachment_tasks.clear();
        for subscription in self.host.subscriptions() {
            let _ = self.host.unsubscribe(subscription);
        }
        self.host.close();
        self.remote_preview.0.close();
        self.remote_preview.1.close();
        images::HostImages::close(self.image_host, cx);
    }

    /// Point the thread's conversation state at where its status says it
    /// lives. A draft that became a thread takes its draft's state along.
    fn sync_conversation_ui(&mut self, session_id: &str) {
        let Some(thread) = self.threads.get(session_id) else {
            return;
        };
        let destination = thread.status.as_ref().map(Self::destination);
        if let (
            Some(tcode_core::ui::ConversationDestination::ProjectDraft(draft)),
            Some(tcode_core::ui::ConversationDestination::Thread(_)),
        ) = (&thread.destination, &destination)
            && thread
                .status
                .as_ref()
                .is_some_and(|status| status.project_id.as_deref() == Some(draft.as_str()))
            && let Some(ui) = self
                .conversation_ui
                .remove(thread.destination.as_ref().unwrap())
        {
            self.conversation_ui
                .insert(destination.clone().unwrap(), ui);
        }
        if let Some((destination, status)) = destination.clone().zip(thread.status.as_ref()) {
            self.conversation_ui.entry(destination).or_insert_with(|| {
                ConversationUiState::new(
                    self.settings_replica.word_wrap_diffs,
                    status.terminal_open,
                    status.terminal_height,
                )
            });
        }
        if let Some(thread) = self.threads.get_mut(session_id) {
            thread.destination = destination;
        }
    }

    fn thread(&self, session_id: Option<&str>) -> Option<&ThreadReplica> {
        self.threads.get(session_id?)
    }

    /// The thread's replica, read as a view depending on its `part`: what
    /// changes there alone, as records stream in or a status is replaced,
    /// draws such a view again without the store being notified.
    fn watched(&self, session_id: Option<&str>, part: Part, cx: &App) -> Option<&ThreadReplica> {
        let thread = self.thread(session_id)?;
        thread.watch(part, cx);
        Some(thread)
    }

    fn thread_status(&self, session_id: Option<&str>, cx: &App) -> Option<&SessionStatus> {
        self.watched(session_id, Part::Status, cx)?.status.as_ref()
    }

    fn thread_timeline(&self, session_id: Option<&str>, cx: &App) -> Option<&Timeline> {
        self.watched(session_id, Part::Timeline, cx)?
            .timeline
            .as_ref()
    }

    fn thread_git(&self, session_id: Option<&str>, cx: &App) -> Option<&GitStatusStatus> {
        self.watched(session_id, Part::Status, cx)?.git.as_ref()
    }

    /// The conversation state of the thread's destination.
    fn thread_conversation_ui(&self, session_id: Option<&str>) -> Option<&ConversationUiState> {
        self.conversation_ui
            .get(self.thread(session_id)?.destination.as_ref()?)
    }

    fn thread_conversation_ui_mut(
        &mut self,
        session_id: Option<&str>,
    ) -> Option<&mut ConversationUiState> {
        let destination = self.thread(session_id)?.destination.clone()?;
        self.conversation_ui.get_mut(&destination)
    }

    /// The thread's replica while a surface shows it; events for any other
    /// thread are not this client's to apply.
    fn open_thread_mut(&mut self, session_id: &str) -> Option<&mut ThreadReplica> {
        self.threads
            .get_mut(session_id)
            .filter(|thread| thread.open)
    }

    pub(crate) fn set_conversation_on_screen(&mut self, on_screen: bool, cx: &mut Context<Self>) {
        if !self.scope.is_full() && on_screen && !self.conversation_on_screen {
            for thread in self.threads.values_mut() {
                thread.read_acknowledged = None;
            }
        }
        self.conversation_on_screen = on_screen;
        self.acknowledge_read(cx);
    }

    /// Report each open thread on screen read through its current
    /// `updated_at` once its conversation has loaded. Only this marks a
    /// thread read: a subscription can reach the host long after the user
    /// left a view that never loaded. An acknowledgement is sent once per
    /// change rather than once per event until the host's visit echo arrives.
    fn acknowledge_read(&mut self, cx: &mut Context<Self>) {
        if !self.conversation_on_screen {
            return;
        }
        let mut acknowledgements = Vec::new();
        for (session_id, thread) in &mut self.threads {
            if !thread.open
                || thread.timeline.is_none()
                || thread.catching_up
                || !self.baseline_topics.contains(&Topic::SessionEvents {
                    session_id: session_id.clone(),
                })
            {
                continue;
            }
            let Some(through) = self
                .index_replica
                .0
                .iter()
                .find(|meta| meta.id == *session_id)
                .map(|meta| meta.updated_at)
            else {
                continue;
            };
            let acknowledged = thread
                .read_acknowledged
                .max(self.settings_replica.last_visited.get(session_id).copied());
            if acknowledged.is_some_and(|at| at >= through) {
                continue;
            }
            thread.read_acknowledged = Some(through);
            acknowledgements.push((session_id.clone(), through));
        }
        for (session_id, through) in acknowledgements {
            self.dispatch(Command::MarkSessionRead {
                session_id,
                through,
            });
            self.local_settings_changed(cx);
        }
    }

    /// Whether a thread is gone from the visible index. A draft stays until
    /// the user navigates away, unless it is deleted or its project leaves
    /// the index: the host lists it for being on screen, which its status
    /// may tell this client of first.
    fn left_index(&self, status: &SessionStatus) -> bool {
        if status.draft {
            let deleted = self
                .removed_session
                .as_ref()
                .is_some_and(|removed| removed.id == status.session_id);
            return deleted
                || status.project_id.as_ref().is_some_and(|project_id| {
                    !self
                        .index_replica
                        .1
                        .iter()
                        .any(|project| project.id == *project_id)
                });
        }
        !self.session_visible(&status.session_id)
    }

    /// Every open thread that left the visible index — archived (alone or
    /// with its lead), deleted — closes, so no surface keeps showing it,
    /// unless it holds an authored write still undelivered, whose failure
    /// needs its Retry and Discard controls on screen. What comes next is
    /// the shell's, whose panes know what they showed
    /// ([`Self::show_after_left`]).
    pub(super) fn reconcile_destination(&mut self, cx: &mut Context<Self>) {
        let left: Vec<String> = self
            .threads
            .iter()
            .filter(|(id, thread)| {
                thread.open
                    && thread
                        .status
                        .as_ref()
                        .is_some_and(|status| self.left_index(status))
                    && self.delivery_messages_for(Some(id), cx).is_empty()
            })
            .map(|(id, _)| id.clone())
            .collect();
        for session_id in left {
            let parent = self
                .index_replica
                .0
                .iter()
                .chain(&self.removed_session)
                .find(|meta| meta.id == session_id)
                .and_then(|meta| meta.parent_session_id.clone());
            match parent {
                Some(parent) => self.left_parents.insert(session_id.clone(), parent),
                None => self.left_parents.remove(&session_id),
            };
            self.close_thread(&session_id, cx);
        }
    }

    /// Show the parent `session_id` left for, while that is visible. True
    /// when the parent is on its way, so the pane it left need not empty.
    pub(crate) fn show_after_left(&mut self, session_id: &str, cx: &mut Context<Self>) -> bool {
        let Some(parent) = self
            .left_parents
            .get(session_id)
            .filter(|parent| self.session_visible(parent))
            .cloned()
        else {
            return false;
        };
        self.place_thread(parent, Some(session_id.to_owned()), false, cx);
        true
    }

    /// After a baseline: a thread still listed is subscribed again, what the
    /// new baseline dropped of it included, and one a scoped host no longer
    /// lists closes.
    fn restore_index_selection(&mut self, cx: &mut Context<Self>) {
        for session_id in self.open_threads() {
            let visible = self
                .index_replica
                .0
                .iter()
                .any(|meta| meta.id == session_id)
                || self
                    .session_status_for(Some(&session_id), cx)
                    .is_some_and(|status| {
                        status.draft
                            && status.project_id.as_ref().is_some_and(|project| {
                                self.index_replica.1.iter().any(|p| &p.id == project)
                            })
                    });
            if visible
                && !self
                    .host
                    .subscribed_topics()
                    .contains(&Topic::SessionStatus {
                        session_id: session_id.clone(),
                    })
            {
                self.reopen_thread(&session_id, cx);
            } else if !visible && !self.scope.is_full() {
                self.close_thread(&session_id, cx);
            }
        }
    }

    /// Subscribe an open thread afresh; a reply in flight is from before.
    fn reopen_thread(&mut self, session_id: &str, cx: &mut App) {
        self.selection_moved_on();
        self.creation_request_generation = self.creation_request_generation.wrapping_add(1);
        self.draft_fallback_pending = false;
        self.open_thread(session_id, cx);
    }

    fn session_visible(&self, session_id: &str) -> bool {
        self.index_replica
            .0
            .iter()
            .any(|meta| meta.id == session_id && meta.archived_at.is_none())
    }

    /// Open the new-thread draft of the project the user last interacted with.
    /// The runtime returns that project's standing draft when it already has
    /// one, keeping its composer attachments. A remembered project that is
    /// gone falls back to the first listed one, and a host with no project
    /// to the quick thread's draft, which only a full workspace may start.
    pub(crate) fn open_fallback_draft(&mut self, cx: &mut Context<Self>) {
        if self.draft_fallback_pending {
            return;
        }
        let remembered = self.settings_replica.last_project_id.as_deref();
        let project = self
            .index_replica
            .1
            .iter()
            .filter(|project| !project.quick_chat)
            .find(|project| Some(project.id.as_str()) == remembered)
            .or_else(|| {
                self.index_replica
                    .1
                    .iter()
                    .find(|project| !project.quick_chat)
            })
            .cloned();
        match project {
            Some(project) => self.fall_back_to_draft(project.id, project.root, cx),
            None if self.scope.is_full() => self.fall_back_to_scratch_draft(cx),
            None => {}
        }
    }

    fn apply_conversation_event(&mut self, session_id: &str, event: &agent::AgentEvent) {
        let destination = ConversationDestination::Thread(session_id.to_string());
        let Some(ui) = self.conversation_ui.get_mut(&destination) else {
            return;
        };
        if matches!(
            event,
            agent::AgentEvent::TurnCompleted { .. } | agent::AgentEvent::RewindCompleted { .. }
        ) {
            ui.refresh_diff()
        }
    }

    pub fn working_sessions_count(&self) -> usize {
        self.index_summary
            .activity
            .values()
            .filter(|activity| activity.working)
            .count()
    }

    /// Threads not working whose harness still runs background work, which
    /// ending the host stops.
    pub fn background_sessions_count(&self) -> usize {
        self.index_summary
            .activity
            .values()
            .filter(|activity| !activity.working && activity.background.tasks > 0)
            .count()
    }

    /// The repository a bare pull request number refers to in this thread, when known.
    pub fn github_repository(&self, session_id: &str, cx: &App) -> Option<String> {
        self.watched(Some(session_id), Part::Status, cx)
            .filter(|thread| thread.open)?
            .git
            .as_ref()?
            .status
            .as_ref()?
            .github_repository
            .clone()
    }

    /// The thread's index entry.
    pub fn thread_meta(&self, session_id: &str) -> Option<&SessionMeta> {
        self.index_replica
            .0
            .get(*self.index_positions.get(session_id)?)
    }

    /// The name the host's model list gives `model` among the models of the
    /// thread's profile; `None` when the list has no such id.
    pub(crate) fn thread_model_name(&self, session_id: &str, model: &str) -> Option<String> {
        let meta = self.thread_meta(session_id)?;
        let profile_id = meta
            .profile_id
            .as_deref()
            .unwrap_or(Settings::builtin_profile_id(meta.provider));
        self.profile_catalog(profile_id)
            .into_iter()
            .find(|spec| spec.id == model)
            .map(|spec| spec.display_name)
    }

    /// The thread `session_id` was forked from: its title, and its id while
    /// it still exists.
    pub(crate) fn fork_source(&self, session_id: &str) -> Option<(String, Option<String>)> {
        let origin = self.thread_meta(session_id)?.fork_origin.as_ref()?;
        let source = self.thread_meta(&origin.thread_id);
        Some((
            source.map_or(origin.title.clone(), |source| source.title.clone()),
            source.map(|source| source.id.clone()),
        ))
    }

    fn reindex_sessions(&mut self) {
        self.index_positions = self
            .index_replica
            .0
            .iter()
            .enumerate()
            .map(|(position, meta)| (meta.id.clone(), position))
            .collect();
    }

    /// The threads `session_id`'s `notify_when` watches follow, in the order it first named
    /// them.
    pub(crate) fn watched_threads(&self, session_id: &str) -> Vec<String> {
        let mut threads: Vec<String> = Vec::new();
        for id in self
            .thread_meta(session_id)
            .into_iter()
            .flat_map(|meta| &meta.control_watches)
            .flat_map(|watch| &watch.thread_ids)
        {
            if !threads.contains(id) {
                threads.push(id.clone());
            }
        }
        threads
    }

    /// The thread's links as its index entry carries them.
    pub fn pull_requests(
        &self,
        session_id: &str,
    ) -> &[tcode_core::pull_request::ThreadPullRequestLink] {
        // A member whose space keeps the remote out of reach works with no
        // pull request: reading one goes through the machine's credentials.
        if !self.space_allows(|policy| policy.git_remote) {
            return &[];
        }
        self.thread_meta(session_id)
            .map_or(&[], |meta| meta.pull_requests.as_slice())
    }

    /// The pull request the thread has open, read inside its Pull requests tab or page.
    pub fn open_pull_request(
        &self,
        session_id: &str,
    ) -> Option<&tcode_core::pull_request::PullRequestKey> {
        self.open_pull_requests.get(session_id)
    }

    /// A pull request the thread no longer shows, unlinked or out of its stack, is not read
    /// on through it: its view closes back to the list.
    fn close_unshown_pull_requests(&mut self) {
        let open = std::mem::take(&mut self.open_pull_requests);
        self.open_pull_requests = open
            .into_iter()
            .filter(|(session_id, key)| {
                tcode_core::pull_request::shown(self.pull_requests(session_id), key)
            })
            .collect();
    }

    pub fn set_open_pull_request(
        &mut self,
        session_id: &str,
        key: Option<tcode_core::pull_request::PullRequestKey>,
        cx: &mut Context<Self>,
    ) {
        let changed = match key {
            Some(key) => {
                self.open_pull_requests
                    .insert(session_id.to_owned(), key.clone())
                    != Some(key)
            }
            None => self.open_pull_requests.remove(session_id).is_some(),
        };
        if changed {
            cx.notify();
        }
    }

    fn conversation_ui_by_key(&self, key: &str) -> Option<&ConversationUiState> {
        self.conversation_ui
            .iter()
            .find_map(|(destination, ui)| (destination.preference_key() == key).then_some(ui))
    }

    /// Whether the conversation `key` names has its right panel open.
    pub(crate) fn right_panel_open_for_key(&self, key: &str) -> bool {
        self.conversation_ui_by_key(key)
            .is_some_and(|ui| ui.right_panel_open)
    }

    /// The project view: projects ordered by the activity of every thread
    /// they hold, agents included, each listing its roster threads newest
    /// updated first. Pinning, arranging and settling order only the threads
    /// view.
    pub fn grouped_sessions(&self) -> Vec<ProjectGroup> {
        let mut groups = group_sessions(
            &self.index_replica.1,
            self.index_replica
                .0
                .iter()
                .filter(|meta| meta.archived_at.is_none()),
            self.settings_replica.project_sort,
        );
        for group in &mut groups {
            group.sessions.retain(tcode_core::thread_sort::in_roster);
            group
                .sessions
                .sort_by_key(|meta| std::cmp::Reverse(meta.updated_at));
        }
        groups
    }

    pub fn settings(&self) -> Settings {
        self.settings_replica.clone()
    }

    /// The settings replica, borrowed, for a read that runs every frame.
    pub(crate) fn settings_ref(&self) -> &Settings {
        &self.settings_replica
    }

    pub(crate) fn thread_list_revision(&self) -> u64 {
        self.thread_list_revision
    }

    pub fn title_generating(&self, session_id: &str) -> bool {
        self.index_summary.title_generating.contains(session_id)
    }

    fn cached_index_fingerprint(&self) -> Option<String> {
        self.index_hydrated
            .then(|| {
                tcode_protocol::index_fingerprint(
                    &self.index_summary,
                    &self.index_replica.0,
                    &self.index_replica.1,
                )
                .ok()
            })
            .flatten()
    }

    fn apply_index_summary(&mut self, summary: &IndexSummary, cx: &mut Context<Self>) {
        self.index_summary = summary.clone();
        if self.archived_holds > 0
            && self
                .archived_replica
                .as_ref()
                .is_none_or(|archived| archived.revision != summary.archived_revision)
            && self
                .archived_failure
                .as_ref()
                .is_none_or(|(revision, _)| *revision != summary.archived_revision)
        {
            self.load_archived_sessions(cx);
        }
    }

    fn load_archived_sessions(&mut self, cx: &mut Context<Self>) {
        if self.archived_task.is_some() {
            return;
        }
        let host = self.host.clone();
        self.archived_task = Some(cx.spawn(async move |this, cx| {
            let result = host.query(Query::ArchivedSessions).await;
            let _ = this.update(cx, |store, cx| {
                store.archived_task = None;
                let failure = match result {
                    Ok(QueryResponse::ArchivedSessions(archived)) => {
                        if archived.revision < store.index_summary.archived_revision {
                            store.load_archived_sessions(cx);
                        } else {
                            store.archived_replica = Some(archived);
                        }
                        None
                    }
                    Ok(other) => Some(format!("unexpected response: {other:?}")),
                    Err(error) => Some(error.message),
                };
                store.archived_failure =
                    failure.map(|message| (store.index_summary.archived_revision, message));
                cx.notify();
            });
        }));
    }

    /// Keep the archived threads current until a matching release. Every
    /// view that shows them holds them, so one closing never empties another.
    pub fn hold_archived_sessions(&mut self, cx: &mut Context<Self>) {
        self.archived_holds += 1;
        if self.archived_holds == 1 {
            self.load_archived_sessions(cx);
        }
    }

    pub fn release_archived_sessions(&mut self) {
        self.archived_holds = self.archived_holds.saturating_sub(1);
        if self.archived_holds == 0 {
            self.archived_replica = None;
            self.archived_failure = None;
            self.archived_task = None;
        }
    }

    pub fn archived_loading(&self) -> bool {
        self.archived_replica.is_none() && self.archived_failure.is_none()
    }

    /// Why the archived threads could not be loaded, until a load succeeds.
    pub fn archived_failure(&self) -> Option<&str> {
        self.archived_failure
            .as_ref()
            .map(|(_, message)| message.as_str())
    }

    pub fn archived_sessions(&self) -> &[SessionMeta] {
        self.archived_replica
            .as_ref()
            .map(|archived| archived.sessions.as_slice())
            .unwrap_or_default()
    }

    /// Whether the Index baseline has arrived, including an empty Index.
    pub fn index_hydrated(&self) -> bool {
        self.index_hydrated
    }

    /// Whether [`WorkspaceStore::settings`] reflects the host yet.
    pub fn settings_hydrated(&self) -> bool {
        self.settings_hydrated
    }

    /// Whether this client can hand a produced file to the platform (a browser
    /// download, a share sheet).
    pub fn supports_artifact_delivery(&self) -> bool {
        self.client_host
            .as_ref()
            .is_some_and(|host| host.supports_artifact_delivery())
    }

    pub fn deliver_artifact(&self, name: &str, mime: &str, bytes: &[u8]) -> Result<(), String> {
        match &self.client_host {
            Some(host) => host.deliver_artifact(name, mime, bytes),
            None => Err("this device cannot save files".into()),
        }
    }

    /// Open a *client-local* path in the user's editor. `None` when this client
    /// has no editor integration at all.
    pub fn open_in_editor(&self, path: &std::path::Path) -> Option<Result<(), String>> {
        self.client_host.as_ref()?.open_in_editor(path)
    }

    /// How an attachment from this client would reach the machine right now.
    pub fn attachment_link(&self) -> crate::attachments::TransferLink {
        use crate::attachments::TransferLink;
        if !self.is_remote() {
            return TransferLink::Local;
        }
        match self.connection_state() {
            tcode_client::ConnectionState::Connected { path }
            | tcode_client::ConnectionState::Syncing { path } => {
                path.map_or(TransferLink::Unknown, |path| match path.kind() {
                    tcode_protocol::PathKind::Lan => TransferLink::Lan,
                    tcode_protocol::PathKind::Tunnel => TransferLink::Tunnel,
                    tcode_protocol::PathKind::Relay { .. } => TransferLink::Relay,
                })
            }
            _ => TransferLink::Unknown,
        }
    }

    pub fn live_command_panel(&self) -> bool {
        !self.settings_replica.live_command_panel_disabled
    }

    /// Whether a command of more than five rows shows only its first four.
    pub fn shortens_long_commands(&self) -> bool {
        self.settings_replica.shorten_long_commands
    }

    /// The `0xRRGGBB` color `meta`'s harness glyph is drawn in on its
    /// sidebar row.
    pub fn provider_color(&self, meta: &SessionMeta) -> u32 {
        self.settings_replica
            .provider_color(&meta.provider_color_key())
    }

    pub fn archived_groups(&self) -> Vec<ProjectGroup> {
        let archived = self
            .archived_replica
            .as_ref()
            .map(|archived| archived.sessions.as_slice())
            .unwrap_or_default();
        let mut groups = group_sessions(
            &self.index_replica.1,
            archived,
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

    pub fn project_sort(&self) -> ProjectSort {
        self.settings_replica.project_sort
    }

    pub fn flat_sessions(&self) -> Vec<SessionMeta> {
        let mut visible: Vec<_> = self
            .index_replica
            .0
            .iter()
            .filter(|meta| tcode_core::thread_sort::in_roster(meta))
            .cloned()
            .collect();
        tcode_core::thread_sort::sort_threads(&mut visible, |meta| self.standing(meta));
        visible
    }

    /// What orders and dates `meta` in the threads view besides the meta
    /// itself: a thread that waits on the user leads the unread ones, those
    /// lead the working ones, and the idle ones follow. A working thread is
    /// not unread yet, as its row shows.
    pub fn standing(&self, meta: &SessionMeta) -> tcode_core::thread_sort::Standing {
        use tcode_core::thread_sort::Attention;
        let id = meta.id.as_str();
        let attention = if self.pending_approval_for(id) || self.pending_user_input_for(id) {
            Attention::Waiting
        } else if self.turn_running_for(id) {
            Attention::Working
        } else if self.session_unread(id) {
            Attention::Unread
        } else {
            Attention::Idle
        };
        tcode_core::thread_sort::Standing {
            attention,
            last_activity_at: self
                .activity(id)
                .and_then(|activity| activity.last_activity_at),
        }
    }

    pub(crate) fn project(&self, id: &str) -> Option<&Project> {
        self.index_replica.1.iter().find(|project| project.id == id)
    }

    /// The project a new thread was last started in.
    pub fn last_project_id(&self) -> Option<&str> {
        self.settings_replica.last_project_id.as_deref()
    }

    pub fn projects(&self) -> Vec<Project> {
        self.index_replica
            .1
            .iter()
            .filter(|project| !project.quick_chat)
            .cloned()
            .collect()
    }

    pub fn is_project_collapsed(&self, project_id: &str) -> bool {
        self.settings_replica
            .collapsed_projects
            .iter()
            .any(|id| id == project_id)
    }

    /// The threads-list row of `session_id`: the thread itself, or for an
    /// agent, the lead it was reached from.
    pub fn roster_row(&self, session_id: &str) -> String {
        let mut id = session_id.to_owned();
        for _ in 0..self.index_replica.0.len() {
            match self
                .thread_meta(&id)
                .and_then(|meta| meta.parent_session_id.clone())
            {
                Some(parent) => id = parent,
                None => break,
            }
        }
        id
    }

    pub fn turn_running_for(&self, session_id: &str) -> bool {
        self.index_summary
            .activity
            .get(session_id)
            .is_some_and(|activity| activity.working)
    }

    /// Whether the host refuses to settle the thread now.
    pub fn settle_blocked_for(&self, session_id: &str) -> bool {
        self.index_summary
            .activity
            .get(session_id)
            .is_some_and(|activity| activity.settle_blocked)
    }

    pub fn failed_for(&self, session_id: &str) -> bool {
        self.index_summary
            .activity
            .get(session_id)
            .is_some_and(|activity| activity.failed)
    }

    /// A dispatched agent's execution and delivery, as its host reports them.
    pub fn agent_status(&self, session_id: &str) -> Option<tcode_protocol::AgentStatus> {
        self.index_summary
            .activity
            .get(session_id)
            .and_then(|activity| activity.agent)
    }

    /// What a thread is doing and what keeps it going, as its host reports
    /// them.
    pub fn activity(&self, session_id: &str) -> Option<&tcode_protocol::SessionActivity> {
        self.index_summary.activity.get(session_id)
    }

    /// What a thread's running turn last started, as its host reports it.
    pub fn latest_activity(&self, session_id: &str) -> Option<tcode_core::session::LatestActivity> {
        self.index_summary
            .activity
            .get(session_id)
            .and_then(|activity| activity.latest_activity.clone())
    }

    pub fn session_unread(&self, session_id: &str) -> bool {
        if !self.scope.is_full() {
            return self.thread_meta(session_id).is_some_and(|meta| {
                !meta.is_settled()
                    && meta.draft_text.is_none()
                    && self
                        .settings_replica
                        .last_visited
                        .get(session_id)
                        .copied()
                        .unwrap_or(0)
                        < meta.updated_at
            });
        }
        self.index_summary
            .activity
            .get(session_id)
            .is_some_and(|activity| activity.unread)
    }

    /// The harness's own subagents waiting on the user through
    /// `session_id`, as its index activity says.
    pub fn asking(&self, session_id: &str) -> &[tcode_protocol::SubagentAsking] {
        self.index_summary
            .activity
            .get(session_id)
            .map(|activity| activity.asking.as_slice())
            .unwrap_or_default()
    }

    pub fn pending_approval_for(&self, session_id: &str) -> bool {
        self.index_summary
            .activity
            .get(session_id)
            .is_some_and(|activity| activity.waiting_for_approval)
    }

    pub fn pending_user_input_for(&self, session_id: &str) -> bool {
        self.index_summary
            .activity
            .get(session_id)
            .is_some_and(|activity| activity.waiting_for_input)
    }

    pub fn sidebar_sessions(&self) -> &[SessionMeta] {
        &self.index_replica.0
    }

    pub fn shell_window_title_for(&self, session_id: Option<&str>, cx: &App) -> String {
        if !self.scope.is_full() {
            return self.machine_label();
        }
        match self.thread_status(session_id, cx) {
            Some(status) if status.draft => crate::tr!("chat.new_thread").into_owned(),
            Some(status) => status.title.clone(),
            None => crate::tr!("app.name").into_owned(),
        }
    }

    /// The stable key of `session_id`'s preview: a project draft's belongs
    /// to its project, whichever draft thread stands for it.
    pub(crate) fn preview_key_for(&self, session_id: &str, cx: &App) -> Option<String> {
        self.thread_status(Some(session_id), cx)
            .map(|status| Self::destination(status).preference_key())
    }

    /// Whether a preview keyed `key` still has a thread to belong to.
    pub(crate) fn preview_key_live(&self, key: &str) -> bool {
        self.thread_meta(key).is_some()
            || self.threads.values().any(|thread| {
                thread.open
                    && thread
                        .destination
                        .as_ref()
                        .is_some_and(|destination| destination.preference_key() == key)
            })
    }

    pub fn preview_browser_settings(&self) -> BrowserSettings {
        self.settings_replica.browser.clone()
    }

    pub fn session_cwd_for(&self, session_id: Option<&str>, cx: &App) -> Option<PathBuf> {
        self.thread_status(session_id, cx)
            .map(|status| status.cwd.clone())
    }

    pub fn project_ids(&self) -> Vec<String> {
        self.index_replica
            .1
            .iter()
            .map(|project| project.id.clone())
            .collect()
    }

    pub fn project_summary(&self, project_id: &str) -> Option<(String, usize)> {
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
            .count()
            + self
                .index_summary
                .archived_counts
                .get(project_id)
                .copied()
                .unwrap_or_default();
        Some((project.name.clone(), count))
    }

    pub fn project_root(&self, project_id: &str) -> Option<PathBuf> {
        self.index_replica
            .1
            .iter()
            .find(|project| project.id == project_id)
            .map(|project| project.root.clone())
    }

    /// Scan the *host's* external-agent histories. A failure is returned rather
    /// than logged away: an empty list and a broken host look identical to the
    /// user otherwise.
    pub fn scan_external_history(
        &self,
        cx: &mut App,
    ) -> Task<Result<tcode_protocol::ExternalHistory, String>> {
        if !self.can_manage_host() {
            return cx.spawn(async |_| Err(crate::tr!("member.unavailable").into_owned()));
        }
        let host = self.host.clone();
        cx.spawn(
            async move |_| match host.query(Query::ScanExternalHistory).await {
                Ok(QueryResponse::ExternalHistory(recent)) => Ok(recent),
                Ok(other) => Err(format!("unexpected external-history response: {other:?}")),
                Err(error) => Err(error.message),
            },
        )
    }

    /// The folders inside a *host* directory, for Add Project's picker. The
    /// path is the host's to resolve (`~` included); its refusal is returned in
    /// its own words.
    pub fn browse_directories(
        &self,
        path: String,
        cx: &mut App,
    ) -> Task<Result<tcode_protocol::DirectoryListing, String>> {
        if !self.can_manage_host() {
            return cx.spawn(async |_| Err(crate::tr!("member.unavailable").into_owned()));
        }
        let host = self.host.clone();
        cx.spawn(
            async move |_| match host.query(Query::BrowseDirectories { path }).await {
                Ok(QueryResponse::Directories(listing)) => Ok(listing),
                Ok(other) => Err(format!("unexpected directory response: {other:?}")),
                Err(error) => Err(error.message),
            },
        )
    }

    /// Subscribe to a project's import status. Callers must do this *before*
    /// starting a run: a fast completion is only recoverable through the
    /// subscription snapshot, not through the start reply.
    pub fn watch_external_import(&self, project_id: &str) {
        if !self.scope.is_full() || !self.baseline_topics.contains(&Topic::Scope) {
            return;
        }
        if let Err(error) = self.host.subscribe(Subscription {
            known_index: None,
            after: None,
            topic: Topic::ExternalImport {
                project_id: project_id.to_string(),
            },
        }) {
            log::error!("failed to watch import status: {}", error.message);
        }
    }

    pub fn unwatch_external_import(&mut self, project_id: &str) {
        let _ = self.host.unsubscribe(Subscription {
            known_index: None,
            after: None,
            topic: Topic::ExternalImport {
                project_id: project_id.to_string(),
            },
        });
        self.import_statuses.remove(project_id);
    }

    /// The latest host-published status for a project, or `None` while the
    /// snapshot is still in flight or no run has ever started.
    pub fn external_import_status(&self, project_id: &str) -> Option<&ExternalImportStatus> {
        self.import_statuses.get(project_id)?.as_ref()
    }

    pub fn start_external_import(
        &self,
        project_id: &str,
        threads: Vec<ExternalThread>,
        cx: &mut App,
    ) -> Task<Result<CommandResponse, ProtocolError>> {
        self.command(
            tcode_protocol::Command::StartExternalImport {
                project_id: project_id.to_string(),
                threads,
            },
            cx,
        )
    }

    /// Ask the host to search its own stored sessions. The index, cache and
    /// session order live there; this client keeps only the answer. A search
    /// that failed whole answers with its failure as what could not be
    /// searched.
    pub fn search_session_content(
        &self,
        query: String,
        limit: u32,
        cx: &mut App,
    ) -> Task<SessionContentHits> {
        let host = self.host.clone();
        cx.spawn(async move |_| {
            let failed = |reason: String| SessionContentHits {
                hits: Vec::new(),
                truncated: false,
                unreadable: vec![reason],
            };
            match host
                .query(Query::SearchSessionContent { query, limit })
                .await
            {
                Ok(QueryResponse::SessionContentHits(found)) => found,
                Ok(other) => failed(format!("unexpected session-content response: {other:?}")),
                Err(error) => failed(error.message),
            }
        })
    }

    /// Ask the host to render a stored thread into transferable bytes. Nothing
    /// is written anywhere: the host owns rendering and its store-flush barrier,
    /// this client owns where the artifact goes.
    pub fn render_thread_export(
        &self,
        session_id: String,
        format: tcode_protocol::ThreadExportFormat,
        cx: &mut App,
    ) -> Task<Result<ThreadExportArtifact, String>> {
        let host = self.host.clone();
        cx.spawn(async move |_| {
            match host
                .query(Query::RenderThreadExport { session_id, format })
                .await
            {
                Ok(QueryResponse::ThreadExport {
                    bytes,
                    suggested_name,
                    mime,
                }) => Ok(ThreadExportArtifact {
                    bytes,
                    suggested_name,
                    mime,
                }),
                Ok(other) => Err(format!("unexpected thread-export response: {other:?}")),
                Err(error) => Err(error.message),
            }
        })
    }

    /// The whole output of an item whose history record carried a preview.
    pub fn read_item_output(
        &self,
        session_id: String,
        item_id: String,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let host = self.host.clone();
        cx.spawn(async move |_| {
            match host
                .query(Query::ReadItemOutput {
                    session_id,
                    item_id,
                })
                .await
            {
                Ok(QueryResponse::ItemOutput(output)) => Ok(output),
                Ok(other) => Err(format!("unexpected item-output response: {other:?}")),
                Err(error) => Err(error.message),
            }
        })
    }

    pub fn computer_use_permissions(
        &self,
        cx: &mut App,
    ) -> Task<Result<tcode_core::permissions::ComputerUsePermissions, String>> {
        if !self.scope.is_full() || !self.baseline_topics.contains(&Topic::Scope) {
            return cx.spawn(async |_| Err(crate::tr!("member.unavailable").into_owned()));
        }
        let host = self.host.clone();
        cx.spawn(
            async move |_| match host.query(Query::ComputerUsePermissions).await {
                Ok(QueryResponse::ComputerUsePermissions(status)) => Ok(status),
                Ok(_) => Err("unexpected computer-use permissions response".into()),
                Err(error) => Err(error.message),
            },
        )
    }

    pub fn hosting(
        &self,
        action: tcode_protocol::HostingAction,
        cx: &mut App,
    ) -> Task<Result<tcode_protocol::HostingState, String>> {
        if !self.scope.is_full() || !self.baseline_topics.contains(&Topic::Scope) {
            return cx.spawn(async |_| Err(crate::tr!("member.unavailable").into_owned()));
        }
        let host = self.host.clone();
        cx.spawn(
            async move |_| match host.query(Query::Hosting { action }).await {
                Ok(QueryResponse::Hosting(state)) => Ok(state),
                Ok(_) => Err("unexpected hosting response".into()),
                Err(error) => Err(error.message),
            },
        )
    }

    /// Follow, or stop following, the hosting state of the machine this
    /// store shows: it arrives as [`tcode_protocol::HostingState`] events.
    pub(crate) fn follow_hosting(&self, follow: bool) {
        let subscription = Subscription {
            known_index: None,
            topic: Topic::Hosting,
            after: None,
        };
        let _ = if follow {
            self.host.subscribe(subscription)
        } else {
            self.host.unsubscribe(subscription)
        };
    }

    /// Follow the host's move of an older build's threads until a matching
    /// release. Every view that shows it holds it, so one closing never
    /// stops another's.
    pub(crate) fn hold_storage_migration(&mut self) {
        self.storage_migration_holds += 1;
        if self.storage_migration_holds == 1 {
            let _ = self.host.subscribe(Subscription {
                known_index: None,
                topic: Topic::StorageMigration,
                after: None,
            });
        }
    }

    pub(crate) fn release_storage_migration(&mut self) {
        self.storage_migration_holds = self.storage_migration_holds.saturating_sub(1);
        if self.storage_migration_holds == 0 {
            let _ = self.host.unsubscribe(Subscription {
                known_index: None,
                topic: Topic::StorageMigration,
                after: None,
            });
            self.storage_migration = None;
        }
    }

    /// The host's move of an older build's threads, while one runs.
    pub(crate) fn storage_migration(&self) -> Option<&StorageMigration> {
        self.storage_migration.as_ref()
    }

    /// Follow the host's disk usage until a matching release; the host
    /// measures when it has nothing measured yet.
    pub(crate) fn hold_disk_usage(&mut self) {
        self.disk_usage_holds += 1;
        if self.disk_usage_holds == 1 {
            let _ = self.host.subscribe(Subscription {
                known_index: None,
                topic: Topic::DiskUsage,
                after: None,
            });
        }
    }

    pub(crate) fn release_disk_usage(&mut self) {
        self.disk_usage_holds = self.disk_usage_holds.saturating_sub(1);
        if self.disk_usage_holds == 0 {
            let _ = self.host.unsubscribe(Subscription {
                known_index: None,
                topic: Topic::DiskUsage,
                after: None,
            });
            self.disk_usage = DiskUsageState::default();
        }
    }

    pub(crate) fn disk_usage(&self) -> &DiskUsageState {
        &self.disk_usage
    }

    /// Ask the host to measure its disk usage again.
    pub(crate) fn measure_disk_usage(&mut self) {
        self.dispatch(Command::MeasureDiskUsage);
    }

    /// The latest `tcode_control` writes across every thread, newest first.
    pub fn native_mcp_servers(
        &self,
        profile_id: String,
        cx: &mut App,
    ) -> Task<Result<Vec<agent::NativeMcpServer>, String>> {
        let host = self.host.clone();
        cx.spawn(
            async move |_| match host.query(Query::NativeMcpServers { profile_id }).await {
                Ok(QueryResponse::NativeMcpServers(servers)) => Ok(servers),
                Ok(_) => Err("unexpected MCP server response".into()),
                Err(error) => Err(error.message),
            },
        )
    }

    pub fn control_activity(
        &self,
        limit: u32,
        filter: tcode_protocol::ControlActivityFilter,
        cx: &mut App,
    ) -> Task<Result<tcode_protocol::ControlActivityPage, String>> {
        if !self.scope.is_full() || !self.baseline_topics.contains(&Topic::Scope) {
            return cx.spawn(async |_| Err(crate::tr!("member.unavailable").into_owned()));
        }
        let host = self.host.clone();
        cx.spawn(
            async move |_| match host.query(Query::ControlActivity { limit, filter }).await {
                Ok(QueryResponse::ControlActivity(page)) => Ok(page),
                Ok(_) => Err("unexpected control activity response".into()),
                Err(error) => Err(error.message),
            },
        )
    }

    pub fn read_file_preview(&self, path: PathBuf, cx: &mut App) -> Task<std::io::Result<Vec<u8>>> {
        let host = self.host.clone();
        cx.spawn(
            async move |_| match host.query(Query::ReadFilePreview { path }).await {
                Ok(QueryResponse::FileBytes(bytes)) => Ok(bytes),
                Ok(other) => Err(protocol_io_error(format!(
                    "unexpected file-bytes response: {other:?}"
                ))),
                Err(error) => Err(protocol_io_error(error.message)),
            },
        )
    }

    /// One read of a linked pull request, its answer taken out by `answer`, such as
    /// [`tcode_protocol::PullRequestReadResponse::files`]. An answer of another kind than the
    /// read asks is a protocol error.
    pub fn read_pull_request<T: 'static>(
        &self,
        session_id: String,
        key: tcode_core::pull_request::PullRequestKey,
        read: tcode_protocol::PullRequestRead,
        answer: impl FnOnce(tcode_protocol::PullRequestReadResponse) -> Option<T> + 'static,
        cx: &mut App,
    ) -> Task<Result<(T, u64), ProtocolError>> {
        let host = self.host.clone();
        cx.spawn(async move |_| {
            match host
                .query(Query::PullRequest {
                    session_id,
                    key,
                    read,
                })
                .await?
            {
                QueryResponse::PullRequest {
                    response,
                    fresh_for,
                } => answer(*response)
                    .map(|answer| (answer, fresh_for))
                    .ok_or_else(unexpected_pull_request_response),
                _ => Err(unexpected_pull_request_response()),
            }
        })
    }

    pub(crate) fn browse_icon_images(
        &self,
        directory: PathBuf,
        cx: &mut App,
    ) -> Task<Result<QueryResponse, ProtocolError>> {
        let host = self.host.clone();
        cx.spawn(async move |_| host.query(Query::BrowseIconImages { directory }).await)
    }

    pub(crate) fn set_project_icon(
        &self,
        project_id: String,
        path: Option<PathBuf>,
        cx: &mut App,
    ) -> Task<Result<tcode_protocol::CommandResponse, ProtocolError>> {
        if !self.scope.is_full() || !self.baseline_topics.contains(&Topic::Scope) {
            return cx.spawn(async |_| {
                Err(ProtocolError::out_of_scope(
                    "project icons are managed by the space owner",
                ))
            });
        }
        let host = self.host.clone();
        cx.spawn(async move |_| {
            let png = if let Some(path) = path {
                match host.query(Query::ReadIconImage { path }).await? {
                    QueryResponse::FileBytes(bytes) => Some(bytes),
                    _ => {
                        return Err(ProtocolError {
                            code: "invalid_image_response".into(),
                            message: "Unexpected image response".into(),
                        });
                    }
                }
            } else {
                None
            };
            host.command(tcode_protocol::Command::SetProjectIcon { project_id, png })
                .await
        })
    }

    pub fn list_workspace_for(
        &self,
        session_id: &str,
        cx: &mut App,
    ) -> Task<Result<tcode_protocol::WorkspaceListing, String>> {
        let session_id = session_id.to_owned();
        let host = self.host.clone();
        cx.spawn(
            async move |_| match host.query(Query::ListActiveWorkspace { session_id }).await {
                Ok(QueryResponse::ActiveWorkspace(listing)) => Ok(listing),
                Ok(other) => Err(format!("unexpected active-workspace response: {other:?}")),
                Err(error) => Err(error.message),
            },
        )
    }

    pub fn save_attachment_to_dir(
        &self,
        dir: PathBuf,
        bytes: Vec<u8>,
        ext: String,
        cx: &mut App,
    ) -> Task<std::io::Result<PathBuf>> {
        let host = self.host.clone();
        cx.spawn(
            async move |_| match host.query(Query::SaveAttachment { dir, bytes, ext }).await {
                Ok(QueryResponse::SavedAttachment(path)) => Ok(path),
                Ok(other) => Err(protocol_io_error(format!(
                    "unexpected save-attachment response: {other:?}"
                ))),
                Err(error) => Err(protocol_io_error(error.message)),
            },
        )
    }

    /// Reserve the host path a client-only file uploads to.
    pub fn begin_upload(
        &self,
        session_id: String,
        name: String,
        size: u64,
        cx: &mut App,
    ) -> Task<std::io::Result<PathBuf>> {
        let host = self.host.clone();
        cx.spawn(async move |_| {
            match host
                .query(Query::BeginUpload {
                    session_id,
                    name,
                    size,
                })
                .await
            {
                Ok(QueryResponse::UploadBegun(path)) => Ok(path),
                Ok(other) => Err(protocol_io_error(format!(
                    "unexpected begin-upload response: {other:?}"
                ))),
                Err(error) => Err(protocol_io_error(error.message)),
            }
        })
    }

    pub fn write_upload(
        &self,
        session_id: String,
        path: PathBuf,
        offset: u64,
        bytes: Vec<u8>,
        cx: &mut App,
    ) -> Task<std::io::Result<()>> {
        let host = self.host.clone();
        cx.spawn(async move |_| {
            match host
                .query(Query::WriteUpload {
                    session_id,
                    path,
                    offset,
                    bytes,
                })
                .await
            {
                Ok(QueryResponse::UploadWritten) => Ok(()),
                Ok(other) => Err(protocol_io_error(format!(
                    "unexpected write-upload response: {other:?}"
                ))),
                Err(error) => Err(protocol_io_error(error.message)),
            }
        })
    }

    pub fn discard_upload(
        &self,
        session_id: String,
        path: PathBuf,
        cx: &mut App,
    ) -> Task<std::io::Result<()>> {
        let host = self.host.clone();
        cx.spawn(
            async move |_| match host.query(Query::DiscardUpload { session_id, path }).await {
                Ok(QueryResponse::UploadDiscarded) => Ok(()),
                Ok(other) => Err(protocol_io_error(format!(
                    "unexpected discard-upload response: {other:?}"
                ))),
                Err(error) => Err(protocol_io_error(error.message)),
            },
        )
    }

    pub fn remove_user_file(&self, path: PathBuf, cx: &mut App) -> Task<std::io::Result<()>> {
        let host = self.host.clone();
        cx.spawn(
            async move |_| match host.query(Query::RemoveUserFile { path }).await {
                Ok(QueryResponse::UserFileRemoved) => Ok(()),
                Ok(other) => Err(protocol_io_error(format!(
                    "unexpected remove-file response: {other:?}"
                ))),
                Err(error) => Err(protocol_io_error(error.message)),
            },
        )
    }
}

impl Drop for WorkspaceStore {
    fn drop(&mut self) {
        for subscription in self.host.subscriptions() {
            let _ = self.host.unsubscribe(subscription);
        }
        self.host.close();
    }
}

impl EventEmitter<RuntimeEvent> for WorkspaceStore {}
/// What the host answered a pull request read or write with, when it is not the kind asked.
pub(crate) fn unexpected_pull_request_response() -> ProtocolError {
    ProtocolError {
        code: "invalid_pull_request_response".into(),
        message: "Unexpected pull request response".into(),
    }
}

impl EventEmitter<StoreChange> for WorkspaceStore {}
impl EventEmitter<TerminalChange> for WorkspaceStore {}
impl EventEmitter<ShowThread> for WorkspaceStore {}
impl EventEmitter<PreviewReleased> for WorkspaceStore {}
impl EventEmitter<ConnectionState> for WorkspaceStore {}
impl EventEmitter<tcode_protocol::HostingState> for WorkspaceStore {}

#[cfg(test)]
pub(crate) mod tests;
