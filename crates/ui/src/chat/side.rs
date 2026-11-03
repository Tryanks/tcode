//! An agent opened from its lead, shown inside the lead's own chat: on a
//! card in the side rail's place where the body has room for it beside the
//! lead's column, over the lead's timeline and composer otherwise. The
//! lead's header stays; the agent has a header of its own. The store records
//! which agent is beside which lead ([`WorkspaceStore::side_thread_for`]);
//! the lead's chat hosts it, holding the agent's thread open and its
//! timeline leased while it draws it.

use std::time::Duration;
#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
#[cfg(target_family = "wasm")]
use web_time::Instant;

use gpui::{
    AnyElement, App, AppContext as _, Context, DragMoveEvent, Entity, EntityId,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    WeakEntity, Window, div, prelude::FluentBuilder as _, px, relative,
};
use gpui_base::{ResizeHandleContext, ResizeHandleState};

use super::ChatView;
use crate::icon::IconName;
use crate::material::CHAT_RAIL_GAP;
use crate::motion::Colour;
use crate::motion::owned::Owned;
use crate::sizing::Sizable as _;
use crate::store::WorkspaceStore;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};

gpui::actions!(tcode_chat, [CloseSide]);

/// The key context of an agent shown beside its lead, where Escape closes it.
pub(crate) const SIDE_CONTEXT: &str = "ChatSide";

/// The share of the body an agent's card beside its lead takes until its
/// edge is dragged.
const SIDE_SHARE: f32 = 1. / 3.;

/// How long the agent takes to slide over the lead, or off it.
const SLIDE: Duration = Duration::from_millis(250);

/// How long one agent beside the lead takes to cross-fade into another.
const SWAP: Duration = Duration::from_millis(200);

/// How long what the side header shows only some of the time takes to come
/// in or go, and the agent's card to come into the rail's place or leave it.
const SHOW: Duration = Duration::from_millis(200);

/// The response of the spring the agent's card grows out of what opened it
/// along, and shrinks back into it, as gpui-base's `Spring::new` takes one:
/// critically damped, so it never passes its place.
const EXPAND: Duration = Duration::from_millis(400);

/// How near its end the card's growth comes before it is there.
const EXPAND_REST: f32 = 0.001;

/// The share of the card's growth over which what it holds fades in, at the
/// end of it.
const EXPAND_CONTENT: f32 = 0.4;

/// How long what the card holds takes to fade out as it closes, before the
/// card shrinks back.
const EXPAND_FADE: Duration = Duration::from_millis(120);

/// The grip of the card's edge: the sheet grabber's length, standing.
const GRIP_LENGTH: f32 = 36.;
const GRIP_WIDTH: f32 = 4.;

/// Where an agent beside its lead is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SideMode {
    /// On a card in the side rail's place, right of the lead's column.
    Slot,
    /// Over the lead's timeline and composer, under the lead's header.
    Overlay,
}

/// An agent's chat hosted by its lead's.
struct SideHost {
    session_id: String,
    /// The lead's thread it was opened beside.
    lead: String,
    chat: Entity<ChatView>,
    /// Whether the host opened the thread, and so closes it.
    opened: bool,
    /// What it was opened from, in the lead's [`ChatView::side_origins`],
    /// which it shrinks back into as it closes.
    origin: Option<SharedString>,
}

/// The agent's card growing out of what opened it, or shrinking back.
struct Expand {
    /// Its key in the lead's [`ChatView::side_origins`].
    origin: SharedString,
    /// Where it was last on screen, and the radius of its corners.
    from: Option<(gpui::Bounds<gpui::Pixels>, gpui::Pixels)>,
    phase: ExpandPhase,
}

#[derive(Clone, Copy)]
enum ExpandPhase {
    /// Growing out of the origin since `at`, from `from` of the way.
    Open { at: Instant, from: f32 },
    /// Closed at `at`, `grown` of the way out with what it holds `shown` of
    /// the way in: that fades out, then the card shrinks back into the
    /// origin (`Some(true)`), or fades and slides away as it does with no
    /// origin where that is nowhere on screen then (`Some(false)`).
    Closing {
        at: Instant,
        grown: f32,
        shown: f32,
        back: Option<bool>,
    },
}

