//! The desktop chat column as a gpui-base dock of thread panes.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    Action, AnyElement, AnyView, App, Axis, Bounds, Context, Div, Empty, Entity, EventEmitter,
    FocusHandle, Focusable, InteractiveElement as _, IntoElement, ParentElement as _, Pixels,
    Render, Stateful, Styled as _, Subscription, WeakEntity, Window, actions, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::dock::{
    DockAreaRenderer, DockLayout, DropIndicator, PaneNode, PaneRef, Panel, PanelEvent, PanelId,
    TabGroup, TabGroupContext, TabGroupRenderer,
};
use gpui_base::{ElementExt as _, Placement, v_flex};
use serde::Deserialize;

use crate::chat::{ChatView, WindowEdges};
use crate::icon::{Icon, IconName};
use crate::theme::ActiveTheme as _;

actions!(tcode, [SplitRight, SplitDown, ClosePane, ZoomPane]);

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

    /// The dock's tab group holding the pane.
    pub(crate) fn group(&self) -> Option<Entity<TabGroup>> {
        self.group.as_ref()?.upgrade()
    }

    /// Where the pane was last painted.
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
        // A pane beside others waits for a thread rather than offering the
        // empty workspace's way to start one.
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
            layers.push((
                WAITING,
                v_flex()
                    .debug_selector(|| "waiting-pane".into())
                    .size_full()
                    .child(crate::material::empty_state(
                        Icon::new(IconName::Columns2),
                        crate::tr!("panes.waiting"),
                        "",
                        cx,
                    ))
                    .into_any_element(),
            ));
        }
        if content.shows(CHAT) {
            layers.push((CHAT, self.chat.clone().into_any_element()));
        }
        div()
            .size_full()
            .on_prepaint(move |painted, _, _| bounds.set(painted))
            .on_action(cx.listener(|this, _: &SplitRight, window, cx| this.claim(window, cx)))
            .on_action(cx.listener(|this, _: &SplitDown, window, cx| this.claim(window, cx)))
            .on_action(cx.listener(|this, _: &ClosePane, window, cx| this.claim(window, cx)))
            .on_action(cx.listener(|this, _: &ZoomPane, window, cx| this.claim(window, cx)))
            .child(content.render(layers).size_full())
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
/// A pane opens, closes, zooms and resizes without motion: gpui-base's dock
/// lays its groups out with no layout transition and unmounts a closed group
/// at once, so neither a pane's size nor its leaving can be animated here.
pub(crate) struct ThreadPanes {
    lands: DropLands,
}

impl ThreadPanes {
    pub(crate) fn new(lands: DropLands) -> Self {
        Self { lands }
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
}

/// A pane's thread wears its own header, so the group draws no tab bar.
struct ThreadTabs {
    lands: DropLands,
}

impl TabGroupRenderer for ThreadTabs {
    fn render_tab_bar(&self, _: &TabGroupContext, _: &mut Window, _: &mut App) -> AnyElement {
        Empty.into_any_element()
    }

    /// Among several panes, the one holding the keyboard wears the focus
    /// ring on top of its thread. `TabGroupContext` carries no focus state of
    /// its own; the displayed panel's focus handle answers. A group that may
    /// close is one with a neighbour, and a zoomed group may not.
    fn render_active_panel(
        &self,
        panel: AnyView,
        group: &TabGroupContext,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        // Always sampled, so the ring glides in and out as the keyboard moves
        // between panes, a neighbour comes or goes, or the pane zooms.
        let focused = group.is_closable()
            && group
                .active_panel()
                .is_some_and(|panel| panel.focus_handle(cx).contains_focused(window, cx));
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
    fn render_drop_indicator(
        &self,
        indicator: DropIndicator,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        if !(self.lands)(indicator.bounds(), indicator.placement(), window, cx) {
            return None;
        }
        let to = indicator.to();
        let id = "pane-drop-placeholder";
        let spring = |channel, target, window: &mut Window, cx: &mut App| -> Pixels {
            gpui_base::motion::spring((id, channel), target, crate::motion::FOLLOW, window, cx)
        };
        let left = spring("left", to.origin().x, window, cx);
        let top = spring("top", to.origin().y, window, cx);
        let width = spring("width", to.size().width, window, cx);
        let height = spring("height", to.size().height, window, cx);
        let inset = cx.theme().tokens.spacing.sm;
        let accent = cx.theme().primary;
        Some(
            div()
                .debug_selector(|| "pane-drop-indicator".into())
                .absolute()
                .left(left + inset)
                .top(top + inset)
                .w((width - inset * 2.).max(px(0.)))
                .h((height - inset * 2.).max(px(0.)))
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
