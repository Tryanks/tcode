use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    rc::Rc,
};

use crate::material::{Bar, COMPACT_PAGE_INSET, bar, bar_height, empty_row, selectable_row};
use crate::motion::drag_ghost;
use crate::motion::list_rows::Glide;
use crate::overlay::{DialogButtons, Notification, OverlayExt as _};
use crate::remote::spaces::{self, ShareTarget, SpacesObserver};
use crate::theme::ActiveTheme as _;
use crate::widgets::TooltipTrigger as _;
use crate::widgets::button::{Button, ButtonVariant, ButtonVariants as _};
use crate::widgets::input::{Input, InputEvent, InputState};
use crate::widgets::menu::{ContextMenuExt as _, DropdownMenu as _};
use crate::widgets::scrollbar::Scrollbar;
use crate::widgets::spinner::Spinner;
use crate::widgets::tooltip::Tooltip;
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use gpui::{
    Action, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ListAlignment, ListState, ParentElement as _, Render, Role, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, list,
    prelude::FluentBuilder as _, px,
};
use gpui_base::motion::transition;
use gpui_base::{StyledExt as _, h_flex, v_flex};
use serde::Deserialize;
use tcode_core::thread_sort::{ThreadSection, ThreadSections, partition_threads};
use tcode_protocol::{Command, ThreadExportFormat};

use tcode_core::{
    project::{ProjectGroup, SessionMeta},
    settings::SidebarLayout,
};

use crate::store::{ForkAvailability, StoreChange, TopicKind, WorkspaceStore};
use crate::time::{humanize_ago, now_secs};
use crate::typography::TextRole as _;
use crate::window_drag_area;
use crate::window_state::{Destination, Route, WindowState};

mod arrange;
mod presence;
mod swipe;
pub(crate) use arrange::thread_carried;
use arrange::{DropZone, Landing, ThreadDrag};
use gpui_base::dock::AnyDrag;

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a row drawn here takes to fold in or out, or to move to a new place.
const ROWS: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

/// The provider mark behind a thread row: its provider's glyph at this alpha,
/// sized to the row height minus this vertical inset so it sits inside the row.
const PROVIDER_MARK_ALPHA: f32 = 0.32;
const PROVIDER_MARK_INSET: f32 = 8.;
/// A project's threads indent beneath its header's chevron and artwork;
/// `selectable_row` has no indent.
const THREAD_INDENT: f32 = 30.;

/// Rows a project shows in the project view before "Show more".
const THREADS_COLLAPSED_LIMIT: usize = 6;

fn thread_list_toggle_label(total: usize, expanded: bool) -> Option<Cow<'static, str>> {
    (total > THREADS_COLLAPSED_LIMIT).then(|| {
        if expanded {
            crate::tr!("sidebar.show_less")
        } else {
            crate::tr!("sidebar.show_more")
        }
    })
}

/// What a wide chat column shows, by the rows of its threads: an agent's is
/// its lead's. Empty wherever no conversation is on screen.
#[derive(Clone, Default, PartialEq)]
pub(crate) struct ShownThreads {
    pub(crate) current: Option<String>,
    pub(crate) shown: Vec<String>,
    /// The column's panes, in the layout's order, with two or more on screen.
    pub(crate) map: PaneMap,
}

pub(crate) type PaneMap = Rc<[MapCell]>;

/// A pane of the column.
#[derive(Clone, PartialEq)]
pub(crate) struct MapCell {
    pub(crate) pane: gpui::EntityId,
    /// The row of its thread; a pane waiting for one has none.
    pub(crate) row: Option<String>,
    /// Where it lies, as a share of the column.
    pub(crate) rect: gpui::Bounds<f32>,
}

impl MapCell {
    /// Where the pane lies, in a reader's words.
    fn place(&self) -> Cow<'static, str> {
        use std::cmp::Ordering;
        /// Whether `start..start + length` of the column lies toward its
        /// start, toward its end, or neither; `None` when it runs its whole
        /// length.
        fn side(start: f32, length: f32) -> Option<Ordering> {
            let centre = start + length / 2.;
            (length < 0.8).then_some(match centre {
                centre if centre < 0.4 => Ordering::Less,
                centre if centre > 0.6 => Ordering::Greater,
                _ => Ordering::Equal,
            })
        }
        let across = side(self.rect.origin.x, self.rect.size.width);
        let down = side(self.rect.origin.y, self.rect.size.height);
        match (across, down) {
            (Some(Ordering::Less), Some(Ordering::Less)) => crate::tr!("panes.place_top_left"),
            (Some(Ordering::Greater), Some(Ordering::Less)) => crate::tr!("panes.place_top_right"),
            (Some(Ordering::Less), Some(Ordering::Greater)) => {
                crate::tr!("panes.place_bottom_left")
            }
            (Some(Ordering::Greater), Some(Ordering::Greater)) => {
                crate::tr!("panes.place_bottom_right")
            }
            (Some(Ordering::Less), _) => crate::tr!("panes.place_left"),
            (Some(Ordering::Greater), _) => crate::tr!("panes.place_right"),
            (_, Some(Ordering::Less)) => crate::tr!("panes.place_top"),
            (_, Some(Ordering::Greater)) => crate::tr!("panes.place_bottom"),
            _ => crate::tr!("panes.place_middle"),
        }
    }
}

impl ShownThreads {
    /// The panes showing `row`'s thread.
    fn cells(&self, row: &str) -> impl Iterator<Item = &MapCell> {
        self.map
            .iter()
            .filter(move |cell| cell.row.as_deref() == Some(row))
    }

    fn light(&self, row: &str) -> RowLight {
        if self.current.as_deref() == Some(row) {
            RowLight::Current
        } else if self.shown.iter().any(|shown| shown == row) {
            RowLight::Shown
        } else {
            RowLight::None
        }
    }
}

/// How a thread's row says what the chat column does with its thread.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowLight {
    None,
    Shown,
    Current,
}

/// The gap between the parts of a desktop row (`selectable_row`'s).
const ROW_GAP: gpui::Pixels = px(6.);
/// How far the selected fill comes in on a row shown but not current, as
/// macOS unemphasizes a selection the keyboard is away from.
const SHOWN_FILL: f32 = 0.5;
/// The layout glyph: a row's map of the panes, as wide as tall as the column
/// tends to be.
const GLYPH_WIDTH: f32 = 14.;
const GLYPH_HEIGHT: f32 = 12.;

/// Each flat slot: the desktop two-line row and the 2pt gap reserved below it.
const FLAT_ROW_HEIGHT: f32 = crate::material::selectable_row_two_line_height(false) + 2.;

/// A status shown beside other row content: it fades in and out in place,
/// and a change from one value to another cross-fades.
fn faded_status<T: Clone + PartialEq + 'static>(
    id: impl Into<SharedString>,
    value: Option<T>,
    window: &mut Window,
    cx: &mut App,
    build: impl Fn(&T, &mut Window, &mut App) -> gpui::AnyElement,
) -> Option<gpui::AnyElement> {
    let id: SharedString = id.into();
    let shown = crate::motion::shown(id.clone(), value, SHOW, window, cx)?;
    let content = crate::motion::cross_fade(
        (gpui::ElementId::from(id), "swap"),
        shown.value.clone(),
        CHANGE,
        window,
        cx,
        build,
    );
    Some(shown.fade(content))
}

/// The disclosures of a sidebar list: a project's rows, a project's rows
/// past the first few, and a Settled shelf (by its shelf key).
#[derive(Clone, Copy)]
enum Fold {
    Project,
    More,
    Shelf,
}

fn fold_key(fold: Fold, id: &str) -> String {
    match fold {
        Fold::Project => format!("project:{id}"),
        Fold::More => format!("more:{id}"),
        Fold::Shelf => format!("shelf:{id}"),
    }
}

/// The rows of a fold that move; past them a moving fold leaves its rows out
/// of the list, so a fold of hundreds of rows lays out a screenful.
const FOLD_ROWS: usize = 12;

/// How far row `index` of a fold's `count` rows shows at the fold's
/// `progress`: the rows open one after another from the top, and shut from
/// the bottom. Negative for a row left out while the fold moves.
fn fold_factor(progress: f32, index: usize, count: usize) -> f32 {
    let count = count.min(FOLD_ROWS);
    if index >= count {
        return if progress >= 1. { 1. } else { -1. };
    }
    (progress * count as f32 - index as f32).clamp(0., 1.)
}

/// A row's factor inside two folds; a row either leaves out stays out.
fn nested_fold(outer: f32, inner: f32) -> f32 {
    if outer < 0. || inner < 0. {
        -1.
    } else {
        outer * inner
    }
}

/// A sidebar label that owns the remaining row width, on one line.
fn sidebar_label(text: impl Into<SharedString>) -> gpui::Div {
    div()
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .child(crate::widgets::fade_text(text))
}

