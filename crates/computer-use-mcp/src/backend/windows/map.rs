use crate::backend::{KeyChord, MouseButton};

const WM_KEYDOWN: u32 = 0x0100;
const WM_KEYUP: u32 = 0x0101;
const WM_CHAR: u32 = 0x0102;
const WM_SYSKEYDOWN: u32 = 0x0104;
const WM_SYSKEYUP: u32 = 0x0105;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_MOUSEWHEEL: u32 = 0x020A;
const WM_MOUSEHWHEEL: u32 = 0x020E;

const VK_SHIFT: u16 = 0x10;
const VK_CONTROL: u16 = 0x11;
const VK_MENU: u16 = 0x12;
const VK_LWIN: u16 = 0x5B;

/// One window message for `PostMessageW`, built without touching the system
/// cursor or the global input stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PostedMessage {
    pub(super) message: u32,
    pub(super) wparam: usize,
    pub(super) lparam: isize,
}

impl PostedMessage {
    fn new(message: u32, wparam: usize, lparam: isize) -> Self {
        Self {
            message,
            wparam,
            lparam,
        }
    }
}

/// `MAKELPARAM` of a client- or screen-coordinate point; each coordinate is a
/// signed 16-bit word, so points left of or above the origin stay negative.
pub(super) fn point_lparam((x, y): (i32, i32)) -> isize {
    let low = u32::from(x as i16 as u16);
    let high = u32::from(y as i16 as u16);
    ((high << 16) | low) as i32 as isize
}

/// (down, up, double-click, `MK_*` key-state flag) for a button.
fn button_messages(button: MouseButton) -> (u32, u32, u32, usize) {
    match button {
        MouseButton::Left => (0x0201, 0x0202, 0x0203, 0x0001),
        MouseButton::Right => (0x0204, 0x0205, 0x0206, 0x0002),
        MouseButton::Middle => (0x0207, 0x0208, 0x0209, 0x0010),
    }
}

/// A hover followed by `click_count` presses at one client point. Windows
/// turns a second press into a double-click message only for window classes
/// with `CS_DBLCLKS`, so a posted sequence does the same; a third press is a
/// plain press again.
pub(super) fn click_messages(
    client: (i32, i32),
    button: MouseButton,
    click_count: u32,
    class_double_clicks: bool,
) -> Vec<PostedMessage> {
    let (down, up, double, held) = button_messages(button);
    let lparam = point_lparam(client);
    let mut messages = vec![hover_message(client)];
    for index in 0..click_count {
        let press = if index % 2 == 1 && class_double_clicks {
            double
        } else {
            down
        };
        messages.push(PostedMessage::new(press, held, lparam));
        messages.push(PostedMessage::new(up, 0, lparam));
    }
    messages
}

pub(super) fn hover_message(client: (i32, i32)) -> PostedMessage {
    PostedMessage::new(WM_MOUSEMOVE, 0, point_lparam(client))
}

/// A press at the first client point, moves holding the button through the
/// rest, and the release at the last.
pub(super) fn drag_messages(path: &[(i32, i32)], button: MouseButton) -> Vec<PostedMessage> {
    let (Some(&first), Some(&last)) = (path.first(), path.last()) else {
        return Vec::new();
    };
    let (down, up, _, held) = button_messages(button);
    let mut messages = vec![
        hover_message(first),
        PostedMessage::new(down, held, point_lparam(first)),
    ];
    messages.extend(
        path[1..]
            .iter()
            .map(|&point| PostedMessage::new(WM_MOUSEMOVE, held, point_lparam(point))),
    );
    messages.push(PostedMessage::new(up, 0, point_lparam(last)));
    messages
}

