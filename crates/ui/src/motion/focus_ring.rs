//! Keyboard focus rings that fade in and out.
//!
//! GPUI's `focus_visible` is a style that switches at once, and it is the only
//! way a control learns that the keyboard focused it unless it holds its own
//! focus handle: gpui-base's controls keep theirs, and a raw clickable's
//! `tab_index` makes one GPUI keeps. [`paint`] draws the ring of a control
//! whose handle Tcode holds, fading over [`GLIDE`] as the keyboard brings focus
//! to it and takes it away. The fades are kept by focus handle for the whole
//! app: at rest it holds the one focused control, whatever the windows draw.
//!
//! gpui-base's own controls (Button, Switch, Checkbox, Radio, Tab) draw
//! their keyboard focus only through `focus_visible`, so Tcode's facades
//! hand each its focus handle ([`handle`]) and draw the ring over it with
//! [`Ringed`]. Remove both once gpui-base's controls fade their own ring.

use std::cell::RefCell;
use std::collections::HashMap;

#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
#[cfg(target_family = "wasm")]
use web_time::Instant;

use gpui::{
    AbsoluteLength, AnyElement, App, Bounds, BoxShadow, Corners, CornersRefinement, ElementId,
    Entity, EntityId, FocusHandle, Global, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, WeakEntity, WeakFocusHandle, Window,
};

/// How long a focus ring takes to fade in or out.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// A ring's fade: from where, to where, since when.
type Fade = (f32, f32, Instant);

/// The rings that are shown or on their way in or out, by whose they are. A
/// few at most, so a list.
#[derive(Default)]
struct FocusRings(RefCell<Vec<(WeakFocusHandle, Fade)>>);

impl Global for FocusRings {}

/// How far the ring of the control `handle` focuses is in, from 0 to 1: in
/// while the keyboard has put focus on it, out otherwise, keeping frames
/// coming while it moves.
fn reveal(handle: &FocusHandle, window: &mut Window, cx: &mut App) -> f32 {
    let target = if handle.is_focused(window) && window.last_input_was_keyboard() {
        1.
    } else {
        0.
    };
    let now = cx.background_executor().now();
    let reduce = cx.reduce_motion();
    let rings = cx.default_global::<FocusRings>();
    let mut rings = rings.0.borrow_mut();
    // A ring whose control has gone goes with it.
    rings.retain(|(weak, _)| weak.upgrade().is_some());
    let index = rings.iter().position(|(weak, _)| handle == weak);
    let value = |(from, to, at): Fade| {
        // A ring that turns back part way runs the part it has left.
        let span = GLIDE.mul_f32((to - from).abs().max(0.01));
        let elapsed = now.saturating_duration_since(at);
        if elapsed >= span {
            return to;
        }
        let progress = crate::motion::Curve::new(GLIDE)
            .progress(GLIDE.mul_f32(elapsed.as_secs_f32() / span.as_secs_f32()));
        from + (to - from) * progress
    };
    let fade = match index.map(|index| rings[index].1) {
        None if target == 0. => return 0.,
        None => (if reduce { 1. } else { 0. }, 1., now),
        Some(fade) if fade.1 != target => (if reduce { target } else { value(fade) }, target, now),
        Some(fade) => fade,
    };
    let shown = value(fade);
    let kept = (shown != target || target > 0.).then_some(if shown == target {
        (1., 1., now)
    } else {
        fade
    });
    match (index, kept) {
        (Some(index), Some(kept)) => rings[index].1 = kept,
        (Some(index), None) => {
            rings.swap_remove(index);
        }
        (None, Some(kept)) => rings.push((handle.downgrade(), kept)),
        (None, None) => {}
    }
    if shown != target {
        window.request_animation_frame();
    }
    shown
}

