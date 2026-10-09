use std::ops::Range;

use crate::highlight::HighlightTheme;
use agent::FileChangeKind;
use gpui::{HighlightStyle, Hsla};

use super::algorithm::{line_diff, word_diff_ranges};
use super::parse::{RowKind, parse_hunk_header, parse_unified_diff};
use crate::highlight;

#[derive(Debug, Clone)]
pub struct DiffColors {
    pub added_word_bg: Hsla,
    pub removed_word_bg: Hsla,
}

#[derive(Debug, Clone)]
pub struct RenderedRow {
    pub kind: RowKind,
    pub old: Option<u32>,
    pub new: Option<u32>,
    pub text: String,
    pub runs: Vec<(Range<usize>, HighlightStyle)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairedRow {
    pub left: Option<usize>,
    pub right: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct RenderedFile {
    pub path: String,
    pub kind: FileChangeKind,
    pub added: u32,
    pub removed: u32,
    pub all_rows: Vec<RenderedRow>,
    pub collapsed: Vec<Range<u32>>,
    pub expandable: bool,
    pub all_split: Vec<PairedRow>,
}

pub struct FileDiffInput<'a> {
    pub path: &'a str,
    pub kind: FileChangeKind,
    pub old_text: Option<&'a str>,
    pub new_text: Option<&'a str>,
    pub patch: Option<&'a str>,
    pub ignore_whitespace: bool,
    pub show_invisibles: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisibleItem {
    Gap {
        count: u32,
        new_lines: Range<u32>,
        expandable: bool,
    },
    Row(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisibleSplitItem {
    Gap {
        count: u32,
        new_lines: Range<u32>,
        expandable: bool,
    },
    Pair(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpandDir {
    Up,
    Down,
    All,
}

#[derive(Clone, Copy)]
struct SourceRange {
    start: usize,
    end: usize,
}

struct TextLine<'a> {
    text: &'a str,
    source: SourceRange,
}

fn text_lines(text: &str) -> Vec<TextLine<'_>> {
    let mut offset = 0;
    imara_diff::sources::lines(text)
        .map(|line| {
            let content = line.strip_suffix('\n').unwrap_or(line);
            let content = content.strip_suffix('\r').unwrap_or(content);
            let result = TextLine {
                text: content,
                source: SourceRange {
                    start: offset,
                    end: offset + content.len(),
                },
            };
            offset += line.len();
            result
        })
        .collect()
}

pub fn reconstruct_from_text(new_text: String, patch: &str) -> Option<(String, String)> {
    let old_text = if patch.lines().any(|line| line.starts_with("@@")) {
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

    let new_lines = text_lines(new_text)
        .into_iter()
        .map(|line| line.text.to_string())
        .collect::<Vec<_>>();
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

    let mut new_lines = text_lines(new_text)
        .into_iter()
        .map(|line| line.text.to_string())
        .collect::<Vec<_>>();
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

pub fn build_file(
    input: &FileDiffInput<'_>,
    display_path: String,
    lang: &str,
    theme: &HighlightTheme,
    colors: &DiffColors,
    whitespace_style: &HighlightStyle,
) -> RenderedFile {
    let display_path = if display_path.is_empty() {
        input.path.to_string()
    } else {
        display_path
    };
    let mut file = match (input.old_text, input.new_text) {
        (Some(old), Some(new)) => build_from_texts(input, display_path, lang, theme, old, new),
        _ => build_from_patch(input, display_path, lang, theme),
    };

    apply_word_highlights(&mut file.all_rows, colors);
    if input.show_invisibles {
        for row in &mut file.all_rows {
            let (new_text, new_runs) = apply_invisibles(&row.text, &row.runs, whitespace_style);
            row.text = new_text;
            row.runs = new_runs;
        }
    }
    file.all_split = pair_rendered_rows(&file.all_rows);
    file
}

fn build_from_texts(
    input: &FileDiffInput<'_>,
    display_path: String,
    lang: &str,
    theme: &HighlightTheme,
    old: &str,
    new: &str,
) -> RenderedFile {
    let hunks = line_diff(old, new, input.ignore_whitespace);
    let old_lines = text_lines(old);
    let new_lines = text_lines(new);
    let old_styles = highlight::highlight_source(old, lang, theme);
    let new_styles = highlight::highlight_source(new, lang, theme);
    let mut rows = Vec::with_capacity(old_lines.len() + new_lines.len());
    let mut old_cursor = 0usize;
    let mut new_cursor = 0usize;

    for hunk in &hunks {
        while old_cursor < hunk.old.start as usize && new_cursor < hunk.new.start as usize {
            push_context_row(&mut rows, &new_lines, &new_styles, old_cursor, new_cursor);
            old_cursor += 1;
            new_cursor += 1;
        }
        for old_index in hunk.old.clone().map(|line| line as usize) {
            let line = &old_lines[old_index];
            rows.push(RenderedRow {
                kind: RowKind::Removed,
                old: Some(old_index as u32 + 1),
                new: None,
                text: line.text.to_string(),
                runs: sub_runs(&old_styles, line.source.start, line.source.end),
            });
        }
        for new_index in hunk.new.clone().map(|line| line as usize) {
            let line = &new_lines[new_index];
            rows.push(RenderedRow {
                kind: RowKind::Added,
                old: None,
                new: Some(new_index as u32 + 1),
                text: line.text.to_string(),
                runs: sub_runs(&new_styles, line.source.start, line.source.end),
            });
        }
        old_cursor = hunk.old.end as usize;
        new_cursor = hunk.new.end as usize;
    }
    while old_cursor < old_lines.len() && new_cursor < new_lines.len() {
        push_context_row(&mut rows, &new_lines, &new_styles, old_cursor, new_cursor);
        old_cursor += 1;
        new_cursor += 1;
    }
    for (old_index, line) in old_lines.iter().enumerate().skip(old_cursor) {
        rows.push(RenderedRow {
            kind: RowKind::Removed,
            old: Some(old_index as u32 + 1),
            new: None,
            text: line.text.to_string(),
            runs: sub_runs(&old_styles, line.source.start, line.source.end),
        });
    }
    for (new_index, line) in new_lines.iter().enumerate().skip(new_cursor) {
        rows.push(RenderedRow {
            kind: RowKind::Added,
            old: None,
            new: Some(new_index as u32 + 1),
            text: line.text.to_string(),
            runs: sub_runs(&new_styles, line.source.start, line.source.end),
        });
    }

    let collapsed = collapsed_context(&hunks, new_lines.len() as u32);
    RenderedFile {
        path: display_path,
        kind: input.kind,
        added: hunks.iter().map(|hunk| hunk.new.end - hunk.new.start).sum(),
        removed: hunks.iter().map(|hunk| hunk.old.end - hunk.old.start).sum(),
        all_rows: rows,
        collapsed,
        expandable: true,
        all_split: Vec::new(),
    }
}

fn push_context_row(
    rows: &mut Vec<RenderedRow>,
    new_lines: &[TextLine<'_>],
    new_styles: &[(Range<usize>, HighlightStyle)],
    old_index: usize,
    new_index: usize,
) {
    let line = &new_lines[new_index];
    rows.push(RenderedRow {
        kind: RowKind::Context,
        old: Some(old_index as u32 + 1),
        new: Some(new_index as u32 + 1),
        text: line.text.to_string(),
        runs: sub_runs(new_styles, line.source.start, line.source.end),
    });
}

fn collapsed_context(hunks: &[super::algorithm::LineHunk], new_line_count: u32) -> Vec<Range<u32>> {
    let mut shown = Vec::<Range<u32>>::new();
    for hunk in hunks {
        let window =
            hunk.new.start.saturating_sub(3)..hunk.new.end.saturating_add(3).min(new_line_count);
        if let Some(last) = shown.last_mut()
            && last.end >= window.start
        {
            last.end = last.end.max(window.end);
        } else {
            shown.push(window);
        }
    }
    let mut collapsed = Vec::new();
    let mut cursor = 0;
    for window in shown {
        if cursor < window.start {
            collapsed.push(cursor + 1..window.start + 1);
        }
        cursor = cursor.max(window.end);
    }
    if cursor < new_line_count {
        collapsed.push(cursor + 1..new_line_count + 1);
    }
    collapsed
}

fn build_from_patch(
    input: &FileDiffInput<'_>,
    display_path: String,
    lang: &str,
    theme: &HighlightTheme,
) -> RenderedFile {
    let parsed = input.patch.map(parse_unified_diff).unwrap_or_default();
    let mut new_src = String::new();
    let mut old_src = String::new();
    let mut rows_with_sources = Vec::new();
    let mut collapsed = Vec::new();
    let mut new_cursor = 1u32;

    for hunk in &parsed.hunks {
        if hunk.gap_before > 0 {
            collapsed.push(new_cursor..new_cursor + hunk.gap_before);
            new_cursor += hunk.gap_before;
        }
        for row in &hunk.rows {
            let (source, range) = match row.kind {
                RowKind::Added | RowKind::Context => {
                    let start = new_src.len();
                    new_src.push_str(&row.text);
                    let end = new_src.len();
                    new_src.push('\n');
                    new_cursor += 1;
                    (RowKind::Added, SourceRange { start, end })
                }
                RowKind::Removed => {
                    let start = old_src.len();
                    old_src.push_str(&row.text);
                    let end = old_src.len();
                    old_src.push('\n');
                    (RowKind::Removed, SourceRange { start, end })
                }
            };
            rows_with_sources.push((row, source, range));
        }
    }
    let new_styles = highlight::highlight_source(&new_src, lang, theme);
    let old_styles = highlight::highlight_source(&old_src, lang, theme);
    let all_rows = rows_with_sources
        .into_iter()
        .map(|(row, source, range)| RenderedRow {
            kind: row.kind,
            old: row.old_line,
            new: row.new_line,
            text: row.text.clone(),
            runs: if source == RowKind::Removed {
                sub_runs(&old_styles, range.start, range.end)
            } else {
                sub_runs(&new_styles, range.start, range.end)
            },
        })
        .collect();

    RenderedFile {
        path: display_path,
        kind: input.kind,
        added: parsed.added,
        removed: parsed.removed,
        all_rows,
        collapsed,
        expandable: false,
        all_split: Vec::new(),
    }
}

fn apply_word_highlights(rows: &mut [RenderedRow], colors: &DiffColors) {
    let mut index = 0;
    while index < rows.len() {
        if rows[index].kind == RowKind::Context {
            index += 1;
            continue;
        }
        let start = index;
        while index < rows.len() && matches!(rows[index].kind, RowKind::Added | RowKind::Removed) {
            index += 1;
        }
        let removed = block_text_and_offsets(rows, start..index, RowKind::Removed);
        let added = block_text_and_offsets(rows, start..index, RowKind::Added);
        let Some((old_ranges, new_ranges)) = word_diff_ranges(&removed.0, &added.0) else {
            continue;
        };
        apply_block_ranges(rows, &removed.1, &old_ranges, colors.removed_word_bg);
        apply_block_ranges(rows, &added.1, &new_ranges, colors.added_word_bg);
    }
}

fn block_text_and_offsets(
    rows: &[RenderedRow],
    range: Range<usize>,
    wanted: RowKind,
) -> (String, Vec<(usize, Range<usize>)>) {
    let mut text = String::new();
    let mut offsets = Vec::new();
    for index in range {
        let RenderedRow {
            kind,
            text: row_text,
            ..
        } = &rows[index];
        if *kind == wanted {
            let start = text.len();
            text.push_str(row_text);
            let end = text.len();
            offsets.push((index, start..end));
            text.push('\n');
        }
    }
    (text, offsets)
}

fn apply_block_ranges(
    rows: &mut [RenderedRow],
    offsets: &[(usize, Range<usize>)],
    ranges: &[Range<usize>],
    background: Hsla,
) {
    for (index, row_range) in offsets {
        let local = ranges
            .iter()
            .filter_map(|range| {
                let start = range.start.max(row_range.start);
                let end = range.end.min(row_range.end);
                (start < end).then(|| start - row_range.start..end - row_range.start)
            })
            .collect::<Vec<_>>();
        let row = &mut rows[*index];
        row.runs = overlay_background(row.text.len(), &row.runs, &local, background);
    }
}

fn overlay_background(
    text_len: usize,
    runs: &[(Range<usize>, HighlightStyle)],
    highlights: &[Range<usize>],
    background: Hsla,
) -> Vec<(Range<usize>, HighlightStyle)> {
    if text_len == 0 {
        return Vec::new();
    }
    let mut boundaries = vec![0, text_len];
    for (range, _) in runs {
        boundaries.extend([range.start.min(text_len), range.end.min(text_len)]);
    }
    for range in highlights {
        boundaries.extend([range.start.min(text_len), range.end.min(text_len)]);
    }
    boundaries.sort_unstable();
    boundaries.dedup();
    let mut output = Vec::new();
    for pair in boundaries.windows(2) {
        let range = pair[0]..pair[1];
        if range.is_empty() {
            continue;
        }
        let mut style = runs
            .iter()
            .find(|(candidate, _)| candidate.start <= range.start && candidate.end > range.start)
            .map(|(_, style)| *style)
            .unwrap_or_default();
        if highlights
            .iter()
            .any(|highlight| highlight.start < range.end && highlight.end > range.start)
        {
            style.background_color = Some(background);
        }
        push_style_run(&mut output, range, style);
    }
    output
}

fn push_style_run(
    runs: &mut Vec<(Range<usize>, HighlightStyle)>,
    range: Range<usize>,
    style: HighlightStyle,
) {
    if let Some((last_range, last_style)) = runs.last_mut()
        && last_range.end == range.start
        && *last_style == style
    {
        last_range.end = range.end;
    } else {
        runs.push((range, style));
    }
}

pub(crate) fn sub_runs(
    all: &[(Range<usize>, HighlightStyle)],
    start: usize,
    end: usize,
) -> Vec<(Range<usize>, HighlightStyle)> {
    all.iter()
        .filter(|(range, _)| range.start < end && range.end > start)
        .map(|(range, style)| {
            (
                range.start.max(start) - start..range.end.min(end) - start,
                *style,
            )
        })
        .collect()
}

pub fn apply_invisibles(
    text: &str,
    runs: &[(Range<usize>, HighlightStyle)],
    ws_style: &HighlightStyle,
) -> (String, Vec<(Range<usize>, HighlightStyle)>) {
    let mut output = String::with_capacity(text.len());
    let mut output_runs = Vec::new();
    for (old_start, ch) in text.char_indices() {
        let old_end = old_start + ch.len_utf8();
        let replacement = match ch {
            ' ' => "·",
            '\t' => "→",
            _ => &text[old_start..old_end],
        };
        let new_start = output.len();
        output.push_str(replacement);
        let new_end = output.len();
        let style = if matches!(ch, ' ' | '\t') {
            *ws_style
        } else {
            runs.iter()
                .find(|(range, _)| range.start <= old_start && range.end > old_start)
                .map(|(_, style)| *style)
                .unwrap_or_default()
        };
        push_style_run(&mut output_runs, new_start..new_end, style);
    }
    (output, output_runs)
}

fn pair_rendered_rows(rows: &[RenderedRow]) -> Vec<PairedRow> {
    let mut output = Vec::new();
    let mut index = 0;
    while index < rows.len() {
        if rows[index].kind == RowKind::Context {
            output.push(PairedRow {
                left: Some(index),
                right: Some(index),
            });
            index += 1;
            continue;
        }
        let start = index;
        while index < rows.len() && matches!(rows[index].kind, RowKind::Added | RowKind::Removed) {
            index += 1;
        }
        let removed = (start..index)
            .filter(|&row| rows[row].kind == RowKind::Removed)
            .collect::<Vec<_>>();
        let added = (start..index)
            .filter(|&row| rows[row].kind == RowKind::Added)
            .collect::<Vec<_>>();
        let old_text = joined_row_text(rows, &removed);
        let new_text = joined_row_text(rows, &added);
        let hunks = line_diff(&old_text, &new_text, false);
        let mut old_cursor = 0usize;
        let mut new_cursor = 0usize;
        for hunk in hunks {
            while old_cursor < hunk.old.start as usize && new_cursor < hunk.new.start as usize {
                output.push(PairedRow {
                    left: Some(removed[old_cursor]),
                    right: Some(added[new_cursor]),
                });
                old_cursor += 1;
                new_cursor += 1;
            }
            let old_end = hunk.old.end as usize;
            let new_end = hunk.new.end as usize;
            while old_cursor < old_end || new_cursor < new_end {
                output.push(PairedRow {
                    left: (old_cursor < old_end).then(|| removed[old_cursor]),
                    right: (new_cursor < new_end).then(|| added[new_cursor]),
                });
                old_cursor += usize::from(old_cursor < old_end);
                new_cursor += usize::from(new_cursor < new_end);
            }
        }
        while old_cursor < removed.len() || new_cursor < added.len() {
            output.push(PairedRow {
                left: removed.get(old_cursor).copied(),
                right: added.get(new_cursor).copied(),
            });
            old_cursor += usize::from(old_cursor < removed.len());
            new_cursor += usize::from(new_cursor < added.len());
        }
    }
    output
}

fn joined_row_text(rows: &[RenderedRow], indices: &[usize]) -> String {
    let mut output = String::new();
    for index in indices {
        output.push_str(&rows[*index].text);
        output.push('\n');
    }
    output
}

fn row_anchors(rows: &[RenderedRow]) -> Vec<Option<u32>> {
    let mut anchors = vec![None; rows.len()];
    let mut following_new = None;
    for (index, row) in rows.iter().enumerate().rev() {
        if row.new.is_some() {
            following_new = row.new;
        }
        anchors[index] = if row.new.is_some() {
            row.new
        } else if row.kind == RowKind::Removed {
            following_new
        } else {
            None
        };
    }
    anchors
}

fn collapsed_for_line(collapsed: &[Range<u32>], line: Option<u32>) -> bool {
    line.is_some_and(|line| collapsed.iter().any(|range| range.contains(&line)))
}

pub fn visible_unified(file: &RenderedFile) -> Vec<VisibleItem> {
    let anchors = row_anchors(&file.all_rows);
    let mut output = Vec::new();
    let mut emitted_gaps = vec![false; file.collapsed.len()];
    for (index, anchor) in anchors.into_iter().enumerate() {
        for (gap_index, gap) in file.collapsed.iter().enumerate() {
            if !emitted_gaps[gap_index] && anchor.is_some_and(|line| line >= gap.end) {
                output.push(VisibleItem::Gap {
                    count: gap.end - gap.start,
                    new_lines: gap.clone(),
                    expandable: file.expandable,
                });
                emitted_gaps[gap_index] = true;
            }
        }
        if !collapsed_for_line(&file.collapsed, anchor) {
            output.push(VisibleItem::Row(index));
        }
    }
    for (gap_index, gap) in file.collapsed.iter().enumerate() {
        if !emitted_gaps[gap_index] {
            output.push(VisibleItem::Gap {
                count: gap.end - gap.start,
                new_lines: gap.clone(),
                expandable: file.expandable,
            });
        }
    }
    output
}

pub fn visible_split(file: &RenderedFile) -> Vec<VisibleSplitItem> {
    let anchors = row_anchors(&file.all_rows);
    let mut output = Vec::new();
    let mut emitted_gaps = vec![false; file.collapsed.len()];
    for (pair_index, pair) in file.all_split.iter().enumerate() {
        let anchor = pair
            .right
            .and_then(|index| anchors[index])
            .or_else(|| pair.left.and_then(|index| anchors[index]));
        for (gap_index, gap) in file.collapsed.iter().enumerate() {
            if !emitted_gaps[gap_index] && anchor.is_some_and(|line| line >= gap.end) {
                output.push(VisibleSplitItem::Gap {
                    count: gap.end - gap.start,
                    new_lines: gap.clone(),
                    expandable: file.expandable,
                });
                emitted_gaps[gap_index] = true;
            }
        }
        let left_hidden = pair
            .left
            .is_none_or(|index| collapsed_for_line(&file.collapsed, anchors[index]));
        let right_hidden = pair
            .right
            .is_none_or(|index| collapsed_for_line(&file.collapsed, anchors[index]));
        if !(left_hidden && right_hidden) {
            output.push(VisibleSplitItem::Pair(pair_index));
        }
    }
    for (gap_index, gap) in file.collapsed.iter().enumerate() {
        if !emitted_gaps[gap_index] {
            output.push(VisibleSplitItem::Gap {
                count: gap.end - gap.start,
                new_lines: gap.clone(),
                expandable: file.expandable,
            });
        }
    }
    output
}

pub fn expand(file: &mut RenderedFile, gap: Range<u32>, direction: ExpandDir, amount: u32) {
    if !file.expandable {
        return;
    }
    let Some(index) = file.collapsed.iter().position(|range| *range == gap) else {
        return;
    };
    match direction {
        ExpandDir::All => {
            file.collapsed.remove(index);
        }
        ExpandDir::Up => {
            file.collapsed[index].start = file.collapsed[index]
                .start
                .saturating_add(amount)
                .min(gap.end);
            if file.collapsed[index].is_empty() {
                file.collapsed.remove(index);
            }
        }
        ExpandDir::Down => {
            file.collapsed[index].end = file.collapsed[index]
                .end
                .saturating_sub(amount)
                .max(gap.start);
            if file.collapsed[index].is_empty() {
                file.collapsed.remove(index);
            }
        }
    }
}

pub fn diff_content_widths(files: &[RenderedFile]) -> (f32, f32) {
    const MONO_ADVANCE: f32 = 8.;
    const UNIFIED_CHROME: f32 = 106.;
    const SPLIT_CHROME: f32 = 117.;
    const HEADER_CHROME: f32 = 180.;

    let mut unified_columns = 0;
    let mut split_columns = 0;
    let mut header_columns = 0;
    for file in files {
        header_columns = header_columns.max(display_columns(&file.path));
        for row in &file.all_rows {
            unified_columns = unified_columns.max(display_columns(&row.text));
        }
        for pair in &file.all_split {
            let columns = pair
                .left
                .map(|index| display_columns(&file.all_rows[index].text))
                .unwrap_or(0)
                + pair
                    .right
                    .map(|index| display_columns(&file.all_rows[index].text))
                    .unwrap_or(0);
            split_columns = split_columns.max(columns);
        }
    }
    let header_width = header_columns as f32 * MONO_ADVANCE + HEADER_CHROME;
    (
        (unified_columns as f32 * MONO_ADVANCE + UNIFIED_CHROME).max(header_width),
        (split_columns as f32 * MONO_ADVANCE + SPLIT_CHROME).max(header_width),
    )
}

pub fn display_columns(text: &str) -> usize {
    text.chars().fold(0, |columns, ch| match ch {
        '\t' => columns + (4 - columns % 4),
        ch if ch.is_ascii_control() => columns,
        ch if ch.is_ascii() => columns + 1,
        _ => columns + 2,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors() -> DiffColors {
        DiffColors {
            added_word_bg: gpui::hsla(0.3, 0.8, 0.5, 0.3),
            removed_word_bg: gpui::hsla(0., 0.8, 0.5, 0.28),
        }
    }

    fn code(kind: RowKind, old: Option<u32>, new: Option<u32>, text: &str) -> RenderedRow {
        RenderedRow {
            kind,
            old,
            new,
            text: text.into(),
            runs: Vec::new(),
        }
    }

    fn file(rows: Vec<RenderedRow>, collapsed: Vec<Range<u32>>) -> RenderedFile {
        let all_split = pair_rendered_rows(&rows);
        RenderedFile {
            path: "test.rs".into(),
            kind: FileChangeKind::Modify,
            added: 0,
            removed: 0,
            all_rows: rows,
            collapsed,
            expandable: true,
            all_split,
        }
    }

    #[test]
    fn removed_row_uses_nearest_following_new_line_for_collapse() {
        let file = file(
            vec![
                code(RowKind::Context, Some(1), Some(1), "one"),
                code(RowKind::Removed, Some(2), None, "old"),
                code(RowKind::Added, None, Some(2), "new"),
                code(RowKind::Context, Some(3), Some(3), "three"),
            ],
            std::iter::once(2..3).collect(),
        );
        assert_eq!(
            visible_unified(&file),
            vec![
                VisibleItem::Row(0),
                VisibleItem::Gap {
                    count: 1,
                    new_lines: 2..3,
                    expandable: true
                },
                VisibleItem::Row(3)
            ]
        );
    }

    #[test]
    fn no_wrap_widths_cover_unified_and_split_rows() {
        // The long prefix puts content beyond header chrome, so a wrong tab
        // or wide-character width cannot hide behind the header's minimum.
        let tabbed = "a".repeat(32) + "ab\tcd";
        let wide = "a".repeat(32) + "a界b";
        for (left, right, left_columns, right_columns) in [
            (
                "short".to_string(),
                "a much longer replacement".to_string(),
                5usize,
                25usize,
            ),
            (tabbed.clone(), wide.clone(), 32 + 6, 32 + 4),
            (wide, tabbed, 32 + 4, 32 + 6),
        ] {
            let files = vec![file(
                vec![
                    code(RowKind::Removed, Some(1), None, &left),
                    code(RowKind::Added, None, Some(1), &right),
                ],
                Vec::new(),
            )];
            let (unified, split) = diff_content_widths(&files);
            assert_eq!(
                unified,
                left_columns.max(right_columns) as f32 * 8. + 106.,
                "{left:?} / {right:?}"
            );
            assert_eq!(
                split,
                (left_columns + right_columns) as f32 * 8. + 117.,
                "{left:?} / {right:?}"
            );
        }
    }

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

    #[test]
    fn full_text_pipeline_builds_expanded_rows_collapsed_context_and_word_runs() {
        let theme = HighlightTheme::default_dark();
        let whitespace = HighlightStyle {
            color: Some(gpui::hsla(0., 0., 0.5, 1.)),
            ..Default::default()
        };
        for (case, old, new, language, ignore_whitespace, show_invisibles) in [
            (
                "word and context",
                "one\ntwo\nthree\nfour\nlet x = 1;\nsix\nseven\neight\nnine\nten\n",
                "one\ntwo\nthree\nfour\nlet x = 2;\nsix\nseven\neight\nnine\nten\n",
                "rust",
                false,
                false,
            ),
            (
                "whitespace visible",
                "let x = 1;\n",
                "let  x=1;\n",
                "rust",
                false,
                false,
            ),
            (
                "whitespace ignored",
                "let x = 1;\n",
                "let  x=1;\n",
                "rust",
                true,
                false,
            ),
            (
                "line-local styles",
                "/* α\nβ */ \"tail\nend\"\n",
                "/* α\nβ */ \"tail\nend\"\n",
                "rust",
                false,
                false,
            ),
            (
                "invisible glyphs",
                "é \tx\n",
                "é \tx\n",
                "unknown-language",
                false,
                true,
            ),
            (
                "context expansion",
                "one\ntwo\nthree\nfour\nfive\nsix\n",
                "one\ntwo\nthree\nfour\nfive\nsix\n",
                "text",
                false,
                false,
            ),
        ] {
            let input = FileDiffInput {
                path: "src/test.rs",
                kind: FileChangeKind::Modify,
                old_text: Some(old),
                new_text: Some(new),
                patch: None,
                ignore_whitespace,
                show_invisibles,
            };
            let mut file = build_file(
                &input,
                input.path.into(),
                language,
                &theme,
                &colors(),
                &whitespace,
            );
            assert!(file.expandable, "{case}");
            for row in &file.all_rows {
                assert!(
                    row.runs.iter().all(|(range, _)| {
                        range.start < range.end
                            && range.end <= row.text.len()
                            && row.text.is_char_boundary(range.start)
                            && row.text.is_char_boundary(range.end)
                    }),
                    "{case}: {row:?}"
                );
                assert!(
                    row.runs
                        .windows(2)
                        .all(|pair| pair[0].0.end <= pair[1].0.start),
                    "{case}: {row:?}"
                );
            }
            match case {
                "word and context" => {
                    assert_eq!((file.added, file.removed), (1, 1));
                    assert_eq!(file.all_rows.len(), 11);
                    assert_eq!(file.collapsed, vec![1..2, 9..11]);
                    let changed = file
                        .all_rows
                        .iter()
                        .filter(|row| matches!(row.kind, RowKind::Added | RowKind::Removed))
                        .collect::<Vec<_>>();
                    assert_eq!(changed.len(), 2);
                    for row in changed {
                        let (word, background) = if row.kind == RowKind::Added {
                            ("2", colors().added_word_bg)
                        } else {
                            ("1", colors().removed_word_bg)
                        };
                        assert!(
                            row.runs
                                .iter()
                                .any(|(range, style)| &row.text[range.clone()] == word
                                    && style.background_color == Some(background))
                        );
                    }
                }
                "whitespace visible" => {
                    assert_eq!((file.added, file.removed), (1, 1));
                    assert_eq!(
                        file.all_rows.iter().map(|row| row.kind).collect::<Vec<_>>(),
                        [RowKind::Removed, RowKind::Added]
                    );
                }
                "whitespace ignored" => {
                    assert_eq!((file.added, file.removed), (0, 0));
                    assert_eq!(file.all_rows.len(), 1);
                    assert_eq!(file.all_rows[0].kind, RowKind::Context);
                    assert_eq!(file.all_rows[0].text, "let  x=1;");
                }
                "line-local styles" => {
                    assert_eq!(
                        file.all_rows
                            .iter()
                            .map(|row| row.text.as_str())
                            .collect::<Vec<_>>(),
                        ["/* α", "β */ \"tail", "end\""]
                    );
                    let comment = theme.style("comment").unwrap();
                    let string = theme.style("string").unwrap();
                    assert_eq!(file.all_rows[0].runs, vec![(0..5, comment)]);
                    // The second line begins inside the preceding comment and
                    // ends inside a string continued on the third line. Both
                    // styles must be clipped and rebased, with the intervening
                    // ordinary space preserved as a separate run.
                    assert_eq!(
                        file.all_rows[1].runs,
                        vec![
                            (0..5, comment),
                            (5..6, HighlightStyle::default()),
                            (6..11, string),
                        ]
                    );
                    assert_eq!(file.all_rows[2].runs, vec![(0..4, string)]);
                }
                "invisible glyphs" => {
                    assert_eq!(file.all_rows.len(), 1);
                    let row = &file.all_rows[0];
                    assert_eq!(row.text, "é·→x");
                    assert_eq!(row.text.len(), 8);
                    assert_eq!(
                        row.runs,
                        vec![
                            (0..2, HighlightStyle::default()),
                            (2..7, whitespace),
                            (7..8, HighlightStyle::default()),
                        ]
                    );
                }
                "context expansion" => {
                    assert_eq!(file.collapsed, vec![1..7]);
                    expand(&mut file, 1..7, ExpandDir::Up, 2);
                    assert_eq!(file.collapsed, vec![3..7]);
                    assert!(visible_unified(&file).contains(&VisibleItem::Row(1)));
                    expand(&mut file, 3..7, ExpandDir::Down, 1);
                    assert_eq!(file.collapsed, vec![3..6]);
                    assert!(visible_unified(&file).contains(&VisibleItem::Row(5)));
                    expand(&mut file, 3..6, ExpandDir::All, 20);
                    assert!(file.collapsed.is_empty());
                    assert_eq!(visible_unified(&file).len(), 6);
                    assert_eq!(visible_split(&file).len(), 6);
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn patch_pipeline_preserves_fixed_gap_and_applies_word_runs() {
        use RowKind::{Added, Context, Removed};
        for (case, patch, added, removed, gaps, rows, word_highlights) in [
            (
                "word highlights",
                "@@ -10,2 +10,2 @@\n-let x = 1;\n+let x = 2;\n tail",
                1,
                1,
                std::iter::once(1..10).collect(),
                vec![
                    (Removed, Some(10), None, "let x = 1;"),
                    (Added, None, Some(10), "let x = 2;"),
                    (Context, Some(11), Some(11), "tail"),
                ],
                true,
            ),
            (
                "bare creation",
                "+def f():\n+    return 1",
                2,
                0,
                vec![],
                vec![
                    (Added, None, Some(1), "def f():"),
                    (Added, None, Some(2), "    return 1"),
                ],
                false,
            ),
            (
                "bare edit",
                "-old one\n-old two\n+new one\n+new two\n+new three",
                3,
                2,
                vec![],
                vec![
                    (Removed, Some(1), None, "old one"),
                    (Removed, Some(2), None, "old two"),
                    (Added, None, Some(1), "new one"),
                    (Added, None, Some(2), "new two"),
                    (Added, None, Some(3), "new three"),
                ],
                false,
            ),
            (
                "multiple hunks",
                "diff --git a/util.py b/util.py\n--- a/util.py\n+++ b/util.py\n@@ -1,3 +1,4 @@\n import sys\n-x = 1\n+x = 2\n+y = 3\n print(x)\n@@ -20,2 +21,2 @@\n last_ctx\n-tail_old\n+tail_new\n\\ No newline at end of file",
                3,
                2,
                std::iter::once(5..21).collect(),
                vec![
                    (Context, Some(1), Some(1), "import sys"),
                    (Removed, Some(2), None, "x = 1"),
                    (Added, None, Some(2), "x = 2"),
                    (Added, None, Some(3), "y = 3"),
                    (Context, Some(3), Some(4), "print(x)"),
                    (Context, Some(20), Some(21), "last_ctx"),
                    (Removed, Some(21), None, "tail_old"),
                    (Added, None, Some(22), "tail_new"),
                ],
                false,
            ),
            (
                "initial gap",
                "@@ -10,2 +10,3 @@\n ctx\n+added\n more",
                1,
                0,
                std::iter::once(1..10).collect(),
                vec![
                    (Context, Some(10), Some(10), "ctx"),
                    (Added, None, Some(11), "added"),
                    (Context, Some(11), Some(12), "more"),
                ],
                false,
            ),
            (
                "omitted hunk counts",
                "@@ -1 +1 @@\n-a\n+b",
                1,
                1,
                vec![],
                vec![(Removed, Some(1), None, "a"), (Added, None, Some(1), "b")],
                false,
            ),
        ] {
            let input = FileDiffInput {
                path: "src/test.rs",
                kind: FileChangeKind::Modify,
                old_text: None,
                new_text: None,
                patch: Some(patch),
                ignore_whitespace: false,
                show_invisibles: false,
            };
            let file = build_file(
                &input,
                input.path.into(),
                "rust",
                &HighlightTheme::default_dark(),
                &colors(),
                &HighlightStyle::default(),
            );
            assert_eq!((file.added, file.removed), (added, removed), "{case}");
            assert_eq!(file.collapsed, gaps, "{case}");
            assert!(!file.expandable, "{case}");
            assert_eq!(
                file.all_rows
                    .iter()
                    .map(|row| (row.kind, row.old, row.new, row.text.as_str()))
                    .collect::<Vec<_>>(),
                rows,
                "{case}"
            );
            let visible_gaps = visible_unified(&file)
                .into_iter()
                .filter_map(|item| match item {
                    VisibleItem::Gap {
                        count,
                        new_lines,
                        expandable,
                    } => Some((count, new_lines, expandable)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                visible_gaps,
                gaps.iter()
                    .map(|range| (range.end - range.start, range.clone(), false))
                    .collect::<Vec<_>>(),
                "{case}"
            );
            if word_highlights {
                assert!(file.all_rows[..2].iter().all(|row| {
                    row.runs
                        .iter()
                        .any(|(_, style)| style.background_color.is_some())
                }));
            }
        }
    }

    #[test]
    fn split_pairing_aligns_multi_hunk_change_blocks() {
        for (patch, expected) in [
            (
                "@@ -1,4 +1,4 @@\n one\n-old a\n-old b\n+new a\n two\n@@ -20,2 +20,3 @@\n tail\n+extra\n",
                vec![
                    (Some(0), Some(0)),
                    (Some(1), Some(3)),
                    (Some(2), None),
                    (Some(4), Some(4)),
                    (Some(5), Some(5)),
                    (None, Some(6)),
                ],
            ),
            (
                "-same\n-old\n+same\n+new",
                vec![(Some(0), Some(2)), (Some(1), Some(3))],
            ),
        ] {
            let input = FileDiffInput {
                path: "src/test.rs",
                kind: FileChangeKind::Modify,
                old_text: None,
                new_text: None,
                patch: Some(patch),
                ignore_whitespace: false,
                show_invisibles: false,
            };
            let file = build_file(
                &input,
                input.path.into(),
                "rust",
                &HighlightTheme::default_dark(),
                &colors(),
                &HighlightStyle::default(),
            );
            assert_eq!(
                file.all_split
                    .iter()
                    .map(|pair| (pair.left, pair.right))
                    .collect::<Vec<_>>(),
                expected,
                "{patch}"
            );
        }
    }
}
