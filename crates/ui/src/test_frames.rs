//! Frames for headless window tests, delivered as a display delivers them.

use std::time::{Duration, Instant};

use gpui::VisualTestContext;

/// The spacing of a display's frames.
const FRAME: Duration = Duration::from_millis(16);
/// How long a window may keep asking for frames before it counts as never
/// coming to rest.
const FRAME_BUDGET: usize = 320;

/// Draws one full frame. A frame that reuses any view keeps every debug
/// bound of the previous one, so an element no longer painted would still
/// be found.
pub(crate) fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| {
        window.refresh();
        let _ = window.draw(cx);
    });
}

/// Draws a full frame, then delivers every frame the window asks for, one
/// display frame apart, until it asks for none: the layout the app shows
/// once its motion has come to rest.
///
/// Frames advance the test clock, which drives Tcode's motion. gpui-base
/// 0.8.1 samples some of its own motion with `Instant::now()` instead:
/// `Scrollbar`'s visibility and width transitions (`scrollbar.rs`,
/// prepaint) and `ScrollBounce`'s spring (`scroll_bounce.rs`). So the wall
/// clock is kept from falling behind the test clock, as it never does in
/// the app: a frame waits only for the wall time the frames before it did
/// not already take.
///
/// A spinner or a running turn's working indicator repeats for as long as
/// what it shows runs, so a window showing one never comes to rest; a test
/// that holds a turn, a command or a page in flight reduces motion, under
/// which they rest.
pub(crate) fn draw_settled(cx: &mut VisualTestContext) {
    draw(cx);
    let started = Instant::now();
    let mut elapsed = Duration::ZERO;
    for _ in 0..FRAME_BUDGET {
        cx.run_until_parked();
        if cx.update(|window, cx| window.simulate_next_frame(cx)) == 0 {
            return;
        }
        elapsed += FRAME;
        std::thread::sleep(elapsed.saturating_sub(started.elapsed()));
        cx.executor().advance_clock(FRAME);
    }
    panic!(
        "the window kept asking for frames after {FRAME_BUDGET} frames; \
         a repeating animation rests only with motion reduced"
    );
}
