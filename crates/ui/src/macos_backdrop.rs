//! The macOS window backdrop: one Liquid Glass `NSGlassEffectView` under the
//! GPUI view (two while the theme glides), left exactly as AppKit builds it.
//!
//! GPUI's `WindowBackgroundAppearance::Blurred` strips its material down to a
//! colorless blur and has no Liquid Glass option, so the window opens
//! `Transparent` and the system glass goes underneath instead. The glass
//! carries its own tint, honours Reduce Transparency and the Liquid Glass
//! slider, and follows the appearance it is given.

use gpui::{App, Global, Window};
use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly as _};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization as _, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
    NSAutoresizingMaskOptions, NSGlassEffectView, NSGlassEffectViewStyle, NSView,
    NSWindowOrderingMode,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::theme::ThemeMode;

/// The glass under the window, and the glasses of earlier appearances fading
/// out over it while the theme glides.
struct SystemBackdrop {
    content: Retained<NSView>,
    glass: Retained<NSGlassEffectView>,
    mode: Option<ThemeMode>,
    /// Each with the alpha it had when the glide under way began.
    leaving: Vec<(Retained<NSGlassEffectView>, f64)>,
}
impl Global for SystemBackdrop {}

/// Whether the window carries a system material that already tints the
/// backdrop, so the theme canvas must not tint it a second time.
pub(crate) fn installed(cx: &App) -> bool {
    cx.has_global::<SystemBackdrop>()
}

/// Slide the glass under GPUI's view. A no-op if the native window cannot be
/// reached, in which case the theme canvas keeps tinting the transparent window.
pub(crate) fn install(window: &Window, cx: &mut App) {
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return;
    };
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    // SAFETY: GPUI hands out its live NSView, which outlives this call.
    let gpui_view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
    let Some(content) = gpui_view.window().and_then(|window| window.contentView()) else {
        return;
    };
    let glass = new_glass(&content, mtm);
    content.addSubview_positioned_relativeTo(&glass, NSWindowOrderingMode::Below, None);
    cx.set_global(SystemBackdrop {
        content,
        glass,
        mode: None,
        leaving: Vec::new(),
    });
}

fn new_glass(content: &NSView, mtm: MainThreadMarker) -> Retained<NSGlassEffectView> {
    let glass = NSGlassEffectView::initWithFrame(NSGlassEffectView::alloc(mtm), content.bounds());
    glass.setStyle(NSGlassEffectViewStyle::Regular);
    glass.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    glass
}

fn appearance(mode: ThemeMode) -> Option<Retained<NSAppearance>> {
    let name = if mode.is_dark() {
        unsafe { NSAppearanceNameDarkAqua }
    } else {
        unsafe { NSAppearanceNameAqua }
    };
    NSAppearance::appearanceNamed(name)
}

/// The glass follows the window's appearance, but the theme mode can be forced
/// from Settings; pin the glass to the theme so a dark palette never sits on
/// light glass. At once: a glass still fading out goes with it.
pub(crate) fn sync_appearance(mode: ThemeMode, cx: &mut App) {
    let Some(backdrop) = cx.try_global::<SystemBackdrop>() else {
        return;
    };
    if backdrop.mode == Some(mode) && backdrop.leaving.is_empty() {
        return;
    }
    let backdrop = cx.global_mut::<SystemBackdrop>();
    for (glass, _) in backdrop.leaving.drain(..) {
        glass.removeFromSuperview();
    }
    backdrop.glass.setAppearance(appearance(mode).as_deref());
    backdrop.mode = Some(mode);
}

/// Start the glass toward `mode`'s appearance with the theme's glide.
///
/// Gap: AppKit gives the glass no transition between appearances, and it is
/// drawn outside GPUI's scene, so GPUI cannot fade anything over it either.
/// So a glass of the new appearance goes in under the one shown, which fades
/// out over it on the glide's own frames ([`glide_step`]) and then leaves, as
/// `motion::native_fade` fades the web views it has no other way to fade.
pub(crate) fn glide_appearance(mode: ThemeMode, cx: &mut App) {
    let Some(backdrop) = cx.try_global::<SystemBackdrop>() else {
        return;
    };
    if backdrop.mode == Some(mode) {
        return;
    }
    let Some(mtm) = MainThreadMarker::new() else {
        return sync_appearance(mode, cx);
    };
    let backdrop = cx.global_mut::<SystemBackdrop>();
    let glass = new_glass(&backdrop.content, mtm);
    glass.setAppearance(appearance(mode).as_deref());
    backdrop.content.addSubview_positioned_relativeTo(
        &glass,
        NSWindowOrderingMode::Below,
        Some(&backdrop.glass),
    );
    let shown = std::mem::replace(&mut backdrop.glass, glass);
    // A glide turned back part way fades out what is left of each glass
    // still leaving, from where it is.
    for (glass, from) in &mut backdrop.leaving {
        *from = glass.alphaValue();
    }
    backdrop.leaving.push((shown, 1.));
    backdrop.mode = Some(mode);
}

/// How far the theme's glide has come, from 0 to 1: each glass leaving has
/// faded out by as much, and goes once the glide is done.
pub(crate) fn glide_step(progress: f32, cx: &mut App) {
    let Some(backdrop) = cx.try_global::<SystemBackdrop>() else {
        return;
    };
    if backdrop.leaving.is_empty() {
        return;
    }
    let backdrop = cx.global_mut::<SystemBackdrop>();
    if progress >= 1. {
        for (glass, _) in backdrop.leaving.drain(..) {
            glass.removeFromSuperview();
        }
        return;
    }
    for (glass, from) in &backdrop.leaving {
        glass.setAlphaValue(*from * f64::from(1. - progress));
    }
}
