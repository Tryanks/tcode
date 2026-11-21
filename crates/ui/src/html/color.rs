use gpui::{Hsla, Rgba};

use super::css::{MAX_NESTING, number};

/// Parsed CSS color that may still depend on the inherited `color` property.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum CssColor {
    Exact(Hsla),
    CurrentColor,
}

impl CssColor {
    pub(crate) fn resolve(self, current: Hsla) -> Hsla {
        match self {
            Self::Exact(color) => color,
            Self::CurrentColor => current,
        }
    }
}

/// Format an `Hsla` color as CSS `rgb(...)` / `rgba(...)` for injecting into SVG.
pub(crate) fn hsla_to_css(color: Hsla) -> String {
    let rgba: Rgba = color.into();
    let r = (rgba.r.clamp(0.0, 1.0) * 255.0).round() as u8;
    let g = (rgba.g.clamp(0.0, 1.0) * 255.0).round() as u8;
    let b = (rgba.b.clamp(0.0, 1.0) * 255.0).round() as u8;
    let a = rgba.a.clamp(0.0, 1.0);
    if (a - 1.0).abs() < 0.002 {
        format!("#{r:02x}{g:02x}{b:02x}")
    } else {
        format!("rgba({r},{g},{b},{a:.3})")
    }
}

/// Parse a CSS color string into [`CssColor`] for the active light/dark appearance.
pub(crate) fn parse_color_for_mode(raw: &str, dark: bool) -> Option<CssColor> {
    parse_nested_color(raw, dark, 0)
}

/// `light-dark()` and `color-mix()` take colours, which may be either again.
fn parse_nested_color(raw: &str, dark: bool, depth: usize) -> Option<CssColor> {
    if depth > MAX_NESTING {
        return None;
    }
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if s.eq_ignore_ascii_case("currentcolor") {
        return Some(CssColor::CurrentColor);
    }
    if s.eq_ignore_ascii_case("transparent") {
        return Some(CssColor::Exact(Hsla::transparent_black()));
    }
    if let Some(hex) = s.strip_prefix('#') {
        return parse_hex(hex).map(CssColor::Exact);
    }
    let lower = s.to_ascii_lowercase();
    if let Some(inner) = strip_func(&lower, "light-dark") {
        let parts = split_top_level_commas(inner);
        if parts.len() == 2 {
            let chosen = if dark { parts[1] } else { parts[0] };
            return parse_nested_color(chosen, dark, depth + 1);
        }
        return None;
    }
    if let Some(inner) = strip_func(&lower, "rgba").or_else(|| strip_func(&lower, "rgb")) {
        return parse_rgb(inner).map(CssColor::Exact);
    }
    if let Some(inner) = strip_func(&lower, "hsla").or_else(|| strip_func(&lower, "hsl")) {
        return parse_hsl(inner).map(CssColor::Exact);
    }
    if let Some(inner) = strip_func(&lower, "color-mix") {
        return parse_color_mix(inner, dark, depth);
    }
    named_color(&lower).map(|rgb| CssColor::Exact(rgb_u32(rgb, 1.0)))
}

fn strip_func<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let rest = s.strip_prefix(name)?.trim_start();
    let inner = rest.strip_prefix('(')?.strip_suffix(')')?;
    Some(inner.trim())
}

fn rgb_u32(hex: u32, alpha: f32) -> Hsla {
    let r = ((hex >> 16) & 0xff) as f32 / 255.0;
    let g = ((hex >> 8) & 0xff) as f32 / 255.0;
    let b = (hex & 0xff) as f32 / 255.0;
    Rgba { r, g, b, a: alpha }.into()
}

fn parse_hex(hex: &str) -> Option<Hsla> {
    let bytes = hex.as_bytes();
    let hex_val = |b: u8| -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    };
    let pair = |hi: u8, lo: u8| -> Option<f32> {
        Some(((hex_val(hi)? << 4) | hex_val(lo)?) as f32 / 255.0)
    };
    let dup = |c: u8| -> Option<f32> {
        let v = hex_val(c)?;
        Some(((v << 4) | v) as f32 / 255.0)
    };
    let (r, g, b, a) = match bytes.len() {
        3 => (dup(bytes[0])?, dup(bytes[1])?, dup(bytes[2])?, 1.0),
        4 => (
            dup(bytes[0])?,
            dup(bytes[1])?,
            dup(bytes[2])?,
            dup(bytes[3])?,
        ),
        6 => (
            pair(bytes[0], bytes[1])?,
            pair(bytes[2], bytes[3])?,
            pair(bytes[4], bytes[5])?,
            1.0,
        ),
        8 => (
            pair(bytes[0], bytes[1])?,
            pair(bytes[2], bytes[3])?,
            pair(bytes[4], bytes[5])?,
            pair(bytes[6], bytes[7])?,
        ),
        _ => return None,
    };
    Some(Rgba { r, g, b, a }.into())
}

