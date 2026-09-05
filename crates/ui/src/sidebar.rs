use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
};

use crate::overlay::{DialogButtons, Notification, OverlayExt as _};
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
    IntoElement, ListAlignment, ListState, ParentElement as _, Render, Role, SpringAnimation,
    SpringConfig, StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, list,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};
use serde::Deserialize;
use tcode_protocol::ThreadExportFormat;

use tcode_core::{
    project::{ProjectGroup, SessionMeta},
    settings::SidebarLayout,
};

use crate::shortcut::format_secondary_shortcut;
use crate::store::{ForkAvailability, TopicKind, WorkspaceStore, observe_store_topics};
use crate::time::{humanize_ago, now_secs};
use crate::window_drag_area;
use crate::window_state::WindowState;

/// Left padding on the sidebar's top row so branding clears the native macOS
/// traffic lights (ending near x=72 on macOS 26); a small inset elsewhere.
#[cfg(target_os = "macos")]
const TRAFFIC_LIGHT_INSET: f32 = 80.;
#[cfg(not(target_os = "macos"))]
const TRAFFIC_LIGHT_INSET: f32 = 8.;

/// Max threads shown per project group before the "Show more" row.
const THREADS_COLLAPSED_LIMIT: usize = 6;

/// Flat-list row geometry, including the 2px gap reserved below every row.
const FLAT_ROOT_ROW_HEIGHT: f32 = 50.;
const FLAT_CHILD_ROW_HEIGHT: f32 = 32.;

/// A critically damped spring keeps reordering legible without bouncing rows
/// past their destinations. GPUI also makes this snap to the target when the
/// operating system's reduced-motion preference is enabled.
const FLAT_REORDER_SPRING: SpringConfig = SpringConfig::new(420., 41., 1.);

/// Localized thread-list toggle, when the project has enough threads to need
/// one. Keeping the toggle present in both states is what lets an expanded list
/// be collapsed again.
fn thread_list_toggle_label(total: usize, expanded: bool) -> Option<Cow<'static, str>> {
    (total > THREADS_COLLAPSED_LIMIT).then(|| {
        if expanded {
            crate::tr!("sidebar.show_less")
        } else {
            crate::tr!("sidebar.show_more")
        }
    })
}

/// A sidebar label that owns the remaining row width and always truncates on
/// one line. `text_ellipsis` alone still leaves GPUI's default wrapping on,
/// which lets a glyph move onto a second line at resize boundaries.
fn truncated_sidebar_label() -> gpui::Div {
    div().flex_1().min_w_0().truncate()
}

fn child_count_badge(
    session_id: &str,
    total: usize,
    active: usize,
    cx: &mut Context<SessionsSidebar>,
) -> gpui::Stateful<gpui::Div> {
    let label = if active > 0 {
        format!("{active}/{total}")
    } else {
        total.to_string()
    };
    div()
        .id(gpui::SharedString::from(format!(
            "child-count-{session_id}"
        )))
        .flex_none()
        .min_w(px(18.))
        .text_center()
        .text_size(px(11.))
        .line_height(px(18.))
        .text_color(cx.theme().muted_foreground)
        .tooltip(move |window, cx| {
            Tooltip::new(crate::tr!("sidebar.child_threads", count = total).into_owned())
                .build(window, cx)
        })
        .child(label)
}

/// Fold indicator for a parent row. Trails the title (just before the
/// child-count badge) so the leading edge stays reserved for the title.
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

#[derive(Debug, PartialEq, Eq)]
struct ThreadRenderState {
    is_child: bool,
    show_unread: bool,
    direct_children: usize,
    active_direct_children: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ThreadFlags {
    unread: bool,
    waiting_for_approval: bool,
    waiting_for_input: bool,
    working: bool,
    /// Working only because background tasks remain; the turn has finished.
    background: bool,
}

struct ThreadRowState {
    session_id: String,
    row_key: String,
    waiting_for_approval: bool,
    waiting_for_input: bool,
    background: bool,
    is_worktree: bool,
    is_child: bool,
    show_unread: bool,
    direct_children: usize,
    active_direct_children: usize,
    children_collapsed: bool,
    renaming: Option<Entity<InputState>>,
    menu_can_fork: bool,
}

impl ThreadRowState {
    fn has_direct_children(&self) -> bool {
        self.direct_children > 0
    }

    fn waiting(&self) -> bool {
        self.waiting_for_approval || self.waiting_for_input
    }
}

fn derive_thread_render_state(
    meta: &SessionMeta,
    sessions: &[SessionMeta],
    flags: &HashMap<String, ThreadFlags>,
) -> ThreadRenderState {
    let own_flags = flags.get(&meta.id).copied().unwrap_or_default();
    let is_child = meta
        .parent_session_id
        .as_ref()
        .is_some_and(|parent_id| sessions.iter().any(|session| session.id == *parent_id));
    let direct_children = sessions
        .iter()
        .filter(|session| session.parent_session_id.as_deref() == Some(meta.id.as_str()))
        .count();
    let active_direct_children = sessions
        .iter()
        .filter(|session| session.parent_session_id.as_deref() == Some(meta.id.as_str()))
        .filter(|session| flags.get(&session.id).is_some_and(|flags| flags.working))
        .count();

    ThreadRenderState {
        is_child,
        // Orphaned child metadata is still child metadata and must not surface
        // completion unread state as an ordinary-thread blue dot.
        show_unread: meta.parent_session_id.is_none() && own_flags.unread && !own_flags.working,
        direct_children,
        active_direct_children,
    }
}

fn thread_visible(meta: &SessionMeta, collapsed_parents: &HashSet<String>) -> bool {
    meta.parent_session_id
        .as_ref()
        .is_none_or(|parent_id| !collapsed_parents.contains(parent_id))
}

/// Threads a group will actually render, in order. Children hidden by a
/// collapsed parent are dropped before the collapsed limit is applied, so
/// hidden rows never consume "Show more" slots.
fn visible_threads<'a>(
    sessions: &'a [SessionMeta],
    collapsed_parents: &HashSet<String>,
) -> Vec<&'a SessionMeta> {
    sessions
        .iter()
        .filter(|meta| thread_visible(meta, collapsed_parents))
        .collect()
}

#[derive(Debug)]
struct FlatThreadBlock<'a> {
    sessions: Vec<&'a SessionMeta>,
    bucket: u8,
    updated_at: u64,
}

/// Sort a parent-first flat session pool by block attention and recency, then
/// apply project filtering and the shared parent-collapse state.
fn flat_visible_threads<'a>(
    sessions: &'a [SessionMeta],
    collapsed_parents: &HashSet<String>,
    project_filter: Option<&str>,
    flags: &HashMap<String, ThreadFlags>,
) -> Vec<&'a SessionMeta> {
    let ids: HashSet<&str> = sessions.iter().map(|session| session.id.as_str()).collect();
    let mut blocks: Vec<Vec<&SessionMeta>> = Vec::new();
    for session in sessions {
        let is_root = session
            .parent_session_id
            .as_deref()
            .is_none_or(|parent_id| !ids.contains(parent_id));
        if is_root || blocks.is_empty() {
            blocks.push(vec![session]);
        } else if let Some(block) = blocks.last_mut() {
            block.push(session);
        }
    }

    let mut blocks: Vec<FlatThreadBlock<'_>> = blocks
        .into_iter()
        .filter(|block| {
            project_filter
                .is_none_or(|project_id| block[0].project_id.as_deref() == Some(project_id))
        })
        .map(|block| {
            let waiting = block.iter().any(|session| {
                flags
                    .get(&session.id)
                    .is_some_and(|flags| flags.waiting_for_approval || flags.waiting_for_input)
            });
            let working = block
                .iter()
                .any(|session| flags.get(&session.id).is_some_and(|flags| flags.working));
            let updated_at = block
                .iter()
                .map(|session| session.updated_at)
                .max()
                .unwrap_or_default();
            FlatThreadBlock {
                sessions: block,
                bucket: if waiting {
                    0
                } else if working {
                    1
                } else {
                    2
                },
                updated_at,
            }
        })
        .collect();
    blocks.sort_by(|a, b| {
        a.bucket
            .cmp(&b.bucket)
            .then_with(|| b.updated_at.cmp(&a.updated_at))
    });
    blocks
        .into_iter()
        .flat_map(|block| block.sessions)
        .filter(|meta| thread_visible(meta, collapsed_parents))
        .collect()
}

