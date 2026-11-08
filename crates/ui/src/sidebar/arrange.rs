//! Pinning and arranging threads. Rows of the desktop threads view drag within
//! and between the Pinned, Active and Settled sections; the phone's Arrange
//! threads page drags by handle; Move up / Move down write the same keys
//! without a drag. Rows of the project view, and every row in member scope,
//! are not arranged; on the desktop they still drag, onto the chat column's
//! panes.
//!
//! gpui-base has no sortable-list primitive (its dock drag is specific to tab
//! groups), so the drag composes GPUI's `on_drag`, `on_drag_move` and
//! `on_drop`, and GPUI does not cancel a drag on Escape by itself. The payload
//! is the dock's host-owned `AnyDrag`, the only kind its panes accept; the row
//! it carries is a [`drag_ghost`].

use super::*;
use crate::motion::drag_ghost;
use gpui::{DragMoveEvent, KeystrokeEvent, MouseButton, Pixels, WeakEntity, deferred};
use gpui_base::dock::AnyDrag;
use tcode_core::thread_sort::{order_key_between, plan_reorder, thread_section};

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

/// A `semantic_chip`'s height: one `xs` line and its 1pt insets.
const BOUNDARY_LABEL_HEIGHT: f32 = 18.;
const EMPTY_TARGET_HEIGHT: f32 = 36.;
const ARRANGE_HANDLE_SIZE: f32 = 44.;

/// The drag payload, carried as an `AnyDrag`; the sidebar keeps the drag's
/// state in [`ThreadDrag`].
#[derive(Clone)]
pub(super) struct DraggedThread {
    thread: ThreadKey,
    /// The thread's name in element ids ([`named`]).
    name: String,
    /// Whether the drag can move the row within the list.
    arranges: bool,
    from: ThreadSection,
    scope: Option<ProjectKey>,
    title: SharedString,
    can_settle: bool,
    compact: bool,
}

/// The thread `item` carries, when a threads list row started it.
pub(crate) fn thread_carried(item: &AnyDrag) -> Option<&ThreadKey> {
    item.value()
        .downcast_ref::<DraggedThread>()
        .map(|dragged| &dragged.thread)
}

pub(super) struct ThreadDrag {
    pub(super) thread: ThreadKey,
    arranges: bool,
    from: ThreadSection,
    /// The project the drag stays inside; `None` is every project.
    scope: Option<ProjectKey>,
    /// Where the thread lands in Pinned or Active, as an index among that
    /// section's other rows; `None` leaves it where it was.
    gap: Option<(ThreadSection, usize)>,
    /// The other rows of each section inside `scope` on the thread's host
    /// as the drag began, which gap indexes count.
    others: ThreadSections<String>,
    over: Option<ThreadSection>,
    /// Where the drag began, which it returns to once it leaves the list.
    home: (Option<(ThreadSection, usize)>, Option<ThreadSection>),
    can_settle: bool,
    auto_scroll: gpui_base::AutoScroll,
    _escape: Subscription,
}

/// Where a drop put a thread, which the list shows until the store lists it
/// there: the host takes the move a round trip after the drop, and the
/// store lists the thread where it was until then, which would send its
/// row back there and on again.
pub(super) struct Landing {
    pub(super) thread: ThreadKey,
    from: ThreadSection,
    scope: Option<ProjectKey>,
    gap: (ThreadSection, usize),
    /// The store's thread list revision when the host took the move; the
    /// first list after it is the host's word on where the thread is.
    taken: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum DragAction {
    Pin,
    Unpin,
    Settle,
    Unsettle,
}

impl DragAction {
    pub(super) fn icon(self) -> IconName {
        match self {
            Self::Pin => IconName::Pin,
            Self::Unpin => IconName::PinOff,
            Self::Settle => IconName::CircleCheck,
            Self::Unsettle => IconName::Undo2,
        }
    }

    pub(super) fn label(self) -> Cow<'static, str> {
        match self {
            Self::Pin => crate::tr!("sidebar.pin"),
            Self::Unpin => crate::tr!("sidebar.unpin"),
            Self::Settle => crate::tr!("sidebar.settle"),
            Self::Unsettle => crate::tr!("sidebar.unsettle"),
        }
    }
}

/// What a row, a section boundary or the Settled shelf is to a drag over it.
#[derive(Clone)]
pub(super) enum DropZone {
    Row {
        thread: ThreadKey,
        section: ThreadSection,
    },
    /// The start of Pinned or Active: its label, or its empty target.
    Start(ThreadSection),
    Settled,
}

impl ThreadDrag {
    fn action(&self) -> Option<DragAction> {
        use ThreadSection::*;
        match (self.from, self.over?) {
            (Settled, Settled) => None,
            (_, Settled) => self.can_settle.then_some(DragAction::Settle),
            (Pinned, Pinned) | (Active, Active) => None,
            (_, Pinned) => Some(DragAction::Pin),
            (Pinned, Active) => Some(DragAction::Unpin),
            (Settled, Active) => Some(DragAction::Unsettle),
        }
    }

    /// `host`'s sections as they would be after the drop, when the drag is
    /// over a list showing `scope`.
    fn preview<'a>(
        &self,
        host: &HostKey,
        scope: Option<&ProjectKey>,
        sections: ThreadSections<&'a SessionMeta>,
    ) -> ThreadSections<&'a SessionMeta> {
        match self
            .gap
            .filter(|_| self.thread.host == *host && self.scope.as_ref() == scope)
        {
            Some(gap) => moved(sections, &self.thread.session_id, self.from, gap),
            None => sections,
        }
    }

    /// The section a list showing `scope` marks as the drop target.
    pub(super) fn target(&self, scope: Option<&ProjectKey>) -> Option<ThreadSection> {
        self.over.filter(|over| {
            self.scope.as_ref() == scope && (*over != ThreadSection::Settled || self.can_settle)
        })
    }
}

