use std::mem::size_of;
use std::time::Duration;

use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use super::super::{BackendError, BackendErrorCode, KeyModifiers, MouseButton, parse_key_chord};

pub(super) fn click(
    x: f64,
    y: f64,
    button: MouseButton,
    click_count: u32,
) -> Result<(), BackendError> {
    if !x.is_finite() || !y.is_finite() {
        return Err(invalid("click coordinates must be finite"));
    }
    if !(1..=3).contains(&click_count) {
        return Err(invalid("click_count must be between 1 and 3"));
    }
    move_mouse(x, y)?;
    let (down, up) = mouse_button_flags(button);
    for click_index in 0..click_count {
        let up_event = mouse_event(up, 0);
        if let Err(error) = send_input(&[mouse_event(down, 0), up_event]) {
            let _ = send_input(&[up_event]);
            return Err(error);
        }
        if click_index + 1 < click_count {
            std::thread::sleep(Duration::from_millis(45));
        }
    }
    Ok(())
}

pub(super) fn move_mouse(x: f64, y: f64) -> Result<(), BackendError> {
    send_input(&[absolute_move_event(x, y)?])
}

pub(super) fn scroll(x: f64, y: f64) -> Result<(), BackendError> {
    if !x.is_finite() || !y.is_finite() {
        return Err(invalid("scroll deltas must be finite"));
    }
    let mut inputs = Vec::with_capacity(2);
    if y != 0.0 {
        inputs.push(mouse_event(MOUSEEVENTF_WHEEL, wheel_data(y)));
    }
    if x != 0.0 {
        inputs.push(mouse_event(MOUSEEVENTF_HWHEEL, wheel_data(x)));
    }
    send_input(&inputs)
}

pub(super) fn drag(path: &[[f64; 2]], button: MouseButton) -> Result<(), BackendError> {
    if path.len() < 2 {
        return Err(invalid("drag requires at least two path points"));
    }
    if path
        .iter()
        .flatten()
        .any(|coordinate| !coordinate.is_finite())
    {
        return Err(invalid("drag path coordinates must be finite"));
    }
    let moves = path
        .iter()
        .map(|point| absolute_move_event(point[0], point[1]))
        .collect::<Result<Vec<_>, _>>()?;
    let (down, up) = mouse_button_flags(button);
    let first = moves
        .first()
        .copied()
        .ok_or_else(|| invalid("drag requires at least two path points"))?;
    send_input(&[first])?;
    send_input(&[mouse_event(down, 0)])?;
    for move_event in &moves[1..] {
        if let Err(error) = send_input(&[*move_event]) {
            let _ = send_input(&[mouse_event(up, 0)]);
            return Err(error);
        }
        std::thread::sleep(Duration::from_millis(12));
    }
    send_input(&[mouse_event(up, 0)])
}

pub(super) fn keypress(keys: &[String]) -> Result<(), BackendError> {
    let chord = parse_key_chord(keys).map_err(invalid)?;
    if chord.modifiers.function {
        return Err(invalid(
            "the fn modifier has no Windows virtual-key equivalent",
        ));
    }
    let modifiers = modifier_keycodes(chord.modifiers);
    let mut inputs = Vec::with_capacity(modifiers.len() * 2 + 2);
    inputs.extend(modifiers.iter().map(|key| key_event(*key, false)));
    inputs.push(key_event(chord.keycode, false));
    inputs.push(key_event(chord.keycode, true));
    inputs.extend(modifiers.iter().rev().map(|key| key_event(*key, true)));
    if let Err(error) = send_input(&inputs) {
        let mut releases = vec![key_event(chord.keycode, true)];
        releases.extend(modifiers.iter().rev().map(|key| key_event(*key, true)));
        let _ = send_input(&releases);
        return Err(error);
    }
    Ok(())
}

