use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    rc::Rc,
};

use crate::overlay::{DialogButtons, Notification, OverlayExt as _};
use crate::remote::spaces::{self, ShareTarget, SpacesObserver};
use crate::scroll::ScrollableElement as _;
use crate::theme::ActiveTheme as _;
use crate::widgets::button::{Button, ButtonVariant, ButtonVariants as _};
use crate::widgets::input::{Input, InputEvent, InputState};
use crate::widgets::menu::{ContextMenuExt as _, DropdownMenu as _};
use crate::widgets::spinner::Spinner;
use crate::widgets::tooltip::Tooltip;
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use gpui::{
    Action, AnimationExt as _, App, AppContext as _, Context, Entity, InteractiveElement as _,
    IntoElement, ListAlignment, ListState, ParentElement as _, Render, Role, SharedString,
    SpringAnimation, SpringConfig, StatefulInteractiveElement as _, Styled as _, Subscription,
    Window, canvas, div, list, prelude::FluentBuilder as _, px,
};
use gpui_base::{Scrollbar, StyledExt as _, h_flex, v_flex};
use serde::Deserialize;
use tcode_core::settlement::AgentDelivery;
use tcode_core::thread_sort::{ThreadSection, ThreadSections, partition_threads};
use tcode_protocol::{Command, ThreadExportFormat};

use tcode_core::{
    project::{ProjectGroup, SessionMeta},
    settings::SidebarLayout,
};

use crate::shortcut::format_secondary_shortcut;
use crate::store::{ForkAvailability, StoreChange, TopicKind, WorkspaceStore};
use crate::time::{humanize_ago, now_secs};
use crate::window_drag_area;
use crate::window_state::{Destination, Route, WindowState};

mod arrange;
use arrange::{DraggedThread, DropZone, ThreadDrag};

/// The provider mark behind a thread row: its provider's glyph at this alpha,
/// sized to the row height minus this vertical inset so it sits inside the row.
const PROVIDER_MARK_ALPHA: f32 = 0.32;
const PROVIDER_MARK_INSET: f32 = 8.;
/// Horizontal padding of the grouped and flat thread rows (`px_2` / `pr_2`).
const THREAD_ROW_PADDING_X: f32 = 8.;

/// Left padding on the sidebar's top row so branding clears the native macOS
/// traffic lights (ending near x=72 on macOS 26); a small inset elsewhere.
#[cfg(target_os = "macos")]
const TRAFFIC_LIGHT_INSET: f32 = 80.;
#[cfg(not(target_os = "macos"))]
const TRAFFIC_LIGHT_INSET: f32 = 8.;

/// Flat-list row geometry, including the 2px gap reserved below every row.
const FLAT_ROW_HEIGHT: f32 = 50.;
/// The clickable row inside each flat slot (the slot adds 2px of spacing).
const FLAT_ROW_INNER_HEIGHT: f32 = 48.;
const GROUPED_ROW_HEIGHT: f32 = 30.;
const SETTLED_HEADER_HEIGHT: f32 = 34.;

/// A critically damped spring keeps reordering legible without bouncing rows
/// past their destinations. GPUI also makes this snap to the target when the
/// operating system's reduced-motion preference is enabled.
const FLAT_REORDER_SPRING: SpringConfig = SpringConfig::new(420., 41., 1.);

/// A sidebar label that owns the remaining row width and always truncates on
/// one line. `text_ellipsis` alone still leaves GPUI's default wrapping on,
/// which lets a glyph move onto a second line at resize boundaries.
fn truncated_sidebar_label() -> gpui::Div {
    div().flex_1().min_w_0().truncate()
}

/// Fold indicator for a collapsible section header.
fn collapse_chevron(collapsed: bool, cx: &Context<SessionsSidebar>) -> Icon {
    Icon::new(if collapsed {
        IconName::ChevronRight
    } else {
        IconName::ChevronDown
    })
    .flex_none()
    .size_3()
    .text_color(cx.theme().muted_foreground)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ThreadFlags {
    unread: bool,
    waiting_for_approval: bool,
    waiting_for_input: bool,
    working: bool,
    failed: bool,
    /// Background tasks run, or a child thread has not finished.
    waiting: bool,
}

#[derive(Clone)]
struct ThreadRowState {
    session_id: String,
    row_key: String,
    waiting_for_approval: bool,
    waiting_for_input: bool,
    waiting: bool,
    failed: bool,
    auto_settle_enabled: bool,
    is_worktree: bool,
    show_unread: bool,
    renaming: Option<Entity<InputState>>,
    menu_can_fork: bool,
    title_generating: bool,
    /// Numbers of the pull requests a watch keeps this thread waiting on.
    watching: Vec<u64>,
}

impl ThreadRowState {
    fn waiting(&self) -> bool {
        self.waiting_for_approval || self.waiting_for_input
    }
}

/// What a Waiting thread waits on, from the agents its host reports: those
/// still running, those awaiting its settle, the pull requests it watches, or
/// else its own background work.
fn waiting_reason(store: &WorkspaceStore, session_id: &str, watching: &[u64]) -> String {
    let (mut running, mut unsettled) = (0, 0);
    for meta in store.sidebar_sessions() {
        if meta.parent_session_id.as_deref() != Some(session_id) {
            continue;
        }
        match store.agent_status(&meta.id).map(|status| status.delivery) {
            Some(AgentDelivery::Running) => running += 1,
            Some(AgentDelivery::AwaitingSettle) => unsettled += 1,
            _ => {}
        }
    }
    let mut parts = Vec::new();
    match running {
        0 => {}
        1 => parts.push(crate::tr!("sidebar.waiting_agents_running_one").into_owned()),
        count => {
            parts.push(crate::tr!("sidebar.waiting_agents_running", count = count).into_owned())
        }
    }
    match unsettled {
        0 => {}
        1 => parts.push(crate::tr!("sidebar.waiting_agents_unsettled_one").into_owned()),
        count => {
            parts.push(crate::tr!("sidebar.waiting_agents_unsettled", count = count).into_owned())
        }
    }
    match watching {
        [] => {}
        [number] => parts.push(
            crate::tr!("sidebar.watching_pull_request", number = number.to_string()).into_owned(),
        ),
        numbers => parts.push(
            crate::tr!(
                "sidebar.watching_pull_requests",
                count = numbers.len().to_string()
            )
            .into_owned(),
        ),
    }
    if parts.is_empty() {
        parts.push(crate::tr!("sidebar.waiting_background").into_owned());
    }
    parts.join(" · ")
}

/// The rows a list shows by section, narrowed to one project when a filter is set.
fn project_threads<'a>(
    sessions: &'a [SessionMeta],
    project: Option<&str>,
) -> ThreadSections<&'a SessionMeta> {
    partition_threads(
        sessions
            .iter()
            .filter(|meta| project.is_none_or(|id| meta.project_id.as_deref() == Some(id))),
    )
}

fn animate_flat_thread_position(row: gpui::Div, session_id: &str, target_top: f32) -> gpui::Div {
    // `list` positions each item root explicitly during prepaint, which
    // overrides relative offsets applied to that root. Keep an unanimated
    // outer item for the list to position and move the row inside it instead.
    div().w_full().child(
        row.with_spring(
            gpui::SharedString::from(format!("flat-thread-position-{session_id}")),
            SpringAnimation::new(FLAT_REORDER_SPRING)
                .to(px(target_top))
                .with_epsilon(0.25),
            move |row, animated_top| row.relative().top(animated_top - px(target_top)),
        ),
    )
}

// Thread-row context-menu actions (each carries the target session id, so a
// single set of handlers on the sidebar root serves every row).
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadRename(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadRegenerateTitle(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadFork(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadMergeWorktree(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadMarkUnread(String);

#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = tcode_sidebar, no_json)]
struct ThreadLinkPullRequest(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadCopyPath(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadCopyId(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadExportJsonl(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadExportMarkdown(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadArchive(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadSettle(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadAutoSettle(String, bool);

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode, no_json)]
pub(crate) struct ThreadUndo;

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadPin(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadUnpin(String);
/// Move a thread one place up (`false`) or down (`true`) within its section.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadMove(String, bool);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadArrange(String);

#[derive(Clone, Copy, PartialEq, Eq)]
enum UndoKind {
    Settle,
    Unpin,
    Archive,
}
struct LifecycleUndo {
    kind: UndoKind,
    entries: Vec<UndoEntry>,
}
/// What reverses one undoable action, sent in order.
struct UndoEntry {
    commands: Vec<Command>,
    reopen: Option<String>,
}
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadMakeActive(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadDelete(String);

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_project, no_json)]
struct ProjectArchiveAll(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_project, no_json)]
struct ProjectThreadRules(String);

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_project, no_json)]
struct ProjectDelete(String);

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Action)]
#[action(namespace = tcode_project, no_json)]
struct ChangeProjectIcon(String);
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Action)]
#[action(namespace = tcode_project, no_json)]
struct ChangeProjectRoot(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_project, no_json)]
struct ProjectReveal(String);

/// Ask before removing a project and its threads from Tcode.
pub(super) fn confirm_remove_project(
    store: Entity<WorkspaceStore>,
    project_id: String,
    window: &mut Window,
    cx: &mut App,
) {
    let Some((project_name, count)) = store.read(cx).project_summary(&project_id) else {
        return;
    };
    window.open_alert_dialog(cx, move |alert, _, cx| {
        let alert = alert.bg(cx.theme().popover);
        let store = store.clone();
        let project_id = project_id.clone();
        alert
            .title(crate::tr!(
                "sidebar.remove_project_title",
                project = project_name.clone()
            ))
            .description(crate::tr!(
                "sidebar.remove_project_description",
                count = count
            ))
            .button_props(
                DialogButtons::default()
                    .ok_variant(ButtonVariant::Danger)
                    .ok_text(crate::tr!("sidebar.remove_project_action"))
                    .cancel_text(crate::tr!("settings.cancel"))
                    .show_cancel(true),
            )
            .on_ok(move |_, _, cx| {
                store.update(cx, |store, _cx| {
                    store.delete_project(project_id.clone());
                });
                true
            })
    });
}

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_sidebar, no_json)]
struct FilterProject(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_sidebar, no_json)]
struct StartDraftForProject(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_sidebar, no_json)]
struct QuickChat;

/// In-progress inline rename of a thread row.
struct RenameState {
    session_id: String,
    input: Entity<InputState>,
    _sub: Subscription,
}

#[derive(Clone)]
struct CompactThreadRow {
    meta: SessionMeta,
    state: ThreadRowState,
    working: bool,
    title: SharedString,
    row_id: SharedString,
    label: SharedString,
    project_name: Option<SharedString>,
    relative_time: SharedString,
    separator: bool,
}

#[derive(Clone)]
struct CompactProjectRow {
    project_id: String,
    row_id: SharedString,
    name: SharedString,
    label: SharedString,
    count: SharedString,
    collapsed: bool,
}

#[derive(Clone)]
enum CompactListRow {
    Project(CompactProjectRow),
    Caption { key: String, label: SharedString },
    Settled { key: String, count: usize },
    More { key: String, count: usize },
    Empty { key: String },
    Thread(Rc<CompactThreadRow>),
    BottomInset,
}

impl CompactListRow {
    fn key(&self) -> &str {
        match self {
            Self::Project(row) => &row.row_id,
            Self::Caption { key, .. } => key,
            Self::Settled { key, .. } => key,
            Self::More { key, .. } => key,
            Self::Empty { key } => key,
            Self::Thread(row) => &row.row_id,
            Self::BottomInset => "compact-bottom-inset",
        }
    }
}

enum FlatListRow {
    Thread(Box<SessionMeta>, f32),
    /// The start of Pinned or Active, drawn only while dragging; whether the
    /// section is empty.
    Boundary(ThreadSection, bool),
    Settled,
    More(usize),
    Empty,
}

/// A row of the desktop grouped list. Project-scoped rows index into the
/// frame's `grouped_sessions`.
enum GroupedListRow {
    Project {
        group: usize,
        collapsed: bool,
    },
    /// The start of a group's Pinned or Active rows, drawn only while dragging.
    Boundary {
        group: usize,
        section: ThreadSection,
        empty: bool,
    },
    Thread(Box<SessionMeta>),
    More {
        group: usize,
        count: usize,
    },
    Empty {
        group: usize,
    },
    Settled {
        group: usize,
        count: usize,
    },
}

impl GroupedListRow {
    fn key<'a>(&'a self, groups: &'a [ProjectGroup]) -> (&'static str, &'a str) {
        let project = |group: &usize| groups[*group].project.id.as_str();
        match self {
            Self::Project { group, .. } => ("project", project(group)),
            Self::Boundary {
                group,
                section: ThreadSection::Pinned,
                ..
            } => ("pinned", project(group)),
            Self::Boundary { group, .. } => ("active", project(group)),
            Self::Thread(meta) => ("thread", &meta.id),
            Self::More { group, .. } => ("more", project(group)),
            Self::Empty { group } => ("empty", project(group)),
            Self::Settled { group, .. } => ("settled", project(group)),
        }
    }
}

#[derive(Clone)]
struct CompactListModel {
    rows: Vec<CompactListRow>,
    has_projects: bool,
    locale: String,
    minute: u64,
}

pub struct SessionsSidebar {
    store: Entity<WorkspaceStore>,
    window_state: Entity<WindowState>,
    /// Project ids whose thread list is expanded past the collapsed limit.
    expanded_settled: HashSet<String>,
    settled_limits: HashMap<String, usize>,
    lifecycle_undo: Option<LifecycleUndo>,
    drag: Option<ThreadDrag>,
    /// The project the Arrange threads page shows; `None` is every project.
    arrange_scope: Option<String>,
    arrange_settled_expanded: bool,
    arrange_list_state: ListState,
    arrange_row_keys: Vec<String>,
    last_selected: Option<String>,
    /// Optional project id filter for the session-local flat list.
    project_filter: Option<String>,
    settled_scope: Option<(SidebarLayout, Option<String>)>,
    /// The thread currently being renamed inline, if any.
    renaming: Option<RenameState>,
    flat_list_state: ListState,
    grouped_list_state: ListState,
    /// The row keys `grouped_list_state` was last spliced for.
    grouped_row_keys: Vec<(&'static str, String)>,
    compact_list_state: ListState,
    compact_model: Option<Rc<CompactListModel>>,
    compact_model_dirty: bool,
    /// Returning from a thread scrolls its row into the upper part of the
    /// list; the row's next paint performs the scroll and clears this.
    compact_reveal_active: bool,
    #[cfg(test)]
    compact_rows_rendered: std::cell::Cell<usize>,
    spaces_observer: SpacesObserver,
    _subscriptions: Vec<Subscription>,
}

/// Included desktop rows plus the pre-disclosure counts used by list controls.
struct ThreadRows<'a> {
    pinned: Vec<&'a SessionMeta>,
    active: Vec<&'a SessionMeta>,
    settled: Vec<&'a SessionMeta>,
    settled_count: usize,
    settled_hidden_count: usize,
}

fn session_flags(sessions: &[SessionMeta], store: &WorkspaceStore) -> HashMap<String, ThreadFlags> {
    sessions
        .iter()
        .map(|meta| {
            (
                meta.id.clone(),
                ThreadFlags {
                    unread: store.session_unread(&meta.id),
                    waiting_for_approval: store.pending_approval_for(&meta.id),
                    waiting_for_input: store.pending_user_input_for(&meta.id),
                    working: store.turn_running_for(&meta.id),
                    failed: store.failed_for(&meta.id),
                    waiting: store.waiting_for(&meta.id),
                },
            )
        })
        .collect()
}

