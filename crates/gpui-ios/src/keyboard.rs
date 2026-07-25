use gpui::PlatformKeyboardLayout;

pub(crate) struct IosKeyboardLayout;

impl PlatformKeyboardLayout for IosKeyboardLayout {
    fn id(&self) -> &str {
        "android"
    }

    fn name(&self) -> &str {
        "Ios"
    }
}
