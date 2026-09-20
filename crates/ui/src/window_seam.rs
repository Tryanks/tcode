//! The window's outer seam and the one layout rule derived from it.
//!
//! A window is not always the rectangle it reports: a status bar, a notch, a
//! home indicator or a software keyboard can cover part of it. Those edges are
//! a property of the *window*, not of the host the workspace is attached to,
//! so they are deliberately not part of `ClientHost` — and they are owned by
//! GPUI, not by this crate. [`Window::fully_visible_bounds`] is the viewport
//! intersected with the platform's visual viewport and inset by
//! `WindowInsets::effective()`, and GPUI refreshes the window whenever either
//! changes. Everything here is a pure function over that window, read where
//! it is consumed: nothing is cached, polled or installed as a global.

use gpui::{App, Edges, Pixels, Window, px};

/// The single layout rule: below this much usable content width the shell uses
/// its compact layout, at or above it the wide split. Nothing else — not the
/// platform, not the input device, not a stored preference — decides it.
pub(crate) const COMPACT_BREAKPOINT: f32 = 900.;

/// The one safe content rectangle shared by pages, palette, dialogs and
/// sheets, as insets from the window's edges. Bottom avoidance is
/// `max(safe.bottom, ime.bottom)`, never their sum: a keyboard that already
/// covers the home indicator does not need it counted a second time — that is
/// what `WindowInsets::effective()` computes for the window. Backgrounds still
/// paint edge to edge; only interactive content is constrained, and only once.
pub(crate) fn content_insets(window: &Window) -> Edges<Pixels> {
    let viewport = window.viewport_size();
    let visible = window.fully_visible_bounds();
    Edges {
        top: visible.origin.y,
        right: viewport.width - visible.right(),
        bottom: viewport.height - visible.bottom(),
        left: visible.origin.x,
    }
}

/// Compact iff the width the window can actually lay content out in — the
/// viewport minus whatever the system occludes on its left and right — is under
/// [`COMPACT_BREAKPOINT`]. At exactly 900 the layout is wide.
pub(crate) fn compact_for(content_width: Pixels) -> bool {
    content_width < px(COMPACT_BREAKPOINT)
}

/// [`compact_for`] applied to the width of this window's fully visible bounds.
pub(crate) fn window_is_compact(window: &Window) -> bool {
    compact_for(window.fully_visible_bounds().size.width)
}

/// Whether a software keyboard covers the bottom of this window: the visual
/// viewport — the part of the layout viewport the user can see — ends above
/// the window's bottom edge. Safe areas do not move the visual viewport, so a
/// home indicator alone never counts as a keyboard.
pub(crate) fn keyboard_covers_window(window: &Window) -> bool {
    window.visual_viewport_bounds().bottom() < window.viewport_size().height
}

/// Whether text entry here is a software keyboard. This is an input-device
/// capability, not a width: a wide iPad still types on glass, and a desktop
/// window dragged narrow still has a hardware Enter key. Forwards to
/// [`gpui_base::is_mobile`] (compiled for iOS or Android); tests override it
/// to exercise both branches on a desktop.
pub(crate) fn is_mobile(_cx: &App) -> bool {
    #[cfg(test)]
    if let Some(override_) = _cx.try_global::<MobileOverride>() {
        return override_.0;
    }
    gpui_base::is_mobile()
}

#[cfg(test)]
struct MobileOverride(bool);

#[cfg(test)]
impl gpui::Global for MobileOverride {}

/// Override the platform capability inside one isolated GPUI test app.
#[cfg(test)]
pub(crate) fn override_mobile_for_test(cx: &mut App, value: bool) {
    cx.set_global(MobileOverride(value));
}

/// Occlude a test window the way a phone would: the visual viewport shrinks
/// to the viewport minus `insets`. GPUI's test window exposes only the visual
/// viewport to tests, and [`Window::fully_visible_bounds`] intersects it with
/// the safe area exactly as it does for native insets, so this drives every
/// seam consumer through the same path. Pass zero edges to clear it.
#[cfg(test)]
pub(crate) fn occlude_for_test(cx: &mut gpui::VisualTestContext, insets: Edges<Pixels>) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Bounds, Render, TestAppContext, WindowInsets, point, size};

    struct Probe;

    impl Render for Probe {
        fn render(
            &mut self,
            _: &mut Window,
            _: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            gpui::div()
        }
    }

    /// The breakpoint is on usable width, and 900 itself is wide.
    #[test]
    fn the_breakpoint_measures_content_width_and_is_wide_at_nine_hundred() {
        assert!(compact_for(px(899.)));
        assert!(!compact_for(px(900.)));
    }

    /// A keyboard over the home indicator is one occlusion, not two.
    #[test]
    fn keyboard_and_home_indicator_do_not_stack() {
        let insets = WindowInsets {
            safe_area: Edges {
                bottom: px(34.),
                ..Default::default()
            },
            ime: Edges {
                bottom: px(300.),
                ..Default::default()
            },
        };
        assert_eq!(insets.effective().bottom, px(300.));
    }

    /// The seam is read from the window's fully visible bounds: a landscape
    /// phone whose system occludes 59px on each side is 918px wide but has
    /// only 800px to lay out in, and the keyboard cover reaches the bottom
    /// inset, the compact rule and the Back handler through the same bounds.
    #[gpui::test]
    fn seam_follows_the_window_fully_visible_bounds(cx: &mut TestAppContext) {
        let (_, cx) = cx.add_window_view(|_, _| Probe);
        cx.simulate_resize(size(px(918.), px(420.)));
        cx.update(|window, _| {
            assert_eq!(content_insets(window), Edges::default());
            assert!(!window_is_compact(window));
            assert!(!keyboard_covers_window(window));
        });
        let handle = cx.update(|window, _| window.window_handle());
        cx.simulate_window_visual_viewport_change(
            handle,
            Bounds::new(point(px(59.), px(0.)), size(px(800.), px(120.))),
        );
        cx.update(|window, _| {
            assert_eq!(
                content_insets(window),
                Edges {
                    top: px(0.),
                    right: px(59.),
                    bottom: px(300.),
                    left: px(59.),
                }
            );
            assert!(window_is_compact(window));
            assert!(keyboard_covers_window(window));
        });
    }
}
