//! The edit window: one row per file an edit touched, opening on its diff.
//!
//! An edit is a `FileChange`, which carries each file's diff from its
//! harness.

use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use agent::{FileChangeKind, ItemContent, ItemStatus};
use gpui::{
    AnyElement, App, Axis, ElementId, HighlightStyle, Hsla, InteractiveElement as _,
    IntoElement as _, ListHorizontalSizingBehavior, ListSizingBehavior, ParentElement as _,
    SharedString, Styled as _, StyledText, UniformListScrollHandle, div,
    prelude::FluentBuilder as _, px, uniform_list,
};
use gpui_base::{ScrollableMask, h_flex};
use tcode_core::session::{EntryContent, TimelineEntry};

use super::copy;
use super::window::{Argument, Chip, Header, Verb};
use crate::chat::model::{LiveEditRow, live_edit_rows};
use crate::diff::model::{DiffColors, FileDiffInput, RenderedRow, build_file, display_columns};
use crate::diff::parse::{RowKind, parse_hunk_header};
use crate::icon::IconName;
use crate::theme::{ActiveTheme as _, HighlightTheme};
use crate::typography::TextRole as _;

/// One file an edit touched.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::chat) struct EditedFile {
    pub(in crate::chat) row: LiveEditRow,
    /// Whether the diff's line numbers are the file's: a diff whose hunks
    /// have no header knows no lines.
    pub(in crate::chat) numbered: bool,
}

/// An edit: the files it touched and how it stands.
pub(in crate::chat) struct Edit {
    pub(in crate::chat) files: Vec<EditedFile>,
    pub(in crate::chat) status: ItemStatus,
}

/// The edit `entry` made, when it is a file change.
pub(in crate::chat) fn edit(entry: &TimelineEntry, cwd: &Path) -> Option<Edit> {
    let EntryContent::Item(ItemContent::FileChange { changes, status }) = &entry.content else {
        return None;
    };
    Some(Edit {
        files: live_edit_rows(changes, cwd)
            .into_iter()
            .map(|row| EditedFile {
                numbered: row
                    .diff
                    .as_deref()
                    .is_some_and(|diff| diff.lines().any(|line| parse_hunk_header(line).is_some())),
                row,
            })
            .collect(),
        status: *status,
    })
}

/// The header of one edited file's window.
pub(in crate::chat) fn file_header(file: &EditedFile) -> Header {
    let (icon, verb) = match file.row.kind {
        FileChangeKind::Create => (IconName::FileText, Verb("create")),
        FileChangeKind::Modify => (IconName::Pencil, Verb("edit")),
        FileChangeKind::Delete => (IconName::Trash, Verb("delete")),
        FileChangeKind::Rename => (IconName::Replace, Verb("rename")),
    };
    Header::new(icon, verb)
        .argument(Argument::mono(file.row.path.clone()))
        .chip(
            file.row
                .counts
                .map(|(added, deleted)| Chip::Lines { added, deleted }),
        )
}

/// What an edited file's window copies: its path and its diff.
pub(in crate::chat) fn file_copy_items(file: &EditedFile) -> Vec<(String, String)> {
    let mut items = Vec::new();
    copy(&mut items, "chat.copy_path", &file.row.path);
    copy(
        &mut items,
        "tools.copy_diff",
        file.row.diff.as_deref().unwrap_or_default(),
    );
    items
}

/// The built inline diffs of expanded live file edits, by row key.
///
/// Building one diffs and syntax-highlights the whole patch, while its row
/// renders every frame and the patch changes only when the provider reports
/// more of it. An entry is rebuilt when its path, patch or theme changes, and
/// kept only while its row renders it: [`Self::sweep`] drops every entry the
/// frame before did not render, as GPUI drops the element state of an element
/// a frame leaves out, so a diff scrolled or folded away is built again.
pub(in crate::chat) struct InlineDiffCache {
    diffs: HashMap<String, InlineDiff>,
}

struct InlineDiff {
    path: String,
    numbered: bool,
    kind: FileChangeKind,
    patch: Option<String>,
    theme: InlineDiffTheme,
    rows: Rc<[RenderedRow]>,
    widest: usize,
    scroll: UniformListScrollHandle,
    rendered: bool,
}

struct InlineDiffTheme {
    highlight: Arc<HighlightTheme>,
    added: Hsla,
    removed: Hsla,
}

impl InlineDiffTheme {
    fn from_theme(cx: &App) -> Self {
        Self {
            highlight: cx.theme().highlight_theme.clone(),
            added: cx.theme().success,
            removed: cx.theme().danger,
        }
    }

    fn matches(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.highlight, &other.highlight)
            && self.added == other.added
            && self.removed == other.removed
    }
}

impl InlineDiffCache {
    pub(in crate::chat) fn new() -> Self {
        Self {
            diffs: HashMap::new(),
        }
    }

