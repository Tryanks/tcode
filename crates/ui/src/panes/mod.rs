//! The desktop chat column as a gpui-base dock of thread panes.

use std::cell::Cell;
use std::rc::Rc;
#[cfg(not(target_family = "wasm"))]
use std::time::Instant;

use gpui::{
    Action, Along as _, AnyElement, AnyView, App, AppContext as _, Axis, Bounds, Context, Div,
    Empty, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, Point, Render, Size, Stateful, Styled as _, Subscription,
    WeakEntity, Window, actions, div, prelude::FluentBuilder as _, px,
};
use gpui_base::dock::{
    DockAreaRenderer, DockLayout, NodeId, PaneNode, PaneRef, Panel, PanelEvent, PanelId, TabGroup,
    TabGroupContext, TabGroupRenderer,
};
use gpui_base::motion::Interpolate as _;
use gpui_base::{ElementExt as _, Placement, ResizeHandleContext, v_flex};
use serde::Deserialize;
#[cfg(target_family = "wasm")]
use web_time::Instant;

mod drawer;
mod glide;
mod layout_swap;
mod zoom;

pub(crate) use drawer::{CloseRightPanel, Drawer, RIGHT_PANEL_CONTEXT, panel_covers};
pub(crate) use glide::{Before, Glides, Held, window_jumped};
pub(crate) use layout_swap::LayoutSwap;
pub(crate) use zoom::{PaneZoom, Zoom};

use crate::chat::{ChatView, WindowEdges};
use crate::icon::{Icon, IconName};
use crate::shell::PaneCommand;
use crate::theme::ActiveTheme as _;
use crate::widgets::button::Button;

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How far the selected fill comes in over a pane whose row is hovered: it
/// lies over the whole conversation, so less than on the row.
const ROW_HOVERED_FILL: f32 = 0.6;

actions!(
    tcode,
    [SplitRight, SplitDown, ClosePane, CloseOtherPanes, ZoomPane]
);

/// The key context of a pane waiting for a thread beside others, where
/// Escape closes it.
pub(crate) const WAITING_CONTEXT: &str = "WaitingPane";

/// A side of a pane, where its spatial neighbour is found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub(crate) enum Side {
    Left,
    Right,
    Up,
    Down,
}

impl Side {
    pub(crate) const ALL: [Self; 4] = [Self::Left, Self::Right, Self::Up, Self::Down];

    pub(crate) fn icon(self) -> IconName {
        match self {
            Self::Left => IconName::ArrowLeft,
            Self::Right => IconName::ArrowRight,
            Self::Up => IconName::ArrowUp,
            Self::Down => IconName::ArrowDown,
        }
    }
}

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

impl FocusPane {
    /// The side a spatial move goes to; `None` for the layout's order.
    pub(crate) fn side(self) -> Option<Side> {
        match self {
            Self::Left => Some(Side::Left),
            Self::Right => Some(Side::Right),
            Self::Up => Some(Side::Up),
            Self::Down => Some(Side::Down),
            Self::Next | Self::Previous => None,
        }
    }
}

/// Exchange the acting pane's thread with its neighbour's on a side; the
/// keyboard stays in the pane, which now shows the neighbour's thread.
#[derive(Action, Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode, no_json)]
pub(crate) struct SwapPane(pub(crate) Side);

impl SwapPane {
    pub(crate) fn label(self) -> std::borrow::Cow<'static, str> {
        match self.0 {
            Side::Left => crate::tr!("panes.swap_left"),
            Side::Right => crate::tr!("panes.swap_right"),
            Side::Up => crate::tr!("panes.swap_up"),
            Side::Down => crate::tr!("panes.swap_down"),
        }
    }
}

/// Move the acting pane's thread to its neighbour on a side, the thread
/// there coming back into the acting pane, and the keyboard with the moved
/// thread: [`SwapPane`]'s exchange, with the keyboard following instead of
/// staying. Nothing closes.
#[derive(Action, Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode, no_json)]
pub(crate) struct MoveThreadToPane(pub(crate) Side);