/// The target top edge for each visible flat-list row. These positions mirror
/// `render_flat_thread`: parent rows are 48px tall and child rows are 30px,
/// with another 2px of bottom spacing supplied by the list item wrapper.
fn flat_thread_top_offsets(visible: &[&SessionMeta], sessions: &[SessionMeta]) -> Vec<f32> {
    let ids = sessions
        .iter()
        .map(|session| session.id.as_str())
        .collect::<HashSet<_>>();
    let mut top = 0.;

    visible
        .iter()
        .map(|meta| {
            let offset = top;
            let is_child = meta
                .parent_session_id
                .as_deref()
                .is_some_and(|parent_id| ids.contains(parent_id));
            top += if is_child {
                FLAT_CHILD_ROW_HEIGHT
            } else {
                FLAT_ROOT_ROW_HEIGHT
            };
            offset
        })
        .collect()
}

fn animate_flat_thread_position(
    row: gpui::Div,
    session_id: &str,
    target_top: f32,
) -> impl IntoElement + use<> {
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

/// Startup fold state: every thread with visible direct children begins
/// collapsed, except the active thread and its ancestors so the restored
/// selection stays on screen (and, when it is itself a parent, open).
fn initial_collapsed_parents(sessions: &[SessionMeta], active_id: Option<&str>) -> HashSet<String> {
    let visible: Vec<&SessionMeta> = sessions
        .iter()
        .filter(|meta| meta.archived_at.is_none())
        .collect();
    let mut collapsed: HashSet<String> = visible
        .iter()
        .filter(|meta| {
            visible
                .iter()
                .any(|child| child.parent_session_id.as_deref() == Some(meta.id.as_str()))
        })
        .map(|meta| meta.id.clone())
        .collect();
    let mut current = active_id;
    let mut walked = HashSet::new();
    while let Some(id) = current {
        // A cyclic parent chain in a corrupt index must not hang startup.
        if !walked.insert(id) {
            break;
        }
        collapsed.remove(id);
        current = visible
            .iter()
            .find(|meta| meta.id == id)
            .and_then(|meta| meta.parent_session_id.as_deref());
    }
    collapsed
}

fn toggle_parent_for_row_click(
    collapsed_parents: &mut HashSet<String>,
    parent_id: &str,
    is_selected: bool,
    has_direct_children: bool,
) {
    if is_selected && has_direct_children && !collapsed_parents.remove(parent_id) {
        collapsed_parents.insert(parent_id.to_string());
    }
}

// Thread-row context-menu actions (each carries the target session id, so a
// single set of handlers on the sidebar root serves every row).
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadRename(String);
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
struct ThreadDelete(String);

// Project-header context-menu actions.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_project, no_json)]
struct ProjectArchiveAll(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_project, no_json)]
struct ProjectDelete(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_project, no_json)]
struct ProjectReveal(String);

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_sidebar, no_json)]
struct FilterProject(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_sidebar, no_json)]
struct StartDraftForProject(String);

/// In-progress inline rename of a thread row.
struct RenameState {
    session_id: String,
    input: Entity<InputState>,
    _sub: Subscription,
}

pub struct SessionsSidebar {
    loading: bool,
    store: Entity<WorkspaceStore>,
    window_state: Entity<WindowState>,
    /// Project ids whose thread list is expanded past the collapsed limit.
    expanded_groups: HashSet<String>,
    /// Parent session ids whose direct child rows are folded away.
    collapsed_parents: HashSet<String>,
    /// Optional project id filter for the session-local flat list.
    project_filter: Option<String>,
    /// The thread currently being renamed inline, if any.
    renaming: Option<RenameState>,
    /// Last expansion sweep result, cleared on collapse or the next sweep.
    auto_archive_notice: Option<(String, usize)>,
    /// First-run explainer queued by the launch sweep (count, days, keep).
    /// Opened from the first frame: the dialog needs the window's `Root`,
    /// which does not exist yet while the sidebar is constructed.
    startup_archive_dialog: Option<(usize, u32, usize)>,
    flat_list_state: ListState,
    _subscriptions: Vec<Subscription>,
}

impl SessionsSidebar {
    pub fn set_loading(&mut self, loading: bool, cx: &mut Context<Self>) {
        if self.loading != loading {
            self.loading = loading;
            cx.notify();
        }
    }

    fn compact(&self, cx: &gpui::App) -> bool {
        self.window_state.read(cx).compact
    }

    pub fn new(
        store: Entity<WorkspaceStore>,
        window_state: Entity<WindowState>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![observe_store_topics(
            &store,
            &[
                TopicKind::Index,
                TopicKind::Settings,
                TopicKind::ActiveSession,
                TopicKind::SessionStatus,
            ],
            cx,
        )];
        // Launch sweep: the same auto-archive pass expanding a thread list
        // runs, applied to every project up front so stale threads are gone
        // before the first paint (and before the fold state is seeded below).
        let project_ids = store.read(cx).project_ids();
        let mut sweeps = Vec::with_capacity(project_ids.len());
        for project_id in project_ids {
            sweeps.push(store.update(cx, |store, cx| store.auto_archive_sweep(project_id, cx)));
        }
        cx.spawn(async move |sidebar, cx| {
            let mut archived = 0;
            for sweep in sweeps {
                match sweep.await {
                    Ok(tcode_protocol::CommandResponse::ArchivedCount(count)) => {
                        archived += count;
                    }
                    Ok(other) => {
                        log::error!("unexpected auto-archive response: {other:?}");
                    }
                    Err(error) => {
                        log::error!("startup auto-archive sweep failed: {}", error.message);
                    }
                }
            }
            let _ = sidebar.update(cx, |sidebar, cx| {
                let settings = sidebar.store.read(cx).settings();
                sidebar.startup_archive_dialog =
                    (archived > 0 && !settings.auto_archive_notice_shown).then(|| {
                        (
                            archived,
                            settings.auto_archive_max_idle_days.max(1),
                            settings.auto_archive_keep_count.max(1),
                        )
                    });
                cx.notify();
            });
        })
        .detach();
        let collapsed_parents = {
            let sessions = store.read(cx).sidebar_sessions();
            let active_id = store.read(cx).active_session_id();
            initial_collapsed_parents(&sessions, active_id.as_deref())
        };
        Self {
            loading: false,
            store,
            window_state,
            expanded_groups: HashSet::new(),
            collapsed_parents,
            project_filter: None,
            renaming: None,
            auto_archive_notice: None,
            startup_archive_dialog: None,
            flat_list_state: ListState::new(0, ListAlignment::Top, px(120.)),
            _subscriptions: subscriptions,
        }
    }

    // -- actions ------------------------------------------------------------

