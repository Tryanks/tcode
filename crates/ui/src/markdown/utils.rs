//! List-marker helper adapted from gpui-component's Apache-2.0 text renderer.

const UPPER: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const LOWER: &str = "abcdefghijklmnopqrstuvwxyz";
const BULLETS: [&str; 5] = ["•", "◦", "▪", "‣", "⁃"];

pub(super) fn list_item_prefix(ix: usize, ordered: bool, start: u32, depth: usize) -> String {
    if ordered {
        match depth {
            0 => format!("{}. ", start as usize + ix),
            1 => format!("{}. ", UPPER.chars().nth(ix % UPPER.len()).unwrap()),
            _ => format!("{}. ", LOWER.chars().nth(ix % LOWER.len()).unwrap()),
        }
    } else {
        format!("{} ", BULLETS[depth.min(BULLETS.len() - 1)])
    }
}

#[cfg(test)]
mod tests {
    use super::list_item_prefix;

    #[test]
    fn ordered_prefix_honours_list_start() {
        assert_eq!(list_item_prefix(0, true, 1, 0), "1. ");
        assert_eq!(list_item_prefix(2, true, 1, 0), "3. ");
        assert_eq!(list_item_prefix(0, true, 4, 0), "4. ");
        assert_eq!(list_item_prefix(1, true, 4, 0), "5. ");
    }
}
