//! The desktop chat column as a gpui-base dock of thread panes.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    Action, AnyElement, AnyView, App, Axis, Bounds, Context, Div, Empty, Entity, EventEmitter,
    FocusHandle, Focusable, InteractiveElement as _, IntoElement, ParentElement as _, Pixels,
    Point, Render, Stateful, Styled as _, Subscription, WeakEntity, Window, actions, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::dock::{
    DockAreaRenderer, DockLayout, DropIndicator, NodeId, PaneNode, PaneRef, Panel, PanelEvent,
    PanelId, TabGroup, TabGroupContext, TabGroupRenderer,
};
use gpui_base::{ElementExt as _, Placement, ResizeHandleContext, v_flex};
use serde::Deserialize;

mod glide;

pub(crate) use glide::{Before, Glides, Held};

use crate::chat::{ChatView, WindowEdges};
use crate::icon::{Icon, IconName};
use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::widgets::button::{Button, ButtonVariants as _};

actions!(tcode, [SplitRight, SplitDown, ClosePane, ZoomPane]);

/// The key context of a pane waiting for a thread beside others, where
/// Escape closes it.
pub(crate) const WAITING_CONTEXT: &str = "WaitingPane";

/// Move the keyboard to another pane: the nearest painted one on a side, or
/// the next or previous one in the layout's order.
#[derive(Action, Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode, no_json)]
pub(crate) enum FocusPane {
    Left,
    Right,
    Up,
    Down,
    Next,
    Previous,
}

/// Show a thread in a new pane right of the focused one, or focus the pane
/// already showing it.
#[derive(Action, Clone, Debug, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode, no_json)]
pub(crate) struct OpenInNewPane(pub(crate) String);

/// Lay the whole column out at once. The panes there fill the new slots in
/// the layout's order, the focused one first; panes left over close, and
/// slots left over wait for a thread.
#[derive(Action, Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode, no_json)]
pub(crate) enum PaneLayout {
    Columns,
    Grid,
    OneBesideTwo,
    Single,
    /// The focused thread on the left, its agents stacked on the right.
    SpreadAgents,
}

impl PaneLayout {
    pub(crate) const ALL: [Self; 5] = [
        Self::Columns,
        Self::Grid,
        Self::OneBesideTwo,
        Self::Single,
        Self::SpreadAgents,
    ];

    /// How a layout of `count` panes grows from its first: pane `ix + 1`
    /// opens on the given side of pane `from`.
    pub(crate) fn splits(self, count: usize) -> Vec<(usize, Placement)> {
        match self {
            Self::Single => Vec::new(),
            Self::Columns => vec![(0, Placement::Right)],
            Self::Grid => vec![
                (0, Placement::Right),
                (0, Placement::Bottom),
                (1, Placement::Bottom),
            ],
            Self::OneBesideTwo => vec![(0, Placement::Right), (1, Placement::Bottom)],
            Self::SpreadAgents => (0..count.saturating_sub(1))
                .map(|ix| match ix {
                    0 => (0, Placement::Right),
                    ix => (ix, Placement::Bottom),
                })
                .collect(),
        }
    }

    pub(crate) fn icon(self) -> IconName {
        match self {
            Self::Columns => IconName::Columns2,
            Self::Grid | Self::OneBesideTwo => IconName::LayoutDashboard,
            Self::Single => IconName::Square,
            Self::SpreadAgents => IconName::Users,
        }
    }

    pub(crate) fn label(self) -> std::borrow::Cow<'static, str> {
        match self {
            Self::Columns => crate::tr!("panes.layout_columns"),
            Self::Grid => crate::tr!("panes.layout_grid"),
            Self::OneBesideTwo => crate::tr!("panes.layout_one_beside_two"),
            Self::Single => crate::tr!("panes.layout_single"),
            Self::SpreadAgents => crate::tr!("panes.spread_agents"),
        }
    }
}

/// The most panes the chat column holds.
pub(crate) const MAX_PANES: usize = 6;

