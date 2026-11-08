//! Keys by name, as evdev codes on the US layout the nest's keyboard uses.

/// Modifier keys, as evdev codes.
pub mod modifier {
    pub const SHIFT: u32 = 42;
    pub const CONTROL: u32 = 29;
    pub const ALT: u32 = 56;
    pub const SUPER: u32 = 125;
}

/// The evdev code that types `key` on a US layout, and whether Shift must be
/// held for it. `key` is a character (`"a"`, `"A"`, `"!"`) or a key name in
/// GPUI's spelling (`"enter"`, `"pageup"`, `"f5"`).
pub fn evdev_key(key: &str) -> Option<(u32, bool)> {
    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return character(c);
    }
    let code = match key {
        "escape" => 1,
        "backspace" => 14,
        "tab" => 15,
        "enter" => 28,
        "space" => 57,
        "capslock" => 58,
        "home" => 102,
        "up" => 103,
        "pageup" => 104,
        "left" => 105,
        "right" => 106,
        "end" => 107,
        "down" => 108,
        "pagedown" => 109,
        "insert" => 110,
        "delete" => 111,
        "menu" => 139,
        _ => return function_key(key).map(|code| (code, false)),
    };
    Some((code, false))
}

fn function_key(key: &str) -> Option<u32> {
    let n: u32 = key.strip_prefix('f')?.parse().ok()?;
    match n {
        1..=10 => Some(58 + n),
        11 => Some(87),
        12 => Some(88),
        _ => None,
    }
}

fn character(c: char) -> Option<(u32, bool)> {
    const LETTERS: [u32; 26] = [
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17,
        45, 21, 44,
    ];
    if c.is_ascii_lowercase() {
        return Some((LETTERS[(c as u8 - b'a') as usize], false));
    }
    if c.is_ascii_uppercase() {
        return Some((LETTERS[(c as u8 - b'A') as usize], true));
    }
    let key = match c {
        '1'..='9' => (c as u32 - '1' as u32 + 2, false),
        '0' => (11, false),
        '-' => (12, false),
        '=' => (13, false),
        '[' => (26, false),
        ']' => (27, false),
        ';' => (39, false),
        '\'' => (40, false),
        '`' => (41, false),
        '\\' => (43, false),
        ',' => (51, false),
        '.' => (52, false),
        '/' => (53, false),
        ' ' => (57, false),
        '!' => (2, true),
        '@' => (3, true),
        '#' => (4, true),
        '$' => (5, true),
        '%' => (6, true),
        '^' => (7, true),
        '&' => (8, true),
        '*' => (9, true),
        '(' => (10, true),
        ')' => (11, true),
        '_' => (12, true),
        '+' => (13, true),
        '{' => (26, true),
        '}' => (27, true),
        ':' => (39, true),
        '"' => (40, true),
        '~' => (41, true),
        '|' => (43, true),
        '<' => (51, true),
        '>' => (52, true),
        '?' => (53, true),
        _ => return None,
    };
    Some(key)
}
