use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{App, Context, Entity, EventEmitter, Subscription as GpuiSubscription, Task};
use tcode_client::{
    ConnectionState, HostLink,
    host::{ClientHost, ClientPreferences},
};
use tcode_core::{
    git::{GitFileEntry, MenuItem, QuickAction, menu_items, quick_action},
    project::{
        Project, ProjectGroup, SessionMeta, WorktreeInfo, group_sessions,
        order_sessions_with_children,
    },
    provider_models::{ResolvedModel, picker_models, resolve_models},
    provider_status::ProviderSnapshot,
    session::{EntryContent, ReviewComment, StoredEvent, Timeline},
    settings::{
        BrowserSettings, ProjectSort, ProviderSettings, ResolvedProfile, Settings, SidebarLayout,
        ThemeMode,
    },
    ui::{ConversationDestination, RightTab},
};
use tcode_protocol::{AcpMarketplaceItem, RuntimeNotification as RuntimeEvent};
use tcode_protocol::{
    CommandResponse, EventEnvelope, ExternalImportStatus, ExternalThread, GitDiffResult,
    GitDiffScope, GitStatusStatus, PathEntry, ProtocolError, ProviderVersionStatus,
    ProvidersStatus, Query, QueryResponse, RecentDir, ServerEvent, SessionSearchHit, SessionStatus,
    Subscription, TerminalFrame, Topic,
};
pub(crate) mod terminal;
pub(crate) use terminal::ClientTerminal;
use terminal::TerminalWorkspace;

use crate::conversation_ui::{ConversationUiState, DiffFocus};

mod history;
pub(crate) use history::HISTORY_WINDOW_SCREENS;
mod images;
mod intents;
pub(crate) use images::host_image;
mod snapshots;

pub(crate) use snapshots::{ComposerState, PanelState};

/// Payload-free topic discriminant used by views to subscribe only to the
/// store projections they render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopicKind {
    SessionEvents,
    SessionStatus,
    Index,
    Settings,
    Providers,
    GitStatus,
    RuntimeEvents,
    ActiveSession,
    Terminal,
    Preview,
    ExternalImport,
}

