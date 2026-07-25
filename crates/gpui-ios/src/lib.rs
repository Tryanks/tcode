//! GPUI platform backend for iOS.
//!
//! Deliberately the same shape as `gpui-android`: this crate owns the
//! `gpui::Platform` implementation and the wgpu/Metal plumbing, and knows
//! nothing about UIKit. A separate `tcode-ios` shell owns the Objective-C side.
//! Keeping them apart is what lets the backend be offered upstream on its own.
//!
//! **iOS needs no upstream change.** An earlier plan recorded that it did,
//! because `gpui_wgpu::WgpuContext::instance` hardcodes `VULKAN | GL`. That
//! helper is not the only entry point: `WgpuContext::new` accepts an instance
//! the caller builds, and `WgpuRenderer::new` takes a `GpuContext` slot it fills
//! only when empty. Pre-filling that slot with a Metal context bypasses the
//! default — which is exactly what [`metal_context`] does.

#[cfg(target_os = "ios")]
mod native;
#[cfg(target_os = "ios")]
mod platform;

#[cfg(target_os = "ios")]
pub use native::{IosDisplayMetrics, IosHost, IosLayer, IosSurface, metal_context};
#[cfg(target_os = "ios")]
pub use platform::{IosEventSink, IosPlatform};
