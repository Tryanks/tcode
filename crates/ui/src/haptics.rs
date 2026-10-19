//! The platform's touch feedback, where it has one.

/// The haptic a phone plays when a long press is recognized, before the
/// finger lifts: the moment the press has done its work and can be released.
/// Desktops and the web have none.
pub(crate) fn long_press() {
    #[cfg(target_os = "ios")]
    gpui_ios::haptic_long_press();
    #[cfg(all(feature = "native-preview", target_os = "android"))]
    gpui_android::haptic_long_press();
}
