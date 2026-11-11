use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    rc::Rc,
};

use crate::device::DevicePreferences;
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
use tcode_core::thread_sort::{
    ThreadSection, ThreadSections, merge_by_shown_time, partition_threads,
};
use tcode_protocol::{Command, ThreadExportFormat};

use tcode_core::{
    project::{ProjectGroup, SessionMeta},
    settings::SidebarLayout,
};

use crate::store::{HostKey, StoreChange, ThreadKey, TopicKind, WorkspaceStore};
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
    pub(crate) current: Option<ThreadKey>,
    pub(crate) shown: Vec<ThreadKey>,
    /// The column's panes, in the layout's order, with two or more on screen.
    pub(crate) map: PaneMap,
}

pub(crate) type PaneMap = Rc<[MapCell]>;

/// A pane of the column.
#[derive(Clone, PartialEq)]
pub(crate) struct MapCell {
    pub(crate) pane: gpui::EntityId,
    /// The row of its thread; a pane waiting for one has none.
    pub(crate) row: Option<ThreadKey>,
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
    fn cells(&self, row: &ThreadKey) -> impl Iterator<Item = &MapCell> {
        self.map
            .iter()
            .filter(move |cell| cell.row.as_ref() == Some(row))
    }

    fn light(&self, row: &ThreadKey) -> RowLight {
        if self.current.as_ref() == Some(row) {
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

/// The disclosures of a sidebar list: a host's section, a project's rows, a
/// project's rows past the first few, and a Settled shelf (by its shelf key).
#[derive(Clone, Copy)]
enum Fold {
    Host,
    Project,
    More,
    Shelf,
}

fn fold_key(fold: Fold, id: &str) -> String {
    match fold {
        Fold::Host => format!("host:{id}"),
        Fold::Project => format!("project:{id}"),
        Fold::More => format!("more:{id}"),
        Fold::Shelf => format!("shelf:{id}"),
    }
}

/// Whether a window over several hosts shows `store`'s heading alone and
/// offers nothing of it elsewhere: the host is out of reach, refused this
/// device or speaks another protocol, and its threads are not shown from what
/// the client last held.
pub(crate) fn host_down(store: &WorkspaceStore) -> bool {
    crate::remote::connection_failure(store.connection_state()).is_some()
}

/// A host whose threads the list shows, as a menu offers it.
struct HostProjects {
    host: HostKey,
    title: SharedString,
    /// Whether it holds its projects, not only a space.
    full: bool,
    projects: Vec<(ProjectKey, tcode_core::project::Project)>,
}

/// `menu` goes on to a host's items, under its name over several hosts.
fn host_section(
    menu: crate::widgets::menu::PopupMenu,
    sectioned: bool,
    title: &SharedString,
) -> crate::widgets::menu::PopupMenu {
    if sectioned {
        menu.separator().label(title.clone())
    } else {
        menu
    }
}

/// How element ids, debug selectors and row keys name `id` of `host` in a
/// list over `hosts` hosts: as it is over one, and after its host over
/// several, where a session or project id is unique only on its host.
pub(crate) fn named<'a>(hosts: usize, host: &HostKey, id: &'a str) -> Cow<'a, str> {
    if hosts <= 1 {
        return Cow::Borrowed(id);
    }
    match host {
        HostKey::Local => Cow::Owned(format!("local/{id}")),
        HostKey::Remote(host_id) => Cow::Owned(format!("{host_id}/{id}")),
    }
}

/// A project as the sidebar names it: a project id is unique only on its
/// host.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Deserialize)]
pub(crate) struct ProjectKey {
    pub(crate) host: HostKey,
    pub(crate) project_id: String,
}

/// What a list narrowed to `scope` shows of `host`: nothing when `scope` is
/// another host's project, else the project it names there, if any.
fn host_scope<'a>(scope: Option<&'a ProjectKey>, host: &HostKey) -> Option<Option<&'a str>> {
    match scope {
        None => Some(None),
        Some(project) if project.host == *host => Some(Some(project.project_id.as_str())),
        Some(_) => None,
    }
}

/// A Settled shelf: the threads view's, or a project's.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Shelf {
    Recent,
    Project(ProjectKey),
}

/// A thread a list shows: its host, and its name in the list's ids.
#[derive(Clone)]
struct ListedThread {
    host: HostKey,
    meta: SessionMeta,
    name: String,
}

impl ListedThread {
    fn key(&self) -> ThreadKey {
        ThreadKey {
            host: self.host.clone(),
            session_id: self.meta.id.clone(),
        }
    }
}

/// A thread of one host, as a list is put together.
type Listed<'a> = (&'a HostKey, &'a SessionMeta);

fn tagged<'a>(
    host: &'a HostKey,
    rows: Vec<&'a SessionMeta>,
) -> impl Iterator<Item = Listed<'a>> + use<'a> {
    rows.into_iter().map(move |meta| (host, meta))
}

fn tagged_sections<'a>(
    host: &'a HostKey,
    sections: ThreadSections<&'a SessionMeta>,
) -> ThreadSections<Listed<'a>> {
    ThreadSections {
        pinned: tagged(host, sections.pinned).collect(),
        active: tagged(host, sections.active).collect(),
        settled: tagged(host, sections.settled).collect(),
    }
}

/// A row key as a list kept it, to compare with one it draws now.
fn drawn_key<'a>((kind, id): &'a (&'static str, String)) -> (&'static str, Cow<'a, str>) {
    (kind, Cow::Borrowed(id.as_str()))
}

fn no_sections<T>() -> ThreadSections<T> {
    ThreadSections {
        pinned: vec![],
        active: vec![],
        settled: vec![],
    }
}

fn listed_meta<'a>(row: &'a Listed<'_>) -> &'a SessionMeta {
    row.1
}

/// Each host's sections, in the sidebar's order of hosts, as one list's.
fn merged<'a>(hosts: Vec<ThreadSections<Listed<'a>>>) -> ThreadSections<Listed<'a>> {
    let mut sections = no_sections::<Vec<Listed>>();
    for host in hosts {
        sections.pinned.push(host.pinned);
        sections.active.push(host.active);
        sections.settled.push(host.settled);
    }
    ThreadSections {
        pinned: merge_by_shown_time(sections.pinned, listed_meta),
        active: merge_by_shown_time(sections.active, listed_meta),
        settled: merge_by_shown_time(sections.settled, listed_meta),
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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ThreadFlags {
    unread: bool,
    waiting_for_approval: bool,
    waiting_for_input: bool,
    working: bool,
    /// The host refuses to settle the thread now.
    settle_blocked: bool,
    failed: bool,
    /// Pieces of background work and agents still running.
    background: (u32, u32),
    /// Dispatched agents finished and await the thread's settle.
    unsettled: bool,
    /// What wakes the thread later though nothing of it runs.
    wakes: tcode_protocol::ThreadWakes,
    /// The host can fork the thread's conversation, now or once it can.
    forks: bool,
}

#[derive(Clone)]
struct ThreadRowState {
    thread: ThreadKey,
    /// The thread's name in element ids ([`named`]).
    name: String,
    row_key: String,
    waiting_for_approval: bool,
    waiting_for_input: bool,
    /// The host refuses to settle the thread now.
    settle_blocked: bool,
    /// Pieces of background work and agents still running.
    background: (u32, u32),
    unsettled: bool,
    wakes: tcode_protocol::ThreadWakes,
    failed: bool,
    auto_settle_enabled: bool,
    is_worktree: bool,
    show_unread: bool,
    renaming: Option<Entity<InputState>>,
    menu_can_fork: bool,
    title_generating: bool,
}

impl ThreadRowState {
    fn waiting(&self) -> bool {
        self.waiting_for_approval || self.waiting_for_input
    }
}

/// What keeps a thread going, written out: how much of its background work
/// and how many of its agents run, and what will wake it.
fn keeps_going(state: &ThreadRowState) -> Option<String> {
    let (tasks, agents) = state.background;
    let mut parts = Vec::new();
    if tasks > 0 {
        parts.push(crate::tr!("sidebar.keeps_background", count = tasks).into_owned());
    }
    if agents > 0 {
        parts.push(crate::tr!("sidebar.keeps_agents", count = agents).into_owned());
    }
    let wakes = &state.wakes;
    parts.extend(
        wakes
            .pull_requests
            .iter()
            .map(|key| crate::tr!("background.keeps_watch", number = key.number).into_owned()),
    );
    if let Some(at) = wakes.scheduled_at {
        parts.push(
            crate::tr!(
                "sidebar.keeps_scheduled",
                time = crate::chat::model::format_local_time(at.saturating_mul(1000))
            )
            .into_owned(),
        );
    }
    if wakes.notify > 0 {
        parts.push(crate::tr!("sidebar.keeps_notify", count = wakes.notify).into_owned());
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
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

// Thread-row context-menu actions (each carries the target thread, so a
// single set of handlers on the sidebar root serves every row).
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadRename(ThreadKey);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadRegenerateTitle(ThreadKey);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadStopTitle(ThreadKey);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadFork(ThreadKey);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadMergeWorktree(ThreadKey);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadMarkUnread(ThreadKey);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadMoveToProject(ThreadKey);

#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = tcode_sidebar, no_json)]
struct ThreadLinkPullRequest(ThreadKey);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadCopyPath(ThreadKey);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadCopyId(ThreadKey);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadExportJsonl(ThreadKey);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadExportMarkdown(ThreadKey);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadArchive(ThreadKey);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadSettle(ThreadKey);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadAutoSettle(ThreadKey, bool);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadFollowUp(ThreadKey, bool);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadPark(ThreadKey, bool);

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode, no_json)]
pub(crate) struct ThreadUndo;

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadPin(ThreadKey);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadUnpin(ThreadKey);
/// Move a thread one place up (`false`) or down (`true`) within its section.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadMove(ThreadKey, bool);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadArrange(ThreadKey);

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
/// What reverses one undoable action, sent in order to its host.
struct UndoEntry {
    host: HostKey,
    commands: Vec<Command>,
    reopen: Option<String>,
}
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadMakeActive(ThreadKey);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadDelete(ThreadKey);

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_project, no_json)]
struct ProjectArchiveAll(ProjectKey);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_project, no_json)]
struct ProjectThreadRules(ProjectKey);

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_project, no_json)]
struct ProjectDelete(ProjectKey);

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Action)]
#[action(namespace = tcode_project, no_json)]
struct ChangeProjectIcon(ProjectKey);
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Action)]
#[action(namespace = tcode_project, no_json)]
struct ChangeProjectRoot(ProjectKey);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_project, no_json)]
struct ProjectReveal(ProjectKey);

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

/// Narrow the flat list to a project; `None` shows every project.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_sidebar, no_json)]
struct FilterProject(Option<ProjectKey>);
/// A new thread in a project, on the project's host.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_sidebar, no_json)]
pub(crate) struct StartDraftForProject(pub(crate) ProjectKey);
/// A quick thread on a host.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_sidebar, no_json)]
pub(crate) struct QuickChat(pub(crate) HostKey);
/// Prompt for a directory and make it a project of this host.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_sidebar, no_json)]
struct AddProject(HostKey);
/// Order this host's projects the next way.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_sidebar, no_json)]
struct SortProjects(HostKey);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_sidebar, no_json)]
struct ManageHosts;
/// Open Settings on this host.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_sidebar, no_json)]
struct HostSettings(HostKey);
/// Take this host out of the window's workspace.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_sidebar, no_json)]
struct LeaveHost(HostKey);

/// What a host's heading offers: a thread in one of its projects or a quick
/// thread, a new project, its settings, its projects' order, leaving the
/// workspace, and the Machines page.
#[derive(Clone)]
struct HostMenu {
    host: HostKey,
    /// Its projects, while it is up and holds them.
    projects: Option<Vec<(ProjectKey, String)>>,
    /// How its projects are ordered, while it is up.
    sort: Option<String>,
}

impl HostMenu {
    fn build(&self, menu: crate::widgets::menu::PopupMenu) -> crate::widgets::menu::PopupMenu {
        let host = &self.host;
        let mut menu = menu;
        if let Some(projects) = &self.projects {
            menu = menu.label(crate::tr!("sidebar.create_thread")).menu(
                crate::tr!("sidebar.quick_chat"),
                Box::new(QuickChat(host.clone())),
            );
            for (project, name) in projects {
                menu = menu.menu(
                    name.clone(),
                    Box::new(StartDraftForProject(project.clone())),
                );
            }
            menu = menu
                .separator()
                .menu(
                    crate::tr!("sidebar.add_project"),
                    Box::new(AddProject(host.clone())),
                )
                .menu(
                    crate::tr!("settings.title"),
                    Box::new(HostSettings(host.clone())),
                );
        }
        if let Some(sort) = &self.sort {
            menu = menu.menu(
                crate::tr!("sidebar.sort", mode = sort.clone()),
                Box::new(SortProjects(host.clone())),
            );
        }
        // The host in this process is always in the workspace.
        let menu = menu.separator().when(*host != HostKey::Local, |menu| {
            menu.menu(
                crate::tr!("hosts.disconnect"),
                Box::new(LeaveHost(host.clone())),
            )
        });
        menu.menu(crate::tr!("sidebar.manage_hosts"), Box::new(ManageHosts))
    }
}

/// The parts of a host's heading, drawn in each layout's own row.
struct HostHeadingParts {
    title: SharedString,
    dot: gpui::Hsla,
    status: Option<gpui::AnyElement>,
    pair: Option<gpui::AnyElement>,
    menu: gpui::AnyElement,
}

/// In-progress inline rename of a thread row.
struct RenameState {
    thread: ThreadKey,
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
    /// Its host's name, in a list of several hosts' threads by time.
    host_name: Option<SharedString>,
    /// The time the row shows, the one its list sorts it by.
    timestamp: u64,
    relative_time: SharedString,
    separator: bool,
}

#[derive(Clone)]
struct CompactProjectRow {
    project: ProjectKey,
    /// The project's name in element ids ([`named`]).
    id: String,
    row_id: SharedString,
    name: SharedString,
    label: SharedString,
    count: SharedString,
    collapsed: bool,
}

/// The heading of a host's section, in a list over several hosts.
#[derive(Clone)]
struct CompactHostRow {
    host: HostKey,
    row_id: SharedString,
    /// Whether the section is shut; `None` where nothing under it folds.
    collapsed: Option<bool>,
}

#[derive(Clone)]
enum CompactListRow {
    Host(CompactHostRow),
    Project(CompactProjectRow),
    Caption {
        key: String,
        label: SharedString,
    },
    Settled {
        key: String,
        shelf: Shelf,
        count: usize,
    },
    More {
        key: String,
        shelf: Shelf,
        count: usize,
    },
    Empty {
        key: String,
    },
    Thread(Rc<CompactThreadRow>),
    BottomInset,
}

impl CompactListRow {
    fn key(&self) -> &str {
        match self {
            Self::Host(row) => &row.row_id,
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
    /// The heading of a host whose link is down, which lists none of its
    /// threads.
    Host(HostKey),
    Thread(Box<ListedThread>),
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
            Self::Host(host) => ("host", host.name()),
            Self::Thread(row) => ("thread", &row.name),
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
    scope: Option<ProjectKey>,
    settled_count: usize,
    project_names: HashMap<ProjectKey, String>,
    lists: ThreadLists,
}

/// A row of the desktop grouped list. Host- and project-scoped rows index
/// into the frame's lists: `host` names the host's, `group` its project group.
enum GroupedListRow {
    /// The heading of a host's section, in a list over several hosts.
    Host {
        host: usize,
        collapsed: bool,
    },
    Project {
        host: usize,
        group: usize,
        collapsed: bool,
    },
    Thread(Box<ListedThread>),
    ToggleThreads {
        host: usize,
        group: usize,
        expanded: bool,
        label: SharedString,
    },
    Settled {
        host: usize,
        group: usize,
        count: usize,
    },
    SettledMore {
        host: usize,
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
    lists: ThreadLists,
}

impl GroupedListRow {
    fn key<'a>(&'a self, lists: &'a [HostLists]) -> (&'static str, Cow<'a, str>) {
        let project = |host: &usize, group: &usize| {
            let of = &lists[*host];
            named(lists.len(), &of.host, &of.groups[*group].project.id)
        };
        match self {
            Self::Host { host, .. } => ("host", Cow::Borrowed(lists[*host].host.name())),
            Self::Project { host, group, .. } => ("project", project(host, group)),
            Self::Thread(row) => ("thread", Cow::Borrowed(row.name.as_str())),
            Self::ToggleThreads { host, group, .. } => ("toggle", project(host, group)),
            Self::Settled { host, group, .. } => ("settled", project(host, group)),
            Self::SettledMore { host, group, .. } => ("settled-more", project(host, group)),
            Self::Hint(_) => ("hint", Cow::Borrowed("")),
        }
    }
}

/// What the thread lists show of one host's store, derived once for each
/// [`WorkspaceStore::thread_list_revision`].
#[derive(Clone)]
struct HostLists {
    host: HostKey,
    revision: u64,
    /// The host's link is down in a list over several hosts: it lists none
    /// of the host's threads ([`host_down`]).
    down: bool,
    groups: Rc<Vec<ProjectGroup>>,
    flat: Rc<Vec<SessionMeta>>,
    flags: Rc<HashMap<String, ThreadFlags>>,
}

/// Each host's [`HostLists`], in the sidebar's order of hosts.
type ThreadLists = Rc<[HostLists]>;

#[derive(Clone, Default)]
struct CompactListModel {
    rows: Vec<CompactListRow>,
    /// Each row's fold factor, as `SessionsSidebar::fold_row` takes it.
    folds: Vec<f32>,
    /// Whether the list has anything to show in place of its empty page:
    /// a project, or a host's heading.
    has_rows: bool,
    locale: String,
    minute: u64,
}

/// One host the sidebar lists.
struct SidebarHost {
    key: HostKey,
    store: Entity<WorkspaceStore>,
    spaces_observer: SpacesObserver,
    _subscription: Subscription,
}

pub struct SessionsSidebar {
    /// The hosts it lists, in order.
    hosts: Vec<SidebarHost>,
    window_state: Entity<WindowState>,
    /// Projects whose thread list is expanded past the collapsed limit.
    expanded_groups: HashSet<ProjectKey>,
    expanded_settled: HashSet<Shelf>,
    settled_limits: HashMap<Shelf, usize>,
    lifecycle_undo: Option<LifecycleUndo>,
    /// A thread pinned from its menu: Pinned leads the list, above where the
    /// user was looking, so the list scrolls to the row once it shows there.
    reveal_pinned: Option<ThreadKey>,
    row_swipes: swipe::RowSwipes,
    drag: Option<ThreadDrag>,
    landing: Option<Landing>,
    /// The project the Arrange threads page shows; `None` is every project.
    arrange_scope: Option<ProjectKey>,
    arrange_settled_expanded: bool,
    arrange_settled_fold: f32,
    arrange_list_state: ListState,
    arrange_row_keys: Vec<String>,
    arrange_flip: crate::motion::ListFlip,
    last_selected: Option<ThreadKey>,
    /// What the chat column shows, as its shell last told the list.
    shown: ShownThreads,
    /// Optional project filter for the session-local flat list.
    project_filter: Option<ProjectKey>,
    settled_scope: Option<(SidebarLayout, Option<ProjectKey>)>,
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
    _subscriptions: Vec<Subscription>,
}

/// Included desktop rows plus the pre-disclosure counts used by list controls.
struct ThreadRows<'a> {
    pinned: Vec<Listed<'a>>,
    active: Vec<Listed<'a>>,
    settled: Vec<Listed<'a>>,
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
                    settle_blocked: store.settle_blocked_for(&meta.id),
                    failed: store.failed_for(&meta.id),
                    background: store.activity(&meta.id).map_or((0, 0), |activity| {
                        (activity.background.tasks, activity.background.agents)
                    }),
                    unsettled: store
                        .activity(&meta.id)
                        .is_some_and(|activity| activity.unsettled > 0),
                    wakes: store
                        .activity(&meta.id)
                        .map(|activity| activity.wakes.clone())
                        .unwrap_or_default(),
                    forks: store.activity(&meta.id).is_some_and(|activity| {
                        activity.fork != tcode_protocol::ForkAvailability::Unsupported
                    }),
                },
            )
        })
        .collect()
}

