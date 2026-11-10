//! Tcode's vertical overlay scrollbar.
//!
//! Gap: gpui-base's `Scrollbar` keeps its fade, its hover and its thumb's
//! width in a private `ScrollbarState` under its element path
//! (`Window::use_state`), with no owner a view could hold instead. A
//! scrollbar its view draws under another parent (a pane a zoom lifts over
//! the column, a page as the window turns compact or wide) started there
//! hidden, whatever it was doing. This is that scrollbar rebuilt over
//! gpui-base's pieces — its `ScrollbarHandle`, the mode and
//! `ScrollbarMotion` its theme carries, its easing — with the same layout,
//! look, choreography and timings, its state kept by the view's
//! [`Owned`] when it has one ([`Scrollbar::motion`]). It also samples the
//! executor's clock, as every other motion does, where gpui-base's reads the
//! wall clock. Back to gpui-base's once it keys that state by an owner.

use std::cell::Cell;
use std::panic::Location;
use std::rc::Rc;
use std::time::Duration;

#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
#[cfg(target_family = "wasm")]
use web_time::Instant;

use gpui::{
    Anchor, App, Bounds, ContentMask, CursorStyle, Element, ElementId, EntityId, GlobalElementId,
    Hitbox, HitboxBehavior, Hsla, InspectorElementId, IntoElement, IsZero as _, LayoutId,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Position, ScrollEvent, Size,
    Style, TouchDragEvent, TouchPhase, Window, fill, point, px, relative, size,
};
use gpui_base::animation::{ease_in_cubic, ease_out_cubic};
use gpui_base::{ScrollbarEntrance, ScrollbarHandle, ScrollbarMode};

use crate::motion::owned::Owned;
use crate::theme::{ActiveTheme as _, Theme};

/// gpui-base's track: the widest thumb and its inset on both sides.
pub(crate) const TRACK: Pixels = px(16.);
const MIN_THUMB: Pixels = px(48.);
/// The thumb at rest, and under the pointer or dragged.
const THUMB: Pixels = px(6.);
const THUMB_ACTIVE: Pixels = px(8.);
const THUMB_INSET: Pixels = px(4.);
/// How often a drag redraws the view at most.
const DRAG_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 120);

/// What a scrollbar paints with, from the theme: one owner for Tcode's
/// scrollbar and the styles `theme::install` gives gpui-base's own.
pub(crate) struct Look {
    pub(crate) track: Hsla,
    pub(crate) thumb: Hsla,
    /// The thumb under the pointer or dragged.
    pub(crate) thumb_hover: Hsla,
    pub(crate) radius: Pixels,
}

impl Look {
    pub(crate) fn of(theme: &Theme) -> Self {
        Self {
            track: theme.scrollbar,
            thumb: theme.scrollbar_thumb,
            thumb_hover: theme.scrollbar_thumb_hover,
            radius: if theme.tokens.radius.md.is_zero() {
                px(0.)
            } else {
                theme.tokens.radius.full
            },
        }
    }
}

/// A vertical scrollbar over the viewport of a scroll handle.
pub(crate) struct Scrollbar {
    id: ElementId,
    handle: Rc<dyn ScrollbarHandle>,
    owner: Option<Owned>,
    use_layout_bounds: bool,
}

impl Scrollbar {
    #[track_caller]
    pub(crate) fn vertical<H: ScrollbarHandle + Clone>(handle: &H) -> Self {
        Self {
            id: ElementId::CodeLocation(*Location::caller()),
            handle: Rc::new(handle.clone()),
            owner: None,
            use_layout_bounds: false,
        }
    }

    pub(crate) fn id(mut self, id: impl Into<ElementId>) -> Self {
        self.id = id.into();
        self
    }

    /// Keep the scrollbar's state in `owner`, the view's, so it carries on
    /// wherever the view is drawn; in element state otherwise.
    pub(crate) fn motion(mut self, owner: &Owned) -> Self {
        self.owner = Some(owner.clone());
        self
    }

    /// Overlay the scrollbar's own layout bounds rather than the handle's
    /// viewport.
    pub(crate) fn viewport_from_layout(mut self) -> Self {
        self.use_layout_bounds = true;
        self
    }
}

/// A value travelling from `from` to `target` over `duration`.
#[derive(Clone, Copy)]
struct Leg<T> {
    from: T,
    target: T,
    started_at: Instant,
    duration: Duration,
}

