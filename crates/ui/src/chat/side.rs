//! An agent opened from its lead, shown inside the lead's own chat: on a
//! card in the side rail's place where the body has room for it beside the
//! lead's column, on a sheet raised over the lead's timeline and composer
//! otherwise, the one growing into the other as the body crosses between
//! them. The
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
use crate::material::CHAT_SIDE_GRIP;
use crate::motion::Colour;
use crate::motion::owned::Owned;
use crate::sizing::Sizable as _;
use crate::store::WorkspaceStore;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};

/// The chat an agent opened from what `chat` draws shows beside its thread
/// (an agent's row in its timeline or its rail): `chat` while it shows a
/// thread, or for an agent's chat beside its `lead`, the lead's, where the
/// agent opened takes its place. None in the compact layout, which shows
/// one thread at a time.
pub(crate) fn agent_host(
    chat: &WeakEntity<ChatView>,
    lead: Option<&WeakEntity<ChatView>>,
    shows_thread: bool,
    window_state: &Entity<crate::window_state::WindowState>,
    cx: &App,
) -> Option<Entity<ChatView>> {
    if window_state.read(cx).compact {
        return None;
    }
    match lead {
        Some(lead) => lead.upgrade(),
        None => shows_thread.then(|| chat.upgrade()).flatten(),
    }
}

impl ChatView {
    /// The chat an agent opened from what this view draws over its own
    /// timeline shows beside its thread: see [`agent_host`].
    pub(super) fn agent_host(&self, cx: &mut Context<Self>) -> Option<Entity<ChatView>> {
        agent_host(
            &cx.entity().downgrade(),
            self.side_of.as_ref().map(|of| &of.lead),
            self.session_id.is_some(),
            &self.window_state,
            cx,
        )
    }
}

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
/// along, or crosses to the other place along, as gpui-base's `Spring::new`
/// takes one: critically damped, so it never passes its place.
const EXPAND: Duration = Duration::from_millis(400);

/// The response of the same spring as the card shrinks back.
const EXPAND_BACK: Duration = Duration::from_millis(300);

/// How much of the spring's way is left when the card is there.
const EXPAND_REST: f32 = 0.001;

/// The share of its way by which the surface is opaque, growing.
const OPEN_SURFACE: f32 = 0.4;

/// The share of its way over which what it holds fades in, growing.
const OPEN_CONTENT: (f32, f32) = (0.3, 0.8);

/// The share of its way from which the surface and what it holds fade
/// together, shrinking: the surface is never an empty block.
const BACK_FADE: f32 = 0.45;

/// How long what it holds takes to fade out as it leaves with no origin on
/// screen.
const LEAVE_FADE: Duration = Duration::from_millis(120);

/// The share of a crossing's way at which what the surface holds takes the
/// size of the new place, and how far in it is then.
const CROSS_SWAP: f32 = 0.5;
const CROSS_LOW: f32 = 0.25;

/// The key the agent's surface records where it is drawn under, in the
/// lead's [`ChatView::side_origins`], so that crossing to the other place
/// starts from there.
const SURFACE: &str = "chat-side-surface";

/// The key what the surface holds records where it is laid out under.
const HELD: &str = "chat-side-held";

/// How far the sheet over the lead is inset from the sides of the lead's
/// body. At the top it is as far down as the slot, so the card and the sheet
/// share a top edge, and it ends where the slot does.
const SHEET_INSET: f32 = 8.;

/// How dark the scrim under the sheet is, as a share of a modal's
/// ([`crate::material::scrim`]): about 5% darker in light, 7.5% in dark. The
/// lead stays legible under it; only that it is not what the keyboard is
/// in is said.
const SHEET_SCRIM: f32 = 0.15;

/// The widest Back on the sheet gets with the lead's title in it, as the
/// way back to the lead in an agent's own header is.
const BACK_WIDEST: f32 = 160.;

/// The grip of the card's edge: the sheet grabber's length, standing.
const GRIP_LENGTH: f32 = 36.;
const GRIP_WIDTH: f32 = 4.;

/// Where an agent beside its lead is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SideMode {
    /// On a card in the side rail's place, right of the lead's column.
    Slot,
    /// On a sheet raised over the lead's timeline and composer, under the
    /// lead's header.
    Overlay,
}

/// An agent's chat hosted by its lead's.
struct SideHost {
    session_id: String,
    /// The lead's thread it was opened beside.
    lead: String,
    chat: Entity<ChatView>,
    frame: Entity<super::ChatFrame>,
    /// Whether the host opened the thread, and so closes it.
    opened: bool,
    /// What it was opened from, in the lead's [`ChatView::side_origins`],
    /// which it shrinks back into as it closes.
    origin: Option<SharedString>,
}

