use gpui::PlatformKeyboardLayout;

pub(crate) struct AndroidKeyboardLayout;

impl PlatformKeyboardLayout for AndroidKeyboardLayout {
    fn id(&self) -> &str {
        "android"
    }

    fn name(&self) -> &str {
        "Android"
    }
}