/// Paints the keyboard focus ring of a control that tracks `handle` over its
/// `bounds`, rounded by `corners`: `ring` faded in while the keyboard has
/// focused the control. Called from the control's paint, after its content.
pub(crate) fn paint(
    handle: &FocusHandle,
    ring: BoxShadow,
    bounds: Bounds<Pixels>,
    corners: &CornersRefinement<AbsoluteLength>,
    window: &mut Window,
    cx: &mut App,
) {
    let shown = reveal(handle, window, cx);
    if shown <= 0. {
        return;
    }
    let rem = window.rem_size();
    let radius = |corner: Option<AbsoluteLength>| {
        corner
            .map(|corner| corner.to_pixels(rem))
            .unwrap_or_default()
            .min(bounds.size.width.min(bounds.size.height) / 2.)
    };
    let corners = Corners {
        top_left: radius(corners.top_left),
        top_right: radius(corners.top_right),
        bottom_right: radius(corners.bottom_right),
        bottom_left: radius(corners.bottom_left),
    };
    window.paint_inset_shadows(
        bounds,
        corners,
        &[BoxShadow {
            color: ring.color.opacity(shown),
            ..ring
        }],
    );
}

/// The focus handle of the control `id`, kept under it, for a control that
/// would otherwise keep its handle to itself; a tab stop.
///
/// Kept in element state, which GPUI keys by the whole id path, so a control
/// its view draws under a new parent (a page as the window turns compact or
/// wide, a pane as a zoom lifts it over the column) gets a new place: there
/// it takes over the focused handle the same view's control `id` held where
/// it was drawn before, so the keyboard stays on it and its ring carries on.
/// A control still drawn in its old place this frame keeps its handle, and
/// one that took it over is given a new one.
pub(crate) fn handle(id: &ElementId, window: &mut Window, cx: &mut App) -> FocusHandle {
    let key = (window.current_view(), id.clone());
    let frame = Handles::frame(cx);
    let slot = window.use_keyed_state(super::child_id(id, "focus"), cx, |window, cx| {
        moved_here(&key, frame, window).unwrap_or_else(|| cx.focus_handle())
    });
    claim(key, &slot, frame, cx);
    slot.read(cx).clone().tab_index(0).tab_stop(true)
}

/// [`handle`] for a control shown only on hover, with how far focus on it
/// reveals it, from 0 to 1: it fades in as focus comes onto it, as it does
/// under the pointer, rather than showing at once. The control focuses
/// through the handle (`Button::track_focus`). `keyboard` counts only focus
/// the keyboard gave, as GPUI's `focus_visible` does, and moves with the
/// control's ring; otherwise a click's focus counts too.
pub(crate) fn focus_reveal(
    id: &ElementId,
    keyboard: bool,
    window: &mut Window,
    cx: &mut App,
) -> (FocusHandle, f32) {
    let handle = handle(id, window, cx);
    let revealed = if keyboard {
        reveal(&handle, window, cx)
    } else {
        gpui_base::motion::transition(
            super::child_id(id, "focus-reveal"),
            if handle.is_focused(window) { 1f32 } else { 0. },
            gpui_base::motion::Transition::new(GLIDE),
            window,
            cx,
        )
    };
    (handle, revealed)
}

/// Where each view's controls keep their focus handles, by the view and the
/// control's id. Not a GPUI global: a view that reads a global while it is
/// built is built again whenever the global changes, and this one changes
/// after every draw.
struct Handles;

thread_local! {
    static CLAIMS: RefCell<Claims> = RefCell::default();
}

#[derive(Default)]
struct Claims {
    places: HashMap<(EntityId, ElementId), Vec<Place>>,
    /// Counts the draws that asked for handles: it moves on once the draw
    /// asking for one now is done.
    frame: u64,
    counting: bool,
}

/// One place a control was drawn in: the element state holding its handle,
/// the handle, the draw it last asked in, and the draw it took over another
/// place's handle in.
struct Place {
    slot: WeakEntity<FocusHandle>,
    handle: WeakFocusHandle,
    at: u64,
    took: Option<u64>,
}

impl Handles {
    /// The draw under way.
    fn frame(cx: &mut App) -> u64 {
        let (frame, started) = CLAIMS.with_borrow_mut(|claims| {
            let started = !claims.counting;
            claims.counting = true;
            (claims.frame, started)
        });
        if started {
            cx.defer(|_| {
                CLAIMS.with_borrow_mut(|claims| {
                    claims.frame += 1;
                    claims.counting = false;
                    // A place no longer drawn goes, and a control with it.
                    claims.places.retain(|_, places| {
                        places.retain(|place| place.slot.upgrade().is_some());
                        !places.is_empty()
                    });
                })
            });
        }
        frame
    }
}