/// How far a critically damped spring with gpui-base's `Spring::new(EXPAND)`
/// response has still to go, `elapsed` after it left from rest.
///
/// gpui-base's spring is stepped frame by frame toward the target it is
/// given on a frame, so a phase can only start on a frame. The card's phases
/// are timed from when they start (its shrink, from the end of the fade
/// before it), so this is the same spring's step response, in closed form.
fn expand_left(elapsed: Duration) -> f32 {
    let omega = std::f32::consts::TAU / EXPAND.as_secs_f32();
    let t = omega * elapsed.as_secs_f32();
    (1. + t) * (-t).exp()
}

impl Expand {
    /// How far out of the origin it is at `now`, and how far in what it
    /// holds is.
    fn at(&self, now: Instant) -> (f32, f32) {
        let content =
            |growth: f32| ((growth - (1. - EXPAND_CONTENT)) / EXPAND_CONTENT).clamp(0., 1.);
        match self.phase {
            ExpandPhase::Open { at, from } => {
                let growth = 1. - (1. - from) * expand_left(now.saturating_duration_since(at));
                (growth, content(growth))
            }
            ExpandPhase::Closing {
                at,
                grown,
                shown,
                back,
            } => {
                let since = now.saturating_duration_since(at);
                let shown = shown * (1. - since.as_secs_f32() / EXPAND_FADE.as_secs_f32()).max(0.);
                let growth = match back {
                    Some(true) => grown * expand_left(since.saturating_sub(EXPAND_FADE)),
                    _ => grown,
                };
                (growth, shown)
            }
        }
    }
}

/// What the lead's chat keeps of the agent beside it.
pub(super) struct Side {
    /// The agent shown.
    host: Option<SideHost>,
    /// The agent last shown, drawn until it has slid or faded away.
    leaving: Option<SideHost>,
    /// The share of the body the agent's card takes, as its edge was last
    /// dragged; kept by the view only, never saved.
    share: f32,
    /// Where the agent drawn was last drawn: a leaving one stays there.
    slot: SideMode,
    sync_pending: bool,
    /// The card on its way out of what opened it, or back into it.
    expand: Option<Expand>,
}

impl Side {
    pub(super) fn new() -> Self {
        Self {
            host: None,
            leaving: None,
            share: SIDE_SHARE,
            slot: SideMode::Slot,
            sync_pending: false,
            expand: None,
        }
    }
}

/// A drag of the edge of the agent's card beside the lead `0`, which only
/// that lead's chat follows.
#[derive(Clone, Copy)]
struct SideEdgeDrag(EntityId);

impl Render for SideEdgeDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// One frame of the agent beside the thread: see [`ChatView::render_side`].
#[derive(Default)]
pub(super) struct SideFrame {
    /// Its card in the rail's place, while it is there or fading out of it.
    pub(super) card: Option<AnyElement>,
    /// It over the lead, while it is there or sliding on or off.
    overlay: Option<AnyElement>,
    /// How far over the lead it is.
    over: f32,
    /// Whether what is over the lead is leaving it.
    leaving: bool,
    /// How far over the lead it has slid, and how far the lead has moved
    /// left under it: a card growing out of what opened it, or going back
    /// into it, covers the lead where it stands.
    slide: f32,
    shift: f32,
}

/// Where the card comes from and how far it has grown from there: see
/// [`crate::motion::expand::Morph::from`].
type Morphing = (Option<(gpui::Bounds<gpui::Pixels>, gpui::Pixels)>, f32);

/// This frame of the card's way out of what opened it, or back into it.
struct ExpandFrame {
    /// Where it comes from and how far it has grown, while it moves.
    morph: Option<Morphing>,
    /// How far in what it holds is.
    content: f32,
    /// How far in the card is, where this rather than the body's layout
    /// says.
    presence: Option<f32>,
    /// Whether it leaves as it does with no origin, `presence` of the way
    /// in.
    fading: bool,
}

impl Default for ExpandFrame {
    fn default() -> Self {
        Self {
            morph: None,
            content: 1.,
            presence: None,
            fading: false,
        }
    }
}

