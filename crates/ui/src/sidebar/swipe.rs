//! Swiping a phone thread row left settles it, or makes a settled one active
//! again: the long-press menu's Settle / Unsettle, one gesture away. A thread
//! whose turn runs, or that waits on the user, has neither: its row follows
//! the finger all the same, shows what the thread is busy with, and comes
//! back. What the row shows follows the thread while the finger is down, and
//! the release does what it shows.
//!
//! gpui-base has no swipe-action row, so the row claims GPUI's touch pan
//! itself. GPUI locks a pan to one axis and reports it as `ScrollEvent`s
//! at the pan's original contact; the row takes a horizontal one in the
//! capture phase, and a vertical one goes on to the list.
//!
//! A swipe outlives the row's element: it is kept here, by thread, from the
//! finger taking the row until the row is at rest again. gpui-base's `spring`
//! keeps its state in the element, starts from rest and shows neither where
//! it is nor how fast it goes, so it can neither carry on from a finger nor be
//! caught by one; a row's flight is GPUI's `SpringConfig`, at gpui-base's
//! parameters, sampled from the release until gpui-base's spring takes a
//! velocity and gives up its state.
//!
//! A thread goes when its row has: the command is sent as the row lands, and
//! the row is out of the list from then on, so that its room closes without
//! waiting for the host and a finger put on it in flight still undoes the
//! swipe. The list takes it back if the host refuses.

use std::collections::{HashSet, VecDeque};
use std::f32::consts::TAU;
use std::time::Duration;
#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
#[cfg(target_family = "wasm")]
use web_time::Instant;

use super::arrange::DragAction;
use super::*;
use crate::edge_back::{EDGE_WIDTH, SETTLE};
use gpui::{
    Bounds, DispatchPhase, HitboxBehavior, Pixels, Point, ScrollDelta, ScrollEvent, SpringConfig,
    SpringState, TouchDragEvent, TouchPhase, Transformation, canvas, size,
};

/// How much of the finger's recent past is its speed at release.
const SPEED_WINDOW: Duration = Duration::from_millis(40);
/// A gap this long between a finger's reports means it stopped: what came
/// before is an earlier motion. gpui's recognizer draws the same line.
const STOPPED: Duration = Duration::from_millis(40);
/// A span too short to tell a speed from the panel's jitter.
const SHORTEST_SPAN: Duration = Duration::from_millis(4);
/// How far past the release a moving finger counts as having gone, in
/// seconds of its speed: `UIScrollView`'s fast deceleration comes to about
/// this.
const PROJECTION: f32 = 0.12;
/// A shorter swipe is no action however fast it was.
const SHORTEST_FLICK: Pixels = px(32.);
/// A finger going back at this speed, in points a second, has changed its
/// mind, wherever the row is.
const RETURNING: f32 = 100.;
/// `UIScrollView`'s rubber band.
const BAND_STIFFNESS: f32 = 0.55;
/// The furthest the row goes the way it has no action.
const WRONG_WAY: Pixels = px(72.);
/// The furthest a row going home passes its place.
const BOUNCE: Pixels = px(8.);
/// How close to gone a flying row is taken as gone: from there the fold that
/// closes its room carries on over it.
const LANDED: Pixels = px(8.);
/// Within this of its place, and as slow, a row going home is there.
const AT_REST: f32 = 0.5;
const FLIGHT_STEP: Duration = Duration::from_millis(4);

/// `gpui_base::motion::Spring::new(SETTLE)`'s physics.
fn spring() -> SpringConfig {
    let frequency = TAU / SETTLE.as_secs_f32();
    SpringConfig::new(frequency * frequency, 2. * frequency, 1.)
}

/// How far the row must go for its release to act, and past which it resists.
fn action_width(width: Pixels) -> Pixels {
    width / 3.
}

fn rubber(distance: Pixels, extent: Pixels) -> Pixels {
    let extent = extent.max(px(1.));
    extent * (1. - 1. / (distance / extent * BAND_STIFFNESS + 1.))
}