type FocusIn = Rc<dyn Fn(Entity<ThreadPanel>, &mut Window, &mut App)>;

/// One thread shown in one pane.
pub(crate) struct ThreadPanel {
    chat: Entity<ChatView>,
    /// The thread `chat` is bound to, held here too so the shell can read a
    /// pane's binding while that pane's conversation is rendering.
    session_id: Option<String>,
    /// The pane's own focus, tracked by its tab group. The composer's would
    /// make the group's click-to-focus bypass the composer's refusals (a
    /// software keyboard, a read-only thread) and add the composer to the tab
    /// order a second time.
    focus_handle: FocusHandle,
    group: Option<WeakEntity<TabGroup>>,
    bounds: Rc<Cell<Bounds<Pixels>>>,
    among_others: bool,
    /// The palette slot of the pane's outline, given by the shell when the
    /// pane joins the column and kept while it changes thread and while it
    /// leaves.
    colour: Option<usize>,
    /// Leaving the column: still drawn while its slot glides shut, inert.
    closing: bool,
    /// How the content is drawn while its split glides.
    held: Held,
    _focus: Subscription,
    _focus_in: Subscription,
}

impl ThreadPanel {
    /// `on_focus_in` hears every time the keyboard enters the pane, once the
    /// pane is no longer being updated.
    pub(crate) fn new(
        chat: Entity<ChatView>,
        on_focus_in: impl Fn(Entity<ThreadPanel>, &mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        // A pane waiting for its thread draws no composer to hand focus to.
        let focus = cx.on_focus(&focus_handle, window, |panel, window, cx| {
            if panel.session_id.is_some() {
                panel
                    .chat
                    .update(cx, |chat, cx| chat.focus_composer(window, cx));
            }
        });
        let on_focus_in: FocusIn = Rc::new(on_focus_in);
        let focus_in = cx.on_focus_in(&focus_handle, window, move |_, window, cx| {
            let (pane, on_focus_in) = (cx.entity(), on_focus_in.clone());
            window.defer(cx, move |window, cx| on_focus_in(pane, window, cx));
        });
        Self {
            session_id: chat.read(cx).session_id().map(str::to_owned),
            chat,
            focus_handle,
            group: None,
            bounds: Rc::default(),
            among_others: false,
            colour: None,
            closing: false,
            held: Held::default(),
            _focus: focus,
            _focus_in: focus_in,
        }
    }

    pub(crate) fn chat(&self) -> Entity<ChatView> {
        self.chat.clone()
    }

    /// The thread the pane shows.
    pub(crate) fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub(crate) fn set_session(&mut self, session_id: Option<String>, cx: &mut Context<Self>) {
        self.session_id = session_id.clone();
        self.chat
            .update(cx, |chat, cx| chat.set_session(session_id, cx));
        cx.notify();
    }

    /// Whether other panes share the column.
    pub(crate) fn set_among_others(&mut self, among_others: bool, cx: &mut Context<Self>) {
        if self.among_others != among_others {
            self.among_others = among_others;
            cx.notify();
        }
    }

    /// The palette slot of the pane's outline, once it has one.
    pub(crate) fn colour(&self) -> Option<usize> {
        self.colour
    }

    pub(crate) fn set_colour(&mut self, slot: usize, cx: &mut Context<Self>) {
        self.colour = Some(slot);
        cx.notify();
    }

    /// Whether the pane is leaving the column.
    pub(crate) fn is_closing(&self) -> bool {
        self.closing
    }

    /// Leave the column: the pane takes no pointer from now on, and the shell
    /// takes it out once its slot has glided shut.
    pub(crate) fn set_closing(&mut self, cx: &mut Context<Self>) {
        self.closing = true;
        cx.notify();
    }

    /// Lay the content out at `held`'s size along the axes it names, clipped
    /// to the pane, rather than at the pane's size, and draw it at its
    /// opacity.
    pub(crate) fn set_held(&mut self, held: Held, cx: &mut Context<Self>) {
        if self.held != held {
            self.held = held;
            cx.notify();
        }
    }

    /// The dock's tab group holding the pane.
    pub(crate) fn group(&self) -> Option<Entity<TabGroup>> {
        self.group.as_ref()?.upgrade()
    }

    /// Where the pane was last painted, as far as it shows.
    pub(crate) fn bounds(&self) -> Bounds<Pixels> {
        self.bounds.get()
    }

    /// A pane action raised inside the pane acts on it, wherever the keyboard
    /// was: the shell acts on the pane holding the focus.
    fn claim(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus_handle.contains_focused(window, cx) {
            window.focus(&self.focus_handle, cx);
        }
        cx.propagate();
    }
}

impl EventEmitter<PanelEvent> for ThreadPanel {}

impl Focusable for ThreadPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ThreadPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        const WAITING: usize = 0;
        const CHAT: usize = 1;
        let bounds = self.bounds.clone();
        let held = self.held;
        // A pane beside others waits for a thread rather than offering the
        // empty workspace's way to start one. The swap cross-fades when the
        // binding changes inside a layout that holds, and when the last
        // neighbour starts closing, which the dock re-parents the group for
        // only once its glide has played; when a neighbour comes it lands
        // with the dock re-parenting the group and snaps (see
        // `ThreadPanes`). A pane whose thread closes is rebound before
        // the fade, so the leaving layer is this chat already showing the
        // empty workspace: GPUI cannot draw an element tree from an earlier
        // frame, and the store frees a closed thread's timeline in
        // `close_thread` before the shell hears of it, so a chat kept bound
        // to it would draw its loading skeleton instead.
        let waiting = self.among_others && self.session_id.is_none();
        let content = crate::motion::CrossFade::new(
            "pane-content",
            if waiting { WAITING } else { CHAT },
            2,
            window,
            cx,
        )
        .fill();
        let mut layers = Vec::new();
        if content.shows(WAITING) {
            // A waiting pane draws no chat header, so no pane menu: this is
            // its way to close, as the menu's row is.
            let pane = self.focus_handle.clone();
            let spacing = cx.theme().tokens.spacing.sm;
            layers.push((
                WAITING,
                v_flex()
                    .debug_selector(|| "waiting-pane".into())
                    .relative()
                    .size_full()
                    .child(crate::material::empty_state(
                        Icon::new(IconName::Columns2),
                        crate::tr!("panes.waiting"),
                        "",
                        cx,
                    ))
                    .child(
                        div().absolute().top(spacing).right(spacing).child(
                            Button::new("waiting-pane-close")
                                .debug_selector(|| "waiting-pane-close".into())
                                .ghost()
                                .small()
                                .compact()
                                .icon(IconName::Close)
                                .tooltip(crate::tr!("panes.close"))
                                .on_click(move |_, window, cx| {
                                    window.focus(&pane, cx);
                                    window.dispatch_action(Box::new(ClosePane), cx);
                                }),
                        ),
                    )
                    .into_any_element(),
            ));
        }
        if content.shows(CHAT) {
            layers.push((CHAT, self.chat.clone().into_any_element()));
        }
        // A slot gliding below the dock's slot floor runs past its split's
        // end, where the split's frame clips it: the pane's bounds are the
        // part that shows. Content held wider than the slot is clipped by
        // the tab group's content frame.
        div()
            .relative()
            .size_full()
            // A waiting pane has no composer to hold the keyboard, so the
            // pane holds it, and pane commands and Escape reach it there.
            .when(waiting, |pane| {
                pane.track_focus(&self.focus_handle)
                    .key_context(WAITING_CONTEXT)
            })
            .on_prepaint(move |painted, window, _| {
                bounds.set(painted.intersect(&window.content_mask().bounds))
            })
            .on_action(cx.listener(|this, _: &SplitRight, window, cx| this.claim(window, cx)))
            .on_action(cx.listener(|this, _: &SplitDown, window, cx| this.claim(window, cx)))
            .on_action(cx.listener(|this, _: &ClosePane, window, cx| this.claim(window, cx)))
            .on_action(cx.listener(|this, _: &ZoomPane, window, cx| this.claim(window, cx)))
            .child(
                content
                    .render(layers)
                    .map(|content| match held.size.width {
                        Some(width) => content.w(width).flex_none(),
                        None => content.w_full(),
                    })
                    .map(|content| match held.size.height {
                        Some(height) => content.h(height).flex_none(),
                        None => content.h_full(),
                    })
                    .opacity(held.opacity),
            )
            .when(self.closing, |pane| pane.child(crate::motion::shield()))
    }
}