impl<T: Copy + PartialEq> Leg<T> {
    fn settled(value: T, now: Instant) -> Self {
        Self {
            from: value,
            target: value,
            started_at: now,
            duration: Duration::ZERO,
        }
    }

    /// Where it is at `now`, and whether it is still moving.
    fn sample(&self, now: Instant, interpolate: impl FnOnce(T, T, f32) -> T) -> (T, bool) {
        if self.from == self.target || self.duration.is_zero() {
            return (self.target, false);
        }
        let linear = now.saturating_duration_since(self.started_at).as_secs_f32()
            / self.duration.as_secs_f32();
        if linear >= 1. {
            (self.target, false)
        } else {
            (interpolate(self.from, self.target, linear.max(0.)), true)
        }
    }

    fn start(&mut self, from: T, target: T, duration: Duration, now: Instant) {
        *self = Self {
            from,
            target,
            started_at: now,
            duration,
        };
    }
}

/// The thumb's width on its way to the width its state gives it.
#[derive(Clone, Copy)]
struct Width {
    leg: Leg<Pixels>,
    initialized: bool,
}

impl Width {
    fn sample(&self, now: Instant) -> (Pixels, bool) {
        self.leg.sample(now, |from, target, t| {
            from + (target - from) * ease_out_cubic(t)
        })
    }

    /// Toward `target` over `duration`; the first width, and any with a
    /// zero duration, is taken at once.
    fn set_target(&mut self, target: Pixels, duration: Duration, now: Instant) -> (Pixels, bool) {
        if duration.is_zero() || !self.initialized {
            self.leg = Leg::settled(target, now);
            self.initialized = true;
        } else if self.leg.target != target {
            let from = self.sample(now).0;
            self.leg.start(from, target, duration, now);
        }
        self.sample(now)
    }
}

/// How far the scrollbar is in: its opacity and, sliding in from the edge,
/// its position.
#[derive(Clone, Copy)]
struct Visibility {
    opacity: Leg<f32>,
    position: Leg<f32>,
    entrance: ScrollbarEntrance,
}

#[derive(Clone, Copy)]
struct Shown {
    opacity: f32,
    position: f32,
    running: bool,
}

impl Visibility {
    fn hidden(now: Instant) -> Self {
        Self {
            opacity: Leg::settled(0., now),
            position: Leg::settled(0., now),
            entrance: ScrollbarEntrance::Fade,
        }
    }

    /// Coming in, it fades linearly and slides easing out; going, both ease
    /// in.
    fn sample(&self, now: Instant) -> Shown {
        let entering =
            self.opacity.target > self.opacity.from || self.position.target > self.position.from;
        let (opacity, fading) = self.opacity.sample(now, |from, target, t| {
            from + (target - from) * if entering { t } else { ease_in_cubic(t) }
        });
        let (position, sliding) = self.position.sample(now, |from, target, t| {
            from + (target - from)
                * if entering {
                    ease_out_cubic(t)
                } else {
                    ease_in_cubic(t)
                }
        });
        Shown {
            opacity,
            position,
            running: fading || sliding,
        }
    }

    /// Toward `visible`, over `enter` or `exit` scaled by the distance left,
    /// so a turn part way keeps its speed.
    fn set_visible(
        &mut self,
        visible: bool,
        entrance: ScrollbarEntrance,
        enter: Duration,
        exit: Duration,
        now: Instant,
    ) {
        let target = if visible { 1. } else { 0. };
        let full = if visible { enter } else { exit };
        if full.is_zero() {
            self.opacity = Leg::settled(target, now);
            self.position = Leg::settled(target, now);
            self.entrance = entrance;
            return;
        }
        if self.opacity.target == target
            && self.position.target == target
            && self.entrance == entrance
        {
            return;
        }
        let shown = self.sample(now);
        let from_position = if visible && entrance == ScrollbarEntrance::Fade {
            1.
        } else {
            shown.position
        };
        let distance = (target - shown.opacity)
            .abs()
            .max((target - from_position).abs());
        let duration = full.mul_f32(distance);
        self.opacity.start(shown.opacity, target, duration, now);
        self.position.start(from_position, target, duration, now);
        self.entrance = entrance;
    }
}

