//! Classic xterm input encoders, kept independent of the UI framework.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
    pub platform: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridPoint {
    pub row: usize,
    pub column: usize,
}

fn modifier_bits(modifiers: Modifiers) -> u8 {
    u8::from(modifiers.shift) * 4 + u8::from(modifiers.alt) * 8 + u8::from(modifiers.control) * 16
}

fn button_code(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

pub fn mouse_button_report(
    point: GridPoint,
    button: MouseButton,
    modifiers: Modifiers,
    pressed: bool,
    mode: crate::ModeSnapshot,
) -> Option<Vec<u8>> {
    mode.mouse_mode()
        .then(|| mouse_report(point, button_code(button), modifiers, pressed, mode))?
}

pub fn mouse_move_report(
    point: GridPoint,
    button: Option<MouseButton>,
    modifiers: Modifiers,
    mode: crate::ModeSnapshot,
) -> Option<Vec<u8>> {
    if !(mode.mouse_motion || mode.mouse_drag && button.is_some()) {
        return None;
    }
    let code = button.map_or(35, |button| button_code(button) + 32);
    mouse_report(point, code, modifiers, true, mode)
}

pub fn scroll_report(
    point: GridPoint,
    lines: i32,
    modifiers: Modifiers,
    mode: crate::ModeSnapshot,
) -> Option<Vec<u8>> {
    if !mode.mouse_mode() || lines == 0 {
        return None;
    }
    let code = if lines > 0 { 64 } else { 65 };
    let report = mouse_report(point, code, modifiers, true, mode)?;
    Some(report.repeat(lines.unsigned_abs() as usize))
}

pub fn alt_scroll(lines: i32) -> Vec<u8> {
    let suffix = if lines > 0 { b'A' } else { b'B' };
    let mut bytes = Vec::with_capacity(lines.unsigned_abs() as usize * 3);
    for _ in 0..lines.unsigned_abs() {
        bytes.extend_from_slice(&[0x1b, b'O', suffix]);
    }
    bytes
}

fn mouse_report(
    point: GridPoint,
    button: u8,
    modifiers: Modifiers,
    pressed: bool,
    mode: crate::ModeSnapshot,
) -> Option<Vec<u8>> {
    let button = button + modifier_bits(modifiers);
    if mode.sgr_mouse {
        let suffix = if pressed { 'M' } else { 'm' };
        Some(
            format!(
                "\x1b[<{button};{};{}{suffix}",
                point.column + 1,
                point.row + 1
            )
            .into_bytes(),
        )
    } else {
        normal_mouse_report(
            point,
            if pressed {
                button
            } else {
                3 + modifier_bits(modifiers)
            },
            mode.utf8_mouse,
        )
    }
}

fn normal_mouse_report(point: GridPoint, button: u8, utf8: bool) -> Option<Vec<u8>> {
    let max_point = if utf8 { 2015 } else { 223 };
    if point.row >= max_point || point.column >= max_point {
        return None;
    }
    let mut bytes = vec![0x1b, b'[', b'M', 32 + button];
    let encode = |position: usize| {
        let position = 33 + position;
        vec![(0xc0 + position / 64) as u8, (0x80 + (position & 63)) as u8]
    };
    if utf8 && point.column >= 95 {
        bytes.extend(encode(point.column));
    } else {
        bytes.push(33 + point.column as u8);
    }
    if utf8 && point.row >= 95 {
        bytes.extend(encode(point.row));
    } else {
        bytes.push(33 + point.row as u8);
    }
    Some(bytes)
}

pub fn key_bytes(
    key: &str,
    modifiers: Modifiers,
    mode: crate::ModeSnapshot,
    option_as_meta: bool,
) -> Option<Vec<u8>> {
    let none = !modifiers.shift && !modifiers.alt && !modifiers.control && !modifiers.platform;
    let only = |shift, alt, control| {
        modifiers.shift == shift
            && modifiers.alt == alt
            && modifiers.control == control
            && !modifiers.platform
    };
    let fixed = match key {
        "tab" if none => Some("\t"),
        "escape" if none => Some("\x1b"),
        "enter" if none => Some("\r"),
        "enter" if only(true, false, false) => Some("\n"),
        "enter" if only(false, true, false) => Some("\x1b\r"),
        "backspace" if none || only(true, false, false) => Some("\x7f"),
        "tab" if only(true, false, false) => Some("\x1b[Z"),
        "backspace" if only(false, false, true) => Some("\x08"),
        "backspace" if only(false, true, false) => Some("\x1b\x7f"),
        "space" | "@" if only(false, false, true) => Some("\0"),
        "home" if none => Some(if mode.app_cursor { "\x1bOH" } else { "\x1b[H" }),
        "end" if none => Some(if mode.app_cursor { "\x1bOF" } else { "\x1b[F" }),
        "up" if none => Some(if mode.app_cursor { "\x1bOA" } else { "\x1b[A" }),
        "down" if none => Some(if mode.app_cursor { "\x1bOB" } else { "\x1b[B" }),
        "right" if none => Some(if mode.app_cursor { "\x1bOC" } else { "\x1b[C" }),
        "left" if none => Some(if mode.app_cursor { "\x1bOD" } else { "\x1b[D" }),
        "back" if none => Some("\x7f"),
        "insert" if none => Some("\x1b[2~"),
        "delete" if none => Some("\x1b[3~"),
        "pageup" if none => Some("\x1b[5~"),
        "pagedown" if none => Some("\x1b[6~"),
        "f1" if none => Some("\x1bOP"),
        "f2" if none => Some("\x1bOQ"),
        "f3" if none => Some("\x1bOR"),
        "f4" if none => Some("\x1bOS"),
        "f5" if none => Some("\x1b[15~"),
        "f6" if none => Some("\x1b[17~"),
        "f7" if none => Some("\x1b[18~"),
        "f8" if none => Some("\x1b[19~"),
        "f9" if none => Some("\x1b[20~"),
        "f10" if none => Some("\x1b[21~"),
        "f11" if none => Some("\x1b[23~"),
        "f12" if none => Some("\x1b[24~"),
        "f13" if none => Some("\x1b[25~"),
        "f14" if none => Some("\x1b[26~"),
        "f15" if none => Some("\x1b[28~"),
        "f16" if none => Some("\x1b[29~"),
        "f17" if none => Some("\x1b[31~"),
        "f18" if none => Some("\x1b[32~"),
        "f19" if none => Some("\x1b[33~"),
        "f20" if none => Some("\x1b[34~"),
        _ => None,
    };
    if let Some(fixed) = fixed {
        return Some(fixed.as_bytes().to_vec());
    }
    if (only(false, false, true) || only(true, false, true)) && key.chars().count() == 1 {
        let ch = key.chars().next()?;
        let control = match ch {
            'a'..='z' | 'A'..='Z' => (ch.to_ascii_uppercase() as u8) - b'@',
            '[' => 27,
            '\\' => 28,
            ']' => 29,
            '^' => 30,
            '_' => 31,
            '?' => 127,
            _ => return None,
        };
        return Some(vec![control]);
    }
    if modifiers.shift || modifiers.alt || modifiers.control || modifiers.platform {
        let code = 1
            + u8::from(modifiers.shift)
            + 2 * u8::from(modifiers.alt)
            + 4 * u8::from(modifiers.control);
        let sequence = match key {
            "up" => format!("\x1b[1;{code}A"),
            "down" => format!("\x1b[1;{code}B"),
            "right" => format!("\x1b[1;{code}C"),
            "left" => format!("\x1b[1;{code}D"),
            "home" => format!("\x1b[1;{code}H"),
            "end" => format!("\x1b[1;{code}F"),
            "f1" => format!("\x1b[1;{code}P"),
            "f2" => format!("\x1b[1;{code}Q"),
            "f3" => format!("\x1b[1;{code}R"),
            "f4" => format!("\x1b[1;{code}S"),
            "insert" => format!("\x1b[2;{code}~"),
            "delete" => format!("\x1b[3;{code}~"),
            "pageup" => format!("\x1b[5;{code}~"),
            "pagedown" => format!("\x1b[6;{code}~"),
            key @ ("f5" | "f6" | "f7" | "f8" | "f9" | "f10" | "f11" | "f12" | "f13" | "f14"
            | "f15" | "f16" | "f17" | "f18" | "f19" | "f20") => {
                let prefix = [
                    15, 17, 18, 19, 20, 21, 23, 24, 25, 26, 28, 29, 31, 32, 33, 34,
                ][key[1..].parse::<usize>().ok()? - 5];
                format!("\x1b[{prefix};{code}~")
            }
            _ => {
                let alt_meta = modifiers.alt
                    && !modifiers.control
                    && !modifiers.platform
                    && (cfg!(not(target_os = "macos")) || option_as_meta);
                if alt_meta && key.is_ascii() {
                    return Some(
                        format!(
                            "\x1b{}",
                            if modifiers.shift {
                                key.to_ascii_uppercase()
                            } else {
                                key.to_string()
                            }
                        )
                        .into_bytes(),
                    );
                }
                return None;
            }
        };
        return Some(sequence.into_bytes());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sgr() -> crate::ModeSnapshot {
        crate::ModeSnapshot {
            mouse_click: true,
            sgr_mouse: true,
            ..Default::default()
        }
    }
    #[test]
    fn sgr_press_release_move_and_modifiers() {
        let point = GridPoint { row: 2, column: 4 };
        assert_eq!(
            mouse_button_report(point, MouseButton::Left, Modifiers::default(), true, sgr()),
            Some(b"\x1b[<0;5;3M".to_vec())
        );
        assert_eq!(
            mouse_button_report(point, MouseButton::Left, Modifiers::default(), false, sgr()),
            Some(b"\x1b[<0;5;3m".to_vec())
        );
        let mode = crate::ModeSnapshot {
            mouse_motion: true,
            sgr_mouse: true,
            ..Default::default()
        };
        assert_eq!(
            mouse_move_report(
                point,
                None,
                Modifiers {
                    shift: true,
                    alt: true,
                    control: true,
                    ..Default::default()
                },
                mode
            ),
            Some(b"\x1b[<63;5;3M".to_vec())
        );
    }
    #[test]
    fn normal_utf8_scroll_and_alt_scroll() {
        let mode = crate::ModeSnapshot {
            mouse_click: true,
            ..Default::default()
        };
        assert_eq!(
            mouse_button_report(
                GridPoint { row: 0, column: 0 },
                MouseButton::Left,
                Modifiers::default(),
                true,
                mode
            ),
            Some(vec![27, 91, 77, 32, 33, 33])
        );
        assert_eq!(
            mouse_button_report(
                GridPoint { row: 0, column: 0 },
                MouseButton::Left,
                Modifiers::default(),
                false,
                mode
            ),
            Some(vec![27, 91, 77, 35, 33, 33])
        );
        let utf8 = crate::ModeSnapshot {
            mouse_click: true,
            utf8_mouse: true,
            ..Default::default()
        };
        assert_eq!(
            mouse_button_report(
                GridPoint {
                    row: 95,
                    column: 95
                },
                MouseButton::Left,
                Modifiers::default(),
                true,
                utf8
            ),
            Some(vec![27, 91, 77, 32, 0xc2, 0x80, 0xc2, 0x80])
        );
        assert_eq!(
            scroll_report(
                GridPoint { row: 0, column: 0 },
                -2,
                Modifiers::default(),
                sgr()
            ),
            Some(b"\x1b[<65;1;1M\x1b[<65;1;1M".to_vec())
        );
        assert_eq!(alt_scroll(2), b"\x1bOA\x1bOA");
        assert_eq!(alt_scroll(-1), b"\x1bOB");
    }
    #[test]
    fn key_ctrl_caret_and_application_cursor() {
        let none = crate::ModeSnapshot::default();
        assert_eq!(
            key_bytes(
                "c",
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                none,
                true
            ),
            Some(vec![3])
        );
        assert_eq!(
            key_bytes(
                "?",
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                none,
                true
            ),
            Some(vec![127])
        );
        assert_eq!(
            key_bytes("up", Modifiers::default(), none, true),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            key_bytes(
                "up",
                Modifiers::default(),
                crate::ModeSnapshot {
                    app_cursor: true,
                    ..none
                },
                true
            ),
            Some(b"\x1bOA".to_vec())
        );
    }
    #[test]
    fn key_function_keys_and_modifiers() {
        let none = crate::ModeSnapshot::default();
        assert_eq!(
            key_bytes("f1", Modifiers::default(), none, true),
            Some(b"\x1bOP".to_vec())
        );
        assert_eq!(
            key_bytes("f20", Modifiers::default(), none, true),
            Some(b"\x1b[34~".to_vec())
        );
        assert_eq!(
            key_bytes(
                "up",
                Modifiers {
                    shift: true,
                    control: true,
                    ..Default::default()
                },
                none,
                true
            ),
            Some(b"\x1b[1;6A".to_vec())
        );
        assert_eq!(
            key_bytes(
                "f5",
                Modifiers {
                    alt: true,
                    ..Default::default()
                },
                none,
                true
            ),
            Some(b"\x1b[15;3~".to_vec())
        );
    }
}
