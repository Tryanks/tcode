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

use std::ops::Range;
use std::path::Path;

use agent::{FileChange, FileChangeKind};
use gpui::{
    AnyElement, AppContext as _, Context, Entity, HighlightStyle, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, ScrollHandle, StatefulInteractiveElement as _,
    Styled as _, StyledText, Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Rope, Selectable as _, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    highlighter::{HighlightTheme, Language, SyntaxHighlighter},
    popover::Popover,
    v_flex,
};

use crate::app::{AppState, RightTab};
use crate::session::EntryContent;
use crate::ui::plan_panel::PlanPanel;

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

/// The tree-sitter language name for a file path's extension (falls back to
/// plain text). Uses gpui-component's [`Language`] extension table.
fn language_name(path: &str) -> &'static str {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|ext| Language::from_str(ext).name())
        .unwrap_or("text")
}

/// Highlight `src` (a full reconstructed file side) once and return the token
/// style spans in byte offsets.
fn highlight_source(
    src: &str,
    lang: &str,
    theme: &HighlightTheme,
) -> Vec<(Range<usize>, HighlightStyle)> {
    if src.is_empty() {
        return Vec::new();
    }
    let mut hl = SyntaxHighlighter::new(lang);
    hl.update(None, &Rope::from_str(src), None);
    hl.styles(&(0..src.len()), theme)
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

struct RenderedFile {
    path: String,
    kind: FileChangeKind,
    added: u32,
    removed: u32,
    rows: Vec<RenderedRow>,
}

/// Build a file's rendered rows: parse the diff, reconstruct each side's source
/// for context-aware highlighting, then attach per-line style runs.
fn render_file(change: &FileChange, cwd: &Path, theme: &HighlightTheme) -> RenderedFile {
    let display = relativize(&change.path, cwd);
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
    for (item, src) in flat.into_iter().zip(row_src.into_iter()) {
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

    RenderedFile {
        path: display,
        kind: change.kind,
        added: parsed.added,
        removed: parsed.removed,
        rows,
    }
}

/// Make `path` relative to the session cwd (trying the symlink-resolved form
/// too, as providers report canonical paths).
fn relativize(path: &str, cwd: &Path) -> String {
    let canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let p = Path::new(path);
    p.strip_prefix(cwd)
        .or_else(|_| p.strip_prefix(&canonical))
        .map(|rel| rel.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string())
}

// ---------------------------------------------------------------------------
// Panel entity
// ---------------------------------------------------------------------------

/// Cache of rendered files, invalidated when the session, selected turn, or
/// theme brightness changes (highlight colors are theme-resolved).
struct DiffCache {
    session: String,
    turn: usize,
    dark: bool,
    files: Vec<RenderedFile>,
}

pub struct DiffPanel {
    app_state: Entity<AppState>,
    /// The Plan/Tasks tab content (the other tab in this right panel).
    plan: Entity<PlanPanel>,
    vscroll: ScrollHandle,
    /// Soft-wrap toggle for long code lines (the one real toolbar button).
    wrap: bool,
    cache: Option<DiffCache>,
    _subscriptions: Vec<Subscription>,
}

impl DiffPanel {
    pub fn new(app_state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        // Soft-wrap defaults to the user's "Word wrap in diffs" setting.
        let wrap = app_state.read(cx).settings.word_wrap_diffs;
        let plan = cx.new(|cx| PlanPanel::new(app_state.clone(), cx));
        let subscriptions = vec![cx.observe(&app_state, |_, _, cx| cx.notify())];
        Self {
            app_state,
            plan,
            vscroll: ScrollHandle::new(),
            wrap,
            cache: None,
            _subscriptions: subscriptions,
        }
    }

    /// Rebuild the rendered-file cache when its key (session / turn / theme)
    /// changed. Returns whether there is anything to show.
    fn ensure_cache(&mut self, cx: &mut Context<Self>) -> bool {
        let dark = cx.theme().mode.is_dark();
        let (session, turn, changes, cwd) = {
            let state = self.app_state.read(cx);
            let Some(active) = state.active.as_ref() else {
                self.cache = None;
                return false;
            };
            let Some(turn) = state.diff_selected_turn() else {
                self.cache = None;
                return false;
            };
            let changes: Vec<FileChange> = active
                .timeline
                .entries
                .iter()
                .filter(|e| e.turn == turn)
                .filter_map(|e| match &e.content {
                    EntryContent::FileChange { changes, .. } => Some(changes.clone()),
                    _ => None,
                })
                .flatten()
                .collect();
            (
                active.meta.id.clone(),
                turn,
                changes,
                active.meta.cwd.clone(),
            )
        };

        let fresh = self.cache.as_ref().map_or(true, |c| {
            c.session != session || c.turn != turn || c.dark != dark
        });
        if fresh {
            let theme = cx.theme().highlight_theme.clone();
            let files = changes
                .iter()
                .map(|c| render_file(c, &cwd, &theme))
                .collect();
            self.cache = Some(DiffCache {
                session,
                turn,
                dark,
                files,
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
            rust_i18n::t!("plan.tab_plan")
        } else {
            rust_i18n::t!("plan.tab_tasks")
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
                .rounded(px(6.))
                .cursor_pointer()
                .text_size(px(13.))
                .font_medium()
                .when(is_active, |s| s.bg(tab_active))
                .when(!is_active, |s| s.text_color(muted).hover(|s| s.bg(cx.theme().muted)))
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
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                tab(
                    "diff-tab",
                    IconName::File,
                    rust_i18n::t!("diff.title").into_owned().into(),
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
                        rust_i18n::t!("diff.restore_width")
                    } else {
                        rust_i18n::t!("diff.expand_width")
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
                    .tooltip(rust_i18n::t!("diff.layout_soon")),
            )
            .child(
                Button::new("diff-close")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::Close)
                    .tooltip(rust_i18n::t!("diff.close"))
                    .on_click(move |_, _, cx| {
                        app2.update(cx, |state, cx| state.close_diff_panel(cx));
                    }),
            )
            .into_any_element()
    }

    // -- toolbar (turn selector + view controls) ----------------------------

    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let state = self.app_state.read(cx);
        let selected = state.diff_selected_turn();
        let turns = state.diff_turns();
        let label = match selected {
            Some(t) => rust_i18n::t!("diff.turn", count = t + 1).into_owned(),
            None => rust_i18n::t!("diff.no_changes").into_owned(),
        };
        let muted = cx.theme().muted_foreground;
        let app = self.app_state.clone();

        let trigger = Button::new("diff-turn-select").ghost().compact().child(
            h_flex()
                .gap_1p5()
                .items_center()
                .text_size(px(13.))
                .font_medium()
                .child(label)
                .child(Icon::new(IconName::ChevronDown).xsmall().text_color(muted)),
        );

        let selector =
            Popover::new("diff-turn-popover")
                .trigger(trigger)
                .content(move |_, _, cx| {
                    // Newest first.
                    let mut items = turns.clone();
                    items.reverse();
                    let mut list = v_flex().p_1().min_w(px(160.)).gap_0p5();
                    for turn in items {
                        let app = app.clone();
                        let is_sel = selected == Some(turn);
                        list = list.child(
                            h_flex()
                                .id(("diff-turn-item", turn))
                                .w_full()
                                .px_2()
                                .py_1()
                                .gap_2()
                                .items_center()
                                .rounded(px(6.))
                                .text_size(px(13.))
                                .cursor_pointer()
                                .hover(|s| s.bg(cx.theme().accent))
                                .when(is_sel, |this| this.bg(cx.theme().accent))
                                .child(
                                    div()
                                        .flex_1()
                                        .child(rust_i18n::t!("diff.turn", count = turn + 1)),
                                )
                                .when(is_sel, |this| {
                                    this.child(Icon::new(IconName::Check).xsmall())
                                })
                                .on_click({
                                    let popover = cx.entity();
                                    move |_, window, cx| {
                                        app.update(cx, |state, cx| state.set_diff_turn(turn, cx));
                                        popover.update(cx, |st, cx| st.dismiss(window, cx));
                                    }
                                }),
                        );
                    }
                    list
                });

        let wrap_on = self.wrap;
        h_flex()
            .flex_none()
            .h(px(40.))
            .w_full()
            .px_2()
            .gap_1()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(selector)
            .child(div().flex_1())
            .child(
                Button::new("diff-view-split")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::PanelLeft)
                    .tooltip(rust_i18n::t!("diff.split_unified")),
            )
            .child(
                Button::new("diff-wrap")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::Menu)
                    .selected(wrap_on)
                    .tooltip(rust_i18n::t!("diff.toggle_wrap"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.wrap = !this.wrap;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("diff-whitespace")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::Eye)
                    .tooltip(rust_i18n::t!("diff.whitespace_soon")),
            )
            .child(
                Button::new("diff-invisibles")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::CaseSensitive)
                    .tooltip(rust_i18n::t!("diff.invisibles_soon")),
            )
            .into_any_element()
    }

    // -- body ---------------------------------------------------------------

    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(cache) = self.cache.as_ref() else {
            return self.render_empty(cx);
        };
        let wrap = self.wrap;

        let mut content = v_flex().min_w_full().pb_6();
        for file in &cache.files {
            content = content.child(self.render_file_header(file, cx));
            for row in &file.rows {
                content = content.child(match row {
                    RenderedRow::Gap(n) => self.render_gap(*n, cx),
                    RenderedRow::Code {
                        kind,
                        old,
                        new,
                        text,
                        runs,
                    } => self.render_code_row(*kind, *old, *new, text, runs, wrap, cx),
                });
            }
        }

        let base = div()
            .id("diff-body")
            .flex_1()
            .min_h_0()
            .track_scroll(&self.vscroll)
            .text_size(px(12.))
            .font_family(cx.theme().mono_font_family.clone());
        let base = if wrap {
            base.overflow_y_scroll()
        } else {
            base.overflow_scroll()
        };
        base.child(content).into_any_element()
    }

    fn render_file_header(&self, file: &RenderedFile, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let kind_label = match file.kind {
            FileChangeKind::Create => Some(rust_i18n::t!("diff.created")),
            FileChangeKind::Delete => Some(rust_i18n::t!("diff.deleted")),
            FileChangeKind::Rename => Some(rust_i18n::t!("diff.renamed")),
            FileChangeKind::Modify => None,
        };
        h_flex()
            .min_w_full()
            .h(px(34.))
            .px_3()
            .gap_2()
            .items_center()
            .bg(cx.theme().secondary)
            .border_b_1()
            .border_color(cx.theme().border)
            .font_family(cx.theme().font_family.clone())
            .child(Icon::new(IconName::File).xsmall().text_color(muted))
            .child(
                div()
                    .text_size(px(12.))
                    .font_medium()
                    .child(file.path.clone()),
            )
            .when_some(kind_label, |this, label| {
                this.child(
                    div()
                        .px_1p5()
                        .rounded(px(4.))
                        .bg(cx.theme().muted)
                        .text_size(px(10.))
                        .text_color(muted)
                        .child(label),
                )
            })
            .child(div().flex_1())
            .child(
                h_flex()
                    .flex_none()
                    .gap_2()
                    .text_size(px(12.))
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
            .child(rust_i18n::t!("diff.unmodified_lines", count = count))
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_code_row(
        &self,
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

        let gutter = |n: Option<u32>| {
            div()
                .flex_none()
                .w(px(44.))
                .px_1()
                .text_right()
                .text_size(px(11.))
                .text_color(muted)
                .child(n.map(|v| v.to_string()).unwrap_or_default())
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
            .child(gutter(old))
            .child(gutter(new))
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
                    .text_size(px(14.))
                    .text_color(cx.theme().muted_foreground)
                    .child(rust_i18n::t!("diff.empty")),
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
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .border_l_1()
            .border_color(cx.theme().border)
            .child(self.render_tab_strip(cx));
        root = match tab {
            // Preview is rendered by its own panel (see ui/mod.rs); the diff
            // container only handles Diff/Plan, so treat Preview as the diff view
            // for the unreachable fallback.
            RightTab::Diff | RightTab::Preview => {
                root.child(self.render_toolbar(cx)).child(self.render_body(cx))
            }
            RightTab::Plan => root.child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(self.plan.clone()),
            ),
        };
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