impl SessionsSidebar {
    /// The rows `scope` shows, as a drag over them would leave them.
    fn thread_rows<'a>(
        &self,
        sessions: &'a [SessionMeta],
        scope: Option<&str>,
        key: &str,
    ) -> ThreadRows<'a> {
        let sections = project_threads(sessions, scope);
        let ThreadSections {
            pinned,
            active,
            mut settled,
        } = match &self.drag {
            Some(drag) => drag.preview(scope, sections),
            None => sections,
        };
        let settled_count = settled.len();
        self.limit_settled_rows(key, &mut settled);
        ThreadRows {
            pinned,
            active,
            settled_hidden_count: settled_count - settled.len(),
            settled,
            settled_count,
        }
    }

    fn grouped_rows(&self, groups: &[ProjectGroup], store: &WorkspaceStore) -> Vec<GroupedListRow> {
        let mut rows = Vec::new();
        for (group, project) in groups.iter().enumerate() {
            let project_id = &project.project.id;
            let collapsed = store.is_project_collapsed(project_id);
            rows.push(GroupedListRow::Project { group, collapsed });
            if collapsed {
                continue;
            }
            let threads = self.thread_rows(&project.sessions, Some(project_id), project_id);
            let thread = |meta: &&SessionMeta| GroupedListRow::Thread(Box::new((*meta).clone()));
            rows.push(GroupedListRow::Boundary {
                group,
                section: ThreadSection::Pinned,
                empty: threads.pinned.is_empty(),
            });
            rows.extend(threads.pinned.iter().map(thread));
            rows.push(GroupedListRow::Boundary {
                group,
                section: ThreadSection::Active,
                empty: threads.active.is_empty(),
            });
            rows.extend(threads.active.iter().map(thread));
            if threads.settled_count > 0 {
                if threads.pinned.is_empty() && threads.active.is_empty() {
                    rows.push(GroupedListRow::Empty { group });
                }
                rows.push(GroupedListRow::Settled {
                    group,
                    count: threads.settled_count,
                });
                rows.extend(threads.settled.iter().map(thread));
                if let Some(count) =
                    self.settled_more_count(project_id, threads.settled_hidden_count)
                {
                    rows.push(GroupedListRow::More { group, count });
                }
            }
        }
        rows
    }

    /// Use the same filters, disclosures and ordering as the rendered list,
    /// including rows outside the scroll viewport.
    fn navigation_threads(&mut self, cx: &mut Context<Self>) -> Vec<String> {
        if self.compact(cx) {
            return self
                .compact_model(cx)
                .rows
                .iter()
                .filter_map(|row| match row {
                    CompactListRow::Thread(row) => Some(row.meta.id.clone()),
                    _ => None,
                })
                .collect();
        }

        let store = self.store.read(cx);
        let mut ids = Vec::new();
        match store.sidebar_layout() {
            SidebarLayout::Flat => {
                let sessions = store.flat_sessions();
                let rows = self.thread_rows(&sessions, self.project_filter.as_deref(), "recent");
                ids.extend(
                    rows.pinned
                        .into_iter()
                        .chain(rows.active)
                        .chain(rows.settled)
                        .map(|meta| meta.id.clone()),
                );
            }
            SidebarLayout::Grouped => {
                ids.extend(
                    self.grouped_rows(&store.grouped_sessions(), store)
                        .into_iter()
                        .filter_map(|row| match row {
                            GroupedListRow::Thread(meta) => Some(meta.id),
                            _ => None,
                        }),
                );
            }
        }
        ids
    }

    pub(crate) fn navigate_thread(
        &mut self,
        action: &crate::shortcut::NavigateThread,
        cx: &mut Context<Self>,
    ) -> bool {
        use crate::shortcut::NavigateThread;

        let threads = self.navigation_threads(cx);
        if threads.is_empty() {
            return false;
        }
        let active = self.store.read(cx).roster_session_id();
        let current = threads.iter().position(|id| Some(id) == active.as_ref());
        let index = match action {
            NavigateThread::Index(index) => *index,
            NavigateThread::Next => current.map_or(0, |index| (index + 1) % threads.len()),
            NavigateThread::Previous => current.map_or(threads.len() - 1, |index| {
                (index + threads.len() - 1) % threads.len()
            }),
        };
        if let Some(id) = threads.get(index) {
            self.compact_model_dirty = true;
            self.store.update(cx, |store, cx| {
                store.select_session(id.clone());
                cx.notify();
            });
            self.window_state
                .update(cx, |state, cx| state.open_thread(cx));
            cx.notify();
            return true;
        }
        false
    }

    fn compact(&self, cx: &gpui::App) -> bool {
        self.window_state.read(cx).compact
    }
    pub fn new(
        store: Entity<WorkspaceStore>,
        window_state: Entity<WindowState>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut last_destination = window_state.read(cx).destination();
        let subscriptions = vec![
            cx.subscribe(&store, |this, _, change: &StoreChange, cx| {
                if matches!(
                    change.topic,
                    TopicKind::Index
                        | TopicKind::Settings
                        | TopicKind::ActiveSession
                        | TopicKind::SessionStatus
                ) {
                    this.compact_model_dirty = true;
                    cx.notify();
                }
            }),
            cx.observe(&window_state, move |this, state, cx| {
                let state = state.read(cx);
                let destination = state.destination();
                if state.compact
                    && last_destination == Destination::Thread
                    && destination == Destination::Threads
                {
                    this.compact_reveal_active = true;
                    cx.notify();
                }
                last_destination = destination;
            }),
        ];
        Self {
            store,
            window_state,
            expanded_settled: HashSet::new(),
            settled_limits: HashMap::new(),
            lifecycle_undo: None,
            drag: None,
            arrange_scope: None,
            arrange_settled_expanded: false,
            arrange_list_state: ListState::new(0, ListAlignment::Top, px(120.)),
            arrange_row_keys: Vec::new(),
            last_selected: None,
            project_filter: None,
            settled_scope: None,
            renaming: None,
            flat_list_state: ListState::new(0, ListAlignment::Top, px(120.)),
            grouped_list_state: ListState::new(0, ListAlignment::Top, px(120.)),
            grouped_row_keys: Vec::new(),
            compact_list_state: ListState::new(0, ListAlignment::Top, px(120.)),
            compact_model: None,
            compact_model_dirty: true,
            compact_reveal_active: false,
            #[cfg(test)]
            compact_rows_rendered: std::cell::Cell::new(0),
            spaces_observer: SpacesObserver::default(),
            _subscriptions: subscriptions,
        }
    }

    /// Prompt for a directory, then create a project rooted there.
    fn add_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.store.read(cx).scope().is_full() {
            return;
        }
        crate::add_project_dialog::open(self.store.clone(), window, cx);
    }

    fn toggle_project(&mut self, project_id: &str, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| {
            store.toggle_project_collapsed(project_id.to_string(), cx);
        });
        cx.notify();
    }

    fn on_filter_project(
        &mut self,
        action: &FilterProject,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.project_filter = (!action.0.is_empty()).then(|| action.0.clone());
        self.expanded_settled.clear();
        self.settled_limits.clear();
        self.compact_model_dirty = true;
        self.window_state
            .update(cx, |state, cx| state.leave_route_for_chat(cx));
        cx.notify();
    }

    fn on_quick_chat(&mut self, _: &QuickChat, _window: &mut Window, cx: &mut Context<Self>) {
        self.store
            .update(cx, |store, cx| store.start_scratch_draft(cx));
        self.window_state
            .update(cx, |state, cx| state.open_thread(cx));
    }

    fn on_start_draft_for_project(
        &mut self,
        action: &StartDraftForProject,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self
            .store
            .read(cx)
            .projects()
            .into_iter()
            .find(|project| project.id == action.0)
        else {
            return;
        };
        self.store.update(cx, |store, cx| {
            store.start_draft(project.id, project.root, cx);
        });
        self.window_state
            .update(cx, |state, cx| state.open_thread(cx));
    }

    fn on_link_pull_request(
        &mut self,
        action: &ThreadLinkPullRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        crate::pull_requests::open_link_dialog(self.store.clone(), action.0.clone(), window, cx);
    }
    fn on_rename(&mut self, action: &ThreadRename, window: &mut Window, cx: &mut Context<Self>) {
        let session_id = action.0.clone();
        let title = self
            .store
            .read(cx)
            .sidebar_sessions()
            .iter()
            .find(|m| m.id == session_id)
            .map(|m| m.title.clone())
            .unwrap_or_default();
        let input = cx.new(|cx| InputState::new(window, cx));
        input.update(cx, |state, cx| {
            state.set_value(&title, window, cx);
            state.focus(window, cx);
        });
        let sub = cx.subscribe_in(
            &input,
            window,
            |this, _input, event, window, cx| match event {
                InputEvent::PressEnter { .. } => this.commit_rename(window, cx),
                InputEvent::Blur => this.cancel_rename(cx),
                _ => {}
            },
        );
        self.renaming = Some(RenameState {
            session_id,
            input,
            _sub: sub,
        });
        cx.notify();
    }

    fn on_regenerate_title(
        &mut self,
        action: &ThreadRegenerateTitle,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.store.update(cx, |store, _| {
            store.regenerate_session_title(action.0.clone());
        });
    }

    fn on_fork(&mut self, action: &ThreadFork, window: &mut Window, cx: &mut Context<Self>) {
        let refusal = match self.store.read(cx).fork_availability(&action.0) {
            ForkAvailability::Available => None,
            ForkAvailability::Unsupported => {
                Some(crate::tr!("sidebar.fork_unsupported").into_owned())
            }
            ForkAvailability::Empty => Some(crate::tr!("sidebar.fork_empty").into_owned()),
            ForkAvailability::Running => Some(crate::tr!("sidebar.fork_running").into_owned()),
        };
        if let Some(message) = refusal {
            window.push_notification(Notification::error(message), cx);
            return;
        }
        let id = action.0.clone();
        self.store.update(cx, |store, cx| {
            store.fork_thread(id, cx);
        });
    }

    fn on_merge_worktree(
        &mut self,
        action: &ThreadMergeWorktree,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.store.update(cx, |store, _cx| {
            store.merge_worktree(action.0.clone());
        });
    }

    fn commit_rename(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = self.renaming.take() {
            let value = state.input.read(cx).value().to_string();
            self.store.update(cx, |store, _cx| {
                store.rename_session(state.session_id, value);
            });
            cx.notify();
        }
    }

    fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        if self.renaming.take().is_some() {
            cx.notify();
        }
    }

    fn on_mark_unread(
        &mut self,
        action: &ThreadMarkUnread,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = action.0.clone();
        self.store.update(cx, |store, cx| {
            store.mark_session_unread(id, cx);
        });
    }

    fn on_copy_path(
        &mut self,
        action: &ThreadCopyPath,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(meta) = self
            .store
            .read(cx)
            .sidebar_sessions()
            .iter()
            .find(|m| m.id == action.0)
        {
            let path = meta.cwd.to_string_lossy().into_owned();
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(path));
        }
    }

    fn on_copy_id(&mut self, action: &ThreadCopyId, _window: &mut Window, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(action.0.clone()));
    }

    fn prompt_export(
        &self,
        session_id: &str,
        format: ThreadExportFormat,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(meta) = self
            .store
            .read(cx)
            .sidebar_sessions()
            .into_iter()
            .find(|meta| meta.id == session_id)
        else {
            return;
        };
        crate::thread_export::prompt_thread_export(
            self.store.clone(),
            meta.id,
            meta.cwd,
            format,
            window,
            cx,
        );
    }

    fn on_export_jsonl(
        &mut self,
        action: &ThreadExportJsonl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.prompt_export(&action.0, ThreadExportFormat::Jsonl, window, cx);
    }

    fn on_export_markdown(
        &mut self,
        action: &ThreadExportMarkdown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.prompt_export(&action.0, ThreadExportFormat::Markdown, window, cx);
    }

    fn on_auto_settle(
        &mut self,
        action: &ThreadAutoSettle,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.store.update(cx, |store, _| {
            store.set_auto_settle(action.0.clone(), action.1)
        });
    }

    fn on_settle(&mut self, action: &ThreadSettle, window: &mut Window, cx: &mut Context<Self>) {
        self.settle_thread(&action.0, window, cx);
    }

    /// Settle a thread; its undo also restores a pin it loses.
    fn settle_thread(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let pin = self
            .store
            .read(cx)
            .thread_meta(id)
            .filter(|meta| meta.pinned_at.is_some())
            .map(|meta| Command::PinSession {
                session_id: id.to_owned(),
                order_key: meta.pin_order.clone(),
            });
        let undo = std::iter::once(Command::UnsettleSession {
            session_id: id.to_owned(),
        })
        .chain(pin)
        .collect();
        self.perform_lifecycle(
            vec![Command::SettleSession {
                session_id: id.to_owned(),
            }],
            Some((UndoKind::Settle, undo)),
            window,
            cx,
        );
    }

    /// Send `commands` in order and stop at the first refusal; once every
    /// one is accepted, offer `undo` in the shared toast.
    fn perform_lifecycle(
        &mut self,
        commands: Vec<Command>,
        undo: Option<(UndoKind, Vec<Command>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let reopen = match commands.first() {
            Some(Command::ArchiveSession { session_id })
                if self.store.read(cx).active_session_id().as_ref() == Some(session_id) =>
            {
                Some(session_id.clone())
            }
            _ => None,
        };
        let store = self.store.clone();
        cx.spawn_in(window, async move |this, cx| {
            for command in commands {
                let settling = matches!(command, Command::SettleSession { .. });
                let Ok(request) =
                    cx.update(|_, cx| store.update(cx, |store, cx| store.command(command, cx)))
                else {
                    return;
                };
                if let Err(error) = request.await {
                    let _ = this.update_in(cx, |_, window, cx| {
                        let message = if settling && error.code == "thread_busy" {
                            crate::tr!("sidebar.settle_refused").into_owned()
                        } else {
                            error.message
                        };
                        window.push_notification(Notification::warning(message), cx)
                    });
                    return;
                }
            }
            if let Some((kind, commands)) = undo {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.push_lifecycle_undo(kind, UndoEntry { commands, reopen }, window, cx);
                });
            }
        })
        .detach();
    }

    fn push_lifecycle_undo(
        &mut self,
        kind: UndoKind,
        entry: UndoEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let live = window.has_notification::<ThreadUndo>(cx);
        if !live
            || self
                .lifecycle_undo
                .as_ref()
                .is_none_or(|undo| undo.kind != kind)
        {
            self.lifecycle_undo = Some(LifecycleUndo {
                kind,
                entries: vec![],
            });
        }
        self.lifecycle_undo.as_mut().unwrap().entries.push(entry);
        let count = self.lifecycle_undo.as_ref().unwrap().entries.len();
        let title = match (kind, count) {
            (UndoKind::Settle, 1) => crate::tr!("sidebar.undo_settled_one"),
            (UndoKind::Unpin, 1) => crate::tr!("sidebar.undo_unpinned_one"),
            (UndoKind::Archive, 1) => crate::tr!("sidebar.undo_archived_one"),
            (UndoKind::Settle, _) => crate::tr!("sidebar.undo_settled", count = count),
            (UndoKind::Unpin, _) => crate::tr!("sidebar.undo_unpinned", count = count),
            (UndoKind::Archive, _) => crate::tr!("sidebar.undo_archived", count = count),
        }
        .into_owned();
        let weak = cx.entity().downgrade();
        window.push_notification(
            Notification::new()
                .id::<ThreadUndo>()
                .message(title)
                .action(move |_, window, cx| {
                    let weak = weak.clone();
                    let button = Button::new("undo-thread-lifecycle")
                        .small()
                        .label(crate::tr!("sidebar.undo"))
                        .on_click(move |_, window, cx| {
                            let _ = weak.update(cx, |this, cx| this.undo_lifecycle(window, cx));
                        });
                    // The phone pill has no keyboard to hint at.
                    if crate::window_seam::window_is_compact(window, cx) {
                        button.ghost()
                    } else {
                        button.outline().when_some(
                            crate::widgets::kbd::Kbd::binding_for_action(
                                &ThreadUndo,
                                Some("TcodeShell"),
                                window,
                            ),
                            |button, kbd| button.child(kbd),
                        )
                    }
                })
                .autohide(true),
            cx,
        );
    }

    pub(crate) fn undo_lifecycle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !window.has_notification::<ThreadUndo>(cx) {
            self.lifecycle_undo = None;
            return;
        }
        let Some(undo) = self.lifecycle_undo.take() else {
            return;
        };
        window.remove_notification::<ThreadUndo>(cx);
        let store = self.store.clone();
        cx.spawn_in(window, async move |_, cx| {
            for entry in undo.entries.into_iter().rev() {
                for command in entry.commands {
                    let Ok(request) =
                        cx.update(|_, cx| store.update(cx, |store, cx| store.command(command, cx)))
                    else {
                        return;
                    };
                    if let Err(error) = request.await {
                        let _ = cx.update(|window, cx| {
                            window.push_notification(
                                Notification::error(crate::tr!(
                                    "sidebar.undo_failed",
                                    reason = error.message
                                )),
                                cx,
                            )
                        });
                        return;
                    }
                }
                if let Some(id) = entry.reopen {
                    store.update(cx, |store, _| store.select_session(id));
                }
            }
        })
        .detach();
    }

    fn on_make_active(
        &mut self,
        action: &ThreadMakeActive,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.store
            .update(cx, |store, _| store.make_session_active(action.0.clone()));
    }

    fn reveal_selected_settled(&mut self, cx: &mut Context<Self>) {
        let selected = self.store.read(cx).roster_session_id();
        if selected == self.last_selected {
            return;
        }
        let sessions = self.store.read(cx).sidebar_sessions();
        if selected.is_some()
            && !sessions
                .iter()
                .any(|meta| Some(&meta.id) == selected.as_ref())
        {
            return;
        }
        self.last_selected = selected.clone();
        let Some(meta) = sessions
            .iter()
            .find(|meta| Some(&meta.id) == selected.as_ref())
        else {
            return;
        };
        if meta.is_settled()
            && let Some(project_id) = &meta.project_id
            && self.store.read(cx).is_project_collapsed(project_id)
        {
            self.store.update(cx, |store, cx| {
                store.toggle_project_collapsed(project_id.clone(), cx)
            });
        }
        self.compact_model_dirty = true;
    }

    fn limit_settled_rows(&self, key: &str, rows: &mut Vec<&SessionMeta>) {
        let limit = if self.expanded_settled.contains(key) {
            self.settled_limits.get(key).copied().unwrap_or(10)
        } else {
            0
        };
        let selected = self.last_selected.as_deref();
        let mut index = 0;
        rows.retain(|meta| {
            let keep = index < limit || selected == Some(&meta.id);
            index += 1;
            keep
        });
    }

    fn settled_more_count(&self, key: &str, hidden: usize) -> Option<usize> {
        (self.expanded_settled.contains(key) && hidden > 0).then(|| hidden.min(25))
    }

    fn render_settled_more(
        &self,
        key: &str,
        count: usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let key = key.to_owned();
        Button::new(SharedString::from(format!("settled-more-{key}")))
            .debug_selector({
                let key = key.clone();
                move || format!("settled-more-{key}")
            })
            .ghost()
            .icon(IconName::Plus)
            .label(crate::tr!("sidebar.show_more_settled", count = count))
            .w_full()
            .h(px(if self.compact(cx) { 44. } else { 34. }))
            .on_click(cx.listener(move |this, _, _, cx| {
                *this.settled_limits.entry(key.clone()).or_insert(10) += 25;
                this.compact_model_dirty = true;
                cx.notify();
            }))
            .into_any_element()
    }

    fn render_active_empty(&self, cx: &App) -> gpui::AnyElement {
        div()
            .px_2()
            .py_3()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(crate::tr!("sidebar.active_empty"))
            .into_any_element()
    }

    fn render_settled_header(
        &self,
        key: &str,
        count: usize,
        scope: Option<&str>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let expanded = self.expanded_settled.contains(key);
        let color = match &self.drag {
            None => cx.theme().muted_foreground,
            Some(drag) if drag.target(scope) == Some(ThreadSection::Settled) => cx.theme().primary,
            Some(_) => cx.theme().sidebar_foreground,
        };
        let key = key.to_string();
        crate::material::accessible_clickable(
            h_flex(),
            SharedString::from(format!("settled-{key}")),
            Role::Button,
            crate::tr!("sidebar.settled"),
            cx,
        )
        .aria_expanded(expanded)
        .debug_selector({
            let key = key.clone();
            move || format!("settled-{key}")
        })
        .w_full()
        .h(px(if self.compact(cx) {
            44.
        } else {
            SETTLED_HEADER_HEIGHT
        }))
        .gap_2()
        .px_3()
        .text_size(px(12.))
        .text_color(color)
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, _, cx| {
            if !this.expanded_settled.remove(&key) {
                this.expanded_settled.insert(key.clone());
            } else {
                this.settled_limits.remove(&key);
            }
            this.compact_model_dirty = true;
            cx.notify();
        }))
        .child(collapse_chevron(!expanded, cx).text_color(color))
        .child(crate::tr!("sidebar.settled"))
        .child(count.to_string())
        .into_any_element()
    }

    fn on_archive(&mut self, action: &ThreadArchive, window: &mut Window, cx: &mut Context<Self>) {
        let id = action.0.clone();
        let title = self
            .store
            .read(cx)
            .sidebar_sessions()
            .iter()
            .find(|m| m.id == id)
            .map(|m| m.title.clone())
            .unwrap_or_default();
        self.archive_thread(&id, &title, window, cx);
    }

    fn on_delete(&mut self, action: &ThreadDelete, window: &mut Window, cx: &mut Context<Self>) {
        if !self.store.read(cx).scope().is_full() {
            return;
        }
        let id = action.0.clone();
        let title = self
            .store
            .read(cx)
            .sidebar_sessions()
            .iter()
            .find(|m| m.id == id)
            .map(|m| m.title.clone())
            .unwrap_or_default();
        self.delete_thread(&id, &title, window, cx);
    }

    fn on_project_thread_rules(
        &mut self,
        action: &ProjectThreadRules,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(project) = self.store.read(cx).project(&action.0).cloned() {
            crate::settings_page::thread_behavior::open_project_rules(
                self.store.clone(),
                project,
                window,
                cx,
            );
        }
    }

    fn on_project_archive_all(
        &mut self,
        action: &ProjectArchiveAll,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let session_ids: Vec<String> = store
            .read(cx)
            .sidebar_sessions()
            .iter()
            .filter(|meta| {
                meta.project_id.as_deref() == Some(action.0.as_str())
                    && meta.archived_at.is_none()
                    && !store.read(cx).turn_running_for(&meta.id)
            })
            .map(|meta| meta.id.clone())
            .collect();
        if session_ids.is_empty() {
            return;
        }
        let count = session_ids.len();
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let alert = alert.bg(cx.theme().popover);
            let store = store.clone();
            let session_ids = session_ids.clone();
            alert
                .title(crate::tr!("sidebar.archive_all_title"))
                .description(crate::tr!("sidebar.archive_all_description", count = count))
                .button_props(
                    DialogButtons::default()
                        .ok_text(crate::tr!("sidebar.archive_all_action"))
                        .cancel_text(crate::tr!("settings.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, _, cx| {
                    store.update(cx, |store, _cx| {
                        for session_id in &session_ids {
                            store.archive_session(session_id.clone());
                        }
                    });
                    true
                })
        });
    }

    fn on_change_project_icon(
        &mut self,
        action: &ChangeProjectIcon,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.store.read(cx).scope().is_full() {
            return;
        }
        let project = self.store.read(cx).project(&action.0).cloned();
        if let Some(project) = project {
            crate::project_icon::open(self.store.clone(), project, window, cx);
        }
    }

    fn on_project_delete(
        &mut self,
        action: &ProjectDelete,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.store.read(cx).scope().is_full() {
            return;
        }
        confirm_remove_project(self.store.clone(), action.0.clone(), window, cx);
    }

    /// A project's share items, where the machine this window shows has
    /// spaces this client may manage.
    fn share_target(
        &self,
        project_id: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Option<ShareTarget> {
        let project_id = project_id?;
        let project_name = self.store.read(cx).project(project_id)?.name.clone();
        Some(ShareTarget {
            spaces: spaces::for_store(&self.store, cx)?,
            project_id: project_id.to_owned(),
            project_name,
        })
    }

    /// The mark on a project header shared in at least one space.
    fn shared_badge(&self, project_id: &str, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let spaces = spaces::for_store(&self.store, cx)?;
        let names = spaces.read(cx).sharing(project_id);
        (!names.is_empty()).then(|| spaces::shared_badge(project_id, &names, cx))
    }

    fn on_toggle_share(
        &mut self,
        action: &spaces::ToggleShare,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(spaces) = spaces::for_store(&self.store, cx) {
            spaces::toggle_share(&spaces, action, window, cx);
        }
    }

    fn on_new_space_and_share(
        &mut self,
        action: &spaces::NewSpaceAndShare,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(share) = self.share_target(Some(&action.0), cx) {
            spaces::new_space_and_share(
                share.spaces,
                share.project_id,
                share.project_name,
                window,
                cx,
            );
        }
    }

    fn on_change_project_root(
        &mut self,
        action: &ChangeProjectRoot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.store.read(cx).scope().is_full() {
            return;
        }
        crate::change_project_root_dialog::open(self.store.clone(), action.0.clone(), window, cx);
    }

    fn on_project_reveal(
        &mut self,
        action: &ProjectReveal,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(root) = self.store.read(cx).project_root(&action.0) {
            cx.reveal_path(&root);
        }
    }

    /// Archive a thread, honoring the delete-confirmation setting. Blocked while
    /// the turn runs (`archive_session` no-ops then; the caller's tooltip warns).
    fn archive_thread(
        &mut self,
        session_id: &str,
        title: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let sidebar = cx.entity().downgrade();
        if store.read(cx).turn_running_for(session_id) {
            return;
        }
        let session_id = session_id.to_string();
        if store.read(cx).settings().skip_delete_confirmation {
            self.perform_lifecycle(
                vec![Command::ArchiveSession {
                    session_id: session_id.clone(),
                }],
                Some((
                    UndoKind::Archive,
                    vec![Command::UnarchiveSession { session_id }],
                )),
                window,
                cx,
            );
            return;
        }
        let title = title.to_string();
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let alert = alert.bg(cx.theme().popover);
            let sidebar = sidebar.clone();
            let session_id = session_id.clone();
            alert
                .title(crate::tr!("sidebar.archive_title"))
                .description(crate::tr!("sidebar.archive_description", title = title))
                .button_props(
                    DialogButtons::default()
                        .ok_text(crate::tr!("sidebar.archive_action"))
                        .cancel_text(crate::tr!("settings.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    let _ = sidebar.update(cx, |this, cx| {
                        this.perform_lifecycle(
                            vec![Command::ArchiveSession {
                                session_id: session_id.clone(),
                            }],
                            Some((
                                UndoKind::Archive,
                                vec![Command::UnarchiveSession {
                                    session_id: session_id.clone(),
                                }],
                            )),
                            window,
                            cx,
                        )
                    });
                    true
                })
        });
    }

    /// Permanently delete a thread and every thread under it: an optional
    /// confirm, then (when it orphans a worktree) a second "remove the worktree
    /// too?" prompt.
    fn delete_thread(
        &mut self,
        session_id: &str,
        title: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let session_id = session_id.to_string();
        let skip = store.read(cx).settings().skip_delete_confirmation;
        if skip {
            proceed_delete(store, session_id, window, cx);
            return;
        }
        confirm_delete(
            store,
            session_id,
            title.to_string(),
            crate::tr!("sidebar.delete_action").into(),
            window,
            cx,
        );
    }

    fn render_app_row(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        window_drag_area(
            "sidebar-app-row-drag",
            h_flex()
                .h(px(52.))
                .flex_none()
                .items_center()
                .gap_2()
                .pl(px(TRAFFIC_LIGHT_INSET))
                .pr_2(),
            window,
            cx,
        )
        .child(
            div()
                .text_sm()
                .font_bold()
                .text_color(cx.theme().sidebar_foreground)
                .child(crate::tr!("app.name")),
        )
        // The collapse toggle lives in the chat header (`crate::chat`), not
        // here: collapsing takes the sidebar to zero width, so a control that
        // rode the sidebar would take itself off screen.
        .child(div().flex_1())
    }

    fn render_search_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div().flex_none().px_2().pb_1().child(
            crate::material::accessible_clickable(
                h_flex(),
                "sidebar-search",
                Role::Button,
                crate::tr!("sidebar.search"),
                cx,
            )
            .h(px(32.))
            .items_center()
            .gap_2()
            .px_2()
            .rounded(cx.theme().tokens.radius.md)
            .cursor_pointer()
            .hover(|s| s.bg(cx.theme().sidebar_accent))
            .on_click(cx.listener(|this, _, _, cx| {
                this.window_state
                    .update(cx, |state, cx| state.open_palette(cx));
            }))
            .child(
                Icon::new(IconName::Search)
                    .small()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .flex_1()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(crate::tr!("sidebar.search")),
            )
            .child(
                div()
                    .px_1()
                    .py(px(1.))
                    .rounded_sm()
                    .border_1()
                    .border_color(cx.theme().border)
                    .text_color(cx.theme().muted_foreground)
                    .text_size(px(10.))
                    .child(format_secondary_shortcut("k")),
            ),
        )
    }

    /// The feature area: the window's persistent entries, directly under the
    /// search field at both widths. Today it holds one — Hosts — and the next
    /// one is a row in this list, not another one-off control.
    fn render_feature_rows(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let compact = self.window_state.read(cx).compact;
        let active = self.window_state.read(cx).route() == Route::Hosts;
        let store = self.store.read(cx);
        // A remote link has a machine to name and a state to report: the dot
        // says whether the link is up, hovering says how it is carried. The
        // window that is the host itself has neither, so its row is the
        // entry alone.
        let remote = store.remote_host_name().map(|_| {
            let connection_state = store.connection_state();
            (
                SharedString::from(store.machine_label()),
                cx.theme().connection_color(&connection_state),
                SharedString::from(crate::remote::connection_label(&connection_state)),
            )
        });
        div()
            .flex_none()
            .px(px(if compact { COMPACT_PAGE_PADDING } else { 8. }))
            .pb_1()
            .child(
                crate::material::accessible_clickable(
                    h_flex(),
                    "sidebar-hosts",
                    Role::Button,
                    crate::tr!("hosts.title"),
                    cx,
                )
                .h(px(if compact { 44. } else { 32. }))
                .debug_selector(|| "sidebar-feature-hosts".into())
                .items_center()
                .gap_2()
                .px_2()
                .rounded(cx.theme().tokens.radius.md)
                .cursor_pointer()
                .when(active, |row| row.bg(cx.theme().list_active))
                .when(!active, |row| {
                    row.hover(|s| s.bg(cx.theme().sidebar_accent))
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    this.window_state
                        .update(cx, |state, cx| state.go(Destination::Hosts, cx));
                }))
                // The row truncates a long machine or space name; the tooltip
                // carries it whole, above the link's state.
                .when_some(remote.clone(), |row, (host, _, connection)| {
                    row.tooltip(move |window, cx| {
                        let (host, connection) = (host.clone(), connection.clone());
                        Tooltip::element(move |_, cx| {
                            v_flex().child(host.clone()).child(
                                div()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(connection.clone()),
                            )
                        })
                        .build(window, cx)
                    })
                })
                .child(
                    Icon::new(IconName::Network)
                        .small()
                        .flex_none()
                        .text_color(cx.theme().muted_foreground),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(px(if compact { 15. } else { 13. }))
                        .text_color(cx.theme().sidebar_foreground)
                        .child(crate::tr!("hosts.title")),
                )
                .when_some(remote, |row, (host, connection_color, _)| {
                    row.child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(if compact { 13. } else { 12. }))
                            .text_align(gpui::TextAlign::Right)
                            .text_color(cx.theme().muted_foreground)
                            .child(host),
                    )
                    .child(
                        div()
                            .flex_none()
                            .size(px(8.))
                            .rounded_full()
                            .bg(connection_color),
                    )
                }),
            )
    }

    fn render_layout_toggle(&self, layout: SidebarLayout, cx: &mut Context<Self>) -> Button {
        let tooltip = match layout {
            SidebarLayout::Flat => crate::tr!("sidebar.layout_grouped"),
            SidebarLayout::Grouped => crate::tr!("sidebar.layout_flat"),
        };
        let compact = self.compact(cx);
        Button::new("toggle-sidebar-layout")
            .ghost()
            .xsmall()
            .compact()
            .map(|button| {
                if compact {
                    button
                        .with_size(px(44.))
                        .w(px(44.))
                        .h(px(44.))
                        .child(Icon::new(IconName::LayoutDashboard).size(px(18.)))
                } else {
                    button.icon(IconName::LayoutDashboard)
                }
            })
            .aria_label(tooltip.clone())
            .tooltip(tooltip)
            .on_click(cx.listener(move |this, _, _, cx| {
                let next = match layout {
                    SidebarLayout::Flat => SidebarLayout::Grouped,
                    SidebarLayout::Grouped => SidebarLayout::Flat,
                };
                this.store.update(cx, |store, _cx| {
                    store.set_sidebar_layout(next);
                });
            }))
    }

    fn render_projects_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let sort_label = crate::settings::project_sort_label(self.store.read(cx).project_sort());
        h_flex()
            .flex_none()
            .h(px(28.))
            .items_center()
            .justify_between()
            .px_3()
            .child(
                div()
                    .text_size(px(11.))
                    .font_medium()
                    .text_color(cx.theme().muted_foreground)
                    // Small-caps section label; `to_uppercase` is a no-op on CJK.
                    .child(crate::tr!("sidebar.projects").to_uppercase()),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .child(
                        Button::new("sort-projects")
                            .ghost()
                            .xsmall()
                            .compact()
                            .icon(IconName::SortAscending)
                            .tooltip(crate::tr!("sidebar.sort", mode = sort_label))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.store.update(cx, |store, cx| {
                                    store.cycle_project_sort(cx);
                                });
                            })),
                    )
                    .child(self.render_layout_toggle(SidebarLayout::Grouped, cx))
                    .when(self.store.read(cx).scope().is_full(), |row| {
                        row.child(
                            Button::new("quick-chat-grouped")
                                .ghost()
                                .xsmall()
                                .compact()
                                .icon(IconName::Plus)
                                .tooltip(crate::tr!("sidebar.quick_chat"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.on_quick_chat(&QuickChat, window, cx)
                                })),
                        )
                        .child(
                            Button::new("add-project")
                                .ghost()
                                .xsmall()
                                .compact()
                                .icon(
                                    Icon::empty()
                                        .path("icons/folder-plus.svg")
                                        .text_color(cx.theme().muted_foreground),
                                )
                                .tooltip(crate::tr!("sidebar.add_project"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.add_project(window, cx);
                                })),
                        )
                    }),
            )
    }

    fn render_flat_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let projects = self.store.read(cx).projects();
        let active_filter = self.project_filter.clone();
        let filter_icon_color = if active_filter.is_some() {
            cx.theme().primary
        } else {
            cx.theme().muted_foreground
        };
        let filter_projects = projects.clone();
        let filter_for_menu = active_filter.clone();
        let filter_button = Button::new("filter-sidebar-project")
            .ghost()
            .xsmall()
            .compact()
            .icon(Icon::new(IconName::Folder).text_color(filter_icon_color))
            .tooltip(crate::tr!("sidebar.filter_project"))
            .dropdown_menu(move |menu, _window, _cx| {
                let mut menu = menu.menu_with_check(
                    crate::tr!("sidebar.all_projects").into_owned(),
                    filter_for_menu.is_none(),
                    Box::new(FilterProject(String::new())),
                );
                for project in &filter_projects {
                    menu = menu.menu_with_check(
                        project.name.clone(),
                        filter_for_menu.as_deref() == Some(project.id.as_str()),
                        Box::new(FilterProject(project.id.clone())),
                    );
                }
                menu
            });

        let draft_project = active_filter
            .as_deref()
            .and_then(|id| projects.iter().find(|project| project.id == id))
            .cloned()
            .or_else(|| (projects.len() == 1).then(|| projects[0].clone()));
        let new_thread = if let Some(project) = draft_project {
            let project_id = project.id;
            let cwd = project.root;
            Button::new("new-flat-thread")
                .ghost()
                .xsmall()
                .compact()
                .icon(IconName::Plus)
                .tooltip(crate::tr!("sidebar.create_thread"))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.store.update(cx, |store, cx| {
                        store.start_draft(project_id.clone(), cwd.clone(), cx);
                    });
                    this.window_state
                        .update(cx, |state, cx| state.open_thread(cx));
                }))
                .into_any_element()
        } else {
            let draft_projects = projects.clone();
            let full_scope = self.store.read(cx).scope().is_full();
            Button::new("new-flat-thread")
                .ghost()
                .xsmall()
                .compact()
                .icon(IconName::Plus)
                .tooltip(crate::tr!("sidebar.create_thread"))
                .dropdown_menu(move |menu, _window, _cx| {
                    let mut menu = menu.when(full_scope, |menu| {
                        menu.menu(crate::tr!("sidebar.quick_chat"), Box::new(QuickChat))
                    });
                    for project in &draft_projects {
                        menu = menu.menu(
                            project.name.clone(),
                            Box::new(StartDraftForProject(project.id.clone())),
                        );
                    }
                    menu
                })
                .into_any_element()
        };

        h_flex()
            .flex_none()
            .h(px(28.))
            .items_center()
            .justify_between()
            .px_3()
            .child(
                div()
                    .text_size(px(11.))
                    .font_medium()
                    .text_color(cx.theme().muted_foreground)
                    .child(crate::tr!("sidebar.threads").to_uppercase()),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .child(filter_button)
                    .child(self.render_layout_toggle(SidebarLayout::Flat, cx))
                    .child(new_thread)
                    .when(self.store.read(cx).scope().is_full(), |row| {
                        row.child(
                            Button::new("add-project")
                                .ghost()
                                .xsmall()
                                .compact()
                                .icon(
                                    Icon::empty()
                                        .path("icons/folder-plus.svg")
                                        .text_color(cx.theme().muted_foreground),
                                )
                                .tooltip(crate::tr!("sidebar.add_project"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.add_project(window, cx);
                                })),
                        )
                    }),
            )
    }

    fn render_group_header(
        &self,
        group: &ProjectGroup,
        flags: &HashMap<String, ThreadFlags>,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let project_id = group.project.id.clone();
        let has_unread = group.sessions.iter().any(|meta| {
            meta.parent_session_id.is_none()
                && flags.get(&meta.id).is_some_and(|flags| flags.unread)
        });
        let group_key = format!("group-{project_id}");

        let share = self.share_target(Some(&project_id), cx);
        let header_toggle_id = project_id.clone();
        let plus_cwd = group.project.root.clone();
        let plus_project_id = project_id.clone();
        let menu_project_id = project_id.clone();
        let can_archive = group
            .sessions
            .iter()
            .any(|meta| !flags.get(&meta.id).is_some_and(|flags| flags.working));

        let header_label =
            crate::tr!("sidebar.project", name = group.project.name.clone()).into_owned();
        let header = crate::material::accessible_clickable(
            h_flex(),
            gpui::SharedString::from(format!("project-header-{project_id}")),
            Role::Button,
            header_label,
            cx,
        )
        .aria_expanded(!collapsed)
        .debug_selector({
            let project_id = project_id.clone();
            move || format!("project-header-{project_id}")
        })
        .group(group_key.clone())
        .h(px(30.))
        .items_center()
        .gap_1()
        .px_2()
        .rounded(cx.theme().tokens.radius.md)
        .cursor_pointer()
        .hover(|s| s.bg(cx.theme().sidebar_accent))
        .on_click(cx.listener(move |this, _, _, cx| {
            this.toggle_project(&header_toggle_id, cx);
        }))
        .child(
            Icon::new(if collapsed {
                IconName::ChevronRight
            } else {
                IconName::ChevronDown
            })
            .size_4()
            .text_color(cx.theme().muted_foreground),
        )
        .child(crate::project_icon::artwork(&group.project, 16.))
        .child(
            truncated_sidebar_label()
                .text_sm()
                .font_medium()
                .text_color(cx.theme().sidebar_foreground)
                .child(group.project.name.clone()),
        )
        .children(self.shared_badge(&project_id, cx))
        // Unread dot when any child thread is unread (hidden on hover so
        // the "+" can take the slot).
        .when(has_unread, |row| {
            row.child(
                div()
                    .flex_none()
                    .group_hover(group_key.clone(), |s| s.invisible())
                    .child(div().size(px(6.)).rounded_full().bg(cx.theme().primary)),
            )
        })
        .child(
            crate::material::accessible_clickable(
                h_flex(),
                gpui::SharedString::from(format!("new-thread-{project_id}")),
                Role::Button,
                crate::tr!("sidebar.create_thread"),
                cx,
            )
            .size_5()
            .items_center()
            .justify_center()
            .rounded(cx.theme().tokens.radius.sm)
            .cursor_pointer()
            // Opacity (rather than `visibility: hidden`) keeps this in Root's
            // tab-stop registry so keyboard focus can reveal it. `focus`, not
            // `in_focus`: a click-focused ancestor row would otherwise pin the
            // control visible.
            .opacity(0.)
            .group_hover(group_key.clone(), |s| s.opacity(1.))
            .focus(|s| s.opacity(1.).bg(cx.theme().sidebar_accent))
            .hover(|s| s.bg(cx.theme().sidebar_accent))
            .tooltip(|window, cx| {
                Tooltip::new(crate::tr!("sidebar.create_thread").into_owned()).build(window, cx)
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                crate::widgets::stop_click_propagation(window, cx);
                let cwd = plus_cwd.clone();
                let project_id = plus_project_id.clone();
                this.store.update(cx, |store, cx| {
                    store.start_draft(project_id, cwd, cx);
                });
                this.window_state
                    .update(cx, |state, cx| state.open_thread(cx));
            }))
            .child(
                Icon::new(IconName::Plus)
                    .xsmall()
                    .text_color(cx.theme().muted_foreground),
            ),
        );
        let scope = self.store.read(cx).scope().clone();
        header
            .context_menu(move |menu, _window, cx| {
                let id = menu_project_id.clone();
                let delete_label = crate::tr!("sidebar.remove_project").into_owned();
                menu.when(scope.is_full(), |menu| {
                    menu.menu(
                        crate::tr!("project_icon.title"),
                        Box::new(ChangeProjectIcon(id.clone())),
                    )
                    .menu(
                        crate::tr!("sidebar.change_project_root"),
                        Box::new(ChangeProjectRoot(id.clone())),
                    )
                })
                .when(scope.is_full(), |menu| {
                    menu.menu(
                        crate::tr!("sidebar.project_thread_rules"),
                        Box::new(ProjectThreadRules(id.clone())),
                    )
                })
                .menu_with_enable(
                    crate::tr!("sidebar.archive_all").into_owned(),
                    Box::new(ProjectArchiveAll(id.clone())),
                    can_archive,
                )
                .when(scope.is_full(), |menu| {
                    menu.menu_element(Box::new(ProjectDelete(id.clone())), move |_window, cx| {
                        div()
                            .flex_1()
                            .text_color(cx.theme().danger)
                            .child(delete_label.clone())
                    })
                })
                .menu(
                    crate::tr!("sidebar.reveal_project").into_owned(),
                    Box::new(ProjectReveal(id)),
                )
                .when_some(share.as_ref(), |menu, share| {
                    spaces::share_items(menu, share, false, cx)
                })
            })
            .touch(false)
            .into_any_element()
    }

    fn render_grouped_row(
        &self,
        index: usize,
        row: &GroupedListRow,
        groups: &[ProjectGroup],
        flags: &HashMap<String, ThreadFlags>,
        active_id: Option<&str>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let project = |group: &usize| Some(groups[*group].project.id.clone());
        let item = v_flex().w_full().px_2().when(
            index > 0 && matches!(row, GroupedListRow::Project { .. }),
            |item| item.pt(px(2.)),
        );
        match row {
            GroupedListRow::Project { group, collapsed } => item
                .child(self.render_group_header(&groups[*group], flags, *collapsed, cx))
                .into_any_element(),
            GroupedListRow::Boundary {
                group,
                section,
                empty,
            } => item
                .child(self.render_drag_boundary(*section, *empty, project(group), cx))
                .into_any_element(),
            GroupedListRow::Thread(meta) => {
                let item = if self
                    .drag
                    .as_ref()
                    .is_some_and(|drag| drag.session_id == meta.id)
                {
                    item.child(Self::render_drag_gap(GROUPED_ROW_HEIGHT))
                } else {
                    item.child(self.render_thread(
                        meta,
                        flags,
                        active_id == Some(meta.id.as_str()),
                        cx,
                    ))
                };
                let zone = DropZone::Row {
                    id: meta.id.clone(),
                    section: tcode_core::thread_sort::thread_section(meta),
                };
                self.drop_zone(item, zone, meta.project_id.clone(), 8., cx)
                    .into_any_element()
            }
            GroupedListRow::More { group, count } => item
                .child(self.render_settled_more(&groups[*group].project.id, *count, cx))
                .into_any_element(),
            GroupedListRow::Empty { .. } => {
                item.child(self.render_active_empty(cx)).into_any_element()
            }
            GroupedListRow::Settled { group, count } => {
                let header = self.render_settled_header(
                    &groups[*group].project.id,
                    *count,
                    project(group).as_deref(),
                    cx,
                );
                self.drop_zone(
                    item.child(header),
                    DropZone::Settled,
                    project(group),
                    8.,
                    cx,
                )
                .into_any_element()
            }
        }
    }

    fn thread_row_state(
        &self,
        meta: &SessionMeta,
        flags: &HashMap<String, ThreadFlags>,
        row_key: String,
        cx: &App,
    ) -> ThreadRowState {
        let session_id = meta.id.clone();
        let own_flags = flags.get(&session_id).copied().unwrap_or_default();
        ThreadRowState {
            renaming: self
                .renaming
                .as_ref()
                .filter(|rename| rename.session_id == session_id)
                .map(|rename| rename.input.clone()),
            session_id,
            row_key,
            waiting_for_approval: own_flags.waiting_for_approval,
            waiting_for_input: own_flags.waiting_for_input,
            waiting: own_flags.waiting,
            failed: own_flags.failed,
            auto_settle_enabled: meta.auto_settle_disabled_at.is_none(),
            is_worktree: meta.worktree.is_some(),
            show_unread: own_flags.unread && !own_flags.working,
            menu_can_fork: meta.provider.caps().supports_fork,
            title_generating: self.store.read(cx).title_generating(&meta.id),
            watching: tcode_core::pull_request::watched(&meta.pull_requests)
                .map(|link| link.key.number)
                .collect(),
        }
    }

    /// The decorative provider mark painted under a thread row's content:
    /// `meta`'s protocol glyph in its provider color, fitted to the row height
    /// minus [`PROVIDER_MARK_INSET`], vertically centred and right-aligned
    /// inside the row's `right_padding`. `None` while Provider marks is off.
    /// It has no handlers, so the row's own hover, click and menu are unaffected.
    fn provider_mark(
        &self,
        meta: &SessionMeta,
        row_height: f32,
        right_padding: f32,
        cx: &App,
    ) -> Option<gpui::Div> {
        let color: gpui::Hsla = gpui::rgb(self.store.read(cx).provider_color(meta)?).into();
        let glyph = match meta.provider {
            // ACP agents share the box glyph the composer rail and ACP panel use.
            agent::ProviderKind::Acp => Icon::empty().path("icons/box.svg"),
            kind => crate::provider_card::provider_glyph(kind),
        };
        let size = px(row_height - PROVIDER_MARK_INSET);
        let id = meta.id.clone();
        Some(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .right(px(right_padding))
                .flex()
                .items_center()
                .child(
                    div()
                        .size(size)
                        .debug_selector(move || format!("sidebar-provider-mark-{id}"))
                        .child(
                            glyph
                                .size(size)
                                .text_color(color.opacity(PROVIDER_MARK_ALPHA)),
                        ),
                ),
        )
    }

    fn thread_clickable_row(
        &self,
        base: gpui::Div,
        row_id: gpui::SharedString,
        meta: &SessionMeta,
        state: &ThreadRowState,
        is_active: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let session_id = state.session_id.clone();
        let row = crate::material::accessible_clickable(
            base,
            row_id,
            Role::Button,
            crate::tr!("sidebar.thread", title = meta.title.clone()).into_owned(),
            cx,
        )
        .aria_selected(is_active)
        .debug_selector({
            let id = meta.id.clone();
            move || format!("sidebar-thread-{id}")
        })
        .group(state.row_key.clone())
        .cursor_pointer()
        .when(is_active, |row| row.bg(cx.theme().list_active))
        .when(!is_active, |row| {
            row.hover(|row| row.bg(cx.theme().sidebar_accent))
        })
        .on_click(cx.listener(move |this, _, _, cx| {
            let session_id = session_id.clone();
            this.compact_model_dirty = true;
            this.store.update(cx, |store, cx| {
                store.select_session(session_id.clone());
                cx.notify();
            });
            this.window_state
                .update(cx, |state, cx| state.open_thread(cx));
            cx.notify();
        }));
        let row = match self
            .dragged_thread(meta, state, cx)
            .filter(|_| !self.compact(cx))
        {
            Some(dragged) => Self::drag_source(row, dragged, cx),
            None => row,
        };
        row.when(state.waiting_for_approval, |row| {
            row.tooltip(|window, cx| {
                Tooltip::new(crate::tr!("sidebar.waiting_approval_tooltip").into_owned())
                    .build(window, cx)
            })
        })
        .when(
            state.waiting_for_input && !state.waiting_for_approval,
            |row| {
                row.tooltip(|window, cx| {
                    Tooltip::new(crate::tr!("sidebar.waiting_input_tooltip").into_owned())
                        .build(window, cx)
                })
            },
        )
    }

    fn thread_title_or_input(
        &self,
        meta: &SessionMeta,
        state: &ThreadRowState,
        emphasize_unread: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let title = if let Some(input) = &state.renaming {
            div()
                .flex_1()
                .min_w_0()
                .on_mouse_down_out(cx.listener(|this, _, _, cx| this.cancel_rename(cx)))
                .child(Input::new(input).small())
                .into_any_element()
        } else {
            truncated_sidebar_label()
                .text_size(px(13.))
                .line_height(px(18.))
                .text_color(if meta.is_settled() {
                    cx.theme().muted_foreground
                } else {
                    cx.theme().sidebar_foreground
                })
                .when(
                    emphasize_unread && state.show_unread && !meta.is_settled(),
                    |title| title.font_semibold(),
                )
                .child(meta.title.clone())
                .into_any_element()
        };
        h_flex()
            .flex_1()
            .min_w_0()
            .gap(px(6.))
            .child(title)
            .when(state.title_generating, |row| {
                row.child(
                    div()
                        .flex_none()
                        .child(Spinner::new().xsmall().color(cx.theme().muted_foreground)),
                )
            })
            .into_any_element()
    }

    /// A pinned row's pin; on row hover it becomes the Unpin button.
    fn pin_glyph(
        &self,
        meta: &SessionMeta,
        row_key: &str,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Div> {
        if meta.pinned_at.is_none() || meta.is_settled() {
            return None;
        }
        let id = meta.id.clone();
        let label = crate::tr!("sidebar.unpin_tooltip");
        Some(
            div()
                .relative()
                .flex_none()
                .size(px(20.))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .group_hover(row_key.to_owned(), |glyph| glyph.invisible())
                        .child(
                            Icon::new(IconName::Pin)
                                .size(px(12.))
                                .text_color(cx.theme().muted_foreground),
                        ),
                )
                .child(
                    Button::new(SharedString::from(format!("unpin-thread-{id}")))
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(IconName::PinOff).text_color(cx.theme().muted_foreground))
                        .aria_label(label.clone())
                        .tooltip(label)
                        .absolute()
                        .top_0()
                        .left_0()
                        // Opacity keeps the button a tab stop, as the settle button does.
                        .opacity(0.)
                        .group_hover(row_key.to_owned(), |button| button.opacity(1.))
                        .focus_visible(|button| button.opacity(1.))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            crate::widgets::stop_click_propagation(window, cx);
                            this.unpin_thread(&id, window, cx);
                        })),
                ),
        )
    }

    /// Status text and colour shared by every thread row shape, and whether
    /// it is the Waiting status.
    fn thread_status_label(
        state: &ThreadRowState,
        working: bool,
        cx: &Context<Self>,
    ) -> Option<(gpui::Hsla, std::borrow::Cow<'static, str>, bool)> {
        if state.waiting_for_approval {
            Some((
                cx.theme().warning,
                crate::tr!("sidebar.waiting_approval"),
                false,
            ))
        } else if state.waiting_for_input {
            Some((
                cx.theme().primary,
                crate::tr!("sidebar.waiting_input"),
                false,
            ))
        } else if state.failed {
            Some((cx.theme().danger, crate::tr!("sidebar.failed"), false))
        } else if working {
            Some((cx.theme().primary, crate::tr!("sidebar.working"), false))
        } else if state.waiting {
            Some((
                cx.theme().muted_foreground,
                crate::tr!("sidebar.waiting"),
                true,
            ))
        } else {
            None
        }
    }

    /// Explain a Waiting status on hover with what the thread waits on.
    fn waiting_tooltip(
        element: gpui::Stateful<gpui::Div>,
        store: &Entity<WorkspaceStore>,
        session_id: &str,
        watching: &[u64],
    ) -> gpui::Stateful<gpui::Div> {
        let store = store.clone();
        let session_id = session_id.to_owned();
        let watching = watching.to_vec();
        element.tooltip(move |window, cx| {
            Tooltip::new(waiting_reason(store.read(cx), &session_id, &watching)).build(window, cx)
        })
    }

    fn thread_status_badge(
        state: &ThreadRowState,
        working: bool,
        store: &Entity<WorkspaceStore>,
        cx: &Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let (color, label, waiting) = Self::thread_status_label(state, working, cx)?;
        Some(
            h_flex()
                .id(SharedString::from(format!(
                    "thread-status-{}",
                    state.session_id
                )))
                .when(waiting, |badge| {
                    Self::waiting_tooltip(badge, store, &state.session_id, &state.watching)
                })
                .flex_none()
                .items_center()
                .gap_1()
                .child(div().size(px(6.)).rounded_full().bg(color))
                .child(
                    div()
                        .whitespace_nowrap()
                        .text_size(px(11.))
                        .line_height(px(18.))
                        .text_color(color)
                        .child(label),
                )
                .when(state.failed && !state.waiting(), |badge| {
                    badge.tooltip(|window, cx| {
                        Tooltip::new(crate::tr!("sidebar.failed_tooltip")).build(window, cx)
                    })
                })
                .into_any_element(),
        )
    }

    fn thread_context_menu(
        &self,
        row: gpui::Stateful<gpui::Div>,
        meta: &SessionMeta,
        state: &ThreadRowState,
        running: bool,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let section = tcode_core::thread_sort::thread_section(meta);
        let settled = section == ThreadSection::Settled;
        let share = self.share_target(meta.project_id.as_deref(), cx);
        let scope = self.store.read(cx).scope().clone();
        let sidebar = cx.entity().downgrade();
        let session_id = state.session_id.clone();
        let can_fork = state.menu_can_fork;
        let is_worktree = state.is_worktree;
        let title_generating = state.title_generating;
        let blocked = state.waiting_for_approval || state.waiting_for_input;
        let auto_settle_enabled = state.auto_settle_enabled;
        row.context_menu(move |menu, _window, cx| {
            let id = session_id.clone();
            // Move up / Move down stay within the section the list shows.
            let moves = sidebar
                .upgrade()
                .filter(|_| !settled && scope.is_full())
                .and_then(|sidebar| sidebar.read(cx).move_bounds(&id, cx));
            menu.menu(
                crate::tr!("sidebar.ctx_rename").into_owned(),
                Box::new(ThreadRename(id.clone())),
            )
            .menu_with_enable(
                if title_generating {
                    crate::tr!("sidebar.ctx_regenerating_title").into_owned()
                } else {
                    crate::tr!("sidebar.ctx_regenerate_title").into_owned()
                },
                Box::new(ThreadRegenerateTitle(id.clone())),
                !title_generating,
            )
            .when(can_fork, |menu| {
                menu.menu(
                    crate::tr!("sidebar.ctx_fork").into_owned(),
                    Box::new(ThreadFork(id.clone())),
                )
            })
            .when(is_worktree, |menu| {
                menu.menu(
                    crate::tr!("sidebar.ctx_merge_worktree").into_owned(),
                    Box::new(ThreadMergeWorktree(id.clone())),
                )
            })
            .menu(
                crate::tr!("pull_requests.link_menu").into_owned(),
                Box::new(ThreadLinkPullRequest(id.clone())),
            )
            .menu(
                crate::tr!("sidebar.ctx_mark_unread").into_owned(),
                Box::new(ThreadMarkUnread(id.clone())),
            )
            .separator()
            .menu(
                if section == ThreadSection::Pinned {
                    crate::tr!("sidebar.ctx_unpin").into_owned()
                } else {
                    crate::tr!("sidebar.ctx_pin").into_owned()
                },
                if section == ThreadSection::Pinned {
                    Box::new(ThreadUnpin(id.clone())) as Box<dyn Action>
                } else {
                    Box::new(ThreadPin(id.clone()))
                },
            )
            .menu_with_enable(
                if settled {
                    crate::tr!("sidebar.ctx_unsettle").into_owned()
                } else {
                    crate::tr!("sidebar.ctx_settle").into_owned()
                },
                if settled {
                    Box::new(ThreadMakeActive(id.clone())) as Box<dyn Action>
                } else {
                    Box::new(ThreadSettle(id.clone()))
                },
                settled || (!running && !blocked),
            )
            .when_some(moves, |menu, (up, down)| {
                menu.menu_with_enable(
                    crate::tr!("sidebar.ctx_move_up").into_owned(),
                    Box::new(ThreadMove(id.clone(), false)),
                    up,
                )
                .menu_with_enable(
                    crate::tr!("sidebar.ctx_move_down").into_owned(),
                    Box::new(ThreadMove(id.clone(), true)),
                    down,
                )
            })
            .when(compact, |menu| {
                menu.menu(
                    crate::tr!("sidebar.ctx_arrange").into_owned(),
                    Box::new(ThreadArrange(id.clone())),
                )
            })
            .separator()
            .label(crate::tr!("sidebar.ctx_auto_settle"))
            .menu_with_check(
                crate::tr!("sidebar.ctx_auto_settle_enabled"),
                auto_settle_enabled,
                Box::new(ThreadAutoSettle(id.clone(), true)),
            )
            .menu_with_check(
                crate::tr!("sidebar.ctx_auto_settle_disabled"),
                !auto_settle_enabled,
                Box::new(ThreadAutoSettle(id.clone(), false)),
            )
            .separator()
            .menu(
                crate::tr!("sidebar.ctx_copy_path").into_owned(),
                Box::new(ThreadCopyPath(id.clone())),
            )
            .menu(
                crate::tr!("sidebar.ctx_copy_id").into_owned(),
                Box::new(ThreadCopyId(id.clone())),
            )
            .separator()
            .menu(
                crate::tr!("sidebar.ctx_export_jsonl").into_owned(),
                Box::new(ThreadExportJsonl(id.clone())),
            )
            .menu(
                crate::tr!("sidebar.ctx_export_markdown").into_owned(),
                Box::new(ThreadExportMarkdown(id.clone())),
            )
            .separator()
            .menu_with_enable(
                crate::tr!("sidebar.archive").into_owned(),
                Box::new(ThreadArchive(id.clone())),
                !running,
            )
            .when(scope.is_full(), |menu| {
                menu.menu_element(Box::new(ThreadDelete(id.clone())), |_, cx| {
                    div()
                        .text_color(cx.theme().danger)
                        .child(crate::tr!("sidebar.ctx_delete"))
                })
            })
            .when_some(share.as_ref(), |menu, share| {
                spaces::share_items(menu, share, true, cx)
            })
        })
        .touch(compact)
        .into_any_element()
    }

    fn render_thread(
        &self,
        meta: &SessionMeta,
        flags: &HashMap<String, ThreadFlags>,
        is_active: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let working = flags.get(&meta.id).is_some_and(|flags| flags.working);
        let state = self.thread_row_state(meta, flags, format!("thread-{}", meta.id), cx);
        let session_id = state.session_id.clone();
        let row_key = state.row_key.clone();
        let is_worktree = state.is_worktree;
        let show_unread = state.show_unread;

        let row = self
            .thread_clickable_row(
                h_flex(),
                gpui::SharedString::from(format!("thread-row-{session_id}")),
                meta,
                &state,
                is_active,
                cx,
            )
            .h(px(GROUPED_ROW_HEIGHT))
            .items_center()
            .gap_2()
            .pl(px(30.))
            .pr(px(THREAD_ROW_PADDING_X))
            .rounded(cx.theme().tokens.radius.sm)
            // First child so the row's content paints over the mark.
            .when_some(
                self.provider_mark(meta, GROUPED_ROW_HEIGHT, THREAD_ROW_PADDING_X, cx),
                |row, mark| row.relative().child(mark),
            )
            .when_some(
                Self::thread_status_badge(&state, working, &self.store, cx),
                |row, badge| row.child(badge),
            );

        // Row body: rename input, or the (unread dot + worktree glyph + title).
        let row = if state.renaming.is_some() {
            row.child(self.thread_title_or_input(meta, &state, false, cx))
        } else {
            row.when(show_unread, |row| {
                row.child(
                    div()
                        .flex_none()
                        .size(px(6.))
                        .rounded_full()
                        .bg(cx.theme().primary),
                )
            })
            .when(is_worktree, |row| {
                row.child(
                    Icon::empty()
                        .path("icons/git-branch.svg")
                        .xsmall()
                        .text_color(cx.theme().muted_foreground),
                )
            })
            .child(self.thread_title_or_input(meta, &state, false, cx))
            .children(self.pin_glyph(meta, &row_key, cx))
            .when_some(
                (state.renaming.is_none())
                    .then(|| {
                        crate::pull_requests::sidebar_badge(
                            &meta.pull_requests,
                            &meta.id,
                            self.store.clone(),
                            cx,
                        )
                    })
                    .flatten(),
                |row, badge| row.child(badge),
            )
            .when(!working, |row| {
                row.child(self.render_flat_thread_right_slot(meta, &row_key, false, true, cx))
            })
        };

        self.thread_context_menu(row, meta, &state, working, false, cx)
    }

    fn render_flat_thread_right_slot(
        &self,
        meta: &SessionMeta,
        row_key: &str,
        _waiting: bool,
        action_on_hover: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let session_id = meta.id.clone();
        let action_id = session_id.clone();
        let settled = meta.is_settled();
        let timestamp = if settled {
            tcode_core::thread_sort::settled_timestamp(meta)
        } else {
            meta.updated_at
        };
        let ago = humanize_ago(now_secs().saturating_sub(timestamp));
        let action_on_hover = settled
            || (action_on_hover
                && !self.store.read(cx).pending_approval_for(&meta.id)
                && !self.store.read(cx).pending_user_input_for(&meta.id));
        let label = if settled {
            crate::tr!("sidebar.unsettle_tooltip")
        } else {
            crate::tr!("sidebar.settle_tooltip")
        };
        let row_key = row_key.to_string();
        div()
            .relative()
            .flex_none()
            .h(px(20.))
            .min_w(px(20.))
            .child(
                h_flex()
                    .h_full()
                    .items_center()
                    .whitespace_nowrap()
                    .text_size(px(11.))
                    .text_color(cx.theme().muted_foreground)
                    .when(action_on_hover, |time| {
                        time.group_hover(row_key.clone(), |time| time.invisible())
                    })
                    .child(ago),
            )
            .when(action_on_hover, |slot| {
                slot.child(
                    Button::new(SharedString::from(format!(
                        "settle-flat-thread-{session_id}"
                    )))
                    .ghost()
                    .xsmall()
                    .icon(
                        Icon::new(if settled {
                            IconName::Undo2
                        } else {
                            IconName::CircleCheck
                        })
                        .text_color(cx.theme().muted_foreground),
                    )
                    .aria_label(label.clone())
                    .tooltip(label)
                    .absolute()
                    .right_0()
                    .top_0()
                    // Opacity, not visibility, keeps the button a tab stop so
                    // keyboard focus can reveal it; a click's focus must not
                    // keep it over the time once the row moves.
                    .opacity(0.)
                    .group_hover(row_key, |button| button.opacity(1.))
                    .focus_visible(|button| button.opacity(1.))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        crate::widgets::stop_click_propagation(window, cx);
                        if settled {
                            this.on_make_active(&ThreadMakeActive(action_id.clone()), window, cx);
                        } else {
                            this.on_settle(&ThreadSettle(action_id.clone()), window, cx);
                        }
                    })),
                )
            })
    }

    fn render_flat_thread(
        &self,
        meta: &SessionMeta,
        flags: &HashMap<String, ThreadFlags>,
        project_name: Option<String>,
        is_active: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let working = flags.get(&meta.id).is_some_and(|flags| flags.working);
        let state = self.thread_row_state(meta, flags, format!("flat-thread-{}", meta.id), cx);
        let session_id = state.session_id.clone();
        let row_key = state.row_key.clone();
        let waiting = state.waiting();
        let show_unread = state.show_unread;
        let renaming = state.renaming.is_some();
        let status = Self::thread_status_label(&state, working, cx);

        let row = self
            .thread_clickable_row(
                v_flex(),
                gpui::SharedString::from(format!("flat-thread-row-{session_id}")),
                meta,
                &state,
                is_active,
                cx,
            )
            .debug_selector({
                let id = session_id.clone();
                move || format!("sidebar-thread-{id}")
            })
            .h(px(FLAT_ROW_INNER_HEIGHT))
            .justify_center()
            .gap(px(2.))
            .px(px(THREAD_ROW_PADDING_X))
            .rounded(cx.theme().tokens.radius.sm)
            // First child so the row's content paints over the mark.
            .when_some(
                self.provider_mark(meta, FLAT_ROW_INNER_HEIGHT, THREAD_ROW_PADDING_X, cx),
                |row, mark| row.relative().child(mark),
            );

        let row = {
            let title_or_input = self.thread_title_or_input(meta, &state, true, cx);
            // Unread wins over the status colour; renaming hides the dot so the
            // input keeps the row's leading width (same as render_thread).
            let dot = (!renaming)
                .then(|| {
                    show_unread
                        .then(|| cx.theme().primary)
                        .or(status.as_ref().map(|(color, _, _)| *color))
                })
                .flatten();
            let line_one = h_flex()
                .w_full()
                .min_w_0()
                .items_center()
                .gap_2()
                .when_some(dot, |line, color| {
                    line.child(div().flex_none().size(px(6.)).rounded_full().bg(color))
                })
                .child(title_or_input)
                .when(!renaming, |line| {
                    line.children(self.pin_glyph(meta, &row_key, cx)).child(
                        self.render_flat_thread_right_slot(meta, &row_key, waiting, !working, cx),
                    )
                });

            let has_project = project_name.is_some();
            let watching: Vec<u64> = tcode_core::pull_request::watched(&meta.pull_requests)
                .map(|link| link.key.number)
                .collect();
            let line_two = h_flex()
                .w_full()
                .min_w_0()
                .items_center()
                .gap_1()
                .text_size(px(11.))
                .text_color(cx.theme().muted_foreground)
                .when_some(status, |line, (color, label, waiting)| {
                    line.child(
                        div()
                            .id(SharedString::from(format!("thread-status-{session_id}")))
                            .flex_none()
                            .text_color(color)
                            .when(waiting, |status| {
                                Self::waiting_tooltip(status, &self.store, &session_id, &watching)
                            })
                            .child(label),
                    )
                })
                .when((waiting || working) && has_project, |line| {
                    line.child(div().flex_none().child("·"))
                })
                .when_some(project_name, |line, project_name| {
                    line.child(
                        self.store
                            .read(cx)
                            .project(meta.project_id.as_deref().unwrap_or_default())
                            .map(|project| {
                                crate::project_icon::artwork(project, 12.).into_any_element()
                            })
                            .unwrap_or_else(|| {
                                Icon::new(IconName::Folder).size_3().into_any_element()
                            }),
                    )
                    .child(truncated_sidebar_label().child(project_name))
                })
                // Without a project label there is no flex-1 element on the
                // line, so a spacer keeps the worktree glyph bottom-right.
                .when(!has_project, |line| line.child(div().flex_1()))
                .when(meta.worktree.is_some(), |line| {
                    line.child(
                        Icon::empty()
                            .path("icons/git-branch.svg")
                            .xsmall()
                            .text_color(cx.theme().muted_foreground),
                    )
                });
            let line_two = line_two.when_some(
                (!renaming)
                    .then(|| {
                        crate::pull_requests::sidebar_badge(
                            &meta.pull_requests,
                            &meta.id,
                            self.store.clone(),
                            cx,
                        )
                    })
                    .flatten(),
                |line, badge| line.child(badge),
            );
            row.child(line_one).child(line_two)
        };

        self.thread_context_menu(row, meta, &state, working, false, cx)
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div().flex_none().child(
            crate::material::accessible_clickable(
                h_flex(),
                "sidebar-settings",
                Role::Button,
                crate::tr!("settings.title"),
                cx,
            )
            .h(px(40.))
            .items_center()
            .gap_2()
            .px_3()
            .cursor_pointer()
            .hover(|s| s.bg(cx.theme().sidebar_accent))
            .on_click(cx.listener(|this, _, _, cx| {
                this.window_state
                    .update(cx, |state, cx| state.open_settings(cx));
            }))
            .child(
                Icon::new(IconName::Settings)
                    .size_4()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(cx.theme().sidebar_foreground)
                    .child(crate::tr!("settings.title")),
            ),
        )
    }
}

