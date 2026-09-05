use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use gpui::{App, Context, Entity, EventEmitter, Subscription as GpuiSubscription, Task};
use tcode_client::{ConnectionState, HostLink};
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
    },
    ui::{ConversationDestination, RightTab},
};
#[cfg(feature = "desktop")]
use tcode_protocol::ExternalThread;
use tcode_protocol::{AcpMarketplaceItem, RuntimeNotification as RuntimeEvent};
use tcode_protocol::{
    EventEnvelope, GitDiffResult, GitDiffScope, GitStatusStatus, PathEntry, ProviderVersionStatus,
    ProvidersStatus, Query, QueryResponse, RecentDir, ServerEvent, SessionStatus, Subscription,
    Topic,
};
#[cfg(all(feature = "local-host", feature = "desktop"))]
use tcode_runtime::pipe::{ImportRoutes, start_external_import};
#[cfg(all(feature = "local-host", feature = "terminal"))]
use tcode_runtime::terminal::{LocalTerminalRegistry, TerminalWorkspace};
#[cfg(feature = "desktop")]
use tcode_services::import::ExternalImportUpdate;

use crate::conversation_ui::{ConversationUiState, DiffFocus};

mod intents;
mod snapshots;

pub(crate) use snapshots::{ChatPanelState, ComposerState, DiffPanelChrome, ShellPanelState};

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

#[cfg(feature = "local-host")]
pub struct LocalAffordances {
    #[cfg(feature = "terminal")]
    pub terminals: LocalTerminalRegistry,
    #[cfg(feature = "desktop")]
    pub preview_requests: Option<async_channel::Receiver<preview_mcp::BrokerRequest>>,
    #[cfg(feature = "desktop")]
    pub import_routes: ImportRoutes,
}

/// The client-facing projection and command boundary for workspace state.
///
/// Views observe this entity and use its typed accessors instead of retaining
/// or reading the backend `AppState` entity directly.
pub struct WorkspaceStore {
    host: HostLink,
    #[cfg(all(feature = "local-host", feature = "terminal"))]
    terminal_registry: LocalTerminalRegistry,
    #[cfg(all(feature = "local-host", feature = "desktop"))]
    preview_requests: Option<async_channel::Receiver<preview_mcp::BrokerRequest>>,
    #[cfg(all(feature = "local-host", feature = "desktop"))]
    import_routes: Option<ImportRoutes>,
    /// Name of the remote host this store is a client of. `None` means the host
    /// runs in this process, so local affordances are available.
    remote_host: Option<String>,
    connection_state: ConnectionState,
    index_replica: (Vec<SessionMeta>, Vec<Project>),
    settings_replica: Settings,
    selected_session_id: Option<String>,
    session_records: HashMap<String, Vec<StoredEvent>>,
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

impl WorkspaceStore {
    fn destination(status: &SessionStatus) -> ConversationDestination {
        if status.draft {
            ConversationDestination::ProjectDraft(status.session_id.clone())
        } else {
            ConversationDestination::Thread(status.session_id.clone())
        }
    }

