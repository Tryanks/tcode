use std::ops::Range;
use std::path::Path;

use agent::FileChangeKind;
use gpui::{HighlightStyle, Hsla};
use gpui_component::highlighter::HighlightTheme;

use super::algorithm::{line_diff, word_diff_ranges};
use super::parse::{RowKind, parse_hunk_header, parse_unified_diff};
use crate::highlight;

const MAX_RECONSTRUCT_FILE_BYTES: u64 = 512 * 1024;

#[derive(Debug, Clone)]
pub struct DiffColors {
    pub added_word_bg: Hsla,
    pub removed_word_bg: Hsla,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GapInfo {
    pub count: u32,
    pub new_lines: Range<u32>,
}

#[derive(Debug, Clone)]
pub enum RenderedRow {
    Gap(GapInfo),
    Code {
        kind: RowKind,
        old: Option<u32>,
        new: Option<u32>,
        text: String,
        runs: Vec<(Range<usize>, HighlightStyle)>,
    },
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
    Gap { count: u32, expandable: bool },
    Row(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisibleSplitItem {
    Gap { count: u32, expandable: bool },
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

pub fn reconstruct_from_disk(abs_path: &Path, patch: &str) -> Option<(String, String)> {
    let metadata = std::fs::metadata(abs_path).ok()?;
    if metadata.len() > MAX_RECONSTRUCT_FILE_BYTES {
        return None;
    }
    let new_text = std::fs::read_to_string(abs_path).ok()?;
    if new_text.len() as u64 > MAX_RECONSTRUCT_FILE_BYTES {
        return None;
    }

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
            if let RenderedRow::Code { text, runs, .. } = row {
                let (new_text, new_runs) = apply_invisibles(text, runs, whitespace_style);
                *text = new_text;
                *runs = new_runs;
            }
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
            rows.push(RenderedRow::Code {
                kind: RowKind::Removed,
                old: Some(old_index as u32 + 1),
                new: None,
                text: line.text.to_string(),
                runs: sub_runs(&old_styles, line.source.start, line.source.end),
            });
        }
        for new_index in hunk.new.clone().map(|line| line as usize) {
            let line = &new_lines[new_index];
            rows.push(RenderedRow::Code {
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
        rows.push(RenderedRow::Code {
            kind: RowKind::Removed,
            old: Some(old_index as u32 + 1),
            new: None,
            text: line.text.to_string(),
            runs: sub_runs(&old_styles, line.source.start, line.source.end),
        });
    }
    for (new_index, line) in new_lines.iter().enumerate().skip(new_cursor) {
        rows.push(RenderedRow::Code {
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
    rows.push(RenderedRow::Code {
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
        .map(|(row, source, range)| RenderedRow::Code {
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
        if row_kind(&rows[index]) == Some(RowKind::Context) {
            index += 1;
            continue;
        }
        let start = index;
        while index < rows.len()
            && matches!(
                row_kind(&rows[index]),
                Some(RowKind::Added | RowKind::Removed)
            )
        {
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
        let RenderedRow::Code {
            kind,
            text: row_text,
            ..
        } = &rows[index]
        else {
            continue;
        };
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
        if let RenderedRow::Code { text, runs, .. } = &mut rows[*index] {
            *runs = overlay_background(text.len(), runs, &local, background);
        }
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

pub fn sub_runs(
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
        if row_kind(&rows[index]) == Some(RowKind::Context) {
            output.push(PairedRow {
                left: Some(index),
                right: Some(index),
            });
            index += 1;
            continue;
        }
        let start = index;
        while index < rows.len()
            && matches!(
                row_kind(&rows[index]),
                Some(RowKind::Added | RowKind::Removed)
            )
        {
            index += 1;
        }
        let removed = (start..index)
            .filter(|&row| row_kind(&rows[row]) == Some(RowKind::Removed))
            .collect::<Vec<_>>();
        let added = (start..index)
            .filter(|&row| row_kind(&rows[row]) == Some(RowKind::Added))
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
        if let RenderedRow::Code { text, .. } = &rows[*index] {
            output.push_str(text);
            output.push('\n');
        }
    }
    output
}

fn row_kind(row: &RenderedRow) -> Option<RowKind> {
    match row {
        RenderedRow::Gap(_) => None,
        RenderedRow::Code { kind, .. } => Some(*kind),
    }
}

fn row_anchors(rows: &[RenderedRow]) -> Vec<Option<u32>> {
    let mut anchors = vec![None; rows.len()];
    let mut following_new = None;
    for (index, row) in rows.iter().enumerate().rev() {
        if let RenderedRow::Code {
            new: Some(line), ..
        } = row
        {
            following_new = Some(*line);
        }
        anchors[index] = match row {
            RenderedRow::Code {
                new: Some(line), ..
            } => Some(*line),
            RenderedRow::Code {
                kind: RowKind::Removed,
                ..
            } => following_new,
            _ => None,
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
            if let Some(text) = rendered_row_text(row) {
                unified_columns = unified_columns.max(display_columns(text));
            }
        }
        for pair in &file.all_split {
            let columns = pair
                .left
                .and_then(|index| rendered_row_text(&file.all_rows[index]))
                .map(display_columns)
                .unwrap_or(0)
                + pair
                    .right
                    .and_then(|index| rendered_row_text(&file.all_rows[index]))
                    .map(display_columns)
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

pub fn rendered_row_text(row: &RenderedRow) -> Option<&str> {
    match row {
        RenderedRow::Code { text, .. } => Some(text),
        RenderedRow::Gap(_) => None,
    }
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
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    struct TempDir {
        path: std::path::PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "tcode-diff-model-{}-{}",
                std::process::id(),
                NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).expect("create temporary test directory");
            Self { path }
        }

        fn file(&self, name: &str, content: &str) -> std::path::PathBuf {
            let path = self.path.join(name);
            std::fs::write(&path, content).expect("write temporary test file");
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.path).expect("remove temporary test directory");
        }
    }

    fn colors() -> DiffColors {
        DiffColors {
            added_word_bg: gpui::hsla(0.3, 0.8, 0.5, 0.3),
            removed_word_bg: gpui::hsla(0., 0.8, 0.5, 0.28),
        }
    }

    fn code(kind: RowKind, old: Option<u32>, new: Option<u32>, text: &str) -> RenderedRow {
        RenderedRow::Code {
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
    fn sub_runs_clips_and_rebases() {
        let all = vec![
            (0..5, HighlightStyle::default()),
            (8..12, HighlightStyle::default()),
        ];
        assert_eq!(sub_runs(&all, 4, 10)[0].0, 0..1);
        assert_eq!(sub_runs(&all, 4, 10)[1].0, 4..6);
    }

    #[test]
    fn invisibles_remap_bytes_and_override_styles() {
        let normal = HighlightStyle::default();
        let ws = HighlightStyle {
            color: Some(gpui::hsla(0., 0., 0.5, 1.)),
            ..Default::default()
        };
        let (text, runs) = apply_invisibles("é \tx", &[(0..5, normal)], &ws);
        assert_eq!(text, "é·→x");
        assert_eq!(text.len(), 8);
        assert_eq!(runs[0].0, 0..2);
        assert_eq!(runs[1].0, 2..7);
        assert_eq!(runs[1].1, ws);
        assert_eq!(runs[2].0, 7..8);
        assert!(
            runs.iter()
                .all(|(range, _)| range.start < range.end && range.end <= text.len())
        );
        assert!(runs.windows(2).all(|pair| pair[0].0.end <= pair[1].0.start));
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
                    expandable: true
                },
                VisibleItem::Row(3)
            ]
        );
    }

    #[test]
    fn expand_up_down_and_all_update_visibility() {
        let rows = (1..=6)
            .map(|line| code(RowKind::Context, Some(line), Some(line), "line"))
            .collect();
        let mut top = file(rows, std::iter::once(2..6).collect());
        expand(&mut top, 2..6, ExpandDir::Up, 2);
        assert_eq!(top.collapsed, vec![4..6]);
        assert!(visible_unified(&top).contains(&VisibleItem::Row(1)));

        let mut bottom = top.clone();
        expand(&mut bottom, 4..6, ExpandDir::Down, 1);
        assert_eq!(bottom.collapsed, vec![4..5]);
        assert!(visible_unified(&bottom).contains(&VisibleItem::Row(4)));

        expand(&mut bottom, 4..5, ExpandDir::All, 20);
        assert!(bottom.collapsed.is_empty());
        assert_eq!(visible_unified(&bottom).len(), 6);
        assert_eq!(visible_split(&bottom).len(), 6);
    }

    #[test]
    fn split_pairing_aligns_equal_content_inside_change_block() {
        let rows = vec![
            code(RowKind::Removed, Some(1), None, "same"),
            code(RowKind::Removed, Some(2), None, "old"),
            code(RowKind::Added, None, Some(1), "same"),
            code(RowKind::Added, None, Some(2), "new"),
        ];
        let pairs = pair_rendered_rows(&rows);
        assert_eq!(
            pairs,
            vec![
                PairedRow {
                    left: Some(0),
                    right: Some(2)
                },
                PairedRow {
                    left: Some(1),
                    right: Some(3)
                }
            ]
        );
    }

    #[test]
    fn display_columns_accounts_for_tabs_and_wide_characters() {
        assert_eq!(display_columns("ab\tcd"), 6);
        assert_eq!(display_columns("a界b"), 4);
    }

    #[test]
    fn language_name_maps_extensions() {
        assert_eq!(highlight::language_name_for_path("/x/util.py"), "python");
        assert_eq!(highlight::language_name_for_path("/x/main.rs"), "rust");
        assert_eq!(highlight::language_name_for_path("/x/noext"), "text");
    }

    #[test]
    fn no_wrap_widths_cover_unified_and_split_rows() {
        let rows = vec![
            code(RowKind::Removed, Some(1), None, "short"),
            code(RowKind::Added, None, Some(1), "a much longer replacement"),
        ];
        let files = vec![file(rows, Vec::new())];
        let (unified, split) = diff_content_widths(&files);
        assert!(unified >= 24. * 8. + 106.);
        assert!(split >= (5. + 24.) * 8. + 117.);
    }

    #[test]
    fn reconstructs_matching_unified_patch_into_expandable_full_text() {
        let temp = TempDir::new();
        let new = "one\ntwo\nthree\nfour\nnew value\nsix\nseven\neight\nnine\nten\n";
        let old = "one\ntwo\nthree\nfour\nold value\nsix\nseven\neight\nnine\nten\n";
        let path = temp.file("unified.txt", new);
        let patch = "@@ -3,5 +3,5 @@\n three\n four\n-old value\n+new value\n six\n seven\n";

        let (reconstructed_old, reconstructed_new) =
            reconstruct_from_disk(&path, patch).expect("matching patch should reconstruct");
        assert_eq!(reconstructed_old, old);
        assert_eq!(reconstructed_new, new);

        let file = build_file(
            &FileDiffInput {
                path: path.to_str().expect("UTF-8 test path"),
                kind: FileChangeKind::Modify,
                old_text: Some(&reconstructed_old),
                new_text: Some(&reconstructed_new),
                patch: Some(patch),
                ignore_whitespace: false,
                show_invisibles: false,
            },
            "unified.txt".into(),
            "text",
            &HighlightTheme::default_dark(),
            &colors(),
            &HighlightStyle::default(),
        );
        assert!(file.expandable);
        assert_eq!(file.added, 1);
        assert_eq!(file.removed, 1);
        assert!(!file.collapsed.is_empty());
    }

    #[test]
    fn rejects_unified_patch_when_disk_is_stale() {
        let temp = TempDir::new();
        let path = temp.file("stale.txt", "one\nchanged again\nthree\n");
        let patch = "@@ -1,3 +1,3 @@\n one\n-old\n+new\n three\n";
        assert_eq!(reconstruct_from_disk(&path, patch), None);
    }

    #[test]
    fn reconstructs_matching_bare_write() {
        let temp = TempDir::new();
        let content = "alpha\nbeta\n";
        let path = temp.file("write.txt", content);
        assert_eq!(
            reconstruct_from_disk(&path, "+alpha\n+beta"),
            Some((String::new(), content.to_string()))
        );
    }

    #[test]
    fn reconstructs_bare_edit_with_unique_added_block() {
        let temp = TempDir::new();
        let new = "before\nnew one\nnew two\nafter\n";
        let old = "before\nold one\nold two\nafter\n";
        let path = temp.file("edit.txt", new);
        let patch = "-old one\n-old two\n+new one\n+new two";
        assert_eq!(
            reconstruct_from_disk(&path, patch),
            Some((old.to_string(), new.to_string()))
        );
    }

    #[test]
    fn rejects_bare_edit_with_ambiguous_added_block() {
        let temp = TempDir::new();
        let path = temp.file("ambiguous.txt", "new\nbetween\nnew\n");
        assert_eq!(
            reconstruct_from_disk(&path, "-old\n+new"),
            None,
            "the added block must occur exactly once"
        );
    }

    #[test]
    fn reconstruction_returns_none_for_missing_file() {
        let temp = TempDir::new();
        assert_eq!(
            reconstruct_from_disk(&temp.path.join("missing.txt"), "+content"),
            None
        );
    }

    #[test]
    fn full_text_pipeline_builds_expanded_rows_collapsed_context_and_word_runs() {
        let old = "one\ntwo\nthree\nfour\nlet x = 1;\nsix\nseven\neight\nnine\nten\n";
        let new = "one\ntwo\nthree\nfour\nlet x = 2;\nsix\nseven\neight\nnine\nten\n";
        let input = FileDiffInput {
            path: "src/test.rs",
            kind: FileChangeKind::Modify,
            old_text: Some(old),
            new_text: Some(new),
            patch: None,
            ignore_whitespace: false,
            show_invisibles: false,
        };
        let file = build_file(
            &input,
            "src/test.rs".into(),
            "rust",
            &HighlightTheme::default_dark(),
            &colors(),
            &HighlightStyle::default(),
        );

        assert_eq!(file.added, 1);
        assert_eq!(file.removed, 1);
        assert_eq!(file.all_rows.len(), 11);
        assert_eq!(file.collapsed, vec![1..2, 9..11]);
        assert!(file.expandable);
        let changed = file
            .all_rows
            .iter()
            .filter_map(|row| match row {
                RenderedRow::Code {
                    kind, text, runs, ..
                } if matches!(kind, RowKind::Added | RowKind::Removed) => Some((kind, text, runs)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(changed.len(), 2);
        for (kind, text, runs) in changed {
            let expected = match kind {
                RowKind::Added => colors().added_word_bg,
                RowKind::Removed => colors().removed_word_bg,
                RowKind::Context => unreachable!(),
            };
            assert!(runs.iter().any(|(range, style)| {
                &text[range.clone()] == if *kind == RowKind::Added { "2" } else { "1" }
                    && style.background_color == Some(expected)
            }));
        }
    }

    #[test]
    fn patch_pipeline_preserves_fixed_gap_and_applies_word_runs() {
        let input = FileDiffInput {
            path: "src/test.rs",
            kind: FileChangeKind::Modify,
            old_text: None,
            new_text: None,
            patch: Some("@@ -10,2 +10,2 @@\n-let x = 1;\n+let x = 2;\n tail"),
            ignore_whitespace: false,
            show_invisibles: false,
        };
        let file = build_file(
            &input,
            "src/test.rs".into(),
            "rust",
            &HighlightTheme::default_dark(),
            &colors(),
            &HighlightStyle::default(),
        );

        assert_eq!(file.collapsed, std::iter::once(1..10).collect::<Vec<_>>());
        assert!(!file.expandable);
        assert_eq!(
            visible_unified(&file).first(),
            Some(&VisibleItem::Gap {
                count: 9,
                expandable: false,
            })
        );
        assert!(file.all_rows[..2].iter().all(|row| match row {
            RenderedRow::Code { runs, .. } => {
                runs.iter()
                    .any(|(_, style)| style.background_color.is_some())
            }
            RenderedRow::Gap(_) => false,
        }));
    }

    #[test]
    fn split_pairing_aligns_multi_hunk_change_blocks() {
        let input = FileDiffInput {
            path: "src/test.rs",
            kind: FileChangeKind::Modify,
            old_text: None,
            new_text: None,
            patch: Some(
                "@@ -1,4 +1,4 @@\n one\n-old a\n-old b\n+new a\n two\n@@ -20,2 +20,3 @@\n tail\n+extra\n",
            ),
            ignore_whitespace: false,
            show_invisibles: false,
        };
        let file = build_file(
            &input,
            "src/test.rs".into(),
            "rust",
            &HighlightTheme::default_dark(),
            &colors(),
            &HighlightStyle::default(),
        );

        assert_eq!(file.all_split.len(), 6);
        assert_eq!(
            file.all_split[0],
            PairedRow {
                left: Some(0),
                right: Some(0)
            }
        );
        assert_eq!(
            file.all_split[1],
            PairedRow {
                left: Some(1),
                right: Some(3)
            }
        );
        assert_eq!(
            file.all_split[2],
            PairedRow {
                left: Some(2),
                right: None
            }
        );
        assert_eq!(
            file.all_split[5],
            PairedRow {
                left: None,
                right: Some(6)
            }
        );
    }
}
