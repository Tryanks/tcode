use std::ops::Range;

use imara_diff::{Algorithm, Diff, InternedInput};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineHunk {
    pub old: Range<u32>,
    pub new: Range<u32>,
}

pub type WordDiffRanges = (Vec<Range<usize>>, Vec<Range<usize>>);

pub fn line_diff(old: &str, new: &str, ignore_whitespace: bool) -> Vec<LineHunk> {
    if ignore_whitespace {
        let old = imara_diff::sources::lines(old)
            .map(|line| {
                line.chars()
                    .filter(|ch| !ch.is_whitespace())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let new = imara_diff::sources::lines(new)
            .map(|line| {
                line.chars()
                    .filter(|ch| !ch.is_whitespace())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let mut input = InternedInput::default();
        input.update_before(old.into_iter());
        input.update_after(new.into_iter());
        compute_line_hunks(&input)
    } else {
        let input = InternedInput::new(
            imara_diff::sources::lines(old),
            imara_diff::sources::lines(new),
        );
        compute_line_hunks(&input)
    }
}

fn compute_line_hunks<T: AsRef<[u8]>>(input: &InternedInput<T>) -> Vec<LineHunk> {
    let mut diff = Diff::compute(Algorithm::Histogram, input);
    diff.postprocess_lines(input);
    diff.hunks()
        .map(|hunk| LineHunk {
            old: hunk.before,
            new: hunk.after,
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Word,
    Whitespace,
    Punctuation,
}

fn char_class(ch: char) -> CharClass {
    if ch.is_alphanumeric() || ch == '_' {
        CharClass::Word
    } else if ch.is_whitespace() {
        CharClass::Whitespace
    } else {
        CharClass::Punctuation
    }
}

pub(crate) fn word_token_ranges(text: &str) -> Vec<Range<usize>> {
    let mut tokens = Vec::new();
    let mut chars = text.char_indices();
    let Some((_, first)) = chars.next() else {
        return tokens;
    };
    let mut start = 0;
    let mut previous = first;
    let mut previous_class = char_class(first);
    for (index, ch) in chars {
        let class = char_class(ch);
        if class != previous_class || (class == CharClass::Punctuation && ch != previous) {
            tokens.push(start..index);
            start = index;
        }
        previous = ch;
        previous_class = class;
    }
    tokens.push(start..text.len());
    tokens
}

pub fn word_diff_ranges(old: &str, new: &str) -> Option<WordDiffRanges> {
    const MAX_WORD_DIFF_LEN: usize = 512;
    const MAX_WORD_DIFF_LINE_COUNT: usize = 8;

    if old.is_empty()
        || new.is_empty()
        || old.len() > MAX_WORD_DIFF_LEN
        || new.len() > MAX_WORD_DIFF_LEN
        || old.lines().count() > MAX_WORD_DIFF_LINE_COUNT
        || new.lines().count() > MAX_WORD_DIFF_LINE_COUNT
    {
        return None;
    }

    let old_tokens = word_token_ranges(old);
    let new_tokens = word_token_ranges(new);
    let mut input = InternedInput::default();
    input.update_before(old_tokens.iter().map(|range| &old[range.clone()]));
    input.update_after(new_tokens.iter().map(|range| &new[range.clone()]));
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);

    let mut old_ranges = Vec::new();
    let mut new_ranges = Vec::new();
    for hunk in diff.hunks() {
        if let Some(range) = token_hunk_to_byte_range(&old_tokens, hunk.before) {
            push_merged(&mut old_ranges, range);
        }
        if let Some(range) = token_hunk_to_byte_range(&new_tokens, hunk.after) {
            push_merged(&mut new_ranges, range);
        }
    }
    Some((old_ranges, new_ranges))
}

fn token_hunk_to_byte_range(tokens: &[Range<usize>], hunk: Range<u32>) -> Option<Range<usize>> {
    if hunk.is_empty() {
        return None;
    }
    let first = &tokens[hunk.start as usize];
    let last = &tokens[hunk.end as usize - 1];
    Some(first.start..last.end)
}

fn push_merged(ranges: &mut Vec<Range<usize>>, range: Range<usize>) {
    if let Some(last) = ranges.last_mut()
        && last.end >= range.start
    {
        last.end = last.end.max(range.end);
    } else {
        ranges.push(range);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(text: &str) -> Vec<&str> {
        word_token_ranges(text)
            .into_iter()
            .map(|range| &text[range])
            .collect()
    }

    #[test]
    fn tokenizer_splits_word_whitespace_and_distinct_punctuation() {
        assert_eq!(
            tokens("one.two(three)"),
            vec!["one", ".", "two", "(", "three", ")"]
        );
        assert_eq!(tokens("hello  world"), vec!["hello", "  ", "world"]);
        assert_eq!(tokens("a_1 += b"), vec!["a_1", " ", "+", "=", " ", "b"]);
    }

    #[test]
    fn line_diff_reports_basic_hunks() {
        let hunks = line_diff("one\ntwo\nthree\n", "one\nTWO\nthree\nfour\n", false);
        assert_eq!(
            hunks,
            vec![
                LineHunk {
                    old: 1..2,
                    new: 1..2
                },
                LineHunk {
                    old: 3..3,
                    new: 3..4
                }
            ]
        );
    }

    #[test]
    fn line_diff_can_ignore_all_whitespace() {
        assert!(line_diff("let x = 1;\n", "let  x=1;\n", true).is_empty());
        assert!(!line_diff("let x = 1;\n", "let  x=1;\n", false).is_empty());
    }

    #[test]
    fn word_diff_flags_changed_number() {
        let old = "let x = 1;";
        let new = "let x = 2;";
        let (old_ranges, new_ranges) = word_diff_ranges(old, new).unwrap();
        assert_eq!(old_ranges, vec![8..9]);
        assert_eq!(new_ranges, vec![8..9]);
    }

    #[test]
    fn word_diff_is_gated_by_input_size() {
        assert!(word_diff_ranges(&"a".repeat(513), "b").is_none());
        assert!(word_diff_ranges("a", &"b".repeat(513)).is_none());
    }
}