impl MoveThreadToPane {
    pub(crate) fn label(self) -> std::borrow::Cow<'static, str> {
        match self.0 {
            Side::Left => crate::tr!("panes.move_left"),
            Side::Right => crate::tr!("panes.move_right"),
            Side::Up => crate::tr!("panes.move_up"),
            Side::Down => crate::tr!("panes.move_down"),
        }
    }
}

/// Show a thread in a new pane right of the acting one, or focus the pane
/// already showing it.
#[derive(Action, Clone, Debug, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode, no_json)]
pub(crate) struct OpenInNewPane(pub(crate) String);

/// Lay the whole column out anew, as a layout swap (see [`LayoutSwap`]). The
/// panes there fill the new slots in the layout's order, the current pane
/// first; panes left over close, and slots left over wait for a thread.
#[derive(Action, Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode, no_json)]
pub(crate) enum PaneLayout {
    Columns,
    Grid,
    OneBesideTwo,
    Single,
    /// The current pane's thread on the left, its agents stacked on the right.
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

type Entered = Rc<dyn Fn(Entity<ThreadPanel>, &mut Window, &mut App)>;

/// One thread shown in one pane.
pub(crate) struct ThreadPanel {
    chat: Entity<ChatView>,
    /// What draws the chat, taken as the chat is: reading the chat as the
    /// pane renders would draw the pane again with every frame the chat
    /// draws.
    frame: Entity<crate::chat::ChatFrame>,
    drawer: Entity<Drawer>,
    /// What lays the pane's content out at the pane's width: the drawer,
    /// which draws the conversation and the panel beside it.
    split: Entity<drawer::Split>,
    /// The drawer's Preview tab once made: the window syncs its page on
    /// every frame, and a read of the drawer, which writes itself as it lays
    /// the pane out, would build the window again after each.
    preview: Rc<std::cell::RefCell<Option<Entity<crate::preview_panel::PreviewPanel>>>>,
    /// Where the conversation was last painted: the pane less the drawer's
    /// room.
    conversation: Rc<Cell<Bounds<Pixels>>>,
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
    /// Where the dock last laid the pane's slot out, as far as it shows: its
    /// placeholder's bounds while the pane is drawn over the column.
    slot: Rc<Cell<Bounds<Pixels>>>,
    among_others: bool,
    row_hovered: bool,
    zoom: PaneZoom,
    /// Leaving the column: still drawn while its slot glides shut, inert.
    closing: bool,
    /// The thread whose timeline the pane holds in the store for its chat,
    /// so a thread that closes while the pane still draws it (gliding out,
    /// or fading under the waiting hint) shows itself, not nothing.
    lease: Option<String>,
    /// The pane's thread closed beside others: its chat stays bound to it
    /// while it fades out under the waiting hint.
    fading_out: bool,
    /// A thread came to the pane while it waited beside others: the waiting
    /// hint stays, and the chat is drawn unseen under it, until the chat
    /// [appears](ChatView::appears), when one cross-fades into the other.
    awaiting: bool,
    /// How the content is drawn while its split glides.
    held: Held,
    /// The chat on its way in from another pane: where it was drawn there,
    /// and when it set off.
    flight: Option<(Bounds<Pixels>, Instant)>,
    /// The pane's fades: the waiting hint's cross-fade with its chat, and the
    /// fill of a hovered row. Kept here because the dock draws the pane under
    /// a new parent whenever the layout around it changes (see
    /// [`ThreadPanes`]).
    motion: crate::motion::owned::Owned,
    /// Hears the pane being entered.
    on_entered: Entered,
    _focus: Subscription,
    _focus_in: Subscription,
    _release: Subscription,
}

impl ThreadPanel {
    /// `on_entered` hears every press anywhere in the pane and every time the
    /// keyboard enters it, once the pane is no longer being updated.
    pub(crate) fn new(
        chat: Entity<ChatView>,
        drawer: Drawer,
        on_entered: impl Fn(Entity<ThreadPanel>, &mut Window, &mut App) + 'static,
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
        let on_entered: Entered = Rc::new(on_entered);
        let focus_in = cx.on_focus_in(&focus_handle, window, {
            let on_entered = on_entered.clone();
            move |_, window, cx| {
                let (pane, on_entered) = (cx.entity(), on_entered.clone());
                window.defer(cx, move |window, cx| on_entered(pane, window, cx));
            }
        });
        let session_id = chat.read(cx).session_id().map(str::to_owned);
        let frame = chat.read(cx).frame();
        // A pane taken down mid-fade, or with its glide never landing,
        // lets its lease go with it.
        let release = cx.on_release(|panel: &mut Self, cx| panel.hold(None, cx));
        let preview = drawer.preview();
        let drawer = {
            let pane = cx.weak_entity();
            cx.new(|_| drawer.in_pane(pane))
        };
        let mut panel = Self {
            session_id: None,
            chat,
            frame,
            preview,
            split: drawer::Split::new(
                drawer.downgrade(),
                |drawer: &mut Drawer, width| drawer.set_pane_width(width),
                cx,
            ),
            drawer,
            conversation: Rc::default(),
            focus_handle,
            group: None,
            bounds: Rc::default(),
            slot: Rc::default(),
            among_others: false,
            row_hovered: false,
            zoom: PaneZoom::default(),
            closing: false,
            lease: None,
            fading_out: false,
            awaiting: false,
            held: Held::default(),
            flight: None,
            motion: Default::default(),
            on_entered,
            _focus: focus,
            _focus_in: focus_in,
            _release: release,
        };
        panel.hold(session_id.as_deref(), cx);
        panel.session_id = session_id;
        panel
    }