    pub fn new(host: HostLink, cx: &mut Context<Self>) -> Self {
        let store = Self {
            host: host.clone(),
            #[cfg(all(feature = "local-host", feature = "terminal"))]
            terminal_registry: LocalTerminalRegistry::default(),
            #[cfg(all(feature = "local-host", feature = "desktop"))]
            preview_requests: None,
            #[cfg(all(feature = "local-host", feature = "desktop"))]
            import_routes: None,
            remote_host: None,
            connection_state: ConnectionState::Connected,
            index_replica: (Vec::new(), Vec::new()),
            settings_replica: Settings::default(),
            selected_session_id: None,
            session_records: HashMap::new(),
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
        };
        #[cfg(feature = "local-host")]
        let mut store = store;

        // Construction seeding is itself protocol traffic: subscribe, then
        // apply each snapshot event. No live AppState read exists here.
        let seed_topics = [Topic::Index, Topic::Settings, Topic::Providers];
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
        // A desktop build constructs its in-process or remote host before the
        // first window and reads settings immediately afterwards. Preserve
        // that synchronous seed contract only when local-host support is in
        // the build. Portable clients return immediately and let the task
        // below apply snapshots as they arrive, which is essential on a
        // single-threaded wasm executor.
        #[cfg(feature = "local-host")]
        {
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
                            store.apply_domain_event(&envelope);
                        }
                    }
                    Err(tcode_client::HostEventTryRecvError::Empty) => {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    Err(tcode_client::HostEventTryRecvError::Closed) => break,
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

        #[cfg(not(test))]
        {
            let event_messages = events;
            cx.spawn(async move |this, cx| {
                while let Ok(envelope) = event_messages.recv().await {
                    if this
                        .update(cx, |store, cx| {
                            if let ServerEvent::Runtime(event) = &envelope.event {
                                cx.emit(event.clone());
                            } else {
                                store.apply_domain_event(&envelope);
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
            })
            .detach();
        }

        store
    }

    #[cfg(feature = "local-host")]
    pub fn new_local(host: &tcode_runtime::pipe::SpawnedHost, cx: &mut Context<Self>) -> Self {
        let mut store = Self::new(host.link(), cx);
        store.attach_local(LocalAffordances {
            #[cfg(feature = "terminal")]
            terminals: host.terminals.clone(),
            #[cfg(feature = "desktop")]
            preview_requests: host.preview_requests.clone(),
            #[cfg(feature = "desktop")]
            import_routes: host.import_routes.clone(),
        });
        store
    }

    #[cfg(feature = "local-host")]
    pub fn attach_local(&mut self, local: LocalAffordances) {
        #[cfg(not(any(feature = "terminal", feature = "desktop")))]
        let _ = local;
        #[cfg(feature = "terminal")]
        {
            self.terminal_registry = local.terminals;
        }
        #[cfg(feature = "desktop")]
        {
            self.preview_requests = local.preview_requests;
            self.import_routes = Some(local.import_routes);
        }
    }

    /// Mark this store as a client of a remote host and start tracking the
    /// link's connection state so the workspace can show its banner.
    pub fn attach_remote(&mut self, host_name: String, cx: &mut Context<Self>) {
        self.remote_host = Some(host_name);
        self.connection_state = self.host.connection_state();
        let changes = self.host.connection_state_changes();
        cx.spawn(async move |this, cx| {
            while let Ok(state) = changes.recv().await {
                if this
                    .update(cx, |store, cx| {
                        store.connection_state = state;
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    /// Whether the host lives in another process. Local-only affordances
    /// (terminals, preview, computer use, the native directory picker) are
    /// unavailable while this is true — see P4 of docs/plans/remote-and-mobile.md.
    pub fn is_remote(&self) -> bool {
        self.remote_host.is_some()
    }

    pub fn remote_host_name(&self) -> Option<&str> {
        self.remote_host.as_deref()
    }

    pub fn connection_state(&self) -> &ConnectionState {
        &self.connection_state
    }

    pub fn sync_active_conversation_ui(&mut self) {
        let destination = self.session_status_replica.as_ref().map(Self::destination);
        if let (
            Some(tcode_core::ui::ConversationDestination::ProjectDraft(draft)),
            Some(tcode_core::ui::ConversationDestination::Thread(thread)),
        ) = (&self.active_destination, &destination)
            && draft == thread
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

    fn apply_domain_event(&mut self, envelope: &EventEnvelope) {
        if !self.host.subscription_reply_is_current(envelope) {
            return;
        }
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
                self.conversation_ui
                    .remove(&ConversationDestination::ProjectDraft(project_id.clone()));
            }
            (Topic::Index, ServerEvent::IndexSnapshot(snapshot)) => {
                self.index_replica = (snapshot.sessions.clone(), snapshot.projects.clone());
                self.background_session_flags = snapshot.activity.clone();
                if let Some(id) = &self.selected_session_id {
                    self.background_session_flags.remove(id);
                }
                if self.session_status_replica.as_ref().is_some_and(|status| {
                    !status.draft
                        && !snapshot
                            .sessions
                            .iter()
                            .any(|meta| meta.id == status.session_id && meta.archived_at.is_none())
                }) {
                    self.leave_session();
                }
            }
            (Topic::Settings, ServerEvent::SettingsReplaced(settings))
            | (Topic::Settings, ServerEvent::SettingsSnapshot(settings)) => {
                self.settings_replica = settings.clone();
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
                self.session_statuses
                    .insert(session_id.clone(), status.clone());
                if self.selected_session_id.as_ref() == Some(session_id) {
                    let mut status = status.clone();
                    status.native_rewind_prefill_available =
                        self.native_rewind_prefills.contains_key(session_id);
                    self.session_status_replica = Some(status);
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
            (
                Topic::SessionEvents { session_id },
                ServerEvent::SessionSnapshot { from, records },
            ) => {
                if self.selected_session_id.as_ref() != Some(session_id) {
                    return;
                }
                let held = self.session_records.entry(session_id.clone()).or_default();
                if *from == 0 {
                    held.clear();
                } else if *from != held.len() as u64 {
                    held.clear();
                    self.session_replica = None;
                    let _ = self.host.subscribe(Subscription {
                        topic: envelope.topic.clone(),
                        after: None,
                    });
                    return;
                }
                held.extend(records.iter().cloned());
                let mut timeline = Timeline::fold_events(held.iter().cloned());
                if !self
                    .session_status_replica
                    .as_ref()
                    .is_some_and(|status| status.turn_running)
                {
                    timeline.mark_idle();
                }
                self.session_replica = Some((session_id.clone(), timeline));
                let _ = self.host.update_after(&envelope.topic, held.len() as u64);
            }
            (Topic::SessionEvents { session_id }, ServerEvent::SessionEvent(record)) => {
                if self.selected_session_id.as_ref() != Some(session_id) {
                    return;
                }
                let held = self.session_records.entry(session_id.clone()).or_default();
                held.push(record.clone());
                let _ = self.host.update_after(&envelope.topic, held.len() as u64);
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
    fn drain_host_events_for_test(&mut self, cx: &mut Context<Self>) {
        let events = self.host.events();
        while let Ok(envelope) = events.try_recv() {
            if let ServerEvent::Runtime(event) = &envelope.event {
                cx.emit(event.clone());
            } else {
                self.apply_domain_event(&envelope);
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
        self.settings_replica.clone()
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

    pub(crate) fn shell_panel_state(&self) -> ShellPanelState {
        snapshots::shell_panel_state(self.active_conversation_ui())
    }

    pub fn preview_active_identity(&self) -> Option<(String, String)> {
        self.session_status_replica.as_ref().map(|status| {
            (
                status.session_id.clone(),
                Self::destination(status).preference_key(),
            )
        })
    }

    /// Only the native preview panel prunes by liveness; Linux compiles it out.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
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

    /// Local-handle crossing: take the preview broker receiver exactly once.
    ///
    /// Commands and preview registration metadata remain typed; this
    /// receiver carries native WebView reply senders and is the deliberate
    /// reverse-RPC affordance documented by the local host seam.
    #[cfg(all(feature = "local-host", feature = "desktop"))]
    pub fn take_preview_requests(
        &mut self,
    ) -> Option<async_channel::Receiver<preview_mcp::BrokerRequest>> {
        self.preview_requests.take()
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

    pub fn scan_external_history(&self, cx: &mut App) -> Task<Vec<RecentDir>> {
        let host = self.host.clone();
        cx.spawn(
            async move |_| match host.query(Query::ScanExternalHistory).await {
                Ok(QueryResponse::ExternalHistory(recent)) => recent,
                Ok(other) => {
                    log::error!("unexpected external-history response: {other:?}");
                    Vec::new()
                }
                Err(error) => {
                    log::error!("external-history query failed: {}", error.message);
                    Vec::new()
                }
            },
        )
    }

    /// Starts the typed import command, then returns a client-local
    /// receiver fed by the single construction-time progress bus. See
    /// the local host import route for the remote replacement
    /// (correlated progress events).
    #[cfg(feature = "desktop")]
    pub fn start_external_import(
        &self,
        project_id: &str,
        threads: Vec<ExternalThread>,
        cx: &mut App,
    ) -> Task<Result<Option<async_channel::Receiver<ExternalImportUpdate>>, String>> {
        #[cfg(feature = "local-host")]
        {
            let host = self.host.clone();
            let routes = self.import_routes.clone();
            let project_id = project_id.to_string();
            cx.spawn(async move |_| {
                let Some(routes) = routes else {
                    return Ok(None);
                };
                start_external_import(&host, &routes, project_id, threads)
                    .await
                    .map_err(|error| error.message)
            })
        }
        #[cfg(not(feature = "local-host"))]
        {
            let _ = (project_id, threads);
            cx.spawn(async move |_| Ok(None))
        }
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

    pub(crate) fn diff_panel_chrome_state(&self) -> DiffPanelChrome {
        snapshots::diff_panel_chrome(
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

    pub fn is_directory(&self, path: PathBuf, cx: &mut App) -> Task<bool> {
        let host = self.host.clone();
        cx.spawn(
            async move |_| match host.query(Query::IsDirectory { path }).await {
                Ok(QueryResponse::IsDirectory(is_directory)) => is_directory,
                Ok(other) => {
                    log::error!("unexpected is-directory response: {other:?}");
                    false
                }
                Err(error) => {
                    log::error!("is-directory query failed: {}", error.message);
                    false
                }
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
            .map(|(_, turn)| *turn)
    }

    pub(crate) fn take_pending_chat_turn(&mut self, session_id: &str, turn: usize) {
        if self.pending_chat_turn.as_ref() == Some(&(session_id.to_string(), turn)) {
            self.pending_chat_turn = None;
        }
    }

    #[cfg(test)]
    pub(crate) fn set_session_replica_for_test(&mut self, session_id: String, timeline: Timeline) {
        self.select_session(session_id.clone());
        self.host
            .command_blocking(tcode_protocol::Command::ClearRelaunchMarker)
            .expect("subscription fence");
        while let Ok(envelope) = self.host.events().try_recv() {
            self.apply_domain_event(&envelope);
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

    /// Consumes a prefill already delivered by the typed
    /// `NativeRewindPrefill` event; no backend consuming read remains.
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

    /// Borrows the live terminal workspace for terminal emulation and PTY I/O.
    ///
    /// Terminal lifecycle and preference mutations still cross [`Command`];
    /// layout/context metadata comes from `SessionStatus`. This sole local
    /// crossing accesses the `PtyHandle`/`GridEmulator`-backed live terminal
    /// objects and is documented by `LocalTerminalRegistry`; a remote
    /// transport must substitute raw byte streams, never terminal JSON.
    #[cfg(all(feature = "local-host", feature = "terminal"))]
    pub fn with_terminal_workspace<R>(
        &self,
        read: impl FnOnce(&TerminalWorkspace) -> R,
    ) -> Option<R> {
        let status = self.session_status_replica.as_ref()?;
        let workspace = TerminalWorkspace::from_replica(status, &self.terminal_registry);
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

    pub fn chat_active_session(&self) -> Option<(String, PathBuf, bool)> {
        self.session_status_replica
            .as_ref()
            .map(|status| (status.title.clone(), status.cwd.clone(), status.draft))
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

    pub(crate) fn chat_panel_state(&self) -> ChatPanelState {
        snapshots::chat_panel_state(self.active_conversation_ui())
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
    };
    use tcode_protocol::{Command, EventEnvelope, ServerEvent, SessionEventRecord, Topic};
    use tcode_runtime::host::HostEvent;
    use tcode_runtime::pipe::{HostServices, SpawnedHost, spawn_host};
    use tcode_services::store::SessionStore;

    use super::WorkspaceStore;

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
        let workspace = cx.new(|cx| WorkspaceStore::new_local(&host, cx));
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
            .set_connection_state(tcode_client::ConnectionState::Reconnecting { attempt: 1 });
        host.link()
            .set_connection_state(tcode_client::ConnectionState::Connected);
        command(&host, Command::ClearRelaunchMarker);
        workspace.update(cx, |store, cx| {
            store.drain_host_events_for_test(cx);
            assert_eq!(store.session_records["reconnect"].len(), 3);
            store.apply_domain_event(&EventEnvelope {
                request_id: None,
                topic: Topic::SessionEvents {
                    session_id: "reconnect".into(),
                },
                event: ServerEvent::SessionSnapshot {
                    from: 2,
                    records: vec![],
                },
            });
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
        std::fs::remove_dir_all(root).unwrap();
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
        let workspace = cx.new(|cx| WorkspaceStore::new_local(&host, cx));
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
        let workspace = cx.new(|cx| WorkspaceStore::new_local(&host, cx));

        command(
            &host,
            Command::CreateProject {
                root: root.join("created"),
            },
        );
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
        let workspace = cx.new(|cx| WorkspaceStore::new_local(&host, cx));
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
        let workspace = cx.new(|cx| WorkspaceStore::new_local(&host, cx));
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
        workspace.update(cx, |store, _| {
            let mut status = store
                .session_status_replica
                .clone()
                .expect("active session status");
            status.session_id = background_session_id.clone();
            status.pending_user_input = true;
            store.apply_domain_event(&EventEnvelope {
                request_id: None,
                topic: Topic::SessionStatus {
                    session_id: background_session_id.clone(),
                },
                event: ServerEvent::SessionStatusReplaced(status),
            });
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
        let workspace = cx.new(|cx| WorkspaceStore::new_local(&host, cx));
        workspace.update(cx, |store, _| store.select_session(first.id.clone()));
        wait_until(cx, &workspace, "first selected session", |cx| {
            workspace.read_with(cx, |store, _| {
                store
                    .session_status_replica
                    .as_ref()
                    .is_some_and(|status| status.session_id == first.id)
            })
        });

        workspace.update(cx, |store, _| {
            let mut parked = store
                .session_status_replica
                .clone()
                .expect("first session status");
            parked.turn_running = true;
            parked.working = true;
            store.apply_domain_event(&EventEnvelope {
                request_id: None,
                topic: Topic::SessionStatus {
                    session_id: first.id.clone(),
                },
                event: ServerEvent::SessionStatusReplaced(parked.clone()),
            });

            let mut next = parked;
            next.session_id = second.id.clone();
            next.cwd = second.cwd.clone();
            next.turn_running = false;
            next.working = false;
            store.apply_domain_event(&EventEnvelope {
                request_id: None,
                topic: Topic::SessionStatus {
                    session_id: second.id.clone(),
                },
                event: ServerEvent::SessionStatusReplaced(next.clone()),
            });
            store.select_session(next.session_id.clone());
            store.apply_domain_event(&EventEnvelope {
                request_id: None,
                topic: Topic::SessionStatus {
                    session_id: next.session_id.clone(),
                },
                event: ServerEvent::SessionStatusReplaced(next),
            });
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
        let workspace = cx.new(|cx| WorkspaceStore::new_local(&host, cx));
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
        let workspace = cx.new(|cx| WorkspaceStore::new_local(&host, cx));
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
        let workspace = cx.new(|cx| WorkspaceStore::new_local(&host, cx));
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
        let workspace = cx.new(|cx| WorkspaceStore::new_local(&host, cx));

        workspace.update(cx, |store, _| store.select_session("git-replica".into()));
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
}