/// What makes a chat an agent's shown beside its lead.
pub(super) struct SideOf {
    lead: WeakEntity<ChatView>,
    lead_session: String,
    /// Where the lead draws it, which its header follows.
    mode: SideMode,
}

impl ChatView {
    /// A chat for `session_id` shown beside `lead`, which hosts it: no
    /// header of the lead's kind, no terminal drawer, no side of its own.
    fn new_side(
        workspace_store: Entity<WorkspaceStore>,
        window_state: Entity<crate::window_state::WindowState>,
        lead: WeakEntity<ChatView>,
        lead_session: String,
        session_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut chat = Self::new(workspace_store, window_state, window, cx);
        chat.side_of = Some(SideOf {
            lead,
            lead_session,
            mode: SideMode::Slot,
        });
        chat.set_session(Some(session_id), cx);
        chat
    }

    /// Whether the view is an agent's shown beside its lead.
    pub(super) fn is_side(&self) -> bool {
        self.side_of.is_some()
    }

    /// Whether the view is an agent drawn on the card beside its lead, which
    /// paints the surface under it.
    pub(super) fn on_side_card(&self) -> bool {
        self.side_of
            .as_ref()
            .is_some_and(|of| of.mode == SideMode::Slot)
    }

    /// The agent shown beside the thread.
    pub(crate) fn side_session_id(&self) -> Option<&str> {
        self.side.host.as_ref().map(|host| host.session_id.as_str())
    }

    /// The share of the body the agent beside the thread takes in the
    /// rail's place, if one is shown: never in the compact layout.
    pub(super) fn side_share(&self, cx: &App) -> Option<f32> {
        (self.side.host.is_some() && !self.window_state.read(cx).compact).then_some(self.side.share)
    }

    /// The agent the store records beside this view's thread, where the
    /// view hosts one: never on an agent's own view, nor in the compact
    /// layout, which shows one thread at a time.
    fn wanted_side(&self, cx: &App) -> Option<String> {
        if self.side_of.is_some() || self.window_state.read(cx).compact {
            return None;
        }
        let lead = self.session_id.as_deref()?;
        self.workspace_store
            .read(cx)
            .side_thread_for(lead)
            .filter(|side| side != lead)
    }

    /// Whether the agent hosted, beside the thread it was opened beside, is
    /// the one [`Self::wanted_side`] names.
    fn side_in_sync(&self, cx: &App) -> bool {
        let hosted = self
            .side
            .host
            .as_ref()
            .map(|host| (host.lead.as_str(), host.session_id.as_str()));
        let wanted = self.wanted_side(cx);
        hosted == self.session_id.as_deref().zip(wanted.as_deref())
    }