    /// Prompt for a directory, then create a project rooted there.
    fn add_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        crate::add_project_dialog::open(self.store.clone(), window, cx);
    }

    fn toggle_group(&mut self, project_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.expanded_groups.remove(project_id) {
            if self
                .auto_archive_notice
                .as_ref()
                .is_some_and(|(notice_project, _)| notice_project == project_id)
            {
                self.auto_archive_notice = None;
            }
        } else {
            self.auto_archive_notice = None;
            let (notice_shown, days, keep) = {
                let settings = self.store.read(cx).settings();
                (
                    settings.auto_archive_notice_shown,
                    settings.auto_archive_max_idle_days.max(1),
                    settings.auto_archive_keep_count.max(1),
                )
            };
            let sweep = self.store.update(cx, |store, cx| {
                store.auto_archive_sweep(project_id.to_string(), cx)
            });
            self.expanded_groups.insert(project_id.to_string());
            let project_id = project_id.to_string();
            cx.spawn_in(window, async move |sidebar, cx| {
                let count = match sweep.await {
                    Ok(tcode_protocol::CommandResponse::ArchivedCount(count)) => count,
                    Ok(other) => {
                        log::error!("unexpected auto-archive response: {other:?}");
                        return;
                    }
                    Err(error) => {
                        log::error!("auto-archive sweep failed: {}", error.message);
                        return;
                    }
                };
                let _ = sidebar.update_in(cx, |sidebar, window, cx| {
                    if count > 0 {
                        sidebar.auto_archive_notice = Some((project_id, count));
                        if !notice_shown {
                            sidebar.show_auto_archive_dialog(count, days, keep, window, cx);
                        }
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        cx.notify();
    }

    fn show_auto_archive_dialog(
        &self,
        count: usize,
        days: u32,
        keep: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.store.update(cx, |store, _cx| {
            store.set_auto_archive_notice_shown(true);
        });
        let window_state = self.window_state.clone();
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let alert = alert.bg(cx.theme().popover);
            let window_state = window_state.clone();
            alert
                .title(crate::tr!("sidebar.auto_archive_dialog.title"))
                .description(crate::tr!(
                    "sidebar.auto_archive_dialog.body",
                    count = count,
                    days = days,
                    keep = keep
                ))
                .button_props(
                    DialogButtons::default()
                        .ok_text(crate::tr!("sidebar.auto_archive_dialog.open_settings"))
                        .cancel_text(crate::tr!("sidebar.auto_archive_dialog.got_it"))
                        .show_cancel(true),
                )
                .on_ok(move |_, _, cx| {
                    window_state.update(cx, |state, cx| {
                        state.pending_settings_section = Some("archived".into());
                        state.open_settings(cx);
                    });
                    true
                })
        });
    }

    // -- context-menu action handlers ---------------------------------------

    fn on_filter_project(
        &mut self,
        action: &FilterProject,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.project_filter = (!action.0.is_empty()).then(|| action.0.clone());
        cx.notify();
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
        self.store.update(cx, |store, _cx| {
            store.mark_session_unread(id);
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
            meta.title,
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
                        .ok_variant(ButtonVariant::Danger)
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

    fn on_project_delete(
        &mut self,
        action: &ProjectDelete,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let project_id = action.0.clone();
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
        if store.read(cx).turn_running_for(session_id) {
            return;
        }
        let session_id = session_id.to_string();
        if store.read(cx).settings().skip_delete_confirmation {
            store.update(cx, |store, _cx| {
                store.archive_session(session_id.clone());
            });
            return;
        }
        let title = title.to_string();
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let alert = alert.bg(cx.theme().popover);
            let store = store.clone();
            let session_id = session_id.clone();
            alert
                .title(crate::tr!("sidebar.archive_title"))
                .description(crate::tr!("sidebar.archive_description", title = title))
                .button_props(
                    DialogButtons::default()
                        .ok_variant(ButtonVariant::Danger)
                        .ok_text(crate::tr!("sidebar.archive_action"))
                        .cancel_text(crate::tr!("settings.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, _, cx| {
                    store.update(cx, |store, _cx| {
                        store.archive_session(session_id.clone());
                    });
                    true
                })
        });
    }

    /// Permanently delete a thread: an optional confirm, then (when it orphans a
    /// worktree) a second "remove the worktree too?" prompt.
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
        let title = title.to_string();
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let alert = alert.bg(cx.theme().popover);
            let store = store.clone();
            let session_id = session_id.clone();
            alert
                .title(crate::tr!("sidebar.delete_title", title = title.clone()))
                .description(crate::tr!("sidebar.delete_description"))
                .button_props(
                    DialogButtons::default()
                        .ok_variant(ButtonVariant::Danger)
                        .ok_text(crate::tr!("sidebar.delete_action"))
                        .cancel_text(crate::tr!("settings.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    proceed_delete(store.clone(), session_id.clone(), window, cx);
                    true
                })
        });
    }

    // -- rendering ----------------------------------------------------------

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
                .child("tcode"),
        )
        .child(
            div()
                .px_1()
                .py(px(1.))
                .rounded_sm()
                .bg(cx.theme().muted)
                .text_color(cx.theme().muted_foreground)
                .text_size(px(9.))
                .font_semibold()
                .child("DEV"),
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
            .rounded(cx.theme().radius)
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

    fn render_layout_toggle(&self, layout: SidebarLayout, cx: &mut Context<Self>) -> Button {
        let tooltip = match layout {
            SidebarLayout::Flat => crate::tr!("sidebar.layout_grouped"),
            SidebarLayout::Grouped => crate::tr!("sidebar.layout_flat"),
        };
        Button::new("toggle-sidebar-layout")
            .ghost()
            .xsmall()
            .compact()
            .icon(IconName::LayoutDashboard)
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
                                this.store.update(cx, |store, _cx| {
                                    store.cycle_project_sort();
                                });
                            })),
                    )
                    .child(self.render_layout_toggle(SidebarLayout::Grouped, cx))
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
                    ),
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
            Button::new("new-flat-thread")
                .ghost()
                .xsmall()
                .compact()
                .icon(IconName::Plus)
                .tooltip(crate::tr!("sidebar.create_thread"))
                .dropdown_menu(move |menu, _window, _cx| {
                    let mut menu = menu;
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
                    ),
            )
    }

    fn render_group(
        &self,
        group: &ProjectGroup,
        sessions: &[SessionMeta],
        flags: &HashMap<String, ThreadFlags>,
        collapsed: bool,
        active_id: Option<&str>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let project_id = group.project.id.clone();
        let has_unread = group.sessions.iter().any(|meta| {
            meta.parent_session_id.is_none()
                && flags.get(&meta.id).is_some_and(|flags| flags.unread)
        });
        let group_key = format!("group-{project_id}");

        let expanded = self.expanded_groups.contains(&project_id);
        let threads = visible_threads(&group.sessions, &self.collapsed_parents);
        let total = threads.len();
        let visible = if expanded {
            total
        } else {
            total.min(THREADS_COLLAPSED_LIMIT)
        };

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
        .group(group_key.clone())
        .h(px(30.))
        .items_center()
        .gap_1()
        .px_2()
        .rounded(cx.theme().radius)
        .cursor_pointer()
        .hover(|s| s.bg(cx.theme().sidebar_accent))
        .on_click(cx.listener(move |this, _, _, cx| {
            this.store.update(cx, |store, _cx| {
                store.toggle_project_collapsed(header_toggle_id.clone());
            });
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
        .child(
            Icon::new(IconName::Folder)
                .size_4()
                .text_color(cx.theme().muted_foreground),
        )
        .child(
            truncated_sidebar_label()
                .text_sm()
                .font_medium()
                .text_color(cx.theme().sidebar_foreground)
                .child(group.project.name.clone()),
        )
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
            .rounded(cx.theme().radius * 0.5)
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
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
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
        let mut container = v_flex().flex_none().child(
            header
                .context_menu(move |menu, _window, _cx| {
                    let id = menu_project_id.clone();
                    let delete_label = crate::tr!("sidebar.remove_project").into_owned();
                    menu.menu_with_enable(
                        crate::tr!("sidebar.archive_all").into_owned(),
                        Box::new(ProjectArchiveAll(id.clone())),
                        can_archive,
                    )
                    .menu_element(Box::new(ProjectDelete(id.clone())), move |_window, cx| {
                        div()
                            .flex_1()
                            .text_color(cx.theme().danger)
                            .child(delete_label.clone())
                    })
                    .menu(
                        crate::tr!("sidebar.reveal_project").into_owned(),
                        Box::new(ProjectReveal(id)),
                    )
                })
                .touch(false),
        );

        if !collapsed {
            for meta in threads.iter().take(visible).copied() {
                let is_active = active_id == Some(meta.id.as_str());
                // "Working" covers parked sessions too — a thread that keeps
                // running in the background keeps its green dot.
                container =
                    container.child(self.render_thread(meta, sessions, flags, is_active, cx));
            }
            if let Some(toggle_label) = thread_list_toggle_label(total, expanded) {
                let toggle_id = project_id.clone();
                container = container.child(
                    crate::material::accessible_clickable(
                        div(),
                        gpui::SharedString::from(format!("show-more-{project_id}")),
                        Role::Button,
                        toggle_label.clone(),
                        cx,
                    )
                    .aria_expanded(expanded)
                    .pl(px(30.))
                    .py_1()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .cursor_pointer()
                    .hover(|s| s.text_color(cx.theme().sidebar_foreground))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.toggle_group(&toggle_id, window, cx);
                    }))
                    .child(toggle_label),
                );
            }
            if expanded
                && let Some((_, count)) = self
                    .auto_archive_notice
                    .as_ref()
                    .filter(|(notice_project, _)| notice_project == &project_id)
            {
                let label = crate::tr!("sidebar.auto_archived", count = *count);
                let window_state = self.window_state.clone();
                container = container.child(
                    crate::material::accessible_clickable(
                        div(),
                        gpui::SharedString::from(format!("auto-archived-{project_id}")),
                        Role::Button,
                        label.clone(),
                        cx,
                    )
                    .pl(px(30.))
                    .py_1()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .cursor_pointer()
                    .hover(|s| s.text_color(cx.theme().sidebar_foreground))
                    .on_click(move |_, _, cx| {
                        window_state.update(cx, |state, cx| {
                            state.pending_settings_section = Some("archived".into());
                            state.open_settings(cx);
                        });
                    })
                    .child(label),
                );
            }
        }

        container
    }

    fn thread_row_state(
        &self,
        meta: &SessionMeta,
        sessions: &[SessionMeta],
        flags: &HashMap<String, ThreadFlags>,
        row_key: String,
    ) -> ThreadRowState {
        let session_id = meta.id.clone();
        let own_flags = flags.get(&session_id).copied().unwrap_or_default();
        let render_state = derive_thread_render_state(meta, sessions, flags);
        ThreadRowState {
            children_collapsed: self.collapsed_parents.contains(&session_id),
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
            is_worktree: meta.worktree.is_some(),
            is_child: render_state.is_child,
            show_unread: render_state.show_unread,
            direct_children: render_state.direct_children,
            active_direct_children: render_state.active_direct_children,
            menu_can_fork: meta.provider.caps().supports_fork,
        }
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
        let has_direct_children = state.has_direct_children();
        crate::material::accessible_clickable(
            base,
            row_id,
            Role::Button,
            crate::tr!("sidebar.thread", title = meta.title.clone()).into_owned(),
            cx,
        )
        .aria_selected(is_active)
        .when(has_direct_children, |row| {
            row.aria_expanded(!state.children_collapsed)
        })
        .group(state.row_key.clone())
        .cursor_pointer()
        .when(is_active, |row| row.bg(cx.theme().list_active))
        .when(!is_active, |row| {
            row.hover(|row| row.bg(cx.theme().sidebar_accent))
        })
        .on_click(cx.listener(move |this, _, _, cx| {
            let session_id = session_id.clone();
            toggle_parent_for_row_click(
                &mut this.collapsed_parents,
                &session_id,
                is_active,
                has_direct_children,
            );
            this.store.update(cx, |store, _cx| {
                store.select_session(session_id.clone());
            });
            this.window_state
                .update(cx, |state, cx| state.open_thread(cx));
            cx.notify();
        }))
        .when(state.waiting_for_approval, |row| {
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
        if let Some(input) = &state.renaming {
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
                .text_color(cx.theme().sidebar_foreground)
                .when(emphasize_unread && state.show_unread, |title| {
                    title.font_semibold()
                })
                .child(meta.title.clone())
                .into_any_element()
        }
    }

    fn thread_status_badge(
        state: &ThreadRowState,
        working: bool,
        cx: &Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let (color, label) = if state.waiting_for_approval {
            (cx.theme().warning, crate::tr!("sidebar.waiting_approval"))
        } else if state.waiting_for_input {
            (cx.theme().warning, crate::tr!("sidebar.waiting_input"))
        } else if working && state.background {
            (
                cx.theme().muted_foreground,
                crate::tr!("sidebar.background_tasks"),
            )
        } else if working {
            (cx.theme().primary, crate::tr!("sidebar.working"))
        } else {
            return None;
        };
        Some(
            h_flex()
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
                .into_any_element(),
        )
    }

    fn thread_context_menu(
        row: gpui::Stateful<gpui::Div>,
        session_id: String,
        running: bool,
        can_fork: bool,
        is_worktree: bool,
        compact: bool,
    ) -> gpui::AnyElement {
        row.context_menu(move |menu, _window, _cx| {
            let id = session_id.clone();
            menu.menu(
                crate::tr!("sidebar.ctx_rename").into_owned(),
                Box::new(ThreadRename(id.clone())),
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
                crate::tr!("sidebar.ctx_mark_unread").into_owned(),
                Box::new(ThreadMarkUnread(id.clone())),
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
            .menu(
                crate::tr!("sidebar.ctx_delete").into_owned(),
                Box::new(ThreadDelete(id.clone())),
            )
        })
        .touch(compact)
        .into_any_element()
    }

    fn render_thread(
        &self,
        meta: &SessionMeta,
        sessions: &[SessionMeta],
        flags: &HashMap<String, ThreadFlags>,
        is_active: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let working = flags.get(&meta.id).is_some_and(|flags| flags.working);
        let state = self.thread_row_state(meta, sessions, flags, format!("thread-{}", meta.id));
        let session_id = state.session_id.clone();
        let row_key = state.row_key.clone();
        let is_worktree = state.is_worktree;
        let is_child = state.is_child;
        let show_unread = state.show_unread;
        let direct_children = state.direct_children;
        let active_direct_children = state.active_direct_children;
        let has_direct_children = state.has_direct_children();
        let children_collapsed = state.children_collapsed;

        let row = self
            .thread_clickable_row(
                h_flex(),
                gpui::SharedString::from(format!("thread-row-{session_id}")),
                meta,
                &state,
                is_active,
                cx,
            )
            .h(px(30.))
            .items_center()
            .gap_2()
            .pl(px(if is_child { 42. } else { 30. }))
            .pr_2()
            // macOS sidebar selection: a tight 6px rounded rect, not a capsule.
            .rounded(px(6.))
            .when_some(
                Self::thread_status_badge(&state, working, cx),
                |row, badge| row.child(badge),
            )
            .when(is_child, |row| {
                row.child(
                    div()
                        .flex_none()
                        .text_size(px(13.))
                        .text_color(cx.theme().muted_foreground)
                        .child("↳"),
                )
            });

        // Row body: rename input, or the (unread dot + worktree glyph + title).
        // The fold chevron trails the title, right before the child-count
        // badge, so the title keeps the row's full leading width.
        let row = if state.renaming.is_some() {
            row.child(self.thread_title_or_input(meta, &state, false, cx))
                .when(has_direct_children, |row| {
                    row.child(collapse_chevron(children_collapsed, cx))
                        .child(child_count_badge(
                            &session_id,
                            direct_children,
                            active_direct_children,
                            cx,
                        ))
                })
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
            .when(has_direct_children, |row| {
                row.child(collapse_chevron(children_collapsed, cx))
                    .child(child_count_badge(
                        &session_id,
                        direct_children,
                        active_direct_children,
                        cx,
                    ))
            })
            .when(!working, |row| {
                row.child(self.render_flat_thread_right_slot(meta, &row_key, false, true, cx))
            })
        };

        Self::thread_context_menu(
            row,
            session_id,
            working,
            state.menu_can_fork,
            is_worktree,
            false,
        )
    }

    fn render_flat_thread_right_slot(
        &self,
        meta: &SessionMeta,
        row_key: &str,
        waiting: bool,
        archive_on_hover: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let session_id = meta.id.clone();
        let archive_id = session_id.clone();
        let archive_title = meta.title.clone();
        let ago = humanize_ago(now_secs().saturating_sub(meta.updated_at));
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
                    .text_color(if waiting {
                        cx.theme().warning
                    } else {
                        cx.theme().muted_foreground
                    })
                    .when(archive_on_hover, |time| {
                        time.group_hover(row_key.clone(), |time| time.invisible())
                    })
                    .child(ago),
            )
            .when(archive_on_hover, |slot| {
                slot.child(
                    crate::material::accessible_clickable(
                        h_flex(),
                        gpui::SharedString::from(format!("archive-flat-thread-{session_id}")),
                        Role::Button,
                        crate::tr!("sidebar.archive"),
                        cx,
                    )
                    .absolute()
                    .right_0()
                    .top_0()
                    .size_5()
                    .items_center()
                    .justify_center()
                    .rounded(cx.theme().radius * 0.5)
                    .cursor_pointer()
                    .opacity(0.)
                    .group_hover(row_key, |button| button.opacity(1.))
                    .focus(|button| button.opacity(1.).bg(cx.theme().sidebar_accent))
                    .hover(|button| button.bg(cx.theme().sidebar_accent))
                    .tooltip(|window, cx| {
                        Tooltip::new(crate::tr!("sidebar.archive").into_owned()).build(window, cx)
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.archive_thread(&archive_id, &archive_title, window, cx);
                    }))
                    .child(
                        Icon::empty()
                            .path("icons/archive.svg")
                            .xsmall()
                            .text_color(cx.theme().muted_foreground),
                    ),
                )
            })
    }

    fn render_flat_thread(
        &self,
        meta: &SessionMeta,
        sessions: &[SessionMeta],
        flags: &HashMap<String, ThreadFlags>,
        project_name: Option<String>,
        is_active: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let working = flags.get(&meta.id).is_some_and(|flags| flags.working);
        let state =
            self.thread_row_state(meta, sessions, flags, format!("flat-thread-{}", meta.id));
        let session_id = state.session_id.clone();
        let row_key = state.row_key.clone();
        let waiting_for_approval = state.waiting_for_approval;
        let waiting_for_input = state.waiting_for_input;
        let waiting = state.waiting();
        let is_child = state.is_child;
        let show_unread = state.show_unread;
        let direct_children = state.direct_children;
        let active_direct_children = state.active_direct_children;
        let has_direct_children = state.has_direct_children();
        let children_collapsed = state.children_collapsed;

        let row = self
            .thread_clickable_row(
                if state.is_child { h_flex() } else { v_flex() },
                gpui::SharedString::from(format!("flat-thread-row-{session_id}")),
                meta,
                &state,
                is_active,
                cx,
            )
            .when(is_child, |row| row.h(px(30.)).items_center().ml(px(12.)))
            .when(!is_child, |row| row.h(px(48.)).justify_center().gap(px(2.)))
            .px_2()
            .rounded(px(6.));

        let row = if is_child {
            let title_or_input = self.thread_title_or_input(meta, &state, false, cx);
            row.child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .items_center()
                    .gap_2()
                    .when_some(
                        Self::thread_status_badge(&state, working, cx),
                        |line, badge| line.child(badge),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(13.))
                            .text_color(cx.theme().muted_foreground)
                            .child("↳"),
                    )
                    .child(title_or_input)
                    .when(has_direct_children, |line| {
                        line.child(collapse_chevron(children_collapsed, cx)).child(
                            child_count_badge(
                                &session_id,
                                direct_children,
                                active_direct_children,
                                cx,
                            ),
                        )
                    })
                    .when(!working, |line| {
                        line.child(
                            self.render_flat_thread_right_slot(meta, &row_key, waiting, true, cx),
                        )
                    }),
            )
        } else {
            let title_or_input = self.thread_title_or_input(meta, &state, true, cx);
            let line_one = h_flex()
                .w_full()
                .min_w_0()
                .items_center()
                .gap_2()
                .when(show_unread, |line| {
                    line.child(
                        div()
                            .flex_none()
                            .size(px(6.))
                            .rounded_full()
                            .bg(cx.theme().primary),
                    )
                })
                .when(!show_unread && waiting, |line| {
                    line.child(
                        div()
                            .flex_none()
                            .size(px(6.))
                            .rounded_full()
                            .bg(cx.theme().warning),
                    )
                })
                .when(!show_unread && !waiting && working, |line| {
                    line.child(
                        div()
                            .flex_none()
                            .size(px(6.))
                            .rounded_full()
                            .bg(cx.theme().primary),
                    )
                })
                .child(title_or_input)
                .child(self.render_flat_thread_right_slot(meta, &row_key, waiting, !working, cx));

            let has_project = project_name.is_some();
            let line_two = h_flex()
                .w_full()
                .min_w_0()
                .items_center()
                .gap_1()
                .text_size(px(11.))
                .text_color(cx.theme().muted_foreground)
                .when(waiting_for_approval, |line| {
                    line.child(
                        div()
                            .flex_none()
                            .text_color(cx.theme().warning)
                            .child(crate::tr!("sidebar.waiting_approval")),
                    )
                })
                .when(waiting_for_input && !waiting_for_approval, |line| {
                    line.child(
                        div()
                            .flex_none()
                            .text_color(cx.theme().warning)
                            .child(crate::tr!("sidebar.waiting_input")),
                    )
                })
                .when(working && !waiting, |line| {
                    line.child(
                        div()
                            .flex_none()
                            .text_color(cx.theme().primary)
                            .child(crate::tr!("sidebar.working")),
                    )
                })
                .when((waiting || working) && has_project, |line| {
                    line.child(div().flex_none().child("·"))
                })
                .when_some(project_name, |line, project_name| {
                    line.child(
                        Icon::new(IconName::Folder)
                            .flex_none()
                            .size_3()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(truncated_sidebar_label().child(project_name))
                })
                // Without a project label there is no flex-1 element on the
                // line, so a spacer keeps the chevron and badge bottom-right.
                .when(!has_project, |line| line.child(div().flex_1()))
                .when(meta.worktree.is_some(), |line| {
                    line.child(
                        Icon::empty()
                            .path("icons/git-branch.svg")
                            .xsmall()
                            .text_color(cx.theme().muted_foreground),
                    )
                })
                .when(has_direct_children, |line| {
                    line.child(collapse_chevron(children_collapsed, cx))
                        .child(child_count_badge(
                            &session_id,
                            direct_children,
                            active_direct_children,
                            cx,
                        ))
                });
            row.child(line_one).child(line_two)
        };

        Self::thread_context_menu(
            row,
            session_id,
            working,
            state.menu_can_fork,
            meta.worktree.is_some(),
            false,
        )
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex_none()
            // No full-bleed divider above the footer: whitespace separates it.
            .child(
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

/// Delete `session_id`, first asking whether to also remove an orphaned worktree.
fn proceed_delete(
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

// ---------------------------------------------------------------------------
// Compact (phone) thread list — docs/mobile-design.md §3.3
// ---------------------------------------------------------------------------

/// The phone's list geometry (docs/mobile-design.md §5).
const COMPACT_PAGE_PADDING: f32 = 16.;
const COMPACT_ROW_HEIGHT: f32 = 56.;
const COMPACT_SEARCH_HEIGHT: f32 = 40.;

impl SessionsSidebar {
    /// The phone's thread list: a pill search field, project group headers with
    /// a thread count and a collapse chevron, and 56pt rows carrying a status
    /// glyph plus a "status word · relative time" subtitle.
    ///
    /// None of the desktop chrome is rendered here — no app row, no
    /// "Threads / Projects" header with its sort, layout and add-project
    /// controls (that row owns the *second* "+" the phone must not have), no
    /// footer, and no persistent selection highlight: navigating away from the
    /// list is what "selects" a thread on a phone.
    fn render_compact(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (groups, collapsed_projects, sessions, flags) = {
            let store = self.store.read(cx);
            let sessions = store.sidebar_sessions();
            let flags = sessions
                .iter()
                .map(|meta| {
                    (
                        meta.id.clone(),
                        ThreadFlags {
                            unread: store.session_unread(&meta.id),
                            waiting_for_approval: store.pending_approval_for(&meta.id),
                            waiting_for_input: store.pending_user_input_for(&meta.id),
                            working: store.turn_running_for(&meta.id),
                            background: store.background_only_for(&meta.id),
                        },
                    )
                })
                .collect::<HashMap<_, _>>();
            let groups = store.grouped_sessions();
            let collapsed = groups
                .iter()
                .filter(|group| store.is_project_collapsed(&group.project.id))
                .map(|group| group.project.id.clone())
                .collect::<HashSet<_>>();
            (groups, collapsed, sessions, flags)
        };

        let body = if self.loading {
            // Three skeleton rows while the first index snapshot is in flight.
            v_flex()
                .flex_1()
                .px(px(COMPACT_PAGE_PADDING))
                .pt(px(8.))
                .gap(px(4.))
                .children((0..3).map(|_| {
                    v_flex()
                        .h(px(COMPACT_ROW_HEIGHT))
                        .justify_center()
                        .gap(px(8.))
                        .opacity(0.3)
                        .child(
                            div()
                                .w(gpui::relative(0.7))
                                .h(px(14.))
                                .rounded(px(4.))
                                .bg(cx.theme().secondary),
                        )
                        .child(
                            div()
                                .w(gpui::relative(0.35))
                                .h(px(11.))
                                .rounded(px(4.))
                                .bg(cx.theme().secondary),
                        )
                }))
                .into_any_element()
        } else if groups.is_empty() {
            crate::material::empty_state(
                Icon::new(IconName::Folder),
                crate::tr!("mobile.projects_empty"),
                crate::tr!("mobile.projects_help"),
                cx,
            )
            .into_any_element()
        } else {
            let mut list = v_flex().w_full().px(px(COMPACT_PAGE_PADDING)).pb(px(24.));
            for group in &groups {
                let collapsed = collapsed_projects.contains(&group.project.id);
                list = list.child(self.render_compact_group_header(group, collapsed, cx));
                if collapsed {
                    continue;
                }
                for meta in visible_threads(&group.sessions, &self.collapsed_parents) {
                    list = list.child(self.render_compact_thread(meta, &sessions, &flags, cx));
                }
            }
            div()
                .id("compact-thread-list")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(list)
                .into_any_element()
        };

        v_flex()
            .size_full()
            .bg(crate::material::content_surface(cx))
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(Self::on_rename))
            .on_action(cx.listener(Self::on_fork))
            .on_action(cx.listener(Self::on_merge_worktree))
            .on_action(cx.listener(Self::on_mark_unread))
            .on_action(cx.listener(Self::on_copy_path))
            .on_action(cx.listener(Self::on_copy_id))
            .on_action(cx.listener(Self::on_export_jsonl))
            .on_action(cx.listener(Self::on_export_markdown))
            .on_action(cx.listener(Self::on_archive))
            .on_action(cx.listener(Self::on_delete))
            .child(self.render_compact_search(cx))
            .child(body)
            .into_any_element()
    }

    /// The pill search field (§3.3): 40pt tall, T2 fill, magnifier + placeholder.
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

    /// Project group header (§3.3): folder + name + thread count + chevron.
    fn render_compact_group_header(
        &self,
        group: &ProjectGroup,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let project_id = group.project.id.clone();
        let count = visible_threads(&group.sessions, &self.collapsed_parents).len();
        crate::material::accessible_clickable(
            h_flex(),
            gpui::SharedString::from(format!("compact-group-{project_id}")),
            Role::Button,
            crate::tr!("sidebar.project", name = group.project.name.clone()).into_owned(),
            cx,
        )
        .aria_expanded(!collapsed)
        .w_full()
        .mt(px(20.))
        .mb(px(6.))
        .h(px(24.))
        .items_center()
        .gap(px(8.))
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, _, cx| {
            this.store.update(cx, |store, _cx| {
                store.toggle_project_collapsed(project_id.clone());
            });
        }))
        .child(
            Icon::new(IconName::Folder)
                .size(px(16.))
                .text_color(cx.theme().muted_foreground),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(14.))
                .font_semibold()
                .child(group.project.name.clone()),
        )
        .child(
            div()
                .flex_none()
                .text_size(px(13.))
                .text_color(cx.theme().muted_foreground)
                .child(count.to_string()),
        )
        .child(
            Icon::new(if collapsed {
                IconName::ChevronRight
            } else {
                IconName::ChevronDown
            })
            .size(px(16.))
            .text_color(cx.theme().muted_foreground),
        )
    }

    /// One 56pt thread row (§3.3). No chevron, no persistent selection — only a
    /// pressed tint; long press opens the shared thread context menu.
    fn render_compact_thread(
        &self,
        meta: &SessionMeta,
        sessions: &[SessionMeta],
        flags: &HashMap<String, ThreadFlags>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let working = flags.get(&meta.id).is_some_and(|flags| flags.working);
        let state =
            self.thread_row_state(meta, sessions, flags, format!("compact-thread-{}", meta.id));
        let session_id = state.session_id.clone();
        let status = compact_status_line(&state, working, cx);
        let click_id = session_id.clone();

        let row = crate::material::accessible_clickable(
            h_flex(),
            gpui::SharedString::from(format!("compact-thread-row-{session_id}")),
            Role::Button,
            crate::tr!("sidebar.thread", title = meta.title.clone()).into_owned(),
            cx,
        )
        .w_full()
        .min_h(px(COMPACT_ROW_HEIGHT))
        .items_center()
        .gap(px(12.))
        .py(px(8.))
        .px(px(8.))
        .rounded(crate::material::radius_card())
        // Rows needing the user carry a 6% semantic wash; everything else sits
        // on the paper with only a pressed tint.
        .when(state.waiting_for_approval, |row| {
            row.bg(cx.theme().warning.opacity(0.06))
        })
        .when(
            state.waiting_for_input && !state.waiting_for_approval,
            |row| row.bg(cx.theme().primary.opacity(0.06)),
        )
        .cursor_pointer()
        .active(|s| s.bg(cx.theme().foreground.opacity(0.08)))
        .on_click(cx.listener(move |this, _, _, cx| {
            this.store.update(cx, |store, _cx| {
                store.select_session(click_id.clone());
            });
            this.window_state
                .update(cx, |state, cx| state.open_thread(cx));
        }))
        .child(compact_status_glyph(&state, working, cx))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap(px(2.))
                .child(
                    div()
                        .w_full()
                        .min_w_0()
                        .truncate()
                        .text_size(px(16.))
                        .line_height(px(21.))
                        .when(state.show_unread, |title| title.font_semibold())
                        .child(meta.title.clone()),
                )
                .child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap(px(4.))
                        .text_size(px(13.))
                        .line_height(px(18.))
                        .text_color(cx.theme().muted_foreground)
                        .when_some(status, |line, (label, color)| {
                            line.child(div().flex_none().text_color(color).child(label))
                                .child(div().flex_none().child("·"))
                        })
                        .child(
                            div()
                                .flex_none()
                                .child(humanize_ago(now_secs().saturating_sub(meta.updated_at))),
                        ),
                ),
        );
        Self::thread_context_menu(
            row,
            session_id,
            working,
            state.menu_can_fork,
            meta.worktree.is_some(),
            true,
        )
    }
}