impl From<&Topic> for TopicKind {
    fn from(topic: &Topic) -> Self {
        match topic {
            Topic::SessionEvents { .. } => Self::SessionEvents,
            Topic::SessionStatus { .. } => Self::SessionStatus,
            Topic::Index => Self::Index,
            Topic::Settings => Self::Settings,
            Topic::Providers => Self::Providers,
            Topic::GitStatus { .. } => Self::GitStatus,
            Topic::RuntimeEvents => Self::RuntimeEvents,
            Topic::Terminal { .. } => Self::Terminal,
            Topic::Preview { .. } => Self::Preview,
            Topic::ExternalImport { .. } => Self::ExternalImport,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreChange {
    pub topic: TopicKind,
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

/// The client-facing projection and command boundary for workspace state.
///
/// Views observe this entity and use its typed accessors instead of retaining
/// or reading the backend `AppState` entity directly.
pub struct WorkspaceStore {
    host: HostLink,
    attachment: WorkspaceAttachment,
    client_host: Option<Rc<dyn ClientHost>>,
    client_preferences: ClientPreferences,
    image_namespace: u64,
    attachment_tasks: Vec<Task<()>>,
    /// Replicated terminal grids, keyed by the host's terminal id.
    terminals: HashMap<u64, std::rc::Rc<ClientTerminal>>,
    /// Preview requests routed to this client. Every client owns the channel:
    /// one without a backend still has to answer `unsupported` rather than
    /// leave the agent's call hanging.
    remote_preview: (
        async_channel::Sender<EventEnvelope>,
        async_channel::Receiver<EventEnvelope>,
    ),
    /// Latest host-published import status per project, replicated from
    /// [`Topic::ExternalImport`]. The dialog renders this rather than owning a
    /// second events consumer.
    import_statuses: HashMap<String, Option<ExternalImportStatus>>,
    connection_state: ConnectionState,
    index_replica: (Vec<SessionMeta>, Vec<Project>),
    settings_replica: Settings,
    /// Whether `settings_replica` is the host's settings or still the local
    /// defaults it was constructed with. Views that copy a setting into an
    /// editable input must not treat the defaults as the host's answer.
    settings_hydrated: bool,
    baseline_topics: HashSet<Topic>,
    index_hydrated: bool,
    last_refresh_attempt: Option<u32>,
    address_refresh: Option<Task<()>>,
    hydrated_sessions: HashSet<String>,
    selected_session_id: Option<String>,
    session_records: HashMap<String, Vec<StoredEvent>>,
    session_from: HashMap<String, u64>,
    selection_generation: u64,
    session_turn_offset: usize,
    history_task: Option<Task<()>>,
    history_error: Option<String>,
    history_pages_fetched: usize,
    history_logged_records: Option<usize>,
    session_catching_up: bool,
    session_statuses: HashMap<String, SessionStatus>,
    git_statuses: HashMap<String, GitStatusStatus>,
    session_replica: Option<(String, Timeline)>,
    session_status_replica: Option<SessionStatus>,
    providers_replica: ProvidersStatus,
    git_status_replica: GitStatusStatus,
    /// (working, pending_approval, pending_user_input, background_only) for
    /// parked sessions.
    background_session_flags: HashMap<String, (bool, bool, bool, bool)>,
    active_destination: Option<ConversationDestination>,
    /// One-shot turn navigation requested by a cross-session content search.
    pending_chat_turn: Option<(String, usize)>,
    native_rewind_prefills: HashMap<String, String>,
    fallback_blocks: HashMap<String, FallbackBlock>,
    fallback_reviews: HashMap<String, FallbackReview>,
    conversation_ui: HashMap<ConversationDestination, ConversationUiState>,
    /// A project-draft fallback is in flight, so the reconcile step does not
    /// ask for one more draft per index event while it resolves.
    draft_fallback_pending: bool,
}

/// A turn stopped by Claude Code's safety classifier, kept per session so the
/// composer can offer recovery after the turn already ended.
#[derive(Debug, Clone)]
pub struct FallbackBlock {
    pub category: Option<agent::ClassifierCategory>,
    /// The model that refused (or was expected, on a silent reroute).
    pub model: Option<String>,
    /// The model Claude rerouted to; `None` when the request was blocked.
    pub fallback_model: Option<String>,
    pub detail: String,
}

/// A second model's read on a classifier stop: whether it looks like a false
/// positive, plus a clarification the user may review, edit and send. Both are
/// suggestions — nothing here is sent without a click.
#[derive(Debug, Clone)]
pub struct FallbackReview {
    pub assessment: String,
    /// Empty when the reviewer did not judge the flag a false positive.
    pub draft: String,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkAvailability {
    Available,
    Unsupported,
    Empty,
    Running,
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

fn effective_client_settings(host: &Settings, preferences: &ClientPreferences) -> Settings {
    let mut settings = host.clone();
    settings.theme_mode = match preferences.appearance.as_deref() {
        Some("system") => ThemeMode::System,
        Some("light") => ThemeMode::Light,
        Some("dark") => ThemeMode::Dark,
        _ => settings.theme_mode,
    };
    settings.language = match preferences.language.as_deref() {
        Some("system") => None,
        Some(language) => Some(language.to_owned()),
        None => settings.language,
    };
    settings
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

    /// An attached, blocking-seeded local store. Callers that must not block —
    /// a phone or browser on a single-threaded executor — go through
    /// [`WorkspaceStore::new_attached`] directly.
    pub fn new(host: HostLink, cx: &mut Context<Self>) -> Self {
        Self::new_attached(host, WorkspaceAttachment::Local, None, true, cx)
    }

    /// Construct the complete projection for exactly one client link.
    ///
    /// `seed_blocking` makes construction wait for the first Index/Settings/
    /// Providers snapshots. Only the desktop composition root asks for it: it
    /// applies the locale and theme from `settings()` the instant the store
    /// exists. Every other client renders immediately and re-renders when the
    /// snapshots land, which is the only option on a single-threaded executor.
    pub fn new_attached(
        host: HostLink,
        attachment: WorkspaceAttachment,
        client_host: Option<Rc<dyn ClientHost>>,
        seed_blocking: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        static NEXT_IMAGE_NAMESPACE: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(1);
        let image_namespace =
            NEXT_IMAGE_NAMESPACE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        cx.set_global(images::HostImages {
            link: Some(host.clone()),
            namespace: image_namespace,
        });
        let client_preferences = client_host
            .as_ref()
            .map(|host| host.load_preferences())
            .unwrap_or_default();
        let remote = matches!(attachment, WorkspaceAttachment::Remote { .. });
        let store = Self {
            host: host.clone(),
            attachment,
            client_host,
            client_preferences,
            image_namespace,
            attachment_tasks: Vec::new(),
            terminals: HashMap::new(),
            remote_preview: async_channel::unbounded(),
            import_statuses: HashMap::new(),
            connection_state: if remote {
                host.connection_state()
            } else {
                ConnectionState::Connected
            },
            index_replica: (Vec::new(), Vec::new()),
            settings_replica: Settings::default(),
            settings_hydrated: false,
            baseline_topics: HashSet::new(),
            index_hydrated: false,
            last_refresh_attempt: None,
            address_refresh: None,
            hydrated_sessions: HashSet::new(),
            selected_session_id: None,
            session_records: HashMap::new(),
            session_from: HashMap::new(),
            selection_generation: 0,
            session_turn_offset: 0,
            history_task: None,
            history_error: None,
            history_pages_fetched: 0,
            history_logged_records: None,
            session_catching_up: false,
            session_statuses: HashMap::new(),
            git_statuses: HashMap::new(),
            session_replica: None,
            session_status_replica: None,
            providers_replica: ProvidersStatus::default(),
            git_status_replica: GitStatusStatus::default(),
            background_session_flags: HashMap::new(),
            active_destination: None,
            pending_chat_turn: None,
            native_rewind_prefills: HashMap::new(),
            fallback_blocks: HashMap::new(),
            fallback_reviews: HashMap::new(),
            conversation_ui: HashMap::new(),
            draft_fallback_pending: false,
        };
        let mut store = store;

        // Construction seeding is itself protocol traffic: subscribe, then
        // apply each snapshot event. No live AppState read exists here.
        // Settings first: the index seed reconciles the destination, and that
        // decision reads the remembered project out of settings.
        let seed_topics = [Topic::Settings, Topic::Index, Topic::Providers];
        for topic in &seed_topics {
            if let Err(error) = host.subscribe(Subscription {
                after: None,
                topic: topic.clone(),
            }) {
                log::error!("failed to subscribe to {topic:?}: {}", error.message);
            }
        }
        let _ = host.subscribe(Subscription {
            topic: Topic::RuntimeEvents,
            after: None,
        });
        let events = host.events();
        #[cfg(not(target_family = "wasm"))]
        if seed_blocking {
            let mut seeded = HashSet::new();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while seeded.len() < seed_topics.len() && std::time::Instant::now() < deadline {
                match events.try_recv() {
                    Ok(envelope) => {
                        match (&envelope.topic, &envelope.event) {
                            (Topic::Index, ServerEvent::IndexSnapshot(_))
                            | (Topic::Settings, ServerEvent::SettingsSnapshot(_))
                            | (Topic::Providers, ServerEvent::ProvidersReplaced(_)) => {
                                seeded.insert(envelope.topic.clone());
                            }
                            _ => {}
                        }
                        if let ServerEvent::Runtime(event) = &envelope.event {
                            cx.emit(event.clone());
                        } else {
                            store.apply_domain_event(&envelope, cx);
                        }
                    }
                    Err(async_channel::TryRecvError::Empty) => {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    Err(async_channel::TryRecvError::Closed) => break,
                }
            }
            if seeded.len() != seed_topics.len() {
                log::error!(
                    "host snapshot seeding timed out: received {}/{} domains",
                    seeded.len(),
                    seed_topics.len()
                );
            }
        }
        #[cfg(target_family = "wasm")]
        let _ = seed_blocking;

        #[cfg(not(test))]
        {
            let event_messages = events;
            store.attachment_tasks.push(cx.spawn(async move |this, cx| {
                while let Ok(envelope) = event_messages.recv().await {
                    if this
                        .update(cx, |store, cx| {
                            if let ServerEvent::Runtime(event) = &envelope.event {
                                cx.emit(event.clone());
                            } else {
                                store.apply_domain_event(&envelope, cx);
                                cx.emit(StoreChange {
                                    topic: TopicKind::from(&envelope.topic),
                                });
                            }
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
                            store.refresh_address(&state, cx);
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

    /// Compatibility path for the mobile coordinator until it adopts the
    /// attachment identity constructor.
    pub fn attach_remote(&mut self, host_name: String, cx: &mut Context<Self>) {
        self.attachment = WorkspaceAttachment::Remote {
            host_id: String::new(),
            host_name,
        };
        self.connection_state = self.host.connection_state();
        let changes = self.host.connection_state_changes();
        self.attachment_tasks.push(cx.spawn(async move |this, cx| {
            while let Ok(state) = changes.recv().await {
                if this
                    .update(cx, |store, cx| {
                        store.refresh_address(&state, cx);
                        store.apply_connection_state(state);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

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

    pub(crate) fn remote_preview_requests(&self) -> async_channel::Receiver<EventEnvelope> {
        self.remote_preview.1.clone()
    }

    #[cfg(all(
        feature = "native-preview",
        any(target_os = "macos", target_os = "windows", target_os = "android")
    ))]
    pub(crate) fn preview_proxy(
        &self,
    ) -> Result<Option<tcode_client::pairing::PairedHost>, String> {
        if !self.is_remote() {
            return Ok(None);
        }
        self.client_host
            .as_ref()
            .and_then(|client| {
                client
                    .load_hosts()
                    .into_iter()
                    .find(|host| Some(host.host_id.as_str()) == self.remote_host_id())
            })
            .map(Some)
            .ok_or_else(|| "remote preview requires a paired machine credential".into())
    }

    pub fn is_remote(&self) -> bool {
        matches!(self.attachment, WorkspaceAttachment::Remote { .. })
    }

    pub fn remote_host_name(&self) -> Option<&str> {
        match &self.attachment {
            WorkspaceAttachment::Local => None,
            WorkspaceAttachment::Remote { host_name, .. } => Some(host_name),
        }
    }

    pub fn remote_host_id(&self) -> Option<&str> {
        match &self.attachment {
            WorkspaceAttachment::Local => None,
            WorkspaceAttachment::Remote { host_id, .. } => Some(host_id),
        }
    }

    pub fn connection_state(&self) -> &ConnectionState {
        if matches!(
            self.connection_state,
            ConnectionState::Connected | ConnectionState::Syncing
        ) {
            if self.baseline_ready() {
                &ConnectionState::Connected
            } else {
                &ConnectionState::Syncing
            }
        } else {
            &self.connection_state
        }
    }

    fn refresh_address(&mut self, state: &ConnectionState, cx: &mut Context<Self>) {
        if matches!(
            state,
            ConnectionState::Connected | ConnectionState::Syncing | ConnectionState::Offline { .. }
        ) {
            self.address_refresh.take();
            self.last_refresh_attempt = None;
        }
        let ConnectionState::Reconnecting {
            attempt,
            reason:
                Some(
                    tcode_client::ConnectionFailure::Unreachable
                    | tcode_client::ConnectionFailure::Timeout,
                ),
        } = state
        else {
            return;
        };
        if self.last_refresh_attempt == Some(*attempt) {
            return;
        }
        self.last_refresh_attempt = Some(*attempt);
        let Some(host_id) = self.remote_host_id().map(str::to_owned) else {
            return;
        };
        let Some(client) = self.client_host.clone() else {
            return;
        };
        self.address_refresh = Some(cx.spawn(async move |this, cx| {
            if let Some(origin) = client.refresh_origin(&host_id).await {
                let _ = this.update(cx, |store, _| {
                    store
                        .host
                        .wake(tcode_client::recovery::Wake::Origin(origin));
                });
            }
        }));
    }

    fn apply_connection_state(&mut self, state: ConnectionState) {
        if matches!(
            state,
            ConnectionState::Syncing
                | ConnectionState::Reconnecting { .. }
                | ConnectionState::Offline { .. }
        ) {
            self.baseline_topics.clear();
        }
        self.connection_state = state;
        if self.connection_state == ConnectionState::Syncing {
            // State and domain events arrive on separate queues. Request a fresh
            // baseline after invalidation, so a late event from the old socket
            // cannot satisfy readiness for the new one. HostLink correlates the
            // replies against these new request IDs and retains applied cursors.
            for subscription in self.host.subscriptions() {
                let _ = self.host.subscribe(subscription);
            }
        }
    }

    pub fn queued_outgoing(&self) -> usize {
        self.host.queued_outgoing()
    }

    pub fn baseline_ready(&self) -> bool {
        self.baseline_topics.contains(&Topic::Index)
            && self.baseline_topics.contains(&Topic::Settings)
            && self.selected_session_id.as_ref().is_none_or(|id| {
                self.baseline_topics.contains(&Topic::SessionStatus {
                    session_id: id.clone(),
                }) && self.baseline_topics.contains(&Topic::SessionEvents {
                    session_id: id.clone(),
                })
            })
    }

    /// Cached content remains readable while a new baseline is replayed.
    pub fn threads_loading(&self) -> bool {
        !self.index_hydrated
            || !self.settings_hydrated
            || (!matches!(self.connection_state(), ConnectionState::Connected)
                && self.index_replica.0.is_empty()
                && self.index_replica.1.is_empty())
    }

    pub fn chat_loading(&self) -> bool {
        if !self.index_hydrated || !self.settings_hydrated {
            return true;
        }
        if let Some(id) = &self.selected_session_id {
            self.session_loading()
                || !self.hydrated_sessions.contains(id)
                || !self.session_statuses.contains_key(id)
        } else {
            self.threads_loading()
        }
    }

    /// End this store's one-link lifetime before its views are replaced.
    pub fn detach(&mut self, cx: &mut App) {
        self.history_task = None;
        self.attachment_tasks.clear();
        self.address_refresh.take();
        for subscription in self.host.subscriptions() {
            let _ = self.host.unsubscribe(subscription);
        }
        self.host.close();
        self.remote_preview.0.close();
        self.remote_preview.1.close();
        if let Some(images) = cx.try_global::<images::HostImages>()
            && images.namespace == self.image_namespace
        {
            cx.set_global(images::HostImages {
                link: None,
                namespace: self.image_namespace,
            });
        }
    }

    pub fn sync_active_conversation_ui(&mut self) {
        let destination = self.session_status_replica.as_ref().map(Self::destination);
        if let (
            Some(tcode_core::ui::ConversationDestination::ProjectDraft(draft)),
            Some(tcode_core::ui::ConversationDestination::Thread(_)),
        ) = (&self.active_destination, &destination)
            && self
                .session_status_replica
                .as_ref()
                .is_some_and(|status| status.project_id.as_deref() == Some(draft.as_str()))
            && let Some(ui) = self
                .conversation_ui
                .remove(self.active_destination.as_ref().unwrap())
        {
            self.conversation_ui
                .insert(destination.clone().unwrap(), ui);
        }
        if let Some((destination, status)) = destination
            .clone()
            .zip(self.session_status_replica.as_ref())
        {
            self.conversation_ui.entry(destination).or_insert_with(|| {
                ConversationUiState::new(
                    self.settings_replica.word_wrap_diffs,
                    status.terminal_open,
                    status.terminal_height,
                )
            });
        }
        self.active_destination = destination;
    }

    fn apply_domain_event(&mut self, envelope: &EventEnvelope, cx: &mut Context<Self>) {
        if !self.host.subscription_reply_is_current(envelope) {
            return;
        }
        match (&envelope.topic, &envelope.event) {
            (
                Topic::Preview { session_id },
                ServerEvent::PreviewRequest {
                    session_id: requested,
                    ..
                },
            ) if session_id == requested
                && self.selected_session_id.as_ref() == Some(session_id) =>
            {
                let _ = self.remote_preview.0.try_send(envelope.clone());
            }
            (
                Topic::Terminal { terminal_id },
                ServerEvent::TerminalFrame {
                    terminal_id: frame_id,
                    frame,
                },
            ) if terminal_id == frame_id => self.apply_terminal_frame(*terminal_id, frame),
            (
                Topic::Terminal { terminal_id },
                ServerEvent::TerminalDelta {
                    terminal_id: delta_id,
                    delta,
                },
            ) if terminal_id == delta_id => self.apply_terminal_delta(*terminal_id, delta),
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
                self.native_rewind_prefills.remove(session_id);
                self.fallback_blocks.remove(session_id);
                self.fallback_reviews.remove(session_id);
                self.conversation_ui
                    .remove(&ConversationDestination::Thread(session_id.clone()));
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
                self.baseline_topics.insert(Topic::Index);
                self.index_replica = (snapshot.sessions.clone(), snapshot.projects.clone());
                // Client state for a conversation the index no longer lists has
                // nothing left to return to: a deleted project takes its draft's
                // state, a deleted thread its own. Archived threads stay listed,
                // so archiving keeps its state.
                self.conversation_ui
                    .retain(|destination, _| match destination {
                        ConversationDestination::ProjectDraft(project_id) => snapshot
                            .projects
                            .iter()
                            .any(|project| project.id == *project_id),
                        ConversationDestination::Thread(session_id) => {
                            snapshot.sessions.iter().any(|meta| meta.id == *session_id)
                        }
                    });
                self.background_session_flags = snapshot.activity.clone();
                if let Some(id) = &self.selected_session_id {
                    self.background_session_flags.remove(id);
                }
            }
            (Topic::Settings, ServerEvent::SettingsReplaced(settings))
            | (Topic::Settings, ServerEvent::SettingsSnapshot(settings)) => {
                self.settings_replica = settings.clone();
                self.settings_hydrated = true;
                if matches!(envelope.event, ServerEvent::SettingsSnapshot(_)) {
                    self.baseline_topics.insert(Topic::Settings);
                }
            }
            (Topic::Providers, ServerEvent::ProvidersReplaced(status)) => {
                self.providers_replica = status.clone();
            }
            (Topic::GitStatus { session_id }, ServerEvent::GitStatusReplaced(status)) => {
                self.git_statuses.insert(session_id.clone(), status.clone());
                if self.selected_session_id.as_ref() == Some(session_id) {
                    self.git_status_replica = status.clone();
                }
            }
            (Topic::SessionStatus { session_id }, ServerEvent::SessionStatusReplaced(status))
                if status.session_id == *session_id =>
            {
                self.baseline_topics.insert(envelope.topic.clone());
                self.session_statuses
                    .insert(session_id.clone(), status.clone());
                if self.selected_session_id.as_ref() == Some(session_id) {
                    let mut status = status.clone();
                    status.native_rewind_prefill_available =
                        self.native_rewind_prefills.contains_key(session_id);
                    self.session_status_replica = Some(status);
                    self.sync_terminal_topics();
                    self.sync_active_conversation_ui();
                    self.background_session_flags.remove(session_id);
                } else {
                    self.background_session_flags.insert(
                        session_id.clone(),
                        (
                            status.working,
                            status.pending_approval,
                            status.pending_user_input,
                            Self::status_background_only(status),
                        ),
                    );
                }
            }
            (Topic::SessionEvents { session_id }, ServerEvent::SessionHistoryError(error)) => {
                if self.selected_session_id.as_ref() == Some(session_id) {
                    self.history_error = Some(history::history_error_message(error.clone()));
                }
            }
            (
                Topic::SessionEvents { session_id },
                ServerEvent::SessionSnapshot {
                    from,
                    records,
                    total,
                    total_turns,
                    ..
                },
            ) => {
                if self.selected_session_id.as_ref() != Some(session_id) {
                    return;
                }
                let held = self.session_records.entry(session_id.clone()).or_default();
                let start = self.session_from.entry(session_id.clone()).or_insert(*from);
                if *from == 0 {
                    held.clear();
                    *start = 0;
                } else if *from != *start + held.len() as u64 {
                    held.clear();
                    self.session_from.remove(session_id);
                    self.session_replica = None;
                    self.hydrated_sessions.remove(session_id);
                    self.baseline_topics.remove(&envelope.topic);
                    self.session_catching_up = false;
                    let _ = self.host.subscribe(Subscription {
                        topic: envelope.topic.clone(),
                        after: None,
                    });
                    return;
                }
                if records.is_empty() && *from != 0 && self.session_replica.is_some() {
                    self.baseline_topics.insert(envelope.topic.clone());
                    self.hydrated_sessions.insert(session_id.clone());
                    return;
                }
                held.extend(records.iter().cloned());
                let after = *start + held.len() as u64;
                self.session_catching_up = after < *total;
                let _ = self.host.update_after(&envelope.topic, after);
                if self.session_catching_up {
                    return;
                }
                let mut timeline = Timeline::fold_events(held.iter().cloned());
                if !self
                    .session_status_replica
                    .as_ref()
                    .is_some_and(|status| status.turn_running)
                {
                    timeline.mark_idle();
                }
                self.baseline_topics.insert(envelope.topic.clone());
                self.hydrated_sessions.insert(session_id.clone());
                self.session_turn_offset =
                    (*total_turns as usize).saturating_sub(timeline.turns.len());
                self.session_replica = Some((session_id.clone(), timeline));
            }
            (Topic::SessionEvents { session_id }, ServerEvent::SessionEvent(record)) => {
                if self.selected_session_id.as_ref() != Some(session_id) {
                    return;
                }
                if self.session_catching_up || !self.session_from.contains_key(session_id) {
                    return;
                }
                let held = self.session_records.entry(session_id.clone()).or_default();
                held.push(record.clone());
                let after = self.session_from[session_id] + held.len() as u64;
                let _ = self.host.update_after(&envelope.topic, after);
                // A new turn means the user moved on; the recovery card for the
                // stopped one is stale.
                if matches!(record.event, agent::AgentEvent::TurnStarted { .. }) {
                    self.fallback_blocks.remove(session_id);
                    self.fallback_reviews.remove(session_id);
                }
                self.apply_conversation_event(session_id, &record.event);
                if let Some((replica_id, timeline)) = self.session_replica.as_mut()
                    && replica_id == session_id
                {
                    timeline.apply_at(record.ts, &record.event);
                }
            }
            (
                Topic::SessionStatus { session_id },
                ServerEvent::NativeRewindPrefill {
                    session_id: event_session,
                    text,
                },
            ) if session_id == event_session => {
                self.native_rewind_prefills
                    .insert(session_id.clone(), text.clone());
                if let Some(status) = self
                    .session_status_replica
                    .as_mut()
                    .filter(|status| status.session_id == *session_id)
                {
                    status.native_rewind_prefill_available = true;
                }
            }
            (
                Topic::SessionStatus { session_id },
                ServerEvent::ModelFallbackBlocked {
                    session_id: event_session,
                    category,
                    model,
                    fallback_model,
                    detail,
                },
            ) if session_id == event_session => {
                self.fallback_blocks.insert(
                    session_id.clone(),
                    FallbackBlock {
                        category: category.clone(),
                        model: model.clone(),
                        fallback_model: fallback_model.clone(),
                        detail: detail.clone(),
                    },
                );
            }
            (
                Topic::SessionStatus { session_id },
                ServerEvent::FallbackReviewReady {
                    session_id: event_session,
                    assessment,
                    draft,
                },
            ) if session_id == event_session => {
                self.fallback_reviews.insert(
                    session_id.clone(),
                    FallbackReview {
                        assessment: assessment.clone(),
                        draft: draft.clone(),
                    },
                );
            }
            _ => {}
        }
        self.load_pending_chat_history(cx);
        // Every index mutation re-decides the destination in one place.
        if envelope.topic == Topic::Index {
            self.reconcile_destination(cx);
        }
    }

    /// Decide what the workspace shows after the index changed.
    ///
    /// Only the conversation on screen is reconciled, so archiving a
    /// background thread (an Orchestrate sibling completing, a sweep) never
    /// steals navigation. When the thread on screen leaves the visible index —
    /// auto-archived on completion, archived by hand, deleted — the workspace
    /// follows its still-visible parent; with no such parent it falls back to
    /// the last interacted project's draft, which is also what an empty
    /// workspace opens.
    fn reconcile_destination(&mut self, cx: &mut Context<Self>) {
        match &self.session_status_replica {
            // A draft has no index entry of its own; it stays until the user
            // navigates away. Its project leaving the index is the exception:
            // there is nothing left to draft into, so it is treated like a
            // vanished thread.
            Some(status) if status.draft => {
                let project_gone = status.project_id.as_ref().is_some_and(|project_id| {
                    !self
                        .index_replica
                        .1
                        .iter()
                        .any(|project| project.id == *project_id)
                });
                if !project_gone {
                    return;
                }
                self.leave_session();
            }
            Some(status) => {
                let session_id = status.session_id.clone();
                if self.session_visible(&session_id) {
                    return;
                }
                let parent = self
                    .index_replica
                    .0
                    .iter()
                    .find(|meta| meta.id == session_id)
                    .and_then(|meta| meta.parent_session_id.clone())
                    .filter(|parent| self.session_visible(parent));
                if let Some(parent) = parent {
                    self.select_session(parent);
                    return;
                }
                self.leave_session();
            }
            // Selected, but its first status has not arrived: nothing to
            // decide yet. Without a selection this is the empty workspace.
            None if self.selected_session_id.is_some() => return,
            None => {}
        }
        self.open_last_project_draft(cx);
    }

    fn session_visible(&self, session_id: &str) -> bool {
        self.index_replica
            .0
            .iter()
            .any(|meta| meta.id == session_id && meta.archived_at.is_none())
    }

    /// Open the new-thread draft of the project the user last interacted with,
    /// so an empty workspace offers a composer instead of a dead end. The
    /// runtime returns that project's standing draft when it already has one,
    /// keeping its composer attachments. A remembered project that is gone
    /// falls back to the first listed one; with no projects at all the chat
    /// view keeps its add-project state.
    fn open_last_project_draft(&mut self, cx: &mut Context<Self>) {
        if self.draft_fallback_pending {
            return;
        }
        let remembered = self.settings_replica.last_project_id.as_deref();
        let Some(project) = self
            .index_replica
            .1
            .iter()
            .find(|project| Some(project.id.as_str()) == remembered)
            .or_else(|| self.index_replica.1.first())
            .cloned()
        else {
            return;
        };
        self.draft_fallback_pending = true;
        self.start_draft(project.id, project.root, cx);
    }

    fn apply_conversation_event(&mut self, session_id: &str, event: &agent::AgentEvent) {
        let destination = ConversationDestination::Thread(session_id.to_string());
        let Some(ui) = self.conversation_ui.get_mut(&destination) else {
            return;
        };
        match event {
            agent::AgentEvent::TurnStarted { .. } => {
                ui.auto_open_task_suppressed = false;
            }
            agent::AgentEvent::PlanUpdated { .. }
                if self.settings_replica.auto_open_task_panel
                    && !ui.auto_open_task_suppressed
                    && !(ui.right_panel_open && ui.right_tab == RightTab::Plan) =>
            {
                ui.right_panel_open = true;
                ui.right_tab = RightTab::Plan;
            }
            agent::AgentEvent::TurnCompleted { .. } | agent::AgentEvent::RewindCompleted { .. } => {
                ui.refresh_diff()
            }
            _ => {}
        }
    }

    pub fn all_provider_profiles(&self) -> Vec<ResolvedProfile> {
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

    pub fn enabled_profiles(&self) -> Vec<ResolvedProfile> {
        self.all_provider_profiles()
            .into_iter()
            .filter(|profile| profile.settings.enabled)
            .collect()
    }

    pub fn profile_catalog(&self, profile_id: &str) -> Vec<agent::ModelSpec> {
        if Settings::is_builtin_profile_id(profile_id) {
            let kind = self
                .settings_replica
                .resolved_profile(profile_id)
                .map(|profile| profile.kind)
                .unwrap_or(agent::ProviderKind::ClaudeCode);
            self.providers_replica
                .model_catalogs
                .get(&kind)
                .cloned()
                .unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    #[cfg(test)]
    pub(crate) fn drain_host_events_for_test(&mut self, cx: &mut Context<Self>) {
        let events = self.host.events();
        while let Ok(envelope) = events.try_recv() {
            if let ServerEvent::Runtime(event) = &envelope.event {
                cx.emit(event.clone());
            } else {
                self.apply_domain_event(&envelope, cx);
                cx.emit(StoreChange {
                    topic: TopicKind::from(&envelope.topic),
                });
            }
            cx.notify();
        }
    }

    pub fn working_sessions_count(&self) -> usize {
        let active = usize::from(
            self.session_status_replica
                .as_ref()
                .is_some_and(|status| status.working),
        );
        active
            + self
                .background_session_flags
                .values()
                .filter(|(working, ..)| *working)
                .count()
    }

    fn active_conversation_ui(&self) -> Option<&crate::conversation_ui::ConversationUiState> {
        self.conversation_ui.get(self.active_destination.as_ref()?)
    }

    fn active_conversation_ui_mut(
        &mut self,
    ) -> Option<&mut crate::conversation_ui::ConversationUiState> {
        let destination = self.active_destination.clone()?;
        self.conversation_ui.get_mut(&destination)
    }

    fn active_turn_running(&self) -> bool {
        self.session_status_replica
            .as_ref()
            .is_some_and(|status| status.turn_running)
    }

    fn suppress_task_auto_open_if_running(&mut self) {
        let running = self.active_turn_running();
        if running && let Some(ui) = self.active_conversation_ui_mut() {
            ui.auto_open_task_suppressed = true;
        }
    }

    pub fn toggle_diff_panel(&mut self, cx: &mut Context<Self>) {
        let closing = self
            .active_conversation_ui()
            .is_some_and(|ui| ui.right_panel_open && ui.right_tab == RightTab::Diff);
        if let Some(ui) = self.active_conversation_ui_mut() {
            if closing {
                ui.right_panel_open = false;
                ui.pending_diff_focus = None;
            } else {
                ui.right_panel_open = true;
                ui.right_tab = RightTab::Diff;
                ui.refresh_diff();
            }
        }
        if closing {
            self.suppress_task_auto_open_if_running();
        }
        cx.notify();
    }

    pub fn open_diff_for_turn(&mut self, turn: usize, cx: &mut Context<Self>) {
        if let Some(ui) = self.active_conversation_ui_mut() {
            ui.pending_diff_focus = None;
            ui.right_panel_open = true;
            ui.right_tab = RightTab::Diff;
            ui.diff_selected_turn = Some(turn);
            ui.refresh_diff();
            cx.notify();
        }
    }

    pub fn open_diff_for_file(&mut self, turn: usize, path: String, cx: &mut Context<Self>) {
        let session = self
            .session_status_replica
            .as_ref()
            .map(|status| status.session_id.clone());
        if let (Some(session), Some(ui)) = (session, self.active_conversation_ui_mut()) {
            ui.right_panel_open = true;
            ui.right_tab = RightTab::Diff;
            ui.diff_selected_turn = Some(turn);
            ui.pending_diff_focus = Some(DiffFocus {
                session,
                turn,
                path,
            });
            ui.refresh_diff();
            cx.notify();
        }
    }

    pub fn select_diff_turn(&mut self, turn: usize, cx: &mut Context<Self>) {
        if let Some(ui) = self.active_conversation_ui_mut() {
            ui.pending_diff_focus = None;
            ui.diff_selected_turn = Some(turn);
            ui.refresh_diff();
            cx.notify();
        }
    }

    pub fn discard_diff_focus(&mut self, cx: &mut Context<Self>) {
        if let Some(ui) = self.active_conversation_ui_mut() {
            ui.discard_diff_focus();
            cx.notify();
        }
    }

    pub fn close_diff_panel(&mut self, cx: &mut Context<Self>) {
        if let Some(ui) = self.active_conversation_ui_mut() {
            ui.pending_diff_focus = None;
            ui.right_panel_open = false;
        }
        self.suppress_task_auto_open_if_running();
        cx.notify();
    }

    pub fn toggle_diff_expanded(&mut self, cx: &mut Context<Self>) {
        if let Some(ui) = self.active_conversation_ui_mut() {
            ui.right_panel_expanded = !ui.right_panel_expanded;
            cx.notify();
        }
    }

    pub fn set_right_tab(&mut self, tab: RightTab, cx: &mut Context<Self>) {
        if let Some(ui) = self.active_conversation_ui_mut() {
            ui.right_tab = tab;
            cx.notify();
        }
    }

    fn toggle_tab_panel(&mut self, tab: RightTab, cx: &mut Context<Self>) {
        let closing = self
            .active_conversation_ui()
            .is_some_and(|ui| ui.right_panel_open && ui.right_tab == tab);
        if let Some(ui) = self.active_conversation_ui_mut() {
            ui.right_panel_open = !closing;
            ui.right_tab = tab;
        }
        if closing {
            self.suppress_task_auto_open_if_running();
        }
        cx.notify();
    }

    pub fn toggle_plan_panel(&mut self, cx: &mut Context<Self>) {
        self.toggle_tab_panel(RightTab::Plan, cx);
    }

    pub fn toggle_preview_panel(&mut self, cx: &mut Context<Self>) {
        self.toggle_tab_panel(RightTab::Preview, cx);
    }

    pub fn close_preview_panel(&mut self, cx: &mut Context<Self>) {
        let showing = self
            .active_conversation_ui()
            .is_some_and(|ui| ui.right_panel_open && ui.right_tab == RightTab::Preview);
        if showing && let Some(ui) = self.active_conversation_ui_mut() {
            ui.right_panel_open = false;
        }
        if showing {
            self.suppress_task_auto_open_if_running();
            cx.notify();
        }
    }

    pub fn open_preview_panel(&mut self, cx: &mut Context<Self>) {
        if let Some(ui) = self.active_conversation_ui_mut()
            && !(ui.right_panel_open && ui.right_tab == RightTab::Preview)
        {
            ui.right_panel_open = true;
            ui.right_tab = RightTab::Preview;
            cx.notify();
        }
    }

    pub fn open_preview_panel_for(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let destination = if self
            .session_status_replica
            .as_ref()
            .is_some_and(|status| status.session_id == session_id)
        {
            self.active_destination
                .clone()
                .unwrap_or_else(|| ConversationDestination::Thread(session_id.to_string()))
        } else {
            ConversationDestination::Thread(session_id.to_string())
        };
        let ui = self.conversation_ui.entry(destination).or_insert_with(|| {
            ConversationUiState::new(self.settings_replica.word_wrap_diffs, false, 240.)
        });
        ui.right_panel_open = true;
        ui.right_tab = RightTab::Preview;
        cx.notify();
    }

    fn conversation_ui_by_key(&self, key: &str) -> Option<&ConversationUiState> {
        self.conversation_ui
            .iter()
            .find_map(|(destination, ui)| (destination.preference_key() == key).then_some(ui))
    }

    fn conversation_ui_by_key_mut(&mut self, key: &str) -> Option<&mut ConversationUiState> {
        self.conversation_ui
            .iter_mut()
            .find_map(|(destination, ui)| (destination.preference_key() == key).then_some(ui))
    }

    pub fn preview_url(&self, key: &str) -> Option<String> {
        self.conversation_ui_by_key(key)
            .and_then(|ui| ui.preview_url.clone())
    }

    pub fn set_preview_url(&mut self, key: &str, url: String, cx: &mut Context<Self>) {
        if let Some(ui) = self.conversation_ui_by_key_mut(key) {
            ui.preview_url = Some(url);
            cx.notify();
        }
    }

    pub fn preview_canvas(&self, key: &str) -> Option<(u32, u32)> {
        self.conversation_ui_by_key(key)
            .and_then(|ui| ui.preview_canvas)
    }

    pub fn set_preview_canvas(
        &mut self,
        key: &str,
        canvas: Option<(u32, u32)>,
        cx: &mut Context<Self>,
    ) {
        if let Some(ui) = self.conversation_ui_by_key_mut(key) {
            ui.preview_canvas = canvas;
            cx.notify();
        }
    }

    pub fn clear_preview_chrome(&mut self, key: &str, cx: &mut Context<Self>) {
        if let Some(ui) = self.conversation_ui_by_key_mut(key) {
            ui.preview_url = None;
            ui.preview_canvas = None;
            cx.notify();
        }
    }

    pub fn grouped_sessions(&self) -> Vec<ProjectGroup> {
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

    pub fn settings(&self) -> Settings {
        effective_client_settings(&self.settings_replica, &self.client_preferences)
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

    pub fn client_theme_override(&self) -> Option<ThemeMode> {
        match self.client_preferences.appearance.as_deref() {
            Some("system") => Some(ThemeMode::System),
            Some("light") => Some(ThemeMode::Light),
            Some("dark") => Some(ThemeMode::Dark),
            _ => None,
        }
    }

    pub fn set_client_theme(&mut self, mode: Option<ThemeMode>) {
        self.client_preferences.appearance = mode.map(|mode| match mode {
            ThemeMode::System => "system".to_owned(),
            ThemeMode::Light => "light".to_owned(),
            ThemeMode::Dark => "dark".to_owned(),
        });
        self.save_client_preferences();
    }

    pub fn client_language_override(&self) -> Option<&str> {
        self.client_preferences.language.as_deref()
    }

    pub fn set_client_language(&mut self, language: Option<String>) {
        self.client_preferences.language = language;
        self.save_client_preferences();
    }

    pub fn client_device_name_override(&self) -> Option<&str> {
        self.client_preferences.device_name.as_deref()
    }

    pub fn client_device_name(&self) -> String {
        self.client_preferences
            .device_name
            .clone()
            .filter(|name| !name.trim().is_empty())
            .or_else(|| self.client_host.as_ref().map(|host| host.device_name()))
            .unwrap_or_else(|| "tcode".into())
    }

    pub fn set_client_device_name(&mut self, name: Option<String>) {
        self.client_preferences.device_name = name.filter(|name| !name.trim().is_empty());
        self.save_client_preferences();
    }

    pub fn reset_client_preferences(&mut self) {
        self.client_preferences = ClientPreferences::default();
        self.save_client_preferences();
    }

    fn save_client_preferences(&self) {
        if let Some(host) = &self.client_host {
            let mut preferences = self.client_preferences.clone();
            // Navigation is written by the shell while this store is alive.
            // Appearance edits must not replace it with our startup snapshot.
            preferences.navigation = host.load_preferences().navigation;
            host.save_preferences(&preferences);
        }
    }

    pub fn live_command_panel(&self) -> bool {
        !self.settings_replica.live_command_panel_disabled
    }

    pub fn archived_groups(&self) -> Vec<ProjectGroup> {
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

    pub fn project_sort(&self) -> ProjectSort {
        self.settings_replica.project_sort
    }

    pub fn sidebar_layout(&self) -> SidebarLayout {
        self.settings_replica.sidebar_layout
    }

    pub fn flat_sessions(&self) -> Vec<SessionMeta> {
        let visible = self
            .index_replica
            .0
            .iter()
            .filter(|meta| meta.archived_at.is_none())
            .cloned()
            .collect();
        order_sessions_with_children(visible)
    }

    pub fn projects(&self) -> Vec<Project> {
        self.index_replica.1.clone()
    }

    pub fn is_project_collapsed(&self, project_id: &str) -> bool {
        self.settings_replica
            .collapsed_projects
            .iter()
            .any(|id| id == project_id)
    }

    pub fn active_session_id(&self) -> Option<String> {
        self.selected_session_id.clone()
    }

    pub fn turn_running_for(&self, session_id: &str) -> bool {
        self.session_status_replica
            .as_ref()
            .filter(|status| status.session_id == session_id)
            .map(|status| status.working)
            .or_else(|| {
                self.background_session_flags
                    .get(session_id)
                    .map(|flags| flags.0)
            })
            .unwrap_or(false)
    }

    /// Working only because provider background tasks are still running: the
    /// turn itself has finished and nothing is queued or in delivery.
    fn status_background_only(status: &SessionStatus) -> bool {
        status.working
            && !status.turn_running
            && status.delivery_in_flight.is_none()
            && status.queued_messages.is_empty()
    }

    pub fn background_only_for(&self, session_id: &str) -> bool {
        self.session_status_replica
            .as_ref()
            .filter(|status| status.session_id == session_id)
            .map(Self::status_background_only)
            .or_else(|| {
                self.background_session_flags
                    .get(session_id)
                    .map(|flags| flags.3)
            })
            .unwrap_or(false)
    }

    pub fn session_unread(&self, session_id: &str) -> bool {
        if self
            .session_status_replica
            .as_ref()
            .is_some_and(|status| status.session_id == session_id)
        {
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

    pub fn pending_approval_for(&self, session_id: &str) -> bool {
        self.session_status_replica
            .as_ref()
            .filter(|status| status.session_id == session_id)
            .map(|status| status.pending_approval)
            .or_else(|| {
                self.background_session_flags
                    .get(session_id)
                    .map(|flags| flags.1)
            })
            .unwrap_or(false)
    }

    pub fn pending_user_input_for(&self, session_id: &str) -> bool {
        self.session_status_replica
            .as_ref()
            .filter(|status| status.session_id == session_id)
            .map(|status| status.pending_user_input)
            .or_else(|| {
                self.background_session_flags
                    .get(session_id)
                    .map(|flags| flags.2)
            })
            .unwrap_or(false)
    }

    pub fn fork_availability(&self, session_id: &str) -> ForkAvailability {
        let Some(meta) = self
            .index_replica
            .0
            .iter()
            .find(|meta| meta.id == session_id)
        else {
            return ForkAvailability::Available;
        };
        if !meta.provider.caps().supports_fork {
            ForkAvailability::Unsupported
        } else if meta.resume_cursor.is_none() {
            ForkAvailability::Empty
        } else if self.turn_running_for(session_id) {
            ForkAvailability::Running
        } else {
            ForkAvailability::Available
        }
    }

    pub fn sidebar_sessions(&self) -> Vec<SessionMeta> {
        self.index_replica.0.clone()
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

    pub fn window_caption_state(&self) -> (bool, tcode_core::ui::RightTab) {
        self.active_conversation_ui()
            .map(|ui| (ui.right_panel_open, ui.right_tab))
            .unwrap_or((false, RightTab::default()))
    }

    pub fn shell_window_title(&self) -> String {
        match self.session_status_replica.as_ref() {
            Some(status) if status.draft => crate::tr!("chat.new_thread").into_owned(),
            Some(status) => status.title.clone(),
            None => "tcode".to_string(),
        }
    }

    pub fn preview_active_identity(&self) -> Option<(String, String)> {
        self.session_status_replica.as_ref().map(|status| {
            (
                status.session_id.clone(),
                Self::destination(status).preference_key(),
            )
        })
    }

    pub(crate) fn preview_live_keys(&self) -> HashSet<String> {
        let mut keys = self
            .index_replica
            .0
            .iter()
            .map(|session| session.id.clone())
            .collect::<HashSet<_>>();
        if let Some(destination) = &self.active_destination {
            keys.insert(destination.preference_key());
        }
        keys
    }

    pub fn preview_panel_showing(&self) -> bool {
        self.active_conversation_ui()
            .is_some_and(|ui| ui.right_panel_open && ui.right_tab == RightTab::Preview)
    }

    pub fn preview_browser_settings(&self) -> BrowserSettings {
        self.settings_replica.browser.clone()
    }

    pub fn provider_profile_kind(&self, profile_id: &str) -> agent::ProviderKind {
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
            &self.settings_replica.favorite_models,
        )
    }

    pub fn provider_profile_display_name(&self, profile_id: &str) -> String {
        self.settings_replica.profile_display_name(profile_id)
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

    pub fn tcode_update_status(&self) -> tcode_protocol::TcodeUpdateStatus {
        self.providers_replica.tcode_update.clone()
    }

    pub fn provider_profile_accent(&self, profile_id: &str) -> Option<u32> {
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

    pub fn provider_update_command(&self, provider: agent::ProviderKind) -> Option<String> {
        self.providers_replica
            .provider_versions
            .get(&provider)
            .and_then(|status| status.update_command.clone())
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
            &self.settings_replica.favorite_models,
        )
    }

    pub fn installed_acp_agent(
        &self,
        agent_id: &str,
    ) -> Option<tcode_core::acp::InstalledAcpAgent> {
        self.settings_replica.acp_agent(agent_id).cloned()
    }

    pub fn acp_marketplace_items(&self) -> Vec<AcpMarketplaceItem> {
        let mut items = self.providers_replica.acp_marketplace_items.clone();
        for item in &mut items {
            item.installed = self.settings_replica.acp_agents.contains_key(&item.id);
        }
        items
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
            .count();
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
    pub fn scan_external_history(&self, cx: &mut App) -> Task<Result<Vec<RecentDir>, String>> {
        let host = self.host.clone();
        cx.spawn(
            async move |_| match host.query(Query::ScanExternalHistory).await {
                Ok(QueryResponse::ExternalHistory(recent)) => Ok(recent),
                Ok(other) => Err(format!("unexpected external-history response: {other:?}")),
                Err(error) => Err(error.message),
            },
        )
    }

    /// Subscribe to a project's import status. Callers must do this *before*
    /// starting a run: a fast completion is only recoverable through the
    /// subscription snapshot, not through the start reply.
    pub fn watch_external_import(&self, project_id: &str) {
        if let Err(error) = self.host.subscribe(Subscription {
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
    /// session order live there; this client keeps only the answer.
    pub fn search_session_content(
        &self,
        query: String,
        limit: u32,
        cx: &mut App,
    ) -> Task<Vec<SessionSearchHit>> {
        let host = self.host.clone();
        cx.spawn(async move |_| {
            match host
                .query(Query::SearchSessionContent { query, limit })
                .await
            {
                Ok(QueryResponse::SessionContentHits(hits)) => hits,
                Ok(other) => {
                    log::error!("unexpected session-content response: {other:?}");
                    Vec::new()
                }
                Err(error) => {
                    log::error!("session-content query failed: {}", error.message);
                    Vec::new()
                }
            }
        })
    }

    pub(crate) fn commit_dialog_state(&self) -> CommitDialogState {
        CommitDialogState {
            files: self
                .git_status_replica
                .status
                .as_ref()
                .map(|status| status.changed_files.clone())
                .unwrap_or_default(),
            branch: self
                .git_status_replica
                .status
                .as_ref()
                .and_then(|status| status.branch.clone()),
            on_default_branch: self
                .git_status_replica
                .status
                .as_ref()
                .is_some_and(|status| status.is_default_branch),
        }
    }

    pub(crate) fn diff_active_state(&self) -> Option<DiffActiveState> {
        self.session_status_replica
            .as_ref()
            .map(|status| DiffActiveState {
                session: status.session_id.clone(),
                cwd: status.cwd.clone(),
                branches: status.branches.clone(),
            })
    }

    pub fn diff_turns(&self) -> Vec<usize> {
        self.with_active_timeline(|timeline| {
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

    pub fn diff_selected_turn(&self) -> Option<usize> {
        let turns = self.diff_turns();
        let explicit = self
            .active_conversation_ui()
            .and_then(|ui| ui.diff_selected_turn);
        match explicit {
            Some(turn) if turns.contains(&turn) => Some(turn),
            _ => turns.last().copied(),
        }
    }

    pub fn with_diff_turn_changes<R>(
        &self,
        turn: usize,
        read: impl FnOnce(&[agent::FileChange], agent::ChangeCompleteness) -> R,
    ) -> Option<R> {
        self.with_active_timeline(|timeline| {
            let changes = timeline.turns.get(turn)?.changes.as_ref()?;
            Some(read(&changes.changes, changes.completeness))
        })
        .flatten()
    }

    pub(crate) fn pending_diff_focus(&self) -> Option<DiffFocus> {
        self.active_conversation_ui()
            .and_then(|ui| ui.pending_diff_focus.clone())
    }

    /// UI-only consuming selector. The underlying diff focus is replica state;
    /// this does not cross the host boundary.
    pub(crate) fn take_diff_focus(&mut self, session: &str, turn: usize) -> Option<DiffFocus> {
        self.active_conversation_ui_mut()?
            .take_diff_focus(session, turn)
    }

    pub fn diff_refresh_generation(&self) -> u64 {
        self.active_conversation_ui()
            .map(|ui| ui.diff_refresh_generation)
            .unwrap_or(0)
    }

    pub fn diff_word_wrap(&self) -> bool {
        self.active_conversation_ui()
            .map(|ui| ui.diff_wrap)
            .unwrap_or(self.settings_replica.word_wrap_diffs)
    }

    pub fn diff_split(&self) -> bool {
        self.active_conversation_ui()
            .is_some_and(|ui| ui.diff_split)
    }

    pub fn set_diff_split(&mut self, split: bool, cx: &mut Context<Self>) {
        if let Some(ui) = self.active_conversation_ui_mut() {
            ui.diff_split = split;
            cx.notify();
        }
    }

    pub fn toggle_diff_wrap(&mut self, cx: &mut Context<Self>) {
        if let Some(ui) = self.active_conversation_ui_mut() {
            ui.diff_wrap = !ui.diff_wrap;
            cx.notify();
        }
    }

    pub(crate) fn panel_state(&self) -> PanelState {
        snapshots::panel_state(
            self.active_conversation_ui(),
            self.session_status_replica.as_ref(),
            self.session_replica.as_ref().map(|(_, timeline)| timeline),
        )
    }

    pub fn review_comments(&self) -> Vec<ReviewComment> {
        self.session_status_replica
            .as_ref()
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

    /// Ask the host to re-render a stored command's output at `cols`. The
    /// emulator is the host's; a client only ever asks for a width.
    pub fn render_stored_output(
        &self,
        session_id: String,
        item_id: String,
        cols: u16,
        cx: &mut App,
    ) -> Task<Result<TerminalFrame, String>> {
        let host = self.host.clone();
        cx.spawn(async move |_| {
            match host
                .query(Query::RenderStoredOutput {
                    session_id,
                    item_id,
                    cols,
                })
                .await
            {
                Ok(QueryResponse::TerminalFrame(frame)) => Ok(*frame),
                Ok(other) => Err(format!("unexpected stored-output response: {other:?}")),
                Err(error) => Err(error.message),
            }
        })
    }

    #[cfg(target_family = "wasm")]
    pub fn hosting(
        &self,
        action: tcode_protocol::HostingAction,
        cx: &mut App,
    ) -> Task<Result<tcode_protocol::HostingState, String>> {
        let host = self.host.clone();
        cx.spawn(
            async move |_| match host.query(Query::Hosting { action }).await {
                Ok(QueryResponse::Hosting(state)) => Ok(state),
                Ok(_) => Err("unexpected hosting response".into()),
                Err(error) => Err(error.message),
            },
        )
    }

    pub fn read_file_bytes(&self, path: PathBuf, cx: &mut App) -> Task<std::io::Result<Vec<u8>>> {
        let host = self.host.clone();
        cx.spawn(
            async move |_| match host.query(Query::ReadFileBytes { path }).await {
                Ok(QueryResponse::FileBytes(bytes)) => Ok(bytes),
                Ok(other) => Err(protocol_io_error(format!(
                    "unexpected file-bytes response: {other:?}"
                ))),
                Err(error) => Err(protocol_io_error(error.message)),
            },
        )
    }

    pub fn with_active_timeline<R>(&self, read: impl FnOnce(&Timeline) -> R) -> Option<R> {
        self.session_replica
            .as_ref()
            .map(|(_, timeline)| read(timeline))
    }

    pub(crate) fn pending_chat_turn(&self, session_id: &str) -> Option<usize> {
        self.pending_chat_turn
            .as_ref()
            .filter(|(id, _)| id == session_id)
            .and_then(|(_, turn)| turn.checked_sub(self.session_turn_offset))
    }

    pub(crate) fn take_pending_chat_turn(&mut self, session_id: &str, turn: usize) {
        if self.pending_chat_turn.as_ref()
            == Some(&(session_id.to_string(), turn + self.session_turn_offset))
        {
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
        self.select_session(session_id.clone());
        self.host
            .command_blocking(tcode_protocol::Command::ClearRelaunchMarker)
            .expect("subscription fence");
        while let Ok(envelope) = self.host.events().try_recv() {
            self.apply_domain_event(&envelope, cx);
        }
        self.session_replica = Some((session_id, timeline));
    }

    pub fn with_composer_destination<R>(
        &self,
        read: impl FnOnce(bool, &str, Option<&str>) -> R,
    ) -> Option<R> {
        self.session_status_replica.as_ref().map(|status| {
            read(
                status.draft,
                &status.session_id,
                status.project_id.as_deref(),
            )
        })
    }

    pub(crate) fn composer_state(&self) -> ComposerState {
        snapshots::composer_state(
            self.session_status_replica.as_ref(),
            self.session_replica.as_ref().map(|(_, timeline)| timeline),
            &self.settings_replica,
            &self.providers_replica,
        )
    }

    /// Consume the active session's prefill delivered by `NativeRewindPrefill`.
    pub fn take_native_rewind_prefill(&mut self) -> Option<String> {
        let active_id = self.session_status_replica.as_ref()?.session_id.clone();
        let prefill = self.native_rewind_prefills.remove(&active_id)?;
        if let Some(status) = self.session_status_replica.as_mut() {
            status.native_rewind_prefill_available = false;
        }
        Some(prefill)
    }

    /// The classifier stop the active session is currently showing, if any.
    pub fn active_fallback_block(&self) -> Option<&FallbackBlock> {
        let status = self.session_status_replica.as_ref()?;
        self.fallback_blocks.get(&status.session_id)
    }

    pub fn dismiss_fallback_block(&mut self) {
        if let Some(status) = self.session_status_replica.as_ref() {
            self.fallback_blocks.remove(&status.session_id);
        }
    }

    /// The advisory review of the active session's classifier stop, if any.
    pub fn active_fallback_review(&self) -> Option<&FallbackReview> {
        let status = self.session_status_replica.as_ref()?;
        self.fallback_reviews.get(&status.session_id)
    }

    pub fn dismiss_fallback_review(&mut self) {
        if let Some(status) = self.session_status_replica.as_ref() {
            self.fallback_reviews.remove(&status.session_id);
        }
    }

    /// The active session's last user message: its turn index and the words the
    /// user actually typed (any injected context prefix stripped).
    pub fn last_user_message(&self) -> Option<(usize, String)> {
        self.with_active_timeline(|timeline| {
            timeline.entries.iter().rev().find_map(|entry| {
                let EntryContent::Item(agent::ItemContent::UserMessage {
                    text, context_len, ..
                }) = &entry.content
                else {
                    return None;
                };
                let visible = context_len
                    .filter(|len| *len <= text.len() && text.is_char_boundary(*len))
                    .map_or(text.as_str(), |len| &text[len..]);
                Some((entry.turn, visible.to_string()))
            })
        })
        .flatten()
    }

    /// Build renderer handles from replicated layout. Local affordances keep
    /// direct PTY/grid access; otherwise the handles wrap client emulators.
    pub fn with_terminal_workspace<R>(
        &self,
        read: impl FnOnce(&TerminalWorkspace) -> R,
    ) -> Option<R> {
        let status = self.session_status_replica.as_ref()?;
        let workspace = TerminalWorkspace::from_replica(status, |id| self.client_terminal(id));
        Some(read(&workspace))
    }

    pub fn list_active_workspace(&self, cx: &mut App) -> Task<Vec<PathEntry>> {
        let session_id = self.active_session_id().unwrap_or_default();
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

    pub(crate) fn session_loading(&self) -> bool {
        self.selected_session_id.is_some()
            && (self.session_replica.is_none() || self.session_status_replica.is_none())
    }

    pub fn chat_active_session(&self) -> Option<(String, PathBuf, bool)> {
        self.session_status_replica
            .as_ref()
            .map(|status| (status.title.clone(), status.cwd.clone(), status.draft))
            .or_else(|| {
                let selected = self.selected_session_id.as_ref()?;
                self.index_replica
                    .0
                    .iter()
                    .find(|meta| &meta.id == selected)
                    .map(|meta| (meta.title.clone(), meta.cwd.clone(), false))
            })
    }

    pub fn chat_requested_model(&self) -> Option<String> {
        self.session_status_replica
            .as_ref()
            .and_then(|status| status.requested_model.clone())
    }

    pub fn chat_turn_changes(
        &self,
        turn: usize,
    ) -> (Vec<agent::FileChange>, agent::ChangeCompleteness) {
        self.with_active_timeline(|timeline| {
            timeline
                .turns
                .get(turn)
                .and_then(|turn| turn.changes.as_ref())
                .map(|changes| (changes.changes.clone(), changes.completeness))
        })
        .flatten()
        .unwrap_or((Vec::new(), agent::ChangeCompleteness::Partial))
    }

    pub fn chat_native_rewind_state(&self, turn: usize) -> Option<(bool, bool)> {
        let status = self.session_status_replica.as_ref()?;
        let has_checkpoint = self
            .with_active_timeline(|timeline| {
                timeline
                    .turns
                    .get(turn)
                    .and_then(|turn| turn.provider_checkpoint_id.as_ref())
                    .is_some()
            })
            .unwrap_or(false);
        Some((
            status.provider.caps().native_rewind && has_checkpoint,
            status.turn_running
                || !status.queued_messages.is_empty()
                || status.native_rewind_pending,
        ))
    }

    pub fn chat_git_controls(&self) -> Option<(QuickAction, Vec<MenuItem>)> {
        self.git_status_replica.status.as_ref().map(|status| {
            (
                quick_action(status, self.git_status_replica.busy),
                menu_items(status, self.git_status_replica.busy),
            )
        })
    }

    pub fn generate_commit_message(
        &self,
        included: Option<Vec<String>>,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let session_id = self.active_session_id().unwrap_or_default();
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

    pub fn plan_panel_state(&self) -> (Option<String>, Vec<agent::PlanStep>) {
        self.with_active_timeline(|timeline| {
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

    pub fn worktree_orphaned_by_delete(&self, session_id: &str) -> Option<WorktreeInfo> {
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

#[cfg(test)]
mod tests {
    use agent::{AgentEvent, ItemContent, ProviderKind, ThreadItem, TurnStatus};
    use gpui::{AppContext as _, TestAppContext};
    use tcode_core::{
        git::{GitFileEntry, GitStatus},
        project::{Project, SessionMeta},
        session::{ReviewComment, ReviewSide},
        settings::{Settings, ThemeMode},
    };
    use tcode_protocol::{Command, EventEnvelope, ServerEvent, SessionEventRecord, Topic};
    use tcode_runtime::host::HostEvent;
    use tcode_runtime::pipe::{HostServices, SpawnedHost, spawn_host};
    use tcode_services::store::SessionStore;

    use super::{
        ConversationDestination, WorkspaceAttachment, WorkspaceStore, effective_client_settings,
    };

    #[gpui::test]
    fn threads_wait_for_baseline_before_rendering_empty(cx: &mut TestAppContext) {
        use gpui::{px, size};
        cx.update(crate::theme::init);
        let (to_host, _outgoing) = async_channel::unbounded();
        let (_incoming, from_host) = async_channel::unbounded();
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                tcode_client::HostLink::new(to_host, from_host),
                super::WorkspaceAttachment::Remote {
                    host_id: "test".into(),
                    host_name: "Test".into(),
                },
                None,
                false,
                cx,
            )
        });
        let window_state =
            cx.new(|_| crate::window_state::WindowState::new(false).with_compact(true));
        let (_sidebar, cx) = cx.add_window_view(|_, cx| {
            crate::sidebar::SessionsSidebar::new(store.clone(), window_state, cx)
        });
        cx.simulate_resize(size(px(393.), px(852.)));
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert!(
            cx.debug_bounds("baseline-loading").is_some(),
            "Threads must render a skeleton before Index arrives"
        );
        assert!(cx.debug_bounds("threads-empty").is_none());
        store.update(cx, |store, cx| {
            store.apply_domain_event(
                &EventEnvelope {
                    request_id: None,
                    topic: Topic::Settings,
                    event: ServerEvent::SettingsSnapshot(Settings::default()),
                },
                cx,
            );
            store.apply_domain_event(
                &EventEnvelope {
                    request_id: None,
                    topic: Topic::Index,
                    event: ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                        sessions: vec![],
                        projects: vec![],
                        activity: Default::default(),
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
        assert!(cx.debug_bounds("baseline-loading").is_none());
        assert!(
            cx.debug_bounds("threads-empty").is_some(),
            "An applied empty baseline must render the empty message"
        );
    }

    #[test]
    fn client_preferences_persist_and_override_host_settings_only_when_set() {
        use tcode_client::host::{ClientHost as _, ClientPreferences};

        let root = std::env::temp_dir().join(format!(
            "tcode-desktop-preferences-{}",
            tcode_services::store::now_millis()
        ));
        let client = tcode_remote::NativeClientHost::new(root.clone(), "fallback device");
        let host = Settings {
            theme_mode: ThemeMode::Dark,
            language: Some(crate::LANGUAGE_SIMPLIFIED_CHINESE.into()),
            ..Settings::default()
        };

        assert_eq!(
            effective_client_settings(&host, &client.load_preferences()),
            host
        );

        client.save_preferences(&ClientPreferences {
            appearance: Some("light".into()),
            language: Some("system".into()),
            device_name: Some("Desk client".into()),
            ..Default::default()
        });
        let reloaded = tcode_remote::NativeClientHost::new(root.clone(), "different fallback");
        let preferences = reloaded.load_preferences();
        let effective = effective_client_settings(&host, &preferences);
        assert_eq!(effective.theme_mode, ThemeMode::Light);
        assert_eq!(effective.language, None);
        assert_eq!(reloaded.device_name(), "Desk client");

        reloaded.save_preferences(&ClientPreferences::default());
        assert_eq!(
            effective_client_settings(&host, &reloaded.load_preferences()),
            host,
            "clearing the client override must reveal the replicated host fallback"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[gpui::test]
    fn rapid_selection_is_immediate_idempotent_and_retires_old_loads(cx: &mut TestAppContext) {
        let (to_host, outgoing) = async_channel::unbounded();
        let (_incoming, from_host) = async_channel::unbounded();
        let host = tcode_client::HostLink::new(to_host, from_host);
        let workspace = cx.new(|cx| {
            WorkspaceStore::new_attached(host.clone(), WorkspaceAttachment::Local, None, false, cx)
        });
        workspace.update(cx, |store, cx| {
            let task_count = store.attachment_tasks.len();
            for index in 0..20 {
                let id = if index % 2 == 0 { "one" } else { "two" };
                store.select_session(id.into());
                assert_eq!(store.active_session_id().as_deref(), Some(id));
                assert!(store.session_loading());
                let messages = outgoing.len();
                store.select_session(id.into());
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
            store.apply_domain_event(
                &EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionEvents {
                        session_id: "one".into(),
                    },
                    event: ServerEvent::SessionSnapshot {
                        total: 0,
                        total_turns: 0,
                        truncated: false,
                        from: 0,
                        records: vec![],
                    },
                },
                cx,
            );
            assert!(
                store.session_loading(),
                "retired load must not replace the skeleton"
            );
            assert!(store.session_replica.is_none());
        });
    }

    #[gpui::test]
    fn paged_history_keeps_event_and_turn_cursors_absolute(cx: &mut TestAppContext) {
        let (to_host, outgoing) = async_channel::unbounded();
        let (_incoming, from_host) = async_channel::unbounded();
        let host = tcode_client::HostLink::new(to_host, from_host);
        let workspace = cx.new(|cx| {
            WorkspaceStore::new_attached(host.clone(), WorkspaceAttachment::Local, None, false, cx)
        });
        workspace.update(cx, |store, cx| {
            store.select_session("large".into());
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
            store.apply_domain_event(
                &EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionEvents {
                        session_id: "large".into(),
                    },
                    event: ServerEvent::SessionSnapshot {
                        from: 1800,
                        records,
                        total: 2000,
                        total_turns: 500,
                        truncated: false,
                    },
                },
                cx,
            );
            assert_eq!(store.session_turn_offset, 450);
            assert_eq!(
                host.subscriptions()
                    .iter()
                    .find(|sub| matches!(sub.topic, Topic::SessionEvents { .. }))
                    .unwrap()
                    .after,
                Some(2000)
            );
            while outgoing.try_recv().is_ok() {}
            store.rewind_turn(2, agent::RewindMode::Conversation);
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
            let retained_entry = store.session_replica.as_ref().unwrap().1.entries[0].clone();
            store.apply_domain_event(
                &EventEnvelope {
                    request_id: None,
                    topic: topic.clone(),
                    event: ServerEvent::SessionSnapshot {
                        from: 2000,
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
                    &store.session_replica.as_ref().unwrap().1.entries[0]
                ),
                "empty replay must retain the existing timeline"
            );
        });
    }

    #[gpui::test]
    fn history_prefetch_keeps_one_bounded_page_in_flight(cx: &mut TestAppContext) {
        let root = std::env::temp_dir().join(format!(
            "tcode-prefetch-test-{}",
            tcode_services::store::now_millis()
        ));
        let host = test_host(SessionStore::open_at(root.clone()).unwrap());
        let status = smol::block_on(host.update_state_for_test(|state, cx| {
            let id = state.start_draft("history".into(), std::env::temp_dir(), cx);
            state.session_status_snapshot(&id).unwrap()
        }))
        .unwrap();
        shutdown_test_host(&host);
        std::fs::remove_dir_all(root).unwrap();

        let (to_host, outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump_link = link.clone();
        let _pump = cx
            .background_executor
            .spawn(async move { pump_link.pump().await });
        let workspace = cx.new(|cx| {
            WorkspaceStore::new_attached(link, WorkspaceAttachment::Local, None, false, cx)
        });
        workspace.update(cx, |store, cx| {
            store.selected_session_id = Some("large".into());
            store.session_status_replica = Some(status);
            store.apply_domain_event(
                &EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionEvents {
                        session_id: "large".into(),
                    },
                    event: ServerEvent::SessionSnapshot {
                        from: 1800,
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
        workspace.update(cx, |store, cx| store.update_history_window(1., cx));
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
            assert!(store.history_loading());
            store.load_earlier_messages(cx);
        });
        cx.run_until_parked();
        assert!(
            outgoing.try_recv().is_err(),
            "scrolling while loading must not queue another page"
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
                            truncated: false,
                        }),
                    })
                    .unwrap(),
                )
                .unwrap();
            wait_until(cx, &workspace, "prefetched page applied", |cx| {
                workspace.read_with(cx, |store, _| store.session_from["large"] == before - 200)
            });
            assert!(
                outgoing.try_recv().is_err(),
                "yield between automatic pages"
            );
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(250));
            cx.run_until_parked();
            workspace.update(cx, |store, cx| {
                store.update_history_window(if page < 3 { 2. + page as f32 } else { 6. }, cx);
            });
            cx.run_until_parked();
            if page < 3 {
                wait_until(cx, &workspace, "next prefetch request", |_| {
                    !outgoing.is_empty()
                });
                request =
                    tcode_protocol::decode_client_line(&outgoing.try_recv().unwrap()).unwrap();
            }
        }
        assert!(
            outgoing.try_recv().is_err(),
            "stop when six screens are covered, without a scroll event"
        );
        workspace.read_with(cx, |store, _| {
            assert_eq!(store.session_from["large"], 1000);
            assert_eq!(store.session_records["large"].len(), 1000);
            assert!(!store.history_loading());
        });
    }

    fn test_host(store: SessionStore) -> SpawnedHost {
        spawn_host(store, HostServices::default()).expect("spawn test host")
    }

    macro_rules! update_host {
        ($host:expr, $update:expr) => {
            smol::block_on($host.update_state_for_test($update)).expect("update test host")
        };
    }

    fn command(host: &SpawnedHost, command: Command) {
        smol::block_on(host.link().command(command)).expect("typed host command");
    }

    fn shutdown_test_host(host: &SpawnedHost) {
        host.shutdown_blocking()
            .expect("drain test store and stop host");
    }

    fn wait_until(
        cx: &mut TestAppContext,
        workspace: &gpui::Entity<WorkspaceStore>,
        description: &str,
        ready: impl Fn(&TestAppContext) -> bool,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            workspace.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            cx.run_until_parked();
            if ready(cx) {
                return;
            }
            smol::block_on(smol::Timer::after(std::time::Duration::from_millis(1)));
        }
        panic!("timed out waiting for {description}");
    }

    #[gpui::test]
    fn reconnect_retains_replicas_until_all_selected_thread_baselines_arrive(
        cx: &mut TestAppContext,
    ) {
        use tcode_client::ConnectionState;
        let root = scratch_root("baseline-replay");
        let disk = SessionStore::open_at(root.clone()).unwrap();
        disk.upsert_project(&project_at("p", &root)).unwrap();
        disk.upsert_meta(&thread(&root, "one", "p", None)).unwrap();
        let host = test_host(disk);
        let workspace = cx.new(|cx| WorkspaceStore::new(host.link(), cx));
        workspace.update(cx, |store, _| store.select_session("one".into()));
        wait_until(cx, &workspace, "selected thread baseline", |cx| {
            workspace.read_with(cx, |store, _| store.baseline_ready())
        });
        workspace.update(cx, |store, cx| {
            let status = store.session_status_replica.clone().unwrap();
            let snapshots = [
                (
                    Topic::Index,
                    ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                        sessions: store.index_replica.0.clone(),
                        projects: store.index_replica.1.clone(),
                        activity: Default::default(),
                    }),
                ),
                (
                    Topic::Settings,
                    ServerEvent::SettingsSnapshot(store.settings_replica.clone()),
                ),
                (
                    Topic::SessionStatus {
                        session_id: "one".into(),
                    },
                    ServerEvent::SessionStatusReplaced(status),
                ),
                (
                    Topic::SessionEvents {
                        session_id: "one".into(),
                    },
                    ServerEvent::SessionSnapshot {
                        from: 0,
                        records: vec![],
                        total: 0,
                        total_turns: 0,
                        truncated: false,
                    },
                ),
            ];
            store.apply_connection_state(ConnectionState::Reconnecting {
                attempt: 2,
                reason: None,
            });
            // Old socket events can already be queued when loss is published.
            for (topic, event) in &snapshots {
                store.apply_domain_event(
                    &EventEnvelope {
                        request_id: None,
                        topic: topic.clone(),
                        event: event.clone(),
                    },
                    cx,
                );
            }
            assert!(store.baseline_ready());
            store.apply_connection_state(ConnectionState::Syncing);
            assert!(!store.threads_loading(), "cached list remains visible");
            assert!(!store.chat_loading(), "cached thread remains visible");
            assert_eq!(store.active_session_id().as_deref(), Some("one"));
            for (topic, event) in snapshots {
                assert_eq!(store.connection_state(), &ConnectionState::Syncing);
                store.apply_domain_event(
                    &EventEnvelope {
                        request_id: None,
                        topic,
                        event,
                    },
                    cx,
                );
            }
            assert_eq!(store.connection_state(), &ConnectionState::Connected);
        });
        host.shutdown_blocking().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    fn scratch_root(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "tcode-{label}-{}",
            tcode_services::store::now_millis()
        ))
    }

    fn project_at(id: &str, root: &std::path::Path) -> Project {
        Project {
            id: id.into(),
            name: id.into(),
            root: root.to_path_buf(),
            created_at: 0,
        }
    }

    fn thread(
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
        workspace.read_with(cx, |store, _| {
            store
                .session_status_replica
                .as_ref()
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

    fn archived(cx: &TestAppContext, workspace: &gpui::Entity<WorkspaceStore>, id: &str) -> bool {
        workspace.read_with(cx, |store, _| {
            store
                .index_replica
                .0
                .iter()
                .any(|meta| meta.id == id && meta.archived_at.is_some())
        })
    }

    /// An Orchestrate child auto-archived on completion hands the workspace to
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
        let workspace = cx.new(|cx| WorkspaceStore::new(host.link(), cx));

        workspace.update(cx, |store, _| store.select_session("parent".into()));
        wait_until(cx, &workspace, "parent selected", |cx| {
            selected_status(cx, &workspace, "parent")
        });
        workspace.update(cx, |store, _| store.select_session("child".into()));
        wait_until(cx, &workspace, "child selected", |cx| {
            selected_status(cx, &workspace, "child")
        });

        command(
            &host,
            Command::ArchiveSession {
                session_id: "child".into(),
            },
        );
        wait_until(cx, &workspace, "parent reopened", |cx| {
            selected_status(cx, &workspace, "parent")
        });
        workspace.read_with(cx, |store, _| {
            assert!(
                store.session_records.contains_key("parent"),
                "the parent's replicated records were dropped on the way back"
            );
        });
        assert!(archived(cx, &workspace, "child"));

        command(
            &host,
            Command::ArchiveSession {
                session_id: "sibling".into(),
            },
        );
        wait_until(cx, &workspace, "sibling archived", |cx| {
            archived(cx, &workspace, "sibling")
        });
        assert!(
            selected_status(cx, &workspace, "parent"),
            "archiving a background thread moved the user"
        );

        shutdown_test_host(&host);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Archiving a parent archives its children in one batch. Viewing one of
    /// those children leaves no visible parent to return to, so the workspace
    /// falls back to the standing draft of the last interacted project —
    /// the same draft session id, so its composer state survives.
    #[gpui::test]
    fn batch_archive_without_a_visible_parent_reopens_the_standing_draft(cx: &mut TestAppContext) {
        let root = scratch_root("archive-to-draft");
        let disk = SessionStore::open_at(root.clone()).expect("open test store");
        // "other" is listed first, so a draft for it proves nothing about the
        // remembered project; "p" is the one the user last worked in.
        disk.upsert_project(&project_at("other", &root.join("other")))
            .expect("persist project");
        disk.upsert_project(&project_at("p", &root))
            .expect("persist project");
        for meta in [
            thread(&root, "parent", "p", None),
            thread(&root, "child", "p", Some("parent")),
        ] {
            disk.upsert_meta(&meta).expect("persist session");
        }
        let host = test_host(disk);
        let workspace = cx.new(|cx| WorkspaceStore::new(host.link(), cx));

        // Let the launch fallback settle on the first project before the user
        // navigates into "p" themselves.
        wait_until(cx, &workspace, "launch draft", |cx| {
            workspace.read_with(cx, |store, _| store.selected_session_id.is_some())
        });
        workspace.update(cx, |store, cx| {
            store.start_draft("p".into(), root.clone(), cx)
        });
        wait_until(cx, &workspace, "draft for the last project", |cx| {
            workspace.read_with(cx, |store, _| {
                store
                    .session_status_replica
                    .as_ref()
                    .is_some_and(|status| status.draft && status.project_id.as_deref() == Some("p"))
            })
        });
        let draft_id = workspace
            .read_with(cx, |store, _| store.selected_session_id.clone())
            .expect("draft selected");
        workspace.update(cx, |store, _| {
            store
                .conversation_ui
                .get_mut(&ConversationDestination::ProjectDraft("p".into()))
                .expect("draft conversation state")
                .right_panel_open = true;
        });

        workspace.update(cx, |store, _| store.select_session("child".into()));
        wait_until(cx, &workspace, "child selected", |cx| {
            selected_status(cx, &workspace, "child")
        });

        command(
            &host,
            Command::ArchiveSession {
                session_id: "parent".into(),
            },
        );
        wait_until(cx, &workspace, "the standing draft reopened", |cx| {
            workspace.read_with(cx, |store, _| {
                store.selected_session_id.as_deref() == Some(draft_id.as_str())
            })
        });
        workspace.read_with(cx, |store, _| {
            assert!(
                store
                    .conversation_ui
                    .get(&ConversationDestination::ProjectDraft("p".into()))
                    .is_some_and(|ui| ui.right_panel_open),
                "the reopened draft lost the state the user left in it"
            );
        });
        assert!(archived(cx, &workspace, "parent"));
        assert!(archived(cx, &workspace, "child"));

        shutdown_test_host(&host);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Launching with nothing selected opens the remembered project's new
    /// thread page instead of a dead empty page; a workspace with no project
    /// at all keeps its add-project state.
    #[gpui::test]
    fn a_workspace_with_no_conversation_opens_the_remembered_projects_draft(
        cx: &mut TestAppContext,
    ) {
        let root = scratch_root("remembered-project");
        let disk = SessionStore::open_at(root.clone()).expect("open test store");
        disk.upsert_project(&project_at("first", &root.join("first")))
            .expect("persist project");
        disk.upsert_project(&project_at("remembered", &root))
            .expect("persist project");
        let host = test_host(disk);
        command(
            &host,
            Command::PatchSettings {
                patch: tcode_core::settings::SettingsPatch::LastProject(Some("remembered".into())),
            },
        );
        let workspace = cx.new(|cx| WorkspaceStore::new(host.link(), cx));
        wait_until(cx, &workspace, "remembered project draft", |cx| {
            workspace.read_with(cx, |store, _| {
                store.session_status_replica.as_ref().is_some_and(|status| {
                    status.draft && status.project_id.as_deref() == Some("remembered")
                })
            })
        });
        shutdown_test_host(&host);
        let _ = std::fs::remove_dir_all(&root);

        let empty_root = scratch_root("no-projects");
        let empty_host = test_host(SessionStore::open_at(empty_root.clone()).expect("open store"));
        let empty = cx.new(|cx| WorkspaceStore::new(empty_host.link(), cx));
        for _ in 0..5 {
            empty.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            cx.run_until_parked();
        }
        empty.read_with(cx, |store, _| {
            assert!(store.projects().is_empty());
            assert_eq!(
                store.selected_session_id, None,
                "a workspace with no project must stay on its add-project state"
            );
        });
        shutdown_test_host(&empty_host);
        let _ = std::fs::remove_dir_all(&empty_root);
    }

    #[gpui::test]
    fn reconnect_and_mismatched_tail_preserve_exactly_one_copy_of_each_record(
        cx: &mut TestAppContext,
    ) {
        let root = std::env::temp_dir().join(format!(
            "p4a-reconnect-{}",
            tcode_services::store::now_millis()
        ));
        let disk = SessionStore::open_at(root.clone()).unwrap();
        let mut meta = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
        meta.id = "reconnect".into();
        disk.upsert_meta(&meta).unwrap();
        let host = test_host(disk);
        let workspace = cx.new(|cx| WorkspaceStore::new(host.link(), cx));
        workspace.update(cx, |store, _| store.select_session("reconnect".into()));
        wait_until(cx, &workspace, "selected status", |cx| {
            workspace.read_with(cx, |store, _| store.session_status_replica.is_some())
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
        wait_until(cx, &workspace, "three records", |cx| {
            workspace.read_with(cx, |store, _| store.session_records["reconnect"].len() == 3)
        });
        host.link()
            .set_connection_state(tcode_client::ConnectionState::Reconnecting {
                attempt: 1,
                reason: None,
            });
        host.link()
            .set_connection_state(tcode_client::ConnectionState::Connected);
        command(&host, Command::ClearRelaunchMarker);
        workspace.update(cx, |store, cx| {
            store.drain_host_events_for_test(cx);
            assert_eq!(store.session_records["reconnect"].len(), 3);
            store.apply_domain_event(
                &EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionEvents {
                        session_id: "reconnect".into(),
                    },
                    event: ServerEvent::SessionSnapshot {
                        total: 0,
                        total_turns: 0,
                        truncated: false,
                        from: 2,
                        records: vec![],
                    },
                },
                cx,
            );
            assert!(
                store.session_replica.is_none(),
                "invalid tail must request a full replacement"
            );
        });
        wait_until(
            cx,
            &workspace,
            "full replacement after invalid tail",
            |cx| workspace.read_with(cx, |store, _| store.session_records["reconnect"].len() == 3),
        );
        workspace.read_with(cx, |store, _| {
            assert_eq!(
                store.session_records["reconnect"]
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
        shutdown_test_host(&host);
        // Windows keeps the just-flushed JSONL handle briefly after shutdown;
        // the directory is a temp dir, so retry and then give up quietly.
        for _ in 0..20 {
            if std::fs::remove_dir_all(&root).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

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

        let host = test_host(session_store);
        let workspace = cx.new(|cx| WorkspaceStore::new(host.link(), cx));
        workspace.update(cx, |store, _| store.select_session(session_id.clone()));
        wait_until(cx, &workspace, "initial session timeline replica", |cx| {
            workspace.read_with(cx, |store, _| {
                store
                    .session_replica
                    .as_ref()
                    .is_some_and(|(id, timeline)| id == &session_id && timeline.turns.len() == 1)
            })
        });

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
            let event_session_id = session_id.clone();
            update_host!(&host, move |state, cx| {
                state.record_event_for_replica_test(
                    &event_session_id,
                    200 + offset as u64,
                    &event,
                    cx,
                );
            });
        }
        let target_id = session_id.clone();
        let live = update_host!(&host, move |state, _| {
            let timeline = &state
                .residents
                .live
                .get(&target_id)
                .expect("selected session")
                .timeline;
            (
                timeline
                    .entries
                    .iter()
                    .map(|entry| (entry.id.clone(), entry.turn, format!("{:?}", entry.content)))
                    .collect::<Vec<_>>(),
                timeline.turns.len(),
            )
        });
        // Wait for the replica to catch up to the live timeline's shape before
        // comparing contents; turn count alone flips at TurnStarted, while later
        // events may still be queued.
        wait_until(
            cx,
            &workspace,
            "incremental session timeline replica",
            |cx| {
                workspace.read_with(cx, |store, _| {
                    store.session_replica.as_ref().is_some_and(|(_, timeline)| {
                        timeline.turns.len() == live.1 && timeline.entries.len() == live.0.len()
                    })
                })
            },
        );
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

        shutdown_test_host(&host);
        std::fs::remove_dir_all(root).expect("remove test data");
    }

    #[gpui::test]
    fn index_and_settings_replicas_follow_representative_commands(cx: &mut TestAppContext) {
        let root = std::env::temp_dir().join(format!(
            "tcode-replica-consistency-test-{}",
            tcode_services::store::now_millis()
        ));
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
        let workspace = cx.new(|cx| WorkspaceStore::new(host.link(), cx));
        wait_until(cx, &workspace, "initial session index", |cx| {
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
        command(&host, Command::CreateProject { root: created_root });
        command(
            &host,
            Command::ArchiveSession {
                session_id: seed_session_id.clone(),
            },
        );
        let mut settings = workspace.read_with(cx, |store, _cx| store.settings());
        settings.word_wrap_diffs = !settings.word_wrap_diffs;
        let expected_word_wrap = settings.word_wrap_diffs;
        command(
            &host,
            Command::PatchSettings {
                patch: tcode_protocol::SettingsPatch::WordWrapDiffs(settings.word_wrap_diffs),
            },
        );
        wait_until(cx, &workspace, "index and settings replicas", |cx| {
            workspace.read_with(cx, |store, _| {
                store.index_replica.1.len() == 2
                    && store
                        .index_replica
                        .0
                        .iter()
                        .find(|meta| meta.id == seed_session_id)
                        .is_some_and(|meta| meta.archived_at.is_some())
                    && store.settings_replica.word_wrap_diffs == expected_word_wrap
            })
        });

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
            assert!(
                store
                    .archived_groups()
                    .iter()
                    .any(|group| { group.sessions.iter().any(|meta| meta.id == seed_session_id) })
            );
        });

        let live_index = update_host!(&host, |state, _| {
            (
                serde_json::to_value(&state.sessions).unwrap(),
                serde_json::to_value(&state.projects).unwrap(),
            )
        });
        let live_settings = update_host!(&host, |state, _| {
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

        shutdown_test_host(&host);
        std::fs::remove_dir_all(root).expect("remove test data");
    }

    #[gpui::test]
    fn session_status_replica_matches_live_after_queue_and_interaction_mode_change(
        cx: &mut TestAppContext,
    ) {
        let root = std::env::temp_dir().join(format!(
            "tcode-session-status-replica-consistency-test-{}",
            tcode_services::store::now_millis()
        ));
        let session_store = SessionStore::open_at(root.clone()).expect("open test store");
        let meta = SessionMeta::new(ProviderKind::Codex, root.join("worktree"), None);
        let session_id = meta.id.clone();
        session_store.upsert_meta(&meta).expect("persist session");

        let host = test_host(session_store);
        let workspace = cx.new(|cx| WorkspaceStore::new(host.link(), cx));
        workspace.update(cx, |store, _| store.select_session(session_id.clone()));
        wait_until(cx, &workspace, "selected session status", |cx| {
            workspace.read_with(cx, |store, _| {
                store
                    .session_status_replica
                    .as_ref()
                    .is_some_and(|status| status.session_id == session_id)
            })
        });
        let target_id = session_id.clone();
        update_host!(&host, move |state, cx| {
            state.queue_message_for_replica_test(&target_id, "queued for replication".into(), cx);
        });
        command(
            &host,
            Command::SetInteractionMode {
                session_id: session_id.clone(),
                mode: agent::InteractionMode::Plan,
            },
        );
        command(
            &host,
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
        wait_until(cx, &workspace, "queued-message and review replicas", |cx| {
            workspace.read_with(cx, |store, _| {
                store.session_status_replica.as_ref().is_some_and(|status| {
                    status.queued_messages.len() == 1
                        && status.interaction_mode == agent::InteractionMode::Plan
                        && status.review_comment_drafts.len() == 1
                })
            })
        });

        let live = update_host!(&host, move |state, _| {
            state
                .session_status_snapshot(&session_id)
                .expect("live session status")
        });
        let replica = workspace.read_with(cx, |store, _| {
            store
                .session_status_replica
                .clone()
                .expect("session status replica")
        });

        assert_eq!(replica, live);
        assert_eq!(replica.queued_messages.len(), 1);
        assert_eq!(replica.queued_messages[0].text, "queued for replication");
        assert_eq!(replica.interaction_mode, agent::InteractionMode::Plan);
        assert_eq!(replica.review_comment_drafts.len(), 1);
        assert_eq!(
            workspace.read_with(cx, |store, _cx| store.review_comments()),
            replica.review_comment_drafts
        );

        shutdown_test_host(&host);
        std::fs::remove_dir_all(root).expect("remove test data");
    }

    #[gpui::test]
    fn background_session_status_tracks_pending_user_input(cx: &mut TestAppContext) {
        let root = std::env::temp_dir().join(format!(
            "tcode-background-user-input-status-test-{}",
            tcode_services::store::now_millis()
        ));
        let session_store = SessionStore::open_at(root.clone()).expect("open test store");
        let meta = SessionMeta::new(ProviderKind::Codex, root.join("worktree"), None);
        let session_id = meta.id.clone();
        session_store.upsert_meta(&meta).expect("persist session");

        let host = test_host(session_store);
        let workspace = cx.new(|cx| WorkspaceStore::new(host.link(), cx));
        workspace.update(cx, |store, _| store.select_session(session_id.clone()));
        wait_until(cx, &workspace, "selected session status", |cx| {
            workspace.read_with(cx, |store, _| {
                store
                    .session_status_replica
                    .as_ref()
                    .is_some_and(|status| status.session_id == session_id)
            })
        });

        let background_session_id = "background-session".to_string();
        workspace.update(cx, |store, cx| {
            let mut status = store
                .session_status_replica
                .clone()
                .expect("active session status");
            status.session_id = background_session_id.clone();
            status.pending_user_input = true;
            store.apply_domain_event(
                &EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionStatus {
                        session_id: background_session_id.clone(),
                    },
                    event: ServerEvent::SessionStatusReplaced(status),
                },
                cx,
            );
        });

        assert!(workspace.read_with(cx, |store, _cx| {
            store.pending_user_input_for(&background_session_id)
        }));

        shutdown_test_host(&host);
        std::fs::remove_dir_all(root).expect("remove test data");
    }

    #[gpui::test]
    fn active_session_handoff_preserves_parked_working_status(cx: &mut TestAppContext) {
        let root = std::env::temp_dir().join(format!(
            "tcode-background-working-handoff-test-{}",
            tcode_services::store::now_millis()
        ));
        let session_store = SessionStore::open_at(root.clone()).expect("open test store");
        let first = SessionMeta::new(ProviderKind::Codex, root.join("first"), None);
        let second = SessionMeta::new(ProviderKind::Codex, root.join("second"), None);
        session_store
            .upsert_meta(&first)
            .expect("persist first session");
        session_store
            .upsert_meta(&second)
            .expect("persist second session");

        let host = test_host(session_store);
        let workspace = cx.new(|cx| WorkspaceStore::new(host.link(), cx));
        workspace.update(cx, |store, _| store.select_session(first.id.clone()));
        wait_until(cx, &workspace, "first selected session", |cx| {
            workspace.read_with(cx, |store, _| {
                store
                    .session_status_replica
                    .as_ref()
                    .is_some_and(|status| status.session_id == first.id)
            })
        });

        workspace.update(cx, |store, cx| {
            let mut parked = store
                .session_status_replica
                .clone()
                .expect("first session status");
            parked.turn_running = true;
            parked.working = true;
            store.apply_domain_event(
                &EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionStatus {
                        session_id: first.id.clone(),
                    },
                    event: ServerEvent::SessionStatusReplaced(parked.clone()),
                },
                cx,
            );

            let mut next = parked;
            next.session_id = second.id.clone();
            next.cwd = second.cwd.clone();
            next.turn_running = false;
            next.working = false;
            store.apply_domain_event(
                &EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionStatus {
                        session_id: second.id.clone(),
                    },
                    event: ServerEvent::SessionStatusReplaced(next.clone()),
                },
                cx,
            );
            store.select_session(next.session_id.clone());
            store.apply_domain_event(
                &EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionStatus {
                        session_id: next.session_id.clone(),
                    },
                    event: ServerEvent::SessionStatusReplaced(next),
                },
                cx,
            );
        });

        assert!(workspace.read_with(cx, |store, _cx| { store.turn_running_for(&first.id) }));
        assert!(!workspace.read_with(cx, |store, _cx| { store.turn_running_for(&second.id) }));
        assert_eq!(
            workspace.read_with(cx, |store, _cx| store.working_sessions_count()),
            1
        );

        shutdown_test_host(&host);
        std::fs::remove_dir_all(root).expect("remove test data");
    }

    #[gpui::test]
    fn native_rewind_prefill_events_remain_keyed_to_parked_sessions(cx: &mut TestAppContext) {
        let root = std::env::temp_dir().join(format!(
            "tcode-native-rewind-replica-test-{}",
            tcode_services::store::now_millis()
        ));
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
        let workspace = cx.new(|cx| WorkspaceStore::new(host.link(), cx));
        workspace.update(cx, |store, _| store.select_session(first.id.clone()));
        wait_until(cx, &workspace, "first selected session", |cx| {
            workspace.read_with(cx, |store, _| {
                store
                    .session_status_replica
                    .as_ref()
                    .is_some_and(|status| status.session_id == first.id)
            })
        });

        // This replica test deliberately owns both status subscriptions. Ordinary
        // navigation owns only its selected session; mux isolation is tested separately.
        host.link()
            .subscribe(tcode_protocol::Subscription {
                topic: Topic::SessionStatus {
                    session_id: second.id.clone(),
                },
                after: None,
            })
            .unwrap();
        for (session_id, text) in [
            (first.id.clone(), "first parked prefill".to_string()),
            (second.id.clone(), "second parked prefill".to_string()),
        ] {
            update_host!(&host, move |_state, cx| {
                cx.emit(HostEvent::Domain(EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionStatus {
                        session_id: session_id.clone(),
                    },
                    event: ServerEvent::NativeRewindPrefill { session_id, text },
                }));
            });
        }
        wait_until(cx, &workspace, "both rewind prefill events", |cx| {
            workspace.read_with(cx, |store, _| store.native_rewind_prefills.len() == 2)
        });

        assert_eq!(
            workspace.update(cx, |store, _cx| store.take_native_rewind_prefill()),
            Some("first parked prefill".into())
        );
        workspace.update(cx, |store, _| store.select_session(second.id.clone()));
        wait_until(cx, &workspace, "second selected session", |cx| {
            workspace.read_with(cx, |store, _| {
                store
                    .session_status_replica
                    .as_ref()
                    .is_some_and(|status| status.session_id == second.id)
            })
        });
        assert_eq!(
            workspace.update(cx, |store, _cx| store.take_native_rewind_prefill()),
            Some("second parked prefill".into())
        );

        shutdown_test_host(&host);
        std::fs::remove_dir_all(root).expect("remove test data");
    }

    #[gpui::test]
    fn classifier_block_survives_until_the_next_turn_starts(cx: &mut TestAppContext) {
        let root = std::env::temp_dir().join(format!(
            "tcode-fallback-block-test-{}",
            tcode_services::store::now_millis()
        ));
        let session_store = SessionStore::open_at(root.clone()).expect("open test store");
        let meta = SessionMeta::new(ProviderKind::ClaudeCode, root.join("worktree"), None);
        session_store.upsert_meta(&meta).expect("persist session");

        let host = test_host(session_store);
        let workspace = cx.new(|cx| WorkspaceStore::new(host.link(), cx));
        workspace.update(cx, |store, _| store.select_session(meta.id.clone()));
        wait_until(cx, &workspace, "selected session", |cx| {
            workspace.read_with(cx, |store, _| {
                store
                    .session_status_replica
                    .as_ref()
                    .is_some_and(|status| status.session_id == meta.id)
            })
        });

        let session_id = meta.id.clone();
        update_host!(&host, move |_state, cx| {
            cx.emit(HostEvent::Domain(EventEnvelope {
                request_id: None,
                topic: Topic::SessionStatus {
                    session_id: session_id.clone(),
                },
                event: ServerEvent::ModelFallbackBlocked {
                    session_id,
                    category: Some(agent::ClassifierCategory::Cyber),
                    model: Some("claude-sonnet-4-5".into()),
                    fallback_model: None,
                    detail: "request blocked by classifier".into(),
                },
            }));
        });
        wait_until(cx, &workspace, "classifier block", |cx| {
            workspace.read_with(cx, |store, _| store.active_fallback_block().is_some())
        });

        let session_id = meta.id.clone();
        update_host!(&host, move |_state, cx| {
            cx.emit(HostEvent::Domain(EventEnvelope {
                request_id: None,
                topic: Topic::SessionEvents {
                    session_id: session_id.clone(),
                },
                event: ServerEvent::SessionEvent(SessionEventRecord {
                    ts: None,
                    event: AgentEvent::TurnStarted {
                        turn_id: "turn-next".into(),
                    },
                }),
            }));
        });
        wait_until(cx, &workspace, "block cleared by the next turn", |cx| {
            workspace.read_with(cx, |store, _| store.active_fallback_block().is_none())
        });

        shutdown_test_host(&host);
        std::fs::remove_dir_all(root).expect("remove test data");
    }

    #[gpui::test]
    fn fallback_review_survives_until_the_next_turn_starts(cx: &mut TestAppContext) {
        let root = std::env::temp_dir().join(format!(
            "tcode-fallback-review-test-{}",
            tcode_services::store::now_millis()
        ));
        let session_store = SessionStore::open_at(root.clone()).expect("open test store");
        let meta = SessionMeta::new(ProviderKind::ClaudeCode, root.join("worktree"), None);
        session_store.upsert_meta(&meta).expect("persist session");

        let host = test_host(session_store);
        let workspace = cx.new(|cx| WorkspaceStore::new(host.link(), cx));
        workspace.update(cx, |store, _| store.select_session(meta.id.clone()));
        wait_until(cx, &workspace, "selected session", |cx| {
            workspace.read_with(cx, |store, _| {
                store
                    .session_status_replica
                    .as_ref()
                    .is_some_and(|status| status.session_id == meta.id)
            })
        });

        let session_id = meta.id.clone();
        update_host!(&host, move |_state, cx| {
            cx.emit(HostEvent::Domain(EventEnvelope {
                request_id: None,
                topic: Topic::SessionStatus {
                    session_id: session_id.clone(),
                },
                event: ServerEvent::FallbackReviewReady {
                    session_id,
                    assessment: "looks like a false positive".into(),
                    draft: "I am auditing my own service.".into(),
                },
            }));
        });
        wait_until(cx, &workspace, "review ready", |cx| {
            workspace.read_with(cx, |store, _| store.active_fallback_review().is_some())
        });

        let session_id = meta.id.clone();
        update_host!(&host, move |_state, cx| {
            cx.emit(HostEvent::Domain(EventEnvelope {
                request_id: None,
                topic: Topic::SessionEvents {
                    session_id: session_id.clone(),
                },
                event: ServerEvent::SessionEvent(SessionEventRecord {
                    ts: None,
                    event: AgentEvent::TurnStarted {
                        turn_id: "turn-next".into(),
                    },
                }),
            }));
        });
        wait_until(cx, &workspace, "review cleared by the next turn", |cx| {
            workspace.read_with(cx, |store, _| store.active_fallback_review().is_none())
        });

        shutdown_test_host(&host);
        std::fs::remove_dir_all(root).expect("remove test data");
    }

    #[gpui::test]
    fn providers_and_git_replicas_match_live_after_representative_mutations(
        cx: &mut TestAppContext,
    ) {
        let root = std::env::temp_dir().join(format!(
            "tcode-provider-git-replica-consistency-test-{}",
            tcode_services::store::now_millis()
        ));
        let session_store = SessionStore::open_at(root.clone()).expect("open test store");
        let mut meta = SessionMeta::new(ProviderKind::Codex, root.join("worktree"), None);
        meta.id = "git-replica".into();
        session_store.upsert_meta(&meta).unwrap();
        let host = test_host(session_store);
        let workspace = cx.new(|cx| WorkspaceStore::new(host.link(), cx));

        workspace.update(cx, |store, _| store.select_session("git-replica".into()));
        // Subscribing adopts the session and spawns a real git probe of the
        // (non-repo) cwd. Let that probe land before injecting the fixture
        // status, or its late result overwrites the injected one.
        wait_until(cx, &workspace, "initial git probe", |cx| {
            workspace.read_with(cx, |store, _| store.git_status_replica.status.is_some())
        });
        command(&host, Command::ClearRelaunchMarker);
        update_host!(&host, |state, _cx| {
            state.acp_registry = Some(
                serde_json::from_value(serde_json::json!({
                    "agents": [{
                        "id": "replicated-agent",
                        "name": "Replicated Agent",
                        "version": "1.0.0",
                        "description": "registry refresh result",
                        "distribution": { "npx": { "package": "replicated-agent" } }
                    }]
                }))
                .expect("registry fixture"),
            );
            state.acp_registry_loading = false;
            state.acp_registry_error = None;
            state
                .providers
                .provider_versions
                .entry(ProviderKind::Codex)
                .or_default()
                .checking = true;
            state.git_status.insert(
                "git-replica".into(),
                GitStatus {
                    is_repo: true,
                    branch: Some("feature/replica".into()),
                    has_working_tree_changes: true,
                    changed_files: vec![GitFileEntry {
                        path: "src/replica.rs".into(),
                        insertions: 4,
                        deletions: 2,
                    }],
                    ..Default::default()
                },
            );
            state.git_busy.insert("git-replica".into());
        });
        wait_until(cx, &workspace, "provider and git replicas", |cx| {
            workspace.read_with(cx, |store, _| {
                store.providers_replica.providers_checking
                    && store
                        .git_status_replica
                        .status
                        .as_ref()
                        .is_some_and(|status| status.branch.as_deref() == Some("feature/replica"))
            })
        });

        let (live_providers, live_git) = update_host!(&host, |state, _| {
            (
                state.providers_status_snapshot(),
                state.git_status_snapshot("git-replica"),
            )
        });
        let (replica_providers, replica_git) = workspace.read_with(cx, |store, _| {
            (
                store.providers_replica.clone(),
                store.git_status_replica.clone(),
            )
        });

        assert_eq!(replica_providers, live_providers);
        assert_eq!(replica_git, live_git);
        assert!(replica_providers.providers_checking);
        assert_eq!(
            replica_providers.acp_marketplace_items[0].id,
            "replicated-agent"
        );
        assert_eq!(
            replica_git.status.expect("git replica").changed_files[0].path,
            "src/replica.rs"
        );

        shutdown_test_host(&host);
        std::fs::remove_dir_all(root).expect("remove test data");
    }

    /// A draft's client state follows its project, so deleting the project
    /// takes the draft's `ConversationUiState` with it instead of stranding an
    /// entry keyed by the transient draft session id.
    #[gpui::test]
    fn removing_a_project_clears_its_drafts_conversation_state(cx: &mut TestAppContext) {
        let root = scratch_root("remove-project-draft-ui");
        let disk = SessionStore::open_at(root.clone()).expect("open test store");
        for project in ["doomed", "kept"] {
            disk.upsert_project(&project_at(project, &root))
                .expect("persist project");
        }
        disk.upsert_meta(&thread(&root, "kept-thread", "kept", None))
            .expect("persist session");
        let host = test_host(disk);
        let workspace = cx.new(|cx| WorkspaceStore::new(host.link(), cx));

        workspace.update(cx, |store, cx| {
            store.start_draft("doomed".into(), root.clone(), cx)
        });
        wait_until(cx, &workspace, "draft for the doomed project", |cx| {
            workspace.read_with(cx, |store, _| {
                store.session_status_replica.as_ref().is_some_and(|status| {
                    status.draft && status.project_id.as_deref() == Some("doomed")
                })
            })
        });
        mark_draft_state(cx, &workspace);

        // The user leaves the draft standing and deletes its project while
        // viewing another thread.
        workspace.update(cx, |store, _| store.select_session("kept-thread".into()));
        wait_until(cx, &workspace, "kept thread selected", |cx| {
            selected_status(cx, &workspace, "kept-thread")
        });
        workspace.update(cx, |store, _| store.delete_project("doomed".into()));
        wait_until(cx, &workspace, "project removed", |cx| {
            workspace.read_with(cx, |store, _| {
                !store
                    .index_replica
                    .1
                    .iter()
                    .any(|project| project.id == "doomed")
            })
        });
        assert_no_draft_state(cx, &workspace);
        assert!(
            selected_status(cx, &workspace, "kept-thread"),
            "deleting a background project moved the user"
        );

        shutdown_test_host(&host);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Deleting the project whose draft is on screen leaves nothing to draft
    /// into, so the workspace falls back to another project's draft instead of
    /// sitting on a conversation that no longer exists.
    #[gpui::test]
    fn removing_the_viewed_drafts_project_falls_back_to_another_draft(cx: &mut TestAppContext) {
        let root = scratch_root("remove-viewed-project-draft-ui");
        let disk = SessionStore::open_at(root.clone()).expect("open test store");
        for project in ["doomed", "kept"] {
            disk.upsert_project(&project_at(project, &root))
                .expect("persist project");
        }
        let host = test_host(disk);
        let workspace = cx.new(|cx| WorkspaceStore::new(host.link(), cx));

        workspace.update(cx, |store, cx| {
            store.start_draft("doomed".into(), root.clone(), cx)
        });
        wait_until(cx, &workspace, "draft for the doomed project", |cx| {
            workspace.read_with(cx, |store, _| {
                store.session_status_replica.as_ref().is_some_and(|status| {
                    status.draft && status.project_id.as_deref() == Some("doomed")
                })
            })
        });
        mark_draft_state(cx, &workspace);

        workspace.update(cx, |store, _| store.delete_project("doomed".into()));
        wait_until(cx, &workspace, "the kept project's draft", |cx| {
            workspace.read_with(cx, |store, _| {
                store.session_status_replica.as_ref().is_some_and(|status| {
                    status.draft && status.project_id.as_deref() == Some("kept")
                })
            })
        });
        assert_no_draft_state(cx, &workspace);

        shutdown_test_host(&host);
        let _ = std::fs::remove_dir_all(&root);
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
        let workspace = cx.new(|cx| WorkspaceStore::new(host.link(), cx));

        workspace.update(cx, |store, cx| {
            store.start_draft("p".into(), root.clone(), cx)
        });
        wait_until(cx, &workspace, "draft for p", |cx| {
            workspace.read_with(cx, |store, _| {
                store
                    .session_status_replica
                    .as_ref()
                    .is_some_and(|status| status.draft && status.project_id.as_deref() == Some("p"))
            })
        });
        let draft_id = workspace
            .read_with(cx, |store, _| store.selected_session_id.clone())
            .expect("draft selected");
        workspace.update(cx, |store, _| {
            store
                .conversation_ui
                .get_mut(&ConversationDestination::ProjectDraft("p".into()))
                .expect("draft conversation state")
                .right_panel_open = true;
        });

        command(
            &host,
            Command::SendTurn {
                session_id: draft_id.clone(),
                text: "first turn".into(),
                attachment_paths: Vec::new(),
            },
        );
        wait_until(cx, &workspace, "draft committed", |cx| {
            workspace.read_with(cx, |store, _| {
                store
                    .session_status_replica
                    .as_ref()
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

        shutdown_test_host(&host);
        let _ = std::fs::remove_dir_all(&root);
    }
}
