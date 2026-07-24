use gpui::{Keystroke, Modifiers};
use gpui_component::kbd::Kbd;

/// Format a shortcut using GPUI's semantic secondary modifier.
///
/// The secondary modifier is Command on macOS and Control on Windows/Linux.
pub(crate) fn format_secondary_shortcut(key: &str) -> String {
    Kbd::format(&Keystroke {
        modifiers: Modifiers::secondary_key(),
        key: key.to_owned(),
        key_char: None,
    })
}

#[cfg(test)]
mod tests {
    use super::format_secondary_shortcut;

    #[test]
    fn formats_secondary_shortcuts_for_current_target() {
        if cfg!(target_os = "macos") {
            assert_eq!(format_secondary_shortcut("k"), "⌘K");
            assert_eq!(format_secondary_shortcut("1"), "⌘1");
            assert_eq!(format_secondary_shortcut("enter"), "⌘⏎");
        } else {
            assert_eq!(format_secondary_shortcut("k"), "Ctrl+K");
            assert_eq!(format_secondary_shortcut("1"), "Ctrl+1");
            assert_eq!(format_secondary_shortcut("enter"), "Ctrl+Enter");
        }
    }
}
