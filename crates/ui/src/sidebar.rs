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

mod actions;
mod arrange;
mod compact;
mod headers;
mod layout;
mod lists;
mod presence;
mod settled;
mod swipe;
mod thread_row;
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
/// The threads view's threads: `project`'s, or without one every thread
/// outside the `hidden` projects.
fn project_threads<'a>(
    sessions: &'a [SessionMeta],
    project: Option<&str>,
    hidden: &HashSet<String>,
) -> ThreadSections<&'a SessionMeta> {
    partition_threads(sessions.iter().filter(|meta| {
        let of = meta.project_id.as_deref();
        match project {
            Some(id) => of == Some(id),
            None => of.is_none_or(|id| !hidden.contains(id)),
        }
    }))
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
#[action(namespace = tcode_thread, no_json)]
struct ThreadStopWatches(ThreadKey);

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
    /// The host's projects a space hides from the threads view, when this
    /// client manages its spaces; the project view still lists them.
    hidden: Rc<HashSet<String>>,
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
        // Read before the compact list so a change of spaces rebuilds it.
        let thread_lists = self.thread_lists(cx);
        if self.compact(cx) {
            return self.render_compact(window, cx);
        }

        let layout = DevicePreferences::sidebar_layout(cx);
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
        .on_action(cx.listener(Self::on_stop_watches))
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
mod tests;