impl SessionsSidebar {
    fn thread_lists(&mut self, cx: &App) -> ThreadLists {
        let cached = self.thread_lists.take().unwrap_or_default();
        let sectioned = self.sectioned();
        let down = |host: &SidebarHost| sectioned && host_down(host.store.read(cx));
        let current = |host: &SidebarHost, lists: &HostLists| {
            lists.host == host.key
                && lists.revision == host.store.read(cx).thread_list_revision()
                && lists.down == down(host)
        };
        if cached.len() == self.hosts.len()
            && self
                .hosts
                .iter()
                .zip(cached.iter())
                .all(|(host, lists)| current(host, lists))
        {
            self.thread_lists = Some(cached.clone());
            return cached;
        }
        let lists: ThreadLists = self
            .hosts
            .iter()
            .map(|host| {
                let kept = cached.iter().find(|lists| current(host, lists));
                kept.cloned().unwrap_or_else(|| {
                    let store = host.store.read(cx);
                    let revision = store.thread_list_revision();
                    if down(host) {
                        return HostLists {
                            host: host.key.clone(),
                            revision,
                            down: true,
                            groups: Rc::default(),
                            flat: Rc::default(),
                            flags: Rc::default(),
                        };
                    }
                    HostLists {
                        host: host.key.clone(),
                        revision,
                        down: false,
                        groups: Rc::new(store.grouped_sessions()),
                        flat: Rc::new(store.flat_sessions()),
                        flags: Rc::new(session_flags(store.sidebar_sessions(), store)),
                    }
                })
            })
            .collect();
        self.thread_lists = Some(lists.clone());
        lists
    }

    /// Whether the list is over several hosts, each in its own section.
    fn sectioned(&self) -> bool {
        self.hosts.len() > 1
    }

    /// Whether any host the list shows holds its projects, not only a space.
    fn full_scope(&self, cx: &App) -> bool {
        self.hosts
            .iter()
            .any(|host| host.store.read(cx).scope().is_full())
    }

    /// The store of `host`, while the sidebar lists it.
    fn store_of(&self, host: &HostKey) -> Option<Entity<WorkspaceStore>> {
        self.hosts
            .iter()
            .find(|listed| listed.key == *host)
            .map(|listed| listed.store.clone())
    }

