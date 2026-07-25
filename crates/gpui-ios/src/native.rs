//! The UIKit seam, and the Metal context that makes iOS work without an
//! upstream change.
//!
//! Everything UIKit-shaped is behind [`IosHost`]. This crate never links
//! Objective-C itself: the shell that owns a `UIViewController` implements the
//! trait, so the backend stays a plain Rust crate that could be offered
//! upstream.

use std::cell::RefCell;
use std::rc::Rc;

use anyhow::{Context as _, Result, anyhow, bail};
use gpui::{Bounds, Pixels, Size, WindowAppearance, point, px, size};
use gpui_wgpu::wgpu;
use gpui_wgpu::{GpuContext, WgpuContext};
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawWindowHandle,
    UiKitWindowHandle, WindowHandle,
};

/// Physical size and scale of the `CAMetalLayer` GPUI draws into.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IosDisplayMetrics {
    /// Width of the current surface in physical pixels.
    pub width_px: u32,
    /// Height of the current surface in physical pixels.
    pub height_px: u32,
    /// Number of physical pixels per GPUI logical pixel.
    pub scale_factor: f32,
}

impl IosDisplayMetrics {
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
            bail!("iOS drawable dimensions must be non-zero");
        }
        if self.width_px > i32::MAX as u32 || self.height_px > i32::MAX as u32 {
            bail!("iOS drawable dimensions exceed GPUI's device-pixel range");
        }
        if !self.scale_factor.is_finite() || self.scale_factor <= 0.0 {
            bail!("iOS contentScaleFactor must be finite and positive");
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

/// A retained `CAMetalLayer` pointer, owned by the UIKit shell.
///
/// wgpu needs a `RawWindowHandle::UiKit` carrying the *view*, and iOS keeps the
/// layer alive as long as its view lives. This type therefore borrows rather
/// than owns: the shell must guarantee the pointer outlives the surface, which
/// is why [`IosSurface`] is created and destroyed around UIKit's own
/// view-lifecycle callbacks.
#[derive(Debug, Clone, Copy)]
pub struct IosLayer {
    /// `UIView*` whose layer is a `CAMetalLayer`.
    view: *mut std::ffi::c_void,
}

// SAFETY: the pointer is only dereferenced by wgpu on the thread that created
// the surface, and the shell keeps the view alive across that window. Marking it
// Send/Sync is what lets the surface be held in GPUI's window state, which is
// not itself thread-local.
unsafe impl Send for IosLayer {}
unsafe impl Sync for IosLayer {}

impl IosLayer {
    /// # Safety
    /// `view` must be a live `UIView*` whose `layer` is a `CAMetalLayer`, and it
    /// must outlive every surface created from it.
    pub unsafe fn from_raw(view: *mut std::ffi::c_void) -> Result<Self> {
        if view.is_null() {
            return Err(anyhow!("a null UIView cannot back a Metal surface"));
        }
        Ok(Self { view })
    }
}

impl HasWindowHandle for IosLayer {
    fn window_handle(&self) -> std::result::Result<WindowHandle<'_>, HandleError> {
        let pointer = std::ptr::NonNull::new(self.view).ok_or(HandleError::Unavailable)?;
        let handle = UiKitWindowHandle::new(pointer);
        // SAFETY: the handle borrows `self`, and `from_raw` established that the
        // pointer is a live view the shell keeps alive.
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::UiKit(handle)) })
    }
}

impl HasDisplayHandle for IosLayer {
    fn display_handle(&self) -> std::result::Result<DisplayHandle<'_>, HandleError> {
        // UIKit has no display object to hand out; the window handle carries
        // everything Metal needs.
        Ok(DisplayHandle::uikit())
    }
}

/// One drawable generation: a layer plus the metrics it was measured at.
#[derive(Debug, Clone, Copy)]
pub struct IosSurface {
    /// The view whose layer is a `CAMetalLayer`.
    pub window: IosLayer,
    /// Size and scale this generation was measured at.
    pub metrics: IosDisplayMetrics,
}

impl IosSurface {
    pub fn new(layer: IosLayer, metrics: IosDisplayMetrics) -> Result<Self> {
        metrics.validate()?;
        Ok(Self {
            window: layer,
            metrics,
        })
    }

    pub fn layer(&self) -> IosLayer {
        self.window
    }
}