/// The 20×20 status slot at the head of a compact row (§3.3). The slot is
/// always taken so titles line up whether or not a thread has a status.
fn compact_status_glyph(state: &ThreadRowState, working: bool, cx: &App) -> gpui::AnyElement {
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
    if working {
        return slot
            .justify_center()
            .child(Spinner::new().small().color(cx.theme().primary))
            .into_any_element();
    }
    if state.show_unread {
        return slot
            .justify_center()
            .child(div().size(px(8.)).rounded_full().bg(cx.theme().primary))
            .into_any_element();
    }
    slot.into_any_element()
}

/// The subtitle's status word and its color (§1 / §5), or `None` for an idle
/// thread — copy rule 6: a status word only appears when it carries
/// information, so a settled thread shows its time alone.
fn compact_status_line(
    state: &ThreadRowState,
    working: bool,
    cx: &App,
) -> Option<(Cow<'static, str>, gpui::Hsla)> {
    if state.waiting_for_approval {
        Some((crate::tr!("mobile.approval"), cx.theme().warning))
    } else if state.waiting_for_input {
        Some((crate::tr!("mobile.answer"), cx.theme().primary))
    } else if working {
        Some((crate::tr!("mobile.working"), cx.theme().primary))
    } else if state.show_unread {
        Some((crate::tr!("mobile.unread"), cx.theme().primary))
    } else {
        None
    }
}

