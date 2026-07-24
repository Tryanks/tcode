#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiffRow {
    pub kind: RowKind,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hunk {
    pub gap_before: u32,
    pub rows: Vec<DiffRow>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedDiff {
    pub hunks: Vec<Hunk>,
    pub added: u32,
    pub removed: u32,
}

pub fn parse_hunk_header(line: &str) -> Option<(u32, u32)> {
    let rest = line.strip_prefix("@@")?;
    let body = rest.split("@@").next().unwrap_or("").trim();
    let mut old_start = None;
    let mut new_start = None;
    for tok in body.split_whitespace() {
        if let Some(v) = tok.strip_prefix('-') {
            old_start = v.split(',').next().and_then(|n| n.parse::<u32>().ok());
        } else if let Some(v) = tok.strip_prefix('+') {
            new_start = v.split(',').next().and_then(|n| n.parse::<u32>().ok());
        }
    }
    Some((old_start?, new_start?))
}

pub fn parse_unified_diff(diff: &str) -> ParsedDiff {
    if diff.lines().any(|l| l.starts_with("@@")) {
        parse_standard(diff)
    } else {
        parse_bare(diff)
    }
}

pub fn parse_standard(diff: &str) -> ParsedDiff {
    let mut out = ParsedDiff::default();
    let mut cur: Option<Hunk> = None;
    let mut seen_hunk = false;
    let mut old_cursor = 0u32;
    let mut new_cursor = 0u32;
    let mut prev_new_end = 1u32;

    for line in diff.lines() {
        if let Some((old_start, new_start)) = parse_hunk_header(line) {
            if let Some(hunk) = cur.take() {
                prev_new_end = new_cursor;
                out.hunks.push(hunk);
            }
            let gap = new_start.saturating_sub(prev_new_end);
            old_cursor = old_start;
            new_cursor = new_start;
            cur = Some(Hunk {
                gap_before: gap,
                rows: Vec::new(),
            });
            seen_hunk = true;
            continue;
        }
        if !seen_hunk {
            continue;
        }
        let Some(hunk) = cur.as_mut() else { continue };
        let mut chars = line.chars();
        match chars.next() {
            Some('+') => {
                out.added += 1;
                hunk.rows.push(DiffRow {
                    kind: RowKind::Added,
                    old_line: None,
                    new_line: Some(new_cursor),
                    text: chars.as_str().to_string(),
                });
                new_cursor += 1;
            }
            Some('-') => {
                out.removed += 1;
                hunk.rows.push(DiffRow {
                    kind: RowKind::Removed,
                    old_line: Some(old_cursor),
                    new_line: None,
                    text: chars.as_str().to_string(),
                });
                old_cursor += 1;
            }
            Some('\\') => {}
            Some(' ') => {
                hunk.rows.push(DiffRow {
                    kind: RowKind::Context,
                    old_line: Some(old_cursor),
                    new_line: Some(new_cursor),
                    text: chars.as_str().to_string(),
                });
                old_cursor += 1;
                new_cursor += 1;
            }
            None => {
                hunk.rows.push(DiffRow {
                    kind: RowKind::Context,
                    old_line: Some(old_cursor),
                    new_line: Some(new_cursor),
                    text: String::new(),
                });
                old_cursor += 1;
                new_cursor += 1;
            }
            Some(_) => {
                hunk.rows.push(DiffRow {
                    kind: RowKind::Context,
                    old_line: Some(old_cursor),
                    new_line: Some(new_cursor),
                    text: line.to_string(),
                });
                old_cursor += 1;
                new_cursor += 1;
            }
        }
    }
    if let Some(hunk) = cur.take() {
        out.hunks.push(hunk);
    }
    out
}

pub fn parse_bare(diff: &str) -> ParsedDiff {
    let mut out = ParsedDiff::default();
    let mut rows = Vec::new();
    let mut old_cursor = 1u32;
    let mut new_cursor = 1u32;
    for line in diff.lines() {
        let mut chars = line.chars();
        match chars.next() {
            Some('+') => {
                out.added += 1;
                rows.push(DiffRow {
                    kind: RowKind::Added,
                    old_line: None,
                    new_line: Some(new_cursor),
                    text: chars.as_str().to_string(),
                });
                new_cursor += 1;
            }
            Some('-') => {
                out.removed += 1;
                rows.push(DiffRow {
                    kind: RowKind::Removed,
                    old_line: Some(old_cursor),
                    new_line: None,
                    text: chars.as_str().to_string(),
                });
                old_cursor += 1;
            }
            _ => {
                rows.push(DiffRow {
                    kind: RowKind::Context,
                    old_line: Some(old_cursor),
                    new_line: Some(new_cursor),
                    text: line.to_string(),
                });
                old_cursor += 1;
                new_cursor += 1;
            }
        }
    }
    if !rows.is_empty() {
        out.hunks.push(Hunk {
            gap_before: 0,
            rows,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_create_diff() {
        let parsed = parse_unified_diff("+def f():\n+    return 1");
        assert_eq!(parsed.added, 2);
        assert_eq!(parsed.removed, 0);
        assert_eq!(parsed.hunks.len(), 1);
        let rows = &parsed.hunks[0].rows;
        assert_eq!(rows[0].kind, RowKind::Added);
        assert_eq!(rows[0].new_line, Some(1));
        assert_eq!(rows[0].old_line, None);
        assert_eq!(rows[0].text, "def f():");
        assert_eq!(rows[1].new_line, Some(2));
    }

    #[test]
    fn parses_bare_edit_diff() {
        let parsed = parse_unified_diff("-old one\n-old two\n+new one\n+new two\n+new three");
        assert_eq!(parsed.removed, 2);
        assert_eq!(parsed.added, 3);
        let rows = &parsed.hunks[0].rows;
        assert_eq!(rows[0].old_line, Some(1));
        assert_eq!(rows[1].old_line, Some(2));
        assert_eq!(rows[2].new_line, Some(1));
        assert_eq!(rows[4].new_line, Some(3));
    }

    #[test]
    fn parses_multi_hunk_with_gaps_create_edit_and_no_newline() {
        let diff = "\
diff --git a/util.py b/util.py
--- a/util.py
+++ b/util.py
@@ -1,3 +1,4 @@
 import sys
-x = 1
+x = 2
+y = 3
 print(x)
@@ -20,2 +21,2 @@
 last_ctx
-tail_old
+tail_new
\\ No newline at end of file";
        let parsed = parse_unified_diff(diff);
        assert_eq!(parsed.hunks.len(), 2);
        assert_eq!(parsed.hunks[0].gap_before, 0);
        assert_eq!(parsed.hunks[1].gap_before, 16);
        assert_eq!(parsed.added, 3);
        assert_eq!(parsed.removed, 2);
        let h0 = &parsed.hunks[0].rows;
        assert_eq!(h0[0].kind, RowKind::Context);
        assert_eq!((h0[0].old_line, h0[0].new_line), (Some(1), Some(1)));
        assert_eq!(h0[1].kind, RowKind::Removed);
        assert_eq!((h0[1].old_line, h0[1].new_line), (Some(2), None));
        assert_eq!(h0[2].kind, RowKind::Added);
        assert_eq!((h0[2].old_line, h0[2].new_line), (None, Some(2)));
        assert_eq!(h0[3].kind, RowKind::Added);
        assert_eq!(h0[3].new_line, Some(3));
        assert_eq!(h0[4].kind, RowKind::Context);
        assert_eq!((h0[4].old_line, h0[4].new_line), (Some(3), Some(4)));
        assert_eq!(parsed.hunks[1].rows.len(), 3);
    }

    #[test]
    fn gap_before_first_hunk_when_not_at_top() {
        let parsed = parse_unified_diff("@@ -10,2 +10,3 @@\n ctx\n+added\n more");
        assert_eq!(parsed.hunks[0].gap_before, 9);
    }

    #[test]
    fn hunk_header_without_counts_parses() {
        let parsed = parse_unified_diff("@@ -1 +1 @@\n-a\n+b");
        assert_eq!(parsed.hunks.len(), 1);
        assert_eq!(parsed.added, 1);
        assert_eq!(parsed.removed, 1);
    }
}
