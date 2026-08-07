use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Task, Window};
use tcode_core::{
    git::{GitFileEntry, MenuItem, QuickAction, menu_items, quick_action},
    project::{Project, SessionMeta, WorktreeInfo, group_sessions, order_sessions_with_children},
    provider_models::{ResolvedModel, picker_models, resolve_models},
    provider_status::ProviderSnapshot,
    session::{ReviewComment, StoredEvent, Timeline},
    settings::{
        BrowserSettings, ProjectSort, ProviderSettings, ResolvedProfile, Settings, SidebarLayout,
    },
    ui::{ConversationDestination, RightTab},
};
use tcode_protocol::{AcpMarketplaceItem, ExternalThread};
use tcode_protocol::{
    Command, EventEnvelope, GitDiffResult, GitDiffScope, GitStatusStatus, HostMessage, PathEntry,
    ProviderVersionStatus, ProvidersStatus, Query, QueryResponse, QueuedMessageStatus, RecentDir,
    ServerEvent, SessionStatus, Subscription, Topic, decode_host_line,
};
use tcode_runtime::{
    app::ProjectGroup,
    event::RuntimeEvent,
    pipe::HostHandle,
    terminal::{LocalTerminalRegistry, TerminalContext, TerminalWorkspace},
    ui_facade::ExternalImportUpdate,
};

use crate::{
    composer::Composer,
    conversation_ui::{ConversationUi, DiffFocus},
    terminal_drawer::TerminalDrawer,
    window_state::WindowState,
};

/// The client-facing projection and command boundary for workspace state.
///
/// Views observe this entity and use its typed accessors instead of retaining
/// or reading the backend `AppState` entity directly.
pub struct WorkspaceStore {
    host: HostHandle,
    terminal_registry: LocalTerminalRegistry,
    index_replica: (Vec<SessionMeta>, Vec<Project>),
    settings_replica: Settings,
    session_replica: Option<(String, Timeline)>,
    session_status_replica: Option<SessionStatus>,
    providers_replica: ProvidersStatus,
    git_status_replica: GitStatusStatus,
    background_session_flags: HashMap<String, (bool, bool, bool)>,
    active_destination: Option<ConversationDestination>,
    native_rewind_prefills: HashMap<String, String>,
    conversation_ui: ConversationUi,
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

pub(crate) struct DiffActiveState {
    pub session: String,
    pub cwd: PathBuf,
    pub branches: Vec<String>,
}

fn protocol_io_error(message: impl Into<String>) -> std::io::Error {
    std::io::Error::other(message.into())
}

impl WorkspaceStore {
    fn destination(status: &SessionStatus) -> ConversationDestination {
        if status.draft {
            ConversationDestination::ProjectDraft(
                status
                    .project_id
                    .clone()
                    .unwrap_or_else(|| status.session_id.clone()),
            )
        } else {
            ConversationDestination::Thread(status.session_id.clone())
        }
    }