impl Render for SessionsSidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.compact(cx) {
            return self.render_compact(cx);
        }
        if let Some((count, days, keep)) = self.startup_archive_dialog.take() {
            // Deferred: opening a dialog walks the window `Root`, which is an
            // ancestor of this view and still borrowed during render.
            cx.defer_in(window, move |this, window, cx| {
                this.show_auto_archive_dialog(count, days, keep, window, cx);
            });
        }
        let (
            layout,
            active_id,
            sessions,
            groups,
            flat_sessions,
            projects,
            collapsed_projects,
            flags,
        ) = {
            let store = self.store.read(cx);
            let sessions = store.sidebar_sessions();
            let flags = sessions
                .iter()
                .map(|meta| {
                    (
                        meta.id.clone(),
                        ThreadFlags {
                            unread: store.session_unread(&meta.id),
                            waiting_for_approval: store.pending_approval_for(&meta.id),
                            waiting_for_input: store.pending_user_input_for(&meta.id),
                            working: store.turn_running_for(&meta.id),
                            background: store.background_only_for(&meta.id),
                        },
                    )
                })
                .collect::<HashMap<_, _>>();
            let groups = store.grouped_sessions();
            let collapsed_projects = groups
                .iter()
                .filter(|group| store.is_project_collapsed(&group.project.id))
                .map(|group| group.project.id.clone())
                .collect::<HashSet<_>>();
            (
                store.sidebar_layout(),
                store.active_session_id(),
                sessions,
                groups,
                store.flat_sessions(),
                store.projects(),
                collapsed_projects,
                flags,
            )
        };
        let (header, thread_list) = match layout {
            SidebarLayout::Grouped => {
                let mut list_content = v_flex().w_full().px_2().pb_2().gap(px(2.));
                if groups.is_empty() {
                    list_content = list_content.child(
                        div()
                            .px_2()
                            .py_3()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(crate::tr!("sidebar.empty")),
                    );
                } else {
                    for group in &groups {
                        list_content = list_content.child(self.render_group(
                            group,
                            &sessions,
                            &flags,
                            collapsed_projects.contains(&group.project.id),
                            active_id.as_deref(),
                            cx,
                        ));
                    }
                }
                let thread_list = div()
                    .id("sidebar-project-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .child(div().size_full().child(list_content))
                    .into_any_element();
                (
                    self.render_projects_header(cx).into_any_element(),
                    thread_list,
                )
            }
            SidebarLayout::Flat => {
                let visible = flat_visible_threads(
                    &flat_sessions,
                    &self.collapsed_parents,
                    self.project_filter.as_deref(),
                    &flags,
                );
                if visible.is_empty() {
                    // An active project filter can empty the list while threads
                    // exist; that state gets its own hint, not the no-projects one.
                    let hint = if flat_sessions.is_empty() {
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
                    let top_offsets = flat_thread_top_offsets(&visible, &flat_sessions);
                    let visible = visible
                        .into_iter()
                        .cloned()
                        .zip(top_offsets)
                        .collect::<Vec<_>>();
                    if self.flat_list_state.item_count() != visible.len() {
                        self.flat_list_state.reset(visible.len());
                    }
                    let project_names = projects
                        .into_iter()
                        .map(|project| (project.id, project.name))
                        .collect::<HashMap<_, _>>();
                    let active_id = active_id.clone();
                    let thread_list =
                        list(
                            self.flat_list_state.clone(),
                            cx.processor(move |this, index: usize, _window, cx| {
                                let Some((meta, target_top)) = visible.get(index) else {
                                    return div().into_any_element();
                                };
                                let target_top = *target_top;
                                let project_name = meta
                                    .project_id
                                    .as_ref()
                                    .and_then(|project_id| project_names.get(project_id))
                                    .cloned();
                                let is_active = active_id.as_deref() == Some(meta.id.as_str());
                                let row = div().w_full().px_2().pb(px(2.)).child(
                                    this.render_flat_thread(
                                        meta,
                                        &flat_sessions,
                                        &flags,
                                        project_name,
                                        is_active,
                                        cx,
                                    ),
                                );
                                animate_flat_thread_position(row, &meta.id, target_top)
                                    .into_any_element()
                            }),
                        )
                        .flex_1()
                        .min_h_0()
                        .into_any_element();
                    (self.render_flat_header(cx).into_any_element(), thread_list)
                }
            }
        };

        v_flex()
            .size_full()
            // No seam against the content column: material contrast does the work.
            .bg(cx.theme().sidebar)
            .text_color(cx.theme().sidebar_foreground)
            .on_action(cx.listener(Self::on_rename))
            .on_action(cx.listener(Self::on_fork))
            .on_action(cx.listener(Self::on_merge_worktree))
            .on_action(cx.listener(Self::on_mark_unread))
            .on_action(cx.listener(Self::on_copy_path))
            .on_action(cx.listener(Self::on_copy_id))
            .on_action(cx.listener(Self::on_export_jsonl))
            .on_action(cx.listener(Self::on_export_markdown))
            .on_action(cx.listener(Self::on_archive))
            .on_action(cx.listener(Self::on_delete))
            .on_action(cx.listener(Self::on_project_archive_all))
            .on_action(cx.listener(Self::on_project_delete))
            .on_action(cx.listener(Self::on_project_reveal))
            .on_action(cx.listener(Self::on_filter_project))
            .on_action(cx.listener(Self::on_start_draft_for_project))
            .child(self.render_app_row(window, cx))
            .child(self.render_search_row(cx))
            .child(header)
            .child(thread_list)
            .child(self.render_footer(cx))
            .into_any_element()
    }
}