    /// Host what the store records beside the thread, once the update
    /// asking is over: the shell and a pane may be mid-update when a
    /// thread is bound, and closing the agent asks the shell whether a
    /// pane shows it.
    pub(super) fn request_side_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.side_of.is_some() || self.side.sync_pending || self.side_in_sync(cx) {
            return;
        }
        self.side.sync_pending = true;
        cx.defer_in(window, |this, window, cx| {
            this.side.sync_pending = false;
            this.sync_side(window, cx);
        });
    }

    /// Host the agent the store records beside the thread, and only it. The
    /// one place an agent's thread is opened and closed for its lead, and
    /// its timeline leased and released: opened (unless already open) and
    /// leased as it comes; closed (unless the view did not open it, or a
    /// pane shows it now) as it goes, its lease kept until it has left.
    fn sync_side(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.side_in_sync(cx) {
            return;
        }
        let wanted = self.wanted_side(cx);
        let store = self.workspace_store.clone();
        if let Some(host) = self.side.host.take() {
            close_side_thread(&store, &host, cx);
            // Out of the compact layout, and beside the same thread, it
            // leaves where it was drawn; otherwise there is nothing for it
            // to leave from.
            let stays = !self.window_state.read(cx).compact
                && Some(host.lead.as_str()) == self.session_id.as_deref();
            if stays {
                if let Some(earlier) = self.side.leaving.replace(host) {
                    release_side_host(&store, earlier, cx);
                }
            } else {
                release_side_host(&store, host, cx);
                self.side.expand = None;
            }
        }
        if let (Some(id), Some(lead)) = (wanted, self.session_id.clone()) {
            let host = match self.side.leaving.take() {
                Some(host) if host.session_id == id && host.lead == lead => Some(host),
                other => {
                    self.side.leaving = other;
                    None
                }
            };
            let mut host = host.unwrap_or_else(|| {
                let (window_state, me) = (self.window_state.clone(), cx.entity().downgrade());
                let chat = cx.new(|cx| {
                    ChatView::new_side(
                        store.clone(),
                        window_state,
                        me,
                        lead.clone(),
                        id.clone(),
                        window,
                        cx,
                    )
                });
                store.update(cx, |store, _| store.lease_timeline(&id));
                SideHost {
                    session_id: id.clone(),
                    lead: lead.clone(),
                    chat,
                    opened: false,
                    origin: None,
                }
            });
            host.opened = store.update(cx, |store, _| {
                let opens = !store.is_thread_open(&id);
                if opens {
                    store.open_thread(&id);
                }
                opens
            });
            self.side.host = Some(host);
        }
        cx.notify();
    }

    /// The view goes: so does the agent it hosts, and the one leaving.
    pub(super) fn release_side(&mut self, cx: &mut App) {
        let store = self.workspace_store.clone();
        if let Some(host) = self.side.host.take() {
            close_side_thread(&store, &host, cx);
            release_side_host(&store, host, cx);
        }
        if let Some(host) = self.side.leaving.take() {
            release_side_host(&store, host, cx);
        }
    }

    /// Where what opens agents beside the thread is drawn.
    pub(crate) fn side_origins(&self) -> &crate::motion::expand::Origins {
        &self.side_origins
    }

    /// Show `id` beside the thread, with the keyboard, opened from `origin`
    /// in [`Self::side_origins`]: its card grows out of the origin where
    /// that is on screen and no agent is shown already, which it cross-fades
    /// over instead.
    pub(crate) fn open_side(
        &mut self,
        id: String,
        origin: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(lead) = self.session_id.clone() else {
            return;
        };
        let shown = self.side.host.is_some();
        self.workspace_store
            .update(cx, |store, cx| store.set_side_thread(&lead, Some(id), cx));
        self.sync_side(window, cx);
        if let Some(host) = &mut self.side.host {
            host.origin = origin.clone();
        }
        if !shown && self.side.host.is_some() {
            let from = origin
                .as_ref()
                .filter(|_| !cx.reduce_motion())
                .and_then(|origin| self.side_origins.resolve(origin));
            let now = cx.background_executor().now();
            match (origin, from) {
                (Some(origin), Some(from)) => {
                    // Out of an origin it is shrinking back into, it grows
                    // again from where it is.
                    let grown = self
                        .side
                        .expand
                        .as_ref()
                        .filter(|expand| expand.origin == origin)
                        .map_or(0., |expand| expand.at(now).0);
                    self.side.expand = Some(Expand {
                        origin,
                        from: Some(from),
                        phase: ExpandPhase::Open {
                            at: now,
                            from: grown,
                        },
                    });
                }
                _ => self.side.expand = None,
            }
        }
        // The view's own focus, which it hands on to its composer: drawn from
        // the first frame, where the composer is not until the thread loads.
        if let Some(host) = &self.side.host {
            let focus = host.chat.read(cx).focus_handle.clone();
            focus.focus(window, cx);
        }
    }

    /// Close the agent beside the thread; the keyboard it had comes back to
    /// the thread's composer.
    fn close_side(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(lead) = self.session_id.clone() else {
            return;
        };
        let had_keyboard = self
            .side
            .host
            .as_ref()
            .is_some_and(|host| host.chat.read(cx).focus_handle.contains_focused(window, cx));
        self.workspace_store
            .update(cx, |store, cx| store.set_side_thread(&lead, None, cx));
        self.sync_side(window, cx);
        // Back into what opened it, where it stays beside the lead to leave.
        let origin = self
            .side
            .leaving
            .as_ref()
            .and_then(|host| host.origin.clone())
            .filter(|_| !cx.reduce_motion());
        let now = cx.background_executor().now();
        let was = self.side.expand.take();
        self.side.expand = origin.map(|origin| {
            let was = was.filter(|expand| expand.origin == origin);
            let (grown, shown) = was.as_ref().map_or((1., 1.), |expand| expand.at(now));
            Expand {
                from: was.and_then(|expand| expand.from),
                origin,
                phase: ExpandPhase::Closing {
                    at: now,
                    grown,
                    shown,
                    back: None,
                },
            }
        });
        if had_keyboard {
            self.focus_composer(window, cx);
        }
    }

    /// This frame's agent beside the thread, laid out by the body's layout
    /// (`self.body`): its card in the rail's place, for the timeline to
    /// hold, and what slides over the lead, for [`Self::cover_with_side`].
    pub(super) fn render_side(&mut self, window: &mut Window, cx: &mut Context<Self>) -> SideFrame {
        if self.side_of.is_some() {
            return SideFrame::default();
        }
        let compact = self.window_state.read(cx).compact;
        let mode = if self.body.side() {
            SideMode::Slot
        } else {
            SideMode::Overlay
        };
        let open = self.side.host.is_some() && !compact;
        if open {
            self.side.slot = mode;
        }
        if compact {
            self.side.expand = None;
        }
        let presence = self.body.side_presence();
        let expand = self.step_expand(window, cx);
        // gpui-base has no slide-over for one surface replacing another; the
        // agent comes in over the lead as the pull request page comes in
        // over its list (`PullRequestsPanel::render`).
        let over = self.motion.appear(
            "side-overlay",
            open && mode == SideMode::Overlay,
            SLIDE,
            window,
            cx,
        );
        let swap = self.side.host.as_ref().map_or(1., |host| {
            self.motion
                .entered("side-swap", host.session_id.clone(), SWAP, window, cx)
        });
        let settled = match self.side.host {
            Some(_) => swap >= 1.,
            None => presence <= 0. && over <= 0. && self.side.expand.is_none(),
        };
        if (settled || compact)
            && let Some(left) = self.side.leaving.take()
        {
            let store = self.workspace_store.clone();
            cx.defer(move |cx| release_side_host(&store, left, cx));
        }

        // Drawn once a frame, in the one place it was last put; the other
        // place, still sliding or fading, holds an empty surface.
        let slot = self.side.slot;
        if let Some(host) = &self.side.host {
            host.chat.update(cx, |chat, _| {
                if let Some(of) = &mut chat.side_of {
                    of.mode = slot;
                }
            });
        }
        let mut live = Some(
            div()
                .size_full()
                .opacity(expand.content)
                .child(self.render_side_surface(swap, cx))
                .into_any_element(),
        );
        let mut take_live = |here: SideMode| {
            if slot == here { live.take() } else { None }
                .unwrap_or_else(|| div().size_full().into_any_element())
        };
        let morph = expand.morph;
        // The card stays while it fades out empty, so it never goes in the
        // frame the agent crosses over the lead; and while it shrinks back.
        let card = (presence > 0. || (morph.is_some() && slot == SideMode::Slot)).then(|| {
            let content = take_live(SideMode::Slot);
            let presence = expand.presence.unwrap_or(presence);
            self.render_side_card(content, presence, morph, cx)
        });
        // Drawn from the frame it is asked for, at nothing, so the keyboard
        // given to it has somewhere to be.
        let covering = morph.is_some() && slot == SideMode::Overlay;
        let expands = covering && !expand.fading;
        let overlay = (over > 0. || (open && mode == SideMode::Overlay) || covering).then(|| {
            let content = take_live(SideMode::Overlay);
            let surface = div()
                .debug_selector(|| "chat-side-cover".into())
                .bg(crate::material::content_surface(cx));
            crate::motion::expand::Morph::new(surface, px(0.), px(0.), content)
                .from(
                    morph.and_then(|morph| morph.0),
                    morph.map_or(1., |morph| morph.1),
                )
                .into_any_element()
        });
        let over = match (expands, covering) {
            (true, _) => morph.map_or(1., |morph| morph.1) * expand.content,
            // Leaving with no origin to go back into, it slides off the
            // lead as it would have without one.
            (false, true) => expand.presence.unwrap_or(over),
            (false, false) => over,
        };
        SideFrame {
            card,
            overlay,
            over,
            leaving: !(open && mode == SideMode::Overlay),
            slide: if expands { 1. } else { over },
            shift: if covering { 0. } else { over },
        }
    }

    /// The card's way out of what opened it or back into it, this frame:
    /// how far it has grown and from where, how far in what it holds is,
    /// and how far in the card is while it fades away instead.
    fn step_expand(&mut self, window: &mut Window, cx: &mut Context<Self>) -> ExpandFrame {
        let now = cx.background_executor().now();
        let Some(expand) = &mut self.side.expand else {
            return ExpandFrame::default();
        };
        let on_screen = self.side_origins.resolve(&expand.origin);
        if let Some(from) = on_screen {
            expand.from = Some(from);
        }
        // Once what the card holds has faded, it shrinks back into the
        // origin if that is on screen, and fades away otherwise.
        if let ExpandPhase::Closing { at, back, .. } = &mut expand.phase
            && back.is_none()
            && now.saturating_duration_since(*at) >= EXPAND_FADE
        {
            *back = Some(on_screen.is_some() && expand.from.is_some());
        }
        let (growth, content) = expand.at(now);
        let (frame, done) = match expand.phase {
            ExpandPhase::Open { .. } => (
                ExpandFrame {
                    morph: Some((expand.from, growth)),
                    content,
                    presence: Some(1.),
                    fading: false,
                },
                1. - growth < EXPAND_REST,
            ),
            ExpandPhase::Closing {
                at,
                back: Some(false),
                ..
            } => {
                let gone = crate::motion::Curve::new(SHOW).progress(
                    now.saturating_duration_since(at)
                        .saturating_sub(EXPAND_FADE),
                );
                (
                    ExpandFrame {
                        morph: Some((None, 1.)),
                        content: 0.,
                        presence: Some(1. - gone),
                        fading: true,
                    },
                    gone >= 1.,
                )
            }
            ExpandPhase::Closing { .. } => (
                ExpandFrame {
                    morph: Some((expand.from, growth)),
                    content,
                    presence: Some(1.),
                    fading: false,
                },
                growth < EXPAND_REST,
            ),
        };
        if done {
            let closed = matches!(expand.phase, ExpandPhase::Closing { .. });
            self.side.expand = None;
            return if closed {
                ExpandFrame {
                    presence: Some(0.),
                    ..ExpandFrame::default()
                }
            } else {
                ExpandFrame::default()
            };
        }
        window.request_animation_frame();
        frame
    }

    /// The agent's card in the rail's place, `presence` of the way in,
    /// holding `content`, on its way out of an origin or back into it as
    /// `morph` says, its leading edge dragged to widen or narrow it.
    fn render_side_card(
        &self,
        content: AnyElement,
        presence: f32,
        morph: Option<Morphing>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let shown = self.body.side();
        // gpui-base's resize handle straddles the leading edge of what holds
        // it; held by nothing as wide, it straddles the middle of the gap
        // left of the card, which the drag measures the card's width from.
        let edge = (shown && morph.is_none()).then(|| {
            let motion = self.motion.clone();
            let lead = cx.entity_id();
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(px(-CHAT_RAIL_GAP / 2.))
                .child(
                    gpui_base::resize_handle::<SideEdgeDrag, SideEdgeDrag>(
                        "chat-side-edge",
                        gpui::Axis::Horizontal,
                    )
                    .with_appearance(std::rc::Rc::new(
                        move |handle: &ResizeHandleContext, window: &mut Window, cx: &mut App| {
                            Some(side_grip(&motion, handle, window, cx))
                        },
                    ))
                    .on_drag(SideEdgeDrag(lead), |drag, _, _, cx| {
                        cx.stop_propagation();
                        cx.new(|_| *drag)
                    }),
                )
        });
        let surface = crate::material::card_raised(cx)
            .debug_selector(|| "chat-side-card".into())
            // The agent's timeline takes the wheel over the card, not the
            // lead's under it. gpui-base's page viewport gives a wheel to
            // the innermost one under the pointer, but the lead's does not
            // hold the card: drawn before it, it would scroll before the
            // agent's claims the wheel, had the card not blocked it.
            .occlude()
            .p_0()
            .gap_0();
        let card = crate::motion::expand::Morph::new(
            surface,
            crate::material::radius_card(cx),
            px(1.),
            content,
        )
        .from(
            morph.and_then(|morph| morph.0),
            morph.map_or(1., |morph| morph.1),
        );
        super::rail::slot(self.body.side_width(), presence, cx)
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<SideEdgeDrag>, _, cx| {
                    if event.drag(cx).0 != cx.entity_id() {
                        return;
                    }
                    let width =
                        event.bounds.right() - event.event.position.x - px(CHAT_RAIL_GAP / 2.);
                    let share = this.body.side_share_at(width);
                    if share != this.side.share {
                        this.side.share = share;
                        cx.notify();
                    }
                }),
            )
            .child(card)
            .children(edge)
            .when(!shown, |card| card.child(crate::motion::shield()))
            .into_any_element()
    }

    /// `main`, the thread's timeline, deliveries and composer, under the
    /// agent sliding over it in `side`. Always the same elements around
    /// `main`, so it keeps its place in the tree as an agent comes and goes.
    pub(super) fn cover_with_side(&self, main: AnyElement, side: SideFrame) -> AnyElement {
        if self.side_of.is_some() {
            return main;
        }
        let (over, shift, slide) = (side.over, side.shift, side.slide);
        div()
            .relative()
            .size_full()
            .overflow_hidden()
            .child(
                // The lead moves a quarter left under the agent sliding over
                // it, and is not drawn once covered.
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(relative(-0.25 * shift))
                    .right(relative(0.25 * shift))
                    .when(over >= 1., |lead| lead.invisible())
                    .child(main),
            )
            .when_some(side.overlay, |body, overlay| {
                body.child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(relative(1. - slide))
                        .right(relative(slide - 1.))
                        .opacity(if side.leaving || slide >= 1. {
                            1.
                        } else {
                            over
                        })
                        .occlude()
                        .child(overlay),
                )
            })
            // Neither takes input while one slides over the other.
            .when(over > 0. && over < 1., |body| {
                body.child(div().absolute().inset_0().occlude())
            })
            .into_any_element()
    }

    /// The agent drawn beside the thread, the one it replaced fading out
    /// under it, where Escape closes it.
    fn render_side_surface(&self, swap: f32, cx: &mut Context<Self>) -> AnyElement {
        let current = self.side.host.as_ref().or(self.side.leaving.as_ref());
        let under = self
            .side
            .host
            .as_ref()
            .and(self.side.leaving.as_ref())
            .filter(|_| swap < 1.);
        div()
            .size_full()
            .relative()
            .key_context(SIDE_CONTEXT)
            .on_action(cx.listener(|this, _: &CloseSide, window, cx| this.close_side(window, cx)))
            .children(under.map(|under| div().absolute().inset_0().child(under.chat.clone())))
            .children(current.map(|current| {
                div()
                    .absolute()
                    .inset_0()
                    .when(under.is_some(), |layer| layer.opacity(swap))
                    .child(current.chat.clone())
            }))
            .into_any_element()
    }

    /// An agent's own header beside its lead: Back at its start while it
    /// covers the lead, its state and title, and Close at its end while it
    /// sits beside the lead.
    pub(super) fn render_side_header(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let of = self.side_of.as_ref()?;
        let session = self.session_id.clone()?;
        let (heading, meta_title, lead_title) = {
            let store = self.workspace_store.read(cx);
            let heading =
                crate::agents_panel::Agents::of(store, &of.lead_session).heading(&session);
            (
                heading,
                store.thread_meta(&session).map(|meta| meta.title.clone()),
                store
                    .thread_meta(&of.lead_session)
                    .map(|meta| meta.title.clone())
                    .unwrap_or_default(),
            )
        };
        let title = heading
            .as_ref()
            .map(|heading| heading.title.clone())
            .or(meta_title)
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| crate::tr!("agents.untitled").into_owned());
        let execution = heading.as_ref().and_then(|heading| heading.execution);
        let key = heading.map_or_else(|| session.clone(), |heading| heading.key);

        let close = {
            let lead = of.lead.clone();
            move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| {
                let _ = lead.update(cx, |lead, cx| lead.close_side(window, cx));
            }
        };
        // Back while it covers the lead, Close while it sits beside it: one
        // folds out as the other folds in when the body crosses between.
        let back = self
            .motion
            .shown(
                "side-back",
                (of.mode == SideMode::Overlay).then_some(()),
                SHOW,
                window,
                cx,
            )
            .map(|shown| {
                let name: SharedString = crate::tr!("agents.back_to_parent", title = lead_title)
                    .into_owned()
                    .into();
                shown.fold_across(
                    Button::new("side-back")
                        .motion(&self.motion)
                        .debug_selector(|| "side-back".into())
                        .ghost()
                        .xsmall()
                        .icon(IconName::ArrowLeft)
                        .aria_label(name.clone())
                        .tooltip(name)
                        .on_click(close.clone()),
                )
            });
        let close = self
            .motion
            .shown(
                "side-close",
                (of.mode == SideMode::Slot).then_some(()),
                SHOW,
                window,
                cx,
            )
            .map(|shown| {
                let name: SharedString = crate::tr!("agents.close_side").into_owned().into();
                shown.fold_across(
                    Button::new("side-close")
                        .motion(&self.motion)
                        .debug_selector(|| "side-close".into())
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .aria_label(name.clone())
                        .tooltip(name)
                        .on_click(close),
                )
            });
        Some(
            crate::material::bar("chat-side-header", crate::material::Bar::Toolbar, false, cx)
                .debug_selector(|| "chat-side-header".into())
                .items_center()
                .children(back)
                .child(div().flex_none().child(crate::agents_panel::status_mark(
                    &key, execution, window, cx,
                )))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_body(false, cx)
                        .child(crate::widgets::fade_text(title)),
                )
                .children(close)
                .into_any_element(),
        )
    }
}

