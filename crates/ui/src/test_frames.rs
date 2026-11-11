//! Frames for headless window tests, delivered as a display delivers them.

use std::time::Duration;

use gpui::VisualTestContext;

/// The spacing of a display's frames.
const FRAME: Duration = Duration::from_millis(16);

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
/// A frame advances the executor's clock, which Tcode's motion reads. Some
/// of gpui-fast's and gpui-base's time reads the wall clock instead
/// (`Instant::now()`), which these frames do not move: gpui-fast holds hover
/// still for 100 ms of wall time after a wheel (`hover_freeze.rs`), however
/// many frames that is; its touch recognizer sees a finger's reports at
/// almost one instant (`gestures.rs`); and gpui-base's `Scrollbar`, which
/// its inputs draw, fades for 200 ms of wall time (`scrollbar.rs`,
/// prepaint), however many frames that is.
///
/// A spinner or a running turn's working indicator repeats for as long as
/// what it shows runs, so a window showing one never comes to rest, and this
/// does not return; a test that holds a turn, a command or a page in flight
/// reduces motion, under which they rest.
pub(crate) fn draw_settled(cx: &mut VisualTestContext) {
    draw(cx);
    loop {
        cx.run_until_parked();
        if cx.update(|window, cx| window.simulate_next_frame(cx)) == 0 {
            return;
        }
        cx.executor().advance_clock(FRAME);
    }
}

/// Occlude a test window the way a phone would: the visual viewport shrinks
/// to the viewport minus `insets`. GPUI's test window exposes only the visual
/// viewport to tests, and [`gpui::Window::fully_visible_bounds`] intersects it with
/// the safe area exactly as it does for native insets, so this drives every
/// seam consumer through the same path. Pass zero edges to clear it.
pub(crate) fn occlude(cx: &mut VisualTestContext, insets: gpui::Edges<gpui::Pixels>) {
    let (handle, viewport) =
        cx.update(|window, _| (window.window_handle(), window.viewport_size()));
    cx.simulate_window_visual_viewport_change(
        handle,
        gpui::Bounds::from_corners(
            gpui::point(insets.left, insets.top),
            gpui::point(
                viewport.width - insets.right,
                viewport.height - insets.bottom,
            ),
        ),
    );
}