/// Wheel messages carry screen coordinates. A positive `y` scrolls up and a
/// positive `x` scrolls right, in wheel units of 120 per notch.
pub(super) fn wheel_messages(screen: (i32, i32), x: f64, y: f64) -> Vec<PostedMessage> {
    let lparam = point_lparam(screen);
    let delta = |value: f64| {
        let delta = value
            .round()
            .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16;
        usize::from(delta as u16) << 16
    };
    let mut messages = Vec::new();
    if y != 0.0 {
        messages.push(PostedMessage::new(WM_MOUSEWHEEL, delta(y), lparam));
    }
    if x != 0.0 {
        messages.push(PostedMessage::new(WM_MOUSEHWHEEL, delta(x), lparam));
    }
    messages
}

/// The key messages a physical chord produces: modifiers down in order, the
/// key down and up, and modifiers up in reverse. While Alt is held without
/// Ctrl they are the `WM_SYS*` variants with the context bit set.
/// `scan_code` maps a virtual key to its scan code.
///
/// Posting leaves the target thread's key state untouched, so an application
/// that reads modifiers with `GetKeyState` sees the key alone.
pub(super) fn key_messages(
    chord: KeyChord,
    scan_code: impl Fn(u16) -> u32,
) -> Result<Vec<PostedMessage>, String> {
    if chord.modifiers.function {
        return Err("the fn modifier has no Windows virtual-key equivalent".into());
    }
    let modifiers: Vec<u16> = [
        (chord.modifiers.command, VK_LWIN),
        (chord.modifiers.control, VK_CONTROL),
        (chord.modifiers.option, VK_MENU),
        (chord.modifiers.shift, VK_SHIFT),
    ]
    .into_iter()
    .filter_map(|(held, key)| held.then_some(key))
    .collect();
    let system = chord.modifiers.option && !chord.modifiers.control;
    let message = |key: u16, released: bool| {
        let mut lparam = 1_u32 | ((scan_code(key) & 0xFF) << 16);
        if is_extended_key(key) {
            lparam |= 1 << 24;
        }
        if system {
            lparam |= 1 << 29;
        }
        if released {
            lparam |= 3 << 30;
        }
        let message = match (system, released) {
            (false, false) => WM_KEYDOWN,
            (false, true) => WM_KEYUP,
            (true, false) => WM_SYSKEYDOWN,
            (true, true) => WM_SYSKEYUP,
        };
        PostedMessage::new(message, usize::from(key), lparam as i32 as isize)
    };
    let mut messages: Vec<PostedMessage> =
        modifiers.iter().map(|&key| message(key, false)).collect();
    messages.push(message(chord.keycode, false));
    messages.push(message(chord.keycode, true));
    messages.extend(modifiers.iter().rev().map(|&key| message(key, true)));
    Ok(messages)
}

fn is_extended_key(key: u16) -> bool {
    matches!(key, 0x21..=0x28 | 0x2D | 0x2E | VK_LWIN)
}