    /// Hold `session_id`'s timeline in the store, letting go of the one held
    /// before.
    fn hold(&mut self, session_id: Option<&str>, cx: &mut App) {
        if self.lease.as_deref() == session_id {
            return;
        }
        let store = self.chat.read(cx).workspace_store().clone();
        let released = std::mem::replace(&mut self.lease, session_id.map(str::to_owned));
        store.update(cx, |store, cx| {
            if let Some(session_id) = session_id {
                store.lease_timeline(session_id);
            }
            if let Some(released) = released {
                store.release_timeline(&released, cx);
            }
        });
    }

    pub(crate) fn chat(&self) -> Entity<ChatView> {
        self.chat.clone()
    }

    /// What draws the pane's chat.
    pub(crate) fn frame(&self) -> Entity<crate::chat::ChatFrame> {
        self.frame.clone()
    }

    /// The thread the pane shows.
    pub(crate) fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Show `chat`, as bound, in place of the pane's own, which it returns.
    /// A chat drawn at `from` in another pane on the last frame travels
    /// from there into this pane over [`GLIDE`], drawn over the
    /// column on the opaque canvas, at the pane's size; under reduced motion
    /// it is here at once.
    ///
    /// The shell says where the chat comes from rather than the pane
    /// noticing a chat it drew elsewhere (as `motion::SharedMoves` notices
    /// within one view): a pane is a view of its own, drawn again from the
    /// last frame while nothing in it changes, so it does not see each
    /// frame of its neighbours.
    pub(crate) fn replace_chat(
        &mut self,
        chat: Entity<ChatView>,
        from: Option<Bounds<Pixels>>,
        cx: &mut Context<Self>,
    ) -> Entity<ChatView> {
        let session_id = chat.read(cx).session_id().map(str::to_owned);
        self.frame = chat.read(cx).frame();
        self.fading_out = false;
        self.await_chat(false, cx);
        let replaced = std::mem::replace(&mut self.chat, chat);
        self.hold(session_id.as_deref(), cx);
        self.session_id = session_id;
        self.flight = from
            .filter(|from| !cx.reduce_motion() && *from != self.conversation_bounds())
            .map(|from| (from, cx.background_executor().now()));
        cx.notify();
        replaced
    }

    /// Where the pane's chat is drawn now: on its way in, or in the pane.
    pub(crate) fn chat_drawn(&self, cx: &App) -> Bounds<Pixels> {
        let to = self.conversation_bounds();
        match self.flight {
            Some((from, at)) => from.interpolate(&to, flight_progress(at, cx)),
            None => to,
        }
    }

