//! The edit window: one row per file an edit touched, opening on its diff.
//!
//! A `FileChange` carries each file's diff from its harness. An edit made as
//! a tool call (an imported Claude Code `Edit`/`Write`, Codex's imported
//! `apply_patch`) carries only what it asked for: its replaced and replacing
//! text, the content it wrote, or its patch. The window builds the same diff
//! from that, without line numbers where the call never said which lines.

use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use agent::{FileChange, FileChangeKind, ItemContent, ItemStatus};
use gpui::{
    AnyElement, App, Axis, ElementId, HighlightStyle, Hsla, InteractiveElement as _,
    IntoElement as _, ListHorizontalSizingBehavior, ListSizingBehavior, ParentElement as _,
    SharedString, Styled as _, StyledText, UniformListScrollHandle, div,
    prelude::FluentBuilder as _, px, uniform_list,
};
use gpui_base::{ScrollableMask, h_flex};
use serde::Deserialize;
use tcode_core::session::{EntryContent, TimelineEntry};

use super::window::{Argument, Chip, Header, Verb};
use super::{Call, copy};
use crate::chat::model::{LiveEditRow, live_edit_counts, live_edit_rows};
use crate::diff::model::{DiffColors, FileDiffInput, RenderedRow, build_file, display_columns};
use crate::diff::parse::{RowKind, parse_hunk_header};
use crate::icon::IconName;
use crate::theme::{ActiveTheme as _, HighlightTheme};
use crate::typography::TextRole as _;

/// One file an edit touched.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::chat) struct EditedFile {
    pub(in crate::chat) row: LiveEditRow,
    /// Whether the diff's line numbers are the file's: a diff built from a
    /// replaced string knows its lines only relative to the string.
    pub(in crate::chat) numbered: bool,
}

/// An edit: the files it touched, how it stands, and the line a failure is
/// told by.
pub(in crate::chat) struct Edit {
    pub(in crate::chat) files: Vec<EditedFile>,
    pub(in crate::chat) status: ItemStatus,
    pub(in crate::chat) failure: Option<String>,
    /// A harness's own diff, which opens by itself while it is the latest.
    pub(in crate::chat) live: bool,
}

/// The edit `entry` made: a file change, or a call to an edit tool whose
/// input is an edit's shape.
pub(in crate::chat) fn edit(entry: &TimelineEntry, cwd: &Path) -> Option<Edit> {
    match &entry.content {
        EntryContent::Item(ItemContent::FileChange { changes, status }) => Some(Edit {
            files: live_edit_rows(changes, cwd)
                .into_iter()
                .map(|row| EditedFile {
                    numbered: row.diff.as_deref().is_some_and(|diff| {
                        diff.lines().any(|line| parse_hunk_header(line).is_some())
                    }),
                    row,
                })
                .collect(),
            status: *status,
            failure: None,
            live: true,
        }),
        EntryContent::Item(ItemContent::ToolCall { .. }) => {
            let call = Call::of(entry)?;
            let files = tool_edit(&call)?;
            let failure = (call.status == ItemStatus::Failed)
                .then(|| call.output.and_then(super::window::first_line))
                .flatten();
            Some(Edit {
                files: files
                    .into_iter()
                    .map(|(change, numbered)| EditedFile {
                        row: LiveEditRow {
                            path: super::display_path(&change.path, cwd),
                            kind: change.kind,
                            counts: live_edit_counts(change.diff.as_deref()),
                            diff: change.diff,
                        },
                        numbered,
                    })
                    .collect(),
                status: call.status,
                failure,
                live: false,
            })
        }
        _ => None,
    }
}

const REPLACE: &[&str] = &[
    "edit",
    "multiedit",
    "str_replace",
    "str_replace_editor",
    "search_replace",
    "edit_file",
    "replace",
];
const WRITE: &[&str] = &["write", "write_file", "create_file"];
const PATCH: &[&str] = &["apply_patch"];

#[derive(Deserialize)]
struct Replace<'a> {
    #[serde(
        borrow,
        alias = "path",
        alias = "filePath",
        alias = "target_file",
        alias = "absolute_path"
    )]
    file_path: &'a str,
    #[serde(flatten, borrow)]
    change: Option<Strings<'a>>,
    #[serde(default, borrow)]
    edits: Vec<Strings<'a>>,
}

#[derive(Deserialize)]
struct Strings<'a> {
    #[serde(borrow, alias = "oldString", alias = "oldText", alias = "old_str")]
    old_string: &'a str,
    #[serde(borrow, alias = "newString", alias = "newText", alias = "new_str")]
    new_string: &'a str,
}

#[derive(Deserialize)]
struct Write<'a> {
    #[serde(
        borrow,
        alias = "path",
        alias = "filePath",
        alias = "target_file",
        alias = "absolute_path"
    )]
    file_path: &'a str,
    #[serde(borrow, alias = "contents", alias = "fileText", alias = "file_text")]
    content: &'a str,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Patch {
    Text(String),
    Field {
        #[serde(alias = "input")]
        patch: String,
    },
}

