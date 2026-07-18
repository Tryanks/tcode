//! The right-side diff panel: a resizable pane that renders the file changes
//! of a selected turn as a syntax-highlighted unified diff (see docs/DESIGN.md
//! "Diff panel").
//!
//! The unified-diff parser ([`parse_unified_diff`]) is hand-written (no deps).
//! It accepts both proper unified diffs (with `@@` hunk headers, `+++`/`---`
//! file headers, space-prefixed context, and `\ No newline at end of file`
//! markers) and the header-less `+`/`-` line diffs that the Claude provider
//! emits for Write/Edit tool calls. Line rows carry both old and new line
//! numbers; the count of unmodified lines skipped between hunks drives the
//! "N unmodified lines" separator rows.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent::{FileChange, FileChangeKind};
use gpui::{
    AnyElement, AppContext as _, Context, Entity, HighlightStyle, InteractiveElement as _,
    IntoElement, ListAlignment, ListSizingBehavior, ListState, MouseButton, MouseDownEvent,
    MouseMoveEvent, ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _,
    StyledText, Subscription, Window, div, list, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Selectable as _, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    highlighter::HighlightTheme,
    input::{Input, InputState},
    popover::Popover,
    v_flex,
};

use crate::plan_panel::PlanPanel;
use crate::{highlight, material};
use tcode_core::session::{ReviewComment, ReviewSide};
use tcode_core::settings::DiffViewMode;
use tcode_runtime::app::{AppState, RightTab};
use tcode_runtime::ui_facade::{
    GitDiffResult, GitDiffScope, GitFileContent, StructuralFile, StructuralHighlight,
    StructuralSide, load_git_diff, relativize_to_workspace, run_structural_diff,
};

// ---------------------------------------------------------------------------
// Unified-diff parser
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Context,
    Added,
    Removed,
}

/// One rendered line of a diff: its side line numbers and content (the leading
/// `+`/`-`/space marker already stripped).
#[derive(Debug, Clone, PartialEq)]
pub struct DiffRow {
    pub kind: RowKind,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub text: String,
}

/// A contiguous block of changed/context lines, preceded by the count of
/// unmodified lines skipped since the previous hunk (0 for the first hunk when
/// it starts at line 1).
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

