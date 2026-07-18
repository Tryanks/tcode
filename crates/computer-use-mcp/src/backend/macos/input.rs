use std::time::Duration;

use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, EventField,
    ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;

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
    let point = CGPoint::new(x, y);
    let (down, up, cg_button) = mouse_types(button);
    for click_index in 0..click_count {
        let down_event = mouse_event(down, point, cg_button)?;
        down_event.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, click_count.into());
        down_event.post(CGEventTapLocation::HID);
        let up_event = mouse_event(up, point, cg_button)?;
        up_event.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, click_count.into());
        up_event.post(CGEventTapLocation::HID);
        if click_index + 1 < click_count {
            std::thread::sleep(Duration::from_millis(45));
        }
    }
    Ok(())
}

pub(super) fn move_mouse(x: f64, y: f64) -> Result<(), BackendError> {
    if !x.is_finite() || !y.is_finite() {
        return Err(invalid("mouse coordinates must be finite"));
    }
    mouse_event(
        CGEventType::MouseMoved,
        CGPoint::new(x, y),
        CGMouseButton::Left,
    )?
    .post(CGEventTapLocation::HID);
    Ok(())
}

pub(super) fn scroll(x: f64, y: f64) -> Result<(), BackendError> {
    if !x.is_finite() || !y.is_finite() {
        return Err(invalid("scroll deltas must be finite"));
    }
    let event = CGEvent::new_scroll_event(
        event_source()?,
        ScrollEventUnit::PIXEL,
        2,
        y.round() as i32,
        x.round() as i32,
        0,
    )
    .map_err(|()| operation("CoreGraphics could not create a scroll event"))?;
    event.post(CGEventTapLocation::HID);
    Ok(())
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
    let (_, up, cg_button) = mouse_types(button);
    let down = match button {
        MouseButton::Left => CGEventType::LeftMouseDown,
        MouseButton::Right => CGEventType::RightMouseDown,
        MouseButton::Middle => CGEventType::OtherMouseDown,
    };
    let dragged = match button {
        MouseButton::Left => CGEventType::LeftMouseDragged,
        MouseButton::Right => CGEventType::RightMouseDragged,
        MouseButton::Middle => CGEventType::OtherMouseDragged,
    };
    let first = CGPoint::new(path[0][0], path[0][1]);
    mouse_event(down, first, cg_button)?.post(CGEventTapLocation::HID);
    for point in &path[1..path.len() - 1] {
        mouse_event(dragged, CGPoint::new(point[0], point[1]), cg_button)?
            .post(CGEventTapLocation::HID);
        std::thread::sleep(Duration::from_millis(12));
    }
    let last = path[path.len() - 1];
    let last = CGPoint::new(last[0], last[1]);
    mouse_event(dragged, last, cg_button)?.post(CGEventTapLocation::HID);
    mouse_event(up, last, cg_button)?.post(CGEventTapLocation::HID);
    Ok(())
}

pub(super) fn keypress(keys: &[String]) -> Result<(), BackendError> {
    let chord = parse_key_chord(keys).map_err(invalid)?;
    let flags = modifier_flags(chord.modifiers);
    let down = CGEvent::new_keyboard_event(event_source()?, chord.keycode, true)
        .map_err(|()| operation("CoreGraphics could not create a key-down event"))?;
    down.set_flags(flags);
    down.post(CGEventTapLocation::HID);
    let up = CGEvent::new_keyboard_event(event_source()?, chord.keycode, false)
        .map_err(|()| operation("CoreGraphics could not create a key-up event"))?;
    up.set_flags(flags);
    up.post(CGEventTapLocation::HID);
    Ok(())
}

pub(super) fn type_text(text: &str) -> Result<(), BackendError> {
    let chunks = unicode_chunks(text, 20);
    for chunk in chunks {
        let down = CGEvent::new_keyboard_event(event_source()?, 0, true)
            .map_err(|()| operation("CoreGraphics could not create a Unicode key-down event"))?;
        down.set_flags(CGEventFlags::CGEventFlagNull);
        down.set_string_from_utf16_unchecked(&chunk);
        down.post(CGEventTapLocation::HID);
        let up = CGEvent::new_keyboard_event(event_source()?, 0, false)
            .map_err(|()| operation("CoreGraphics could not create a Unicode key-up event"))?;
        up.set_flags(CGEventFlags::CGEventFlagNull);
        up.set_string_from_utf16_unchecked(&chunk);
        up.post(CGEventTapLocation::HID);
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

fn event_source() -> Result<CGEventSource, BackendError> {
    CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|()| operation("CoreGraphics could not create an event source"))
}

fn mouse_event(
    event_type: CGEventType,
    point: CGPoint,
    button: CGMouseButton,
) -> Result<CGEvent, BackendError> {
    CGEvent::new_mouse_event(event_source()?, event_type, point, button)
        .map_err(|()| operation("CoreGraphics could not create a mouse event"))
}

fn mouse_types(button: MouseButton) -> (CGEventType, CGEventType, CGMouseButton) {
    match button {
        MouseButton::Left => (
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseUp,
            CGMouseButton::Left,
        ),
        MouseButton::Right => (
            CGEventType::RightMouseDown,
            CGEventType::RightMouseUp,
            CGMouseButton::Right,
        ),
        MouseButton::Middle => (
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
            CGMouseButton::Center,
        ),
    }
}

fn modifier_flags(modifiers: KeyModifiers) -> CGEventFlags {
    let mut flags = CGEventFlags::CGEventFlagNull;
    if modifiers.command {
        flags |= CGEventFlags::CGEventFlagCommand;
    }
    if modifiers.control {
        flags |= CGEventFlags::CGEventFlagControl;
    }
    if modifiers.option {
        flags |= CGEventFlags::CGEventFlagAlternate;
    }
    if modifiers.shift {
        flags |= CGEventFlags::CGEventFlagShift;
    }
    if modifiers.function {
        flags |= CGEventFlags::CGEventFlagSecondaryFn;
    }
    flags
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
        let chunks = unicode_chunks("1234567890123456789🦀x", 20);
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|chunk| chunk.len() <= 20));
        let decoded: String = chunks
            .into_iter()
            .flat_map(|chunk| char::decode_utf16(chunk).map(Result::unwrap))
            .collect();
        assert_eq!(decoded, "1234567890123456789🦀x");
    }
}
