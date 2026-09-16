//! The macOS window backdrop: one semantic `NSVisualEffectView` under the GPUI
//! view, left exactly as AppKit builds it.
//!
//! GPUI's own `WindowBackgroundAppearance::Blurred` strips the material's tint
//! layers and relies on the `.selection` material for the blur; on macOS 27
//! `.selection` no longer carries a backdrop layer at all, so that path renders
//! no blur (#445). A stock semantic material blurs on every release, follows the
//! window's active state, and honours Reduce Transparency and the macOS 27
//! Liquid Glass slider without any tint of our own on top.

use gpui::{App, Global, Window};
use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly as _};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization as _, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
    NSAutoresizingMaskOptions, NSView, NSVisualEffectBlendingMode, NSVisualEffectMaterial,
    NSVisualEffectState, NSVisualEffectView, NSWindowOrderingMode,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::theme::ThemeMode;

struct SystemBackdrop(Retained<NSVisualEffectView>);
impl Global for SystemBackdrop {}

/// Whether the window carries a system material that already tints the
/// backdrop, so the theme canvas must not tint it a second time.
pub(crate) fn installed(cx: &App) -> bool {
    cx.has_global::<SystemBackdrop>()
}

/// Slide the material under GPUI's view. A no-op if the native window cannot be
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
    let effect =
        NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), content.bounds());
    effect.setMaterial(NSVisualEffectMaterial::Sidebar);
    effect.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
    effect.setState(NSVisualEffectState::FollowsWindowActiveState);
    effect.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    content.addSubview_positioned_relativeTo(&effect, NSWindowOrderingMode::Below, None);
    cx.set_global(SystemBackdrop(effect));
}

/// The material follows the window's appearance, but the theme mode can be
/// forced from Settings; pin the material to the theme so a dark palette never
/// sits on a light material.
pub(crate) fn sync_appearance(mode: ThemeMode, cx: &App) {
    let Some(backdrop) = cx.try_global::<SystemBackdrop>() else {
        return;
    };
    let name = if mode.is_dark() {
        unsafe { NSAppearanceNameDarkAqua }
    } else {
        unsafe { NSAppearanceNameAqua }
    };
    backdrop
        .0
        .setAppearance(NSAppearance::appearanceNamed(name).as_deref());
}