fn unrubber(shown: Pixels, extent: Pixels) -> Pixels {
    let extent = extent.max(px(1.));
    let shown = shown.min(extent * 0.99);
    extent / BAND_STIFFNESS * (shown / (extent - shown))
}

/// Where a row stands for a finger `travel` leftward of the row's rest: under
/// the finger up to `reach`, and resisting past it and the wrong way.
fn banded(travel: Pixels, reach: Pixels, width: Pixels) -> Pixels {
    if travel < px(0.) {
        -rubber(-travel, WRONG_WAY)
    } else if travel <= reach {
        travel
    } else {
        reach + rubber(travel - reach, width - reach)
    }
}

/// Whether letting go of a row at `offset` acts, the finger going at `speed`:
/// the row is judged at where it was heading, and a finger on its way back
/// has decided otherwise.
fn commits(offset: Pixels, speed: f32, width: Pixels) -> bool {
    speed > -RETURNING
        && offset >= SHORTEST_FLICK
        && offset + px(speed.max(0.) * PROJECTION) >= action_width(width)
}

/// A finger's recent travel. gpui's velocity tracker is private to its
/// recognizer, which keeps the speed it finds to itself, so a swipe measures
/// its own over the reports it has seen.
#[derive(Default)]
struct FingerSpeed {
    samples: VecDeque<(Instant, Pixels)>,
}

impl FingerSpeed {
    fn push(&mut self, at: Instant, travel: Pixels) {
        self.samples.push_back((at, travel));
        while self
            .samples
            .front()
            .is_some_and(|(first, _)| at.saturating_duration_since(*first) > SPEED_WINDOW)
        {
            self.samples.pop_front();
        }
    }

    /// In points a second, leftward.
    fn at(&self, now: Instant) -> f32 {
        let Some(&(last, travel)) = self.samples.back() else {
            return 0.;
        };
        if now.saturating_duration_since(last) > STOPPED {
            return 0.;
        }
        let (mut first, mut from) = (last, travel);
        for &(at, travelled) in self.samples.iter().rev().skip(1) {
            if first.saturating_duration_since(at) > STOPPED {
                break;
            }
            (first, from) = (at, travelled);
        }
        let span = last.saturating_duration_since(first);
        if span < SHORTEST_SPAN {
            return 0.;
        }
        f32::from(travel - from) / span.as_secs_f32()
    }
}

/// A row under a finger.
struct Drag {
    /// Where the finger found the row, as its leftward travel.
    start: Pixels,
    /// How far the row goes under the finger before it resists: past where a
    /// finger found it, a row caught in flight would otherwise leap.
    reach: Pixels,
    moved: Pixels,
    speed: FingerSpeed,
    /// Whether the last haptic said the release acts.
    felt: bool,
}

impl Drag {
    fn from(offset: Pixels, width: Pixels) -> Self {
        Self {
            start: if offset < px(0.) {
                -unrubber(-offset, WRONG_WAY)
            } else {
                offset
            },
            reach: action_width(width).max(offset).min(width),
            moved: px(0.),
            speed: FingerSpeed::default(),
            felt: false,
        }
    }

    fn offset(&self, width: Pixels) -> Pixels {
        banded(self.start + self.moved, self.reach, width).min(width)
    }

    /// How much of the finger's speed reaches the row where it stands.
    fn give(&self, width: Pixels) -> f32 {
        let travel = self.start + self.moved;
        (banded(travel + px(1.), self.reach, width) - banded(travel, self.reach, width)) / px(1.)
    }
}

/// A row on its own: where it left the finger, for where it is going.
#[derive(Clone, Copy)]
pub(super) struct Flight {
    from: SpringState,
    target: f32,
    at: Instant,
    width: Pixels,
}

impl Flight {
    /// With motion reduced the row is simply there.
    fn new(from: SpringState, target: f32, width: Pixels, at: Instant, cx: &App) -> Self {
        let from = if cx.reduce_motion() {
            SpringState {
                position: target,
                velocity: 0.,
            }
        } else {
            from
        };
        Self {
            from,
            target,
            at,
            width,
        }
    }