// ---------------------------------------------------------------------------
// Relative-time humanizer
// ---------------------------------------------------------------------------

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
                let target_top = index as f32 * FLAT_ROOT_ROW_HEIGHT;
                animate_flat_thread_position(
                    div()
                        .h(px(FLAT_ROOT_ROW_HEIGHT))
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

    fn thread_flags(entries: &[(&str, ThreadFlags)]) -> HashMap<String, ThreadFlags> {
        entries
            .iter()
            .map(|(id, flags)| ((*id).to_string(), *flags))
            .collect()
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
        assert_eq!(second_start - first_start, px(FLAT_ROOT_ROW_HEIGHT));

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
        let store = cx.new(|cx| WorkspaceStore::new_local(&host, cx));

        let window_state = cx.new(|_| WindowState::new(false));
        let sidebar = cx.new(|cx| SessionsSidebar::new(store, window_state.clone(), cx));
        let (_, cx) = cx.add_window_view(|_, _| WorkingThreadRowProbe);
        let cx: &mut VisualTestContext = cx;
        cx.update(|window, cx| {
            sidebar.update(cx, |sidebar, cx| {
                sidebar.on_rename(&ThreadRename(session_id.clone()), window, cx);
                let input = sidebar.renaming.as_ref().unwrap().input.clone();
                input.update(cx, |input, cx| input.set_value("Unsaved title", window, cx));
                sidebar.cancel_rename(cx);
            });
        });

        cx.update(|_, cx| {
            assert!(sidebar.read(cx).renaming.is_none());
        });
        let title =
            smol::block_on(host.update_state_for_test(|state, _| state.sessions[0].title.clone()))
                .expect("read host title");
        assert_eq!(title, "Original title");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn oversized_thread_list_keeps_its_toggle_after_expanding() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::set_locale(crate::LANGUAGE_SIMPLIFIED_CHINESE);

        assert_eq!(thread_list_toggle_label(6, false), None);
        assert_eq!(
            thread_list_toggle_label(7, false).as_deref(),
            Some("显示更多")
        );
        assert_eq!(thread_list_toggle_label(7, true).as_deref(), Some("收起"));
    }

    #[test]
    fn child_unread_is_suppressed_by_render_state_derivation() {
        let parent = session("parent", None);
        let child = session("child", Some("parent"));
        let sessions = vec![parent, child.clone()];

        let flags = thread_flags(&[(
            "child",
            ThreadFlags {
                unread: true,
                ..ThreadFlags::default()
            },
        )]);
        let state = derive_thread_render_state(&child, &sessions, &flags);

        assert!(state.is_child);
        assert!(!state.show_unread);

        let orphan = session("orphan-child", Some("missing-parent"));
        let flags = thread_flags(&[(
            "orphan-child",
            ThreadFlags {
                unread: true,
                ..ThreadFlags::default()
            },
        )]);
        let state = derive_thread_render_state(&orphan, std::slice::from_ref(&orphan), &flags);
        assert!(!state.is_child);
        assert!(!state.show_unread);
    }

    #[gpui::test]
    fn launch_runs_the_auto_archive_sweep_across_every_project(cx: &mut TestAppContext) {
        let root = std::env::temp_dir().join(format!(
            "tcode-sidebar-launch-sweep-test-{}",
            tcode_services::store::now_millis()
        ));
        let session_store = SessionStore::open_at(root.clone()).unwrap();
        let host = spawn_host(session_store, HostServices::default())
            .expect("spawn auto-archive test host");

        let project_a = Project::from_root(root.join("a"));
        let project_b = Project::from_root(root.join("b"));
        let mut sessions = Vec::new();
        for (project, prefix) in [(&project_a, "a"), (&project_b, "b")] {
            for i in 0..3u64 {
                let mut meta = session(&format!("{prefix}-{i}"), None);
                meta.project_id = Some(project.id.clone());
                // Ancient timestamps, newest last, so each project keeps
                // exactly its `keep_count = 1` most recent thread.
                meta.updated_at = 1 + i;
                sessions.push(meta);
            }
        }
        smol::block_on(host.update_state_for_test(move |state, _| {
            state.settings.auto_archive_keep_count = 1;
            state.settings.auto_archive_max_idle_days = 1;
            state.projects = vec![project_a, project_b];
            state.sessions = sessions;
        }))
        .expect("seed auto-archive host");
        let store = cx.new(|cx| WorkspaceStore::new_local(&host, cx));

        let window_state = cx.new(|_| WindowState::new(false));
        let sidebar = cx.new(|cx| SessionsSidebar::new(store, window_state.clone(), cx));
        cx.run_until_parked();

        let archived = smol::block_on(host.update_state_for_test(|state, _| {
            let mut archived: Vec<&str> = state
                .sessions
                .iter()
                .filter(|meta| meta.archived_at.is_some())
                .map(|meta| meta.id.as_str())
                .collect();
            archived.sort_unstable();
            archived.into_iter().map(str::to_string).collect::<Vec<_>>()
        }))
        .expect("read archived sessions");
        assert_eq!(archived, vec!["a-0", "a-1", "b-0", "b-1"]);
        sidebar.update(cx, |sidebar, _| {
            assert_eq!(
                sidebar.startup_archive_dialog,
                Some((4, 1, 1)),
                "first launch queues the explainer dialog with the total count"
            );
        });

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn startup_collapses_every_parent_except_the_active_chain() {
        let sessions = vec![
            session("parent-a", None),
            session("child-a", Some("parent-a")),
            session("parent-b", None),
            session("child-b", Some("parent-b")),
            session("grandchild-b", Some("child-b")),
            session("plain", None),
        ];

        let collapsed = initial_collapsed_parents(&sessions, None);
        assert_eq!(
            collapsed,
            HashSet::from([
                "parent-a".to_string(),
                "parent-b".to_string(),
                "child-b".to_string()
            ]),
            "with no selection, every parent starts folded and leaves do not"
        );

        let collapsed = initial_collapsed_parents(&sessions, Some("grandchild-b"));
        assert!(collapsed.contains("parent-a"));
        assert!(
            !collapsed.contains("parent-b") && !collapsed.contains("child-b"),
            "the selected thread's ancestor chain stays open"
        );

        let collapsed = initial_collapsed_parents(&sessions, Some("parent-a"));
        assert!(
            !collapsed.contains("parent-a"),
            "a selected parent keeps its own children visible"
        );
    }

    #[test]
    fn startup_fold_ignores_archived_children_and_orphan_parent_ids() {
        let mut archived_child = session("archived-child", Some("quiet-parent"));
        archived_child.archived_at = Some(1);
        let sessions = vec![
            session("quiet-parent", None),
            archived_child,
            session("orphan", Some("missing-parent")),
        ];

        let collapsed = initial_collapsed_parents(&sessions, None);
        assert!(
            !collapsed.contains("quiet-parent"),
            "archived children do not make their parent fold"
        );
        assert!(
            !collapsed.contains("missing-parent"),
            "a nonexistent parent id must never enter the fold set, or its \
             orphaned children would be hidden with no row to toggle"
        );
    }

    #[test]
    fn repeat_click_on_selected_parent_toggles_direct_child_rows() {
        let mut collapsed = HashSet::new();

        toggle_parent_for_row_click(&mut collapsed, "parent", false, true);
        assert!(!collapsed.contains("parent"), "first click only selects");

        toggle_parent_for_row_click(&mut collapsed, "parent", true, true);
        assert!(collapsed.contains("parent"));

        toggle_parent_for_row_click(&mut collapsed, "parent", true, true);
        assert!(!collapsed.contains("parent"), "repeat click restores rows");
    }

    #[test]
    fn active_direct_child_count_excludes_grandchildren() {
        let parent = session("parent", None);
        let child_working = session("child-working", Some("parent"));
        let child_idle = session("child-idle", Some("parent"));
        let grandchild_working = session("grandchild-working", Some("child-working"));
        let sessions = vec![
            parent.clone(),
            child_working,
            child_idle,
            grandchild_working,
        ];

        let flags = thread_flags(&[
            (
                "child-working",
                ThreadFlags {
                    working: true,
                    ..ThreadFlags::default()
                },
            ),
            (
                "grandchild-working",
                ThreadFlags {
                    working: true,
                    ..ThreadFlags::default()
                },
            ),
        ]);
        let state = derive_thread_render_state(&parent, &sessions, &flags);

        assert_eq!(state.direct_children, 2);
        assert_eq!(state.active_direct_children, 1);
    }

    #[test]
    fn collapsed_children_do_not_consume_thread_list_slots() {
        let mut sessions = vec![session("parent", None)];
        for i in 0..7 {
            sessions.push(session(&format!("child-{i}"), Some("parent")));
        }
        for i in 0..5 {
            sessions.push(session(&format!("thread-{i}"), None));
        }

        let collapsed = HashSet::from(["parent".to_string()]);
        let threads = visible_threads(&sessions, &collapsed);

        // Parent plus the five ordinary threads all fit inside the collapsed
        // limit once the hidden children stop counting toward it.
        assert_eq!(threads.len(), THREADS_COLLAPSED_LIMIT);
        assert!(threads.iter().all(|meta| meta.parent_session_id.is_none()));
        assert_eq!(
            thread_list_toggle_label(threads.len(), false),
            None,
            "no Show more row when every visible thread already fits"
        );

        // Expanding the parent brings the children back into the count.
        let expanded = visible_threads(&sessions, &HashSet::new());
        assert_eq!(expanded.len(), sessions.len());
    }

    #[test]
    fn collapsed_parent_hides_only_its_own_direct_children() {
        let collapsed = HashSet::from(["parent-a".to_string()]);

        assert!(!thread_visible(
            &session("child-a", Some("parent-a")),
            &collapsed
        ));
        assert!(thread_visible(
            &session("child-b", Some("parent-b")),
            &collapsed
        ));
        assert!(thread_visible(&session("parent-a", None), &collapsed));
    }

    #[test]
    fn flat_blocks_sort_by_attention_then_recency_and_keep_children_adjacent() {
        let mut waiting_root = session("waiting-root", None);
        waiting_root.updated_at = 10;
        let mut waiting_child = session("waiting-child", Some("waiting-root"));
        waiting_child.updated_at = 11;
        let mut working_root = session("working-root", None);
        working_root.updated_at = 40;
        let mut idle_new = session("idle-new", None);
        idle_new.updated_at = 30;
        let mut idle_old = session("idle-old", None);
        idle_old.updated_at = 20;
        let sessions = vec![
            working_root,
            waiting_root,
            waiting_child,
            idle_old,
            idle_new,
        ];

        let flags = thread_flags(&[
            (
                "waiting-root",
                ThreadFlags {
                    waiting_for_approval: true,
                    ..ThreadFlags::default()
                },
            ),
            (
                "working-root",
                ThreadFlags {
                    working: true,
                    ..ThreadFlags::default()
                },
            ),
        ]);
        let visible = flat_visible_threads(&sessions, &HashSet::new(), None, &flags);
        let ids: Vec<&str> = visible.iter().map(|meta| meta.id.as_str()).collect();

        assert_eq!(
            ids,
            vec![
                "waiting-root",
                "waiting-child",
                "working-root",
                "idle-new",
                "idle-old"
            ]
        );
    }

    #[test]
    fn flat_row_offsets_follow_the_rendered_root_and_child_heights() {
        let root_a = session("root-a", None);
        let child_a = session("child-a", Some("root-a"));
        let root_b = session("root-b", None);
        let sessions = vec![root_a, child_a, root_b];
        let visible = sessions.iter().collect::<Vec<_>>();

        assert_eq!(
            flat_thread_top_offsets(&visible, &sessions),
            vec![
                0.,
                FLAT_ROOT_ROW_HEIGHT,
                FLAT_ROOT_ROW_HEIGHT + FLAT_CHILD_ROW_HEIGHT
            ]
        );
    }

    #[test]
    fn flat_row_offsets_treat_orphaned_children_as_root_rows() {
        let orphan = session("orphan", Some("missing-parent"));
        let root = session("root", None);
        let sessions = vec![orphan, root];
        let visible = sessions.iter().collect::<Vec<_>>();

        assert_eq!(
            flat_thread_top_offsets(&visible, &sessions),
            vec![0., FLAT_ROOT_ROW_HEIGHT]
        );
    }

    #[test]
    fn flat_block_attention_is_lifted_from_a_waiting_child() {
        let mut lifted_root = session("lifted-root", None);
        lifted_root.updated_at = 1;
        let lifted_child = session("lifted-child", Some("lifted-root"));
        let mut working_root = session("working-root", None);
        working_root.updated_at = 100;
        let sessions = vec![working_root, lifted_root, lifted_child];

        let flags = thread_flags(&[
            (
                "lifted-child",
                ThreadFlags {
                    waiting_for_input: true,
                    ..ThreadFlags::default()
                },
            ),
            (
                "working-root",
                ThreadFlags {
                    working: true,
                    ..ThreadFlags::default()
                },
            ),
        ]);
        let visible = flat_visible_threads(&sessions, &HashSet::new(), None, &flags);
        let ids: Vec<&str> = visible.iter().map(|meta| meta.id.as_str()).collect();

        assert_eq!(ids, vec!["lifted-root", "lifted-child", "working-root"]);
    }

    #[test]
    fn flat_order_applies_the_shared_collapse_filter() {
        let sessions = vec![
            session("parent", None),
            session("child", Some("parent")),
            session("other", None),
        ];
        let collapsed = HashSet::from(["parent".to_string()]);

        let visible = flat_visible_threads(&sessions, &collapsed, None, &HashMap::new());
        let ids: Vec<&str> = visible.iter().map(|meta| meta.id.as_str()).collect();

        assert_eq!(ids, vec!["parent", "other"]);
    }

    #[test]
    fn flat_project_filter_keeps_only_matching_blocks_with_their_children() {
        let mut project_a_root = session("a-root", None);
        project_a_root.project_id = Some("project-a".into());
        let mut project_a_child = session("a-child", Some("a-root"));
        project_a_child.project_id = Some("project-a".into());
        let mut project_b_root = session("b-root", None);
        project_b_root.project_id = Some("project-b".into());
        let sessions = vec![project_b_root, project_a_root, project_a_child];

        let visible = flat_visible_threads(
            &sessions,
            &HashSet::new(),
            Some("project-a"),
            &HashMap::new(),
        );
        let ids: Vec<&str> = visible.iter().map(|meta| meta.id.as_str()).collect();

        assert_eq!(ids, vec!["a-root", "a-child"]);
    }
}
