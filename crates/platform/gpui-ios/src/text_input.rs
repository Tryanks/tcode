use std::ops::Range;

pub const NOT_FOUND: usize = isize::MAX as usize;

/// UIKit's NSRange uses UTF-16 code units and NSNotFound for an absent range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct TextRange {
    pub location: usize,
    pub length: usize,
}

impl TextRange {
    pub fn range(self) -> Option<Range<usize>> {
        (self.location < NOT_FOUND)
            .then(|| {
                self.location
                    .checked_add(self.length)
                    .filter(|end| *end < NOT_FOUND)
                    .map(|end| self.location..end)
            })
            .flatten()
    }
}

impl From<Option<Range<usize>>> for TextRange {
    fn from(range: Option<Range<usize>>) -> Self {
        match range {
            Some(range) if range.start <= range.end && range.end < NOT_FOUND => Self {
                location: range.start,
                length: range.end - range.start,
            },
            _ => Self {
                location: NOT_FOUND,
                length: 0,
            },
        }
    }
}

/// Reject offsets inside a surrogate pair rather than replacing a different character.
pub fn utf16_slice(text: &str, range: Range<usize>) -> Option<&str> {
    fn byte_offset(text: &str, target: usize) -> Option<usize> {
        let mut offset = 0;
        for (index, ch) in text.char_indices() {
            if offset == target {
                return Some(index);
            }
            offset += ch.len_utf16();
        }
        (offset == target).then_some(text.len())
    }
    text.get(byte_offset(text, range.start)?..byte_offset(text, range.end)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_ranges_preserve_cjk_emoji_and_combining_text() {
        let text = "前😀e\u{301}后";
        for (range, expected) in [
            (0..1, "前"),
            (1..3, "😀"),
            (3..5, "e\u{301}"),
            (5..6, "后"),
            (6..6, ""),
        ] {
            assert_eq!(utf16_slice(text, range), Some(expected));
        }
        for range in [1..2, 2..3, 0..7, 5..3] {
            assert_eq!(utf16_slice(text, range), None);
        }
    }

    #[test]
    fn native_ranges_distinguish_absent_empty_and_overflow() {
        assert_eq!(
            TextRange::from(Some(1..3)),
            TextRange {
                location: 1,
                length: 2
            }
        );
        assert_eq!(
            TextRange::from(Some(3..3)),
            TextRange {
                location: 3,
                length: 0
            }
        );
        assert_eq!(TextRange::from(None).range(), None);
        assert_eq!(TextRange::from(None).location, isize::MAX as usize);
        assert_eq!(
            TextRange {
                location: 3,
                length: 0
            }
            .range(),
            Some(3..3)
        );
        assert_eq!(
            TextRange {
                location: 1,
                length: 2
            }
            .range(),
            Some(1..3)
        );
        assert_eq!(
            TextRange {
                location: NOT_FOUND - 1,
                length: 4
            }
            .range(),
            None
        );
        assert_eq!(
            TextRange {
                location: isize::MAX as usize,
                length: 0
            }
            .range(),
            None
        );
    }
}
