//! iOS app shell for tcode: the UIKit bridge that drives `gpui-ios`.
//!
//! Mirrors `tcode-android`. That crate owns Java and knows nothing about GPUI's
//! internals; this one owns Objective-C and likewise. The platform backends stay
//! plain Rust crates that could be offered upstream on their own.
//!
//! Everything is behind `target_os = "ios"`: a `UIApplication`, a `CAMetalLayer`
//! and a main run loop do not exist on a host, and the desktop workspace build
//! must stay green.

#[cfg(target_os = "ios")]
mod bridge;
#[cfg(target_os = "ios")]
mod entry;

#[cfg(target_os = "ios")]
pub use bridge::UiKitHost;