impl Panel for ThreadPanel {
    fn panel_name(&self) -> &'static str {
        "thread"
    }

    fn on_added_to(&mut self, group: WeakEntity<TabGroup>, _: &mut Window, _: &mut Context<Self>) {
        self.group = Some(group);
    }
}

/// Whether a thread dragged over a pane would land there: the pane's content
/// bounds and the edge it would open on, `None` for the pane itself.
pub(crate) type DropLands = Rc<dyn Fn(Bounds<Pixels>, Option<Placement>, &Window, &App) -> bool>;

/// The panes' appearance: the area is the chat column's reading surface.
///
/// A pane opening or closing glides its split's slots (see [`Glides`]); a
/// zoom and the widths a window resize redistributes land at once.
///
/// The dock re-parents a group whenever the tree around it changes: a
/// lone group sits in the area's frame, a split puts it under
/// `("dock-split", node)`, a merge or a close lifts it back out, and zoom
/// draws it straight into the frame. Element state is keyed by the full id
/// path from the window root, so every pane that moves in the tree starts
/// over: its colour outline, its focus ring and its waiting-hint
/// cross-fade snap to their targets in the frame of a split, of a zoom, or
/// the frame a closing pane is taken out once its glide has played, and its
/// chat loses its handle-less stateful scrolls, such as the header's
/// `chat-header-scroll`, and any other keyed state under it.
pub(crate) struct ThreadPanes {
    lands: DropLands,
    handle_held: HandleHeld,
}