/// Ask before deleting `session_id`, stating how many threads go with it, then
/// continue with [`proceed_delete`]. The count needs the archived threads, so
/// the dialog waits for them when they are not held.
pub(crate) fn confirm_delete(
    store: Entity<WorkspaceStore>,
    session_id: String,
    title: String,
    ok_text: SharedString,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    if let Some(count) = store.read(cx).held_deletion_count(&session_id) {
        open_delete_confirmation(store, session_id, title, ok_text, count, window, cx);
        return;
    }
    let count = store.update(cx, |store, cx| store.fetch_deletion_count(&session_id, cx));
    window
        .spawn(cx, async move |cx| {
            let count = count.await;
            let _ = cx.update(|window, cx| {
                open_delete_confirmation(store, session_id, title, ok_text, count, window, cx)
            });
        })
        .detach();
}

fn open_delete_confirmation(
    store: Entity<WorkspaceStore>,
    session_id: String,
    title: String,
    ok_text: SharedString,
    count: usize,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    window.open_alert_dialog(cx, move |alert, _, cx| {
        let alert = alert.bg(cx.theme().popover);
        let store = store.clone();
        let session_id = session_id.clone();
        let description = if count > 1 {
            crate::tr!("sidebar.delete_tree_description", count = count)
        } else {
            crate::tr!("sidebar.delete_description")
        };
        alert
            .title(crate::tr!("sidebar.delete_title", title = title.clone()))
            .description(description)
            .button_props(
                DialogButtons::default()
                    .ok_variant(ButtonVariant::Danger)
                    .ok_text(ok_text.clone())
                    .cancel_text(crate::tr!("settings.cancel"))
                    .show_cancel(true),
            )
            .on_ok(move |_, window, cx| {
                let store = store.clone();
                let session_id = session_id.clone();
                // The alert closes after this callback; open the next prompt afterwards.
                window.defer(cx, move |window, cx| {
                    proceed_delete(store, session_id, window, cx);
                });
                true
            })
    });
}

