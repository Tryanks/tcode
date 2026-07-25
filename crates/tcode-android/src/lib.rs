//! Android app shell for tcode: the JNI bridge that drives `gpui-android`.
//!
//! The host build exports nothing. Everything here needs a `JavaVM`, an
//! `ANativeWindow` and a Looper, none of which exist off Android, so the whole
//! implementation stays behind `target_os = "android"` and ordinary desktop
//! workspace builds are unaffected.
//!
//! See `crates/gpui-android/README.md` for the contract this implements. The
//! division of labour there is deliberate: `gpui-android` owns the GPUI
//! `Platform` implementation and knows nothing about Java, while this crate owns
//! Java and knows nothing about GPUI's internals. Keeping them separate is what
//! lets the backend be offered upstream on its own.

#[cfg(target_os = "android")]
mod bridge;
#[cfg(target_os = "android")]
mod entry;

#[cfg(target_os = "android")]
pub use bridge::JniHost;
