//! Keyboard focus rings that fade in and out.
//!
//! GPUI's `focus_visible` is a style that switches at once, and it is the only
//! way a control learns that the keyboard focused it unless it holds its own
//! focus handle: gpui-base's controls keep theirs, and a raw clickable's
//! `tab_index` makes one GPUI keeps. [`paint`] draws the ring of a control
//! whose handle Tcode holds, fading over `SETTLE` as the keyboard brings focus
//! to it and takes it away. The fades are kept by focus handle for the whole
//! app: at rest it holds the one focused control, whatever the windows draw.
//!
//! gpui-base's own controls (Button, Switch, Checkbox, Radio, Tab) draw
//! their keyboard focus only through `focus_visible`, so Tcode's facades
//! hand each its focus handle ([`handle`]) and draw the ring over it with
//! [`Ringed`]. Remove both once gpui-base's controls fade their own ring.

use std::cell::RefCell;

#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
#[cfg(target_family = "wasm")]
use web_time::Instant;

use gpui::{
    AbsoluteLength, AnyElement, App, Bounds, BoxShadow, Corners, CornersRefinement, ElementId,
    FocusHandle, Global, GlobalElementId, InspectorElementId, IntoElement, LayoutId, Pixels,
    WeakFocusHandle, Window,
};

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
        let span = crate::edge_back::SETTLE.mul_f32((to - from).abs().max(0.01));
        let elapsed = now.saturating_duration_since(at);
        if elapsed >= span {
            return to;
        }
        let progress = super::settle_progress(
            crate::edge_back::SETTLE.mul_f32(elapsed.as_secs_f32() / span.as_secs_f32()),
        );
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
pub(crate) fn handle(id: &ElementId, window: &mut Window, cx: &mut App) -> FocusHandle {
    window
        .use_keyed_state(super::child_id(id, "focus"), cx, |_, cx| cx.focus_handle())
        .read(cx)
        .clone()
        .tab_index(0)
        .tab_stop(true)
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

    /// How many frames the window asks for, one display frame apart, until
    /// it rests.
    fn frames(cx: &mut VisualTestContext) -> usize {
        draw(cx);
        let mut asked = 0;
        while cx.update(|window, cx| window.simulate_next_frame(cx)) > 0 {
            asked += 1;
            assert!(asked < 100, "a ring kept asking for frames");
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

    /// The keyboard taking focus to a control fades its ring in over
    /// several frames, moving on fades it out as the next one's fades in,
    /// and letting focus go fades the last out; each time the window comes
    /// to rest holding the focused control's ring alone, or none. So for a
    /// raw clickable and for every gpui-base control Tcode draws, whose own
    /// ring would switch at once. With motion reduced each change lands at
    /// once.
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
                } else {
                    assert!(asked >= 6, "step {step}: the ring snapped ({asked} frames)");
                }
                assert_eq!(held(cx), vec![1.; rings], "step {step}");
            }
        }
    }
}