/// Delete `session_id`, first asking whether to also remove an orphaned worktree.
pub(crate) fn proceed_delete(
    store: Entity<WorkspaceStore>,
    session_id: String,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let orphan = store.read(cx).worktree_orphaned_by_delete(&session_id);
    let Some(worktree) = orphan else {
        store.update(cx, |store, _cx| {
            store.delete_session(session_id, false);
        });
        return;
    };
    let path = worktree.root_project_path.display().to_string();
    window.open_alert_dialog(cx, move |alert, _, cx| {
        let alert = alert.bg(cx.theme().popover);
        let store = store.clone();
        let session_id = session_id.clone();
        let remove = session_id.clone();
        let keep = session_id.clone();
        let store_remove = store.clone();
        alert
            .title(crate::tr!("sidebar.worktree_cleanup_title"))
            .description(crate::tr!(
                "sidebar.worktree_cleanup_description",
                path = path.clone()
            ))
            .button_props(
                DialogButtons::default()
                    .ok_variant(ButtonVariant::Danger)
                    .ok_text(crate::tr!("sidebar.worktree_cleanup_remove"))
                    .cancel_text(crate::tr!("sidebar.worktree_cleanup_keep"))
                    .show_cancel(true),
            )
            .on_ok(move |_, _, cx| {
                store_remove.update(cx, |store, _cx| {
                    store.delete_session(remove.clone(), true);
                });
                true
            })
            .on_cancel(move |_, _, cx| {
                store.update(cx, |store, _cx| {
                    store.delete_session(keep.clone(), false);
                });
                true
            })
    });
}

const COMPACT_PAGE_PADDING: f32 = 16.;
const COMPACT_SEARCH_HEIGHT: f32 = 40.;

fn thread_list_scrollbar(
    id: &'static str,
    state: &ListState,
    estimated_row_height: f32,
) -> impl IntoElement {
    div()
        .absolute()
        .inset_0()
        .child(crate::scroll::list_height_hint(
            state,
            px(estimated_row_height),
        ))
        .child(Scrollbar::vertical(state).id(id))
}

/// Replace every row of `state`. A list at its top stays there, so a project
/// that moves up by recent activity is shown. Otherwise the row at the scroll
/// top keeps its place while it keeps its key, and a vanished row leaves the
/// list at the same index instead of jumping to the top.
fn replace_list_rows<K: PartialEq>(
    state: &ListState,
    previous_key: impl FnOnce(usize) -> Option<K>,
    mut keys: impl ExactSizeIterator<Item = K>,
) {
    let mut anchor = state.logical_scroll_top();
    let count = keys.len();
    let at_top = anchor.item_ix == 0 && anchor.offset_in_item <= px(0.);
    let index =
        previous_key(anchor.item_ix).and_then(|previous| keys.position(|key| key == previous));
    state.splice(0..state.item_count(), count);
    if at_top || count == 0 {
        return;
    }
    match index {
        Some(index) => anchor.item_ix = index,
        None => anchor.item_ix = anchor.item_ix.min(count - 1),
    }
    state.scroll_to(anchor);
}

impl SessionsSidebar {
    /// Store changes and local disclosures invalidate the model. Scroll and
    /// navigation-animation frames only clone the shared snapshot; ListState
    /// measures the visible rows, including project captions of different height.
    fn compact_model(&mut self, cx: &mut Context<Self>) -> Rc<CompactListModel> {
        let now = now_secs();
        let locale = rust_i18n::locale();
        if self.compact_model_dirty
            || self
                .compact_model
                .as_ref()
                .is_none_or(|model| model.locale.as_str() != &*locale)
        {
            let (groups, collapsed_projects, sessions, flags, layout) = {
                let store = self.store.read(cx);
                let sessions = store.flat_sessions();
                let flags = session_flags(&sessions, store);
                let groups = store.grouped_sessions();
                let collapsed = groups
                    .iter()
                    .filter(|group| store.is_project_collapsed(&group.project.id))
                    .map(|group| group.project.id.clone())
                    .collect::<HashSet<_>>();
                (groups, collapsed, sessions, flags, store.sidebar_layout())
            };

            let mut rows = Vec::new();
            let thread_rows = |visible: Vec<&SessionMeta>, recent: bool| {
                let mut rows = Vec::new();
                let last = visible.len().saturating_sub(1);
                for (index, meta) in visible.into_iter().enumerate() {
                    let state = self.thread_row_state(
                        meta,
                        &flags,
                        format!("compact-thread-{}", meta.id),
                        cx,
                    );
                    let project_name = recent
                        .then(|| {
                            groups.iter().find(|group| {
                                meta.project_id.as_deref() == Some(group.project.id.as_str())
                            })
                        })
                        .flatten()
                        .map(|group| SharedString::from(group.project.name.clone()));
                    rows.push(CompactListRow::Thread(Rc::new(CompactThreadRow {
                        title: meta.title.clone().into(),
                        row_id: format!("compact-thread-row-{}", meta.id).into(),
                        label: crate::tr!("sidebar.thread", title = meta.title.clone())
                            .into_owned()
                            .into(),
                        relative_time: humanize_ago(now.saturating_sub(if meta.is_settled() {
                            tcode_core::thread_sort::settled_timestamp(meta)
                        } else {
                            meta.updated_at
                        }))
                        .into(),
                        working: flags.get(&meta.id).is_some_and(|flags| flags.working),
                        state,
                        meta: meta.clone(),
                        project_name,
                        separator: index != last,
                    })));
                }
                rows
            };
            let grouped_rows = |project: Option<&str>, recent: bool, key: &str| {
                let ThreadSections {
                    pinned,
                    active,
                    settled,
                } = project_threads(&sessions, project);
                let mut rows = Vec::new();
                // A phone has no hover to reveal section labels, so they stay.
                if !pinned.is_empty() {
                    rows.push(CompactListRow::Caption {
                        key: format!("{key}-pinned"),
                        label: crate::tr!("sidebar.pinned_caption").into_owned().into(),
                    });
                    rows.extend(thread_rows(pinned, recent));
                    if !active.is_empty() {
                        rows.push(CompactListRow::Caption {
                            key: format!("{key}-active"),
                            label: crate::tr!("sidebar.active_caption").into_owned().into(),
                        });
                    }
                }
                rows.extend(thread_rows(active, recent));
                if rows.is_empty() && !settled.is_empty() {
                    rows.push(CompactListRow::Empty {
                        key: format!("{key}-empty"),
                    });
                }
                if !settled.is_empty() {
                    rows.push(CompactListRow::Settled {
                        key: key.into(),
                        count: settled.len(),
                    });
                    let count = settled.len();
                    let mut visible = settled;
                    self.limit_settled_rows(key, &mut visible);
                    let hidden = count - visible.len();
                    rows.extend(thread_rows(visible, recent));
                    if let Some(count) = self.settled_more_count(key, hidden) {
                        rows.push(CompactListRow::More {
                            key: format!("{key}-more"),
                            count,
                        });
                    }
                }
                rows
            };
            if layout == SidebarLayout::Flat {
                rows.extend(grouped_rows(None, true, "recent"));
            } else {
                for group in &groups {
                    let count = sessions
                        .iter()
                        .filter(|meta| meta.project_id.as_deref() == Some(&group.project.id))
                        .count();
                    let collapsed =
                        groups.len() > 1 && collapsed_projects.contains(&group.project.id);
                    let start = rows.len();
                    if !collapsed {
                        rows.extend(grouped_rows(
                            Some(&group.project.id),
                            false,
                            &group.project.id,
                        ));
                    }
                    if groups.len() > 1 {
                        rows.insert(
                            start,
                            CompactListRow::Project(CompactProjectRow {
                                project_id: group.project.id.clone(),
                                row_id: format!("compact-group-{}", group.project.id).into(),
                                name: group.project.name.clone().into(),
                                label: crate::tr!(
                                    "sidebar.project",
                                    name = group.project.name.clone()
                                )
                                .into_owned()
                                .into(),
                                count: count.to_string().into(),
                                collapsed,
                            }),
                        );
                    }
                }
            }
            rows.push(CompactListRow::BottomInset);
            replace_list_rows(
                &self.compact_list_state,
                |index| {
                    self.compact_model
                        .as_ref()
                        .and_then(|model| model.rows.get(index))
                        // The loading/empty model contains only padding. Anchoring to
                        // it would open the first Index snapshot at the list's bottom.
                        .filter(|row| !matches!(row, CompactListRow::BottomInset))
                        .map(CompactListRow::key)
                },
                rows.iter().map(CompactListRow::key),
            );
            self.compact_model = Some(Rc::new(CompactListModel {
                rows,
                has_projects: !groups.is_empty(),
                locale: locale.to_string(),
                minute: now / 60,
            }));
            self.compact_model_dirty = false;
        }
        let model = self
            .compact_model
            .as_mut()
            .expect("compact model initialized");
        if model.minute != now / 60 {
            let model = Rc::make_mut(model);
            model.minute = now / 60;
            for row in &mut model.rows {
                if let CompactListRow::Thread(row) = row {
                    let row = Rc::make_mut(row);
                    row.relative_time =
                        humanize_ago(now.saturating_sub(if row.meta.is_settled() {
                            tcode_core::thread_sort::settled_timestamp(&row.meta)
                        } else {
                            row.meta.updated_at
                        }))
                        .into();
                }
            }
        }
        model.clone()
    }