/// Set in every frame that draws a split's handle held by the pointer: the
/// handle's axis and where the pointer was, for [`held_split`].
pub(crate) type HandleHeld = Rc<Cell<Option<(Axis, Point<Pixels>)>>>;

impl ThreadPanes {
    pub(crate) fn new(lands: DropLands, handle_held: HandleHeld) -> Self {
        Self { lands, handle_held }
    }
}

impl DockAreaRenderer for ThreadPanes {
    fn frame(&self, _: &mut Window, cx: &mut App) -> Stateful<Div> {
        div()
            .id("thread-panes")
            .bg(crate::material::content_surface(cx))
            .shadow_sm()
    }

    fn tab_group_renderer(&self) -> Rc<dyn TabGroupRenderer> {
        Rc::new(ThreadTabs {
            lands: self.lands.clone(),
        })
    }

    /// Base's own line. A handle the pointer holds takes its split's sizes
    /// over, so the shell stops that split's glide (see [`Glides::stop`]).
    fn render_split_handle(
        &self,
        handle: &ResizeHandleContext,
        window: &mut Window,
        _: &mut App,
    ) -> Option<AnyElement> {
        if handle.is_active() {
            self.handle_held
                .set(Some((handle.axis(), window.mouse_position())));
        }
        None
    }
}

/// A pane's thread wears its own header, so the group draws no tab bar.
struct ThreadTabs {
    lands: DropLands,
}

impl TabGroupRenderer for ThreadTabs {
    fn render_tab_bar(&self, _: &TabGroupContext, _: &mut Window, _: &mut App) -> AnyElement {
        Empty.into_any_element()
    }