/// Parse a `@@ -a,b +c,d @@` hunk header into (old_start, new_start). Counts
/// are ignored (line cursors are advanced by the actual rows).
fn parse_hunk_header(line: &str) -> Option<(u32, u32)> {
    let rest = line.strip_prefix("@@")?;
    // "... -a,b +c,d @@ section" -> take the "-a,b +c,d" part.
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

/// Parse a unified diff string into hunks + totals, tolerating both real
/// unified diffs and the header-less `+`/`-` form.
pub fn parse_unified_diff(diff: &str) -> ParsedDiff {
    if diff.lines().any(|l| l.starts_with("@@")) {
        parse_standard(diff)
    } else {
        parse_bare(diff)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum DiffScope {
    Turn(usize),
    WorkingTree,
    Branch,
}

fn parse_standard(diff: &str) -> ParsedDiff {
    let mut out = ParsedDiff::default();
    let mut cur: Option<Hunk> = None;
    let mut seen_hunk = false;
    let mut old_cursor = 0u32;
    let mut new_cursor = 0u32;
    // Where the new-side cursor stood at the end of the previous hunk; the gap
    // before the next hunk is `next_new_start - prev_new_end`.
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
            // Skip file headers (`diff --git`, `index`, `--- a/x`, `+++ b/x`,
            // `new file mode`, `rename ...`) that precede the first hunk.
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
            // "\ No newline at end of file" — a marker for the preceding row.
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
            // A fully blank line inside a hunk is empty context.
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
            // Anything else: treat the whole line as context content.
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

/// Parse a header-less `+`/`-` diff (Claude Write/Edit): a single hunk with no
/// gap, line numbers assigned sequentially from 1 on each side.
fn parse_bare(diff: &str) -> ParsedDiff {
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

// ---------------------------------------------------------------------------
// Highlighting helpers
// ---------------------------------------------------------------------------

fn language_name(path: &str) -> &'static str {
    highlight::language_name_for_path(path)
}

/// Highlight `src` (a full reconstructed file side) once and return the token
/// style spans in byte offsets.
fn highlight_source(
    src: &str,
    lang: &str,
    theme: &HighlightTheme,
) -> Vec<(Range<usize>, HighlightStyle)> {
    highlight::highlight_source(src, lang, theme)
}

/// Slice the style spans overlapping `[start, end)` and rebase them to be
/// relative to `start` (for a single line extracted from a full-file highlight).
fn sub_runs(
    all: &[(Range<usize>, HighlightStyle)],
    start: usize,
    end: usize,
) -> Vec<(Range<usize>, HighlightStyle)> {
    all.iter()
        .filter(|(r, _)| r.start < end && r.end > start)
        .map(|(r, style)| {
            let s = r.start.max(start) - start;
            let e = r.end.min(end) - start;
            (s..e, *style)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Rendered (cached) diff model
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum RenderedRow {
    /// A "N unmodified lines" separator.
    Gap(u32),
    Code {
        kind: RowKind,
        old: Option<u32>,
        new: Option<u32>,
        text: String,
        runs: Vec<(Range<usize>, HighlightStyle)>,
    },
}

#[derive(Clone)]
struct RenderedFile {
    path: String,
    kind: FileChangeKind,
    added: u32,
    removed: u32,
    rows: Vec<RenderedRow>,
    split_rows: Vec<PairedRenderedRow>,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
pub struct SplitPair {
    pub left: Option<DiffRow>,
    pub right: Option<DiffRow>,
}

/// Align context on both sides and zip each adjacent remove/add block by
/// position. Called per hunk so change blocks never pair across hunk gaps.
#[cfg(test)]
pub fn pair_split_rows(rows: &[DiffRow]) -> Vec<SplitPair> {
    let mut paired = Vec::new();
    let mut index = 0;
    while index < rows.len() {
        if rows[index].kind == RowKind::Context {
            paired.push(SplitPair {
                left: Some(rows[index].clone()),
                right: Some(rows[index].clone()),
            });
            index += 1;
            continue;
        }
        let start = index;
        while index < rows.len() && rows[index].kind != RowKind::Context {
            index += 1;
        }
        let block = &rows[start..index];
        let removed = block
            .iter()
            .filter(|row| row.kind == RowKind::Removed)
            .cloned()
            .collect::<Vec<_>>();
        let added = block
            .iter()
            .filter(|row| row.kind == RowKind::Added)
            .cloned()
            .collect::<Vec<_>>();
        for offset in 0..removed.len().max(added.len()) {
            paired.push(SplitPair {
                left: removed.get(offset).cloned(),
                right: added.get(offset).cloned(),
            });
        }
    }
    paired
}

#[derive(Clone)]
enum PairedRenderedRow {
    Gap(u32),
    Pair {
        left: Option<(usize, RenderedRow)>,
        right: Option<(usize, RenderedRow)>,
    },
}

fn pair_rendered_rows(rows: &[RenderedRow]) -> Vec<PairedRenderedRow> {
    let mut output = Vec::new();
    let mut index = 0;
    while index < rows.len() {
        match &rows[index] {
            RenderedRow::Gap(gap) => {
                output.push(PairedRenderedRow::Gap(*gap));
                index += 1;
            }
            RenderedRow::Code {
                kind: RowKind::Context,
                ..
            } => {
                output.push(PairedRenderedRow::Pair {
                    left: Some((index, rows[index].clone())),
                    right: Some((index, rows[index].clone())),
                });
                index += 1;
            }
            RenderedRow::Code { .. } => {
                let start = index;
                while index < rows.len()
                    && matches!(
                        rows[index],
                        RenderedRow::Code {
                            kind: RowKind::Added | RowKind::Removed,
                            ..
                        }
                    )
                {
                    index += 1;
                }
                let removed = rows[start..index]
                    .iter()
                    .enumerate()
                    .filter(|row| {
                        matches!(
                            row.1,
                            RenderedRow::Code {
                                kind: RowKind::Removed,
                                ..
                            }
                        )
                    })
                    .map(|(offset, row)| (start + offset, row.clone()))
                    .collect::<Vec<_>>();
                let added = rows[start..index]
                    .iter()
                    .enumerate()
                    .filter(|row| {
                        matches!(
                            row.1,
                            RenderedRow::Code {
                                kind: RowKind::Added,
                                ..
                            }
                        )
                    })
                    .map(|(offset, row)| (start + offset, row.clone()))
                    .collect::<Vec<_>>();
                for offset in 0..removed.len().max(added.len()) {
                    output.push(PairedRenderedRow::Pair {
                        left: removed.get(offset).cloned(),
                        right: added.get(offset).cloned(),
                    });
                }
            }
        }
    }
    output
}

/// Build a file's rendered rows: parse the diff, reconstruct each side's source
/// for context-aware highlighting, then attach per-line style runs.
fn render_file(change: &FileChange, cwd: &Path, theme: &HighlightTheme) -> RenderedFile {
    let display = relativize_to_workspace(&change.path, cwd);
    let lang = language_name(&change.path);
    let parsed = change
        .diff
        .as_deref()
        .map(parse_unified_diff)
        .unwrap_or_default();

    // Reconstruct the visible new-side and old-side sources (context appears in
    // both) so multi-line constructs highlight correctly, tracking each row's
    // byte range into its source.
    let mut new_src = String::new();
    let mut old_src = String::new();
    enum Src {
        New(usize, usize),
        Old(usize, usize),
        None,
    }
    let mut row_src: Vec<Src> = Vec::new();
    enum FlatItem<'a> {
        Gap(u32),
        Row(&'a DiffRow),
    }
    let mut flat: Vec<FlatItem> = Vec::new();

    for hunk in &parsed.hunks {
        if hunk.gap_before > 0 {
            flat.push(FlatItem::Gap(hunk.gap_before));
            row_src.push(Src::None);
        }
        for row in &hunk.rows {
            let (start, end) = match row.kind {
                RowKind::Added | RowKind::Context => {
                    let s = new_src.len();
                    new_src.push_str(&row.text);
                    let e = new_src.len();
                    new_src.push('\n');
                    (s, e)
                }
                RowKind::Removed => {
                    let s = old_src.len();
                    old_src.push_str(&row.text);
                    let e = old_src.len();
                    old_src.push('\n');
                    (s, e)
                }
            };
            row_src.push(match row.kind {
                RowKind::Removed => Src::Old(start, end),
                _ => Src::New(start, end),
            });
            flat.push(FlatItem::Row(row));
        }
    }

    let new_styles = highlight_source(&new_src, lang, theme);
    let old_styles = highlight_source(&old_src, lang, theme);

    let mut rows = Vec::with_capacity(flat.len());
    for (item, src) in flat.into_iter().zip(row_src) {
        let row = match item {
            FlatItem::Gap(gap) => {
                rows.push(RenderedRow::Gap(gap));
                continue;
            }
            FlatItem::Row(row) => row,
        };
        let runs = match src {
            Src::New(s, e) => sub_runs(&new_styles, s, e),
            Src::Old(s, e) => sub_runs(&old_styles, s, e),
            Src::None => Vec::new(),
        };
        rows.push(RenderedRow::Code {
            kind: row.kind,
            old: row.old_line,
            new: row.new_line,
            text: row.text.clone(),
            runs,
        });
    }

    let split_rows = pair_rendered_rows(&rows);
    RenderedFile {
        path: display,
        kind: change.kind,
        added: parsed.added,
        removed: parsed.removed,
        rows,
        split_rows,
    }
}

fn structural_style(kind: StructuralHighlight, theme: &HighlightTheme) -> HighlightStyle {
    let key = match kind {
        StructuralHighlight::Delimiter => "punctuation",
        StructuralHighlight::Normal | StructuralHighlight::Unknown => "variable",
        StructuralHighlight::String => "string",
        StructuralHighlight::Type => "type",
        StructuralHighlight::Comment => "comment",
        StructuralHighlight::Keyword => "keyword",
        StructuralHighlight::TreeSitterError => "error",
    };
    theme.style(key).unwrap_or_default()
}

fn structural_runs(
    side: &StructuralSide,
    theme: &HighlightTheme,
) -> Vec<(Range<usize>, HighlightStyle)> {
    side.spans
        .iter()
        .filter(|span| {
            span.start < span.end
                && span.end <= side.text.len()
                && side.text.is_char_boundary(span.start)
                && side.text.is_char_boundary(span.end)
        })
        .map(|span| {
            (
                span.start..span.end,
                structural_style(span.highlight, theme),
            )
        })
        .collect()
}

fn render_structural_file(
    change: &FileChange,
    structural: &StructuralFile,
    cwd: &Path,
    theme: &HighlightTheme,
) -> RenderedFile {
    let mut rows = Vec::new();
    let mut added = 0;
    let mut removed = 0;
    for row in &structural.rows {
        if !row.changed
            && let (Some(lhs), Some(rhs)) = (&row.lhs, &row.rhs)
        {
            rows.push(RenderedRow::Code {
                kind: RowKind::Context,
                old: Some(lhs.line_number),
                new: Some(rhs.line_number),
                text: rhs.text.clone(),
                runs: Vec::new(),
            });
            continue;
        }
        if let Some(lhs) = &row.lhs {
            removed += 1;
            rows.push(RenderedRow::Code {
                kind: RowKind::Removed,
                old: Some(lhs.line_number),
                new: None,
                text: lhs.text.clone(),
                runs: structural_runs(lhs, theme),
            });
        }
        if let Some(rhs) = &row.rhs {
            added += 1;
            rows.push(RenderedRow::Code {
                kind: RowKind::Added,
                old: None,
                new: Some(rhs.line_number),
                text: rhs.text.clone(),
                runs: structural_runs(rhs, theme),
            });
        }
    }
    let split_rows = pair_rendered_rows(&rows);
    RenderedFile {
        path: relativize_to_workspace(&change.path, cwd),
        kind: change.kind,
        added,
        removed,
        rows,
        split_rows,
    }
}

// ---------------------------------------------------------------------------
// Panel entity
// ---------------------------------------------------------------------------

/// Cache of rendered files, invalidated when the session, selected turn, or
/// theme brightness changes (highlight colors are theme-resolved).
struct DiffCache {
    session: String,
    scope: DiffScope,
    revision: u64,
    dark: bool,
    mode: DiffViewMode,
    files: Vec<RenderedFile>,
    unified_items: Vec<DiffListItem>,
    split_items: Vec<DiffListItem>,
    unified_list: ListState,
    split_list: ListState,
    structural_unavailable: bool,
}

#[derive(Debug, Clone, Copy)]
enum DiffListItem {
    Header(usize),
    UnifiedRow { file: usize, row: usize },
    SplitRow { file: usize, row: usize },
}

fn build_list_items(files: &[RenderedFile]) -> (Vec<DiffListItem>, Vec<DiffListItem>) {
    let unified_capacity = files.len() + files.iter().map(|file| file.rows.len()).sum::<usize>();
    let split_capacity = files.len()
        + files
            .iter()
            .map(|file| file.split_rows.len())
            .sum::<usize>();
    let mut unified = Vec::with_capacity(unified_capacity);
    let mut split = Vec::with_capacity(split_capacity);
    for (file_index, file) in files.iter().enumerate() {
        unified.push(DiffListItem::Header(file_index));
        split.push(DiffListItem::Header(file_index));
        unified.extend((0..file.rows.len()).map(|row| DiffListItem::UnifiedRow {
            file: file_index,
            row,
        }));
        split.extend(
            (0..file.split_rows.len()).map(|row| DiffListItem::SplitRow {
                file: file_index,
                row,
            }),
        );
    }
    (unified, split)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RenderKey {
    session: String,
    scope: DiffScope,
    revision: u64,
    dark: bool,
    mode: DiffViewMode,
}

struct GitPreview {
    session: String,
    scope: DiffScope,
    base: Option<String>,
    revision: u64,
    result: GitDiffResult,
}

#[derive(Clone)]
struct CommentSelection {
    file: String,
    row_start: usize,
    row_end: usize,
    line_start: u32,
    line_end: u32,
    side: ReviewSide,
    start_index: usize,
    end_index: usize,
}

pub struct DiffPanel {
    app_state: Entity<AppState>,
    /// The Plan/Tasks tab content (the other tab in this right panel).
    plan: Entity<PlanPanel>,
    /// Soft-wrap toggle for long code lines (the one real toolbar button).
    wrap: bool,
    scopes: HashMap<String, DiffScope>,
    split: HashMap<String, bool>,
    bases: HashMap<String, String>,
    cache: Option<DiffCache>,
    git_preview: Option<GitPreview>,
    loading_key: Option<(String, DiffScope, Option<String>, u64)>,
    render_loading_key: Option<RenderKey>,
    selection: Option<CommentSelection>,
    comment_input: Option<Entity<InputState>>,
    _subscriptions: Vec<Subscription>,
}

impl DiffPanel {
    pub fn new(app_state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        // Soft-wrap defaults to the user's "Word wrap in diffs" setting.
        let wrap = app_state.read(cx).settings.word_wrap_diffs;
        let plan = cx.new(|cx| PlanPanel::new(app_state.clone(), cx));
        let subscriptions = vec![cx.observe(&app_state, |this, _, cx| {
            this.remeasure_lists();
            cx.notify();
        })];
        Self {
            app_state,
            plan,
            wrap,
            scopes: HashMap::new(),
            split: HashMap::new(),
            bases: HashMap::new(),
            cache: None,
            git_preview: None,
            loading_key: None,
            render_loading_key: None,
            selection: None,
            comment_input: None,
            _subscriptions: subscriptions,
        }
    }

    fn selected_scope(&self, state: &AppState, session: &str) -> Option<DiffScope> {
        self.scopes
            .get(session)
            .copied()
            .or_else(|| state.diff_selected_turn().map(DiffScope::Turn))
            .or(Some(DiffScope::WorkingTree))
    }

    fn remeasure_lists(&self) {
        if let Some(cache) = &self.cache {
            cache.unified_list.remeasure();
            cache.split_list.remeasure();
        }
    }

    fn request_git_preview(
        &mut self,
        session: String,
        cwd: PathBuf,
        scope: DiffScope,
        base: Option<String>,
        revision: u64,
        cx: &mut Context<Self>,
    ) {
        let runtime_scope = match scope {
            DiffScope::WorkingTree => GitDiffScope::WorkingTree,
            DiffScope::Branch => GitDiffScope::Branch,
            DiffScope::Turn(_) => return,
        };
        let key = (session.clone(), scope, base.clone(), revision);
        if self.loading_key.as_ref() == Some(&key)
            || self.git_preview.as_ref().is_some_and(|preview| {
                preview.session == session
                    && preview.scope == scope
                    && preview.base == base
                    && preview.revision == revision
            })
        {
            return;
        }
        self.loading_key = Some(key.clone());
        cx.spawn(async move |this, cx| {
            let result = tcode_runtime::blocking::unblock(cx.background_executor(), move || {
                load_git_diff(&cwd, runtime_scope, base.as_deref())
            })
            .await;
            let _ = this.update(cx, |panel, cx| {
                if panel.loading_key.as_ref() == Some(&key) {
                    panel.git_preview = Some(GitPreview {
                        session,
                        scope,
                        base: key.2.clone(),
                        revision,
                        result,
                    });
                    panel.loading_key = None;
                    panel.cache = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn request_rendered_files(
        &mut self,
        key: RenderKey,
        changes: Vec<FileChange>,
        contents: Vec<GitFileContent>,
        cwd: PathBuf,
        theme: Arc<HighlightTheme>,
        cx: &mut Context<Self>,
    ) {
        if self.render_loading_key.as_ref() == Some(&key) {
            return;
        }
        self.cache = None;
        self.render_loading_key = Some(key.clone());
        let worker_mode = key.mode;
        cx.spawn(async move |this, cx| {
            let (files, unified_items, split_items, structural_unavailable) =
                tcode_runtime::blocking::unblock(cx.background_executor(), move || {
                    let mut structural_unavailable = false;
                    let files = changes
                        .iter()
                        .map(|change| {
                            if worker_mode == DiffViewMode::Structural {
                                let content =
                                    contents.iter().find(|content| content.path == change.path);
                                if let Some((old, new)) = content.and_then(|content| {
                                    Some((content.old.as_deref()?, content.new.as_deref()?))
                                }) {
                                    match run_structural_diff(Path::new(&change.path), old, new) {
                                        Ok(structural) => {
                                            return render_structural_file(
                                                change,
                                                &structural,
                                                &cwd,
                                                &theme,
                                            );
                                        }
                                        Err(_) => structural_unavailable = true,
                                    }
                                } else {
                                    structural_unavailable = true;
                                }
                            }
                            render_file(change, &cwd, &theme)
                        })
                        .collect::<Vec<_>>();
                    let (unified_items, split_items) = build_list_items(&files);
                    (files, unified_items, split_items, structural_unavailable)
                })
                .await;
            let _ = this.update(cx, |panel, cx| {
                if panel.render_loading_key.as_ref() == Some(&key) {
                    let unified_list =
                        ListState::new(unified_items.len(), ListAlignment::Top, px(180.));
                    let split_list =
                        ListState::new(split_items.len(), ListAlignment::Top, px(180.));
                    panel.cache = Some(DiffCache {
                        session: key.session.clone(),
                        scope: key.scope,
                        revision: key.revision,
                        dark: key.dark,
                        mode: key.mode,
                        files,
                        unified_items,
                        split_items,
                        unified_list,
                        split_list,
                        structural_unavailable,
                    });
                    panel.render_loading_key = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Rebuild the rendered-file cache when its key (session / turn / theme)
    /// changed. Returns whether there is anything to show.
    fn ensure_cache(&mut self, cx: &mut Context<Self>) -> bool {
        let debug = {
            let state = self.app_state.read(cx);
            state.active.as_ref().map(|active| {
                (
                    active.meta.id.clone(),
                    state.debug_diff_scope.clone(),
                    state.debug_diff_split,
                )
            })
        };
        if let Some((session, scope, split)) = debug {
            if !self.scopes.contains_key(&session) {
                match scope.as_deref() {
                    Some("working") | Some("working-tree") => {
                        self.scopes.insert(session.clone(), DiffScope::WorkingTree);
                    }
                    Some("branch") => {
                        self.scopes.insert(session.clone(), DiffScope::Branch);
                    }
                    _ => {}
                }
            }
            if split {
                self.split.insert(session, true);
            }
        }
        let dark = cx.theme().mode.is_dark();
        let (session, scope, revision, mode, mut changes, cwd) = {
            let state = self.app_state.read(cx);
            let Some(active) = state.active.as_ref() else {
                self.cache = None;
                return false;
            };
            let session = active.meta.id.clone();
            let Some(scope) = self.selected_scope(state, &session) else {
                self.cache = None;
                return false;
            };
            let changes = match scope {
                DiffScope::Turn(turn) => active
                    .timeline
                    .turns
                    .get(turn)
                    .and_then(|turn| turn.changes.as_ref())
                    .map(|changes| changes.changes.clone())
                    .unwrap_or_default(),
                DiffScope::WorkingTree | DiffScope::Branch => Vec::new(),
            };
            (
                session,
                scope,
                state.diff_refresh_generation,
                state.settings.diff_view_mode,
                changes,
                active.meta.cwd.clone(),
            )
        };
        let mut contents = Vec::new();

        if matches!(scope, DiffScope::WorkingTree | DiffScope::Branch) {
            let base = (scope == DiffScope::Branch)
                .then(|| self.bases.get(&session).cloned())
                .flatten();
            self.request_git_preview(
                session.clone(),
                cwd.clone(),
                scope,
                base.clone(),
                revision,
                cx,
            );
            let Some(preview) = self.git_preview.as_ref().filter(|preview| {
                preview.session == session
                    && preview.scope == scope
                    && preview.base == base
                    && preview.revision == revision
            }) else {
                self.cache = None;
                return false;
            };
            changes = preview.result.changes.clone();
            contents = preview.result.contents.clone();
        }

        let fresh = self.cache.as_ref().is_none_or(|c| {
            c.session != session
                || c.scope != scope
                || c.revision != revision
                || c.dark != dark
                || c.mode != mode
        });
        if fresh {
            let theme = cx.theme().highlight_theme.clone();
            self.request_rendered_files(
                RenderKey {
                    session,
                    scope,
                    revision,
                    dark,
                    mode,
                },
                changes,
                contents,
                cwd,
                theme,
                cx,
            );
            return false;
        }
        let debug_comment = self.app_state.read(cx).debug_review_comment
            && self.app_state.read(cx).review_comments().is_empty();
        if debug_comment
            && let Some((scope, file, row_index, line, side, text)) =
                self.cache.as_ref().and_then(|cache| {
                    cache.files.iter().find_map(|file| {
                        file.rows
                            .iter()
                            .enumerate()
                            .find_map(|(index, row)| match row {
                                RenderedRow::Code {
                                    kind,
                                    old,
                                    new,
                                    text,
                                    ..
                                } => {
                                    let (line, side) = match kind {
                                        RowKind::Removed => ((*old)?, ReviewSide::Old),
                                        RowKind::Added | RowKind::Context => {
                                            ((*new)?, ReviewSide::New)
                                        }
                                    };
                                    Some((
                                        cache.scope,
                                        file.path.clone(),
                                        index,
                                        line,
                                        side,
                                        text.clone(),
                                    ))
                                }
                                RenderedRow::Gap(_) => None,
                            })
                    })
                })
        {
            let (section_id, section_title) = match scope {
                DiffScope::Turn(turn) => (format!("turn:{turn}"), format!("Turn {}", turn + 1)),
                DiffScope::WorkingTree => ("unstaged".into(), "Working tree".into()),
                DiffScope::Branch => ("branch".into(), "Branch changes".into()),
            };
            let marker = if side == ReviewSide::Old { '-' } else { '+' };
            let excerpt = format!("@@ -{line},1 +{line},1 @@\n{marker}{text}");
            let comment = ReviewComment::new(
                file,
                line,
                line,
                side,
                "Please review this line before sending.".into(),
                excerpt,
                section_id,
                section_title,
                row_index,
                row_index,
            );
            self.app_state.update(cx, |state, cx| {
                state.debug_review_comment = false;
                state.add_review_comment(comment, cx);
            });
        }
        self.cache.as_ref().is_some_and(|c| !c.files.is_empty())
    }

    // -- top strip (tab look + right icon cluster) --------------------------

    fn render_tab_strip(&self, cx: &mut Context<Self>) -> AnyElement {
        let state = self.app_state.read(cx);
        let expanded = state.diff_panel_expanded();
        let active = state.right_tab();
        // The second tab is "Plan" when a plan exists or the session is in Plan
        // mode, else "Tasks" (S1 §6).
        let plan_label = if state.plan_tab_active_label() {
            tcode_i18n::tr!("plan.tab_plan")
        } else {
            tcode_i18n::tr!("plan.tab_tasks")
        };
        let app = self.app_state.clone();
        let app2 = self.app_state.clone();
        let app_diff = self.app_state.clone();
        let app_plan = self.app_state.clone();
        let muted = cx.theme().muted_foreground;
        let tab_active = cx.theme().tab_active;

        let tab = |id: &'static str,
                   icon: IconName,
                   label: gpui::SharedString,
                   is_active: bool,
                   cx: &mut Context<Self>|
         -> gpui::Stateful<gpui::Div> {
            h_flex()
                .id(id)
                .h(px(28.))
                .px_2p5()
                .gap_1p5()
                .items_center()
                .rounded(material::radius_button())
                .cursor_pointer()
                .text_size(px(13.))
                .font_medium()
                .when(is_active, |s| s.bg(tab_active))
                .when(!is_active, |s| {
                    s.text_color(muted).hover(|s| s.bg(cx.theme().muted))
                })
                .child(Icon::new(icon).xsmall().text_color(muted))
                .child(label)
        };

        h_flex()
            .flex_none()
            .h(px(40.))
            .w_full()
            .px_2()
            .gap_1()
            .items_center()
            .child(
                tab(
                    "diff-tab",
                    IconName::File,
                    tcode_i18n::tr!("diff.title").into_owned().into(),
                    active == RightTab::Diff,
                    cx,
                )
                .on_click(move |_, _, cx| {
                    app_diff.update(cx, |state, cx| state.set_right_tab(RightTab::Diff, cx));
                }),
            )
            .child(
                tab(
                    "plan-tab",
                    IconName::Map,
                    plan_label.into_owned().into(),
                    active == RightTab::Plan,
                    cx,
                )
                .on_click(move |_, _, cx| {
                    app_plan.update(cx, |state, cx| state.set_right_tab(RightTab::Plan, cx));
                }),
            )
            // Right icon cluster: expand toggle, a layout no-op, close.
            .child(div().flex_1())
            .child(
                Button::new("diff-expand")
                    .ghost()
                    .small()
                    .compact()
                    .icon(if expanded {
                        IconName::Minimize
                    } else {
                        IconName::Maximize
                    })
                    .tooltip(if expanded {
                        tcode_i18n::tr!("diff.restore_width")
                    } else {
                        tcode_i18n::tr!("diff.expand_width")
                    })
                    .on_click(move |_, _, cx| {
                        app.update(cx, |state, cx| state.toggle_diff_expanded(cx));
                    }),
            )
            .child(
                Button::new("diff-layout")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::PanelRight)
                    .tooltip(tcode_i18n::tr!("diff.layout_soon")),
            )
            .child(
                Button::new("diff-close")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::Close)
                    .tooltip(tcode_i18n::tr!("diff.close"))
                    .on_click(move |_, _, cx| {
                        app2.update(cx, |state, cx| state.close_diff_panel(cx));
                    }),
            )
            .into_any_element()
    }

    // -- toolbar (turn selector + view controls) ----------------------------

    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let state = self.app_state.read(cx);
        let session = state
            .active
            .as_ref()
            .map(|active| active.meta.id.clone())
            .unwrap_or_default();
        let selected_scope = self.selected_scope(state, &session);
        let turns = state.diff_turns();
        let label = match selected_scope {
            Some(DiffScope::Turn(turn)) => {
                tcode_i18n::tr!("diff.turn", count = turn + 1).into_owned()
            }
            Some(DiffScope::WorkingTree) => tcode_i18n::tr!("diff.working_tree").into_owned(),
            Some(DiffScope::Branch) => tcode_i18n::tr!("diff.branch_changes").into_owned(),
            None => tcode_i18n::tr!("diff.no_changes").into_owned(),
        };
        let muted = cx.theme().muted_foreground;
        let panel = cx.entity();
        let session_selector = session.clone();

        let trigger = Button::new("diff-turn-select").ghost().compact().child(
            h_flex()
                .gap_1p5()
                .items_center()
                .text_size(px(13.))
                .font_medium()
                .child(label)
                .child(Icon::new(IconName::ChevronDown).xsmall().text_color(muted)),
        );

        let selector = Popover::new("diff-turn-popover")
            .default_open(state.debug_diff_scope_menu)
            .trigger(trigger)
            .content(move |_, _, cx| {
                let panel_for = panel.clone();
                let session_for = session_selector.clone();
                let scope_row = |id: &'static str,
                                     label: gpui::SharedString,
                                     scope: DiffScope,
                                     cx: &mut gpui::Context<
                        gpui_component::popover::PopoverState,
                    >| {
                        let panel = panel_for.clone();
                        let session = session_for.clone();
                        h_flex()
                            .id(id)
                            .flex_none()
                            .w_full()
                            .px_2()
                            .py_1()
                            .items_center()
                            .rounded(px(6.))
                            .text_size(px(13.))
                            .cursor_pointer()
                            .hover(|row| row.bg(cx.theme().list_hover))
                            .when(selected_scope == Some(scope), |row| {
                                row.bg(cx.theme().list_active)
                            })
                            .child(div().flex_1().child(label))
                            .when(selected_scope == Some(scope), |row| {
                                row.child(Icon::new(IconName::Check).xsmall())
                            })
                            .on_click({
                                let popover = cx.entity();
                                move |_, window, cx| {
                                    panel.update(cx, |this, cx| {
                                        this.scopes.insert(session.clone(), scope);
                                        this.cache = None;
                                        this.selection = None;
                                        cx.notify();
                                    });
                                    popover.update(cx, |state, cx| state.dismiss(window, cx));
                                }
                            })
                    };
                let mut list = v_flex()
                    .w_full()
                    .p_1()
                    .gap_0p5()
                    .child(scope_row(
                        "diff-scope-working",
                        tcode_i18n::tr!("diff.working_tree").into_owned().into(),
                        DiffScope::WorkingTree,
                        cx,
                    ))
                    .child(scope_row(
                        "diff-scope-branch",
                        tcode_i18n::tr!("diff.branch_changes").into_owned().into(),
                        DiffScope::Branch,
                        cx,
                    ))
                    .child(
                        div()
                            .flex_none()
                            .px_2()
                            .pt_2()
                            .pb_1()
                            .text_size(px(11.))
                            .text_color(cx.theme().muted_foreground)
                            .child(tcode_i18n::tr!("diff.turns")),
                    );
                let mut items = turns.clone();
                items.reverse();
                for turn in items {
                    let panel = panel.clone();
                    let session = session_selector.clone();
                    let is_sel = selected_scope == Some(DiffScope::Turn(turn));
                    list = list.child(
                        h_flex()
                            .id(("diff-turn-item", turn))
                            .flex_none()
                            .w_full()
                            .px_2()
                            .py_1()
                            .gap_2()
                            .items_center()
                            .rounded(px(6.))
                            .text_size(px(13.))
                            .cursor_pointer()
                            .hover(|s| s.bg(cx.theme().list_hover))
                            .when(is_sel, |this| this.bg(cx.theme().list_active))
                            .child(
                                div()
                                    .flex_1()
                                    .child(tcode_i18n::tr!("diff.turn", count = turn + 1)),
                            )
                            .when(is_sel, |this| {
                                this.child(Icon::new(IconName::Check).xsmall())
                            })
                            .on_click({
                                let popover = cx.entity();
                                move |_, window, cx| {
                                    panel.update(cx, |this, cx| {
                                        this.scopes.insert(session.clone(), DiffScope::Turn(turn));
                                        this.cache = None;
                                        this.selection = None;
                                        cx.notify();
                                    });
                                    popover.update(cx, |st, cx| st.dismiss(window, cx));
                                }
                            }),
                    );
                }
                div()
                    .id("diff-turn-list")
                    .min_w(px(190.))
                    .max_h(px(320.))
                    .overflow_y_scroll()
                    .child(list)
            })
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
            .shadow_xl()
            .rounded(material::radius_overlay());

        let wrap_on = self.wrap;
        let split_on = self.split.get(&session).copied().unwrap_or(false);
        let view_mode = self.app_state.read(cx).settings.diff_view_mode;
        let next_view_mode = match view_mode {
            DiffViewMode::Line => DiffViewMode::Structural,
            DiffViewMode::Structural => DiffViewMode::Line,
        };
        let app_state = self.app_state.clone();
        let panel_split = cx.entity();
        let session_split = session.clone();
        let mut toolbar = h_flex()
            .flex_none()
            .h(px(40.))
            .w_full()
            .px_2()
            .gap_1()
            .items_center()
            .child(selector)
            .child(div().flex_1())
            .child(
                Button::new("diff-view-mode")
                    .ghost()
                    .small()
                    .compact()
                    .label(match view_mode {
                        DiffViewMode::Line => tcode_i18n::tr!("diff.line_view"),
                        DiffViewMode::Structural => tcode_i18n::tr!("diff.structural_view"),
                    })
                    .tooltip(match next_view_mode {
                        DiffViewMode::Line => tcode_i18n::tr!("diff.switch_to_line_view"),
                        DiffViewMode::Structural => {
                            tcode_i18n::tr!("diff.switch_to_structural_view")
                        }
                    })
                    .on_click(move |_, _, cx| {
                        app_state.update(cx, |state, cx| {
                            let mut settings = state.settings.clone();
                            settings.diff_view_mode = next_view_mode;
                            state.update_settings(settings, cx);
                        });
                    }),
            )
            .child(
                Button::new("diff-view-split")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::PanelLeft)
                    .selected(split_on)
                    .tooltip(if split_on {
                        tcode_i18n::tr!("diff.unified_view")
                    } else {
                        tcode_i18n::tr!("diff.split_view")
                    })
                    .on_click(move |_, _, cx| {
                        panel_split.update(cx, |this, cx| {
                            this.split.insert(session_split.clone(), !split_on);
                            this.remeasure_lists();
                            cx.notify();
                        });
                    }),
            )
            .child(
                Button::new("diff-wrap")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::Menu)
                    .selected(wrap_on)
                    .tooltip(tcode_i18n::tr!("diff.toggle_wrap"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.wrap = !this.wrap;
                        this.remeasure_lists();
                        cx.notify();
                    })),
            )
            .child(
                Button::new("diff-whitespace")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::Eye)
                    .tooltip(tcode_i18n::tr!("diff.whitespace_soon")),
            )
            .child(
                Button::new("diff-invisibles")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::CaseSensitive)
                    .tooltip(tcode_i18n::tr!("diff.invisibles_soon")),
            );

        if selected_scope == Some(DiffScope::Branch) {
            let branches = self
                .git_preview
                .as_ref()
                .map(|preview| preview.result.branches.clone())
                .unwrap_or_default();
            let current = self
                .bases
                .get(&session)
                .cloned()
                .or_else(|| {
                    self.git_preview
                        .as_ref()
                        .and_then(|p| p.result.default_base.clone())
                })
                .unwrap_or_else(|| "HEAD".to_string());
            let panel = cx.entity();
            let session_base = session.clone();
            let trigger = Button::new("diff-base-select")
                .ghost()
                .compact()
                .label(current.clone())
                .icon(IconName::ChevronDown);
            toolbar = toolbar.child(
                Popover::new("diff-base-popover")
                    .trigger(trigger)
                    .content(move |_, _, cx| {
                        let mut list = v_flex().w_full().p_1().gap_0p5();
                        for (branch_index, branch) in branches.clone().into_iter().enumerate() {
                            let panel = panel.clone();
                            let session = session_base.clone();
                            let chosen = branch.clone();
                            let selected = branch == current;
                            list = list.child(
                                h_flex()
                                    .id(("diff-base-item", branch_index))
                                    .flex_none()
                                    .w_full()
                                    .px_2()
                                    .py_1()
                                    .rounded(px(6.))
                                    .cursor_pointer()
                                    .hover(|row| row.bg(cx.theme().list_hover))
                                    .when(selected, |row| row.bg(cx.theme().list_active))
                                    .child(div().flex_1().child(branch))
                                    .when(selected, |row| {
                                        row.child(Icon::new(IconName::Check).xsmall())
                                    })
                                    .on_click({
                                        let popover = cx.entity();
                                        move |_, window, cx| {
                                            panel.update(cx, |this, cx| {
                                                this.bases.insert(session.clone(), chosen.clone());
                                                this.cache = None;
                                                this.git_preview = None;
                                                cx.notify();
                                            });
                                            popover
                                                .update(cx, |state, cx| state.dismiss(window, cx));
                                        }
                                    }),
                            );
                        }
                        div()
                            .id("diff-base-list")
                            .min_w(px(180.))
                            .max_h(px(280.))
                            .overflow_y_scroll()
                            .child(list)
                    })
                    .bg(cx.theme().popover)
                    .border_1()
                    .border_color(cx.theme().border)
                    .shadow_xl()
                    .rounded(material::radius_overlay()),
            );
        }
        toolbar.into_any_element()
    }

    // -- body ---------------------------------------------------------------

    fn select_line(&mut self, file: String, row: usize, line: u32, side: ReviewSide, drag: bool) {
        if drag
            && let Some(selection) = self.selection.as_mut()
            && selection.file == file
            && selection.side == side
        {
            selection.row_end = row;
            selection.line_end = line;
            selection.end_index = row;
        } else {
            self.selection = Some(CommentSelection {
                file,
                row_start: row,
                row_end: row,
                line_start: line,
                line_end: line,
                side,
                start_index: row,
                end_index: row,
            });
            self.comment_input = None;
        }
        self.remeasure_lists();
    }

    fn review_excerpt(&self, selection: &CommentSelection) -> String {
        let Some(file) = self
            .cache
            .as_ref()
            .and_then(|cache| cache.files.iter().find(|file| file.path == selection.file))
        else {
            return String::new();
        };
        let start = selection.row_start.min(selection.row_end);
        let end = selection.row_start.max(selection.row_end);
        let selected = file
            .rows
            .iter()
            .enumerate()
            .filter(|(index, _)| *index >= start && *index <= end)
            .filter_map(|(_, row)| match row {
                RenderedRow::Code {
                    kind,
                    old,
                    new,
                    text,
                    ..
                } => Some((*kind, *old, *new, text)),
                RenderedRow::Gap(_) => None,
            })
            .collect::<Vec<_>>();
        let old_start = selected.iter().find_map(|(_, old, _, _)| *old).unwrap_or(0);
        let new_start = selected.iter().find_map(|(_, _, new, _)| *new).unwrap_or(0);
        let old_count = selected
            .iter()
            .filter(|(kind, ..)| *kind != RowKind::Added)
            .count();
        let new_count = selected
            .iter()
            .filter(|(kind, ..)| *kind != RowKind::Removed)
            .count();
        let mut lines = vec![format!(
            "@@ -{old_start},{old_count} +{new_start},{new_count} @@"
        )];
        lines.extend(selected.into_iter().map(|(kind, _, _, text)| {
            let marker = match kind {
                RowKind::Added => '+',
                RowKind::Removed => '-',
                RowKind::Context => ' ',
            };
            format!("{marker}{text}")
        }));
        lines.join("\n")
    }

    fn submit_comment(&mut self, cx: &mut Context<Self>) {
        let Some(selection) = self.selection.clone() else {
            return;
        };
        let Some(input) = self.comment_input.as_ref() else {
            return;
        };
        let text = input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        let (section_id, section_title) = match self.cache.as_ref().map(|cache| cache.scope) {
            Some(DiffScope::Turn(turn)) => (format!("turn:{turn}"), format!("Turn {}", turn + 1)),
            Some(DiffScope::WorkingTree) => ("unstaged".to_string(), "Working tree".to_string()),
            Some(DiffScope::Branch) => ("branch".to_string(), "Branch changes".to_string()),
            None => ("diff".to_string(), "Review".to_string()),
        };
        let comment = ReviewComment::new(
            selection.file.clone(),
            selection.line_start,
            selection.line_end,
            selection.side,
            text,
            self.review_excerpt(&selection),
            section_id,
            section_title,
            selection.start_index,
            selection.end_index,
        );
        self.app_state
            .update(cx, |state, cx| state.add_review_comment(comment, cx));
        self.selection = None;
        self.comment_input = None;
        self.remeasure_lists();
        cx.notify();
    }

    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(cache) = self.cache.as_ref() else {
            if self.loading_key.is_some() || self.render_loading_key.is_some() {
                return self.render_status(tcode_i18n::tr!("diff.loading").into_owned(), cx);
            }
            return self.render_empty(cx);
        };
        let split = self.split.get(&cache.session).copied().unwrap_or(false);
        let list_state = if split {
            cache.split_list.clone()
        } else {
            cache.unified_list.clone()
        };
        let panel = cx.entity();
        let mut rows = list(list_state, move |index, _, cx| {
            panel.update(cx, |this, cx| this.render_list_item(index, split, cx))
        })
        .flex_1()
        .min_h_0()
        .text_size(px(13.))
        .font_family(cx.theme().mono_font_family.clone());
        if self.wrap {
            rows = rows.w_full();
        } else {
            rows = rows.with_sizing_behavior(ListSizingBehavior::Infer);
        }

        let viewport = div()
            .id("diff-body")
            .flex_1()
            .min_h_0()
            .overflow_x_scroll()
            .child(rows);
        let mut content = v_flex().size_full().min_h_0().child(viewport);

        if cache.mode == DiffViewMode::Structural && cache.structural_unavailable {
            content = content.child(self.render_notice(
                tcode_i18n::tr!("diff.structural_unavailable").into_owned(),
                cx,
            ));
        }

        if let Some(preview) = self
            .git_preview
            .as_ref()
            .filter(|preview| preview.session == cache.session && preview.scope == cache.scope)
        {
            if preview.result.truncated {
                content = content
                    .child(self.render_notice(tcode_i18n::tr!("diff.truncated").into_owned(), cx));
            }
            if let Some(error) = &preview.result.error {
                content = content.child(self.render_notice(error.clone(), cx));
            }
        }

        content.into_any_element()
    }

    fn render_list_item(&self, index: usize, split: bool, cx: &mut Context<Self>) -> AnyElement {
        let Some(cache) = self.cache.as_ref() else {
            return div().into_any_element();
        };
        let item = if split {
            cache.split_items.get(index)
        } else {
            cache.unified_items.get(index)
        };
        let Some(item) = item.copied() else {
            return div().into_any_element();
        };
        match item {
            DiffListItem::Header(file_index) => {
                self.render_file_header(&cache.files[file_index], cx)
            }
            DiffListItem::UnifiedRow { file, row } => {
                let file = &cache.files[file];
                let rendered = match &file.rows[row] {
                    RenderedRow::Gap(count) => self.render_gap(*count, cx),
                    RenderedRow::Code {
                        kind,
                        old,
                        new,
                        text,
                        runs,
                    } => self.render_code_row(
                        &file.path, row, *kind, *old, *new, text, runs, self.wrap, cx,
                    ),
                };
                v_flex()
                    .min_w_full()
                    .child(rendered)
                    .children(self.render_comment_ui(&file.path, row, cx))
                    .into_any_element()
            }
            DiffListItem::SplitRow { file, row } => {
                let file = &cache.files[file];
                let rendered = match &file.split_rows[row] {
                    PairedRenderedRow::Gap(count) => self.render_gap(*count, cx),
                    PairedRenderedRow::Pair { left, right } => self.render_split_row(
                        &file.path,
                        left.as_ref(),
                        right.as_ref(),
                        self.wrap,
                        cx,
                    ),
                };
                let comment_rows = match &file.split_rows[row] {
                    PairedRenderedRow::Gap(_) => Vec::new(),
                    PairedRenderedRow::Pair { left, right } => {
                        let mut indices = left
                            .iter()
                            .map(|(index, _)| *index)
                            .chain(right.iter().map(|(index, _)| *index))
                            .collect::<Vec<_>>();
                        indices.sort_unstable();
                        indices.dedup();
                        indices
                            .into_iter()
                            .flat_map(|index| self.render_comment_ui(&file.path, index, cx))
                            .collect()
                    }
                };
                v_flex()
                    .min_w_full()
                    .child(rendered)
                    .children(comment_rows)
                    .into_any_element()
            }
        }
    }

    fn render_file_header(&self, file: &RenderedFile, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let rail = match file.kind {
            FileChangeKind::Create => Some(cx.theme().success),
            FileChangeKind::Delete => Some(cx.theme().danger),
            FileChangeKind::Rename => Some(cx.theme().info),
            FileChangeKind::Modify => None,
        };
        let kind_label = match file.kind {
            FileChangeKind::Create => Some((
                tcode_i18n::tr!("diff.created"),
                cx.theme().success.opacity(0.12),
                cx.theme().success_foreground,
            )),
            FileChangeKind::Delete => Some((
                tcode_i18n::tr!("diff.deleted"),
                cx.theme().danger.opacity(0.12),
                cx.theme().danger_foreground,
            )),
            FileChangeKind::Rename => Some((
                tcode_i18n::tr!("diff.renamed"),
                cx.theme().info.opacity(0.12),
                cx.theme().info_foreground,
            )),
            FileChangeKind::Modify => None,
        };
        h_flex()
            .min_w_full()
            .h(px(34.))
            .px_3()
            .gap_2()
            .items_center()
            .bg(cx.theme().secondary)
            .rounded(material::radius_card())
            .relative()
            .when_some(rail, |this, color| {
                this.child(
                    div()
                        .absolute()
                        .left(px(0.))
                        .top(px(6.))
                        .bottom(px(6.))
                        .w(px(2.))
                        .rounded_full()
                        .bg(color),
                )
            })
            .font_family(cx.theme().font_family.clone())
            .child(Icon::new(IconName::File).xsmall().text_color(muted))
            .child(
                div()
                    .text_size(px(13.))
                    .font_medium()
                    .child(file.path.clone()),
            )
            .when_some(kind_label, |this, (label, background, foreground)| {
                this.child(
                    div()
                        .px_1p5()
                        .rounded_full()
                        .bg(background)
                        .text_size(px(11.))
                        .text_color(foreground)
                        .child(label),
                )
            })
            .child(div().flex_1())
            .child(
                h_flex()
                    .flex_none()
                    .gap_2()
                    .text_size(px(13.))
                    .child(
                        div()
                            .text_color(cx.theme().success)
                            .child(format!("+{}", file.added)),
                    )
                    .child(
                        div()
                            .text_color(cx.theme().danger)
                            .child(format!("-{}", file.removed)),
                    ),
            )
            .into_any_element()
    }

    fn render_gap(&self, count: u32, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .min_w_full()
            .h(px(24.))
            .px_3()
            .items_center()
            .bg(cx.theme().muted)
            .text_size(px(11.))
            .text_color(cx.theme().muted_foreground)
            .font_family(cx.theme().font_family.clone())
            .child(tcode_i18n::tr!("diff.unmodified_lines", count = count))
            .into_any_element()
    }

    fn render_notice(&self, message: String, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .min_w_full()
            .px_3()
            .py_2()
            .bg(cx.theme().warning.opacity(0.12))
            .text_size(px(11.))
            .text_color(cx.theme().warning_foreground)
            .font_family(cx.theme().font_family.clone())
            .child(Icon::new(IconName::TriangleAlert).xsmall())
            .child(message)
            .into_any_element()
    }

    fn render_status(&self, message: String, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .text_color(cx.theme().muted_foreground)
            .child(message)
            .into_any_element()
    }

    fn render_comment_ui(
        &self,
        file: &str,
        row_index: usize,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut rows = self
            .app_state
            .read(cx)
            .review_comments()
            .iter()
            .filter(|comment| comment.file == file && comment.end_index() == row_index)
            .map(|comment| {
                h_flex()
                    .min_w_full()
                    .px_3()
                    .py_1p5()
                    .gap_2()
                    .relative()
                    .rounded(material::radius_card())
                    .bg(cx.theme().muted)
                    .font_family(cx.theme().font_family.clone())
                    .text_size(px(11.))
                    .child(
                        div()
                            .absolute()
                            .left(px(0.))
                            .top(px(6.))
                            .bottom(px(6.))
                            .w(px(2.))
                            .rounded_full()
                            .bg(cx.theme().primary),
                    )
                    .child(Icon::empty().path("icons/pencil.svg").xsmall())
                    .child(comment.text.clone())
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let selection = self
            .selection
            .as_ref()
            .filter(|selection| selection.file == file && selection.row_end == row_index);
        if selection.is_some() {
            if let Some(input) = &self.comment_input {
                rows.push(
                    v_flex()
                        .min_w_full()
                        .px_3()
                        .py_2()
                        .gap_2()
                        .bg(cx.theme().muted)
                        .rounded(material::radius_card())
                        .font_family(cx.theme().font_family.clone())
                        .child(Input::new(input).appearance(false))
                        .child(
                            h_flex().justify_end().child(
                                Button::new("diff-submit-comment")
                                    .primary()
                                    .small()
                                    .label(tcode_i18n::tr!("diff.submit_comment"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.submit_comment(cx);
                                    })),
                            ),
                        )
                        .into_any_element(),
                );
            } else {
                rows.push(
                    h_flex()
                        .min_w_full()
                        .px_3()
                        .py_1()
                        .bg(cx.theme().muted)
                        .rounded(material::radius_card())
                        .font_family(cx.theme().font_family.clone())
                        .child(
                            Button::new("diff-add-comment")
                                .ghost()
                                .small()
                                .label(tcode_i18n::tr!("diff.add_comment"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.comment_input = Some(cx.new(|cx| {
                                        InputState::new(window, cx).placeholder(tcode_i18n::tr!(
                                            "diff.comment_placeholder"
                                        ))
                                    }));
                                    this.remeasure_lists();
                                    cx.notify();
                                })),
                        )
                        .into_any_element(),
                );
            }
        }
        rows
    }

    fn render_split_row(
        &self,
        file: &str,
        left: Option<&(usize, RenderedRow)>,
        right: Option<&(usize, RenderedRow)>,
        wrap: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cell =
            |row: Option<&(usize, RenderedRow)>, side: ReviewSide, cx: &mut Context<Self>| {
                let row_index = row.map(|(index, _)| *index).unwrap_or_default();
                let Some(RenderedRow::Code {
                    kind,
                    old,
                    new,
                    text,
                    runs,
                }) = row.map(|(_, row)| row)
                else {
                    return div().flex_1().min_w_0().min_h(px(18.)).into_any_element();
                };
                let line = match side {
                    ReviewSide::Old => *old,
                    ReviewSide::New => *new,
                };
                self.render_split_cell(file, row_index, *kind, line, side, text, runs, wrap, cx)
            };
        h_flex()
            .min_w_full()
            .items_stretch()
            .child(cell(left, ReviewSide::Old, cx))
            .child(div().w_px().bg(cx.theme().border.opacity(0.)))
            .child(cell(right, ReviewSide::New, cx))
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_split_cell(
        &self,
        file: &str,
        row_index: usize,
        kind: RowKind,
        line: Option<u32>,
        side: ReviewSide,
        text: &str,
        runs: &[(Range<usize>, HighlightStyle)],
        wrap: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let bg = match kind {
            RowKind::Added => Some(cx.theme().success.opacity(0.13)),
            RowKind::Removed => Some(cx.theme().danger.opacity(0.12)),
            RowKind::Context => None,
        };
        let file_down = file.to_string();
        let file_move = file.to_string();
        let gutter = div()
            .flex_none()
            .w(px(42.))
            .px_1()
            .text_right()
            .text_size(px(11.))
            .text_color(cx.theme().muted_foreground)
            .cursor_pointer()
            .child(line.map(|value| value.to_string()).unwrap_or_default())
            .when_some(line, |gutter, line| {
                gutter
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                            this.select_line(file_down.clone(), row_index, line, side, false);
                            cx.notify();
                        }),
                    )
                    .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                        if event.dragging() {
                            this.select_line(file_move.clone(), row_index, line, side, true);
                            cx.notify();
                        }
                    }))
            });
        let mut code = div()
            .flex_1()
            .min_w_0()
            .px_2()
            .text_color(cx.theme().foreground)
            .child(StyledText::new(text.to_string()).with_highlights(runs.iter().cloned()));
        if !wrap {
            code = code.whitespace_nowrap();
        }
        h_flex()
            .flex_1()
            .min_w_0()
            .min_h(px(18.))
            .items_start()
            .when_some(bg, |cell, color| cell.bg(color))
            .child(gutter)
            .child(code)
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_code_row(
        &self,
        file: &str,
        row_index: usize,
        kind: RowKind,
        old: Option<u32>,
        new: Option<u32>,
        text: &str,
        runs: &[(Range<usize>, HighlightStyle)],
        wrap: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (bg, accent) = match kind {
            RowKind::Added => (
                Some(cx.theme().success.opacity(0.13)),
                Some(cx.theme().success),
            ),
            RowKind::Removed => (
                Some(cx.theme().danger.opacity(0.12)),
                Some(cx.theme().danger),
            ),
            RowKind::Context => (None, None),
        };
        let muted = cx.theme().muted_foreground;

        let gutter = |n: Option<u32>, side: ReviewSide, cx: &mut Context<Self>| {
            let file_down = file.to_string();
            let file_move = file.to_string();
            div()
                .flex_none()
                .w(px(44.))
                .px_1()
                .text_right()
                .text_size(px(11.))
                .text_color(muted)
                .child(n.map(|v| v.to_string()).unwrap_or_default())
                .cursor_pointer()
                .when_some(n, |gutter, line| {
                    gutter
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                this.select_line(file_down.clone(), row_index, line, side, false);
                                cx.notify();
                            }),
                        )
                        .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                            if event.dragging() {
                                this.select_line(file_move.clone(), row_index, line, side, true);
                                cx.notify();
                            }
                        }))
                })
        };

        let mut code = div()
            .flex_1()
            .px_2()
            .text_color(cx.theme().foreground)
            .child(StyledText::new(text.to_string()).with_highlights(runs.iter().cloned()));
        if wrap {
            code = code.min_w_0();
        } else {
            code = code.whitespace_nowrap();
        }

        h_flex()
            .min_w_full()
            .min_h(px(18.))
            .items_start()
            .border_l_2()
            .border_color(accent.unwrap_or(gpui::transparent_black()))
            .when_some(bg, |this, c| this.bg(c))
            .child(gutter(old, ReviewSide::Old, cx))
            .child(gutter(new, ReviewSide::New, cx))
            .child(code)
            .into_any_element()
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .gap_1()
            .child(
                div()
                    .text_size(px(15.))
                    .text_color(cx.theme().muted_foreground)
                    .child(tcode_i18n::tr!("diff.empty")),
            )
            .into_any_element()
    }
}

impl Render for DiffPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_cache(cx);
        let tab = self.app_state.read(cx).right_tab();
        let mut root = v_flex()
            .size_full()
            .min_w_0()
            .text_color(cx.theme().foreground)
            .child(self.render_tab_strip(cx));
        root = match tab {
            // Preview is rendered by its own panel (see ui/mod.rs); the diff
            // container only handles Diff/Plan, so treat Preview as the diff view
            // for the unreachable fallback.
            RightTab::Diff | RightTab::Preview => root
                .child(self.render_toolbar(cx))
                .child(self.render_body(cx)),
            RightTab::Plan => root.child(div().flex_1().min_h_0().child(self.plan.clone())),
        };
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn parses_bare_create_diff() {
        // Claude Write: every line prefixed with `+`, no headers.
        let diff = "+def f():\n+    return 1";
        let parsed = parse_unified_diff(diff);
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
        // Claude Edit: removed block then added block, no headers.
        let diff = "-old one\n-old two\n+new one\n+new two\n+new three";
        let parsed = parse_unified_diff(diff);
        assert_eq!(parsed.removed, 2);
        assert_eq!(parsed.added, 3);
        let rows = &parsed.hunks[0].rows;
        assert_eq!(rows[0].old_line, Some(1));
        assert_eq!(rows[1].old_line, Some(2));
        // Added lines number from 1 on the new side.
        assert_eq!(rows[2].new_line, Some(1));
        assert_eq!(rows[4].new_line, Some(3));
    }

    #[test]
    fn parses_multi_hunk_with_gaps_create_edit_and_no_newline() {
        // Two hunks with a gap between; a `\ No newline` marker; file headers.
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
        // First hunk starts at line 1: no leading gap.
        assert_eq!(parsed.hunks[0].gap_before, 0);
        // Second hunk: new side jumped from 5 (end of hunk 1) to 21 => gap 16.
        assert_eq!(parsed.hunks[1].gap_before, 16);
        assert_eq!(parsed.added, 3);
        assert_eq!(parsed.removed, 2);

        // Context/added/removed numbering in the first hunk.
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

        // The `\ No newline` marker produced no extra row.
        assert_eq!(parsed.hunks[1].rows.len(), 3);
    }

    #[test]
    fn gap_before_first_hunk_when_not_at_top() {
        let diff = "@@ -10,2 +10,3 @@\n ctx\n+added\n more";
        let parsed = parse_unified_diff(diff);
        // First shown new line is 10 => 9 unmodified lines above.
        assert_eq!(parsed.hunks[0].gap_before, 9);
    }

    #[test]
    fn hunk_header_without_counts_parses() {
        let diff = "@@ -1 +1 @@\n-a\n+b";
        let parsed = parse_unified_diff(diff);
        assert_eq!(parsed.hunks.len(), 1);
        assert_eq!(parsed.added, 1);
        assert_eq!(parsed.removed, 1);
    }

    #[test]
    fn sub_runs_clips_and_rebases() {
        let all = vec![
            (0..5, HighlightStyle::default()),
            (8..12, HighlightStyle::default()),
        ];
        // Row occupies bytes 4..10 of the source.
        let runs = sub_runs(&all, 4, 10);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].0, 0..1); // 4..5 -> 0..1
        assert_eq!(runs[1].0, 4..6); // 8..10 -> 4..6
    }

    #[test]
    fn language_name_maps_extensions() {
        assert_eq!(language_name("/x/util.py"), "python");
        assert_eq!(language_name("/x/main.rs"), "rust");
        assert_eq!(language_name("/x/noext"), "text");
    }

    #[test]
    fn split_pairing_aligns_multi_hunk_change_blocks() {
        let parsed = parse_unified_diff(
            "@@ -1,4 +1,4 @@\n one\n-old a\n-old b\n+new a\n two\n@@ -20,2 +20,3 @@\n tail\n+extra\n",
        );
        assert_eq!(parsed.hunks.len(), 2);
        let first = pair_split_rows(&parsed.hunks[0].rows);
        assert_eq!(first.len(), 4);
        assert_eq!(first[0].left.as_ref().unwrap().kind, RowKind::Context);
        assert_eq!(first[1].left.as_ref().unwrap().text, "old a");
        assert_eq!(first[1].right.as_ref().unwrap().text, "new a");
        assert!(first[2].right.is_none());
        let second = pair_split_rows(&parsed.hunks[1].rows);
        assert_eq!(second.len(), 2);
        assert!(second[1].left.is_none());
        assert_eq!(second[1].right.as_ref().unwrap().text, "extra");
    }

    #[test]
    fn large_diff_builds_virtual_list_models_without_row_elements() {
        let rows = (1..=5_000)
            .map(|line| RenderedRow::Code {
                kind: RowKind::Added,
                old: None,
                new: Some(line),
                text: format!("let value_{line} = {line};"),
                runs: Vec::new(),
            })
            .collect::<Vec<_>>();
        let split_rows = pair_rendered_rows(&rows);
        let files = vec![RenderedFile {
            path: "src/large.rs".into(),
            kind: FileChangeKind::Modify,
            added: 5_000,
            removed: 0,
            rows,
            split_rows,
        }];

        let (unified, split) = build_list_items(&files);

        assert_eq!(unified.len(), 5_001);
        assert_eq!(split.len(), 5_001);
        assert!(matches!(unified[0], DiffListItem::Header(0)));
        assert!(matches!(
            unified[5_000],
            DiffListItem::UnifiedRow {
                file: 0,
                row: 4_999
            }
        ));
    }

    #[gpui::test]
    fn virtual_list_constructs_only_the_large_diff_viewport(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let cx = cx.add_empty_window();
        let constructions = Arc::new(AtomicUsize::new(0));
        let state = ListState::new(5_001, ListAlignment::Top, px(180.));

        struct TestList {
            state: ListState,
            constructions: Arc<AtomicUsize>,
        }
        impl Render for TestList {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let observed = self.constructions.clone();
                list(self.state.clone(), move |_, _, _| {
                    observed.fetch_add(1, Ordering::Relaxed);
                    div().h(px(18.)).w_full().into_any_element()
                })
                .size_full()
            }
        }
        let view_constructions = constructions.clone();

        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(px(900.), px(720.)),
            move |_, cx| {
                cx.new(|_| TestList {
                    state,
                    constructions: view_constructions,
                })
                .into_any_element()
            },
        );

        let count = constructions.load(Ordering::Relaxed);
        assert!(
            count < 100,
            "expected viewport-only construction for 5,001 items, got {count}"
        );
    }

    #[test]
    fn recorded_structural_fixture_maps_to_native_rows() {
        let before = include_str!("../../services/tests/fixtures/difftastic_0_69_0_before.rs");
        let after = include_str!("../../services/tests/fixtures/difftastic_0_69_0_after.rs");
        let json = include_str!("../../services/tests/fixtures/difftastic_0_69_0_rust.json");
        let structural = tcode_runtime::ui_facade::parse_difft_json(json, before, after).unwrap();
        let change = FileChange {
            path: "/workspace/example.rs".into(),
            kind: FileChangeKind::Modify,
            diff: None,
        };
        let theme = HighlightTheme::default_dark();

        let rendered =
            render_structural_file(&change, &structural, Path::new("/workspace"), &theme);

        assert_eq!(rendered.path, "example.rs");
        assert_eq!((rendered.added, rendered.removed), (4, 4));
        assert_eq!(rendered.rows.len(), 12);
        assert_eq!(rendered.split_rows.len(), 8);
        let first = rendered.rows.first().unwrap();
        assert!(matches!(
            first,
            RenderedRow::Code {
                kind: RowKind::Removed,
                old: Some(1),
                new: None,
                runs,
                ..
            } if !runs.is_empty()
        ));
    }
}
