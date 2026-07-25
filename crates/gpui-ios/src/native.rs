//! The UIKit seam, and the Metal context that makes iOS work without an
//! upstream change.
//!
//! Everything UIKit-shaped is behind [`IosHost`]. This crate never links
//! Objective-C itself: the shell that owns a `UIViewController` implements the
//! trait, so the backend stays a plain Rust crate that could be offered
//! upstream.

use std::cell::RefCell;
use std::rc::Rc;

use anyhow::{Context as _, Result, anyhow};
use gpui::{Bounds, Pixels, WindowAppearance};
use gpui_wgpu::wgpu;
use gpui_wgpu::{GpuContext, WgpuContext};
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawWindowHandle,
    UiKitWindowHandle, WindowHandle,
};

/// Physical size and scale of the layer GPUI draws into.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IosDisplayMetrics {
    width_px: u32,
    height_px: u32,
    scale_factor: f32,
}

impl IosDisplayMetrics {
    /// Validated metrics.
    ///
    /// Rejected rather than clamped: a zero dimension or a non-positive scale
    /// means UIKit handed us a layer that is not ready, and configuring a
    /// surface against it produces a driver error much further from the cause.
    pub fn new(width_px: u32, height_px: u32, scale_factor: f32) -> Result<Self> {
        if width_px == 0 || height_px == 0 {
            return Err(anyhow!(
                "a drawable layer cannot be {width_px}x{height_px} pixels"
            ));
        }
        if !scale_factor.is_finite() || scale_factor <= 0.0 {
            return Err(anyhow!(
                "contentScaleFactor must be positive, got {scale_factor}"
            ));
        }
        Ok(Self {
            width_px,
            height_px,
            scale_factor,
        })
    }

    pub fn width_px(&self) -> u32 {
        self.width_px
    }

    pub fn height_px(&self) -> u32 {
        self.height_px
    }

    pub fn scale_factor(&self) -> f32 {
        self.scale_factor
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
    layer: IosLayer,
    metrics: IosDisplayMetrics,
}

impl IosSurface {
    pub fn new(layer: IosLayer, metrics: IosDisplayMetrics) -> Self {
        Self { layer, metrics }
    }

    pub fn layer(&self) -> IosLayer {
        self.layer
    }

    pub fn metrics(&self) -> IosDisplayMetrics {
        self.metrics
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
}