    pub(crate) fn set_session(&mut self, session_id: Option<String>, cx: &mut Context<Self>) {
        // Beside others, a pane whose thread closes fades its chat out
        // under the waiting hint: the chat stays bound to the thread, which
        // the pane's lease keeps drawn, until the fade has played.
        let fading_out = session_id.is_none()
            && self.among_others
            && (self.session_id.is_some() || self.fading_out);
        let waited = self.among_others && self.session_id.is_none() && !self.fading_out;
        self.session_id = session_id.clone();
        self.fading_out = fading_out;
        self.await_chat(waited && session_id.is_some(), cx);
        if !fading_out {
            self.hold(session_id.as_deref(), cx);
            self.chat
                .update(cx, |chat, cx| chat.set_session(session_id, cx));
        }
        cx.notify();
    }

    fn await_chat(&mut self, awaiting: bool, cx: &mut Context<Self>) {
        if self.awaiting != awaiting {
            self.awaiting = awaiting;
            ChatView::set_held(&self.chat, awaiting, cx);
        }
    }

    /// Whether other panes share the column.
    pub(crate) fn set_among_others(this: &Entity<Self>, among_others: bool, cx: &mut App) {
        if this.read(cx).among_others != among_others {
            this.update(cx, |pane, cx| {
                pane.among_others = among_others;
                cx.notify();
            });
        }
    }

    /// Whether the pointer is over the threads list's row of the pane's
    /// thread, which the pane then answers with a soft fill.
    pub(crate) fn set_row_hovered(this: &Entity<Self>, hovered: bool, cx: &mut App) {
        if this.read(cx).row_hovered != hovered {
            this.update(cx, |pane, cx| {
                pane.row_hovered = hovered;
                cx.notify();
            });
        }
    }

    /// How a zoom of the column leaves the pane.
    pub(crate) fn set_zoom(this: &Entity<Self>, zoom: PaneZoom, cx: &mut App) {
        if this.read(cx).zoom != zoom {
            this.update(cx, |pane, cx| {
                pane.zoom = zoom;
                pane.set_preview_covered(zoom.beneath, cx);
                cx.notify();
            });
        }
    }

