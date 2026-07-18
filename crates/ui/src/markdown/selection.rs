//! Word-selection helpers adapted from gpui-component's Apache-2.0
//! `text/selection.rs` implementation.

use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharType {
    Word,
    Whitespace,
    Newline,
    Other,
}

fn is_word_char(c: char) -> bool {
    matches!(c, '_')
        || c.is_ascii_alphanumeric()
        || matches!(c, '\u{00C0}'..='\u{00FF}')
        || matches!(c, '\u{0100}'..='\u{017F}')
        || matches!(c, '\u{0180}'..='\u{024F}')
        || matches!(c, '\u{0400}'..='\u{04FF}')
        || matches!(c, '\u{1E00}'..='\u{1EFF}')
        || matches!(c, '\u{0300}'..='\u{036F}')
}

impl From<char> for CharType {
    fn from(c: char) -> Self {
        match c {
            c if is_word_char(c) => Self::Word,
            '\n' | '\r' => Self::Newline,
            c if c.is_whitespace() => Self::Whitespace,
            _ => Self::Other,
        }
    }
}

impl CharType {
    fn is_connectable(self, c: char) -> bool {
        matches!(
            (self, Self::from(c)),
            (Self::Word, Self::Word) | (Self::Whitespace, Self::Whitespace)
        )
    }
}

pub(super) fn word_range_at(text: &str, offset: usize) -> Option<Range<usize>> {
    if text.is_empty() {
        return None;
    }
    let offset = clip_offset(text, offset);
    let c = text[offset..].chars().next()?;
    let char_type = CharType::from(c);
    let mut start = offset;
    let mut end = offset + c.len_utf8();

    for previous in text[..offset].chars().rev().take(128) {
        if !char_type.is_connectable(previous) {
            break;
        }
        start -= previous.len_utf8();
    }
    for next in text[end..].chars().take(128) {
        if !char_type.is_connectable(next) {
            break;
        }
        end += next.len_utf8();
    }
    Some(start..end)
}

fn clip_offset(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    if offset == text.len() {
        return text.char_indices().next_back().map_or(0, |(ix, _)| ix);
    }
    if text.is_char_boundary(offset) {
        offset
    } else {
        text.char_indices()
            .map(|(ix, _)| ix)
            .take_while(|ix| *ix < offset)
            .last()
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_unicode_aware_words() {
        let text = "test text\nabcde 中文🎉 rök";
        assert_eq!(&text[word_range_at(text, 0).unwrap()], "test");
        assert_eq!(&text[word_range_at(text, 16).unwrap()], "中");
        assert_eq!(&text[word_range_at(text, 27).unwrap()], "rök");
    }
}