/// One `WM_CHAR` per UTF-16 unit, so characters outside the BMP arrive as
/// their surrogate pair. A line break is the carriage return Enter types.
pub(super) fn char_messages(text: &str) -> Vec<PostedMessage> {
    text.replace("\r\n", "\r")
        .replace('\n', "\r")
        .encode_utf16()
        .map(|unit| PostedMessage::new(WM_CHAR, usize::from(unit), 1))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::KeyModifiers;

    #[test]
    fn posted_clicks_follow_the_window_class_double_click_rule() {
        let lparam = point_lparam((30, 40));
        let messages = |button, count, double| {
            click_messages((30, 40), button, count, double)
                .into_iter()
                .map(|message| (message.message, message.wparam, message.lparam))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            messages(MouseButton::Left, 2, true),
            vec![
                (0x0200, 0, lparam),
                (0x0201, 1, lparam),
                (0x0202, 0, lparam),
                (0x0203, 1, lparam),
                (0x0202, 0, lparam),
            ]
        );
        assert_eq!(
            messages(MouseButton::Left, 3, false)[1..]
                .iter()
                .map(|message| message.0)
                .collect::<Vec<_>>(),
            vec![0x0201, 0x0202, 0x0201, 0x0202, 0x0201, 0x0202]
        );
        assert_eq!(
            messages(MouseButton::Right, 1, true)[1..],
            [(0x0204, 2, lparam), (0x0205, 0, lparam)]
        );
    }

    #[test]
    fn message_points_pack_signed_words() {
        assert_eq!(point_lparam((30, 40)), (40 << 16) | 30);
        let packed = point_lparam((-5, -7)) as u32;
        assert_eq!((packed & 0xFFFF) as u16 as i16, -5);
        assert_eq!((packed >> 16) as u16 as i16, -7);
    }

    #[test]
    fn posted_drags_hold_the_button_from_first_point_to_last() {
        let path = [(0, 0), (10, 5), (20, 10)];
        let messages: Vec<_> = drag_messages(&path, MouseButton::Left)
            .into_iter()
            .map(|message| (message.message, message.wparam, message.lparam))
            .collect();
        assert_eq!(
            messages,
            vec![
                (0x0200, 0, point_lparam((0, 0))),
                (0x0201, 1, point_lparam((0, 0))),
                (0x0200, 1, point_lparam((10, 5))),
                (0x0200, 1, point_lparam((20, 10))),
                (0x0202, 0, point_lparam((20, 10))),
            ]
        );
    }

    #[test]
    fn wheel_deltas_ride_in_the_high_word_with_screen_coordinates() {
        let messages = wheel_messages((100, 200), 60.0, -240.0);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].message, 0x020A);
        assert_eq!((messages[0].wparam >> 16) as u16 as i16, -240);
        assert_eq!(messages[0].lparam, point_lparam((100, 200)));
        assert_eq!(messages[1].message, 0x020E);
        assert_eq!((messages[1].wparam >> 16) as u16 as i16, 60);
        assert!(wheel_messages((0, 0), 0.0, 0.0).is_empty());
    }

    #[test]
    fn chords_post_modifier_downs_the_key_and_reverse_ups() {
        let scan = |key: u16| u32::from(key) + 1;
        let chord = |keycode, modifiers| KeyChord { keycode, modifiers };
        let messages = key_messages(
            chord(
                0x53,
                KeyModifiers {
                    control: true,
                    shift: true,
                    ..KeyModifiers::default()
                },
            ),
            scan,
        )
        .unwrap();
        assert_eq!(
            messages
                .iter()
                .map(|message| (message.message, message.wparam))
                .collect::<Vec<_>>(),
            vec![
                (0x0100, 0x11),
                (0x0100, 0x10),
                (0x0100, 0x53),
                (0x0101, 0x53),
                (0x0101, 0x10),
                (0x0101, 0x11),
            ]
        );
        assert_eq!(messages[2].lparam, 1 | (0x54 << 16));
        assert_eq!(messages[3].lparam as u32, 1 | (0x54 << 16) | (3 << 30));

        let alt_f4 = key_messages(
            chord(
                0x73,
                KeyModifiers {
                    option: true,
                    ..KeyModifiers::default()
                },
            ),
            scan,
        )
        .unwrap();
        assert_eq!(
            alt_f4
                .iter()
                .map(|message| message.message)
                .collect::<Vec<_>>(),
            vec![0x0104, 0x0104, 0x0105, 0x0105]
        );
        assert!(alt_f4.iter().all(|message| message.lparam & (1 << 29) != 0));

        let left = key_messages(chord(0x25, KeyModifiers::default()), scan).unwrap();
        assert!(left[0].lparam & (1 << 24) != 0, "arrows are extended keys");
        assert!(
            key_messages(
                chord(
                    0x53,
                    KeyModifiers {
                        function: true,
                        ..KeyModifiers::default()
                    }
                ),
                scan
            )
            .is_err()
        );
    }

    #[test]
    fn text_posts_one_char_per_utf16_unit_and_enter_for_line_breaks() {
        let units: Vec<usize> = char_messages("a🦀\r\nb\n")
            .into_iter()
            .inspect(|message| assert_eq!((message.message, message.lparam), (0x0102, 1)))
            .map(|message| message.wparam)
            .collect();
        assert_eq!(units, vec![0x61, 0xD83E, 0xDD80, 0x0D, 0x62, 0x0D]);
    }
}
