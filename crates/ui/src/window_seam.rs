//! The window's outer seam and the one layout rule derived from it.
//!
//! A window is not always the rectangle it reports: a status bar, a notch, a
//! home indicator or a software keyboard can cover part of it. Those edges are
//! a property of the *window*, not of the host the workspace is attached to, so
//! they are deliberately not part of `ClientHost`.
//!
//! Platform backends already schedule a frame when their insets change
//! (`gpui-ios` `update_insets`, `gpui-android` `update_insets`), so the shell
//! reads the seam per frame instead of polling it on a timer.

use std::rc::Rc;

use gpui::{App, Edges, Global, Pixels, Window, WindowInsets, px};

/// The single layout rule: below this much usable content width the shell uses
/// its compact layout, at or above it the wide split. Nothing else — not the
/// platform, not the input device, not a stored preference — decides it.
pub const COMPACT_BREAKPOINT: f32 = 900.;

/// Where the system occludes this window, and where the keyboard is.
#[derive(Clone)]
pub struct WindowSeam(Rc<dyn Fn() -> WindowInsets>);

impl Global for WindowSeam {}

impl WindowSeam {
    /// `insets` is read every frame; give it the platform's live accessor
    /// (`gpui_ios::insets`, `gpui_android::insets`) rather than a snapshot.
    pub fn new(insets: impl Fn() -> WindowInsets + 'static) -> Self {
        Self(Rc::new(insets))
    }

    /// A window the system does not occlude: a desktop window, and a browser
    /// canvas — whose own element is already resized around the keyboard, so
    /// subtracting one here would subtract it twice.
    pub fn flush() -> Self {
        Self::new(WindowInsets::default)
    }

    pub fn insets(&self) -> WindowInsets {
        (self.0)()
    }

    /// The one safe content rectangle shared by pages, palette, dialogs and
    /// sheets. Bottom avoidance is `max(safe.bottom, ime.bottom)`, never their
    /// sum: a keyboard that already covers the home indicator does not need it
    /// counted a second time. Backgrounds still paint edge to edge; only
    /// interactive content is constrained, and only once.
    pub fn content_insets(&self) -> Edges<Pixels> {
        self.insets().effective()
    }

    /// This window's seam, or a flush one where bootstrap installed none.
    pub fn current(cx: &App) -> Self {
        cx.try_global::<Self>().cloned().unwrap_or_else(Self::flush)
    }
}

/// Compact iff the width the window can actually lay content out in — the
/// viewport minus whatever the system occludes on its left and right — is under
/// [`COMPACT_BREAKPOINT`]. At exactly 900 the layout is wide.
pub fn compact_for(viewport_width: Pixels, insets: &WindowInsets) -> bool {
    viewport_width - insets.safe_area.left - insets.safe_area.right < px(COMPACT_BREAKPOINT)
}

/// [`compact_for`] applied to this window and its installed seam.
pub fn window_is_compact(window: &Window, cx: &App) -> bool {
    compact_for(
        window.viewport_size().width,
        &WindowSeam::current(cx).insets(),
    )
}

/// Whether text entry here is a software keyboard. This is an input-device
/// capability, not a width: a wide iPad still types on glass, and a desktop
/// window dragged narrow still has a hardware Enter key.
pub const fn soft_keyboard() -> bool {
    cfg!(any(target_os = "ios", target_os = "android"))
}

pub(crate) fn soft_keyboard_for_key_bar(_cx: &App) -> bool {
    #[cfg(test)]
    if let Some(override_) = _cx.try_global::<SoftKeyboardOverride>() {
        return override_.0;
    }
    soft_keyboard()
}

#[cfg(test)]
struct SoftKeyboardOverride(bool);

#[cfg(test)]
impl Global for SoftKeyboardOverride {}

/// Override the platform capability inside one isolated GPUI test app.
#[cfg(test)]
pub(crate) fn override_soft_keyboard_for_test(cx: &mut App, value: bool) {
    cx.set_global(SoftKeyboardOverride(value));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insets(left: f32, right: f32, bottom: f32, ime_bottom: f32) -> WindowInsets {
        WindowInsets {
            safe_area: Edges {
                top: px(0.),
                right: px(right),
                bottom: px(bottom),
                left: px(left),
            },
            ime: Edges {
                top: px(0.),
                right: px(0.),
                bottom: px(ime_bottom),
                left: px(0.),
            },
        }
    }

    /// The breakpoint is on usable width, and 900 itself is wide.
    #[test]
    fn the_breakpoint_measures_content_width_and_is_wide_at_nine_hundred() {
        let flush = WindowInsets::default();
        assert!(compact_for(px(899.), &flush));
        assert!(!compact_for(px(900.), &flush));
        // A landscape phone whose notch eats 59px on each side is 918px wide
        // but has only 800px to lay out in.
        assert!(compact_for(px(918.), &insets(59., 59., 21., 0.)));
    }

    /// A keyboard over the home indicator is one occlusion, not two.
    #[test]
    fn keyboard_and_home_indicator_do_not_stack() {
        let content = insets(0., 0., 34., 300.).effective();
        assert_eq!(content.bottom, px(300.));
    }
}
