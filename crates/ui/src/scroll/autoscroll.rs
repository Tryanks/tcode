//! Middle-button autoscroll: holding the middle button and moving the
//! pointer away from where it was pressed scrolls the content under that
//! point, faster the farther the pointer goes, until the button is released.
//!
//! Gap: gpui-base's `AutoScroll` scrolls only a drag selection at a
//! viewport's edges. Remove this once gpui-base has a pointer autoscroll.
//!
//! It starts only where something under the press has room to scroll. GPUI
//! keeps a container's extent to itself, so each scroller reports its room
//! as the press is captured: Tcode's areas ([`AreaMask`](super::AreaMask)),
//! its pages ([`page_viewport`](super::page_viewport)) and every other
//! scrolling element, through [`report_room`] or [`AutoscrollTarget`].
//!
//! Each frame the scroll is delivered as a wheel packet at the point that was
//! pressed, so it reaches whichever scroller is there, nested areas included,
//! exactly as a wheel would: the innermost one that can move on that axis
//! takes it. Packets are wheel lines, not pixels, so they never stretch an
//! edge ([`gpui_base::ScrollBounce`]) or open a gesture
//! ([`gesture`](super::gesture)).
//!
//! A press a field or a terminal claims (it stops the press propagating)
//! does not start one: there the middle button pastes the primary selection
//! on Linux, or is reported to the program reading the mouse.

#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
#[cfg(target_family = "wasm")]
use web_time::Instant;

use gpui::{
    AnyElement, App, Bounds, Context, DispatchPhase, Element, ElementId, Entity, Global,
    GlobalElementId, HitboxBehavior, HitboxId, InspectorElementId, InteractiveElement as _,
    IntoElement, LayoutId, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ParentElement as _, Pixels, PlatformInput, Point, RenderOnce, ScrollDelta, ScrollEvent,
    ScrollHandle, ScrollSource, Stateful, StatefulInteractiveElement, Styled as _, TouchPhase,
    Window, anchored, canvas, deferred, div, point, px,
};
use gpui::{LineCap, LineJoin, PathBuilder, PathStyle, StrokeOptions};
use gpui_base::ScrollbarHandle;

use crate::theme::ActiveTheme as _;

/// Within this distance of the press, on each axis, the content holds still.
const DEAD_ZONE: f32 = 12.;
/// The frame length [`speed`] is expressed in.
const SPEED_FRAME_MS: f32 = 20.;
/// The marker drawn where the button went down.
const MARKER_SIZE: f32 = 30.;
const MARKER_DOT: f32 = 4.;
const MARKER_STROKE: f32 = 1.5;
/// Each arrow's half-width across its axis, and its depth along it.
const MARKER_ARROW_HALF: f32 = 4.;
const MARKER_ARROW_DEPTH: f32 = 4.;
/// Content longer than its viewport by no more than this has nowhere to go:
/// a following list that fits reports a sliver of room to its bounce
/// (`FITTING_ROOM`).
const NO_ROOM: Pixels = px(1.);

/// Pixels the content moves per [`SPEED_FRAME_MS`] for a pointer `offset`
/// pixels from the press on one axis: Firefox's autoscroll curve, which
/// starts slowly past the dead zone and grows faster than the distance.
fn speed(offset: f32) -> f32 {
    let steps = offset / DEAD_ZONE;
    if steps > 1. {
        steps * steps.sqrt() - 1.
    } else if steps < -1. {
        steps * (-steps).sqrt() + 1.
    } else {
        0.
    }
}

/// The axes a scroller can move on.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(super) struct Axes {
    pub(super) horizontal: bool,
    pub(super) vertical: bool,
}

/// The axes the scrollers under the middle press being dispatched can move
/// on, gathered as it is captured.
#[derive(Default)]
struct PressRoom(Axes);

impl Global for PressRoom {}