#[derive(Clone, Copy)]
struct State {
    hovered_bar: bool,
    hovered_thumb: bool,
    dragging: bool,
    /// Where in the thumb the drag holds it.
    grab: Pixels,
    last_offset: Point<Pixels>,
    /// The viewport's size as of `last_offset`.
    last_viewport: Size<Pixels>,
    last_scroll: Option<Instant>,
    last_update: Instant,
    drag_update_scheduled: bool,
    drag_generation: u64,
    idle_timer_scheduled: bool,
    visibility: Visibility,
    width: Width,
}

impl State {
    fn new(now: Instant) -> Self {
        Self {
            hovered_bar: false,
            hovered_thumb: false,
            dragging: false,
            grab: px(0.),
            last_offset: Point::default(),
            last_viewport: Size::default(),
            last_scroll: None,
            last_update: now,
            drag_update_scheduled: false,
            drag_generation: 0,
            idle_timer_scheduled: false,
            visibility: Visibility::hidden(now),
            width: Width {
                leg: Leg::settled(px(0.), now),
                initialized: false,
            },
        }
    }

    fn visible(&self, now: Instant) -> bool {
        self.dragging || self.visibility.sample(now).opacity > 0.
    }

    fn start_drag(&mut self, grab: Pixels, now: Instant) {
        self.grab = grab;
        self.dragging = true;
        self.last_update = now;
        self.drag_update_scheduled = false;
        self.drag_generation = self.drag_generation.wrapping_add(1);
    }

    fn end_drag(&mut self, now: Instant) {
        self.dragging = false;
        self.drag_update_scheduled = false;
        self.drag_generation = self.drag_generation.wrapping_add(1);
        self.last_scroll = Some(now);
    }
}

/// A scrollbar's state, shared by its frame's handlers and kept by its
/// owner or its element.
#[derive(Clone)]
struct Shared(Rc<Cell<Option<State>>>);

impl Default for Shared {
    fn default() -> Self {
        Self(Rc::new(Cell::new(None)))
    }
}

impl Shared {
    fn get(&self, now: Instant) -> State {
        self.0.get().unwrap_or_else(|| State::new(now))
    }

    fn set(&self, state: State) {
        self.0.set(Some(state));
    }

    fn update(&self, now: Instant, change: impl FnOnce(&mut State)) {
        let mut state = self.get(now);
        change(&mut state);
        self.set(state);
    }

    /// Redraw `view` for a drag, at most once per [`DRAG_INTERVAL`],
    /// keeping a trailing redraw for the last move.
    fn notify_drag(&self, now: Instant, view: EntityId, window: &mut Window, cx: &mut App) {
        let mut state = self.get(now);
        if state.drag_update_scheduled {
            return;
        }
        let elapsed = now.saturating_duration_since(state.last_update);
        if elapsed >= DRAG_INTERVAL {
            state.last_update = now;
            self.set(state);
            cx.notify(view);
            return;
        }
        state.drag_update_scheduled = true;
        self.set(state);
        let generation = state.drag_generation;
        let shared = self.clone();
        window
            .spawn(cx, async move |cx| {
                cx.background_executor()
                    .timer(DRAG_INTERVAL - elapsed)
                    .await;
                let now = cx.background_executor().now();
                let mut state = shared.get(now);
                // A release or a new drag supersedes it.
                if !state.drag_update_scheduled || state.drag_generation != generation {
                    return;
                }
                state.drag_update_scheduled = false;
                state.last_update = now;
                shared.set(state);
                cx.update(|_, cx| cx.notify(view)).ok();
            })
            .detach();
    }
}

/// The thumb's place along the track: `length` without its inset, `travel`
/// what the whole thumb can move, `extent` what the content can scroll.
#[derive(Clone, Copy)]
struct Geometry {
    origin: Pixels,
    inset: Pixels,
    length: Pixels,
    travel: Pixels,
    extent: Pixels,
}

impl Geometry {
    fn new(
        origin: Pixels,
        track: Pixels,
        viewport: Pixels,
        content: Pixels,
        inset: Pixels,
    ) -> Self {
        let length = (viewport / content * track).max(MIN_THUMB).min(track);
        let inset = inset.clamp(px(0.), length / 2.);
        Self {
            origin,
            inset,
            length: length - inset * 2.,
            travel: track - length,
            extent: content - viewport,
        }
    }

    fn start(self, offset: Pixels) -> Pixels {
        self.origin + self.inset + (-offset / self.extent).clamp(0., 1.) * self.travel
    }

