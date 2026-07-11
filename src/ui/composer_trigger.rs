//! Composer inline-trigger detection and mention serialization.
//!
//! A faithful Rust port of T3's `packages/shared/src/composerTrigger.ts`: given
//! the composer text and the cursor byte offset, detect an active `@file`,
//! `$skill`, or `/command` trigger, and serialize a picked file path into the
//! exact Markdown link T3 emits (`[basename](encoded-path)`).

use std::ops::Range;

/// The kind of inline trigger active at the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerKind {
    /// `@` file/folder mention.
    Path,
    /// `/` provider/built-in command.
    SlashCommand,
    /// The special `/model` command (opens the model picker).
    SlashModel,
    /// `$` skill.
    Skill,
}

/// A detected trigger: its kind, the query typed after the trigger char, and the
/// byte range (trigger char … cursor) that a selection replaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerTrigger {
    pub kind: TriggerKind,
    pub query: String,
    pub range: Range<usize>,
}

/// ASCII whitespace token boundary (matches T3's `isWhitespace`).
fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\n' | b'\t' | b'\r')
}

/// Detect an active trigger at `cursor` (a UTF-8 byte offset into `text`).
///
/// `/` is only recognized at the start of the current line; `@` and `$` are
/// recognized after any whitespace boundary. Mirrors T3's `detectComposerTrigger`.
pub fn detect_composer_trigger(text: &str, cursor: usize) -> Option<ComposerTrigger> {
    let cursor = cursor.min(text.len());
    // Snap to a char boundary defensively (byte offsets from the input are on
    // boundaries, but clamping above could land mid-char in pathological input).
    let cursor = (0..=cursor).rev().find(|&i| text.is_char_boundary(i))?;

    let line_start = text[..cursor].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line_prefix = &text[line_start..cursor];

    if let Some(rest) = line_prefix.strip_prefix('/') {
        // `^/(\S*)$`: a slash command with no whitespace after the slash.
        if !rest.bytes().any(is_ws) {
            if rest.eq_ignore_ascii_case("model") {
                return Some(ComposerTrigger {
                    kind: TriggerKind::SlashModel,
                    query: String::new(),
                    range: line_start..cursor,
                });
            }
            return Some(ComposerTrigger {
                kind: TriggerKind::SlashCommand,
                query: rest.to_string(),
                range: line_start..cursor,
            });
        }
        // `^/model(?:\s+(.*))?$`: `/model <query>`.
        if let Some(after) = line_prefix.strip_prefix("/model")
            && after.starts_with(|c: char| c.is_whitespace())
        {
            return Some(ComposerTrigger {
                kind: TriggerKind::SlashModel,
                query: after.trim().to_string(),
                range: line_start..cursor,
            });
        }
        // A `/word …` that is not a bare command and not `/model`: fall through.
    }

    // Walk back over the current whitespace-delimited token.
    let bytes = text.as_bytes();
    let mut token_start = cursor;
    while token_start > 0 && !is_ws(bytes[token_start - 1]) {
        token_start -= 1;
    }
    let token = &text[token_start..cursor];
    if let Some(query) = token.strip_prefix('$') {
        return Some(ComposerTrigger {
            kind: TriggerKind::Skill,
            query: query.to_string(),
            range: token_start..cursor,
        });
    }
    if let Some(query) = token.strip_prefix('@') {
        return Some(ComposerTrigger {
            kind: TriggerKind::Path,
            query: query.to_string(),
            range: token_start..cursor,
        });
    }
    None
}

/// The basename of a `/`- or `\`-separated path.
pub fn basename(path: &str) -> &str {
    let idx = path.rfind(['/', '\\']).map(|i| i + 1).unwrap_or(0);
    &path[idx..]
}

fn escape_markdown_link_label(label: &str) -> String {
    label
        .replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]")
}

/// Percent-encode like JS `encodeURI`: keep unreserved + reserved URI chars,
/// escape everything else per UTF-8 byte.
fn encode_uri(s: &str) -> String {
    // Chars `encodeURI` leaves untouched.
    const KEEP: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.!~*'();,/?:@&=+$#";
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if KEEP.contains(&b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

fn encode_markdown_link_destination(path: &str) -> String {
    encode_uri(path)
        .replace('(', "%28")
        .replace(')', "%29")
        .replace('#', "%23")
        .replace('?', "%3F")
        .replace('\\', "%5C")
}

/// Serialize a picked file path into T3's exact Markdown link form,
/// `[basename](encoded-path)`.
pub fn serialize_composer_file_link(path: &str) -> String {
    let label = escape_markdown_link_label(basename(path));
    let dest = encode_markdown_link_destination(path);
    format!("[{label}]({dest})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_at_mention_after_whitespace() {
        let text = "look at @src/ma";
        let t = detect_composer_trigger(text, text.len()).unwrap();
        assert_eq!(t.kind, TriggerKind::Path);
        assert_eq!(t.query, "src/ma");
        assert_eq!(&text[t.range.clone()], "@src/ma");
    }

    #[test]
    fn at_mention_needs_whitespace_boundary() {
        // `@` glued to a word (email-like) is still a token starting with the
        // preceding non-space run, so it does NOT start with `@` → no trigger.
        let text = "foo@bar";
        assert!(detect_composer_trigger(text, text.len()).is_none());
    }

    #[test]
    fn detects_skill_trigger() {
        let text = "use $rev";
        let t = detect_composer_trigger(text, text.len()).unwrap();
        assert_eq!(t.kind, TriggerKind::Skill);
        assert_eq!(t.query, "rev");
    }

    #[test]
    fn slash_only_at_line_start() {
        let t = detect_composer_trigger("/pla", 4).unwrap();
        assert_eq!(t.kind, TriggerKind::SlashCommand);
        assert_eq!(t.query, "pla");
        // Mid-line slash is not a command.
        assert!(detect_composer_trigger("hello /pla", 10).is_none());
        // On a second line at its start it is recognized.
        let t2 = detect_composer_trigger("hi\n/de", 6).unwrap();
        assert_eq!(t2.kind, TriggerKind::SlashCommand);
        assert_eq!(t2.query, "de");
    }

    #[test]
    fn slash_model_special_cases() {
        let m = detect_composer_trigger("/model", 6).unwrap();
        assert_eq!(m.kind, TriggerKind::SlashModel);
        assert_eq!(m.query, "");
        let m2 = detect_composer_trigger("/model gpt", 10).unwrap();
        assert_eq!(m2.kind, TriggerKind::SlashModel);
        assert_eq!(m2.query, "gpt");
    }

    #[test]
    fn serialize_matches_t3() {
        assert_eq!(
            serialize_composer_file_link("src/main.rs"),
            "[main.rs](src/main.rs)"
        );
        assert_eq!(
            serialize_composer_file_link("my file.txt"),
            "[my file.txt](my%20file.txt)"
        );
        // Parentheses in the path are escaped in the destination but not the label.
        assert_eq!(
            serialize_composer_file_link("a(b).js"),
            "[a(b).js](a%28b%29.js)"
        );
        // A `#` and `?` in the path get further-escaped past encodeURI.
        assert_eq!(
            serialize_composer_file_link("weird#name?.md"),
            "[weird#name?.md](weird%23name%3F.md)"
        );
    }

    #[test]
    fn basename_handles_both_separators() {
        assert_eq!(basename("a/b/c.rs"), "c.rs");
        assert_eq!(basename("a\\b\\c.rs"), "c.rs");
        assert_eq!(basename("bare.txt"), "bare.txt");
    }
}