/// The axes `handle`'s content overflows its viewport on.
pub(super) fn room(handle: &impl ScrollbarHandle) -> Axes {
    let content = handle.content_size();
    let viewport = handle.viewport_bounds().size;
    Axes {
        horizontal: content.width - viewport.width > NO_ROOM,
        vertical: content.height - viewport.height > NO_ROOM,
    }
}

/// Reports, for a middle press over `hitbox`, the room `handle` has on the
/// `axes` it scrolls. Call it while painting.
pub(super) fn listen_for_press<H: ScrollbarHandle + 'static>(
    hitbox: HitboxId,
    handle: H,
    axes: Axes,
    window: &mut Window,
) {
    window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
        if event.button != MouseButton::Middle
            || phase != DispatchPhase::Capture
            || !hitbox.should_handle_scroll(window)
        {
            return;
        }
        // Phones mount no `MiddleAutoscroll`, which sets this up.
        if !cx.has_global::<PressRoom>() {
            return;
        }
        let room = room(&handle);
        let PressRoom(press) = cx.global_mut::<PressRoom>();
        press.horizontal |= axes.horizontal && room.horizontal;
        press.vertical |= axes.vertical && room.vertical;
    });
}

/// Paints `child`, a scrolling element `handle` follows, and reports its
/// room to a middle press over its viewport. Adds no layout of its own.
pub(crate) fn report_room<H: ScrollbarHandle + Clone + 'static>(
    handle: &H,
    child: impl IntoElement,
) -> RoomReport<H> {
    RoomReport {
        handle: handle.clone(),
        child: child.into_any_element(),
        page: false,
    }
}

pub(crate) struct RoomReport<H> {
    handle: H,
    child: AnyElement,
    page: bool,
}

impl<H> RoomReport<H> {
    /// The viewport is a page's: a fling that runs into its end ends there
    /// ([`gesture::end_flings_at_page_end`](super::gesture::end_flings_at_page_end)).
    pub(super) fn page(mut self) -> Self {
        self.page = true;
        self
    }
}

impl<H: ScrollbarHandle + Clone + 'static> Element for RoomReport<H> {
    type RequestLayoutState = ();
    type PrepaintState = HitboxId;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> HitboxId {
        self.child.prepaint(window, cx);
        // After the child, whose layout has just given the handle its bounds.
        window
            .insert_hitbox(self.handle.viewport_bounds(), HitboxBehavior::Normal)
            .id
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        hitbox: &mut HitboxId,
        window: &mut Window,
        cx: &mut App,
    ) {
        if self.page {
            super::gesture::end_flings_at_page_end(*hitbox, self.handle.clone(), window);
        }
        self.child.paint(window, cx);
        listen_for_press(
            *hitbox,
            self.handle.clone(),
            Axes {
                horizontal: true,
                vertical: true,
            },
            window,
        );
    }
}

/// A scrolling element that follows no scroll handle of its own, given one
/// so that it can [`report_room`]. Call it last, once the element is built.
pub(crate) trait AutoscrollTarget: StatefulInteractiveElement + Element {
    fn autoscroll_target(self) -> Tracked<Self> {
        Tracked { element: self }
    }
}

impl<E> AutoscrollTarget for Stateful<E> where Stateful<E>: StatefulInteractiveElement + Element {}

#[derive(IntoElement)]
pub(crate) struct Tracked<E: StatefulInteractiveElement + Element> {
    element: E,
}

impl<E: StatefulInteractiveElement + Element> RenderOnce for Tracked<E> {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let id = Element::id(&self.element).expect("a stateful element has an id");
        let handle = window
            .use_keyed_state((id, "autoscroll"), cx, |_, _| ScrollHandle::new())
            .read(cx)
            .clone();
        report_room(&handle, self.element.track_scroll(&handle))
    }
}

struct Session {
    /// Told apart from a later one, whose frames must not run this one's.
    id: u64,
    anchor: Point<Pixels>,
    pointer: Point<Pixels>,
    last_frame: Instant,
    room: Axes,
}

#[derive(Default)]
struct Autoscroll {
    session: Option<Session>,
    sessions: u64,
}