/// The focused handle the control `key` names held in a place not drawn in
/// this draw yet: where it was before it moved.
fn moved_here(key: &(EntityId, ElementId), frame: u64, window: &Window) -> Option<FocusHandle> {
    CLAIMS.with_borrow(|claims| {
        claims
            .places
            .get(key)?
            .iter()
            .filter(|place| place.at < frame)
            .find_map(|place| {
                place
                    .handle
                    .upgrade()
                    .filter(|handle| handle.is_focused(window))
            })
    })
}

/// Records that the place `slot` asked for control `key`'s handle in draw
/// `frame`. A place that took over another's handle shares it for the draw
/// the control moves in, which may still draw both; if both are drawn again
/// after that, they are two controls with one id, and the place that took
/// the handle is given a new one.
fn claim(key: (EntityId, ElementId), slot: &Entity<FocusHandle>, frame: u64, cx: &mut App) {
    let handle = slot.read(cx).clone();
    let id = slot.entity_id();
    let taker = CLAIMS.with_borrow_mut(|claims| {
        let places = claims.places.entry(key).or_default();
        let took = places
            .iter()
            .any(|place| place.slot.entity_id() != id && handle == place.handle)
            .then_some(frame);
        match places.iter_mut().find(|place| place.slot.entity_id() == id) {
            Some(place) => {
                place.at = frame;
                if !(handle == place.handle) {
                    place.handle = handle.downgrade();
                    place.took = None;
                }
            }
            None => places.push(Place {
                slot: slot.downgrade(),
                handle: handle.downgrade(),
                at: frame,
                took,
            }),
        }
        let mine = places
            .iter()
            .find(|place| place.slot.entity_id() == id)
            .and_then(|place| place.took);
        places
            .iter()
            .filter(|place| place.slot.entity_id() != id && handle == place.handle)
            .filter(|place| place.at == frame)
            .find_map(|place| match (mine, place.took) {
                (Some(took), _) if took < frame => Some(slot.downgrade()),
                (_, Some(took)) if took < frame => Some(place.slot.clone()),
                _ => None,
            })
    });
    if let Some(taker) = taker.and_then(|taker| taker.upgrade()) {
        taker.update(cx, |taken, cx| *taken = cx.focus_handle());
    }
}

/// `child`, a control that tracks `handle`, with its keyboard focus ring
/// painted over it ([`paint`]), rounded by `corners`. It lays out as `child`
/// does and adds nothing to the element path.
pub(crate) struct Ringed {
    child: Option<AnyElement>,
    handle: FocusHandle,
    ring: BoxShadow,
    corners: CornersRefinement<AbsoluteLength>,
}

impl Ringed {
    pub(crate) fn new(
        child: impl IntoElement,
        handle: &FocusHandle,
        corners: CornersRefinement<AbsoluteLength>,
        cx: &App,
    ) -> Self {
        Self {
            child: Some(child.into_any_element()),
            handle: handle.clone(),
            ring: crate::material::focus_ring(cx),
            corners,
        }
    }

    /// The ring at `opacity` of its colour: a control dimmed as a whole
    /// dims its ring with it.
    pub(crate) fn opacity(mut self, opacity: f32) -> Self {
        self.ring.color = self.ring.color.opacity(opacity);
        self
    }
}

/// The same radius at every corner.
pub(crate) fn corners(radius: Pixels) -> CornersRefinement<AbsoluteLength> {
    corners_of(Corners::all(radius))
}

/// A radius per corner.
pub(crate) fn corners_of(corners: Corners<Pixels>) -> CornersRefinement<AbsoluteLength> {
    let radius = |radius: Pixels| Some(AbsoluteLength::Pixels(radius));
    CornersRefinement {
        top_left: radius(corners.top_left),
        top_right: radius(corners.top_right),
        bottom_right: radius(corners.bottom_right),
        bottom_left: radius(corners.bottom_left),
    }
}