    /// Compact thread list with shared layout preference. Navigation replaces
    /// persistent row selection; desktop-only controls stay in the desktop list.
    fn pending_thread_rows(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let cached = self.store.read(cx).sidebar_sessions();
        let loading = self.store.read(cx).threads_loading();
        v_flex()
            .w_full()
            .children(
                self.store
                    .read(cx)
                    .pending_sessions()
                    .into_iter()
                    .filter(|(id, _)| loading || !cached.iter().any(|meta| &meta.id == id))
                    .map(|(id, preview)| {
                        v_flex()
                            .id(SharedString::from(format!("pending-thread-{id}")))
                            .debug_selector(|| "pending-thread-row".into())
                            .px_4()
                            .py_2()
                            .gap_1()
                            .cursor_pointer()
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .child(crate::tr!("chat.waiting_connection")),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .truncate()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(preview),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.store.update(cx, |store, cx| {
                                    store.select_session(id.clone());
                                    cx.notify();
                                });
                                this.window_state
                                    .update(cx, |state, cx| state.open_thread(cx));
                            }))
                    }),
            )
            .into_any_element()
    }

    fn render_compact(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        #[cfg(test)]
        self.compact_rows_rendered.set(0);
        let model = self.compact_model(cx);
        let layout = self.store.read(cx).sidebar_layout();
        let body = if self.store.read(cx).threads_loading() {
            if self.store.read(cx).pending_sessions().is_empty() {
                crate::material::loading_skeleton(cx)
            } else {
                self.pending_thread_rows(cx)
            }
        } else if !model.has_projects {
            div()
                .id("threads-empty")
                .debug_selector(|| "threads-empty".into())
                .size_full()
                .child(crate::material::empty_state(
                    Icon::new(IconName::Folder),
                    if self.store.read(cx).scope().is_full() {
                        crate::tr!("mobile.projects_empty")
                    } else {
                        crate::tr!("member.empty_title")
                    },
                    if self.store.read(cx).scope().is_full() {
                        crate::tr!("mobile.projects_help")
                    } else {
                        crate::tr!("member.empty_description")
                    },
                    cx,
                ))
                .into_any_element()
        } else {
            if self.compact_reveal_active {
                let active = self.store.read(cx).roster_session_id();
                match model.rows.iter().position(|row| {
                    matches!(row, CompactListRow::Thread(row) if Some(&row.meta.id) == active.as_ref())
                }) {
                    Some(index) => self.compact_list_state.scroll_to(gpui::ListOffset {
                        item_ix: index,
                        offset_in_item: px(0.),
                    }),
                    None => self.compact_reveal_active = false,
                }
            }
            // The list borrows its state while it renders rows, so the lead
            // is measured here. The last layout is the one before the thread
            // page; a list never laid out takes a third of the window.
            let reveal_lead = self.compact_reveal_active.then(|| {
                let viewport = self.compact_list_state.viewport_bounds().size.height;
                let height = if viewport > px(0.) {
                    viewport
                } else {
                    window.viewport_size().height
                };
                height / 3.
            });
            div()
                .id("compact-thread-list")
                .debug_selector(|| "compact-thread-list".into())
                .flex_1()
                .min_h_0()
                .relative()
                .child(crate::scroll::page_viewport(
                    "compact-thread-bounce",
                    crate::wheel_easing::Handle::List(self.compact_list_state.clone()),
                    list(
                        self.compact_list_state.clone(),
                        cx.processor(move |this, index: usize, _, cx| match &model.rows[index] {
                            CompactListRow::Settled { key, count } => {
                                this.render_settled_header(key, *count, None, cx)
                            }
                            CompactListRow::Caption { label, .. } => {
                                crate::material::list_caption(label.clone(), cx).into_any_element()
                            }
                            CompactListRow::More { key, count } => {
                                this.render_settled_more(key.trim_end_matches("-more"), *count, cx)
                            }
                            CompactListRow::Empty { .. } => this.render_active_empty(cx),
                            CompactListRow::Project(row) => {
                                this.render_compact_group_header(row, cx).into_any_element()
                            }
                            CompactListRow::Thread(row) => {
                                let reveal = reveal_lead.filter(|_| {
                                    this.compact_reveal_active
                                        && this.store.read(cx).active_session_id().as_deref()
                                            == Some(row.meta.id.as_str())
                                });
                                this.compact_reveal_active &= reveal.is_none();
                                v_flex()
                                    .w_full()
                                    .when_some(reveal, |item, lead| {
                                        item.relative().child(reveal_row(lead))
                                    })
                                    .child(this.render_compact_thread(row, cx))
                                    .when(row.separator, |list| {
                                        list.child(
                                            div()
                                                .w_full()
                                                .pl(px(crate::material::COMPACT_PAGE_INSET))
                                                .child(
                                                    div()
                                                        .w_full()
                                                        .h(px(1.))
                                                        .bg(cx.theme().border.opacity(0.6)),
                                                ),
                                        )
                                    })
                                    .into_any_element()
                            }
                            CompactListRow::BottomInset => div().h(px(24.)).into_any_element(),
                        }),
                    )
                    .size_full(),
                ))
                .when(!window.is_inspector_picking(cx), |list| {
                    list.child(thread_list_scrollbar(
                        "compact-thread-scrollbar",
                        &self.compact_list_state,
                        58.,
                    ))
                })
                .into_any_element()
        };

        v_flex()
            .size_full()
            .bg(crate::material::content_surface(cx))
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(Self::on_link_pull_request))
            .on_action(cx.listener(Self::on_rename))
            .on_action(cx.listener(Self::on_regenerate_title))
            .on_action(cx.listener(Self::on_fork))
            .on_action(cx.listener(Self::on_merge_worktree))
            .on_action(cx.listener(Self::on_mark_unread))
            .on_action(cx.listener(Self::on_copy_path))
            .on_action(cx.listener(Self::on_copy_id))
            .on_action(cx.listener(Self::on_export_jsonl))
            .on_action(cx.listener(Self::on_export_markdown))
            .on_action(cx.listener(Self::on_settle))
            .on_action(cx.listener(Self::on_auto_settle))
            .on_action(cx.listener(Self::on_make_active))
            .on_action(cx.listener(Self::on_pin))
            .on_action(cx.listener(Self::on_unpin))
            .on_action(cx.listener(Self::on_move))
            .on_action(cx.listener(Self::on_arrange))
            .on_action(cx.listener(Self::on_archive))
            .on_action(cx.listener(Self::on_delete))
            .child(self.render_compact_search(cx))
            .child(self.render_feature_rows(cx))
            .child(
                h_flex()
                    .flex_none()
                    .px(px(COMPACT_PAGE_PADDING))
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(cx.theme().muted_foreground)
                            .child(match layout {
                                SidebarLayout::Flat => crate::tr!("sidebar.recent"),
                                SidebarLayout::Grouped => crate::tr!("sidebar.by_project"),
                            }),
                    )
                    .child(
                        div()
                            .debug_selector(|| "compact-layout-toggle".into())
                            .child(self.render_layout_toggle(layout, cx)),
                    ),
            )
            .when(!self.store.read(cx).threads_loading(), |list| {
                list.child(self.pending_thread_rows(cx))
            })
            .child(body)
            .into_any_element()
    }

    fn render_compact_search(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex_none()
            .px(px(COMPACT_PAGE_PADDING))
            .pt(px(8.))
            .pb(px(4.))
            .child(
                crate::material::accessible_clickable(
                    h_flex(),
                    "compact-search",
                    Role::Button,
                    crate::tr!("mobile.search_threads"),
                    cx,
                )
                .debug_selector(|| "compact-search".into())
                .h(px(COMPACT_SEARCH_HEIGHT))
                .items_center()
                .gap(px(8.))
                .px(px(12.))
                .rounded_full()
                .bg(cx.theme().secondary)
                .cursor_pointer()
                .active(|s| s.opacity(0.8))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.window_state
                        .update(cx, |state, cx| state.open_palette(cx));
                }))
                .child(
                    Icon::new(IconName::Search)
                        .size(px(16.))
                        .text_color(cx.theme().muted_foreground),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(15.))
                        .text_color(cx.theme().muted_foreground)
                        .child(crate::tr!("mobile.search_threads")),
                ),
            )
    }

    fn render_compact_group_header(
        &self,
        row: &CompactProjectRow,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let project_id = row.project_id.clone();
        let collapsed = row.collapsed;
        crate::material::accessible_clickable(
            h_flex(),
            row.row_id.clone(),
            Role::Button,
            row.label.clone(),
            cx,
        )
        .aria_expanded(!collapsed)
        .debug_selector(|| "compact-group-header".into())
        // A project is a section of the thread list, so its header is the
        // shared list caption — with the collapse affordance it also carries.
        .w_full()
        .px(px(COMPACT_PAGE_PADDING))
        .pt(px(16.))
        .pb(px(4.))
        .items_center()
        .gap(px(8.))
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, _, cx| {
            this.toggle_project(&project_id, cx);
        }))
        .text_size(px(13.))
        .text_color(cx.theme().muted_foreground)
        .when_some(
            self.store.read(cx).project(&row.project_id),
            |el, project| el.child(crate::project_icon::artwork(project, 14.)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_medium()
                .child(row.name.clone()),
        )
        .children(self.shared_badge(&row.project_id, cx))
        .child(div().flex_none().child(row.count.clone()))
        .child(
            Icon::new(if collapsed {
                IconName::ChevronRight
            } else {
                IconName::ChevronDown
            })
            .size(px(14.)),
        )
    }

    /// One 56pt thread row. Long press opens the shared thread context menu.
    fn render_compact_thread(
        &self,
        cached: &CompactThreadRow,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        #[cfg(test)]
        self.compact_rows_rendered
            .set(self.compact_rows_rendered.get() + 1);
        let meta = &cached.meta;
        let state = &cached.state;
        let working = cached.working;
        let project_name = cached.project_name.clone();
        let project = if project_name.is_some() {
            self.store
                .read(cx)
                .project(meta.project_id.as_deref().unwrap_or_default())
                .cloned()
        } else {
            None
        };
        let session_id = state.session_id.clone();
        let status = compact_status_line(state, working, cx);
        let click_id = session_id.clone();
        let mark = self.provider_mark(
            meta,
            crate::material::LIST_ROW_MIN_HEIGHT,
            crate::material::COMPACT_PAGE_INSET,
            cx,
        );

        let label = crate::pull_requests::badge_label(&meta.pull_requests, cx).map_or_else(
            || cached.label.to_string(),
            |badge| format!("{}, {badge}", cached.label),
        );
        let row = crate::material::list_row(cached.row_id.clone(), label.into(), cx)
            .debug_selector({
                let id = session_id.clone();
                move || format!("compact-row-{id}")
            })
            // First child so the row's content paints over the mark.
            .when_some(mark, |row, mark| row.relative().child(mark))
            .when(
                self.store.read(cx).roster_session_id().as_deref() == Some(session_id.as_str()),
                |row| row.bg(cx.theme().list_active).aria_selected(true),
            )
            // Rows needing the user carry a 6% semantic wash; everything else sits
            // on the paper with only hover and pressed tints.
            .when(state.waiting_for_approval, |row| {
                row.bg(cx.theme().warning.opacity(0.06))
            })
            .when(
                state.waiting_for_input && !state.waiting_for_approval,
                |row| row.bg(cx.theme().primary.opacity(0.06)),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.store.read(cx).session_loading()
                    && this.store.read(cx).active_session_id().as_deref() == Some(click_id.as_str())
                {
                    return;
                }
                this.store.update(cx, |store, cx| {
                    store.select_session(click_id.clone());
                    cx.notify();
                });
                this.window_state
                    .update(cx, |state, cx| state.open_thread(cx));
            }))
            .child(compact_status_glyph(state, working, project.as_ref(), cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        h_flex()
                            .w_full()
                            .min_w_0()
                            .gap(px(6.))
                            .child(
                                truncated_sidebar_label()
                                    .text_size(px(16.))
                                    .line_height(px(21.))
                                    .text_color(if meta.is_settled() {
                                        cx.theme().muted_foreground
                                    } else {
                                        cx.theme().foreground
                                    })
                                    .when(!meta.is_settled(), |title| title.font_medium())
                                    .when(state.show_unread && !meta.is_settled(), |title| {
                                        title.font_semibold()
                                    })
                                    .debug_selector({
                                        let id = session_id.clone();
                                        move || format!("compact-title-{id}")
                                    })
                                    .child(cached.title.clone()),
                            )
                            .when(meta.pinned_at.is_some() && !meta.is_settled(), |row| {
                                row.child(
                                    Icon::new(IconName::Pin)
                                        .size(px(14.))
                                        .flex_none()
                                        .text_color(cx.theme().muted_foreground),
                                )
                            })
                            .when(state.title_generating, |row| {
                                row.child(div().flex_none().child(
                                    Spinner::new().xsmall().color(cx.theme().muted_foreground),
                                ))
                            }),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .min_w_0()
                            .gap(px(4.))
                            .text_size(px(13.))
                            .line_height(px(18.))
                            .text_color(cx.theme().muted_foreground)
                            .when_some(project_name, |line, name| {
                                line.child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .debug_selector({
                                            let name = name.clone();
                                            move || format!("compact-project-{name}")
                                        })
                                        .child(name),
                                )
                                .child(div().flex_none().child("·"))
                            })
                            .when_some(status, |line, (label, color)| {
                                line.child(div().flex_none().text_color(color).child(label))
                                    .child(div().flex_none().child("·"))
                            })
                            .when_some(
                                crate::pull_requests::badge(&meta.pull_requests, 14., cx),
                                |line, badge| line.child(badge).child("·"),
                            )
                            .child(div().flex_none().child(cached.relative_time.clone())),
                    ),
            );
        self.thread_context_menu(row, meta, state, working, true, cx)
    }
}