    /// Among several panes, each wears the outline of its colour, and the
    /// one holding the keyboard the focus ring on top of it.
    /// `TabGroupContext` carries no focus state of its own; the displayed
    /// panel's focus handle answers. A group that may close is one with a
    /// neighbour, and a zoomed group may not; a pane whose last neighbour is
    /// closing is alone already.
    fn render_active_panel(
        &self,
        panel: AnyView,
        group: &TabGroupContext,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        // Both always sampled, so they glide in and out while the panes
        // stay where they are: the ring as the keyboard moves between them,
        // both out as the last neighbour starts closing. A split or a zoom
        // re-parents the group and they snap (see `ThreadPanes`).
        let pane = panel.clone().downcast::<ThreadPanel>().ok();
        let shared =
            group.is_closable() && pane.as_ref().is_some_and(|pane| pane.read(cx).among_others);
        let focused = shared
            && group
                .active_panel()
                .is_some_and(|panel| panel.focus_handle(cx).contains_focused(window, cx));
        let outline = match pane
            .and_then(|pane| pane.read(cx).colour)
            .map(|slot| crate::material::pane_colour(slot, cx))
        {
            Some(colour) if shared => colour,
            Some(colour) => colour.opacity(0.),
            None => gpui::transparent_black(),
        };
        let outline = gpui_base::motion::transition(
            gpui::ElementId::NamedInteger("pane-outline".into(), panel.entity_id().as_u64()),
            outline,
            crate::motion::settle(),
            window,
            cx,
        );
        let ring = crate::material::focus_ring(cx);
        let color = gpui_base::motion::transition(
            gpui::ElementId::NamedInteger("pane-focus-ring".into(), panel.entity_id().as_u64()),
            if focused {
                ring.color
            } else {
                ring.color.opacity(0.)
            },
            crate::motion::settle(),
            window,
            cx,
        );
        div()
            .relative()
            .size_full()
            .child(panel)
            .when(outline.a > 0., |this| {
                this.child(div().absolute().inset_0().border_1().border_color(outline))
            })
            .when(color.a > 0., |this| {
                this.child(
                    div()
                        .when(focused, |ring| {
                            ring.debug_selector(|| "focused-pane".into())
                        })
                        .absolute()
                        .inset_0()
                        .shadow(vec![gpui::BoxShadow { color, ..ring }]),
                )
            })
            .into_any_element()
    }

    /// The pane a dragged thread would open as, or the pane it would replace.
    /// `DropIndicator` names no group, so the shell finds the pane by the
    /// bounds the drag hovers.
    ///
    /// Over a spot where the drop cannot land the placeholder fades out where
    /// it last could, and fades back in when the drag returns; within one
    /// pane it springs between the edges and the middle. Moving to another
    /// pane it neither travels nor fades: each tab group draws its own
    /// indicator under its own element path (gpui-base's `TabGroup::render`)
    /// and the dock has no area-level one, so the old pane's is gone and the
    /// new one's starts at rest. Base also stops calling this hook once the
    /// drag leaves the group, lands or is cancelled, so there is nothing left
    /// to draw an exit with, and an entry fade alone would be enter-only.
    fn render_drop_indicator(
        &self,
        indicator: DropIndicator,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        let id = "pane-drop-placeholder";
        let lands = (self.lands)(indicator.bounds(), indicator.placement(), window, cx);
        let shown = crate::motion::shown(id, lands.then(|| indicator.to()), window, cx)?;
        let to = shown.value;
        let spring = |channel, target, window: &mut Window, cx: &mut App| -> Pixels {
            gpui_base::motion::spring((id, channel), target, crate::motion::FOLLOW, window, cx)
        };
        let left = spring("left", to.origin().x, window, cx);
        let top = spring("top", to.origin().y, window, cx);
        let width = spring("width", to.size().width, window, cx);
        let height = spring("height", to.size().height, window, cx);
        let inset = cx.theme().tokens.spacing.sm;
        let accent = cx.theme().primary;
        // The opacity is set here rather than through `Shown::fade`, whose
        // relative frame would become the absolute placeholder's containing
        // block, and whose shield would take the drop from the group beneath.
        Some(
            div()
                .debug_selector(|| "pane-drop-indicator".into())
                .absolute()
                .left(left + inset)
                .top(top + inset)
                .w((width - inset * 2.).max(px(0.)))
                .h((height - inset * 2.).max(px(0.)))
                .opacity(shown.progress())
                .rounded(crate::material::radius_card(cx))
                .border_1()
                .border_color(accent.opacity(0.5))
                .bg(accent.opacity(0.1))
                .into_any_element(),
        )
    }
}

