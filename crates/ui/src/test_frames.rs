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

/// Every shadow the last frame drew, wherever [`painted_quads`] finds a quad.
pub(crate) fn painted_shadows(window: &gpui::Window) -> Vec<gpui::Shadow> {
    fn collect(scene: &gpui::Scene, shadows: &mut Vec<gpui::Shadow>) {
        shadows.extend_from_slice(&scene.shadows);
        for frame in scene
            .layers
            .frames
            .iter()
            .filter(|frame| frame.tile_size == 0)
        {
            if let Some(group) = frame.content.scene() {
                collect(group, shadows);
            }
        }
    }
    let mut shadows = Vec::new();
    collect(window.painted_scene(), &mut shadows);
    shadows
}

/// Every quad the last frame drew, as the renderer draws them. gpui-fast's
/// `Window::painted_quads` lists only the frame's top scene, but a rounded
/// container that clips its content (`overflow_hidden` with corner radii)
/// has that content moved into a scene of its own as the frame finishes
/// (`rounded_clip_scene::compile`), so none of it is listed there.
pub(crate) fn painted_quads(window: &gpui::Window) -> Vec<gpui::Quad> {
    fn collect(scene: &gpui::Scene, quads: &mut Vec<gpui::Quad>) {
        quads.extend_from_slice(&scene.quads);
        for frame in scene
            .layers
            .frames
            .iter()
            .filter(|frame| frame.tile_size == 0)
        {
            if let Some(group) = frame.content.scene() {
                collect(group, quads);
            }
        }
    }
    let mut quads = Vec::new();
    collect(window.painted_scene(), &mut quads);
    quads
}