/// `sections` with thread `id` taken out of `from` and put at `index` among
/// the other rows of `section`.
fn moved<'a>(
    mut sections: ThreadSections<&'a SessionMeta>,
    id: &str,
    from: ThreadSection,
    (section, index): (ThreadSection, usize),
) -> ThreadSections<&'a SessionMeta> {
    let rows = sections.section_mut(from);
    let Some(position) = rows.iter().position(|meta| meta.id == id) else {
        return sections;
    };
    let meta = rows.remove(position);
    let to = sections.section_mut(section);
    to.insert(index.min(to.len()), meta);
    sections
}

/// The slot a thread's row lifts from and settles into, in whichever list
/// shows it, by the thread's [`named`] name.
pub(super) fn thread_slot(name: &str) -> SharedString {
    format!("thread-{name}").into()
}

/// The row a drag carries: its title, and what dropping it where it is would
/// do.
fn carried_thread(
    sidebar: &WeakEntity<SessionsSidebar>,
    title: &SharedString,
    compact: bool,
    lift: f32,
    window: &mut Window,
    cx: &mut App,
) -> gpui::AnyElement {
    let action = sidebar
        .upgrade()
        .and_then(|sidebar| sidebar.read(cx).drag.as_ref()?.action());
    if compact {
        // The lifted `list_row` a finger carries: its height and inset,
        // floated at the overlay radius.
        let badge = crate::motion::cross_fade_part(
            "thread-drag-action",
            action,
            CHANGE,
            window,
            cx,
            |action, _, cx| {
                div()
                    .text_secondary(true, cx)
                    .text_color(cx.theme().primary)
                    .children(action.map(DragAction::label))
                    .into_any_element()
            },
        );
        return drag_ghost::lifted(v_flex(), lift)
            .w_full()
            .min_h(px(crate::material::LIST_ROW_MIN_HEIGHT))
            .justify_center()
            .px(px(COMPACT_PAGE_INSET))
            .py(cx.theme().tokens.spacing.sm)
            .rounded(crate::material::radius_overlay(cx))
            .bg(cx.theme().list_active)
            .child(
                div()
                    .text_body(true, cx)
                    .line_clamp(2)
                    .text_color(cx.theme().foreground)
                    .child(title.clone()),
            )
            .child(badge)
            .into_any_element();
    }
    let badge = crate::motion::cross_fade_part(
        "thread-drag-action",
        action,
        CHANGE,
        window,
        cx,
        |action, _, cx| {
            let Some(action) = *action else {
                return div().into_any_element();
            };
            h_flex()
                .id("thread-drag-badge")
                .role(Role::Status)
                .aria_label(action.label())
                // A bordered status badge with a leading icon:
                // `semantic_chip` takes neither.
                .flex_none()
                .h(px(20.))
                .px(px(6.))
                .gap_1()
                .items_center()
                .rounded(crate::material::radius_chip(cx))
                .border_1()
                .border_color(cx.theme().primary.opacity(0.4))
                .bg(cx.theme().primary.opacity(0.1))
                .text_token(&cx.theme().tokens.typography.xs)
                .font_medium()
                .text_color(cx.theme().primary)
                .child(Icon::new(action.icon()))
                .child(action.label())
                .into_any_element()
        },
    );
    drag_ghost::lifted(
        crate::material::selectable_row_two_line(h_flex(), true, false, cx),
        lift,
    )
    .w_full()
    .text_color(cx.theme().sidebar_foreground)
    .child(sidebar_label(title.clone()).text_body(false, cx))
    .child(div().flex_none().child(badge))
    .into_any_element()
}

impl SessionsSidebar {
    /// Whether rows are listed by project, where they are neither dragged
    /// nor moved: the project view orders them by activity alone.
    fn in_project_view(&self, cx: &App) -> bool {
        crate::device::DevicePreferences::sidebar_layout(cx) == SidebarLayout::Grouped
            && self.window_state.read(cx).destination() != Destination::ArrangeThreads
    }

    /// The project a list shows a thread under: the filter in the desktop's
    /// flat list, every project otherwise.
    pub(super) fn list_scope(&self, cx: &App) -> Option<ProjectKey> {
        if self.window_state.read(cx).destination() == Destination::ArrangeThreads {
            self.arrange_scope.clone()
        } else if self.compact(cx) {
            None
        } else {
            self.project_filter.clone()
        }
    }

    /// Every thread `host` lists, by section, in display order, and whether
    /// each is inside `scope`.
    fn scoped_sections(
        &self,
        host: &HostKey,
        scope: Option<&ProjectKey>,
        cx: &App,
    ) -> ThreadSections<(SessionMeta, bool)> {
        let sessions = self
            .store_of(host)
            .map(|store| store.read(cx).flat_sessions())
            .unwrap_or_default();
        let sections = partition_threads(&sessions);
        let project = host_scope(scope, host);
        let scoped = |rows: &Vec<&SessionMeta>| {
            rows.iter()
                .map(|meta| {
                    let inside = project.is_some_and(|project| {
                        project.is_none_or(|id| meta.project_id.as_deref() == Some(id))
                    });
                    ((*meta).clone(), inside)
                })
                .collect()
        };
        ThreadSections {
            pinned: scoped(&sections.pinned),
            active: scoped(&sections.active),
            settled: scoped(&sections.settled),
        }
    }

    /// The payload a row starts a drag with, or `None` while the row is
    /// being renamed.
    pub(super) fn dragged_thread(
        &self,
        meta: &SessionMeta,
        state: &ThreadRowState,
        cx: &App,
    ) -> Option<DraggedThread> {
        let store = self.store_of(&state.thread.host)?;
        let store = store.read(cx);
        state.renaming.is_none().then(|| DraggedThread {
            thread: state.thread.clone(),
            name: state.name.clone(),
            arranges: store.scope().is_full() && !self.in_project_view(cx),
            from: thread_section(meta),
            scope: self.list_scope(cx),
            title: meta.title.clone().into(),
            can_settle: !store.turn_running_for(&meta.id)
                && !store.pending_approval_for(&meta.id)
                && !store.pending_user_input_for(&meta.id),
            compact: self.compact(cx),
        })
    }