    /// `id` of `host`, as [`named`] names it in this sidebar.
    fn name<'a>(&self, host: &HostKey, id: &'a str) -> Cow<'a, str> {
        named(self.hosts.len(), host, id)
    }

    /// The thread [`Self::name`] names `name`.
    fn thread_named(&self, name: &str) -> Option<ThreadKey> {
        self.hosts.iter().find_map(|host| {
            let session_id = name.strip_prefix(self.name(&host.key, "").as_ref())?;
            Some(ThreadKey {
                host: host.key.clone(),
                session_id: session_id.to_owned(),
            })
        })
    }

    fn shelf_name(&self, shelf: &Shelf) -> String {
        match shelf {
            Shelf::Recent => "recent".into(),
            Shelf::Project(project) => self.name(&project.host, &project.project_id).into_owned(),
        }
    }

    fn listed(&self, (host, meta): Listed) -> ListedThread {
        ListedThread {
            host: host.clone(),
            meta: meta.clone(),
            name: self.name(host, &meta.id).into_owned(),
        }
    }

    fn is_selected(&self, host: &HostKey, id: &str) -> bool {
        self.last_selected
            .as_ref()
            .is_some_and(|selected| selected.host == *host && selected.session_id == id)
    }

    /// The rows `scope` shows, as a drag over them would leave them, each
    /// section of every host's merged by time ([`merged`]).
    fn thread_rows<'a>(
        &self,
        lists: &'a [HostLists],
        scope: Option<&ProjectKey>,
        shelf: &Shelf,
    ) -> ThreadRows<'a> {
        let hosts = lists
            .iter()
            .filter_map(|lists| {
                let project = host_scope(scope, &lists.host)?;
                let host = self.arranged(&lists.host, scope, project_threads(&lists.flat, project));
                Some(tagged_sections(&lists.host, host))
            })
            .collect();
        let ThreadSections {
            pinned,
            active,
            mut settled,
        } = merged(hosts);
        let settled_count = settled.len();
        self.limit_settled_rows(shelf, &mut settled);
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
    fn grouped_rows(&self, lists: &[HostLists], cx: &App) -> Vec<(GroupedListRow, f32)> {
        let mut rows = self.grouped_rows_folding(lists, cx);
        rows.retain(|(_, factor)| *factor >= 0.);
        rows
    }

    /// [`Self::grouped_rows`] with the rows a moving disclosure leaves out of
    /// the list, at a negative factor. Each host's projects follow the last
    /// host's, under its heading when there are several.
    fn grouped_rows_folding(&self, lists: &[HostLists], cx: &App) -> Vec<(GroupedListRow, f32)> {
        let mut rows = Vec::new();
        let sectioned = lists.len() > 1;
        for (host, of) in lists.iter().enumerate() {
            let Some(store) = self.store_of(&of.host) else {
                continue;
            };
            let store = store.read(cx);
            let host_collapsed = DevicePreferences::host_collapsed(&of.host, cx);
            let host_key = fold_key(Fold::Host, of.host.name());
            if sectioned {
                rows.push((
                    GroupedListRow::Host {
                        host,
                        collapsed: host_collapsed,
                    },
                    1.,
                ));
                if !self.fold_shown(&host_key, !host_collapsed) {
                    continue;
                }
            }
            let host_start = rows.len();
            for (group, project) in of.groups.iter().enumerate() {
                let project_id = &project.project.id;
                let name = self.name(&of.host, project_id);
                let key = ProjectKey {
                    host: of.host.clone(),
                    project_id: project_id.clone(),
                };
                let collapsed = store.is_project_collapsed(project_id);
                rows.push((
                    GroupedListRow::Project {
                        host,
                        group,
                        collapsed,
                    },
                    1.,
                ));
                let project_key = fold_key(Fold::Project, &name);
                if !self.fold_shown(&project_key, !collapsed) {
                    continue;
                }
                let start = rows.len();
                let (active, settled) = project_group_threads(&project.sessions);
                let expanded = self.expanded_groups.contains(&key);
                let more_key = fold_key(Fold::More, &name);
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
                    let row = self.listed((&of.host, meta));
                    (GroupedListRow::Thread(Box::new(row)), factor)
                }));
                if let Some(label) = thread_list_toggle_label(active.len(), expanded) {
                    rows.push((
                        GroupedListRow::ToggleThreads {
                            host,
                            group,
                            expanded,
                            label: label.into(),
                        },
                        1.,
                    ));
                }
                if !settled.is_empty() {
                    let count = settled.len();
                    rows.push((GroupedListRow::Settled { host, group, count }, 1.));
                    let shelf = Shelf::Project(key);
                    let mut listed: Vec<Listed> = tagged(&of.host, settled).collect();
                    self.limit_settled_rows(&shelf, &mut listed);
                    let hidden = count - listed.len();
                    let more = self.settled_more_count(&shelf, hidden);
                    let (factors, more_factor) =
                        self.shelf_factors(&shelf, &listed, more.is_some());
                    rows.extend(listed.into_iter().zip(factors).map(|(row, factor)| {
                        (GroupedListRow::Thread(Box::new(self.listed(row))), factor)
                    }));
                    if let Some(count) = more {
                        rows.push((
                            GroupedListRow::SettledMore { host, group, count },
                            more_factor,
                        ));
                    }
                }
                let progress = self.fold_progress(&project_key, !collapsed);
                let count = rows.len() - start;
                for (index, (_, factor)) in rows[start..].iter_mut().enumerate() {
                    *factor = nested_fold(fold_factor(progress, index, count), *factor);
                }
            }
            if sectioned {
                let progress = self.fold_progress(&host_key, !host_collapsed);
                let count = rows.len() - host_start;
                for (index, (_, factor)) in rows[host_start..].iter_mut().enumerate() {
                    *factor = nested_fold(fold_factor(progress, index, count), *factor);
                }
            }
        }
        rows
    }

    /// Use the same filters, disclosures and ordering as the rendered list,
    /// including rows outside the scroll viewport.
    fn navigation_threads(&mut self, cx: &mut Context<Self>) -> Vec<ThreadKey> {
        if self.compact(cx) {
            return self
                .compact_model(None, cx)
                .rows
                .iter()
                .filter_map(|row| match row {
                    CompactListRow::Thread(row) => Some(row.state.thread.clone()),
                    _ => None,
                })
                .collect();
        }

        let lists = self.thread_lists(cx);
        let mut threads = Vec::new();
        match DevicePreferences::sidebar_layout(cx) {
            SidebarLayout::Flat => {
                let rows = self.thread_rows(&lists, self.project_filter.as_ref(), &Shelf::Recent);
                threads.extend(
                    rows.pinned
                        .into_iter()
                        .chain(rows.active)
                        .chain(rows.settled)
                        .map(|(host, meta)| ThreadKey {
                            host: host.clone(),
                            session_id: meta.id.clone(),
                        }),
                );
            }
            SidebarLayout::Grouped => {
                threads.extend(
                    self.grouped_rows(&lists, cx)
                        .into_iter()
                        .filter_map(|(row, _)| match row {
                            GroupedListRow::Thread(row) => Some(row.key()),
                            _ => None,
                        }),
                );
            }
        }
        threads
    }

    pub(crate) fn navigate_thread(
        &mut self,
        action: &crate::shortcut::NavigateThread,
        current: Option<&ThreadKey>,
        cx: &mut Context<Self>,
    ) -> bool {
        use crate::shortcut::NavigateThread;

        let threads = self.navigation_threads(cx);
        if threads.is_empty() {
            return false;
        }
        let active = current.and_then(|thread| {
            let store = self.store_of(&thread.host)?;
            Some(ThreadKey {
                host: thread.host.clone(),
                session_id: store.read(cx).roster_row(&thread.session_id),
            })
        });
        let current = threads.iter().position(|id| Some(id) == active.as_ref());
        let index = match action {
            NavigateThread::Index(index) => *index,
            NavigateThread::Next => current.map_or(0, |index| (index + 1) % threads.len()),
            NavigateThread::Previous => current.map_or(threads.len() - 1, |index| {
                (index + threads.len() - 1) % threads.len()
            }),
        };
        if let Some(thread) = threads.get(index)
            && let Some(store) = self.store_of(&thread.host)
        {
            self.compact_model_dirty = true;
            store.update(cx, |store, cx| {
                store.show_thread(thread.session_id.clone(), cx);
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
        let layout = DevicePreferences::sidebar_layout(cx);
        let lists = self.thread_lists(cx);
        let mut targets = Vec::new();
        match layout {
            SidebarLayout::Flat => targets.push((
                fold_key(Fold::Shelf, "recent"),
                self.expanded_settled.contains(&Shelf::Recent),
            )),
            SidebarLayout::Grouped => {
                let groups: usize = lists.iter().map(|lists| lists.groups.len()).sum();
                let sectioned = self.sectioned();
                let headers = !compact || groups > 1 || sectioned;
                for lists in lists.iter() {
                    let Some(store) = self.store_of(&lists.host) else {
                        continue;
                    };
                    if sectioned {
                        targets.push((
                            fold_key(Fold::Host, lists.host.name()),
                            !DevicePreferences::host_collapsed(&lists.host, cx),
                        ));
                    }
                    for group in lists.groups.iter() {
                        let id = group.project.id.as_str();
                        let name = self.name(&lists.host, id);
                        let project = ProjectKey {
                            host: lists.host.clone(),
                            project_id: id.to_owned(),
                        };
                        let collapsed = store.read(cx).is_project_collapsed(id)
                            && (headers || group.project.quick_chat);
                        targets.push((fold_key(Fold::Project, &name), !collapsed));
                        targets.push((
                            fold_key(Fold::More, &name),
                            self.expanded_groups.contains(&project),
                        ));
                        targets.push((
                            fold_key(Fold::Shelf, &name),
                            self.expanded_settled.contains(&Shelf::Project(project)),
                        ));
                    }
                }
            }
        }
        let filter = self
            .project_filter
            .as_ref()
            .map(|project| self.name(&project.host, &project.project_id));
        let scope = format!("{layout:?}-{filter:?}-{compact}");
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

    /// Whether the threads are loading on every host: once one host has
    /// them, the list shows them. A host whose link is down in a list over
    /// several is not loading: it shows its heading.
    fn threads_loading(&self, cx: &App) -> bool {
        let sectioned = self.sectioned();
        self.hosts.iter().all(|host| {
            let store = host.store.read(cx);
            store.threads_loading() && !(sectioned && host_down(store))
        })
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
        let loading = self.threads_loading(cx);
        // The host in this process answers within moments of the window
        // opening: a skeleton would be gone almost as soon as drawn, so the
        // list stays as it was until that host answers, then shows at once.
        if loading
            && self
                .hosts
                .iter()
                .any(|host| !host.store.read(cx).is_remote())
        {
            return div().flex_1().into_any_element();
        }
        let skeleton = loading
            && self
                .hosts
                .iter()
                .all(|host| host.store.read(cx).pending_sessions().is_empty());
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
        stores: Vec<Entity<WorkspaceStore>>,
        window_state: Entity<WindowState>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut last_destination = window_state.read(cx).destination();
        let subscriptions = vec![cx.observe(&window_state, move |this, state, cx| {
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
        })];
        let mut sidebar = Self {
            hosts: Vec::new(),
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
            _subscriptions: subscriptions,
        };
        sidebar.set_hosts(stores, cx);
        sidebar
    }

    /// List the threads of `stores`, in their order. What the list kept of
    /// a host it no longer lists goes with it.
    pub(crate) fn set_hosts(
        &mut self,
        stores: Vec<Entity<WorkspaceStore>>,
        cx: &mut Context<Self>,
    ) {
        let mut kept = std::mem::take(&mut self.hosts);
        self.hosts = stores
            .into_iter()
            .map(
                |store| match kept.iter().position(|host| host.store == store) {
                    Some(index) => kept.remove(index),
                    None => SidebarHost {
                        key: store.read(cx).host_key(),
                        spaces_observer: SpacesObserver::default(),
                        _subscription: cx.subscribe(&store, |this, _, change: &StoreChange, cx| {
                            if matches!(
                                change.topic,
                                TopicKind::Index | TopicKind::Settings | TopicKind::SessionStatus
                            ) {
                                this.compact_model_dirty = true;
                                cx.notify();
                            }
                        }),
                        store,
                    },
                },
            )
            .collect();
        if kept.is_empty() {
            return;
        }
        let listed = |host: &HostKey| self.hosts.iter().any(|listed| listed.key == *host);
        self.expanded_groups.retain(|project| listed(&project.host));
        let shelf_listed = |shelf: &Shelf| match shelf {
            Shelf::Recent => true,
            Shelf::Project(project) => listed(&project.host),
        };
        self.expanded_settled.retain(shelf_listed);
        self.settled_limits.retain(|shelf, _| shelf_listed(shelf));
        let gone = |thread: &Option<ThreadKey>| thread.as_ref().is_some_and(|t| !listed(&t.host));
        if gone(&self.reveal_pinned) {
            self.reveal_pinned = None;
        }
        if gone(&self.last_selected) {
            self.last_selected = None;
        }
        let gone =
            |project: &Option<ProjectKey>| project.as_ref().is_some_and(|p| !listed(&p.host));
        if gone(&self.project_filter) {
            self.project_filter = None;
        }
        if gone(&self.arrange_scope) {
            self.arrange_scope = None;
        }
        if self
            .renaming
            .as_ref()
            .is_some_and(|rename| !listed(&rename.thread.host))
        {
            self.renaming = None;
        }
        if self
            .drag
            .as_ref()
            .is_some_and(|drag| !listed(&drag.thread.host))
        {
            self.drag = None;
        }
        if self
            .landing
            .as_ref()
            .is_some_and(|landing| !listed(&landing.thread.host))
        {
            self.landing = None;
        }
        self.compact_model_dirty = true;
        cx.notify();
    }

    /// Prompt for a directory, then create a project rooted there on `host`.
    fn add_project(&mut self, host: &HostKey, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(store) = self.store_of(host) {
            crate::add_project_dialog::open(store, window, cx);
        }
    }

    fn toggle_project(&mut self, project: &ProjectKey, cx: &mut Context<Self>) {
        let Some(store) = self.store_of(&project.host) else {
            return;
        };
        if !store.read(cx).is_project_collapsed(&project.project_id) {
            self.expanded_groups.remove(project);
        }
        store.update(cx, |store, cx| {
            store.toggle_project_collapsed(project.project_id.clone(), cx);
        });
        cx.notify();
    }

    fn on_filter_project(
        &mut self,
        action: &FilterProject,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.project_filter = action.0.clone();
        self.expanded_settled.clear();
        self.settled_limits.clear();
        self.compact_model_dirty = true;
        self.window_state
            .update(cx, |state, cx| state.leave_route_for_chat(cx));
        cx.notify();
    }

    fn on_add_project(&mut self, action: &AddProject, window: &mut Window, cx: &mut Context<Self>) {
        self.add_project(&action.0, window, cx);
    }

    fn on_sort_projects(&mut self, action: &SortProjects, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(store) = self.store_of(&action.0) {
            store.update(cx, |store, cx| store.cycle_project_sort(cx));
        }
    }

    fn on_leave_host(&mut self, action: &LeaveHost, window: &mut Window, cx: &mut Context<Self>) {
        let request = cx.global::<crate::remote::ClientAttachment>().requester();
        request(
            crate::remote::HostRequest::Leave(action.0.clone()),
            window,
            cx,
        );
    }

    fn on_host_settings(&mut self, action: &HostSettings, _: &mut Window, cx: &mut Context<Self>) {
        let host = action.0.clone();
        self.window_state
            .update(cx, |state, cx| state.open_settings_on(host, cx));
    }

    fn on_manage_hosts(&mut self, _: &ManageHosts, _: &mut Window, cx: &mut Context<Self>) {
        self.window_state
            .update(cx, |state, cx| state.go(Destination::Hosts, cx));
    }

    fn toggle_host(&mut self, host: &HostKey, cx: &mut Context<Self>) {
        let collapsed = DevicePreferences::host_collapsed(host, cx);
        DevicePreferences::set_host_collapsed(host, !collapsed, cx);
        self.compact_model_dirty = true;
        cx.notify();
    }

    /// Open `host`'s section, which the window just brought in.
    pub(crate) fn reveal_host(&mut self, host: &HostKey, cx: &mut Context<Self>) {
        if DevicePreferences::host_collapsed(host, cx) {
            self.toggle_host(host, cx);
        }
    }

    fn on_link_pull_request(
        &mut self,
        action: &ThreadLinkPullRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(store) = self.store_of(&action.0.host) {
            crate::pull_requests::open_link_dialog(store, action.0.session_id.clone(), window, cx);
        }
    }

    /// `thread`'s title, as its host lists it.
    fn thread_title(&self, thread: &ThreadKey, cx: &App) -> Option<String> {
        let store = self.store_of(&thread.host)?;
        store
            .read(cx)
            .sidebar_sessions()
            .iter()
            .find(|meta| meta.id == thread.session_id)
            .map(|meta| meta.title.clone())
    }

    fn on_rename(&mut self, action: &ThreadRename, window: &mut Window, cx: &mut Context<Self>) {
        let thread = action.0.clone();
        let title = self.thread_title(&thread, cx).unwrap_or_default();
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
            thread,
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
        if let Some(store) = self.store_of(&action.0.host) {
            store.update(cx, |store, _| {
                store.regenerate_session_title(action.0.session_id.clone());
            });
        }
    }

    fn on_stop_title(&mut self, action: &ThreadStopTitle, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(store) = self.store_of(&action.0.host) {
            store.update(cx, |store, _| {
                store.cancel_title_generation(action.0.session_id.clone());
            });
        }
    }

    fn on_fork(&mut self, action: &ThreadFork, _: &mut Window, cx: &mut Context<Self>) {
        let Some(store) = self.store_of(&action.0.host) else {
            return;
        };
        let id = action.0.session_id.clone();
        store.update(cx, |store, cx| {
            store.fork_thread(id, cx);
        });
    }

    fn on_move_to_project(
        &mut self,
        action: &ThreadMoveToProject,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(store) = self.store_of(&action.0.host) {
            crate::move_thread_dialog::open(store, action.0.session_id.clone(), window, cx);
        }
    }

    fn on_merge_worktree(
        &mut self,
        action: &ThreadMergeWorktree,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(store) = self.store_of(&action.0.host) {
            store.update(cx, |store, _cx| {
                store.merge_worktree(action.0.session_id.clone());
            });
        }
    }

    fn commit_rename(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = self.renaming.take() {
            let value = state.input.read(cx).value().to_string();
            if let Some(store) = self.store_of(&state.thread.host) {
                store.update(cx, |store, _cx| {
                    store.rename_session(state.thread.session_id, value);
                });
            }
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
        if let Some(store) = self.store_of(&action.0.host) {
            let id = action.0.session_id.clone();
            store.update(cx, |store, cx| {
                store.mark_session_unread(id, cx);
            });
        }
    }

    fn on_copy_path(
        &mut self,
        action: &ThreadCopyPath,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.store_of(&action.0.host) else {
            return;
        };
        if let Some(meta) = store
            .read(cx)
            .sidebar_sessions()
            .iter()
            .find(|m| m.id == action.0.session_id)
        {
            let path = meta.cwd.to_string_lossy().into_owned();
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(path));
        }
    }

    fn on_copy_id(&mut self, action: &ThreadCopyId, _window: &mut Window, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(action.0.session_id.clone()));
    }

    fn prompt_export(
        &self,
        thread: &ThreadKey,
        format: ThreadExportFormat,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.store_of(&thread.host) else {
            return;
        };
        let Some(cwd) = store
            .read(cx)
            .thread_meta(&thread.session_id)
            .map(|meta| meta.cwd.clone())
        else {
            return;
        };
        crate::thread_export::prompt_thread_export(
            store,
            thread.session_id.clone(),
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
        if let Some(store) = self.store_of(&action.0.host) {
            store.update(cx, |store, _| {
                store.set_auto_settle(action.0.session_id.clone(), action.1)
            });
        }
    }

    fn on_follow_up(&mut self, action: &ThreadFollowUp, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(store) = self.store_of(&action.0.host) {
            store.update(cx, |store, _| {
                store.set_follow_up(action.0.session_id.clone(), action.1)
            });
        }
    }

    fn on_park(&mut self, action: &ThreadPark, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(store) = self.store_of(&action.0.host) {
            store.update(cx, |store, _| {
                store.set_parked(action.0.session_id.clone(), action.1)
            });
        }
    }

    fn on_settle(&mut self, action: &ThreadSettle, window: &mut Window, cx: &mut Context<Self>) {
        self.settle_thread(&action.0, window, cx);
    }

    /// Settle a thread; its undo also restores a pin it loses.
    fn settle_thread(&mut self, thread: &ThreadKey, window: &mut Window, cx: &mut Context<Self>) {
        let Some(store) = self.store_of(&thread.host) else {
            return;
        };
        let id = thread.session_id.as_str();
        let pin = store
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
            &thread.host,
            vec![Command::SettleSession {
                session_id: id.to_owned(),
            }],
            Some((UndoKind::Settle, undo)),
            window,
            cx,
        );
    }

    /// Send `commands` to `host` in order and stop at the first refusal;
    /// once every one is accepted, offer `undo` in the shared toast.
    fn perform_lifecycle(
        &mut self,
        host: &HostKey,
        commands: Vec<Command>,
        undo: Option<(UndoKind, Vec<Command>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.store_of(host) else {
            return;
        };
        let reopen = match commands.first() {
            Some(Command::ArchiveSession { session_id })
                if crate::shell::pane_shows(&store.read(cx).thread_key(session_id), cx) =>
            {
                Some(session_id.clone())
            }
            _ => None,
        };
        let host = host.clone();
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
                    | Command::UnsettleSession { session_id } => Some(ThreadKey {
                        host: host.clone(),
                        session_id: session_id.clone(),
                    }),
                    _ => None,
                };
                let Ok(request) =
                    cx.update(|_, cx| store.update(cx, |store, cx| store.command(command, cx)))
                else {
                    return;
                };
                if let Err(error) = request.await {
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.lifecycle_answered(&host, &sessions, false, cx);
                        if pinning {
                            this.reveal_pinned = None;
                        }
                        if let Some(thread) = swiped {
                            this.row_swipe_refused(&thread);
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
            let _ = this.update(cx, |this, cx| {
                this.lifecycle_answered(&host, &sessions, true, cx)
            });
            if let Some((kind, commands)) = undo {
                let _ = this.update_in(cx, |this, window, cx| {
                    let store = store.read(cx);
                    let harnesses = store
                        .sidebar_sessions()
                        .iter()
                        .filter(|meta| sessions.contains(&meta.id))
                        .map(|meta| store.harness_name_of(meta))
                        .collect();
                    this.push_lifecycle_undo(
                        kind,
                        UndoEntry {
                            host,
                            commands,
                            reopen,
                        },
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
            crate::tr!("providers.harness.unnamed").into_owned()
        } else {
            crate::i18n::join_list(&undo.harnesses)
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
        let entries: Vec<_> = undo
            .entries
            .into_iter()
            .rev()
            .filter_map(|entry| Some((self.store_of(&entry.host)?, entry)))
            .collect();
        cx.spawn_in(window, async move |_, cx| {
            for (store, entry) in entries {
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
        if let Some(store) = self.store_of(&action.0.host) {
            store.update(cx, |store, _| {
                store.make_session_active(action.0.session_id.clone())
            });
        }
    }

    fn reveal_selected_settled(&mut self, cx: &mut Context<Self>) {
        let selected = self.current_row(cx);
        if selected == self.last_selected {
            return;
        }
        let found = selected.as_ref().and_then(|thread| {
            let store = self.store_of(&thread.host)?;
            let meta = store.read(cx).thread_meta(&thread.session_id)?.clone();
            Some((store, meta))
        });
        if selected.is_some() && found.is_none() {
            return;
        }
        let host = selected.as_ref().map(|thread| thread.host.clone());
        self.last_selected = selected;
        let (Some((store, meta)), Some(host)) = (found, host) else {
            return;
        };
        if meta.is_settled()
            && let Some(project_id) = meta.project_id.clone()
        {
            // The project view's shelf opens; the threads view shows the
            // viewed row under its closed shelf instead.
            let shelf = Shelf::Project(ProjectKey {
                host,
                project_id: project_id.clone(),
            });
            if self.expanded_settled.insert(shelf.clone()) {
                self.settled_limits.remove(&shelf);
            }
            if store.read(cx).is_project_collapsed(&project_id) {
                store.update(cx, |store, cx| {
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

    fn limit_settled_rows(&self, shelf: &Shelf, rows: &mut Vec<Listed>) {
        let open = self.expanded_settled.contains(shelf);
        let limit = if self.fold_shown(&fold_key(Fold::Shelf, &self.shelf_name(shelf)), open) {
            self.settled_limits.get(shelf).copied().unwrap_or(10)
        } else {
            0
        };
        let mut index = 0;
        rows.retain(|(host, meta)| {
            let keep = index < limit || self.is_selected(host, &meta.id);
            index += 1;
            keep
        });
    }

    /// The fold factor of each row a Settled shelf shows, and of its "Show
    /// more" row after them. The selected row a shut shelf keeps stays put.
    fn shelf_factors(&self, shelf: &Shelf, rows: &[Listed], more: bool) -> (Vec<f32>, f32) {
        let open = self.expanded_settled.contains(shelf);
        let progress = self.fold_progress(&fold_key(Fold::Shelf, &self.shelf_name(shelf)), open);
        let count = rows
            .iter()
            .filter(|(host, meta)| !self.is_selected(host, &meta.id))
            .count()
            + usize::from(more);
        let mut index = 0;
        let factors = rows
            .iter()
            .map(|(host, meta)| {
                if self.is_selected(host, &meta.id) {
                    return 1.;
                }
                index += 1;
                fold_factor(progress, index - 1, count)
            })
            .collect();
        (factors, fold_factor(progress, index, count))
    }

    fn settled_more_count(&self, shelf: &Shelf, hidden: usize) -> Option<usize> {
        let open = self.expanded_settled.contains(shelf);
        (self.fold_shown(&fold_key(Fold::Shelf, &self.shelf_name(shelf)), open) && hidden > 0)
            .then(|| hidden.min(25))
    }

    fn render_settled_more(
        &self,
        shelf: &Shelf,
        count: usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let key = self.shelf_name(shelf);
        let shelf = shelf.clone();
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
        .debug_selector(move || format!("settled-more-{key}"))
        .w_full()
        .text_secondary(compact, cx)
        .cursor_pointer()
        .child(Icon::new(IconName::Plus))
        .child(crate::widgets::fade_text(crate::tr!(
            "sidebar.show_more_settled",
            count = count
        )))
        .on_click(cx.listener(move |this, _, _, cx| {
            *this.settled_limits.entry(shelf.clone()).or_insert(10) += 25;
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
        shelf: &Shelf,
        count: usize,
        scope: Option<&ProjectKey>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let expanded = self.expanded_settled.contains(shelf);
        let key = self.shelf_name(shelf);
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
        let turned = self.fold_progress(&fold_key(Fold::Shelf, &key), expanded);
        let shelf = shelf.clone();
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
        .debug_selector(move || format!("settled-{key}"))
        .w_full()
        .h(px(bar_height(Bar::Section, compact)))
        .px(inset)
        .gap(spacing.sm)
        .text_caption(cx)
        .text_color(color)
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, _, cx| {
            // A shelf shuts with the rows it showed and opens at its first page.
            if !this.expanded_settled.remove(&shelf) {
                this.settled_limits.remove(&shelf);
                this.expanded_settled.insert(shelf.clone());
            }
            this.compact_model_dirty = true;
            cx.notify();
        }))
        .child(collapse_chevron(turned, cx).text_color(color))
        .child(crate::widgets::fade_text(crate::tr!("sidebar.settled")))
        .child(count.to_string())
        .into_any_element()
    }

    fn on_archive(&mut self, action: &ThreadArchive, window: &mut Window, cx: &mut Context<Self>) {
        let title = self.thread_title(&action.0, cx).unwrap_or_default();
        self.archive_thread(&action.0, &title, window, cx);
    }

    fn on_delete(&mut self, action: &ThreadDelete, window: &mut Window, cx: &mut Context<Self>) {
        let title = self.thread_title(&action.0, cx).unwrap_or_default();
        self.delete_thread(&action.0, &title, window, cx);
    }

    fn on_project_thread_rules(
        &mut self,
        action: &ProjectThreadRules,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.store_of(&action.0.host) else {
            return;
        };
        if let Some(project) = store.read(cx).project(&action.0.project_id).cloned() {
            crate::settings_page::thread_behavior::open_project_rules(store, project, window, cx);
        }
    }

    fn on_project_archive_all(
        &mut self,
        action: &ProjectArchiveAll,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.store_of(&action.0.host) else {
            return;
        };
        let session_ids: Vec<String> = store
            .read(cx)
            .sidebar_sessions()
            .iter()
            .filter(|meta| {
                meta.project_id.as_deref() == Some(action.0.project_id.as_str())
                    && meta.archived_at.is_none()
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
                .on_ok(move |_, window, cx| {
                    let requests: Vec<_> = session_ids
                        .iter()
                        .map(|session_id| {
                            store.update(cx, |store, cx| {
                                store.command(
                                    Command::ArchiveSession {
                                        session_id: session_id.clone(),
                                    },
                                    cx,
                                )
                            })
                        })
                        .collect();
                    // The host refuses a thread it will not archive (one still running);
                    // each distinct reason is shown once, with how many it kept.
                    window
                        .spawn(cx, async move |cx| {
                            let mut refused: Vec<(String, usize)> = Vec::new();
                            for request in requests {
                                if let Err(error) = request.await {
                                    match refused
                                        .iter_mut()
                                        .find(|(reason, _)| *reason == error.message)
                                    {
                                        Some((_, count)) => *count += 1,
                                        None => refused.push((error.message, 1)),
                                    }
                                }
                            }
                            let _ = cx.update(|window, cx| {
                                for (reason, count) in refused {
                                    window.push_notification(
                                        Notification::warning(crate::tr!(
                                            "sidebar.archive_all_refused",
                                            count = count,
                                            reason = reason
                                        )),
                                        cx,
                                    );
                                }
                            });
                        })
                        .detach();
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
        let Some(store) = self.store_of(&action.0.host) else {
            return;
        };
        let project = store.read(cx).project(&action.0.project_id).cloned();
        if let Some(project) = project {
            crate::project_icon::open(store, project, window, cx);
        }
    }

    fn on_project_delete(
        &mut self,
        action: &ProjectDelete,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(store) = self.store_of(&action.0.host) {
            confirm_remove_project(store, action.0.project_id.clone(), window, cx);
        }
    }

    /// A project's share items, where its machine has spaces this client
    /// may manage.
    fn share_target(
        &self,
        host: &HostKey,
        project_id: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Option<ShareTarget> {
        let project_id = project_id?;
        let store = self.store_of(host)?;
        let project_name = store.read(cx).project(project_id)?.name.clone();
        Some(ShareTarget {
            spaces: spaces::for_store(&store, cx)?,
            host: host.clone(),
            project_id: project_id.to_owned(),
            project_name,
        })
    }

    /// The badge of a project shared in a space; it fades in as the
    /// project is shared and out as it stops being.
    fn shared_badge(
        &self,
        project: &ProjectKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let names = self
            .store_of(&project.host)
            .and_then(|store| spaces::for_store(&store, cx))
            .map(|spaces| spaces.read(cx).sharing(&project.project_id))
            .filter(|names| !names.is_empty());
        let name = self.name(&project.host, &project.project_id);
        let shown = crate::motion::shown(
            SharedString::from(format!("shared-badge-{name}")),
            names,
            SHOW,
            window,
            cx,
        )?;
        Some(shown.fade(spaces::shared_badge(&name, &shown.value, cx)))
    }

    fn on_toggle_share(
        &mut self,
        action: &spaces::ToggleShare,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A space is one machine's: the host whose spaces hold it.
        let stores: Vec<_> = self.hosts.iter().map(|host| host.store.clone()).collect();
        let spaces = stores.iter().find_map(|store| {
            spaces::for_store(store, cx).filter(|spaces| {
                spaces
                    .read(cx)
                    .spaces()
                    .iter()
                    .any(|space| space.id == action.space_id)
            })
        });
        if let Some(spaces) = spaces {
            spaces::toggle_share(&spaces, action, window, cx);
        }
    }

    fn on_new_space_and_share(
        &mut self,
        action: &spaces::NewSpaceAndShare,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(share) = self.share_target(&action.0, Some(&action.1), cx) {
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
        if let Some(store) = self.store_of(&action.0.host) {
            crate::change_project_root_dialog::open(store, action.0.project_id.clone(), window, cx);
        }
    }

    fn on_project_reveal(
        &mut self,
        action: &ProjectReveal,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(root) = self
            .store_of(&action.0.host)
            .and_then(|store| store.read(cx).project_root(&action.0.project_id))
        {
            cx.reveal_path(&root);
        }
    }

    /// Archive a thread, honoring the delete-confirmation setting.
    fn archive_thread(
        &mut self,
        thread: &ThreadKey,
        title: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.store_of(&thread.host) else {
            return;
        };
        let sidebar = cx.entity().downgrade();
        let commands = |session_id: &str| {
            (
                vec![Command::ArchiveSession {
                    session_id: session_id.to_owned(),
                }],
                Some((
                    UndoKind::Archive,
                    vec![Command::UnarchiveSession {
                        session_id: session_id.to_owned(),
                    }],
                )),
            )
        };
        if store.read(cx).settings().skip_delete_confirmation {
            let (archive, undo) = commands(&thread.session_id);
            self.perform_lifecycle(&thread.host, archive, undo, window, cx);
            return;
        }
        let title = title.to_string();
        let thread = thread.clone();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let sidebar = sidebar.clone();
            let thread = thread.clone();
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
                        let (archive, undo) = commands(&thread.session_id);
                        this.perform_lifecycle(&thread.host, archive, undo, window, cx)
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
        thread: &ThreadKey,
        title: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.store_of(&thread.host) else {
            return;
        };
        let session_id = thread.session_id.clone();
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

    /// The control that leads to Machines: over one host, that host's
    /// machine and how its link is; over several, whose headings say that
    /// of each, Machines itself. Compact has none: the threads page's nav
    /// bar leads there.
    fn render_machine_control(
        &self,
        lead: &Entity<WorkspaceStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.window_state.read(cx).route() == Route::Hosts;
        let store = lead.read(cx);
        let connection_state = store.connection_state();

        // Local, the control says "This machine" and the tooltip its name;
        // remote, the control names the machine and the tooltip the link.
        let (name, detail): (SharedString, Option<SharedString>) = if self.sectioned() {
            (crate::tr!("hosts.title").into_owned().into(), None)
        } else {
            (
                store.host_title(),
                match store.host_key() {
                    HostKey::Remote(_) => {
                        Some(crate::remote::connection_label(&connection_state).into())
                    }
                    HostKey::Local => crate::remote::local_machine_name(cx).map(SharedString::from),
                },
            )
        };
        let dot = (!self.sectioned()).then(|| {
            transition(
                ("sidebar-hosts", "connection"),
                cx.theme().connection_color(&connection_state),
                gpui_base::motion::Transition::new(GLIDE),
                window,
                cx,
            )
        });
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
                    .children(dot.map(crate::material::status_dot))
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
        .on_click(move |_, _, cx| {
            let next = match layout {
                SidebarLayout::Flat => SidebarLayout::Grouped,
                SidebarLayout::Grouped => SidebarLayout::Flat,
            };
            DevicePreferences::set_sidebar_layout(next, cx);
        })
    }

    /// The "add project" control, which a caller points at a host.
    fn add_project_button(&self, cx: &mut Context<Self>) -> Button {
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
    }

    /// The project view's header. Over one host its sort, quick chat and
    /// new project are that host's; over several, each host's heading
    /// carries its own.
    fn render_projects_header(
        &self,
        lead: &Entity<WorkspaceStore>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let host = lead.read(cx).host_key();
        let sort_label = crate::settings::project_sort_label(lead.read(cx).project_sort());
        let sort = lead.clone();
        let one_host = !self.sectioned();
        bar("sidebar-projects-header", Bar::Section, false, cx)
            .items_center()
            .justify_end()
            .child(
                h_flex()
                    .gap(cx.theme().tokens.spacing.xxs)
                    .when(one_host, |row| {
                        row.child(
                            Button::new("sort-projects")
                                .ghost()
                                .xsmall()
                                .compact()
                                .icon(IconName::SortAscending)
                                .tooltip(crate::tr!("sidebar.sort", mode = sort_label))
                                .on_click(move |_, _, cx| {
                                    sort.update(cx, |store, cx| {
                                        store.cycle_project_sort(cx);
                                    });
                                }),
                        )
                    })
                    .child(self.render_layout_toggle(SidebarLayout::Grouped, cx))
                    .when(one_host && lead.read(cx).scope().is_full(), |row| {
                        let quick_chat = lead.clone();
                        row.child(
                            Button::new("quick-chat-grouped")
                                .ghost()
                                .xsmall()
                                .compact()
                                .icon(IconName::Plus)
                                .tooltip(crate::tr!("sidebar.quick_chat"))
                                .on_click(move |_, _, cx| {
                                    quick_chat.update(cx, |store, cx| store.start_scratch_draft(cx))
                                }),
                        )
                        .child(self.add_project_button(cx).on_click(cx.listener(
                            move |this, _, window, cx| this.add_project(&host, window, cx),
                        )))
                    }),
            )
    }

    /// Each host whose threads the list shows.
    fn host_projects(&self, cx: &App) -> Rc<Vec<HostProjects>> {
        let sectioned = self.sectioned();
        Rc::new(
            self.hosts
                .iter()
                .filter_map(|host| {
                    let store = host.store.read(cx);
                    if sectioned && host_down(store) {
                        return None;
                    }
                    let projects = store
                        .projects()
                        .into_iter()
                        .map(|project| {
                            let key = ProjectKey {
                                host: host.key.clone(),
                                project_id: project.id.clone(),
                            };
                            (key, project)
                        })
                        .collect();
                    Some(HostProjects {
                        host: host.key.clone(),
                        title: store.host_title(),
                        full: store.scope().is_full(),
                        projects,
                    })
                })
                .collect(),
        )
    }

    /// `trigger` opens the choice of where a new thread starts: each host's
    /// quick thread and projects, under its name over several hosts.
    pub(crate) fn new_thread_chooser(
        &self,
        trigger: Button,
        touch: bool,
        cx: &App,
    ) -> impl IntoElement + use<> {
        let hosts = self.host_projects(cx);
        let sectioned = self.sectioned();
        trigger
            .dropdown_menu(move |menu, _, _| {
                hosts.iter().fold(menu, |menu, host| {
                    let menu = host_section(menu, sectioned, &host.title).when(host.full, |menu| {
                        menu.menu(
                            crate::tr!("sidebar.quick_chat"),
                            Box::new(QuickChat(host.host.clone())),
                        )
                    });
                    host.projects.iter().fold(menu, |menu, (key, project)| {
                        menu.menu(
                            project.name.clone(),
                            Box::new(StartDraftForProject(key.clone())),
                        )
                    })
                })
            })
            .touch(touch)
    }

    /// The threads view's header. Its filter and new thread list every
    /// host's projects, under each host's name when there are several, as
    /// its new project lists the hosts that hold their projects.
    fn render_flat_header(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sectioned = self.sectioned();
        let hosts = self.host_projects(cx);
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
        let filter_hosts = hosts.clone();
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
                    Box::new(FilterProject(None)),
                );
                for host in filter_hosts.iter() {
                    menu = host_section(menu, sectioned, &host.title);
                    for (key, project) in &host.projects {
                        menu = menu.menu_with_check(
                            project.name.clone(),
                            filter_for_menu.as_ref() == Some(key),
                            Box::new(FilterProject(Some(key.clone()))),
                        );
                    }
                }
                menu
            });

        let mut projects = hosts.iter().flat_map(|host| &host.projects);
        let draft_project = match &active_filter {
            Some(filter) => projects.find(|(key, _)| key == filter),
            // Over several hosts every other host's quick thread is a choice
            // too.
            None if !sectioned => projects.next().filter(|_| projects.next().is_none()),
            None => None,
        }
        .cloned();
        let new_thread = Button::new("new-flat-thread")
            .ghost()
            .xsmall()
            .compact()
            .icon(IconName::Plus);
        let new_thread = if let Some((key, project)) = draft_project {
            let tooltip = hosts
                .iter()
                .find(|host| host.host == key.host)
                .filter(|_| sectioned)
                .map_or(crate::tr!("sidebar.create_thread"), |host| {
                    crate::tr!("sidebar.create_thread_on", host = host.title.clone())
                });
            let cwd = project.root;
            new_thread
                .tooltip(tooltip)
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(store) = this.store_of(&key.host) {
                        store.update(cx, |store, cx| {
                            store.start_draft(key.project_id.clone(), cwd.clone(), cx);
                        });
                    }
                }))
                .into_any_element()
        } else {
            self.new_thread_chooser(
                new_thread.tooltip(crate::tr!("sidebar.create_thread")),
                false,
                cx,
            )
            .into_any_element()
        };

        let full: Vec<(HostKey, SharedString)> = hosts
            .iter()
            .filter(|host| host.full)
            .map(|host| (host.host.clone(), host.title.clone()))
            .collect();
        let add_project = match full.as_slice() {
            [] => None,
            // Over several hosts the menu names the host before it is asked.
            [(host, _)] if !sectioned => {
                let host = host.clone();
                Some(
                    self.add_project_button(cx)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.add_project(&host, window, cx)
                        }))
                        .into_any_element(),
                )
            }
            _ => Some(
                self.add_project_button(cx)
                    .dropdown_menu(move |menu, _window, _cx| {
                        full.iter().fold(menu, |menu, (host, title)| {
                            menu.menu(title.clone(), Box::new(AddProject(host.clone())))
                        })
                    })
                    .into_any_element(),
            ),
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
                    .children(add_project),
            )
    }

    fn render_group_header(
        &self,
        host: &HostKey,
        group: &ProjectGroup,
        flags: &HashMap<String, ThreadFlags>,
        collapsed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(store) = self.store_of(host) else {
            return div().into_any_element();
        };
        let project = ProjectKey {
            host: host.clone(),
            project_id: group.project.id.clone(),
        };
        let name_id = self.name(host, &project.project_id).into_owned();
        let has_unread = group.sessions.iter().any(|meta| {
            meta.parent_session_id.is_none()
                && flags.get(&meta.id).is_some_and(|flags| flags.unread)
        });
        let group_key = format!("group-{name_id}");

        let share = self.share_target(host, Some(&project.project_id), cx);
        let header_toggle = project.clone();
        let plus_cwd = group.project.root.clone();
        let plus_project_id = project.project_id.clone();
        let menu_project = project.clone();
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
        let plus_id = gpui::ElementId::from(SharedString::from(format!("new-thread-{name_id}")));
        let (plus_focus, plus_focused) =
            crate::motion::focus_ring::focus_reveal(&plus_id, true, window, cx);
        let hover = self
            .hover_progress(&group_key, window, cx)
            .max(plus_focused);
        // The dot shows while unread or fading out, and gives its slot to
        // the "+" under the pointer.
        let unread_dot = crate::motion::shown(
            SharedString::from(format!("project-unread-{name_id}")),
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
                gpui::SharedString::from(format!("project-header-{name_id}")),
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
            let name_id = name_id.clone();
            move || {
                if quick_chat {
                    "quick-chats-header".into()
                } else {
                    format!("project-header-{name_id}")
                }
            }
        })
        .text_body(false, cx)
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, _, cx| {
            this.toggle_project(&header_toggle, cx);
        }))
        .child(collapse_chevron(
            self.fold_progress(&fold_key(Fold::Project, &name_id), !collapsed),
            cx,
        ))
        .child(crate::project_icon::artwork(
            store.read(cx).image_host(),
            &group.project,
            16.,
        ))
        .child(
            sidebar_label(name)
                .font_medium()
                .text_color(cx.theme().sidebar_foreground),
        )
        .children(self.shared_badge(&project, window, cx))
        // Unread dot when any child thread is unread.
        .children(unread_dot)
        .child({
            let store = store.clone();
            Button::new(plus_id)
                .track_focus(&plus_focus)
                .ghost()
                .xsmall()
                .icon(Icon::new(IconName::Plus).text_color(cx.theme().muted_foreground))
                .aria_label(new_thread_label.clone())
                .tooltip(new_thread_label)
                // Opacity keeps the button a tab stop, as the settle button does.
                .opacity(hover)
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    let cwd = plus_cwd.clone();
                    let project_id = plus_project_id.clone();
                    store.update(cx, |store, cx| {
                        if quick_chat {
                            store.start_scratch_draft(cx);
                        } else {
                            store.start_draft(project_id, cwd, cx);
                        }
                    });
                })
        });
        let header = self.track_hover(header, &group_key);
        if quick_chat {
            return header.into_any_element();
        }
        let scope = store.read(cx).scope().clone();
        // Another machine's project is not on this client to reveal.
        let local = !store.read(cx).is_remote();
        header
            .context_menu(move |menu, _window, cx| {
                let id = menu_project.clone();
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
                            .child(crate::widgets::fade_text(delete_label.clone()))
                    })
                })
                .when(local, |menu| {
                    menu.menu(
                        crate::tr!("sidebar.reveal_project").into_owned(),
                        Box::new(ProjectReveal(id)),
                    )
                })
                .when_some(share.as_ref(), |menu, share| {
                    spaces::share_items(menu, share, false, cx)
                })
            })
            .touch(false)
            .reveals(&self.hover, group_key)
            .into_any_element()
    }

    /// The parts of `host`'s heading every layout draws: its link's dot,
    /// how the link is while it is not up, Pair again while the host refuses
    /// this device, and the menu of its actions.
    fn host_heading_parts(
        &self,
        host: &HostKey,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<HostHeadingParts> {
        let store = self.store_of(host)?;
        let store = store.read(cx);
        let state = store.connection_state();
        let failure = crate::remote::connection_failure(state.clone());
        let up = failure.is_none();
        let title = store.host_title();
        let menu = HostMenu {
            host: host.clone(),
            projects: (up && store.scope().is_full()).then(|| {
                store
                    .projects()
                    .into_iter()
                    .map(|project| {
                        let key = ProjectKey {
                            host: host.clone(),
                            project_id: project.id,
                        };
                        (key, project.name)
                    })
                    .collect()
            }),
            sort: up.then(|| crate::settings::project_sort_label(store.project_sort())),
        };
        let status = (!matches!(
            state,
            tcode_client::ConnectionState::Connected { .. }
                | tcode_client::ConnectionState::Syncing { .. }
        ))
        .then(|| crate::remote::connection_label_beside_pair_again(&state));
        let refused = failure == Some(tcode_client::ConnectionFailure::AuthenticationRejected);
        let id = host.name();
        let part = |part: &str| SharedString::from(format!("host-{part}-{id}"));
        let dot = transition(
            (part("dot"), "connection"),
            cx.theme().connection_color(&state),
            gpui_base::motion::Transition::new(GLIDE),
            window,
            cx,
        );
        let status_id = part("status");
        let status =
            crate::motion::shown(status_id.clone(), status, SHOW, window, cx).map(|shown| {
                shown.fold(
                    div()
                        .debug_selector(move || status_id.to_string())
                        .text_secondary(compact, cx)
                        .overflow_hidden()
                        .child(crate::widgets::fade_text(shown.value.clone())),
                )
            });
        let pair = crate::motion::shown(part("pair"), refused.then_some(()), SHOW, window, cx).map(
            |shown| {
                let pair_id = part("pair-again");
                shown.fade(
                    Button::new(pair_id.clone())
                        .debug_selector(move || pair_id.to_string())
                        .primary()
                        .xsmall()
                        .compact()
                        .label(crate::tr!("hosts.pair_again"))
                        // A new pairing needs a new invitation from the machine.
                        .on_click(cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.window_state
                                .update(cx, |state, cx| state.go(Destination::Pair, cx));
                        })),
                )
            },
        );
        let menu_id = gpui::ElementId::from(part("menu"));
        let (menu_focus, menu_focused) =
            crate::motion::focus_ring::focus_reveal(&menu_id, true, window, cx);
        let reveal = if compact {
            1.
        } else {
            self.hover_progress(&format!("host-{id}"), window, cx)
                .max(menu_focused)
        };
        let menu = Button::new(menu_id)
            .track_focus(&menu_focus)
            .ghost()
            .xsmall()
            .icon(Icon::new(IconName::Ellipsis).text_color(cx.theme().muted_foreground))
            .aria_label(crate::tr!("hosts.actions", name = title.clone()))
            // Opacity keeps the button a tab stop, as a project's "+" does.
            .opacity(reveal)
            .dropdown_menu(move |popup, _, _| menu.build(popup))
            .touch(compact);
        Some(HostHeadingParts {
            title,
            dot,
            status,
            pair,
            // The heading must not arm its own press before the menu opens.
            menu: div()
                .flex_none()
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(menu)
                .into_any_element(),
        })
    }

    /// The heading of `host`'s section of a list over several hosts.
    /// `collapsed` is `None` where nothing under it folds.
    fn render_host_heading(
        &self,
        host: &HostKey,
        collapsed: Option<bool>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(parts) = self.host_heading_parts(host, false, window, cx) else {
            return div().into_any_element();
        };
        let id = host.name().to_owned();
        let group = format!("host-{id}");
        let toggle = host.clone();
        let heading = selectable_row(
            crate::material::accessible_clickable(
                h_flex(),
                SharedString::from(format!("host-heading-{id}")),
                Role::Button,
                crate::tr!("sidebar.host", name = parts.title.clone()),
                cx,
            ),
            false,
            false,
            cx,
        )
        .debug_selector({
            let id = id.clone();
            move || format!("host-heading-{id}")
        })
        .text_body(false, cx)
        .when_some(collapsed, |heading, collapsed| {
            heading
                .aria_expanded(!collapsed)
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_host(&toggle, cx)))
                .child(collapse_chevron(
                    self.fold_progress(&fold_key(Fold::Host, &id), !collapsed),
                    cx,
                ))
        })
        .child(crate::material::status_dot(parts.dot))
        .child(
            sidebar_label(parts.title)
                .font_semibold()
                .text_color(cx.theme().sidebar_foreground),
        )
        .children(parts.pair)
        .child(parts.menu);
        v_flex()
            .w_full()
            .child(self.track_hover(heading, &group))
            .children(
                parts
                    .status
                    .map(|status| div().pl(px(THREAD_INDENT)).pr_2().child(status)),
            )
            .into_any_element()
    }

    fn render_grouped_row(
        &self,
        index: usize,
        row: &GroupedListRow,
        lists: &[HostLists],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let item = v_flex().w_full().px_2().when(
            index > 0 && matches!(row, GroupedListRow::Project { .. }),
            |item| item.pt(px(2.)),
        );
        let project = |host: &usize, group: &usize| {
            let of = &lists[*host];
            (of, &of.groups[*group].project)
        };
        match row {
            GroupedListRow::Host { host, collapsed } => item
                .when(index > 0, |item| item.pt(cx.theme().tokens.spacing.sm))
                .child(self.render_host_heading(&lists[*host].host, Some(*collapsed), window, cx))
                .into_any_element(),
            GroupedListRow::Project {
                host,
                group,
                collapsed,
            } => {
                let of = &lists[*host];
                item.child(self.render_group_header(
                    &of.host,
                    &of.groups[*group],
                    &of.flags,
                    *collapsed,
                    window,
                    cx,
                ))
                .into_any_element()
            }
            GroupedListRow::Thread(row) => {
                let Some(of) = lists.iter().find(|of| of.host == row.host) else {
                    return item.into_any_element();
                };
                item.child({
                    let thread = self.render_thread(row, &of.flags, window, cx);
                    drag_ghost::slot(arrange::thread_slot(&row.name), thread, window, cx)
                })
                .into_any_element()
            }
            GroupedListRow::ToggleThreads {
                host,
                group,
                expanded,
                label,
            } => {
                let (of, project) = project(host, group);
                let name = self.name(&of.host, &project.id).into_owned();
                let toggle = ProjectKey {
                    host: of.host.clone(),
                    project_id: project.id.clone(),
                };
                item.child(
                    div().w_full().pl(px(THREAD_INDENT)).child(
                        Button::new(SharedString::from(format!("show-more-{name}")))
                            .text()
                            .xsmall()
                            .label(label.clone())
                            .aria_expanded(*expanded)
                            .debug_selector(move || format!("show-more-{name}"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.expanded_groups.remove(&toggle) {
                                    this.expanded_groups.insert(toggle.clone());
                                }
                                cx.notify();
                            })),
                    ),
                )
                .into_any_element()
            }
            GroupedListRow::Settled { host, group, count } => {
                let (of, project) = project(host, group);
                let shelf = Shelf::Project(ProjectKey {
                    host: of.host.clone(),
                    project_id: project.id.clone(),
                });
                item.child(self.render_settled_header(&shelf, *count, None, window, cx))
                    .into_any_element()
            }
            GroupedListRow::SettledMore { host, group, count } => {
                let (of, project) = project(host, group);
                let shelf = Shelf::Project(ProjectKey {
                    host: of.host.clone(),
                    project_id: project.id.clone(),
                });
                item.child(self.render_settled_more(&shelf, *count, cx))
                    .into_any_element()
            }
            GroupedListRow::Hint(hint) => item
                .pb_2()
                .child(empty_row(hint.clone(), cx))
                .into_any_element(),
        }
    }

    fn thread_row_state(
        &self,
        store: &WorkspaceStore,
        row: &ListedThread,
        flags: &HashMap<String, ThreadFlags>,
        row_key: String,
    ) -> ThreadRowState {
        let meta = &row.meta;
        let thread = row.key();
        let own_flags = flags.get(&meta.id).cloned().unwrap_or_default();
        ThreadRowState {
            renaming: self
                .renaming
                .as_ref()
                .filter(|rename| rename.thread == thread)
                .map(|rename| rename.input.clone()),
            thread,
            name: row.name.clone(),
            row_key,
            waiting_for_approval: own_flags.waiting_for_approval,
            waiting_for_input: own_flags.waiting_for_input,
            settle_blocked: own_flags.settle_blocked,
            background: own_flags.background,
            unsettled: own_flags.unsettled,
            wakes: own_flags.wakes,
            failed: own_flags.failed,
            auto_settle_enabled: meta.auto_settle_disabled_at.is_none(),
            is_worktree: meta.worktree.is_some(),
            show_unread: own_flags.unread && !own_flags.working,
            menu_can_fork: own_flags.forks,
            title_generating: store.title_generating(&meta.id),
        }
    }

    /// The decorative provider mark painted under a thread row's content:
    /// `meta`'s protocol glyph in its provider color, fitted to the row height
    /// minus [`PROVIDER_MARK_INSET`], vertically centred and right-aligned
    /// inside the row's `right_padding`. `None` while Provider marks is off.
    /// It has no handlers, so the row's own hover, click and menu are unaffected.
    fn provider_mark(
        store: &WorkspaceStore,
        meta: &SessionMeta,
        name: &str,
        row_height: f32,
        right_padding: f32,
    ) -> Option<gpui::Div> {
        let color: gpui::Hsla = gpui::rgb(store.provider_color(meta)?).into();
        let glyph = crate::provider_card::provider_glyph(meta.provider);
        let size = px(row_height - PROVIDER_MARK_INSET);
        let id = name.to_owned();
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
    pub(crate) fn hovered_thread(&self) -> Option<ThreadKey> {
        let row = self.hover.hovered()?;
        let thread = row
            .strip_prefix("flat-thread-")
            .or_else(|| row.strip_prefix("thread-"))?;
        self.thread_named(thread)
    }

    /// The map of the panes, on a row whose thread one shows: the column's
    /// panes in a tiny grid, each laid out as it lies, those showing this
    /// thread filled. It widens into the row as it comes, the title giving it
    /// room gradually, and narrows away as it goes.
    fn pane_glyph(
        &self,
        state: &ThreadRowState,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<gpui::AnyElement> {
        let row = &state.thread;
        let map = self.shown.cells(row).next().map(|_| self.shown.map.clone());
        let id = SharedString::from(format!("pane-glyph-{}", state.name));
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
            .h(px(GLYPH_HEIGHT));
        for cell in shown.value.iter() {
            let fill = transition(
                (id.clone(), format!("cell-{}", cell.pane.as_u64())),
                if cell.row.as_ref() == Some(row) {
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
    fn current_row(&self, cx: &App) -> Option<ThreadKey> {
        let thread = crate::shell::current_thread(cx)?;
        let store = self.store_of(&thread.host)?;
        let session_id = store.read(cx).roster_row(&thread.session_id);
        Some(ThreadKey {
            host: thread.host,
            session_id,
        })
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
        let thread = state.thread.clone();
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
        let places: Vec<_> = self
            .shown
            .cells(&state.thread)
            .map(MapCell::place)
            .collect();
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
                    let name = state.name.clone();
                    move || format!("sidebar-thread-{name}")
                })
                .cursor_pointer()
                .on_click(
                    cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                        if event.modifiers().alt && !this.compact(cx) {
                            window.dispatch_action(
                                Box::new(crate::panes::OpenInNewPane(thread.clone())),
                                cx,
                            );
                            return;
                        }
                        let Some(store) = this.store_of(&thread.host) else {
                            return;
                        };
                        this.compact_model_dirty = true;
                        store.update(cx, |store, cx| {
                            store.show_thread(thread.session_id.clone(), cx);
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
    ) -> gpui::Div {
        let title = if let Some(input) = &state.renaming {
            self.rename_input(input, cx).into_any_element()
        } else {
            sidebar_label(meta.title.clone())
                // As wide as the title, so the row knows what it cuts.
                .flex_auto()
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
            .flex_auto()
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
        name: &str,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<gpui::AnyElement> {
        let shown = crate::motion::shown(
            SharedString::from(format!("follow-up-shown-{name}")),
            meta.follow_up.is_some().then_some(()),
            SHOW,
            window,
            cx,
        )?;
        Some(
            shown.fold_across_gap(
                ROW_GAP,
                div()
                    .id(SharedString::from(format!("follow-up-{name}")))
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
        store: &Entity<WorkspaceStore>,
        listed: &ListedThread,
        show: bool,
        gap: gpui::Pixels,
        slot: &'static str,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<gpui::AnyElement> {
        let (meta, name) = (&listed.meta, &listed.name);
        let links = show
            .then(|| meta.pull_requests.clone())
            .filter(|links| crate::pull_requests::badge_label(links, cx).is_some());
        let shown = crate::motion::shown(
            SharedString::from(format!("{slot}-{name}")),
            links,
            SHOW,
            window,
            cx,
        )?;
        let badge = crate::pull_requests::sidebar_badge(&shown.value, &meta.id, store.clone(), cx)?;
        Some(shown.fold_across_gap(gap, badge))
    }

    fn pin_glyph(
        &self,
        meta: &SessionMeta,
        state: &ThreadRowState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Div> {
        if meta.pinned_at.is_none() || meta.is_settled() {
            return None;
        }
        let thread = state.thread.clone();
        let label = crate::tr!("sidebar.unpin_tooltip");
        let button_id =
            gpui::ElementId::from(SharedString::from(format!("unpin-thread-{}", state.name)));
        let (focus, focused) =
            crate::motion::focus_ring::focus_reveal(&button_id, true, window, cx);
        let hover = self.hover_progress(&state.row_key, window, cx).max(focused);
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
                            this.unpin_thread(&thread, window, cx);
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
        } else if state.unsettled {
            Some((cx.theme().warning, crate::tr!("sidebar.unsettled"), false))
        } else if state.background != (0, 0) {
            Some((
                cx.theme().muted_foreground,
                crate::tr!("sidebar.background"),
                true,
            ))
        } else if !state.wakes.is_empty() {
            Some((
                cx.theme().muted_foreground,
                crate::tr!("sidebar.waiting"),
                false,
            ))
        } else {
            None
        }
    }

    /// The dot leading a thread row's title: unread over the status's colour,
    /// breathing while only background work keeps the thread going; none
    /// while the row is renamed. A change cross-fades.
    fn status_dot(
        name: &str,
        show_unread: bool,
        renaming: bool,
        status: Option<(gpui::Hsla, bool)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let dot = (!renaming)
            .then(|| show_unread.then(|| (cx.theme().primary, false)).or(status))
            .flatten();
        faded_status(
            format!("status-dot-{name}"),
            dot,
            window,
            cx,
            |(color, breathes), _, _| {
                if *breathes {
                    crate::widgets::BreathingDot::new(*color).into_any_element()
                } else {
                    div()
                        .size(px(6.))
                        .rounded_full()
                        .bg(*color)
                        .into_any_element()
                }
            },
        )
    }

    /// The word naming a thread row's status, its tooltip what keeps the
    /// thread going or why it failed. A change cross-fades, and a status
    /// that ends fades out in its place.
    fn status_word(
        state: &ThreadRowState,
        status: Option<(gpui::Hsla, std::borrow::Cow<'static, str>, bool)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Stateful<gpui::Div>> {
        let name = &state.name;
        let word = faded_status(
            format!("status-word-{name}"),
            status.map(|(color, label, _)| (SharedString::from(label), color)),
            window,
            cx,
            |(label, color), _, cx| {
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_secondary(false, cx)
                    .text_color(*color)
                    .child(crate::widgets::fade_text(label.clone()))
                    .into_any_element()
            },
        )?;
        let keeps = keeps_going(state);
        Some(
            div()
                .id(SharedString::from(format!("thread-status-{name}")))
                .when_some(keeps, |word, keeps| {
                    word.with_tooltip(move |_, _| Tooltip::new(keeps.clone()))
                })
                .when(state.failed && !state.waiting(), |word| {
                    word.with_tooltip(|_, _| Tooltip::new(crate::tr!("sidebar.failed_tooltip")))
                })
                .child(word),
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
        let Some(store) = self.store_of(&state.thread.host) else {
            return row.into_any_element();
        };
        let share = self.share_target(&state.thread.host, meta.project_id.as_deref(), cx);
        let scope = store.read(cx).scope().clone();
        let sidebar = cx.entity().downgrade();
        let thread = state.thread.clone();
        let can_fork = state.menu_can_fork;
        let is_worktree = state.is_worktree;
        let title_generating = state.title_generating;
        let settle_blocked = state.settle_blocked;
        let auto_settle_enabled = state.auto_settle_enabled;
        let follow_up = meta.follow_up.is_some();
        let parked = meta.parked_at.is_some();
        let hover = self.hover.clone();
        let row_key = state.row_key.clone();
        let grant = crate::control::thread_grant_line(&store.read(cx).settings().control, meta);
        let menu = row
            .context_menu(move |menu, window, cx| {
                let id = thread.clone();
                // Move up / Move down stay within the section the list shows.
                let moves = sidebar
                    .upgrade()
                    .filter(|_| !settled && scope.is_full())
                    .and_then(|sidebar| sidebar.read(cx).move_bounds(&id, cx));
                let open_beside = crate::shell::pane_controls(window, cx)
                    .map(|panes| panes.split_right || crate::shell::shown_in_pane(&thread, cx));
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
                .when(title_generating, |menu| {
                    menu.menu(
                        crate::tr!("sidebar.ctx_stop_title").into_owned(),
                        Box::new(ThreadStopTitle(id.clone())),
                    )
                })
                .when(!title_generating, |menu| {
                    menu.menu(
                        crate::tr!("sidebar.ctx_regenerate_title").into_owned(),
                        Box::new(ThreadRegenerateTitle(id.clone())),
                    )
                })
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
                    settled || !settle_blocked,
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
                            .child(crate::widgets::fade_text(crate::tr!("sidebar.ctx_delete")))
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
        listed: &ListedThread,
        flags: &HashMap<String, ThreadFlags>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(store) = self.store_of(&listed.host) else {
            return div().into_any_element();
        };
        let meta = &listed.meta;
        let working = flags.get(&meta.id).is_some_and(|flags| flags.working);
        let state = self.thread_row_state(
            store.read(cx),
            listed,
            flags,
            format!("thread-{}", listed.name),
        );
        let light = self.shown.light(&state.thread);
        let name = state.name.clone();
        let is_worktree = state.is_worktree;
        let show_unread = state.show_unread;
        let status = Self::thread_status_label(&state, working, cx);

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
            .children(Self::provider_mark(
                store.read(cx),
                meta,
                &name,
                crate::material::selectable_row_height(false),
                crate::material::selectable_row_inset(false).into(),
            ))
            .children(Self::status_dot(
                &name,
                show_unread,
                state.renaming.is_some(),
                status
                    .as_ref()
                    .map(|(color, _, breathes)| (*color, *breathes)),
                window,
                cx,
            ));

        // Row body: rename input, or the worktree glyph, the title and the
        // status word beside it, which gives up its width before the title.
        let row = if state.renaming.is_some() {
            row.child(self.thread_title_or_input(meta, &state, false, window, cx))
        } else {
            row.when(is_worktree, |row| {
                row.child(
                    Icon::empty()
                        .path("icons/git-branch.svg")
                        .text_color(cx.theme().muted_foreground),
                )
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        self.thread_title_or_input(meta, &state, false, window, cx)
                            .flex_initial(),
                    )
                    .children(Self::status_word(&state, status, window, cx).map(|word| {
                        word.flex_shrink(1000.)
                            .min_w_0()
                            .overflow_hidden()
                            .pl(ROW_GAP)
                    })),
            )
            .children(Self::follow_up_glyph(meta, &name, window, cx))
            .children(self.pin_glyph(meta, &state, window, cx))
            .children(Self::pr_badge(
                &store,
                listed,
                state.renaming.is_none(),
                ROW_GAP,
                "pr-badge-row",
                window,
                cx,
            ))
            .children(self.pane_glyph(&state, window, cx))
            // The time narrows away while the thread works, giving the
            // title its width gradually, and widens back after.
            .when_some(
                crate::motion::shown(
                    SharedString::from(format!("thread-time-{name}")),
                    (!working).then_some(()),
                    SHOW,
                    window,
                    cx,
                ),
                |row, shown| {
                    let slot = self.render_flat_thread_right_slot(
                        meta,
                        &state,
                        project_view_timestamp(meta),
                        !state.settle_blocked,
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
    /// A settled row offers to make it active under the pointer, and an
    /// active one to settle it while it `offers_settle`.
    fn render_flat_thread_right_slot(
        &self,
        meta: &SessionMeta,
        state: &ThreadRowState,
        timestamp: u64,
        offers_settle: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let action_id = state.thread.clone();
        let row_key = state.row_key.as_str();
        let settled = meta.is_settled();
        let ago = self
            .store_of(&state.thread.host)
            .map(|store| humanize_ago(store.read(cx).host_now_secs(cx).saturating_sub(timestamp)));
        let action_on_hover = settled || offers_settle;
        let label = if settled {
            crate::tr!("sidebar.unsettle_tooltip")
        } else {
            crate::tr!("sidebar.settle_tooltip")
        };
        let button_id = gpui::ElementId::from(SharedString::from(format!(
            "settle-flat-thread-{}",
            state.name
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
                    .children(ago.map(crate::widgets::fade_text)),
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
        listed: &ListedThread,
        flags: &HashMap<String, ThreadFlags>,
        project_name: Option<String>,
        host_name: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(store) = self.store_of(&listed.host) else {
            return div().into_any_element();
        };
        let meta = &listed.meta;
        let working = flags.get(&meta.id).is_some_and(|flags| flags.working);
        let state = self.thread_row_state(
            store.read(cx),
            listed,
            flags,
            format!("flat-thread-{}", listed.name),
        );
        let light = self.shown.light(&state.thread);
        let name = state.name.clone();
        let show_unread = state.show_unread;
        let renaming = state.renaming.is_some();
        let status = Self::thread_status_label(&state, working, cx);

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
                let name = name.clone();
                move || format!("sidebar-thread-{name}")
            })
            .items_stretch()
            .justify_center()
            // Ahead of the row's content, which paints over the mark.
            .relative()
            .children(Self::provider_mark(
                store.read(cx),
                meta,
                &name,
                crate::material::selectable_row_two_line_height(false),
                crate::material::selectable_row_inset(false).into(),
            ));

        let row = {
            let title_or_input = self.thread_title_or_input(meta, &state, true, window, cx);
            let dot = Self::status_dot(
                &name,
                show_unread,
                renaming,
                status
                    .as_ref()
                    .map(|(color, _, breathes)| (*color, *breathes)),
                window,
                cx,
            );
            let line_one = h_flex()
                .w_full()
                .min_w_0()
                .items_center()
                .gap(px(6.))
                .children(dot)
                .child(title_or_input)
                .when(!renaming, |line| {
                    line.children(Self::follow_up_glyph(meta, &name, window, cx))
                        .children(self.pin_glyph(meta, &state, window, cx))
                        .children(self.pane_glyph(&state, window, cx))
                        .child(self.render_flat_thread_right_slot(
                            meta,
                            &state,
                            tcode_core::thread_sort::shown_timestamp(meta),
                            !state.settle_blocked,
                            window,
                            cx,
                        ))
                });

            let has_project = project_name.is_some();
            let has_status = status.is_some();
            let status_word = Self::status_word(&state, status, window, cx);
            let line_two = h_flex()
                .w_full()
                .min_w_0()
                .items_center()
                .gap_1()
                .text_secondary(false, cx)
                .children(status_word.map(|word| word.flex_none()))
                .when(has_status && has_project, |line| {
                    line.child(div().flex_none().child("·"))
                })
                .when_some(project_name, |line, project_name| {
                    let store = store.read(cx);
                    line.child(
                        store
                            .project(meta.project_id.as_deref().unwrap_or_default())
                            .map(|project| {
                                crate::project_icon::artwork(store.image_host(), project, 12.)
                                    .into_any_element()
                            })
                            .unwrap_or_else(|| Icon::new(IconName::Folder).into_any_element()),
                    )
                    .child(sidebar_label(project_name))
                })
                // Without a project label there is no flex-1 element on the
                // line, so a spacer keeps the host and worktree glyph
                // bottom-right.
                .when(!has_project, |line| line.child(div().flex_1()))
                .when_some(host_name, |line, host| {
                    line.child(
                        div()
                            .debug_selector({
                                let name = name.clone();
                                move || format!("flat-host-{name}")
                            })
                            .min_w_0()
                            .max_w(gpui::relative(0.5))
                            .overflow_hidden()
                            .child(crate::widgets::fade_text(host)),
                    )
                })
                .when(meta.worktree.is_some(), |line| {
                    line.child(
                        Icon::empty()
                            .path("icons/git-branch.svg")
                            .text_color(cx.theme().muted_foreground),
                    )
                });
            let line_two = line_two.children(Self::pr_badge(
                &store,
                listed,
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
            .child(crate::widgets::fade_text(crate::tr!("settings.title"))),
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
            let _ = cx.update(|window, cx| match count {
                Ok(count) => {
                    open_delete_confirmation(store, session_id, title, ok_text, count, window, cx)
                }
                // Without the archived threads the dialog could not say how
                // many threads the delete takes.
                Err(error) => window.push_notification(
                    Notification::error(
                        crate::tr!("settings.archived_failed", error = error).into_owned(),
                    ),
                    cx,
                ),
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
        // The list is drawn again as this client's minute turns; each row
        // tells its time by its own host's clock.
        let now = now_secs(cx);
        let host_nows: Vec<(HostKey, u64)> = self
            .hosts
            .iter()
            .map(|host| (host.key.clone(), host.store.read(cx).host_now_secs(cx)))
            .collect();
        let host_now = move |host: &HostKey| {
            host_nows
                .iter()
                .find(|(key, _)| key == host)
                .map(|(_, now)| *now)
        };
        let locale = rust_i18n::locale();
        if self.compact_model_dirty
            || self
                .compact_model
                .as_ref()
                .is_none_or(|model| model.locale.as_str() != &*locale)
        {
            let swiped_off = self.swiped_off(cx);
            let lists = self.thread_lists(cx);
            let layout = DevicePreferences::sidebar_layout(cx);
            let kept = |host: &HostKey, meta: &SessionMeta| {
                !swiped_off
                    .iter()
                    .any(|thread| thread.host == *host && thread.session_id == meta.id)
            };
            // Each host's threads and groups, less those swiped off.
            let hosts: Vec<(HostKey, Vec<SessionMeta>, Vec<ProjectGroup>)> = lists
                .iter()
                .map(|lists| {
                    let mut sessions = (*lists.flat).clone();
                    sessions.retain(|meta| kept(&lists.host, meta));
                    let mut groups = (*lists.groups).clone();
                    for group in &mut groups {
                        group.sessions.retain(|meta| kept(&lists.host, meta));
                    }
                    (lists.host.clone(), sessions, groups)
                })
                .collect();
            let collapsed_projects: HashSet<ProjectKey> = hosts
                .iter()
                .filter_map(|(host, _, groups)| {
                    let store = self.store_of(host)?;
                    let store = store.read(cx);
                    Some(
                        groups
                            .iter()
                            .filter(|group| store.is_project_collapsed(&group.project.id))
                            .map(|group| ProjectKey {
                                host: host.clone(),
                                project_id: group.project.id.clone(),
                            })
                            .collect::<Vec<_>>(),
                    )
                })
                .flatten()
                .collect();
            let group_count: usize = hosts.iter().map(|(_, _, groups)| groups.len()).sum();
            let sectioned = self.sectioned();
            let host_row = |host: &HostKey, collapsed| {
                CompactListRow::Host(CompactHostRow {
                    host: host.clone(),
                    row_id: format!("compact-host-{}", host.name()).into(),
                    collapsed,
                })
            };

            let mut rows = Vec::new();
            let mut folds: Vec<f32> = Vec::new();
            let thread_rows = |visible: Vec<Listed>, recent: bool| {
                let mut rows = Vec::new();
                let last = visible.len().saturating_sub(1);
                for (index, (host, meta)) in visible.into_iter().enumerate() {
                    let (Some(store), Some(of)) = (
                        self.store_of(host),
                        lists.iter().find(|lists| lists.host == *host),
                    ) else {
                        continue;
                    };
                    let listed = self.listed((host, meta));
                    let state = self.thread_row_state(
                        store.read(cx),
                        &listed,
                        &of.flags,
                        format!("compact-thread-{}", listed.name),
                    );
                    let project_name = recent
                        .then(|| {
                            hosts.iter().find(|(other, _, _)| other == host).and_then(
                                |(_, _, groups)| {
                                    groups.iter().find(|group| {
                                        meta.project_id.as_deref()
                                            == Some(group.project.id.as_str())
                                    })
                                },
                            )
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
                    let host_name = (recent && sectioned).then(|| store.read(cx).host_title());
                    rows.push(CompactListRow::Thread(Rc::new(CompactThreadRow {
                        title: meta.title.clone().into(),
                        row_id: format!("compact-thread-row-{}", listed.name).into(),
                        label: crate::tr!("sidebar.thread", title = meta.title.clone())
                            .into_owned()
                            .into(),
                        timestamp,
                        relative_time: humanize_ago(
                            store.read(cx).host_now_secs(cx).saturating_sub(timestamp),
                        )
                        .into(),
                        working: of.flags.get(&meta.id).is_some_and(|flags| flags.working),
                        state,
                        meta: meta.clone(),
                        project_name,
                        host_name,
                        separator: index != last,
                    })));
                }
                rows
            };
            if layout == SidebarLayout::Flat {
                let key = "recent";
                let shelf = Shelf::Recent;
                rows.extend(
                    lists
                        .iter()
                        .filter(|lists| lists.down)
                        .map(|lists| host_row(&lists.host, None)),
                );
                let ThreadSections {
                    pinned,
                    active,
                    settled,
                } = merged(
                    hosts
                        .iter()
                        .map(|(host, sessions, _)| {
                            tagged_sections(host, project_threads(sessions, None))
                        })
                        .collect(),
                );
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
                        shelf: shelf.clone(),
                        count: settled.len(),
                    });
                    let count = settled.len();
                    let mut visible = settled;
                    self.limit_settled_rows(&shelf, &mut visible);
                    let hidden = count - visible.len();
                    let more = self.settled_more_count(&shelf, hidden);
                    let (shelf_folds, more_factor) =
                        self.shelf_factors(&shelf, &visible, more.is_some());
                    folds.resize(rows.len(), 1.);
                    rows.extend(thread_rows(visible, true));
                    folds.extend(shelf_folds);
                    if let Some(count) = more {
                        rows.push(CompactListRow::More {
                            key: format!("{key}-more"),
                            shelf,
                            count,
                        });
                        folds.push(more_factor);
                    }
                }
            } else {
                for (host, _, groups) in &hosts {
                    let host_collapsed = DevicePreferences::host_collapsed(host, cx);
                    let host_key = fold_key(Fold::Host, host.name());
                    if sectioned {
                        folds.resize(rows.len(), 1.);
                        rows.push(host_row(host, Some(host_collapsed)));
                        folds.push(1.);
                        if !self.fold_shown(&host_key, !host_collapsed) {
                            continue;
                        }
                    }
                    let host_start = rows.len();
                    for group in groups {
                        let project = ProjectKey {
                            host: host.clone(),
                            project_id: group.project.id.clone(),
                        };
                        let name = self.name(host, &group.project.id).into_owned();
                        let key = name.as_str();
                        let count = group.sessions.len();
                        let has_header = sectioned || group_count > 1 || group.project.quick_chat;
                        let collapsed = has_header && collapsed_projects.contains(&project);
                        let start = rows.len();
                        let project_key = fold_key(Fold::Project, key);
                        if self.fold_shown(&project_key, !collapsed) {
                            folds.resize(rows.len(), 1.);
                            let (active, settled) = project_group_threads(&group.sessions);
                            rows.extend(thread_rows(tagged(host, active).collect(), false));
                            if !settled.is_empty() {
                                let count = settled.len();
                                let shelf = Shelf::Project(project.clone());
                                rows.push(CompactListRow::Settled {
                                    key: key.into(),
                                    shelf: shelf.clone(),
                                    count,
                                });
                                let mut settled: Vec<Listed> = tagged(host, settled).collect();
                                self.limit_settled_rows(&shelf, &mut settled);
                                let hidden = count - settled.len();
                                let more = self.settled_more_count(&shelf, hidden);
                                let (shelf_folds, more_factor) =
                                    self.shelf_factors(&shelf, &settled, more.is_some());
                                folds.resize(rows.len(), 1.);
                                rows.extend(thread_rows(settled, false));
                                folds.extend(shelf_folds);
                                if let Some(count) = more {
                                    rows.push(CompactListRow::More {
                                        key: format!("{key}-more"),
                                        shelf,
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
                                    row_id: format!("compact-group-{name}").into(),
                                    id: name.clone(),
                                    project,
                                    name: crate::project_icon::group_label(&group.project).into(),
                                    label: if group.project.quick_chat {
                                        crate::tr!("sidebar.quick_chats").into_owned()
                                    } else {
                                        crate::tr!(
                                            "sidebar.project",
                                            name = group.project.name.clone()
                                        )
                                        .into_owned()
                                    }
                                    .into(),
                                    count: count.to_string().into(),
                                    collapsed,
                                }),
                            );
                        }
                    }
                    if sectioned {
                        folds.resize(rows.len(), 1.);
                        let progress = self.fold_progress(&host_key, !host_collapsed);
                        let count = rows.len() - host_start;
                        for (index, factor) in folds[host_start..].iter_mut().enumerate() {
                            *factor = nested_fold(fold_factor(progress, index, count), *factor);
                        }
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
                        CompactListRow::Thread(row) if row.state.thread == id => Some((
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
                has_rows: group_count > 0 || sectioned,
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
                    if let Some(now) = host_now(&row.state.thread.host) {
                        row.relative_time = humanize_ago(now.saturating_sub(row.timestamp)).into();
                    }
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
        let mut pending: Vec<(SharedString, (ThreadKey, String))> = Vec::new();
        for host in &self.hosts {
            let store = host.store.read(cx);
            let cached = store.sidebar_sessions();
            let loading = store.threads_loading();
            pending.extend(
                store
                    .pending_sessions()
                    .into_iter()
                    .filter(|(id, _)| loading || !cached.iter().any(|meta| &meta.id == id))
                    .map(|(id, preview)| {
                        let name = SharedString::from(self.name(&host.key, &id).into_owned());
                        let thread = ThreadKey {
                            host: host.key.clone(),
                            session_id: id,
                        };
                        (name, (thread, preview))
                    }),
            );
        }
        crate::motion::keyed(
            "pending-thread-rows",
            pending,
            ROWS,
            window,
            cx,
            |name, (thread, preview), _, cx| {
                let thread = thread.clone();
                crate::material::list_row(
                    SharedString::from(format!("pending-thread-{name}")),
                    format!("{}, {preview}", crate::tr!("chat.waiting_connection")).into(),
                    cx,
                )
                .debug_selector(|| "pending-thread-row".into())
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(cx.theme().tokens.spacing.xs)
                        .child(div().text_body(true, cx).child(crate::widgets::fade_text(
                            crate::tr!("chat.waiting_connection"),
                        )))
                        .child(
                            div()
                                .text_secondary(true, cx)
                                .overflow_hidden()
                                .child(crate::widgets::fade_text(preview.clone())),
                        ),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(store) = this.store_of(&thread.host) {
                        store.update(cx, |store, cx| {
                            store.show_thread(thread.session_id.clone(), cx);
                        });
                    }
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
            CompactListRow::Settled { shelf, count, .. } => {
                self.render_settled_header(shelf, *count, None, window, cx)
            }
            CompactListRow::Caption { label, .. } => {
                crate::material::list_caption(label.clone(), true, cx).into_any_element()
            }
            CompactListRow::More { shelf, count, .. } => {
                self.render_settled_more(shelf, *count, cx)
            }
            CompactListRow::Empty { .. } => self.render_active_empty(cx),
            CompactListRow::Host(row) => self.render_compact_host_heading(row, window, cx),
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
        let full_scope = self.full_scope(cx);
        self.tidy_row_swipes(cx);
        let model = self.compact_model(Some(window), cx);
        let layout = DevicePreferences::sidebar_layout(cx);
        let body = self.loading_body(
            move |this, window, cx| {
                // The empty page fades in where the last project's list was.
                let empty = crate::motion::shown(
                    "threads-empty",
                    (!model.has_rows).then_some(()),
                    SHOW,
                    window,
                    cx,
                );
                if let Some(empty) = empty.filter(|_| !model.has_rows) {
                    div()
                        .id("threads-empty")
                        .opacity(empty.progress())
                        .debug_selector(|| "threads-empty".into())
                        .size_full()
                        .child(crate::material::empty_state(
                            Icon::new(IconName::Folder),
                            if full_scope {
                                crate::tr!("mobile.projects_empty")
                            } else {
                                crate::tr!("member.empty_title")
                            },
                            if full_scope {
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
                                matches!(row, CompactListRow::Thread(row) if Some(&row.state.thread) == active.as_ref())
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
            .on_action(cx.listener(Self::on_stop_title))
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
            .on_action(cx.listener(Self::on_add_project))
            .on_action(cx.listener(Self::on_sort_projects))
            .on_action(cx.listener(Self::on_manage_hosts))
            .on_action(cx.listener(Self::on_host_settings))
            .on_action(cx.listener(Self::on_leave_host))
            .child(self.render_compact_search(cx))
            .child(
                bar("compact-layout-bar", Bar::Section, true, cx)
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_secondary(true, cx)
                            .child(crate::widgets::fade_text(match layout {
                                SidebarLayout::Flat => crate::tr!("sidebar.recent"),
                                SidebarLayout::Grouped => crate::tr!("sidebar.by_project"),
                            })),
                    )
                    .child(
                        div()
                            .debug_selector(|| "compact-layout-toggle".into())
                            .child(self.render_layout_toggle(layout, cx)),
                    ),
            )
            .when(!self.threads_loading(cx), |list| {
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

    /// A host's heading in the phone's list over several hosts: a section's
    /// caption, at body size since projects are captions under it.
    fn render_compact_host_heading(
        &self,
        row: &CompactHostRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(parts) = self.host_heading_parts(&row.host, true, window, cx) else {
            return div().into_any_element();
        };
        let id = row.host.name().to_owned();
        let toggle = row.host.clone();
        let spacing = &cx.theme().tokens.spacing;
        let heading = crate::material::accessible_clickable(
            h_flex(),
            row.row_id.clone(),
            Role::Button,
            crate::tr!("sidebar.host", name = parts.title.clone()),
            cx,
        )
        .debug_selector({
            let id = id.clone();
            move || format!("compact-host-{id}")
        })
        .w_full()
        .items_center()
        .gap(spacing.sm)
        .text_body(true, cx)
        .font_semibold()
        .child(crate::material::status_dot(parts.dot))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .child(crate::widgets::fade_text(parts.title)),
        )
        .children(parts.pair)
        .child(parts.menu)
        .when_some(row.collapsed, |heading, collapsed| {
            heading
                .aria_expanded(!collapsed)
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_host(&toggle, cx)))
                .child(
                    collapse_chevron(
                        self.fold_progress(&fold_key(Fold::Host, &id), !collapsed),
                        cx,
                    )
                    .text_color(cx.theme().foreground),
                )
        });
        v_flex()
            .w_full()
            .px(px(COMPACT_PAGE_INSET))
            .pt(spacing.lg)
            .pb(spacing.xs)
            .child(heading)
            .children(parts.status)
            .into_any_element()
    }

    fn render_compact_group_header(
        &self,
        row: &CompactProjectRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let project = row.project.clone();
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
            this.toggle_project(&project, cx);
        }))
        .text_caption(cx)
        .when_some(
            self.store_of(&row.project.host).and_then(|store| {
                let store = store.read(cx);
                let project = store.project(&row.project.project_id)?;
                Some(crate::project_icon::artwork(
                    store.image_host(),
                    project,
                    14.,
                ))
            }),
            |el, artwork| el.child(artwork),
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
        .children(self.shared_badge(&row.project, window, cx))
        .child(div().flex_none().child(row.count.clone()))
        .child(
            collapse_chevron(
                self.fold_progress(&fold_key(Fold::Project, &row.id), !collapsed),
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
        let meta = &cached.meta;
        let state = &cached.state;
        let Some(store) = self.store_of(&state.thread.host) else {
            return div().into_any_element();
        };
        let working = cached.working;
        let project_name = cached.project_name.clone();
        let project = if project_name.is_some() {
            store
                .read(cx)
                .project(meta.project_id.as_deref().unwrap_or_default())
                .cloned()
        } else {
            None
        };
        let name = state.name.clone();
        let status = faded_status(
            format!("compact-status-{name}"),
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
                            .child(crate::widgets::fade_text(label.clone())),
                    )
                    .child("·")
                    .into_any_element()
            },
        );
        let click = state.thread.clone();
        let renaming = self
            .renaming
            .as_ref()
            .filter(|rename| rename.thread == state.thread);
        let mark = Self::provider_mark(
            store.read(cx),
            meta,
            &name,
            crate::material::LIST_ROW_MIN_HEIGHT,
            COMPACT_PAGE_INSET,
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
                let name = name.clone();
                move || format!("compact-row-{name}")
            })
            // First child so the row's content paints over the mark.
            .relative()
            .children(mark)
            .bg(fill)
            .on_click(cx.listener(move |this, _, _, cx| {
                if this
                    .renaming
                    .as_ref()
                    .is_some_and(|rename| rename.thread == click)
                {
                    return;
                }
                let Some(store) = this.store_of(&click.host) else {
                    return;
                };
                let id = &click.session_id;
                if store.read(cx).is_thread_open(id) && store.read(cx).session_loading_for(id) {
                    return;
                }
                store.update(cx, |store, cx| {
                    store.show_thread(click.session_id.clone(), cx);
                });
            }))
            .child(compact_status_glyph(
                store.read(cx).image_host(),
                &name,
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
                                        let name = name.clone();
                                        move || format!("compact-title-{name}")
                                    })
                                    .into_any_element()
                            })
                            .children(
                                crate::motion::shown(
                                    SharedString::from(format!("compact-pin-{name}")),
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
                            .when_some(cached.host_name.clone(), |line, host| {
                                line.child(
                                    div()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .child(crate::widgets::fade_text(host)),
                                )
                                .child(div().flex_none().child("·"))
                            })
                            .children(status)
                            .when_some(
                                crate::pull_requests::badge(&meta.pull_requests, cx),
                                |line, badge| line.child(badge).child("·"),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .child(crate::widgets::fade_text(cached.relative_time.clone())),
                            ),
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
        } else if state.unsettled {
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
    } else if state.unsettled {
        Some((crate::tr!("sidebar.unsettled"), cx.theme().warning))
    } else if state.background != (0, 0) {
        Some((
            crate::tr!("sidebar.background"),
            cx.theme().muted_foreground,
        ))
    } else if state.show_unread {
        Some((crate::tr!("mobile.unread"), cx.theme().primary))
    } else if !state.wakes.is_empty() {
        Some((crate::tr!("sidebar.waiting"), cx.theme().muted_foreground))
    } else {
        None
    }
}

impl SessionsSidebar {
    /// The Grouped layout's header and list.
    fn render_grouped_layout(
        &mut self,
        lead: &Entity<WorkspaceStore>,
        lists: ThreadLists,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (gpui::AnyElement, gpui::AnyElement) {
        let mut rows = self.grouped_rows_folding(&lists, cx);
        if lists.iter().all(|lists| lists.groups.is_empty()) {
            let hint = if self.full_scope(cx) {
                crate::tr!("sidebar.empty")
            } else {
                crate::tr!("member.empty_description")
            };
            rows.push((GroupedListRow::Hint(hint), 1.));
        }
        let all: Vec<(u64, f32)> = rows
            .iter()
            .map(|(row, factor)| (presence::row_key(row.key(&lists)), *factor))
            .collect();
        rows.retain(|(_, factor)| *factor >= 0.);
        let keys = rows.iter().map(|(row, _)| row.key(&lists));
        let drawn = self.grouped_drawn.clone();
        self.grouped_presence.update(
            &all,
            self.folding,
            &self.grouped_list_state,
            |index| (drawn.clone(), index),
            cx,
        );
        if !keys.clone().eq(self.grouped_row_keys.iter().map(drawn_key)) {
            replace_gliding_rows(
                &self.grouped_list_state,
                &self.grouped_flip,
                |index| self.grouped_row_keys.get(index).map(drawn_key),
                keys.clone(),
                window,
            );
            self.grouped_row_keys = keys.map(|(kind, id)| (kind, id.into_owned())).collect();
        }
        self.grouped_presence
            .settle(&self.grouped_list_state, window, cx);
        let (rows, folds) = rows.into_iter().unzip();
        let frame = Rc::new(GroupedFrame { rows, folds, lists });
        self.grouped_drawn = frame.clone();
        let flip = self.grouped_flip.clone();
        let thread_list = list(
            self.grouped_list_state.clone(),
            cx.processor(move |this, index: usize, window, cx| {
                let Some(row) = frame.rows.get(index) else {
                    return div().into_any_element();
                };
                let factor = frame.folds.get(index).copied().unwrap_or(1.);
                let name = row.key(&frame.lists);
                let fold_id = SharedString::from(format!("grouped-fold-{}-{}", name.0, name.1));
                let folded = this.fold_row(fold_id, factor, window, cx, |this, window, cx| {
                    this.render_grouped_row(index, row, &frame.lists, window, cx)
                });
                let key = presence::row_key(&name);
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
                        this.render_grouped_row(*index, row, &frame.lists, window, cx)
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
            self.render_projects_header(lead, cx).into_any_element(),
            thread_list,
        )
    }

    /// The Flat layout's header and list.
    fn render_flat_layout(
        &mut self,
        lists: ThreadLists,
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
        } = self.thread_rows(&lists, scope.as_ref(), &Shelf::Recent);
        let mut visible: Vec<FlatListRow> = lists
            .iter()
            .filter(|lists| lists.down)
            .map(|lists| FlatListRow::Host(lists.host.clone()))
            .collect();
        for (section, rows) in [
            (ThreadSection::Pinned, &pinned),
            (ThreadSection::Active, &active),
        ] {
            visible.push(FlatListRow::Boundary(section, rows.is_empty()));
            for row in rows.iter() {
                visible.push(FlatListRow::Thread(Box::new(self.listed(*row))));
            }
        }
        let more = self.settled_more_count(&Shelf::Recent, settled_hidden_count);
        let (shelf, more_factor) =
            self.shelf_factors(&Shelf::Recent, &settled_visible, more.is_some());
        let mut folds = vec![1.; visible.len()];
        if pinned.is_empty() && active.is_empty() && settled_count == 0 {
            // An active project filter can empty the list while threads
            // exist; that state gets its own hint, not the no-projects one.
            let no_threads = lists.iter().all(|lists| lists.flat.is_empty());
            let hint = if !self.full_scope(cx) && no_threads {
                if self
                    .hosts
                    .iter()
                    .all(|host| host.store.read(cx).projects().is_empty())
                {
                    crate::tr!("member.empty_description")
                } else {
                    crate::tr!("member.threads_empty")
                }
            } else if no_threads {
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
            for (row, factor) in settled_visible.into_iter().zip(shelf) {
                visible.push(FlatListRow::Thread(Box::new(self.listed(row))));
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
        let pinned_row = |thread: &ThreadKey| {
            let index = visible
                .iter()
                .position(|row| matches!(row, FlatListRow::Thread(row) if row.key() == *thread))?;
            Some((
                index,
                pinned
                    .iter()
                    .any(|(host, meta)| **host == thread.host && meta.id == thread.session_id),
            ))
        };
        if let Some(id) = self.reveal_pinned.clone() {
            self.reveal_pinned_row(
                &self.flat_list_state.clone(),
                &self.flat_scroll.clone(),
                pinned_row(&id),
                cx,
            );
        }
        let project_names = lists
            .iter()
            .flat_map(|lists| {
                lists.groups.iter().map(|group| {
                    let project = ProjectKey {
                        host: lists.host.clone(),
                        project_id: group.project.id.clone(),
                    };
                    (project, crate::project_icon::label(&group.project))
                })
            })
            .collect::<HashMap<_, _>>();
        let frame = Rc::new(FlatFrame {
            rows: visible,
            folds,
            scope,
            settled_count,
            project_names,
            lists,
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
        let listed = match row {
            FlatListRow::Host(host) => {
                return div()
                    .w_full()
                    .px_2()
                    .pb(px(2.))
                    .child(self.render_host_heading(host, None, window, cx))
                    .into_any_element();
            }
            FlatListRow::Thread(listed) => listed,
            FlatListRow::Boundary(section, empty) => {
                return div()
                    .w_full()
                    .px_2()
                    .child(self.render_drag_boundary(*section, *empty, scope, window, cx))
                    .into_any_element();
            }
            FlatListRow::Settled => {
                let header = self.render_settled_header(
                    &Shelf::Recent,
                    frame.settled_count,
                    scope.as_ref(),
                    window,
                    cx,
                );
                return self
                    .drop_zone(div().w_full().child(header), DropZone::Settled, scope, cx)
                    .into_any_element();
            }
            FlatListRow::More(count) => {
                return self.render_settled_more(&Shelf::Recent, *count, cx);
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
        let project_name = listed
            .meta
            .project_id
            .as_ref()
            .and_then(|project_id| {
                frame.project_names.get(&ProjectKey {
                    host: listed.host.clone(),
                    project_id: project_id.clone(),
                })
            })
            .cloned();
        let Some(of) = frame.lists.iter().find(|lists| lists.host == listed.host) else {
            return div().into_any_element();
        };
        let host_name = self
            .store_of(&listed.host)
            .filter(|_| self.sectioned())
            .map(|store| store.read(cx).host_title());
        let thread =
            self.render_flat_thread(listed, &of.flags, project_name, host_name, window, cx);
        let row = div().w_full().px_2().pb(px(2.)).child(drag_ghost::slot(
            arrange::thread_slot(&listed.name),
            thread,
            window,
            cx,
        ));
        let zone = DropZone::Row {
            thread: listed.key(),
            section: tcode_core::thread_sort::thread_section(&listed.meta),
        };
        self.drop_zone(row, zone, scope, cx).into_any_element()
    }
}

impl Render for SessionsSidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let scope = (
            DevicePreferences::sidebar_layout(cx),
            self.project_filter.clone(),
        );
        if self.settled_scope.as_ref() != Some(&scope) {
            self.settled_scope = Some(scope);
            self.expanded_settled.clear();
            self.settled_limits.clear();
            self.compact_model_dirty = true;
        }
        let Some(lead) = self.hosts.first().map(|host| host.store.clone()) else {
            return div().into_any_element();
        };
        self.reveal_selected_settled(cx);
        self.sample_folds(window, cx);
        self.clear_finished_drag(cx);
        for host in &mut self.hosts {
            let spaces = spaces::for_store(&host.store, cx);
            host.spaces_observer.watch(spaces.as_ref(), cx);
        }
        if self.compact(cx) {
            return self.render_compact(window, cx);
        }

        let layout = DevicePreferences::sidebar_layout(cx);
        let thread_lists = self.thread_lists(cx);
        // Flat and Grouped are each their own list; switching cross-fades
        // them, the one left keeping its scroll while it fades. Until the
        // settings and threads load, the layout is only adopted.
        let current = usize::from(layout == SidebarLayout::Flat);
        let loading = self.threads_loading(cx);
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
                self.render_grouped_layout(&lead, thread_lists.clone(), window, cx);
            headers.push((0, header));
            lists.push((0, v_flex().size_full().child(list).into_any_element()));
        }
        if fade.shows(1) {
            let (header, list) = self.render_flat_layout(thread_lists, window, cx);
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
        .on_action(cx.listener(Self::on_stop_title))
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
        .on_action(cx.listener(Self::on_add_project))
        .on_action(cx.listener(Self::on_sort_projects))
        .on_action(cx.listener(Self::on_manage_hosts))
        .on_action(cx.listener(Self::on_host_settings))
        .on_action(cx.listener(Self::on_leave_host))
        .on_action(cx.listener(Self::on_toggle_share))
        .on_action(cx.listener(Self::on_new_space_and_share))
        .child(self.render_app_row(window, cx))
        .child(self.render_search_row(cx))
        .child(self.render_machine_control(&lead, window, cx))
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

    /// Thread `id` of the host in this process, which these sidebars list.
    fn local(id: impl Into<String>) -> ThreadKey {
        ThreadKey {
            host: HostKey::Local,
            session_id: id.into(),
        }
    }

    impl SessionsSidebar {
        /// The session ids [`SessionsSidebar::navigation_threads`] lists.
        fn navigation_ids(&mut self, cx: &mut Context<Self>) -> Vec<String> {
            self.navigation_threads(cx)
                .into_iter()
                .map(|thread| thread.session_id)
                .collect()
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
    fn a_phone_row_says_whether_its_thread_works_runs_background_work_or_waits(
        cx: &mut TestAppContext,
    ) {
        use tcode_protocol::{
            EventEnvelope, HostMessage, IndexSnapshot, IndexSummary, ServerEvent, SessionActivity,
            Topic, encode_line,
        };
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
        cx.update(crate::test_client::init);
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
        // The thread runs a turn, or only its background work goes on, or
        // nothing of it runs and a watched pull request will wake it.
        #[derive(Clone, Copy, PartialEq)]
        enum Doing {
            Turn,
            Background,
            Watch,
        }
        let summary = |doing: Doing| IndexSummary {
            activity: HashMap::from([(
                "background".into(),
                SessionActivity {
                    working: doing == Doing::Turn,
                    turn_running: doing == Doing::Turn,
                    background: tcode_protocol::BackgroundActivity {
                        tasks: (doing == Doing::Background).into(),
                        agents: 0,
                    },
                    unsettled: 0,
                    wakes: tcode_protocol::ThreadWakes {
                        pull_requests: if doing == Doing::Watch {
                            vec![tcode_core::pull_request::PullRequestKey::new(
                                "github.com",
                                "sample/project",
                                1048,
                            )]
                        } else {
                            Vec::new()
                        },
                        ..Default::default()
                    },
                    waiting_for_approval: false,
                    waiting_for_input: false,
                    settle_blocked: false,
                    failed: false,
                    unread: false,
                    fork: tcode_protocol::ForkAvailability::Available,
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
                summary: summary(Doing::Background),
            }),
        );
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump_link = link.clone();
        let _pump = cx.background_executor.spawn(async move {
            pump_link.pump().await;
        });
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        store.update(cx, |store, cx| store.show_alone("background".into(), cx));
        let window_state = cx.new(|_| WindowState::default().with_compact(true));
        let (sidebar, cx) = cx.add_window_view(|_, cx| {
            SessionsSidebar::new(vec![store.clone()], window_state.clone(), cx)
        });
        cx.simulate_resize(size(px(393.), px(852.)));
        for (doing, expected) in [
            (Doing::Background, "Background"),
            (Doing::Turn, "Working"),
            (Doing::Watch, "Waiting"),
        ] {
            send(
                Topic::Index,
                ServerEvent::IndexSummaryReplaced(summary(doing)),
            );
            cx.run_until_parked();
            cx.run_until_parked();
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
                        CompactListRow::Thread(row)
                            if row.state.thread.session_id == "background" =>
                        {
                            Some(row)
                        }
                        _ => None,
                    })
                    .expect("rendered phone thread");
                let (label, _) = compact_status_line(&row.state, row.working, cx).unwrap();
                assert_eq!(label, expected);
            });
        }
    }

    /// A host of its own holding `threads` (id and last activity) in one
    /// project at a fresh root, and a store over it as `attachment` names it.
    struct TestHost {
        store: Entity<WorkspaceStore>,
        driver: crate::store::tests::LocalHostDriver,
        host: tcode_runtime::pipe::SpawnedHost,
        root: PathBuf,
    }

    impl TestHost {
        fn spawn(
            cx: &mut TestAppContext,
            name: &str,
            attachment: crate::store::WorkspaceAttachment,
            threads: &[(&str, u64)],
        ) -> Self {
            let root = crate::store::tests::scratch_root(name);
            let session_store = SessionStore::open_at(root.clone()).unwrap();
            let project = Project::from_root(root.clone());
            let sessions: Vec<SessionMeta> = threads
                .iter()
                .map(|(id, at)| {
                    let mut meta = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
                    meta.project_id = Some(project.id.clone());
                    meta.id = (*id).into();
                    meta.title = (*id).into();
                    meta.created_at = *at;
                    meta.updated_at = *at;
                    meta
                })
                .collect();
            session_store.upsert_project(&project).unwrap();
            for meta in &sessions {
                session_store.upsert_meta(meta).unwrap();
            }
            let host = spawn_host(session_store, HostServices::default())
                .expect("spawn sidebar test host");
            let mut driver = crate::store::tests::LocalHostDriver::new(&host);
            let store = cx.new(|cx| {
                WorkspaceStore::new_attached(driver.link.clone(), attachment, None, None, cx)
            });
            driver.wait_until(cx, "sidebar baseline", |cx| {
                store.read_with(cx, |store, _| store.baseline_ready())
            });
            Self {
                store,
                driver,
                host,
                root,
            }
        }

        /// A machine of its own, `host_id`, to the client.
        fn remote(cx: &mut TestAppContext, host_id: &str, threads: &[(&str, u64)]) -> Self {
            let attachment = crate::store::WorkspaceAttachment::Remote {
                host_id: host_id.into(),
                host_name: format!("{host_id} name"),
            };
            Self::spawn(cx, &format!("sidebar-{host_id}"), attachment, threads)
        }

        /// Session ids in the order the host lists them.
        fn listed(&self, cx: &mut TestAppContext) -> Vec<String> {
            self.store.read_with(cx, |store, _| {
                store
                    .flat_sessions()
                    .iter()
                    .map(|meta| meta.id.clone())
                    .collect()
            })
        }

        fn shutdown(mut self, cx: &mut TestAppContext) {
            self.driver
                .link
                .set_connection_state(tcode_client::ConnectionState::Connected { path: None });
            self.driver.shutdown(cx, &self.host);
            let _ = std::fs::remove_dir_all(self.root);
        }
    }

    /// A desktop sidebar over two machines, `host-a` holding `a` and
    /// `host-b` holding `b`, drawn in a window of `height` in `layout`.
    fn sidebar_over_two_hosts<'a>(
        cx: &'a mut TestAppContext,
        layout: SidebarLayout,
        a: &[(&str, u64)],
        b: &[(&str, u64)],
        height: f32,
    ) -> (
        Entity<SessionsSidebar>,
        Entity<WindowState>,
        TestHost,
        TestHost,
        &'a mut VisualTestContext,
    ) {
        cx.update(crate::test_client::init);
        cx.update(|cx| DevicePreferences::set_sidebar_layout(layout, cx));
        let (a, b) = (
            TestHost::remote(cx, "host-a", a),
            TestHost::remote(cx, "host-b", b),
        );
        let window_state = cx.new(|_| WindowState::default());
        let stores = vec![a.store.clone(), b.store.clone()];
        let sidebar = cx.new(|cx| SessionsSidebar::new(stores, window_state.clone(), cx));
        let (_, cx) =
            cx.add_window_view(|window, cx| gpui_base::Root::new(sidebar.clone(), window, cx));
        cx.simulate_resize(size(px(320.), px(height)));
        crate::test_frames::draw_settled(cx);
        (sidebar, window_state, a, b, cx)
    }

    #[gpui::test]
    fn canceling_inline_rename_on_the_phone_discards_the_unsaved_title(cx: &mut TestAppContext) {
        cx.update(crate::test_client::init);
        cx.update(crate::window_seam::force_mobile_layout);
        let root = crate::store::tests::scratch_root("sidebar-rename-test");
        let session_store = SessionStore::open_at(root.clone()).unwrap();
        let project = Project::from_root(root.clone());
        let mut meta = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
        meta.project_id = Some(project.id.clone());
        meta.title = "Original title".into();
        meta.id = "rename-thread".into();
        session_store.upsert_project(&project).unwrap();
        session_store.upsert_meta(&meta).unwrap();
        let host =
            spawn_host(session_store, HostServices::default()).expect("spawn sidebar test host");
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                cx,
            )
        });
        driver.wait_until(cx, "sidebar baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });

        let window_state = cx.new(|_| WindowState::default());
        let sidebar =
            cx.new(|cx| SessionsSidebar::new(vec![store.clone()], window_state.clone(), cx));
        let (_, cx) =
            cx.add_window_view(|window, cx| gpui_base::Root::new(sidebar.clone(), window, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(360.), px(800.)));
        window_state.update(cx, |state, cx| {
            state.set_compact(true, cx);
            state.go(crate::window_state::Destination::Threads, cx);
        });
        draw(cx);
        let row = cx.debug_bounds("compact-row-rename-thread").unwrap();
        let trigger = row.center();
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
        let rename = cx
            .debug_bounds("menu-item-2")
            .expect("thread menu opens")
            .center();
        touch(cx, gpui::TouchPhase::Started, rename);
        touch(cx, gpui::TouchPhase::Ended, rename);
        crate::test_frames::draw_settled(cx);
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
        cx.update(|window, cx| {
            sidebar.update(cx, |sidebar, cx| {
                let input = sidebar
                    .renaming
                    .as_ref()
                    .expect("Rename starts editing")
                    .input
                    .clone();
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
        driver.round_trip(cx);
        assert_eq!(
            store.read_with(cx, |store, _| store
                .thread_meta("rename-thread")
                .map(|meta| meta.title.clone())),
            Some("Original title".into()),
            "the host keeps the title the rename never saved"
        );

        driver.shutdown(cx, &host);
        let _ = std::fs::remove_dir_all(root);
    }

    #[gpui::test]
    fn thread_rows_carry_the_provider_mark_until_the_setting_is_off(cx: &mut TestAppContext) {
        cx.update(crate::test_client::init);
        let root = crate::store::tests::scratch_root("provider-mark");
        let disk = SessionStore::open_at(root.clone()).unwrap();
        let project = Project::from_root(root.clone());
        let mut codex = session("codex-thread", None);
        codex.project_id = Some(project.id.clone());
        let mut claude = session("claude-thread", None);
        claude.provider = ProviderKind::ClaudeCode;
        claude.project_id = Some(project.id.clone());
        disk.upsert_project(&project).unwrap();
        for meta in [&codex, &claude] {
            disk.upsert_meta(meta).unwrap();
        }
        let host = spawn_host(disk, HostServices::default()).unwrap();
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                cx,
            )
        });
        driver.wait_until(cx, "sidebar baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });
        let window_state = cx.new(|_| WindowState::default());
        let (_, cx) =
            cx.add_window_view(|_, cx| SessionsSidebar::new(vec![store.clone()], window_state, cx));
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
            |cx: &mut VisualTestContext, driver: &mut crate::store::tests::LocalHostDriver| loop {
                driver.step(cx);
                draw(cx);
                let rows: Vec<_> = selectors
                    .iter()
                    .filter_map(|(row, _)| cx.debug_bounds(row))
                    .collect();
                if rows.len() == selectors.len() {
                    return rows;
                }
                driver.wait_for_host("the thread rows");
            };

        let set_marks =
            |cx: &mut VisualTestContext,
             enabled: bool,
             driver: &mut crate::store::tests::LocalHostDriver| {
                store.update(cx, |store, _| store.set_sidebar_provider_marks(enabled));
                driver.wait_until(cx, "the setting to replicate", |cx| {
                    store.read_with(cx, |store, _| store.settings().sidebar_provider_marks)
                        == enabled
                });
            };

        // Off by default: rows render without a mark.
        wait_for_rows(cx, &mut driver);
        for (_, mark_selector) in selectors {
            assert!(cx.debug_bounds(mark_selector).is_none());
        }

        set_marks(cx, true, &mut driver);
        wait_for_rows(cx, &mut driver);
        for (_, mark_selector) in selectors {
            assert!(
                cx.debug_bounds(mark_selector).is_some(),
                "each row carries its provider mark once enabled"
            );
        }

        set_marks(cx, false, &mut driver);
        wait_for_rows(cx, &mut driver);
        for (_, mark_selector) in selectors {
            assert!(
                cx.debug_bounds(mark_selector).is_none(),
                "with marks off again no mark is drawn"
            );
        }
        driver.shutdown(cx, &host);
        let _ = std::fs::remove_dir_all(root);
    }

    #[gpui::test]
    fn dismissing_a_phone_rows_menu_keeps_the_selection_and_the_next_tap_opens(
        cx: &mut TestAppContext,
    ) {
        use gpui::{PlatformInput, TouchEvent, TouchId, TouchPhase};
        cx.update(crate::test_client::init);
        let root = crate::store::tests::scratch_root("menu-selection");
        let disk = SessionStore::open_at(root.clone()).unwrap();
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
        disk.upsert_project(&project).unwrap();
        for meta in &sessions {
            disk.upsert_meta(meta).unwrap();
        }
        let host = spawn_host(disk, HostServices::default()).unwrap();
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                cx,
            )
        });
        crate::store::tests::place_shown_threads(&store, cx);
        driver.wait_until(cx, "sidebar baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });
        let navigation = cx.new(|_| WindowState::default().with_compact(true));
        let (_, cx) = cx.add_window_view(|_, cx| {
            SessionsSidebar::new(vec![store.clone()], navigation.clone(), cx)
        });
        cx.simulate_resize(size(px(393.), px(852.)));
        draw(cx);
        // A remote host may not have sent status yet. Selection must repaint
        // from the client notification without waiting for a host event.
        driver.shutdown(cx, &host);
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
        crate::test_frames::draw_settled(cx);
        assert!(cx.debug_bounds("tcode-popup-menu").is_none());
        assert_eq!(
            store.read_with(cx, |store, _| store.open_threads()),
            selected
        );
        assert_eq!(
            navigation.read_with(cx, |state, _| state.destination()),
            destination
        );
        send(cx, 3, TouchPhase::Started, b.center());
        send(cx, 3, TouchPhase::Ended, b.center());
        crate::test_frames::draw_settled(cx);
        assert_eq!(
            store.read_with(cx, |store, _| store.open_threads()),
            ["menu-10"]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    fn compact_list_has_project_headers(
        cx: &mut TestAppContext,
        projects: usize,
        quick_chat: bool,
    ) -> bool {
        use tcode_client::host::ClientHost as _;
        cx.update(crate::test_client::init);
        let root = crate::store::tests::scratch_root(&format!("sidebar-{projects}-project"));
        let device = || tcode_traverse::NativeClientHost::new(root.join("device"), "phone");
        let client: std::rc::Rc<dyn tcode_client::host::ClientHost> = std::rc::Rc::new(device());
        cx.update(|cx| DevicePreferences::install(client, cx));
        // The host's quick threads work in its documents folder; a project
        // there is the quick-thread project.
        let user_directories = tcode_services::user_files::UserDirectories {
            home: None,
            documents: Some(root.join("documents")),
        };
        let scratch = user_directories.scratch_root().unwrap();
        let seeded: Vec<Project> = (0..projects)
            .map(|index| {
                Project::from_root(if quick_chat && index == 0 {
                    scratch.clone()
                } else {
                    root.join(format!("project-{index}"))
                })
            })
            .collect();
        let disk = SessionStore::open_at(root.clone()).unwrap();
        for (index, project) in seeded.iter().enumerate() {
            let mut meta = session(&format!("thread-{index}"), None);
            meta.project_id = Some(project.id.clone());
            meta.updated_at = 100 + index as u64;
            meta.created_at = meta.updated_at;
            disk.upsert_project(project).unwrap();
            disk.upsert_meta(&meta).unwrap();
        }
        let host = spawn_host(
            disk,
            HostServices {
                user_directories,
                ..HostServices::default()
            },
        )
        .expect("spawn sidebar test host");

        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                cx,
            )
        });
        driver.wait_until(cx, "sidebar baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });
        let window_state = cx.new(|_| WindowState::default().with_compact(true));
        let (sidebar, cx) =
            cx.add_window_view(|_, cx| SessionsSidebar::new(vec![store.clone()], window_state, cx));
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
            cx.update(|_, cx| DevicePreferences::sidebar_layout(cx)),
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
        cx.simulate_click(toggle.center(), gpui::Modifiers::default());
        draw(cx);
        assert_eq!(
            device().load_preferences().unwrap().sidebar_layout,
            Some(SidebarLayout::Grouped),
            "the actual header action persists this device's choice"
        );
        // The list the new layout brings folds its rows in; the reader
        // presses a header once it is there.
        crate::test_frames::draw_settled(cx);
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
                driver.wait_until(cx, "the project toggle to reach the replica", |cx| {
                    store.read_with(cx, |store, _| {
                        seeded
                            .iter()
                            .any(|project| store.is_project_collapsed(&project.id))
                    }) == collapsed
                });
                crate::test_frames::draw_settled(cx);
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

        driver.shutdown(cx, &host);
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
        cx.update(crate::test_client::init);
        cx.update(|cx| DevicePreferences::set_sidebar_layout(SidebarLayout::Grouped, cx));
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
                ServerEvent::SettingsSnapshot(Default::default()),
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
        let _pump = cx.background_executor.spawn(async move {
            pump_link.pump().await;
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
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        let window_state = cx.new(|_| WindowState::default());
        let (sidebar, cx) =
            cx.add_window_view(|_, cx| SessionsSidebar::new(vec![store.clone()], window_state, cx));
        cx.simulate_resize(size(px(320.), px(900.)));

        // The machine's spaces: one, holding the projects it was last given.
        let mut shared: Vec<String> = Vec::new();
        let mut asked: Vec<Vec<String>> = Vec::new();
        let mut serve = |cx: &mut VisualTestContext| {
            for _ in 0..4 {
                cx.run_until_parked();
                cx.run_until_parked();
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
                            policy: tcode_protocol::SpacePolicy::unrestricted(),
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
    fn the_grouped_list_keeps_the_readers_place_as_threads_change(cx: &mut TestAppContext) {
        use tcode_protocol::{
            EventEnvelope, HostMessage, IndexSnapshot, ServerEvent, Topic, encode_line,
        };
        cx.update(crate::test_client::init);
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
        let mut sessions = (0..60)
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
        cx.update(|cx| DevicePreferences::set_sidebar_layout(SidebarLayout::Grouped, cx));
        send(
            Topic::Settings,
            ServerEvent::SettingsSnapshot(Default::default()),
        );
        send(Topic::Index, snapshot(&sessions));
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump_link = link.clone();
        let _pump = cx.background_executor.spawn(async move {
            pump_link.pump().await;
        });
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        store.update(cx, |store, cx| store.show_alone("virtual-0".into(), cx));
        let window_state = cx.new(|_| WindowState::default());
        let (sidebar, cx) =
            cx.add_window_view(|_, cx| SessionsSidebar::new(vec![store.clone()], window_state, cx));
        cx.run_until_parked();
        cx.run_until_parked();
        cx.simulate_resize(size(px(300.), px(800.)));
        sidebar.update(cx, |_, cx| {
            cx.notify();
        });
        draw(cx);
        let more = cx.debug_bounds("show-more-project").unwrap();
        cx.simulate_click(more.center(), gpui::Modifiers::default());
        crate::test_frames::draw_settled(cx);

        let far_row = |sidebar: &SessionsSidebar| {
            sidebar
                .grouped_row_keys
                .iter()
                .position(|(_, id)| id == "virtual-30")
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
        assert!(cx.debug_bounds("sidebar-thread-virtual-30").is_some());

        let mut newer = session("virtual-new", None);
        newer.project_id = Some(project.id.clone());
        newer.updated_at = 2_000;
        newer.created_at = 2_000;
        sessions.insert(0, newer);
        let apply = |sessions: &Vec<SessionMeta>, cx: &mut VisualTestContext| {
            send(Topic::Index, snapshot(sessions));
            cx.run_until_parked();
            cx.run_until_parked();
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
        assert!(cx.debug_bounds("sidebar-thread-virtual-30").is_some());

        let top = sidebar.read_with(cx, |sidebar, _| far_row(sidebar));
        sessions
            .iter_mut()
            .find(|meta| meta.id == "virtual-30")
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
        assert!(cx.debug_bounds("sidebar-thread-virtual-31").is_some());

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
        cx.update(crate::test_client::init);
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
        let _pump = cx.background_executor.spawn(async move {
            pump_link.pump().await;
        });
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        // A selection keeps the store from opening a draft against a host
        // this fake transport never answers.
        store.update(cx, |store, cx| store.show_alone("other".into(), cx));
        let window_state = cx.new(|_| WindowState::default().with_compact(true));
        let (_, cx) = cx.add_window_view(|_, cx| {
            SessionsSidebar::new(vec![store.clone()], window_state.clone(), cx)
        });
        cx.simulate_resize(size(px(393.), px(852.)));
        cx.run_until_parked();
        cx.run_until_parked();
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
        cx.update(crate::test_client::init);
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
        let _pump = cx.background_executor.spawn(async move {
            pump_link.pump().await;
        });
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                cx,
            )
        });
        crate::shell::tests::shell_behind(&store, cx);
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        store.update(cx, |store, cx| store.show_thread("settled-39".into(), cx));
        let window_state = cx.new(|_| WindowState::default());
        let (sidebar, cx) = cx.add_window_view(|_, cx| {
            SessionsSidebar::new(vec![store.clone()], window_state.clone(), cx)
        });
        cx.simulate_resize(size(px(393.), px(6000.)));
        for compact in [false, true] {
            window_state.update(cx, |state, cx| {
                state.compact = compact;
                cx.notify();
            });
            cx.run_until_parked();
            cx.run_until_parked();
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
                let ids = sidebar.update(cx, |sidebar, cx| sidebar.navigation_ids(cx));
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
                    sidebar.project_filter = Some(ProjectKey {
                        host: HostKey::Local,
                        project_id: "missing-project".into(),
                    });
                    cx.notify();
                });
                draw(cx);
                assert!(
                    sidebar
                        .update(cx, |sidebar, cx| sidebar.navigation_ids(cx))
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
        cx.update(crate::test_client::init);
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
        let _pump = cx.background_executor.spawn(async move {
            pump_link.pump().await;
        });
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                cx,
            )
        });
        crate::store::tests::place_shown_threads(&store, cx);
        crate::shell::tests::shell_behind(&store, cx);
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        // The open thread is settled, so the closed shelf shows its row.
        store.update(cx, |store, cx| store.show_thread("settled-20".into(), cx));
        let window_state = cx.new(|_| WindowState::default());
        let (sidebar, cx) = cx.add_window_view(|_, cx| {
            SessionsSidebar::new(vec![store.clone()], window_state.clone(), cx)
        });
        cx.run_until_parked();
        cx.run_until_parked();
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
            sidebar.update(cx, |sidebar, cx| sidebar.navigation_ids(cx))
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
        cx.run_until_parked();
        draw(cx);
        assert_eq!(
            sidebar.update(cx, |sidebar, cx| sidebar.navigation_ids(cx))[0],
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
        cx.update(crate::test_client::init);
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
            ServerEvent::SettingsSnapshot(Default::default()),
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
        let _pump = cx.background_executor.spawn(async move {
            pump_link.pump().await;
        });
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                cx,
            )
        });
        crate::shell::tests::shell_behind(&store, cx);
        // Chosen on this device once its shell has read its preferences.
        cx.update(|cx| DevicePreferences::set_sidebar_layout(SidebarLayout::Grouped, cx));
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        store.update(cx, |store, cx| store.show_thread("mixed-a".into(), cx));
        let window_state = cx.new(|_| WindowState::default());
        let (sidebar, cx) = cx.add_window_view(|_, cx| {
            SessionsSidebar::new(vec![store.clone()], window_state.clone(), cx)
        });
        cx.run_until_parked();
        cx.run_until_parked();
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
            crate::test_frames::draw_settled(cx);
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
            crate::test_frames::draw_settled(cx);
            let shelves = sidebar.update(cx, |sidebar, cx| {
                if compact {
                    sidebar
                        .compact_model(None, cx)
                        .rows
                        .iter()
                        .filter_map(|row| match row {
                            CompactListRow::Settled { key, count, .. } => {
                                Some((key.clone(), *count))
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                } else {
                    let lists = sidebar.thread_lists(cx);
                    sidebar
                        .grouped_rows(&lists, cx)
                        .into_iter()
                        .filter_map(|(row, _)| match row {
                            GroupedListRow::Settled { host, group, count } => {
                                Some((lists[host].groups[group].project.id.clone(), count))
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
                    sidebar.update(cx, |sidebar, cx| sidebar.navigation_ids(cx)),
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
            crate::test_frames::draw_settled(cx);
            verify(cx, expected(shown, false, true));

            send(
                Topic::Settings,
                ServerEvent::SettingsSnapshot(tcode_core::settings::Settings {
                    collapsed_projects: vec!["mixed".into()],
                    ..Default::default()
                }),
            );
            cx.run_until_parked();
            cx.run_until_parked();
            crate::test_frames::draw_settled(cx);
            assert_eq!(
                sidebar.update(cx, |sidebar, cx| sidebar.navigation_ids(cx)),
                lead(false)
                    .into_iter()
                    .chain(many(8, true))
                    .collect::<Vec<_>>(),
                "a collapsed project lists nothing; compact={compact}"
            );
            send(
                Topic::Settings,
                ServerEvent::SettingsSnapshot(Default::default()),
            );
            cx.run_until_parked();
            cx.run_until_parked();
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
        cx.update(crate::test_client::init);
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
        driver.wait_until(cx, "the threads", |cx| {
            store.read_with(cx, |store, _| !store.flat_sessions().is_empty())
        });
        let window_state = cx.new(|_| WindowState::default());
        let mut sidebar = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view =
                cx.new(|cx| SessionsSidebar::new(vec![store.clone()], window_state.clone(), cx));
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
                sidebar.on_pin(&ThreadPin(local(id)), window, cx)
            });
            driver.wait_until(cx, "pin command reaches the host", |_| {
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
            driver.wait_until(cx, "the pin", |cx| {
                store.read_with(cx, |store, _| {
                    store
                        .thread_meta(id)
                        .is_some_and(|meta| meta.pinned_at.is_some())
                })
            });
            draw(cx);

            let ids = sidebar.update(cx, |sidebar, cx| sidebar.navigation_ids(cx));
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
        driver.wait_until(cx, "the new thread", |cx| {
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
        cx.update(crate::test_client::init);
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
        driver.wait_until(cx, "the threads", |cx| {
            store.read_with(cx, |store, _| !store.flat_sessions().is_empty())
        });
        let window_state = cx.new(|_| WindowState::default());
        let mut sidebar = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view =
                cx.new(|cx| SessionsSidebar::new(vec![store.clone()], window_state.clone(), cx));
            sidebar = Some(view.clone());
            gpui_base::Root::new(view, window, cx)
        });
        let sidebar = sidebar.unwrap();
        cx.simulate_resize(size(px(393.), px(2000.)));
        // The lifecycle commands accepted since the last call.
        let sent = |cx: &mut VisualTestContext,
                    driver: &mut crate::store::tests::LocalHostDriver,
                    expected: usize| {
            driver.wait_until(cx, "lifecycle commands reach the host", |_| {
                recorded.lock().unwrap().len() >= expected
            });
            // A FIFO reply also establishes absence after a cancelled gesture.
            let link = driver.link.clone();
            let mut barrier = std::pin::pin!(link.query(tcode_protocol::Query::Ping));
            driver.wait_until(cx, "lifecycle command barrier", |_| {
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
            let ids = sidebar.update(cx, |sidebar, cx| sidebar.navigation_ids(cx));
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
            sidebar.on_unpin(&ThreadUnpin(local("pin-a")), window, cx)
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
            sidebar.on_move(&ThreadMove(local("pin-a"), true), window, cx)
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
            sidebar.settle_thread(&local("pin-b"), window, cx)
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

    /// Over two machines the project view lists each under its heading,
    /// host › project › thread. A thread id both hosts use is a row under
    /// each, and choosing one shows it on its own host; a heading shuts its
    /// own host's section alone, on the desktop and the phone alike.
    #[gpui::test]
    fn two_hosts_list_their_projects_and_threads_under_their_headings(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
        let (_, window_state, a, b, cx) = sidebar_over_two_hosts(
            cx,
            SidebarLayout::Grouped,
            &[("shared", 300), ("a-only", 200)],
            &[("shared", 250), ("b-only", 100)],
            800.,
        );
        let project = |host: &TestHost, cx: &mut VisualTestContext| {
            host.store
                .read_with(cx, |store, _| store.projects()[0].id.clone())
        };
        let (project_a, project_b) = (project(&a, cx), project(&b, cx));
        let drawn = |selector: &str, cx: &mut VisualTestContext| {
            cx.debug_bounds(selector.to_owned().leak())
        };
        let order = [
            "host-heading-host-a".to_owned(),
            format!("project-header-host-a/{project_a}"),
            "sidebar-thread-host-a/shared".into(),
            "sidebar-thread-host-a/a-only".into(),
            "host-heading-host-b".into(),
            format!("project-header-host-b/{project_b}"),
            "sidebar-thread-host-b/shared".into(),
            "sidebar-thread-host-b/b-only".into(),
        ];
        let tops: Vec<_> = order
            .iter()
            .map(|selector| {
                drawn(selector, cx)
                    .unwrap_or_else(|| panic!("{selector} is drawn"))
                    .top()
            })
            .collect();
        assert!(
            tops.windows(2).all(|pair| pair[0] < pair[1]),
            "{order:?} lie in that order: {tops:?}"
        );

        let shown = Rc::new(std::cell::RefCell::new(Vec::new()));
        for (host, store) in [("host-a", &a.store), ("host-b", &b.store)] {
            let shown = shown.clone();
            cx.update(|_, cx| {
                cx.subscribe(store, move |_, event: &crate::store::ShowThread, _| {
                    shown.borrow_mut().push((host, event.session_id.clone()));
                })
                .detach()
            });
        }
        let row = drawn("sidebar-thread-host-b/shared", cx).unwrap();
        cx.simulate_click(row.center(), gpui::Modifiers::default());
        draw(cx);
        assert_eq!(*shown.borrow(), [("host-b", "shared".to_owned())]);

        let heading = drawn("host-heading-host-a", cx).unwrap();
        cx.simulate_click(heading.center(), gpui::Modifiers::default());
        crate::test_frames::draw_settled(cx);
        for selector in &order[1..4] {
            assert!(drawn(selector, cx).is_none(), "{selector} folds away");
        }
        for selector in order.iter().take(1).chain(&order[4..]) {
            assert!(drawn(selector, cx).is_some(), "{selector} stays");
        }

        // The phone's list has the same sections, and the same one is shut.
        window_state.update(cx, |state, cx| state.set_compact(true, cx));
        crate::test_frames::draw_settled(cx);
        let compact = [
            "compact-host-host-a",
            "compact-host-host-b",
            "compact-row-host-b/shared",
            "compact-row-host-b/b-only",
        ];
        let tops: Vec<_> = compact
            .iter()
            .map(|selector| {
                drawn(selector, cx)
                    .unwrap_or_else(|| panic!("{selector} is drawn"))
                    .top()
            })
            .collect();
        assert!(
            tops.windows(2).all(|pair| pair[0] < pair[1]),
            "{compact:?} lie in that order: {tops:?}"
        );
        assert!(drawn("compact-row-host-a/shared", cx).is_none());
        let heading = drawn("compact-host-host-a", cx).unwrap();
        cx.simulate_click(heading.center(), gpui::Modifiers::default());
        crate::test_frames::draw_settled(cx);
        let opened = drawn("compact-row-host-a/shared", cx).expect("host A's section opens");
        assert!(
            heading.top() < opened.top() && opened.top() < drawn(compact[1], cx).unwrap().top()
        );

        a.shutdown(cx);
        b.shutdown(cx);
    }

    /// Over two machines the threads view lists their threads by time, each
    /// row naming its host. A thread carried over another host's row lands
    /// among its own host's rows, after those above that row; only its host
    /// is asked to move it, and the list keeps each host's order after.
    #[gpui::test]
    fn a_thread_carried_over_another_hosts_row_lands_among_its_own(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
        let (_, _, mut a, mut b, cx) = sidebar_over_two_hosts(
            cx,
            SidebarLayout::Flat,
            &[("a1", 300), ("a2", 250), ("a3", 100)],
            &[("b1", 280), ("b2", 200)],
            800.,
        );
        let row = |id: &str, cx: &mut VisualTestContext| {
            cx.debug_bounds(format!("sidebar-thread-{id}").leak())
                .unwrap_or_else(|| panic!("{id} is drawn"))
        };
        let drawn = |cx: &mut VisualTestContext| {
            let mut rows: Vec<_> = [
                "host-a/a1",
                "host-a/a2",
                "host-a/a3",
                "host-b/b1",
                "host-b/b2",
            ]
            .into_iter()
            .map(|id| (row(id, cx).top(), id))
            .collect();
            rows.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
            rows.into_iter().map(|(_, id)| id).collect::<Vec<_>>()
        };
        assert_eq!(
            drawn(cx),
            [
                "host-a/a1",
                "host-b/b1",
                "host-a/a2",
                "host-b/b2",
                "host-a/a3"
            ]
        );
        assert!(
            cx.debug_bounds("flat-host-host-b/b1").is_some(),
            "a row names its host"
        );

        let (left, none) = (gpui::MouseButton::Left, gpui::Modifiers::default());
        let frame = |cx: &mut VisualTestContext| {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(16));
            cx.update(|window, cx| window.simulate_next_frame(cx));
            draw(cx);
        };
        let (from, over) = (row("host-a/a3", cx), row("host-b/b1", cx));
        cx.simulate_mouse_down(from.center(), left, none);
        cx.simulate_mouse_move(from.center() + gpui::point(px(0.), px(8.)), left, none);
        draw(cx);
        let to = gpui::point(over.center().x, over.top() + px(4.));
        cx.simulate_mouse_move(to, left, none);
        for _ in 0..20 {
            frame(cx);
        }
        cx.simulate_mouse_up(to, left, none);
        draw(cx);
        let store = a.store.clone();
        a.driver.wait_until(cx, "the move", |cx| {
            store.read_with(cx, |store, _| {
                store
                    .flat_sessions()
                    .iter()
                    .map(|meta| meta.id.as_str())
                    .eq(["a1", "a3", "a2"])
            })
        });
        b.driver.step(cx);
        assert_eq!(b.listed(cx), ["b1", "b2"]);
        assert!(
            b.store.read_with(cx, |store, _| store
                .flat_sessions()
                .iter()
                .all(|meta| meta.active_order.is_none() && meta.pinned_at.is_none())),
            "the other host is not asked to move anything"
        );
        crate::test_frames::draw_settled(cx);
        assert_eq!(
            drawn(cx),
            [
                "host-a/a1",
                "host-b/b1",
                "host-b/b2",
                "host-a/a3",
                "host-a/a2"
            ],
            "host A's arranged order holds among host B's rows by time"
        );

        a.shutdown(cx);
        b.shutdown(cx);
    }

    /// One host's link going down leaves the other host's rows alone: their
    /// keys and where the list is scrolled stay. The host that went shows
    /// its heading and how its link is in place of its threads, and Pair
    /// again once it refused this device.
    #[gpui::test]
    fn a_host_going_down_leaves_the_other_hosts_rows_and_scroll(cx: &mut TestAppContext) {
        use tcode_client::{ConnectionFailure, ConnectionState};
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
        cx.update(|cx| cx.set_reduce_motion(true));
        let a: Vec<(String, u64)> = (0..30)
            .map(|index| (format!("a-{index:02}"), 1_000 - 2 * index))
            .collect();
        let b: Vec<(String, u64)> = (0..5)
            .map(|index| (format!("b-{index}"), 999 - 2 * index))
            .collect();
        fn refs(threads: &[(String, u64)]) -> Vec<(&str, u64)> {
            threads.iter().map(|(id, at)| (id.as_str(), *at)).collect()
        }
        let (sidebar, window_state, a, b, cx) =
            sidebar_over_two_hosts(cx, SidebarLayout::Flat, &refs(&a), &refs(&b), 400.);
        let list = sidebar.read_with(cx, |sidebar, _| sidebar.flat_list_state.clone());
        let keys = |cx: &mut VisualTestContext| {
            sidebar.read_with(cx, |sidebar, _| sidebar.flat_row_keys.clone())
        };
        let scroll_to = |item_ix, cx: &mut VisualTestContext| {
            list.scroll_to(gpui::ListOffset {
                item_ix,
                offset_in_item: px(10.),
            });
            sidebar.update(cx, |_, cx| cx.notify());
            draw(cx);
        };
        fn host_a(keys: &[(&'static str, String)]) -> Vec<(&'static str, String)> {
            keys.iter()
                .filter(|(_, id)| id.starts_with("host-a/"))
                .cloned()
                .collect()
        }
        for (state, refused) in [
            (
                ConnectionState::Reconnecting {
                    attempt: 1,
                    reason: Some(ConnectionFailure::Unreachable(None)),
                },
                false,
            ),
            (
                ConnectionState::Offline {
                    reason: ConnectionFailure::AuthenticationRejected,
                },
                true,
            ),
        ] {
            let before = keys(cx);
            let anchor = before
                .iter()
                .position(|key| *key == ("thread", "host-a/a-20".to_owned()))
                .expect("a-20 is listed");
            scroll_to(anchor, cx);
            let offset = list.logical_scroll_top();
            let top = cx
                .debug_bounds("sidebar-thread-host-a/a-20")
                .expect("a-20 is drawn")
                .top();

            b.driver.link.set_connection_state(state.clone());
            cx.run_until_parked();
            draw(cx);
            let after = keys(cx);
            assert_eq!(host_a(&after), host_a(&before), "{state:?}");
            assert_eq!(
                after[list.logical_scroll_top().item_ix],
                ("thread", "host-a/a-20".to_owned()),
                "{state:?}"
            );
            assert_eq!(
                list.logical_scroll_top().offset_in_item,
                offset.offset_in_item
            );
            assert_eq!(
                cx.debug_bounds("sidebar-thread-host-a/a-20")
                    .map(|row| row.top()),
                Some(top),
                "{state:?}"
            );
            assert!(
                !after.iter().any(|(_, id)| id.starts_with("host-b/")),
                "{state:?} lists none of its threads"
            );
            assert_eq!(after[0], ("host", "host-b".to_owned()));

            list.scroll_to(gpui::ListOffset::default());
            sidebar.update(cx, |_, cx| cx.notify());
            draw(cx);
            assert!(cx.debug_bounds("host-heading-host-b").is_some());
            assert!(cx.debug_bounds("host-status-host-b").is_some());
            assert_eq!(
                cx.debug_bounds("host-pair-again-host-b").is_some(),
                refused,
                "{state:?}"
            );
        }
        let pair = cx.debug_bounds("host-pair-again-host-b").unwrap();
        cx.simulate_click(pair.center(), gpui::Modifiers::default());
        draw(cx);
        assert_eq!(
            window_state.read_with(cx, |state, _| state.destination()),
            Destination::Pair
        );

        a.shutdown(cx);
        b.shutdown(cx);
    }
}