/// Scrolls the list so the row this is painted in sits `lead` below the
/// viewport top, or as far down as the list start allows. The list answers
/// a child's autoscroll request by walking into the rows above and measuring
/// them as it goes, so nothing above the row has to be laid out beforehand.
fn reveal_row(lead: gpui::Pixels) -> impl IntoElement {
    canvas(
        move |bounds, window, _| {
            window.request_autoscroll(gpui::Bounds::from_corners(
                gpui::point(bounds.left(), bounds.top() - lead),
                bounds.bottom_right(),
            ));
        },
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// The 20×20 status slot at the head of a compact row. The slot is
/// always taken so titles line up. Idle ungrouped rows show their project artwork.
fn compact_status_glyph(
    state: &ThreadRowState,
    working: bool,
    project: Option<&tcode_core::project::Project>,
    cx: &App,
) -> gpui::AnyElement {
    let slot = div().flex_none().size(px(20.)).flex().items_center();
    if state.waiting_for_approval {
        return slot
            .justify_center()
            .rounded_full()
            .bg(cx.theme().warning)
            .child(
                div()
                    .text_size(px(12.))
                    .font_semibold()
                    .text_color(gpui::white())
                    .child("!"),
            )
            .into_any_element();
    }
    if state.waiting_for_input {
        return slot
            .justify_center()
            .rounded_full()
            .bg(cx.theme().primary)
            .child(
                div()
                    .text_size(px(12.))
                    .font_semibold()
                    .text_color(cx.theme().primary_foreground)
                    .child("?"),
            )
            .into_any_element();
    }
    if state.failed {
        return slot
            .justify_center()
            .child(
                Icon::new(IconName::TriangleAlert)
                    .size(px(16.))
                    .text_color(cx.theme().danger),
            )
            .into_any_element();
    }
    if working || state.waiting {
        let color = if !working && state.waiting {
            cx.theme().muted_foreground
        } else {
            cx.theme().primary
        };
        return slot
            .justify_center()
            .child(Spinner::new().small().color(color))
            .into_any_element();
    }
    if state.show_unread {
        return slot
            .justify_center()
            .child(div().size(px(8.)).rounded_full().bg(cx.theme().primary))
            .into_any_element();
    }
    slot.justify_center()
        .text_color(cx.theme().muted_foreground)
        .when_some(project, |slot, project| {
            slot.child(crate::project_icon::artwork(project, 20.))
        })
        .into_any_element()
}

/// Status label and color, or `None` for an idle thread that shows only its time.
fn compact_status_line(
    state: &ThreadRowState,
    working: bool,
    cx: &App,
) -> Option<(Cow<'static, str>, gpui::Hsla)> {
    if state.waiting_for_approval {
        Some((crate::tr!("mobile.approval"), cx.theme().warning))
    } else if state.waiting_for_input {
        Some((crate::tr!("mobile.answer"), cx.theme().primary))
    } else if state.failed {
        Some((crate::tr!("sidebar.failed"), cx.theme().danger))
    } else if working {
        Some((crate::tr!("mobile.working"), cx.theme().primary))
    } else if state.waiting {
        Some((crate::tr!("sidebar.waiting"), cx.theme().muted_foreground))
    } else if state.show_unread {
        Some((crate::tr!("mobile.unread"), cx.theme().primary))
    } else {
        None
    }
}

impl Render for SessionsSidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let scope = (
            self.store.read(cx).sidebar_layout(),
            self.project_filter.clone(),
        );
        if self.settled_scope.as_ref() != Some(&scope) {
            self.settled_scope = Some(scope);
            self.expanded_settled.clear();
            self.settled_limits.clear();
            self.compact_model_dirty = true;
        }
        self.reveal_selected_settled(cx);
        self.clear_finished_drag(cx);
        let spaces = spaces::for_store(&self.store, cx);
        self.spaces_observer.watch(spaces.as_ref(), cx);
        if self.compact(cx) {
            return self.render_compact(window, cx);
        }

        let (layout, active_id, groups, flat_sessions, projects, flags) = {
            let store = self.store.read(cx);
            let sessions = store.sidebar_sessions();
            let flags = session_flags(&sessions, store);
            (
                store.sidebar_layout(),
                store.roster_session_id(),
                store.grouped_sessions(),
                store.flat_sessions(),
                store.projects(),
                flags,
            )
        };
        let (header, thread_list) = match layout {
            SidebarLayout::Grouped => {
                let thread_list = if groups.is_empty() {
                    div()
                        .id("sidebar-project-list")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scrollbar()
                        .child(
                            div().size_full().child(
                                v_flex().w_full().px_2().pb_2().child(
                                    div()
                                        .px_2()
                                        .py_3()
                                        .text_sm()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(if self.store.read(cx).scope().is_full() {
                                            crate::tr!("sidebar.empty")
                                        } else {
                                            crate::tr!("member.empty_description")
                                        }),
                                ),
                            ),
                        )
                        .into_any_element()
                } else {
                    let rows = self.grouped_rows(&groups, self.store.read(cx));
                    let keys = rows.iter().map(|row| row.key(&groups));
                    if !keys.clone().eq(self
                        .grouped_row_keys
                        .iter()
                        .map(|(kind, id)| (*kind, id.as_str())))
                    {
                        replace_list_rows(
                            &self.grouped_list_state,
                            |index| {
                                self.grouped_row_keys
                                    .get(index)
                                    .map(|(kind, id)| (*kind, id.as_str()))
                            },
                            keys.clone(),
                        );
                        self.grouped_row_keys =
                            keys.map(|(kind, id)| (kind, id.to_owned())).collect();
                    }
                    let thread_list = list(
                        self.grouped_list_state.clone(),
                        cx.processor(move |this, index: usize, _window, cx| {
                            this.render_grouped_row(
                                index,
                                &rows[index],
                                &groups,
                                &flags,
                                active_id.as_deref(),
                                cx,
                            )
                        }),
                    )
                    .flex_1()
                    .min_h_0()
                    .pb_2();
                    v_flex()
                        .id("grouped-thread-list")
                        .flex_1()
                        .min_h_0()
                        .relative()
                        .on_drop(cx.listener(|this, _: &DraggedThread, window, cx| {
                            this.drop_thread(window, cx)
                        }))
                        .on_drag_move(cx.listener(
                            |this, event: &gpui::DragMoveEvent<DraggedThread>, _, cx| {
                                this.drag_auto_scroll(event, cx)
                            },
                        ))
                        .child(crate::scroll::page_viewport(
                            "grouped-thread-bounce",
                            crate::wheel_easing::Handle::List(self.grouped_list_state.clone()),
                            thread_list,
                        ))
                        .when(!window.is_inspector_picking(cx), |list| {
                            list.child(thread_list_scrollbar(
                                "grouped-thread-scrollbar",
                                &self.grouped_list_state,
                                GROUPED_ROW_HEIGHT,
                            ))
                        })
                        .into_any_element()
                };
                (
                    self.render_projects_header(cx).into_any_element(),
                    thread_list,
                )
            }
            SidebarLayout::Flat => {
                let scope = self.project_filter.clone();
                let ThreadRows {
                    pinned,
                    active,
                    settled: settled_visible,
                    settled_count,
                    settled_hidden_count,
                } = self.thread_rows(&flat_sessions, scope.as_deref(), "recent");
                if pinned.is_empty() && active.is_empty() && settled_count == 0 {
                    // An active project filter can empty the list while threads
                    // exist; that state gets its own hint, not the no-projects one.
                    let hint = if !self.store.read(cx).scope().is_full() && flat_sessions.is_empty()
                    {
                        if self.store.read(cx).projects().is_empty() {
                            crate::tr!("member.empty_description")
                        } else {
                            crate::tr!("member.threads_empty")
                        }
                    } else if flat_sessions.is_empty() {
                        crate::tr!("sidebar.empty")
                    } else {
                        crate::tr!("sidebar.filter_empty")
                    };
                    let list_content = v_flex().w_full().px_2().pb_2().child(
                        div()
                            .px_2()
                            .py_3()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(hint),
                    );
                    let thread_list = div()
                        .id("sidebar-project-list")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scrollbar()
                        .child(div().size_full().child(list_content))
                        .into_any_element();
                    (self.render_flat_header(cx).into_any_element(), thread_list)
                } else {
                    // Each row's resting top, so a row moved by a drag or a
                    // section change springs from where it was.
                    let mut top = 0.;
                    let mut visible = Vec::new();
                    for (section, rows) in [
                        (ThreadSection::Pinned, &pinned),
                        (ThreadSection::Active, &active),
                    ] {
                        let empty = rows.is_empty();
                        visible.push(FlatListRow::Boundary(section, empty));
                        top += self.drag_boundary_height(empty, scope.as_deref());
                        for meta in rows.iter() {
                            visible.push(FlatListRow::Thread(Box::new((*meta).clone()), top));
                            top += FLAT_ROW_HEIGHT;
                        }
                    }
                    if settled_count > 0 {
                        if pinned.is_empty() && active.is_empty() {
                            visible.push(FlatListRow::Empty);
                        }
                        visible.push(FlatListRow::Settled);
                        top += SETTLED_HEADER_HEIGHT;
                        for meta in settled_visible {
                            visible.push(FlatListRow::Thread(Box::new(meta.clone()), top));
                            top += FLAT_ROW_HEIGHT;
                        }
                    }
                    if let Some(count) = self.settled_more_count("recent", settled_hidden_count) {
                        visible.push(FlatListRow::More(count));
                    }
                    if self.flat_list_state.item_count() != visible.len() {
                        self.flat_list_state.reset(visible.len());
                    }
                    let project_names = projects
                        .into_iter()
                        .map(|project| (project.id, project.name))
                        .collect::<HashMap<_, _>>();
                    let active_id = active_id.clone();
                    let thread_list = list(
                        self.flat_list_state.clone(),
                        cx.processor(move |this, index: usize, _window, cx| {
                            let Some(row) = visible.get(index) else {
                                return div().into_any_element();
                            };
                            let (meta, target_top) = match row {
                                FlatListRow::Thread(meta, top) => (meta, top),
                                FlatListRow::Boundary(section, empty) => {
                                    return div()
                                        .w_full()
                                        .px_2()
                                        .child(this.render_drag_boundary(
                                            *section,
                                            *empty,
                                            scope.clone(),
                                            cx,
                                        ))
                                        .into_any_element();
                                }
                                FlatListRow::Settled => {
                                    let header = this.render_settled_header(
                                        "recent",
                                        settled_count,
                                        scope.as_deref(),
                                        cx,
                                    );
                                    return this
                                        .drop_zone(
                                            div().w_full().child(header),
                                            DropZone::Settled,
                                            scope.clone(),
                                            0.,
                                            cx,
                                        )
                                        .into_any_element();
                                }
                                FlatListRow::More(count) => {
                                    return this.render_settled_more("recent", *count, cx);
                                }
                                FlatListRow::Empty => return this.render_active_empty(cx),
                            };
                            let target_top = *target_top;
                            let row = div().w_full().px_2().pb(px(2.));
                            let row = if this
                                .drag
                                .as_ref()
                                .is_some_and(|drag| drag.session_id == meta.id)
                            {
                                row.child(Self::render_drag_gap(FLAT_ROW_INNER_HEIGHT))
                            } else {
                                let project_name = meta
                                    .project_id
                                    .as_ref()
                                    .and_then(|project_id| project_names.get(project_id))
                                    .cloned();
                                let is_active = active_id.as_deref() == Some(meta.id.as_str());
                                row.child(this.render_flat_thread(
                                    meta,
                                    &flags,
                                    project_name,
                                    is_active,
                                    cx,
                                ))
                            };
                            let zone = DropZone::Row {
                                id: meta.id.clone(),
                                section: tcode_core::thread_sort::thread_section(meta),
                            };
                            this.drop_zone(
                                animate_flat_thread_position(row, &meta.id, target_top),
                                zone,
                                scope.clone(),
                                8.,
                                cx,
                            )
                            .into_any_element()
                        }),
                    )
                    .flex_1()
                    .min_h_0()
                    .into_any_element();
                    (
                        self.render_flat_header(cx).into_any_element(),
                        v_flex()
                            .id("flat-thread-list")
                            .flex_1()
                            .min_h_0()
                            .relative()
                            .on_drop(cx.listener(|this, _: &DraggedThread, window, cx| {
                                this.drop_thread(window, cx)
                            }))
                            .on_drag_move(cx.listener(
                                |this, event: &gpui::DragMoveEvent<DraggedThread>, _, cx| {
                                    this.drag_auto_scroll(event, cx)
                                },
                            ))
                            .child(crate::scroll::page_viewport(
                                "flat-thread-bounce",
                                crate::wheel_easing::Handle::List(self.flat_list_state.clone()),
                                thread_list,
                            ))
                            .when(!window.is_inspector_picking(cx), |list| {
                                list.child(thread_list_scrollbar(
                                    "flat-thread-scrollbar",
                                    &self.flat_list_state,
                                    FLAT_ROW_HEIGHT,
                                ))
                            })
                            .into_any_element(),
                    )
                }
            }
        };

        v_flex()
            .size_full()
            .bg(cx.theme().sidebar)
            .text_color(cx.theme().sidebar_foreground)
            .on_action(cx.listener(Self::on_link_pull_request))
            .on_action(cx.listener(Self::on_rename))
            .on_action(cx.listener(Self::on_regenerate_title))
            .on_action(cx.listener(Self::on_fork))
            .on_action(cx.listener(Self::on_merge_worktree))
            .on_action(cx.listener(Self::on_mark_unread))
            .on_action(cx.listener(Self::on_copy_path))
            .on_action(cx.listener(Self::on_copy_id))
            .on_action(cx.listener(Self::on_export_jsonl))
            .on_action(cx.listener(Self::on_export_markdown))
            .on_action(cx.listener(Self::on_settle))
            .on_action(cx.listener(Self::on_auto_settle))
            .on_action(cx.listener(Self::on_make_active))
            .on_action(cx.listener(Self::on_pin))
            .on_action(cx.listener(Self::on_unpin))
            .on_action(cx.listener(Self::on_move))
            .on_action(cx.listener(Self::on_arrange))
            .on_action(cx.listener(Self::on_archive))
            .on_action(cx.listener(Self::on_delete))
            .on_action(cx.listener(Self::on_project_archive_all))
            .on_action(cx.listener(Self::on_project_thread_rules))
            .on_action(cx.listener(Self::on_project_delete))
            .on_action(cx.listener(Self::on_change_project_icon))
            .on_action(cx.listener(Self::on_change_project_root))
            .on_action(cx.listener(Self::on_project_reveal))
            .on_action(cx.listener(Self::on_filter_project))
            .on_action(cx.listener(Self::on_start_draft_for_project))
            .on_action(cx.listener(Self::on_quick_chat))
            .on_action(cx.listener(Self::on_toggle_share))
            .on_action(cx.listener(Self::on_new_space_and_share))
            .child(self.render_app_row(window, cx))
            .child(self.render_search_row(cx))
            .child(self.render_feature_rows(cx))
            .child(header)
            .child(if self.store.read(cx).threads_loading() {
                if self.store.read(cx).pending_sessions().is_empty() {
                    crate::material::loading_skeleton(cx)
                } else {
                    self.pending_thread_rows(cx)
                }
            } else {
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.pending_thread_rows(cx))
                    .child(thread_list)
                    .into_any_element()
            })
            .child(self.render_footer(cx))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::ProviderKind;
    use gpui::{TestAppContext, VisualTestContext, size};
    use std::path::PathBuf;
    use tcode_core::project::Project;
    use tcode_runtime::pipe::{HostServices, spawn_host};
    use tcode_services::store::SessionStore;

    struct WorkingThreadRowProbe;

    struct FlatReorderAnimationProbe {
        reversed: bool,
        list_state: ListState,
    }

    impl Render for FlatReorderAnimationProbe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let order = if self.reversed {
                ["second", "first"]
            } else {
                ["first", "second"]
            };

            list(self.list_state.clone(), move |index, _, _| {
                let id = order[index];
                let target_top = index as f32 * FLAT_ROW_HEIGHT;
                animate_flat_thread_position(
                    div()
                        .h(px(FLAT_ROW_HEIGHT))
                        .debug_selector(move || format!("flat-reorder-row-{id}")),
                    id,
                    target_top,
                )
                .into_any_element()
            })
            .size_full()
        }
    }

    impl Render for WorkingThreadRowProbe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            h_flex()
                .w_full()
                .h(px(30.))
                .items_center()
                .gap_2()
                .pl(px(42.))
                .pr_2()
                .debug_selector(|| "thread-row".into())
                .child(
                    h_flex()
                        .flex_none()
                        .items_center()
                        .gap_1()
                        .child(div().size(px(6.)))
                        .child(div().whitespace_nowrap().text_size(px(11.)).child("工作中")),
                )
                .child(div().flex_none().text_size(px(13.)).child("↳"))
                .child(
                    truncated_sidebar_label()
                        .debug_selector(|| "thread-title".into())
                        .text_size(px(13.))
                        .child("Phase 0 修复架构约束测试"),
                )
        }
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });
    }

    fn session(id: &str, parent_id: Option<&str>) -> SessionMeta {
        let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/project"), None);
        meta.id = id.to_string();
        meta.title = id.to_string();
        meta.parent_session_id = parent_id.map(str::to_string);
        meta
    }

    #[gpui::test]
    fn compact_thread_distinguishes_host_waiting_activity(cx: &mut TestAppContext) {
        use tcode_protocol::{
            EventEnvelope, HostMessage, IndexSnapshot, IndexSummary, ServerEvent, SessionActivity,
            Topic, encode_line,
        };
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
        cx.update(crate::theme::init);
        let (to_host, _outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let send = |topic, event| {
            incoming
                .try_send(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic,
                        event,
                    }))
                    .unwrap(),
                )
                .unwrap();
        };
        let summary = |waiting: bool| IndexSummary {
            activity: HashMap::from([(
                "background".into(),
                SessionActivity {
                    working: !waiting,
                    turn_running: !waiting,
                    waiting,
                    waiting_for_approval: false,
                    waiting_for_input: false,
                    failed: false,
                    unread: false,
                    fork: ForkAvailability::Available,
                    agent: None,
                },
            )]),
            ..Default::default()
        };
        let project = Project::from_root(PathBuf::from("/project"));
        let mut meta = session("background", None);
        meta.project_id = Some(project.id.clone());
        send(
            Topic::Settings,
            ServerEvent::SettingsSnapshot(Default::default()),
        );
        send(
            Topic::Index,
            ServerEvent::IndexSnapshot(IndexSnapshot {
                sessions: vec![meta],
                projects: vec![project],
                summary: summary(true),
            }),
        );
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump_link = link.clone();
        let executor = cx.background_executor.clone();
        let _pump = cx.background_executor.spawn(async move {
            pump_link
                .pump_with_timer(|| executor.timer(std::time::Duration::from_millis(25)))
                .await;
        });
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        store.update(cx, |store, _| store.select_session("background".into()));
        let window_state = cx.new(|_| WindowState::new(false).with_compact(true));
        let (sidebar, cx) = cx
            .add_window_view(|_, cx| SessionsSidebar::new(store.clone(), window_state.clone(), cx));
        cx.simulate_resize(size(px(393.), px(852.)));
        for (waiting, expected) in [(true, "Waiting"), (false, "Working")] {
            send(
                Topic::Index,
                ServerEvent::IndexSummaryReplaced(summary(waiting)),
            );
            cx.run_until_parked();
            store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            draw(cx);
            assert!(cx.debug_bounds("compact-row-background").is_some());
            sidebar.read_with(cx, |sidebar, cx| {
                let row = sidebar
                    .compact_model
                    .as_ref()
                    .unwrap()
                    .rows
                    .iter()
                    .find_map(|row| match row {
                        CompactListRow::Thread(row) if row.state.session_id == "background" => {
                            Some(row)
                        }
                        _ => None,
                    })
                    .expect("rendered phone thread");
                let (label, color) = compact_status_line(&row.state, row.working, cx).unwrap();
                assert_eq!(label, expected);
                assert_eq!(
                    color,
                    if waiting {
                        cx.theme().muted_foreground
                    } else {
                        cx.theme().primary
                    }
                );
            });
        }
    }

    #[gpui::test]
    fn working_thread_title_stays_inside_row_at_every_sidebar_width(cx: &mut TestAppContext) {
        let (_, cx) = cx.add_window_view(|_, _| WorkingThreadRowProbe);
        let cx: &mut VisualTestContext = cx;

        // The resizable sidebar is constrained to 220..=380px. Half-pixel
        // increments cover Retina resize boundaries where glyph rounding used
        // to push the final character onto a second line.
        for half_pixel_width in 440..=760 {
            let width = half_pixel_width as f32 / 2.;
            cx.simulate_resize(size(px(width), px(60.)));
            draw(cx);

            let row = cx.debug_bounds("thread-row").expect("row bounds");
            let title = cx.debug_bounds("thread-title").expect("title bounds");
            assert!(
                title.top() >= row.top() && title.bottom() <= row.bottom(),
                "title escaped the row vertically at {width}px: row={row:?}, title={title:?}"
            );
            assert!(
                title.left() >= row.left() && title.right() <= row.right(),
                "title escaped the row horizontally at {width}px: row={row:?}, title={title:?}"
            );
        }
    }

    #[gpui::test]
    fn flat_rows_start_reordering_from_their_previous_positions(cx: &mut TestAppContext) {
        let (probe, cx) = cx.add_window_view(|_, _| FlatReorderAnimationProbe {
            reversed: false,
            list_state: ListState::new(2, ListAlignment::Top, px(0.)),
        });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(200.), px(200.)));
        draw(cx);

        let first_start = cx.debug_bounds("flat-reorder-row-first").unwrap().top();
        let second_start = cx.debug_bounds("flat-reorder-row-second").unwrap().top();
        assert_eq!(second_start - first_start, px(FLAT_ROW_HEIGHT));

        probe.update(cx, |probe, cx| {
            probe.reversed = true;
            cx.notify();
        });
        draw(cx);
        assert_eq!(
            cx.debug_bounds("flat-reorder-row-first").unwrap().top(),
            first_start,
            "first row snapped to its destination instead of starting at its old position"
        );
        assert_eq!(
            cx.debug_bounds("flat-reorder-row-second").unwrap().top(),
            second_start,
            "second row snapped to its destination instead of starting at its old position"
        );

        let callbacks = cx.update(|window, cx| window.simulate_next_frame(cx));
        assert!(callbacks > 0, "spring did not request an animation frame");
    }

    #[gpui::test]
    fn canceling_inline_rename_discards_the_unsaved_title(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let root = std::env::temp_dir().join(format!(
            "tcode-sidebar-rename-test-{}",
            tcode_services::store::now_millis()
        ));
        let session_store = SessionStore::open_at(root.clone()).unwrap();
        let project = Project::from_root(root.clone());
        let mut meta = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
        meta.project_id = Some(project.id.clone());
        meta.title = "Original title".into();
        let session_id = meta.id.clone();
        let host =
            spawn_host(session_store, HostServices::default()).expect("spawn sidebar test host");
        smol::block_on(host.update_state_for_test(move |state, _| {
            state.projects = vec![project];
            state.sessions = vec![meta];
        }))
        .expect("seed sidebar host");
        let store = cx.new(|cx| WorkspaceStore::new(host.link(), cx));

        let window_state = cx.new(|_| WindowState::new(false));
        let (sidebar, cx) =
            cx.add_window_view(|_, cx| SessionsSidebar::new(store, window_state.clone(), cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(360.), px(800.)));
        draw(cx);
        cx.update(|window, cx| {
            sidebar.update(cx, |sidebar, cx| {
                sidebar.on_rename(&ThreadRename(session_id.clone()), window, cx);
                let input = sidebar.renaming.as_ref().unwrap().input.clone();
                input.update(cx, |input, cx| input.set_value("Unsaved title", window, cx));
            });
        });

        draw(cx);
        sidebar.read_with(cx, |sidebar, cx| {
            let input = &sidebar
                .renaming
                .as_ref()
                .expect("mounted rename editor")
                .input;
            assert_eq!(input.read(cx).value().as_str(), "Unsaved title");
        });
        // This is the supported cancellation path: pointer-down outside the
        // mounted Input triggers its owning row's cancel handler.
        cx.simulate_click(gpui::point(px(340.), px(760.)), gpui::Modifiers::default());
        draw(cx);
        cx.update(|_, cx| {
            assert!(sidebar.read(cx).renaming.is_none());
        });
        let title =
            smol::block_on(host.update_state_for_test(|state, _| state.sessions[0].title.clone()))
                .expect("read host title");
        assert_eq!(title, "Original title");

        host.shutdown_blocking().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[gpui::test]
    fn thread_rows_carry_the_provider_mark_until_the_setting_is_off(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let root = std::env::temp_dir().join(format!(
            "tcode-provider-mark-{}",
            tcode_services::store::now_millis()
        ));
        let host = spawn_host(
            SessionStore::open_at(root.clone()).unwrap(),
            HostServices::default(),
        )
        .unwrap();
        let project = Project::from_root(root.clone());
        let mut codex = session("codex-thread", None);
        codex.project_id = Some(project.id.clone());
        let mut claude = session("claude-thread", None);
        claude.provider = ProviderKind::ClaudeCode;
        claude.project_id = Some(project.id.clone());
        smol::block_on(host.update_state_for_test(move |state, _| {
            state.projects = vec![project];
            state.sessions = vec![codex, claude];
        }))
        .unwrap();
        let store = cx.new(|cx| WorkspaceStore::new(host.link(), cx));
        let window_state = cx.new(|_| WindowState::new(false));
        let (_, cx) =
            cx.add_window_view(|_, cx| SessionsSidebar::new(store.clone(), window_state, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(320.), px(600.)));
        store.update(cx, |store, _| store.select_session("codex-thread".into()));
        // (row selector, mark selector); the active Codex row comes first.
        let selectors = [
            (
                "sidebar-thread-codex-thread",
                "sidebar-provider-mark-codex-thread",
            ),
            (
                "sidebar-thread-claude-thread",
                "sidebar-provider-mark-claude-thread",
            ),
        ];
        let wait_for_rows = |cx: &mut VisualTestContext| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
                draw(cx);
                let rows: Vec<_> = selectors
                    .iter()
                    .filter_map(|(row, _)| cx.debug_bounds(row))
                    .collect();
                if rows.len() == selectors.len() {
                    return rows;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "thread rows never rendered"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };

        let set_marks = |cx: &mut VisualTestContext, enabled: bool| {
            store.update(cx, |store, _| store.set_sidebar_provider_marks(enabled));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while store.read_with(cx, |store, _| store.settings().sidebar_provider_marks) != enabled
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "setting never replicated"
                );
                store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };

        // Off by default: rows render without a mark.
        wait_for_rows(cx);
        for (_, mark_selector) in selectors {
            assert!(cx.debug_bounds(mark_selector).is_none());
        }

        set_marks(cx, true);
        let rows = wait_for_rows(cx);
        for ((_, mark_selector), row) in selectors.iter().zip(&rows) {
            let mark = cx
                .debug_bounds(mark_selector)
                .expect("each row carries its provider mark once enabled");
            assert!(
                mark.top() >= row.top()
                    && mark.bottom() <= row.bottom()
                    && mark.right() <= row.right()
                    && mark.left() >= row.left(),
                "mark sits entirely inside the row: {mark:?} in {row:?}"
            );
            assert_eq!(
                mark.size.height,
                row.size.height - px(PROVIDER_MARK_INSET),
                "mark fits the row height minus the inset"
            );
        }
        let list_active = cx.update(|_, cx| cx.theme().list_active);
        let painted_active = |cx: &mut VisualTestContext, bounds: gpui::Bounds<gpui::Pixels>| {
            cx.update(|window, _| {
                window.painted_quads().iter().any(|quad| {
                    quad.bounds == bounds.scale(window.scale_factor())
                        && quad.background == gpui::Background::from(list_active)
                })
            })
        };
        assert!(
            painted_active(cx, rows[0]),
            "the active row keeps the neutral selected surface"
        );

        set_marks(cx, false);
        let rows = wait_for_rows(cx);
        for (_, mark_selector) in selectors {
            assert!(
                cx.debug_bounds(mark_selector).is_none(),
                "with marks off again no mark is drawn"
            );
        }
        assert!(painted_active(cx, rows[0]));
        let _ = std::fs::remove_dir_all(root);
    }

    #[gpui::test]
    fn compact_menu_dismissal_preserves_selection_and_clears_row_surface(cx: &mut TestAppContext) {
        use gpui::{PlatformInput, TouchEvent, TouchId, TouchPhase};
        cx.update(crate::theme::init);
        let root = std::env::temp_dir().join(format!(
            "tcode-menu-selection-{}",
            tcode_services::store::now_millis()
        ));
        let host = spawn_host(
            SessionStore::open_at(root.clone()).unwrap(),
            HostServices::default(),
        )
        .unwrap();
        let project = Project::from_root(root.clone());
        let sessions = (0..12)
            .map(|index| {
                let mut meta = session(&format!("menu-{index}"), None);
                meta.project_id = Some(project.id.clone());
                meta.updated_at = 1000 - index;
                meta.created_at = meta.updated_at;
                meta
            })
            .collect();
        smol::block_on(host.update_state_for_test(move |state, _| {
            state.projects = vec![project];
            state.sessions = sessions;
        }))
        .unwrap();
        let store = cx.new(|cx| WorkspaceStore::new(host.link(), cx));
        let navigation = cx.new(|_| WindowState::new(false).with_compact(true));
        let (_, cx) =
            cx.add_window_view(|_, cx| SessionsSidebar::new(store.clone(), navigation.clone(), cx));
        cx.simulate_resize(size(px(393.), px(852.)));
        draw(cx);
        // A remote host may not have sent status yet. Selection must repaint
        // from the client notification without waiting for a host event.
        host.shutdown_blocking().unwrap();
        draw(cx);
        let a = cx.debug_bounds("compact-row-menu-0").unwrap();
        let b = cx.debug_bounds("compact-row-menu-10").unwrap();
        let send = |cx: &mut VisualTestContext, id, phase, position| {
            cx.update(|window, cx| {
                window.dispatch_event(
                    PlatformInput::Touch(TouchEvent {
                        id: TouchId(id),
                        phase,
                        position,
                        predicted_position: None,
                        force: None,
                    }),
                    cx,
                );
            })
        };
        send(cx, 1, TouchPhase::Started, a.center());
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(801));
        cx.run_until_parked();
        send(cx, 1, TouchPhase::Ended, a.center());
        draw(cx);
        let menu = cx
            .debug_bounds("tcode-popup-menu")
            .expect("long press opens menu");
        let outside = gpui::point(b.left() + px(12.), b.center().y);
        assert!(!menu.contains(&outside));
        let selected = store.read_with(cx, |store, _| store.active_session_id());
        let destination = navigation.read_with(cx, |state, _| state.destination());
        send(cx, 2, TouchPhase::Started, outside);
        send(cx, 2, TouchPhase::Ended, outside);
        draw(cx);
        assert!(cx.debug_bounds("tcode-popup-menu").is_none());
        assert_eq!(
            store.read_with(cx, |store, _| store.active_session_id()),
            selected
        );
        assert_eq!(
            navigation.read_with(cx, |state, _| state.destination()),
            destination
        );
        let selected_surface = |cx: &mut VisualTestContext, bounds: gpui::Bounds<gpui::Pixels>| {
            cx.update(|window, cx| {
                window.painted_quads().iter().any(|quad| {
                    quad.bounds == bounds.scale(window.scale_factor())
                        && quad.background == gpui::Background::from(cx.theme().list_active)
                })
            })
        };
        assert!(
            !selected_surface(cx, a),
            "dismissed A has no pressed or selected fill"
        );
        send(cx, 3, TouchPhase::Started, b.center());
        send(cx, 3, TouchPhase::Ended, b.center());
        draw(cx);
        assert_eq!(
            store
                .read_with(cx, |store, _| store.active_session_id())
                .as_deref(),
            Some("menu-10")
        );
        assert!(!selected_surface(cx, a));
        assert!(selected_surface(cx, b), "selected surface follows B");
        let _ = std::fs::remove_dir_all(root);
    }

    /// Exercise Recent and its persisted layout toggle at phone geometry,
    /// returning whether By project draws a section header.
    fn compact_list_has_project_headers(cx: &mut TestAppContext, projects: usize) -> bool {
        cx.update(crate::theme::init);
        let root = std::env::temp_dir().join(format!(
            "tcode-sidebar-{projects}-project-{}",
            tcode_services::store::now_millis()
        ));
        let host = spawn_host(
            SessionStore::open_at(root.clone()).unwrap(),
            HostServices::default(),
        )
        .expect("spawn sidebar test host");
        let seeded: Vec<Project> = (0..projects)
            .map(|index| Project::from_root(root.join(format!("project-{index}"))))
            .collect();
        let sessions = seeded
            .iter()
            .enumerate()
            .map(|(index, project)| {
                let mut meta = session(&format!("thread-{index}"), None);
                meta.project_id = Some(project.id.clone());
                meta.updated_at = 100 + index as u64;
                meta.created_at = meta.updated_at;
                meta
            })
            .collect::<Vec<_>>();
        let projects_seed = seeded.clone();
        smol::block_on(host.update_state_for_test(move |state, _| {
            state.projects = projects_seed;
            state.sessions = sessions;
        }))
        .expect("seed projects");

        let store = cx.new(|cx| WorkspaceStore::new(host.link(), cx));
        let window_state = cx.new(|_| WindowState::new(false).with_compact(true));
        let (sidebar, cx) =
            cx.add_window_view(|_, cx| SessionsSidebar::new(store.clone(), window_state, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(393.), px(852.)));
        sidebar.update(cx, |_, cx| {
            cx.notify();
        });
        draw(cx);
        assert!(
            cx.debug_bounds("compact-thread-list").is_some(),
            "the seeded threads are listed"
        );
        assert_eq!(
            store.read_with(cx, |store, _| store.sidebar_layout()),
            SidebarLayout::Flat
        );
        assert!(cx.debug_bounds("compact-group-header").is_none());
        for index in 0..projects {
            assert!(
                cx.debug_bounds(if index == 0 {
                    "compact-project-project-0"
                } else {
                    "compact-project-project-1"
                })
                .is_some(),
                "Recent names each project in the subtitle"
            );
        }
        if projects == 2 {
            assert!(
                cx.debug_bounds("compact-row-thread-1").unwrap().top()
                    < cx.debug_bounds("compact-row-thread-0").unwrap().top(),
                "creation orders unarranged threads across projects"
            );
        }
        let toggle = cx.debug_bounds("compact-layout-toggle").unwrap();
        assert_eq!(toggle.size, size(px(44.), px(44.)));
        cx.simulate_click(toggle.center(), gpui::Modifiers::default());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            draw(cx);
            if store.read_with(cx, |store, _| store.sidebar_layout()) == SidebarLayout::Grouped {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "layout change reaches the replica"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let headers = cx.debug_bounds("compact-group-header").is_some();
        if projects > 1 {
            for collapsed in [true, false] {
                let header = cx.debug_bounds("compact-group-header").unwrap();
                cx.simulate_click(header.center(), gpui::Modifiers::default());
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                loop {
                    store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
                    draw(cx);
                    let any_collapsed = store.read_with(cx, |store, _| {
                        seeded
                            .iter()
                            .any(|project| store.is_project_collapsed(&project.id))
                    });
                    if any_collapsed == collapsed {
                        break;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "project toggle reaches the replica"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                let visible = ["compact-row-thread-0", "compact-row-thread-1"]
                    .into_iter()
                    .filter(|selector| cx.debug_bounds(selector).is_some())
                    .count();
                assert_eq!(
                    visible,
                    if collapsed { 1 } else { 2 },
                    "the folder action hides and restores its threads"
                );
            }
        }

        host.shutdown_blocking().expect("stop host");
        let persisted = tcode_services::settings::SettingsStore::new(root.clone()).load();
        assert_eq!(
            persisted.sidebar_layout,
            SidebarLayout::Grouped,
            "the actual header action persists the shared choice"
        );
        let _ = std::fs::remove_dir_all(root);
        headers
    }

    /// A project header separates one project from the next. With a single
    /// project there is nothing to separate, so the compact list shows its
    /// threads directly; a second project brings the headers back.
    #[gpui::test]
    fn compact_recent_orders_projects_and_switches_to_persisted_grouped_view(
        cx: &mut TestAppContext,
    ) {
        assert!(
            !compact_list_has_project_headers(cx, 1),
            "a single project needs no header to separate it from anything"
        );
        assert!(
            compact_list_has_project_headers(cx, 2),
            "two projects need their headers back"
        );
    }

    #[gpui::test]
    fn thread_list_scrollbar_drags_without_selecting_a_thread(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        cx.update(crate::theme::init);
        let root = std::env::temp_dir().join(format!(
            "tcode-sidebar-scrollbar-{}",
            tcode_services::store::now_millis()
        ));
        let host = spawn_host(
            SessionStore::open_at(root.clone()).unwrap(),
            HostServices::default(),
        )
        .unwrap();
        let project = Project::from_root(root.join("project"));
        smol::block_on(host.update_state_for_test(move |state, _| {
            state.sessions = (0..60)
                .map(|index| {
                    let mut meta = session(&format!("scrollbar-{index}"), None);
                    meta.project_id = Some(project.id.clone());
                    meta.updated_at = now_secs().saturating_sub(index);
                    meta
                })
                .collect();
            state.projects = vec![project];
        }))
        .unwrap();
        let store = cx.new(|cx| WorkspaceStore::new(host.link(), cx));
        let window_state = cx.new(|_| WindowState::new(false));
        let (sidebar, cx) = cx
            .add_window_view(|_, cx| SessionsSidebar::new(store.clone(), window_state.clone(), cx));

        for compact in [false, true] {
            window_state.update(cx, |state, cx| {
                state.compact = compact;
                cx.notify();
            });
            cx.simulate_resize(size(px(if compact { 393. } else { 300. }), px(800.)));
            draw(cx);
            let list = sidebar.read_with(cx, |sidebar, _| {
                if compact {
                    sidebar.compact_list_state.clone()
                } else {
                    sidebar.flat_list_state.clone()
                }
            });
            let selected = store.read_with(cx, |store, _| store.active_session_id());
            let viewport = list.viewport_bounds();
            let thumb = gpui::point(viewport.right() - px(5.), viewport.top() + px(8.));
            cx.simulate_mouse_move(thumb, None, gpui::Modifiers::default());
            draw(cx);
            cx.simulate_event(gpui::MouseDownEvent {
                position: thumb,
                button: gpui::MouseButton::Left,
                ..Default::default()
            });
            let target = gpui::point(thumb.x, viewport.bottom() - px(8.));
            cx.simulate_mouse_move(
                target,
                Some(gpui::MouseButton::Left),
                gpui::Modifiers::default(),
            );
            cx.simulate_event(gpui::MouseUpEvent {
                position: target,
                button: gpui::MouseButton::Left,
                ..Default::default()
            });
            draw(cx);
            assert_eq!(
                list.is_scrolled_to_end(),
                Some(true),
                "dragging to the bottom must reach the last thread, compact={compact}"
            );
            let last = cx
                .debug_bounds(if compact {
                    "compact-row-scrollbar-59"
                } else {
                    "sidebar-thread-scrollbar-59"
                })
                .expect("last thread is rendered after dragging to the bottom");
            assert!(last.bottom() <= viewport.bottom(), "last thread is clipped");
            assert_eq!(
                store.read_with(cx, |store, _| store.active_session_id()),
                selected,
                "dragging must not activate a thread under the scrollbar"
            );
        }
        host.shutdown_blocking().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[gpui::test]
    fn compact_scroll_reuses_model_and_renders_only_viewport(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        struct SlidingPage {
            sidebar: Entity<SessionsSidebar>,
            offset: gpui::Pixels,
        }
        impl Render for SlidingPage {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().size_full().overflow_hidden().child(
                    div()
                        .absolute()
                        .left(self.offset)
                        .size_full()
                        .child(self.sidebar.clone()),
                )
            }
        }
        cx.update(crate::theme::init);
        let (to_host, _outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let project = Project::from_root(PathBuf::from("/project"));
        let sessions = (0..300)
            .map(|index| {
                let mut meta = session(&format!("virtual-{index}"), None);
                meta.project_id = Some(project.id.clone());
                meta.updated_at = 1_000 - index;
                meta.created_at = meta.updated_at;
                meta
            })
            .collect();
        for (topic, event) in [
            (
                tcode_protocol::Topic::Settings,
                tcode_protocol::ServerEvent::SettingsSnapshot(Default::default()),
            ),
            (
                tcode_protocol::Topic::Index,
                tcode_protocol::ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                    summary: Default::default(),
                    sessions,
                    projects: vec![project],
                }),
            ),
        ] {
            incoming
                .try_send(
                    tcode_protocol::encode_line(&tcode_protocol::HostMessage::Event(
                        tcode_protocol::EventEnvelope {
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
        let executor = cx.background_executor.clone();
        let _pump = cx.background_executor.spawn(async move {
            pump_link
                .pump_with_timer(|| executor.timer(std::time::Duration::from_millis(25)))
                .await;
        });
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        store.update(cx, |store, _| store.select_session("virtual-0".into()));
        let window_state = cx.new(|_| WindowState::new(false).with_compact(true));
        let (page, cx) = cx.add_window_view(|_, cx| SlidingPage {
            sidebar: cx.new(|cx| SessionsSidebar::new(store.clone(), window_state, cx)),
            offset: px(0.),
        });
        let sidebar = page.read_with(cx, |page, _| page.sidebar.clone());
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        cx.simulate_resize(size(px(393.), px(852.)));
        draw(cx);
        // Only the address is kept: a second strong reference, which the app never
        // holds, would turn the wall-clock minute refresh of relative times into a
        // copy of the whole model.
        let model = sidebar.read_with(cx, |sidebar, _| {
            let model = sidebar.compact_model.as_ref().unwrap();
            assert_eq!(model.rows.len(), 301);
            Rc::as_ptr(model)
        });
        for (index, selector) in [
            (0, "compact-row-virtual-0"),
            (100, "compact-row-virtual-100"),
            (200, "compact-row-virtual-200"),
        ] {
            sidebar.update(cx, |sidebar, cx| {
                sidebar.compact_rows_rendered.set(0);
                sidebar.compact_list_state.scroll_to(gpui::ListOffset {
                    item_ix: index,
                    offset_in_item: px(0.),
                });
                cx.notify();
            });
            page.update(cx, |page, cx| {
                page.offset = px(index as f32);
                cx.notify();
            });
            draw(cx);
            sidebar.read_with(cx, |sidebar, _| {
                let current = sidebar.compact_model.as_ref().unwrap();
                assert!(
                    std::ptr::eq(model, Rc::as_ptr(current)),
                    "scrolling must not rebuild families or labels"
                );
                assert!(
                    sidebar.compact_rows_rendered.get() < 40,
                    "one phone viewport must not construct 300 rows: rendered {} at index {index}",
                    sidebar.compact_rows_rendered.get()
                );
            });
            assert!(cx.debug_bounds(selector).is_some());
        }
        store.update(cx, |_, cx| {
            cx.emit(StoreChange {
                topic: TopicKind::Index,
            })
        });
        draw(cx);
        sidebar.read_with(cx, |sidebar, _| {
            assert!(
                !std::ptr::eq(model, Rc::as_ptr(sidebar.compact_model.as_ref().unwrap())),
                "Index updates invalidate cached row state"
            );
            assert_eq!(
                sidebar.compact_list_state.logical_scroll_top().item_ix,
                200,
                "store changes retain the visible thread anchor"
            );
        });
    }

    #[gpui::test]
    fn grouped_layout_lays_out_only_the_visible_threads(cx: &mut TestAppContext) {
        use tcode_protocol::{
            EventEnvelope, HostMessage, IndexSnapshot, ServerEvent, Topic, encode_line,
        };
        cx.update(crate::theme::init);
        let (to_host, _outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let send = |topic, event| {
            incoming
                .try_send(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic,
                        event,
                    }))
                    .unwrap(),
                )
                .unwrap()
        };
        let mut project = Project::from_root(PathBuf::from("/project"));
        project.id = "project".into();
        let mut sessions = (0..300)
            .map(|index| {
                let mut meta = session(&format!("virtual-{index}"), None);
                meta.project_id = Some(project.id.clone());
                meta.updated_at = 1_000 - index;
                meta.created_at = meta.updated_at;
                meta
            })
            .collect::<Vec<_>>();
        let mut other = Project::from_root(PathBuf::from("/other"));
        other.id = "other".into();
        let mut other_thread = session("other-0", None);
        other_thread.project_id = Some(other.id.clone());
        other_thread.updated_at = 1;
        sessions.push(other_thread.clone());
        let snapshot = |sessions: &Vec<SessionMeta>| {
            ServerEvent::IndexSnapshot(IndexSnapshot {
                summary: Default::default(),
                sessions: sessions.clone(),
                projects: vec![project.clone(), other.clone()],
            })
        };
        send(
            Topic::Settings,
            ServerEvent::SettingsSnapshot(tcode_core::settings::Settings {
                sidebar_layout: SidebarLayout::Grouped,
                ..Default::default()
            }),
        );
        send(Topic::Index, snapshot(&sessions));
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump_link = link.clone();
        let executor = cx.background_executor.clone();
        let _pump = cx.background_executor.spawn(async move {
            pump_link
                .pump_with_timer(|| executor.timer(std::time::Duration::from_millis(25)))
                .await;
        });
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        store.update(cx, |store, _| store.select_session("virtual-0".into()));
        let window_state = cx.new(|_| WindowState::new(false));
        let (sidebar, cx) =
            cx.add_window_view(|_, cx| SessionsSidebar::new(store.clone(), window_state, cx));
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        cx.simulate_resize(size(px(300.), px(800.)));
        sidebar.update(cx, |_, cx| {
            cx.notify();
        });
        draw(cx);
        assert!(cx.debug_bounds("sidebar-thread-virtual-5").is_some());
        assert!(
            cx.debug_bounds("sidebar-thread-virtual-250").is_none(),
            "an expanded project must not lay out all 300 thread rows"
        );

        let far_row = |sidebar: &SessionsSidebar| {
            sidebar
                .grouped_row_keys
                .iter()
                .position(|(_, id)| id == "virtual-250")
                .unwrap()
        };
        sidebar.update(cx, |sidebar, cx| {
            sidebar.grouped_list_state.scroll_to(gpui::ListOffset {
                item_ix: far_row(sidebar),
                offset_in_item: px(0.),
            });
            cx.notify();
        });
        draw(cx);
        assert!(cx.debug_bounds("sidebar-thread-virtual-250").is_some());
        assert!(cx.debug_bounds("sidebar-thread-virtual-5").is_none());

        let mut newer = session("virtual-new", None);
        newer.project_id = Some(project.id.clone());
        newer.updated_at = 2_000;
        newer.created_at = 2_000;
        sessions.insert(0, newer);
        let apply = |sessions: &Vec<SessionMeta>, cx: &mut VisualTestContext| {
            send(Topic::Index, snapshot(sessions));
            cx.run_until_parked();
            store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            draw(cx);
        };
        apply(&sessions, cx);
        sidebar.read_with(cx, |sidebar, _| {
            assert_eq!(
                sidebar.grouped_list_state.logical_scroll_top().item_ix,
                far_row(sidebar),
                "a new thread above the viewport keeps the visible thread anchored"
            );
        });
        assert!(cx.debug_bounds("sidebar-thread-virtual-250").is_some());

        let top = sidebar.read_with(cx, |sidebar, _| far_row(sidebar));
        sessions
            .iter_mut()
            .find(|meta| meta.id == "virtual-250")
            .unwrap()
            .archived_at = Some(1);
        apply(&sessions, cx);
        sidebar.read_with(cx, |sidebar, _| {
            assert_eq!(
                sidebar.grouped_list_state.logical_scroll_top().item_ix,
                top,
                "archiving the top thread leaves the list where it was"
            );
        });
        assert!(cx.debug_bounds("sidebar-thread-virtual-251").is_some());

        sidebar.update(cx, |sidebar, cx| {
            sidebar
                .grouped_list_state
                .scroll_to(gpui::ListOffset::default());
            cx.notify();
        });
        draw(cx);
        sessions
            .iter_mut()
            .find(|meta| meta.id == "other-0")
            .unwrap()
            .updated_at = 5_000;
        apply(&sessions, cx);
        sidebar.read_with(cx, |sidebar, _| {
            assert_eq!(sidebar.grouped_row_keys[0].1, "other");
            assert_eq!(
                sidebar.grouped_list_state.logical_scroll_top().item_ix,
                0,
                "a list at its top shows the project that moved up"
            );
        });
        assert!(cx.debug_bounds("sidebar-thread-other-0").is_some());
    }

    #[gpui::test]
    fn returning_from_a_thread_reveals_its_row_below_the_top_edge(cx: &mut TestAppContext) {
        use tcode_protocol::{
            EventEnvelope, HostMessage, IndexSnapshot, ServerEvent, Topic, encode_line,
        };
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        /// The compact shell mounts only the page on top of the history and
        /// answers `OpenThread` by pushing the thread page.
        struct TopPage {
            sidebar: Entity<SessionsSidebar>,
            window_state: Entity<WindowState>,
        }
        impl Render for TopPage {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let threads = self.window_state.read(cx).destination() == Destination::Threads;
                div()
                    .size_full()
                    .when(threads, |page| page.child(self.sidebar.clone()))
            }
        }
        cx.update(crate::theme::init);
        let (to_host, _outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let projects = ["alpha", "beta"].map(|id| {
            let mut project = Project::from_root(PathBuf::from(format!("/{id}")));
            project.id = id.into();
            project
        });
        let sessions = projects
            .iter()
            .flat_map(|project| {
                (0..40).map(move |index| {
                    let mut meta = session(&format!("{}-{index}", project.id), None);
                    meta.project_id = Some(project.id.clone());
                    meta.updated_at = 10_000 - index;
                    meta.created_at = meta.updated_at;
                    meta
                })
            })
            .collect();
        let settings = tcode_core::settings::Settings {
            sidebar_layout: SidebarLayout::Grouped,
            project_sort: tcode_core::settings::ProjectSort::NameAsc,
            ..Default::default()
        };
        for (topic, event) in [
            (Topic::Settings, ServerEvent::SettingsSnapshot(settings)),
            (
                Topic::Index,
                ServerEvent::IndexSnapshot(IndexSnapshot {
                    summary: Default::default(),
                    sessions,
                    projects: projects.to_vec(),
                }),
            ),
        ] {
            incoming
                .try_send(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic,
                        event,
                    }))
                    .unwrap(),
                )
                .unwrap();
        }
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump_link = link.clone();
        let executor = cx.background_executor.clone();
        let _pump = cx.background_executor.spawn(async move {
            pump_link
                .pump_with_timer(|| executor.timer(std::time::Duration::from_millis(25)))
                .await;
        });
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        store.update(cx, |store, _| store.select_session("beta-0".into()));
        let window_state = cx.new(|cx| {
            let mut state = WindowState::new(false).with_compact(true);
            state.enter_workspace(cx);
            state
        });
        let (page, cx) = cx.add_window_view(|_, cx| {
            cx.observe(&window_state, |_, _, cx| cx.notify()).detach();
            cx.subscribe(
                &window_state,
                |_, state, _: &crate::window_state::OpenThread, cx| {
                    state.update(cx, |state, cx| state.go(Destination::Thread, cx));
                },
            )
            .detach();
            TopPage {
                sidebar: cx.new(|cx| SessionsSidebar::new(store.clone(), window_state.clone(), cx)),
                window_state: window_state.clone(),
            }
        });
        let sidebar = page.read_with(cx, |page, _| page.sidebar.clone());
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        cx.simulate_resize(size(px(393.), px(852.)));
        draw(cx);
        let list = sidebar.read_with(cx, |sidebar, _| sidebar.compact_list_state.clone());
        let rows = sidebar.read_with(cx, |sidebar, _| {
            sidebar.compact_model.as_ref().unwrap().rows.clone()
        });
        assert_eq!(rows.len(), 83, "two project headers, 80 threads, the inset");
        let row_selector = |index: usize| -> &'static str {
            match &rows[index] {
                CompactListRow::Thread(row) => format!("compact-row-{}", row.meta.id).leak(),
                other => panic!("row {index} is a thread, not {:?}", other.key()),
            }
        };
        let open_and_return = |cx: &mut VisualTestContext, selector: &'static str| {
            let row = cx
                .debug_bounds(selector)
                .expect("the row to open is on screen");
            cx.simulate_click(row.center(), gpui::Modifiers::default());
            draw(cx);
            assert_eq!(
                window_state.read_with(cx, |state, _| state.destination()),
                Destination::Thread
            );
            assert!(
                cx.debug_bounds("compact-thread-list").is_none(),
                "the thread page covers the list"
            );
            // An admission that reopens a settled thread moves it to the unarranged head.
            let active = store.read_with(cx, |store, _| store.active_session_id().unwrap());
            let mut sessions = store.read_with(cx, |store, _| store.sidebar_sessions());
            let latest = sessions.iter().map(|meta| meta.updated_at).max().unwrap();
            sessions
                .iter_mut()
                .find(|meta| meta.id == active)
                .unwrap()
                .unsettled_at = Some(latest + 1);
            sessions.sort_by_key(|meta| std::cmp::Reverse(meta.updated_at));
            incoming
                .try_send(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic: Topic::Index,
                        event: ServerEvent::IndexSnapshot(IndexSnapshot {
                            summary: Default::default(),
                            sessions,
                            projects: projects.to_vec(),
                        }),
                    }))
                    .unwrap(),
                )
                .unwrap();
            cx.run_until_parked();
            store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            window_state.update(cx, |state, cx| assert!(state.back(cx)));
            draw(cx);
            draw(cx);
        };

        let deep = 55;
        sidebar.update(cx, |sidebar, cx| {
            sidebar.compact_list_state.scroll_to(gpui::ListOffset {
                item_ix: deep - 9,
                offset_in_item: px(0.),
            });
            cx.notify();
        });
        draw(cx);
        let viewport = list.viewport_bounds();
        let before = cx.debug_bounds(row_selector(deep)).unwrap();
        assert!(
            before.top() > viewport.center().y,
            "the row starts in the lower half"
        );
        open_and_return(cx, row_selector(deep));
        let key = rows[deep].key();
        sidebar.read_with(cx, |sidebar, _| {
            let rows = &sidebar.compact_model.as_ref().unwrap().rows;
            let index = rows.iter().position(|row| row.key() == key).unwrap();
            assert!(
                matches!(rows[index - 1], CompactListRow::Project(_)),
                "the thread just left heads its project group"
            );
        });
        let row = cx
            .debug_bounds(row_selector(deep))
            .expect("the thread just left is on screen");
        let depth = row.top() - viewport.top();
        assert!(
            depth >= viewport.size.height / 4. && depth <= viewport.size.height / 2.,
            "the row sits in the upper part of the list, not on its edge: {depth:?} of {:?}",
            viewport.size.height
        );

        sidebar.update(cx, |sidebar, cx| {
            sidebar.compact_list_state.scroll_to(gpui::ListOffset {
                item_ix: 1,
                offset_in_item: px(0.),
            });
            cx.notify();
        });
        draw(cx);
        open_and_return(cx, row_selector(1));
        let top = list.logical_scroll_top();
        assert_eq!(
            (top.item_ix, top.offset_in_item),
            (0, px(0.)),
            "a row near the start leaves the list at its top"
        );
        assert_eq!(
            cx.debug_bounds("compact-group-header").unwrap().top(),
            list.viewport_bounds().top(),
            "the first project header is back on screen"
        );
    }

    #[gpui::test]
    fn agents_are_not_listed_and_an_open_agent_selects_its_lead(cx: &mut TestAppContext) {
        use tcode_protocol::{
            EventEnvelope, HostMessage, IndexSnapshot, ServerEvent, Topic, encode_line,
        };
        cx.update(crate::theme::init);
        let (to_host, _outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let project = Project::from_root(PathBuf::from("/project"));
        let sessions = [
            ("parent", None),
            ("child", Some("parent")),
            ("mirror", Some("parent")),
            ("other", None),
        ]
        .into_iter()
        .map(|(id, parent)| {
            let mut meta = session(id, parent);
            meta.project_id = Some(project.id.clone());
            meta.native_subagent = (id == "mirror").then(|| "spawn-1".into());
            meta
        })
        .collect();
        let send = |topic, event| {
            incoming
                .try_send(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic,
                        event,
                    }))
                    .unwrap(),
                )
                .unwrap()
        };
        send(
            Topic::Settings,
            ServerEvent::SettingsSnapshot(Default::default()),
        );
        send(
            Topic::Index,
            ServerEvent::IndexSnapshot(IndexSnapshot {
                summary: Default::default(),
                sessions,
                projects: vec![project],
            }),
        );
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump_link = link.clone();
        let executor = cx.background_executor.clone();
        let _pump = cx.background_executor.spawn(async move {
            pump_link
                .pump_with_timer(|| executor.timer(std::time::Duration::from_millis(25)))
                .await;
        });
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        // A selection keeps the store from opening a draft against a host
        // this fake transport never answers.
        store.update(cx, |store, _| store.select_session("other".into()));
        let window_state = cx.new(|_| WindowState::new(false).with_compact(true));
        let (sidebar, cx) = cx
            .add_window_view(|_, cx| SessionsSidebar::new(store.clone(), window_state.clone(), cx));
        cx.simulate_resize(size(px(393.), px(852.)));
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        draw(cx);
        assert!(cx.debug_bounds("compact-row-parent").is_some());
        assert!(cx.debug_bounds("compact-row-child").is_none());
        assert!(cx.debug_bounds("compact-row-mirror").is_none());

        store.update(cx, |store, _| store.select_session("child".into()));
        sidebar.update(cx, |_, cx| cx.notify());
        draw(cx);
        let lead = cx.debug_bounds("compact-row-parent").unwrap();
        let selected = cx.update(|window, cx| {
            window.painted_quads().iter().any(|quad| {
                quad.bounds == lead.scale(window.scale_factor())
                    && quad.background == gpui::Background::from(cx.theme().list_active)
            })
        });
        assert!(
            selected,
            "the lead's row is drawn selected for its open agent"
        );
    }

    #[gpui::test]
    fn the_share_item_toggles_the_project_in_a_space_on_this_machine(cx: &mut TestAppContext) {
        use gpui::BorrowAppContext as _;
        use tcode_traverse::{HostConfig, HostMux, TraverseHost, TraverseMode};
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
        cx.update(crate::theme::init);
        let root = std::env::temp_dir().join(format!(
            "tcode-sidebar-share-{}",
            tcode_services::store::now_millis()
        ));
        let host = spawn_host(
            SessionStore::open_at(root.join("store")).unwrap(),
            HostServices::default(),
        )
        .unwrap();
        let mut project = Project::from_root(root.join("a"));
        project.id = "a".into();
        smol::block_on(host.update_state_for_test(move |state, _| {
            state.settings.sidebar_layout = SidebarLayout::Grouped;
            let mut meta = session("thread", None);
            meta.project_id = Some(project.id.clone());
            state.sessions.push(meta);
            state.projects = vec![project];
        }))
        .unwrap();
        // Idle pipes: no remote client attaches through the mux here.
        let (to_host, _host_rx) = async_channel::unbounded::<String>();
        let (_host_tx, from_host) = async_channel::unbounded::<String>();
        let mux = HostMux::new(to_host.clone(), from_host.clone());
        // A random port: the desktop's fixed one may be taken on this machine.
        let traverse = TraverseHost::start(
            mux.clone(),
            HostConfig {
                host_name: "Studio".into(),
                data_dir: root.join("traverse"),
                traverse: TraverseMode::Off,
                pairing_enabled: true,
                bind_port: None,
            },
        )
        .unwrap();
        let space_id = traverse.create_space("Design".into()).unwrap();
        cx.update(|cx| {
            let mut controller = crate::remote::RemoteController::new(
                mux,
                root.clone(),
                tcode_client::HostLink::new(to_host, from_host),
                Default::default(),
            );
            controller.adopt_host(traverse);
            cx.set_global(controller);
        });
        let store = cx.new(|cx| WorkspaceStore::new(host.link(), cx));
        let window_state = cx.new(|_| WindowState::new(false));
        let (sidebar, cx) = cx
            .add_window_view(|_, cx| SessionsSidebar::new(store.clone(), window_state.clone(), cx));
        cx.simulate_resize(size(px(320.), px(900.)));
        draw(cx);
        assert!(cx.debug_bounds("project-header-a").is_some());
        assert!(cx.debug_bounds("project-shared-a").is_none());

        let share = spaces::ToggleShare {
            space_id,
            project_id: "a".into(),
        };
        let shared = |cx: &mut VisualTestContext| {
            cx.read(|cx| {
                cx.global::<crate::remote::RemoteController>()
                    .hosting(tcode_protocol::HostingAction::State)
                    .unwrap()
                    .spaces[0]
                    .project_ids
                    .clone()
            })
        };
        sidebar.update_in(cx, |sidebar, window, cx| {
            sidebar.on_toggle_share(&share, window, cx)
        });
        draw(cx);
        assert_eq!(shared(cx), ["a"]);
        assert!(
            cx.debug_bounds("project-shared-a").is_some(),
            "a shared project is marked"
        );

        sidebar.update_in(cx, |sidebar, window, cx| {
            sidebar.on_toggle_share(&share, window, cx)
        });
        draw(cx);
        assert!(shared(cx).is_empty());
        assert!(cx.debug_bounds("project-shared-a").is_none());

        cx.update(|_, cx| {
            cx.update_global::<crate::remote::RemoteController, _>(|controller, _| {
                controller.stop_hosting()
            });
        });
        let _ = std::fs::remove_dir_all(root);
    }
    #[gpui::test]
    fn settled_shelf_pages_without_expanding_for_selection_and_navigation_matches_rows(
        cx: &mut TestAppContext,
    ) {
        use tcode_protocol::{
            EventEnvelope, HostMessage, IndexSnapshot, ServerEvent, Topic, encode_line,
        };
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
        cx.update(crate::theme::init);
        cx.update(|cx| cx.set_reduce_motion(true));
        let (to_host, _outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let send = |topic, event| {
            incoming
                .try_send(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic,
                        event,
                    }))
                    .unwrap(),
                )
                .unwrap();
        };
        let mut project = Project::from_root(PathBuf::from("/sample"));
        project.id = "sample".into();
        let sessions = (0..8)
            .map(|index| {
                let mut meta = session(&format!("active-{index}"), None);
                meta.project_id = Some(project.id.clone());
                meta.created_at = 100 - index;
                meta
            })
            .chain((0..40).map(|index| {
                let mut meta = session(&format!("settled-{index}"), None);
                meta.project_id = Some(project.id.clone());
                meta.settled_at = Some(100 - index);
                meta
            }))
            .collect();
        send(
            Topic::Settings,
            ServerEvent::SettingsSnapshot(Default::default()),
        );
        send(
            Topic::Index,
            ServerEvent::IndexSnapshot(IndexSnapshot {
                sessions,
                projects: vec![project, Project::from_root(PathBuf::from("/other-sample"))],
                summary: Default::default(),
            }),
        );
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump_link = link.clone();
        let executor = cx.background_executor.clone();
        let _pump = cx.background_executor.spawn(async move {
            pump_link
                .pump_with_timer(|| executor.timer(std::time::Duration::from_millis(25)))
                .await;
        });
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        store.update(cx, |store, _| store.select_session("settled-39".into()));
        let window_state = cx.new(|_| WindowState::new(false));
        let (sidebar, cx) = cx
            .add_window_view(|_, cx| SessionsSidebar::new(store.clone(), window_state.clone(), cx));
        cx.simulate_resize(size(px(393.), px(6000.)));
        for compact in [false, true] {
            window_state.update(cx, |state, cx| {
                state.compact = compact;
                cx.notify();
            });
            for layout in [SidebarLayout::Flat, SidebarLayout::Grouped] {
                send(
                    Topic::Settings,
                    ServerEvent::SettingsSnapshot(tcode_core::settings::Settings {
                        sidebar_layout: layout,
                        ..Default::default()
                    }),
                );
                cx.run_until_parked();
                store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
                sidebar.update(cx, |sidebar, cx| {
                    sidebar.expanded_settled.clear();
                    sidebar.settled_limits.clear();
                    sidebar.compact_model_dirty = true;
                    cx.notify();
                });
                draw(cx);
                let key = if layout == SidebarLayout::Flat {
                    "recent"
                } else {
                    "sample"
                };
                let selector = |id: &str| -> &'static str {
                    if compact {
                        format!("compact-row-{id}")
                    } else {
                        format!("sidebar-thread-{id}")
                    }
                    .leak()
                };
                let expected = |limit: usize| {
                    (0..8)
                        .map(|index| format!("active-{index}"))
                        .chain((0..limit).map(|index| format!("settled-{index}")))
                        .chain(std::iter::once("settled-39".into()))
                        .collect::<Vec<_>>()
                };
                let verify = |cx: &mut VisualTestContext, limit| {
                    let ids = sidebar.update(cx, |sidebar, cx| sidebar.navigation_threads(cx));
                    assert_eq!(ids, expected(limit), "{compact:?} {layout:?}");
                    let mut previous = None;
                    for id in ids {
                        let bounds = cx
                            .debug_bounds(selector(&id))
                            .unwrap_or_else(|| panic!("missing visible row {id}"));
                        if let Some(previous) = previous {
                            assert!(
                                bounds.top() > previous,
                                "row {id}: top {:?} <= previous {:?}; compact={compact} layout={layout:?} limit={limit}",
                                bounds.top(),
                                previous
                            );
                        }
                        previous = Some(bounds.top());
                    }
                };
                verify(cx, 0);
                assert!(cx.debug_bounds(selector("settled-0")).is_none());
                let header = cx
                    .debug_bounds(if key == "recent" {
                        "settled-recent"
                    } else {
                        "settled-sample"
                    })
                    .unwrap();
                cx.simulate_click(header.center(), gpui::Modifiers::default());
                draw(cx);
                verify(cx, 10);
                assert!(cx.debug_bounds(selector("settled-10")).is_none());
                let more = cx
                    .debug_bounds(if key == "recent" {
                        "settled-more-recent"
                    } else {
                        "settled-more-sample"
                    })
                    .unwrap();
                cx.simulate_click(more.center(), gpui::Modifiers::default());
                draw(cx);
                verify(cx, 35);
                assert!(cx.debug_bounds(selector("settled-35")).is_none());
                let header = cx
                    .debug_bounds(if key == "recent" {
                        "settled-recent"
                    } else {
                        "settled-sample"
                    })
                    .unwrap();
                cx.simulate_click(header.center(), gpui::Modifiers::default());
                draw(cx);
                verify(cx, 0);
                let header = cx
                    .debug_bounds(if key == "recent" {
                        "settled-recent"
                    } else {
                        "settled-sample"
                    })
                    .unwrap();
                cx.simulate_click(header.center(), gpui::Modifiers::default());
                draw(cx);
                verify(cx, 10);
                if layout == SidebarLayout::Grouped {
                    send(
                        Topic::Settings,
                        ServerEvent::SettingsSnapshot(tcode_core::settings::Settings {
                            sidebar_layout: layout,
                            collapsed_projects: vec!["sample".into()],
                            ..Default::default()
                        }),
                    );
                    cx.run_until_parked();
                    store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
                }
                sidebar.update(cx, |sidebar, cx| {
                    if !compact && layout == SidebarLayout::Flat {
                        sidebar.project_filter = Some("missing-project".into());
                    }
                    sidebar.compact_model_dirty = true;
                    cx.notify();
                });
                draw(cx);
                // Compact has its existing project-only scope; only desktop Flat
                // exposes the filter. Grouped collapse applies on both surfaces.
                if !compact || layout == SidebarLayout::Grouped {
                    assert!(
                        sidebar
                            .update(cx, |sidebar, cx| sidebar.navigation_threads(cx))
                            .is_empty()
                    );
                }
                if layout == SidebarLayout::Grouped {
                    send(
                        Topic::Settings,
                        ServerEvent::SettingsSnapshot(tcode_core::settings::Settings {
                            sidebar_layout: layout,
                            ..Default::default()
                        }),
                    );
                    cx.run_until_parked();
                    store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
                }
                sidebar.update(cx, |sidebar, cx| {
                    sidebar.project_filter = None;
                    sidebar.compact_model_dirty = true;
                    cx.notify();
                });
                draw(cx);
                if !compact && layout == SidebarLayout::Flat {
                    verify(cx, 0);
                    let header = cx.debug_bounds("settled-recent").unwrap();
                    cx.simulate_click(header.center(), gpui::Modifiers::default());
                    draw(cx);
                }
                verify(cx, 10);
            }
        }
    }

    /// Pinned rows lead in key order with keyless pins after them, new and
    /// reopened rows lead Active, and activity moves nothing. Dragging an
    /// active row between two pins pins it with a key between theirs; Escape
    /// cancels a drag; Move down writes only the moved key; unpinning or
    /// settling a pinned thread undoes with its old key.
    #[gpui::test]
    fn pinned_rows_lead_and_a_drag_or_undo_writes_their_keys(cx: &mut TestAppContext) {
        use tcode_protocol::{
            ClientPayload, CommandResponse, EventEnvelope, HostMessage, IndexSnapshot, ServerEvent,
            Topic, decode_client_line, encode_line,
        };
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
        cx.update(crate::theme::init);
        cx.update(|cx| cx.set_reduce_motion(true));
        let (to_host, outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let mut project = Project::from_root(PathBuf::from("/sample"));
        project.id = "sample".into();
        let thread = |id: &str, created: u64| {
            let mut meta = session(id, None);
            meta.project_id = Some("sample".into());
            meta.created_at = created;
            meta.updated_at = created;
            meta
        };
        let pinned = |id: &str, created: u64, key: Option<&str>| {
            let mut meta = thread(id, created);
            meta.pinned_at = Some(created);
            meta.pin_order = key.map(str::to_owned);
            meta
        };
        let mut reopened = thread("reopened", 10);
        reopened.unsettled_at = Some(99);
        let mut busy = thread("busy", 50);
        busy.updated_at = 1_000;
        let mut arranged = thread("arranged", 100);
        arranged.active_order = Some("m".into());
        let mut settled = thread("settled", 60);
        settled.settled_at = Some(70);
        let sessions = vec![
            settled,
            arranged,
            busy,
            reopened,
            thread("fresh", 95),
            pinned("pin-old", 80, None),
            pinned("pin-new", 90, None),
            pinned("pin-c", 3, Some("w")),
            pinned("pin-b", 1, Some("t")),
            pinned("pin-a", 2, Some("f")),
        ];
        for (topic, event) in [
            (
                Topic::Settings,
                ServerEvent::SettingsSnapshot(Default::default()),
            ),
            (
                Topic::Index,
                ServerEvent::IndexSnapshot(IndexSnapshot {
                    sessions,
                    projects: vec![project],
                    summary: Default::default(),
                }),
            ),
        ] {
            incoming
                .try_send(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic,
                        event,
                    }))
                    .unwrap(),
                )
                .unwrap();
        }
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        store.update(cx, |store, _| store.select_session("arranged".into()));
        // A test store blocks on each command's acknowledgement, so the host
        // side answers from its own threads: one pumps the link, one accepts
        // and records every lifecycle command.
        std::thread::spawn(move || smol::block_on(link.pump()));
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let host_recorded = recorded.clone();
        let host_incoming = incoming.clone();
        std::thread::spawn(move || {
            while let Ok(line) = outgoing.recv_blocking() {
                let message = decode_client_line(&line).unwrap();
                if let ClientPayload::Command(command) = message.payload {
                    if matches!(
                        command,
                        Command::PinSession { .. }
                            | Command::UnpinSession { .. }
                            | Command::ReorderPinned { .. }
                            | Command::ReorderActive { .. }
                            | Command::SettleSession { .. }
                            | Command::UnsettleSession { .. }
                    ) {
                        host_recorded.lock().unwrap().push(command);
                    }
                    let ack = HostMessage::Ack {
                        id: message.id,
                        result: Ok(CommandResponse::Unit),
                    };
                    let _ = host_incoming.send_blocking(encode_line(&ack).unwrap());
                }
            }
        });
        while store.read_with(cx, |store, _| store.flat_sessions().is_empty()) {
            std::thread::yield_now();
            store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        }
        let window_state = cx.new(|_| WindowState::new(false));
        let mut sidebar = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| SessionsSidebar::new(store.clone(), window_state.clone(), cx));
            sidebar = Some(view.clone());
            gpui_base::Root::new(view, window, cx)
        });
        let sidebar = sidebar.unwrap();
        cx.simulate_resize(size(px(393.), px(2000.)));
        // The lifecycle commands accepted since the last call.
        let sent = |cx: &mut VisualTestContext| {
            cx.run_until_parked();
            std::mem::take(&mut *recorded.lock().unwrap())
        };
        sent(cx);
        draw(cx);
        let order = [
            "pin-a", "pin-b", "pin-c", "pin-new", "pin-old", "reopened", "fresh", "busy",
            "arranged",
        ];
        for compact in [false, true] {
            window_state.update(cx, |state, cx| {
                state.compact = compact;
                cx.notify();
            });
            sidebar.update(cx, |sidebar, _| sidebar.compact_model_dirty = true);
            draw(cx);
            let ids = sidebar.update(cx, |sidebar, cx| sidebar.navigation_threads(cx));
            assert_eq!(ids, order, "compact={compact}");
            let mut previous = None;
            for id in order {
                let selector = if compact {
                    format!("compact-row-{id}")
                } else {
                    format!("sidebar-thread-{id}")
                };
                let top = cx.debug_bounds(selector.leak()).unwrap().top();
                assert!(previous.is_none_or(|previous| top > previous), "{id}");
                previous = Some(top);
            }
        }
        window_state.update(cx, |state, cx| {
            state.compact = false;
            cx.notify();
        });
        draw(cx);

        let left = gpui::MouseButton::Left;
        let none = gpui::Modifiers::default();
        let row = |cx: &mut VisualTestContext, id: &str| {
            cx.debug_bounds(format!("sidebar-thread-{id}").leak())
                .unwrap()
        };
        let start = row(cx, "fresh").center();
        cx.simulate_mouse_down(start, left, none);
        cx.simulate_mouse_move(start + gpui::point(px(0.), px(8.)), left, none);
        draw(cx);
        let target = row(cx, "pin-b");
        let over = gpui::point(target.center().x, target.top() + px(4.));
        cx.simulate_mouse_move(over, left, none);
        draw(cx);
        cx.simulate_mouse_up(over, left, none);
        let commands = sent(cx);
        let [
            Command::PinSession {
                session_id,
                order_key: Some(key),
            },
        ] = commands.as_slice()
        else {
            panic!("{commands:?}");
        };
        assert_eq!(session_id, "fresh");
        assert!("f" < key.as_str() && key.as_str() < "t", "{key}");

        let start = row(cx, "busy").center();
        cx.simulate_mouse_down(start, left, none);
        cx.simulate_mouse_move(start + gpui::point(px(0.), px(8.)), left, none);
        draw(cx);
        let target = row(cx, "pin-a");
        cx.simulate_mouse_move(target.center(), left, none);
        cx.simulate_keystrokes("escape");
        cx.simulate_mouse_up(target.center(), left, none);
        assert_eq!(sent(cx), vec![], "Escape cancels the drag");

        sidebar.update_in(cx, |sidebar, window, cx| {
            sidebar.on_unpin(&ThreadUnpin("pin-a".into()), window, cx)
        });
        assert_eq!(
            sent(cx),
            vec![Command::UnpinSession {
                session_id: "pin-a".into()
            }]
        );
        sidebar.update_in(cx, |sidebar, window, cx| sidebar.undo_lifecycle(window, cx));
        assert_eq!(
            sent(cx),
            vec![Command::PinSession {
                session_id: "pin-a".into(),
                order_key: Some("f".into()),
            }]
        );

        sidebar.update_in(cx, |sidebar, window, cx| {
            sidebar.on_move(&ThreadMove("pin-a".into(), true), window, cx)
        });
        let commands = sent(cx);
        let [
            Command::ReorderPinned {
                session_id,
                order_key,
            },
        ] = commands.as_slice()
        else {
            panic!("{commands:?}");
        };
        assert_eq!(session_id, "pin-a");
        assert!(
            "t" < order_key.as_str() && order_key.as_str() < "w",
            "{order_key}"
        );

        sidebar.update_in(cx, |sidebar, window, cx| {
            sidebar.settle_thread("pin-b", window, cx)
        });
        assert_eq!(
            sent(cx),
            vec![Command::SettleSession {
                session_id: "pin-b".into()
            }]
        );
        sidebar.update_in(cx, |sidebar, window, cx| sidebar.undo_lifecycle(window, cx));
        assert_eq!(
            sent(cx),
            vec![
                Command::UnsettleSession {
                    session_id: "pin-b".into()
                },
                Command::PinSession {
                    session_id: "pin-b".into(),
                    order_key: Some("t".into()),
                },
            ]
        );
    }
}