    fn resting(offset: Pixels, width: Pixels, at: Instant) -> Self {
        let position = f32::from(offset);
        Self {
            from: SpringState {
                position,
                velocity: 0.,
            },
            target: position,
            at,
            width,
        }
    }

    fn state(&self, now: Instant) -> SpringState {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f32();
        spring().step(self.from, self.target, elapsed)
    }

    fn offset(&self, now: Instant) -> Pixels {
        let position = px(self.state(now).position);
        if position < px(0.) && self.from.position >= 0. {
            -rubber(-position, BOUNCE)
        } else {
            position
        }
    }

    fn at_rest(&self, now: Instant) -> bool {
        spring().is_settled(self.state(now), self.target, AT_REST)
    }

    /// How long after it left the row counts as gone.
    fn lands_after(&self) -> Duration {
        (0..128u32)
            .map(|step| FLIGHT_STEP * step)
            .find(|elapsed| {
                let state = spring().step(self.from, self.target, elapsed.as_secs_f32());
                px(state.position) >= px(self.target) - LANDED
            })
            .unwrap_or(FLIGHT_STEP * 128)
    }
}

enum Motion {
    Dragged(Drag),
    Home(Flight),
    /// Going off the row's left edge. Once `sent`, the thread's command has
    /// gone to the host and the row is out of the list, until the list shows
    /// the thread moved or the host refuses.
    Off {
        flight: Flight,
        sent: bool,
    },
}

pub(super) struct RowSwipe {
    thread: ThreadKey,
    width: Pixels,
    /// What the release does, as the row shows it: nothing for a busy thread.
    action: Option<DragAction>,
    /// Where the pan or the touch on the row began, while it lasts. A swipe
    /// ended under a finger keeps it, so that the rest of the pan is spent.
    finger: Option<Point<Pixels>>,
    motion: Motion,
}

impl RowSwipe {
    fn state(&self, now: Instant) -> SpringState {
        match &self.motion {
            Motion::Dragged(drag) => SpringState {
                position: f32::from(drag.offset(self.width)),
                velocity: drag.speed.at(now) * drag.give(self.width),
            },
            Motion::Home(flight) | Motion::Off { flight, .. } => flight.state(now),
        }
    }

    fn offset(&self, now: Instant) -> Pixels {
        match &self.motion {
            Motion::Dragged(drag) => drag.offset(self.width),
            Motion::Home(flight) | Motion::Off { flight, .. } => flight.offset(now),
        }
    }

    /// What is left to draw of the row, once it is no longer in the list.
    fn flight(&self, now: Instant) -> Flight {
        match &self.motion {
            Motion::Dragged(drag) => Flight::resting(drag.offset(self.width), self.width, now),
            Motion::Home(flight) | Motion::Off { flight, .. } => *flight,
        }
    }

    fn at_rest(&self, now: Instant) -> bool {
        matches!(&self.motion, Motion::Home(flight) if flight.at_rest(now))
    }

    fn sent(&self) -> bool {
        matches!(self.motion, Motion::Off { sent: true, .. })
    }

    fn unsettles(&self) -> bool {
        self.action == Some(DragAction::Unsettle)
    }

    fn send_home(&mut self, now: Instant, cx: &App) {
        self.motion = Motion::Home(Flight::new(self.state(now), 0., self.width, now, cx));
    }

    fn take(&mut self, finger: Point<Pixels>, now: Instant) {
        let offset = self.offset(now);
        self.finger = Some(finger);
        let mut drag = Drag::from(offset, self.width);
        drag.felt = self.action.is_some() && drag.offset(self.width) >= action_width(self.width);
        self.motion = Motion::Dragged(drag);
    }

    /// The finger is `moved` leftward of where it took the row; a report the
    /// finger's speed counts `sampled`.
    fn follow(&mut self, moved: Pixels, sampled: bool, now: Instant) {
        let Motion::Dragged(drag) = &mut self.motion else {
            return;
        };
        drag.moved = moved;
        if sampled {
            drag.speed.push(now, moved);
        }
        self.feel();
    }