impl Autoscroll {
    fn start(
        &mut self,
        event: &MouseDownEvent,
        room: Axes,
        now: Instant,
        cx: &mut Context<Self>,
    ) -> u64 {
        self.sessions += 1;
        self.session = Some(Session {
            id: self.sessions,
            anchor: event.position,
            pointer: event.position,
            last_frame: now,
            room,
        });
        cx.notify();
        self.sessions
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        if self.session.take().is_some() {
            cx.notify();
        }
    }

    /// The scroll, in pixels, the frame at `now` owes session `id`, or `None`
    /// once that session has ended.
    fn frame(&mut self, id: u64, now: Instant) -> Option<Frame> {
        let session = self.session.as_mut().filter(|session| session.id == id)?;
        let elapsed = now.saturating_duration_since(session.last_frame);
        session.last_frame = now;
        let frames = elapsed.as_secs_f32() * 1000. / SPEED_FRAME_MS;
        let offset = session.pointer - session.anchor;
        // Content moves toward the pointer: a pointer below the press reveals
        // what is below, which is a wheel's negative delta. An axis nothing
        // under the press scrolls on is left out, so that moving across it
        // does not take the packet from the other.
        let along = |room: bool, offset: Pixels| {
            if room {
                px(-speed(offset.as_f32()) * frames)
            } else {
                px(0.)
            }
        };
        Some(Frame {
            anchor: session.anchor,
            pointer: session.pointer,
            delta: point(
                along(session.room.horizontal, offset.x),
                along(session.room.vertical, offset.y),
            ),
        })
    }
}

struct Frame {
    anchor: Point<Pixels>,
    pointer: Point<Pixels>,
    delta: Point<Pixels>,
}

/// Runs a frame of session `id`, then asks for the next one.
fn run_frame(state: Entity<Autoscroll>, id: u64, window: &mut Window, cx: &mut App) {
    let now = cx.background_executor().now();
    let Some(Frame {
        anchor,
        pointer,
        delta,
    }) = state.update(cx, |state, _| state.frame(id, now))
    else {
        return;
    };
    if delta != Point::default() {
        let line_height = window.line_height();
        let modifiers = window.modifiers();
        // The keys held mean nothing to this scroll, as in a browser: Shift
        // would turn a wheel sideways.
        window.dispatch_event(
            PlatformInput::Scroll(ScrollEvent {
                position: anchor,
                delta: ScrollDelta::Lines(point(delta.x / line_height, delta.y / line_height)),
                modifiers: Modifiers::default(),
                touch_phase: TouchPhase::Moved,
                source: ScrollSource::Pointer,
            }),
            cx,
        );
        // A wheel packet moves the window's pointer to where it points; put
        // it back where it is, for hover and for what the next press hits.
        window.dispatch_event(
            PlatformInput::MouseMove(MouseMoveEvent {
                position: pointer,
                pressed_button: Some(MouseButton::Middle),
                modifiers,
            }),
            cx,
        );
    }
    window.on_next_frame(move |window, cx| run_frame(state, id, window, cx));
}

/// Listens for the middle button across the window and marks where an
/// autoscroll started. Mount it before the window's content, so that it hears
/// a press before the scrollers under it report their room, and again only
/// after everything under the pointer has had the chance to claim it.
#[derive(IntoElement)]
pub(crate) struct MiddleAutoscroll;