impl IntoElement for Ringed {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl gpui::Element for Ringed {
    type RequestLayoutState = AnyElement;
    type PrepaintState = ();

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
    ) -> (LayoutId, AnyElement) {
        let mut child = self
            .child
            .take()
            .expect("a ringed control is laid out once");
        (child.request_layout(window, cx), child)
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        child: &mut AnyElement,
        window: &mut Window,
        cx: &mut App,
    ) {
        child.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        child: &mut AnyElement,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        child.paint(window, cx);
        paint(
            &self.handle,
            self.ring.clone(),
            bounds,
            &self.corners,
            window,
            cx,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gpui::{
        Context, IntoElement, ParentElement as _, Render, Role, Styled as _, TestAppContext,
        VisualTestContext, Window, div, px,
    };

    use super::FocusRings;
    use crate::test_frames::{draw, draw_settled};

    /// A raw clickable, as `accessible_clickable` makes it, and each of
    /// gpui-base's controls as Tcode's facades draw them.
    struct Controls;

    /// How many tab stops [`Controls`] draws.
    const STOPS: usize = 7;

    impl Render for Controls {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            use crate::widgets::{
                Button, Checkbox, Switch,
                choice::{Choice, ChoiceGroup},
                tabs::{TabBar, TabItem},
            };
            div()
                .size_full()
                .child(crate::material::accessible_clickable(
                    div().size(px(40.)),
                    "clickable",
                    Role::Button,
                    "clickable",
                    cx,
                ))
                .child(Button::new("button").label("Button"))
                .child(Switch::new("switch").label("Switch"))
                .child(Checkbox::new("checkbox").label("Checkbox"))
                .child(
                    TabBar::new("tabs")
                        .item(TabItem::new("tab-a", "A").selected(true))
                        .item(TabItem::new("tab-b", "B")),
                )
                .child(ChoiceGroup::new("choices").choice(Choice::new("choice", "Choice")))
        }
    }

    /// Two controls shown only on hover, as the sidebar's "+" (revealed by
    /// the keyboard's focus) and a pull request row's menu (by any focus)
    /// draw theirs: a 20px red square at the opacity focus reveals it to.
    struct Reveals;

    /// The ids of [`Reveals`]'s controls, and whether each counts only the
    /// keyboard's focus.
    const REVEALS: [(&str, bool); 2] = [("keyboard-reveal", true), ("any-reveal", false)];

    impl Render for Reveals {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            use gpui::InteractiveElement as _;
            let mut root = div().size_full().flex().gap_2();
            for (id, keyboard) in REVEALS {
                let (handle, revealed) =
                    super::focus_reveal(&gpui::ElementId::from(id), keyboard, window, cx);
                root = root.child(
                    div()
                        .id(id)
                        .debug_selector(move || id.into())
                        .track_focus(&handle)
                        .size(px(20.))
                        .bg(gpui::red())
                        .opacity(revealed),
                );
            }
            root
        }
    }

    /// How far each of [`Reveals`]'s controls is painted in, from 0 to 1.
    fn revealed(cx: &mut VisualTestContext) -> [f32; 2] {
        REVEALS.map(|(id, _)| {
            let bounds = cx.debug_bounds(id).expect("the control is laid out");
            cx.update(|window, _| {
                let bounds = bounds.scale(window.scale_factor());
                window
                    .painted_quads()
                    .iter()
                    .filter(|quad| quad.bounds == bounds)
                    .filter_map(|quad| quad.background.as_solid())
                    .filter(|fill| fill.s > 0.5)
                    .map(|fill| fill.a)
                    .fold(0., f32::max)
            })
        })
    }

