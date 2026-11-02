//! Keyboard focus rings that fade in and out.
//!
//! GPUI's `focus_visible` is a style that switches at once, and it is the only
//! way a control learns that the keyboard focused it unless it holds its own
//! focus handle: gpui-base's controls keep theirs, and a raw clickable's
//! `tab_index` makes one GPUI keeps. [`paint`] draws the ring of a control
//! whose handle Tcode holds, fading over `SETTLE` as the keyboard brings focus
//! to it and takes it away. The fades are kept by focus handle for the whole
//! app: at rest it holds the one focused control, whatever the windows draw.

use std::cell::RefCell;

#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
#[cfg(target_family = "wasm")]
use web_time::Instant;

use gpui::{
    AbsoluteLength, App, Bounds, BoxShadow, Corners, CornersRefinement, FocusHandle, Global,
    Pixels, WeakFocusHandle, Window,
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gpui::{
        Context, IntoElement, ParentElement as _, Render, Role, Styled as _, TestAppContext,
        VisualTestContext, Window, div, px,
    };

    use super::FocusRings;
    use crate::test_frames::{draw, draw_settled};

    /// Two raw clickables, as `accessible_clickable` makes them.
    struct Two;

    impl Render for Two {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let clickable = |id: &'static str| {
                crate::material::accessible_clickable(div().size(px(40.)), id, Role::Button, id, cx)
            };
            div()
                .size_full()
                .child(clickable("first"))
                .child(clickable("second"))
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

    /// The keyboard taking focus to a clickable fades its ring in over
    /// several frames, moving on fades it out as the next one's fades in,
    /// and letting focus go fades the last out; each time the window comes
    /// to rest holding the focused control's ring alone, or none. With
    /// motion reduced each change lands at once.
    #[gpui::test]
    fn a_focus_ring_fades_in_and_out_with_the_keyboard(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        for reduce in [false, true] {
            let (_, cx) = cx.add_window_view(|_, _| Two);
            cx.update(|_, cx| cx.set_reduce_motion(reduce));
            draw_settled(cx);
            // The keyboard is the last input, as when Tab moves focus.
            cx.simulate_keystrokes("a");
            for (step, rings) in [(0, 1), (1, 1), (2, 0)] {
                cx.update(|window, cx| {
                    if step < 2 {
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