/// What the edge of the agent's card paints in the gap left of the card: no
/// line, for the gap is the divider, and a short pill centred in it, fading
/// in while the pointer is on the edge and firming while it holds it.
///
/// gpui-base's handle paints a hairline in the middle of its band; its
/// appearance is the caller's to replace, and this is laid out in that
/// one-pixel middle, centred on its leading side.
fn side_grip(
    motion: &Owned,
    handle: &ResizeHandleContext,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let fill = match handle.state() {
        ResizeHandleState::Idle => muted.opacity(0.),
        ResizeHandleState::Hovered => muted.opacity(0.3),
        ResizeHandleState::Pressed | ResizeHandleState::Dragging => muted.opacity(0.6),
    };
    let fill = motion.transition(
        "side-grip",
        Colour(fill),
        crate::material::HOVER,
        window,
        cx,
    );
    div()
        .flex_none()
        .h_full()
        .w(px(GRIP_WIDTH))
        .ml(px(-GRIP_WIDTH / 2.))
        .flex()
        .items_center()
        .child(
            div()
                .debug_selector(|| "chat-side-grip".into())
                .w_full()
                .h(px(GRIP_LENGTH))
                .rounded_full()
                .bg(fill.0),
        )
        .into_any_element()
}

/// Close `host`'s thread if the host opened it and no pane shows it now.
fn close_side_thread(store: &Entity<WorkspaceStore>, host: &SideHost, cx: &mut App) {
    if host.opened
        && store.read(cx).is_thread_open(&host.session_id)
        && !crate::shell::shown_in_pane(&host.session_id, cx)
    {
        store.update(cx, |store, cx| store.close_thread(&host.session_id, cx));
    }
}

/// Let go of `host`'s lease; its chat goes with it.
fn release_side_host(store: &Entity<WorkspaceStore>, host: SideHost, cx: &mut App) {
    store.update(cx, |store, cx| store.release_timeline(&host.session_id, cx));
}