    /// Make `element` start a drag of `dragged`.
    pub(super) fn drag_source<E: gpui::StatefulInteractiveElement>(
        element: E,
        dragged: DraggedThread,
        cx: &Context<Self>,
    ) -> E {
        let sidebar = cx.entity().downgrade();
        element.on_drag(AnyDrag::new(dragged.clone()), move |_, grab, window, cx| {
            Self::begin_drag(sidebar.clone(), &dragged, grab, window, cx)
        })
    }

    /// `host`'s `sections` of a list showing `scope` as a drag over it
    /// would leave them, or as a drop the store has yet to list left them.
    pub(super) fn arranged<'a>(
        &self,
        host: &HostKey,
        scope: Option<&ProjectKey>,
        sections: ThreadSections<&'a SessionMeta>,
    ) -> ThreadSections<&'a SessionMeta> {
        if let Some(drag) = &self.drag {
            return drag.preview(host, scope, sections);
        }
        match self
            .landing
            .as_ref()
            .filter(|landing| landing.thread.host == *host && landing.scope.as_ref() == scope)
        {
            Some(landing) => moved(
                sections,
                &landing.thread.session_id,
                landing.from,
                landing.gap,
            ),
            None => sections,
        }
    }

    /// `host` answered the lifecycle commands for `sessions`: a drop of
    /// one of them it took is listed where it landed from the next thread
    /// list on; one it refused goes back.
    pub(super) fn lifecycle_answered(
        &mut self,
        host: &HostKey,
        sessions: &[String],
        taken: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.landing.as_ref().is_some_and(|landing| {
            landing.thread.host == *host && sessions.contains(&landing.thread.session_id)
        }) {
            return;
        }
        match self.store_of(host).filter(|_| taken) {
            Some(store) => {
                let revision = store.read(cx).thread_list_revision();
                self.landing.as_mut().expect("checked above").taken = Some(revision);
            }
            None => self.landing = None,
        }
        cx.notify();
    }

    /// The thread a drag in progress carries.
    pub(crate) fn dragged_session(&self) -> Option<&ThreadKey> {
        self.drag.as_ref().map(|drag| &drag.thread)
    }

    fn begin_drag(
        sidebar: WeakEntity<Self>,
        dragged: &DraggedThread,
        grab: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<drag_ghost::Carried> {
        let escape = {
            let sidebar = sidebar.clone();
            cx.intercept_keystrokes(move |event: &KeystrokeEvent, window, cx| {
                if event.keystroke.key == "escape"
                    && sidebar
                        .update(cx, |this, cx| this.cancel_drag(window, cx))
                        .unwrap_or(false)
                {
                    cx.stop_propagation();
                }
            })
        };
        let _ = sidebar.update(cx, |this, cx| {
            this.landing = None;
            let thread = &dragged.thread;
            let sections = this.scoped_sections(&thread.host, dragged.scope.as_ref(), cx);
            let gap = (dragged.arranges && dragged.from != ThreadSection::Settled)
                .then(|| {
                    sections
                        .section(dragged.from)
                        .iter()
                        .filter(|(_, inside)| *inside)
                        .position(|(meta, _)| meta.id == thread.session_id)
                        .map(|index| (dragged.from, index))
                })
                .flatten();
            let others = |section| {
                sections
                    .section(section)
                    .iter()
                    .filter(|(meta, inside)| *inside && meta.id != thread.session_id)
                    .map(|(meta, _)| meta.id.clone())
                    .collect()
            };
            let over = dragged.arranges.then_some(dragged.from);
            this.drag = Some(ThreadDrag {
                thread: thread.clone(),
                arranges: dragged.arranges,
                from: dragged.from,
                scope: dragged.scope.clone(),
                gap,
                others: ThreadSections {
                    pinned: others(ThreadSection::Pinned),
                    active: others(ThreadSection::Active),
                    settled: vec![],
                },
                over,
                home: (gap, over),
                can_settle: dragged.can_settle,
                auto_scroll: Default::default(),
                _escape: escape,
            });
            this.compact_model_dirty = true;
            cx.notify();
        });
        crate::haptics::threshold();
        let (title, compact) = (dragged.title.clone(), dragged.compact);
        drag_ghost::lift(
            thread_slot(&dragged.name),
            grab,
            move |lift, window, cx| carried_thread(&sidebar, &title, compact, lift, window, cx),
            window,
            cx,
        )
    }

    /// Escape: the row returns to its place.
    fn cancel_drag(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.drag.take().is_none() {
            return false;
        }
        cx.stop_active_drag(window);
        cx.notify();
        true
    }

    /// A drag released outside every drop target ends without a drop; a
    /// drop's landing ends once the store lists the thread where it landed,
    /// or lists the threads again after the host took the move.
    pub(super) fn clear_finished_drag(&mut self, cx: &App) {
        if self.drag.is_some() && !cx.has_active_drag() {
            self.drag = None;
        }
        let Some(host) = self
            .landing
            .as_ref()
            .map(|landing| landing.thread.host.clone())
        else {
            return;
        };
        let lists = self.thread_lists(cx);
        let (Some(of), Some(store)) = (
            lists.iter().find(|lists| lists.host == host),
            self.store_of(&host),
        ) else {
            self.landing = None;
            return;
        };
        let revision = store.read(cx).thread_list_revision();
        let landing = self.landing.as_ref().expect("checked above");
        let (section, index) = landing.gap;
        let listed = host_scope(landing.scope.as_ref(), &host).is_some_and(|project| {
            project_threads(&of.flat, project)
                .section(section)
                .iter()
                .position(|meta| meta.id == landing.thread.session_id)
                == Some(index)
        });
        if listed || landing.taken.is_some_and(|taken| taken != revision) {
            self.landing = None;
        }
    }

    /// Track the pointer over `element`, the unanimated slot of `zone`, so a
    /// row sliding through the pointer never moves the gap back.
    pub(super) fn drop_zone(
        &self,
        element: gpui::Div,
        zone: DropZone,
        scope: Option<ProjectKey>,
        cx: &Context<Self>,
    ) -> gpui::Div {
        element.on_drag_move::<AnyDrag>(cx.listener(
            move |this, event: &DragMoveEvent<AnyDrag>, _, cx| {
                if event.bounds.contains(&event.event.position) {
                    let lower = event.event.position.y > event.bounds.center().y;
                    this.drag_over(&zone, scope.as_ref(), lower, cx);
                }
            },
        ))
    }

    fn drag_over(
        &mut self,
        zone: &DropZone,
        scope: Option<&ProjectKey>,
        lower: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(dragged) = self
            .drag
            .as_ref()
            .filter(|drag| drag.arranges)
            .map(|drag| drag.thread.clone())
        else {
            return;
        };
        // A row of another host, which the list interleaves by time: the
        // thread lands among its own host's rows, after those drawn above.
        let among_own = match zone {
            DropZone::Row { thread, section }
                if thread.host != dragged.host && *section != ThreadSection::Settled =>
            {
                let lists = self.thread_lists(cx);
                let rows = self.thread_rows(&lists, scope, &Shelf::Recent);
                let rows = match section {
                    ThreadSection::Pinned => rows.pinned,
                    _ => rows.active,
                };
                Some(
                    rows.iter()
                        .take_while(|(host, meta)| {
                            **host != thread.host || meta.id != thread.session_id
                        })
                        .filter(|(host, meta)| {
                            **host == dragged.host && meta.id != dragged.session_id
                        })
                        .count(),
                )
            }
            _ => None,
        };
        let drag = self.drag.as_ref().expect("checked above");
        let (mut gap, mut over) = (drag.gap, drag.over);
        if drag.scope.as_ref() != scope {
            over = None;
        } else {
            match zone {
                DropZone::Settled
                | DropZone::Row {
                    section: ThreadSection::Settled,
                    ..
                } => over = Some(ThreadSection::Settled),
                DropZone::Start(section) => {
                    gap = Some((*section, 0));
                    over = Some(*section);
                }
                DropZone::Row { thread, .. } if *thread == drag.thread => {
                    over = Some(gap.map_or(drag.from, |(section, _)| section));
                }
                DropZone::Row { thread, section } => {
                    let index = match among_own {
                        Some(index) => Some(index),
                        None => drag
                            .others
                            .section(*section)
                            .iter()
                            .position(|other| *other == thread.session_id)
                            .map(|index| index + usize::from(lower)),
                    };
                    if let Some(index) = index {
                        gap = Some((*section, index));
                        over = Some(*section);
                    }
                }
            }
        }
        let drag = self.drag.as_mut().expect("checked above");
        if (drag.gap, drag.over) != (gap, over) {
            crate::haptics::selection();
            drag.gap = gap;
            drag.over = over;
            self.compact_model_dirty = true;
            cx.notify();
        }
    }

    /// A drag carried out of the list, toward the chat column's panes, puts
    /// the row back where it began until it returns.
    pub(super) fn drag_left_list(
        &mut self,
        event: &DragMoveEvent<AnyDrag>,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.drag.as_mut() else {
            return;
        };
        if event.bounds.contains(&event.event.position) || (drag.gap, drag.over) == drag.home {
            return;
        }
        (drag.gap, drag.over) = drag.home;
        self.compact_model_dirty = true;
        cx.notify();
    }

    /// Scroll the list under a drag held near its top or bottom edge.
    pub(super) fn drag_auto_scroll(
        &mut self,
        event: &DragMoveEvent<AnyDrag>,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.drag.as_mut() else {
            return;
        };
        let position = event.event.position;
        let delta = (event.bounds.left() <= position.x && position.x <= event.bounds.right())
            .then(|| gpui_base::AutoScroll::compute_delta(position.y, event.bounds))
            .flatten();
        drag.auto_scroll.set(delta, cx, |delta, this, cx| {
            if let Some(list) = this.drag_list(cx) {
                list.scroll_by(delta);
                cx.notify();
            }
        });
    }

    /// The list a drag in progress scrolls: the Arrange threads page's, or
    /// else the threads view's.
    fn drag_list(&self, cx: &App) -> Option<ListState> {
        self.drag.as_ref()?;
        Some(
            if self.window_state.read(cx).destination() == Destination::ArrangeThreads {
                self.arrange_list_state.clone()
            } else {
                self.flat_list_state.clone()
            },
        )
    }

    pub(super) fn drop_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        cx.notify();
        let gap = match (drag.over, drag.gap) {
            (Some(ThreadSection::Settled), _) => {
                (drag.action() == Some(DragAction::Settle)).then(|| {
                    self.settle_thread(&drag.thread, window, cx);
                    // The shelf lists the newest settled first.
                    (ThreadSection::Settled, 0)
                })
            }
            (Some(over), Some((section, index))) if over == section => self
                .place_thread(
                    &drag.thread,
                    section,
                    index,
                    drag.scope.as_ref(),
                    window,
                    cx,
                )
                .then_some((section, index)),
            _ => None,
        };
        self.landing = gap.map(|gap| Landing {
            thread: drag.thread,
            from: drag.from,
            scope: drag.scope,
            gap,
            taken: None,
        });
    }

    /// Put a thread at `index` among the other rows of `to` that `scope`
    /// shows, pinning, unpinning or un-settling it on the way. Only the moved
    /// thread's key is written unless its neighbours need fresh keys; rows
    /// outside `scope` keep theirs. Returns whether it asked the host to
    /// move the thread.
    fn place_thread(
        &mut self,
        thread: &ThreadKey,
        to: ThreadSection,
        index: usize,
        scope: Option<&ProjectKey>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let id = thread.session_id.as_str();
        let Some(meta) = self
            .store_of(&thread.host)
            .and_then(|store| store.read(cx).thread_meta(id).cloned())
        else {
            return false;
        };
        let from = thread_section(&meta);
        let key = |meta: &SessionMeta| match to {
            ThreadSection::Pinned => meta.pin_order.clone(),
            _ => meta.active_order.clone(),
        };
        let sections = self.scoped_sections(&thread.host, scope, cx);
        let rows = sections.section(to);
        if from == to
            && rows
                .iter()
                .filter(|(_, inside)| *inside)
                .position(|(meta, _)| meta.id == id)
                == Some(index)
        {
            return false;
        }
        let others: Vec<_> = rows.iter().filter(|(meta, _)| meta.id != id).collect();
        let mut ordered: Vec<(String, Option<String>)> = others
            .iter()
            .filter(|(_, inside)| *inside)
            .map(|(meta, _)| (meta.id.clone(), key(meta)))
            .collect();
        ordered.insert(index.min(ordered.len()), (id.to_owned(), key(&meta)));
        let hidden: Vec<String> = others
            .iter()
            .filter(|(_, inside)| !*inside)
            .filter_map(|(meta, _)| key(meta))
            .collect();
        let ordered: Vec<_> = ordered
            .iter()
            .map(|(id, key)| (id.as_str(), key.as_deref()))
            .collect();
        let hidden: Vec<_> = hidden.iter().map(String::as_str).collect();
        let reorder = |session_id: String, order_key: String| match to {
            ThreadSection::Pinned => Command::ReorderPinned {
                session_id,
                order_key,
            },
            _ => Command::ReorderActive {
                session_id,
                order_key,
            },
        };
        let mut moved_key = None;
        let mut neighbours = vec![];
        for (session_id, order_key) in plan_reorder(&ordered, &hidden, id) {
            if session_id == id {
                moved_key = Some(order_key);
            } else {
                neighbours.push(reorder(session_id, order_key));
            }
        }
        let session_id = id.to_owned();
        let (mut commands, undo) = match (from, to) {
            (ThreadSection::Pinned, ThreadSection::Pinned)
            | (ThreadSection::Active, ThreadSection::Active) => (
                moved_key
                    .map(|key| reorder(session_id.clone(), key))
                    .into_iter()
                    .collect(),
                None,
            ),
            (_, ThreadSection::Pinned) => (
                vec![Command::PinSession {
                    session_id,
                    order_key: moved_key,
                }],
                None,
            ),
            (from, _) => {
                let first = if from == ThreadSection::Pinned {
                    Command::UnpinSession {
                        session_id: session_id.clone(),
                    }
                } else {
                    Command::UnsettleSession {
                        session_id: session_id.clone(),
                    }
                };
                let undo = (from == ThreadSection::Pinned).then(|| {
                    (
                        UndoKind::Unpin,
                        vec![Command::PinSession {
                            session_id: session_id.clone(),
                            order_key: meta.pin_order.clone(),
                        }],
                    )
                });
                (
                    std::iter::once(first)
                        .chain(moved_key.map(|key| reorder(session_id, key)))
                        .collect(),
                    undo,
                )
            }
        };
        commands.extend(neighbours);
        if commands.is_empty() {
            return false;
        }
        self.perform_lifecycle(&thread.host, commands, undo, window, cx);
        true
    }

    fn thread_meta(&self, thread: &ThreadKey, cx: &App) -> Option<SessionMeta> {
        let store = self.store_of(&thread.host)?;
        store.read(cx).thread_meta(&thread.session_id).cloned()
    }

    /// Whether a thread can move up and down within its section, or `None`
    /// for a settled thread and in the project view.
    pub(super) fn move_bounds(&self, thread: &ThreadKey, cx: &App) -> Option<(bool, bool)> {
        let meta = self.thread_meta(thread, cx)?;
        let section = thread_section(&meta);
        if section == ThreadSection::Settled || self.in_project_view(cx) {
            return None;
        }
        let scope = self.list_scope(cx);
        let sections = self.scoped_sections(&thread.host, scope.as_ref(), cx);
        let rows: Vec<_> = sections
            .section(section)
            .iter()
            .filter(|(_, inside)| *inside)
            .collect();
        let index = rows
            .iter()
            .position(|(other, _)| other.id == thread.session_id)?;
        Some((index > 0, index + 1 < rows.len()))
    }

    pub(super) fn on_move(
        &mut self,
        action: &ThreadMove,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let thread = &action.0;
        let Some(meta) = self.thread_meta(thread, cx) else {
            return;
        };
        let section = thread_section(&meta);
        if section == ThreadSection::Settled {
            return;
        }
        let scope = self.list_scope(cx);
        let sections = self.scoped_sections(&thread.host, scope.as_ref(), cx);
        let Some(index) = sections
            .section(section)
            .iter()
            .filter(|(_, inside)| *inside)
            .position(|(other, _)| other.id == meta.id)
        else {
            return;
        };
        let Some(index) = (if action.1 {
            index.checked_add(1)
        } else {
            index.checked_sub(1)
        }) else {
            return;
        };
        self.place_thread(thread, section, index, scope.as_ref(), window, cx);
    }

    /// A pin from the menu leads its host's pinned rows.
    pub(super) fn on_pin(
        &mut self,
        action: &ThreadPin,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let thread = &action.0;
        let Some(store) = self.store_of(&thread.host) else {
            return;
        };
        let first = store
            .read(cx)
            .flat_sessions()
            .into_iter()
            .filter(|meta| thread_section(meta) == ThreadSection::Pinned)
            .filter_map(|meta| meta.pin_order)
            .min();
        if crate::device::DevicePreferences::sidebar_layout(cx) == SidebarLayout::Flat {
            self.reveal_pinned = Some(thread.clone());
        }
        self.perform_lifecycle(
            &thread.host,
            vec![Command::PinSession {
                session_id: thread.session_id.clone(),
                order_key: order_key_between(None, first.as_deref()),
            }],
            None,
            window,
            cx,
        );
    }

    pub(super) fn on_unpin(
        &mut self,
        action: &ThreadUnpin,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.unpin_thread(&action.0, window, cx);
    }

    pub(super) fn unpin_thread(
        &mut self,
        thread: &ThreadKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(meta) = self.thread_meta(thread, cx) else {
            return;
        };
        let id = thread.session_id.as_str();
        self.perform_lifecycle(
            &thread.host,
            vec![Command::UnpinSession {
                session_id: id.to_owned(),
            }],
            Some((
                UndoKind::Unpin,
                vec![Command::PinSession {
                    session_id: id.to_owned(),
                    order_key: meta.pin_order,
                }],
            )),
            window,
            cx,
        );
    }

    /// The Arrange threads page shows the opening thread's project in
    /// Grouped and every thread in Flat.
    pub(super) fn on_arrange(
        &mut self,
        action: &ThreadArrange,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.arrange_scope = (crate::device::DevicePreferences::sidebar_layout(cx)
            == SidebarLayout::Grouped)
            .then(|| self.thread_meta(&action.0, cx))
            .flatten()
            .and_then(|meta| meta.project_id)
            .map(|project_id| ProjectKey {
                host: action.0.host.clone(),
                project_id,
            });
        self.arrange_settled_expanded = false;
        self.window_state
            .update(cx, |state, cx| state.go(Destination::ArrangeThreads, cx));
    }

    /// While a drag is in progress, the label at the start of Pinned or
    /// Active, overlaying the rows so they never move, or the target that
    /// stands in for an empty section. Nothing at rest.
    pub(super) fn render_drag_boundary(
        &self,
        section: ThreadSection,
        empty: bool,
        scope: Option<ProjectKey>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let drag = self.drag.as_ref().filter(|drag| drag.scope == scope);
        let targeted = drag.map(|drag| drag.target(scope.as_ref()) == Some(section));
        let id = SharedString::from(format!("drag-boundary-{section:?}"));
        // The seam's label fades in as a drag starts and out as it ends, and
        // lights up while the drag targets its section; an empty section's
        // target folds open in its place instead.
        let shown = crate::motion::shown(id.clone(), targeted.filter(|_| !empty), SHOW, window, cx);
        let target = crate::motion::shown(
            SharedString::from(format!("drag-boundary-target-{section:?}")),
            targeted.filter(|_| empty).map(|_| ()),
            SHOW,
            window,
            cx,
        );
        let lit = transition(
            (id, "lit"),
            if targeted == Some(true) { 1. } else { 0. },
            gpui_base::motion::Transition::new(GLIDE),
            window,
            cx,
        );
        let seam = shown.map(|shown| self.render_seam_label(section, lit, shown, cx));
        let Some(target) = target else {
            return seam.unwrap_or_else(|| div().into_any_element());
        };
        let color = gpui_base::animation::Lerp::lerp(
            &cx.theme().sidebar_foreground,
            &cx.theme().primary,
            lit,
        );
        let label = match section {
            ThreadSection::Pinned => crate::tr!("sidebar.pinned"),
            _ => crate::tr!("sidebar.active"),
        };
        let target_row = div().w_full().py_1().child(
            h_flex()
                .h(px(EMPTY_TARGET_HEIGHT - 8.))
                .px_2()
                .items_center()
                .rounded(crate::material::radius_chip(cx))
                .border_1()
                .border_dashed()
                .border_color(cx.theme().primary)
                .text_caption(cx)
                .text_color(color)
                .child(label),
        );
        div()
            .w_full()
            .child(self.drop_zone(
                div().w_full().child(target.fold(target_row)),
                DropZone::Start(section),
                scope,
                cx,
            ))
            .children(seam)
            .into_any_element()
    }

    /// The label on the seam at the start of a non-empty section, lit as far
    /// as `lit` toward the drag-target colour.
    fn render_seam_label(
        &self,
        section: ThreadSection,
        lit: f32,
        shown: crate::motion::Shown<bool>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let label = match section {
            ThreadSection::Pinned => crate::tr!("sidebar.pinned"),
            _ => crate::tr!("sidebar.active"),
        };
        let theme = cx.theme();
        let rule =
            gpui_base::animation::Lerp::lerp(&theme.border, &theme.primary.opacity(0.5), lit);
        let text = gpui_base::animation::Lerp::lerp(&theme.muted_foreground, &theme.primary, lit);
        // Deferred so the label paints after the rows on both sides of the
        // seam, centered on the seam rather than on a row's title line. The
        // sidebar background is transparent, so the hairline stops at the pill
        // instead of running under it.
        div()
            .relative()
            .w_full()
            .h_0()
            .child(deferred(
                div()
                    .absolute()
                    .top(px(-BOUNDARY_LABEL_HEIGHT / 2.))
                    .left_0()
                    .right_0()
                    .child(
                        shown.fade(
                            h_flex()
                                .w_full()
                                .h(px(BOUNDARY_LABEL_HEIGHT))
                                .gap_1()
                                .items_center()
                                .child(div().flex_none().w_2().h(px(1.)).bg(rule))
                                .child(crate::material::semantic_chip(label, theme.muted, text, cx))
                                .child(div().flex_1().h(px(1.)).bg(rule)),
                        ),
                    ),
            ))
            .into_any_element()
    }

    pub(crate) fn render_arrange(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        self.clear_finished_drag(cx);
        let scope = self.arrange_scope.clone();
        let lists = self.thread_lists(cx);
        let sections = merged(
            lists
                .iter()
                .filter_map(|lists| {
                    let project = host_scope(scope.as_ref(), &lists.host)?;
                    let host = self.arranged(
                        &lists.host,
                        scope.as_ref(),
                        project_threads(&lists.flat, project),
                    );
                    Some(tagged_sections(&lists.host, host))
                })
                .collect(),
        );
        let tag = |rows: Vec<Listed>| -> Vec<ListedThread> {
            rows.into_iter().map(|row| self.listed(row)).collect()
        };
        let sections = ThreadSections {
            pinned: tag(sections.pinned),
            active: tag(sections.active),
            settled: tag(sections.settled),
        };
        // The empty-Pinned row stays in the list, folding in and out with
        // the last pinned thread.
        let mut rows: Vec<ArrangeRow> = vec![
            ArrangeRow::Caption(ThreadSection::Pinned, sections.pinned.len()),
            ArrangeRow::PinnedEmpty(sections.pinned.is_empty()),
        ];
        rows.extend(
            sections
                .pinned
                .iter()
                .map(|row| ArrangeRow::Thread(Box::new(row.clone()))),
        );
        rows.push(ArrangeRow::Caption(
            ThreadSection::Active,
            sections.active.len(),
        ));
        rows.extend(
            sections
                .active
                .iter()
                .map(|row| ArrangeRow::Thread(Box::new(row.clone()))),
        );
        let open = self.arrange_settled_expanded;
        let settled_fold = transition(
            ("arrange-settled", "fold"),
            if open { 1. } else { 0. },
            gpui_base::motion::Transition::new(GLIDE),
            window,
            cx,
        );
        let mut folds = vec![1.; rows.len()];
        if !sections.settled.is_empty() {
            rows.push(ArrangeRow::Settled(sections.settled.len()));
            folds.push(1.);
            if open || settled_fold > 0. {
                let count = sections.settled.len();
                for (index, row) in sections.settled.iter().enumerate() {
                    let factor = fold_factor(settled_fold, index, count);
                    if factor >= 0. {
                        rows.push(ArrangeRow::Thread(Box::new(row.clone())));
                        folds.push(factor);
                    }
                }
            }
        }
        self.arrange_settled_fold = settled_fold;
        let keys: Vec<String> = rows.iter().map(ArrangeRow::key).collect();
        if keys != self.arrange_row_keys {
            replace_gliding_rows(
                &self.arrange_list_state,
                &self.arrange_flip,
                |index| self.arrange_row_keys.get(index).cloned(),
                keys.iter().cloned(),
                window,
            );
            self.arrange_row_keys = keys;
        }
        let projects: HashMap<ProjectKey, String> = self
            .hosts
            .iter()
            .flat_map(|host| {
                host.store.read(cx).projects().into_iter().map(|project| {
                    let key = ProjectKey {
                        host: host.key.clone(),
                        project_id: project.id,
                    };
                    (key, project.name)
                })
            })
            .collect();
        let rows = Rc::new(rows);
        let flip = self.arrange_flip.clone();
        v_flex()
            .size_full()
            .child(
                div()
                    .debug_selector(|| "arrange-title".into())
                    .flex_none()
                    .px(px(COMPACT_PAGE_INSET))
                    .pt(cx.theme().tokens.spacing.sm)
                    .text_token(&cx.theme().tokens.typography.xl)
                    .font_semibold()
                    .child(crate::tr!("sidebar.arrange_title")),
            )
            .child(
                div()
                    .flex_none()
                    .px(px(COMPACT_PAGE_INSET))
                    .py(cx.theme().tokens.spacing.sm)
                    .text_secondary(true, cx)
                    .child(crate::tr!("sidebar.arrange_hint")),
            )
            .child(
                div()
                    .id("arrange-thread-list")
                    .flex_1()
                    .min_h_0()
                    .on_drop(
                        cx.listener(|this, _: &AnyDrag, window, cx| this.drop_thread(window, cx)),
                    )
                    .on_drag_move(cx.listener(|this, event: &DragMoveEvent<AnyDrag>, _, cx| {
                        this.drag_auto_scroll(event, cx)
                    }))
                    .child(
                        list(
                            self.arrange_list_state.clone(),
                            cx.processor(move |this, index: usize, window, cx| {
                                let factor = folds.get(index).copied().unwrap_or(1.);
                                let key =
                                    SharedString::from(format!("arrange-{}", rows[index].key()));
                                let folded = this.fold_row(
                                    SharedString::from(format!("{key}-fold")),
                                    factor,
                                    window,
                                    cx,
                                    |this, window, cx| {
                                        this.render_arrange_row(&rows[index], &projects, window, cx)
                                    },
                                );
                                flip.row(index, key, folded)
                            }),
                        )
                        .size_full(),
                    ),
            )
            .into_any_element()
    }

    fn render_arrange_row(
        &self,
        row: &ArrangeRow,
        projects: &HashMap<ProjectKey, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let scope = self.arrange_scope.clone();
        let target = self
            .drag
            .as_ref()
            .and_then(|drag| drag.target(scope.as_ref()));
        // A caption lights up as a drag takes its section as the target.
        let mut caption_color = |section: ThreadSection| {
            transition(
                (
                    SharedString::from(format!("arrange-caption-{section:?}")),
                    "color",
                ),
                if target == Some(section) {
                    cx.theme().primary
                } else {
                    cx.theme().muted_foreground
                },
                gpui_base::motion::Transition::new(GLIDE),
                window,
                cx,
            )
        };
        match row {
            ArrangeRow::Caption(section, count) => {
                let label = match section {
                    ThreadSection::Pinned => crate::tr!("sidebar.pinned_count", count = count),
                    _ => crate::tr!("sidebar.active_count", count = count),
                };
                let color = caption_color(*section);
                let caption = crate::material::list_caption(label.into_owned().into(), true, cx)
                    .text_color(color);
                self.drop_zone(caption, DropZone::Start(*section), scope, cx)
                    .into_any_element()
            }
            ArrangeRow::PinnedEmpty(empty) => {
                let Some(shown) = crate::motion::shown(
                    "arrange-pinned-empty",
                    empty.then_some(()),
                    SHOW,
                    window,
                    cx,
                ) else {
                    return div().into_any_element();
                };
                let row =
                    crate::material::empty_row(crate::tr!("sidebar.arrange_pinned_empty"), cx)
                        .debug_selector(|| "arrange-pinned-empty".into());
                self.drop_zone(
                    div().w_full().child(shown.fold(row)),
                    DropZone::Start(ThreadSection::Pinned),
                    scope,
                    cx,
                )
                .into_any_element()
            }
            ArrangeRow::Settled(count) => {
                let expanded = self.arrange_settled_expanded;
                let color = caption_color(ThreadSection::Settled);
                // The compact sidebar's settled disclosure, at a touch
                // section bar's height and inset.
                let header = crate::material::accessible_clickable(
                    h_flex(),
                    "arrange-settled",
                    Role::Button,
                    crate::tr!("sidebar.settled"),
                    cx,
                )
                .aria_expanded(expanded)
                .debug_selector(|| "arrange-settled".into())
                .w_full()
                .h(px(bar_height(Bar::Section, true)))
                .gap(cx.theme().tokens.spacing.sm)
                .px(px(COMPACT_PAGE_INSET))
                .items_center()
                .text_caption(cx)
                .text_color(color)
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.arrange_settled_expanded = !this.arrange_settled_expanded;
                    cx.notify();
                }))
                .child(collapse_chevron(self.arrange_settled_fold, cx).text_color(color))
                .child(crate::tr!("sidebar.settled_count", count = count));
                self.drop_zone(div().w_full().child(header), DropZone::Settled, scope, cx)
                    .into_any_element()
            }
            ArrangeRow::Thread(listed) => {
                let zone = DropZone::Row {
                    thread: listed.key(),
                    section: thread_section(&listed.meta),
                };
                // While carried, the row keeps its place, hidden under its
                // ghost.
                let row = self.render_arrange_thread(listed, projects, cx);
                let element = div().w_full().child(drag_ghost::slot(
                    thread_slot(&listed.name),
                    row,
                    window,
                    cx,
                ));
                self.drop_zone(element, zone, scope, cx).into_any_element()
            }
        }
    }

    fn render_arrange_thread(
        &self,
        listed: &ListedThread,
        projects: &HashMap<ProjectKey, String>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(store) = self.store_of(&listed.host) else {
            return div().into_any_element();
        };
        let meta = &listed.meta;
        let name = listed.name.clone();
        let project = meta
            .project_id
            .as_ref()
            .and_then(|id| {
                projects.get(&ProjectKey {
                    host: listed.host.clone(),
                    project_id: id.clone(),
                })
            })
            .cloned();
        let host_name = self
            .sectioned()
            .then(|| host_title(store.read(cx)).to_string());
        let project = match (project, host_name) {
            (Some(project), Some(host)) => Some(format!("{project} · {host}")),
            (project, host) => project.or(host),
        };
        let state = self.thread_row_state(
            store.read(cx),
            listed,
            &HashMap::new(),
            format!("arrange-thread-{name}"),
        );
        let handle = crate::material::accessible_clickable(
            div(),
            SharedString::from(format!("arrange-handle-{name}")),
            Role::Button,
            crate::tr!("sidebar.arrange_handle", title = meta.title.clone()),
            cx,
        )
        .debug_selector({
            let name = name.clone();
            move || format!("arrange-handle-{name}")
        })
        .flex_none()
        .size(px(ARRANGE_HANDLE_SIZE))
        .flex()
        .items_center()
        .justify_center()
        .cursor_grab()
        // The handle alone starts a drag, so a press must not reach the row.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(
            Icon::new(IconName::GripVertical)
                .size(px(20.))
                .text_color(cx.theme().muted_foreground),
        );
        let handle = match self
            .dragged_thread(meta, &state, cx)
            .filter(|dragged| dragged.arranges)
        {
            Some(dragged) => Self::drag_source(handle, dragged, cx),
            None => handle,
        };
        crate::material::list_row(
            SharedString::from(format!("arrange-row-{name}")),
            meta.title.clone().into(),
            cx,
        )
        .debug_selector(move || format!("arrange-row-{name}"))
        // The 44pt grip must reach the page edge; `list_row`'s trailing inset
        // would push it in.
        .pr(cx.theme().tokens.spacing.xs)
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap(cx.theme().tokens.spacing.xxs)
                .child(
                    div()
                        .text_body(true, cx)
                        .line_clamp(2)
                        .text_color(if meta.is_settled() {
                            cx.theme().muted_foreground
                        } else {
                            cx.theme().foreground
                        })
                        .child(meta.title.clone()),
                )
                .when_some(project, |column, project| {
                    column.child(
                        div()
                            .text_secondary(true, cx)
                            .overflow_hidden()
                            .child(crate::widgets::fade_text(project)),
                    )
                }),
        )
        .child(handle)
        .into_any_element()
    }
}

enum ArrangeRow {
    Caption(ThreadSection, usize),
    /// Whether Pinned is empty: the row folds in and out with it.
    PinnedEmpty(bool),
    Thread(Box<ListedThread>),
    Settled(usize),
}

impl ArrangeRow {
    fn key(&self) -> String {
        match self {
            Self::Caption(section, _) => format!("caption-{section:?}"),
            Self::PinnedEmpty(_) => "pinned-empty".into(),
            Self::Thread(row) => format!("thread-{}", row.name),
            Self::Settled(_) => "settled".into(),
        }
    }
}
