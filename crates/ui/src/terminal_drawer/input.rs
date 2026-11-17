use super::*;

pub(super) struct TerminalInputHandler {
    pub(super) drawer: Entity<TerminalDrawer>,
    pub(super) terminal_id: u64,
    pub(super) cursor_bounds: Option<Bounds<Pixels>>,
    pub(super) cell_width: Pixels,
}

impl InputHandler for TerminalInputHandler {
    fn paste(&mut self, item: ClipboardItem, _window: &mut Window, cx: &mut App) {
        if let Some(text) = item.text() {
            let terminal_id = self.terminal_id;
            self.drawer.update(cx, |drawer, cx| {
                drawer.paste_to_terminal(terminal_id, &text, cx)
            });
        }
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(&mut self, _window: &mut Window, cx: &mut App) -> Option<Range<usize>> {
        self.drawer
            .read(cx)
            .marked_text
            .as_ref()
            .filter(|marked| marked.terminal_id == self.terminal_id)
            .map(|marked| 0..marked.text.encode_utf16().count())
    }

    fn text_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<String> {
        None
    }

    fn replace_text_in_range(
        &mut self,
        _replacement_range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut App,
    ) {
        let terminal_id = self.terminal_id;
        let text = text.to_string();
        self.drawer.update(cx, |drawer, cx| {
            drawer.marked_text = None;
            drawer.restart_blink();
            drawer.bell_tabs.remove(&terminal_id);
            if !text.is_empty() {
                drawer.send_committed_text(terminal_id, &text, cx);
            }
            cx.notify();
        });
        window.invalidate_character_coordinates();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range_utf16: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let terminal_id = self.terminal_id;
        let marked_text = (!new_text.is_empty()).then(|| MarkedText {
            terminal_id,
            text: new_text.to_string(),
        });
        self.drawer.update(cx, |drawer, cx| {
            drawer.marked_text = marked_text;
            cx.notify();
        });
        window.invalidate_character_coordinates();
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut App) {
        let terminal_id = self.terminal_id;
        self.drawer.update(cx, |drawer, cx| {
            if drawer
                .marked_text
                .as_ref()
                .is_some_and(|marked| marked.terminal_id == terminal_id)
            {
                drawer.marked_text = None;
                cx.notify();
            }
        });
        window.invalidate_character_coordinates();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        let mut bounds = self.cursor_bounds?;
        bounds.origin.x += self.cell_width * range_utf16.start as f32;
        Some(bounds)
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<usize> {
        None
    }

    fn apple_press_and_hold_enabled(&mut self) -> bool {
        false
    }
}

pub(super) fn terminal_clipboard_shortcut(
    key: &str,
    modifiers: gpui::Modifiers,
    use_platform_modifier: bool,
) -> Option<ClipboardShortcut> {
    let expected_modifiers = if use_platform_modifier {
        modifiers.platform
            && !modifiers.control
            && !modifiers.alt
            && !modifiers.shift
            && !modifiers.function
    } else {
        modifiers.control
            && modifiers.shift
            && !modifiers.platform
            && !modifiers.alt
            && !modifiers.function
    };
    if !expected_modifiers {
        return None;
    }
    if key.eq_ignore_ascii_case("c") {
        Some(ClipboardShortcut::Copy)
    } else if key.eq_ignore_ascii_case("v") {
        Some(ClipboardShortcut::Paste)
    } else {
        None
    }
}

pub(super) fn terminal_link_modifier(
    modifiers: gpui::Modifiers,
    use_platform_modifier: bool,
) -> bool {
    if use_platform_modifier {
        modifiers.platform
    } else {
        modifiers.control
    }
}

pub(super) fn prepare_terminal_paste(text: &str, bracketed_paste: bool) -> String {
    if bracketed_paste {
        format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', ""))
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r")
    }
}

pub(super) fn terminal_key_bytes(
    keystroke: &gpui::Keystroke,
    mode: Mode,
    keyboard_mode: tcode_protocol::terminal::KeyboardModes,
    modify_other_keys: Option<u8>,
) -> Option<Vec<u8>> {
    mappings::key_bytes(
        &keystroke.key,
        term_modifiers(keystroke.modifiers),
        mode,
        keyboard_mode,
        modify_other_keys,
        true,
    )
}

pub(super) fn term_modifiers(modifiers: gpui::Modifiers) -> TermModifiers {
    TermModifiers {
        shift: modifiers.shift,
        alt: modifiers.alt,
        control: modifiers.control,
        platform: modifiers.platform,
    }
}

pub(super) fn term_mouse_button(button: MouseButton) -> Option<TermMouseButton> {
    match button {
        MouseButton::Left => Some(TermMouseButton::Left),
        MouseButton::Middle => Some(TermMouseButton::Middle),
        MouseButton::Right => Some(TermMouseButton::Right),
        _ => None,
    }
}

pub(super) fn shell_quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', "'\\''"))
}