impl RenderOnce for MiddleAutoscroll {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = window.use_keyed_state("middle-autoscroll", cx, |_, _| Autoscroll::default());
        let marked = state
            .read(cx)
            .session
            .as_ref()
            .map(|session| (session.anchor, session.room));
        let listeners = canvas(
            |_, _, _| {},
            move |_, _, window, _| {
                let down = state.clone();
                window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                    let active = down.read(cx).session.is_some();
                    if event.button != MouseButton::Middle {
                        // Another button ends it, as in a browser.
                        if active && phase == DispatchPhase::Capture {
                            down.update(cx, |state, cx| state.stop(cx));
                        }
                        return;
                    }
                    if active {
                        return;
                    }
                    if phase == DispatchPhase::Capture {
                        cx.set_global(PressRoom::default());
                        return;
                    }
                    let PressRoom(room) = *cx.global::<PressRoom>();
                    if !room.horizontal && !room.vertical {
                        return;
                    }
                    let now = cx.background_executor().now();
                    let id = down.update(cx, |state, cx| state.start(event, room, now, cx));
                    let state = down.clone();
                    window.on_next_frame(move |window, cx| run_frame(state, id, window, cx));
                });
                let up = state.clone();
                window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                    if event.button == MouseButton::Middle && phase == DispatchPhase::Capture {
                        up.update(cx, |state, cx| state.stop(cx));
                    }
                });
                let moved = state.clone();
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                    if phase != DispatchPhase::Capture || moved.read(cx).session.is_none() {
                        return;
                    }
                    moved.update(cx, |state, _| {
                        if let Some(session) = state.session.as_mut() {
                            session.pointer = event.position;
                        }
                    });
                });
            },
        )
        .absolute()
        .size(px(0.));
        div()
            .absolute()
            .size(px(0.))
            .child(listeners)
            .children(marked.map(|(anchor, room)| marker(anchor, room, cx)))
    }
}

