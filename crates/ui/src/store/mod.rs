use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

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
    IndexSummary, PathEntry, ProtocolError, ProviderVersionStatus, ProvidersStatus, Query,
    QueryResponse, Scope, ScopedProviderChoice, ServerEvent, SessionContentHits, SessionPlan,
    SessionStatus, StorageMigration, Subscription, Topic,
};
pub(crate) mod terminal;
pub(crate) use terminal::ClientTerminal;
use terminal::TerminalWorkspace;

use crate::conversation_ui::{ConversationUiState, DiffFocus};

mod command_output;
mod history;
pub(crate) use history::HISTORY_WINDOW_SCREENS;
pub(crate) mod images;
mod intents;
mod preview;
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
    archived_holds: usize,
    archived_task: Option<Task<()>>,
    /// The thread the index event being applied removed, so the destination
    /// can still follow it to its parent.
    removed_session: Option<SessionMeta>,
    settings_replica: Settings,
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
            preview: preview::PreviewReplica::new(crate::device::DevicePreferences::device_name(
                cx,
            )),
            import_statuses: HashMap::new(),
            connection_state: if remote {
                host.connection_state()
            } else {
                ConnectionState::Connected { path: None }
            },
            index_replica: (Vec::new(), Vec::new()),
            index_positions: HashMap::new(),
            index_summary: IndexSummary::default(),
            thread_list_revision: 0,
            archived_replica: None,
            archived_holds: 0,
            archived_task: None,
            storage_migration: None,
            storage_migration_holds: 0,
            disk_usage: DiskUsageState::default(),
            disk_usage_holds: 0,
            removed_session: None,
            settings_replica: Settings::default(),
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
        };
        let _ = host.subscribe(Subscription {
            known_index: None,
            topic: Topic::Scope,
            after: None,
        });
        #[cfg(not(test))]
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

        #[cfg(not(test))]
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
        self.scope.is_full() && crate::preview_panel::preview_backend()
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

    fn save_member_settings(&self) {
        if let Some(host) = &self.client_host {
            let mut preferences = host.load_preferences();
            let navigation = preferences
                .navigation
                .get_or_insert_with(|| serde_json::json!({}));
            navigation["member_settings"][self.member_settings_key()] =
                serde_json::to_value(&self.settings_replica).unwrap();
            host.save_preferences(&preferences);
        }
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
                if crate::preview_panel::preview_backend() {
                    self.offer_preview();
                }
                for session_id in self.open_threads() {
                    self.reopen_thread(&session_id, cx);
                }
            }
            Scope::Full => {}
            Scope::Space {
                projects,
                providers,
                ..
            } => {
                if changed {
                    self.settings_replica = self
                        .client_host
                        .as_ref()
                        .and_then(|host| host.load_preferences().navigation)
                        .and_then(|navigation| {
                            navigation
                                .get("member_settings")?
                                .get(self.member_settings_key())
                                .cloned()
                        })
                        .and_then(|settings| serde_json::from_value(settings).ok())
                        .unwrap_or_default();
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

    /// Outbox-derived navigation only; no host metadata is invented or cached.
    pub(crate) fn pending_sessions(&self) -> Vec<(String, String)> {
        let mut sessions = Vec::new();
        for (_, command) in self.host.pending_commands() {
            let Some(id) = command.session_id().map(str::to_owned) else {
                continue;
            };
            let preview = match command {
                Command::SendTurn { text, .. }
                | Command::ScheduleTurn { text, .. }
                | Command::Steer { text, .. }
                | Command::ConfirmRelayAndSend { text, .. } => text,
                _ => String::new(),
            };
            if !sessions.iter().any(|(existing, _)| existing == &id) {
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

    pub(crate) fn retry_delivery(&self, key: &str) {
        if let Err(error) = self.host.retry_failed(key) {
            log::error!("retry failed: {}", error.message);
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

    /// Apply one envelope from the host and tell whoever it concerns.
    fn receive(&mut self, envelope: EventEnvelope, cx: &mut Context<Self>) {
        let event = match envelope.event {
            // Presented by whoever subscribes; nothing the store holds changes.
            ServerEvent::Runtime(event) => return cx.emit(event),
            event => event,
        };
        let change = StoreChange {
            topic: TopicKind::from(&envelope.topic),
        };
        match self.apply_domain_event(EventEnvelope { event, ..envelope }, cx) {
            Reach::Store => cx.notify(),
            Reach::Thread(session_id, parts) => {
                if let Some(thread) = self.threads.get(&session_id) {
                    for part in parts {
                        thread.bump(*part, cx);
                    }
                }
            }
            // Its grid and label revisions are moved on as it applies.
            Reach::Terminal(terminal_id) => cx.emit(TerminalChange { terminal_id }),
            Reach::Preview(session_ids) => {
                for session_id in session_ids {
                    self.bump_preview(&session_id, cx);
                }
            }
        }
        cx.emit(change);
    }

    /// Apply `envelope` to the replicas: what it reached.
    fn apply_domain_event(&mut self, envelope: EventEnvelope, cx: &mut Context<Self>) -> Reach {
        if !self.host.subscription_reply_is_current(&envelope) {
            return Reach::Store;
        }
        if matches!(
            TopicKind::from(&envelope.topic),
            TopicKind::Index | TopicKind::Settings
        ) {
            self.thread_list_revision += 1;
        }
        if envelope.topic == Topic::Scope {
            if let ServerEvent::ScopeSnapshot(scope) = &envelope.event
                && envelope.request_id.is_some()
                && envelope.request_id != self.applied_scope_request
                && !matches!(
                    self.host.connection_state(),
                    ConnectionState::Reconnecting { .. } | ConnectionState::Offline { .. }
                )
            {
                self.baseline_topics.clear();
                self.apply_scope(scope, cx);
                self.applied_scope_request = envelope.request_id;
            }
            return Reach::Store;
        }
        if matches!(envelope.topic, Topic::Index | Topic::SpaceIndex { .. })
            && envelope.topic != self.index_topic()
        {
            return Reach::Store;
        }
        if !self.scope.is_full()
            && matches!(
                envelope.topic,
                Topic::Settings
                    | Topic::Providers
                    | Topic::RuntimeEvents
                    | Topic::Preview { .. }
                    | Topic::PreviewSession { .. }
                    | Topic::ExternalImport { .. }
                    | Topic::Hosting
                    | Topic::StorageMigration
                    | Topic::DiskUsage
            )
        {
            return Reach::Store;
        }
        if matches!(envelope.topic, Topic::DevServers { .. })
            && !self.space_allows(|policy| policy.preview)
        {
            return Reach::Store;
        }
        let index_topic = self.index_topic();
        let index = Topic::Index;
        let topic = if envelope.topic == index_topic {
            &index
        } else {
            &envelope.topic
        };
        match (topic, &envelope.event) {
            (Topic::SessionStatus { session_id }, ServerEvent::SessionStatusReplaced(status))
                if status.session_id == *session_id =>
            {
                for message in &status.queued_messages {
                    if let Some(key) = &message.delivery_key {
                        self.retire_delivery_for(session_id, key);
                    }
                }
            }
            (Topic::SessionEvents { session_id }, ServerEvent::SessionEvent(record)) => {
                self.retire_record_delivery(session_id, record);
            }
            (Topic::SessionEvents { session_id }, ServerEvent::SessionSnapshot { records, .. }) => {
                for record in records {
                    self.retire_record_delivery(session_id, record);
                }
            }
            _ => {}
        }
        let index_removal = match &envelope.event {
            ServerEvent::IndexRemoveSession { session_id } if envelope.topic == index_topic => {
                Some(session_id.clone())
            }
            _ => None,
        };
        let EventEnvelope {
            request_id,
            topic: envelope_topic,
            event,
        } = envelope;
        let topic = if envelope_topic == index_topic {
            &index
        } else {
            &envelope_topic
        };
        let mut reach = Reach::Store;
        // What the replicas keep is moved into them rather than copied.
        let event = match (topic, event) {
            (
                Topic::SessionEvents { session_id },
                ServerEvent::SessionSnapshot {
                    from,
                    end,
                    records,
                    total,
                    total_turns,
                    ..
                },
            ) => {
                let window = SnapshotWindow {
                    from,
                    end,
                    total,
                    total_turns: total_turns as usize,
                };
                if !self.apply_session_snapshot(
                    session_id,
                    &envelope_topic,
                    request_id,
                    window,
                    records,
                ) {
                    return Reach::Store;
                }
                None
            }
            (Topic::SessionEvents { session_id }, ServerEvent::SessionEvent(record)) => {
                // A turn's start and end change what reaches past the thread
                // (its conversation state, its recovery card); a record in
                // between changes the thread's log alone.
                let within_turn = !matches!(
                    record.event,
                    agent::AgentEvent::TurnStarted { .. }
                        | agent::AgentEvent::TurnCompleted { .. }
                        | agent::AgentEvent::RewindCompleted { .. }
                        | agent::AgentEvent::PlanUpdated { .. }
                );
                let Some(items) = self.apply_live_record(session_id, &envelope_topic, record)
                else {
                    return Reach::Store;
                };
                if within_turn {
                    reach = Reach::Thread(
                        session_id.clone(),
                        if items {
                            &[Part::Timeline, Part::Entries]
                        } else {
                            &[Part::Timeline]
                        },
                    );
                }
                None
            }
            (
                Topic::Terminal { terminal_id },
                ServerEvent::TerminalFrame {
                    terminal_id: frame_id,
                    frame,
                },
            ) if *terminal_id == frame_id => {
                self.apply_terminal_frame(frame_id, *frame, cx);
                reach = Reach::Terminal(frame_id);
                None
            }
            (
                Topic::CommandOutput {
                    session_id,
                    item_id,
                    cols,
                },
                ServerEvent::CommandOutputReplaced(frame),
            ) => {
                self.apply_command_output(session_id, item_id, *cols, *frame);
                reach = Reach::Thread(session_id.clone(), &[Part::Outputs]);
                None
            }
            (_, event) => Some(event),
        };
        if let Some(event) = &event
            && let Some(changed) = self.apply_preview_event(topic, event, cx)
        {
            reach = Reach::Preview(changed);
        }
        if let Some(mut event) = event {
            match (topic, &mut event) {
                (Topic::Preview { .. }, ServerEvent::PreviewRequest { .. }) => {
                    let _ = self.remote_preview.0.try_send(EventEnvelope {
                        request_id,
                        topic: envelope_topic.clone(),
                        event,
                    });
                }
                (
                    Topic::Terminal { terminal_id },
                    ServerEvent::TerminalDelta {
                        terminal_id: delta_id,
                        delta,
                    },
                ) if terminal_id == delta_id => {
                    self.apply_terminal_delta(*terminal_id, delta, cx);
                    reach = Reach::Terminal(*terminal_id);
                }
                (
                    Topic::CommandOutput {
                        session_id,
                        item_id,
                        cols,
                    },
                    ServerEvent::CommandOutputDelta(delta),
                ) => {
                    self.apply_command_output_delta(session_id, item_id, *cols, delta);
                    reach = Reach::Thread(session_id.clone(), &[Part::Outputs]);
                }
                (Topic::Index, ServerEvent::IndexUpsertSession(meta)) => {
                    if let Some(archived) = &mut self.archived_replica {
                        archived.sessions.retain(|archived| archived.id != meta.id);
                    }
                    match self
                        .index_replica
                        .0
                        .iter_mut()
                        .find(|existing| existing.id == meta.id)
                    {
                        Some(existing) => *existing = (**meta).clone(),
                        None => self.index_replica.0.push((**meta).clone()),
                    }
                    self.index_replica
                        .0
                        .sort_by_key(|meta| std::cmp::Reverse(meta.updated_at));
                    self.reindex_sessions();
                    self.close_unshown_pull_requests();
                }
                (Topic::Index, ServerEvent::IndexUpsertProject(project)) => {
                    images::invalidate_project_icon(
                        self.image_host,
                        project,
                        images::IconReset::Changed,
                        cx,
                    );
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
                    if let Some(position) = self
                        .index_replica
                        .0
                        .iter()
                        .position(|meta| meta.id == *session_id)
                    {
                        self.removed_session = Some(self.index_replica.0.remove(position));
                        self.reindex_sessions();
                    }
                    self.native_rewind_prefills.remove(session_id);
                    self.conversation_ui
                        .remove(&ConversationDestination::Thread(session_id.clone()));
                    self.open_pull_requests.remove(session_id);
                }
                (Topic::Index, ServerEvent::IndexRemoveProject { project_id }) => {
                    self.index_replica
                        .1
                        .retain(|project| project.id != *project_id);
                    self.import_statuses.remove(project_id);
                    self.conversation_ui
                        .remove(&ConversationDestination::ProjectDraft(project_id.clone()));
                }
                (
                    Topic::ExternalImport { project_id },
                    ServerEvent::ExternalImportStatusReplaced {
                        project_id: replaced,
                        status,
                    },
                ) if project_id == replaced => {
                    self.import_statuses
                        .insert(project_id.clone(), status.clone());
                }
                (Topic::Index, ServerEvent::IndexSnapshot(snapshot)) => {
                    self.index_hydrated = true;
                    let fresh_baseline = self.baseline_topics.insert(index_topic.clone());
                    for project in &snapshot.projects {
                        let changed =
                            self.index_replica.1.iter().any(|old| {
                                old.id == project.id && old.icon_path != project.icon_path
                            });
                        if changed {
                            images::invalidate_project_icon(
                                self.image_host,
                                project,
                                images::IconReset::Changed,
                                cx,
                            );
                        } else if fresh_baseline {
                            images::invalidate_project_icon(
                                self.image_host,
                                project,
                                images::IconReset::Baseline,
                                cx,
                            );
                        }
                    }
                    self.index_replica = (
                        std::mem::take(&mut snapshot.sessions),
                        std::mem::take(&mut snapshot.projects),
                    );
                    self.reindex_sessions();
                    self.close_unshown_pull_requests();
                    // Client state for a conversation the index no longer lists has
                    // nothing left to return to: a deleted project takes its draft's
                    // state, a deleted or archived thread its own.
                    let (sessions, projects) = &self.index_replica;
                    self.conversation_ui
                        .retain(|destination, _| match destination {
                            ConversationDestination::ProjectDraft(project_id) => {
                                projects.iter().any(|project| project.id == *project_id)
                            }
                            ConversationDestination::Thread(session_id) => {
                                sessions.iter().any(|meta| meta.id == *session_id)
                            }
                        });
                    self.threads.retain(|session_id, thread| {
                        thread.open || sessions.iter().any(|meta| meta.id == *session_id)
                    });
                    self.restore_index_selection(cx);
                    self.apply_index_summary(&snapshot.summary, cx);
                    if self.archived_holds > 0 {
                        self.load_archived_sessions(cx);
                    }
                }
                (
                    Topic::Index,
                    ServerEvent::IndexUnchanged {
                        fingerprint,
                        archived_revision,
                    },
                ) => {
                    if self.cached_index_fingerprint().as_ref() == Some(fingerprint) {
                        self.index_summary.archived_revision = *archived_revision;
                        if self.baseline_topics.insert(index_topic.clone()) {
                            for project in &self.index_replica.1 {
                                images::invalidate_project_icon(
                                    self.image_host,
                                    project,
                                    images::IconReset::Baseline,
                                    cx,
                                );
                            }
                        }
                        self.restore_index_selection(cx);
                        if self.archived_holds > 0 {
                            self.load_archived_sessions(cx);
                        }
                    } else {
                        let _ = self.host.subscribe(Subscription {
                            topic: index_topic.clone(),
                            after: None,
                            known_index: None,
                        });
                    }
                }
                (Topic::Index, ServerEvent::IndexSummaryReplaced(summary)) => {
                    self.apply_index_summary(summary, cx);
                }
                (Topic::Index, ServerEvent::IndexActivitiesChanged { upserts, removed }) => {
                    if self.baseline_topics.contains(&index_topic) {
                        for id in removed {
                            self.index_summary.activity.remove(id);
                        }
                        self.index_summary.activity.extend(
                            upserts
                                .iter()
                                .map(|(id, activity)| (id.clone(), activity.clone())),
                        );
                    }
                }
                (Topic::Settings, ServerEvent::LastVisitedChanged(visits)) => {
                    self.settings_replica
                        .last_visited
                        .extend(visits.iter().map(|(id, at)| (id.clone(), *at)));
                }
                (Topic::Settings, ServerEvent::SettingsReplaced(settings)) => {
                    self.settings_replica = std::mem::take(settings);
                    self.settings_hydrated = true;
                }
                (Topic::Settings, ServerEvent::SettingsSnapshot(settings)) => {
                    self.settings_replica = std::mem::take(settings);
                    self.settings_hydrated = true;
                    self.baseline_topics.insert(Topic::Settings);
                }
                (Topic::Providers, ServerEvent::ProvidersReplaced(status)) => {
                    self.providers_replica = std::mem::take(status);
                    self.baseline_topics.insert(Topic::Providers);
                }
                (Topic::Hosting, ServerEvent::HostingReplaced(state)) => cx.emit((**state).clone()),
                (Topic::StorageMigration, ServerEvent::StorageMigrationReplaced(migration))
                    if self.storage_migration_holds > 0 =>
                {
                    self.storage_migration.clone_from(migration);
                }
                (Topic::DiskUsage, ServerEvent::DiskUsageReplaced(usage))
                    if self.disk_usage_holds > 0 =>
                {
                    self.disk_usage.clone_from(usage);
                }
                (
                    Topic::Providers,
                    ServerEvent::ProviderUsageChanged {
                        provider_usage,
                        usage_checking,
                    },
                ) if self.baseline_topics.contains(&envelope_topic) => {
                    self.providers_replica
                        .provider_usage
                        .clone_from(provider_usage);
                    self.providers_replica
                        .usage_checking
                        .clone_from(usage_checking);
                }
                (Topic::GitStatus { session_id }, ServerEvent::GitStatusReplaced(status)) => {
                    if let Some(thread) = self.threads.get_mut(session_id) {
                        thread.git = Some(std::mem::take(status));
                        reach = Reach::Thread(session_id.clone(), &[Part::Status]);
                    }
                }
                (
                    Topic::SessionStatus { session_id },
                    ServerEvent::SessionStatusReplaced(status),
                ) if status.session_id == *session_id => {
                    let loaded = self.baseline_topics.insert(envelope_topic.clone());
                    if let Some(thread) = self.threads.get_mut(session_id) {
                        let status = status.as_ref().clone();
                        let drawer = (status.terminal_open, status.terminal_height);
                        thread.status = Some(status);
                        if thread.open {
                            thread.resettle();
                            let destination = thread.destination.clone();
                            let conversations = self.conversation_ui.len();
                            self.sync_terminal_topics(cx);
                            self.sync_conversation_ui(session_id);
                            // A status that moves the thread's conversation
                            // state, or first brings it, reaches past the
                            // thread.
                            if !loaded
                                && self.conversation_ui.len() == conversations
                                && self.threads[session_id].destination == destination
                            {
                                reach = Reach::Thread(
                                    session_id.clone(),
                                    &[Part::Status, Part::Timeline, Part::Entries],
                                );
                            }
                            // The host owns the drawer, which lives only with its
                            // running terminals: a conversation that loads shows it
                            // as the host has it, not as this client last left it.
                            if loaded
                                && let Some(ui) = self.thread_conversation_ui_mut(Some(session_id))
                            {
                                (ui.terminal_open, ui.terminal_height) = drawer;
                            }
                        }
                    }
                }
                (
                    Topic::SessionStatus { session_id },
                    ServerEvent::SessionUsageChanged {
                        usage,
                        context_window,
                    },
                ) if self.baseline_topics.contains(&envelope_topic) => {
                    if let Some(status) = self
                        .threads
                        .get_mut(session_id)
                        .and_then(|thread| thread.status.as_mut())
                    {
                        status.usage = *usage;
                        status.context_window = *context_window;
                        reach = Reach::Thread(session_id.clone(), &[Part::Status]);
                    }
                }
                (Topic::SessionPlan { session_id }, ServerEvent::SessionPlanReplaced(plan))
                    if plan.session_id == *session_id =>
                {
                    let loaded = self.baseline_topics.insert(envelope_topic.clone());
                    if let Some(thread) = self.threads.get_mut(session_id) {
                        thread.plan = Some(plan.clone());
                        if !loaded {
                            reach = Reach::Thread(session_id.clone(), &[Part::Status]);
                        }
                    }
                }
                (Topic::SessionEvents { session_id }, ServerEvent::SessionHistoryError(error)) => {
                    if let Some(thread) = self.open_thread_mut(session_id) {
                        thread.history_error = Some(history::history_error_message(error.clone()));
                        thread.catching_up = false;
                    }
                }
                (
                    Topic::SessionStatus { session_id },
                    ServerEvent::RewindPrefill {
                        session_id: event_session,
                        client_id,
                        text,
                        attachments,
                    },
                ) if session_id == event_session && *client_id == self.client_id() => {
                    self.native_rewind_prefills.insert(
                        session_id.clone(),
                        RewindPrefill {
                            text: text.clone(),
                            attachments: attachments.clone(),
                        },
                    );
                }
                _ => {}
            }
        }
        self.load_pending_chat_history(cx);
        if envelope_topic == index_topic {
            self.host
                .remember_known_index(&index_topic, self.cached_index_fingerprint());
            self.reconcile_destination(cx);
            self.removed_session = None;
            // A thread kept on screen (its failed send still offers Retry)
            // keeps its replicas until the user leaves it.
            if let Some(session_id) = index_removal
                && !self
                    .threads
                    .get(&session_id)
                    .is_some_and(|thread| thread.open)
            {
                self.threads.remove(&session_id);
            }
        }
        self.acknowledge_read(cx);
        reach
    }

    /// A window of the thread's log, answering its subscription or a page:
    /// whether anything after it in the envelope still applies.
    fn apply_session_snapshot(
        &mut self,
        session_id: &str,
        topic: &Topic,
        request_id: Option<u64>,
        window: SnapshotWindow,
        records: Vec<StoredEvent>,
    ) -> bool {
        let SnapshotWindow {
            from,
            end,
            total,
            total_turns,
        } = window;
        if !self
            .threads
            .get(session_id)
            .is_some_and(|thread| thread.open)
        {
            return false;
        }
        let after = match request_id {
            Some(_) => match self.host.consume_session_snapshot(topic, request_id) {
                Some(after) => after,
                None => return false,
            },
            None => None,
        };
        let continuation = after.filter(|after| *after <= total);
        let thread = self.threads.get_mut(session_id).expect("open thread");
        let retain_timeline = continuation == Some(from)
            && records.is_empty()
            && thread.total_turns.is_some()
            && from != 0
            && thread.history.as_ref().is_some_and(|held| held.end == from)
            && thread.timeline.is_some();
        thread.total_turns = None;
        let held = &mut thread.history;
        match held {
            // Continues the held cursor.
            Some(held) if continuation == Some(from) && held.end == from => {
                held.extend(records, end);
            }
            _ => *held = Some(history::HeldHistory::new(from, end, records)),
        }
        // The window reaches the end of the log: the replica is at the
        // host's position, and live records continue it.
        thread.catching_up = false;
        self.host.remember_after(topic, end);
        thread.total_turns = Some(total_turns);
        self.baseline_topics.insert(topic.clone());
        if retain_timeline {
            let timeline = thread.timeline.as_ref().expect("retained timeline");
            thread.turn_offset = total_turns.saturating_sub(timeline.turns.len());
            return false;
        }
        thread.refold(total_turns);
        true
    }

    /// One record streamed live into an open thread: whether it applied.
    /// Apply a record streamed into an open thread's log: `None` when it was
    /// not, else whether it changed the fold's items ([`Part::Entries`]).
    fn apply_live_record(
        &mut self,
        session_id: &str,
        topic: &Topic,
        record: StoredEvent,
    ) -> Option<bool> {
        let baseline = self.baseline_topics.contains(topic);
        let thread = self.open_thread_mut(session_id)?;
        // Until its window arrives the thread may hold an earlier
        // visit's records, which the window continues: a record sent
        // before the window is part of it.
        if thread.catching_up || !baseline || thread.timeline.is_none() {
            return None;
        }
        let held = thread.history.as_ref()?;
        let after = held.end + 1;
        self.host.remember_after(topic, after);
        self.apply_conversation_event(session_id, &record.event);
        let thread = self.threads.get_mut(session_id).expect("open thread");
        // Text streamed into an item it continues changes no item; a delta
        // that brings its item first does.
        let mut items = !matches!(record.event, agent::AgentEvent::Delta { .. });
        if let Some(mut timeline) = thread.timeline.take() {
            let entries = timeline.entries.len();
            timeline.apply_stored(&record);
            items |= timeline.entries.len() != entries;
            thread.settle_running_turn(&mut timeline);
            thread.total_turns = Some(thread.turn_offset + timeline.turns.len());
            thread.timeline = Some(timeline);
        }
        thread
            .history
            .as_mut()
            .expect("held above")
            .extend(vec![record], after);
        Some(items)
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

    /// Whether a thread is gone from the visible index. A draft has no index
    /// entry of its own and stays until the user navigates away, unless its
    /// project leaves the index.
    fn left_index(&self, status: &SessionStatus) -> bool {
        if status.draft {
            return status.project_id.as_ref().is_some_and(|project_id| {
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
    /// gone falls back to the first listed one; with no projects at all the
    /// chat view keeps its add-project state.
    pub(crate) fn open_last_project_draft(&mut self, cx: &mut Context<Self>) {
        if self.draft_fallback_pending {
            return;
        }
        let remembered = self.settings_replica.last_project_id.as_deref();
        let Some(project) = self
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
            .cloned()
        else {
            return;
        };
        self.fall_back_to_draft(project.id, project.root, cx);
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

    pub fn all_provider_profiles(&self) -> Vec<ResolvedProfile> {
        if !self.scope.is_full() {
            return self
                .scoped_providers
                .iter()
                .map(|choice| ResolvedProfile {
                    id: choice.profile_id.clone().unwrap_or_else(|| {
                        tcode_core::settings::provider_key(choice.provider).to_owned()
                    }),
                    kind: choice.provider,
                    settings: ProviderSettings {
                        enabled: true,
                        ..Default::default()
                    },
                    shared_env: Vec::new(),
                })
                .collect();
        }
        agent::ProviderKind::NATIVE
            .into_iter()
            .flat_map(|kind| self.settings_replica.profiles_for_kind(kind))
            .collect()
    }

    pub fn enabled_profiles(&self) -> Vec<ResolvedProfile> {
        self.all_provider_profiles()
            .into_iter()
            .filter(|profile| profile.settings.enabled)
            .collect()
    }

    fn scoped_choice(&self, profile_id: &str) -> Option<&ScopedProviderChoice> {
        self.scoped_providers.iter().find(|choice| {
            choice
                .profile_id
                .as_deref()
                .unwrap_or(tcode_core::settings::provider_key(choice.provider))
                == profile_id
        })
    }

    pub fn profile_catalog(&self, profile_id: &str) -> Vec<agent::ModelSpec> {
        if !self.scope.is_full() {
            return self
                .scoped_choice(profile_id)
                .map(|choice| choice.models.clone())
                .unwrap_or_default();
        }
        self.settings_replica
            .resolved_profile(profile_id)
            .filter(|profile| {
                Settings::is_builtin_profile_id(profile_id) || profile.supports_account_usage()
            })
            .map(|profile| self.provider_model_catalog(profile.kind))
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn drain_host_events_for_test(&mut self, cx: &mut Context<Self>) {
        let events = self.host.events();
        while let Ok(envelope) = events.try_recv() {
            self.receive(envelope, cx);
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

    /// Open the diff of `session_id`'s `turn`, at `path` if one is given.
    pub fn open_diff(
        &mut self,
        session_id: &str,
        turn: usize,
        path: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(ui) = self.thread_conversation_ui_mut(Some(session_id)) {
            ui.right_panel_open = true;
            ui.right_tab = RightTab::Diff;
            ui.diff_selected_turn = Some(turn);
            ui.pending_diff_focus = Some(DiffFocus {
                session: session_id.to_owned(),
                turn,
                path,
            });
            ui.refresh_diff();
            cx.notify();
        }
    }

    pub fn select_diff_turn_for(
        &mut self,
        session_id: Option<&str>,
        turn: usize,
        cx: &mut Context<Self>,
    ) {
        if let Some(ui) = self.thread_conversation_ui_mut(session_id) {
            ui.pending_diff_focus = None;
            ui.diff_selected_turn = Some(turn);
            ui.refresh_diff();
            cx.notify();
        }
    }

    pub fn discard_diff_focus_for(&mut self, session_id: Option<&str>, cx: &mut Context<Self>) {
        if let Some(ui) = self.thread_conversation_ui_mut(session_id) {
            ui.discard_diff_focus();
            cx.notify();
        }
    }

    /// Close `session_id`'s right panel, whichever tab it shows.
    pub fn close_right_panel_for(&mut self, session_id: Option<&str>, cx: &mut Context<Self>) {
        if let Some(ui) = self.thread_conversation_ui_mut(session_id) {
            ui.pending_diff_focus = None;
            ui.right_panel_open = false;
        }
        cx.notify();
    }

    pub fn toggle_diff_expanded_for(&mut self, session_id: Option<&str>, cx: &mut Context<Self>) {
        if let Some(ui) = self.thread_conversation_ui_mut(session_id) {
            ui.right_panel_expanded = !ui.right_panel_expanded;
            cx.notify();
        }
    }

    pub fn set_right_tab_for(
        &mut self,
        session_id: Option<&str>,
        tab: RightTab,
        cx: &mut Context<Self>,
    ) {
        if tab == RightTab::Preview && !self.offers_preview() {
            return;
        }
        if let Some(ui) = self.thread_conversation_ui_mut(session_id) {
            if tab == RightTab::Diff && ui.right_tab != RightTab::Diff {
                ui.refresh_diff();
            }
            ui.right_tab = tab;
            cx.notify();
        }
    }

    /// Show `tab` of `session_id`'s right panel, or close the panel while it
    /// shows that tab.
    pub fn toggle_tab_panel_for(
        &mut self,
        session_id: &str,
        tab: RightTab,
        cx: &mut Context<Self>,
    ) {
        if tab == RightTab::Preview && !self.offers_preview() {
            return;
        }
        let showing = self
            .thread_conversation_ui(Some(session_id))
            .is_some_and(|ui| ui.right_panel_open && ui.right_tab == tab);
        if showing {
            self.close_right_panel_for(Some(session_id), cx);
        } else {
            self.open_tab_for(session_id, tab, cx);
        }
    }
    pub fn close_preview_panel_for(&mut self, session_id: Option<&str>, cx: &mut Context<Self>) {
        if let Some(ui) = self
            .thread_conversation_ui_mut(session_id)
            .filter(|ui| ui.right_panel_open && ui.right_tab == RightTab::Preview)
        {
            ui.right_panel_open = false;
            cx.notify();
        }
    }

    pub fn open_preview_panel_for(&mut self, session_id: Option<&str>, cx: &mut Context<Self>) {
        if !self.offers_preview() {
            return;
        }
        if let Some(ui) = self.thread_conversation_ui_mut(session_id)
            && !(ui.right_panel_open && ui.right_tab == RightTab::Preview)
        {
            ui.right_panel_open = true;
            ui.right_tab = RightTab::Preview;
            cx.notify();
        }
    }

    pub fn open_tab_for(&mut self, session_id: &str, tab: RightTab, cx: &mut Context<Self>) {
        if tab == RightTab::Preview && !self.offers_preview() {
            return;
        }
        let destination = self
            .threads
            .get(session_id)
            .filter(|thread| thread.open)
            .and_then(|thread| thread.destination.clone())
            .unwrap_or_else(|| ConversationDestination::Thread(session_id.to_string()));
        let ui = self.conversation_ui.entry(destination).or_insert_with(|| {
            ConversationUiState::new(self.settings_replica.word_wrap_diffs, false, 240.)
        });
        ui.right_panel_open = true;
        ui.right_tab = tab;
        if tab == RightTab::Diff {
            ui.refresh_diff();
        }
        cx.notify();
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
                match result {
                    Ok(QueryResponse::ArchivedSessions(archived)) => {
                        if archived.revision < store.index_summary.archived_revision {
                            store.load_archived_sessions(cx);
                        } else {
                            store.archived_replica = Some(archived);
                        }
                    }
                    Ok(other) => log::warn!("unexpected archived-sessions response: {other:?}"),
                    Err(error) => log::warn!("archived sessions failed: {}", error.message),
                }
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
            self.archived_task = None;
        }
    }

    pub fn archived_loading(&self) -> bool {
        self.archived_replica.is_none()
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

    /// The `0xRRGGBB` color of `meta`'s sidebar provider mark, `None` while
    /// the Provider marks setting is off.
    pub fn provider_color(&self, meta: &SessionMeta) -> Option<u32> {
        self.settings_replica.sidebar_provider_marks.then(|| {
            self.settings_replica
                .provider_color(&meta.provider_color_key())
        })
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
        tcode_core::thread_sort::sort_threads(&mut visible);
        visible
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

    pub fn fork_availability(&self, session_id: &str) -> ForkAvailability {
        self.index_summary
            .activity
            .get(session_id)
            .map_or(ForkAvailability::Available, |activity| activity.fork)
    }

    pub fn sidebar_sessions(&self) -> &[SessionMeta] {
        &self.index_replica.0
    }

    pub fn settings_installed_acp_agents(&self) -> Vec<tcode_core::acp::InstalledAcpAgent> {
        self.settings_replica
            .installed_acp_agents()
            .into_iter()
            .cloned()
            .collect()
    }

    pub fn providers_checked_at(&self) -> Option<u64> {
        self.providers_replica.providers_checked_at
    }

    pub fn providers_checking(&self) -> bool {
        self.providers_replica.providers_checking
    }

    /// The latest usage fetch for a provider profile, when one has landed.
    pub fn provider_usage(&self, profile_id: &str) -> Option<tcode_core::usage::ProviderUsage> {
        self.providers_replica
            .provider_usage
            .get(profile_id)
            .cloned()
    }

    /// A usage fetch is in flight for this profile.
    pub fn usage_checking(&self, profile_id: &str) -> bool {
        self.providers_replica.usage_checking.contains(profile_id)
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

    pub fn provider_profile_kind(&self, profile_id: &str) -> agent::ProviderKind {
        if let Some(choice) = self.scoped_choice(profile_id) {
            return choice.provider;
        }
        self.settings_replica
            .resolved_profile(profile_id)
            .map(|profile| profile.kind)
            .unwrap_or(agent::ProviderKind::ClaudeCode)
    }

    pub fn provider_profile_settings(&self, profile_id: &str) -> ProviderSettings {
        self.settings_replica
            .resolved_profile(profile_id)
            .map(|profile| profile.settings)
            .unwrap_or_default()
    }

    pub fn provider_model_catalog(&self, provider: agent::ProviderKind) -> Vec<agent::ModelSpec> {
        self.providers_replica
            .model_catalogs
            .get(&provider)
            .cloned()
            .unwrap_or_default()
    }

    pub(crate) fn models_loading(&self, provider: agent::ProviderKind) -> bool {
        self.providers_replica.models_loading.get(&provider) == Some(&true)
            && self
                .providers_replica
                .model_catalogs
                .get(&provider)
                .is_none_or(Vec::is_empty)
    }

    pub fn picker_models_for_profile(&self, profile_id: &str) -> Vec<ResolvedModel> {
        picker_models(
            &self.profile_catalog(profile_id),
            &self.provider_profile_settings(profile_id),
            &self.favorite_ids(profile_id),
        )
    }

    fn favorite_ids(&self, profile_id: &str) -> Vec<String> {
        self.settings_replica
            .favorite_ids(self.provider_profile_kind(profile_id), profile_id)
    }

    pub fn provider_profile_display_name(&self, profile_id: &str) -> String {
        if let Some(choice) = self.scoped_choice(profile_id) {
            return choice.name.clone();
        }
        let email = self
            .provider_profile_snapshot(profile_id)
            .as_ref()
            .and_then(crate::provider_status::signed_in_account)
            .and_then(|auth| auth.email.clone());
        self.settings_replica
            .profile_display_name_as(profile_id, email.as_deref())
    }

    /// What the host says each harness is: the native ones, then each
    /// installed ACP agent.
    pub fn harness_descriptions(&self) -> &[tcode_protocol::HarnessDescription] {
        &self.providers_replica.harnesses
    }

    /// A user profile's own settings, without what it takes from its
    /// harness; `None` for a built-in profile.
    pub fn provider_profile_own_settings(&self, profile_id: &str) -> Option<ProviderSettings> {
        self.settings_replica
            .profiles
            .get(profile_id)
            .map(|profile| profile.settings.clone())
    }

    /// The threads that run on `profile_id` and fall back to the default
    /// account once it is deleted.
    pub fn threads_using_profile(&self, profile_id: &str) -> usize {
        self.index_replica
            .0
            .iter()
            .filter(|meta| meta.profile_id.as_deref() == Some(profile_id))
            .count()
    }

    pub fn provider_profile_supports_account_login(&self, profile_id: &str) -> bool {
        self.settings_replica
            .resolved_profile(profile_id)
            .is_some_and(|profile| profile.supports_account_login())
    }

    pub fn provider_profile_snapshot(&self, profile_id: &str) -> Option<ProviderSnapshot> {
        self.providers_replica
            .provider_snapshots
            .get(profile_id)
            .cloned()
    }

    pub fn provider_version_status(
        &self,
        provider: agent::ProviderKind,
    ) -> Option<ProviderVersionStatus> {
        self.providers_replica
            .provider_versions
            .get(&provider)
            .cloned()
    }

    pub fn provider_update_run(&self) -> Option<tcode_protocol::ProviderUpdateRun> {
        self.providers_replica.update_run.clone()
    }

    pub fn automatic_provider_updates(&self) -> Vec<agent::ProviderKind> {
        agent::ProviderKind::NATIVE
            .into_iter()
            .filter(|provider| {
                self.providers_replica
                    .provider_versions
                    .get(provider)
                    .is_some_and(|status| {
                        status.update_available
                            && status.update_command.is_some()
                            && !status.update_requires_terminal
                    })
            })
            .collect()
    }

    pub fn tcode_update_status(&self) -> tcode_protocol::TcodeUpdateStatus {
        self.providers_replica.tcode_update.clone()
    }

    /// The `0xRRGGBB` color a provider profile is drawn in; see
    /// [`Settings::provider_color`].
    pub fn provider_profile_color(&self, profile_id: &str) -> u32 {
        self.settings_replica.provider_color(profile_id)
    }

    pub fn provider_profile_stored_secret_names(&self, profile_id: &str) -> HashSet<String> {
        self.providers_replica
            .secret_names
            .get(profile_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn provider_dialog_models(
        &self,
        profile_id: &str,
        custom_models: &[String],
        hidden_models: &[String],
    ) -> Vec<ResolvedModel> {
        let mut settings = self.provider_profile_settings(profile_id);
        settings.custom_models = custom_models.to_vec();
        settings.hidden_models = hidden_models.to_vec();
        resolve_models(
            &self.profile_catalog(profile_id),
            &settings,
            &self.favorite_ids(profile_id),
        )
    }

    pub fn installed_acp_agent(
        &self,
        agent_id: &str,
    ) -> Option<tcode_core::acp::InstalledAcpAgent> {
        self.settings_replica.acp_agent(agent_id).cloned()
    }

    pub fn acp_marketplace_items(&self) -> Vec<AcpMarketplaceItem> {
        self.providers_replica.acp_marketplace_items.clone()
    }

    /// A profile's plugin catalog listed from `cwd`, or from outside any
    /// project when `cwd` is `None`.
    pub fn provider_plugin_catalog(
        &self,
        profile_id: &str,
        cwd: Option<&std::path::Path>,
    ) -> Option<&tcode_protocol::ProviderPluginCatalog> {
        self.providers_replica.plugins.iter().find(|catalog| {
            catalog.profile_id == profile_id && catalog.context_cwd.as_deref() == cwd
        })
    }

    /// Every plugin catalog the host holds for this client.
    pub fn provider_plugin_catalogs(&self) -> &[tcode_protocol::ProviderPluginCatalog] {
        &self.providers_replica.plugins
    }

    pub fn session_cwd_for(&self, session_id: Option<&str>, cx: &App) -> Option<PathBuf> {
        self.thread_status(session_id, cx)
            .map(|status| status.cwd.clone())
    }

    pub fn acp_registry_loading(&self) -> bool {
        self.providers_replica.acp_registry_loading
    }

    pub fn acp_registry_error(&self) -> Option<String> {
        self.providers_replica.acp_registry_error.clone()
    }

    pub fn acp_installing(&self, agent_id: &str) -> bool {
        self.providers_replica.acp_installing.contains(agent_id)
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

    pub(crate) fn commit_dialog_state_for(
        &self,
        session_id: Option<&str>,
        cx: &App,
    ) -> CommitDialogState {
        let status = self
            .thread_git(session_id, cx)
            .and_then(|git| git.status.as_ref());
        CommitDialogState {
            files: status
                .map(|status| status.changed_files.clone())
                .unwrap_or_default(),
            branch: status.and_then(|status| status.branch.clone()),
            on_default_branch: status.is_some_and(|status| status.is_default_branch),
        }
    }

    pub(crate) fn diff_active_state_for(
        &self,
        session_id: Option<&str>,
        cx: &App,
    ) -> Option<DiffActiveState> {
        self.thread_status(session_id, cx)
            .map(|status| DiffActiveState {
                session: status.session_id.clone(),
                cwd: status.cwd.clone(),
                branches: status.branches.clone(),
            })
    }

    pub fn diff_turns_for(&self, session_id: Option<&str>, cx: &App) -> Vec<usize> {
        self.with_timeline_for(
            session_id,
            |timeline| {
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
            },
            cx,
        )
        .unwrap_or_default()
    }

    pub fn diff_selected_turn_for(&self, session_id: Option<&str>, cx: &App) -> Option<usize> {
        let turns = self.diff_turns_for(session_id, cx);
        let explicit = self
            .thread_conversation_ui(session_id)
            .and_then(|ui| ui.diff_selected_turn);
        match explicit {
            Some(turn) if turns.contains(&turn) => Some(turn),
            _ => turns.last().copied(),
        }
    }

    pub fn with_diff_turn_changes_for<R>(
        &self,
        session_id: Option<&str>,
        turn: usize,
        read: impl FnOnce(&[agent::FileChange], agent::ChangeCompleteness) -> R,
        cx: &App,
    ) -> Option<R> {
        self.with_timeline_for(
            session_id,
            |timeline| {
                let changes = timeline.turns.get(turn)?.changes.as_ref()?;
                Some(read(&changes.changes, changes.completeness))
            },
            cx,
        )
        .flatten()
    }

    pub(crate) fn pending_diff_focus_for(&self, session_id: Option<&str>) -> Option<DiffFocus> {
        self.thread_conversation_ui(session_id)
            .and_then(|ui| ui.pending_diff_focus.clone())
    }

    /// UI-only consuming selector. The underlying diff focus is replica state;
    /// this does not cross the host boundary.
    pub(crate) fn take_diff_focus_for(
        &mut self,
        session_id: Option<&str>,
        session: &str,
        turn: usize,
    ) -> Option<DiffFocus> {
        self.thread_conversation_ui_mut(session_id)?
            .take_diff_focus(session, turn)
    }

    pub fn diff_refresh_generation_for(&self, session_id: Option<&str>) -> u64 {
        self.thread_conversation_ui(session_id)
            .map(|ui| ui.diff_refresh_generation)
            .unwrap_or(0)
    }

    pub fn diff_word_wrap_for(&self, session_id: Option<&str>) -> bool {
        self.thread_conversation_ui(session_id)
            .map(|ui| ui.diff_wrap)
            .unwrap_or(self.settings_replica.word_wrap_diffs)
    }

    pub fn diff_split_for(&self, session_id: Option<&str>) -> bool {
        self.thread_conversation_ui(session_id)
            .is_some_and(|ui| ui.diff_split)
    }

    pub fn set_diff_split_for(
        &mut self,
        session_id: Option<&str>,
        split: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(ui) = self.thread_conversation_ui_mut(session_id) {
            ui.diff_split = split;
            cx.notify();
        }
    }

    pub fn toggle_diff_wrap_for(&mut self, session_id: Option<&str>, cx: &mut Context<Self>) {
        if let Some(ui) = self.thread_conversation_ui_mut(session_id) {
            ui.diff_wrap = !ui.diff_wrap;
            cx.notify();
        }
    }

    /// The panels of `session_id`'s conversation, or of no thread.
    pub(crate) fn panel_state_for(&self, session_id: Option<&str>) -> PanelState {
        let mut state = snapshots::panel_state(self.thread_conversation_ui(session_id));
        state.terminal_open &= self.space_allows(|policy| policy.terminals);
        state
    }

    pub fn review_comments_for(&self, session_id: Option<&str>, cx: &App) -> Vec<ReviewComment> {
        self.thread_status(session_id, cx)
            .map(|status| status.review_comment_drafts.clone())
            .unwrap_or_default()
    }

    pub fn load_git_diff(
        &self,
        cwd: &std::path::Path,
        scope: GitDiffScope,
        base: Option<&str>,
        ignore_whitespace: bool,
        cx: &mut App,
    ) -> Task<GitDiffResult> {
        let host = self.host.clone();
        let query = Query::LoadGitDiff {
            cwd: cwd.to_path_buf(),
            scope,
            base: base.map(str::to_string),
            ignore_whitespace,
        };
        cx.spawn(async move |_| match host.query(query).await {
            Ok(QueryResponse::GitDiff(diff)) => diff,
            Ok(other) => GitDiffResult {
                error: Some(format!("unexpected git-diff response: {other:?}")),
                ..GitDiffResult::default()
            },
            Err(error) => GitDiffResult {
                error: Some(error.message),
                ..GitDiffResult::default()
            },
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

    pub fn read_pull_request(
        &self,
        session_id: String,
        key: tcode_core::pull_request::PullRequestKey,
        read: tcode_protocol::PullRequestRead,
        cx: &mut App,
    ) -> Task<Result<(tcode_protocol::PullRequestReadResponse, u64), ProtocolError>> {
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
                    expires_at,
                } => Ok((*response, expires_at)),
                _ => Err(ProtocolError {
                    code: "invalid_pull_request_response".into(),
                    message: "Unexpected pull request response".into(),
                }),
            }
        })
    }

    pub(crate) fn message_byline_for(
        &self,
        session_id: Option<&str>,
        entry_id: &str,
        cx: &App,
    ) -> Option<String> {
        let held = self
            .watched(session_id, Part::Timeline, cx)?
            .history
            .as_ref()?;
        let attribution = &held.attribution;
        // The lead's messages to a child carry no author: they are no member's.
        if attribution.origins.get(entry_id) == Some(&tcode_core::session::MessageOrigin::Agent) {
            return Some(crate::tr!("chat.from_orchestrator").into_owned());
        }
        let author = attribution.authors.get(entry_id)?;
        let own_id = self.client_host.as_ref().map(|host| host.device_id());
        // The host attributes only space members' messages. An unattributed
        // one is the owner's, which is someone else only from inside a space.
        let other = author.as_ref().map_or(!self.scope.is_full(), |author| {
            own_id.as_deref() != Some(author.device_id.as_str())
        });
        (attribution.distinct_authors.len() > 1 || other).then(|| {
            author
                .as_ref()
                .map(|author| author.name.clone())
                .unwrap_or_else(|| crate::tr!("member.owner").into_owned())
        })
    }

    pub fn with_timeline_for<R>(
        &self,
        session_id: Option<&str>,
        read: impl FnOnce(&Timeline) -> R,
        cx: &App,
    ) -> Option<R> {
        self.thread_timeline(session_id, cx).map(read)
    }

    /// Reads `session_id`'s timeline as a view depending on its items alone
    /// ([`Part::Entries`]): text streamed into an item does not draw such a
    /// view again, so `read` must not look at a message's, a thought's or a
    /// command output's text.
    pub fn with_entries_for<R>(
        &self,
        session_id: Option<&str>,
        read: impl FnOnce(&Timeline) -> R,
        cx: &App,
    ) -> Option<R> {
        self.watched(session_id, Part::Entries, cx)?
            .timeline
            .as_ref()
            .map(read)
    }

    /// Moves on with every change to `session_id`'s timeline items, read as
    /// a view depending on them: see [`Self::with_entries_for`].
    pub(crate) fn entries_revision_for(&self, session_id: Option<&str>, cx: &App) -> Option<u64> {
        Some(self.thread(session_id)?.watch(Part::Entries, cx))
    }

    /// Moves on with every change to `session_id`'s timeline, read as a view
    /// depending on it: the store is not notified of a record streamed in.
    pub(crate) fn timeline_revision_for(&self, session_id: Option<&str>, cx: &App) -> Option<u64> {
        Some(self.thread(session_id)?.watch(Part::Timeline, cx))
    }

    /// Changes each time `session_id`'s timeline is folded whole from the
    /// records held for it (opening it, catching up, a history page, pages
    /// dropped above the tail) instead of extended by a live record.
    pub(crate) fn timeline_folds_for(&self, session_id: Option<&str>) -> u64 {
        self.thread(session_id).map_or(0, |thread| thread.folds)
    }

    pub(crate) fn pending_chat_turn(&self, session_id: &str) -> Option<usize> {
        let turn_offset = self.threads.get(session_id)?.turn_offset;
        self.pending_chat_turn
            .as_ref()
            .filter(|(id, _)| id == session_id)
            .and_then(|(_, turn)| turn.checked_sub(turn_offset))
    }

    pub(crate) fn take_pending_chat_turn(&mut self, session_id: &str, turn: usize) {
        let turn_offset = self
            .threads
            .get(session_id)
            .map_or(0, |thread| thread.turn_offset);
        if self.pending_chat_turn.as_ref() == Some(&(session_id.to_string(), turn + turn_offset)) {
            self.pending_chat_turn = None;
        }
    }

    #[cfg(test)]
    pub(crate) fn set_session_replica_for_test(
        &mut self,
        session_id: String,
        timeline: Timeline,
        cx: &mut Context<Self>,
    ) {
        if !self.is_thread_open(&session_id) {
            self.open_thread(&session_id, cx);
            self.put_back_thread(session_id.clone(), cx);
        }
        self.threads
            .get_mut(&session_id)
            .expect("the selected thread is held")
            .timeline = Some(timeline);
        // As every change the host's events make: what draws the thread
        // hears of it and folds its rows again.
        cx.notify();
    }

    pub fn with_composer_destination_for<R>(
        &self,
        session_id: Option<&str>,
        read: impl FnOnce(bool, &str, Option<&str>) -> R,
        cx: &App,
    ) -> Option<R> {
        self.thread_status(session_id, cx).map(|status| {
            read(
                status.draft,
                &status.session_id,
                status.project_id.as_deref(),
            )
        })
    }

    pub(crate) fn native_subagent_readonly_for(&self, session_id: Option<&str>, cx: &App) -> bool {
        self.thread_status(session_id, cx)
            .is_some_and(|status| status.conversation_read_only)
    }

    /// The composer of `session_id`'s thread, or of no thread.
    pub fn composer_state_for(&self, session_id: Option<&str>, cx: &App) -> ComposerState {
        let mut state = snapshots::composer_state(
            self.thread_status(session_id, cx),
            &self.settings_replica,
            &self.providers_replica,
        );
        state.space_policy = self.space_policy.clone();
        state
    }

    /// Puts `text` into the thread's composer after whatever is typed there; nothing is sent.
    pub fn append_to_composer(&mut self, session_id: String, text: String, cx: &mut Context<Self>) {
        let pending = self.composer_appends.entry(session_id).or_default();
        if !pending.is_empty() {
            pending.push_str("\n\n");
        }
        pending.push_str(&text);
        cx.emit(StoreChange {
            topic: TopicKind::SessionStatus,
        });
        cx.notify();
    }

    /// Whether text from [`Self::append_to_composer`] or a native rewind's
    /// prefill waits for the thread's composer.
    pub fn composer_text_waiting_for(&self, session_id: Option<&str>, cx: &App) -> bool {
        self.thread_status(session_id, cx).is_some_and(|status| {
            self.composer_appends.contains_key(&status.session_id)
                || self.native_rewind_prefills.contains_key(&status.session_id)
        })
    }

    /// Consume the thread's text from [`Self::append_to_composer`].
    pub fn take_composer_append_for(
        &mut self,
        session_id: Option<&str>,
        cx: &App,
    ) -> Option<String> {
        let session_id = self.thread_status(session_id, cx)?.session_id.clone();
        self.composer_appends.remove(&session_id)
    }

    /// Consume the prompt a rewind this client asked for gave back.
    pub fn take_native_rewind_prefill_for(
        &mut self,
        session_id: Option<&str>,
        cx: &App,
    ) -> Option<RewindPrefill> {
        let session_id = self.thread_status(session_id, cx)?.session_id.clone();
        self.native_rewind_prefills.remove(&session_id)
    }

    /// Build renderer handles from replicated layout. Local affordances keep
    /// direct PTY/grid access; otherwise the handles wrap client emulators.
    pub fn with_terminal_workspace_for<R>(
        &self,
        session_id: Option<&str>,
        read: impl FnOnce(&TerminalWorkspace) -> R,
        cx: &App,
    ) -> Option<R> {
        let status = self.thread_status(session_id, cx)?;
        let workspace = TerminalWorkspace::from_replica(status, |id| self.client_terminal(id));
        Some(read(&workspace))
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
        session_id: Option<&str>,
        cx: &mut App,
    ) -> Task<Vec<PathEntry>> {
        let session_id = session_id.unwrap_or_default().to_owned();
        let host = self.host.clone();
        cx.spawn(
            async move |_| match host.query(Query::ListActiveWorkspace { session_id }).await {
                Ok(QueryResponse::ActiveWorkspace(entries)) => entries,
                Ok(other) => {
                    log::error!("unexpected active-workspace response: {other:?}");
                    Vec::new()
                }
                Err(error) => {
                    log::error!("active-workspace query failed: {}", error.message);
                    Vec::new()
                }
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

    /// Whether `session_id`'s timeline is held whole: its log and status
    /// are here and it is not catching up on records it missed, so what
    /// draws it now draws what it will go on drawing.
    pub(crate) fn timeline_settled_for(&self, session_id: &str) -> bool {
        self.threads.get(session_id).is_some_and(|thread| {
            thread.timeline.is_some() && thread.status.is_some() && !thread.catching_up
        })
    }

    pub(crate) fn session_loading_for(&self, session_id: &str) -> bool {
        self.threads
            .get(session_id)
            .is_none_or(|thread| thread.timeline.is_none() || thread.status.is_none())
    }

    /// The title, working directory and draft flag a conversation showing
    /// `session_id`, or no thread, draws in its header.
    pub fn chat_active_session_for(
        &self,
        session_id: Option<&str>,
        cx: &App,
    ) -> Option<(String, PathBuf, bool)> {
        self.thread_status(session_id, cx)
            .map(|status| (status.title.clone(), status.cwd.clone(), status.draft))
            .or_else(|| {
                let id = session_id?;
                self.index_replica
                    .0
                    .iter()
                    .find(|meta| meta.id == id)
                    .map(|meta| (meta.title.clone(), meta.cwd.clone(), false))
            })
            .or_else(|| {
                (!self.delivery_messages_for(session_id, cx).is_empty()).then(|| {
                    (
                        crate::tr!("chat.waiting_connection").into_owned(),
                        PathBuf::new(),
                        false,
                    )
                })
            })
    }

    pub fn chat_requested_model_for(&self, session_id: Option<&str>, cx: &App) -> Option<String> {
        self.thread_status(session_id, cx)
            .and_then(|status| status.requested_model.clone())
    }

    pub fn chat_turn_changes_for(
        &self,
        session_id: Option<&str>,
        turn: usize,
        cx: &App,
    ) -> (Vec<agent::FileChange>, agent::ChangeCompleteness) {
        self.with_timeline_for(
            session_id,
            |timeline| {
                timeline
                    .turns
                    .get(turn)
                    .and_then(|turn| turn.changes.as_ref())
                    .map(|changes| (changes.changes.clone(), changes.completeness))
            },
            cx,
        )
        .flatten()
        .unwrap_or((Vec::new(), agent::ChangeCompleteness::Partial))
    }

    /// The rewind the thread's Harness offers to before the prompt that
    /// opened `turn` (local to the held window), or the steer `steer` in it;
    /// `None` when that message is no target.
    pub fn chat_rewind_target_for(
        &self,
        session_id: Option<&str>,
        turn: usize,
        steer: Option<&str>,
        cx: &App,
    ) -> Option<RewindTarget> {
        let status = self.thread_status(session_id, cx)?;
        let rewind = status.rewind.clone()?;
        let absolute = self.absolute_turn_for(session_id, turn);
        let (checkpoint_id, later_turns, started) = self.with_timeline_for(
            session_id,
            |timeline| {
                let meta = timeline.turns.get(turn)?;
                let checkpoint = match steer {
                    None => meta.provider_checkpoint_id.clone(),
                    Some(steer) => meta
                        .steer_checkpoints
                        .iter()
                        .find(|(id, _)| id == steer)
                        .map(|(_, checkpoint)| checkpoint.clone()),
                }?;
                let opened: HashSet<usize> = timeline
                    .entries
                    .iter()
                    .filter(|entry| {
                        entry.turn >= turn
                            && matches!(
                                entry.content,
                                tcode_core::session::EntryContent::Item(
                                    agent::ItemContent::UserMessage { .. }
                                )
                            )
                    })
                    .map(|entry| entry.turn)
                    .collect();
                let later = opened.len() - usize::from(steer.is_some() && opened.contains(&turn));
                Some((checkpoint, later, meta.start_ts))
            },
            cx,
        )??;
        let modes: Vec<agent::RewindMode> = rewind
            .modes
            .iter()
            .copied()
            .filter(|&mode| rewind.offers(mode, absolute, steer.is_some()))
            .collect();
        if modes.is_empty() {
            return None;
        }
        let since = started.map(|ms| ms / 1000);
        let delegated_after = self
            .index_replica
            .0
            .iter()
            .filter(|meta| {
                meta.parent_session_id.as_deref() == Some(status.session_id.as_str())
                    && since.is_some_and(|since| meta.created_at >= since)
            })
            .count();
        let harness = self.harness_name_for(session_id, cx);
        Some(RewindTarget {
            in_progress: rewind
                .pending
                .as_ref()
                .is_some_and(|pending| !pending.undo && pending.checkpoint_id == checkpoint_id),
            checkpoint_id,
            modes,
            later_turns,
            delegated_after,
            queued: status.queued_messages.len(),
            turn_running: status.activity.turn_running,
            worktree: status.worktree.is_some(),
            harness,
            rewind,
        })
    }

    /// The fork the thread's Harness offers through finished `turn` (local to
    /// the held window).
    pub fn chat_fork_target_for(
        &self,
        session_id: Option<&str>,
        turn: usize,
        cx: &App,
    ) -> Option<ForkTarget> {
        let fork = self.thread_status(session_id, cx)?.fork_at?;
        let checkpoint_id = self.with_timeline_for(
            session_id,
            |timeline| {
                timeline.fork_edge(turn)?;
                Some(timeline.turns[turn].provider_checkpoint_id.clone())
            },
            cx,
        )??;
        Some(ForkTarget {
            turn: self.absolute_turn_for(session_id, turn),
            checkpoint_id,
            blocked: fork.blocked,
        })
    }

    /// How this client names itself to the host.
    pub(crate) fn client_id(&self) -> &str {
        self.preview.client_id()
    }

    /// The name the thread's Harness goes by: an ACP agent's own, else the
    /// product's with its account.
    pub fn harness_name_for(&self, session_id: Option<&str>, cx: &App) -> String {
        let Some(status) = self.thread_status(session_id, cx) else {
            return "Harness".to_owned();
        };
        self.harness_name(
            status.acp_agent_id.as_deref(),
            status.requested_profile_id.as_deref(),
            status.provider,
        )
    }

    /// As [`Self::harness_name_for`], for a thread known only by its index row.
    pub fn harness_name_of(&self, meta: &SessionMeta) -> String {
        self.harness_name(
            meta.acp_agent_id.as_deref(),
            meta.profile_id.as_deref(),
            meta.provider,
        )
    }

    fn harness_name(
        &self,
        acp_agent_id: Option<&str>,
        profile_id: Option<&str>,
        provider: agent::ProviderKind,
    ) -> String {
        acp_agent_id
            .and_then(|id| self.installed_acp_agent(id))
            .map(|agent| agent.name)
            .unwrap_or_else(|| {
                self.provider_profile_display_name(
                    profile_id
                        .unwrap_or(tcode_core::settings::Settings::builtin_profile_id(provider)),
                )
            })
    }

    /// What a rewind to the target would restore, as its Harness tells
    /// before restoring anything.
    pub fn preview_rewind_for(
        &self,
        session_id: String,
        turn: usize,
        steer: Option<String>,
        checkpoint_id: String,
        mode: agent::RewindMode,
        cx: &mut App,
    ) -> Task<Result<agent::RewindPreviewResult, String>> {
        let command = self.command(
            Command::PreviewRewind {
                turn: self.absolute_turn_for(Some(&session_id), turn),
                session_id,
                steer,
                checkpoint_id: Some(checkpoint_id),
                mode,
            },
            cx,
        );
        cx.spawn(async move |_| match command.await {
            Ok(CommandResponse::RewindPreview(result)) => Ok(result),
            Ok(other) => Err(format!("unexpected answer {other:?}")),
            Err(error) => Err(error.message),
        })
    }

    pub fn chat_git_controls_for(
        &self,
        session_id: Option<&str>,
        cx: &App,
    ) -> Option<(QuickAction, Vec<MenuItem>)> {
        let git = self.thread_git(session_id, cx)?;
        let remote = self.space_allows(|policy| policy.git_remote);
        git.status.as_ref().map(|status| {
            if remote {
                (quick_action(status, git.busy), menu_items(status, git.busy))
            } else {
                tcode_core::git::local_controls(status, git.busy)
            }
        })
    }

    pub fn generate_commit_message_for(
        &self,
        session_id: Option<&str>,
        included: Option<Vec<String>>,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let session_id = session_id.unwrap_or_default().to_owned();
        let host = self.host.clone();
        cx.spawn(async move |_| {
            match host
                .query(Query::GenerateCommitMessage {
                    session_id,
                    included,
                })
                .await
            {
                Ok(QueryResponse::CommitMessage(message)) => Ok(message),
                Ok(other) => Err(format!("unexpected commit-message response: {other:?}")),
                Err(error) => Err(error.message),
            }
        })
    }

    pub fn absolute_turn_for(&self, session_id: Option<&str>, local: usize) -> usize {
        local
            + self
                .thread(session_id)
                .map_or(0, |thread| thread.turn_offset)
    }

    pub fn session_plan_for(&self, session_id: Option<&str>, cx: &App) -> Option<&SessionPlan> {
        self.watched(session_id, Part::Status, cx)?.plan.as_ref()
    }

    pub fn session_status_for(&self, session_id: Option<&str>, cx: &App) -> Option<&SessionStatus> {
        self.thread_status(session_id, cx)
    }

    /// How many threads deleting `session_id` removes (the host deletes every
    /// thread under it too), while the archived threads are held: archived
    /// descendants are not in the index.
    pub fn held_deletion_count(&self, session_id: &str) -> Option<usize> {
        let archived = self.archived_replica.as_ref()?;
        Some(self.deletion_count(session_id, &archived.sessions))
    }

    /// [`Self::held_deletion_count`] over the archived threads, fetched once.
    pub fn fetch_deletion_count(&self, session_id: &str, cx: &mut Context<Self>) -> Task<usize> {
        let host = self.host.clone();
        let session_id = session_id.to_string();
        cx.spawn(async move |this, cx| {
            let archived = match host.query(Query::ArchivedSessions).await {
                Ok(QueryResponse::ArchivedSessions(archived)) => archived.sessions,
                Ok(other) => {
                    log::warn!("unexpected archived-sessions response: {other:?}");
                    Vec::new()
                }
                Err(error) => {
                    log::warn!("archived sessions failed: {}", error.message);
                    Vec::new()
                }
            };
            this.read_with(cx, |store, _| store.deletion_count(&session_id, &archived))
                .unwrap_or(1)
        })
    }

    /// What deleting every listed archived thread removes, while the archived
    /// threads are held. Only the archived threads with no archived ancestor
    /// are sent: the host deletes each one's tree, so a thread under one is
    /// neither sent nor counted twice.
    pub fn archived_deletion(&self) -> Option<ArchivedDeletion> {
        let held = self.archived_replica.as_ref()?;
        let listed: HashSet<String> = self
            .archived_groups()
            .into_iter()
            .flat_map(|group| group.sessions.into_iter().map(|meta| meta.id))
            .collect();
        let all = || self.index_replica.0.iter().chain(&held.sessions);
        let parents: HashMap<&str, &str> = all()
            .filter_map(|meta| Some((meta.id.as_str(), meta.parent_session_id.as_deref()?)))
            .collect();
        let under_archived = |id: &str| {
            let mut seen = HashSet::from([id]);
            let mut current = id;
            while let Some(&parent) = parents.get(current) {
                if !seen.insert(parent) {
                    return false;
                }
                if listed.contains(parent) {
                    return true;
                }
                current = parent;
            }
            false
        };
        let roots: Vec<String> = held
            .sessions
            .iter()
            .filter(|meta| listed.contains(&meta.id) && !under_archived(&meta.id))
            .map(|meta| meta.id.clone())
            .collect();
        let deleted: HashSet<String> = roots
            .iter()
            .flat_map(|root| descendant_session_ids(all(), root))
            .collect();
        Some(ArchivedDeletion {
            archived: listed.len(),
            unarchived: deleted.iter().filter(|id| !listed.contains(*id)).count(),
            roots,
        })
    }

    fn deletion_count(&self, session_id: &str, archived: &[SessionMeta]) -> usize {
        descendant_session_ids(self.index_replica.0.iter().chain(archived), session_id)
            .len()
            .max(1)
    }

    pub fn worktree_orphaned_by_delete(&self, session_id: &str) -> Option<WorktreeInfo> {
        let (meta, shared) = if let Some(meta) = self
            .index_replica
            .0
            .iter()
            .find(|meta| meta.id == session_id)
        {
            (meta, &self.index_summary.worktree_shared)
        } else {
            let archived = self.archived_replica.as_ref()?;
            (
                archived
                    .sessions
                    .iter()
                    .find(|meta| meta.id == session_id)?,
                &archived.worktree_shared,
            )
        };
        // Worktrees it owns elsewhere are checked for other users when it is deleted.
        (!shared.contains(session_id))
            .then(|| meta.worktree.clone())
            .flatten()
            .or_else(|| meta.other_worktrees.first().map(|owned| owned.info.clone()))
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
impl EventEmitter<StoreChange> for WorkspaceStore {}
impl EventEmitter<TerminalChange> for WorkspaceStore {}
impl EventEmitter<ShowThread> for WorkspaceStore {}
impl EventEmitter<PreviewReleased> for WorkspaceStore {}
impl EventEmitter<ConnectionState> for WorkspaceStore {}
impl EventEmitter<tcode_protocol::HostingState> for WorkspaceStore {}

#[cfg(test)]
pub(crate) mod tests {
    use agent::{AgentEvent, ItemContent, ProviderKind, ThreadItem, TurnStatus};
    use gpui::{App, AppContext as _, TestAppContext};
    use tcode_core::{
        project::{Project, SessionMeta},
        session::{EntryContent, ReviewComment, ReviewSide, StoredEvent},
        settings::Settings,
    };
    use tcode_protocol::{Command, EventEnvelope, ServerEvent, Subscription, Topic};
    use tcode_runtime::host::HostEvent;
    use tcode_runtime::pipe::{HostServices, SpawnedHost, spawn_host};
    use tcode_services::store::SessionStore;

    use super::{
        ConversationDestination, WorkspaceAttachment, WorkspaceStore, history::HeldHistory,
    };

    pub(crate) fn seed_full_scope(
        store: &gpui::Entity<WorkspaceStore>,
        incoming: &async_channel::Sender<String>,
        deferred: Vec<String>,
        cx: &mut TestAppContext,
    ) {
        seed_full_scope_reply(store, incoming, 1, deferred, cx);
    }

    pub(crate) fn seed_full_scope_reply(
        store: &gpui::Entity<WorkspaceStore>,
        incoming: &async_channel::Sender<String>,
        request_id: u64,
        deferred: Vec<String>,
        cx: &mut TestAppContext,
    ) {
        incoming
            .try_send(
                tcode_protocol::encode_line(&tcode_protocol::HostMessage::Event(EventEnvelope {
                    request_id: Some(request_id),
                    topic: Topic::Scope,
                    event: ServerEvent::ScopeSnapshot(tcode_protocol::Scope::Full),
                }))
                .unwrap(),
            )
            .unwrap();
        let link = store.read_with(cx, |store, _| store.host.clone());
        let mut pump = std::pin::pin!(link.pump());
        let mut task_cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        for line in deferred {
            incoming.try_send(line).unwrap();
        }
    }

    struct ScriptedSpace {
        link: tcode_client::HostLink,
        scope: std::sync::Arc<std::sync::Mutex<tcode_protocol::Scope>>,
        requests: std::sync::Arc<std::sync::Mutex<Vec<tcode_protocol::ClientMessage>>>,
        worker: Option<std::thread::JoinHandle<()>>,
        driver: LocalHostDriver,
    }

    impl ScriptedSpace {
        fn new() -> Self {
            use tcode_protocol::{ClientPayload, CommandResponse, HostMessage, Scope};
            let (to_host, requests) = async_channel::unbounded();
            let (replies, from_host) = async_channel::unbounded();
            let link = tcode_client::HostLink::new(to_host, from_host);
            let project = project_at("shared", std::path::Path::new("/shared"));
            let scope = std::sync::Arc::new(std::sync::Mutex::new(Scope::Space {
                space_id: "space-one".into(),
                space_name: "Team".into(),
                projects: vec![project],
                providers: vec![tcode_protocol::ScopedProviderChoice {
                    provider: ProviderKind::Codex,
                    profile_id: Some("team-agent".into()),
                    name: "Team agent".into(),
                    models: vec![agent::ModelSpec {
                        id: "team-model".into(),
                        display_name: "Team model".into(),
                        is_default: true,
                        options: Vec::new(),
                    }],
                }],
                policy: tcode_protocol::SpacePolicy::unrestricted(),
            }));
            let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let worker_scope = scope.clone();
            let worker_recorded = recorded.clone();
            let worker = std::thread::spawn(move || {
                smol::block_on(async move {
                    while let Ok(line) = requests.recv().await {
                        let request = tcode_protocol::decode_client_line(&line).unwrap();
                        worker_recorded.lock().unwrap().push(request.clone());
                        let scope = worker_scope.lock().unwrap().clone();
                        let response = match request.payload {
                            ClientPayload::Subscribe(subscription)
                                if matches!(
                                    subscription.topic,
                                    Topic::RuntimeEvents | Topic::Preview { .. }
                                ) && scope == Scope::Full =>
                            {
                                HostMessage::Ack {
                                    id: request.id,
                                    result: Ok(CommandResponse::Unit),
                                }
                            }
                            ClientPayload::Subscribe(subscription) => {
                                let event = match (&subscription.topic, &scope) {
                                    (Topic::Scope, _) => ServerEvent::ScopeSnapshot(scope.clone()),
                                    (
                                        Topic::SpaceIndex { space_id },
                                        Scope::Space {
                                            space_id: scoped_id,
                                            projects,
                                            ..
                                        },
                                    ) if space_id == scoped_id => {
                                        let sessions = projects
                                            .first()
                                            .map(|project| {
                                                let mut session = thread(
                                                    &project.root,
                                                    "shared-thread",
                                                    &project.id,
                                                    None,
                                                );
                                                session.updated_at = 100;
                                                session
                                            })
                                            .into_iter()
                                            .collect();
                                        ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                                            projects: projects.clone(),
                                            sessions,
                                            summary: Default::default(),
                                        })
                                    }
                                    (Topic::Index, Scope::Full) => {
                                        ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                                            projects: vec![],
                                            sessions: vec![],
                                            summary: Default::default(),
                                        })
                                    }
                                    (Topic::Settings, Scope::Full) => {
                                        ServerEvent::SettingsSnapshot(Settings::default())
                                    }
                                    (Topic::Providers, Scope::Full) => {
                                        ServerEvent::ProvidersReplaced(
                                            tcode_protocol::ProvidersStatus::default(),
                                        )
                                    }
                                    _ => {
                                        let rejection = HostMessage::Ack {
                                            id: request.id,
                                            result: Err(
                                                tcode_protocol::ProtocolError::out_of_scope(
                                                    "subscription outside current space",
                                                ),
                                            ),
                                        };
                                        if replies
                                            .send(tcode_protocol::encode_line(&rejection).unwrap())
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                        continue;
                                    }
                                };
                                HostMessage::Event(EventEnvelope {
                                    request_id: Some(request.id),
                                    topic: subscription.topic,
                                    event,
                                })
                            }
                            ClientPayload::Command(Command::StartDraft { .. }) => {
                                HostMessage::Ack {
                                    id: request.id,
                                    result: Ok(CommandResponse::SessionId(None)),
                                }
                            }
                            ClientPayload::Command(_) => HostMessage::Ack {
                                id: request.id,
                                result: Ok(CommandResponse::Unit),
                            },
                            ClientPayload::Query(tcode_protocol::Query::Hosting { action }) => {
                                let created = matches!(
                                    action,
                                    tcode_protocol::HostingAction::Spaces(
                                        tcode_protocol::SpaceAction::Create { .. }
                                    )
                                );
                                let shared = matches!(
                                    action,
                                    tcode_protocol::HostingAction::Spaces(
                                        tcode_protocol::SpaceAction::SetProjects { .. }
                                    )
                                );
                                let spaces = ["created", "concurrent"]
                                    .into_iter()
                                    .map(|id| tcode_protocol::SpaceInfo {
                                        id: id.into(),
                                        name: id.into(),
                                        created_unix: 1,
                                        project_ids: if shared && id == "created" {
                                            vec!["shared".into()]
                                        } else {
                                            vec![]
                                        },
                                        link: None,
                                        link_enabled: true,
                                        link_dead: false,
                                        members: vec![],
                                        policy: tcode_protocol::SpacePolicy::unrestricted(),
                                    })
                                    .collect();
                                HostMessage::QueryResult {
                                    id: request.id,
                                    result: Ok(tcode_protocol::QueryResponse::Hosting(
                                        tcode_protocol::HostingState {
                                            created_space_id: created.then(|| "created".into()),
                                            spaces: if matches!(
                                                action,
                                                tcode_protocol::HostingAction::State
                                            ) {
                                                vec![]
                                            } else {
                                                spaces
                                            },
                                            enabled: true,
                                            expires_in_secs: 0,
                                            host_id: "machine".into(),
                                            host_name: "Studio".into(),
                                            invite: None,
                                            devices: vec![],
                                            traverse: vec![],
                                            group: None,
                                        },
                                    )),
                                }
                            }
                            ClientPayload::Query(tcode_protocol::Query::Ping) => {
                                HostMessage::QueryResult {
                                    id: request.id,
                                    result: Ok(tcode_protocol::QueryResponse::Pong),
                                }
                            }
                            ClientPayload::Query(tcode_protocol::Query::ReadProjectIcon {
                                ..
                            }) => HostMessage::QueryResult {
                                id: request.id,
                                result: Err(tcode_protocol::ProtocolError {
                                    code: "not_found".into(),
                                    message: "no project icon".into(),
                                }),
                            },
                            ClientPayload::Unsubscribe(_) => continue,
                            other => panic!("unexpected member request: {other:?}"),
                        };
                        let line = tcode_protocol::encode_line(&response).unwrap();
                        if replies.send(line).await.is_err() {
                            break;
                        }
                    }
                });
            });
            Self {
                driver: LocalHostDriver::from_link(link.clone()),
                link,
                scope,
                requests: recorded,
                worker: Some(worker),
            }
        }

        fn store(&mut self, cx: &mut TestAppContext) -> gpui::Entity<WorkspaceStore> {
            let workspace = cx.new(|cx| {
                WorkspaceStore::new_attached(
                    self.link.clone(),
                    WorkspaceAttachment::Remote {
                        host_id: "machine".into(),
                        host_name: "Studio".into(),
                    },
                    None,
                    None,
                    cx,
                )
            });
            self.driver
                .wait_until(cx, &workspace, "space baseline", |cx| {
                    workspace.read_with(cx, |store, _| store.baseline_ready())
                });
            workspace
        }

        fn barrier(&mut self, cx: &mut TestAppContext, workspace: &gpui::Entity<WorkspaceStore>) {
            self.driver
                .await_with_events(cx, workspace, self.link.query(tcode_protocol::Query::Ping))
                .unwrap();
        }
    }

    impl Drop for ScriptedSpace {
        fn drop(&mut self) {
            self.link.close();
            self.worker.take().unwrap().join().unwrap();
        }
    }

    /// Another account runs the CLI the default one does, so both pickers list
    /// that CLI's catalog; a third-party endpoint answers to its card alone.
    /// The host answers over a scripted transport.
    #[gpui::test]
    fn a_second_account_lists_the_cli_catalog_and_a_third_party_endpoint_only_its_card(
        cx: &mut TestAppContext,
    ) {
        use tcode_core::settings::{EnvVar, ProviderProfile, ProviderSettings};
        cx.update(crate::theme::init);
        let (to_host, _outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let profile = |settings| ProviderProfile {
            kind: agent::ProviderKind::ClaudeCode,
            settings,
        };
        let mut settings = Settings::default();
        settings.profiles.extend([
            (
                "second-account".to_owned(),
                profile(ProviderSettings {
                    home_path: Some("/profile-homes/second-account".into()),
                    ..Default::default()
                }),
            ),
            (
                "kimi".to_owned(),
                profile(ProviderSettings {
                    env: vec![EnvVar {
                        name: "ANTHROPIC_BASE_URL".into(),
                        value: "https://api.kimi.test/anthropic".into(),
                        sensitive: false,
                    }],
                    custom_models: vec!["kimi-model".into()],
                    ..Default::default()
                }),
            ),
        ]);
        let providers = tcode_protocol::ProvidersStatus {
            model_catalogs: [(
                agent::ProviderKind::ClaudeCode,
                vec![agent::ModelSpec {
                    id: "cli-model".into(),
                    display_name: "CLI model".into(),
                    is_default: true,
                    options: Vec::new(),
                }],
            )]
            .into(),
            ..Default::default()
        };
        for (topic, event) in [
            (Topic::Settings, ServerEvent::SettingsSnapshot(settings)),
            (Topic::Providers, ServerEvent::ProvidersReplaced(providers)),
        ] {
            incoming
                .try_send(
                    tcode_protocol::encode_line(&tcode_protocol::HostMessage::Event(
                        EventEnvelope {
                            request_id: None,
                            topic,
                            event,
                        },
                    ))
                    .unwrap(),
                )
                .unwrap();
        }
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump_link = link.clone();
        let _pump = cx.background_executor.spawn(async move {
            pump_link.pump().await;
        });
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(link, WorkspaceAttachment::Local, None, None, cx)
        });
        seed_full_scope(&store, &incoming, deferred, cx);
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        let ids = |store: &WorkspaceStore, profile: &str| {
            store
                .picker_models_for_profile(profile)
                .into_iter()
                .map(|model| model.id)
                .collect::<Vec<_>>()
        };
        store.read_with(cx, |store, _| {
            assert_eq!(ids(store, "claude"), ["cli-model"]);
            assert_eq!(ids(store, "second-account"), ["cli-model"]);
            assert_eq!(ids(store, "kimi"), ["kimi-model"]);
        });
    }

    #[gpui::test]
    fn member_scope_seeds_only_the_space_index_and_reseeds_on_reconnect(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(|cx| cx.set_reduce_motion(true));
        let mut host = ScriptedSpace::new();
        let workspace = host.store(cx);
        host.driver
            .wait_until(cx, &workspace, "member baseline", |cx| {
                workspace.read_with(cx, |store, _| store.baseline_ready())
            });
        host.barrier(cx, &workspace);
        workspace.read_with(cx, |store, _| {
            assert_eq!(
                store
                    .projects()
                    .iter()
                    .map(|p| p.id.as_str())
                    .collect::<Vec<_>>(),
                ["shared"]
            );
            assert_eq!(store.grouped_sessions()[0].sessions[0].id, "shared-thread");
            assert_eq!(store.enabled_profiles()[0].id, "team-agent");
            assert_eq!(
                store.picker_models_for_profile("team-agent")[0].id,
                "team-model"
            );
            assert!(store.settings_hydrated());
        });
        let subscriptions = host
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter_map(|request| match &request.payload {
                tcode_protocol::ClientPayload::Subscribe(subscription) => {
                    Some(subscription.topic.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            subscriptions,
            [
                Topic::Scope,
                Topic::SpaceIndex {
                    space_id: "space-one".into()
                }
            ]
        );
        cx.update(|cx| {
            crate::device::DevicePreferences::set_sidebar_layout(
                tcode_core::settings::SidebarLayout::Grouped,
                cx,
            )
        });
        let window_state = cx.new(|_| crate::window_state::WindowState::default());
        let (_sidebar, cx) = cx.add_window_view(|_, cx| {
            crate::sidebar::SessionsSidebar::new(vec![workspace.clone()], window_state, cx)
        });
        cx.simulate_resize(gpui::size(gpui::px(360.), gpui::px(800.)));
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert!(
            cx.debug_bounds("project-header-shared").is_some(),
            "the scoped project renders in the sidebar"
        );
        assert!(
            cx.debug_bounds("sidebar-thread-shared-thread").is_some(),
            "the scoped conversation renders in the sidebar"
        );
        *host.scope.lock().unwrap() = tcode_protocol::Scope::Space {
            space_id: "space-two".into(),
            space_name: "Moved".into(),
            projects: vec![project_at("other", std::path::Path::new("/other"))],
            providers: Vec::new(),
            policy: tcode_protocol::SpacePolicy::unrestricted(),
        };
        workspace.update(cx, |store, _| {
            let state = tcode_client::ConnectionState::Reconnecting {
                attempt: 1,
                reason: None,
            };
            store.host.set_connection_state(state.clone());
            store.apply_connection_state(state);
            let state = tcode_client::ConnectionState::Syncing { path: None };
            store.host.set_connection_state(state.clone());
            store.apply_connection_state(state);
        });
        host.driver
            .wait_until(cx, &workspace, "new space baseline", |cx| {
                workspace.read_with(cx, |store, _| {
                    store.projects().first().is_some_and(|p| p.id == "other")
                        && store.index_hydrated()
                })
            });
        host.barrier(cx, &workspace);
        let requests = host.requests.lock().unwrap();
        let new_space = requests
            .iter()
            .find_map(|request| match &request.payload {
                tcode_protocol::ClientPayload::Subscribe(subscription)
                    if subscription.topic
                        == (Topic::SpaceIndex {
                            space_id: "space-two".into(),
                        }) =>
                {
                    Some(subscription)
                }
                _ => None,
            })
            .expect("new space index subscription");
        assert_eq!(
            new_space.known_index, None,
            "a new space needs its own baseline"
        );
        drop(requests);
        workspace.read_with(cx, |store, _| {
            assert!(store.enabled_profiles().is_empty());
            assert_eq!(
                store.index_topic(),
                Topic::SpaceIndex {
                    space_id: "space-two".into()
                }
            );
        });
        assert!(
            host.requests
                .lock()
                .unwrap()
                .iter()
                .all(|request| !matches!(
                    &request.payload,
                    tcode_protocol::ClientPayload::Subscribe(tcode_protocol::Subscription {
                        topic: Topic::Index
                            | Topic::Settings
                            | Topic::Providers
                            | Topic::RuntimeEvents
                            | Topic::Preview { .. }
                            | Topic::PreviewSession { .. }
                            | Topic::ExternalImport { .. },
                        ..
                    })
                ))
        );
        assert!(
            cx.update(|_, cx| crate::remote::spaces::for_store(&workspace, cx))
                .is_none()
        );
        *host.scope.lock().unwrap() = tcode_protocol::Scope::Full;
        workspace.update(cx, |store, _| {
            let state = tcode_client::ConnectionState::Reconnecting {
                attempt: 1,
                reason: None,
            };
            store.host.set_connection_state(state.clone());
            store.apply_connection_state(state);
            let state = tcode_client::ConnectionState::Syncing { path: None };
            store.host.set_connection_state(state.clone());
            store.apply_connection_state(state);
        });
        host.driver
            .wait_until(cx, &workspace, "full scope baseline", |cx| {
                workspace.read_with(cx, |store, _| {
                    store.scope().is_full() && store.baseline_ready()
                })
            });

        let spaces = cx
            .update(|_, cx| crate::remote::spaces::for_store(&workspace, cx))
            .expect("Full may manage its host without pair-time metadata");
        let created = spaces.update(cx, |spaces, cx| {
            spaces.create("Created".into(), Some("shared".into()), cx)
        });
        let result = std::rc::Rc::new(std::cell::RefCell::new(None));
        let captured = result.clone();
        cx.spawn(async move |_| {
            *captured.borrow_mut() = Some(created.await);
        })
        .detach();
        host.driver
            .wait_until(cx, &workspace, "created space and project share", |_| {
                result.borrow().is_some()
            });
        assert_eq!(
            result.borrow().as_ref().unwrap().as_ref().unwrap().id,
            "created"
        );
        assert!(host.requests.lock().unwrap().iter().any(|request| matches!(&request.payload,
            tcode_protocol::ClientPayload::Query(tcode_protocol::Query::Hosting {
                action: tcode_protocol::HostingAction::Spaces(tcode_protocol::SpaceAction::SetProjects { id, .. })
            }) if id == "created")));
        assert_eq!(
            spaces.read_with(cx, |spaces, _| spaces.sharing("shared")),
            ["created"]
        );
        *host.scope.lock().unwrap() = tcode_protocol::Scope::Space {
            space_id: "space-one".into(),
            space_name: "Current".into(),
            projects: vec![],
            providers: vec![],
            policy: tcode_protocol::SpacePolicy::unrestricted(),
        };
        workspace.update(cx, |store, cx| {
            let state = tcode_client::ConnectionState::Reconnecting {
                attempt: 1,
                reason: None,
            };
            store.host.set_connection_state(state.clone());
            store.apply_connection_state(state);
            store.receive(
                EventEnvelope {
                    request_id: None,
                    topic: Topic::Scope,
                    event: ServerEvent::ScopeSnapshot(tcode_protocol::Scope::Full),
                },
                cx,
            );
            assert!(
                !store.can_manage_host(),
                "an uncorrelated scope cannot establish a role while pending"
            );
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.update(|_, cx| crate::remote::spaces::for_store(&workspace, cx))
                .is_none()
        );
        assert!(
            spaces.read_with(cx, |spaces, _| spaces.spaces().is_empty()),
            "the cached share badges are invalidated"
        );
        workspace.update(cx, |store, _| {
            let state = tcode_client::ConnectionState::Syncing { path: None };
            store.host.set_connection_state(state.clone());
            store.apply_connection_state(state);
        });
        host.driver
            .wait_until(cx, &workspace, "moved member baseline", |cx| {
                workspace.read_with(cx, |store, _| {
                    !store.scope().is_full() && store.baseline_ready()
                })
            });
        workspace.update(cx, |store, cx| {
            store.receive(
                EventEnvelope {
                    request_id: None,
                    topic: Topic::Scope,
                    event: ServerEvent::ScopeSnapshot(tcode_protocol::Scope::Full),
                },
                cx,
            );
            assert!(!store.scope().is_full());
            assert!(store.machine_label().contains("Current"));
        });
        assert!(
            cx.update(|_, cx| crate::remote::spaces::for_store(&workspace, cx))
                .is_none()
        );
    }

    #[gpui::test]
    fn member_sidebar_and_read_preferences_change_without_host_commands(cx: &mut TestAppContext) {
        let mut host = ScriptedSpace::new();
        let workspace = host.store(cx);
        host.driver
            .wait_until(cx, &workspace, "member baseline", |cx| {
                workspace.read_with(cx, |store, _| store.baseline_ready())
            });
        host.barrier(cx, &workspace);
        host.requests.lock().unwrap().clear();
        workspace.update(cx, |store, cx| {
            store.toggle_project_collapsed("shared".into(), cx);
            store.cycle_project_sort(cx);
            store.toggle_favorite_model(agent::ProviderKind::Codex, None, "team-model".into(), cx);
            store.mark_session_unread("shared-thread".into(), cx);
            assert!(store.session_unread("shared-thread"));
            store.dispatch(Command::MarkSessionRead {
                session_id: "shared-thread".into(),
                through: 100,
            });
            assert!(!store.session_unread("shared-thread"));
            assert!(store.is_project_collapsed("shared"));
            assert_eq!(
                store.project_sort(),
                tcode_core::settings::ProjectSort::NameAsc
            );
            assert_eq!(
                store.settings().favorite_models,
                [tcode_core::settings::FavoriteModel::Row {
                    provider: agent::ProviderKind::Codex,
                    profile_id: None,
                    model: "team-model".into(),
                }]
            );
        });
        host.barrier(cx, &workspace);
        assert!(
            host.requests
                .lock()
                .unwrap()
                .iter()
                .all(|request| !matches!(
                    &request.payload,
                    tcode_protocol::ClientPayload::Command(_)
                ))
        );
    }

    #[cfg(preview_backend)]
    #[gpui::test]
    fn preview_reads_the_attachment_route_when_saved_hosts_are_stale(cx: &mut TestAppContext) {
        use std::rc::Rc;
        use tcode_client::host::{ClientHost as _, LiveHost};
        let root = crate::store::tests::scratch_root("preview-route");
        let client = Rc::new(tcode_traverse::NativeClientHost::new(root.clone(), "phone"));
        let mut host = tcode_client::pairing::PairedHost {
            host_id: "09e5c592f3234ef329634a0baa6bfe06193be6b64801121d84242af1d27dbecf".into(),
            name: "Machine".into(),
            traverse: Vec::new(),
            relay: None,
            addrs: vec!["192.168.31.5:47420".into()],
            last_connected_unix: None,
            group_id: None,
            space_id: None,
            space_name: None,
        };
        client.remember_host(host.clone());
        struct NoTunnels;
        impl tcode_client::host::TunnelOpener for NoTunnels {
            fn open(
                &self,
                _target: tcode_client::host::StreamTarget,
            ) -> tcode_client::host::TunnelFuture {
                Box::pin(async { Err(std::io::Error::from(std::io::ErrorKind::NotConnected)) })
            }
        }
        let current_host = LiveHost::with_tunnels(host.clone(), std::sync::Arc::new(NoTunnels));
        let (to_host, _outgoing) = async_channel::unbounded();
        let (_incoming, from_host) = async_channel::unbounded();
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                tcode_client::HostLink::new(to_host, from_host),
                WorkspaceAttachment::Remote {
                    host_id: host.host_id.clone(),
                    host_name: host.name.clone(),
                },
                Some(client.clone()),
                Some(current_host.clone()),
                cx,
            )
        });
        host.addrs = vec!["192.168.1.161:47420".into()];
        current_host.authenticated(&host);
        cx.update(|cx| assert_eq!(store.read(cx).preview_proxy().unwrap().unwrap().0, host));
        assert_eq!(client.load_hosts()[0].addrs, ["192.168.31.5:47420"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(preview_backend)]
    #[gpui::test]
    fn preview_requests_for_an_unviewed_thread_reach_the_panel(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (to_host, _requests) = async_channel::unbounded();
        let (_replies, from_host) = async_channel::unbounded();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(link.clone(), WorkspaceAttachment::Local, None, None, cx)
        });
        let requests = store.read_with(cx, |store, _| store.remote_preview_requests());
        store.update(cx, |store, cx| {
            store.show_alone("viewed".into(), cx);
            store.receive(
                EventEnvelope {
                    request_id: None,
                    topic: Topic::Preview {
                        client: tcode_protocol::PreviewClient {
                            id: "this-client".into(),
                            name: "This client".into(),
                        },
                    },
                    event: ServerEvent::PreviewRequest {
                        request_id: 7,
                        session_id: "background".into(),
                        request: tcode_protocol::PreviewRequest::Status,
                    },
                },
                cx,
            );
        });
        let forwarded = requests
            .try_recv()
            .expect("the panel answers for a background thread");
        assert!(matches!(
            forwarded.event,
            ServerEvent::PreviewRequest { request_id: 7, ref session_id, .. } if session_id == "background"
        ));
    }

    #[gpui::test]
    fn scripted_host_send_waits_for_ack_and_rejection_offers_retry(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (to_host, requests) = async_channel::unbounded();
        let (replies, from_host) = async_channel::unbounded();
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(link.clone(), WorkspaceAttachment::Local, None, None, cx)
        });
        crate::store::tests::seed_full_scope(&store, &replies, deferred, cx);
        // The next command the client sends, past the subscriptions that
        // opening and leaving threads send.
        let next_command = || loop {
            let request =
                tcode_protocol::decode_client_line(&requests.recv_blocking().unwrap()).unwrap();
            if matches!(request.payload, tcode_protocol::ClientPayload::Command(_)) {
                break request;
            }
        };
        store.update(cx, |store, cx| {
            store.show_alone("scripted".into(), cx);
            store.send_turn_for(
                store.sole_thread().unwrap_or_default(),
                "hello".into(),
                Vec::new(),
            );
        });
        let request = next_command();
        assert!(matches!(
            request.payload,
            tcode_protocol::ClientPayload::Command(Command::SendTurn { .. })
        ));
        let key = request.key.clone().unwrap();
        assert_eq!(
            store.read_with(cx, |store, cx| store
                .delivery_messages_for(store.sole_thread().as_deref(), cx)),
            vec![(key.clone(), "hello".into(), None, false)]
        );
        replies
            .send_blocking(
                tcode_protocol::encode_line(&tcode_protocol::HostMessage::Ack {
                    id: request.id,
                    result: Err(tcode_protocol::ProtocolError {
                        code: "rejected".into(),
                        message: "Host refused this send".into(),
                    }),
                })
                .unwrap(),
            )
            .unwrap();
        // Drive the production pump on the test thread; no mocked HostLink.
        let mut pump = std::pin::pin!(link.pump());
        let mut task_cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
        assert_eq!(
            store.read_with(cx, |store, cx| store
                .delivery_messages_for(store.sole_thread().as_deref(), cx)),
            vec![(
                key.clone(),
                "hello".into(),
                Some("Host refused this send".into()),
                false,
            )]
        );
        assert!(
            !store.read_with(cx, |store, cx| store
                .chat_loading_for(store.sole_thread().as_deref(), cx)),
            "a rejected write must be visible without a snapshot"
        );
        let window_state = cx.new(|_| crate::window_state::WindowState::default());
        let (_chat, visual) = crate::chat::tests::add_chat_window(cx, |window, cx| {
            crate::chat::ChatView::new(
                store.clone(),
                window_state,
                Some("scripted".to_owned()),
                window,
                cx,
            )
        });
        visual.simulate_resize(gpui::size(gpui::px(1024.), gpui::px(700.)));
        visual.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert!(visual.debug_bounds("retry-delivery").is_some());
        assert!(visual.debug_bounds("discard-delivery").is_some());
        store.update(cx, |store, _| store.retry_delivery(&key));
        let retry = next_command();
        assert_ne!(retry.key, request.key);
        assert_eq!(retry.payload, request.payload);
        replies
            .send_blocking(
                tcode_protocol::encode_line(&tcode_protocol::HostMessage::Ack {
                    id: retry.id,
                    result: Ok(tcode_protocol::CommandResponse::Unit),
                })
                .unwrap(),
            )
            .unwrap();
        assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
        assert_eq!(
            store.read_with(cx, |store, cx| store
                .delivery_messages_for(store.sole_thread().as_deref(), cx)),
            vec![(retry.key.clone().unwrap(), "hello".into(), None, true)]
        );
        let accepted_key = retry.key.unwrap();
        let same_text_record = |id: String| {
            StoredEvent::from(AgentEvent::ItemCompleted(ThreadItem {
                id,
                parent_item_id: None,
                content: ItemContent::UserMessage {
                    text: "hello".into(),
                    context_len: None,
                    attachments: Vec::new(),
                },
            }))
        };
        store.update(cx, |store, cx| {
            store.receive(
                EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionEvents {
                        session_id: "scripted".into(),
                    },
                    event: ServerEvent::SessionSnapshot {
                        from: 0,
                        end: 1,
                        total: 1,
                        total_turns: 1,
                        records: vec![same_text_record("unrelated".into())],
                        truncated: false,
                    },
                },
                cx,
            );
            assert_eq!(
                store.delivery_messages_for(store.sole_thread().as_deref(), cx),
                vec![(accepted_key.clone(), "hello".into(), None, true)]
            );
            store.show_alone("another".into(), cx);
            // Adoption must survive navigation even when no history is retained.
            store.receive(
                EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionEvents {
                        session_id: "scripted".into(),
                    },
                    event: ServerEvent::SessionEvent(same_text_record(format!(
                        "local-user-{accepted_key}"
                    ))),
                },
                cx,
            );
            store.threads.remove("scripted");
            store.show_alone("scripted".into(), cx);
            assert!(
                store
                    .delivery_messages_for(store.sole_thread().as_deref(), cx)
                    .is_empty()
            );
        });
        store.update(cx, |store, _| {
            store.send_turn_for(
                store.sole_thread().unwrap_or_default(),
                "hello".into(),
                Vec::new(),
            )
        });
        let early = next_command();
        let early_key = early.key.unwrap();
        store.update(cx, |store, cx| {
            store.show_alone("another".into(), cx);
            store.receive(
                EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionEvents {
                        session_id: "scripted".into(),
                    },
                    event: ServerEvent::SessionEvent(same_text_record(format!(
                        "local-user-{early_key}"
                    ))),
                },
                cx,
            );
        });
        replies
            .send_blocking(
                tcode_protocol::encode_line(&tcode_protocol::HostMessage::Ack {
                    id: early.id,
                    result: Ok(tcode_protocol::CommandResponse::Unit),
                })
                .unwrap(),
            )
            .unwrap();
        assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
        store.update(cx, |store, cx| store.show_alone("scripted".into(), cx));
        assert!(
            store.read_with(cx, |store, cx| store
                .delivery_messages_for(store.sole_thread().as_deref(), cx)
                .is_empty()),
            "a replica received before its Ack must stay adopted"
        );
        store.update(cx, |store, _| {
            store.send_turn_for(
                store.sole_thread().unwrap_or_default(),
                "discard me".into(),
                Vec::new(),
            )
        });
        let request = next_command();
        replies
            .send_blocking(
                tcode_protocol::encode_line(&tcode_protocol::HostMessage::Ack {
                    id: request.id,
                    result: Err(tcode_protocol::ProtocolError {
                        code: "unknown_session".into(),
                        message: "gone".into(),
                    }),
                })
                .unwrap(),
            )
            .unwrap();
        assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
        store.update(cx, |store, _| {
            store.discard_delivery(request.key.as_ref().unwrap())
        });
        assert!(link.failed_commands().is_empty());
        assert!(link.pending_commands().is_empty());
        link.close();
    }

    #[gpui::test]
    fn archived_list_coalesces_revision_changes_and_reloads_a_stale_reply(cx: &mut TestAppContext) {
        let (to_host, requests) = async_channel::unbounded();
        let (replies, from_host) = async_channel::unbounded();
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(link.clone(), WorkspaceAttachment::Local, None, None, cx)
        });
        crate::store::tests::seed_full_scope(&store, &replies, deferred, cx);
        let mut pump = std::pin::pin!(link.pump());
        let mut task_cx = std::task::Context::from_waker(std::task::Waker::noop());
        let queries = || {
            std::iter::from_fn(|| requests.try_recv().ok())
                .filter_map(|line| {
                    let message = tcode_protocol::decode_client_line(&line).unwrap();
                    matches!(
                        message.payload,
                        tcode_protocol::ClientPayload::Query(
                            tcode_protocol::Query::ArchivedSessions
                        )
                    )
                    .then_some(message.id)
                })
                .collect::<Vec<_>>()
        };
        let revision_event = |revision| EventEnvelope {
            request_id: None,
            topic: Topic::Index,
            event: ServerEvent::IndexSummaryReplaced(tcode_protocol::IndexSummary {
                archived_revision: revision,
                ..Default::default()
            }),
        };
        store.update(cx, |store, cx| {
            store.receive(revision_event(10), cx);
            store.hold_archived_sessions(cx);
        });
        cx.run_until_parked();
        let first = queries();
        assert_eq!(first.len(), 1);
        store.update(cx, |store, cx| {
            store.receive(revision_event(11), cx);
            store.receive(revision_event(12), cx);
        });
        cx.run_until_parked();
        assert!(
            queries().is_empty(),
            "one reload may be in flight at a time"
        );
        let response = |id, revision, title: &str| {
            let mut meta = SessionMeta::new(
                ProviderKind::Codex,
                std::path::PathBuf::from("/archive"),
                None,
            );
            meta.id = "archived".into();
            meta.title = title.into();
            meta.archived_at = Some(1);
            replies
                .try_send(
                    tcode_protocol::encode_line(&tcode_protocol::HostMessage::QueryResult {
                        id,
                        result: Ok(tcode_protocol::QueryResponse::ArchivedSessions(
                            tcode_protocol::ArchivedSessions {
                                sessions: vec![meta],
                                worktree_shared: Default::default(),
                                revision,
                            },
                        )),
                    })
                    .unwrap(),
                )
                .unwrap();
        };
        response(first[0], 10, "Stale title");
        assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
        cx.run_until_parked();
        let second = queries();
        assert_eq!(second.len(), 1);
        assert!(store.read_with(cx, |store, _| store.archived_loading()));
        response(second[0], 12, "Current title");
        assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            let archived = store.archived_replica.as_ref().unwrap();
            assert_eq!(archived.revision, 12);
            assert_eq!(archived.sessions[0].title, "Current title");
        });
        store.update(cx, |store, cx| store.receive(revision_event(13), cx));
        cx.run_until_parked();
        let third = queries();
        assert_eq!(third.len(), 1, "an archived rename invalidates a held list");
        response(third[0], 13, "Renamed again");
        assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
        cx.run_until_parked();
        store.update(cx, |store, cx| {
            let state = tcode_client::ConnectionState::Reconnecting {
                attempt: 1,
                reason: None,
            };
            store.host.set_connection_state(state.clone());
            store.apply_connection_state(state);
            let state = tcode_client::ConnectionState::Syncing { path: None };
            store.host.set_connection_state(state.clone());
            store.apply_connection_state(state);
            store.receive(
                EventEnvelope {
                    request_id: None,
                    topic: Topic::Index,
                    event: ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                        summary: tcode_protocol::IndexSummary {
                            archived_revision: 1,
                            ..Default::default()
                        },
                        sessions: Vec::new(),
                        projects: Vec::new(),
                    }),
                },
                cx,
            );
        });
        cx.run_until_parked();
        let reconnect = queries();
        assert_eq!(reconnect.len(), 1);
        response(reconnect[0], 1, "New host baseline");
        assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
        cx.run_until_parked();
        store.update(cx, |store, cx| {
            assert_eq!(
                store.archived_replica.as_ref().unwrap().sessions[0].title,
                "New host baseline"
            );
            // Settings and the Agents view hold the list independently.
            store.hold_archived_sessions(cx);
            store.release_archived_sessions();
            assert!(
                !store.archived_loading(),
                "one view closing keeps another's list"
            );
            store.release_archived_sessions();
            assert!(store.archived_loading());
        });
        link.close();
    }

    /// The threads list never says "no threads" before the host's baseline.
    /// A host reached over a link is waited for behind a skeleton, which
    /// fades out over the baseline; the host in this process answers within
    /// moments, so its list stays empty until then and shows at once, with
    /// no skeleton drawn at launch.
    #[gpui::test]
    fn threads_wait_for_baseline_before_rendering_empty(cx: &mut TestAppContext) {
        use gpui::{px, size};
        cx.update(crate::theme::init);
        for remote in [true, false] {
            let (to_host, _outgoing) = async_channel::unbounded();
            let (incoming, from_host) = async_channel::unbounded();
            let store = cx.new(|cx| {
                WorkspaceStore::new_attached(
                    tcode_client::HostLink::new(to_host, from_host),
                    if remote {
                        super::WorkspaceAttachment::Remote {
                            host_id: "test".into(),
                            host_name: "Test".into(),
                        }
                    } else {
                        super::WorkspaceAttachment::Local
                    },
                    None,
                    None,
                    cx,
                )
            });
            seed_full_scope(&store, &incoming, Vec::new(), cx);
            let window_state =
                cx.new(|_| crate::window_state::WindowState::default().with_compact(true));
            let (_sidebar, cx) = cx.add_window_view(|_, cx| {
                crate::sidebar::SessionsSidebar::new(vec![store.clone()], window_state, cx)
            });
            cx.simulate_resize(size(px(393.), px(852.)));
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            assert_eq!(
                cx.debug_bounds("baseline-loading").is_some(),
                remote,
                "a skeleton before Index arrives only for a host over a link"
            );
            assert!(cx.debug_bounds("threads-empty").is_none());
            store.update(cx, |store, cx| {
                store.receive(
                    EventEnvelope {
                        request_id: None,
                        topic: Topic::Settings,
                        event: ServerEvent::SettingsSnapshot(Settings::default()),
                    },
                    cx,
                );
                store.receive(
                    EventEnvelope {
                        request_id: None,
                        topic: Topic::Index,
                        event: ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                            summary: Default::default(),
                            sessions: vec![],
                            projects: vec![],
                        }),
                    },
                    cx,
                );
                cx.emit(super::StoreChange {
                    topic: super::TopicKind::Index,
                });
                cx.notify();
            });
            cx.run_until_parked();
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            assert!(
                cx.debug_bounds("threads-empty").is_some(),
                "the baseline replaces what waited for it"
            );
            assert_eq!(
                cx.debug_bounds("baseline-loading").is_some(),
                remote,
                "only a skeleton that was drawn fades out over the baseline"
            );
            cx.executor().advance_clock(crate::edge_back::SETTLE);
            cx.update(|window, cx| {
                window.simulate_next_frame(cx);
                let _ = window.draw(cx);
            });
            assert!(cx.debug_bounds("baseline-loading").is_none());
            assert!(
                cx.debug_bounds("threads-empty").is_some(),
                "An applied empty baseline must render the empty message"
            );
        }
    }

    #[gpui::test]
    fn deleted_thread_keeps_its_pending_and_rejected_send_visible(cx: &mut TestAppContext) {
        let root = scratch_root("tcode-deleted-pending");
        let disk = SessionStore::open_at(root.clone()).unwrap();
        disk.upsert_project(&project_at("p", &root)).unwrap();
        disk.upsert_meta(&thread(&root, "deleted", "p", None))
            .unwrap();
        let host = test_host(disk);
        let mut driver = LocalHostDriver::new(&host);
        let link = driver.link.clone();
        let workspace = driver.workspace(cx);
        workspace.update(cx, |store, cx| store.show_alone("deleted".into(), cx));
        driver.wait_until(cx, &workspace, "selected thread", |cx| {
            selected_status(cx, &workspace, "deleted")
        });
        link.set_connection_state(tcode_client::ConnectionState::Reconnecting {
            attempt: 1,
            reason: None,
        });
        workspace.update(cx, |store, _| {
            store.send_turn_for(
                store.sole_thread().unwrap_or_default(),
                "keep my failed send".into(),
                Vec::new(),
            )
        });
        smol::block_on(
            host.update_state_for_test(|state, cx| state.delete_session("deleted", false, cx)),
        )
        .unwrap();
        workspace.update(cx, |store, cx| {
            store.receive(
                EventEnvelope {
                    request_id: None,
                    topic: Topic::Index,
                    event: ServerEvent::IndexRemoveSession {
                        session_id: "deleted".into(),
                    },
                },
                cx,
            )
        });
        assert_eq!(
            workspace.read_with(cx, |store, _| store.sole_thread()),
            Some("deleted".into())
        );
        link.set_connection_state(tcode_client::ConnectionState::Connected { path: None });
        driver.wait_until(cx, &workspace, "rejected send", |cx| {
            workspace.read_with(cx, |store, cx| {
                store
                    .delivery_messages_for(store.sole_thread().as_deref(), cx)
                    .iter()
                    .any(|(_, _, error, _)| error.is_some())
            })
        });
        workspace.read_with(cx, |store, cx| {
            assert_eq!(store.sole_thread().as_deref(), Some("deleted"));
            assert_eq!(
                store.delivery_messages_for(store.sole_thread().as_deref(), cx)[0].1,
                "keep my failed send"
            );
            assert!(!store.chat_loading_for(store.sole_thread().as_deref(), cx));
        });
        let key = link.failed_commands()[0].0.key.clone();
        workspace.update(cx, |store, _| store.discard_delivery(&key));
        assert!(link.pending_commands().is_empty() && link.failed_commands().is_empty());
        driver.shutdown(cx, &workspace, &host);
        let _ = std::fs::remove_dir_all(root);
    }

    #[gpui::test]
    fn rapid_selection_is_immediate_idempotent_and_retires_old_loads(cx: &mut TestAppContext) {
        let (to_host, outgoing) = async_channel::unbounded();
        let (_incoming, from_host) = async_channel::unbounded();
        let host = tcode_client::HostLink::new(to_host, from_host);
        let workspace = cx.new(|cx| {
            WorkspaceStore::new_attached(host.clone(), WorkspaceAttachment::Local, None, None, cx)
        });
        seed_full_scope(&workspace, &_incoming, Vec::new(), cx);
        workspace.update(cx, |store, cx| {
            let task_count = store.attachment_tasks.len();
            for index in 0..20 {
                let id = if index % 2 == 0 { "one" } else { "two" };
                store.show_alone(id.into(), cx);
                assert_eq!(store.sole_thread().as_deref(), Some(id));
                assert!(store.session_loading_for(&store.sole_thread().unwrap_or_default()));
                let messages = outgoing.len();
                store.show_alone(id.into(), cx);
                assert_eq!(outgoing.len(), messages, "second tap must send nothing");
                assert_eq!(store.attachment_tasks.len(), task_count);
                assert_eq!(
                    host.subscriptions()
                        .iter()
                        .filter(|sub| matches!(sub.topic, Topic::SessionEvents { .. }))
                        .count(),
                    1
                );
                assert_eq!(
                    host.subscriptions()
                        .iter()
                        .filter(|sub| matches!(sub.topic, Topic::SessionStatus { .. }))
                        .count(),
                    1
                );
            }
            store.receive(
                EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionEvents {
                        session_id: "one".into(),
                    },
                    event: ServerEvent::SessionSnapshot {
                        total: 0,
                        total_turns: 0,
                        truncated: false,
                        from: 0,
                        end: 0,
                        records: vec![],
                    },
                },
                cx,
            );
            assert!(
                store.session_loading_for(&store.sole_thread().unwrap_or_default()),
                "retired load must not replace the skeleton"
            );
            assert!(store.thread_timeline(Some("two"), cx).is_none());
        });
    }

    #[gpui::test]
    fn rapid_thread_revisit_keeps_cached_history_when_old_replies_arrive(cx: &mut TestAppContext) {
        for old_first in [false, true] {
            for empty in [false, true] {
                let (to_host, _outgoing) = async_channel::unbounded();
                let (incoming, from_host) = async_channel::unbounded();
                let host = tcode_client::HostLink::new(to_host, from_host);
                let workspace = cx.new(|cx| {
                    WorkspaceStore::new_attached(
                        host.clone(),
                        WorkspaceAttachment::Local,
                        None,
                        None,
                        cx,
                    )
                });
                seed_full_scope(&workspace, &incoming, Vec::new(), cx);
                workspace.update(cx, |store, cx| {
                    let topic = Topic::SessionEvents { session_id: "a".into() };
                    store.show_alone("a".into(), cx);
                    let record = |text: &str| StoredEvent::from(AgentEvent::Delta {
                        item_id: "answer".into(), kind: agent::DeltaKind::AssistantText, text: text.into(),
                    });
                    let baseline_id = host.subscription_request_id(&topic).unwrap();
                    store.receive(EventEnvelope {
                        request_id: Some(baseline_id), topic: topic.clone(),
                        event: ServerEvent::SessionSnapshot { from: 10, end: 12, records: vec![record("cached")], total: 12, total_turns: 1, truncated: false },
                    }, cx);
                    host.subscribe(Subscription { topic: topic.clone(), after: Some(12), known_index: None }).unwrap();
                    let old_id = host.subscription_request_id(&topic).unwrap();
                    store.show_alone("b".into(), cx);
                    store.show_alone("a".into(), cx);
                    let current_id = host.subscription_request_id(&topic).unwrap();
                    assert_ne!(old_id, current_id);
                    let reply = |id| EventEnvelope {
                        request_id: Some(id), topic: topic.clone(),
                        event: ServerEvent::SessionSnapshot {
                            from: 12, end: if empty { 12 } else { 14 },
                            records: if empty { vec![] } else { vec![record(" continued")] },
                            total: if empty { 12 } else { 14 }, total_turns: 1, truncated: false,
                        },
                    };
                    for id in if old_first { [old_id, current_id] } else { [current_id, old_id] } {
                        store.receive(reply(id), cx);
                    }
                    store.receive(reply(current_id), cx);
                    let held = store.threads["a"].history.as_ref().unwrap();
                    assert_eq!((held.from, held.end), (10, if empty { 12 } else { 14 }));
                    let timeline = &store.thread_timeline(Some("a"), cx).unwrap();
                    assert_eq!(timeline.entries.len(), 1);
                    assert!(matches!(&timeline.entries[0].content, EntryContent::Item(ItemContent::AssistantMessage { text, .. }) if text == if empty { "cached" } else { "cached continued" }));
                    let end = if empty { 12 } else { 14 };
                    host.subscribe(Subscription { topic: topic.clone(), after: None, known_index: None }).unwrap();
                    let replacement_id = host.subscription_request_id(&topic).unwrap();
                    let replacement = EventEnvelope { request_id: Some(replacement_id), topic: topic.clone(), event: ServerEvent::SessionSnapshot { from: end, end: end + 2, records: vec![record("replacement")], total: end + 2, total_turns: 1, truncated: false } };
                    store.receive(replacement.clone(), cx);
                    store.receive(reply(current_id), cx);
                    store.receive(replacement, cx);
                    let held = store.threads["a"].history.as_ref().unwrap();
                    assert_eq!((held.from, held.end), (end, end + 2));
                    let timeline = &store.thread_timeline(Some("a"), cx).unwrap();
                    assert_eq!(timeline.entries.len(), 1);
                    assert!(matches!(&timeline.entries[0].content, EntryContent::Item(ItemContent::AssistantMessage { text, .. }) if text == "replacement"));
                });
            }
        }
    }

    #[gpui::test]
    fn paged_history_keeps_event_and_turn_cursors_absolute(cx: &mut TestAppContext) {
        let (to_host, outgoing) = async_channel::unbounded();
        let (_incoming, from_host) = async_channel::unbounded();
        let host = tcode_client::HostLink::new(to_host, from_host);
        let workspace = cx.new(|cx| {
            WorkspaceStore::new_attached(host.clone(), WorkspaceAttachment::Local, None, None, cx)
        });
        seed_full_scope(&workspace, &_incoming, Vec::new(), cx);
        workspace.update(cx, |store, cx| {
            store.show_alone("large".into(), cx);
            let records = (450..500)
                .flat_map(|index| {
                    [
                        AgentEvent::TurnStarted {
                            turn_id: index.to_string(),
                        }
                        .into(),
                        AgentEvent::ItemCompleted(ThreadItem {
                            id: format!("user-{index}"),
                            parent_item_id: None,
                            content: ItemContent::UserMessage {
                                text: format!("Message {index}"),
                                attachments: vec![],
                                context_len: None,
                            },
                        })
                        .into(),
                        AgentEvent::ItemCompleted(ThreadItem {
                            id: format!("assistant-{index}"),
                            parent_item_id: None,
                            content: ItemContent::AssistantMessage {
                                text: format!("Response {index}"),
                                summarized: false,
                            },
                        })
                        .into(),
                        AgentEvent::TurnCompleted {
                            turn_id: index.to_string(),
                            status: TurnStatus::Completed,
                            usage: None,
                        }
                        .into(),
                    ]
                })
                .collect();
            store.receive(
                EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionEvents {
                        session_id: "large".into(),
                    },
                    event: ServerEvent::SessionSnapshot {
                        from: 1800,
                        end: 2000,
                        records,
                        total: 2000,
                        total_turns: 500,
                        truncated: false,
                    },
                },
                cx,
            );
            assert_eq!(
                store
                    .thread(store.sole_thread().as_deref())
                    .map_or(0, |thread| thread.turn_offset),
                450
            );
            assert_eq!(
                host.subscriptions()
                    .iter()
                    .find(|sub| matches!(sub.topic, Topic::SessionEvents { .. }))
                    .unwrap()
                    .after,
                Some(2000)
            );
            while outgoing.try_recv().is_ok() {}
            store.rewind_turn_for(
                store.sole_thread().unwrap_or_default(),
                2,
                None,
                None,
                agent::RewindMode::Conversation,
            );
            let command =
                tcode_protocol::decode_client_line(&outgoing.try_recv().unwrap()).unwrap();
            assert!(matches!(
                command.payload,
                tcode_protocol::ClientPayload::Command(Command::RewindTurn { turn: 452, .. })
            ));
            let topic = Topic::SessionEvents {
                session_id: "large".into(),
            };
            store.baseline_topics.remove(&topic);
            host.subscribe(Subscription {
                topic: topic.clone(),
                after: Some(2000),
                known_index: None,
            })
            .unwrap();
            let reconnect_id = host.subscription_request_id(&topic).unwrap();
            let retained_entry =
                store.thread_timeline(Some("large"), cx).unwrap().entries[0].clone();
            store.receive(
                EventEnvelope {
                    request_id: Some(reconnect_id),
                    topic: topic.clone(),
                    event: ServerEvent::SessionSnapshot {
                        from: 2000,
                        end: 2000,
                        records: vec![],
                        total: 2000,
                        total_turns: 500,
                        truncated: false,
                    },
                },
                cx,
            );
            assert!(
                store.baseline_topics.contains(&topic),
                "an empty reconnect tail still establishes readiness"
            );
            assert!(
                std::sync::Arc::ptr_eq(
                    &retained_entry,
                    &store.thread_timeline(Some("large"), cx).unwrap().entries[0]
                ),
                "empty replay must retain the existing timeline"
            );
        });
    }

    /// The host merges records, so the cursors a client resumes from are the
    /// window it was sent, not the records it holds; visit times and index
    /// facts arrive as changes rather than replacements.
    #[gpui::test]
    fn merged_windows_visits_and_summaries_apply_as_changes(cx: &mut TestAppContext) {
        use std::collections::{HashMap, HashSet};
        use tcode_core::session::StoredEvent;
        use tcode_protocol::IndexSummary;
        let (to_host, _outgoing) = async_channel::unbounded();
        let (_incoming, from_host) = async_channel::unbounded();
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let workspace = cx.new(|cx| {
            WorkspaceStore::new_attached(link, WorkspaceAttachment::Local, None, None, cx)
        });
        crate::store::tests::seed_full_scope(&workspace, &_incoming, deferred, cx);
        let topic = Topic::SessionEvents {
            session_id: "merged".into(),
        };
        let tool = agent::AgentEvent::ItemCompleted(agent::ThreadItem {
            id: "shot".into(),
            parent_item_id: None,
            content: ItemContent::ToolCall {
                image_reads: Vec::new(),
                name: "screenshot".into(),
                input: serde_json::json!({}),
                output: Some("preview".into()),
                status: agent::ItemStatus::Completed,
            },
        });
        workspace.update(cx, |store, cx| {
            store.show_alone("merged".into(), cx);
            store.settings_replica.last_visited = HashMap::from([("old".into(), 1)]);
            let event = |event| EventEnvelope {
                request_id: None,
                topic: topic.clone(),
                event,
            };
            store.receive(
                event(ServerEvent::SessionSnapshot {
                    from: 10,
                    end: 20,
                    records: vec![StoredEvent {
                        origin: None,
                        author: None,
                        ts: Some(1),
                        event: tool.clone(),
                        elided: Some(700_000),
                    }],
                    total: 20,
                    total_turns: 1,
                    truncated: false,
                }),
                cx,
            );
            assert_eq!(
                (
                    held_history(store, "merged").from,
                    held_history(store, "merged").end
                ),
                (10, 20)
            );
            assert_eq!(
                store.with_timeline_for(
                    store.sole_thread().as_deref(),
                    |timeline| timeline.elided_outputs.get("shot").copied(),
                    cx,
                ),
                Some(Some(700_000))
            );
            store.receive(
                event(ServerEvent::SessionEvent(
                    agent::AgentEvent::TurnStarted {
                        turn_id: "next".into(),
                    }
                    .into(),
                )),
                cx,
            );
            assert_eq!(held_history(store, "merged").end, 21);
            store.receive(
                EventEnvelope {
                    request_id: None,
                    topic: Topic::Settings,
                    event: ServerEvent::LastVisitedChanged(HashMap::from([("new".into(), 2)])),
                },
                cx,
            );
            assert_eq!(
                store.settings_replica.last_visited,
                HashMap::from([("old".into(), 1), ("new".into(), 2)])
            );
            store.receive(
                EventEnvelope {
                    request_id: None,
                    topic: Topic::Index,
                    event: ServerEvent::IndexSummaryReplaced(IndexSummary {
                        title_generating: HashSet::from(["named".into()]),
                        ..IndexSummary::default()
                    }),
                },
                cx,
            );
            assert!(store.title_generating("named"));
        });
    }

    /// A device with full access to a remote machine is the owner's own: the
    /// owner's unattributed messages carry no byline there, while a space
    /// member's message is still attributed.
    #[gpui::test]
    fn a_full_remote_device_attributes_only_space_members(cx: &mut TestAppContext) {
        use tcode_core::session::{Author, StoredEvent};
        let (to_host, _outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let workspace = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                WorkspaceAttachment::Remote {
                    host_id: "machine".into(),
                    host_name: "Machine".into(),
                },
                None,
                None,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&workspace, &incoming, Vec::new(), cx);
        let message = |id: &str, author: Option<Author>| StoredEvent {
            origin: None,
            author,
            ts: Some(1),
            event: agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                id: id.into(),
                parent_item_id: None,
                content: ItemContent::UserMessage {
                    text: id.into(),
                    context_len: None,
                    attachments: Vec::new(),
                },
            }),
            elided: None,
        };
        let snapshot = |records: Vec<StoredEvent>| EventEnvelope {
            request_id: None,
            topic: Topic::SessionEvents {
                session_id: "shared".into(),
            },
            event: ServerEvent::SessionSnapshot {
                from: 0,
                end: records.len() as u64,
                total: records.len() as u64,
                records,
                total_turns: 1,
                truncated: false,
            },
        };
        workspace.update(cx, |store, cx| {
            store.show_alone("shared".into(), cx);
            store.receive(snapshot(vec![message("owner", None)]), cx);
            assert_eq!(
                store.message_byline_for(store.sole_thread().as_deref(), "owner", cx),
                None
            );

            let member = Author {
                device_id: "member-device".into(),
                name: "Alex's laptop".into(),
            };
            store.receive(snapshot(vec![message("member", Some(member))]), cx);
            assert_eq!(
                store
                    .message_byline_for(store.sole_thread().as_deref(), "member", cx)
                    .as_deref(),
                Some("Alex's laptop")
            );
            store.receive(
                EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionEvents {
                        session_id: "shared".into(),
                    },
                    event: ServerEvent::SessionEvent(message("owner", None)),
                },
                cx,
            );
            assert_eq!(
                store
                    .message_byline_for(store.sole_thread().as_deref(), "owner", cx)
                    .as_deref(),
                Some(crate::tr!("member.owner").as_ref())
            );
            assert_eq!(
                store
                    .message_byline_for(store.sole_thread().as_deref(), "member", cx)
                    .as_deref(),
                Some("Alex's laptop")
            );

            // An orchestrator's message to its child is no member's, the owner's included.
            let mut lead = message("lead", None);
            lead.origin = Some(tcode_core::session::MessageOrigin::Agent);
            store.receive(
                EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionEvents {
                        session_id: "shared".into(),
                    },
                    event: ServerEvent::SessionEvent(lead),
                },
                cx,
            );
            assert_eq!(
                store
                    .message_byline_for(store.sole_thread().as_deref(), "lead", cx)
                    .as_deref(),
                Some(crate::tr!("chat.from_orchestrator").as_ref())
            );
        });
    }

    /// A thread is read only once its conversation has loaded and is on
    /// screen, and stays read through updates that land while it is shown. A
    /// view left before its conversation arrived reports nothing.
    #[gpui::test]
    fn a_thread_is_reported_read_once_its_conversation_loads(cx: &mut TestAppContext) {
        let (to_host, outgoing) = async_channel::unbounded();
        let (_incoming, from_host) = async_channel::unbounded();
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let workspace = cx.new(|cx| {
            WorkspaceStore::new_attached(link, WorkspaceAttachment::Local, None, None, cx)
        });
        crate::store::tests::seed_full_scope(&workspace, &_incoming, deferred, cx);
        let reads = || {
            std::iter::from_fn(|| outgoing.try_recv().ok())
                .filter_map(|line| {
                    match tcode_protocol::decode_client_line(&line).unwrap().payload {
                        tcode_protocol::ClientPayload::Command(Command::MarkSessionRead {
                            session_id,
                            through,
                        }) => Some((session_id, through)),
                        _ => None,
                    }
                })
                .collect::<Vec<_>>()
        };
        let upsert = |id: &str, updated_at| {
            let mut meta =
                SessionMeta::new(ProviderKind::Codex, std::path::PathBuf::from("/tmp"), None);
            meta.id = id.into();
            meta.updated_at = updated_at;
            EventEnvelope {
                request_id: None,
                topic: Topic::Index,
                event: ServerEvent::IndexUpsertSession(Box::new(meta)),
            }
        };
        workspace.update(cx, |store, cx| {
            store.receive(upsert("left", 100), cx);
            store.receive(upsert("shown", 100), cx);
            store.settings_replica.last_visited =
                std::collections::HashMap::from([("left".into(), 50), ("shown".into(), 50)]);

            store.set_conversation_on_screen(true, cx);
            store.show_alone("left".into(), cx);
            store.leave_threads(cx);
            store.receive(session_snapshot("left", 0, vec![reply(1)]), cx);
            assert_eq!(reads(), [], "left before the conversation loaded");

            store.show_alone("shown".into(), cx);
            assert_eq!(reads(), [], "nothing has loaded yet");
            // A compact window went back to the thread list, which keeps the
            // thread selected, before the conversation arrived.
            store.set_conversation_on_screen(false, cx);
            store.receive(session_snapshot("shown", 0, vec![reply(1)]), cx);
            assert_eq!(reads(), [], "loaded behind the thread list");
            store.set_conversation_on_screen(true, cx);
            assert_eq!(reads(), [("shown".to_string(), 100)]);
            store.receive(upsert("left", 110), cx);
            assert_eq!(reads(), [], "unchanged for the thread on screen");

            store.receive(upsert("shown", 120), cx);
            assert_eq!(reads(), [("shown".to_string(), 120)]);
        });
    }

    #[gpui::test]
    fn history_prefetch_keeps_one_bounded_page_in_flight(cx: &mut TestAppContext) {
        let root = scratch_root("tcode-prefetch-test");
        let host = test_host(SessionStore::open_at(root.clone()).unwrap());
        let mut status = smol::block_on(host.update_state_for_test(|state, cx| {
            let id = state.start_draft("history".into(), std::env::temp_dir(), cx);
            state.session_status_snapshot(&id).unwrap()
        }))
        .unwrap();
        status.session_id = "large".into();
        host.shutdown_blocking()
            .expect("stop unsubscribed fixture host");
        std::fs::remove_dir_all(root).unwrap();

        let (to_host, outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump_link = link.clone();
        let _pump = cx.background_executor.spawn(async move {
            pump_link.pump().await;
        });
        let workspace = cx.new(|cx| {
            WorkspaceStore::new_attached(link, WorkspaceAttachment::Local, None, None, cx)
        });
        crate::store::tests::seed_full_scope(&workspace, &incoming, deferred, cx);
        workspace.update(cx, |store, cx| {
            store.show_alone("large".into(), cx);
            store.receive(
                EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionStatus {
                        session_id: "large".into(),
                    },
                    event: ServerEvent::SessionStatusReplaced(Box::new(status)),
                },
                cx,
            );
            store.receive(
                EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionEvents {
                        session_id: "large".into(),
                    },
                    event: ServerEvent::SessionSnapshot {
                        from: 1800,
                        end: 2000,
                        records: (0..200)
                            .map(|_| {
                                agent::AgentEvent::Warning {
                                    message: "short".into(),
                                }
                                .into()
                            })
                            .collect(),
                        total: 2000,
                        total_turns: 1,
                        truncated: false,
                    },
                },
                cx,
            );
        });
        while outgoing.try_recv().is_ok() {}
        workspace.update(cx, |store, cx| {
            store.update_history_window_for(store.sole_thread().as_deref(), 1., true, cx)
        });
        cx.run_until_parked();
        let mut request =
            tcode_protocol::decode_client_line(&outgoing.try_recv().unwrap()).unwrap();
        assert!(matches!(
            request.payload,
            tcode_protocol::ClientPayload::Query(tcode_protocol::Query::SessionHistoryPage {
                before: 1800,
                limit: 200,
                ..
            })
        ));
        workspace.update(cx, |store, cx| {
            assert!(store.history_loading_for(store.sole_thread().as_deref()));
            store.update_history_window_for(store.sole_thread().as_deref(), 1., false, cx);
        });
        cx.run_until_parked();
        assert!(
            outgoing.try_recv().is_err(),
            "scrolling while loading must not queue another page"
        );

        incoming
            .try_send(
                tcode_protocol::encode_line(&tcode_protocol::HostMessage::QueryResult {
                    id: request.id,
                    result: Err(tcode_protocol::ProtocolError::decode("offline")),
                })
                .unwrap(),
            )
            .unwrap();
        cx.run_until_parked();
        workspace.update(cx, |store, cx| {
            assert!(
                !store.history_loading_for(store.sole_thread().as_deref()),
                "failed pages hide activity"
            );
            store.update_history_window_for(store.sole_thread().as_deref(), 1., false, cx);
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(4));
        workspace.update(cx, |store, cx| {
            store.update_history_window_for(store.sole_thread().as_deref(), 1., false, cx)
        });
        cx.run_until_parked();
        assert!(
            outgoing.try_recv().is_err(),
            "retry is throttled for five seconds"
        );
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        workspace.update(cx, |store, cx| {
            store.update_history_window_for(store.sole_thread().as_deref(), 1., false, cx)
        });
        cx.run_until_parked();
        let retry = tcode_protocol::decode_client_line(&outgoing.try_recv().unwrap()).unwrap();
        assert_eq!(
            retry.payload, request.payload,
            "retry requests the same bounded page"
        );
        request = retry;
        workspace.update(cx, |store, cx| {
            assert!(store.history_loading_for(store.sole_thread().as_deref()));
            store.update_history_window_for(store.sole_thread().as_deref(), 1., false, cx);
        });
        cx.run_until_parked();
        assert!(
            outgoing.try_recv().is_err(),
            "retry also permits one page in flight"
        );

        for page in 0..4 {
            let before = 1800 - page * 200;
            assert!(matches!(
                request.payload,
                tcode_protocol::ClientPayload::Query(tcode_protocol::Query::SessionHistoryPage {
                    before: requested_before,
                    limit: 200,
                    ..
                }) if requested_before == before
            ));
            let records = (0..100)
                .flat_map(|turn| {
                    let turn_id = format!("{page}-{turn}");
                    [
                        agent::AgentEvent::TurnStarted {
                            turn_id: turn_id.clone(),
                        }
                        .into(),
                        agent::AgentEvent::TurnCompleted {
                            turn_id,
                            status: agent::TurnStatus::Completed,
                            usage: None,
                        }
                        .into(),
                    ]
                })
                .collect();
            incoming
                .try_send(
                    tcode_protocol::encode_line(&tcode_protocol::HostMessage::QueryResult {
                        id: request.id,
                        result: Ok(tcode_protocol::QueryResponse::SessionHistoryPage {
                            records,
                            from: before - 200,
                            end: before,
                            truncated: false,
                        }),
                    })
                    .unwrap(),
                )
                .unwrap();
            cx.run_until_parked();
            workspace.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            cx.run_until_parked();
            assert!(
                workspace.read_with(cx, |store, _| {
                    held_window(store, "large").is_some_and(|held| held.from == before - 200)
                }),
                "the prefetched page is applied"
            );
            assert!(
                outgoing.try_recv().is_err(),
                "yield between automatic pages"
            );
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(250));
            cx.run_until_parked();
            workspace.update(cx, |store, cx| {
                store.update_history_window_for(
                    store.sole_thread().as_deref(),
                    if page < 3 { 2. + page as f32 } else { 6. },
                    true,
                    cx,
                );
            });
            cx.run_until_parked();
            if page < 3 {
                request =
                    tcode_protocol::decode_client_line(&outgoing.try_recv().unwrap()).unwrap();
            }
        }
        assert!(
            outgoing.try_recv().is_err(),
            "stop when six screens are covered, without a scroll event"
        );
        workspace.read_with(cx, |store, _| {
            assert_eq!(held_history(store, "large").from, 1000);
            assert_eq!(held_history(store, "large").records.len(), 1000);
            assert!(!store.history_loading_for(store.sole_thread().as_deref()));
        });
    }

    fn draft_status() -> tcode_protocol::SessionStatus {
        let root = scratch_root("tcode-draft-status-test");
        let host = test_host(SessionStore::open_at(root.clone()).unwrap());
        let status = smol::block_on(host.update_state_for_test(|state, cx| {
            let id = state.start_draft("running".into(), std::env::temp_dir(), cx);
            state.session_status_snapshot(&id).unwrap()
        }))
        .unwrap();
        host.shutdown_blocking()
            .expect("stop unsubscribed fixture host");
        std::fs::remove_dir_all(root).unwrap();
        status
    }

    fn with_running_turn(
        status: &tcode_protocol::SessionStatus,
        running: Option<tcode_core::session::RunningTurn>,
        question: Option<tcode_core::session::PendingUserInput>,
    ) -> tcode_protocol::SessionStatus {
        let mut status = status.clone();
        status.activity.turn_running = running.is_some();
        status.activity.working = running.is_some();
        status.running_turn = running;
        status.pending_user_input = question;
        status
    }

    fn question() -> tcode_core::session::PendingUserInput {
        tcode_core::session::PendingUserInput {
            request_id: "ask".into(),
            questions: vec![agent::UserInputQuestion {
                id: "which".into(),
                header: "Next".into(),
                question: "Which way?".into(),
                options: Vec::new(),
                multi_select: false,
                prefill: None,
            }],
            delivery: agent::UserInputDelivery::Blocking,
        }
    }

    fn recorded(ts: u64, event: AgentEvent) -> StoredEvent {
        StoredEvent {
            origin: None,
            author: None,
            ts: Some(ts),
            ..event.into()
        }
    }

    fn reply(ts: u64) -> StoredEvent {
        recorded(
            ts,
            AgentEvent::ItemCompleted(ThreadItem {
                id: format!("reply-{ts}"),
                parent_item_id: None,
                content: ItemContent::AssistantMessage {
                    text: "working".into(),
                    summarized: false,
                },
            }),
        )
    }

    fn session_snapshot(session_id: &str, from: u64, records: Vec<StoredEvent>) -> EventEnvelope {
        let end = from + records.len() as u64;
        EventEnvelope {
            request_id: None,
            topic: Topic::SessionEvents {
                session_id: session_id.into(),
            },
            event: ServerEvent::SessionSnapshot {
                from,
                end,
                records,
                total: end,
                total_turns: 1,
                truncated: from > 0,
            },
        }
    }

    fn live_turn(workspace: &gpui::Entity<WorkspaceStore>, cx: &TestAppContext) -> Option<u64> {
        workspace.read_with(cx, |store, cx| {
            store
                .with_timeline_for(
                    store.sole_thread().as_deref(),
                    |timeline| {
                        timeline
                            .turns
                            .last()
                            .filter(|turn| turn.running)
                            .and_then(|turn| turn.start_ts)
                    },
                    cx,
                )
                .flatten()
        })
    }

    #[gpui::test]
    fn the_status_settles_the_running_turn_whichever_topic_arrives_first(cx: &mut TestAppContext) {
        let status = draft_status();
        let id = status.session_id.clone();
        let status_event = |running: Option<u64>, question| EventEnvelope {
            request_id: None,
            topic: Topic::SessionStatus {
                session_id: id.clone(),
            },
            event: ServerEvent::SessionStatusReplaced(Box::new(with_running_turn(
                &status,
                running.map(|turn| tcode_core::session::RunningTurn {
                    turn,
                    started_at: Some(1_000),
                }),
                question,
            ))),
        };
        let snapshot = session_snapshot(
            &id,
            0,
            vec![
                recorded(
                    1_000,
                    AgentEvent::TurnStarted {
                        turn_id: "turn".into(),
                    },
                ),
                reply(2_000),
                recorded(
                    3_000,
                    AgentEvent::UserInputRequested {
                        request_id: "ask".into(),
                        questions: question().questions,
                        delivery: agent::UserInputDelivery::Blocking,
                    },
                ),
            ],
        );
        for status_first in [false, true] {
            let (to_host, _outgoing) = async_channel::unbounded();
            let (_incoming, from_host) = async_channel::unbounded();
            let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
            let link = tcode_client::HostLink::new(to_host, from_host);
            let workspace = cx.new(|cx| {
                WorkspaceStore::new_attached(link, WorkspaceAttachment::Local, None, None, cx)
            });
            crate::store::tests::seed_full_scope(&workspace, &_incoming, deferred, cx);
            let open_question = |cx: &TestAppContext| {
                workspace.read_with(cx, |store, cx| {
                    store
                        .composer_state_for(store.sole_thread().as_deref(), cx)
                        .pending_user_input
                        .map(|pending| pending.request_id)
                })
            };
            workspace.update(cx, |store, cx| {
                // The status cached from the last visit, before this turn began.
                store.show_alone(id.clone(), cx);
                store.receive(
                    EventEnvelope {
                        request_id: None,
                        topic: Topic::SessionStatus {
                            session_id: id.clone(),
                        },
                        event: ServerEvent::SessionStatusReplaced(Box::new(status.clone())),
                    },
                    cx,
                );
                store.leave_threads(cx);
                store.show_alone(id.clone(), cx);
                if status_first {
                    store.receive(status_event(Some(0), Some(question())), cx);
                    store.receive(snapshot.clone(), cx);
                } else {
                    store.receive(snapshot.clone(), cx);
                    assert_eq!(
                        store.with_timeline_for(
                            store.sole_thread().as_deref(),
                            |t| t.turn_running,
                            cx
                        ),
                        Some(false)
                    );
                    store.receive(status_event(Some(0), Some(question())), cx);
                }
            });
            assert_eq!(live_turn(&workspace, cx), Some(1_000));
            assert_eq!(open_question(cx).as_deref(), Some("ask"));

            // The next turn runs before its opening record arrives.
            workspace.update(cx, |store, cx| {
                store.receive(status_event(Some(1), None), cx)
            });
            assert_eq!(live_turn(&workspace, cx), None);
            assert_eq!(
                workspace.read_with(cx, |store, cx| store.with_timeline_for(
                    store.sole_thread().as_deref(),
                    |timeline| timeline.turn_running,
                    cx,
                )),
                Some(true)
            );

            workspace.update(cx, |store, cx| store.receive(status_event(None, None), cx));
            assert_eq!(live_turn(&workspace, cx), None);
            assert_eq!(open_question(cx), None);
        }
    }

    #[gpui::test]
    fn open_threads_each_read_their_own_replica(cx: &mut TestAppContext) {
        let a = draft_status();
        let mut b = with_running_turn(&a, None, Some(question()));
        b.session_id = "thread-b".into();
        let status = |status: &tcode_protocol::SessionStatus| EventEnvelope {
            request_id: None,
            topic: Topic::SessionStatus {
                session_id: status.session_id.clone(),
            },
            event: ServerEvent::SessionStatusReplaced(Box::new(status.clone())),
        };
        let (to_host, _outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let workspace = cx.new(|cx| {
            WorkspaceStore::new_attached(link, WorkspaceAttachment::Local, None, None, cx)
        });
        crate::store::tests::seed_full_scope(&workspace, &incoming, deferred, cx);
        workspace.update(cx, |store, cx| {
            store.show_alone(a.session_id.clone(), cx);
            store.open_thread(&b.session_id, cx);
            store.receive(status(&a), cx);
            store.receive(session_snapshot(&a.session_id, 0, vec![reply(1)]), cx);
            store.receive(status(&b), cx);
            store.receive(session_snapshot(&b.session_id, 0, vec![reply(2)]), cx);
        });
        let shown = |store: &WorkspaceStore, id: &str, cx: &App| {
            (
                store
                    .composer_state_for(Some(id), cx)
                    .pending_user_input
                    .map(|pending| pending.request_id),
                store.with_timeline_for(
                    Some(id),
                    |timeline| {
                        timeline
                            .entries
                            .iter()
                            .map(|entry| entry.id.clone())
                            .collect::<Vec<_>>()
                    },
                    cx,
                ),
            )
        };
        let a_shows = (None, Some(vec!["reply-1".to_string()]));
        let b_shows = (Some("ask".to_string()), Some(vec!["reply-2".to_string()]));
        workspace.read_with(cx, |store, cx| {
            assert_eq!(shown(store, &a.session_id, cx), a_shows);
            assert_eq!(shown(store, &b.session_id, cx), b_shows);
        });

        // Each keeps receiving its events.
        workspace.update(cx, |store, cx| {
            store.receive(status(&with_running_turn(&a, None, Some(question()))), cx);
        });
        workspace.read_with(cx, |store, cx| {
            assert_eq!(
                shown(store, &a.session_id, cx),
                (b_shows.0.clone(), a_shows.1)
            );
            assert_eq!(shown(store, &b.session_id, cx), b_shows);
        });
    }

    #[gpui::test]
    fn a_window_cut_inside_the_running_turn_is_timed_from_the_host(cx: &mut TestAppContext) {
        let status = with_running_turn(
            &draft_status(),
            Some(tcode_core::session::RunningTurn {
                turn: 0,
                started_at: Some(1_000),
            }),
            None,
        );
        let id = status.session_id.clone();
        let (to_host, outgoing) = async_channel::unbounded();
        let (_incoming, from_host) = async_channel::unbounded();
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let workspace = cx.new(|cx| {
            WorkspaceStore::new_attached(link, WorkspaceAttachment::Local, None, None, cx)
        });
        crate::store::tests::seed_full_scope(&workspace, &_incoming, deferred, cx);
        workspace.update(cx, |store, cx| {
            store.show_alone(id.clone(), cx);
            store.receive(
                EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionStatus {
                        session_id: id.clone(),
                    },
                    event: ServerEvent::SessionStatusReplaced(Box::new(status)),
                },
                cx,
            );
            store.receive(session_snapshot(&id, 3, vec![reply(4_000)]), cx);
        });
        cx.run_until_parked();
        assert_eq!(live_turn(&workspace, cx), Some(1_000));
        assert!(
            !std::iter::from_fn(|| outgoing.try_recv().ok())
                .map(|line| tcode_protocol::decode_client_line(&line).unwrap())
                .any(|request| matches!(
                    request.payload,
                    tcode_protocol::ClientPayload::Query(
                        tcode_protocol::Query::SessionHistoryPage { .. }
                    )
                )),
            "the turn's start comes with the status, not from earlier pages"
        );
    }

    fn held_window<'a>(store: &'a WorkspaceStore, session_id: &str) -> Option<&'a HeldHistory> {
        store.threads.get(session_id)?.history.as_ref()
    }

    fn held_history<'a>(store: &'a WorkspaceStore, session_id: &str) -> &'a HeldHistory {
        held_window(store, session_id).expect("a held window")
    }

    fn test_host(store: SessionStore) -> SpawnedHost {
        spawn_host(store, HostServices::default()).expect("spawn test host")
    }

    macro_rules! update_host {
        ($host:expr, $update:expr) => {
            smol::block_on($host.update_state_for_test($update)).expect("update test host")
        };
    }

    /// What a test with no shell does in place of its one pane.
    impl WorkspaceStore {
        /// The thread the pane shows, the one open.
        pub(crate) fn sole_thread(&self) -> Option<String> {
            let open = self.open_threads();
            assert!(open.len() < 2, "more than one thread is open: {open:?}");
            open.into_iter().next()
        }

        pub(crate) fn sole_status(&self, cx: &App) -> Option<&tcode_protocol::SessionStatus> {
            self.session_status_for(self.sole_thread().as_deref(), cx)
        }

        /// The pane shows `session_id` in place of what it showed.
        pub(crate) fn show_alone(&mut self, session_id: String, cx: &mut gpui::Context<Self>) {
            self.creation_request_generation = self.creation_request_generation.wrapping_add(1);
            self.draft_fallback_pending = false;
            self.remember_project_of(&session_id);
            for shown in self.open_threads() {
                if shown != session_id {
                    self.close_thread(&shown, cx);
                }
            }
            if !self.is_thread_open(&session_id) {
                self.selection_moved_on();
                self.open_thread(&session_id, cx);
            }
            cx.notify();
        }

        /// The pane shows nothing.
        pub(crate) fn leave_threads(&mut self, cx: &mut gpui::Context<Self>) {
            self.selection_moved_on();
            self.draft_fallback_pending = false;
            for shown in self.open_threads() {
                self.close_thread(&shown, cx);
            }
            cx.notify();
        }
    }

    /// What the shell does with a thread to show, for a test without one.
    pub(crate) fn place_shown_threads(
        store: &gpui::Entity<WorkspaceStore>,
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            cx.subscribe(store, |store, shown: &super::ShowThread, cx| {
                store.update(cx, |store, cx| {
                    store.show_alone(shown.session_id.clone(), cx)
                });
            })
            .detach();
        });
    }

    /// Wakes the test's thread from [`std::thread::park`]: a host answers from
    /// threads of its own, and a future polled with this goes on once it has.
    /// GPUI's test scheduler unparks the same thread when another thread wakes
    /// one of its tasks.
    struct Unpark(std::thread::Thread);

    impl std::task::Wake for Unpark {
        fn wake(self: std::sync::Arc<Self>) {
            self.0.unpark();
        }
    }

    /// A client of a real host whose replies the test's thread decodes, so
    /// what they wake runs on GPUI's test scheduler. Between turns it waits
    /// for the host as the app does: until the host sends something.
    pub(crate) struct LocalHostDriver {
        pub(crate) link: tcode_client::HostLink,
        /// Gone once the host has closed its side of the link.
        pump: Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>>,
        waker: std::task::Waker,
    }

    impl LocalHostDriver {
        pub(crate) fn new(host: &SpawnedHost) -> Self {
            let link = tcode_client::HostLink::new(host.to_host.clone(), host.from_host.clone());
            Self::from_link(link)
        }

        pub(crate) fn from_link(link: tcode_client::HostLink) -> Self {
            let pump_link = link.clone();
            Self {
                link,
                pump: Some(Box::pin(async move { pump_link.pump().await })),
                waker: std::sync::Arc::new(Unpark(std::thread::current())).into(),
            }
        }

        fn workspace(&mut self, cx: &mut TestAppContext) -> gpui::Entity<WorkspaceStore> {
            let workspace = cx.new(|cx| {
                WorkspaceStore::new_attached(
                    self.link.clone(),
                    WorkspaceAttachment::Local,
                    None,
                    None,
                    cx,
                )
            });
            place_shown_threads(&workspace, cx);
            self.wait_until(cx, &workspace, "workspace baseline", |cx| {
                workspace.read_with(cx, |store, _| store.baseline_ready())
            });
            workspace
        }

        pub(crate) fn await_with_events<T>(
            &mut self,
            cx: &mut TestAppContext,
            store: &gpui::Entity<WorkspaceStore>,
            future: impl std::future::Future<Output = T>,
        ) -> T {
            let mut future = std::pin::pin!(future);
            let waker = self.waker.clone();
            let mut answer = None;
            self.wait_until(cx, store, "host reply", |_| {
                let mut task_cx = std::task::Context::from_waker(&waker);
                if let std::task::Poll::Ready(value) =
                    std::future::Future::poll(future.as_mut(), &mut task_cx)
                {
                    answer = Some(value);
                    true
                } else {
                    false
                }
            });
            answer.expect("reply observed")
        }

        /// What `receiver` hears next, the host turned until it has: a
        /// scripted provider hears from the host's threads, not the link.
        pub(crate) fn recv<T>(
            &mut self,
            cx: &mut TestAppContext,
            store: &gpui::Entity<WorkspaceStore>,
            receiver: &async_channel::Receiver<T>,
        ) -> T {
            self.await_with_events(cx, store, receiver.recv())
                .expect("the sender is open")
        }

        /// Decodes what the host has sent, hands it to `store`, and runs what
        /// that woke.
        pub(crate) fn step(
            &mut self,
            cx: &mut TestAppContext,
            store: &gpui::Entity<WorkspaceStore>,
        ) {
            if let Some(pump) = &mut self.pump {
                let mut task_cx = std::task::Context::from_waker(&self.waker);
                if pump.as_mut().poll(&mut task_cx).is_ready() {
                    self.pump = None;
                }
            }
            store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            cx.run_until_parked();
        }

        /// Waits until the host sends something or another thread wakes a
        /// task of the test's. The test fails if the host has closed the link
        /// while it still waits for `what`.
        pub(crate) fn wait_for_host(&self, what: &str) {
            assert!(
                self.pump.is_some(),
                "the host closed the link while the test waited for {what}"
            );
            std::thread::park();
        }

        pub(crate) fn wait_until(
            &mut self,
            cx: &mut TestAppContext,
            store: &gpui::Entity<WorkspaceStore>,
            what: &str,
            mut ready: impl FnMut(&TestAppContext) -> bool,
        ) {
            loop {
                self.step(cx, store);
                if ready(cx) {
                    return;
                }
                self.wait_for_host(what);
            }
        }

        /// [`Self::wait_until`] for a window, drawn at rest after each turn.
        pub(crate) fn settle_until(
            &mut self,
            cx: &mut gpui::VisualTestContext,
            store: &gpui::Entity<WorkspaceStore>,
            what: &str,
            mut ready: impl FnMut(&mut gpui::VisualTestContext) -> bool,
        ) {
            loop {
                self.step(cx, store);
                crate::test_frames::draw_settled(cx);
                if ready(cx) {
                    return;
                }
                self.wait_for_host(what);
            }
        }

        fn command(
            &mut self,
            cx: &mut TestAppContext,
            store: &gpui::Entity<WorkspaceStore>,
            command: Command,
        ) {
            let link = self.link.clone();
            self.await_with_events(cx, store, link.command(command))
                .expect("typed host command");
        }

        pub(crate) fn shutdown(
            &mut self,
            cx: &mut TestAppContext,
            store: &gpui::Entity<WorkspaceStore>,
            host: &SpawnedHost,
        ) {
            let link = self.link.clone();
            self.await_with_events(cx, store, link.shutdown())
                .expect("stop host");
            host.stopped.recv_blocking().expect("host stopped");
        }
    }

    #[gpui::test]
    fn reconnect_restores_workspace_readiness_once_the_link_answers(cx: &mut TestAppContext) {
        use tcode_client::{ConnectionFailure, ConnectionState};
        let root = scratch_root("delayed-reconnect");
        let disk = SessionStore::open_at(root.clone()).unwrap();
        disk.upsert_project(&project_at("p", &root)).unwrap();
        disk.upsert_meta(&thread(&root, "one", "p", None)).unwrap();
        let host = test_host(disk);
        let mux =
            tcode_traverse::mux::HostMux::new(host.attachments.clone(), host.from_host.clone());
        let connection = mux.attach(
            tcode_protocol::Principal::Full,
            tcode_client::attachment::AttachmentPolicy::network(),
        );
        let (incoming, from_host) = async_channel::unbounded();
        let (outgoing, requests) = tcode_client::outgoing::channel();
        let link = tcode_client::HostLink::new(outgoing, from_host);
        let state_link = link.clone();
        let replies = async move {
            while let Ok(line) = connection.from_host.recv().await {
                if matches!(
                    state_link.connection_state(),
                    ConnectionState::Syncing { .. }
                ) {
                    state_link.set_connection_state(ConnectionState::Connected { path: None });
                }
                if incoming.send(line).await.is_err() {
                    break;
                }
            }
        };
        let mut driver = LocalHostDriver::from_link(link);
        let pump = driver.pump.take().unwrap();
        // A busy transport may not read until the whole recovery flight is queued.
        let forwards = async move {
            while let Ok(line) = requests.recv().await {
                if tcode_client::outgoing::subscription_key(&line).is_none() {
                    requests.sent(&line);
                }
                if connection.to_host.send(line).await.is_err() {
                    break;
                }
            }
        };
        driver.pump = Some(Box::pin(smol::future::race(
            pump,
            smol::future::race(forwards, replies),
        )));
        let workspace = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                WorkspaceAttachment::Remote {
                    host_id: "test".into(),
                    host_name: "test".into(),
                },
                None,
                None,
                cx,
            )
        });
        driver.wait_until(cx, &workspace, "initial workspace baseline", |cx| {
            workspace.read_with(cx, |store, _| store.baseline_ready())
        });
        workspace.update(cx, |store, cx| store.show_alone("one".into(), cx));
        driver.wait_until(cx, &workspace, "initial selected thread baseline", |cx| {
            workspace.read_with(cx, |store, _| store.baseline_ready())
        });
        driver
            .link
            .set_connection_state(ConnectionState::Reconnecting {
                attempt: 2,
                reason: Some(ConnectionFailure::Timeout),
            });
        cx.run_until_parked();
        assert!(!workspace.read_with(cx, |store, _| store.baseline_ready()));
        driver
            .link
            .set_connection_state(ConnectionState::Syncing { path: None });
        driver.wait_until(cx, &workspace, "the reconnect baseline", |cx| {
            workspace.read_with(cx, |store, _| store.baseline_ready())
        });
        let link = driver.link.clone();
        driver
            .await_with_events(cx, &workspace, link.command(Command::ShutdownAllAndFlush))
            .unwrap();
        driver.link.close();
        drop(driver);
        // A remote client closes its attachment; the local owner closes the host.
        host.to_host.close();
        host.from_host.close();
        host.stopped.recv_blocking().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    /// Two clients of one host show the same thread's preview: the first to
    /// show it renders it, the other says where it is, and Show here moves
    /// the page across, the first letting its page go.
    #[cfg(preview_backend)]
    #[gpui::test]
    fn show_here_moves_a_threads_preview_between_clients(
        desk_cx: &mut TestAppContext,
        laptop_cx: &mut TestAppContext,
    ) {
        use crate::store::{PreviewPlace, PreviewReleased};
        let root = scratch_root("preview-handover");
        let disk = SessionStore::open_at(root.clone()).unwrap();
        disk.upsert_project(&project_at("p", &root)).unwrap();
        disk.upsert_meta(&thread(&root, "one", "p", None)).unwrap();
        let host = test_host(disk);
        let mux =
            tcode_traverse::mux::HostMux::new(host.attachments.clone(), host.from_host.clone());
        let client = |name: &str, cx: &mut TestAppContext| {
            let connection = mux.attach(
                tcode_protocol::Principal::Full,
                tcode_client::attachment::AttachmentPolicy::network(),
            );
            let mut driver = LocalHostDriver::from_link(tcode_client::HostLink::new(
                connection.to_host,
                connection.from_host,
            ));
            cx.update(|cx| {
                crate::device::DevicePreferences::set_device_name(Some(name.into()), cx)
            });
            let store = driver.workspace(cx);
            store.update(cx, |store, cx| store.show_alone("one".into(), cx));
            (driver, store)
        };
        // Two devices, each its own app with its own name.
        let (mut desk_driver, desk) = client("Desk", desk_cx);
        let (mut laptop_driver, laptop) = client("Laptop", laptop_cx);
        let released = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let _released = desk_cx.update(|cx| {
            let released = released.clone();
            cx.subscribe(&desk, move |_, event: &PreviewReleased, _| {
                released.borrow_mut().push(event.session_id.clone())
            })
        });
        let place = |store: &gpui::Entity<WorkspaceStore>, cx: &TestAppContext| {
            store.read_with(cx, |store, cx| store.preview_place("one", cx))
        };
        let mut settle =
            |desk_cx: &mut TestAppContext,
             laptop_cx: &mut TestAppContext,
             what: &str,
             ready: &mut dyn FnMut(&mut TestAppContext, &mut TestAppContext) -> bool| {
                loop {
                    desk_driver.step(desk_cx, &desk);
                    laptop_driver.step(laptop_cx, &laptop);
                    if ready(desk_cx, laptop_cx) {
                        return;
                    }
                    // Both clients share one host, on this thread.
                    desk_driver.wait_for_host(what);
                }
            };

        // The desk shows the preview first, as its panel would.
        settle(desk_cx, laptop_cx, "the desk renders", &mut |desk_cx, _| {
            desk.update(desk_cx, |store, _| store.claim_preview("one"));
            place(&desk, desk_cx) == PreviewPlace::Here
        });
        let url = "http://localhost:5173/".to_string();
        desk.update(desk_cx, |store, cx| {
            store.set_preview_url("one", url.clone(), cx)
        });
        settle(
            desk_cx,
            laptop_cx,
            "the laptop sees where the page is",
            &mut |_, laptop_cx| {
                laptop.update(laptop_cx, |store, _| store.claim_preview("one"));
                place(&laptop, laptop_cx) == PreviewPlace::Elsewhere("Desk".into())
                    && laptop.read_with(laptop_cx, |store, _| store.preview_url("one"))
                        == Some(url.clone())
            },
        );
        assert!(released.borrow().is_empty());

        laptop.update(laptop_cx, |store, cx| store.show_preview_here("one", cx));
        assert_eq!(place(&laptop, laptop_cx), PreviewPlace::Here);
        settle(
            desk_cx,
            laptop_cx,
            "the desk lets its page go",
            &mut |desk_cx, _| {
                place(&desk, desk_cx) == PreviewPlace::Elsewhere("Laptop".into())
                    && !released.borrow().is_empty()
            },
        );
        assert_eq!(*released.borrow(), ["one"]);
        assert_eq!(place(&laptop, laptop_cx), PreviewPlace::Here);
        assert_eq!(
            laptop.read_with(laptop_cx, |store, _| store.preview_url("one")),
            Some(url),
            "the page reopens where it was"
        );

        let link = desk_driver.link.clone();
        desk_driver
            .await_with_events(desk_cx, &desk, link.command(Command::ShutdownAllAndFlush))
            .unwrap();
        desk_driver.link.close();
        laptop_driver.link.close();
        drop((desk_driver, laptop_driver));
        host.to_host.close();
        host.from_host.close();
        host.stopped.recv_blocking().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[gpui::test]
    fn reconnect_replays_current_index_fingerprint_and_retains_replicas_until_baselines_arrive(
        cx: &mut TestAppContext,
    ) {
        use tcode_client::ConnectionState;
        let root = scratch_root("baseline-replay");
        let disk = SessionStore::open_at(root.clone()).unwrap();
        disk.upsert_project(&project_at("p", &root)).unwrap();
        disk.upsert_meta(&thread(&root, "one", "p", None)).unwrap();
        let host = test_host(disk);
        let (to_host, requests) = async_channel::unbounded::<String>();
        let replayed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = replayed.clone();
        let host_sender = host.to_host.clone();
        let forwarder = std::thread::spawn(move || {
            while let Ok(line) = requests.recv_blocking() {
                recorded
                    .lock()
                    .unwrap()
                    .push(tcode_protocol::decode_client_line(&line).unwrap());
                if host_sender.send_blocking(line).is_err() {
                    break;
                }
            }
            host_sender.close();
        });
        let link = tcode_client::HostLink::new(to_host, host.from_host.clone());
        let mut driver = LocalHostDriver::from_link(link);
        let workspace = driver.workspace(cx);
        workspace.update(cx, |store, cx| store.show_alone("one".into(), cx));
        driver.wait_until(cx, &workspace, "selected thread baseline", |cx| {
            workspace.read_with(cx, |store, _| store.baseline_ready())
        });
        let link = driver.link.clone();
        driver
            .await_with_events(
                cx,
                &workspace,
                link.command(Command::RenameSession {
                    session_id: "one".into(),
                    title: "Updated live".into(),
                }),
            )
            .unwrap();
        driver.wait_until(cx, &workspace, "live index upsert", |cx| {
            workspace.read_with(cx, |store, _| {
                store.index_replica.0[0].title == "Updated live"
            })
        });
        let fingerprint =
            workspace.read_with(cx, |store, _| store.cached_index_fingerprint().unwrap());
        replayed.lock().unwrap().clear();
        workspace.update(cx, |store, cx| {
            let state = ConnectionState::Reconnecting {
                attempt: 2,
                reason: None,
            };
            store.host.set_connection_state(state.clone());
            store.apply_connection_state(state);
            store.receive(
                EventEnvelope {
                    request_id: None,
                    topic: Topic::Scope,
                    event: ServerEvent::ScopeSnapshot(tcode_protocol::Scope::Full),
                },
                cx,
            );
            assert!(!store.baseline_ready());
            let state = ConnectionState::Syncing { path: None };
            store.host.set_connection_state(state.clone());
            store.apply_connection_state(state);
            assert!(!store.threads_loading(), "cached list remains visible");
            assert!(
                !store.chat_loading_for(store.sole_thread().as_deref(), cx),
                "cached thread remains visible"
            );
            assert_eq!(store.sole_thread().as_deref(), Some("one"));
        });
        driver.wait_until(cx, &workspace, "correlated reconnect baselines", |cx| {
            workspace.read_with(cx, |store, _| store.connection_state().is_connected())
        });
        let subscriptions = replayed.lock().unwrap();
        let replay = subscriptions
            .iter()
            .find_map(|request| match &request.payload {
                tcode_protocol::ClientPayload::Subscribe(subscription)
                    if subscription.topic == Topic::Index =>
                {
                    Some(subscription)
                }
                _ => None,
            })
            .expect("replayed index subscription");
        assert_eq!(replay.known_index.as_ref(), Some(&fingerprint));
        drop(subscriptions);
        let reconnecting = ConnectionState::Reconnecting {
            attempt: 3,
            reason: None,
        };
        let syncing = ConnectionState::Syncing { path: None };
        driver.link.set_connection_state(reconnecting.clone());
        driver.link.set_connection_state(syncing.clone());
        let events = driver.link.events();
        let mut deferred_events = Vec::new();
        let scope = smol::block_on(async {
            let mut scope = None;
            let mut unchanged = false;
            loop {
                if unchanged && let Some(scope) = scope.take() {
                    break scope;
                }
                let mut next = std::pin::pin!(events.recv());
                let event = std::future::poll_fn(|task_cx| {
                    let _ = driver.pump.as_mut().unwrap().as_mut().poll(task_cx);
                    next.as_mut().poll(task_cx)
                })
                .await
                .unwrap();
                if matches!(event.event, ServerEvent::ScopeSnapshot(_)) {
                    scope = Some(event);
                } else {
                    if let ServerEvent::IndexUnchanged {
                        fingerprint: received,
                        ..
                    } = &event.event
                    {
                        assert_eq!(received, &fingerprint);
                        unchanged = true;
                    }
                    deferred_events.push(event);
                }
            }
        });
        assert!(matches!(scope.event, ServerEvent::ScopeSnapshot(_)));
        workspace.update(cx, |store, cx| {
            store.receive(scope, cx);
            assert!(
                !store.baseline_ready(),
                "Scope alone cannot make old baselines current"
            );
            store.apply_connection_state(reconnecting);
            store.apply_connection_state(syncing);
            assert!(
                !store.baseline_ready(),
                "other current baselines are still pending"
            );
            assert!(
                !store.chat_loading_for(store.sole_thread().as_deref(), cx),
                "cached thread remains visible"
            );
            for event in deferred_events {
                store.receive(event, cx);
            }
        });
        driver.wait_until(
            cx,
            &workspace,
            "baselines after delayed state notifications",
            |cx| workspace.read_with(cx, |store, _| store.connection_state().is_connected()),
        );
        workspace.read_with(cx, |store, _| {
            assert_eq!(
                store.cached_index_fingerprint().as_ref(),
                Some(&fingerprint)
            );
            assert_eq!(store.index_replica.0[0].title, "Updated live");
        });
        driver.shutdown(cx, &workspace, &host);
        driver.link.close();
        forwarder.join().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    /// A client that visits many threads keeps the replicas of the selected
    /// one and of the few it left last. Re-selecting a released thread asks
    /// for a baseline and holds exactly what a client opening it fresh holds;
    /// re-selecting a kept one sends its cursor and receives only what it
    /// missed. Records appended while a thread was away and live afterwards
    /// are each held once, and a thread deleted from the index is released at
    /// once.
    #[gpui::test]
    fn visiting_many_threads_keeps_the_replicas_of_the_last_few(cx: &mut TestAppContext) {
        let root = scratch_root("visited-threads");
        let disk = SessionStore::open_at(root.clone()).unwrap();
        let ids: Vec<String> = (0..50).map(|index| format!("thread-{index:02}")).collect();
        let answer = |id: &str, turn: usize| {
            AgentEvent::ItemCompleted(ThreadItem {
                id: format!("{id}-answer-{turn}"),
                parent_item_id: None,
                content: ItemContent::AssistantMessage {
                    text: format!("answer {turn} in {id}"),
                    summarized: false,
                },
            })
        };
        let turn = |id: &str, turn: usize| {
            [
                AgentEvent::TurnStarted {
                    turn_id: turn.to_string(),
                },
                answer(id, turn),
                AgentEvent::TurnCompleted {
                    turn_id: turn.to_string(),
                    status: TurnStatus::Completed,
                    usage: None,
                },
            ]
        };
        disk.upsert_project(&project_at("p", &root)).unwrap();
        for id in &ids {
            disk.upsert_meta(&thread(&root, id, "p", None)).unwrap();
            let appends: Vec<_> = (0..3)
                .flat_map(|index| turn(id, index))
                .enumerate()
                .map(|(ts, event)| {
                    tcode_services::store::Mutation::append_event(id, ts as u64 + 1, &event)
                        .unwrap()
                })
                .collect();
            disk.apply(&appends).unwrap();
        }
        let host = test_host(disk);
        let mut driver = LocalHostDriver::new(&host);
        let workspace = driver.workspace(cx);
        let open = |driver: &mut LocalHostDriver,
                    cx: &mut TestAppContext,
                    workspace: &gpui::Entity<WorkspaceStore>,
                    id: &str| {
            workspace.update(cx, |store, cx| store.show_alone(id.into(), cx));
            driver.wait_until(cx, workspace, id, |cx| {
                workspace.read_with(cx, |store, _| {
                    store.baseline_ready()
                        && !store.session_loading_for(&store.sole_thread().unwrap_or_default())
                        && !store
                            .thread(store.sole_thread().as_deref())
                            .is_some_and(|thread| thread.catching_up)
                })
            });
        };
        let events_cursor = |store: &WorkspaceStore| {
            store
                .host
                .subscriptions()
                .into_iter()
                .find(|subscription| matches!(subscription.topic, Topic::SessionEvents { .. }))
                .map(|subscription| subscription.after)
        };
        let held_count = |store: &WorkspaceStore, id: &str, event: &AgentEvent| {
            held_history(store, id)
                .records
                .iter()
                .filter(|record| record.event == *event)
                .count()
        };
        for id in &ids {
            open(&mut driver, cx, &workspace, id);
        }
        workspace.read_with(cx, |store, _| {
            let mut held: Vec<&String> = store.threads.keys().collect();
            held.sort();
            assert_eq!(held, ids[45..].iter().collect::<Vec<_>>());
        });

        // A kept thread continues from its cursor even past what a baseline
        // would carry: the baseline would begin hundreds of records later.
        let kept = &ids[46];
        let end = workspace.read_with(cx, |store, _| held_history(store, kept).end);
        let missed: Vec<AgentEvent> = (3..153).flat_map(|index| turn(kept, index)).collect();
        update_host!(&host, {
            let (kept, missed) = (kept.clone(), missed.clone());
            move |state, cx| {
                for (offset, event) in missed.iter().enumerate() {
                    state.record_event_for_replica_test(&kept, 100 + offset as u64, event, cx);
                }
            }
        });
        workspace.update(cx, |store, cx| {
            store.show_alone(kept.clone(), cx);
            assert_eq!(events_cursor(store), Some(Some(end)));
        });
        let total = end + missed.len() as u64;
        driver.wait_until(cx, &workspace, "the kept thread's continuation", |cx| {
            workspace.read_with(cx, |store, cx| {
                held_window(store, kept).is_some_and(|held| held.end == total)
                    && store.thread_timeline(Some(kept), cx).is_some()
            })
        });
        let after_reselect = answer(kept, 153);
        update_host!(&host, {
            let (kept, after_reselect) = (kept.clone(), after_reselect.clone());
            move |state, cx| state.record_event_for_replica_test(&kept, 1000, &after_reselect, cx)
        });
        driver.wait_until(cx, &workspace, "the kept thread's live record", |cx| {
            workspace.read_with(cx, |store, _| {
                held_window(store, kept).is_some_and(|held| held.end == total + 1)
            })
        });
        workspace.read_with(cx, |store, _| {
            let held = held_history(store, kept);
            assert_eq!(held.from, 0, "a continuation, not a baseline");
            for event in [answer(kept, 0), answer(kept, 152), after_reselect.clone()] {
                assert_eq!(held_count(store, kept, &event), 1);
            }
        });

        // A released thread opens as on a fresh client, with what was
        // appended while it was away and what arrives live once it is open.
        let released = &ids[3];
        let away = answer(released, 3);
        let live = answer(released, 4);
        update_host!(&host, {
            let (released, away) = (released.clone(), away.clone());
            move |state, cx| state.record_event_for_replica_test(&released, 100, &away, cx)
        });
        workspace.update(cx, |store, cx| {
            store.show_alone(released.clone(), cx);
            assert_eq!(events_cursor(store), Some(None));
        });
        driver.wait_until(cx, &workspace, "the released thread's baseline", |cx| {
            workspace.read_with(cx, |store, _| {
                store.baseline_ready()
                    && !store.session_loading_for(&store.sole_thread().unwrap_or_default())
            })
        });
        update_host!(&host, {
            let (released, live) = (released.clone(), live.clone());
            move |state, cx| state.record_event_for_replica_test(&released, 101, &live, cx)
        });
        driver.wait_until(cx, &workspace, "the live record", |cx| {
            workspace.read_with(cx, |store, _| {
                held_window(store, released).is_some_and(|held| held.end == 11)
            })
        });
        let fresh = driver.workspace(cx);
        open(&mut driver, cx, &fresh, released);
        let fresh = fresh.read_with(cx, |store, cx| {
            (
                store.thread_timeline(Some(released), cx).cloned(),
                store
                    .thread(store.sole_thread().as_deref())
                    .map_or(0, |thread| thread.turn_offset),
            )
        });
        workspace.read_with(cx, |store, cx| {
            assert_eq!(
                (
                    store.thread_timeline(Some(released), cx).cloned(),
                    store
                        .thread(store.sole_thread().as_deref())
                        .map_or(0, |thread| thread.turn_offset)
                ),
                fresh
            );
            for event in [&away, &live] {
                assert_eq!(held_count(store, released, event), 1);
            }
        });

        let deleted = ids[47].clone();
        assert!(workspace.read_with(cx, |store, _| store.threads.contains_key(&deleted)));
        driver.command(
            cx,
            &workspace,
            Command::DeleteSession {
                session_id: deleted.clone(),
                remove_worktree: false,
            },
        );
        driver.wait_until(cx, &workspace, "the deletion", |cx| {
            workspace.read_with(cx, |store, _| {
                !store.index_replica.0.iter().any(|meta| meta.id == deleted)
            })
        });
        assert!(!workspace.read_with(cx, |store, _| store.threads.contains_key(&deleted)));

        driver.shutdown(cx, &workspace, &host);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A conversation left with its drawer open but whose shell has exited
    /// comes back with the drawer closed, as the host has it, although this
    /// client last showed it open.
    #[gpui::test]
    fn a_conversation_whose_terminal_exited_comes_back_with_its_drawer_closed(
        cx: &mut TestAppContext,
    ) {
        let root = scratch_root("terminal-drawer-reload");
        let disk = SessionStore::open_at(root.clone()).expect("open test store");
        disk.upsert_project(&project_at("p", &root))
            .expect("persist project");
        for meta in [
            thread(&root, "one", "p", None),
            thread(&root, "two", "p", None),
        ] {
            disk.upsert_meta(&meta).expect("persist session");
        }
        let host = test_host(disk);
        let mut driver = LocalHostDriver::new(&host);
        let workspace = driver.workspace(cx);
        let terminals = |cx: &TestAppContext| {
            workspace.read_with(cx, |store, cx| {
                store
                    .session_status_for(Some("one"), cx)
                    .map(|status| status.terminals.clone())
                    .unwrap_or_default()
            })
        };
        let drawer_open = |cx: &TestAppContext| {
            workspace.read_with(cx, |store, _| {
                store
                    .panel_state_for(store.sole_thread().as_deref())
                    .terminal_open
            })
        };

        workspace.update(cx, |store, cx| store.show_alone("one".into(), cx));
        driver.wait_until(cx, &workspace, "one on screen", |cx| {
            selected_status(cx, &workspace, "one")
        });
        workspace.update(cx, |store, cx| {
            store.toggle_terminal_panel_for(store.sole_thread().unwrap_or_default(), cx)
        });
        driver.wait_until(cx, &workspace, "a terminal", |cx| !terminals(cx).is_empty());
        let terminal_id = terminals(cx)[0].id;
        driver.command(
            cx,
            &workspace,
            Command::TerminalInput {
                terminal_id,
                bytes: b"exit\r".to_vec(),
            },
        );
        driver.wait_until(cx, &workspace, "the shell to exit", |cx| {
            terminals(cx)[0].exited
        });
        assert!(drawer_open(cx), "the drawer stays while its output is read");

        workspace.update(cx, |store, cx| store.show_alone("two".into(), cx));
        driver.wait_until(cx, &workspace, "two on screen", |cx| {
            selected_status(cx, &workspace, "two")
        });
        workspace.update(cx, |store, cx| store.show_alone("one".into(), cx));
        let loaded = Topic::SessionStatus {
            session_id: "one".into(),
        };
        driver.wait_until(cx, &workspace, "one loaded again", |cx| {
            selected_status(cx, &workspace, "one")
                && workspace.read_with(cx, |store, _| store.baseline_topics.contains(&loaded))
        });
        assert!(
            !drawer_open(cx),
            "the drawer opened on a conversation with no shell"
        );
        assert!(terminals(cx).is_empty());

        driver.shutdown(cx, &workspace, &host);
        let _ = std::fs::remove_dir_all(&root);
    }

    pub(crate) fn scratch_root(label: &str) -> std::path::PathBuf {
        static NEXT_ROOT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "tcode-{label}-{}-{}-{}",
            std::process::id(),
            tcode_services::store::now_millis(),
            NEXT_ROOT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    pub(crate) fn project_at(id: &str, root: &std::path::Path) -> Project {
        Project {
            id: id.into(),
            name: id.into(),
            root: root.to_path_buf(),
            quick_chat: false,
            icon_path: None,
            permission_defaults: Default::default(),
            created_at: 0,
        }
    }

    pub(crate) fn thread(
        root: &std::path::Path,
        id: &str,
        project: &str,
        parent: Option<&str>,
    ) -> SessionMeta {
        let mut meta = SessionMeta::new(ProviderKind::Codex, root.to_path_buf(), None);
        meta.id = id.into();
        meta.project_id = Some(project.into());
        meta.parent_session_id = parent.map(str::to_string);
        meta
    }

    fn selected_status(
        cx: &TestAppContext,
        workspace: &gpui::Entity<WorkspaceStore>,
        session_id: &str,
    ) -> bool {
        workspace.read_with(cx, |store, cx| {
            store
                .sole_status(cx)
                .is_some_and(|status| status.session_id == session_id)
        })
    }

    /// Leave user state in whatever project draft is on screen, so a later
    /// assertion can tell a cleared entry from a freshly defaulted one.
    fn mark_draft_state(cx: &mut TestAppContext, workspace: &gpui::Entity<WorkspaceStore>) {
        workspace.update(cx, |store, _| {
            let draft = store
                .conversation_ui
                .keys()
                .find(|destination| matches!(destination, ConversationDestination::ProjectDraft(_)))
                .cloned()
                .expect("draft conversation state");
            store
                .conversation_ui
                .get_mut(&draft)
                .expect("draft conversation state")
                .right_panel_open = true;
        });
    }

    fn assert_no_draft_state(cx: &mut TestAppContext, workspace: &gpui::Entity<WorkspaceStore>) {
        workspace.read_with(cx, |store, _| {
            let stranded: Vec<_> = store
                .conversation_ui
                .iter()
                .filter(|(destination, ui)| {
                    matches!(destination, ConversationDestination::ProjectDraft(_))
                        && ui.right_panel_open
                })
                .map(|(destination, _)| destination)
                .collect();
            assert!(
                stranded.is_empty(),
                "the deleted project's draft state outlived it: {stranded:?}"
            );
        });
    }

    /// Archived threads leave the index; the host still has them.
    pub(crate) fn archived(
        cx: &TestAppContext,
        workspace: &gpui::Entity<WorkspaceStore>,
        id: &str,
    ) -> bool {
        workspace.read_with(cx, |store, _| {
            !store.index_replica.0.iter().any(|meta| meta.id == id)
                && store.index_summary.archived_counts.values().sum::<usize>() > 0
        })
    }

    /// Deleting a thread deletes every thread under it, archived ones too,
    /// though the index carries no archived thread: the confirmation counts
    /// them all.
    #[gpui::test]
    fn delete_confirmation_counts_archived_descendants(cx: &mut TestAppContext) {
        let (to_host, requests) = async_channel::unbounded();
        let (replies, from_host) = async_channel::unbounded();
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(link.clone(), WorkspaceAttachment::Local, None, None, cx)
        });
        crate::store::tests::seed_full_scope(&store, &replies, deferred, cx);
        let mut pump = std::pin::pin!(link.pump());
        let mut task_cx = std::task::Context::from_waker(std::task::Waker::noop());
        let root = std::path::Path::new("/project");
        let archived = |id: &str, parent: &str| {
            let mut meta = thread(root, id, "p", Some(parent));
            meta.archived_at = Some(1);
            meta
        };
        store.update(cx, |store, cx| {
            store.receive(
                EventEnvelope {
                    request_id: None,
                    topic: Topic::Index,
                    event: ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                        summary: Default::default(),
                        sessions: vec![
                            thread(root, "parent", "p", None),
                            thread(root, "other", "p", None),
                        ],
                        projects: Vec::new(),
                    }),
                },
                cx,
            );
        });

        let count = std::rc::Rc::new(std::cell::Cell::new(None));
        let result = count.clone();
        store.update(cx, |store, cx| {
            let task = store.fetch_deletion_count("parent", cx);
            cx.spawn(async move |_, _| result.set(Some(task.await)))
                .detach();
        });
        cx.run_until_parked();
        let query = std::iter::from_fn(|| requests.try_recv().ok())
            .map(|line| tcode_protocol::decode_client_line(&line).unwrap())
            .find(|message| {
                message.payload
                    == tcode_protocol::ClientPayload::Query(tcode_protocol::Query::ArchivedSessions)
            })
            .expect("the count fetches the archived threads");
        replies
            .try_send(
                tcode_protocol::encode_line(&tcode_protocol::HostMessage::QueryResult {
                    id: query.id,
                    result: Ok(tcode_protocol::QueryResponse::ArchivedSessions(
                        tcode_protocol::ArchivedSessions {
                            sessions: vec![
                                archived("child", "parent"),
                                archived("grandchild", "child"),
                            ],
                            worktree_shared: Default::default(),
                            revision: 0,
                        },
                    )),
                })
                .unwrap(),
            )
            .unwrap();
        assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
        cx.run_until_parked();

        assert_eq!(count.get(), Some(3));
        link.close();
    }

    /// Delete all deletes each archived thread's tree: the confirmation counts
    /// the unarchived threads under them, and a thread under another archived
    /// thread is neither counted nor deleted twice.
    #[gpui::test]
    fn delete_all_archived_counts_unarchived_descendants_once(cx: &mut TestAppContext) {
        let root = std::path::Path::new("/project");
        let archived = |id: &str, parent: Option<&str>| {
            let mut meta = thread(root, id, "p", parent);
            meta.archived_at = Some(1);
            meta
        };
        let delete_all = |cx: &mut TestAppContext, index, archived| {
            let (to_host, requests) = async_channel::unbounded();
            let (replies, from_host) = async_channel::unbounded();
            let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
            let link = tcode_client::HostLink::new(to_host, from_host);
            let store = cx.new(|cx| {
                WorkspaceStore::new_attached(
                    link.clone(),
                    WorkspaceAttachment::Local,
                    None,
                    None,
                    cx,
                )
            });
            crate::store::tests::seed_full_scope(&store, &replies, deferred, cx);
            let mut pump = std::pin::pin!(link.pump());
            let mut task_cx = std::task::Context::from_waker(std::task::Waker::noop());
            // A thread on screen, so the empty-workspace draft does not open.
            store.update(cx, |store, cx| {
                store.show_alone("other".into(), cx);
                store.receive(
                    EventEnvelope {
                        request_id: None,
                        topic: Topic::Index,
                        event: ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                            summary: Default::default(),
                            sessions: index,
                            projects: vec![project_at("p", root)],
                        }),
                    },
                    cx,
                );
                store.hold_archived_sessions(cx);
            });
            cx.run_until_parked();
            let query = std::iter::from_fn(|| requests.try_recv().ok())
                .map(|line| tcode_protocol::decode_client_line(&line).unwrap())
                .find(|message| {
                    message.payload
                        == tcode_protocol::ClientPayload::Query(
                            tcode_protocol::Query::ArchivedSessions,
                        )
                })
                .expect("the Archived page fetches the archived threads");
            replies
                .try_send(
                    tcode_protocol::encode_line(&tcode_protocol::HostMessage::QueryResult {
                        id: query.id,
                        result: Ok(tcode_protocol::QueryResponse::ArchivedSessions(
                            tcode_protocol::ArchivedSessions {
                                sessions: archived,
                                worktree_shared: Default::default(),
                                revision: 0,
                            },
                        )),
                    })
                    .unwrap(),
                )
                .unwrap();
            assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
            cx.run_until_parked();
            let deletion = store
                .read_with(cx, |store, _| store.archived_deletion())
                .expect("the archived threads are held");
            let counts = (deletion.archived, deletion.unarchived);
            store.update(cx, |store, _| store.delete_archived(deletion));
            cx.run_until_parked();
            let deleted: Vec<String> = std::iter::from_fn(|| requests.try_recv().ok())
                .filter_map(|line| {
                    match tcode_protocol::decode_client_line(&line).unwrap().payload {
                        tcode_protocol::ClientPayload::Command(Command::DeleteSession {
                            session_id,
                            ..
                        }) => Some(session_id),
                        _ => None,
                    }
                })
                .collect();
            link.close();
            (counts, deleted)
        };

        let (counts, deleted) = delete_all(
            cx,
            vec![
                thread(root, "child", "p", Some("parent")),
                thread(root, "other", "p", None),
            ],
            vec![archived("parent", None)],
        );
        assert_eq!((counts, deleted), ((1, 1), vec!["parent".to_string()]));
        let (counts, deleted) = delete_all(
            cx,
            vec![thread(root, "other", "p", None)],
            vec![archived("child", Some("parent")), archived("parent", None)],
        );
        assert_eq!((counts, deleted), ((2, 0), vec!["parent".to_string()]));
    }

    /// An Orchestrate child archived while on screen hands the workspace to
    /// its parent, keeping the parent's client-side records; archiving a thread
    /// the user is not viewing must not move them at all.
    #[gpui::test]
    fn archiving_the_viewed_child_returns_to_its_parent(cx: &mut TestAppContext) {
        let root = scratch_root("archive-to-parent");
        let disk = SessionStore::open_at(root.clone()).expect("open test store");
        disk.upsert_project(&project_at("p", &root))
            .expect("persist project");
        for meta in [
            thread(&root, "parent", "p", None),
            thread(&root, "child", "p", Some("parent")),
            thread(&root, "sibling", "p", None),
        ] {
            disk.upsert_meta(&meta).expect("persist session");
        }
        let host = test_host(disk);
        let mut driver = LocalHostDriver::new(&host);
        let workspace = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                WorkspaceAttachment::Local,
                None,
                None,
                cx,
            )
        });

        workspace.update(cx, |store, cx| store.show_alone("parent".into(), cx));
        driver.wait_until(cx, &workspace, "parent on screen", |cx| {
            selected_status(cx, &workspace, "parent")
                && workspace.read_with(cx, |store, _| held_window(store, "parent").is_some())
        });
        let parent_cursor =
            workspace.read_with(cx, |store, _| Some(held_history(store, "parent").end));
        workspace.update(cx, |store, cx| store.show_alone("child".into(), cx));
        driver.wait_until(cx, &workspace, "child selected", |cx| {
            selected_status(cx, &workspace, "child")
        });

        driver.command(
            cx,
            &workspace,
            Command::ArchiveSession {
                session_id: "child".into(),
            },
        );
        driver.wait_until(cx, &workspace, "child closed", |cx| {
            workspace.read_with(cx, |store, _| !store.is_thread_open("child"))
        });
        place_shown_threads(&workspace, cx);
        workspace.update(cx, |store, cx| assert!(store.show_after_left("child", cx)));
        driver.wait_until(cx, &workspace, "parent reopened", |cx| {
            selected_status(cx, &workspace, "parent")
        });
        workspace.read_with(cx, |store, _| {
            assert!(
                held_window(store, "parent").is_some(),
                "the parent's replicated records were dropped on the way back"
            );
            let events_cursor = store
                .host
                .subscriptions()
                .into_iter()
                .find(|subscription| matches!(subscription.topic, Topic::SessionEvents { .. }))
                .map(|subscription| subscription.after);
            assert_eq!(
                events_cursor,
                Some(parent_cursor),
                "the parent resumes from its cursor, not from nothing"
            );
        });
        // The index and its summary replicate on their own topics: the
        // return to the parent follows the index, the archived count the
        // summary, and nothing orders one before the other.
        driver.wait_until(cx, &workspace, "child archived", |cx| {
            archived(cx, &workspace, "child")
        });

        driver.command(
            cx,
            &workspace,
            Command::ArchiveSession {
                session_id: "sibling".into(),
            },
        );
        driver.wait_until(cx, &workspace, "sibling archived", |cx| {
            archived(cx, &workspace, "sibling")
        });
        assert!(
            selected_status(cx, &workspace, "parent"),
            "archiving a background thread moved the user"
        );

        driver.shutdown(cx, &workspace, &host);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A pane split beside a thread asks for a draft in the thread's project,
    /// and the thread split from stays open for the pane still showing it.
    #[gpui::test]
    fn a_draft_beside_an_open_thread_leaves_it_open(cx: &mut TestAppContext) {
        let root = scratch_root("draft-beside");
        let disk = SessionStore::open_at(root.clone()).expect("open test store");
        disk.upsert_project(&project_at("p", &root))
            .expect("persist project");
        disk.upsert_meta(&thread(&root, "first", "p", None))
            .expect("persist session");
        let host = test_host(disk);
        let mut driver = LocalHostDriver::new(&host);
        let workspace = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                WorkspaceAttachment::Local,
                None,
                None,
                cx,
            )
        });
        workspace.update(cx, |store, cx| store.show_alone("first".into(), cx));
        driver.wait_until(cx, &workspace, "first selected", |cx| {
            selected_status(cx, &workspace, "first")
        });

        let shown = std::rc::Rc::new(std::cell::RefCell::new(None));
        let _shown = cx.update(|cx| {
            let shown = shown.clone();
            cx.subscribe(&workspace, move |_, event: &super::ShowThread, _| {
                *shown.borrow_mut() = Some(event.session_id.clone());
            })
        });
        workspace.update(cx, |store, cx| {
            store.start_draft_beside(Some("first"), cx);
        });
        driver.wait_until(cx, &workspace, "a draft to show", |_| {
            shown.borrow().is_some()
        });
        let draft = shown.borrow().clone().expect("the draft");
        workspace.update(cx, |store, cx| store.open_thread(&draft, cx));
        driver.wait_until(cx, &workspace, "a draft in the thread's project", |cx| {
            workspace.read_with(cx, |store, cx| {
                store
                    .session_status_for(Some(&draft), cx)
                    .is_some_and(|status| status.draft && status.project_id.as_deref() == Some("p"))
            })
        });
        workspace.read_with(cx, |store, _| {
            assert!(store.is_thread_open("first"));
            assert!(
                store
                    .host
                    .subscribed_topics()
                    .contains(&Topic::SessionEvents {
                        session_id: "first".into()
                    }),
                "the thread split from still receives its events"
            );
        });

        driver.shutdown(cx, &workspace, &host);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Threads opened before the baseline, one per pane on a restored window
    /// or a scope's first answer, are all subscribed by it.
    #[gpui::test]
    fn a_baseline_subscribes_every_open_thread(cx: &mut TestAppContext) {
        let root = scratch_root("baseline-every-thread");
        let disk = SessionStore::open_at(root.clone()).expect("open test store");
        disk.upsert_project(&project_at("p", &root))
            .expect("persist project");
        for id in ["one", "two"] {
            disk.upsert_meta(&thread(&root, id, "p", None))
                .expect("persist session");
        }
        let host = test_host(disk);
        let mut driver = LocalHostDriver::new(&host);
        let workspace = cx.new(|cx| {
            let mut store = WorkspaceStore::new_attached(
                driver.link.clone(),
                WorkspaceAttachment::Local,
                None,
                None,
                cx,
            );
            store.open_thread("one", cx);
            store.open_thread("two", cx);
            store
        });
        driver.wait_until(cx, &workspace, "both threads' baselines", |cx| {
            workspace.read_with(cx, |store, _| store.baseline_ready())
        });
        workspace.read_with(cx, |store, cx| {
            assert!(store.session_status_for(Some("one"), cx).is_some());
            assert!(store.session_status_for(Some("two"), cx).is_some());
        });
        driver.shutdown(cx, &workspace, &host);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[gpui::test]
    fn reconnect_preserves_exactly_one_copy_of_each_record(cx: &mut TestAppContext) {
        let root = scratch_root("p4a-reconnect");
        let disk = SessionStore::open_at(root.clone()).unwrap();
        let mut meta = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
        meta.id = "reconnect".into();
        disk.upsert_meta(&meta).unwrap();
        let host = test_host(disk);
        let mut driver = LocalHostDriver::new(&host);
        let workspace = driver.workspace(cx);
        workspace.update(cx, |store, cx| store.show_alone("reconnect".into(), cx));
        driver.wait_until(cx, &workspace, "selected status", |cx| {
            workspace.read_with(cx, |store, cx| store.sole_status(cx).is_some())
        });
        update_host!(&host, |state, cx| {
            for (ts, text) in [(1, "one"), (2, "two"), (3, "three")] {
                state.record_event_for_replica_test(
                    "reconnect",
                    ts,
                    &AgentEvent::Warning {
                        message: text.into(),
                    },
                    cx,
                );
            }
        });
        driver.wait_until(cx, &workspace, "three records", |cx| {
            workspace.read_with(cx, |store, _| {
                held_window(store, "reconnect").is_some_and(|held| held.records.len() == 3)
            })
        });
        driver
            .link
            .set_connection_state(tcode_client::ConnectionState::Reconnecting {
                attempt: 1,
                reason: None,
            });
        driver
            .link
            .set_connection_state(tcode_client::ConnectionState::Connected { path: None });
        driver.command(cx, &workspace, Command::ClearRelaunchMarker);
        workspace.update(cx, |store, cx| {
            store.drain_host_events_for_test(cx);
            assert_eq!(held_history(store, "reconnect").records.len(), 3);
        });
        workspace.read_with(cx, |store, _| {
            assert_eq!(
                held_history(store, "reconnect")
                    .records
                    .iter()
                    .map(|record| record.ts)
                    .collect::<Vec<_>>(),
                vec![Some(1), Some(2), Some(3)]
            );
            let subscription = store
                .host
                .subscriptions()
                .into_iter()
                .find(|sub| matches!(sub.topic, Topic::SessionEvents { .. }))
                .unwrap();
            assert_eq!(subscription.after, Some(3));
        });
        driver.shutdown(cx, &workspace, &host);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[gpui::test]
    fn index_and_settings_replicas_follow_representative_commands(cx: &mut TestAppContext) {
        let root = scratch_root("tcode-replica-consistency-test");
        let session_store = SessionStore::open_at(root.clone()).expect("open test store");
        let seed_project = Project::from_root(root.join("seed"));
        let mut seed_session =
            SessionMeta::new(ProviderKind::Codex, seed_project.root.clone(), None);
        seed_session.project_id = Some(seed_project.id.clone());
        let seed_session_id = seed_session.id.clone();
        session_store
            .upsert_project(&seed_project)
            .expect("persist seed project");
        session_store
            .upsert_meta(&seed_session)
            .expect("persist seed session");
        let host = test_host(session_store);
        let mut driver = LocalHostDriver::new(&host);
        let workspace = driver.workspace(cx);
        driver.wait_until(cx, &workspace, "initial session index", |cx| {
            workspace.read_with(cx, |store, _| {
                store
                    .index_replica
                    .0
                    .iter()
                    .any(|meta| meta.id == seed_session_id)
            })
        });
        workspace.read_with(cx, |store, _| {
            assert!(
                store
                    .grouped_sessions()
                    .iter()
                    .any(|group| { group.sessions.iter().any(|meta| meta.id == seed_session_id) })
            );
            assert!(
                store
                    .flat_sessions()
                    .iter()
                    .any(|meta| meta.id == seed_session_id)
            );
            assert!(store.archived_groups().is_empty());
        });

        // The host validates a project root against its own filesystem, so this
        // directory has to exist before it will accept it.
        let created_root = root.join("created");
        std::fs::create_dir_all(&created_root).unwrap();
        driver.command(
            cx,
            &workspace,
            Command::CreateProject { root: created_root },
        );
        driver.command(
            cx,
            &workspace,
            Command::ArchiveSession {
                session_id: seed_session_id.clone(),
            },
        );
        let mut settings = workspace.read_with(cx, |store, _cx| store.settings());
        settings.word_wrap_diffs = !settings.word_wrap_diffs;
        let expected_word_wrap = settings.word_wrap_diffs;
        driver.command(
            cx,
            &workspace,
            Command::PatchSettings {
                patch: tcode_protocol::SettingsPatch::WordWrapDiffs(settings.word_wrap_diffs),
            },
        );
        driver.wait_until(cx, &workspace, "index and settings replicas", |cx| {
            workspace.read_with(cx, |store, _| {
                store.index_replica.1.len() == 2
                    && !store
                        .index_replica
                        .0
                        .iter()
                        .any(|meta| meta.id == seed_session_id)
                    && store.settings_replica.word_wrap_diffs == expected_word_wrap
            })
        });
        let link = driver.link.clone();
        let tcode_protocol::QueryResponse::ArchivedSessions(archived) = driver
            .await_with_events(
                cx,
                &workspace,
                link.query(tcode_protocol::Query::ArchivedSessions),
            )
            .expect("archived threads")
        else {
            panic!("archived threads")
        };
        assert!(
            archived
                .sessions
                .iter()
                .any(|meta| meta.id == seed_session_id)
        );

        workspace.read_with(cx, |store, _| {
            assert!(
                !store
                    .grouped_sessions()
                    .iter()
                    .any(|group| { group.sessions.iter().any(|meta| meta.id == seed_session_id) })
            );
            assert!(
                !store
                    .flat_sessions()
                    .iter()
                    .any(|meta| meta.id == seed_session_id)
            );
            assert_eq!(
                store
                    .project_summary(&seed_project.id)
                    .map(|(_, count)| count),
                Some(1),
                "the archived thread still counts toward its project"
            );
        });

        // Credential discovery can update settings after the command reply.
        // Let the UI consume those events before comparing the live replicas.
        driver.wait_until(
            cx,
            &workspace,
            "index and settings equal to live state",
            |cx| {
                let (live_index, live_settings) = update_host!(&host, |state, _| {
                    let index = state.index_snapshot();
                    (
                        (
                            serde_json::to_value(&index.sessions).unwrap(),
                            serde_json::to_value(&index.projects).unwrap(),
                        ),
                        serde_json::to_value(&state.settings).unwrap(),
                    )
                });
                workspace.read_with(cx, |store, _| {
                    serde_json::to_value(&store.index_replica.0).unwrap() == live_index.0
                        && serde_json::to_value(&store.index_replica.1).unwrap() == live_index.1
                        && serde_json::to_value(&store.settings_replica).unwrap() == live_settings
                })
            },
        );

        driver.shutdown(cx, &workspace, &host);
        std::fs::remove_dir_all(root).expect("remove test data");
    }

    #[gpui::test]
    fn session_status_replica_matches_live_after_queue_change(cx: &mut TestAppContext) {
        let root = scratch_root("tcode-session-status-replica-consistency-test");
        let session_store = SessionStore::open_at(root.clone()).expect("open test store");
        let meta = SessionMeta::new(ProviderKind::Codex, root.join("worktree"), None);
        let session_id = meta.id.clone();
        session_store.upsert_meta(&meta).expect("persist session");

        let host = test_host(session_store);
        let mut driver = LocalHostDriver::new(&host);
        let workspace = driver.workspace(cx);
        workspace.update(cx, |store, cx| store.show_alone(session_id.clone(), cx));
        driver.wait_until(cx, &workspace, "selected session status", |cx| {
            workspace.read_with(cx, |store, cx| {
                store
                    .sole_status(cx)
                    .is_some_and(|status| status.session_id == session_id)
            })
        });
        let scripted = tcode_runtime::app::scripted_provider(ProviderKind::Codex);
        update_host!(&host, move |state, _| state
            .set_provider_launcher_for_test(scripted.launcher));
        driver.command(
            cx,
            &workspace,
            Command::SendTurn {
                session_id: session_id.clone(),
                text: "queued for replication".into(),
                attachment_paths: Vec::new(),
            },
        );
        driver.command(
            cx,
            &workspace,
            Command::AddReviewComment {
                session_id: session_id.clone(),
                comment: ReviewComment::new(
                    "src/lib.rs".into(),
                    4,
                    4,
                    ReviewSide::New,
                    "Replicated review draft".into(),
                    "+changed".into(),
                    "turn:0".into(),
                    "Turn 1".into(),
                    0,
                    1,
                ),
            },
        );
        // The scripted provider keeps moving the host (delivery, steering
        // support) after the queue change, so the replica is compared with the
        // live status of the same moment rather than a later snapshot.
        driver.wait_until(cx, &workspace, "replica equal to the live status", |cx| {
            let live_id = session_id.clone();
            let live = update_host!(&host, move |state, _| {
                state
                    .session_status_snapshot(&live_id)
                    .expect("live session status")
            });
            workspace.read_with(cx, |store, cx| {
                store.sole_status(cx).is_some_and(|status| {
                    status.queued_messages.len() == 1
                        && status.review_comment_drafts.len() == 1
                        && *status == live
                })
            })
        });
        let replica = workspace.read_with(cx, |store, cx| {
            store
                .sole_status(cx)
                .cloned()
                .expect("session status replica")
        });

        assert_eq!(replica.queued_messages.len(), 1);
        assert_eq!(replica.queued_messages[0].text, "queued for replication");
        assert_eq!(replica.review_comment_drafts.len(), 1);
        assert_eq!(
            workspace.read_with(cx, |store, cx| store
                .review_comments_for(store.sole_thread().as_deref(), cx)),
            replica.review_comment_drafts
        );

        driver.shutdown(cx, &workspace, &host);
        std::fs::remove_dir_all(root).expect("remove test data");
    }

    #[gpui::test]
    fn rewind_prefills_reach_only_their_client_and_stay_keyed_to_parked_sessions(
        cx: &mut TestAppContext,
    ) {
        let root = scratch_root("tcode-native-rewind-replica-test");
        let session_store = SessionStore::open_at(root.clone()).expect("open test store");
        let first = SessionMeta::new(ProviderKind::ClaudeCode, root.join("first"), None);
        let second = SessionMeta::new(ProviderKind::ClaudeCode, root.join("second"), None);
        session_store
            .upsert_meta(&first)
            .expect("persist first session");
        session_store
            .upsert_meta(&second)
            .expect("persist second session");

        let host = test_host(session_store);
        let mut driver = LocalHostDriver::new(&host);
        let workspace = driver.workspace(cx);
        workspace.update(cx, |store, cx| store.show_alone(first.id.clone(), cx));
        driver.wait_until(cx, &workspace, "first selected session", |cx| {
            workspace.read_with(cx, |store, cx| {
                store
                    .sole_status(cx)
                    .is_some_and(|status| status.session_id == first.id)
            })
        });

        // This replica test deliberately owns both status subscriptions. Ordinary
        // navigation owns only its selected session; mux isolation is tested separately.
        driver
            .link
            .subscribe(tcode_protocol::Subscription {
                known_index: None,
                topic: Topic::SessionStatus {
                    session_id: second.id.clone(),
                },
                after: None,
            })
            .unwrap();
        let own = workspace.read_with(cx, |store, _| store.preview.client_id().to_owned());
        for (session_id, client_id, text) in [
            (
                first.id.clone(),
                own.clone(),
                "first parked prefill".to_string(),
            ),
            (second.id.clone(), own, "second parked prefill".to_string()),
            (
                first.id.clone(),
                "another-client".to_string(),
                "asked elsewhere".to_string(),
            ),
        ] {
            update_host!(&host, move |_state, cx| {
                cx.emit(HostEvent::Domain(EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionStatus {
                        session_id: session_id.clone(),
                    },
                    event: ServerEvent::RewindPrefill {
                        session_id,
                        client_id,
                        text,
                        attachments: Vec::new(),
                    },
                }));
            });
        }
        driver.wait_until(cx, &workspace, "both rewind prefill events", |cx| {
            workspace.read_with(cx, |store, _| store.native_rewind_prefills.len() == 2)
        });

        assert_eq!(
            workspace.update(cx, |store, cx| store
                .take_native_rewind_prefill_for(store.sole_thread().as_deref(), cx)),
            Some(crate::store::RewindPrefill {
                text: "first parked prefill".into(),
                attachments: Vec::new(),
            })
        );
        workspace.update(cx, |store, cx| store.show_alone(second.id.clone(), cx));
        driver.wait_until(cx, &workspace, "second selected session", |cx| {
            workspace.read_with(cx, |store, cx| {
                store
                    .sole_status(cx)
                    .is_some_and(|status| status.session_id == second.id)
            })
        });
        assert_eq!(
            workspace.update(cx, |store, cx| store
                .take_native_rewind_prefill_for(store.sole_thread().as_deref(), cx)),
            Some(crate::store::RewindPrefill {
                text: "second parked prefill".into(),
                attachments: Vec::new(),
            })
        );

        driver.shutdown(cx, &workspace, &host);
        std::fs::remove_dir_all(root).expect("remove test data");
    }

    /// A draft's client state follows its project, so deleting the project
    /// takes the draft's `ConversationUiState` with it instead of stranding an
    /// entry keyed by the transient draft session id.
    #[gpui::test]
    fn removing_a_project_clears_draft_state_and_reconciles_only_its_active_view(
        cx: &mut TestAppContext,
    ) {
        for viewing_draft in [false, true] {
            let root = scratch_root("remove-project-draft-ui");
            let disk = SessionStore::open_at(root.clone()).expect("open test store");
            for project in ["doomed", "kept"] {
                disk.upsert_project(&project_at(project, &root))
                    .expect("persist project");
            }
            disk.upsert_meta(&thread(&root, "kept-thread", "kept", None))
                .expect("persist session");
            let host = test_host(disk);
            let mut driver = LocalHostDriver::new(&host);
            let workspace = driver.workspace(cx);

            workspace.update(cx, |store, cx| {
                store.start_draft("doomed".into(), root.clone(), cx)
            });
            driver.wait_until(cx, &workspace, "draft for the doomed project", |cx| {
                workspace.read_with(cx, |store, cx| {
                    store.sole_status(cx).is_some_and(|status| {
                        status.draft && status.project_id.as_deref() == Some("doomed")
                    })
                })
            });
            mark_draft_state(cx, &workspace);

            if !viewing_draft {
                workspace.update(cx, |store, cx| store.show_alone("kept-thread".into(), cx));
                driver.wait_until(cx, &workspace, "kept thread selected", |cx| {
                    selected_status(cx, &workspace, "kept-thread")
                });
            }
            workspace.update(cx, |store, _| store.delete_project("doomed".into()));
            driver.wait_until(cx, &workspace, "project removed", |cx| {
                workspace.read_with(cx, |store, _| {
                    !store
                        .index_replica
                        .1
                        .iter()
                        .any(|project| project.id == "doomed")
                })
            });
            assert_no_draft_state(cx, &workspace);
            if viewing_draft {
                assert_eq!(
                    workspace.read_with(cx, |store, _| store.sole_thread()),
                    None,
                    "the deleted project's draft stayed open"
                );
            } else {
                assert!(
                    selected_status(cx, &workspace, "kept-thread"),
                    "deleting a background project moved the user"
                );
            }

            driver.shutdown(cx, &workspace, &host);
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    /// Committing a draft into a real session moves its client state from the
    /// project-draft key to the new thread key, so the user keeps the panel
    /// layout they were typing in.
    #[gpui::test]
    fn committing_a_draft_carries_its_conversation_state_to_the_thread(cx: &mut TestAppContext) {
        let root = scratch_root("commit-draft-ui");
        let disk = SessionStore::open_at(root.clone()).expect("open test store");
        disk.upsert_project(&project_at("p", &root))
            .expect("persist project");
        let host = test_host(disk);
        let mut driver = LocalHostDriver::new(&host);
        let workspace = driver.workspace(cx);

        workspace.update(cx, |store, cx| {
            store.start_draft("p".into(), root.clone(), cx)
        });
        driver.wait_until(cx, &workspace, "draft for p", |cx| {
            workspace.read_with(cx, |store, cx| {
                store
                    .sole_status(cx)
                    .is_some_and(|status| status.draft && status.project_id.as_deref() == Some("p"))
            })
        });
        let draft_id = workspace
            .read_with(cx, |store, _| store.sole_thread())
            .expect("draft selected");
        workspace.update(cx, |store, _| {
            store
                .conversation_ui
                .get_mut(&ConversationDestination::ProjectDraft("p".into()))
                .expect("draft conversation state")
                .right_panel_open = true;
        });

        let provider = workspace.read_with(cx, |store, cx| store.sole_status(cx).unwrap().provider);
        let scripted = tcode_runtime::app::scripted_provider(provider);
        let launcher = scripted.launcher.clone();
        update_host!(&host, move |state, _| state
            .set_provider_launcher_for_test(launcher));
        driver.command(
            cx,
            &workspace,
            Command::SendTurn {
                session_id: draft_id.clone(),
                text: "first turn".into(),
                attachment_paths: Vec::new(),
            },
        );
        driver.wait_until(cx, &workspace, "draft committed", |cx| {
            workspace.read_with(cx, |store, cx| {
                store
                    .sole_status(cx)
                    .is_some_and(|status| !status.draft && status.session_id == draft_id)
            })
        });
        workspace.read_with(cx, |store, _| {
            assert!(
                store
                    .conversation_ui
                    .get(&ConversationDestination::Thread(draft_id.clone()))
                    .is_some_and(|ui| ui.right_panel_open),
                "the committed thread lost the state the user left in its draft"
            );
            assert!(
                !store
                    .conversation_ui
                    .contains_key(&ConversationDestination::ProjectDraft("p".into())),
                "the committed draft's state was copied instead of moved"
            );
        });

        driver.shutdown(cx, &workspace, &host);
        let _ = std::fs::remove_dir_all(&root);
    }
}