/// The agent's card growing out of what opened it, or shrinking back; or
/// crossing from where it was drawn to the other place.
struct Expand {
    /// Its key in the lead's [`ChatView::side_origins`]; none while it
    /// crosses from one place to the other.
    origin: Option<SharedString>,
    /// Where it was last on screen, and the radius of its corners.
    from: Option<(gpui::Bounds<gpui::Pixels>, gpui::Pixels)>,
    /// Where its frame ends rather than in its place: where it was drawn,
    /// partway, as it began to leave.
    held: Option<(gpui::Bounds<gpui::Pixels>, gpui::Pixels)>,
    /// Where what it holds stays laid out rather than at its place: the
    /// place it is crossing from, until [`CROSS_SWAP`].
    laid: Option<gpui::Bounds<gpui::Pixels>>,
    phase: ExpandPhase,
}

/// How far in the surface and what it holds are.
#[derive(Clone, Copy)]
struct Shown {
    surface: f32,
    content: f32,
}

impl Shown {
    const FULLY: Self = Self {
        surface: 1.,
        content: 1.,
    };
    const NOT: Self = Self {
        surface: 0.,
        content: 0.,
    };
}

#[derive(Clone, Copy)]
enum ExpandPhase {
    /// Growing since `at` from `grown` of the way out of the origin, or from
    /// where it was into the other place, as `shown` was then.
    Open {
        at: Instant,
        grown: f32,
        shown: Shown,
    },
    /// Shrinking since `at` from `grown` of the way out of the origin, as
    /// `shown` was then; `drawn` once a frame has been drawn since.
    Closing {
        at: Instant,
        grown: f32,
        shown: Shown,
        drawn: bool,
    },
    /// With its origin nowhere on screen as it closed at `at`: fading and
    /// sliding away as it does with no origin.
    Leaving { at: Instant, shown: Shown },
    /// Leaving `leaves` for the other place at `at`, as `shown` was then:
    /// drawn there this once, so where it starts from is where the body's
    /// layout has it now.
    Crossing {
        at: Instant,
        shown: Shown,
        leaves: SideMode,
    },
    /// With motion reduced: the agent fading into the place it is drawn in
    /// since `at`, an empty surface fading out of `leaves`.
    Swapping { at: Instant, leaves: SideMode },
}

/// How much of its way a critically damped spring with gpui-base's
/// `Spring::new(response)` has still to go, `elapsed` after it left from
/// rest.
///
/// gpui-base's spring is stepped frame by frame toward the target it is
/// given on a frame. The card's phases carry on from where another left
/// them and their fades read how far along the spring is, so this is the
/// same spring's step response, in closed form.
fn spring_left(elapsed: Duration, response: Duration) -> f32 {
    let omega = std::f32::consts::TAU / response.as_secs_f32();
    let t = omega * elapsed.as_secs_f32();
    (1. + t) * (-t).exp()
}

/// How far `way` is from `from` to `to`, from 0 to 1.
fn ramp(way: f32, from: f32, to: f32) -> f32 {
    ((way - from) / (to - from)).clamp(0., 1.)
}

fn mix(from: f32, to: f32, t: f32) -> f32 {
    from + (to - from) * t
}

impl Expand {
    /// How much of its spring's way it has still to go at `now`.
    fn left(&self, now: Instant) -> f32 {
        match self.phase {
            ExpandPhase::Open { at, .. } => spring_left(now.saturating_duration_since(at), EXPAND),
            ExpandPhase::Closing { at, .. } => {
                spring_left(now.saturating_duration_since(at), EXPAND_BACK)
            }
            _ => 1.,
        }
    }

