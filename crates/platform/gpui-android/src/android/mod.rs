mod dispatcher;
mod display;
mod host;
mod platform;
mod text;
pub mod webview;
mod window;

use android_activity::AndroidApp;
use std::{cell::RefCell, rc::Rc};

pub(crate) use platform::AndroidPlatform;

#[doc(hidden)]
pub use host::{
    commit_text as jni_commit_text, delete_backward as jni_delete_backward,
    finish_composing_text as jni_finish_composing_text, key_event as jni_key_event,
    on_back as jni_on_back, on_insets as jni_on_insets,
    set_composing_text as jni_set_composing_text,
};

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

/// Current system-bar, display-cutout, and software-keyboard insets.
pub fn insets() -> gpui::WindowInsets {
    PLATFORM.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|platform| platform.insets())
            .unwrap_or_default()
    })
}

pub(crate) use host::show_keyboard;