/// The files a tool call edited, each with its diff and whether that diff
/// carries the file's line numbers.
fn tool_edit(call: &Call<'_>) -> Option<Vec<(FileChange, bool)>> {
    if call.named(REPLACE)
        && let Some(input) = call.input::<Replace<'_>>()
    {
        let pairs: Vec<&Strings<'_>> = input.change.iter().chain(&input.edits).collect();
        if pairs.is_empty() {
            return None;
        }
        let diff = pairs
            .iter()
            .map(|pair| replacement_diff(pair.old_string, pair.new_string))
            .collect::<String>();
        return Some(vec![(
            FileChange {
                path: input.file_path.to_owned(),
                kind: FileChangeKind::Modify,
                diff: Some(diff),
            },
            false,
        )]);
    }
    if call.named(WRITE)
        && let Some(input) = call.input::<Write<'_>>()
    {
        return Some(vec![(
            FileChange {
                path: input.file_path.to_owned(),
                kind: FileChangeKind::Create,
                diff: Some(added_diff(input.content)),
            },
            true,
        )]);
    }
    if call.named(PATCH)
        && let Some(patch) = call.input::<Patch>()
    {
        let text = match patch {
            Patch::Text(text) | Patch::Field { patch: text } => text,
        };
        return patch_files(&text);
    }
    None
}

/// The diff of replacing `old` with `new`, its hunks numbered from the
/// replaced text's first line.
fn replacement_diff(old: &str, new: &str) -> String {
    similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .to_string()
}

/// A whole file's content as added lines.
fn added_diff(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let mut diff = format!("@@ -0,0 +1,{} @@\n", lines.len());
    for line in lines {
        diff.push('+');
        diff.push_str(line);
        diff.push('\n');
    }
    diff
}

/// The files a patch edits: a Git unified diff, or the patch grammar Codex
/// speaks (`*** Begin Patch`, `*** Update File: …`).
fn patch_files(text: &str) -> Option<Vec<(FileChange, bool)>> {
    if text.lines().any(|line| line.starts_with("diff --git ")) {
        return agent::file_changes_from_unified_diff(text)
            .ok()
            .map(|changes| changes.into_iter().map(|change| (change, true)).collect());
    }
    if !text.trim_start().starts_with("*** Begin Patch") {
        return None;
    }
    let mut files: Vec<(FileChange, bool)> = Vec::new();
    let mut moves: Vec<(usize, String)> = Vec::new();
    let mut hunk: Vec<&str> = Vec::new();
    let flush = |files: &mut Vec<(FileChange, bool)>, hunk: &mut Vec<&str>| {
        if let Some((file, _)) = files.last_mut()
            && !hunk.is_empty()
        {
            let diff = file.diff.get_or_insert_with(String::new);
            if file.kind == FileChangeKind::Create {
                diff.push_str(&added_diff(
                    &hunk
                        .iter()
                        .map(|line| line.strip_prefix('+').unwrap_or(line))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ));
            } else {
                let old = hunk.iter().filter(|line| !line.starts_with('+')).count();
                let new = hunk.iter().filter(|line| !line.starts_with('-')).count();
                diff.push_str(&format!("@@ -1,{old} +1,{new} @@\n"));
                for line in hunk.iter() {
                    diff.push_str(line);
                    diff.push('\n');
                }
            }
        }
        hunk.clear();
    };
    for line in text.lines() {
        let file = |kind, path: &str| {
            (
                FileChange {
                    path: path.trim().to_owned(),
                    kind,
                    diff: None,
                },
                // A new file's lines are its own; an update's hunks name
                // no lines.
                kind == FileChangeKind::Create,
            )
        };
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            flush(&mut files, &mut hunk);
            files.push(file(FileChangeKind::Create, path));
        } else if let Some(path) = line.strip_prefix("*** Update File: ") {
            flush(&mut files, &mut hunk);
            files.push(file(FileChangeKind::Modify, path));
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            flush(&mut files, &mut hunk);
            files.push(file(FileChangeKind::Delete, path));
        } else if let Some(path) = line.strip_prefix("*** Move to: ") {
            if let Some(index) = files.len().checked_sub(1) {
                files[index].0.kind = FileChangeKind::Rename;
                moves.push((index, path.trim().to_owned()));
            }
        } else if line.starts_with("@@") || line.starts_with("***") {
            flush(&mut files, &mut hunk);
        } else if !files.is_empty() {
            hunk.push(line);
        }
    }
    flush(&mut files, &mut hunk);
    // Codex states a move the way its own file changes do.
    for (index, to) in moves {
        let diff = files[index].0.diff.get_or_insert_with(String::new);
        diff.push_str(&format!("\n\nMoved to: {to}"));
    }
    (!files.is_empty()).then_some(files)
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