    /// The thread now shows `action`. A row on its way off for another is
    /// called back.
    fn shows(&mut self, action: Option<DragAction>, now: Instant, cx: &App) {
        let was = std::mem::replace(&mut self.action, action);
        match self.motion {
            Motion::Off { sent: false, .. } if was != action => self.send_home(now, cx),
            Motion::Dragged(_) => self.feel(),
            _ => {}
        }
    }

    /// The release coming to act is felt, whether the finger takes the row
    /// past the point at which it acts or the thread lets it act there, and
    /// so is the finger taking it back. A thread turning busy under the
    /// finger is only seen, so one flipping under a still finger is felt
    /// once.
    fn feel(&mut self) {
        let (width, acts) = (self.width, self.action.is_some());
        let Motion::Dragged(drag) = &mut self.motion else {
            return;
        };
        let past = drag.offset(width) >= action_width(width);
        if past && acts && !drag.felt {
            crate::haptics::threshold();
            drag.felt = true;
        } else if !past && drag.felt {
            if acts {
                crate::haptics::threshold();
            }
            drag.felt = false;
        }
    }

    fn moved(&self) -> Pixels {
        match &self.motion {
            Motion::Dragged(drag) => drag.moved,
            _ => px(0.),
        }
    }
}

/// The rows swiped, and on their own, at the moment.
#[derive(Default)]
pub(super) struct RowSwipes {
    rows: Vec<RowSwipe>,
    /// The pan whose fling is being caught: the touch that catches it is no
    /// finger's.
    catching_fling: Option<Point<Pixels>>,
    /// A sideways pan no row took: it moves nothing, but flings all the same.
    idle_pan: Option<Point<Pixels>>,
    /// A pan on a row that began by catching a fling. gpui starts such a pan
    /// at once, with no direction, and gives it the direction of its first
    /// movement.
    caught: Option<Point<Pixels>>,
}

impl RowSwipes {
    fn row(&mut self, thread: &ThreadKey) -> Option<&mut RowSwipe> {
        self.rows.iter_mut().find(|swipe| swipe.thread == *thread)
    }

    /// GPUI follows one touch at a time, so a new one means the last ended
    /// where this list could not hear it: the row it held goes home.
    fn drop_lost_fingers(&mut self, now: Instant, cx: &App) {
        for swipe in &mut self.rows {
            if swipe.finger.take().is_some() && matches!(swipe.motion, Motion::Dragged(_)) {
                swipe.send_home(now, cx);
            }
        }
    }

    /// The flight of the row `row` draws, for a row that leaves the list.
    pub(super) fn flight_of(&self, row: Option<&CompactListRow>, cx: &App) -> Option<Flight> {
        let Some(CompactListRow::Thread(row)) = row else {
            return None;
        };
        let now = cx.background_executor().now();
        self.rows
            .iter()
            .find(|swipe| swipe.thread == row.state.thread)
            .map(|swipe| swipe.flight(now))
    }
}

/// A row in the swipe's listeners, as it was painted.
#[derive(Clone)]
struct SwipeRow {
    thread: ThreadKey,
    action: Option<DragAction>,
    can_swipe: bool,
    bounds: Bounds<Pixels>,
}

fn can_swipe(row: &CompactThreadRow, renaming: Option<&ThreadKey>) -> bool {
    renaming != Some(&row.state.thread)
}

/// The long-press menu's rule for enabling Settle / Unsettle.
fn action(row: &CompactThreadRow) -> Option<DragAction> {
    if row.meta.is_settled() {
        Some(DragAction::Unsettle)
    } else if row.working || row.state.waiting() {
        None
    } else {
        Some(DragAction::Settle)
    }
}

/// What a swiped row uncovers: what its release does, or what keeps its
/// thread from anything, as the row's status line words it.
#[derive(Clone, PartialEq)]
enum Uncovered {
    Action(DragAction),
    Busy(SharedString, gpui::Hsla),
}

