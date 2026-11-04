//! List-marker helper adapted from gpui-component's Apache-2.0 text renderer.

const BULLETS: [&str; 5] = ["•", "◦", "▪", "‣", "⁃"];

/// The marker of item `ix` of a list starting at `start`: a number at the
/// top level, and in a nested list the letters counting from `start`
/// (A … Z, AA, AB, …), lower case below the first level.
pub(super) fn list_item_prefix(ix: usize, ordered: bool, start: u32, depth: usize) -> String {
    if !ordered {
        return format!("{} ", BULLETS[depth.min(BULLETS.len() - 1)]);
    }
    let ordinal = u64::from(start).saturating_add(ix as u64);
    if depth == 0 || start == 0 {
        return format!("{ordinal}. ");
    }
    let first = if depth == 1 { b'A' } else { b'a' };
    let mut letters = Vec::new();
    let mut remaining = ordinal;
    while remaining > 0 {
        remaining -= 1;
        letters.push(char::from(first + (remaining % 26) as u8));
        remaining /= 26;
    }
    letters.reverse();
    format!("{}. ", String::from_iter(letters))
}

#[cfg(test)]
mod tests {
    use super::list_item_prefix;

    #[test]
    fn nested_ordered_lists_letter_past_z_from_their_own_start() {
        for (ix, start, label) in [
            (0, 1, "A"),
            (25, 1, "Z"),
            (26, 1, "AA"),
            (27, 1, "AB"),
            (51, 1, "AZ"),
            (52, 1, "BA"),
            (701, 1, "ZZ"),
            (702, 1, "AAA"),
            // `3.` starts a nested list at its third letter.
            (0, 3, "C"),
            (1, 26, "AA"),
        ] {
            assert_eq!(list_item_prefix(ix, true, start, 1), format!("{label}. "));
            assert_eq!(
                list_item_prefix(ix, true, start, 2),
                format!("{}. ", label.to_ascii_lowercase())
            );
        }
        assert_eq!(list_item_prefix(26, true, 1, 0), "27. ");
        // No letter stands for 0, so a nested list starting there counts.
        assert_eq!(list_item_prefix(1, true, 0, 1), "1. ");
    }
}
