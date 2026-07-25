//! GPUI platform backend for Android.
//!
//! The host build intentionally exports no backend. Android-only dependencies and
//! implementation details stay behind `target_os = "android"` so this workspace can
//! continue to build on desktop hosts.

#[cfg(target_os = "android")]
mod dispatcher;
#[cfg(target_os = "android")]
mod display;
#[cfg(target_os = "android")]
mod keyboard;
#[cfg(target_os = "android")]
mod native;
#[cfg(target_os = "android")]
mod platform;
#[cfg(target_os = "android")]
mod window;

#[cfg(target_os = "android")]
pub use dispatcher::AndroidDispatcher;
#[cfg(target_os = "android")]
pub use native::{AndroidDisplayMetrics, AndroidHost, AndroidNativeWindow, AndroidSurface};
#[cfg(target_os = "android")]
pub use platform::{AndroidEventSink, AndroidPlatform};
