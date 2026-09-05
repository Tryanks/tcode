//! UIKit/Metal platform backend for `gpui-pre`.
//!
//! The UIKit host owns the application run loop and a `CAMetalLayer`-backed
//! view. This crate adapts that view to GPUI and exposes the small C ABI used
//! by the Swift host.

use std::rc::Rc;

#[cfg(target_os = "ios")]
mod ios;

#[cfg(target_os = "ios")]
pub use ios::ffi::*;

/// Returns the lazily-created process platform.
///
/// On iOS this must be called from the application main thread. GPUI itself is
/// single-threaded on that thread; background tasks use GCD global queues.
#[cfg(target_os = "ios")]
pub fn platform() -> Rc<dyn gpui::Platform> {
    ios::platform()
}

/// iOS is the only supported runtime for this platform crate.
#[cfg(not(target_os = "ios"))]
pub fn platform() -> Rc<dyn gpui::Platform> {
    panic!("gpui-ios::platform() is only available when target_os = \"ios\"")
}

/// Current host-view safe-area insets in logical points.
#[cfg(not(target_os = "ios"))]
pub fn safe_area() -> gpui::Edges<gpui::Pixels> {
    gpui::Edges::default()
}

/// Current safe-area and software-keyboard insets.
#[cfg(not(target_os = "ios"))]
pub fn insets() -> gpui::WindowInsets {
    gpui::WindowInsets::default()
}

/// Requests the UIKit software keyboard on iOS.
#[cfg(not(target_os = "ios"))]
pub fn set_keyboard_visible(_visible: bool) {}