fn parse_rgb(inner: &str) -> Option<Hsla> {
    let (channels, alpha) = split_channels_and_alpha(inner);
    if channels.len() < 3 {
        return None;
    }
    let parse_channel = |token: &str| -> Option<f32> {
        let t = token.trim();
        if let Some(pct) = t.strip_suffix('%') {
            Some((number(pct)? / 100.0).clamp(0.0, 1.0))
        } else {
            Some((number(t)? / 255.0).clamp(0.0, 1.0))
        }
    };
    let r = parse_channel(channels[0])?;
    let g = parse_channel(channels[1])?;
    let b = parse_channel(channels[2])?;
    let a = match alpha.or_else(|| channels.get(3).copied()) {
        Some(tok) => parse_alpha(tok)?,
        None => 1.0,
    };
    Some(Rgba { r, g, b, a }.into())
}

fn parse_hsl(inner: &str) -> Option<Hsla> {
    let (channels, alpha) = split_channels_and_alpha(inner);
    if channels.len() < 3 {
        return None;
    }
    let h = parse_angle_deg(channels[0])?.rem_euclid(360.0) / 360.0;
    if !h.is_finite() {
        return None;
    }
    let s = parse_percent_unit(channels[1])?;
    let l = parse_percent_unit(channels[2])?;
    let a = match alpha.or_else(|| channels.get(3).copied()) {
        Some(tok) => parse_alpha(tok)?,
        None => 1.0,
    };
    Some(Hsla { h, s, l, a })
}

fn split_channels_and_alpha(inner: &str) -> (Vec<&str>, Option<&str>) {
    if let Some((left, right)) = inner.split_once('/') {
        let chans = left
            .split(|c: char| c.is_ascii_whitespace() || c == ',')
            .filter(|t| !t.is_empty())
            .collect();
        (chans, Some(right.trim()))
    } else if inner.contains(',') {
        let chans = inner
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .collect();
        (chans, None)
    } else {
        let chans = inner.split_whitespace().collect();
        (chans, None)
    }
}

fn parse_angle_deg(raw: &str) -> Option<f32> {
    let t = raw.trim();
    if let Some(v) = t.strip_suffix("deg") {
        number(v)
    } else if let Some(v) = t.strip_suffix("turn") {
        number(v).map(|x| x * 360.0)
    } else if let Some(v) = t.strip_suffix("grad") {
        number(v).map(|x| x * 0.9)
    } else if let Some(v) = t.strip_suffix("rad") {
        number(v).map(f32::to_degrees)
    } else {
        number(t)
    }
}

fn parse_percent_unit(raw: &str) -> Option<f32> {
    let t = raw.trim();
    if let Some(v) = t.strip_suffix('%') {
        Some((number(v)? / 100.0).clamp(0.0, 1.0))
    } else {
        Some((number(t)? / 100.0).clamp(0.0, 1.0))
    }
}

fn parse_alpha(raw: &str) -> Option<f32> {
    let t = raw.trim();
    if let Some(v) = t.strip_suffix('%') {
        Some((number(v)? / 100.0).clamp(0.0, 1.0))
    } else {
        Some(number(t)?.clamp(0.0, 1.0))
    }
}