impl Uncovered {
    fn of(row: &CompactThreadRow, cx: &App) -> Self {
        action(row).map_or_else(
            || {
                let (label, color) = busy_status(&row.state, cx);
                Self::Busy(label.into(), color)
            },
            Self::Action,
        )
    }

    fn label(&self) -> SharedString {
        match self {
            Self::Action(action) => action.label().into(),
            Self::Busy(label, _) => label.clone(),
        }
    }

    fn icon(&self) -> Option<IconName> {
        match self {
            Self::Action(action) => Some(action.icon()),
            Self::Busy(..) => None,
        }
    }

    /// The wash it is shown on, and the colour it is shown in.
    fn colors(&self, cx: &App) -> (gpui::Hsla, gpui::Hsla) {
        match self {
            Self::Action(DragAction::Settle) => (
                cx.theme().success.opacity(0.12),
                cx.theme().success_foreground,
            ),
            Self::Action(_) => (cx.theme().primary.opacity(0.12), cx.theme().primary),
            Self::Busy(_, color) => (color.opacity(0.12), *color),
        }
    }
}

/// The row's content at `offset` left of its place, in `container`, with what
/// the swipe does showing behind it as it is uncovered, changing to what it
/// does next as its thread changes.
#[allow(clippy::too_many_arguments)]
fn swiped_surface(
    container: gpui::Div,
    id: &str,
    uncovered: Uncovered,
    offset: Pixels,
    width: Pixels,
    content: gpui::AnyElement,
    window: &mut Window,
    cx: &mut App,
) -> gpui::AnyElement {
    let reveal = (offset / action_width(width).max(px(1.))).clamp(0., 1.);
    let scale = 0.6 + 0.4 * reveal;
    let under = (offset > px(0.)).then(|| {
        let key = SharedString::from(format!("row-swipe-{id}"));
        let fill = transition(
            (key.clone(), "fill"),
            uncovered.colors(cx).0,
            gpui_base::motion::Transition::new(CHANGE),
            window,
            cx,
        );
        let id = id.to_owned();
        let shown = crate::motion::cross_fade_part(
            (gpui::ElementId::from(key), "uncovered"),
            uncovered,
            CHANGE,
            window,
            cx,
            move |uncovered, _, cx| {
                let label = uncovered.label();
                h_flex()
                    .debug_selector(|| format!("row-swipe-shows-{label}-{id}"))
                    .items_center()
                    .gap(cx.theme().tokens.spacing.xs)
                    .text_color(uncovered.colors(cx).1)
                    .when_some(uncovered.icon(), |part, icon| {
                        part.child(
                            Icon::new(icon).transform(Transformation::scale(size(scale, scale))),
                        )
                    })
                    .child(label)
                    .into_any_element()
            },
        );
        h_flex()
            .absolute()
            .inset_0()
            .justify_end()
            .items_center()
            .px(px(COMPACT_PAGE_INSET))
            .bg(fill)
            .text_body(true, cx)
            .child(div().opacity(reveal).child(shown))
    });
    container
        .children(under)
        .child(
            div()
                .relative()
                .left(-offset)
                .bg(crate::material::content_surface(cx))
                .child(content),
        )
        .into_any_element()
}