/// Fold indicator for a collapsible section header, turned from pointing
/// right (shut) to down (open) as far as the fold's `progress`.
fn collapse_chevron(progress: f32, cx: &App) -> Icon {
    Icon::new(IconName::ChevronRight)
        .rotate(gpui::radians(std::f32::consts::FRAC_PI_2 * progress))
        .flex_none()
        .text_color(cx.theme().muted_foreground)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ThreadFlags {
    unread: bool,
    waiting_for_approval: bool,
    waiting_for_input: bool,
    working: bool,
    failed: bool,
    /// Pieces of background work and agents still running.
    background: (u32, u32),
    /// Dispatched agents that finished and await the thread's settle.
    unsettled: u32,
}

#[derive(Clone)]
struct ThreadRowState {
    session_id: String,
    row_key: String,
    waiting_for_approval: bool,
    waiting_for_input: bool,
    /// Pieces of background work and agents still running.
    background: (u32, u32),
    unsettled: u32,
    failed: bool,
    auto_settle_enabled: bool,
    is_worktree: bool,
    show_unread: bool,
    renaming: Option<Entity<InputState>>,
    menu_can_fork: bool,
    title_generating: bool,
    /// Numbers of the pull requests a watch follows for the thread.
    watching: Vec<u64>,
    /// When its next scheduled message is due (unix seconds).
    scheduled_at: Option<u64>,
}

impl ThreadRowState {
    fn waiting(&self) -> bool {
        self.waiting_for_approval || self.waiting_for_input
    }
}

/// What keeps a thread going, written out: how much of its background work
/// and how many of its agents run.
fn keeps_going(state: &ThreadRowState) -> Option<String> {
    let (tasks, agents) = state.background;
    let mut parts = Vec::new();
    if tasks > 0 {
        parts.push(crate::tr!("sidebar.keeps_background", count = tasks).into_owned());
    }
    if agents > 0 {
        parts.push(crate::tr!("sidebar.keeps_agents", count = agents).into_owned());
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// What wakes a thread later though nothing of it runs: an eye and the
/// number of each pull request a watch follows, and an hourglass and the
/// time its next scheduled message is due.
fn wake_marks(state: &ThreadRowState, cx: &App) -> Vec<gpui::AnyElement> {
    let muted = cx.theme().muted_foreground;
    let mark = |icon: IconName, text: String| {
        h_flex()
            .flex_none()
            .gap_0p5()
            .items_center()
            .text_color(muted)
            .child(Icon::new(icon).xsmall())
            .child(div().whitespace_nowrap().child(text))
            .into_any_element()
    };
    state
        .watching
        .iter()
        .map(|number| mark(IconName::Eye, format!("#{number}")))
        .chain(state.scheduled_at.map(|at| {
            mark(
                IconName::Hourglass,
                crate::chat::model::format_local_time(at.saturating_mul(1000)),
            )
        }))
        .collect()
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

/// A project's threads in the project view: the active ones (pinned
/// included) in the order the store gives them, and its Settled shelf newest
/// settled first.
fn project_group_threads(sessions: &[SessionMeta]) -> (Vec<&SessionMeta>, Vec<&SessionMeta>) {
    let (mut settled, active): (Vec<_>, Vec<_>) =
        sessions.iter().partition(|meta| meta.is_settled());
    settled.sort_by(|a, b| tcode_core::thread_sort::compare_threads(a, b));
    (active, settled)
}

/// The time a project-view row shows: its last update, or when it settled
/// for a row on the Settled shelf, which sorts by that.
fn project_view_timestamp(meta: &SessionMeta) -> u64 {
    if meta.is_settled() {
        tcode_core::thread_sort::shown_timestamp(meta)
    } else {
        meta.updated_at
    }
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
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadMoveToProject(String);

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
#[action(namespace = tcode_thread, no_json)]
struct ThreadFollowUp(String, bool);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadPark(String, bool);

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
    /// The Harnesses of the threads it covers, each once.
    harnesses: Vec<String>,
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
    window.open_alert_dialog(cx, move |alert, _, _| {
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
    /// The time the row shows, the one its list sorts it by.
    timestamp: u64,
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
    Thread(Box<SessionMeta>),
    /// The start of Pinned or Active, drawn only while dragging; whether the
    /// section is empty.
    Boundary(ThreadSection, bool),
    Settled,
    More(usize),
    Empty,
    /// What an empty list says in place of rows.
    Hint(Cow<'static, str>),
}

impl FlatListRow {
    fn key(&self) -> (&'static str, &str) {
        match self {
            Self::Thread(meta) => ("thread", &meta.id),
            Self::Boundary(ThreadSection::Pinned, _) => ("pinned", ""),
            Self::Boundary(_, _) => ("active", ""),
            Self::Settled => ("settled", ""),
            Self::More(_) => ("more", ""),
            Self::Empty => ("empty", ""),
            Self::Hint(_) => ("hint", ""),
        }
    }
}

/// What one frame of the Flat list drew its rows from, kept so a row that
/// leaves is drawn as it was.
#[derive(Default)]
struct FlatFrame {
    rows: Vec<FlatListRow>,
    folds: Vec<f32>,
    scope: Option<String>,
    settled_count: usize,
    project_names: HashMap<String, String>,
    flags: Rc<HashMap<String, ThreadFlags>>,
}

/// A row of the desktop grouped list. Project-scoped rows index into the
/// frame's `grouped_sessions`.
enum GroupedListRow {
    Project {
        group: usize,
        collapsed: bool,
    },
    Thread(Box<SessionMeta>),
    ToggleThreads {
        group: usize,
        expanded: bool,
        label: SharedString,
    },
    Settled {
        group: usize,
        count: usize,
    },
    SettledMore {
        group: usize,
        count: usize,
    },
    /// What the list says while it has no projects.
    Hint(Cow<'static, str>),
}

/// What one frame of the Grouped list drew its rows from, kept so a row
/// that leaves is drawn as it was.
#[derive(Default)]
struct GroupedFrame {
    rows: Vec<GroupedListRow>,
    folds: Vec<f32>,
    groups: Rc<Vec<ProjectGroup>>,
    flags: Rc<HashMap<String, ThreadFlags>>,
}

impl GroupedListRow {
    fn key<'a>(&'a self, groups: &'a [ProjectGroup]) -> (&'static str, &'a str) {
        let project = |group: &usize| groups[*group].project.id.as_str();
        match self {
            Self::Project { group, .. } => ("project", project(group)),
            Self::Thread(meta) => ("thread", &meta.id),
            Self::ToggleThreads { group, .. } => ("toggle", project(group)),
            Self::Settled { group, .. } => ("settled", project(group)),
            Self::SettledMore { group, .. } => ("settled-more", project(group)),
            Self::Hint(_) => ("hint", ""),
        }
    }
}

/// What the thread lists show of the store, derived once for each
/// [`WorkspaceStore::thread_list_revision`].
#[derive(Clone)]
struct ThreadLists {
    revision: u64,
    groups: Rc<Vec<ProjectGroup>>,
    flat: Rc<Vec<SessionMeta>>,
    flags: Rc<HashMap<String, ThreadFlags>>,
}

#[derive(Clone, Default)]
struct CompactListModel {
    rows: Vec<CompactListRow>,
    /// Each row's fold factor, as `SessionsSidebar::fold_row` takes it.
    folds: Vec<f32>,
    has_projects: bool,
    locale: String,
    minute: u64,
}

pub struct SessionsSidebar {
    store: Entity<WorkspaceStore>,
    window_state: Entity<WindowState>,
    /// Project ids whose thread list is expanded past the collapsed limit.
    expanded_groups: HashSet<String>,
    expanded_settled: HashSet<String>,
    settled_limits: HashMap<String, usize>,
    lifecycle_undo: Option<LifecycleUndo>,
    /// A thread pinned from its menu: Pinned leads the list, above where the
    /// user was looking, so the list scrolls to the row once it shows there.
    reveal_pinned: Option<String>,
    row_swipes: swipe::RowSwipes,
    drag: Option<ThreadDrag>,
    landing: Option<Landing>,
    /// The project the Arrange threads page shows; `None` is every project.
    arrange_scope: Option<String>,
    arrange_settled_expanded: bool,
    arrange_settled_fold: f32,
    arrange_list_state: ListState,
    arrange_row_keys: Vec<String>,
    arrange_flip: crate::motion::ListFlip,
    last_selected: Option<String>,
    /// What the chat column shows, as its shell last told the list.
    shown: ShownThreads,
    /// Optional project id filter for the session-local flat list.
    project_filter: Option<String>,
    settled_scope: Option<(SidebarLayout, Option<String>)>,
    /// The thread currently being renamed inline, if any.
    renaming: Option<RenameState>,
    /// The row or project header under the pointer, by its hover group; its
    /// hover controls fade in from this one value rather than a state per row.
    hover: crate::motion::ListHover,
    /// How far open each disclosure of the list on screen is this frame, by
    /// [`fold_key`]: a closing one keeps its rows until it has shut.
    folds: HashMap<String, f32>,
    /// Whether a disclosure moved this frame, bringing or taking its rows.
    folding: bool,
    /// The scope the disclosures were last sampled in.
    fold_scope: Option<String>,
    flat_list_state: ListState,
    /// The row keys `flat_list_state` was last spliced for.
    flat_row_keys: Vec<(&'static str, String)>,
    flat_flip: crate::motion::ListFlip,
    /// Flat rows coming and going, each leaving one drawn from the frame it
    /// was last drawn in.
    flat_presence: presence::RowPresence<(Rc<FlatFrame>, usize)>,
    flat_drawn: Rc<FlatFrame>,
    flat_scroll: crate::motion::ListScroll,
    grouped_list_state: ListState,
    /// The row keys `grouped_list_state` was last spliced for.
    grouped_row_keys: Vec<(&'static str, String)>,
    grouped_flip: crate::motion::ListFlip,
    grouped_presence: presence::RowPresence<(Rc<GroupedFrame>, usize)>,
    grouped_drawn: Rc<GroupedFrame>,
    thread_lists: Option<ThreadLists>,
    compact_list_state: ListState,
    compact_scroll: crate::motion::ListScroll,
    compact_model: Option<Rc<CompactListModel>>,
    compact_presence: presence::RowPresence<(Rc<CompactListModel>, usize, Option<swipe::Flight>)>,
    compact_flip: crate::motion::ListFlip,
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
                    background: store.activity(&meta.id).map_or((0, 0), |activity| {
                        (activity.background.tasks, activity.background.agents)
                    }),
                    unsettled: store
                        .activity(&meta.id)
                        .map_or(0, |activity| activity.unsettled),
                },
            )
        })
        .collect()
}

impl SessionsSidebar {
    fn thread_lists(&mut self, cx: &App) -> ThreadLists {
        let store = self.store.read(cx);
        let revision = store.thread_list_revision();
        if let Some(lists) = &self.thread_lists
            && lists.revision == revision
        {
            return lists.clone();
        }
        let lists = ThreadLists {
            revision,
            groups: Rc::new(store.grouped_sessions()),
            flat: Rc::new(store.flat_sessions()),
            flags: Rc::new(session_flags(store.sidebar_sessions(), store)),
        };
        self.thread_lists = Some(lists.clone());
        lists
    }

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
        } = self.arranged(scope, sections);
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

    /// The project view lists each project's active threads in the order the
    /// store gives them, then its Settled shelf; Pinned belongs to the threads
    /// view.
    fn grouped_rows(
        &self,
        groups: &[ProjectGroup],
        store: &WorkspaceStore,
    ) -> Vec<(GroupedListRow, f32)> {
        let mut rows = self.grouped_rows_folding(groups, store);
        rows.retain(|(_, factor)| *factor >= 0.);
        rows
    }

    /// [`Self::grouped_rows`] with the rows a moving disclosure leaves out of
    /// the list, at a negative factor.
    fn grouped_rows_folding(
        &self,
        groups: &[ProjectGroup],
        store: &WorkspaceStore,
    ) -> Vec<(GroupedListRow, f32)> {
        let mut rows = Vec::new();
        for (group, project) in groups.iter().enumerate() {
            let project_id = &project.project.id;
            let collapsed = store.is_project_collapsed(project_id);
            rows.push((GroupedListRow::Project { group, collapsed }, 1.));
            let project_key = fold_key(Fold::Project, project_id);
            if !self.fold_shown(&project_key, !collapsed) {
                continue;
            }
            let start = rows.len();
            let (active, mut settled) = project_group_threads(&project.sessions);
            let expanded = self.expanded_groups.contains(project_id);
            let more_key = fold_key(Fold::More, project_id);
            let shown = if self.fold_shown(&more_key, expanded) {
                active.len()
            } else {
                THREADS_COLLAPSED_LIMIT
            };
            let more = self.fold_progress(&more_key, expanded);
            let extra = active.len().saturating_sub(THREADS_COLLAPSED_LIMIT);
            rows.extend(active.iter().take(shown).enumerate().map(|(index, meta)| {
                let factor = match index.checked_sub(THREADS_COLLAPSED_LIMIT) {
                    Some(index) => fold_factor(more, index, extra),
                    None => 1.,
                };
                (GroupedListRow::Thread(Box::new((*meta).clone())), factor)
            }));
            if let Some(label) = thread_list_toggle_label(active.len(), expanded) {
                rows.push((
                    GroupedListRow::ToggleThreads {
                        group,
                        expanded,
                        label: label.into(),
                    },
                    1.,
                ));
            }
            if !settled.is_empty() {
                let count = settled.len();
                rows.push((GroupedListRow::Settled { group, count }, 1.));
                self.limit_settled_rows(project_id, &mut settled);
                let hidden = count - settled.len();
                let more = self.settled_more_count(project_id, hidden);
                let (factors, more_factor) =
                    self.shelf_factors(project_id, &settled, more.is_some());
                rows.extend(settled.into_iter().zip(factors).map(|(meta, factor)| {
                    (GroupedListRow::Thread(Box::new(meta.clone())), factor)
                }));
                if let Some(count) = more {
                    rows.push((GroupedListRow::SettledMore { group, count }, more_factor));
                }
            }
            let progress = self.fold_progress(&project_key, !collapsed);
            let count = rows.len() - start;
            for (index, (_, factor)) in rows[start..].iter_mut().enumerate() {
                *factor = nested_fold(fold_factor(progress, index, count), *factor);
            }
        }
        rows
    }

    /// Use the same filters, disclosures and ordering as the rendered list,
    /// including rows outside the scroll viewport.
    fn navigation_threads(&mut self, cx: &mut Context<Self>) -> Vec<String> {
        if self.compact(cx) {
            return self
                .compact_model(None, cx)
                .rows
                .iter()
                .filter_map(|row| match row {
                    CompactListRow::Thread(row) => Some(row.meta.id.clone()),
                    _ => None,
                })
                .collect();
        }

        let lists = self.thread_lists(cx);
        let store = self.store.read(cx);
        let mut ids = Vec::new();
        match store.sidebar_layout() {
            SidebarLayout::Flat => {
                let rows = self.thread_rows(&lists.flat, self.project_filter.as_deref(), "recent");
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
                    self.grouped_rows(&lists.groups, store)
                        .into_iter()
                        .filter_map(|(row, _)| match row {
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
        current: Option<&str>,
        cx: &mut Context<Self>,
    ) -> bool {
        use crate::shortcut::NavigateThread;

        let threads = self.navigation_threads(cx);
        if threads.is_empty() {
            return false;
        }
        let active = current.map(|id| self.store.read(cx).roster_row(id));
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
                store.show_thread(id.clone(), cx);
            });
            cx.notify();
            return true;
        }
        false
    }

    fn compact(&self, cx: &gpui::App) -> bool {
        self.window_state.read(cx).compact
    }

    /// How far `group`'s hover controls have faded in.
    fn hover_progress(&self, group: &str, window: &mut Window, cx: &mut App) -> f32 {
        self.hover.reveal(group, window, cx)
    }

    /// Whether `key`'s rows are in the list: open, or still shutting.
    fn fold_shown(&self, key: &str, open: bool) -> bool {
        open || self.fold_progress(key, open) > 0.
    }

    fn fold_progress(&self, key: &str, open: bool) -> f32 {
        self.folds
            .get(key)
            .copied()
            .unwrap_or(if open { 1. } else { 0. })
    }

    /// Sample every disclosure the list on screen has, so one that opens or
    /// shuts moves from where it was. Keyed by the list's scope, so a scope
    /// change that resets them does not animate.
    fn sample_folds(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let compact = self.compact(cx);
        let layout = self.store.read(cx).sidebar_layout();
        let groups = self.thread_lists(cx).groups;
        let mut targets = Vec::new();
        match layout {
            SidebarLayout::Flat => targets.push((
                fold_key(Fold::Shelf, "recent"),
                self.expanded_settled.contains("recent"),
            )),
            SidebarLayout::Grouped => {
                let headers = !compact || groups.len() > 1;
                for group in groups.iter() {
                    let id = group.project.id.as_str();
                    let collapsed = self.store.read(cx).is_project_collapsed(id)
                        && (headers || group.project.quick_chat);
                    targets.push((fold_key(Fold::Project, id), !collapsed));
                    targets.push((fold_key(Fold::More, id), self.expanded_groups.contains(id)));
                    targets.push((
                        fold_key(Fold::Shelf, id),
                        self.expanded_settled.contains(id),
                    ));
                }
            }
        }
        let scope = format!("{layout:?}-{:?}-{compact}", self.project_filter);
        let mut folds = HashMap::with_capacity(targets.len());
        let mut moving = false;
        for (key, open) in targets {
            let target = if open { 1. } else { 0. };
            let progress = transition(
                SharedString::from(format!("fold-{scope}-{key}")),
                target,
                gpui_base::motion::Transition::new(GLIDE),
                window,
                cx,
            );
            moving |= progress != target || self.folds.get(&key) != Some(&progress);
            folds.insert(key, progress);
        }
        self.folds = folds;
        // A new scope puts its disclosures where they rest at once: the list
        // it brings is replaced as a whole, its rows coming and going as the
        // list's own (`RowPresence`), not as a disclosure's.
        let rescoped = self.fold_scope.as_deref() != Some(scope.as_str());
        self.folding = moving && !rescoped;
        self.fold_scope = Some(scope);
        if moving {
            self.compact_model_dirty = true;
        }
    }

    /// A list row inside folds, at `factor` of its height: the row is drawn
    /// through [`crate::motion::fold`], clipped and fading with the height
    /// it has. Every row of a list goes through here, so a row keeps its
    /// element tree as its folds move. `id` names the row, not its index.
    fn fold_row(
        &mut self,
        id: impl Into<gpui::ElementId>,
        factor: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
        render: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) -> gpui::AnyElement,
    ) -> gpui::AnyElement {
        let row = render(self, window, cx);
        crate::motion::fold(id, factor.clamp(0., 1.), row)
    }

    /// The list's body: while threads load, the skeleton (or the rows still
    /// waiting for a connection); once loaded, `loaded`, fading in under the
    /// skeleton as it fades out.
    fn loading_body(
        &mut self,
        loaded: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) -> gpui::AnyElement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let loading = self.store.read(cx).threads_loading();
        let skeleton = loading && self.store.read(cx).pending_sessions().is_empty();
        let leaving = crate::motion::shown(
            "sidebar-loading-skeleton",
            skeleton.then_some(()),
            SHOW,
            window,
            cx,
        );
        let arriving = transition(
            ("sidebar-loaded", "opacity"),
            if loading { 0. } else { 1. },
            gpui_base::motion::Transition::new(GLIDE),
            window,
            cx,
        );
        if skeleton {
            return crate::material::loading_skeleton(cx);
        }
        if loading {
            return self.pending_thread_rows(window, cx);
        }
        v_flex()
            .flex_1()
            .min_h_0()
            .relative()
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .opacity(arriving)
                    .child(loaded(self, window, cx)),
            )
            .when_some(leaving, |body, leaving| {
                body.child(
                    div()
                        .absolute()
                        .inset_0()
                        .child(leaving.fade(crate::material::loading_skeleton(cx))),
                )
            })
            .into_any_element()
    }

    /// Track the pointer over `group`'s row, a positioned row that takes
    /// clicks, for [`Self::hover_progress`].
    fn track_hover<R: gpui::ParentElement>(&self, row: R, group: &str) -> R {
        row.child(self.hover.hitbox(group.to_owned()))
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
                    TopicKind::Index | TopicKind::Settings | TopicKind::SessionStatus
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
            expanded_groups: HashSet::new(),
            expanded_settled: HashSet::new(),
            settled_limits: HashMap::new(),
            lifecycle_undo: None,
            reveal_pinned: None,
            row_swipes: Default::default(),
            drag: None,
            landing: None,
            arrange_scope: None,
            arrange_settled_expanded: false,
            arrange_settled_fold: 0.,
            arrange_list_state: ListState::new(0, ListAlignment::Top, px(120.)),
            arrange_row_keys: Vec::new(),
            arrange_flip: Default::default(),
            last_selected: None,
            shown: ShownThreads::default(),
            project_filter: None,
            settled_scope: None,
            renaming: None,
            hover: crate::motion::ListHover::new(crate::material::HOVER),
            folds: HashMap::new(),
            folding: false,
            fold_scope: None,
            flat_list_state: ListState::new(0, ListAlignment::Top, px(120.)),
            flat_row_keys: Vec::new(),
            flat_flip: Default::default(),
            flat_presence: Default::default(),
            flat_drawn: Rc::default(),
            flat_scroll: Default::default(),
            grouped_list_state: ListState::new(0, ListAlignment::Top, px(120.)),
            grouped_row_keys: Vec::new(),
            grouped_flip: Default::default(),
            grouped_presence: Default::default(),
            grouped_drawn: Rc::default(),
            thread_lists: None,
            compact_list_state: ListState::new(0, ListAlignment::Top, px(120.)),
            compact_presence: Default::default(),
            compact_flip: Default::default(),
            compact_scroll: Default::default(),
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
        if !self.store.read(cx).is_project_collapsed(project_id) {
            self.expanded_groups.remove(project_id);
        }
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
                let store = self.store.read(cx);
                let provider = store
                    .sidebar_sessions()
                    .iter()
                    .find(|meta| meta.id == action.0)
                    .map_or("Harness", |meta| {
                        crate::settings::provider_label(meta.provider)
                    });
                Some(crate::tr!("sidebar.fork_unsupported", provider = provider).into_owned())
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

    fn on_move_to_project(
        &mut self,
        action: &ThreadMoveToProject,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        crate::move_thread_dialog::open(self.store.clone(), action.0.clone(), window, cx);
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
        let Some(cwd) = self
            .store
            .read(cx)
            .thread_meta(session_id)
            .map(|meta| meta.cwd.clone())
        else {
            return;
        };
        crate::thread_export::prompt_thread_export(
            self.store.clone(),
            session_id.to_owned(),
            cwd,
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

    fn on_follow_up(&mut self, action: &ThreadFollowUp, _: &mut Window, cx: &mut Context<Self>) {
        self.store.update(cx, |store, _| {
            store.set_follow_up(action.0.clone(), action.1)
        });
    }

    fn on_park(&mut self, action: &ThreadPark, _: &mut Window, cx: &mut Context<Self>) {
        self.store
            .update(cx, |store, _| store.set_parked(action.0.clone(), action.1));
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
                if crate::shell::pane_shows(session_id, cx) =>
            {
                Some(session_id.clone())
            }
            _ => None,
        };
        let store = self.store.clone();
        let sessions: Vec<String> = commands
            .iter()
            .filter_map(|command| command.session_id().map(str::to_owned))
            .collect();
        cx.spawn_in(window, async move |this, cx| {
            for command in commands {
                let settling = matches!(command, Command::SettleSession { .. });
                let pinning = matches!(command, Command::PinSession { .. });
                let swiped = match &command {
                    Command::SettleSession { session_id }
                    | Command::UnsettleSession { session_id } => Some(session_id.clone()),
                    _ => None,
                };
                let Ok(request) =
                    cx.update(|_, cx| store.update(cx, |store, cx| store.command(command, cx)))
                else {
                    return;
                };
                if let Err(error) = request.await {
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.lifecycle_answered(&sessions, false, cx);
                        if pinning {
                            this.reveal_pinned = None;
                        }
                        if let Some(id) = swiped {
                            this.row_swipe_refused(&id);
                        }
                        cx.notify();
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
            let _ = this.update(cx, |this, cx| this.lifecycle_answered(&sessions, true, cx));
            if let Some((kind, commands)) = undo {
                let _ = this.update_in(cx, |this, window, cx| {
                    let store = this.store.read(cx);
                    let harnesses = store
                        .sidebar_sessions()
                        .iter()
                        .filter(|meta| sessions.contains(&meta.id))
                        .map(|meta| store.harness_name_of(meta))
                        .collect();
                    this.push_lifecycle_undo(
                        kind,
                        UndoEntry { commands, reopen },
                        harnesses,
                        window,
                        cx,
                    );
                });
            }
        })
        .detach();
    }

    fn push_lifecycle_undo(
        &mut self,
        kind: UndoKind,
        entry: UndoEntry,
        harnesses: Vec<String>,
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
                harnesses: vec![],
            });
        }
        let undo = self.lifecycle_undo.as_mut().unwrap();
        undo.entries.push(entry);
        for harness in harnesses {
            if !undo.harnesses.contains(&harness) {
                undo.harnesses.push(harness);
            }
        }
        let count = undo.entries.len();
        let named = if undo.harnesses.is_empty() {
            "Harness".to_owned()
        } else {
            undo.harnesses.join(", ")
        };
        let title = match (kind, count) {
            (UndoKind::Settle, 1) => crate::tr!("sidebar.undo_settled_one", harness = named),
            (UndoKind::Unpin, 1) => crate::tr!("sidebar.undo_unpinned_one"),
            (UndoKind::Archive, 1) => crate::tr!("sidebar.undo_archived_one"),
            (UndoKind::Settle, _) => {
                crate::tr!("sidebar.undo_settled", count = count, harnesses = named)
            }
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
                    store.update(cx, |store, cx| store.put_back_thread(id, cx));
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
        let selected = self.current_row(cx);
        if selected == self.last_selected {
            return;
        }
        let meta = selected
            .as_deref()
            .and_then(|id| self.store.read(cx).thread_meta(id));
        if selected.is_some() && meta.is_none() {
            return;
        }
        self.last_selected = selected;
        let Some(meta) = meta else {
            return;
        };
        if meta.is_settled()
            && let Some(project_id) = meta.project_id.clone()
        {
            // The project view's shelf opens; the threads view shows the
            // viewed row under its closed shelf instead.
            if self.expanded_settled.insert(project_id.clone()) {
                self.settled_limits.remove(&project_id);
            }
            if self.store.read(cx).is_project_collapsed(&project_id) {
                self.store.update(cx, |store, cx| {
                    store.toggle_project_collapsed(project_id, cx)
                });
            }
        }
        self.compact_model_dirty = true;
    }

    /// `row` is where the list shows the thread pinned from its menu, and
    /// whether it shows there in Pinned yet: the host's change arrives after
    /// the menu closes. A list that does not show it forgets it.
    fn reveal_pinned_row(
        &mut self,
        list: &ListState,
        scroll: &crate::motion::ListScroll,
        row: Option<(usize, bool)>,
        cx: &App,
    ) {
        match row {
            Some((index, true)) => {
                scroll.reveal(list, index, cx);
                self.reveal_pinned = None;
            }
            Some((_, false)) => {}
            None => self.reveal_pinned = None,
        }
    }

    fn limit_settled_rows(&self, key: &str, rows: &mut Vec<&SessionMeta>) {
        let open = self.expanded_settled.contains(key);
        let limit = if self.fold_shown(&fold_key(Fold::Shelf, key), open) {
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

    /// The fold factor of each row a Settled shelf shows, and of its "Show
    /// more" row after them. The selected row a shut shelf keeps stays put.
    fn shelf_factors(&self, key: &str, rows: &[&SessionMeta], more: bool) -> (Vec<f32>, f32) {
        let open = self.expanded_settled.contains(key);
        let progress = self.fold_progress(&fold_key(Fold::Shelf, key), open);
        let selected = self.last_selected.as_deref();
        let count = rows
            .iter()
            .filter(|meta| selected != Some(meta.id.as_str()))
            .count()
            + usize::from(more);
        let mut index = 0;
        let factors = rows
            .iter()
            .map(|meta| {
                if selected == Some(meta.id.as_str()) {
                    return 1.;
                }
                index += 1;
                fold_factor(progress, index - 1, count)
            })
            .collect();
        (factors, fold_factor(progress, index, count))
    }

    fn settled_more_count(&self, key: &str, hidden: usize) -> Option<usize> {
        let open = self.expanded_settled.contains(key);
        (self.fold_shown(&fold_key(Fold::Shelf, key), open) && hidden > 0).then(|| hidden.min(25))
    }

    fn render_settled_more(
        &self,
        key: &str,
        count: usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let key = key.to_owned();
        let compact = self.compact(cx);
        // A full-width row with its label at the leading edge: `Button`
        // centres its content.
        selectable_row(
            crate::material::accessible_clickable(
                h_flex(),
                SharedString::from(format!("settled-more-{key}")),
                Role::Button,
                crate::tr!("sidebar.show_more_settled", count = count),
                cx,
            ),
            false,
            compact,
            cx,
        )
        .debug_selector({
            let key = key.clone();
            move || format!("settled-more-{key}")
        })
        .w_full()
        .text_secondary(compact, cx)
        .cursor_pointer()
        .child(Icon::new(IconName::Plus))
        .child(crate::tr!("sidebar.show_more_settled", count = count))
        .on_click(cx.listener(move |this, _, _, cx| {
            *this.settled_limits.entry(key.clone()).or_insert(10) += 25;
            this.compact_model_dirty = true;
            cx.notify();
        }))
        .into_any_element()
    }

    fn render_active_empty(&self, cx: &App) -> gpui::AnyElement {
        empty_row(crate::tr!("sidebar.active_empty"), cx).into_any_element()
    }

    fn render_settled_header(
        &self,
        key: &str,
        count: usize,
        scope: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let expanded = self.expanded_settled.contains(key);
        let color = match &self.drag {
            None => cx.theme().muted_foreground,
            Some(drag) if drag.target(scope) == Some(ThreadSection::Settled) => cx.theme().primary,
            Some(_) => cx.theme().sidebar_foreground,
        };
        // The caption lights up as a drag takes it as its target.
        let color = transition(
            (SharedString::from(format!("settled-{key}")), "color"),
            color,
            gpui_base::motion::Transition::new(GLIDE),
            window,
            cx,
        );
        let turned = self.fold_progress(&fold_key(Fold::Shelf, key), expanded);
        let key = key.to_string();
        let compact = self.compact(cx);
        let spacing = &cx.theme().tokens.spacing;
        let inset = if compact {
            px(COMPACT_PAGE_INSET)
        } else {
            spacing.sm
        };
        // A disclosure across a whole section bar, at the bar's height and
        // inset. Not a `bar`: gpui-base's `Toolbar` fixes the toolbar role,
        // and its `Collapsible` has no trigger.
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
        .h(px(bar_height(Bar::Section, compact)))
        .px(inset)
        .gap(spacing.sm)
        .text_caption(cx)
        .text_color(color)
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, _, cx| {
            // A shelf shuts with the rows it showed and opens at its first page.
            if !this.expanded_settled.remove(&key) {
                this.settled_limits.remove(&key);
                this.expanded_settled.insert(key.clone());
            }
            this.compact_model_dirty = true;
            cx.notify();
        }))
        .child(collapse_chevron(turned, cx).text_color(color))
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
        window.open_alert_dialog(cx, move |alert, _, _| {
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
    /// The badge of a project shared in a space; it fades in as the
    /// project is shared and out as it stops being.
    fn shared_badge(
        &self,
        project_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let names = spaces::for_store(&self.store, cx)
            .map(|spaces| spaces.read(cx).sharing(project_id))
            .filter(|names| !names.is_empty());
        let shown = crate::motion::shown(
            SharedString::from(format!("shared-badge-{project_id}")),
            names,
            SHOW,
            window,
            cx,
        )?;
        Some(shown.fade(spaces::shared_badge(project_id, &shown.value, cx)))
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
        window.open_alert_dialog(cx, move |alert, _, _| {
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
        let traffic_lights = crate::window_caption::traffic_light_inset(window);
        window_drag_area(
            "sidebar-app-row-drag",
            bar("sidebar-app-row", Bar::Strip, false, cx)
                .when(traffic_lights > px(0.), |strip| strip.pl(traffic_lights)),
            window,
            cx,
        )
        .child(crate::material::brand_wordmark(cx))
        // The collapse toggle lives in the chat header (`crate::chat`), not
        // here: collapsing takes the sidebar to zero width, so a control that
        // rode the sidebar would take itself off screen.
        .child(div().flex_1())
    }

    fn render_search_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let shortcut = crate::widgets::kbd::Kbd::new(gpui::Keystroke {
            modifiers: gpui::Modifiers::secondary_key(cx),
            key: "k".into(),
            key_char: None,
        });
        div().flex_none().px_2().pb_1().child(
            self.search_field(
                "sidebar-search",
                crate::tr!("sidebar.search").into(),
                false,
                cx,
            )
            .child(div().flex_none().child(shortcut)),
        )
    }

    /// The search control at the head of either sidebar. It opens the
    /// palette rather than taking text, so it is a button drawn as a quiet
    /// search field: an input's height and radius in the muted fill, with no
    /// border to set it apart from the rows around it, and the search glyph
    /// and label in the placeholder tone. A pointer brings the rows' hover
    /// fill in over it; a touch layout, which has no pointer, dims it while
    /// pressed.
    fn search_field(
        &self,
        id: &'static str,
        label: SharedString,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> crate::material::Clickable<gpui::Div> {
        let theme = cx.theme();
        let (fill, hover, placeholder) = (theme.muted, theme.list_hover, theme.muted_foreground);
        let gap = theme.tokens.spacing.sm;
        let radius = crate::material::radius_input(cx);
        crate::material::accessible_clickable(h_flex(), id, Role::Button, label.clone(), cx)
            .debug_selector(move || id.into())
            .relative()
            .h(px(crate::material::input_height(compact)))
            .px(crate::material::selectable_row_inset(compact))
            .gap(gap)
            .items_center()
            .rounded(radius)
            .bg(fill)
            .text_body(compact, cx)
            .text_color(placeholder)
            .cursor_pointer()
            .when(compact, |field| field.active(|s| s.opacity(0.8)))
            .child(crate::motion::hover_layer(
                None,
                hover.opacity(0.),
                hover,
                radius,
                crate::material::HOVER.into(),
            ))
            .on_click(cx.listener(|this, _, _, cx| {
                this.window_state
                    .update(cx, |state, cx| state.open_palette(cx));
            }))
            .child(Icon::new(IconName::Search).flex_none())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .child(crate::widgets::fade_text(label)),
            )
    }

    /// The machine this window is on, as the control that leads to Machines.
    /// Compact has none; Machines is the root its Back returns to.
    fn render_machine_control(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.window_state.read(cx).route() == Route::Hosts;
        let store = self.store.read(cx);
        let connection_state = store.connection_state();
        let dot = cx.theme().connection_color(&connection_state);

        // Local, the control says "This machine" and the tooltip its name;
        // remote, the control names the machine and the tooltip the link.
        let (name, detail): (SharedString, Option<SharedString>) = match store.remote_host_name() {
            Some(_) => (
                store.machine_label().into(),
                Some(crate::remote::connection_label(&connection_state).into()),
            ),
            None => (
                crate::tr!("hosts.this_computer").into_owned().into(),
                crate::remote::local_machine_name(cx).map(SharedString::from),
            ),
        };
        let dot = transition(
            ("sidebar-hosts", "connection"),
            dot,
            gpui_base::motion::Transition::new(GLIDE),
            window,
            cx,
        );
        let tooltip_name = name.clone();
        let control = Button::new("sidebar-hosts")
            .ghost()
            .selected(active)
            .w_full()
            .aria_label(crate::tr!("hosts.title"))
            .debug_selector(|| "sidebar-feature-hosts".into())
            // A toggle: on Machines (or Pair over it) it closes the route,
            // back to the workspace the window was showing.
            .on_click(cx.listener(|this, _, _, cx| {
                this.window_state.update(cx, |state, cx| {
                    if state.route() == Route::Hosts {
                        state.back(cx);
                    } else {
                        state.go(Destination::Hosts, cx);
                    }
                });
            }))
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap(cx.theme().tokens.spacing.sm)
                    .items_center()
                    .text_body(false, cx)
                    .child(crate::material::status_dot(dot))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .child(crate::widgets::fade_text(name)),
                    )
                    .child(
                        Icon::new(IconName::ChevronRight)
                            .flex_none()
                            .text_color(cx.theme().muted_foreground),
                    ),
            )
            // `Button::tooltip` takes text; this one has two lines.
            .map(|control| {
                control.with_tooltip(move |_, _| {
                    let (name, detail) = (tooltip_name.clone(), detail.clone());
                    Tooltip::element(move |_, cx| {
                        v_flex()
                            .child(name.clone())
                            .children(detail.clone().map(|detail| {
                                div().text_color(cx.theme().muted_foreground).child(detail)
                            }))
                    })
                })
            });
        div().flex_none().px_2().pb_1().child(control)
    }

    fn render_layout_toggle(&self, layout: SidebarLayout, cx: &mut Context<Self>) -> Button {
        let tooltip = match layout {
            SidebarLayout::Flat => crate::tr!("sidebar.layout_grouped"),
            SidebarLayout::Grouped => crate::tr!("sidebar.layout_flat"),
        };
        crate::material::toolbar_icon_button(
            "toggle-sidebar-layout",
            IconName::LayoutDashboard,
            tooltip.clone(),
            self.compact(cx),
        )
        .aria_label(tooltip)
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
        bar("sidebar-projects-header", Bar::Section, false, cx)
            .items_center()
            .justify_end()
            .child(
                h_flex()
                    .gap(cx.theme().tokens.spacing.xxs)
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

    fn render_flat_header(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let projects = self.store.read(cx).projects();
        let active_filter = self.project_filter.clone();
        let filter_icon_color = gpui_base::motion::transition(
            "sidebar-filter-icon",
            if active_filter.is_some() {
                cx.theme().primary
            } else {
                cx.theme().muted_foreground
            },
            gpui_base::motion::Transition::new(GLIDE),
            window,
            cx,
        );
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

        bar("sidebar-flat-header", Bar::Section, false, cx)
            .items_center()
            .justify_end()
            .child(
                h_flex()
                    .gap(cx.theme().tokens.spacing.xxs)
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
        window: &mut Window,
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

        let quick_chat = group.project.quick_chat;
        let name = crate::project_icon::group_label(&group.project);
        let header_label = if quick_chat {
            name.clone()
        } else {
            crate::tr!("sidebar.project", name = name.clone()).into_owned()
        };
        let new_thread_label: SharedString = if quick_chat {
            crate::tr!("sidebar.quick_chat")
        } else {
            crate::tr!("sidebar.create_thread")
        }
        .into();
        // The "+" fades in under the pointer, and as the keyboard comes onto
        // it.
        let plus_id = gpui::ElementId::from(SharedString::from(format!("new-thread-{project_id}")));
        let (plus_focus, plus_focused) =
            crate::motion::focus_ring::focus_reveal(&plus_id, true, window, cx);
        let hover = self
            .hover_progress(&group_key, window, cx)
            .max(plus_focused);
        // The dot shows while unread or fading out, and gives its slot to
        // the "+" under the pointer.
        let unread_dot = crate::motion::shown(
            SharedString::from(format!("project-unread-{project_id}")),
            has_unread.then_some(()),
            SHOW,
            window,
            cx,
        )
        .map(|shown| {
            shown.fade(
                div()
                    .opacity(1. - hover)
                    .child(div().size(px(6.)).rounded_full().bg(cx.theme().primary)),
            )
        });
        let header = selectable_row(
            crate::material::accessible_clickable(
                h_flex(),
                gpui::SharedString::from(format!("project-header-{project_id}")),
                Role::Button,
                header_label,
                cx,
            ),
            false,
            false,
            cx,
        )
        .aria_expanded(!collapsed)
        .debug_selector({
            let project_id = project_id.clone();
            move || {
                if quick_chat {
                    "quick-chats-header".into()
                } else {
                    format!("project-header-{project_id}")
                }
            }
        })
        .text_body(false, cx)
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, _, cx| {
            this.toggle_project(&header_toggle_id, cx);
        }))
        .child(collapse_chevron(
            self.fold_progress(&fold_key(Fold::Project, &project_id), !collapsed),
            cx,
        ))
        .child(crate::project_icon::artwork(
            self.store.read(cx).image_host(),
            &group.project,
            16.,
        ))
        .child(
            sidebar_label(name)
                .font_medium()
                .text_color(cx.theme().sidebar_foreground),
        )
        .children(self.shared_badge(&project_id, window, cx))
        // Unread dot when any child thread is unread.
        .children(unread_dot)
        .child(
            Button::new(plus_id)
                .track_focus(&plus_focus)
                .ghost()
                .xsmall()
                .icon(Icon::new(IconName::Plus).text_color(cx.theme().muted_foreground))
                .aria_label(new_thread_label.clone())
                .tooltip(new_thread_label)
                // Opacity keeps the button a tab stop, as the settle button does.
                .opacity(hover)
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    let cwd = plus_cwd.clone();
                    let project_id = plus_project_id.clone();
                    this.store.update(cx, |store, cx| {
                        if quick_chat {
                            store.start_scratch_draft(cx);
                        } else {
                            store.start_draft(project_id, cwd, cx);
                        }
                    });
                })),
        );
        let header = self.track_hover(header, &group_key);
        if quick_chat {
            return header.into_any_element();
        }
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
            .reveals(&self.hover, group_key)
            .into_any_element()
    }

    fn render_grouped_row(
        &self,
        index: usize,
        row: &GroupedListRow,
        groups: &[ProjectGroup],
        flags: &HashMap<String, ThreadFlags>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let item = v_flex().w_full().px_2().when(
            index > 0 && matches!(row, GroupedListRow::Project { .. }),
            |item| item.pt(px(2.)),
        );
        match row {
            GroupedListRow::Project { group, collapsed } => item
                .child(self.render_group_header(&groups[*group], flags, *collapsed, window, cx))
                .into_any_element(),
            GroupedListRow::Thread(meta) => item
                .child({
                    let light = self.shown.light(&meta.id);
                    let thread = self.render_thread(meta, flags, light, window, cx);
                    drag_ghost::slot(arrange::thread_slot(&meta.id), thread, window, cx)
                })
                .into_any_element(),
            GroupedListRow::ToggleThreads {
                group,
                expanded,
                label,
            } => {
                let project_id = groups[*group].project.id.clone();
                let toggle_id = project_id.clone();
                item.child(
                    div().w_full().pl(px(THREAD_INDENT)).child(
                        Button::new(SharedString::from(format!("show-more-{project_id}")))
                            .text()
                            .xsmall()
                            .label(label.clone())
                            .aria_expanded(*expanded)
                            .debug_selector(move || format!("show-more-{project_id}"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.expanded_groups.remove(&toggle_id) {
                                    this.expanded_groups.insert(toggle_id.clone());
                                }
                                cx.notify();
                            })),
                    ),
                )
                .into_any_element()
            }
            GroupedListRow::Settled { group, count } => item
                .child(self.render_settled_header(
                    &groups[*group].project.id,
                    *count,
                    None,
                    window,
                    cx,
                ))
                .into_any_element(),
            GroupedListRow::SettledMore { group, count } => item
                .child(self.render_settled_more(&groups[*group].project.id, *count, cx))
                .into_any_element(),
            GroupedListRow::Hint(hint) => item
                .pb_2()
                .child(empty_row(hint.clone(), cx))
                .into_any_element(),
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
            background: own_flags.background,
            unsettled: own_flags.unsettled,
            failed: own_flags.failed,
            auto_settle_enabled: meta.auto_settle_disabled_at.is_none(),
            is_worktree: meta.worktree.is_some(),
            show_unread: own_flags.unread && !own_flags.working,
            menu_can_fork: meta.provider.caps().supports_fork,
            title_generating: self.store.read(cx).title_generating(&meta.id),
            watching: self
                .store
                .read(cx)
                .activity(&meta.id)
                .map(|activity| activity.wakes.pull_requests.clone())
                .unwrap_or_default(),
            scheduled_at: self
                .store
                .read(cx)
                .activity(&meta.id)
                .and_then(|activity| activity.wakes.scheduled_at),
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

    /// What the chat column shows now, drawn from the next frame.
    pub(crate) fn set_shown(this: &Entity<Self>, shown: ShownThreads, cx: &mut App) {
        if this.read(cx).shown != shown {
            this.update(cx, |sidebar, cx| {
                sidebar.shown = shown;
                cx.notify();
            });
        }
    }

    /// The thread whose row the pointer is over.
    pub(crate) fn hovered_thread(&self) -> Option<SharedString> {
        let row = self.hover.hovered()?;
        let thread = row
            .strip_prefix("flat-thread-")
            .or_else(|| row.strip_prefix("thread-"))?;
        Some(thread.to_owned().into())
    }

    /// The map of the panes, on a row whose thread one shows: the column's
    /// panes in a tiny grid, each laid out as it lies, those showing this
    /// thread filled. It widens into the row as it comes, the title giving it
    /// room gradually, and narrows away as it goes.
    fn pane_glyph(&self, row: &str, window: &mut Window, cx: &mut App) -> Option<gpui::AnyElement> {
        let map = self.shown.cells(row).next().map(|_| self.shown.map.clone());
        let id = SharedString::from(format!("pane-glyph-{row}"));
        let shown = crate::motion::shown(id.clone(), map, SHOW, window, cx)?;
        let (filled, other) = {
            let theme = cx.theme();
            (
                theme.sidebar_foreground,
                theme.muted_foreground.opacity(0.35),
            )
        };
        let mut glyph = div()
            .relative()
            .flex_none()
            .w(px(GLYPH_WIDTH))
            .h(px(GLYPH_HEIGHT))
            .debug_selector({
                let id = id.clone();
                move || id.to_string()
            });
        for cell in shown.value.iter() {
            let fill = transition(
                (id.clone(), format!("cell-{}", cell.pane.as_u64())),
                if cell.row.as_deref() == Some(row) {
                    filled
                } else {
                    other
                },
                gpui_base::motion::Transition::new(GLIDE),
                window,
                cx,
            );
            // Each edge on a half point, a half point in from the grid
            // line, so neighbours stay apart at any share of the column.
            let edge = |share: f32, length: f32| (share * length * 2.).round() / 2.;
            let rect = cell.rect;
            let left = edge(rect.origin.x, GLYPH_WIDTH) + 0.5;
            let right = edge(rect.origin.x + rect.size.width, GLYPH_WIDTH) - 0.5;
            let top = edge(rect.origin.y, GLYPH_HEIGHT) + 0.5;
            let bottom = edge(rect.origin.y + rect.size.height, GLYPH_HEIGHT) - 0.5;
            glyph = glyph.child(
                div()
                    .absolute()
                    .left(px(left))
                    .top(px(top))
                    .w(px((right - left).max(0.)))
                    .h(px((bottom - top).max(0.)))
                    .rounded(px(1.))
                    .bg(fill),
            );
        }
        Some(shown.fold_across_gap(ROW_GAP, glyph))
    }

    /// The row of the current pane's thread.
    fn current_row(&self, cx: &App) -> Option<String> {
        let thread = crate::shell::current_thread(cx)?;
        Some(self.store.read(cx).roster_row(&thread))
    }

    /// `base` carries the row's selectable surface, selected when `light` is
    /// [`RowLight::Current`].
    fn thread_clickable_row(
        &self,
        base: gpui::Div,
        meta: &SessionMeta,
        state: &ThreadRowState,
        light: RowLight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> crate::material::Clickable<gpui::Div> {
        let session_id = state.session_id.clone();
        let row_id = SharedString::from(format!("{}-row", state.row_key));
        // The selected fill glides from the row left to the row chosen.
        let active = cx.theme().list_active;
        let fill = transition(
            (row_id.clone(), "selected-fill"),
            active.opacity(match light {
                RowLight::Current => 1.,
                RowLight::Shown => SHOWN_FILL,
                RowLight::None => 0.,
            }),
            gpui_base::motion::Transition::new(GLIDE),
            window,
            cx,
        );
        let places: Vec<_> = self.shown.cells(&meta.id).map(MapCell::place).collect();
        let label = if places.is_empty() {
            crate::tr!("sidebar.thread", title = meta.title.clone())
        } else {
            crate::tr!(
                "sidebar.thread_in_pane",
                title = meta.title.clone(),
                place = crate::i18n::join_list(places)
            )
        }
        .into_owned();
        let row =
            crate::material::accessible_clickable(base.bg(fill), row_id, Role::Button, label, cx)
                .text_body(false, cx)
                .aria_selected(light == RowLight::Current)
                .debug_selector({
                    let id = meta.id.clone();
                    move || format!("sidebar-thread-{id}")
                })
                .cursor_pointer()
                .on_click(
                    cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                        let session_id = session_id.clone();
                        if event.modifiers().alt && !this.compact(cx) {
                            window.dispatch_action(
                                Box::new(crate::panes::OpenInNewPane(session_id)),
                                cx,
                            );
                            return;
                        }
                        this.compact_model_dirty = true;
                        this.store.update(cx, |store, cx| {
                            store.show_thread(session_id.clone(), cx);
                        });
                        cx.notify();
                    }),
                );
        let row = self.track_hover(row, &state.row_key);
        let row = match self
            .dragged_thread(meta, state, cx)
            .filter(|_| !self.compact(cx))
        {
            Some(dragged) => Self::drag_source(row, dragged, cx),
            None => row,
        };
        row.when(state.waiting_for_approval, |row| {
            row.with_tooltip(|_, _| {
                Tooltip::new(crate::tr!("sidebar.waiting_approval_tooltip").into_owned())
            })
        })
        .when(
            state.waiting_for_input && !state.waiting_for_approval,
            |row| {
                row.with_tooltip(|_, _| {
                    Tooltip::new(crate::tr!("sidebar.waiting_input_tooltip").into_owned())
                })
            },
        )
    }

    fn rename_input(&self, input: &Entity<InputState>, cx: &mut Context<Self>) -> gpui::Div {
        div()
            .flex_1()
            .min_w_0()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.cancel_rename(cx)))
            .child(Input::new(input))
    }

    fn thread_title_or_input(
        &self,
        meta: &SessionMeta,
        state: &ThreadRowState,
        emphasize_unread: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let title = if let Some(input) = &state.renaming {
            self.rename_input(input, cx).into_any_element()
        } else {
            sidebar_label(meta.title.clone())
                .text_body(false, cx)
                .text_color(if meta.is_settled() {
                    cx.theme().muted_foreground
                } else {
                    cx.theme().sidebar_foreground
                })
                .when(emphasize_unread && state.show_unread, |title| {
                    title.font_semibold()
                })
                .into_any_element()
        };
        h_flex()
            .flex_1()
            .min_w_0()
            .gap(px(6.))
            .child(title)
            .when_some(
                Self::fading_spinner(
                    format!("title-spinner-{}", state.row_key),
                    state.title_generating,
                    window,
                    cx,
                ),
                |row, spinner| row.child(spinner),
            )
            .into_any_element()
    }

    /// The spinner beside a title while it generates, faded in and out.
    fn fading_spinner(
        id: String,
        spinning: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<gpui::AnyElement> {
        let shown = crate::motion::shown(id, spinning.then_some(()), SHOW, window, cx)?;
        Some(shown.fade(Spinner::new().color(cx.theme().muted_foreground)))
    }

    /// A pinned row's pin; on row hover it becomes the Unpin button.
    /// A thread on Follow-up: attention the user asked for, not activity.
    /// It widens into the row as it is set, and narrows away as it is
    /// cleared, so the title beside it gives way gradually.
    fn follow_up_glyph(
        meta: &SessionMeta,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<gpui::AnyElement> {
        let shown = crate::motion::shown(
            SharedString::from(format!("follow-up-shown-{}", meta.id)),
            meta.follow_up.is_some().then_some(()),
            SHOW,
            window,
            cx,
        )?;
        Some(
            shown.fold_across_gap(
                ROW_GAP,
                div()
                    .id(SharedString::from(format!("follow-up-{}", meta.id)))
                    .debug_selector({
                        let id = meta.id.clone();
                        move || format!("follow-up-{id}")
                    })
                    .flex_none()
                    .child(Icon::new(IconName::Bell).text_color(cx.theme().muted_foreground))
                    .with_tooltip(|_, _| Tooltip::new(crate::tr!("control.follow_up.marker"))),
            ),
        )
    }

    /// A thread's PR badge, widening in beside what precedes it (across
    /// `gap`) when a PR is linked and narrowing away when the last goes or
    /// the title is renamed over it.
    fn pr_badge(
        &self,
        meta: &SessionMeta,
        show: bool,
        gap: gpui::Pixels,
        slot: &'static str,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<gpui::AnyElement> {
        let links = show
            .then(|| meta.pull_requests.clone())
            .filter(|links| crate::pull_requests::badge_label(links, cx).is_some());
        let shown = crate::motion::shown(
            SharedString::from(format!("{slot}-{}", meta.id)),
            links,
            SHOW,
            window,
            cx,
        )?;
        let badge =
            crate::pull_requests::sidebar_badge(&shown.value, &meta.id, self.store.clone(), cx)?;
        Some(shown.fold_across_gap(gap, badge))
    }

    fn pin_glyph(
        &self,
        meta: &SessionMeta,
        row_key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Div> {
        if meta.pinned_at.is_none() || meta.is_settled() {
            return None;
        }
        let id = meta.id.clone();
        let label = crate::tr!("sidebar.unpin_tooltip");
        let button_id = gpui::ElementId::from(SharedString::from(format!("unpin-thread-{id}")));
        let (focus, focused) =
            crate::motion::focus_ring::focus_reveal(&button_id, true, window, cx);
        let hover = self.hover_progress(row_key, window, cx).max(focused);
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
                        .opacity(1. - hover)
                        .child(Icon::new(IconName::Pin).text_color(cx.theme().muted_foreground)),
                )
                .child(
                    Button::new(button_id)
                        .track_focus(&focus)
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(IconName::PinOff).text_color(cx.theme().muted_foreground))
                        .aria_label(label.clone())
                        .tooltip(label)
                        // As the settle button does, over the row's pointer.
                        .cursor_default()
                        .absolute()
                        .top_0()
                        .left_0()
                        // Opacity keeps the button a tab stop, as the settle button does.
                        .opacity(hover)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.unpin_thread(&id, window, cx);
                        })),
                ),
        )
    }

    /// Status text and colour shared by every thread row shape, and whether
    /// it is the background's, whose dot breathes as a call's window's does
    /// while its work goes on.
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
        } else if state.unsettled > 0 {
            Some((
                cx.theme().warning,
                crate::tr!("sidebar.unsettled", count = state.unsettled),
                false,
            ))
        } else if state.background != (0, 0) {
            Some((
                cx.theme().muted_foreground,
                crate::tr!(
                    "sidebar.background",
                    count = state.background.0 + state.background.1
                ),
                true,
            ))
        } else {
            None
        }
    }

    /// The row's status as a dot and its label; a change of status
    /// cross-fades, and a status that ends fades out in its place.
    fn thread_status_badge(
        state: &ThreadRowState,
        working: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let status = Self::thread_status_label(state, working, cx);
        // A one-line row has no second line to say what keeps the thread
        // going; its status says it on hover.
        let keeps = keeps_going(state);
        let badge = faded_status(
            format!("thread-status-{}", state.session_id),
            status.map(|(color, label, breathes)| (color, SharedString::from(label), breathes)),
            window,
            cx,
            |(color, label, breathes), _, cx| {
                h_flex()
                    .items_center()
                    .gap_1()
                    .child(if *breathes {
                        crate::widgets::BreathingDot::new(*color).into_any_element()
                    } else {
                        div()
                            .size(px(6.))
                            .rounded_full()
                            .bg(*color)
                            .into_any_element()
                    })
                    .child(
                        div()
                            .whitespace_nowrap()
                            .text_secondary(false, cx)
                            .text_color(*color)
                            .child(label.clone()),
                    )
                    .into_any_element()
            },
        )?;
        Some(
            h_flex()
                .id(SharedString::from(format!(
                    "thread-status-{}",
                    state.session_id
                )))
                .when_some(keeps, |badge, keeps| {
                    badge.with_tooltip(move |_, _| Tooltip::new(keeps.clone()))
                })
                .flex_none()
                .child(badge)
                .when(state.failed && !state.waiting(), |badge| {
                    badge.with_tooltip(|_, _| Tooltip::new(crate::tr!("sidebar.failed_tooltip")))
                })
                .into_any_element(),
        )
    }

    fn thread_context_menu(
        &self,
        row: crate::material::Clickable<gpui::Div>,
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
        let follow_up = meta.follow_up.is_some();
        let parked = meta.parked_at.is_some();
        let hover = self.hover.clone();
        let row_key = state.row_key.clone();
        let grant =
            crate::control::thread_grant_line(&self.store.read(cx).settings().control, meta);
        let menu = row
            .context_menu(move |menu, window, cx| {
                let id = session_id.clone();
                // Move up / Move down stay within the section the list shows.
                let moves = sidebar
                    .upgrade()
                    .filter(|_| !settled && scope.is_full())
                    .and_then(|sidebar| sidebar.read(cx).move_bounds(&id, cx));
                let open_beside = crate::shell::pane_controls(window, cx)
                    .map(|panes| panes.split_right || crate::shell::shown_in_pane(&id, cx));
                let menu =
                    menu.when_some(grant.clone(), |menu, grant| menu.label(grant).separator());
                menu.when_some(open_beside, |menu, enabled| {
                    menu.menu_with_enable(
                        crate::tr!("panes.open_beside").into_owned(),
                        Box::new(crate::panes::OpenInNewPane(id.clone())),
                        enabled,
                    )
                    .separator()
                })
                .menu(
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
                .when(!is_worktree, |menu| {
                    menu.menu(
                        crate::tr!("sidebar.ctx_move_thread").into_owned(),
                        Box::new(ThreadMoveToProject(id.clone())),
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
                .menu(
                    if follow_up {
                        crate::tr!("control.follow_up.remove")
                    } else {
                        crate::tr!("control.follow_up.add")
                    }
                    .into_owned(),
                    Box::new(ThreadFollowUp(id.clone(), !follow_up)),
                )
                .menu(
                    if parked {
                        crate::tr!("control.follow_up.unpark")
                    } else {
                        crate::tr!("control.follow_up.park")
                    }
                    .into_owned(),
                    Box::new(ThreadPark(id.clone(), !parked)),
                )
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
            .touch(compact);
        // A compact row reveals nothing.
        if compact {
            menu.into_any_element()
        } else {
            menu.reveals(&hover, row_key).into_any_element()
        }
    }

    fn render_thread(
        &self,
        meta: &SessionMeta,
        flags: &HashMap<String, ThreadFlags>,
        light: RowLight,
        window: &mut Window,
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
                selectable_row(h_flex(), light == RowLight::Current, false, cx),
                meta,
                &state,
                light,
                window,
                cx,
            )
            .pl(px(THREAD_INDENT))
            // Ahead of the row's content, which paints over the mark.
            .relative()
            .children(self.provider_mark(
                meta,
                crate::material::selectable_row_height(false),
                crate::material::selectable_row_inset(false).into(),
                cx,
            ))
            .when_some(
                Self::thread_status_badge(&state, working, window, cx),
                |row, badge| row.child(badge),
            )
            .children(wake_marks(&state, cx));

        // Row body: rename input, or the (unread dot + worktree glyph + title).
        let row = if state.renaming.is_some() {
            row.child(self.thread_title_or_input(meta, &state, false, window, cx))
        } else {
            let unread = crate::motion::shown(
                SharedString::from(format!("unread-{session_id}")),
                show_unread.then_some(()),
                SHOW,
                window,
                cx,
            )
            .map(|shown| shown.fade(div().size(px(6.)).rounded_full().bg(cx.theme().primary)));
            row.children(unread)
                .when(is_worktree, |row| {
                    row.child(
                        Icon::empty()
                            .path("icons/git-branch.svg")
                            .text_color(cx.theme().muted_foreground),
                    )
                })
                .child(self.thread_title_or_input(meta, &state, false, window, cx))
                .children(Self::follow_up_glyph(meta, window, cx))
                .children(self.pin_glyph(meta, &row_key, window, cx))
                .children(self.pr_badge(
                    meta,
                    state.renaming.is_none(),
                    ROW_GAP,
                    "pr-badge-row",
                    window,
                    cx,
                ))
                .children(self.pane_glyph(&meta.id, window, cx))
                // The time narrows away while the thread works, giving the
                // title its width gradually, and widens back after.
                .when_some(
                    crate::motion::shown(
                        SharedString::from(format!("thread-time-{session_id}")),
                        (!working).then_some(()),
                        SHOW,
                        window,
                        cx,
                    ),
                    |row, shown| {
                        let slot = self.render_flat_thread_right_slot(
                            meta,
                            &row_key,
                            project_view_timestamp(meta),
                            true,
                            window,
                            cx,
                        );
                        row.child(shown.fold_across_gap(ROW_GAP, slot))
                    },
                )
        };

        self.thread_context_menu(row, meta, &state, working, false, cx)
    }

    /// `timestamp` is the time the row shows: the one its list sorts it by.
    fn render_flat_thread_right_slot(
        &self,
        meta: &SessionMeta,
        row_key: &str,
        timestamp: u64,
        action_on_hover: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let session_id = meta.id.clone();
        let action_id = session_id.clone();
        let settled = meta.is_settled();
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
        let button_id = gpui::ElementId::from(SharedString::from(format!(
            "settle-flat-thread-{session_id}"
        )));
        let (focus, focused) =
            crate::motion::focus_ring::focus_reveal(&button_id, true, window, cx);
        let hover = if action_on_hover {
            self.hover_progress(row_key, window, cx).max(focused)
        } else {
            0.
        };
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
                    .text_secondary(false, cx)
                    .opacity(1. - hover)
                    .child(ago),
            )
            .when(action_on_hover, |slot| {
                slot.child(
                    Button::new(button_id)
                        .track_focus(&focus)
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
                        // The row's pointer covers its children; the button
                        // keeps Button's own cursor, the default.
                        .cursor_default()
                        .absolute()
                        .right_0()
                        .top_0()
                        // Opacity, not visibility, keeps the button a tab stop so
                        // keyboard focus can reveal it; a click's focus must not
                        // keep it over the time once the row moves.
                        .opacity(hover)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            if settled {
                                this.on_make_active(
                                    &ThreadMakeActive(action_id.clone()),
                                    window,
                                    cx,
                                );
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
        light: RowLight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let working = flags.get(&meta.id).is_some_and(|flags| flags.working);
        let state = self.thread_row_state(meta, flags, format!("flat-thread-{}", meta.id), cx);
        let session_id = state.session_id.clone();
        let row_key = state.row_key.clone();
        let show_unread = state.show_unread;
        let renaming = state.renaming.is_some();
        let status = Self::thread_status_label(&state, working, cx);
        let keeps = keeps_going(&state);
        let wakes = wake_marks(&state, cx);

        let row = self
            .thread_clickable_row(
                crate::material::selectable_row_two_line(
                    v_flex(),
                    light == RowLight::Current,
                    false,
                    cx,
                ),
                meta,
                &state,
                light,
                window,
                cx,
            )
            .debug_selector({
                let id = session_id.clone();
                move || format!("sidebar-thread-{id}")
            })
            .items_stretch()
            .justify_center()
            // Ahead of the row's content, which paints over the mark.
            .relative()
            .children(self.provider_mark(
                meta,
                crate::material::selectable_row_two_line_height(false),
                crate::material::selectable_row_inset(false).into(),
                cx,
            ));

        let row = {
            let title_or_input = self.thread_title_or_input(meta, &state, true, window, cx);
            // Unread wins over the status colour; renaming hides the dot so the
            // input keeps the row's leading width (same as render_thread).
            let dot = (!renaming)
                .then(|| {
                    show_unread
                        .then(|| cx.theme().primary)
                        .or(status.as_ref().map(|(color, _, _)| *color))
                })
                .flatten();
            let dot = faded_status(
                format!("flat-dot-{session_id}"),
                dot,
                window,
                cx,
                |color, _, _| {
                    div()
                        .size(px(6.))
                        .rounded_full()
                        .bg(*color)
                        .into_any_element()
                },
            );
            let line_one = h_flex()
                .w_full()
                .min_w_0()
                .items_center()
                .gap(px(6.))
                .children(dot)
                .child(title_or_input)
                .when(!renaming, |line| {
                    line.children(Self::follow_up_glyph(meta, window, cx))
                        .children(self.pin_glyph(meta, &row_key, window, cx))
                        .children(self.pane_glyph(&meta.id, window, cx))
                        .child(self.render_flat_thread_right_slot(
                            meta,
                            &row_key,
                            tcode_core::thread_sort::shown_timestamp(meta),
                            !working,
                            window,
                            cx,
                        ))
                });

            let has_project = project_name.is_some();
            let has_status = status.is_some();
            let status_line = faded_status(
                format!("flat-status-{session_id}"),
                status.map(|(color, label, _)| (SharedString::from(label), color)),
                window,
                cx,
                |(label, color), _, _| {
                    div()
                        .whitespace_nowrap()
                        .text_color(*color)
                        .child(label.clone())
                        .into_any_element()
                },
            );
            let line_two = h_flex()
                .w_full()
                .min_w_0()
                .items_center()
                .gap_1()
                .text_secondary(false, cx)
                .when_some(status_line, |line, status| {
                    line.child(
                        div()
                            .id(SharedString::from(format!("thread-status-{session_id}")))
                            .flex_none()
                            .child(status),
                    )
                })
                .when_some(keeps, |line, keeps| {
                    line.child(div().flex_none().child("·"))
                        .child(div().flex_none().whitespace_nowrap().child(keeps))
                })
                .children(wakes)
                .when(has_status && has_project, |line| {
                    line.child(div().flex_none().child("·"))
                })
                .when_some(project_name, |line, project_name| {
                    line.child(
                        self.store
                            .read(cx)
                            .project(meta.project_id.as_deref().unwrap_or_default())
                            .map(|project| {
                                crate::project_icon::artwork(
                                    self.store.read(cx).image_host(),
                                    project,
                                    12.,
                                )
                                .into_any_element()
                            })
                            .unwrap_or_else(|| Icon::new(IconName::Folder).into_any_element()),
                    )
                    .child(sidebar_label(project_name))
                })
                // Without a project label there is no flex-1 element on the
                // line, so a spacer keeps the worktree glyph bottom-right.
                .when(!has_project, |line| line.child(div().flex_1()))
                .when(meta.worktree.is_some(), |line| {
                    line.child(
                        Icon::empty()
                            .path("icons/git-branch.svg")
                            .text_color(cx.theme().muted_foreground),
                    )
                });
            let line_two = line_two.children(self.pr_badge(
                meta,
                !renaming,
                px(4.),
                "pr-badge-line",
                window,
                cx,
            ));
            row.child(line_one).child(line_two)
        };

        self.thread_context_menu(row, meta, &state, working, false, cx)
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        bar("sidebar-footer", Bar::Toolbar, false, cx).child(
            selectable_row(
                crate::material::accessible_clickable(
                    h_flex(),
                    "sidebar-settings",
                    Role::Button,
                    crate::tr!("settings.title"),
                    cx,
                ),
                false,
                false,
                cx,
            )
            .flex_1()
            .text_body(false, cx)
            .text_color(cx.theme().sidebar_foreground)
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| {
                this.window_state
                    .update(cx, |state, cx| state.open_settings(cx));
            }))
            .child(Icon::new(IconName::Settings).text_color(cx.theme().muted_foreground))
            .child(crate::tr!("settings.title")),
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
    window.open_alert_dialog(cx, move |alert, _, _| {
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
    window.open_alert_dialog(cx, move |alert, _, _| {
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

/// A compact thread row as [`SessionsSidebar::render_compact_thread`] lays it
/// out — a title and a second line inside `list_row`'s vertical inset, at
/// least `list_row`'s height — and the hairline below it.
fn compact_thread_row_height(cx: &App) -> f32 {
    let theme = cx.theme();
    let (typography, spacing) = (&theme.tokens.typography, &theme.tokens.spacing);
    let lines = typography.md.line_height + spacing.xxs + typography.sm.line_height;
    f32::from(lines + spacing.sm * 2.).max(crate::material::LIST_ROW_MIN_HEIGHT) + 1.
}

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
/// top keeps its place while it stays among the rows it was between, so a row
/// added or removed above the viewport moves nothing on screen; when that row
/// itself moved elsewhere (activity lifted it) or vanished, the list keeps its
/// index instead of following it.
fn replace_list_rows<K: PartialEq>(
    state: &ListState,
    previous_key: impl Fn(usize) -> Option<K>,
    keys: impl ExactSizeIterator<Item = K>,
) {
    replace_rows(state, previous_key, keys.collect());
}

/// [`replace_list_rows`] for a list whose rows glide from where they were
/// drawn to their new places.
fn replace_gliding_rows<K: PartialEq>(
    state: &ListState,
    flip: &crate::motion::ListFlip,
    previous_key: impl Fn(usize) -> Option<K>,
    keys: impl ExactSizeIterator<Item = K>,
    window: &mut Window,
) {
    let keys: Vec<K> = keys.collect();
    flip.replace(state, &previous_key, &keys, window);
    replace_rows(state, previous_key, keys);
}

fn replace_rows<K: PartialEq>(
    state: &ListState,
    previous_key: impl Fn(usize) -> Option<K>,
    keys: Vec<K>,
) {
    let mut anchor = state.logical_scroll_top();
    let count = keys.len();
    let at_top = anchor.item_ix == 0 && anchor.offset_in_item <= px(0.);
    let position = |key: &K| keys.iter().position(|other| other == key);
    let index = previous_key(anchor.item_ix).and_then(|top| {
        let index = position(&top)?;
        let neighbour_kept =
            |old: Option<K>, new: Option<&K>| old.is_some_and(|old| new == Some(&old));
        let before = anchor.item_ix.checked_sub(1).and_then(&previous_key);
        let after = previous_key(anchor.item_ix + 1);
        (neighbour_kept(
            before,
            index.checked_sub(1).and_then(|index| keys.get(index)),
        ) || neighbour_kept(after, keys.get(index + 1)))
        .then_some(index)
    });
    state.splice(0..state.item_count(), count);
    if at_top || count == 0 {
        return;
    }
    anchor.item_ix = index.unwrap_or(anchor.item_ix).min(count - 1);
    state.scroll_to(anchor);
}

impl SessionsSidebar {
    /// Store changes and local disclosures invalidate the model. Scroll and
    /// navigation-animation frames only clone the shared snapshot; ListState
    /// measures the visible rows, including project captions of different height.
    /// With the `window` the list is drawn in, rows that move glide there
    /// from where they were.
    fn compact_model(
        &mut self,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> Rc<CompactListModel> {
        let now = now_secs();
        let locale = rust_i18n::locale();
        if self.compact_model_dirty
            || self
                .compact_model
                .as_ref()
                .is_none_or(|model| model.locale.as_str() != &*locale)
        {
            let swiped_off = self.swiped_off(cx);
            let lists = self.thread_lists(cx);
            let (groups, collapsed_projects, sessions, flags, layout) = {
                let store = self.store.read(cx);
                let mut sessions = (*lists.flat).clone();
                sessions.retain(|meta| !swiped_off.contains(&meta.id));
                let flags = lists.flags;
                let mut groups = (*lists.groups).clone();
                for group in &mut groups {
                    group.sessions.retain(|meta| !swiped_off.contains(&meta.id));
                }
                let collapsed = groups
                    .iter()
                    .filter(|group| store.is_project_collapsed(&group.project.id))
                    .map(|group| group.project.id.clone())
                    .collect::<HashSet<_>>();
                (groups, collapsed, sessions, flags, store.sidebar_layout())
            };

            let mut rows = Vec::new();
            let mut folds: Vec<f32> = Vec::new();
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
                        .map(|group| {
                            SharedString::from(crate::project_icon::label(&group.project))
                        });
                    let timestamp = if recent {
                        tcode_core::thread_sort::shown_timestamp(meta)
                    } else {
                        project_view_timestamp(meta)
                    };
                    rows.push(CompactListRow::Thread(Rc::new(CompactThreadRow {
                        title: meta.title.clone().into(),
                        row_id: format!("compact-thread-row-{}", meta.id).into(),
                        label: crate::tr!("sidebar.thread", title = meta.title.clone())
                            .into_owned()
                            .into(),
                        timestamp,
                        relative_time: humanize_ago(now.saturating_sub(timestamp)).into(),
                        working: flags.get(&meta.id).is_some_and(|flags| flags.working),
                        state,
                        meta: meta.clone(),
                        project_name,
                        separator: index != last,
                    })));
                }
                rows
            };
            if layout == SidebarLayout::Flat {
                let key = "recent";
                let ThreadSections {
                    pinned,
                    active,
                    settled,
                } = project_threads(&sessions, None);
                let pinned_and_active_empty = pinned.is_empty() && active.is_empty();
                // A phone has no hover to reveal section labels, so they stay.
                if !pinned.is_empty() {
                    rows.push(CompactListRow::Caption {
                        key: format!("{key}-pinned"),
                        label: crate::tr!("sidebar.pinned_caption").into_owned().into(),
                    });
                    rows.extend(thread_rows(pinned, true));
                    if !active.is_empty() {
                        rows.push(CompactListRow::Caption {
                            key: format!("{key}-active"),
                            label: crate::tr!("sidebar.active_caption").into_owned().into(),
                        });
                    }
                }
                rows.extend(thread_rows(active, true));
                if pinned_and_active_empty && !settled.is_empty() {
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
                    let more = self.settled_more_count(key, hidden);
                    let (shelf, more_factor) = self.shelf_factors(key, &visible, more.is_some());
                    folds.resize(rows.len(), 1.);
                    rows.extend(thread_rows(visible, true));
                    folds.extend(shelf);
                    if let Some(count) = more {
                        rows.push(CompactListRow::More {
                            key: format!("{key}-more"),
                            count,
                        });
                        folds.push(more_factor);
                    }
                }
            } else {
                for group in &groups {
                    let key = group.project.id.as_str();
                    let count = group.sessions.len();
                    let has_header = groups.len() > 1 || group.project.quick_chat;
                    let collapsed = has_header && collapsed_projects.contains(key);
                    let start = rows.len();
                    let project_key = fold_key(Fold::Project, key);
                    if self.fold_shown(&project_key, !collapsed) {
                        folds.resize(rows.len(), 1.);
                        let (active, mut settled) = project_group_threads(&group.sessions);
                        rows.extend(thread_rows(active, false));
                        if !settled.is_empty() {
                            let count = settled.len();
                            rows.push(CompactListRow::Settled {
                                key: key.into(),
                                count,
                            });
                            self.limit_settled_rows(key, &mut settled);
                            let hidden = count - settled.len();
                            let more = self.settled_more_count(key, hidden);
                            let (shelf, more_factor) =
                                self.shelf_factors(key, &settled, more.is_some());
                            folds.resize(rows.len(), 1.);
                            rows.extend(thread_rows(settled, false));
                            folds.extend(shelf);
                            if let Some(count) = more {
                                rows.push(CompactListRow::More {
                                    key: format!("{key}-more"),
                                    count,
                                });
                                folds.push(more_factor);
                            }
                        }
                        folds.resize(rows.len(), 1.);
                        let progress = self.fold_progress(&project_key, !collapsed);
                        let count = rows.len() - start;
                        for (index, factor) in folds[start..].iter_mut().enumerate() {
                            *factor = nested_fold(fold_factor(progress, index, count), *factor);
                        }
                    }
                    if has_header {
                        folds.resize(start.max(folds.len()), 1.);
                        folds.insert(start, 1.);
                        rows.insert(
                            start,
                            CompactListRow::Project(CompactProjectRow {
                                project_id: group.project.id.clone(),
                                row_id: format!("compact-group-{}", group.project.id).into(),
                                name: crate::project_icon::group_label(&group.project).into(),
                                label: if group.project.quick_chat {
                                    crate::tr!("sidebar.quick_chats").into_owned()
                                } else {
                                    crate::tr!("sidebar.project", name = group.project.name.clone())
                                        .into_owned()
                                }
                                .into(),
                                count: count.to_string().into(),
                                collapsed,
                            }),
                        );
                    }
                }
            }
            rows.push(CompactListRow::BottomInset);
            folds.resize(rows.len(), 1.);
            let all: Vec<(u64, f32)> = rows
                .iter()
                .zip(&folds)
                .map(|(row, factor)| (presence::row_key(row.key()), *factor))
                .collect();
            let mut kept = folds.iter().map(|factor| *factor >= 0.);
            rows.retain(|_| kept.next().unwrap_or(true));
            folds.retain(|factor| *factor >= 0.);
            let drawn = self.compact_model.clone().unwrap_or_default();
            self.compact_presence.update(
                &all,
                self.folding,
                &self.compact_list_state,
                |index| {
                    let flight = self.row_swipes.flight_of(drawn.rows.get(index), cx);
                    (drawn.clone(), index, flight)
                },
                cx,
            );
            self.reconcile_row_swipes(&rows, cx);
            let previous = |index: usize| {
                self.compact_model
                    .as_ref()
                    .and_then(|model| model.rows.get(index))
                    .map(CompactListRow::key)
            };
            if let Some(window) = window
                && !rows
                    .iter()
                    .map(CompactListRow::key)
                    .eq((0..self.compact_list_state.item_count()).filter_map(previous))
            {
                let keys: Vec<&str> = rows.iter().map(CompactListRow::key).collect();
                self.compact_flip
                    .replace(&self.compact_list_state, previous, &keys, window);
            }
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
            if let Some(id) = self.reveal_pinned.clone() {
                let pinned = rows
                    .iter()
                    .enumerate()
                    .find_map(|(index, row)| match row {
                        CompactListRow::Thread(row) if row.meta.id == id => Some((
                            index,
                            tcode_core::thread_sort::thread_section(&row.meta)
                                == ThreadSection::Pinned,
                        )),
                        _ => None,
                    })
                    .map(|(index, pinned)| {
                        // The PINNED caption above the row comes into view with it.
                        let caption =
                            index > 0 && matches!(rows[index - 1], CompactListRow::Caption { .. });
                        (index - usize::from(caption), pinned)
                    });
                self.reveal_pinned_row(
                    &self.compact_list_state.clone(),
                    &self.compact_scroll.clone(),
                    pinned,
                    cx,
                );
            }
            self.compact_model = Some(Rc::new(CompactListModel {
                rows,
                folds,
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
                    row.relative_time = humanize_ago(now.saturating_sub(row.timestamp)).into();
                }
            }
        }
        model.clone()
    }

    /// Compact thread list with shared layout preference. Navigation replaces
    /// persistent row selection; desktop-only controls stay in the desktop list.
    /// The threads waiting for a connection, each folding in as it is
    /// asked for and away once the host has it.
    fn pending_thread_rows(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let store = self.store.read(cx);
        let cached = store.sidebar_sessions();
        let loading = store.threads_loading();
        let pending: Vec<(SharedString, (String, String))> = store
            .pending_sessions()
            .into_iter()
            .filter(|(id, _)| loading || !cached.iter().any(|meta| &meta.id == id))
            .map(|(id, preview)| (SharedString::from(id.clone()), (id, preview)))
            .collect();
        crate::motion::keyed(
            "pending-thread-rows",
            pending,
            ROWS,
            window,
            cx,
            |_, (id, preview), _, cx| {
                let id = id.clone();
                crate::material::list_row(
                    SharedString::from(format!("pending-thread-{id}")),
                    format!("{}, {preview}", crate::tr!("chat.waiting_connection")).into(),
                    cx,
                )
                .debug_selector(|| "pending-thread-row".into())
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(cx.theme().tokens.spacing.xs)
                        .child(
                            div()
                                .text_body(true, cx)
                                .child(crate::tr!("chat.waiting_connection")),
                        )
                        .child(
                            div()
                                .text_secondary(true, cx)
                                .overflow_hidden()
                                .child(crate::widgets::fade_text(preview.clone())),
                        ),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.store.update(cx, |store, cx| {
                        store.show_thread(id.clone(), cx);
                    });
                }))
                .into_any_element()
            },
        )
        .render(v_flex().w_full())
    }

    fn render_compact_row(
        &mut self,
        row: &CompactListRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match row {
            CompactListRow::Settled { key, count } => {
                self.render_settled_header(key, *count, None, window, cx)
            }
            CompactListRow::Caption { label, .. } => {
                crate::material::list_caption(label.clone(), true, cx).into_any_element()
            }
            CompactListRow::More { key, count } => {
                self.render_settled_more(key.trim_end_matches("-more"), *count, cx)
            }
            CompactListRow::Empty { .. } => self.render_active_empty(cx),
            CompactListRow::Project(row) => self
                .render_compact_group_header(row, window, cx)
                .into_any_element(),
            CompactListRow::Thread(row) => {
                let content = self.compact_thread_content(row, window, cx);
                self.swipeable_row(row, content, window, cx)
            }
            CompactListRow::BottomInset => div().h(cx.theme().tokens.spacing.xl).into_any_element(),
        }
    }

    fn compact_thread_content(
        &mut self,
        row: &CompactThreadRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        v_flex()
            .w_full()
            .child(self.render_compact_thread(row, window, cx))
            .when(row.separator, |list| {
                list.child(crate::material::row_hairline(px(COMPACT_PAGE_INSET), cx))
            })
            .into_any_element()
    }

    fn render_compact(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        #[cfg(test)]
        self.compact_rows_rendered.set(0);
        self.tidy_row_swipes(cx);
        let model = self.compact_model(Some(window), cx);
        let layout = self.store.read(cx).sidebar_layout();
        let body = self.loading_body(
            move |this, window, cx| {
                // The empty page fades in where the last project's list was.
                let empty = crate::motion::shown(
                    "threads-empty",
                    (!model.has_projects).then_some(()),
                    SHOW,
                    window,
                    cx,
                );
                if let Some(empty) = empty.filter(|_| !model.has_projects) {
                    div()
                        .id("threads-empty")
                        .opacity(empty.progress())
                        .debug_selector(|| "threads-empty".into())
                        .size_full()
                        .child(crate::material::empty_state(
                            Icon::new(IconName::Folder),
                            if this.store.read(cx).scope().is_full() {
                                crate::tr!("mobile.projects_empty")
                            } else {
                                crate::tr!("member.empty_title")
                            },
                            if this.store.read(cx).scope().is_full() {
                                crate::tr!("mobile.projects_help")
                            } else {
                                crate::tr!("member.empty_description")
                            },
                            cx,
                        ))
                        .into_any_element()
                } else {
                    let reveal = std::mem::take(&mut this.compact_reveal_active)
                        .then(|| {
                            let active = this.current_row(cx);
                            model.rows.iter().position(|row| {
                                matches!(row, CompactListRow::Thread(row) if Some(&row.meta.id) == active.as_ref())
                            })
                        })
                        .flatten();
                    let reveal_model = model.clone();
                    this.compact_scroll.step(&this.compact_list_state, window, cx);
                    this.compact_presence
                        .settle(&this.compact_list_state, window, cx);
                    div()
                        .id("compact-thread-list")
                        .debug_selector(|| "compact-thread-list".into())
                        .flex_1()
                        .min_h_0()
                        .relative()
                        .when_some(reveal, |list, index| {
                            list.child(
                                gpui::canvas(
                                    cx.processor(
                                        move |this, bounds: gpui::Bounds<gpui::Pixels>, window, cx| {
                                            // Child autoscroll retries prepaint without rolling back
                                            // accessibility nodes in gpui-fast 3755a55. Measure the
                                            // lead using this frame's bounds before the list prepaints.
                                            let mut remaining = bounds.size.height / 3.;
                                            let mut first = index;
                                            while first > 0 && remaining > px(0.) {
                                                first -= 1;
                                                remaining -= this
                                                    .render_compact_row(
                                                        &reveal_model.rows[first],
                                                        window,
                                                        cx,
                                                    )
                                                    .layout_as_root(
                                                        gpui::size(
                                                            gpui::AvailableSpace::Definite(
                                                                bounds.size.width,
                                                            ),
                                                            gpui::AvailableSpace::MinContent,
                                                        ),
                                                        window,
                                                        cx,
                                                    )
                                                    .height;
                                            }
                                            this.compact_list_state.scroll_to(gpui::ListOffset {
                                                item_ix: first,
                                                offset_in_item: (-remaining).max(px(0.)),
                                            });
                                        },
                                    ),
                                    |_, _, _, _| {},
                                )
                                .absolute()
                                .size_full(),
                            )
                        })
                        .child(crate::scroll::page_viewport(
                            "compact-thread-bounce",
                            &this.compact_list_state,
                            list(
                                this.compact_list_state.clone(),
                                cx.processor(move |this, index: usize, window, cx| {
                                    let factor = model.folds.get(index).copied().unwrap_or(1.);
                                    let name = model.rows[index].key();
                                    let folded = this.fold_row(SharedString::from(format!("compact-fold-{name}")), factor, window, cx, |this, window, cx| {
                                        this.render_compact_row(&model.rows[index], window, cx)
                                    });
                                    let key = presence::row_key(name);
                                    // The row's content glides to its new place; what folds away against it stays with its layout.
                                    let folded = this.compact_flip.row(
                                        index,
                                        SharedString::from(format!("compact-{name}")),
                                        folded,
                                    );
                                    this.compact_presence.around(key, cx).draw(
                                        "compact-row",
                                        key,
                                        Glide::Rest,
                                        folded,
                                        |(model, index, flight), window| {
                                            this.render_left_compact_row(&model.rows[*index], *flight, window, cx)
                                        },
                                        window,
                                    )
                                }),
                            )
                            .size_full(),
                        ))
                        .when(!window.is_inspector_picking(cx), |list| {
                            list.child(thread_list_scrollbar(
                                "compact-thread-scrollbar",
                                &this.compact_list_state,
                                compact_thread_row_height(cx),
                            ))
                        })
                        .into_any_element()
                }
            },
            window,
            cx,
        );

        v_flex()
            .size_full()
            .bg(crate::material::content_surface(cx))
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(Self::on_link_pull_request))
            .on_action(cx.listener(Self::on_rename))
            .on_action(cx.listener(Self::on_regenerate_title))
            .on_action(cx.listener(Self::on_fork))
            .on_action(cx.listener(Self::on_merge_worktree))
            .on_action(cx.listener(Self::on_move_to_project))
            .on_action(cx.listener(Self::on_mark_unread))
            .on_action(cx.listener(Self::on_copy_path))
            .on_action(cx.listener(Self::on_copy_id))
            .on_action(cx.listener(Self::on_export_jsonl))
            .on_action(cx.listener(Self::on_export_markdown))
            .on_action(cx.listener(Self::on_settle))
            .on_action(cx.listener(Self::on_auto_settle))
            .on_action(cx.listener(Self::on_follow_up))
            .on_action(cx.listener(Self::on_park))
            .on_action(cx.listener(Self::on_make_active))
            .on_action(cx.listener(Self::on_pin))
            .on_action(cx.listener(Self::on_unpin))
            .on_action(cx.listener(Self::on_move))
            .on_action(cx.listener(Self::on_arrange))
            .on_action(cx.listener(Self::on_archive))
            .on_action(cx.listener(Self::on_delete))
            .child(self.render_compact_search(cx))
            .child(
                bar("compact-layout-bar", Bar::Section, true, cx)
                    .items_center()
                    .justify_between()
                    .child(div().text_secondary(true, cx).child(match layout {
                        SidebarLayout::Flat => crate::tr!("sidebar.recent"),
                        SidebarLayout::Grouped => crate::tr!("sidebar.by_project"),
                    }))
                    .child(
                        div()
                            .debug_selector(|| "compact-layout-toggle".into())
                            .child(self.render_layout_toggle(layout, cx)),
                    ),
            )
            .when(!self.store.read(cx).threads_loading(), |list| {
                list.child(self.pending_thread_rows(window, cx))
            })
            .child(body)
            .into_any_element()
    }

    fn render_compact_search(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let spacing = &cx.theme().tokens.spacing;
        let (top, bottom) = (spacing.sm, spacing.xs);
        div()
            .flex_none()
            .px(px(COMPACT_PAGE_INSET))
            .pt(top)
            .pb(bottom)
            .child(self.search_field(
                "compact-search",
                crate::tr!("mobile.search_threads").into(),
                true,
                cx,
            ))
    }

    fn render_compact_group_header(
        &self,
        row: &CompactProjectRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let project_id = row.project_id.clone();
        let collapsed = row.collapsed;
        let spacing = &cx.theme().tokens.spacing;
        crate::material::accessible_clickable(
            h_flex(),
            row.row_id.clone(),
            Role::Button,
            row.label.clone(),
            cx,
        )
        .aria_expanded(!collapsed)
        .debug_selector(|| "compact-group-header".into())
        // A project is a section of the thread list, so its header is a list
        // caption that also carries the collapse affordance; a clickable row
        // rather than a `Button`, which centres its content.
        .w_full()
        .px(px(COMPACT_PAGE_INSET))
        .pt(spacing.lg)
        .pb(spacing.xs)
        .items_center()
        .gap(spacing.sm)
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, _, cx| {
            this.toggle_project(&project_id, cx);
        }))
        .text_caption(cx)
        .when_some(
            self.store.read(cx).project(&row.project_id),
            |el, project| {
                el.child(crate::project_icon::artwork(
                    self.store.read(cx).image_host(),
                    project,
                    14.,
                ))
            },
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .debug_selector({
                    let name = row.name.clone();
                    move || format!("compact-group-{name}")
                })
                .overflow_hidden()
                .child(crate::widgets::fade_text(row.name.clone())),
        )
        .children(self.shared_badge(&row.project_id, window, cx))
        .child(div().flex_none().child(row.count.clone()))
        .child(
            collapse_chevron(
                self.fold_progress(&fold_key(Fold::Project, &row.project_id), !collapsed),
                cx,
            )
            .text_color(cx.theme().foreground),
        )
    }

    /// One 56pt thread row. Long press opens the shared thread context menu.
    fn render_compact_thread(
        &self,
        cached: &CompactThreadRow,
        window: &mut Window,
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
        let status = faded_status(
            format!("compact-status-{}", state.session_id),
            compact_status_line(state, working, cx)
                .map(|(label, color)| (SharedString::from(label), color)),
            window,
            cx,
            |(label, color), _, cx| {
                h_flex()
                    .gap(cx.theme().tokens.spacing.xs)
                    .child(
                        div()
                            .whitespace_nowrap()
                            .text_color(*color)
                            .child(label.clone()),
                    )
                    .child("·")
                    .into_any_element()
            },
        );
        let click_id = session_id.clone();
        let renaming = self
            .renaming
            .as_ref()
            .filter(|rename| rename.session_id == session_id);
        let mark = self.provider_mark(
            meta,
            crate::material::LIST_ROW_MIN_HEIGHT,
            COMPACT_PAGE_INSET,
            cx,
        );

        // Rows needing the user carry a 6% semantic wash; everything else
        // sits on the paper with only hover and pressed tints. The fill
        // glides between them.
        let fill = if state.waiting_for_approval {
            cx.theme().warning.opacity(0.06)
        } else if state.waiting_for_input {
            cx.theme().primary.opacity(0.06)
        } else {
            cx.theme().list_active.opacity(0.)
        };
        let fill = transition(
            (cached.row_id.clone(), "fill"),
            fill,
            gpui_base::motion::Transition::new(GLIDE),
            window,
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
            .relative()
            .children(mark)
            .bg(fill)
            .on_click(cx.listener(move |this, _, _, cx| {
                if this
                    .renaming
                    .as_ref()
                    .is_some_and(|rename| rename.session_id == click_id)
                {
                    return;
                }
                let store = this.store.read(cx);
                if store.is_thread_open(&click_id) && store.session_loading_for(&click_id) {
                    return;
                }
                this.store.update(cx, |store, cx| {
                    store.show_thread(click_id.clone(), cx);
                });
            }))
            .child(compact_status_glyph(
                self.store.read(cx).image_host(),
                &session_id,
                CompactGlyph::of(state, working, cx),
                project.as_ref(),
                window,
                cx,
            ))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(cx.theme().tokens.spacing.xxs)
                    .child(
                        h_flex()
                            .w_full()
                            .min_w_0()
                            .gap(px(6.))
                            .child(if let Some(rename) = renaming {
                                self.rename_input(&rename.input, cx).into_any_element()
                            } else {
                                sidebar_label(cached.title.clone())
                                    .text_body(true, cx)
                                    .text_color(if meta.is_settled() {
                                        cx.theme().muted_foreground
                                    } else {
                                        cx.theme().foreground
                                    })
                                    .when(!meta.is_settled(), |title| title.font_medium())
                                    .when(state.show_unread, |title| title.font_semibold())
                                    .debug_selector({
                                        let id = session_id.clone();
                                        move || format!("compact-title-{id}")
                                    })
                                    .into_any_element()
                            })
                            .children(
                                crate::motion::shown(
                                    SharedString::from(format!("compact-pin-{session_id}")),
                                    (meta.pinned_at.is_some() && !meta.is_settled()).then_some(()),
                                    SHOW,
                                    window,
                                    cx,
                                )
                                .map(|shown| {
                                    shown.fold_across_gap(
                                        px(6.),
                                        Icon::new(IconName::Pin)
                                            .flex_none()
                                            .text_color(cx.theme().muted_foreground),
                                    )
                                }),
                            )
                            .when_some(
                                Self::fading_spinner(
                                    format!("title-spinner-{}", state.row_key),
                                    state.title_generating,
                                    window,
                                    cx,
                                ),
                                |row, spinner| row.child(spinner),
                            ),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .min_w_0()
                            .gap(cx.theme().tokens.spacing.xs)
                            .text_secondary(true, cx)
                            .when_some(project_name, |line, name| {
                                line.child(
                                    div()
                                        .min_w_0()
                                        .debug_selector({
                                            let name = name.clone();
                                            move || format!("compact-project-{name}")
                                        })
                                        .overflow_hidden()
                                        .child(crate::widgets::fade_text(name)),
                                )
                                .child(div().flex_none().child("·"))
                            })
                            .children(status)
                            .when_some(keeps_going(state), |line, keeps| {
                                line.child(div().flex_none().whitespace_nowrap().child(keeps))
                                    .child("·")
                            })
                            .children(wake_marks(state, cx))
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

/// What the status slot at the head of a compact row shows.
#[derive(Clone, Copy, PartialEq)]
enum CompactGlyph {
    Approval,
    Input,
    Failed,
    Spinner(gpui::Hsla),
    /// Dispatched agents await the settle.
    Unsettled,
    /// Background work goes on.
    Background,
    Unread,
    Idle,
}

impl CompactGlyph {
    fn of(state: &ThreadRowState, working: bool, cx: &App) -> Self {
        if state.waiting_for_approval {
            Self::Approval
        } else if state.waiting_for_input {
            Self::Input
        } else if state.failed {
            Self::Failed
        } else if working {
            Self::Spinner(cx.theme().primary)
        } else if state.unsettled > 0 {
            Self::Unsettled
        } else if state.background != (0, 0) {
            Self::Background
        } else if state.show_unread {
            Self::Unread
        } else {
            Self::Idle
        }
    }
}

/// The 20×20 status slot at the head of a compact row; a change of status
/// cross-fades. The slot is always taken so titles line up. Idle ungrouped
/// rows show their project artwork.
fn compact_status_glyph(
    image_host: crate::store::ImageHost,
    id: &str,
    glyph: CompactGlyph,
    project: Option<&tcode_core::project::Project>,
    window: &mut Window,
    cx: &mut App,
) -> gpui::AnyElement {
    let project = project.cloned();
    div()
        .flex_none()
        .size(px(20.))
        .child(crate::motion::cross_fade(
            SharedString::from(format!("compact-glyph-{id}")),
            glyph,
            CHANGE,
            window,
            cx,
            move |glyph, _, cx| {
                compact_glyph(image_host, *glyph, project.as_ref(), cx).into_any_element()
            },
        ))
        .into_any_element()
}

fn compact_glyph(
    image_host: crate::store::ImageHost,
    glyph: CompactGlyph,
    project: Option<&tcode_core::project::Project>,
    cx: &App,
) -> gpui::Div {
    let slot = div().size(px(20.)).flex().items_center().justify_center();
    match glyph {
        CompactGlyph::Approval => slot.rounded_full().bg(cx.theme().warning).child(
            div()
                .text_token(&cx.theme().tokens.typography.xs)
                .font_semibold()
                .text_color(gpui::white())
                .child("!"),
        ),
        CompactGlyph::Input => slot.rounded_full().bg(cx.theme().primary).child(
            div()
                .text_token(&cx.theme().tokens.typography.xs)
                .font_semibold()
                .text_color(cx.theme().primary_foreground)
                .child("?"),
        ),
        CompactGlyph::Failed => slot.child(
            Icon::new(IconName::TriangleAlert)
                .size(px(16.))
                .text_color(cx.theme().danger),
        ),
        CompactGlyph::Spinner(color) => slot.child(Spinner::new().small().color(color)),
        CompactGlyph::Unsettled => slot.child(crate::material::status_dot(cx.theme().warning)),
        CompactGlyph::Background => slot.child(crate::widgets::BreathingDot::new(
            cx.theme().muted_foreground,
        )),
        CompactGlyph::Unread => slot.child(crate::material::status_dot(cx.theme().primary)),
        CompactGlyph::Idle => slot
            .text_color(cx.theme().muted_foreground)
            .when_some(project, |slot, project| {
                slot.child(crate::project_icon::artwork(image_host, project, 20.))
            }),
    }
}

/// What a thread with a turn running or a request open says of itself: the
/// request it waits on the user for, else that it works.
fn busy_status(state: &ThreadRowState, cx: &App) -> (Cow<'static, str>, gpui::Hsla) {
    if state.waiting_for_approval {
        (crate::tr!("mobile.approval"), cx.theme().warning)
    } else if state.waiting_for_input {
        (crate::tr!("mobile.answer"), cx.theme().primary)
    } else {
        (crate::tr!("mobile.working"), cx.theme().primary)
    }
}

/// Status label and color, or `None` for an idle thread that shows only its time.
fn compact_status_line(
    state: &ThreadRowState,
    working: bool,
    cx: &App,
) -> Option<(Cow<'static, str>, gpui::Hsla)> {
    if state.waiting() {
        Some(busy_status(state, cx))
    } else if state.failed {
        Some((crate::tr!("sidebar.failed"), cx.theme().danger))
    } else if working {
        Some(busy_status(state, cx))
    } else if state.unsettled > 0 {
        Some((
            crate::tr!("sidebar.unsettled", count = state.unsettled),
            cx.theme().warning,
        ))
    } else if state.background != (0, 0) {
        Some((
            crate::tr!(
                "sidebar.background",
                count = state.background.0 + state.background.1
            ),
            cx.theme().muted_foreground,
        ))
    } else if state.show_unread {
        Some((crate::tr!("mobile.unread"), cx.theme().primary))
    } else {
        None
    }
}

impl SessionsSidebar {
    /// The Grouped layout's header and list.
    fn render_grouped_layout(
        &mut self,
        groups: Rc<Vec<ProjectGroup>>,
        flags: Rc<HashMap<String, ThreadFlags>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (gpui::AnyElement, gpui::AnyElement) {
        let mut rows = self.grouped_rows_folding(&groups, self.store.read(cx));
        if groups.is_empty() {
            let hint = if self.store.read(cx).scope().is_full() {
                crate::tr!("sidebar.empty")
            } else {
                crate::tr!("member.empty_description")
            };
            rows.push((GroupedListRow::Hint(hint), 1.));
        }
        let all: Vec<(u64, f32)> = rows
            .iter()
            .map(|(row, factor)| (presence::row_key(row.key(&groups)), *factor))
            .collect();
        rows.retain(|(_, factor)| *factor >= 0.);
        let keys = rows.iter().map(|(row, _)| row.key(&groups));
        let drawn = self.grouped_drawn.clone();
        self.grouped_presence.update(
            &all,
            self.folding,
            &self.grouped_list_state,
            |index| (drawn.clone(), index),
            cx,
        );
        if !keys.clone().eq(self
            .grouped_row_keys
            .iter()
            .map(|(kind, id)| (*kind, id.as_str())))
        {
            replace_gliding_rows(
                &self.grouped_list_state,
                &self.grouped_flip,
                |index| {
                    self.grouped_row_keys
                        .get(index)
                        .map(|(kind, id)| (*kind, id.as_str()))
                },
                keys.clone(),
                window,
            );
            self.grouped_row_keys = keys.map(|(kind, id)| (kind, id.to_owned())).collect();
        }
        self.grouped_presence
            .settle(&self.grouped_list_state, window, cx);
        let (rows, folds) = rows.into_iter().unzip();
        let frame = Rc::new(GroupedFrame {
            rows,
            folds,
            groups,
            flags,
        });
        self.grouped_drawn = frame.clone();
        let flip = self.grouped_flip.clone();
        let thread_list = list(
            self.grouped_list_state.clone(),
            cx.processor(move |this, index: usize, window, cx| {
                let Some(row) = frame.rows.get(index) else {
                    return div().into_any_element();
                };
                let factor = frame.folds.get(index).copied().unwrap_or(1.);
                let name = row.key(&frame.groups);
                let fold_id = SharedString::from(format!("grouped-fold-{}-{}", name.0, name.1));
                let folded = this.fold_row(fold_id, factor, window, cx, |this, window, cx| {
                    this.render_grouped_row(index, row, &frame.groups, &frame.flags, window, cx)
                });
                let key = presence::row_key(name);
                // The row's content glides to its new place; what folds away against it stays with its layout.
                let folded = flip.row(
                    index,
                    SharedString::from(format!("grouped-{}-{}", name.0, name.1)),
                    folded,
                );
                this.grouped_presence.around(key, cx).draw(
                    "grouped-row",
                    key,
                    Glide::Rest,
                    folded,
                    |(frame, index): &(Rc<GroupedFrame>, usize), window| {
                        let Some(row) = frame.rows.get(*index) else {
                            return div().into_any_element();
                        };
                        this.render_grouped_row(
                            *index,
                            row,
                            &frame.groups,
                            &frame.flags,
                            window,
                            cx,
                        )
                    },
                    window,
                )
            }),
        )
        .flex_1()
        .min_h_0()
        .pb_2();
        let thread_list = v_flex()
            .id("grouped-thread-list")
            .flex_1()
            .min_h_0()
            .relative()
            .child(crate::scroll::page_viewport(
                "grouped-thread-bounce",
                &self.grouped_list_state,
                thread_list,
            ))
            .when(!window.is_inspector_picking(cx), |list| {
                list.child(thread_list_scrollbar(
                    "grouped-thread-scrollbar",
                    &self.grouped_list_state,
                    crate::material::selectable_row_height(false),
                ))
            })
            .into_any_element();
        (
            self.render_projects_header(cx).into_any_element(),
            thread_list,
        )
    }

    /// The Flat layout's header and list.
    fn render_flat_layout(
        &mut self,
        groups: &[ProjectGroup],
        flat_sessions: &[SessionMeta],
        flags: Rc<HashMap<String, ThreadFlags>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (gpui::AnyElement, gpui::AnyElement) {
        let scope = self.project_filter.clone();
        let ThreadRows {
            pinned,
            active,
            settled: settled_visible,
            settled_count,
            settled_hidden_count,
        } = self.thread_rows(flat_sessions, scope.as_deref(), "recent");
        let mut visible = Vec::new();
        for (section, rows) in [
            (ThreadSection::Pinned, &pinned),
            (ThreadSection::Active, &active),
        ] {
            visible.push(FlatListRow::Boundary(section, rows.is_empty()));
            for meta in rows.iter() {
                visible.push(FlatListRow::Thread(Box::new((*meta).clone())));
            }
        }
        let more = self.settled_more_count("recent", settled_hidden_count);
        let (shelf, more_factor) = self.shelf_factors("recent", &settled_visible, more.is_some());
        let mut folds = vec![1.; visible.len()];
        if pinned.is_empty() && active.is_empty() && settled_count == 0 {
            // An active project filter can empty the list while threads
            // exist; that state gets its own hint, not the no-projects one.
            let hint = if !self.store.read(cx).scope().is_full() && flat_sessions.is_empty() {
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
            visible.push(FlatListRow::Hint(hint));
            folds.push(1.);
        }
        if settled_count > 0 {
            if pinned.is_empty() && active.is_empty() {
                visible.push(FlatListRow::Empty);
            }
            visible.push(FlatListRow::Settled);
            folds.resize(visible.len(), 1.);
            for (meta, factor) in settled_visible.into_iter().zip(shelf) {
                visible.push(FlatListRow::Thread(Box::new(meta.clone())));
                folds.push(factor);
            }
        }
        if let Some(count) = more {
            visible.push(FlatListRow::More(count));
            folds.push(more_factor);
        }
        let all: Vec<(u64, f32)> = visible
            .iter()
            .zip(&folds)
            .map(|(row, factor)| (presence::row_key(row.key()), *factor))
            .collect();
        let mut kept = folds.iter().map(|factor| *factor >= 0.);
        visible.retain(|_| kept.next().unwrap_or(true));
        folds.retain(|factor| *factor >= 0.);
        let keys = visible.iter().map(FlatListRow::key);
        let drawn = self.flat_drawn.clone();
        self.flat_presence.update(
            &all,
            self.folding,
            &self.flat_list_state,
            |index| (drawn.clone(), index),
            cx,
        );
        if !keys.clone().eq(self
            .flat_row_keys
            .iter()
            .map(|(kind, id)| (*kind, id.as_str())))
        {
            replace_gliding_rows(
                &self.flat_list_state,
                &self.flat_flip,
                |index| {
                    self.flat_row_keys
                        .get(index)
                        .map(|(kind, id)| (*kind, id.as_str()))
                },
                keys.clone(),
                window,
            );
            self.flat_row_keys = keys.map(|(kind, id)| (kind, id.to_owned())).collect();
        }
        self.flat_presence.settle(&self.flat_list_state, window, cx);
        let pinned_row = |id: &str| {
            let index = visible
                .iter()
                .position(|row| matches!(row, FlatListRow::Thread(meta) if meta.id == id))?;
            Some((index, pinned.iter().any(|meta| meta.id == id)))
        };
        if let Some(id) = self.reveal_pinned.clone() {
            self.reveal_pinned_row(
                &self.flat_list_state.clone(),
                &self.flat_scroll.clone(),
                pinned_row(&id),
                cx,
            );
        }
        let project_names = groups
            .iter()
            .map(|group| {
                (
                    group.project.id.clone(),
                    crate::project_icon::label(&group.project),
                )
            })
            .collect::<HashMap<_, _>>();
        let frame = Rc::new(FlatFrame {
            rows: visible,
            folds,
            scope,
            settled_count,
            project_names,
            flags,
        });
        self.flat_drawn = frame.clone();
        let flip = self.flat_flip.clone();
        self.flat_scroll.step(&self.flat_list_state, window, cx);
        let thread_list = list(
            self.flat_list_state.clone(),
            cx.processor(move |this, index: usize, window, cx| {
                let factor = frame.folds.get(index).copied().unwrap_or(1.);
                let name = frame.rows.get(index).map_or(("", ""), FlatListRow::key);
                let fold_id = SharedString::from(format!("flat-fold-{}-{}", name.0, name.1));
                let folded = this.fold_row(fold_id, factor, window, cx, |this, window, cx| {
                    this.render_flat_row(&frame, index, window, cx)
                });
                let key = presence::row_key(name);
                // The row's content glides to its new place; what folds away against it stays with its layout.
                let folded = flip.row(
                    index,
                    SharedString::from(format!("flat-{}-{}", name.0, name.1)),
                    folded,
                );
                this.flat_presence.around(key, cx).draw(
                    "flat-row",
                    key,
                    Glide::Rest,
                    folded,
                    |(frame, index): &(Rc<FlatFrame>, usize), window| {
                        this.render_flat_row(frame, *index, window, cx)
                    },
                    window,
                )
            }),
        )
        .flex_1()
        .min_h_0()
        .into_any_element();
        (
            self.render_flat_header(window, cx).into_any_element(),
            v_flex()
                .id("flat-thread-list")
                .flex_1()
                .min_h_0()
                .relative()
                .on_drop(cx.listener(|this, _: &AnyDrag, window, cx| this.drop_thread(window, cx)))
                .on_drag_move(
                    cx.listener(|this, event: &gpui::DragMoveEvent<AnyDrag>, _, cx| {
                        this.drag_left_list(event, cx);
                        this.drag_auto_scroll(event, cx)
                    }),
                )
                .child(crate::scroll::page_viewport(
                    "flat-thread-bounce",
                    &self.flat_list_state,
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

    /// Row `index` of a frame of the Flat list.
    fn render_flat_row(
        &mut self,
        frame: &FlatFrame,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(row) = frame.rows.get(index) else {
            return div().into_any_element();
        };
        let scope = frame.scope.clone();
        let meta = match row {
            FlatListRow::Thread(meta) => meta,
            FlatListRow::Boundary(section, empty) => {
                return div()
                    .w_full()
                    .px_2()
                    .child(self.render_drag_boundary(*section, *empty, scope, window, cx))
                    .into_any_element();
            }
            FlatListRow::Settled => {
                let header = self.render_settled_header(
                    "recent",
                    frame.settled_count,
                    scope.as_deref(),
                    window,
                    cx,
                );
                return self
                    .drop_zone(div().w_full().child(header), DropZone::Settled, scope, cx)
                    .into_any_element();
            }
            FlatListRow::More(count) => {
                return self.render_settled_more("recent", *count, cx);
            }
            FlatListRow::Empty => return self.render_active_empty(cx),
            FlatListRow::Hint(hint) => {
                return div()
                    .w_full()
                    .px_2()
                    .pb_2()
                    .child(empty_row(hint.clone(), cx))
                    .into_any_element();
            }
        };
        // While carried, the row keeps its place, hidden under its ghost.
        let project_name = meta
            .project_id
            .as_ref()
            .and_then(|project_id| frame.project_names.get(project_id))
            .cloned();
        let light = self.shown.light(&meta.id);
        let thread = self.render_flat_thread(meta, &frame.flags, project_name, light, window, cx);
        let row = div().w_full().px_2().pb(px(2.)).child(drag_ghost::slot(
            arrange::thread_slot(&meta.id),
            thread,
            window,
            cx,
        ));
        let zone = DropZone::Row {
            id: meta.id.clone(),
            section: tcode_core::thread_sort::thread_section(meta),
        };
        self.drop_zone(row, zone, scope, cx).into_any_element()
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
        self.sample_folds(window, cx);
        self.clear_finished_drag(cx);
        let spaces = spaces::for_store(&self.store, cx);
        self.spaces_observer.watch(spaces.as_ref(), cx);
        if self.compact(cx) {
            return self.render_compact(window, cx);
        }

        let layout = self.store.read(cx).sidebar_layout();
        let ThreadLists {
            groups,
            flat: flat_sessions,
            flags,
            ..
        } = self.thread_lists(cx);
        // Flat and Grouped are each their own list; switching cross-fades
        // them, the one left keeping its scroll while it fades. Until the
        // settings and threads load, the layout is only adopted.
        let current = usize::from(layout == SidebarLayout::Flat);
        let loading = self.store.read(cx).threads_loading();
        let layout_fade = |id: &'static str, window: &mut Window, cx: &mut App| {
            if loading {
                let rest = (0..2).map(|layer| f32::from(u8::from(layer == current)));
                crate::motion::CrossFade::sampled(id, current, rest.collect())
            } else {
                crate::motion::CrossFade::new(id, current, 2, CHANGE, window, cx)
            }
        };
        let fade = layout_fade("sidebar-layout", window, cx);
        let (mut headers, mut lists) = (Vec::new(), Vec::new());
        if fade.shows(0) {
            let (header, list) =
                self.render_grouped_layout(groups.clone(), flags.clone(), window, cx);
            headers.push((0, header));
            lists.push((0, v_flex().size_full().child(list).into_any_element()));
        }
        if fade.shows(1) {
            let (header, list) =
                self.render_flat_layout(&groups, &flat_sessions, flags, window, cx);
            headers.push((1, header));
            lists.push((1, v_flex().size_full().child(list).into_any_element()));
        }
        let header = layout_fade("sidebar-layout-header", window, cx)
            .span()
            .render(headers)
            .into_any_element();
        let thread_list = fade
            .fill()
            .render(lists)
            .flex_1()
            .min_h_0()
            .into_any_element();

        // Wide, the sidebar is the window's left column, docked or laid over
        // the chat, from the window's top to its bottom.
        crate::window_seam::inset_from_seam(
            v_flex().size_full().bg(cx.theme().sidebar),
            "sidebar-seam",
            gpui::Edges {
                right: false,
                ..gpui::Edges::all(true)
            },
            window,
            cx,
        )
        .text_color(cx.theme().sidebar_foreground)
        .on_action(cx.listener(Self::on_link_pull_request))
        .on_action(cx.listener(Self::on_rename))
        .on_action(cx.listener(Self::on_regenerate_title))
        .on_action(cx.listener(Self::on_fork))
        .on_action(cx.listener(Self::on_merge_worktree))
        .on_action(cx.listener(Self::on_move_to_project))
        .on_action(cx.listener(Self::on_mark_unread))
        .on_action(cx.listener(Self::on_copy_path))
        .on_action(cx.listener(Self::on_copy_id))
        .on_action(cx.listener(Self::on_export_jsonl))
        .on_action(cx.listener(Self::on_export_markdown))
        .on_action(cx.listener(Self::on_settle))
        .on_action(cx.listener(Self::on_auto_settle))
        .on_action(cx.listener(Self::on_follow_up))
        .on_action(cx.listener(Self::on_park))
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
        .child(self.render_machine_control(window, cx))
        .child(header)
        .child(self.loading_body(
            |this, window, cx| {
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .child(this.pending_thread_rows(window, cx))
                    .child(thread_list)
                    .into_any_element()
            },
            window,
            cx,
        ))
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

    /// A row's label tells a screen reader where its pane lies. The test
    /// window has no accessibility tree to read the label from.
    #[test]
    fn a_pane_reads_as_where_it_lies() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let place = |left, top, width, height| {
            MapCell {
                pane: gpui::EntityId::from(1),
                row: None,
                rect: gpui::bounds(gpui::point(left, top), gpui::size(width, height)),
            }
            .place()
            .into_owned()
        };
        for (rect, words) in [
            ((0., 0., 0.5, 1.), "left"),
            ((0.5, 0., 0.5, 1.), "right"),
            ((0., 0., 1., 0.5), "top"),
            ((0., 0.5, 1., 0.5), "bottom"),
            ((0.5, 0., 0.5, 0.5), "top right"),
            ((0., 0.5, 0.5, 0.5), "bottom left"),
            ((1. / 3., 0., 1. / 3., 1.), "middle"),
            ((2. / 3., 0.5, 1. / 3., 0.5), "bottom right"),
        ] {
            assert_eq!(place(rect.0, rect.1, rect.2, rect.3), words, "{rect:?}");
        }
    }

    struct WorkingThreadRowProbe;

    impl Render for WorkingThreadRowProbe {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            selectable_row(h_flex(), false, false, cx)
                .w_full()
                .pl(px(THREAD_INDENT))
                .text_body(false, cx)
                .debug_selector(|| "thread-row".into())
                .child(
                    h_flex()
                        .flex_none()
                        .items_center()
                        .gap_1()
                        .child(div().size(px(6.)))
                        .child(
                            div()
                                .whitespace_nowrap()
                                .text_secondary(false, cx)
                                .child("工作中"),
                        ),
                )
                .child(
                    sidebar_label("Phase 0 修复架构约束测试")
                        .debug_selector(|| "thread-title".into())
                        .text_body(false, cx),
                )
        }
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });
    }

    /// Draws the frame after whatever a change set moving has come to rest.
    fn draw_settled(cx: &mut VisualTestContext) {
        draw(cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        cx.update(|window, cx| window.simulate_next_frame(cx));
        draw(cx);
    }

    fn session(id: &str, parent_id: Option<&str>) -> SessionMeta {
        let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/project"), None);
        meta.id = id.to_string();
        meta.title = id.to_string();
        meta.parent_session_id = parent_id.map(str::to_string);
        meta
    }

    #[gpui::test]
    fn compact_thread_distinguishes_host_background_activity(cx: &mut TestAppContext) {
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
                    background: tcode_protocol::BackgroundActivity {
                        tasks: waiting.into(),
                        agents: 0,
                    },
                    unsettled: 0,
                    wakes: Default::default(),
                    waiting_for_approval: false,
                    waiting_for_input: false,
                    failed: false,
                    unread: false,
                    fork: ForkAvailability::Available,
                    agent: None,
                    latest_activity: None,
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
                .pump_with_timer(|delay| executor.timer(delay))
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
        store.update(cx, |store, cx| store.show_alone("background".into(), cx));
        let window_state = cx.new(|_| WindowState::new(false).with_compact(true));
        let (sidebar, cx) = cx
            .add_window_view(|_, cx| SessionsSidebar::new(store.clone(), window_state.clone(), cx));
        cx.simulate_resize(size(px(393.), px(852.)));
        for (waiting, expected) in [(true, "Background 1"), (false, "Working")] {
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
        cx.update(crate::theme::init);
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

    /// A local host holding `threads` (newest first) in one project, and a
    /// desktop sidebar over it in a window, with motion on.
    fn sidebar_over_host(
        cx: &mut TestAppContext,
        name: &str,
        threads: &[&str],
    ) -> (
        Entity<SessionsSidebar>,
        Entity<WorkspaceStore>,
        Entity<WindowState>,
        crate::store::tests::LocalHostDriver,
        tcode_runtime::pipe::SpawnedHost,
        PathBuf,
    ) {
        cx.update(crate::theme::init);
        let root = crate::store::tests::scratch_root(name);
        let session_store = SessionStore::open_at(root.clone()).unwrap();
        let project = Project::from_root(root.clone());
        let sessions: Vec<SessionMeta> = threads
            .iter()
            .enumerate()
            .map(|(index, id)| {
                let mut meta = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
                meta.project_id = Some(project.id.clone());
                meta.id = (*id).into();
                meta.title = (*id).into();
                meta.created_at = 1_000 - index as u64;
                meta.updated_at = meta.created_at;
                meta
            })
            .collect();
        let host =
            spawn_host(session_store, HostServices::default()).expect("spawn sidebar test host");
        smol::block_on(host.update_state_for_test(move |state, _| {
            state.projects = vec![project];
            state.sessions = sessions.into();
        }))
        .expect("seed sidebar host");
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        driver.wait_until(cx, &store, "sidebar baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });
        let window_state = cx.new(|_| WindowState::new(false));
        let sidebar = cx.new(|cx| SessionsSidebar::new(store.clone(), window_state.clone(), cx));
        (sidebar, store, window_state, driver, host, root)
    }

    /// A thread that moves in the list (pinned here) is drawn where it was,
    /// then glides to its new place; a Follow-up bell widens into its row,
    /// moving over gradually; and switching to Grouped cross-fades the two
    /// lists, the one left fading out rather than vanishing.
    #[gpui::test]
    fn sidebar_rows_glide_to_where_they_move_and_the_layouts_cross_fade(cx: &mut TestAppContext) {
        let (sidebar, store, _, mut driver, host, root) =
            sidebar_over_host(cx, "sidebar-glide-test", &["newest", "middle", "oldest"]);
        let (_, cx) =
            cx.add_window_view(|window, cx| gpui_base::Root::new(sidebar.clone(), window, cx));
        let cx: &mut VisualTestContext = cx;
        cx.update(|_, cx| cx.set_reduce_motion(false));
        cx.simulate_resize(size(px(320.), px(800.)));
        crate::test_frames::draw_settled(cx);
        let top = |id: &str, cx: &mut VisualTestContext| {
            cx.debug_bounds(format!("sidebar-thread-{id}").leak())
                .unwrap_or_else(|| panic!("{id} is drawn"))
                .top()
        };
        let (first, last) = (top("newest", cx), top("oldest", cx));
        assert!(first < last);

        cx.update(|window, cx| {
            sidebar.update(cx, |sidebar, cx| {
                sidebar.on_pin(&ThreadPin("oldest".into()), window, cx)
            })
        });
        driver.wait_until(cx, &store, "the pin", |cx| {
            store.read_with(cx, |store, _| {
                store
                    .thread_meta("oldest")
                    .is_some_and(|meta| meta.pinned_at.is_some())
            })
        });
        draw(cx);
        assert_eq!(top("oldest", cx), last, "it starts from where it was");
        cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
        cx.update(|window, cx| window.simulate_next_frame(cx));
        draw(cx);
        let gliding = top("oldest", cx);
        assert!(gliding < last && gliding > first, "it is on its way up");
        crate::test_frames::draw_settled(cx);
        assert!(top("oldest", cx) < top("newest", cx), "Pinned leads");

        let time_left = |cx: &mut VisualTestContext| {
            cx.debug_bounds("follow-up-middle").map(|bell| bell.left())
        };
        assert_eq!(time_left(cx), None);
        cx.update(|window, cx| {
            sidebar.update(cx, |sidebar, cx| {
                sidebar.on_follow_up(&ThreadFollowUp("middle".into(), true), window, cx)
            })
        });
        driver.wait_until(cx, &store, "the follow-up", |cx| {
            store.read_with(cx, |store, _| {
                store
                    .thread_meta("middle")
                    .is_some_and(|meta| meta.follow_up.is_some())
            })
        });
        draw(cx);
        let start = time_left(cx).expect("the bell is drawn from the start");
        cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
        cx.update(|window, cx| window.simulate_next_frame(cx));
        draw(cx);
        let widening = time_left(cx).unwrap();
        crate::test_frames::draw_settled(cx);
        let rest = time_left(cx).unwrap();
        assert!(
            rest < widening && widening < start,
            "the bell widens in, from {start:?} through {widening:?} to {rest:?}"
        );

        let header = |cx: &mut VisualTestContext| {
            store.read_with(cx, |store, _| store.projects()[0].id.clone())
        };
        let header: &'static str = format!("project-header-{}", header(cx)).leak();
        assert!(
            cx.debug_bounds(header).is_none(),
            "Flat shows no project headers"
        );
        // What the header's layout toggle does.
        let mut switch = |layout, cx: &mut VisualTestContext| {
            store.update(cx, |store, _| store.set_sidebar_layout(layout));
            driver.wait_until(cx, &store, "the layout", |cx| {
                store.read_with(cx, |store, _| store.sidebar_layout() == layout)
            });
            draw(cx);
        };
        switch(SidebarLayout::Grouped, cx);
        crate::test_frames::draw_settled(cx);
        assert!(
            cx.debug_bounds(header).is_some(),
            "Grouped shows its projects"
        );
        switch(SidebarLayout::Flat, cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
        cx.update(|window, cx| window.simulate_next_frame(cx));
        draw(cx);
        assert!(
            cx.debug_bounds(header).is_some(),
            "Grouped fades out rather than vanishing"
        );
        crate::test_frames::draw_settled(cx);
        assert!(cx.debug_bounds(header).is_none(), "and is gone once it has");

        driver.shutdown(cx, &store, &host);
        let _ = std::fs::remove_dir_all(root);
    }

    /// On the phone, pinning a thread is one change that brings the Pinned
    /// and Active captions and moves rows past each other: each thread
    /// starts from where it was and glides to its new place, one way and
    /// never in a jump.
    #[gpui::test]
    fn a_phone_list_change_that_inserts_and_reorders_moves_every_row_gradually(
        cx: &mut TestAppContext,
    ) {
        let (sidebar, store, window_state, mut driver, host, root) =
            sidebar_over_host(cx, "sidebar-phone-reorder", &["newest", "middle", "oldest"]);
        cx.update(|cx| window_state.update(cx, |state, cx| state.set_compact(true, cx)));
        let (_, cx) =
            cx.add_window_view(|window, cx| gpui_base::Root::new(sidebar.clone(), window, cx));
        let cx: &mut VisualTestContext = cx;
        cx.update(|_, cx| cx.set_reduce_motion(false));
        cx.simulate_resize(size(px(393.), px(852.)));
        crate::test_frames::draw_settled(cx);
        let top = |id: &str, cx: &mut VisualTestContext| {
            cx.debug_bounds(format!("compact-row-{id}").leak())
                .unwrap_or_else(|| panic!("{id} is drawn"))
                .top()
        };
        let before = [top("newest", cx), top("oldest", cx)];

        cx.update(|window, cx| {
            sidebar.update(cx, |sidebar, cx| {
                sidebar.on_pin(&ThreadPin("oldest".into()), window, cx)
            })
        });
        driver.wait_until(cx, &store, "the pin", |cx| {
            store.read_with(cx, |store, _| {
                store
                    .thread_meta("oldest")
                    .is_some_and(|meta| meta.pinned_at.is_some())
            })
        });
        draw(cx);
        let mut tops = vec![[top("newest", cx), top("oldest", cx)]];
        for _ in 0..20 {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(16));
            cx.update(|window, cx| window.simulate_next_frame(cx));
            draw(cx);
            tops.push([top("newest", cx), top("oldest", cx)]);
        }
        crate::test_frames::draw_settled(cx);
        let after = [top("newest", cx), top("oldest", cx)];
        assert!(after[1] < after[0], "Pinned leads: {after:?}");
        assert_eq!(tops[0], before, "every row starts from where it was");
        assert_eq!(*tops.last().unwrap(), after, "{tops:?}");
        for row in 0..2 {
            let path: Vec<_> = tops.iter().map(|tops| tops[row]).collect();
            let distance = (after[row] - before[row]).abs();
            assert!(distance > px(0.), "row {row} moved: {path:?}");
            assert!(
                path.windows(2).all(|pair| {
                    let step = f32::from(pair[1] - pair[0]);
                    step.abs() < f32::from(distance) / 2.
                        && step * f32::from(after[row] - before[row]) >= 0.
                }),
                "row {row} jumped or went back: {path:?}"
            );
        }

        driver.shutdown(cx, &store, &host);
        let _ = std::fs::remove_dir_all(root);
    }

    /// A thread archived from the list folds away where it was, taking no
    /// clicks while it goes, and the rows below close up behind it; one that
    /// comes back opens its room again. Nothing below jumps either way, and
    /// with motion reduced the list changes at once.
    #[gpui::test]
    fn threads_that_leave_and_return_fold_out_of_and_into_the_list(cx: &mut TestAppContext) {
        let (sidebar, store, _, mut driver, host, root) =
            sidebar_over_host(cx, "sidebar-presence-test", &["newest", "middle", "oldest"]);
        let (_, cx) =
            cx.add_window_view(|window, cx| gpui_base::Root::new(sidebar.clone(), window, cx));
        let cx: &mut VisualTestContext = cx;
        cx.update(|_, cx| cx.set_reduce_motion(false));
        cx.simulate_resize(size(px(320.), px(800.)));
        crate::test_frames::draw_settled(cx);
        let top = |id: &str, cx: &mut VisualTestContext| {
            cx.debug_bounds(format!("sidebar-thread-{id}").leak())
                .map(|bounds| bounds.top())
        };
        let (middle, oldest) = (top("middle", cx).unwrap(), top("oldest", cx).unwrap());
        let mut lifecycle = |command: Command, listed: bool, cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                sidebar.update(cx, |sidebar, cx| {
                    sidebar.perform_lifecycle(vec![command], None, window, cx)
                })
            });
            driver.wait_until(cx, &store, "the lifecycle change", |cx| {
                store.read_with(cx, |store, _| {
                    store
                        .sidebar_sessions()
                        .iter()
                        .any(|meta| meta.id == "middle")
                        == listed
                })
            });
            draw(cx);
        };
        let halfway = |cx: &mut VisualTestContext| {
            cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
            cx.update(|window, cx| window.simulate_next_frame(cx));
            draw(cx);
        };
        let archive = || Command::ArchiveSession {
            session_id: "middle".into(),
        };

        lifecycle(archive(), false, cx);
        assert_eq!(top("middle", cx), Some(middle), "it folds from its place");
        assert_eq!(
            top("oldest", cx),
            Some(oldest),
            "the row below waits for it"
        );
        let pressed = cx.debug_bounds("sidebar-thread-middle").unwrap().center();
        cx.simulate_click(pressed, Default::default());
        assert_ne!(
            store.read_with(cx, |store, _| store.sole_thread()),
            Some("middle".into()),
            "a leaving row takes no clicks"
        );
        halfway(cx);
        let closing = top("oldest", cx).unwrap();
        assert!(
            closing < oldest && closing > middle,
            "the row below closes up behind it: {closing:?}"
        );
        crate::test_frames::draw_settled(cx);
        assert_eq!(top("middle", cx), None, "it has gone");
        assert_eq!(top("oldest", cx), Some(middle));

        lifecycle(
            Command::UnarchiveSession {
                session_id: "middle".into(),
            },
            true,
            cx,
        );
        assert_eq!(top("oldest", cx), Some(middle), "the row below waits");
        halfway(cx);
        let opening = top("oldest", cx).unwrap();
        assert!(
            opening > middle && opening < oldest,
            "it opens its room: {opening:?}"
        );
        crate::test_frames::draw_settled(cx);
        assert!(top("middle", cx).is_some(), "it is back");
        assert_eq!(top("oldest", cx), Some(oldest));

        cx.update(|_, cx| cx.set_reduce_motion(true));
        lifecycle(archive(), false, cx);
        assert_eq!(top("middle", cx), None, "reduced motion takes it at once");
        assert!(top("oldest", cx) < Some(oldest), "and closes up at once");

        driver.shutdown(cx, &store, &host);
        let _ = std::fs::remove_dir_all(root);
    }

    /// The sidebar, with the reveal of its `row` in each frame: the test
    /// platform lays out text without painting glyphs, so what a row reveals
    /// is read from the reveal it draws it at, on the frame's clock.
    struct Drawn {
        sidebar: Entity<SessionsSidebar>,
        row: &'static str,
        reveals: std::rc::Rc<std::cell::RefCell<Vec<f32>>>,
    }
    impl Render for Drawn {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let hover = self.sidebar.read(cx).hover.clone();
            let reveal = hover.reveal(self.row, window, cx);
            self.reveals.borrow_mut().push(reveal);
            div().size_full().child(self.sidebar.clone())
        }
    }

    /// A row the pointer comes onto and clicks while its time is still
    /// fading out for the row's controls, the pointer still moving as it
    /// presses, selects the thread and fades the time out once: it never
    /// comes back or starts over.
    #[gpui::test]
    fn a_quick_click_fades_the_rows_time_out_once(cx: &mut TestAppContext) {
        let (sidebar, store, _, mut driver, host, root) =
            sidebar_over_host(cx, "sidebar-time-click", &["newest", "middle", "oldest"]);
        crate::store::tests::place_shown_threads(&store, cx);
        let reveals = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let (_, cx) = cx.add_window_view(|window, cx| {
            let drawn = cx.new(|_| Drawn {
                sidebar: sidebar.clone(),
                row: "flat-thread-middle",
                reveals: reveals.clone(),
            });
            gpui_base::Root::new(drawn, window, cx)
        });
        let cx: &mut VisualTestContext = cx;
        cx.update(|_, cx| cx.set_reduce_motion(false));
        cx.simulate_resize(size(px(320.), px(800.)));
        let (none, left) = (gpui::Modifiers::none(), gpui::MouseButton::Left);
        cx.simulate_mouse_move(gpui::point(px(300.), px(790.)), None, none);
        crate::test_frames::draw_settled(cx);
        let frame = |cx: &mut VisualTestContext| {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(16));
            cx.update(|window, cx| window.simulate_next_frame(cx));
            draw(cx);
        };

        let row = cx.debug_bounds("sidebar-thread-middle").unwrap();
        let at = gpui::point(row.center().x, row.top() + px(8.));
        cx.simulate_mouse_move(at, None, none);
        let since = reveals.borrow().len();
        for _ in 0..3 {
            frame(cx);
        }
        cx.simulate_mouse_down(at, left, none);
        let at = at + gpui::point(px(2.), px(0.));
        cx.simulate_mouse_move(at, Some(left), none);
        frame(cx);
        cx.simulate_mouse_up(at, left, none);
        cx.simulate_mouse_move(at + gpui::point(px(1.), px(0.)), None, none);
        for _ in 0..20 {
            frame(cx);
        }

        assert_eq!(
            store.read_with(cx, |store, _| store.open_threads()),
            ["middle"]
        );
        // The time is drawn at what is left of the reveal.
        let drawn = reveals.borrow()[since..]
            .iter()
            .map(|reveal| 1. - reveal)
            .collect::<Vec<_>>();
        assert!(
            drawn[2] > 0. && drawn[2] < 1.,
            "the click lands mid-fade: {drawn:?}"
        );
        assert!(
            drawn.windows(2).all(|pair| pair[1] <= pair[0]),
            "the time came back: {drawn:?}"
        );
        assert_eq!(drawn.last(), Some(&0.));

        driver.shutdown(cx, &store, &host);
        let _ = std::fs::remove_dir_all(root);
    }

    /// On the phone's Arrange page, a thread unpinned leaves Pinned for
    /// Active by gliding there from its place, and the empty-Pinned row
    /// folds in behind it.
    #[gpui::test]
    fn an_arranged_thread_glides_between_sections(cx: &mut TestAppContext) {
        struct Arrange(Entity<SessionsSidebar>);
        impl Render for Arrange {
            fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let page = self
                    .0
                    .update(cx, |sidebar, cx| sidebar.render_arrange(window, cx));
                div().size_full().child(page)
            }
        }
        let (sidebar, store, window_state, mut driver, host, root) =
            sidebar_over_host(cx, "sidebar-arrange-glide-test", &["first", "second"]);
        let (_, cx) = cx.add_window_view(|window, cx| {
            let page = cx.new(|_| Arrange(sidebar.clone()));
            gpui_base::Root::new(page, window, cx)
        });
        let cx: &mut VisualTestContext = cx;
        cx.update(|_, cx| cx.set_reduce_motion(false));
        cx.simulate_resize(size(px(393.), px(852.)));
        window_state.update(cx, |state, cx| {
            state.set_compact(true, cx);
            state.go(crate::window_state::Destination::ArrangeThreads, cx);
        });
        cx.update(|window, cx| {
            sidebar.update(cx, |sidebar, cx| {
                sidebar.on_pin(&ThreadPin("second".into()), window, cx)
            })
        });
        driver.wait_until(cx, &store, "the pin", |cx| {
            store.read_with(cx, |store, _| {
                store
                    .thread_meta("second")
                    .is_some_and(|meta| meta.pinned_at.is_some())
            })
        });
        crate::test_frames::draw_settled(cx);
        let top = |cx: &mut VisualTestContext| {
            cx.debug_bounds("arrange-row-second")
                .expect("the row is drawn")
                .top()
        };
        let pinned = top(cx);
        assert!(cx.debug_bounds("arrange-pinned-empty").is_none());

        cx.update(|window, cx| {
            sidebar.update(cx, |sidebar, cx| sidebar.unpin_thread("second", window, cx))
        });
        driver.wait_until(cx, &store, "the unpin", |cx| {
            store.read_with(cx, |store, _| {
                store
                    .thread_meta("second")
                    .is_some_and(|meta| meta.pinned_at.is_none())
            })
        });
        draw(cx);
        assert_eq!(top(cx), pinned, "it starts from where it was");
        cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
        cx.update(|window, cx| window.simulate_next_frame(cx));
        draw(cx);
        let gliding = top(cx);
        crate::test_frames::draw_settled(cx);
        let active = top(cx);
        assert!(
            pinned < gliding && gliding < active,
            "it glides down into Active: {pinned:?}, {gliding:?}, {active:?}"
        );
        assert!(cx.debug_bounds("arrange-pinned-empty").is_some());

        driver.shutdown(cx, &store, &host);
        let _ = std::fs::remove_dir_all(root);
    }

    /// A row dragged to another place and let go lands from where it was let
    /// go: its ghost travels from there into the row's new place, which the
    /// row holds from the drop on, while the host takes the move and after,
    /// never going back to where it came from. A drag Escape cancels flies
    /// back from where it was let go into the row's own place. With motion
    /// reduced a dropped row is in its new place at once.
    #[gpui::test]
    fn a_dropped_row_lands_from_where_it_was_let_go(cx: &mut TestAppContext) {
        let (sidebar, store, _, mut driver, host, root) =
            sidebar_over_host(cx, "sidebar-drop-landing", &["newest", "middle", "oldest"]);
        let (_, cx) =
            cx.add_window_view(|window, cx| gpui_base::Root::new(sidebar.clone(), window, cx));
        let cx: &mut VisualTestContext = cx;
        cx.update(|_, cx| cx.set_reduce_motion(false));
        cx.simulate_resize(size(px(320.), px(800.)));
        crate::test_frames::draw_settled(cx);
        let row = |id: &str, cx: &mut VisualTestContext| {
            cx.debug_bounds(format!("sidebar-thread-{id}").leak())
                .unwrap_or_else(|| panic!("{id} is drawn"))
        };
        let ghost = |id: &str, cx: &mut VisualTestContext| {
            cx.debug_bounds(format!("drag-ghost-thread-{id}").leak())
                .map(|ghost| ghost.top())
        };
        let frame = |cx: &mut VisualTestContext| {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(16));
            cx.update(|window, cx| window.simulate_next_frame(cx));
            draw(cx);
        };
        let listed = |cx: &mut VisualTestContext| {
            store.read_with(cx, |store, _| {
                store
                    .flat_sessions()
                    .iter()
                    .map(|meta| meta.id.clone())
                    .collect::<Vec<_>>()
            })
        };
        let first = row("newest", cx).top();
        let (left, none) = (gpui::MouseButton::Left, gpui::Modifiers::default());
        // Carries `id` to `to` and returns where its ghost is drawn there.
        let carry = |id: &str, to: gpui::Point<gpui::Pixels>, cx: &mut VisualTestContext| {
            let place = row(id, cx);
            cx.simulate_mouse_down(place.center(), left, none);
            cx.simulate_mouse_move(place.center() + gpui::point(px(0.), px(8.)), left, none);
            draw(cx);
            if !cx.update(|_, cx| cx.reduce_motion()) {
                assert_eq!(ghost(id, cx), Some(place.top()), "it lifts off its place");
            }
            cx.simulate_mouse_move(to, left, none);
            for _ in 0..20 {
                frame(cx);
            }
            ghost(id, cx).expect("the row is carried")
        };

        let newest = row("newest", cx);
        let to = gpui::point(newest.center().x, newest.top() + px(4.));
        let held = carry("oldest", to, cx);
        let dropped = row("oldest", cx).top();
        cx.simulate_mouse_up(to, left, none);
        // The frame the release is drawn in, those before the host has taken
        // the move, then those after.
        draw(cx);
        let (mut ghosts, mut rows) = (vec![ghost("oldest", cx)], vec![row("oldest", cx).top()]);
        for _ in 0..4 {
            frame(cx);
            ghosts.push(ghost("oldest", cx));
            rows.push(row("oldest", cx).top());
        }
        driver.wait_until(cx, &store, "the move", |cx| {
            store.read_with(cx, |store, _| {
                store.flat_sessions().first().map(|meta| meta.id.as_str()) == Some("oldest")
            })
        });
        for _ in 0..20 {
            frame(cx);
            ghosts.push(ghost("oldest", cx));
            rows.push(row("oldest", cx).top());
        }
        assert_eq!(ghosts[0], Some(held), "it lands from where it was let go");
        // Pinned's empty target closes behind the drop, so the new place
        // moves up as the row settles into it.
        assert!(
            rows[0] == dropped
                && rows.windows(2).all(|pair| pair[1] <= pair[0])
                && rows.last() == Some(&first),
            "the row holds the place it was dropped into: {rows:?}"
        );
        assert!(
            ghosts
                .iter()
                .flatten()
                .all(|top| *top >= held && *top <= dropped + px(1.)),
            "the ghost goes from where it was let go into that place: {ghosts:?}"
        );
        assert_eq!(ghosts.last(), Some(&None), "and has settled there");
        assert_eq!(listed(cx), ["oldest", "newest", "middle"]);

        let middle = row("middle", cx);
        let to = gpui::point(middle.center().x, middle.bottom() - px(4.));
        let held = carry("oldest", to, cx);
        cx.simulate_keystrokes("escape");
        draw(cx);
        let mut ghosts = vec![ghost("oldest", cx)];
        for _ in 0..20 {
            frame(cx);
            ghosts.push(ghost("oldest", cx));
        }
        cx.simulate_mouse_up(to, left, none);
        crate::test_frames::draw_settled(cx);
        assert_eq!(
            ghosts[0],
            Some(held),
            "Escape sends it back from where it was let go"
        );
        assert!(
            ghosts
                .iter()
                .flatten()
                .all(|top| *top <= held && *top >= first - px(1.)),
            "into its own place: {ghosts:?}"
        );
        assert_eq!(ghost("oldest", cx), None);
        assert_eq!(row("oldest", cx).top(), first);
        assert_eq!(listed(cx), ["oldest", "newest", "middle"], "nothing moved");

        cx.update(|_, cx| cx.set_reduce_motion(true));
        let oldest = row("oldest", cx);
        let to = gpui::point(oldest.center().x, oldest.top() + px(4.));
        carry("middle", to, cx);
        cx.simulate_mouse_up(to, left, none);
        frame(cx);
        assert_eq!(ghost("middle", cx), None, "reduced motion sets it down");
        assert_eq!(row("middle", cx).top(), first, "in its new place at once");

        driver.shutdown(cx, &store, &host);
        let _ = std::fs::remove_dir_all(root);
    }

    #[gpui::test]
    fn canceling_inline_rename_discards_the_unsaved_title(cx: &mut TestAppContext) {
        for compact in [false, true] {
            cancel_inline_rename(cx, compact);
        }
    }

    fn cancel_inline_rename(cx: &mut TestAppContext, compact: bool) {
        cx.update(crate::theme::init);
        cx.update(|cx| crate::window_seam::override_mobile_for_test(cx, compact));
        let root = crate::store::tests::scratch_root("sidebar-rename-test");
        let session_store = SessionStore::open_at(root.clone()).unwrap();
        let project = Project::from_root(root.clone());
        let mut meta = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
        meta.project_id = Some(project.id.clone());
        meta.title = "Original title".into();
        meta.id = "rename-thread".into();
        let session_id = meta.id.clone();
        let host =
            spawn_host(session_store, HostServices::default()).expect("spawn sidebar test host");
        smol::block_on(host.update_state_for_test(move |state, _| {
            state.projects = vec![project];
            state.sessions = vec![meta].into();
        }))
        .expect("seed sidebar host");
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        driver.wait_until(cx, &store, "sidebar baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });

        let window_state = cx.new(|_| WindowState::new(false));
        let sidebar = cx.new(|cx| SessionsSidebar::new(store.clone(), window_state.clone(), cx));
        let reveals = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let (_, cx) = cx.add_window_view(|window, cx| {
            let drawn = cx.new(|_| Drawn {
                sidebar: sidebar.clone(),
                row: "flat-thread-rename-thread",
                reveals: reveals.clone(),
            });
            gpui_base::Root::new(drawn, window, cx)
        });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(360.), px(800.)));
        window_state.update(cx, |state, cx| {
            state.set_compact(compact, cx);
            state.go(crate::window_state::Destination::Threads, cx);
        });
        draw(cx);
        let row = cx
            .debug_bounds(if compact {
                "compact-row-rename-thread"
            } else {
                "sidebar-thread-rename-thread"
            })
            .unwrap();
        let trigger = row.center();
        // How much of the list hover fill is painted over the row, every
        // layer of it counted.
        let held = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                let fill = cx.theme().list_hover;
                let bounds = row.scale(window.scale_factor());
                crate::test_frames::painted_quads(window)
                    .iter()
                    .filter(|quad| quad.bounds == bounds)
                    .filter_map(|quad| quad.background.as_solid())
                    .filter(|painted| painted.h == fill.h && painted.l == fill.l)
                    .map(|painted| painted.a / fill.a)
                    .sum::<f32>()
            })
        };
        if compact {
            let touch = |cx: &mut VisualTestContext, phase, position| {
                cx.simulate_event(gpui::TouchEvent {
                    id: gpui::TouchId(1),
                    phase,
                    position,
                    predicted_position: None,
                    force: None,
                });
                draw(cx);
            };
            touch(cx, gpui::TouchPhase::Started, trigger);
            cx.executor().advance_clock(
                gpui::GestureTuning::default().long_press_duration
                    + std::time::Duration::from_millis(1),
            );
            cx.run_until_parked();
            draw(cx);
            touch(cx, gpui::TouchPhase::Ended, trigger);
            crate::test_frames::draw_settled(cx);
            assert_eq!(
                held(cx),
                1.,
                "the row a long press opened the menu from stays marked once the finger lifts"
            );
            let rename = cx
                .debug_bounds("menu-item-2")
                .expect("thread menu opens")
                .center();
            touch(cx, gpui::TouchPhase::Started, rename);
            touch(cx, gpui::TouchPhase::Ended, rename);
            crate::test_frames::draw_settled(cx);
            assert_eq!(held(cx), 0., "the mark leaves with the menu");
            cx.simulate_input(" edited");
            assert_eq!(
                sidebar.read_with(cx, |sidebar, cx| sidebar
                    .renaming
                    .as_ref()
                    .expect("Rename starts editing")
                    .input
                    .read(cx)
                    .value()
                    .to_string()),
                "Original title edited",
                "Rename mounts a usable input on the phone"
            );
            let caret = sidebar.read_with(cx, |sidebar, cx| {
                sidebar
                    .renaming
                    .as_ref()
                    .unwrap()
                    .input
                    .read(cx)
                    .text_bounds()
                    .unwrap()
                    .center()
            });
            touch(cx, gpui::TouchPhase::Started, caret);
            touch(cx, gpui::TouchPhase::Ended, caret);
            assert_eq!(
                window_state.read_with(cx, |state, _| state.destination()),
                crate::window_state::Destination::Threads,
                "tapping the rename editor must not open the thread"
            );
            cx.simulate_click(caret, gpui::Modifiers::default());
            draw(cx);
            assert_eq!(
                window_state.read_with(cx, |state, _| state.destination()),
                crate::window_state::Destination::Threads,
                "clicking the rename editor must not open the thread"
            );
        } else {
            // How far the row's hover controls were in on the last frame.
            let revealed = || *reveals.borrow().last().unwrap();
            // The fill and the controls over each of the display frames that
            // follow.
            let frames = |cx: &mut VisualTestContext| {
                (0..24)
                    .map(|_| {
                        cx.executor()
                            .advance_clock(std::time::Duration::from_millis(16));
                        cx.update(|window, cx| window.simulate_next_frame(cx));
                        draw(cx);
                        (held(cx), revealed())
                    })
                    .collect::<Vec<_>>()
            };
            let pointer = |cx: &mut VisualTestContext, position| {
                cx.simulate_mouse_move(position, None, gpui::Modifiers::none());
            };
            let away = gpui::point(px(340.), px(760.));
            pointer(cx, trigger);
            crate::test_frames::draw_settled(cx);
            assert_eq!(held(cx), 1., "the row under the pointer takes its fill");
            let (button, modifiers) = (gpui::MouseButton::Right, gpui::Modifiers::none());
            cx.simulate_mouse_down(trigger, button, modifiers);
            cx.simulate_mouse_up(trigger, button, modifiers);
            assert_eq!(
                frames(cx),
                [(1., 1.); 24],
                "nothing moves as the menu opens under the pointer"
            );
            let item = cx
                .debug_bounds("menu-item-2")
                .expect("thread menu opens")
                .center();
            pointer(cx, item);
            assert_eq!(
                frames(cx),
                [(1., 1.); 24],
                "nothing moves as the pointer goes on to the menu"
            );
            pointer(cx, away);
            crate::test_frames::draw_settled(cx);
            assert_eq!(
                (held(cx), revealed()),
                (1., 1.),
                "the row stays marked with the pointer away"
            );
            pointer(cx, gpui::point(row.left() + px(4.), row.center().y));
            assert_eq!(
                frames(cx),
                [(1., 1.); 24],
                "the row back under the pointer takes no second fill, and its controls stay"
            );
            pointer(cx, away);
            crate::test_frames::draw_settled(cx);
            cx.simulate_click(away, gpui::Modifiers::default());
            let leaving = frames(cx);
            for part in [|frame: &(f32, f32)| frame.0, |frame: &(f32, f32)| frame.1] {
                let part = leaving.iter().map(part).collect::<Vec<_>>();
                assert!(
                    part.iter().any(|value| *value > 0. && *value < 1.)
                        && part.windows(2).all(|pair| pair[1] <= pair[0]),
                    "the row's hover fades out with the menu: {part:?}"
                );
            }
            crate::test_frames::draw_settled(cx);
            assert_eq!(
                (held(cx), revealed()),
                (0., 0.),
                "the mark leaves with the menu"
            );
        }
        cx.update(|window, cx| {
            sidebar.update(cx, |sidebar, cx| {
                if !compact {
                    sidebar.on_rename(&ThreadRename(session_id.clone()), window, cx);
                }
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

        driver.shutdown(cx, &store, &host);
        let _ = std::fs::remove_dir_all(root);
    }

    #[gpui::test]
    fn thread_rows_carry_the_provider_mark_until_the_setting_is_off(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let root = crate::store::tests::scratch_root("provider-mark");
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
            state.sessions = vec![codex, claude].into();
        }))
        .unwrap();
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        driver.wait_until(cx, &store, "sidebar baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });
        let window_state = cx.new(|_| WindowState::new(false));
        let (_, cx) =
            cx.add_window_view(|_, cx| SessionsSidebar::new(store.clone(), window_state, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(320.), px(600.)));
        store.update(cx, |store, cx| store.show_alone("codex-thread".into(), cx));
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
        let wait_for_rows =
            |cx: &mut VisualTestContext, driver: &mut crate::store::tests::LocalHostDriver| {
                let deadline = crate::store::tests::host_deadline();
                loop {
                    driver.step(cx, &store);
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

        let set_marks =
            |cx: &mut VisualTestContext,
             enabled: bool,
             driver: &mut crate::store::tests::LocalHostDriver| {
                store.update(cx, |store, _| store.set_sidebar_provider_marks(enabled));
                let deadline = crate::store::tests::host_deadline();
                while store.read_with(cx, |store, _| store.settings().sidebar_provider_marks)
                    != enabled
                {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "setting never replicated"
                    );
                    driver.step(cx, &store);
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            };

        // Off by default: rows render without a mark.
        wait_for_rows(cx, &mut driver);
        for (_, mark_selector) in selectors {
            assert!(cx.debug_bounds(mark_selector).is_none());
        }

        set_marks(cx, true, &mut driver);
        let rows = wait_for_rows(cx, &mut driver);
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
        draw_settled(cx);

        set_marks(cx, false, &mut driver);
        wait_for_rows(cx, &mut driver);
        for (_, mark_selector) in selectors {
            assert!(
                cx.debug_bounds(mark_selector).is_none(),
                "with marks off again no mark is drawn"
            );
        }
        driver.shutdown(cx, &store, &host);
        let _ = std::fs::remove_dir_all(root);
    }

    #[gpui::test]
    fn compact_menu_dismissal_preserves_selection_and_clears_row_surface(cx: &mut TestAppContext) {
        use gpui::{PlatformInput, TouchEvent, TouchId, TouchPhase};
        cx.update(crate::theme::init);
        let root = crate::store::tests::scratch_root("menu-selection");
        let host = spawn_host(
            SessionStore::open_at(root.clone()).unwrap(),
            HostServices::default(),
        )
        .unwrap();
        let project = Project::from_root(root.clone());
        let sessions: Vec<_> = (0..12)
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
            state.sessions = sessions.into();
        }))
        .unwrap();
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        crate::store::tests::place_shown_threads(&store, cx);
        driver.wait_until(cx, &store, "sidebar baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });
        let navigation = cx.new(|_| WindowState::new(false).with_compact(true));
        let (_, cx) =
            cx.add_window_view(|_, cx| SessionsSidebar::new(store.clone(), navigation.clone(), cx));
        cx.simulate_resize(size(px(393.), px(852.)));
        draw(cx);
        // A remote host may not have sent status yet. Selection must repaint
        // from the client notification without waiting for a host event.
        driver.shutdown(cx, &store, &host);
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
        let selected = store.read_with(cx, |store, _| store.open_threads());
        let destination = navigation.read_with(cx, |state, _| state.destination());
        send(cx, 2, TouchPhase::Started, outside);
        send(cx, 2, TouchPhase::Ended, outside);
        draw(cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        cx.update(|window, cx| window.simulate_next_frame(cx));
        draw(cx);
        assert!(cx.debug_bounds("tcode-popup-menu").is_none());
        assert_eq!(
            store.read_with(cx, |store, _| store.open_threads()),
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
        draw_settled(cx);
        assert_eq!(
            store.read_with(cx, |store, _| store.open_threads()),
            ["menu-10"]
        );
        assert!(!selected_surface(cx, a));
        assert!(
            !selected_surface(cx, b),
            "a phone's list selects no row, the thread being another page"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    fn compact_list_has_project_headers(
        cx: &mut TestAppContext,
        projects: usize,
        quick_chat: bool,
    ) -> bool {
        cx.update(crate::theme::init);
        let root = crate::store::tests::scratch_root(&format!("sidebar-{projects}-project"));
        let host = spawn_host(
            SessionStore::open_at(root.clone()).unwrap(),
            HostServices::default(),
        )
        .expect("spawn sidebar test host");
        let seeded: Vec<Project> = (0..projects)
            .map(|index| {
                let mut project = Project::from_root(root.join(format!("project-{index}")));
                project.quick_chat = quick_chat && index == 0;
                project
            })
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
            state.sessions = sessions.into();
        }))
        .expect("seed projects");

        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        driver.wait_until(cx, &store, "sidebar baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });
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
                    if quick_chat {
                        "compact-project-Quick thread"
                    } else {
                        "compact-project-project-0"
                    }
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
        let deadline = crate::store::tests::host_deadline();
        loop {
            driver.step(cx, &store);
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
        // The list the new layout brings folds its rows in; the reader
        // presses a header once it is there.
        draw_settled(cx);
        let headers = cx.debug_bounds("compact-group-header").is_some();
        if quick_chat {
            assert!(store.read_with(cx, |store, _| {
                store.projects().iter().all(|p| !p.quick_chat)
            }));
            assert!(store.read_with(cx, |store, _| {
                store
                    .grouped_sessions()
                    .iter()
                    .any(|g| g.project.quick_chat)
            }));
            assert!(cx.debug_bounds("compact-group-Quick threads").is_some());
        }
        if projects > 1 {
            for collapsed in [true, false] {
                let header = cx.debug_bounds("compact-group-header").unwrap();
                cx.simulate_click(header.center(), gpui::Modifiers::default());
                let deadline = crate::store::tests::host_deadline();
                loop {
                    driver.step(cx, &store);
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
                draw_settled(cx);
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

        driver.shutdown(cx, &store, &host);
        let persisted = tcode_services::settings::SettingsStore::new(root.clone())
            .try_load()
            .unwrap();
        assert_eq!(
            persisted.sidebar_layout,
            SidebarLayout::Grouped,
            "the actual header action persists the shared choice"
        );
        let _ = std::fs::remove_dir_all(root);
        headers
    }

    #[gpui::test]
    fn compact_recent_orders_projects_and_switches_to_persisted_grouped_view(
        cx: &mut TestAppContext,
    ) {
        assert!(
            !compact_list_has_project_headers(cx, 1, false),
            "a single project needs no header to separate it from anything"
        );
        assert!(
            compact_list_has_project_headers(cx, 2, false),
            "two projects need their headers back"
        );
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
        assert!(compact_list_has_project_headers(cx, 1, true));
        assert!(compact_list_has_project_headers(cx, 2, true));
    }

    #[gpui::test]
    fn thread_list_scrollbar_drags_without_selecting_a_thread(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        cx.update(crate::theme::init);
        let root = crate::store::tests::scratch_root("sidebar-scrollbar");
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
                .collect::<Vec<_>>()
                .into();
            state.projects = vec![project];
        }))
        .unwrap();
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        driver.wait_until(cx, &store, "sidebar baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });
        // The host sends its index from its own threads; wait for every
        // thread, as the app's event pump would, before judging the list.
        driver.wait_until(cx, &store, "every thread", |cx| {
            store.read_with(cx, |store, _| store.flat_sessions().len() == 60)
        });
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
            let selected = store.read_with(cx, |store, _| store.open_threads());
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
                store.read_with(cx, |store, _| store.open_threads()),
                selected,
                "dragging must not activate a thread under the scrollbar"
            );
        }
        driver.shutdown(cx, &store, &host);
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
                .pump_with_timer(|delay| executor.timer(delay))
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
        store.update(cx, |store, cx| store.show_alone("virtual-0".into(), cx));
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

    /// The sidebar's share item puts the project in a space on the machine the
    /// window shows, and takes it out again, and the project is marked shared
    /// while it is in one. The machine answers over a scripted transport.
    #[gpui::test]
    fn the_share_item_toggles_the_project_in_a_space_on_its_machine(cx: &mut TestAppContext) {
        use tcode_protocol::{
            ClientPayload, CommandResponse, EventEnvelope, HostMessage, HostingAction,
            HostingState, IndexSnapshot, Query, QueryResponse, ServerEvent, SpaceAction, SpaceInfo,
            Topic, decode_client_line, encode_line,
        };
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
        cx.update(crate::theme::init);
        let (to_host, outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let event = |topic, event| {
            encode_line(&HostMessage::Event(EventEnvelope {
                request_id: None,
                topic,
                event,
            }))
            .unwrap()
        };
        let mut project = Project::from_root(PathBuf::from("/a"));
        project.id = "a".into();
        let mut thread = session("thread", None);
        thread.project_id = Some(project.id.clone());
        for line in [
            event(
                Topic::Settings,
                ServerEvent::SettingsSnapshot(tcode_core::settings::Settings {
                    sidebar_layout: SidebarLayout::Grouped,
                    ..Default::default()
                }),
            ),
            event(
                Topic::Index,
                ServerEvent::IndexSnapshot(IndexSnapshot {
                    summary: Default::default(),
                    sessions: vec![thread],
                    projects: vec![project],
                }),
            ),
        ] {
            incoming.try_send(line).unwrap();
        }
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump_link = link.clone();
        let executor = cx.background_executor.clone();
        let _pump = cx.background_executor.spawn(async move {
            pump_link
                .pump_with_timer(|delay| executor.timer(delay))
                .await;
        });
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Remote {
                    host_id: "studio".into(),
                    host_name: "Studio".into(),
                },
                None,
                None,
                false,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        let window_state = cx.new(|_| WindowState::new(false));
        let (sidebar, cx) =
            cx.add_window_view(|_, cx| SessionsSidebar::new(store.clone(), window_state, cx));
        cx.simulate_resize(size(px(320.), px(900.)));

        // The machine's spaces: one, holding the projects it was last given.
        let mut shared: Vec<String> = Vec::new();
        let mut asked: Vec<Vec<String>> = Vec::new();
        let mut serve = |cx: &mut VisualTestContext| {
            for _ in 0..4 {
                cx.run_until_parked();
                store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
                while let Ok(line) = outgoing.try_recv() {
                    let message = decode_client_line(&line).unwrap();
                    let state = |shared: &Vec<String>| HostingState {
                        created_space_id: None,
                        spaces: vec![SpaceInfo {
                            id: "design".into(),
                            name: "Design".into(),
                            created_unix: 1,
                            project_ids: shared.clone(),
                            link: None,
                            link_enabled: true,
                            link_dead: false,
                            members: vec![],
                        }],
                        enabled: true,
                        expires_in_secs: 0,
                        host_id: "studio".into(),
                        host_name: "Studio".into(),
                        invite: None,
                        devices: vec![],
                        traverse: vec![],
                        group: None,
                    };
                    let reply = match message.payload {
                        ClientPayload::Subscribe(subscription)
                            if subscription.topic == Topic::Hosting =>
                        {
                            HostMessage::Event(EventEnvelope {
                                request_id: Some(message.id),
                                topic: Topic::Hosting,
                                event: ServerEvent::HostingReplaced(Box::new(state(&shared))),
                            })
                        }
                        ClientPayload::Query(Query::Hosting { action }) => {
                            if let HostingAction::Spaces(SpaceAction::SetProjects {
                                project_ids,
                                ..
                            }) = action
                            {
                                asked.push(project_ids.clone());
                                shared = project_ids;
                            }
                            HostMessage::QueryResult {
                                id: message.id,
                                result: Ok(QueryResponse::Hosting(state(&shared))),
                            }
                        }
                        ClientPayload::Subscribe(_) | ClientPayload::Command(_) => {
                            HostMessage::Ack {
                                id: message.id,
                                result: Ok(CommandResponse::Unit),
                            }
                        }
                        _ => continue,
                    };
                    incoming.try_send(encode_line(&reply).unwrap()).unwrap();
                }
            }
            draw(cx);
        };
        serve(cx);
        assert!(cx.debug_bounds("project-header-a").is_some());
        assert!(cx.debug_bounds("project-shared-a").is_none());

        let share = spaces::ToggleShare {
            space_id: "design".into(),
            project_id: "a".into(),
        };
        sidebar.update_in(cx, |sidebar, window, cx| {
            sidebar.on_toggle_share(&share, window, cx)
        });
        serve(cx);
        assert!(
            cx.debug_bounds("project-shared-a").is_some(),
            "a shared project is marked"
        );

        sidebar.update_in(cx, |sidebar, window, cx| {
            sidebar.on_toggle_share(&share, window, cx)
        });
        serve(cx);
        assert!(cx.debug_bounds("project-shared-a").is_none());
        drop(serve);
        assert_eq!(asked, [vec!["a".to_string()], vec![]]);
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
                .pump_with_timer(|delay| executor.timer(delay))
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
        store.update(cx, |store, cx| store.show_alone("virtual-0".into(), cx));
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
        assert!(cx.debug_bounds("sidebar-thread-virtual-6").is_none());
        let more = cx.debug_bounds("show-more-project").unwrap();
        cx.simulate_click(more.center(), gpui::Modifiers::default());
        draw_settled(cx);
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
    fn agents_are_not_listed(cx: &mut TestAppContext) {
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
                .pump_with_timer(|delay| executor.timer(delay))
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
        store.update(cx, |store, cx| store.show_alone("other".into(), cx));
        let window_state = cx.new(|_| WindowState::new(false).with_compact(true));
        let (_, cx) = cx
            .add_window_view(|_, cx| SessionsSidebar::new(store.clone(), window_state.clone(), cx));
        cx.simulate_resize(size(px(393.), px(852.)));
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        draw(cx);
        assert!(cx.debug_bounds("compact-row-parent").is_some());
        assert!(cx.debug_bounds("compact-row-child").is_none());
        assert!(cx.debug_bounds("compact-row-mirror").is_none());
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
                meta.updated_at = meta.created_at;
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
                .pump_with_timer(|delay| executor.timer(delay))
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
        crate::shell::tests::shell_behind(&store, cx);
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        store.update(cx, |store, cx| store.show_thread("settled-39".into(), cx));
        let window_state = cx.new(|_| WindowState::new(false));
        let (sidebar, cx) = cx
            .add_window_view(|_, cx| SessionsSidebar::new(store.clone(), window_state.clone(), cx));
        cx.simulate_resize(size(px(393.), px(6000.)));
        for compact in [false, true] {
            window_state.update(cx, |state, cx| {
                state.compact = compact;
                cx.notify();
            });
            cx.run_until_parked();
            store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            sidebar.update(cx, |sidebar, cx| {
                sidebar.expanded_settled.clear();
                sidebar.settled_limits.clear();
                sidebar.compact_model_dirty = true;
                cx.notify();
            });
            draw(cx);
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
                assert_eq!(ids, expected(limit), "compact={compact}");
                let mut previous = None;
                for id in ids {
                    let bounds = cx
                        .debug_bounds(selector(&id))
                        .unwrap_or_else(|| panic!("missing visible row {id}"));
                    if let Some(previous) = previous {
                        assert!(
                            bounds.top() > previous,
                            "row {id}: top {:?} <= previous {:?}; compact={compact} limit={limit}",
                            bounds.top(),
                            previous
                        );
                    }
                    previous = Some(bounds.top());
                }
            };
            let click = |cx: &mut VisualTestContext, selector: &'static str| {
                let bounds = cx.debug_bounds(selector).unwrap();
                cx.simulate_click(bounds.center(), gpui::Modifiers::default());
                draw(cx);
            };
            verify(cx, 0);
            assert!(cx.debug_bounds(selector("settled-0")).is_none());
            click(cx, "settled-recent");
            verify(cx, 10);
            assert!(cx.debug_bounds(selector("settled-10")).is_none());
            click(cx, "settled-more-recent");
            verify(cx, 35);
            assert!(cx.debug_bounds(selector("settled-35")).is_none());
            click(cx, "settled-recent");
            verify(cx, 0);
            click(cx, "settled-recent");
            verify(cx, 10);
            if !compact {
                // Only the desktop's flat list has a project filter.
                sidebar.update(cx, |sidebar, cx| {
                    sidebar.project_filter = Some("missing-project".into());
                    cx.notify();
                });
                draw(cx);
                assert!(
                    sidebar
                        .update(cx, |sidebar, cx| sidebar.navigation_threads(cx))
                        .is_empty()
                );
                sidebar.update(cx, |sidebar, cx| {
                    sidebar.project_filter = None;
                    cx.notify();
                });
                draw(cx);
                verify(cx, 0);
                click(cx, "settled-recent");
            }
            verify(cx, 10);
        }
    }

    /// Opening or closing the Settled shelf, selecting a row and activity
    /// lifting the row at the top edge leave a scrolled threads list exactly
    /// where it was, including when the selection takes a viewed settled row
    /// out from under the closed shelf.
    #[gpui::test]
    fn the_threads_list_keeps_its_offset_across_the_shelf_and_selection(cx: &mut TestAppContext) {
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
        let sessions = (0..40)
            .map(|index| {
                let mut meta = session(&format!("active-{index}"), None);
                meta.project_id = Some(project.id.clone());
                meta.updated_at = 1_000 - index;
                meta
            })
            .chain((0..30).map(|index| {
                let mut meta = session(&format!("settled-{index}"), None);
                meta.project_id = Some(project.id.clone());
                meta.settled_at = Some(500 - index);
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
                projects: vec![project],
                summary: Default::default(),
            }),
        );
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump_link = link.clone();
        let executor = cx.background_executor.clone();
        let _pump = cx.background_executor.spawn(async move {
            pump_link
                .pump_with_timer(|delay| executor.timer(delay))
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
        crate::store::tests::place_shown_threads(&store, cx);
        crate::shell::tests::shell_behind(&store, cx);
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        // The open thread is settled, so the closed shelf shows its row.
        store.update(cx, |store, cx| store.show_thread("settled-20".into(), cx));
        let window_state = cx.new(|_| WindowState::new(false));
        let (sidebar, cx) = cx
            .add_window_view(|_, cx| SessionsSidebar::new(store.clone(), window_state.clone(), cx));
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        cx.simulate_resize(size(px(393.), px(852.)));
        draw(cx);
        let list = sidebar.read_with(cx, |sidebar, _| sidebar.flat_list_state.clone());
        list.scroll_to(gpui::ListOffset {
            item_ix: 30,
            offset_in_item: px(20.),
        });
        sidebar.update(cx, |_, cx| cx.notify());
        draw(cx);
        let offset = list.logical_scroll_top();
        let first = |cx: &mut VisualTestContext| {
            sidebar.read_with(cx, |sidebar, _| {
                sidebar.flat_row_keys[sidebar.flat_list_state.logical_scroll_top().item_ix].clone()
            })
        };
        let top_row = first(cx);
        let unchanged = |cx: &mut VisualTestContext, after: &str| {
            assert_eq!(first(cx), top_row, "the top row after {after}");
            assert_eq!(
                list.logical_scroll_top().offset_in_item,
                offset.offset_in_item,
                "the offset after {after}"
            );
        };
        let click = |cx: &mut VisualTestContext, selector: &'static str| {
            let bounds = cx.debug_bounds(selector).unwrap();
            cx.simulate_click(bounds.center(), gpui::Modifiers::default());
            draw(cx);
        };

        click(cx, "settled-recent");
        assert!(cx.debug_bounds("sidebar-thread-settled-0").is_some());
        unchanged(cx, "opening the shelf");
        click(cx, "settled-recent");
        assert!(cx.debug_bounds("sidebar-thread-settled-0").is_none());
        unchanged(cx, "closing the shelf");

        // Beyond the shelf's first page, the open thread shows after it.
        click(cx, "settled-recent");
        let listed = |cx: &mut VisualTestContext| {
            sidebar.update(cx, |sidebar, cx| sidebar.navigation_threads(cx))
        };
        assert!(listed(cx).contains(&"settled-20".to_string()));
        click(cx, "sidebar-thread-active-35");
        assert_eq!(
            store.read_with(cx, |store, _| store.open_threads()),
            ["active-35"]
        );
        assert!(
            !listed(cx).contains(&"settled-20".to_string()),
            "the settled row left with the selection"
        );
        unchanged(cx, "selecting a row");

        // A message lifts the thread at the top edge to the head of Active;
        // the list stays where it was instead of following it there.
        let (kind, lifted) = top_row.clone();
        assert_eq!(kind, "thread");
        let mut sessions = store.read_with(cx, |store, _| store.sidebar_sessions().to_vec());
        sessions
            .iter_mut()
            .find(|meta| meta.id == lifted)
            .unwrap()
            .updated_at = 2_000;
        send(
            Topic::Index,
            ServerEvent::IndexSnapshot(IndexSnapshot {
                sessions,
                projects: store.read_with(cx, |store, _| store.projects()),
                summary: Default::default(),
            }),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        draw(cx);
        assert_eq!(
            sidebar.update(cx, |sidebar, cx| sidebar.navigation_threads(cx))[0],
            lifted
        );
        let top = list.logical_scroll_top();
        assert_eq!(
            (top.item_ix, top.offset_in_item),
            (offset.item_ix, offset.offset_in_item),
            "the lifted row is not followed"
        );
    }

    /// The project view lists a project's pinned and active roots as one list
    /// by last update and ends it in a closed "Settled N" shelf, newest
    /// settled first, that opens per project and opens for a selected settled
    /// thread. Projects order by the activity of every thread in them, agents
    /// included. A project shows six active rows until "Show more", whatever
    /// its shelf holds.
    #[gpui::test]
    fn the_project_view_keeps_a_settled_shelf_per_project(cx: &mut TestAppContext) {
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
        let projects = ["lead", "mixed", "many"].map(|id| {
            let mut project = Project::from_root(PathBuf::from(format!("/{id}")));
            project.id = id.into();
            project.quick_chat = id == "lead";
            project
        });
        let thread = |id: &str, project: &str, updated: u64| {
            let mut meta = session(id, None);
            meta.project_id = Some(project.into());
            meta.created_at = 1;
            meta.updated_at = updated;
            meta
        };
        let settled = |id: &str, project: &str, updated: u64, settled_at: u64| {
            let mut meta = thread(id, project, updated);
            meta.settled_at = Some(settled_at);
            meta
        };
        let mut pinned = thread("lead-pinned", "lead", 50);
        pinned.pinned_at = Some(50);
        pinned.pin_order = Some("n".into());
        let mut agent = thread("lead-agent", "lead", 900);
        agent.parent_session_id = Some("lead-settled-0".into());
        let mut arranged = thread("mixed-b", "mixed", 100);
        arranged.active_order = Some("b".into());
        let mut sessions = vec![
            settled("lead-settled-0", "lead", 60, 60),
            pinned,
            settled("lead-settled-1", "lead", 30, 70),
            settled("lead-settled-2", "lead", 20, 20),
            agent,
            thread("mixed-a", "mixed", 300),
            settled("mixed-old", "mixed", 200, 200),
            arranged,
        ];
        sessions.extend((0..8).map(|index| thread(&format!("many-{index}"), "many", 10 + index)));
        // Newer than every active row in their project, so a limit that
        // counted them would take active rows' places.
        sessions.push(settled("many-settled-0", "many", 41, 41));
        sessions.push(settled("many-settled-1", "many", 40, 40));
        send(
            Topic::Settings,
            ServerEvent::SettingsSnapshot(tcode_core::settings::Settings {
                sidebar_layout: SidebarLayout::Grouped,
                ..Default::default()
            }),
        );
        send(
            Topic::Index,
            ServerEvent::IndexSnapshot(IndexSnapshot {
                sessions,
                projects: projects.to_vec(),
                summary: Default::default(),
            }),
        );
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump_link = link.clone();
        let executor = cx.background_executor.clone();
        let _pump = cx.background_executor.spawn(async move {
            pump_link
                .pump_with_timer(|delay| executor.timer(delay))
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
        crate::shell::tests::shell_behind(&store, cx);
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        store.update(cx, |store, cx| store.show_thread("mixed-a".into(), cx));
        let window_state = cx.new(|_| WindowState::new(false));
        let (sidebar, cx) = cx
            .add_window_view(|_, cx| SessionsSidebar::new(store.clone(), window_state.clone(), cx));
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        cx.simulate_resize(size(px(393.), px(2000.)));
        let ids = |ids: &[&str]| ids.iter().map(|id| id.to_string()).collect::<Vec<_>>();
        let lead = |open: bool| {
            let mut rows = ids(&["lead-pinned"]);
            if open {
                rows.extend(ids(&["lead-settled-1", "lead-settled-0", "lead-settled-2"]));
            }
            rows
        };
        let mixed = ids(&["mixed-a", "mixed-b"]);
        let many = |shown: u64, open: bool| {
            let mut rows = (0..shown)
                .map(|index| format!("many-{}", 7 - index))
                .collect::<Vec<_>>();
            if open {
                rows.extend(ids(&["many-settled-0", "many-settled-1"]));
            }
            rows
        };
        let expected = |shown: u64, lead_open: bool, many_open: bool| {
            lead(lead_open)
                .into_iter()
                .chain(mixed.clone())
                .chain(many(shown, many_open))
                .collect::<Vec<_>>()
        };
        let click = |cx: &mut VisualTestContext, selector: &'static str| {
            let bounds = cx.debug_bounds(selector).unwrap();
            cx.simulate_click(bounds.center(), gpui::Modifiers::default());
            draw_settled(cx);
        };
        for compact in [false, true] {
            store.update(cx, |store, cx| store.show_thread("mixed-a".into(), cx));
            window_state.update(cx, |state, cx| {
                state.compact = compact;
                cx.notify();
            });
            sidebar.update(cx, |sidebar, cx| {
                sidebar.expanded_settled.clear();
                sidebar.compact_model_dirty = true;
                cx.notify();
            });
            draw_settled(cx);
            let shelves = sidebar.update(cx, |sidebar, cx| {
                if compact {
                    sidebar
                        .compact_model(None, cx)
                        .rows
                        .iter()
                        .filter_map(|row| match row {
                            CompactListRow::Settled { key, count } => Some((key.clone(), *count)),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                } else {
                    let store = sidebar.store.read(cx);
                    let groups = store.grouped_sessions();
                    sidebar
                        .grouped_rows(&groups, store)
                        .into_iter()
                        .filter_map(|(row, _)| match row {
                            GroupedListRow::Settled { group, count } => {
                                Some((groups[group].project.id.clone(), count))
                            }
                            _ => None,
                        })
                        .collect()
                }
            });
            assert_eq!(
                shelves,
                [("lead".into(), 3), ("mixed".into(), 1), ("many".into(), 2)],
                "compact={compact}"
            );
            let verify = |cx: &mut VisualTestContext, ids: Vec<String>| {
                assert_eq!(
                    sidebar.update(cx, |sidebar, cx| sidebar.navigation_threads(cx)),
                    ids,
                    "compact={compact}"
                );
                let mut previous = None;
                for id in ids {
                    let selector = if compact {
                        format!("compact-row-{id}")
                    } else {
                        format!("sidebar-thread-{id}")
                    };
                    let top = cx.debug_bounds(selector.leak()).unwrap().top();
                    assert!(previous.is_none_or(|previous| top > previous), "{id}");
                    previous = Some(top);
                }
            };
            assert!(
                cx.debug_bounds(if compact {
                    "compact-group-Quick threads"
                } else {
                    "quick-chats-header"
                })
                .is_some()
            );
            assert!(cx.debug_bounds("project-header-lead").is_none());
            let shelf = cx.debug_bounds("settled-lead").unwrap().top();
            let pinned = if compact {
                "compact-row-lead-pinned"
            } else {
                "sidebar-thread-lead-pinned"
            };
            assert!(cx.debug_bounds(pinned).unwrap().top() < shelf);
            assert!(cx.debug_bounds("settled-mixed").is_some());
            let shown = if compact {
                // The phone lists every active row of a project.
                8
            } else {
                verify(cx, expected(6, false, false));
                click(cx, "show-more-many");
                8
            };
            verify(cx, expected(shown, false, false));
            click(cx, "settled-lead");
            verify(cx, expected(shown, true, false));
            if compact {
                // A shelf row shows when it settled, an active row its last update.
                let shown_times = sidebar.update(cx, |sidebar, cx| {
                    sidebar
                        .compact_model(None, cx)
                        .rows
                        .iter()
                        .filter_map(|row| match row {
                            CompactListRow::Thread(row)
                                if ["lead-settled-1", "mixed-a"]
                                    .contains(&row.meta.id.as_str()) =>
                            {
                                Some((row.meta.id.clone(), row.timestamp))
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                });
                assert_eq!(
                    shown_times,
                    [("lead-settled-1".into(), 70), ("mixed-a".into(), 300)]
                );
            }
            click(cx, "settled-lead");
            verify(cx, expected(shown, false, false));
            store.update(cx, |store, cx| {
                store.show_thread("many-settled-1".into(), cx);
            });
            draw_settled(cx);
            verify(cx, expected(shown, false, true));

            send(
                Topic::Settings,
                ServerEvent::SettingsSnapshot(tcode_core::settings::Settings {
                    sidebar_layout: SidebarLayout::Grouped,
                    collapsed_projects: vec!["mixed".into()],
                    ..Default::default()
                }),
            );
            cx.run_until_parked();
            store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            draw_settled(cx);
            assert_eq!(
                sidebar.update(cx, |sidebar, cx| sidebar.navigation_threads(cx)),
                lead(false)
                    .into_iter()
                    .chain(many(8, true))
                    .collect::<Vec<_>>(),
                "a collapsed project lists nothing; compact={compact}"
            );
            send(
                Topic::Settings,
                ServerEvent::SettingsSnapshot(tcode_core::settings::Settings {
                    sidebar_layout: SidebarLayout::Grouped,
                    ..Default::default()
                }),
            );
            cx.run_until_parked();
            store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        }
    }

    /// A Settled shelf opened at the foot of a list taller than the view
    /// folds its rows in below its header, and scrolling down then shows
    /// each of them at a thread row's full height: in both layouts, on the
    /// desktop and on the phone.
    #[gpui::test]
    fn a_shelf_opened_at_the_foot_of_a_long_list_scrolls_to_its_rows(cx: &mut TestAppContext) {
        use tcode_protocol::{
            EventEnvelope, HostMessage, IndexSnapshot, ServerEvent, Topic, encode_line,
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
        let projects: Vec<Project> = (0..12)
            .map(|index| {
                let mut project = Project::from_root(PathBuf::from(format!("/p{index:02}")));
                project.id = format!("p{index:02}");
                project
            })
            .collect();
        let mut sessions = Vec::new();
        for (index, project) in projects.iter().enumerate() {
            for row in 0..3 {
                let mut meta = session(&format!("{}-{row}", project.id), None);
                meta.project_id = Some(project.id.clone());
                meta.created_at = 1000 - 10 * index as u64 - row;
                meta.updated_at = meta.created_at;
                sessions.push(meta);
            }
        }
        // All in the last project, so its shelf is at the foot of either list.
        sessions.extend((0..12).map(|index| {
            let mut meta = session(&format!("settled-{index}"), None);
            meta.project_id = Some("p11".into());
            meta.created_at = 1;
            meta.updated_at = 1;
            meta.settled_at = Some(100 - index);
            meta
        }));
        let settings = |layout| {
            ServerEvent::SettingsSnapshot(tcode_core::settings::Settings {
                sidebar_layout: layout,
                ..Default::default()
            })
        };
        send(Topic::Settings, settings(SidebarLayout::Flat));
        send(
            Topic::Index,
            ServerEvent::IndexSnapshot(IndexSnapshot {
                sessions,
                projects,
                summary: Default::default(),
            }),
        );
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump_link = link.clone();
        let executor = cx.background_executor.clone();
        let _pump = cx.background_executor.spawn(async move {
            pump_link
                .pump_with_timer(|delay| executor.timer(delay))
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
        let window_state = cx.new(|_| WindowState::new(false));
        let (sidebar, cx) = cx
            .add_window_view(|_, cx| SessionsSidebar::new(store.clone(), window_state.clone(), cx));
        cx.update(|_, cx| cx.set_reduce_motion(false));
        cx.simulate_resize(size(px(393.), px(700.)));
        // Wheels down until the list stops moving: its foot.
        let scroll_down = |list: &ListState, cx: &mut VisualTestContext| {
            let position = gpui::point(px(196.), px(400.));
            cx.simulate_mouse_move(position, None, gpui::Modifiers::default());
            for _ in 0..20 {
                let top = list.logical_scroll_top();
                cx.simulate_event(gpui::ScrollEvent {
                    position,
                    delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-2000.))),
                    ..Default::default()
                });
                crate::test_frames::draw_settled(cx);
                if list.logical_scroll_top() == top {
                    return;
                }
            }
            panic!("the list keeps scrolling");
        };
        for layout in [SidebarLayout::Flat, SidebarLayout::Grouped] {
            send(Topic::Settings, settings(layout));
            cx.run_until_parked();
            store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            let key = match layout {
                SidebarLayout::Flat => "recent",
                SidebarLayout::Grouped => "p11",
            };
            for compact in [false, true] {
                window_state.update(cx, |state, cx| {
                    state.compact = compact;
                    cx.notify();
                });
                sidebar.update(cx, |sidebar, cx| {
                    sidebar.expanded_settled.clear();
                    sidebar.settled_limits.clear();
                    sidebar.compact_model_dirty = true;
                    cx.notify();
                });
                crate::test_frames::draw_settled(cx);
                let list = sidebar.read_with(cx, |sidebar, _| match (compact, layout) {
                    (true, _) => sidebar.compact_list_state.clone(),
                    (false, SidebarLayout::Flat) => sidebar.flat_list_state.clone(),
                    (false, SidebarLayout::Grouped) => sidebar.grouped_list_state.clone(),
                });
                let row = |id: &str| -> &'static str {
                    if compact {
                        format!("compact-row-{id}")
                    } else {
                        format!("sidebar-thread-{id}")
                    }
                    .leak()
                };
                let case = format!("{layout:?} compact={compact}");
                scroll_down(&list, cx);
                let height = cx
                    .debug_bounds(row("p11-2"))
                    .unwrap_or_else(|| panic!("the last active row is in view; {case}"))
                    .size
                    .height;
                let header = cx
                    .debug_bounds(format!("settled-{key}").leak())
                    .unwrap_or_else(|| panic!("the shelf's header is in view; {case}"));
                cx.simulate_click(header.center(), gpui::Modifiers::default());
                crate::test_frames::draw_settled(cx);
                scroll_down(&list, cx);
                let bottom = cx.update(|window, _| window.viewport_size().height);
                // The view's foot shows the shelf's last rows, each whole.
                let shown: Vec<usize> = (0..10)
                    .filter(|index| {
                        let id = format!("settled-{index}");
                        cx.debug_bounds(row(&id)).is_some_and(|bounds| {
                            assert!(
                                (bounds.size.height - height).abs() < px(1.),
                                "{id} is {:?} tall, a row {height:?}; {case}",
                                bounds.size.height
                            );
                            assert!(bounds.bottom() <= bottom, "{id} is in view; {case}");
                            true
                        })
                    })
                    .collect();
                assert!(
                    shown.last() == Some(&9) && shown.windows(2).all(|pair| pair[1] == pair[0] + 1),
                    "the shelf's rows up to its last are in view, not {shown:?}; {case}"
                );
                assert!(
                    cx.debug_bounds(format!("settled-more-{key}").leak())
                        .is_some_and(|more| more.bottom() <= bottom),
                    "the shelf's Show more is in view; {case}"
                );
            }
        }
    }

    /// A thread pinned from its menu while the list is scrolled down leads
    /// Pinned, and the list shows it as soon as the host's change arrives,
    /// on the desktop and on the phone. A row added above a scrolled list
    /// leaves the row at its top in place.
    #[gpui::test]
    fn a_thread_pinned_from_a_scrolled_list_leads_pinned_on_screen(cx: &mut TestAppContext) {
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
        let projects = ["alpha", "beta"].map(|id| {
            let mut project = Project::from_root(PathBuf::from(format!("/{id}")));
            project.id = id.into();
            project
        });
        let mut sessions: Vec<_> = (0..60)
            .map(|index| {
                let mut meta = session(&format!("thread-{index}"), None);
                meta.project_id = Some(projects[index as usize % 2].id.clone());
                meta.created_at = 1_000 - index;
                meta.updated_at = meta.created_at;
                meta
            })
            .collect();
        for (meta, key) in sessions.iter_mut().zip(["h", "n"]) {
            meta.pinned_at = Some(1);
            meta.pin_order = Some(key.into());
        }
        let snapshot = |sessions: &Vec<SessionMeta>| {
            encode_line(&HostMessage::Event(EventEnvelope {
                request_id: None,
                topic: Topic::Index,
                event: ServerEvent::IndexSnapshot(IndexSnapshot {
                    sessions: sessions.clone(),
                    projects: projects.to_vec(),
                    summary: Default::default(),
                }),
            }))
            .unwrap()
        };
        incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic: Topic::Settings,
                    event: ServerEvent::SettingsSnapshot(Default::default()),
                }))
                .unwrap(),
            )
            .unwrap();
        incoming.try_send(snapshot(&sessions)).unwrap();
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
        // The host side answers every command and records the pins.
        let mut driver = crate::store::tests::LocalHostDriver::from_link(link);
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let host_recorded = recorded.clone();
        let host_incoming = incoming.clone();
        std::thread::spawn(move || {
            while let Ok(line) = outgoing.recv_blocking() {
                let message = decode_client_line(&line).unwrap();
                if let ClientPayload::Command(command) = message.payload {
                    if let Command::PinSession {
                        session_id,
                        order_key,
                    } = command
                    {
                        host_recorded.lock().unwrap().push((session_id, order_key));
                    }
                    let ack = HostMessage::Ack {
                        id: message.id,
                        result: Ok(CommandResponse::Unit),
                    };
                    let _ = host_incoming.send_blocking(encode_line(&ack).unwrap());
                }
            }
        });
        driver.wait_until(cx, &store, "the threads", |cx| {
            store.read_with(cx, |store, _| !store.flat_sessions().is_empty())
        });
        let window_state = cx.new(|_| WindowState::new(false));
        let mut sidebar = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| SessionsSidebar::new(store.clone(), window_state.clone(), cx));
            sidebar = Some(view.clone());
            gpui_base::Root::new(view, window, cx)
        });
        let sidebar = sidebar.unwrap();
        cx.simulate_resize(size(px(393.), px(852.)));
        draw(cx);

        for (compact, id) in [(false, "thread-30"), (true, "thread-40")] {
            window_state.update(cx, |state, cx| {
                state.compact = compact;
                cx.notify();
            });
            sidebar.update(cx, |sidebar, _| sidebar.compact_model_dirty = true);
            draw(cx);
            let selector: &'static str = if compact {
                format!("compact-row-{id}")
            } else {
                format!("sidebar-thread-{id}")
            }
            .leak();
            let list = sidebar.read_with(cx, |sidebar, _| {
                if compact {
                    sidebar.compact_list_state.clone()
                } else {
                    sidebar.flat_list_state.clone()
                }
            });
            let target = if compact {
                sidebar.update(cx, |sidebar, cx| {
                    sidebar
                        .compact_model(None, cx)
                        .rows
                        .iter()
                        .position(
                            |row| matches!(row, CompactListRow::Thread(row) if row.meta.id == id),
                        )
                        .expect("target thread")
                })
            } else {
                28
            };
            list.scroll_to(gpui::ListOffset {
                item_ix: target,
                offset_in_item: px(0.),
            });
            sidebar.update(cx, |_, cx| cx.notify());
            draw(cx);
            assert!(cx.debug_bounds(selector).is_some(), "{id} is on screen");

            sidebar.update_in(cx, |sidebar, window, cx| {
                sidebar.on_pin(&ThreadPin(id.into()), window, cx)
            });
            driver.wait_until(cx, &store, "pin command reaches the host", |_| {
                !recorded.lock().unwrap().is_empty()
            });
            let pins = std::mem::take(&mut *recorded.lock().unwrap());
            let first = store.read_with(cx, |store, _| store.flat_sessions()[0].pin_order.clone());
            let [(pinned, Some(key))] = pins.as_slice() else {
                panic!("{pins:?}");
            };
            assert_eq!(pinned, id);
            assert!(key.as_str() < first.unwrap().as_str(), "{key} leads Pinned");
            let meta = sessions.iter_mut().find(|meta| meta.id == id).unwrap();
            meta.pinned_at = Some(2);
            meta.pin_order = Some(key.clone());
            incoming.try_send(snapshot(&sessions)).unwrap();
            driver.wait_until(cx, &store, "the pin", |cx| {
                store.read_with(cx, |store, _| {
                    store
                        .thread_meta(id)
                        .is_some_and(|meta| meta.pinned_at.is_some())
                })
            });
            draw(cx);

            let ids = sidebar.update(cx, |sidebar, cx| sidebar.navigation_threads(cx));
            assert_eq!(ids[0], id, "compact={compact}");
            let row = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("the pinned row is drawn; compact={compact}"));
            let viewport = list.viewport_bounds();
            assert!(
                row.top() >= viewport.top() && row.bottom() <= viewport.bottom(),
                "the pinned row {row:?} lies inside the list {viewport:?}; compact={compact}"
            );
        }

        // A thread started elsewhere adds a row above a scrolled list; the
        // row at the top of the viewport stays there.
        window_state.update(cx, |state, cx| {
            state.compact = false;
            cx.notify();
        });
        draw(cx);
        let list = sidebar.read_with(cx, |sidebar, _| sidebar.flat_list_state.clone());
        list.scroll_to(gpui::ListOffset {
            item_ix: 28,
            offset_in_item: px(0.),
        });
        sidebar.update(cx, |_, cx| cx.notify());
        draw(cx);
        let anchor = sidebar.read_with(cx, |sidebar, _| sidebar.flat_row_keys[28].clone());
        let mut started = session("started-elsewhere", None);
        started.project_id = Some("alpha".into());
        started.created_at = 2_000;
        started.updated_at = 2_000;
        sessions.push(started);
        incoming.try_send(snapshot(&sessions)).unwrap();
        driver.wait_until(cx, &store, "the new thread", |cx| {
            store.read_with(cx, |store, _| {
                store.thread_meta("started-elsewhere").is_some()
            })
        });
        draw(cx);
        sidebar.read_with(cx, |sidebar, _| {
            let top = sidebar.flat_list_state.logical_scroll_top().item_ix;
            assert_eq!(sidebar.flat_row_keys[top], anchor);
        });
    }

    /// Pinned rows lead in key order with keyless pins after them, Active
    /// rows nobody arranged follow by their last activity, newest first, and
    /// arranged rows keep their keys after them. Dragging an
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
        store.update(cx, |store, cx| store.show_alone("arranged".into(), cx));
        let mut driver = crate::store::tests::LocalHostDriver::from_link(link);
        // The host records commands on its own thread; the driver routes replies
        // while the UI awaits each acknowledgement, as in the running app.
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let host_recorded = recorded.clone();
        let host_incoming = incoming.clone();
        std::thread::spawn(move || {
            while let Ok(line) = outgoing.recv_blocking() {
                let message = decode_client_line(&line).unwrap();
                if matches!(
                    message.payload,
                    ClientPayload::Query(tcode_protocol::Query::Ping)
                ) {
                    let reply = HostMessage::QueryResult {
                        id: message.id,
                        result: Ok(tcode_protocol::QueryResponse::Pong),
                    };
                    let _ = host_incoming.send_blocking(encode_line(&reply).unwrap());
                }
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
        driver.wait_until(cx, &store, "the threads", |cx| {
            store.read_with(cx, |store, _| !store.flat_sessions().is_empty())
        });
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
        let sent = |cx: &mut VisualTestContext,
                    driver: &mut crate::store::tests::LocalHostDriver,
                    expected: usize| {
            driver.wait_until(cx, &store, "lifecycle commands reach the host", |_| {
                recorded.lock().unwrap().len() >= expected
            });
            // A FIFO reply also establishes absence after a cancelled gesture.
            let link = driver.link.clone();
            let mut barrier = std::pin::pin!(link.query(tcode_protocol::Query::Ping));
            driver.wait_until(cx, &store, "lifecycle command barrier", |_| {
                let mut task_cx = std::task::Context::from_waker(std::task::Waker::noop());
                match std::future::Future::poll(barrier.as_mut(), &mut task_cx) {
                    std::task::Poll::Ready(result) => {
                        result.expect("host barrier reply");
                        true
                    }
                    std::task::Poll::Pending => false,
                }
            });
            std::mem::take(&mut *recorded.lock().unwrap())
        };
        sent(cx, &mut driver, 0);
        draw(cx);
        let order = [
            "pin-a", "pin-b", "pin-c", "pin-new", "pin-old", "busy", "reopened", "fresh",
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
        let commands = sent(cx, &mut driver, 1);
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
        draw(cx);
        cx.simulate_keystrokes("escape");
        draw(cx);
        cx.simulate_mouse_up(target.center(), left, none);
        draw(cx);
        assert_eq!(sent(cx, &mut driver, 0), vec![], "Escape cancels the drag");

        sidebar.update_in(cx, |sidebar, window, cx| {
            sidebar.on_unpin(&ThreadUnpin("pin-a".into()), window, cx)
        });
        assert_eq!(
            sent(cx, &mut driver, 1),
            vec![Command::UnpinSession {
                session_id: "pin-a".into()
            }]
        );
        sidebar.update_in(cx, |sidebar, window, cx| sidebar.undo_lifecycle(window, cx));
        assert_eq!(
            sent(cx, &mut driver, 1),
            vec![Command::PinSession {
                session_id: "pin-a".into(),
                order_key: Some("f".into()),
            }]
        );

        sidebar.update_in(cx, |sidebar, window, cx| {
            sidebar.on_move(&ThreadMove("pin-a".into(), true), window, cx)
        });
        let commands = sent(cx, &mut driver, 1);
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
            sent(cx, &mut driver, 1),
            vec![Command::SettleSession {
                session_id: "pin-b".into()
            }]
        );
        sidebar.update_in(cx, |sidebar, window, cx| sidebar.undo_lifecycle(window, cx));
        assert_eq!(
            sent(cx, &mut driver, 2),
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
        driver.link.close();
    }
}