/// `node` as a layout to install, each panel the pane `pane` names.
pub(crate) fn layout_of(
    node: &PaneNode,
    pane: &impl Fn(PanelId) -> Option<Entity<ThreadPanel>>,
) -> DockLayout {
    match node.kind() {
        PaneRef::Tabs { panels, .. } => panels
            .iter()
            .filter_map(|panel| pane(*panel))
            .fold(DockLayout::tabs(), DockLayout::panel),
        PaneRef::Split { axis, children, .. } => {
            let split = match axis {
                Axis::Horizontal => DockLayout::h_split(),
                Axis::Vertical => DockLayout::v_split(),
            };
            children.iter().fold(split, |split, child| {
                split.child(layout_of(child, pane), None)
            })
        }
    }
}

/// The narrowest a layout of panes can be: side-by-side panes add up, and
/// stacked ones take the widest. Each pane's minimum depends on which of the
/// layout's `edges` it runs along.
pub(crate) fn min_width(
    node: &PaneNode,
    edges: WindowEdges,
    pane_min: &impl Fn(PanelId, WindowEdges) -> f32,
) -> f32 {
    match node.kind() {
        PaneRef::Tabs { panels, active_ix } => {
            panels.get(active_ix).map_or(0., |id| pane_min(*id, edges))
        }
        PaneRef::Split {
            axis: Axis::Horizontal,
            children,
            ..
        } => children
            .iter()
            .enumerate()
            .map(|(ix, child)| min_width(child, column_edges(edges, ix, children.len()), pane_min))
            .sum(),
        PaneRef::Split { children, .. } => children
            .iter()
            .map(|child| min_width(child, edges, pane_min))
            .fold(0., f32::max),
    }
}

/// The split under `node` whose handle the pointer took at `at`: of the
/// splits along `axis`, the one with a seam between two slots nearest `at`,
/// preferring those whose extent across the seam holds `at`. `painted` is
/// where each panel was last drawn.
///
/// gpui-base's `ResizeHandleContext` names a handle's axis and drag state
/// but not its split, so the split is found from where the hold began: the
/// pointer is on the handle's seam then. The dock splices a split nested
/// along its parent's axis into the parent, so another split's seam along
/// that axis lies inside one of its slots, at least a slot's floor away,
/// or beside it across.
pub(crate) fn held_split(
    node: &PaneNode,
    axis: Axis,
    at: Point<Pixels>,
    painted: &impl Fn(PanelId) -> Option<Bounds<Pixels>>,
) -> Option<NodeId> {
    let mut nearest = None;
    seams(node, axis, at, painted, &mut nearest);
    nearest.map(|(_, _, split)| split)
}

/// Where `node` was drawn, noting in `nearest` its seams along `axis`: how
/// far each is from `at`, whether `at` is off its extent across, and its
/// split.
fn seams(
    node: &PaneNode,
    axis: Axis,
    at: Point<Pixels>,
    painted: &impl Fn(PanelId) -> Option<Bounds<Pixels>>,
    nearest: &mut Option<(bool, Pixels, NodeId)>,
) -> Option<Bounds<Pixels>> {
    let union = |bounds: &mut dyn Iterator<Item = Bounds<Pixels>>| {
        bounds
            .filter(|bounds| !bounds.is_empty())
            .reduce(|all, bounds| all.union(&bounds))
    };
    match node.kind() {
        PaneRef::Tabs { panels, .. } => {
            union(&mut panels.iter().filter_map(|panel| painted(*panel)))
        }
        PaneRef::Split {
            axis: along,
            children,
            ..
        } => {
            let slots: Vec<_> = children
                .iter()
                .map(|child| seams(child, axis, at, painted, nearest))
                .collect();
            let drawn = union(&mut slots.iter().flatten().copied());
            if along == axis
                && let Some(drawn) = drawn
            {
                let (pointer, off) = match axis {
                    Axis::Horizontal => (at.x, at.y < drawn.top() || at.y > drawn.bottom()),
                    Axis::Vertical => (at.y, at.x < drawn.left() || at.x > drawn.right()),
                };
                let span = |slot: &Bounds<Pixels>| match axis {
                    Axis::Horizontal => (slot.left(), slot.right()),
                    Axis::Vertical => (slot.top(), slot.bottom()),
                };
                for pair in slots.windows(2) {
                    let seam = [
                        pair[0].map(|slot| span(&slot).1),
                        pair[1].map(|slot| span(&slot).0),
                    ];
                    for seam in seam.into_iter().flatten() {
                        let away = (seam - pointer).abs();
                        if nearest
                            .is_none_or(|(was_off, was_away, _)| (off, away) < (was_off, was_away))
                        {
                            *nearest = Some((off, away, node.id()));
                        }
                    }
                }
            }
            drawn
        }
    }
}

