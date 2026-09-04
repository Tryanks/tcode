//! Android platform backend for `gpui-pre`.
//!
//! Android owns the activity and window lifecycle, so [`init_platform`] must
//! be called from `android_main` before constructing a GPUI [`gpui::Application`].

use std::rc::Rc;

#[cfg(target_os = "android")]
mod android;

#[cfg(target_os = "android")]
pub use android::{
    init_platform, jni_commit_text, jni_delete_backward, jni_finish_composing_text, jni_key_event,
    jni_on_back, jni_on_insets, jni_set_composing_text, safe_area, set_back_callback,
};

#[cfg(not(target_os = "android"))]
pub fn safe_area() -> gpui::Edges<gpui::Pixels> {
    gpui::Edges::default()
}

/// Returns the process-wide Android platform created by [`init_platform`].
#[cfg(target_os = "android")]
pub fn platform() -> Rc<dyn gpui::Platform> {
    android::platform()
}

/// Android is the only target on which this backend can be initialized.
#[cfg(not(target_os = "android"))]
pub fn platform() -> Rc<dyn gpui::Platform> {
    panic!("gpui-android::platform() is only available on Android")
}