    /// One display frame later.
    fn next_frame(cx: &mut VisualTestContext) {
        cx.executor().advance_clock(Duration::from_millis(16));
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
        });
        draw(cx);
    }

    /// A control shown only on hover fades in as focus comes onto it and
    /// out as focus leaves, rather than appearing at once: one revealed by
    /// the keyboard's focus stays hidden when a click focuses it, one
    /// revealed by any focus shows for the click too. With motion reduced
    /// each lands at once.
    #[gpui::test]
    fn focus_reveals_a_hidden_control_with_a_fade(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        for reduce in [false, true] {
            let (_, cx) = cx.add_window_view(|_, _| Reveals);
            cx.update(|_, cx| cx.set_reduce_motion(reduce));
            draw_settled(cx);
            assert_eq!(revealed(cx), [0., 0.], "hidden at rest");
            for (index, (id, _)) in REVEALS.into_iter().enumerate() {
                // The keyboard is the last input, as when Tab moves focus.
                cx.simulate_keystrokes("a");
                cx.update(|window, cx| window.focus_next(cx));
                draw(cx);
                next_frame(cx);
                let partway = revealed(cx)[index];
                if reduce {
                    assert_eq!(partway, 1., "{id}: reduced motion shows it at once");
                } else {
                    assert!(
                        partway > 0. && partway < 1.,
                        "{id}: the keyboard's focus showed it at once: {partway}"
                    );
                }
                draw_settled(cx);
                assert_eq!(revealed(cx)[index], 1., "{id}: shown under focus");
            }
            cx.update(|window, cx| window.blur(cx));
            draw(cx);
            next_frame(cx);
            if !reduce {
                let [keyboard, any] = revealed(cx);
                assert!(any > 0. && any < 1., "blurred at once: {any}");
                assert_eq!(keyboard, 0., "not focused, so already gone");
            }
            draw_settled(cx);
            assert_eq!(revealed(cx), [0., 0.], "hidden again");

            // A click focuses each: only the one any focus reveals shows.
            for (index, (id, keyboard)) in REVEALS.into_iter().enumerate() {
                let bounds = cx.debug_bounds(id).expect("the control is laid out");
                cx.simulate_click(bounds.center(), gpui::Modifiers::default());
                draw_settled(cx);
                let handle_focused = cx.update(|window, cx| window.focused(cx).is_some());
                assert!(handle_focused, "{id}: the click focused it");
                let shown = revealed(cx)[index];
                assert_eq!(shown, if keyboard { 0. } else { 1. }, "{id} after a click");
            }
        }
    }

    /// How many frames the window asks for, one display frame apart, until
    /// it rests.
    fn frames(cx: &mut VisualTestContext) -> usize {
        draw(cx);
        let mut asked = 0;
        while cx.update(|window, cx| window.simulate_next_frame(cx)) > 0 {
            asked += 1;
            cx.executor().advance_clock(Duration::from_millis(16));
            draw(cx);
        }
        asked
    }

    /// The rings held at rest, and whether each is shown.
    fn held(cx: &mut VisualTestContext) -> Vec<f32> {
        cx.update(|_, cx| {
            cx.default_global::<FocusRings>()
                .0
                .borrow()
                .iter()
                .map(|(_, (_, to, _))| *to)
                .collect()
        })
    }

    /// The keyboard taking focus to a control fades its ring in, moving on
    /// fades it out as the next one's fades in, and letting focus go fades
    /// the last out; each time the window comes to rest holding the focused
    /// control's ring alone, or none. So for a raw clickable and for every
    /// gpui-base control Tcode draws, whose own ring would switch at once.
    /// With motion reduced each change lands at once.
    #[gpui::test]
    fn a_focus_ring_fades_in_and_out_with_the_keyboard(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        for reduce in [false, true] {
            let (_, cx) = cx.add_window_view(|_, _| Controls);
            cx.update(|_, cx| cx.set_reduce_motion(reduce));
            draw_settled(cx);
            // The keyboard is the last input, as when Tab moves focus.
            cx.simulate_keystrokes("a");
            for step in 0..=STOPS {
                let rings = usize::from(step < STOPS);
                cx.update(|window, cx| {
                    if step < STOPS {
                        window.focus_next(cx);
                    } else {
                        window.blur(cx);
                    }
                });
                let asked = frames(cx);
                if reduce {
                    assert_eq!(asked, 0, "step {step}: a ring faded with motion reduced");
                }
                assert_eq!(held(cx), vec![1.; rings], "step {step}");
            }
        }
    }
}
