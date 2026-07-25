//! `IosHost` implemented over UIKit.
//!
//! The structural rule mirrors the Android bridge's: nothing here may assume it
//! is on the main thread. GPUI calls `wake_main_thread` and `request_frame` from
//! worker threads, while UIKit and Core Animation are main-thread-only, so every
//! such call is marshalled rather than performed in place.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, anyhow};
use gpui::{Bounds, Pixels, WindowAppearance};
use gpui_ios::IosHost;
use objc2_foundation::{MainThreadMarker, NSDictionary, NSString, NSURL};
use objc2_ui_kit::{UIApplication, UIPasteboard, UIUserInterfaceStyle};

/// Callbacks the Swift/Objective-C shell registers at launch.
///
/// A function-pointer table rather than a protocol object: these are called from
/// arbitrary Rust threads, and a plain `extern "C"` pointer has no retain/release
/// or thread-affinity semantics to get wrong. The shell is responsible for
/// hopping to the main queue inside them.
#[derive(Clone, Copy)]
pub struct HostCallbacks {
    /// Post to the main queue so it calls `tcode_ios_drain_main_thread`.
    pub wake_main_thread: extern "C" fn(*mut c_void),
    /// Schedule one `CADisplayLink` tick that calls `tcode_ios_frame`.
    pub request_frame: extern "C" fn(*mut c_void),
    /// Make the hosting view first responder, showing the keyboard.
    pub show_keyboard: extern "C" fn(*mut c_void),
    /// Resign first responder.
    pub hide_keyboard: extern "C" fn(*mut c_void),
    /// Opaque shell pointer handed back to every callback.
    pub context: *mut c_void,
}

// SAFETY: the shell must keep `context` alive for the process lifetime and make
// each callback safe to invoke from any thread — which is the documented
// contract for registering them. Without these impls the host could not be
// shared with GPUI's executors, which is the entire point of the type.
unsafe impl Send for HostCallbacks {}
unsafe impl Sync for HostCallbacks {}

pub struct UiKitHost {
    callbacks: HostCallbacks,
    /// True while a display-link tick is already scheduled.
    ///
    /// The trait requires coalescing: GPUI may request a frame several times
    /// within one vsync, and each extra tick would queue a redundant frame
    /// behind the last. Cleared by the frame entry point before the sink runs,
    /// so a rearm during that frame can schedule the next one.
    frame_pending: AtomicBool,
}

impl UiKitHost {
    pub fn new(callbacks: HostCallbacks) -> Self {
        Self {
            callbacks,
            frame_pending: AtomicBool::new(false),
        }
    }

    pub(crate) fn clear_frame_pending(&self) {
        self.frame_pending.store(false, Ordering::Release);
    }
}

impl IosHost for UiKitHost {
    fn wake_main_thread(&self) {
        (self.callbacks.wake_main_thread)(self.callbacks.context);
    }

    fn request_frame(&self) {
        if self
            .frame_pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            (self.callbacks.request_frame)(self.callbacks.context);
        }
    }

    fn finish_activity(&self) {
        // iOS apps do not exit on demand: exit(0) is grounds for App Store
        // rejection and reads as a crash to the user. The system decides when a
        // suspended app dies, so the honest implementation is to do nothing.
    }

    fn open_url(&self, url: &str) -> Result<()> {
        let string = NSString::from_str(url);
        let url = NSURL::URLWithString(&string)
            .ok_or_else(|| anyhow!("{url} is not a URL UIKit can open"))?;
        // `openURL:` is main-thread-only, and objc2 makes that a type
        // requirement rather than a comment. Refusing off-thread is right:
        // opening a URL is always user-initiated, so a background caller is a
        // bug worth reporting instead of a race worth hiding.
        let marker = MainThreadMarker::new()
            .ok_or_else(|| anyhow!("openURL must be called on the main thread"))?;
        let application = UIApplication::sharedApplication(marker);
        // The options/completionHandler form, not the deprecated one-argument
        // `openURL:`. Empty options and no handler mean "open with the default
        // behaviour and do not tell me whether it worked" — which matches what
        // GPUI's `open_url` can express, since its signature returns nothing.
        // SAFETY: a main-thread-checked UIApplication with a valid URL and an
        // empty options dictionary.
        unsafe { application.openURL_options_completionHandler(&url, &NSDictionary::new(), None) };
        Ok(())
    }

    fn read_clipboard_text(&self) -> Result<Option<String>> {
        let pasteboard = UIPasteboard::generalPasteboard();
        // SAFETY: UIPasteboard's accessors are main-thread-only; GPUI reads the
        // clipboard from its main-thread paste handling.
        Ok(unsafe { pasteboard.string() }.map(|value| value.to_string()))
    }

    fn write_clipboard_text(&self, text: &str) -> Result<()> {
        let pasteboard = UIPasteboard::generalPasteboard();
        // SAFETY: as above.
        unsafe { pasteboard.setString(Some(&NSString::from_str(text))) };
        Ok(())
    }

    fn write_credentials(&self, _url: &str, _username: &str, _password: &[u8]) -> Result<()> {
        // Honest failure, not a silent success. A credential store that discards
        // secrets is worse than one reporting itself absent, because the caller
        // would believe the secret was saved. Needs a Security.framework keychain
        // wrapper.
        Err(anyhow!("keychain storage is not wired up on iOS yet"))
    }

    fn read_credentials(&self, _url: &str) -> Result<Option<(String, Vec<u8>)>> {
        // `None` is truthful: nothing was ever stored, so nothing is found.
        Ok(None)
    }

    fn delete_credentials(&self, _url: &str) -> Result<()> {
        Err(anyhow!("keychain storage is not wired up on iOS yet"))
    }

    fn show_soft_keyboard(&self) {
        (self.callbacks.show_keyboard)(self.callbacks.context);
    }

    fn hide_soft_keyboard(&self) {
        (self.callbacks.hide_keyboard)(self.callbacks.context);
    }

    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {
        // Positioning the candidate bar needs a `UITextInput` conformance
        // reporting caret rects. Until the shell has one, iOS places the IME
        // itself — wrong, but not broken.
    }

    fn thermal_state(&self) -> gpui::ThermalState {
        // ProcessInfo.thermalState is delivered as a notification rather than
        // polled, so a shell caches the last value. Until it does, nominal is
        // the honest answer — guessing would make GPUI throttle for nothing.
        gpui::ThermalState::Nominal
    }

    fn text_input_state_changed(&self, _change: gpui::TextInputStateChange) {
        // Needs a UITextInput conformance on the hosting view; without one there
        // is nothing on the UIKit side to tell.
    }

    fn set_back_enabled(&self, _enabled: bool) {
        // iOS has no system back button. The method exists so the shared window
        // code is identical on both backends.
    }

    fn window_appearance(&self) -> WindowAppearance {
        // `UITraitCollection.current` is main-thread-only; GPUI queries this
        // during layout, which is already on the main thread.
        // SAFETY: `UITraitCollection.current` is main-thread-only, and GPUI reads
        // appearance during layout, which is already on the main thread.
        let style = unsafe { objc2_ui_kit::UITraitCollection::currentTraitCollection() };
        match unsafe { style.userInterfaceStyle() } {
            UIUserInterfaceStyle::Dark => WindowAppearance::Dark,
            _ => WindowAppearance::Light,
        }
    }
}