    /// How far out of the origin it is at `now`, and how far in the surface
    /// and what it holds are.
    fn at(&self, now: Instant) -> (f32, Shown) {
        let left = self.left(now);
        let way = 1. - left;
        match self.phase {
            ExpandPhase::Open { grown, shown, .. } => {
                let surface = mix(shown.surface, 1., ramp(way, 0., OPEN_SURFACE));
                let content = if self.origin.is_some() {
                    let (from, to) = OPEN_CONTENT;
                    mix(shown.content, 1., ramp(way, from, to))
                } else if way < CROSS_SWAP {
                    mix(shown.content, CROSS_LOW, ramp(way, 0., CROSS_SWAP))
                } else {
                    mix(CROSS_LOW, 1., ramp(way, CROSS_SWAP, 1.))
                };
                (1. - (1. - grown) * left, Shown { surface, content })
            }
            ExpandPhase::Closing { grown, shown, .. } => {
                let fade = 1. - ramp(way, BACK_FADE, 1.);
                (
                    grown * left,
                    Shown {
                        surface: shown.surface * fade,
                        content: shown.content * fade,
                    },
                )
            }
            ExpandPhase::Leaving { at, shown } => {
                let since = now.saturating_duration_since(at).as_secs_f32();
                let out = (since / LEAVE_FADE.as_secs_f32()).min(1.);
                (
                    1.,
                    Shown {
                        content: shown.content * (1. - out),
                        ..shown
                    },
                )
            }
            ExpandPhase::Crossing { shown, .. } => (1., shown),
            ExpandPhase::Swapping { .. } => (1., Shown::FULLY),
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
    /// How wide its card last was beside the lead: with no agent the body
    /// lays out no width for one, and a card leaving is not measured again.
    width: gpui::Pixels,
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
            width: px(0.),
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
    /// Its card in the rail's place, while it is there or on its way in or
    /// out.
    pub(super) card: Option<AnyElement>,
    /// Its sheet over the lead, while it is there or on its way in or out.
    sheet: Option<AnyElement>,
    /// How far onto the lead the sheet has slid, and how far in it is.
    slide: f32,
    opacity: f32,
    /// The scrim over the lead, while something is over it, which leaves
    /// the lead inert.
    scrim: Option<gpui::Hsla>,
    /// How far above the body's bottom the lead's composer card ends, which
    /// the sheet ends with: see [`super::rail::Slots`].
    below_card: std::rc::Rc<std::cell::Cell<gpui::Pixels>>,
}

/// Where the surface comes from, how far it has grown from there, where it
/// rests instead of its place, where what it holds is laid out, and how far
/// in it is: see [`crate::motion::expand::Morph`].
#[derive(Clone, Copy)]
struct Morphing {
    from: Option<(gpui::Bounds<gpui::Pixels>, gpui::Pixels)>,
    growth: f32,
    held: Option<(gpui::Bounds<gpui::Pixels>, gpui::Pixels)>,
    laid: Option<gpui::Bounds<gpui::Pixels>>,
    surface: f32,
}

/// This frame of the card's way out of what opened it, or back into it,
/// or from one place to the other.
struct ExpandFrame {
    /// Where it comes from and how far it has grown, while it moves.
    morph: Option<Morphing>,
    /// How far in what it holds is.
    content: f32,
    /// How far in the surface is, where this rather than the body's layout
    /// says.
    presence: Option<f32>,
    /// Whether it leaves as it does with no origin, `presence` of the way
    /// in.
    fading: bool,
    /// Whether it is on its way from the other place.
    crossing: bool,
    /// With motion reduced, the place it is fading out of, empty, as it
    /// fades in where it is drawn.
    swap: Option<SideMode>,
}

impl Default for ExpandFrame {
    fn default() -> Self {
        Self {
            morph: None,
            content: 1.,
            presence: None,
            fading: false,
            crossing: false,
            swap: None,
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
        let mut chat = Self::new(workspace_store, window_state, Some(session_id), window, cx);
        chat.timeline
            .update(cx, |timeline, _| timeline.set_lead(lead.clone()));
        chat.rail.update(cx, |rail, _| rail.set_lead(lead.clone()));
        chat.side_of = Some(SideOf {
            lead,
            lead_session,
            mode: SideMode::Slot,
        });
        chat
    }

    /// Whether the view is an agent's shown beside its lead.
    pub(super) fn is_side(&self) -> bool {
        self.side_of.is_some()
    }

    /// The drawer of this view's pane shows `agent`, beside this thread, in
    /// place of what it showed. The drawer holds one thread: closing the
    /// agent's must not reveal the lead's.
    pub(super) fn show_agent_in_drawer(&mut self, agent: String, cx: &mut Context<Self>) {
        let shown = self.drawer_subject().map(str::to_owned);
        self.workspace_store.update(cx, |store, cx| {
            if shown.as_ref() != Some(&agent) {
                store.close_right_panel_for(shown.as_deref(), cx);
            }
        });
        self.drawer_agent = Some(agent);
        cx.notify();
    }

    /// The drawer goes back to this thread, closing on the agent it showed.
    pub(super) fn close_drawer_agent(&mut self, cx: &mut Context<Self>) {
        if let Some(agent) = self.drawer_agent.take() {
            self.workspace_store.update(cx, |store, cx| {
                store.close_right_panel_for(Some(&agent), cx)
            });
            cx.notify();
        }
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
            // Its diff slides out with it, and the lead's panel takes no
            // place. The compact layout draws no drawer to slide.
            if self.drawer_agent.as_ref() == Some(&host.session_id) {
                if self.window_state.read(cx).compact {
                    self.close_drawer_agent(cx);
                } else {
                    store.update(cx, |store, cx| {
                        store.close_right_panel_for(Some(&host.session_id), cx)
                    });
                }
            }
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
                    frame: chat.read(cx).frame(),
                    chat,
                    opened: false,
                    origin: None,
                }
            });
            host.opened = store.update(cx, |store, cx| {
                let opens = !store.is_thread_open(&id);
                if opens {
                    store.open_thread(&id, cx);
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

    /// Where what opens agents beside the thread is drawn by the view
    /// itself; the timeline's rows record theirs on their own.
    pub(crate) fn side_origins(&self) -> &crate::motion::expand::Origins {
        &self.side_origins
    }

    /// Where `origin` was drawn, by the view or by its timeline's rows.
    fn resolve_origin(&self, origin: &str) -> Option<(gpui::Bounds<gpui::Pixels>, gpui::Pixels)> {
        self.side_origins
            .resolve(origin)
            .or_else(|| self.timeline_origins.resolve(origin))
            .or_else(|| self.rail_origins.resolve(origin))
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
                .and_then(|origin| self.resolve_origin(origin));
            let now = cx.background_executor().now();
            match (origin, from) {
                (Some(origin), Some(from)) => {
                    // Out of an origin it is shrinking back into, it grows
                    // again from where it is, as it is.
                    let (grown, shown) = self
                        .side
                        .expand
                        .as_ref()
                        .filter(|expand| expand.origin.as_ref() == Some(&origin))
                        .map_or((0., Shown::NOT), |expand| expand.at(now));
                    self.side.expand = Some(Expand {
                        origin: Some(origin),
                        from: Some(from),
                        held: None,
                        laid: None,
                        phase: ExpandPhase::Open {
                            at: now,
                            grown,
                            shown,
                        },
                    });
                }
                _ => self.side.expand = None,
            }
        }
        self.focus_side(window, cx);
    }

    /// The agent beside the thread takes the keyboard, through the view's own
    /// focus, which it hands on to its composer: drawn from the first frame,
    /// where the composer is not until the thread loads.
    pub(crate) fn focus_side(&self, window: &mut Window, cx: &mut App) {
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
        // Back into what opened it, where it stays beside the lead to leave,
        // from where it is: partway from one place to the other, it starts
        // from there.
        let origin = self
            .side
            .leaving
            .as_ref()
            .and_then(|host| host.origin.clone())
            .filter(|_| !cx.reduce_motion());
        let now = cx.background_executor().now();
        let was = self.side.expand.take();
        let crossing = was
            .as_ref()
            .is_some_and(|expand| expand.origin.is_none() && !cx.reduce_motion());
        let drawn = self.side_origins.resolve(SURFACE);
        let laid = was.as_ref().and_then(|was| was.laid);
        let (from, grown, shown, held) = match was {
            Some(was) if was.origin.is_some() && was.origin == origin => {
                let (grown, shown) = was.at(now);
                (was.from, grown, shown, None)
            }
            Some(was) => (None, 1., was.at(now).1, drawn),
            None => (None, 1., Shown::FULLY, None),
        };
        self.side.expand = (origin.is_some() || crossing).then_some(Expand {
            origin,
            from,
            held,
            laid,
            phase: ExpandPhase::Closing {
                at: now,
                grown,
                shown,
                drawn: false,
            },
        });
        if had_keyboard {
            self.focus_composer(window, cx);
        }
    }

    /// This frame's agent beside the thread, laid out by the body's layout
    /// (`self.body`): its card in the rail's place, for the timeline to
    /// hold, or its sheet over the lead, for [`Self::cover_with_side`].
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
        if compact {
            self.side.expand = None;
        }
        if open {
            self.cross_to(mode, cx);
        }
        if self.body.side() {
            self.side.width = self.body.side_width();
        }
        let presence = self.body.side_presence();
        let expand = self.step_expand(window, cx);
        if open {
            self.side.slot = match self.side.expand {
                Some(Expand {
                    phase: ExpandPhase::Crossing { leaves, .. },
                    ..
                }) => leaves,
                _ => mode,
            };
        }
        let slot = self.side.slot;
        // gpui-base has no slide-over for one surface replacing another; the
        // agent's sheet comes in over the lead as the pull request page comes
        // in over its list (`PullRequestsPanel::render`).
        let over = self.motion.appear(
            "side-overlay",
            open && slot == SideMode::Overlay,
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

        // Drawn once a frame, in the one place it is: with motion reduced,
        // the place it fades out of holds an empty surface meanwhile.
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
        let empty = || div().size_full().into_any_element();
        let morph = expand.morph;
        // With motion reduced, how far in the surface drawn is, and how far
        // in the empty one it fades out of.
        let fade = expand.swap.map(|_| expand.presence.unwrap_or(1.));
        let ghost = |place: SideMode| expand.swap == Some(place);
        let card = if slot == SideMode::Slot && (presence > 0. || expand.presence.is_some()) {
            let presence = expand.presence.unwrap_or(presence);
            let content = live.take().unwrap_or_else(empty);
            Some(self.render_side_card(content, presence, fade, morph, true, cx))
        } else if ghost(SideMode::Slot) {
            let gone = fade.map(|fade| 1. - fade);
            Some(self.render_side_card(empty(), 1., gone, None, false, cx))
        } else {
            None
        };
        let in_sheet = slot == SideMode::Overlay && (over > 0. || open || morph.is_some());
        let (sheet, slide, opacity, dim) = if in_sheet {
            let content = live.take().unwrap_or_else(empty);
            let (slide, opacity, dim) = match (fade, morph) {
                (Some(fade), _) => (1., fade, fade),
                // Leaving with no origin to go back into, it slides off the
                // lead as it would have without one.
                (None, Some(_)) if expand.fading => {
                    let presence = expand.presence.unwrap_or(1.);
                    (presence, 1., presence)
                }
                (None, Some(morph)) => (1., 1., morph.growth),
                (None, None) => (over, if open { over } else { 1. }, over),
            };
            let sheet = self.render_side_sheet(content, morph, true, cx);
            (Some(sheet), slide, opacity, dim)
        } else if ghost(SideMode::Overlay) {
            let gone = fade.map_or(0., |fade| 1. - fade);
            let sheet = self.render_side_sheet(empty(), None, false, cx);
            (Some(sheet), 1., gone, gone)
        } else {
            // Into the card from the sheet, the scrim goes as it shrinks.
            let dim = match morph {
                Some(morph) if expand.crossing => 1. - morph.growth,
                _ => 0.,
            };
            (None, 1., 1., dim)
        };
        let scrim =
            (sheet.is_some() || dim > 0.).then(|| crate::material::scrim(SHEET_SCRIM * dim, cx));
        SideFrame {
            card,
            sheet,
            slide,
            opacity,
            scrim,
            below_card: self.composer_below_card.clone(),
        }
    }

    /// Where the body's layout now draws the agent, `mode`, is not where it
    /// was drawn: it crosses there from where it is on screen. Not on screen
    /// on the last frame, as on the first frame of a scene, it is drawn
    /// there at once.
    fn cross_to(&mut self, mode: SideMode, cx: &App) {
        let leaves = self.side.slot;
        let crossing = self
            .side
            .expand
            .as_ref()
            .is_some_and(|expand| matches!(expand.phase, ExpandPhase::Crossing { .. }));
        if leaves == mode || crossing {
            return;
        }
        let Some(drawn) = self.side_origins.resolve(SURFACE) else {
            return;
        };
        let now = cx.background_executor().now();
        let was = self.side.expand.take();
        let shown = was.as_ref().map_or(Shown::FULLY, |was| was.at(now).1);
        self.side.expand = Some(Expand {
            origin: None,
            from: None,
            // Partway somewhere, it starts from where it is.
            held: was.is_some().then_some(drawn),
            laid: was.and_then(|was| was.laid),
            phase: if cx.reduce_motion() {
                ExpandPhase::Swapping { at: now, leaves }
            } else {
                ExpandPhase::Crossing {
                    at: now,
                    shown,
                    leaves,
                }
            },
        });
    }

    /// The card's way out of what opened it or back into it, or from one
    /// place to the other, this frame: how far it has grown and from where,
    /// how far in it and what it holds are, and how far in the card is while
    /// it fades away instead.
    fn step_expand(&mut self, window: &mut Window, cx: &mut Context<Self>) -> ExpandFrame {
        let now = cx.background_executor().now();
        let Some(expand) = &mut self.side.expand else {
            return ExpandFrame::default();
        };
        let on_screen = expand.origin.as_ref().and_then(|origin| {
            self.side_origins
                .resolve(origin)
                .or_else(|| self.timeline_origins.resolve(origin))
                .or_else(|| self.rail_origins.resolve(origin))
        });
        if let Some(from) = on_screen {
            expand.from = Some(from);
        }
        // Drawn once where it was, it grows from there into the other place.
        if let ExpandPhase::Crossing { at, shown, .. } = expand.phase
            && now > at
        {
            expand.from = expand
                .held
                .take()
                .or_else(|| self.side_origins.resolve(SURFACE));
            expand.laid = self.side_origins.resolve(HELD).map(|(laid, _)| laid);
            expand.phase = ExpandPhase::Open {
                at: now,
                grown: 0.,
                shown,
            };
        }
        // The first frame after it closed lays out what it goes back into,
        // so from the second that is known; not on screen then, there is
        // none. Counted in frames, not by the clock: the first frame is
        // drawn some time after the close.
        if let ExpandPhase::Closing { drawn, .. } = &mut expand.phase
            && !*drawn
        {
            *drawn = true;
        } else if let ExpandPhase::Closing { grown, .. } = expand.phase
            && on_screen.is_none()
        {
            let shown = expand.at(now).1;
            // Already on its way, it stays where it is rather than going
            // back to its place to leave.
            if grown < 1. || expand.from.is_some() {
                expand.held = self.side_origins.resolve(SURFACE);
            }
            expand.phase = ExpandPhase::Leaving { at: now, shown };
        }
        let (growth, shown) = expand.at(now);
        let crossing = expand.origin.is_none();
        let left = expand.left(now);
        // gpui has no way to fade a snapshot, and one `ChatView` is one
        // layout: a second would lose the first's scroll, folds and built
        // markdown. So what it holds changes size at the crossing's low
        // point, rather than the two being drawn across each other.
        if crossing && matches!(expand.phase, ExpandPhase::Open { .. }) && 1. - left >= CROSS_SWAP {
            expand.laid = None;
        }
        let moving = |from, growth, held| Morphing {
            from,
            growth,
            held,
            laid: expand.laid,
            surface: shown.surface,
        };
        let (frame, done) = match expand.phase {
            ExpandPhase::Open { .. } => (
                ExpandFrame {
                    morph: Some(moving(expand.from, growth, None)),
                    content: shown.content,
                    presence: Some(1.),
                    crossing,
                    ..ExpandFrame::default()
                },
                left < EXPAND_REST || (crossing && expand.from.is_none()),
            ),
            ExpandPhase::Leaving { at, .. } => {
                let gone =
                    crate::motion::Curve::new(SHOW).progress(now.saturating_duration_since(at));
                (
                    ExpandFrame {
                        morph: Some(moving(None, 1., expand.held)),
                        content: shown.content,
                        presence: Some(1. - gone),
                        fading: true,
                        ..ExpandFrame::default()
                    },
                    gone >= 1.,
                )
            }
            ExpandPhase::Closing { .. } => (
                ExpandFrame {
                    morph: Some(moving(expand.from, growth, expand.held)),
                    content: shown.content,
                    presence: Some(1.),
                    ..ExpandFrame::default()
                },
                left < EXPAND_REST,
            ),
            ExpandPhase::Crossing { .. } => (
                ExpandFrame {
                    morph: Some(moving(None, 1., expand.held)),
                    content: shown.content,
                    presence: Some(1.),
                    crossing,
                    ..ExpandFrame::default()
                },
                false,
            ),
            ExpandPhase::Swapping { at, leaves } => {
                let shown =
                    crate::motion::Curve::new(SWAP).progress(now.saturating_duration_since(at));
                (
                    ExpandFrame {
                        presence: Some(shown),
                        swap: Some(leaves),
                        ..ExpandFrame::default()
                    },
                    shown >= 1.,
                )
            }
        };
        if done {
            let closed = matches!(
                expand.phase,
                ExpandPhase::Closing { .. } | ExpandPhase::Leaving { .. }
            );
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

    /// The agent's surface, the card's or the sheet's:
    /// `content` on its frame, on its way as `morph` says, recording where
    /// it is drawn while it holds the agent (`live`).
    fn side_surface(
        &self,
        content: AnyElement,
        morph: Option<Morphing>,
        live: bool,
        cx: &App,
    ) -> crate::motion::expand::Morph {
        let surface = crate::material::card_raised(cx)
            // The agent's timeline takes the wheel over its surface, not the
            // lead's under it. gpui-base's page viewport gives a wheel to
            // the innermost one under the pointer, but the lead's does not
            // hold the surface: drawn before it, it would scroll before the
            // agent's claims the wheel, had the surface not blocked it.
            .occlude()
            .p_0()
            .gap_0();
        let surface = crate::motion::expand::Morph::new(
            surface,
            crate::material::radius_card(cx),
            px(1.),
            content,
        )
        .from(
            morph.and_then(|morph| morph.from),
            morph.map_or(1., |morph| morph.growth),
        )
        .held(morph.and_then(|morph| morph.held))
        .laid(morph.and_then(|morph| morph.laid))
        .surface(morph.map_or(1., |morph| morph.surface));
        if live {
            surface.mark(&self.side_origins, SURFACE, HELD)
        } else {
            surface
        }
    }

    /// The agent's card in the rail's place, `presence` of the way in (or,
    /// with motion reduced, `fade` of the way, where it stays), holding
    /// `content`, on its way as `morph` says, its leading edge dragged to
    /// widen or narrow it.
    fn render_side_card(
        &self,
        content: AnyElement,
        presence: f32,
        fade: Option<f32>,
        morph: Option<Morphing>,
        live: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let shown = self.body.side();
        // gpui-base's resize handle straddles the leading edge of what holds
        // it; held by nothing as wide, it straddles the middle of the gap
        // left of the card, which the drag measures the card's width from.
        // gpui-base's band also occludes the wheel, which has no way past it
        // to the lead's timeline under the gap, so the band hands it on.
        let edge = (shown && morph.is_none() && live).then(|| {
            let motion = self.motion.clone();
            let lead = cx.entity_id();
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(px(-CHAT_SIDE_GRIP / 2.))
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
                    })
                    .on_scroll(cx.listener(
                        |this, event: &gpui::ScrollEvent, window, cx| {
                            let delta = event.delta.pixel_delta(window.line_height());
                            this.list_state.scroll_by(-delta.y);
                            this.timeline.update(cx, |_, cx| cx.notify());
                        },
                    )),
                )
        });
        let card = self.side_surface(content, morph, live, cx);
        super::rail::slot(self.side.width, fade.map_or(presence, |_| 1.))
            .when_some(fade, |slot, fade| slot.opacity(fade))
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<SideEdgeDrag>, _, cx| {
                    if event.drag(cx).0 != cx.entity_id() {
                        return;
                    }
                    let width =
                        event.bounds.right() - event.event.position.x - px(CHAT_SIDE_GRIP / 2.);
                    let share = this.body.side_share_at(width);
                    if share != this.side.share {
                        this.side.share = share;
                        cx.notify();
                    }
                }),
            )
            .child(crate::motion::faded::faded(fade.unwrap_or(presence), card))
            .children(edge)
            .when(!shown, |card| card.child(crate::motion::shield()))
            .into_any_element()
    }

