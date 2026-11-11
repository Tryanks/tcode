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