/// Parse `color-mix(in srgb, <color1> <pct1>?, <color2> <pct2>?)`.
fn parse_color_mix(inner: &str, dark: bool, depth: usize) -> Option<CssColor> {
    let parts = split_top_level_commas(inner);
    if parts.len() != 3 {
        return None;
    }
    let (c1, p1) = parse_mix_arg(parts[1], dark, depth)?;
    let (c2, p2) = parse_mix_arg(parts[2], dark, depth)?;
    let (w1, w2) = match (p1, p2) {
        (Some(a), Some(b)) => {
            let sum = a + b;
            if sum <= 0.0 {
                return None;
            }
            (a / sum, b / sum)
        }
        (Some(a), None) => (a.clamp(0.0, 1.0), (1.0 - a).clamp(0.0, 1.0)),
        (None, Some(b)) => ((1.0 - b).clamp(0.0, 1.0), b.clamp(0.0, 1.0)),
        (None, None) => (0.5, 0.5),
    };
    let r1: Rgba = c1.into();
    let r2: Rgba = c2.into();
    let a = r1.a * w1 + r2.a * w2;
    let mixed = if a <= 1e-6 {
        Rgba {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.0,
        }
    } else {
        Rgba {
            r: ((r1.r * r1.a * w1 + r2.r * r2.a * w2) / a).clamp(0.0, 1.0),
            g: ((r1.g * r1.a * w1 + r2.g * r2.a * w2) / a).clamp(0.0, 1.0),
            b: ((r1.b * r1.a * w1 + r2.b * r2.a * w2) / a).clamp(0.0, 1.0),
            a: a.clamp(0.0, 1.0),
        }
    };
    Some(CssColor::Exact(mixed.into()))
}

fn parse_mix_arg(arg: &str, dark: bool, depth: usize) -> Option<(Hsla, Option<f32>)> {
    let trimmed = arg.trim();
    let mut color_str = trimmed;
    let mut pct = None;
    if let Some(last_space) = find_top_level_last_space(trimmed) {
        let head = trimmed[..last_space].trim();
        let tail = trimmed[last_space + 1..].trim();
        if let Some(p) = tail.strip_suffix('%').and_then(number) {
            color_str = head;
            pct = Some(p / 100.0);
        } else if let Some(p) = head.strip_suffix('%').and_then(number) {
            color_str = tail;
            pct = Some(p / 100.0);
        }
    }
    match parse_nested_color(color_str, dark, depth + 1)? {
        CssColor::Exact(c) => Some((c, pct)),
        CssColor::CurrentColor => None,
    }
}