/// The split directly holding `child` under `node`.
pub(crate) fn parent_split(node: &PaneNode, child: NodeId) -> Option<NodeId> {
    let PaneRef::Split { children, .. } = node.kind() else {
        return None;
    };
    if children.iter().any(|held| held.id() == child) {
        return Some(node.id());
    }
    children.iter().find_map(|held| parent_split(held, child))
}

/// Which of `edges` the pane holding `panel` runs along, if `node` holds it.
pub(crate) fn edges_of(node: &PaneNode, edges: WindowEdges, panel: PanelId) -> Option<WindowEdges> {
    match node.kind() {
        PaneRef::Tabs { panels, .. } => panels.contains(&panel).then_some(edges),
        PaneRef::Split { axis, children, .. } => {
            children.iter().enumerate().find_map(|(ix, child)| {
                let edges = match axis {
                    Axis::Horizontal => column_edges(edges, ix, children.len()),
                    Axis::Vertical => edges,
                };
                edges_of(child, edges, panel)
            })
        }
    }
}

/// The edges column `ix` of `count` side-by-side ones keeps.
fn column_edges(edges: WindowEdges, ix: usize, count: usize) -> WindowEdges {
    WindowEdges {
        left: edges.left && ix == 0,
        right: edges.right && ix + 1 == count,
    }
}

/// The pane nearest `from` on `direction`'s side among those beside it
/// across that axis. `None` for a direction that is not a side.
///
/// Local because gpui-base's dock has no keyboard navigation between groups
/// (see `AppShell::focus_pane`).
pub(crate) fn neighbour(
    from: Bounds<Pixels>,
    panes: &[Bounds<Pixels>],
    direction: FocusPane,
) -> Option<usize> {
    let across_rows = |to: &Bounds<Pixels>| to.top() < from.bottom() && to.bottom() > from.top();
    let across_columns = |to: &Bounds<Pixels>| to.left() < from.right() && to.right() > from.left();
    let gap = |to: &Bounds<Pixels>| -> Option<f32> {
        let (gap, beside) = match direction {
            FocusPane::Left => (from.left() - to.right(), across_rows(to)),
            FocusPane::Right => (to.left() - from.right(), across_rows(to)),
            FocusPane::Up => (from.top() - to.bottom(), across_columns(to)),
            FocusPane::Down => (to.top() - from.bottom(), across_columns(to)),
            FocusPane::Next | FocusPane::Previous => return None,
        };
        let gap = f32::from(gap);
        (beside && gap >= -1.).then_some(gap)
    };
    let center = from.center();
    let offset = |to: &Bounds<Pixels>| {
        let offset = to.center() - center;
        f32::from(offset.x).abs() + f32::from(offset.y).abs()
    };
    panes
        .iter()
        .enumerate()
        .filter(|(_, to)| **to != from)
        .filter_map(|(ix, to)| Some((ix, gap(to)?, offset(to))))
        .min_by(|a, b| a.1.total_cmp(&b.1).then(a.2.total_cmp(&b.2)))
        .map(|(ix, ..)| ix)
}
