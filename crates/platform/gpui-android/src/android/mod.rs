#![allow(clippy::missing_const_for_thread_local)]

mod dispatcher;
mod display;
mod host;
mod platform;
mod window;

use android_activity::AndroidApp;
use std::{cell::RefCell, rc::Rc};

pub(crate) use platform::AndroidPlatform;

thread_local! {
    static PLATFORM: RefCell<Option<Rc<AndroidPlatform>>> = const { RefCell::new(None) };
}

/// Creates the process-wide platform. Must be called once, from `android_main`.
pub fn init_platform(app: &AndroidApp) {
    host::initialize(app);
    PLATFORM.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_some() {
            log::warn!("replacing a previous gpui-android platform instance");
        }
        *slot = Some(Rc::new(AndroidPlatform::new(app.clone())));
    });
}

pub fn platform() -> Rc<dyn gpui::Platform> {
    PLATFORM.with(|slot| {
        slot.borrow()
            .as_ref()
            .cloned()
            .expect("gpui-android::init_platform() must be called before platform()")
    })
}

/// Installs a process-level observer for unhandled Android back actions.
///
/// A window-specific handler registered through `PlatformWindow` takes
/// precedence. This hook makes the back button observable by simple hosts.
pub fn set_back_callback(callback: impl FnMut() + 'static) {
    PLATFORM.with(|slot| {
        slot.borrow()
            .as_ref()
            .expect("gpui-android::init_platform() must be called first")
            .set_process_back_callback(Box::new(callback));
    });
}

/// Current system-bar and display-cutout insets in logical pixels.
pub fn safe_area() -> gpui::Edges<gpui::Pixels> {
    PLATFORM.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|platform| platform.safe_area())
            .unwrap_or_default()
    })
}

#[doc(hidden)]
pub fn jni_commit_text(text: String) {
    host::commit_text(text);
}

#[doc(hidden)]
pub fn jni_set_composing_text(text: String) {
    host::set_composing_text(text);
}

#[doc(hidden)]
pub fn jni_finish_composing_text() {
    host::finish_composing_text();
}

#[doc(hidden)]
pub fn jni_delete_backward() {
    host::delete_backward();
}

#[doc(hidden)]
pub fn jni_key_event(key_code: i32, down: bool, unicode_code_point: i32, meta_state: i32) {
    host::key_event(key_code, down, unicode_code_point, meta_state);
}

#[doc(hidden)]
pub fn jni_on_insets(left: i32, top: i32, right: i32, bottom: i32, ime_bottom: i32) {
    host::on_insets(left, top, right, bottom, ime_bottom);
}

#[doc(hidden)]
pub fn jni_on_back() {
    host::on_back();
}
