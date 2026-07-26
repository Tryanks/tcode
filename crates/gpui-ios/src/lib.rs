//! GPUI platform backend for iOS.
//!
//! Deliberately the same module layout as `gpui-android`: `platform`, `window`,
//! `display`, `keyboard`, `dispatcher`, `native`. The two platforms differ in
//! their system APIs, not in what a GPUI backend must provide, so keeping the
//! shapes identical means a reader who knows one knows the other — and a fix to
//! one is trivially checkable against the other.
//!
//! **iOS needs no upstream change.** An earlier plan recorded that it did,
//! because `gpui_wgpu::WgpuContext::instance` hardcodes `VULKAN | GL`. That
//! helper is not the only entry point: `WgpuContext::new` accepts an instance
//! the caller builds, and `WgpuRenderer::new` takes a `GpuContext` slot it fills
//! only when empty. Pre-filling that slot with a Metal context bypasses the
//! default — which is what [`metal_context`] does.

#[cfg(target_os = "ios")]
mod dispatcher;
#[cfg(target_os = "ios")]
mod display;
#[cfg(target_os = "ios")]
mod keyboard;
#[cfg(target_os = "ios")]
mod native;
#[cfg(target_os = "ios")]
mod platform;
#[cfg(target_os = "ios")]
mod window;

#[cfg(target_os = "ios")]
pub use dispatcher::IosDispatcher;
#[cfg(target_os = "ios")]
pub use native::{IosDisplayMetrics, IosHost, IosLayer, IosSurface, metal_context};
#[cfg(target_os = "ios")]
pub use platform::{IosEventSink, IosPlatform};
