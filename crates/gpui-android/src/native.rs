use std::path::Path;

use anyhow::{Result, bail};
use gpui::{
    Bounds, Pixels, Size, TextInputStateChange, ThermalState, WindowAppearance, point, px, size,
};
use ndk::native_window::NativeWindow;
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, WindowHandle,
};

/// Physical display information supplied by the Android activity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AndroidDisplayMetrics {
    /// Width of the current surface in physical pixels.
    pub width_px: u32,
    /// Height of the current surface in physical pixels.
    pub height_px: u32,
    /// Number of physical pixels per GPUI logical pixel.
    pub scale_factor: f32,
}

impl AndroidDisplayMetrics {
    /// Creates validated display metrics.
    pub fn new(width_px: u32, height_px: u32, scale_factor: f32) -> Result<Self> {
        let metrics = Self {
            width_px,
            height_px,
            scale_factor,
        };
        metrics.validate()?;
        Ok(metrics)
    }

    pub(crate) fn validate(self) -> Result<()> {
        if self.width_px == 0 || self.height_px == 0 {
            bail!("Android surface dimensions must be non-zero");
        }
        if self.width_px > i32::MAX as u32 || self.height_px > i32::MAX as u32 {
            bail!("Android surface dimensions exceed GPUI's device-pixel range");
        }
        if !self.scale_factor.is_finite() || self.scale_factor <= 0.0 {
            bail!("Android display scale factor must be finite and positive");
        }
        Ok(())
    }

    pub(crate) fn logical_size(self) -> Size<Pixels> {
        size(
            px(self.width_px as f32 / self.scale_factor),
            px(self.height_px as f32 / self.scale_factor),
        )
    }

    pub(crate) fn logical_bounds(self) -> Bounds<Pixels> {
        Bounds::new(point(px(0.0), px(0.0)), self.logical_size())
    }
}

/// A retained `ANativeWindow` suitable for raw-window-handle and wgpu.
///
/// `ndk::NativeWindow` owns an `ANativeWindow_acquire` reference, so cloning
/// this value keeps the pointer alive until every renderer/surface lease drops.
#[derive(Clone, Debug)]
pub struct AndroidNativeWindow {
    inner: NativeWindow,
}

impl AndroidNativeWindow {
    /// Wraps a retained native window acquired by the JNI/NDK integration.
    pub fn new(inner: NativeWindow) -> Self {
        Self { inner }
    }

    /// Returns the retained NDK window for integrations that need NDK APIs.
    pub fn as_ndk(&self) -> &NativeWindow {
        &self.inner
    }
}

impl From<NativeWindow> for AndroidNativeWindow {
    fn from(inner: NativeWindow) -> Self {
        Self::new(inner)
    }
}

impl HasWindowHandle for AndroidNativeWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        self.inner.window_handle()
    }
}

impl HasDisplayHandle for AndroidNativeWindow {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Ok(DisplayHandle::android())
    }
}

/// One Android surface generation and its matching display metrics.
#[derive(Clone, Debug)]
pub struct AndroidSurface {
    pub(crate) window: AndroidNativeWindow,
    pub(crate) metrics: AndroidDisplayMetrics,
}

impl AndroidSurface {
    /// Creates a surface generation from a retained native window.
    pub fn new(window: AndroidNativeWindow, metrics: AndroidDisplayMetrics) -> Result<Self> {
        metrics.validate()?;
        Ok(Self { window, metrics })
    }

    /// Returns the physical display metrics associated with this surface.
    pub fn metrics(&self) -> AndroidDisplayMetrics {
        self.metrics
    }
}

/// Outbound half of the Android JNI/NDK boundary.
///
/// The Java/Kotlin activity (or `android-activity` glue) implements this trait.
/// Its matching inbound half is [`crate::AndroidEventSink`]. Keeping OS calls
/// here lets surface, lifecycle, Choreographer, clipboard, back, and IME glue be
/// added without changing the GPUI trait implementations.
pub trait AndroidHost: Send + Sync {
    /// Wake the activity's main Looper so it calls `drain_main_thread`.
    fn wake_main_thread(&self);

    /// Schedule one Choreographer callback that calls `frame`.
    ///
    /// The implementation must coalesce calls while a callback is pending.
    /// `AndroidEventSink::frame` rearms the next callback while the window is active.
    fn request_frame(&self);

    /// Finish the owning activity.
    fn finish_activity(&self);

    /// Launch an Android intent for the URI.
    fn open_uri(&self, uri: &str) -> Result<()>;

    /// Launch an Android intent for a filesystem path, if the app can expose it.
    fn open_path(&self, path: &Path) -> Result<()>;

    /// Read plain text through Android's ClipboardManager.
    fn read_clipboard_text(&self) -> Result<Option<String>>;

    /// Write plain text through Android's ClipboardManager.
    fn write_clipboard_text(&self, text: &str) -> Result<()>;

    /// Store a credential through the Android Keystore-backed host service.
    fn write_credentials(&self, url: &str, username: &str, password: &[u8]) -> Result<()>;

    /// Read a credential through the Android Keystore-backed host service.
    fn read_credentials(&self, url: &str) -> Result<Option<(String, Vec<u8>)>>;

    /// Delete a credential through the Android Keystore-backed host service.
    fn delete_credentials(&self, url: &str) -> Result<()>;

    /// Show the activity's input method.
    fn show_soft_keyboard(&self);

    /// Hide the activity's input method.
    fn hide_soft_keyboard(&self);

    /// Update the logical caret bounds used to position the IME.
    fn update_ime_position(&self, bounds: Bounds<Pixels>);

    /// Notify the host that GPUI's focused text-input state changed.
    fn text_input_state_changed(&self, change: TextInputStateChange);

    /// Enable or disable the activity's registered back callback.
    fn set_back_enabled(&self, enabled: bool);

    /// Query the current Android UI mode.
    fn window_appearance(&self) -> WindowAppearance;

    /// Query Android's current thermal status.
    fn thermal_state(&self) -> ThermalState;
}