    /// The scroll offset that puts the thumb, held `grab` into it, at
    /// `position`.
    fn offset(self, position: Pixels, grab: Pixels, mut offset: Point<Pixels>) -> Point<Pixels> {
        if self.travel <= px(0.) {
            return offset;
        }
        offset.y = -self.extent
            * ((position - self.origin - self.inset - grab) / self.travel).clamp(0., 1.);
        offset
    }
}

pub(crate) struct Prepainted {
    hitbox: Hitbox,
    shared: Shared,
    bar: Option<Bar>,
}

struct Bar {
    hitbox: Hitbox,
    bounds: Bounds<Pixels>,
    track: Hsla,
    thumb: Hsla,
    radius: Pixels,
    /// Where the thumb takes the pointer, and where it is painted.
    thumb_bounds: Bounds<Pixels>,
    thumb_fill: Bounds<Pixels>,
    geometry: Geometry,
    active_geometry: Geometry,
    opacity: f32,
    position: f32,
    wanted: bool,
}

impl IntoElement for Scrollbar {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Scrollbar {
    type RequestLayoutState = ();
    type PrepaintState = Prepainted;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let style = Style {
            position: Position::Absolute,
            flex_grow: 1.,
            flex_shrink: 1.,
            size: size(relative(1.).into(), relative(1.).into()),
            ..Default::default()
        };
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Prepainted {
        let bounds = if self.use_layout_bounds {
            bounds
        } else {
            self.handle.viewport_bounds()
        };
        let hitbox = window.with_content_mask(Some(ContentMask { bounds }), |window| {
            window.insert_hitbox(bounds, HitboxBehavior::Normal)
        });
        let shared = crate::motion::Clock::from(self.owner.as_ref()).keep::<Shared>(
            ElementId::NamedChild(std::sync::Arc::new(self.id.clone()), "scrollbar".into()),
            window,
            cx,
        );

        let now = cx.background_executor().now();
        let base = cx.global::<gpui_base::Theme>();
        let mode = base.scrollbar.mode();
        let motion = base.scrollbar.motion();
        // An always-visible scrollbar has no fade, but its thumb still
        // widens; reduced motion snaps both.
        let reduce = cx.reduce_motion();
        let (enter, exit) = if mode != ScrollbarMode::Always && !reduce {
            (motion.enter(), motion.exit())
        } else {
            (Duration::ZERO, Duration::ZERO)
        };
        let expand = if reduce {
            Duration::ZERO
        } else {
            motion.expand()
        };

        let mut state = shared.get(now);
        let offset = self.handle.offset();
        // Gap: gpui-base's scrollbar comes in on any change of the offset,
        // so a window resize, which moves a list held at its end, shows it
        // and fades it out again once the idle hold ends, drawing the view
        // frame by frame long after the size settled. Only a change of the
        // offset within a viewport of the same size is a scroll here.
        let viewport_size = self.handle.viewport_bounds().size;
        let resized =
            state.last_viewport != Size::default() && state.last_viewport != viewport_size;
        if offset != state.last_offset {
            state.last_offset = offset;
            if !resized {
                state.last_scroll = Some(now);
            }
        }
        state.last_viewport = viewport_size;
        let hovered = state.hovered_bar || state.hovered_thumb;
        let showing = state.visibility.sample(now).opacity > 0.;
        // Hovered, it stays in once shown, or comes in when hover shows it.
        let wanted = (hovered
            && (mode == ScrollbarMode::Hover || (mode == ScrollbarMode::Scrolling && showing)))
            || mode == ScrollbarMode::Always
            || state.dragging
            || state
                .last_scroll
                .is_some_and(|last| now.saturating_duration_since(last) < motion.idle());
        let entrance = if mode == ScrollbarMode::Hover && state.hovered_thumb {
            motion.thumb_hover_entrance()
        } else {
            motion.entrance()
        };
        state
            .visibility
            .set_visible(wanted, entrance, enter, exit, now);
        let shown = state.visibility.sample(now);
        if shown.running {
            window.request_animation_frame();
        }
        // The hold after the last scroll ends with a draw, which fades it out.
        if !hovered
            && !state.dragging
            && let Some(last) = state.last_scroll
        {
            let elapsed = now.saturating_duration_since(last);
            if elapsed < motion.idle() && !state.idle_timer_scheduled {
                state.idle_timer_scheduled = true;
                let shared = shared.clone();
                let view = window.current_view();
                let delay = motion.idle() - elapsed;
                window
                    .spawn(cx, async move |cx| {
                        cx.background_executor().timer(delay).await;
                        let now = cx.background_executor().now();
                        shared.update(now, |state| state.idle_timer_scheduled = false);
                        cx.update(|_, cx| cx.notify(view)).ok();
                    })
                    .detach();
            }
        }

        let content = self.handle.content_size().height;
        let viewport = hitbox.size.height;
        if content <= viewport {
            shared.set(state);
            return Prepainted {
                hitbox,
                shared,
                bar: None,
            };
        }
        let track = Bounds {
            origin: point(hitbox.origin.x + hitbox.size.width - TRACK, hitbox.origin.y),
            size: size(TRACK, hitbox.size.height),
        };
        let look = Look::of(cx.theme());
        let (thumb, width) = if state.dragging || state.hovered_thumb {
            (look.thumb_hover, THUMB_ACTIVE)
        } else {
            (look.thumb, THUMB)
        };
        let (width, widening) = state.width.set_target(width, expand, now);
        if widening {
            window.request_animation_frame();
        }
        shared.set(state);

        let geometry = Geometry::new(track.origin.y, viewport, viewport, content, THUMB_INSET);
        let active_geometry = geometry;
        let start = geometry.start(offset.y) - track.origin.y;
        let anchor = track.top_right() + point(-THUMB_INSET, start);
        let thumb_bounds =
            Bounds::from_anchor_and_size(Anchor::TopRight, anchor, size(TRACK, geometry.length));
        let thumb_fill =
            Bounds::from_anchor_and_size(Anchor::TopRight, anchor, size(width, geometry.length));
        let bar_hitbox = window.with_content_mask(Some(ContentMask { bounds: track }), |window| {
            window.insert_hitbox(track, HitboxBehavior::Normal)
        });
        Prepainted {
            hitbox,
            shared,
            bar: Some(Bar {
                hitbox: bar_hitbox,
                bounds: track,
                track: look.track,
                thumb,
                radius: look.radius,
                thumb_bounds,
                thumb_fill,
                geometry,
                active_geometry,
                opacity: shown.opacity,
                position: shown.position,
                wanted,
            }),
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        prepainted: &mut Prepainted,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(bar) = &prepainted.bar else {
            return;
        };
        let hover_mode = cx.global::<gpui_base::Theme>().scrollbar.mode() == ScrollbarMode::Hover;
        let view = window.current_view();
        let area = prepainted.hitbox.bounds;
        let visible = bar.wanted || bar.opacity > 0.;
        let thumb_bounds = bar.thumb_bounds;
        let geometry = bar.geometry;
        let active = bar.active_geometry;
        window.with_content_mask(Some(ContentMask { bounds: area }), |window| {
            // Sliding in, it comes from beyond the trailing edge.
            let slide = point(TRACK * (1. - bar.position.clamp(0., 1.)), px(0.));
            let radius = bar
                .radius
                .min(bar.thumb_fill.size.width / 2.)
                .min(bar.thumb_fill.size.height / 2.);
            window.set_cursor_style(CursorStyle::default(), &bar.hitbox);
            window.paint_layer(area, |window| {
                window.paint_quad(fill(bar.bounds + slide, bar.track.opacity(bar.opacity)));
                window.paint_quad(
                    fill(bar.thumb_fill + slide, bar.thumb.opacity(bar.opacity))
                        .corner_radii(radius),
                );
            });

            window.on_mouse_event({
                let shared = prepainted.shared.clone();
                let handle = self.handle.clone();
                move |event: &ScrollEvent, phase, _, cx| {
                    if phase.bubble() && area.contains(&event.position) {
                        let now = cx.background_executor().now();
                        let state = shared.get(now);
                        if handle.offset() != state.last_offset {
                            shared.update(now, |state| {
                                state.last_offset = handle.offset();
                                state.last_scroll = Some(now);
                            });
                            cx.notify(view);
                        }
                    }
                }
            });

            // A thumb follows the finger, unlike content panning: the touch
            // is claimed before the window turns it into wheel deltas.
            window.on_mouse_event({
                let shared = prepainted.shared.clone();
                let handle = self.handle.clone();
                move |event: &TouchDragEvent, phase, window, cx| {
                    if !phase.bubble() {
                        return;
                    }
                    let now = cx.background_executor().now();
                    if event.phase == TouchPhase::Started {
                        if !visible
                            || window.default_prevented()
                            || !thumb_bounds.contains(&event.start_position)
                        {
                            return;
                        }
                        handle.start_drag();
                        shared.update(now, |state| {
                            state.start_drag(event.start_position.y - thumb_bounds.origin.y, now)
                        });
                    } else if !shared.get(now).dragging {
                        return;
                    }
                    if event.phase != TouchPhase::Cancelled {
                        let grab = shared.get(now).grab;
                        handle.set_offset(active.offset(event.position.y, grab, handle.offset()));
                    }
                    if matches!(event.phase, TouchPhase::Ended | TouchPhase::Cancelled) {
                        handle.end_drag();
                        shared.update(now, |state| state.end_drag(now));
                    }
                    window.prevent_default();
                    cx.stop_propagation();
                    if event.phase == TouchPhase::Moved {
                        shared.notify_drag(now, view, window, cx);
                    } else {
                        cx.notify(view);
                    }
                }
            });

            if visible {
                window.on_mouse_event({
                    // The hitbox rather than the bounds, so that an element
                    // above the bar (a menu, a dialog) keeps the press.
                    let bar_hitbox = bar.hitbox.clone();
                    let shared = prepainted.shared.clone();
                    let handle = self.handle.clone();
                    move |event: &MouseDownEvent, phase, window, cx| {
                        if !phase.bubble() || !bar_hitbox.is_hovered(window) {
                            return;
                        }
                        cx.stop_propagation();
                        let now = cx.background_executor().now();
                        if thumb_bounds.contains(&event.position) {
                            let grab = event.position.y - thumb_bounds.origin.y;
                            handle.start_drag();
                            shared.update(now, |state| state.start_drag(grab, now));
                            handle.set_offset(active.offset(
                                event.position.y,
                                grab,
                                handle.offset(),
                            ));
                        } else {
                            // A press on the track centres the thumb there.
                            handle.set_offset(geometry.offset(
                                event.position.y,
                                geometry.length / 2.,
                                handle.offset(),
                            ));
                        }
                        cx.notify(view);
                    }
                });
            }

            window.on_mouse_event({
                let bar_hitbox = bar.hitbox.clone();
                let shared = prepainted.shared.clone();
                let handle = self.handle.clone();
                move |event: &MouseMoveEvent, _, window, cx| {
                    let now = cx.background_executor().now();
                    let mut state = shared.get(now);
                    let mut notify = false;
                    // Hovering a scrollbar that is shown, or one shown by
                    // hover, keeps it shown.
                    let bar_hovered = bar_hitbox.is_hovered(window);
                    if bar_hovered && (hover_mode || visible) {
                        notify |= !state.hovered_bar;
                        state.hovered_bar = true;
                        state.last_scroll = Some(now);
                    } else if state.hovered_bar {
                        state.hovered_bar = false;
                        state.last_scroll = Some(now);
                        notify = true;
                    }
                    let on_thumb = (hover_mode || visible)
                        && bar_hovered
                        && thumb_bounds.contains(&event.position);
                    if on_thumb != state.hovered_thumb {
                        if on_thumb && state.visible(now) {
                            state.last_scroll = Some(now);
                        }
                        state.hovered_thumb = on_thumb;
                        notify = true;
                    }
                    if state.dragging && event.dragging() {
                        // Not a text selection, nor anything else under it.
                        cx.stop_propagation();
                        let offset = active.offset(event.position.y, state.grab, handle.offset());
                        shared.set(state);
                        if handle.offset() != offset {
                            handle.set_offset(offset);
                            notify = true;
                        }
                        if notify {
                            shared.notify_drag(now, view, window, cx);
                        }
                        return;
                    }
                    shared.set(state);
                    if notify {
                        cx.notify(view);
                    }
                }
            });

            window.on_mouse_event({
                let shared = prepainted.shared.clone();
                let handle = self.handle.clone();
                move |event: &MouseUpEvent, phase, _, cx| {
                    let now = cx.background_executor().now();
                    let state = shared.get(now);
                    if phase.bubble() && state.dragging {
                        handle.set_offset(active.offset(
                            event.position.y,
                            state.grab,
                            handle.offset(),
                        ));
                        handle.end_drag();
                        shared.update(now, |state| state.end_drag(now));
                        cx.notify(view);
                    }
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use gpui::{
        Context, InteractiveElement as _, IntoElement, ParentElement as _, Render, ScrollDelta,
        ScrollHandle, StatefulInteractiveElement as _, Styled as _, TestAppContext,
        VisualTestContext, div, size,
    };

    use super::*;
    use crate::test_frames::{draw, draw_settled, painted_quads};

    /// A list whose view is drawn under another parent in each size class, as
    /// a page is when the window turns compact or wide.
    struct Page {
        motion: Owned,
        scroll: ScrollHandle,
        compact: bool,
    }

    impl Render for Page {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let list = div()
                .id("list")
                .debug_selector(|| "list".into())
                .size_full()
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
                .child(div().h(px(4000.)));
            let page = div()
                .id("page")
                .relative()
                .size_full()
                .child(list)
                .child(Scrollbar::vertical(&self.scroll).motion(&self.motion));
            if self.compact {
                div()
                    .id("compact")
                    .size_full()
                    .child(page)
                    .into_any_element()
            } else {
                div()
                    .id("wide")
                    .size_full()
                    .child(div().id("column").size_full().child(page))
                    .into_any_element()
            }
        }
    }

    /// How far in the thumb was painted in the last frame, from 0 to 1.
    fn thumb(cx: &mut VisualTestContext) -> f32 {
        draw(cx);
        cx.update(|window, cx| {
            let thumb = cx.theme().scrollbar_thumb;
            painted_quads(window)
                .iter()
                .filter_map(|quad| quad.background.as_solid())
                .filter(|fill| (fill.h - thumb.h).abs() < 0.01 && (fill.l - thumb.l).abs() < 0.01)
                .map(|fill| fill.a / thumb.a)
                .fold(0., f32::max)
        })
    }

    fn scroll(by: f32, cx: &mut VisualTestContext) {
        let list = cx.debug_bounds("list").unwrap();
        cx.simulate_event(ScrollEvent {
            position: list.center(),
            delta: ScrollDelta::Pixels(point(px(0.), px(by))),
            touch_phase: TouchPhase::Moved,
            ..Default::default()
        });
    }

    /// A scrollbar fading out as the window changes size class fades on from
    /// where it was under its new parent, rather than starting over hidden,
    /// and with motion reduced it is gone at once. Every step moves the
    /// executor's clock, the one this scrollbar samples.
    #[gpui::test]
    fn a_fading_scrollbar_carries_on_when_the_window_turns_compact(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let (page, cx) = cx.add_window_view(|_, _| Page {
            motion: Default::default(),
            scroll: ScrollHandle::new(),
            compact: false,
        });
        cx.simulate_resize(size(px(600.), px(400.)));
        draw_settled(cx);
        assert_eq!(thumb(cx), 0., "hidden until the list scrolls");

        scroll(-120., cx);
        draw_settled(cx);
        assert_eq!(thumb(cx), 1., "shown as the list scrolls");

        // The hold runs from the scroll, and the frames since took part of it.
        let motion = cx.update(|_, cx| cx.global::<gpui_base::Theme>().scrollbar.motion());
        let step = motion.exit() / 16;
        let mut fading = 1.;
        for _ in 0..(motion.idle().as_millis() / step.as_millis()) {
            cx.executor().advance_clock(step);
            fading = thumb(cx);
            if fading < 0.9 {
                break;
            }
        }
        assert!(fading > 0.05 && fading < 0.9, "fading out: {fading}");

        page.update(cx, |page, cx| {
            page.compact = true;
            cx.notify();
        });
        cx.executor().advance_clock(step);
        let carried = thumb(cx);
        assert!(
            carried > 0.01 && carried < fading,
            "the scrollbar restarted under its new parent: {carried}, from {fading}"
        );
        draw_settled(cx);
        assert_eq!(thumb(cx), 0., "faded out");

        scroll(120., cx);
        draw_settled(cx);
        assert_eq!(thumb(cx), 1.);
        cx.update(|_, cx| cx.set_reduce_motion(true));
        cx.executor().advance_clock(motion.idle());
        cx.run_until_parked();
        assert_eq!(thumb(cx), 0., "reduced motion: gone at once");
    }
}
