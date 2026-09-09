use crate::widgets::kbd::Kbd;
use gpui::{Action, App, KeyBinding, Keystroke, Modifiers, NoAction};
use serde::Deserialize;

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode, no_json)]
pub(crate) enum NavigateThread {
    Index(usize),
    Next,
    Previous,
}

pub(crate) fn init(cx: &mut App) {
    cx.on_action(crate::shell::navigate_thread);
    for number in 1..=9 {
        let key = format!("ctrl-{number}");
        cx.bind_keys([
            KeyBinding::new(&key, NavigateThread::Index(number - 1), None),
            KeyBinding::new(&key, NoAction, Some("ModelPicker || ModelPicker > Input")),
        ]);
    }
    cx.bind_keys([
        KeyBinding::new("ctrl-tab", NavigateThread::Next, None),
        KeyBinding::new("ctrl-shift-tab", NavigateThread::Previous, None),
    ]);
}

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