impl SessionsSidebar {
    pub(super) fn swipeable_row(
        &mut self,
        row: &CompactThreadRow,
        content: gpui::AnyElement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let thread = row.state.thread.clone();
        let uncovered = Uncovered::of(row, cx);
        let action = action(row);
        let renaming = self.renaming.as_ref().map(|rename| &rename.thread);
        let can_swipe = can_swipe(row, renaming);
        let now = cx.background_executor().now();
        let swipe = self.row_swipes.row(&thread);
        let (offset, width) = swipe
            .as_ref()
            .map_or((px(0.), px(0.)), |swipe| (swipe.offset(now), swipe.width));
        if swipe
            .is_some_and(|swipe| !matches!(swipe.motion, Motion::Dragged(_)) && !swipe.at_rest(now))
        {
            window.request_animation_frame();
        }
        let sidebar = cx.entity().downgrade();
        let surface_id = row.state.name.clone();
        swiped_surface(
            div().relative().w_full().overflow_hidden().child(
                canvas(
                    |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
                    move |bounds, hitbox, window, _| {
                        let row = SwipeRow {
                            thread,
                            action,
                            can_swipe,
                            bounds,
                        };
                        window.on_mouse_event({
                            let (row, sidebar, hitbox) =
                                (row.clone(), sidebar.clone(), hitbox.clone());
                            move |event: &ScrollEvent, phase, window, cx| {
                                if phase != DispatchPhase::Capture {
                                    return;
                                }
                                let ScrollDelta::Pixels(delta) = event.delta else {
                                    return;
                                };
                                let over = row.bounds.contains(&event.position)
                                    && hitbox.should_handle_scroll(window);
                                let Some(sidebar) = sidebar.upgrade() else {
                                    return;
                                };
                                sidebar.update(cx, |this, cx| {
                                    this.on_row_pan(&row, event, delta, over, window, cx);
                                });
                            }
                        });
                        window.on_mouse_event(move |event: &TouchDragEvent, phase, window, cx| {
                            if phase != DispatchPhase::Capture {
                                return;
                            }
                            let over = row.bounds.contains(&event.start_position)
                                && hitbox.should_handle_scroll(window);
                            let Some(sidebar) = sidebar.upgrade() else {
                                return;
                            };
                            sidebar.update(cx, |this, cx| {
                                this.on_row_touch(&row, event, over, window, cx);
                            });
                        });
                    },
                )
                .absolute()
                .inset_0(),
            ),
            &surface_id,
            uncovered,
            offset,
            width,
            content,
            window,
            cx,
        )
    }

    /// A row that left the list, as it is drawn while its room closes: on
    /// its way, if a swipe had it.
    pub(super) fn render_left_compact_row(
        &mut self,
        row: &CompactListRow,
        flight: Option<Flight>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match (row, flight) {
            (CompactListRow::Thread(thread), Some(flight)) => {
                let content = self.compact_thread_content(thread, window, cx);
                let offset = flight.offset(cx.background_executor().now());
                swiped_surface(
                    div().relative().w_full().overflow_hidden(),
                    &thread.state.name,
                    Uncovered::of(thread, cx),
                    offset,
                    flight.width,
                    content,
                    window,
                    cx,
                )
            }
            _ => self.render_compact_row(row, window, cx),
        }
    }

    fn on_row_pan(
        &mut self,
        row: &SwipeRow,
        event: &ScrollEvent,
        delta: Point<Pixels>,
        over: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.row_swipes.catching_fling == Some(event.position) {
            cx.stop_propagation();
            return;
        }
        let now = cx.background_executor().now();
        let ended = matches!(event.touch_phase, TouchPhase::Ended | TouchPhase::Cancelled);
        let begins = if event.touch_phase == TouchPhase::Started {
            self.row_swipes.drop_lost_fingers(now, cx);
            if over && delta == Point::default() {
                self.row_swipes.caught = Some(event.position);
            }
            over
        } else if self.row_swipes.caught == Some(event.position)
            && (ended || (over && delta != Point::default()))
        {
            self.row_swipes.caught = None;
            !ended
        } else {
            false
        };
        if begins {
            // Inward from the screen's edge is navigation's. Outward, the
            // edge has no gesture and the row has its own.
            let inward_from_edge =
                delta.x > px(0.) && event.position.x - row.bounds.left() <= EDGE_WIDTH;
            if delta.x != px(0.) && delta.y == px(0.) {
                if row.can_swipe && !inward_from_edge {
                    self.take_row(row, event.position, now);
                } else {
                    self.row_swipes.idle_pan = Some(event.position);
                }
            }
        } else if self.row_swipes.idle_pan == Some(event.position) && ended {
            self.row_swipes.idle_pan = None;
            if event.touch_phase == TouchPhase::Ended {
                self.catch_row_fling(event.position, window, cx);
            }
        }
        let Some(index) = self
            .row_swipes
            .rows
            .iter()
            .position(|swipe| swipe.finger == Some(event.position))
        else {
            return;
        };
        cx.stop_propagation();
        let swipe = &mut self.row_swipes.rows[index];
        let moved = swipe.moved() - delta.x;
        swipe.follow(moved, !ended && delta.x != px(0.), now);
        if ended {
            self.let_go_of_row(index, event.touch_phase == TouchPhase::Ended, window, cx);
        }
        if event.touch_phase == TouchPhase::Ended {
            self.catch_row_fling(event.position, window, cx);
        }
        cx.notify();
    }