/// A round badge centred where the button went down, with an arrow toward
/// each way the content under it can scroll, drawn as Firefox draws its own:
/// GPUI has no panning cursor to show instead, as Chrome does. The arrows are
/// stroked here rather than taken from the icon set, whose strokes are too
/// thin at this size.
fn marker(anchor: Point<Pixels>, room: Axes, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let ink = theme.foreground.opacity(0.7);
    let arrows = canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let centre = bounds.center();
            // The tip lies as far inside the border as the arrow's back lies
            // outside the dot.
            let edge = MARKER_SIZE / 2. - 1.;
            let reach = px((edge + MARKER_DOT / 2. + MARKER_ARROW_DEPTH) / 2.);
            let (half, depth) = (px(MARKER_ARROW_HALF), px(MARKER_ARROW_DEPTH));
            let mut chevron = |tip: Point<Pixels>, back: Point<Pixels>, side: Point<Pixels>| {
                let mut path = PathBuilder::stroke(px(MARKER_STROKE)).with_style(
                    PathStyle::Stroke(StrokeOptions {
                        line_width: MARKER_STROKE,
                        line_cap: LineCap::Round,
                        line_join: LineJoin::Round,
                        ..Default::default()
                    }),
                );
                path.move_to(back + side);
                path.line_to(tip);
                path.line_to(back - side);
                if let Ok(path) = path.build() {
                    window.paint_path(path, ink);
                }
            };
            if room.vertical {
                let side = point(half, px(0.));
                let up = point(centre.x, centre.y - reach);
                chevron(up, point(up.x, up.y + depth), side);
                let down = point(centre.x, centre.y + reach);
                chevron(down, point(down.x, down.y - depth), side);
            }
            if room.horizontal {
                let side = point(px(0.), half);
                let left = point(centre.x - reach, centre.y);
                chevron(left, point(left.x + depth, left.y), side);
                let right = point(centre.x + reach, centre.y);
                chevron(right, point(right.x - depth, right.y), side);
            }
        },
    )
    .absolute()
    .size_full();
    // Children are placed inside the 1 px border.
    let dot_at = px((MARKER_SIZE - 2. - MARKER_DOT) / 2.);
    let badge = div()
        .debug_selector(|| "middle-autoscroll-marker".into())
        .relative()
        .size(px(MARKER_SIZE))
        .rounded_full()
        .bg(theme.popover)
        .border_1()
        .border_color(theme.border)
        .child(
            div()
                .absolute()
                .left(dot_at)
                .top(dot_at)
                .size(px(MARKER_DOT))
                .rounded_full()
                .bg(ink),
        )
        .child(arrows);
    let half = px(MARKER_SIZE / 2.);
    deferred(
        anchored()
            .position_mode(gpui::AnchoredPositionMode::Window)
            .position(point(anchor.x - half, anchor.y - half))
            .child(badge),
    )
    .with_priority(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scroll::ScrollableElement as _;
    use crate::widgets::input::{Input, InputState};
    use gpui::{AppContext as _, Render, ScrollHandle, TestAppContext, VisualTestContext};

    struct Page {
        scroll: ScrollHandle,
        field: Entity<InputState>,
    }

    impl Render for Page {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(300.))
                .h(px(300.))
                .flex()
                .child(MiddleAutoscroll)
                .child(
                    div()
                        .id("long")
                        .w(px(150.))
                        .h_full()
                        .overflow_y_scroll_area()
                        .track_scroll(&self.scroll)
                        .child(
                            div()
                                .debug_selector(|| "field".into())
                                .child(Input::new(&self.field)),
                        )
                        .child(div().h(px(3000.)).flex_none()),
                )
                .child(
                    div()
                        .id("short")
                        .w(px(150.))
                        .h_full()
                        .overflow_y_scroll_area()
                        .child(div().h(px(50.)).flex_none()),
                )
        }
    }

    /// Presses the middle button at `at`, moves to `to` and lets a frame
    /// pass.
    fn hold_middle(cx: &mut VisualTestContext, at: Point<Pixels>, to: Point<Pixels>) {
        cx.simulate_event(MouseDownEvent {
            button: MouseButton::Middle,
            position: at,
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        cx.simulate_event(MouseMoveEvent {
            position: to,
            pressed_button: Some(MouseButton::Middle),
            modifiers: Modifiers::default(),
        });
        next_frame(cx);
    }

    fn release_middle(cx: &mut VisualTestContext, at: Point<Pixels>) {
        cx.simulate_event(MouseUpEvent {
            button: MouseButton::Middle,
            position: at,
            modifiers: Modifiers::default(),
            click_count: 1,
        });
    }

    fn next_frame(cx: &mut VisualTestContext) {
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(16));
        cx.update(|window, cx| window.simulate_next_frame(cx));
        crate::test_frames::draw(cx);
    }

    fn marked(cx: &mut VisualTestContext) -> bool {
        cx.debug_bounds("middle-autoscroll-marker").is_some()
    }

    /// Holding the middle button and moving down scrolls the area under the
    /// press until the button is let go. A press in a field, which owns the
    /// button, or over content that fits its area starts nothing.
    #[gpui::test]
    fn holding_the_middle_button_scrolls_the_area_under_the_press(cx: &mut TestAppContext) {
        cx.update(crate::test_client::init);
        let (page, cx) = cx.add_window_view(|window, cx| Page {
            scroll: ScrollHandle::new(),
            field: cx.new(|cx| InputState::new(window, cx)),
        });
        crate::test_frames::draw_settled(cx);
        let offset =
            |cx: &mut VisualTestContext| page.read_with(cx, |page, _| page.scroll.offset().y);

        let fits = point(px(225.), px(150.));
        hold_middle(cx, fits, point(px(225.), px(250.)));
        assert!(
            !marked(cx),
            "a press over content that fits started an autoscroll"
        );
        release_middle(cx, point(px(225.), px(250.)));

        let field = cx
            .debug_bounds("field")
            .expect("the field is drawn")
            .center();
        hold_middle(cx, field, point(field.x, field.y + px(150.)));
        assert!(!marked(cx), "a press in a field started an autoscroll");
        release_middle(cx, point(field.x, field.y + px(150.)));

        let press = point(px(75.), px(150.));
        let below = point(px(75.), px(250.));
        hold_middle(cx, press, below);
        assert!(marked(cx), "no marker where the autoscroll started");
        let scrolled = offset(cx);
        assert!(scrolled < px(0.), "the area did not scroll down");

        release_middle(cx, below);
        next_frame(cx);
        assert_eq!(
            offset(cx),
            scrolled,
            "the area went on scrolling after release"
        );
    }
}
