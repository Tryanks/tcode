//! Application patch preparation. Kit owns parsing for display, line layout and highlighting.
use super::algorithm::line_diff;
use super::parse::{RowKind, parse_hunk_header, parse_unified_diff};
use agent::FileChangeKind;
use gpui_component::diff::DiffFile;

/// Patch records retained for Tcode's review excerpts and host-side line anchors.
#[derive(Debug, Clone)]
pub struct ReviewLine {
    pub kind: RowKind,
    pub old: Option<u32>,
    pub new: Option<u32>,
    pub text: String,
}

#[derive(Clone)]
pub struct PreparedFile {
    pub path: String,
    pub document: DiffFile,
    pub all_rows: Vec<ReviewLine>,
    pub full_source: bool,
    pub parse_error: bool,
}

pub struct FileDiffInput<'a> {
    pub previous_path: Option<&'a str>,
    pub kind: FileChangeKind,
    pub old_text: Option<&'a str>,
    pub new_text: Option<&'a str>,
    pub patch: Option<&'a str>,
    pub ignore_whitespace: bool,
}

pub fn prepare_file(input: &FileDiffInput<'_>, path: String) -> PreparedFile {
    let old = input
        .old_text
        .or((input.kind == FileChangeKind::Create).then_some(""));
    let new = input
        .new_text
        .or((input.kind == FileChangeKind::Delete).then_some(""));
    let full_source = old.is_some() && new.is_some();
    let body = match (old, new) {
        (Some(old), Some(new)) => full_patch(old, new, input.ignore_whitespace),
        _ => input.patch.map(patch_body).unwrap_or_default(),
    };
    let original = if input.kind == FileChangeKind::Create {
        "/dev/null".to_owned()
    } else {
        quote_path(&format!("a/{}", input.previous_path.unwrap_or(&path)))
    };
    let modified = if input.kind == FileChangeKind::Delete {
        "/dev/null".to_owned()
    } else {
        quote_path(&format!("b/{path}"))
    };
    let patch = format!("--- {original}\n+++ {modified}\n{body}");
    let (document, parse_error) = if input.patch.is_none() && !full_source {
        (DiffFile::unchanged(path.clone(), ""), false)
    } else {
        match DiffFile::parse(&patch) {
            Ok(files) => (
                files
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| DiffFile::unchanged(path.clone(), "")),
                false,
            ),
            Err(error) => {
                log::warn!("Cannot parse diff for {path}: {error}");
                (
                    DiffFile::unchanged(path.clone(), input.patch.unwrap_or(&body)),
                    true,
                )
            }
        }
    };
    let all_rows = if parse_error {
        Vec::new()
    } else {
        parse_unified_diff(&body)
            .hunks
            .into_iter()
            .flat_map(|hunk| hunk.rows)
            .map(|row| ReviewLine {
                kind: row.kind,
                old: row.old_line,
                new: row.new_line,
                text: row.text,
            })
            .collect()
    };
    PreparedFile {
        path,
        document,
        all_rows,
        full_source,
        parse_error,
    }
}

fn patch_body(patch: &str) -> String {
    let mut offset = 0;
    for line in patch.split_inclusive('\n') {
        if parse_hunk_header(line).is_some() {
            return patch[offset..].to_string();
        }
        offset += line.len();
    }
    let parsed = parse_unified_diff(patch);
    let rows = parsed.hunks.iter().flat_map(|hunk| &hunk.rows);
    let (mut old, mut new) = (0, 0);
    for row in rows {
        old += usize::from(row.old_line.is_some());
        new += usize::from(row.new_line.is_some());
    }
    format!(
        "@@ -{},{} +{},{} @@\n{}",
        usize::from(old > 0),
        old,
        usize::from(new > 0),
        new,
        patch
    )
}