    /// Where the dock last laid the pane's slot out.
    pub(crate) fn slot(&self) -> Bounds<Pixels> {
        self.slot.get()
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
    pub(crate) fn set_held(this: &Entity<Self>, held: Held, cx: &mut App) {
        if this.read(cx).held != held {
            this.update(cx, |pane, cx| {
                pane.held = held;
                cx.notify();
            });
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
    /// was.
    fn act(&self, command: PaneCommand, cx: &mut Context<Self>) {
        crate::shell::pane_command(cx.entity(), command, cx);
    }

    /// Hears a press anywhere in the pane, whatever takes it there and
    /// whether it stops or not: in the capture phase, and over all the pane
    /// holds, as what occludes (an agent's card, a skeleton's shield) hides
    /// the pane's root from the hit test. Its hitbox leaves the pointer to
    /// what lies beneath, and overlays drawn over the pane cover it.
    fn press_catcher(&self, cx: &Context<Self>) -> impl IntoElement {
        let (pane, on_entered) = (cx.weak_entity(), self.on_entered.clone());
        div()
            .absolute()
            .inset_0()
            .capture_any_mouse_down(move |_, window, cx| {
                if let Some(pane) = pane.upgrade() {
                    on_entered(pane, window, cx);
                }
            })
    }

    /// The chat on its way in, drawn over the column where it is now, laid
    /// out at the size it lands at. The elements around it carry no id, so
    /// the chat keeps its element path, and its keyed state, as it lands.
    fn flying_chat(&self, cx: &App) -> AnyElement {
        let at = self.chat_drawn(cx);
        let size = self.conversation_bounds().size;
        let session = self.session_id.clone().unwrap_or_default();
        div()
            .size_full()
            .child(gpui::deferred(
                gpui::anchored()
                    .position_mode(gpui::AnchoredPositionMode::Window)
                    .position(at.origin)
                    .child(
                        div()
                            .debug_selector(move || format!("flying-chat-{session}"))
                            .w(at.size.width)
                            .h(at.size.height)
                            .overflow_hidden()
                            // Over the other pane the column's paper would
                            // let that pane's chat through.
                            .bg(crate::material::opaque_canvas(cx)
                                .blend(crate::material::content_surface(cx)))
                            .shadow_sm()
                            .child(div().w(size.width).h(size.height).child(self.frame.clone())),
                    ),
            ))
            .into_any_element()
    }
}

/// How far a chat that set off `at` has come into its pane, from 0 to 1.
fn flight_progress(at: Instant, cx: &App) -> f32 {
    crate::motion::Curve::new(GLIDE)
        .progress(cx.background_executor().now().saturating_duration_since(at))
}

impl EventEmitter<PanelEvent> for ThreadPanel {}

impl Focusable for ThreadPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ThreadPanel {
    /// A pane beside others waits for a thread rather than offering the
    /// empty workspace's way to start one.
    fn waiting(&self) -> bool {
        self.among_others && (self.session_id.is_none() || self.awaiting)
    }

    /// The pane's conversation: its chat, or the waiting hint over a pane
    /// beside others with no thread yet, the two cross-fading. The drawer
    /// draws it beside its panel; the pane changes only as a chat's flight
    /// lands or a closed thread has faded out.
    pub(super) fn conversation(
        this: &Entity<Self>,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        const WAITING: usize = 0;
        const CHAT: usize = 1;
        let (waiting, awaiting, fading_out, motion) = {
            let pane = this.read(cx);
            (
                pane.waiting(),
                pane.awaiting,
                pane.fading_out,
                pane.motion.clone(),
            )
        };
        let content = motion
            .layers(
                "pane-content",
                if waiting { WAITING } else { CHAT },
                2,
                GLIDE,
                window,
                cx,
            )
            .fill();
        let content = if awaiting {
            content.stage(CHAT)
        } else {
            content
        };
        let mut layers = Vec::new();
        if content.shows(WAITING) {
            // A waiting pane draws no chat header, so its close button sits in
            // its corner instead.
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
                        div()
                            .absolute()
                            .top(spacing)
                            .right(spacing)
                            .child(close_button("waiting-pane-close", this.downgrade())),
                    )
                    .into_any_element(),
            ));
        }
        if content.shows(CHAT) {
            let pane = this.read(cx);
            let flying = !cx.reduce_motion()
                && pane
                    .flight
                    .is_some_and(|(_, at)| flight_progress(at, cx) < 1.);
            let landed = !flying && pane.flight.is_some();
            let chat = if flying {
                window.request_animation_frame();
                pane.flying_chat(cx)
            } else {
                pane.frame.clone().into_any_element()
            };
            layers.push((CHAT, chat));
            if landed {
                this.update(cx, |pane, _| pane.flight = None);
            }
        } else if fading_out {
            // The closed thread has faded out: the chat lets it go.
            this.update(cx, |pane, cx| {
                pane.fading_out = false;
                cx.defer_in(window, |panel, _, cx| {
                    if panel.session_id.is_none() && !panel.fading_out {
                        panel.hold(None, cx);
                        panel.chat.update(cx, |chat, cx| chat.set_session(None, cx));
                    }
                });
            });
        }
        content.render(layers).size_full().into_any_element()
    }
}

impl Render for ThreadPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bounds = self.bounds.clone();
        let held = self.held;
        if self.awaiting && self.chat.read(cx).appears(cx) {
            self.await_chat(false, cx);
        }
        if self.awaiting {
            window.request_animation_frame();
        }
        let waiting = self.waiting();
        // A slot gliding below the dock's slot floor runs past its split's
        // end, where the split's frame clips it: the pane's bounds are the
        // part that shows. Content held wider than the slot is clipped by
        // the tab group's content frame.
        div()
            .relative()
            .size_full()
            // A waiting pane has no composer to hold the keyboard, so the
            // pane holds it, and pane commands and Escape reach it there.
            // Drawn over the column, the pane is outside its tab group, which
            // tracks the pane's focus in the slot, so it tracks it itself and
            // the keyboard inside it is still the pane's.
            .when(waiting || self.zoom.overlaid, |pane| {
                pane.track_focus(&self.focus_handle)
            })
            .when(waiting, |pane| pane.key_context(WAITING_CONTEXT))
            .on_prepaint(move |painted, window, _| {
                bounds.set(painted.intersect(&window.content_mask().bounds));
            })
            .on_action(cx.listener(|this, _: &SplitRight, _, cx| {
                this.act(PaneCommand::Split(gpui_base::Placement::Right), cx)
            }))
            .on_action(cx.listener(|this, _: &SplitDown, _, cx| {
                this.act(PaneCommand::Split(gpui_base::Placement::Bottom), cx)
            }))
            .on_action(cx.listener(|this, _: &ClosePane, _, cx| this.act(PaneCommand::Close, cx)))
            .on_action(cx.listener(|this, _: &CloseOtherPanes, _, cx| {
                this.act(PaneCommand::CloseOthers, cx)
            }))
            .on_action(cx.listener(|this, _: &ZoomPane, _, cx| this.act(PaneCommand::Zoom, cx)))
            .on_action(
                cx.listener(|this, swap: &SwapPane, _, cx| this.act(PaneCommand::Swap(swap.0), cx)),
            )
            .on_action(cx.listener(|this, to: &MoveThreadToPane, _, cx| {
                this.act(PaneCommand::Move(to.0), cx)
            }))
            .child(
                // Laid out at the width the dock gives it in this frame:
                // the panel's room comes from it, before the conversation is
                // laid out in what is left. Clipped, as the panel slides past
                // the pane's edge.
                div()
                    .overflow_hidden()
                    .map(|content| match held.size.width {
                        Some(width) => content.w(width).flex_none(),
                        None => content.w_full(),
                    })
                    .map(|content| match held.size.height {
                        Some(height) => content.h(height).flex_none(),
                        None => content.h_full(),
                    })
                    .opacity(held.opacity)
                    .child(crate::motion::faded::faded(
                        held.opacity,
                        self.split.clone(),
                    )),
            )
            .when(self.closing, |pane| pane.child(crate::motion::shield()))
            .child(self.press_catcher(cx))
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

/// A pane's close button, at the right end of its chat header or in the
/// corner of a waiting pane: the pane glides out, as the pane menu's row
/// closes it.
pub(crate) fn close_button(id: &'static str, pane: WeakEntity<ThreadPanel>) -> Button {
    crate::widgets::button::close_button(id, crate::tr!("panes.close")).on_click(move |_, _, cx| {
        if let Some(pane) = pane.upgrade() {
            crate::shell::pane_command(pane, PaneCommand::Close, cx);
        }
    })
}

/// Where a thread dragged over the column would land: the slot it would
/// open as or replace, in window coordinates, `None` while it would land
/// nowhere.
pub(crate) type DropTarget = Rc<dyn Fn(&Window, &App) -> Option<Bounds<Pixels>>>;

/// The panes' appearance: the area is the chat column's reading surface.
///
/// A pane opening or closing glides its split's slots (see [`Glides`]), a
/// zoomed pane grows over the column out of its slot (see [`Zoom`]), a
/// layout is put in while the column is faded out (see [`LayoutSwap`]), and
/// the seams glide when the window jumps to another size
/// ([`Glides::resize`]); a window whose edge is dragged is followed at once.
/// Two panes that swap conversations carry each across to the other
/// ([`ThreadPanel::replace_chat`]).
///
/// The dock re-parents a group whenever the tree around it changes: a
/// lone group sits in the area's frame, a split puts it under
/// `("dock-split", node)`, a merge or a close lifts it back out, and a layout
/// builds every group anew. Element state is keyed by the full id path from
/// the window root, so keyed state in a pane that moves in the tree starts
/// over. What must not — a pane's waiting hint, its header's scroll — lives
/// with the pane's and its chat's entities instead (see
/// [`crate::motion::owned`]). A zoom moves no group, only the zoomed pane's
/// own view.
pub(crate) struct ThreadPanes {
    drop_target: DropTarget,
    handle_held: HandleHeld,
}

/// Set in every frame that draws a split's handle held by the pointer: the
/// handle's axis and where the pointer was, for [`held_split`].
pub(crate) type HandleHeld = Rc<Cell<Option<(Axis, Point<Pixels>)>>>;

impl ThreadPanes {
    pub(crate) fn new(drop_target: DropTarget, handle_held: HandleHeld) -> Self {
        Self {
            drop_target,
            handle_held,
        }
    }
}

impl DockAreaRenderer for ThreadPanes {
    /// The area draws the one placeholder of a dragged thread's drop, over
    /// every pane; see [`drop_placeholder`]. The reading surface beneath the
    /// panes is the chat column's, which runs on under the window's seam.
    fn frame(&self, _: &mut Window, _: &mut App) -> Stateful<Div> {
        let drop_target = self.drop_target.clone();
        div()
            .id("thread-panes")
            .child(crate::motion::Sampled::new(move |window, cx| {
                let target = drop_target(window, cx);
                drop_placeholder(target, window, cx)
            }))
    }

