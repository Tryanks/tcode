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

#[cfg(target_os = "android")]
pub(crate) fn delete_backward(handler: &mut gpui::PlatformInputHandler) {
    let Some(selection) = handler.selected_text_range(true) else {
        return;
    };
    let mut range = selection.range;
    if range.is_empty() {
        let Some(text) = handler.text_for_range(0..range.start, &mut None) else {
            return;
        };
        range = previous_grapheme_range(&text, range.start);
    }
    if range.is_empty() {
        return;
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
}

/// Character preference bypasses GPUI bindings, so control keys must not use it.
pub(crate) fn ime_key_down(keystroke: Keystroke) -> KeyDownEvent {
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
            let event = ime_key_down(Keystroke {
                key: key.into(),
                key_char: None,
                modifiers: Default::default(),
            });
            assert!(!event.prefer_character_input, "{key} bypassed bindings");
        }
        let mut stroke = Keystroke {
            key: "a".into(),
            key_char: Some("a".into()),
            modifiers: Default::default(),
        };
        assert!(ime_key_down(stroke.clone()).prefer_character_input);
        stroke.modifiers.control = true;
        assert!(!ime_key_down(stroke).prefer_character_input);
    }
}