fn quote_path(path: &str) -> String {
    let mut quoted = String::from("\"");
    for character in path.chars() {
        match character {
            '\\' | '"' => {
                quoted.push('\\');
                quoted.push(character);
            }
            '\0'..='\x1f' | '\x7f' => quoted.push_str(&format!("\\{:03o}", character as u32)),
            _ => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

fn full_patch(old: &str, new: &str, ignore_whitespace: bool) -> String {
    let old_lines = old.split_inclusive('\n').collect::<Vec<_>>();
    let new_lines = new.split_inclusive('\n').collect::<Vec<_>>();
    let mut body = format!(
        "@@ -{},{} +{},{} @@\n",
        usize::from(!old_lines.is_empty()),
        old_lines.len(),
        usize::from(!new_lines.is_empty()),
        new_lines.len()
    );
    let mut cursor = 0;
    let mut emit = |marker: char, line: &str| {
        body.push(marker);
        body.push_str(line);
        if !line.ends_with('\n') {
            body.push_str("\n\\ No newline at end of file\n");
        }
    };
    for hunk in line_diff(old, new, ignore_whitespace) {
        for line in &old_lines[cursor..hunk.old.start as usize] {
            emit(' ', line);
        }
        for line in &old_lines[hunk.old.start as usize..hunk.old.end as usize] {
            emit('-', line);
        }
        for line in &new_lines[hunk.new.start as usize..hunk.new.end as usize] {
            emit('+', line);
        }
        cursor = hunk.old.end as usize;
    }
    for line in &old_lines[cursor..] {
        emit(' ', line);
    }
    body
}

pub fn reconstruct_from_text(new_text: String, patch: &str) -> Option<(String, String)> {
    let old_text = if patch.lines().any(|line| parse_hunk_header(line).is_some()) {
        reconstruct_unified(&new_text, patch)?
    } else {
        reconstruct_bare(&new_text, patch)?
    };
    Some((old_text, new_text))
}

fn reconstruct_unified(new_text: &str, patch: &str) -> Option<String> {
    let parsed = parse_unified_diff(patch);
    let new_starts = patch
        .lines()
        .filter_map(parse_hunk_header)
        .map(|(_, new_start)| new_start)
        .collect::<Vec<_>>();
    if parsed.hunks.len() != new_starts.len() {
        return None;
    }

    let new_lines = new_text.lines().map(str::to_owned).collect::<Vec<_>>();
    for (hunk, new_start) in parsed.hunks.iter().zip(&new_starts) {
        let expected = hunk
            .rows
            .iter()
            .filter(|row| matches!(row.kind, RowKind::Context | RowKind::Added))
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>();
        let start = unified_new_index(*new_start, expected.len());
        let end = start.checked_add(expected.len())?;
        if end > new_lines.len()
            || new_lines[start..end]
                .iter()
                .map(String::as_str)
                .ne(expected)
        {
            return None;
        }
    }

    let mut old_lines = new_lines;
    for (hunk, new_start) in parsed.hunks.iter().zip(&new_starts).rev() {
        let new_len = hunk
            .rows
            .iter()
            .filter(|row| matches!(row.kind, RowKind::Context | RowKind::Added))
            .count();
        let start = unified_new_index(*new_start, new_len);
        let end = start.checked_add(new_len)?;
        if end > old_lines.len() {
            return None;
        }
        let replacement = hunk
            .rows
            .iter()
            .filter(|row| matches!(row.kind, RowKind::Removed | RowKind::Context))
            .map(|row| row.text.clone());
        old_lines.splice(start..end, replacement);
    }
    Some(join_reconstructed_lines(&old_lines, new_text))
}

fn unified_new_index(new_start: u32, new_len: usize) -> usize {
    if new_len == 0 {
        new_start as usize
    } else {
        new_start.saturating_sub(1) as usize
    }
}

fn reconstruct_bare(new_text: &str, patch: &str) -> Option<String> {
    let mut removed = Vec::new();
    let mut added = Vec::new();
    for line in patch.lines() {
        if line == r"\ No newline at end of file" {
            continue;
        }
        if let Some(line) = line.strip_prefix('-') {
            removed.push(line.to_string());
        } else if let Some(line) = line.strip_prefix('+') {
            added.push(line.to_string());
        }
    }

    if removed.is_empty() {
        let candidate = format!("{}\n", added.join("\n"));
        return same_except_trailing_newline(&candidate, new_text).then(String::new);
    }
    if added.is_empty() {
        return None;
    }

    let mut new_lines = new_text.lines().map(str::to_owned).collect::<Vec<_>>();
    let matches = new_lines
        .windows(added.len())
        .enumerate()
        .filter_map(|(index, window)| (window == added).then_some(index))
        .collect::<Vec<_>>();
    let [start] = matches.as_slice() else {
        return None;
    };
    new_lines.splice(*start..*start + added.len(), removed);
    Some(join_reconstructed_lines(&new_lines, new_text))
}

fn same_except_trailing_newline(left: &str, right: &str) -> bool {
    strip_one_trailing_newline(left) == strip_one_trailing_newline(right)
}

fn strip_one_trailing_newline(text: &str) -> &str {
    let Some(text) = text.strip_suffix('\n') else {
        return text;
    };
    text.strip_suffix('\r').unwrap_or(text)
}

fn join_reconstructed_lines(lines: &[String], template: &str) -> String {
    // No lines is an empty text, not one blank line: a new file's old side.
    if lines.is_empty() {
        return String::new();
    }
    let newline = if template.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut text = lines.join(newline);
    if template.ends_with('\n') {
        text.push_str(newline);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reconstruction_requires_matching_unambiguous_current_text() {
        for (current, patch, old) in [
            (
                "one\ntwo\nthree\nfour\nnew value\nsix\nseven\neight\nnine\nten\n",
                "@@ -3,5 +3,5 @@\n three\n four\n-old value\n+new value\n six\n seven\n",
                Some("one\ntwo\nthree\nfour\nold value\nsix\nseven\neight\nnine\nten\n"),
            ),
            ("alpha\nbeta\n", "+alpha\n+beta", Some("")),
            (
                "pub struct Cart;\n\npub struct Item;\n",
                "@@ -0,0 +1,3 @@\n+pub struct Cart;\n+\n+pub struct Item;\n",
                Some(""),
            ),
            (
                "before\nnew one\nnew two\nafter\n",
                "-old one\n-old two\n+new one\n+new two",
                Some("before\nold one\nold two\nafter\n"),
            ),
            ("new\nbetween\nnew\n", "-old\n+new", None),
            ("unrelated\n", "-old\n+new", None),
            (
                "one\nchanged again\nthree\n",
                "@@ -1,3 +1,3 @@\n one\n-old\n+new\n three\n",
                None,
            ),
        ] {
            assert_eq!(
                reconstruct_from_text(current.into(), patch),
                old.map(|old| (old.into(), current.into())),
                "{patch:?} against {current:?}"
            );
        }
    }
}