/// A `GpuContext` pre-filled with a Metal-backed `WgpuContext`.
///
/// This is the whole reason iOS needs no upstream patch. `WgpuContext::instance`
/// hardcodes `VULKAN | GL`, but it is only a convenience: `WgpuContext::new`
/// accepts any instance, and `WgpuRenderer::new` uses the context already in the
/// slot rather than building its own. So the backend selection happens here,
/// once, in code we own.
pub fn metal_context(surface_target: &IosSurface) -> Result<GpuContext> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        // Metal only. Listing anything else would let wgpu silently pick a
        // software or GL path on a platform where Metal is the only real answer,
        // and a slow success is harder to diagnose than a clean failure.
        backends: wgpu::Backends::METAL,
        flags: wgpu::InstanceFlags::default(),
        backend_options: wgpu::BackendOptions::default(),
        memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
        display: None,
    });

    let target = wgpu::SurfaceTargetUnsafe::RawHandle {
        raw_display_handle: None,
        raw_window_handle: surface_target
            .layer()
            .window_handle()
            .map_err(|error| anyhow!("the UIKit view has no usable window handle: {error}"))?
            .as_raw(),
    };
    // SAFETY: the handle came from a live view the shell keeps alive for at
    // least as long as this surface.
    let probe = unsafe { instance.create_surface_unsafe(target) }
        .context("creating a Metal surface for adapter selection")?;

    let context =
        WgpuContext::new(instance, &probe, None).context("selecting a Metal adapter and device")?;
    Ok(Rc::new(RefCell::new(Some(context))))
}

/// Everything the UIKit shell must provide.
///
/// Mirrors `gpui-android`'s `AndroidHost` on purpose — the two platforms differ
/// in their callback names, not in what a GPUI backend needs from them, and a
/// shared shape means a reader who knows one knows the other.
pub trait IosHost: Send + Sync {
    /// Wake the main run loop so it calls `drain_main_thread`.
    ///
    /// Called from GPUI's worker threads, so an implementation must post to the
    /// main queue rather than assume it is already there.
    fn wake_main_thread(&self);

    /// Schedule one `CADisplayLink` tick that calls `frame`.
    ///
    /// Must coalesce while a tick is pending: GPUI may ask several times within
    /// one vsync, and each extra tick would queue a redundant frame.
    fn request_frame(&self);

    /// Ask the shell to suspend the app.
    ///
    /// iOS apps do not exit on demand — `exit(0)` is grounds for App Store
    /// rejection and looks like a crash to the user — so this is advisory and a
    /// shell may reasonably do nothing.
    fn finish_activity(&self);

    /// Open a URL through `UIApplication`.
    fn open_url(&self, url: &str) -> Result<()>;

    /// Read plain text from `UIPasteboard`.
    fn read_clipboard_text(&self) -> Result<Option<String>>;

    /// Write plain text to `UIPasteboard`.
    fn write_clipboard_text(&self, text: &str) -> Result<()>;

    /// Store a secret in the iOS keychain.
    fn write_credentials(&self, url: &str, username: &str, password: &[u8]) -> Result<()>;

    /// Read a secret from the iOS keychain.
    fn read_credentials(&self, url: &str) -> Result<Option<(String, Vec<u8>)>>;

    /// Delete a secret from the iOS keychain.
    fn delete_credentials(&self, url: &str) -> Result<()>;

    /// Become first responder so the software keyboard appears.
    fn show_soft_keyboard(&self);

    /// Resign first responder.
    fn hide_soft_keyboard(&self);

    /// Report the caret rectangle so the IME can position its candidate bar.
    fn update_ime_position(&self, bounds: Bounds<Pixels>);

    /// Current `UITraitCollection.userInterfaceStyle`.
    fn window_appearance(&self) -> WindowAppearance;

    /// Current thermal pressure.
    ///
    /// iOS reports this through `ProcessInfo.thermalState`, which is a
    /// notification rather than a poll, so a shell caches the last value.
    fn thermal_state(&self) -> gpui::ThermalState;

    /// GPUI's focused text-input state changed.
    fn text_input_state_changed(&self, change: gpui::TextInputStateChange);

    /// Enable or disable the shell's back gesture, if it has one.
    ///
    /// iOS has no system back button; this exists so the shared window code can
    /// be identical on both backends, and a shell may ignore it.
    fn set_back_enabled(&self, enabled: bool);
}
