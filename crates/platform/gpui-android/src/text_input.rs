use gpui::{KeyDownEvent, Keystroke};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

fn previous_grapheme_range(before_cursor: &str, cursor: usize) -> Range<usize> {
    let length = before_cursor
        .graphemes(true)
        .next_back()
        .map_or(0, |grapheme| grapheme.encode_utf16().count());
    cursor.saturating_sub(length)..cursor
}

fn delete_from_composition(text: &str, range: Range<usize>) -> (String, Range<usize>) {
    let cursor = range.start;
    let mut units = text.encode_utf16().collect::<Vec<_>>();
    units.drain(range);
    (String::from_utf16_lossy(&units), cursor..cursor)
}

// None distinguishes a key-oriented handler from an empty editable field.
fn backward_delete_range(
    selection: Option<Range<usize>>,
    mut text_before: impl FnMut(usize) -> Option<String>,
) -> Option<Range<usize>> {
    let range = selection?;
    if range.is_empty() {
        Some(previous_grapheme_range(
            &text_before(range.start)?,
            range.start,
        ))
    } else {
        Some(range)
    }
}

#[cfg(target_os = "android")]
pub(crate) fn delete_backward(handler: &mut gpui::PlatformInputHandler) -> bool {
    let selection = handler
        .selected_text_range(true)
        .map(|selection| selection.range);
    let Some(range) = backward_delete_range(selection, |cursor| {
        handler.text_for_range(0..cursor, &mut None)
    }) else {
        return false;
    };
    if range.is_empty() {
        return true;
    }
    if let Some(marked) = handler.marked_text_range()
        && marked.start <= range.start
        && range.end <= marked.end
        && let Some(text) = handler.text_for_range(marked.clone(), &mut None)
    {
        // Keep both the remaining preedit and its cursor coherent with the IME.
        let (text, selection) =
            delete_from_composition(&text, range.start - marked.start..range.end - marked.start);
        handler.replace_and_mark_text_in_range(Some(marked), &text, Some(selection));
    } else {
        handler.replace_text_in_range(Some(range), "");
    }
    true
}

/// Plain multiline Enter is text; other control keys must still reach bindings.
pub(crate) fn ime_key_down(mut keystroke: Keystroke, multi_line: bool) -> KeyDownEvent {
    if multi_line && keystroke.key == "enter" && keystroke.modifiers == gpui::Modifiers::default() {
        keystroke.key_char = Some("\n".into());
    }
    let prefer_character_input = keystroke.key_char.is_some()
        && !keystroke.modifiers.control
        && !keystroke.modifiers.platform
        && !keystroke.modifiers.alt;
    KeyDownEvent {
        keystroke,
        is_held: false,
        prefer_character_input,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_and_absent_text_fields_fall_back_to_control_keystrokes() {
        assert_eq!(backward_delete_range(None, |_| None), None);
        assert_eq!(backward_delete_range(Some(0..0), |_| None), None);
        // An empty editor consumes deletion; it must not receive a second key.
        assert_eq!(
            backward_delete_range(Some(0..0), |_| Some(String::new())),
            Some(0..0)
        );
        for key in ["backspace", "enter", "left", "right", "up", "down"] {
            let event = ime_key_down(
                Keystroke {
                    key: key.into(),
                    key_char: None,
                    modifiers: Default::default(),
                },
                false,
            );
            assert_eq!(event.keystroke.key, key);
            assert!(!event.prefer_character_input);
        }
    }

    #[test]
    fn multiline_ime_enter_inserts_newline_while_single_line_runs_action() {
        let enter = Keystroke {
            key: "enter".into(),
            key_char: None,
            modifiers: Default::default(),
        };
        let event = ime_key_down(enter.clone(), true);
        assert_eq!(event.keystroke.key_char.as_deref(), Some("\n"));
        assert!(event.prefer_character_input);
        let event = ime_key_down(enter.clone(), false);
        assert_eq!(event.keystroke.key_char, None);
        assert!(!event.prefer_character_input);
        for modifiers in [
            gpui::Modifiers {
                shift: true,
                ..Default::default()
            },
            gpui::Modifiers {
                control: true,
                ..Default::default()
            },
        ] {
            let event = ime_key_down(
                Keystroke {
                    modifiers,
                    ..enter.clone()
                },
                true,
            );
            assert_eq!(event.keystroke.key_char, None);
            assert!(!event.prefer_character_input);
        }
    }

    #[test]
    fn deleting_preedit_retains_its_suffix_and_relative_utf16_cursor() {
        assert_eq!(delete_from_composition("中文", 1..2), ("中".into(), 1..1));
        assert_eq!(
            delete_from_composition("nihao", 1..2),
            ("nhao".into(), 1..1)
        );
        assert_eq!(delete_from_composition("😀文", 0..2), ("文".into(), 0..0));
        assert_eq!(delete_from_composition("中", 0..1), (String::new(), 0..0));
    }

    #[test]
    fn backspace_removes_a_whole_grapheme_in_utf16_coordinates() {
        for (text, expected) in [
            ("", 0..0),
            ("abc", 2..3),
            ("中文", 1..2),
            ("a😀", 1..3),
            ("ae\u{301}", 1..3),
            ("a👨‍👩‍👧‍👦", 1..12),
        ] {
            assert_eq!(
                previous_grapheme_range(text, text.encode_utf16().count()),
                expected
            );
        }
    }

    #[test]
    fn ime_control_keys_reach_input_bindings() {
        for key in ["backspace", "delete", "enter", "left", "right"] {
            let event = ime_key_down(
                Keystroke {
                    key: key.into(),
                    key_char: None,
                    modifiers: Default::default(),
                },
                false,
            );
            assert!(!event.prefer_character_input, "{key} bypassed bindings");
        }
        let mut stroke = Keystroke {
            key: "a".into(),
            key_char: Some("a".into()),
            modifiers: Default::default(),
        };
        assert!(ime_key_down(stroke.clone(), false).prefer_character_input);
        stroke.modifiers.control = true;
        assert!(!ime_key_down(stroke, false).prefer_character_input);
    }
}
