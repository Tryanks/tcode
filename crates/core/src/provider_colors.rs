//! Per-provider colors for the sidebar thread tint.
//!
//! Built-in providers carry their brand color; everything the user adds (a
//! custom profile, an ACP agent) draws from [`PROVIDER_COLOR_PALETTE`]. The
//! palette slot is derived from the color key alone, so two devices sharing the
//! same settings file agree on every color without persisting an assignment.

use agent::ProviderKind;

/// Hand-picked brand colors (`0xRRGGBB`) for the built-in provider profiles.
/// ACP is not one provider but many, so it has no color of its own: each agent
/// takes a palette slot under its `acp:<id>` key.
pub fn builtin_provider_color(kind: ProviderKind) -> Option<u32> {
    match kind {
        ProviderKind::ClaudeCode => Some(0xD97757),
        ProviderKind::Codex => Some(0x8B5CF6),
        ProviderKind::Pi => Some(0x4D9ABF),
        ProviderKind::OpenCode => Some(0x22A06B),
        ProviderKind::Acp => None,
    }
}

/// Hues for user-added providers (`0xRRGGBB`). They stay clear of the four
/// brand colors above so a custom endpoint never masquerades as a built-in.
pub const PROVIDER_COLOR_PALETTE: [u32; 10] = [
    0xF59E0B, // amber
    0x84CC16, // lime
    0x14B8A6, // teal
    0x06B6D4, // cyan
    0x3B82F6, // blue
    0x4F46E5, // indigo
    0xD946EF, // fuchsia
    0xEC4899, // pink
    0xF43F5E, // rose
    0x6B7FD7, // slate blue
];

/// 64-bit FNV-1a. `DefaultHasher` is not stable across Rust releases, and a
/// color that changed after an update would look like a different provider.
fn fnv1a(key: &str) -> u64 {
    key.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn hashed_slot(key: &str) -> usize {
    (fnv1a(key) % PROVIDER_COLOR_PALETTE.len() as u64) as usize
}

/// The palette color for a custom provider key.
///
/// `known` is every currently configured custom provider key, in sorted order.
/// A known key starts at its hashed slot and bumps forward past slots already
/// taken by keys sorted before it, so configured providers never share a color
/// while the palette has room. A key outside `known` (a thread whose profile
/// was deleted) keeps its plain hashed slot.
pub fn palette_color(key: &str, known: &[&str]) -> u32 {
    let palette_len = PROVIDER_COLOR_PALETTE.len();
    let mut taken = [false; PROVIDER_COLOR_PALETTE.len()];
    let mut assigned = 0;
    for candidate in known {
        let mut slot = hashed_slot(candidate);
        if assigned < palette_len {
            while taken[slot] {
                slot = (slot + 1) % palette_len;
            }
            taken[slot] = true;
            assigned += 1;
        }
        if *candidate == key {
            return PROVIDER_COLOR_PALETTE[slot];
        }
    }
    PROVIDER_COLOR_PALETTE[hashed_slot(key)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_colors_fill_the_palette_before_reusing_slots() {
        let known = ["acp:gemini", "work-claude"];
        assert_eq!(palette_color("acp:gemini", &known), 0x14B8A6);
        assert_eq!(palette_color("work-claude", &known), 0x6B7FD7);
        // A deleted profile's threads keep their hashed color rather than none.
        assert_eq!(palette_color("deleted-profile", &known), 0xF59E0B);

        let keys: Vec<String> = (0..PROVIDER_COLOR_PALETTE.len() + 3)
            .map(|i| format!("profile-{i:02}"))
            .collect();
        let known: Vec<&str> = keys.iter().map(String::as_str).collect();
        let colors: Vec<u32> = known.iter().map(|key| palette_color(key, &known)).collect();
        let mut first_palette = colors[..PROVIDER_COLOR_PALETTE.len()].to_vec();
        first_palette.sort_unstable();
        first_palette.dedup();
        assert_eq!(first_palette.len(), PROVIDER_COLOR_PALETTE.len());
        assert!(
            colors
                .iter()
                .all(|color| PROVIDER_COLOR_PALETTE.contains(color))
        );
    }
}