pub(super) fn type_text(text: &str) -> Result<(), BackendError> {
    for chunk in unicode_chunks(text, 32) {
        let mut inputs = Vec::with_capacity(chunk.len() * 2);
        for unit in chunk {
            inputs.push(unicode_event(unit, false));
            inputs.push(unicode_event(unit, true));
        }
        send_input(&inputs)?;
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

fn unicode_chunks(text: &str, maximum_units: usize) -> Vec<Vec<u16>> {
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    for character in text.chars() {
        let mut encoded = [0_u16; 2];
        let units = character.encode_utf16(&mut encoded);
        if !current.is_empty() && current.len() + units.len() > maximum_units {
            chunks.push(std::mem::take(&mut current));
        }
        current.extend_from_slice(units);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn absolute_move_event(x: f64, y: f64) -> Result<INPUT, BackendError> {
    if !x.is_finite() || !y.is_finite() {
        return Err(invalid("mouse coordinates must be finite"));
    }
    let left = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let top = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    if width <= 0 || height <= 0 {
        return Err(operation(
            "GetSystemMetrics returned an empty virtual desktop",
        ));
    }
    Ok(INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: normalize_absolute(x, left, width),
                dy: normalize_absolute(y, top, height),
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    })
}

fn normalize_absolute(value: f64, origin: i32, extent: i32) -> i32 {
    if extent <= 1 {
        return 0;
    }
    (((value - f64::from(origin)) * 65_535.0 / f64::from(extent - 1))
        .round()
        .clamp(0.0, 65_535.0)) as i32
}

fn mouse_event(flags: MOUSE_EVENT_FLAGS, data: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                mouseData: data,
                dwFlags: flags,
                ..MOUSEINPUT::default()
            },
        },
    }
}

fn mouse_button_flags(button: MouseButton) -> (MOUSE_EVENT_FLAGS, MOUSE_EVENT_FLAGS) {
    match button {
        MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        MouseButton::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
    }
}

fn wheel_data(value: f64) -> u32 {
    (value.round() as i32) as u32
}

fn modifier_keycodes(modifiers: KeyModifiers) -> Vec<u16> {
    let mut keys = Vec::with_capacity(4);
    if modifiers.command {
        keys.push(0x5B); // VK_LWIN
    }
    if modifiers.control {
        keys.push(0xA2); // VK_LCONTROL
    }
    if modifiers.option {
        keys.push(0xA4); // VK_LMENU (Alt)
    }
    if modifiers.shift {
        keys.push(0xA0); // VK_LSHIFT
    }
    keys
}

fn key_event(keycode: u16, key_up: bool) -> INPUT {
    let mut flags = if is_extended_key(keycode) {
        KEYEVENTF_EXTENDEDKEY
    } else {
        Default::default()
    };
    if key_up {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(keycode),
                dwFlags: flags,
                ..KEYBDINPUT::default()
            },
        },
    }
}

fn unicode_event(unit: u16, key_up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wScan: unit,
                dwFlags: if key_up {
                    KEYEVENTF_UNICODE | KEYEVENTF_KEYUP
                } else {
                    KEYEVENTF_UNICODE
                },
                ..KEYBDINPUT::default()
            },
        },
    }
}

fn is_extended_key(keycode: u16) -> bool {
    matches!(keycode, 0x21..=0x2E | 0x5B | 0x5C)
}

fn send_input(inputs: &[INPUT]) -> Result<(), BackendError> {
    if inputs.is_empty() {
        return Ok(());
    }
    let size = i32::try_from(size_of::<INPUT>())
        .map_err(|_| operation("Windows INPUT structure size does not fit i32"))?;
    let inserted = unsafe { SendInput(inputs, size) };
    if inserted as usize == inputs.len() {
        Ok(())
    } else {
        let error = windows::core::Error::from_win32();
        Err(operation(format!(
            "SendInput inserted {inserted} of {} events: {error}",
            inputs.len()
        )))
    }
}

fn invalid(message: impl Into<String>) -> BackendError {
    BackendError::new(BackendErrorCode::InvalidAction, message)
}

fn operation(message: impl Into<String>) -> BackendError {
    BackendError::new(BackendErrorCode::OperationFailed, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_chunks_do_not_split_surrogate_pairs() {
        let chunks = unicode_chunks("1234567890123456789012345678901🦀x", 32);
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|chunk| chunk.len() <= 32));
        let decoded: String = chunks
            .into_iter()
            .flat_map(|chunk| char::decode_utf16(chunk).map(Result::unwrap))
            .collect();
        assert_eq!(decoded, "1234567890123456789012345678901🦀x");
    }
}