    /// GPUI offers a touch to its elements as it lands, before it knows what
    /// the touch is. A row on its own takes it there and then, as the only
    /// way to hold the row from the first point: a pan would have to clear
    /// the slop while the row flew on, and a touch that stayed a tap would
    /// open the thread and never say it ended.
    fn on_row_touch(
        &mut self,
        row: &SwipeRow,
        event: &TouchDragEvent,
        over: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.row_swipes.catching_fling.is_some() {
            return;
        }
        let now = cx.background_executor().now();
        if event.phase == TouchPhase::Started {
            self.row_swipes.drop_lost_fingers(now, cx);
            let flying = self.row_swipes.row(&row.thread).filter(|swipe| {
                !swipe.sent() && !matches!(swipe.motion, Motion::Dragged(_)) && !swipe.at_rest(now)
            });
            if let Some(swipe) = flying.filter(|_| over) {
                swipe.take(event.start_position, now);
                window.prevent_default();
                cx.stop_propagation();
                cx.notify();
            }
            return;
        }
        let Some(index) = self
            .row_swipes
            .rows
            .iter()
            .position(|swipe| swipe.finger == Some(event.start_position))
        else {
            return;
        };
        cx.stop_propagation();
        let moved = event.start_position.x - event.position.x;
        let ended = matches!(event.phase, TouchPhase::Ended | TouchPhase::Cancelled);
        self.row_swipes.rows[index].follow(moved, !ended, now);
        if ended {
            self.let_go_of_row(index, event.phase == TouchPhase::Ended, window, cx);
        }
        cx.notify();
    }

    fn take_row(&mut self, row: &SwipeRow, finger: Point<Pixels>, now: Instant) {
        let swipes = &mut self.row_swipes;
        if swipes.row(&row.thread).is_none() {
            swipes.rows.push(RowSwipe {
                thread: row.thread.clone(),
                width: row.bounds.size.width,
                action: row.action,
                finger: None,
                motion: Motion::Dragged(Drag::from(px(0.), row.bounds.size.width)),
            });
        }
        if let Some(swipe) = swipes.row(&row.thread) {
            swipe.action = row.action;
            swipe.take(finger, now);
        }
    }