    fn tab_group_renderer(&self) -> Rc<dyn TabGroupRenderer> {
        Rc::new(ThreadTabs)
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

/// The pane a dragged thread would open as, or the pane it would replace.
///
/// Drawn once for the whole area rather than through the tab groups'
/// `render_drop_indicator`: gpui-base's `TabGroup::render` calls that under
/// each group's own element path, and stops calling it once the drag leaves
/// the group, lands or is cancelled, so a group's placeholder could neither
/// travel to another pane nor leave. This one fades in where the drop first
/// lands, springs between the edges and middles of one pane and on to the
/// next pane, and fades out where it last could land: over a spot where the
/// drop cannot land, once the drag leaves the column, and as the drop lands
/// or is cancelled. It is built once the area's frame has been rendered, so
/// it reads the groups' drags as they are now, and painted after the panes,
/// over them.
fn drop_placeholder(
    target: Option<Bounds<Pixels>>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let id = "pane-drop-placeholder";
    let Some(shown) = crate::motion::shown(id, target, SHOW, window, cx) else {
        return Empty.into_any_element();
    };
    // Coming in from nothing it starts where it lands; coming back while it
    // fades out it travels from where it was.
    let travel = shown.leaving() || shown.progress() > 0.;
    let to = shown.value;
    let mut spring = |channel, target| -> Pixels {
        gpui_base::motion::spring(
            (id, channel),
            target,
            gpui_base::motion::Spring::new(GLIDE)
                .with_epsilon(0.25)
                .with_travel(travel),
            window,
            cx,
        )
    };
    let left = spring("left", to.left());
    let top = spring("top", to.top());
    let width = spring("width", to.size.width);
    let height = spring("height", to.size.height);
    let inset = cx.theme().tokens.spacing.sm;
    let accent = cx.theme().primary;
    // No hitbox and no shield: the drop is the group's beneath it.
    gpui::deferred(
        gpui::anchored()
            .position_mode(gpui::AnchoredPositionMode::Window)
            .position(gpui::point(left + inset, top + inset))
            .child(
                div()
                    .debug_selector(|| "pane-drop-indicator".into())
                    .w((width - inset * 2.).max(px(0.)))
                    .h((height - inset * 2.).max(px(0.)))
                    .opacity(shown.progress())
                    .rounded(crate::material::radius_card(cx))
                    .border_1()
                    .border_color(accent.opacity(0.5))
                    .bg(accent.opacity(0.1)),
            ),
    )
    .into_any_element()
}

/// A pane's thread wears its own header, so the group draws no tab bar.
struct ThreadTabs;

impl TabGroupRenderer for ThreadTabs {
    fn render_tab_bar(&self, _: &TabGroupContext, _: &mut Window, _: &mut App) -> AnyElement {
        Empty.into_any_element()
    }

    /// The pane drawn over the column leaves a placeholder of its slot's size
    /// here (see [`Zoom`]). Over the pane goes the soft fill that answers a
    /// hovered row: it takes no pointer.
    fn render_active_panel(
        &self,
        panel: AnyView,
        _: &TabGroupContext,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let Ok(pane) = panel.clone().downcast::<ThreadPanel>() else {
            return panel.into_any_element();
        };
        let (zoom, slot, motion, hovered) = {
            let pane = pane.read(cx);
            (
                pane.zoom,
                pane.slot.clone(),
                pane.motion.clone(),
                pane.row_hovered,
            )
        };
        let fill = cx.theme().list_active.opacity(ROW_HOVERED_FILL);
        let fill = motion.transition(
            "row-hovered",
            if hovered { fill } else { fill.opacity(0.) },
            GLIDE,
            window,
            cx,
        );
        div()
            .relative()
            .size_full()
            .on_prepaint(move |painted, window, _| {
                slot.set(painted.intersect(&window.content_mask().bounds))
            })
            .map(|this| match zoom.overlaid {
                true => this.child(div().size_full()),
                false => this.child(panel),
            })
            .when(fill.a > 0., |this| {
                this.child(
                    div()
                        .debug_selector(|| "pane-row-hovered".into())
                        .absolute()
                        .inset_0()
                        .bg(fill),
                )
            })
            .into_any_element()
    }
}

/// Whether `now` and `planned`, drawn at `size`, put the same panes in the
/// same places: the same splits, their slots a pixel apart at most, each
/// group's panels paired by `same`.
pub(crate) fn same_layout(
    now: &PaneNode,
    planned: &PaneNode,
    same: &impl Fn(PanelId, PanelId) -> bool,
    size: Size<Pixels>,
) -> bool {
    match (now.kind(), planned.kind()) {
        (
            PaneRef::Tabs { panels: now, .. },
            PaneRef::Tabs {
                panels: planned, ..
            },
        ) => {
            now.len() == planned.len()
                && now
                    .iter()
                    .zip(planned)
                    .all(|(now, planned)| same(*now, *planned))
        }
        (
            PaneRef::Split {
                axis,
                children: now,
                sizes: now_sizes,
            },
            PaneRef::Split {
                axis: planned_axis,
                children: planned,
                sizes: planned_sizes,
            },
        ) if axis == planned_axis && now.len() == planned.len() => {
            let extent = size.along(axis);
            let shares = glide::tree_shares(now_sizes, extent);
            let planned_shares = glide::tree_shares(planned_sizes, extent);
            now.iter()
                .zip(planned)
                .zip(shares.iter().zip(&planned_shares))
                .all(|((now, planned), (share, planned_share))| {
                    (extent * (share - planned_share)).abs() < px(1.)
                        && same_layout(
                            now,
                            planned,
                            same,
                            size.apply_along(axis, |_| extent * *share),
                        )
                })
        }
        _ => false,
    }
}

/// Where each pane of `node` lies when `node` fills `area`, in the layout's
/// order.
pub(crate) fn pane_rects(
    node: &PaneNode,
    area: Bounds<Pixels>,
    rects: &mut Vec<(PanelId, Bounds<Pixels>)>,
) {
    match node.kind() {
        PaneRef::Tabs { panels, active_ix } => {
            rects.extend(panels.get(active_ix).map(|panel| (*panel, area)));
        }
        PaneRef::Split {
            axis,
            children,
            sizes,
        } => {
            let extent = area.size.along(axis);
            let mut origin = area.origin;
            for (child, share) in children.iter().zip(glide::tree_shares(sizes, extent)) {
                let length = extent * share;
                let size = area.size.apply_along(axis, |_| length);
                pane_rects(child, Bounds::new(origin, size), rects);
                origin = origin.apply_along(axis, |at| at + length);
            }
        }
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
            .enumerate()
            .map(|(ix, child)| min_width(child, row_edges(edges, ix), pane_min))
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
                    Axis::Vertical => row_edges(edges, ix),
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

/// The edges row `ix` of stacked ones keeps: the window's chrome sits in its
/// top corners, so only the top row's header reaches it.
fn row_edges(edges: WindowEdges, ix: usize) -> WindowEdges {
    WindowEdges {
        left: edges.left && ix == 0,
        right: edges.right && ix == 0,
    }
}

/// The pane nearest `from` on `side` among those beside it across that
/// axis.
///
/// Local because gpui-base's dock has no keyboard navigation between groups
/// (see `AppShell::focus_pane`).
pub(crate) fn neighbour(
    from: Bounds<Pixels>,
    panes: &[Bounds<Pixels>],
    side: Side,
) -> Option<usize> {
    let across_rows = |to: &Bounds<Pixels>| to.top() < from.bottom() && to.bottom() > from.top();
    let across_columns = |to: &Bounds<Pixels>| to.left() < from.right() && to.right() > from.left();
    let gap = |to: &Bounds<Pixels>| -> Option<f32> {
        let (gap, beside) = match side {
            Side::Left => (from.left() - to.right(), across_rows(to)),
            Side::Right => (to.left() - from.right(), across_rows(to)),
            Side::Up => (from.top() - to.bottom(), across_columns(to)),
            Side::Down => (to.top() - from.bottom(), across_columns(to)),
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