    /// The agent's sheet over the lead, holding `content`, on its way as
    /// `morph` says: inset from the lead's body at its sides, from the
    /// header's bottom edge down to the bottom of the lead's composer card,
    /// which [`super::rail::Slots`] lays it out to.
    fn render_side_sheet(
        &self,
        content: AnyElement,
        morph: Option<Morphing>,
        live: bool,
        cx: &App,
    ) -> AnyElement {
        div()
            .absolute()
            .top_0()
            .left(px(SHEET_INSET))
            .right(px(SHEET_INSET))
            .bottom_0()
            .child(self.side_surface(content, morph, live, cx))
            .into_any_element()
    }

    /// `main`, the thread's timeline, deliveries and composer, under the
    /// agent's sheet over it in `side`, and the scrim that dims it under
    /// the sheet. Always the same elements around `main`, so it keeps its
    /// place in the tree as an agent comes and goes.
    pub(super) fn cover_with_side(&self, main: AnyElement, side: SideFrame) -> AnyElement {
        if self.side_of.is_some() {
            return main;
        }
        let SideFrame {
            sheet,
            slide,
            opacity,
            scrim,
            below_card,
            ..
        } = side;
        div()
            .relative()
            .size_full()
            .overflow_hidden()
            .child(div().absolute().inset_0().child(main))
            // The lead under the sheet takes no input: the scrim is not a
            // way to close it either.
            .when_some(scrim, |body, scrim| {
                body.child(div().absolute().inset_0().bg(scrim).occlude())
            })
            .when_some(sheet, |body, sheet| {
                body.child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(relative(1. - slide))
                        .right(relative(slide - 1.))
                        .opacity(opacity)
                        .child(super::rail::Slots {
                            below_card,
                            slots: vec![sheet],
                        }),
                )
            })
            // Neither takes input while the sheet slides over the lead.
            .when(slide > 0. && slide < 1., |body| {
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
            .children(under.map(|under| div().absolute().inset_0().child(under.frame.clone())))
            .children(current.map(|current| {
                div()
                    .absolute()
                    .inset_0()
                    .when(under.is_some(), |layer| layer.opacity(swap))
                    .child(current.frame.clone())
            }))
            .into_any_element()
    }

    /// An agent's own header beside its lead: on the sheet over the lead,
    /// Back at its start, naming the lead it goes back to, and a badge
    /// saying this is an agent of the lead's; its state and title; and
    /// Close at its end while it sits beside the lead on its card.
    pub(super) fn render_side_header(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let of = self.side_of.as_ref()?;
        let session = self.session_id.clone()?;
        let (heading, title, lead_title) = {
            let store = self.workspace_store.read(cx);
            let agents = crate::agents_panel::Agents::of(store, &of.lead_session, cx);
            (
                agents.heading(&session),
                agents.title(store, &session),
                store
                    .thread_meta(&of.lead_session)
                    .map(|meta| meta.title.clone())
                    .filter(|title| !title.trim().is_empty()),
            )
        };
        let execution = heading.as_ref().and_then(|heading| heading.execution);
        let key = heading.map_or_else(|| session.clone(), |heading| heading.key);

        let close = {
            let lead = of.lead.clone();
            move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| {
                let _ = lead.update(cx, |lead, cx| lead.close_side(window, cx));
            }
        };
        // Back and the badge on the sheet over the lead, Close on the card
        // beside it: one folds out as the other folds in when the body
        // crosses between.
        let on_sheet = of.mode == SideMode::Overlay;
        let back = self
            .motion
            .shown("side-back", on_sheet.then_some(()), SHOW, window, cx)
            .map(|shown| {
                let name: SharedString = match &lead_title {
                    Some(title) => crate::tr!("agents.back_to_parent", title = title.as_str()),
                    None => crate::tr!("settings.back"),
                }
                .into_owned()
                .into();
                // The lead's title fades rather than pushing the agent's out
                // of the header.
                let label = lead_title.clone().map(|title| {
                    div()
                        .debug_selector(|| "side-back-title".into())
                        .min_w_0()
                        .overflow_hidden()
                        .child(crate::widgets::fade_text(title))
                });
                shown.fold_across(
                    Button::new("side-back")
                        .motion(&self.motion)
                        .debug_selector(|| "side-back".into())
                        .ghost()
                        .xsmall()
                        .icon(IconName::ArrowLeft)
                        .max_w(px(BACK_WIDEST))
                        .children(label)
                        .aria_label(name.clone())
                        .tooltip(name)
                        .on_click(close.clone()),
                )
            });
        let badge = self
            .motion
            .shown("side-badge", on_sheet.then_some(()), SHOW, window, cx)
            .map(|shown| {
                shown.fold_across(
                    crate::material::chip(crate::tr!("agents.side_badge").into_owned(), cx)
                        .debug_selector(|| "chat-side-badge".into()),
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
                .children(badge)
                .child(div().flex_none().child(crate::agents_panel::status_mark(
                    &key, execution, window, cx,
                )))
                .child(
                    div()
                        .debug_selector(|| "chat-side-title".into())
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
        .child(div().w_full().h(px(GRIP_LENGTH)).rounded_full().bg(fill.0))
        .into_any_element()
}

/// Close `host`'s thread if the host opened it and no pane shows it now.
fn close_side_thread(store: &Entity<WorkspaceStore>, host: &SideHost, cx: &mut App) {
    if host.opened
        && store.read(cx).is_thread_open(&host.session_id)
        && !crate::shell::shown_in_pane(&store.read(cx).thread_key(&host.session_id), cx)
    {
        store.update(cx, |store, cx| store.close_thread(&host.session_id, cx));
    }
}

/// Let go of `host`'s lease; its chat goes with it.
fn release_side_host(store: &Entity<WorkspaceStore>, host: SideHost, cx: &mut App) {
    store.update(cx, |store, cx| store.release_timeline(&host.session_id, cx));
}