    /// The finger has left the row: `ended` it, or the system took it away.
    fn let_go_of_row(
        &mut self,
        index: usize,
        ended: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let now = cx.background_executor().now();
        let swipe = &mut self.row_swipes.rows[index];
        swipe.finger = None;
        let Motion::Dragged(drag) = &swipe.motion else {
            return;
        };
        if !ended
            || swipe.action.is_none()
            || !commits(drag.offset(swipe.width), drag.speed.at(now), swipe.width)
        {
            swipe.send_home(now, cx);
            return;
        }
        let flight = Flight::new(
            swipe.state(now),
            f32::from(swipe.width),
            swipe.width,
            now,
            cx,
        );
        swipe.motion = Motion::Off {
            flight,
            sent: false,
        };
        let thread = swipe.thread.clone();
        let lands_after = flight.lands_after();
        if lands_after.is_zero() {
            self.send_swiped_row(&thread, flight.at, window, cx);
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(lands_after).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.send_swiped_row(&thread, flight.at, window, cx);
            });
        })
        .detach();
    }

    /// The row whose flight began `flown_at` has landed, if a finger did not
    /// catch it first.
    fn send_swiped_row(
        &mut self,
        thread: &ThreadKey,
        flown_at: Instant,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(swipe) = self.row_swipes.row(thread) else {
            return;
        };
        let unsettles = swipe.unsettles();
        match &mut swipe.motion {
            Motion::Off { flight, sent } if !*sent && flight.at == flown_at => *sent = true,
            _ => return,
        }
        self.compact_model_dirty = true;
        if unsettles {
            self.perform_lifecycle(
                &thread.host,
                vec![Command::UnsettleSession {
                    session_id: thread.session_id.clone(),
                }],
                None,
                window,
                cx,
            );
        } else {
            self.settle_thread(thread, window, cx);
        }
        cx.notify();
    }

    fn catch_row_fling(
        &mut self,
        origin: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.row_swipes.catching_fling = Some(origin);
        crate::edge_back::catch_fling(origin, window, cx);
        let sidebar = cx.entity().downgrade();
        window.defer(cx, move |_, cx| {
            let _ = sidebar.update(cx, |this, _| this.row_swipes.catching_fling = None);
        });
    }

    /// The host refused what a swipe asked, so the list takes the row back.
    pub(super) fn row_swipe_refused(&mut self, thread: &ThreadKey) {
        self.row_swipes
            .rows
            .retain(|swipe| !(swipe.thread == *thread && swipe.sent()));
        self.compact_model_dirty = true;
    }

    /// Whether the host of `swipe`'s thread still lists it where it was
    /// before the swipe.
    fn unmoved(&self, swipe: &RowSwipe, cx: &App) -> bool {
        self.store_of(&swipe.thread.host).is_some_and(|store| {
            store
                .read(cx)
                .thread_meta(&swipe.thread.session_id)
                .is_some_and(|meta| meta.is_settled() == swipe.unsettles())
        })
    }

    /// The threads the list leaves out for now: they are gone by a swipe, and
    /// the host has yet to say so.
    pub(super) fn swiped_off(&self, cx: &App) -> HashSet<ThreadKey> {
        self.row_swipes
            .rows
            .iter()
            .filter(|swipe| swipe.sent() && self.unmoved(swipe, cx))
            .map(|swipe| swipe.thread.clone())
            .collect()
    }

    /// The list is now `rows`. A row it has lost is at rest whenever it
    /// comes back, a row that can no longer swipe goes home, a row shows
    /// what its thread now lets it do, and a thread that has moved has
    /// finished its swipe.
    pub(super) fn reconcile_row_swipes(&mut self, rows: &[CompactListRow], cx: &App) {
        let now = cx.background_executor().now();
        let mut swipes = std::mem::take(&mut self.row_swipes.rows);
        let renaming = self.renaming.as_ref().map(|rename| &rename.thread);
        swipes.retain_mut(|swipe| {
            if swipe.sent() {
                return self.unmoved(swipe, cx);
            }
            let loading = self
                .store_of(&swipe.thread.host)
                .is_none_or(|store| store.read(cx).threads_loading());
            let row = rows.iter().find_map(|row| match row {
                CompactListRow::Thread(row) if row.state.thread == swipe.thread => Some(row),
                _ => None,
            });
            match row.filter(|_| !loading) {
                None => swipe.motion = Motion::Home(Flight::resting(px(0.), swipe.width, now)),
                Some(row) if !can_swipe(row, renaming) => swipe.send_home(now, cx),
                Some(row) => swipe.shows(action(row), now, cx),
            }
            swipe.finger.is_some() || !swipe.at_rest(now)
        });
        self.row_swipes.rows = swipes;
    }

    /// Lets go of the rows that have come to rest.
    pub(super) fn tidy_row_swipes(&mut self, cx: &App) {
        let now = cx.background_executor().now();
        self.row_swipes
            .rows
            .retain(|swipe| swipe.finger.is_some() || !swipe.at_rest(now));
    }
}

#[cfg(test)]
mod tests;