    pub fn new(host: HostHandle, cx: &mut Context<Self>) -> Self {
        let terminal_registry = host.terminal_registry();
        let mut store = Self {
            host: host.clone(),
            terminal_registry,
            index_replica: (Vec::new(), Vec::new()),
            settings_replica: Settings::default(),
            session_replica: None,
            session_status_replica: None,
            providers_replica: ProvidersStatus::default(),
            git_status_replica: GitStatusStatus::default(),
            background_session_flags: HashMap::new(),
            active_destination: None,
            native_rewind_prefills: HashMap::new(),
            conversation_ui: ConversationUi::default(),
        };

        // Construction seeding is itself protocol traffic: subscribe, then
        // deserialize each snapshot event. No live AppState read exists here.
        if let Err(error) = host.hello(env!("CARGO_PKG_VERSION")) {
            log::error!("failed to send host hello: {}", error.message);
        }
        let seed_topics = [
            Topic::Index,
            Topic::Settings,
            Topic::Providers,
            Topic::GitStatus,
            Topic::ActiveSession,
            Topic::RuntimeEvents,
        ];
        for topic in &seed_topics {
            if let Err(error) = host.subscribe(Subscription {
                topic: topic.clone(),
                after_seq: None,
            }) {
                log::error!("failed to subscribe to {topic:?}: {}", error.message);
            }
        }
        let events = host.event_receiver();
        let mut seeded = HashSet::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while seeded.len() < seed_topics.len() && std::time::Instant::now() < deadline {
            let Ok(line) = events.try_recv() else {
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            };
            match decode_host_line(&line) {
                Ok(HostMessage::Event(envelope)) => {
                    match (&envelope.topic, &envelope.event) {
                        (Topic::Index, ServerEvent::IndexSnapshot(_))
                        | (Topic::Settings, ServerEvent::SettingsSnapshot(_))
                        | (Topic::Providers, ServerEvent::ProvidersReplaced(_))
                        | (Topic::GitStatus, ServerEvent::GitStatusReplaced(_))
                        | (Topic::ActiveSession, ServerEvent::ActiveSessionReplaced(_))
                        | (Topic::RuntimeEvents, ServerEvent::RuntimeSnapshot(_)) => {
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
                Ok(_) => {}
                Err(error) => log::error!("failed to decode seed event: {}", error.message),
            }
        }
        if seeded.len() != seed_topics.len() {
            log::error!(
                "host snapshot seeding timed out: received {}/{} domains",
                seeded.len(),
                seed_topics.len()
            );
        }

        #[cfg(not(test))]
        {
            let event_lines = events;
            cx.spawn(async move |this, cx| {
                while let Ok(line) = event_lines.recv().await {
                    let message = match decode_host_line(&line) {
                        Ok(message) => message,
                        Err(error) => {
                            log::error!("failed to decode host event: {}", error.message);
                            continue;
                        }
                    };
                    let HostMessage::Event(envelope) = message else {
                        continue;
                    };
                    if this
                        .update(cx, |store, cx| {
                            if let ServerEvent::Runtime(event) = &envelope.event {
                                cx.emit(event.clone());
                            } else {
                                store.apply_domain_event(&envelope);
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
            let changed = host.changed_receiver();
            cx.spawn(async move |this, cx| {
                while changed.recv().await.is_ok() {
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            })
            .detach();
        }

        store
    }

    pub fn sync_active_conversation_ui(&mut self, _cx: &App) {
        let destination = self.session_status_replica.as_ref().map(Self::destination);
        if let Some((destination, status)) = destination
            .clone()
            .zip(self.session_status_replica.as_ref())
        {
            self.conversation_ui.ensure(
                destination,
                self.settings_replica.word_wrap_diffs,
                status.terminal_open,
                status.terminal_height,
            );
        }
        self.active_destination = destination;
    }

    fn apply_domain_event(&mut self, envelope: &EventEnvelope) {
        match (&envelope.topic, &envelope.event) {
            (Topic::ActiveSession, ServerEvent::ActiveSessionReplaced(status)) => {
                let previous = self.active_destination.clone();
                let previous_status = self.session_status_replica.clone();
                let previous_session = previous_status
                    .as_ref()
                    .map(|status| status.session_id.clone());
                let mut status = status.clone();
                if let Some(status) = status.as_mut() {
                    status.native_rewind_prefill_available =
                        self.native_rewind_prefills.contains_key(&status.session_id);
                }
                let next_session = status.as_ref().map(|status| status.session_id.as_str());

                // A thread switch parks the old session before ActiveSession is
                // replaced. Its SessionStatus event therefore arrives while the
                // client still considers it active and updates the foreground
                // replica instead of `background_session_flags`. Preserve that
                // final, authoritative status when the active id changes so the
                // sidebar does not lose Working/waiting state during the handoff.
                if let Some(previous_status) = previous_status.as_ref()
                    && next_session != Some(previous_status.session_id.as_str())
                    && !previous_status.draft
                    && self.index_replica.0.iter().any(|meta| {
                        meta.id == previous_status.session_id && meta.archived_at.is_none()
                    })
                {
                    self.background_session_flags.insert(
                        previous_status.session_id.clone(),
                        (
                            previous_status.working,
                            previous_status.pending_approval,
                            previous_status.pending_user_input,
                        ),
                    );
                }
                self.session_status_replica = status.clone();
                let destination = status.as_ref().map(Self::destination);
                if previous_session.as_deref()
                    != status.as_ref().map(|status| status.session_id.as_str())
                {
                    self.session_replica = None;
                }
                if let Some((destination, status)) = destination.clone().zip(status.as_ref()) {
                    if previous.as_ref().is_some_and(|previous| {
                        previous != &destination
                            && matches!(previous, ConversationDestination::ProjectDraft(_))
                            && matches!(destination, ConversationDestination::Thread(_))
                    }) && let Some(previous) = previous.as_ref()
                    {
                        self.conversation_ui
                            .move_entry(previous, destination.clone());
                    }
                    self.conversation_ui.ensure(
                        destination.clone(),
                        self.settings_replica.word_wrap_diffs,
                        status.terminal_open,
                        status.terminal_height,
                    );
                    self.background_session_flags.remove(&status.session_id);
                }
                self.active_destination = destination;
            }
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
            }
            (Topic::Settings, ServerEvent::SettingsReplaced(settings))
            | (Topic::Settings, ServerEvent::SettingsSnapshot(settings)) => {
                self.settings_replica = settings.clone();
            }
            (Topic::Providers, ServerEvent::ProvidersReplaced(status)) => {
                self.providers_replica = status.clone();
            }
            (Topic::GitStatus, ServerEvent::GitStatusReplaced(status)) => {
                self.git_status_replica = status.clone();
            }
            (Topic::SessionStatus { session_id }, ServerEvent::SessionStatusReplaced(status))
                if status.session_id == *session_id =>
            {
                if self
                    .session_status_replica
                    .as_ref()
                    .is_some_and(|active| active.session_id == *session_id)
                {
                    let mut status = status.clone();
                    status.native_rewind_prefill_available =
                        self.native_rewind_prefills.contains_key(session_id);
                    self.session_status_replica = Some(status);
                    self.background_session_flags.remove(session_id);
                } else {
                    self.background_session_flags.insert(
                        session_id.clone(),
                        (
                            status.working,
                            status.pending_approval,
                            status.pending_user_input,
                        ),
                    );
                }
            }
            (Topic::SessionEvents { session_id }, ServerEvent::SessionSnapshot(records)) => {
                let mut timeline =
                    Timeline::fold_events(records.iter().map(|record| StoredEvent {
                        ts: record.ts,
                        event: record.event.clone(),
                    }));
                let live_turn_running = self
                    .session_status_replica
                    .as_ref()
                    .filter(|status| status.session_id == *session_id)
                    .is_some_and(|status| status.turn_running);
                if !live_turn_running {
                    timeline.mark_idle();
                }
                self.session_replica = Some((session_id.clone(), timeline));
            }
            (Topic::SessionEvents { session_id }, ServerEvent::SessionEvent(record)) => {
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

    pub fn profile_catalog(&self, profile_id: &str, _cx: &App) -> Vec<agent::ModelSpec> {
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

    /// Dispatch a serializable backend mutation.
    pub fn dispatch(&mut self, command: Command, _cx: &mut Context<Self>) {
        if let Err(error) = self.host.dispatch(command) {
            log::error!("failed to dispatch host command: {}", error.message);
        }
    }

    pub fn command(
        &self,
        command: Command,
        cx: &mut App,
    ) -> Task<Result<tcode_protocol::CommandResponse, tcode_protocol::ProtocolError>> {
        let host = self.host.clone();
        #[cfg(test)]
        {
            let result = smol::block_on(host.command(command));
            cx.spawn(async move |_| result)
        }
        #[cfg(not(test))]
        {
            cx.spawn(async move |_| host.command(command).await)
        }
    }

    #[cfg(test)]
    fn drain_host_events_for_test(&mut self, cx: &mut Context<Self>) {
        let events = self.host.event_receiver();
        while let Ok(line) = events.try_recv() {
            let Ok(HostMessage::Event(envelope)) = decode_host_line(&line) else {
                continue;
            };
            if let ServerEvent::Runtime(event) = &envelope.event {
                cx.emit(event.clone());
            } else {
                self.apply_domain_event(&envelope);
            }
            cx.notify();
        }
        let changed = self.host.changed_receiver();
        while changed.try_recv().is_ok() {}
    }

    pub fn working_sessions_count(&self, _cx: &App) -> usize {
        let active = usize::from(
            self.session_status_replica
                .as_ref()
                .is_some_and(|status| status.working),
        );
        active
            + self
                .background_session_flags
                .values()
                .filter(|(working, _, _)| *working)
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
        self.conversation_ui
            .open_preview_for(destination, self.settings_replica.word_wrap_diffs);
        cx.notify();
    }

    pub fn preview_url(&self, key: &str) -> Option<String> {
        self.conversation_ui
            .get_by_key(key)
            .and_then(|ui| ui.preview_url.clone())
    }

    pub fn set_preview_url(&mut self, key: &str, url: String, cx: &mut Context<Self>) {
        if let Some(ui) = self.conversation_ui.get_mut_by_key(key) {
            ui.preview_url = Some(url);
            cx.notify();
        }
    }

    pub fn preview_canvas(&self, key: &str) -> Option<(u32, u32)> {
        self.conversation_ui
            .get_by_key(key)
            .and_then(|ui| ui.preview_canvas)
    }

    pub fn set_preview_canvas(
        &mut self,
        key: &str,
        canvas: Option<(u32, u32)>,
        cx: &mut Context<Self>,
    ) {
        if let Some(ui) = self.conversation_ui.get_mut_by_key(key) {
            ui.preview_canvas = canvas;
            cx.notify();
        }
    }

    pub fn clear_preview_chrome(&mut self, key: &str, cx: &mut Context<Self>) {
        if let Some(ui) = self.conversation_ui.get_mut_by_key(key) {
            ui.preview_url = None;
            ui.preview_canvas = None;
            cx.notify();
        }
    }

    pub fn toggle_terminal_panel(&mut self, cx: &mut Context<Self>) {
        let opening = !self
            .active_conversation_ui()
            .is_some_and(|ui| ui.terminal_open);
        if let Some(ui) = self.active_conversation_ui_mut() {
            ui.terminal_open = opening;
        }
        if opening {
            self.dispatch(Command::ToggleTerminalPanel, cx);
        } else {
            self.dispatch(Command::CloseTerminalPanel, cx);
        }
        cx.notify();
    }

    pub fn close_terminal_panel(&mut self, cx: &mut Context<Self>) {
        if let Some(ui) = self.active_conversation_ui_mut() {
            ui.terminal_open = false;
        }
        self.dispatch(Command::CloseTerminalPanel, cx);
        cx.notify();
    }

    pub fn show_terminal_panel(&mut self, cx: &mut Context<Self>) {
        if let Some(ui) = self.active_conversation_ui_mut() {
            ui.terminal_open = true;
            cx.notify();
        }
    }

    pub fn set_terminal_height(&mut self, height: f32, cx: &mut Context<Self>) {
        if let Some(ui) = self.active_conversation_ui_mut() {
            ui.terminal_height = height;
        }
        self.dispatch(Command::SetTerminalHeight { height }, cx);
        cx.notify();
    }

    pub fn close_terminal(&mut self, terminal_id: u64, cx: &mut Context<Self>) {
        let closes_drawer = self
            .session_status_replica
            .as_ref()
            .is_some_and(|status| status.terminals.len() <= 1);
        if closes_drawer && let Some(ui) = self.active_conversation_ui_mut() {
            ui.terminal_open = false;
        }
        self.dispatch(Command::CloseTerminal { terminal_id }, cx);
        cx.notify();
    }

    pub fn grouped_sessions(&self, _cx: &App) -> Vec<ProjectGroup> {
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

    pub fn settings(&self, _cx: &App) -> Settings {
        self.settings_replica.clone()
    }

    pub fn archived_groups(&self, _cx: &App) -> Vec<ProjectGroup> {
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

    pub fn project_sort(&self, _cx: &App) -> ProjectSort {
        self.settings_replica.project_sort
    }

    pub fn sidebar_layout(&self, _cx: &App) -> SidebarLayout {
        self.settings_replica.sidebar_layout
    }

    pub fn flat_sessions(&self, _cx: &App) -> Vec<SessionMeta> {
        let visible = self
            .index_replica
            .0
            .iter()
            .filter(|meta| meta.archived_at.is_none())
            .cloned()
            .collect();
        order_sessions_with_children(visible)
    }

    pub fn projects(&self, _cx: &App) -> Vec<Project> {
        self.index_replica.1.clone()
    }

    pub fn is_project_collapsed(&self, project_id: &str, _cx: &App) -> bool {
        self.settings_replica
            .collapsed_projects
            .iter()
            .any(|id| id == project_id)
    }

    pub fn active_session_id(&self, _cx: &App) -> Option<String> {
        self.session_status_replica
            .as_ref()
            .map(|status| status.session_id.clone())
    }

    pub fn turn_running_for(&self, session_id: &str, _cx: &App) -> bool {
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

    pub fn session_unread(&self, session_id: &str, _cx: &App) -> bool {
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

    pub fn pending_approval_for(&self, session_id: &str, _cx: &App) -> bool {
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

    pub fn pending_user_input_for(&self, session_id: &str, _cx: &App) -> bool {
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

    pub fn fork_availability(&self, session_id: &str, cx: &App) -> ForkAvailability {
        let Some(meta) = self
            .index_replica
            .0
            .iter()
            .find(|meta| meta.id == session_id)
        else {
            return ForkAvailability::Available;
        };
        if !meta.provider.supports_fork() {
            ForkAvailability::Unsupported
        } else if meta.resume_cursor.is_none() {
            ForkAvailability::Empty
        } else if self.turn_running_for(session_id, cx) {
            ForkAvailability::Running
        } else {
            ForkAvailability::Available
        }
    }

    pub fn sidebar_sessions(&self, _cx: &App) -> Vec<SessionMeta> {
        self.index_replica.0.clone()
    }

    pub fn settings_installed_acp_agents(
        &self,
        _cx: &App,
    ) -> Vec<tcode_core::acp::InstalledAcpAgent> {
        self.settings_replica
            .installed_acp_agents()
            .into_iter()
            .cloned()
            .collect()
    }

    pub fn providers_checked_at(&self, _cx: &App) -> Option<u64> {
        self.providers_replica.providers_checked_at
    }

    pub fn providers_checking(&self, _cx: &App) -> bool {
        self.providers_replica.providers_checking
    }

    pub fn window_caption_state(&self, _cx: &App) -> (bool, tcode_core::ui::RightTab) {
        self.active_conversation_ui()
            .map(|ui| (ui.right_panel_open, ui.right_tab))
            .unwrap_or((false, RightTab::default()))
    }

    pub fn shell_window_title(&self, cx: &App) -> String {
        let _ = cx;
        match self.session_status_replica.as_ref() {
            Some(status) if status.draft => tcode_i18n::tr!("chat.new_thread").into_owned(),
            Some(status) => status.title.clone(),
            None => "tcode".to_string(),
        }
    }

    pub fn shell_panel_state(&self, _cx: &App) -> (bool, tcode_core::ui::RightTab, bool) {
        self.active_conversation_ui()
            .map(|ui| (ui.right_panel_open, ui.right_tab, ui.right_panel_expanded))
            .unwrap_or((false, RightTab::default(), false))
    }

    pub fn preview_active_identity(&self, _cx: &App) -> Option<(String, String)> {
        self.session_status_replica.as_ref().map(|status| {
            let key = if status.draft {
                format!(
                    "draft:{}",
                    status.project_id.as_deref().unwrap_or(&status.session_id)
                )
            } else {
                status.session_id.clone()
            };
            (status.session_id.clone(), key)
        })
    }

    pub fn preview_panel_showing(&self, _cx: &App) -> bool {
        self.active_conversation_ui()
            .is_some_and(|ui| ui.right_panel_open && ui.right_tab == RightTab::Preview)
    }

    pub fn preview_browser_settings(&self, _cx: &App) -> BrowserSettings {
        self.settings_replica.browser.clone()
    }

    /// Local-handle crossing: take the preview broker receiver exactly once.
    ///
    /// Commands and preview registration metadata remain serialized; this
    /// receiver carries native WebView reply senders and is the deliberate
    /// reverse-RPC affordance documented by [`HostHandle::take_preview_requests`].
    pub fn take_preview_requests(
        &mut self,
        _cx: &mut Context<Self>,
    ) -> Option<async_channel::Receiver<preview_mcp::BrokerRequest>> {
        self.host.take_preview_requests()
    }

    pub fn provider_profile_kind(&self, profile_id: &str, _cx: &App) -> agent::ProviderKind {
        self.settings_replica
            .resolved_profile(profile_id)
            .map(|profile| profile.kind)
            .unwrap_or(agent::ProviderKind::ClaudeCode)
    }

    pub fn provider_profile_settings(&self, profile_id: &str, _cx: &App) -> ProviderSettings {
        self.settings_replica
            .resolved_profile(profile_id)
            .map(|profile| profile.settings)
            .unwrap_or_default()
    }

    pub fn provider_model_catalog(
        &self,
        provider: agent::ProviderKind,
        _cx: &App,
    ) -> Vec<agent::ModelSpec> {
        self.providers_replica
            .model_catalogs
            .get(&provider)
            .cloned()
            .unwrap_or_default()
    }

    pub fn picker_models_for_profile(&self, profile_id: &str, cx: &App) -> Vec<ResolvedModel> {
        picker_models(
            &self.profile_catalog(profile_id, cx),
            &self.provider_profile_settings(profile_id, cx),
            &self.settings_replica.favorite_models,
        )
    }

    pub fn provider_profile_display_name(&self, profile_id: &str, _cx: &App) -> String {
        self.settings_replica.profile_display_name(profile_id)
    }

    pub fn provider_profile_snapshot(
        &self,
        profile_id: &str,
        _cx: &App,
    ) -> Option<ProviderSnapshot> {
        self.providers_replica
            .provider_snapshots
            .get(profile_id)
            .cloned()
    }

    pub fn provider_version_status(
        &self,
        provider: agent::ProviderKind,
        _cx: &App,
    ) -> Option<ProviderVersionStatus> {
        self.providers_replica
            .provider_versions
            .get(&provider)
            .cloned()
    }

    pub fn provider_profile_accent(&self, profile_id: &str, _cx: &App) -> Option<u32> {
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

    pub fn provider_update_command(
        &self,
        provider: agent::ProviderKind,
        _cx: &App,
    ) -> Option<String> {
        self.providers_replica
            .provider_versions
            .get(&provider)
            .and_then(|status| status.update_command.clone())
    }

    pub fn provider_profile_stored_secret_names(
        &self,
        profile_id: &str,
        _cx: &App,
    ) -> HashSet<String> {
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
        cx: &App,
    ) -> Vec<ResolvedModel> {
        let mut settings = self.provider_profile_settings(profile_id, cx);
        settings.custom_models = custom_models.to_vec();
        settings.hidden_models = hidden_models.to_vec();
        resolve_models(
            &self.profile_catalog(profile_id, cx),
            &settings,
            &self.settings_replica.favorite_models,
        )
    }

    pub fn installed_acp_agent(
        &self,
        agent_id: &str,
        _cx: &App,
    ) -> Option<tcode_core::acp::InstalledAcpAgent> {
        self.settings_replica.acp_agent(agent_id).cloned()
    }

    pub fn acp_marketplace_items(&self, _cx: &App) -> Vec<AcpMarketplaceItem> {
        let mut items = self.providers_replica.acp_marketplace_items.clone();
        for item in &mut items {
            item.installed = self.settings_replica.acp_agents.contains_key(&item.id);
        }
        items
    }

    pub fn acp_registry_loading(&self, _cx: &App) -> bool {
        self.providers_replica.acp_registry_loading
    }

    pub fn acp_registry_error(&self, _cx: &App) -> Option<String> {
        self.providers_replica.acp_registry_error.clone()
    }

    pub fn acp_installing(&self, agent_id: &str, _cx: &App) -> bool {
        self.providers_replica.acp_installing.contains(agent_id)
    }

    pub fn project_ids(&self, _cx: &App) -> Vec<String> {
        self.index_replica
            .1
            .iter()
            .map(|project| project.id.clone())
            .collect()
    }

    pub fn project_summary(&self, project_id: &str, _cx: &App) -> Option<(String, usize)> {
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

    pub fn project_root(&self, project_id: &str, _cx: &App) -> Option<PathBuf> {
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

    /// Starts the serialized import command, then returns a client-local
    /// receiver fed by the single construction-time progress bus. See
    /// [`HostHandle::start_external_import`] for the remote replacement
    /// (correlated progress events).
    pub fn start_external_import(
        &self,
        project_id: &str,
        threads: Vec<ExternalThread>,
        cx: &mut App,
    ) -> Task<Result<Option<async_channel::Receiver<ExternalImportUpdate>>, String>> {
        let host = self.host.clone();
        let project_id = project_id.to_string();
        cx.spawn(async move |_| {
            host.start_external_import(project_id, threads)
                .await
                .map_err(|error| error.message)
        })
    }

    pub fn commit_dialog_state(&self, _cx: &App) -> (Vec<GitFileEntry>, Option<String>, bool) {
        (
            self.git_status_replica
                .status
                .as_ref()
                .map(|status| status.changed_files.clone())
                .unwrap_or_default(),
            self.git_status_replica
                .status
                .as_ref()
                .and_then(|status| status.branch.clone()),
            self.git_status_replica
                .status
                .as_ref()
                .is_some_and(|status| status.is_default_branch),
        )
    }

    pub(crate) fn diff_active_state(&self, _cx: &App) -> Option<DiffActiveState> {
        self.session_status_replica
            .as_ref()
            .map(|status| DiffActiveState {
                session: status.session_id.clone(),
                cwd: status.cwd.clone(),
                branches: status.branches.clone(),
            })
    }

    pub fn diff_turns(&self, cx: &App) -> Vec<usize> {
        self.with_active_timeline(cx, |timeline| {
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

    pub fn diff_selected_turn(&self, cx: &App) -> Option<usize> {
        let turns = self.diff_turns(cx);
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
        cx: &App,
        read: impl FnOnce(&[agent::FileChange], agent::ChangeCompleteness) -> R,
    ) -> Option<R> {
        self.with_active_timeline(cx, |timeline| {
            let changes = timeline.turns.get(turn)?.changes.as_ref()?;
            Some(read(&changes.changes, changes.completeness))
        })
        .flatten()
    }

    pub(crate) fn pending_diff_focus(&self, _cx: &App) -> Option<DiffFocus> {
        self.active_conversation_ui()
            .and_then(|ui| ui.pending_diff_focus.clone())
    }

    /// UI-only consuming selector. The underlying diff focus is replica state;
    /// this does not cross the host boundary.
    pub(crate) fn take_diff_focus(
        &mut self,
        session: &str,
        turn: usize,
        _cx: &mut Context<Self>,
    ) -> Option<DiffFocus> {
        self.active_conversation_ui_mut()?
            .take_diff_focus(session, turn)
    }

    pub fn diff_refresh_generation(&self, _cx: &App) -> u64 {
        self.active_conversation_ui()
            .map(|ui| ui.diff_refresh_generation)
            .unwrap_or(0)
    }

    pub fn diff_word_wrap(&self, _cx: &App) -> bool {
        self.active_conversation_ui()
            .map(|ui| ui.diff_wrap)
            .unwrap_or(self.settings_replica.word_wrap_diffs)
    }

    pub fn diff_split(&self, _cx: &App) -> bool {
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

    pub fn diff_panel_chrome_state(
        &self,
        cx: &App,
    ) -> (bool, bool, tcode_core::ui::RightTab, bool) {
        let plan_tab_active_label = self
            .with_active_timeline(cx, |timeline| timeline.proposed_plan.is_some())
            .unwrap_or(false)
            || self.composer_interaction_mode(cx) == agent::InteractionMode::Plan;
        let (open, expanded, tab) = self
            .active_conversation_ui()
            .map(|ui| (ui.right_panel_open, ui.right_panel_expanded, ui.right_tab))
            .unwrap_or((false, false, RightTab::default()));
        (open, expanded, tab, plan_tab_active_label)
    }

    pub fn diff_review_comments(&self, _cx: &App) -> Vec<ReviewComment> {
        self.session_status_replica
            .as_ref()
            .map(|status| status.review_comment_drafts.clone())
            .unwrap_or_default()
    }

    pub fn with_diff_review_comments<R>(
        &self,
        _cx: &App,
        read: impl FnOnce(&[ReviewComment]) -> R,
    ) -> R {
        read(
            self.session_status_replica
                .as_ref()
                .map(|status| status.review_comment_drafts.as_slice())
                .unwrap_or_default(),
        )
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

    pub fn with_active_timeline<R>(
        &self,
        _cx: &App,
        read: impl FnOnce(&Timeline) -> R,
    ) -> Option<R> {
        self.session_replica
            .as_ref()
            .map(|(_, timeline)| read(timeline))
    }

    #[cfg(test)]
    pub(crate) fn set_session_replica_for_test(&mut self, session_id: String, timeline: Timeline) {
        self.session_replica = Some((session_id, timeline));
    }

    pub fn with_composer_destination<R>(
        &self,
        _cx: &App,
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

    pub fn composer_has_active_session(&self, _cx: &App) -> bool {
        self.session_status_replica.is_some()
    }

    /// Consumes a prefill already delivered by the serialized
    /// `NativeRewindPrefill` event; no backend consuming read remains.
    pub fn take_native_rewind_prefill(&mut self, _cx: &mut Context<Self>) -> Option<String> {
        let active_id = self.session_status_replica.as_ref()?.session_id.clone();
        let prefill = self.native_rewind_prefills.remove(&active_id)?;
        if let Some(status) = self.session_status_replica.as_mut() {
            status.native_rewind_prefill_available = false;
        }
        Some(prefill)
    }

    pub fn composer_terminal_contexts(&self, _cx: &App) -> Vec<TerminalContext> {
        self.session_status_replica
            .as_ref()
            .map(|status| {
                status
                    .terminal_contexts
                    .iter()
                    .map(|context| TerminalContext {
                        id: context.id,
                        terminal_label: context.terminal_label.clone(),
                        line_start: context.line_start,
                        line_end: context.line_end,
                        text: context.text.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Borrows the live terminal workspace for terminal emulation and PTY I/O.
    ///
    /// Terminal lifecycle and preference mutations still cross [`Command`];
    /// layout/context metadata comes from `SessionStatus`. This sole local
    /// crossing accesses the `PtyHandle`/`GridEmulator`-backed live terminal
    /// objects and is documented by `LocalTerminalRegistry`; a remote
    /// transport must substitute raw byte streams, never terminal JSON.
    pub fn with_terminal_workspace<R>(
        &self,
        _cx: &App,
        read: impl FnOnce(&TerminalWorkspace) -> R,
    ) -> Option<R> {
        let status = self.session_status_replica.as_ref()?;
        let workspace = TerminalWorkspace::from_replica(status, &self.terminal_registry);
        Some(read(&workspace))
    }

    pub fn composer_review_comments(&self, _cx: &App) -> Vec<ReviewComment> {
        self.session_status_replica
            .as_ref()
            .map(|status| status.review_comment_drafts.clone())
            .unwrap_or_default()
    }

    pub fn composer_relay_confirmation(&self, _cx: &App) -> Option<(String, String)> {
        self.session_status_replica
            .as_ref()
            .and_then(|status| status.relay_confirmation.clone())
    }

    pub fn composer_active_cwd(&self, _cx: &App) -> Option<PathBuf> {
        self.session_status_replica
            .as_ref()
            .map(|status| status.cwd.clone())
    }

    pub fn list_active_workspace(&self, cx: &mut App) -> Task<Vec<PathEntry>> {
        let host = self.host.clone();
        cx.spawn(
            async move |_| match host.query(Query::ListActiveWorkspace).await {
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

    pub fn composer_provider_commands(&self, _cx: &App) -> Vec<agent::ProviderCommand> {
        self.session_status_replica
            .as_ref()
            .map(|status| status.provider_commands.clone())
            .unwrap_or_default()
    }

    pub fn composer_attachments_dir(&self, _cx: &App) -> Option<PathBuf> {
        self.session_status_replica
            .as_ref()
            .map(|status| status.attachments_dir.clone())
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

    pub fn composer_pending_user_input(
        &self,
        cx: &App,
    ) -> Option<(String, Vec<agent::UserInputQuestion>)> {
        self.with_active_timeline(cx, |timeline| timeline.pending_user_input.clone())
            .flatten()
    }

    pub(crate) fn composer_active_model(&self, _cx: &App) -> Option<ComposerActiveModel> {
        self.session_status_replica
            .as_ref()
            .map(|status| ComposerActiveModel {
                provider: status.provider,
                model: status.requested_model.clone(),
                acp_agent_id: status.acp_agent_id.clone(),
                profile_id: status.requested_profile_id.clone(),
            })
    }

    pub fn composer_picker_models(
        &self,
        provider: agent::ProviderKind,
        cx: &App,
    ) -> Vec<ResolvedModel> {
        picker_models(
            &self.provider_model_catalog(provider, cx),
            &self.settings_replica.provider(provider),
            &self.settings_replica.favorite_models,
        )
    }

    pub fn composer_models_loading(&self, provider: agent::ProviderKind, _cx: &App) -> bool {
        self.providers_replica
            .models_loading
            .get(&provider)
            .copied()
            .unwrap_or(false)
            && self
                .providers_replica
                .model_catalogs
                .get(&provider)
                .is_none_or(Vec::is_empty)
    }

    pub fn composer_model_pending_restart(&self, _cx: &App) -> bool {
        self.session_status_replica
            .as_ref()
            .is_some_and(|status| status.model_pending_restart)
    }

    pub fn composer_active_model_spec(&self, _cx: &App) -> Option<agent::ModelSpec> {
        let status = self.session_status_replica.as_ref()?;
        let model = status.requested_model.as_deref()?;
        self.providers_replica
            .model_catalogs
            .get(&status.provider)?
            .iter()
            .find(|spec| spec.id == model)
            .cloned()
    }

    pub fn composer_active_option_descriptors(&self, _cx: &App) -> Vec<agent::OptionDescriptor> {
        self.session_status_replica
            .as_ref()
            .map(|status| status.provider_option_descriptors.clone())
            .unwrap_or_default()
    }

    pub fn composer_active_option_selections(&self, _cx: &App) -> Vec<agent::OptionSelection> {
        self.session_status_replica
            .as_ref()
            .map(|status| status.provider_option_selections.clone())
            .unwrap_or_default()
    }

    pub fn composer_ultrathink_armed(&self, _cx: &App) -> bool {
        self.session_status_replica
            .as_ref()
            .is_some_and(|status| status.ultrathink_armed)
    }

    pub fn composer_options_pending_restart(&self, _cx: &App) -> bool {
        self.session_status_replica
            .as_ref()
            .is_some_and(|status| status.options_pending_restart)
    }

    pub fn composer_interaction_mode(&self, _cx: &App) -> agent::InteractionMode {
        self.session_status_replica
            .as_ref()
            .map(|status| status.interaction_mode)
            .unwrap_or_default()
    }

    pub fn composer_context(
        &self,
        cx: &App,
    ) -> (Option<agent::TokenUsage>, Option<agent::ProviderKind>) {
        let provider = self
            .session_status_replica
            .as_ref()
            .map(|status| status.provider);
        (
            self.with_active_timeline(cx, |timeline| timeline.usage)
                .flatten(),
            provider,
        )
    }

    pub fn composer_approval_mode(&self, _cx: &App) -> agent::ApprovalMode {
        let mode = self
            .session_status_replica
            .as_ref()
            .map(|status| status.approval_mode)
            .unwrap_or_default();
        if !self.composer_native_approval_modes_enabled(_cx)
            && matches!(
                mode,
                agent::ApprovalMode::Supervised | agent::ApprovalMode::AutoAcceptEdits
            )
        {
            agent::ApprovalMode::FullAccess
        } else {
            mode
        }
    }

    pub fn composer_native_approval_modes_enabled(&self, _cx: &App) -> bool {
        let Some(status) = self.session_status_replica.as_ref() else {
            return true;
        };
        if status.provider != agent::ProviderKind::Pi {
            return true;
        }
        status
            .requested_profile_id
            .as_deref()
            .and_then(|id| self.settings_replica.resolved_profile(id))
            .map(|profile| profile.settings.pi_native_approvals)
            .unwrap_or_else(|| {
                self.settings_replica
                    .provider(agent::ProviderKind::Pi)
                    .pi_native_approvals
            })
    }

    pub fn composer_approval_pending_restart(&self, _cx: &App) -> bool {
        self.session_status_replica
            .as_ref()
            .is_some_and(|status| status.approval_pending_restart)
    }

    pub fn composer_queue(
        &self,
        _cx: &App,
    ) -> Option<(Vec<QueuedMessageStatus>, bool, &'static str)> {
        self.session_status_replica.as_ref().map(|status| {
            (
                status.queued_messages.clone(),
                status.supports_steering,
                status.provider.display_name(),
            )
        })
    }

    pub fn composer_supports_steering(&self, _cx: &App) -> bool {
        self.session_status_replica
            .as_ref()
            .is_some_and(|status| status.supports_steering)
    }

    pub fn composer_preparing_worktree(&self, _cx: &App) -> bool {
        self.session_status_replica
            .as_ref()
            .is_some_and(|status| status.preparing_worktree)
    }

    pub fn composer_plan_ready_markdown(&self, cx: &App) -> Option<String> {
        self.with_active_timeline(cx, |timeline| {
            timeline.plan_ready().map(|plan| plan.markdown.clone())
        })
        .flatten()
    }

    pub(crate) fn composer_checkout_state(&self, _cx: &App) -> Option<ComposerCheckoutState> {
        let status = self.session_status_replica.as_ref()?;
        let branch = status.git_branch.clone().or_else(|| {
            status
                .worktree
                .as_ref()
                .map(|worktree| worktree.branch.clone())
        })?;
        let worktree_base = match &status.draft_workspace {
            tcode_core::ui::WorkspaceMode::NewWorktree { base } => Some(base.clone()),
            _ => None,
        };
        Some(ComposerCheckoutState {
            branch,
            branches: status.branches.clone(),
            turn_running: status.turn_running,
            is_draft: status.draft,
            worktree_base,
            worktree: status.worktree.clone(),
        })
    }

    pub fn composer_render_state(&self, cx: &App) -> (bool, Option<agent::ApprovalRequest>, usize) {
        let turn_running = self
            .session_status_replica
            .as_ref()
            .is_some_and(|status| status.turn_running);
        self.with_active_timeline(cx, |timeline| {
            (
                turn_running,
                timeline.pending_approvals.first().cloned(),
                timeline.pending_approvals.len(),
            )
        })
        .unwrap_or((turn_running, None, 0))
    }

    pub fn composer_is_favorite_model(&self, model: &str, _cx: &App) -> bool {
        self.settings_replica
            .favorite_models
            .iter()
            .any(|favorite| favorite == model)
    }

    pub fn chat_active_session(&self, _cx: &App) -> Option<(String, PathBuf, bool)> {
        self.session_status_replica
            .as_ref()
            .map(|status| (status.title.clone(), status.cwd.clone(), status.draft))
    }

    pub fn chat_requested_model(&self, _cx: &App) -> Option<String> {
        self.session_status_replica
            .as_ref()
            .and_then(|status| status.requested_model.clone())
    }

    pub fn chat_turn_changes(
        &self,
        turn: usize,
        cx: &App,
    ) -> (Vec<agent::FileChange>, agent::ChangeCompleteness) {
        self.with_active_timeline(cx, |timeline| {
            timeline
                .turns
                .get(turn)
                .and_then(|turn| turn.changes.as_ref())
                .map(|changes| (changes.changes.clone(), changes.completeness))
        })
        .flatten()
        .unwrap_or((Vec::new(), agent::ChangeCompleteness::Partial))
    }

    pub fn chat_native_rewind_state(&self, turn: usize, cx: &App) -> Option<(bool, bool)> {
        let status = self.session_status_replica.as_ref()?;
        let has_checkpoint = self
            .with_active_timeline(cx, |timeline| {
                timeline
                    .turns
                    .get(turn)
                    .and_then(|turn| turn.provider_checkpoint_id.as_ref())
                    .is_some()
            })
            .unwrap_or(false);
        Some((
            status.provider == agent::ProviderKind::ClaudeCode && has_checkpoint,
            status.turn_running
                || !status.queued_messages.is_empty()
                || status.native_rewind_pending,
        ))
    }

    pub fn chat_panel_state(
        &self,
        cx: &App,
    ) -> (bool, tcode_core::ui::RightTab, bool, bool, bool, f32) {
        let _ = cx;
        self.active_conversation_ui()
            .map(|ui| {
                (
                    ui.right_panel_open,
                    ui.right_tab,
                    ui.right_panel_open && ui.right_tab == RightTab::Plan,
                    ui.right_panel_open && ui.right_tab == RightTab::Preview,
                    ui.terminal_open,
                    ui.terminal_height,
                )
            })
            .unwrap_or((false, RightTab::default(), false, false, false, 240.))
    }

    pub fn chat_git_controls(&self, _cx: &App) -> Option<(QuickAction, Vec<MenuItem>)> {
        self.git_status_replica.status.as_ref().map(|status| {
            (
                quick_action(status, self.git_status_replica.busy),
                menu_items(status, self.git_status_replica.busy),
            )
        })
    }

    pub fn chat_git_status_loaded(&self, _cx: &App) -> bool {
        self.git_status_replica.status.is_some()
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
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let host = self.host.clone();
        cx.spawn(
            async move |_| match host.query(Query::GenerateCommitMessage { included }).await {
                Ok(QueryResponse::CommitMessage(message)) => Ok(message),
                Ok(other) => Err(format!("unexpected commit-message response: {other:?}")),
                Err(error) => Err(error.message),
            },
        )
    }

    pub fn plan_panel_state(&self, cx: &App) -> (Option<String>, Vec<agent::PlanStep>) {
        self.with_active_timeline(cx, |timeline| {
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

    pub fn worktree_orphaned_by_delete(&self, session_id: &str, _cx: &App) -> Option<WorktreeInfo> {
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

    pub(crate) fn open_add_project_dialog(store: Entity<Self>, window: &mut Window, cx: &mut App) {
        crate::add_project_dialog::open(store, window, cx);
    }
}

impl EventEmitter<RuntimeEvent> for WorkspaceStore {}

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
    use tcode_runtime::{
        app::AppState,
        event::HostEvent,
        host::HostCx,
        pipe::{HostHandle, HostServices, spawn_host},
    };
    use tcode_services::store::SessionStore;

    use super::WorkspaceStore;

    fn test_host(store: SessionStore) -> HostHandle {
        spawn_host(store, HostServices::default()).expect("spawn test host")
    }

    fn update_host<R>(
        host: &HostHandle,
        update: impl FnOnce(&mut AppState, &mut HostCx) -> R + Send + 'static,
    ) -> R
    where
        R: Send + 'static,
    {
        smol::block_on(host.update_state_for_test(update)).expect("update test host")
    }

    fn command(host: &HostHandle, command: Command) {
        smol::block_on(host.command(command)).expect("serialized host command");
    }

    fn shutdown_test_host(host: &HostHandle) {
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
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("timed out waiting for {description}");
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
        let workspace = cx.new(|cx| WorkspaceStore::new(host.clone(), cx));
        command(
            &host,
            Command::SelectSession {
                session_id: session_id.clone(),
            },
        );
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
            update_host(&host, move |state, cx| {
                state
                    .active
                    .as_mut()
                    .expect("active session")
                    .timeline
                    .apply_at(Some(200 + offset as u64), &event);
                cx.emit(HostEvent::Domain(EventEnvelope {
                    topic: Topic::SessionEvents {
                        session_id: event_session_id,
                    },
                    seq: 10 + offset as u64,
                    event: ServerEvent::SessionEvent(SessionEventRecord {
                        ts: Some(200 + offset as u64),
                        event,
                    }),
                }));
            });
        }
        let live = update_host(&host, |state, _| {
            let timeline = &state.active.as_ref().expect("active session").timeline;
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
        let workspace = cx.new(|cx| WorkspaceStore::new(host.clone(), cx));

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
        let mut settings = workspace.read_with(cx, |store, cx| store.settings(cx));
        settings.word_wrap_diffs = !settings.word_wrap_diffs;
        let expected_word_wrap = settings.word_wrap_diffs;
        command(&host, Command::UpdateSettings { settings });
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

        let live_index = update_host(&host, |state, _| {
            let AppState {
                sessions, projects, ..
            } = state;
            (
                serde_json::to_value(sessions).unwrap(),
                serde_json::to_value(projects).unwrap(),
            )
        });
        let live_settings = update_host(&host, |state, _| {
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
        let workspace = cx.new(|cx| WorkspaceStore::new(host.clone(), cx));
        command(
            &host,
            Command::SelectSession {
                session_id: session_id.clone(),
            },
        );
        wait_until(cx, &workspace, "selected session status", |cx| {
            workspace.read_with(cx, |store, _| {
                store
                    .session_status_replica
                    .as_ref()
                    .is_some_and(|status| status.session_id == session_id)
            })
        });
        update_host(&host, |state, cx| {
            state.queue_message_for_replica_test("queued for replication".into(), cx);
        });
        command(
            &host,
            Command::SetInteractionMode {
                mode: agent::InteractionMode::Plan,
            },
        );
        command(
            &host,
            Command::AddReviewComment {
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

        let live = update_host(&host, move |state, _| {
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
            workspace.read_with(cx, |store, cx| store.composer_review_comments(cx)),
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
        let workspace = cx.new(|cx| WorkspaceStore::new(host.clone(), cx));
        command(
            &host,
            Command::SelectSession {
                session_id: session_id.clone(),
            },
        );
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
                topic: Topic::SessionStatus {
                    session_id: background_session_id.clone(),
                },
                seq: 1,
                event: ServerEvent::SessionStatusReplaced(status),
            });
        });

        assert!(workspace.read_with(cx, |store, cx| {
            store.pending_user_input_for(&background_session_id, cx)
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
        let workspace = cx.new(|cx| WorkspaceStore::new(host.clone(), cx));
        command(
            &host,
            Command::SelectSession {
                session_id: first.id.clone(),
            },
        );
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
                topic: Topic::SessionStatus {
                    session_id: first.id.clone(),
                },
                seq: 1,
                event: ServerEvent::SessionStatusReplaced(parked.clone()),
            });

            let mut next = parked;
            next.session_id = second.id.clone();
            next.cwd = second.cwd.clone();
            next.turn_running = false;
            next.working = false;
            store.apply_domain_event(&EventEnvelope {
                topic: Topic::SessionStatus {
                    session_id: second.id.clone(),
                },
                seq: 1,
                event: ServerEvent::SessionStatusReplaced(next.clone()),
            });
            store.apply_domain_event(&EventEnvelope {
                topic: Topic::ActiveSession,
                seq: 2,
                event: ServerEvent::ActiveSessionReplaced(Some(next)),
            });
        });

        assert!(workspace.read_with(cx, |store, cx| { store.turn_running_for(&first.id, cx) }));
        assert!(!workspace.read_with(cx, |store, cx| { store.turn_running_for(&second.id, cx) }));
        assert_eq!(
            workspace.read_with(cx, |store, cx| store.working_sessions_count(cx)),
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
        let workspace = cx.new(|cx| WorkspaceStore::new(host.clone(), cx));
        command(
            &host,
            Command::SelectSession {
                session_id: first.id.clone(),
            },
        );
        wait_until(cx, &workspace, "first selected session", |cx| {
            workspace.read_with(cx, |store, _| {
                store
                    .session_status_replica
                    .as_ref()
                    .is_some_and(|status| status.session_id == first.id)
            })
        });

        for (seq, session_id, text) in [
            (1, first.id.clone(), "first parked prefill".to_string()),
            (2, second.id.clone(), "second parked prefill".to_string()),
        ] {
            update_host(&host, move |_state, cx| {
                cx.emit(HostEvent::Domain(EventEnvelope {
                    topic: Topic::SessionStatus {
                        session_id: session_id.clone(),
                    },
                    seq,
                    event: ServerEvent::NativeRewindPrefill { session_id, text },
                }));
            });
        }
        wait_until(cx, &workspace, "both rewind prefill events", |cx| {
            workspace.read_with(cx, |store, _| store.native_rewind_prefills.len() == 2)
        });

        assert_eq!(
            workspace.update(cx, |store, cx| store.take_native_rewind_prefill(cx)),
            Some("first parked prefill".into())
        );
        command(
            &host,
            Command::SelectSession {
                session_id: second.id.clone(),
            },
        );
        wait_until(cx, &workspace, "second selected session", |cx| {
            workspace.read_with(cx, |store, _| {
                store
                    .session_status_replica
                    .as_ref()
                    .is_some_and(|status| status.session_id == second.id)
            })
        });
        assert_eq!(
            workspace.update(cx, |store, cx| store.take_native_rewind_prefill(cx)),
            Some("second parked prefill".into())
        );

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
        let host = test_host(session_store);
        let workspace = cx.new(|cx| WorkspaceStore::new(host.clone(), cx));

        update_host(&host, |state, cx| {
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
                .provider_versions
                .entry(ProviderKind::Codex)
                .or_default()
                .checking = true;
            state.git_status = Some(GitStatus {
                is_repo: true,
                branch: Some("feature/replica".into()),
                has_working_tree_changes: true,
                changed_files: vec![GitFileEntry {
                    path: "src/replica.rs".into(),
                    insertions: 4,
                    deletions: 2,
                }],
                ..Default::default()
            });
            state.git_busy = true;
            state.emit_provider_and_git_replicas_for_test(cx);
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

        let (live_providers, live_git) = update_host(&host, |state, _| {
            (
                state.providers_status_snapshot(),
                state.git_status_snapshot(),
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