pub(crate) fn split_top_level_commas(input: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0u32;
    let mut start = 0;
    for (i, ch) in input.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(input[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    let tail = input[start..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

fn find_top_level_last_space(input: &str) -> Option<usize> {
    let mut depth = 0u32;
    let mut last = None;
    for (i, ch) in input.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            c if c.is_ascii_whitespace() && depth == 0 => last = Some(i),
            _ => {}
        }
    }
    last
}

fn named_color(name: &str) -> Option<u32> {
    Some(match name {
        "black" => 0x000000,
        "white" => 0xffffff,
        "red" => 0xff0000,
        "green" => 0x008000,
        "blue" => 0x0000ff,
        "yellow" => 0xffff00,
        "cyan" | "aqua" => 0x00ffff,
        "magenta" | "fuchsia" => 0xff00ff,
        "gray" | "grey" => 0x808080,
        "darkgray" | "darkgrey" => 0xa9a9a9,
        "lightgray" | "lightgrey" => 0xd3d3d3,
        "dimgray" | "dimgrey" => 0x696969,
        "silver" => 0xc0c0c0,
        "gainsboro" => 0xdcdcdc,
        "whitesmoke" => 0xf5f5f5,
        "snow" => 0xfffafa,
        "ivory" => 0xfffff0,
        "ghostwhite" => 0xf8f8ff,
        "aliceblue" => 0xf0f8ff,
        "azure" => 0xf0ffff,
        "mintcream" => 0xf5fffa,
        "honeydew" => 0xf0fff0,
        "linen" => 0xfaf0e6,
        "beige" => 0xf5f5dc,
        "cornsilk" => 0xfff8dc,
        "floralwhite" => 0xfffaf0,
        "seashell" => 0xfff5ee,
        "oldlace" => 0xfdf5e6,
        "lavenderblush" => 0xfff0f5,
        "mistyrose" => 0xffe4e1,
        "antiquewhite" => 0xfaebd7,
        "blanchedalmond" => 0xffebcd,
        "bisque" => 0xffe4c4,
        "wheat" => 0xf5deb3,
        "navajowhite" => 0xffdead,
        "peachpuff" => 0xffdab9,
        "papayawhip" => 0xffefd5,
        "moccasin" => 0xffe4b5,
        "lemonchiffon" => 0xfffacd,
        "lightyellow" => 0xffffe0,
        "lightgoldenrodyellow" => 0xfafad2,
        "khaki" => 0xf0e68c,
        "darkkhaki" => 0xbdb76b,
        "palegoldenrod" => 0xeee8aa,
        "gold" => 0xffd700,
        "orange" => 0xffa500,
        "darkorange" => 0xff8c00,
        "coral" => 0xff7f50,
        "tomato" => 0xff6347,
        "orangered" => 0xff4500,
        "salmon" => 0xfa8072,
        "lightsalmon" => 0xffa07a,
        "darksalmon" => 0xe9967a,
        "lightcoral" => 0xf08080,
        "indianred" => 0xcd5c5c,
        "crimson" => 0xdc143c,
        "firebrick" => 0xb22222,
        "darkred" => 0x8b0000,
        "maroon" => 0x800000,
        "brown" => 0xa52a2a,
        "sienna" => 0xa0522d,
        "saddlebrown" => 0x8b4513,
        "chocolate" => 0xd2691e,
        "peru" => 0xcd853f,
        "sandybrown" => 0xf4a460,
        "burlywood" => 0xdeb887,
        "tan" => 0xd2b48c,
        "rosybrown" => 0xbc8f8f,
        "goldenrod" => 0xdaa520,
        "darkgoldenrod" => 0xb8860b,
        "pink" => 0xffc0cb,
        "lightpink" => 0xffb6c1,
        "hotpink" => 0xff69b4,
        "deeppink" => 0xff1493,
        "palevioletred" => 0xdb7093,
        "mediumvioletred" => 0xc71585,
        "lavender" => 0xe6e6fa,
        "thistle" => 0xd8bfd8,
        "plum" => 0xdda0dd,
        "violet" => 0xee82ee,
        "orchid" => 0xda70d6,
        "mediumorchid" => 0xba55d3,
        "darkorchid" => 0x9932cc,
        "darkviolet" => 0x9400d3,
        "blueviolet" => 0x8a2be2,
        "mediumpurple" => 0x9370db,
        "rebeccapurple" => 0x663399,
        "purple" => 0x800080,
        "darkmagenta" => 0x8b008b,
        "indigo" => 0x4b0082,
        "slateblue" => 0x6a5acd,
        "darkslateblue" => 0x483d8b,
        "mediumslateblue" => 0x7b68ee,
        "lime" => 0x00ff00,
        "limegreen" => 0x32cd32,
        "chartreuse" => 0x7fff00,
        "lawngreen" => 0x7cfc00,
        "greenyellow" => 0xadff2f,
        "yellowgreen" => 0x9acd32,
        "springgreen" => 0x00ff7f,
        "mediumspringgreen" => 0x00fa9a,
        "lightgreen" => 0x90ee90,
        "palegreen" => 0x98fb98,
        "darkseagreen" => 0x8fbc8f,
        "mediumseagreen" => 0x3cb371,
        "seagreen" => 0x2e8b57,
        "forestgreen" => 0x228b22,
        "darkgreen" => 0x006400,
        "olive" => 0x808000,
        "olivedrab" => 0x6b8e23,
        "darkolivegreen" => 0x556b2f,
        "aquamarine" => 0x7fffd4,
        "mediumaquamarine" => 0x66cdaa,
        "paleturquoise" => 0xafeeee,
        "turquoise" => 0x40e0d0,
        "mediumturquoise" => 0x48d1cc,
        "darkturquoise" => 0x00ced1,
        "lightseagreen" => 0x20b2aa,
        "cadetblue" => 0x5f9ea0,
        "darkcyan" => 0x008b8b,
        "teal" => 0x008080,
        "lightcyan" => 0xe0ffff,
        "powderblue" => 0xb0e0e6,
        "lightblue" => 0xadd8e6,
        "lightskyblue" => 0x87cefa,
        "skyblue" => 0x87ceeb,
        "deepskyblue" => 0x00bfff,
        "dodgerblue" => 0x1e90ff,
        "cornflowerblue" => 0x6495ed,
        "steelblue" => 0x4682b4,
        "lightsteelblue" => 0xb0c4de,
        "royalblue" => 0x4169e1,
        "mediumblue" => 0x0000cd,
        "darkblue" => 0x00008b,
        "navy" => 0x000080,
        "midnightblue" => 0x191970,
        "slategray" | "slategrey" => 0x708090,
        "lightslategray" | "lightslategrey" => 0x778899,
        "darkslategray" | "darkslategrey" => 0x2f4f4f,
        _ => return None,
    })
}