    pub(in crate::chat) fn clear(&mut self) {
        self.diffs.clear();
    }

    /// Call once per frame, before the rows render.
    pub(in crate::chat) fn sweep(&mut self) {
        self.diffs
            .retain(|_, diff| std::mem::take(&mut diff.rendered));
    }

    pub(in crate::chat) fn render(&mut self, key: &str, file: &EditedFile, cx: &App) -> AnyElement {
        let row = &file.row;
        let theme = InlineDiffTheme::from_theme(cx);
        let current = self.diffs.get(key).is_some_and(|diff| {
            diff.path == row.path
                && diff.numbered == file.numbered
                && diff.kind == row.kind
                && diff.patch == row.diff
                && diff.theme.matches(&theme)
        });
        if !current {
            let rendered = build_file(
                &FileDiffInput {
                    path: &row.path,
                    kind: row.kind,
                    old_text: None,
                    new_text: None,
                    patch: row.diff.as_deref(),
                    ignore_whitespace: false,
                    show_invisibles: false,
                },
                row.path.clone(),
                crate::highlight::language_name_for_path(&row.path),
                &theme.highlight,
                &DiffColors {
                    added_word_bg: theme.added.opacity(0.30),
                    removed_word_bg: theme.removed.opacity(0.28),
                },
                &HighlightStyle::default(),
            );
            let widest = rendered
                .all_rows
                .iter()
                .enumerate()
                .max_by_key(|(_, row)| display_columns(&row.text))
                .map_or(0, |(index, _)| index);
            let scroll = self
                .diffs
                .remove(key)
                .map_or_else(UniformListScrollHandle::new, |diff| diff.scroll);
            self.diffs.insert(
                key.to_string(),
                InlineDiff {
                    path: row.path.clone(),
                    numbered: file.numbered,
                    kind: row.kind,
                    patch: row.diff.clone(),
                    theme,
                    rows: rendered.all_rows.into(),
                    widest,
                    scroll,
                    rendered: false,
                },
            );
        }
        let diff = self.diffs.get_mut(key).expect("built above");
        diff.rendered = true;
        render_inline_diff(key, diff, cx)
    }
}

fn render_inline_diff(key: &str, diff: &InlineDiff, cx: &App) -> AnyElement {
    let id = ElementId::from(SharedString::from(format!("file-edit-diff-{key}")));
    let scroll = diff.scroll.0.borrow().base_handle.clone();
    let rows = diff.rows.clone();
    let numbered = diff.numbered;
    let lines = uniform_list(id.clone(), rows.len(), move |range, _, cx| {
        range
            .map(|index| render_inline_diff_row(index, &rows[index], numbered, cx))
            .collect::<Vec<_>>()
    })
    .track_scroll(&diff.scroll)
    .with_sizing_behavior(ListSizingBehavior::Infer)
    .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
    .with_width_from_item(Some(diff.widest))
    // The masks move the offset and the list only clips: GPUI's own wheel
    // handling would move the timeline with it and map vertical input onto
    // the horizontal axis of a diff too short to scroll vertically.
    .overflow_hidden()
    .w_full()
    .max_h(px(240.));

    let _ = cx;
    div()
        .relative()
        .w_full()
        .min_w_0()
        .child(lines)
        .child(ScrollableMask::new(Axis::Vertical, &scroll).id(id.clone()))
        .child(ScrollableMask::new(Axis::Horizontal, &scroll).id(id))
        .debug_selector(|| "file-edit-diff".into())
        .into_any_element()
}

fn render_inline_diff_row(index: usize, row: &RenderedRow, numbered: bool, cx: &App) -> AnyElement {
    let (background, accent) = match row.kind {
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
    let gutter = |line: Option<u32>| {
        div()
            .flex_none()
            .w(px(36.))
            .px_1()
            .text_right()
            .text_secondary(false, cx)
            .child(line.map(|line| line.to_string()).unwrap_or_default())
    };

    h_flex()
        .debug_selector(move || format!("file-edit-diff-line-{index}"))
        .min_w_full()
        .font_family(cx.theme().mono_font_family.clone())
        .text_token(&cx.theme().tokens.typography.xs)
        .items_start()
        .border_l_2()
        .border_color(accent.unwrap_or(gpui::transparent_black()))
        .when_some(background, |line, background| line.bg(background))
        .when(numbered, |line| {
            line.child(
                gutter(row.old).debug_selector(move || format!("file-edit-diff-gutter-{index}")),
            )
            .child(gutter(row.new))
        })
        .child(
            div()
                .flex_1()
                .px_2()
                .whitespace_nowrap()
                .text_color(cx.theme().foreground)
                .child(StyledText::new(row.text.clone()).with_highlights(row.runs.iter().cloned())),
        )
        .into_any_element()
}
