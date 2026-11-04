use std::path::Path;

use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::menu::ContextMenuExt as _;
use agent::{ChangeCompleteness, FileChange};
use gpui::{
    AnyElement, App, ClickEvent, ElementId, InteractiveElement as _, IntoElement as _,
    ParentElement as _, Role, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    div, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};

use super::super::model::diff_stats;

/// How long a row drawn here takes to fold in or out, or to move to a new place.
const ROWS: std::time::Duration = std::time::Duration::from_millis(200);

pub(crate) type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

pub(crate) struct ChangedFilesHandlers {
    pub(crate) view_diff: ClickHandler,
    pub(crate) toggle_more: ClickHandler,
    pub(crate) open_files: Vec<ClickHandler>,
}

/// Finished-turn file summary with diff counts and file actions.
#[allow(clippy::too_many_arguments)]
pub(crate) fn changed_files(
    index: usize,
    cwd: &Path,
    changes: &[FileChange],
    completeness: ChangeCompleteness,
    show_all: bool,
    handlers: ChangedFilesHandlers,
    motion: &crate::motion::owned::Owned,
    cx: &App,
) -> AnyElement {
    let ChangedFilesHandlers {
        view_diff,
        toggle_more,
        open_files,
    } = handlers;

    let (total_add, total_del): (u32, u32) = changes
        .iter()
        .map(|change| diff_stats(change.diff.as_deref()))
        .fold(
            (0, 0),
            |(added, deleted), (change_added, change_deleted)| {
                (added + change_added, deleted + change_deleted)
            },
        );

    let header = h_flex()
        .w_full()
        .px_1()
        .gap_2()
        .items_center()
        .child(
            h_flex()
                .flex_1()
                .min_w_0()
                .gap_1p5()
                .items_center()
                .text_caption(cx)
                .child(crate::tr!("chat.changed_files", count = changes.len()))
                .when(completeness == ChangeCompleteness::Partial, |header| {
                    header.child(crate::tr!("chat.changed_files_partial"))
                })
                .child("·")
                .child(
                    div()
                        .text_color(cx.theme().success)
                        .child(format!("+{total_add}")),
                )
                .child(
                    div()
                        .text_color(cx.theme().danger)
                        .child(format!("-{total_del}")),
                ),
        )
        .child(
            quiet_control(
                ("view-diff", index),
                crate::tr!("chat.view_diff").into_owned().into(),
                view_diff,
                cx,
            )
            .motion(motion)
            .debug_selector(move || format!("view-diff-{index}"))
            .tooltip(crate::tr!("chat.view_diff_tooltip").into_owned()),
        );

    let mut content = v_flex().w_full().gap_1p5().pt(px(2.)).child(header);

    let visible = if show_all {
        changes.len()
    } else {
        changes.len().min(3)
    };
    // Showing them all adds chips to a row that wraps: each widens in, the
    // control glides to the row's new end, and the row's height glides to
    // the lines it gained; showing fewer takes them back.
    let mut chips: Vec<(SharedString, Chip)> = changes
        .iter()
        .take(visible)
        .map(|change| {
            let display = crate::workspace_walk::relativize_to_workspace(&change.path, cwd);
            let name = Path::new(&display)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| display.clone());
            let (added, deleted) = diff_stats(change.diff.as_deref());
            (
                SharedString::from(format!("file-{}", change.path)),
                Chip::File {
                    name,
                    display,
                    path: change.path.clone(),
                    added,
                    deleted,
                },
            )
        })
        .collect();
    if changes.len() > 3 {
        chips.push((
            SharedString::from("more"),
            Chip::More(
                if show_all {
                    crate::tr!("chat.show_fewer_files")
                } else {
                    crate::tr!("chat.more_files", count = changes.len() - 3)
                }
                .into_owned()
                .into(),
            ),
        ));
    }
    let mut handlers: std::collections::HashMap<SharedString, ClickHandler> = chips
        .iter()
        .map(|(key, _)| key.clone())
        .zip(open_files)
        .collect();
    handlers.insert(SharedString::from("more"), toggle_more);
    let count = chips.len() as u64;
    let cwd = cwd.to_path_buf();
    let motion = motion.clone();
    let body = crate::motion::Sampled::new(move |window, cx| {
        let glide = motion.glide_state(format!("changed-files-glide-{index}"));
        let row = crate::motion::keyed::keyed_owned(
            &motion,
            format!("changed-files-{index}"),
            crate::motion::keyed::Fold::Across(px(0.)),
            chips,
            ROWS,
            window,
            cx,
            |key, chip, _, cx| {
                let on_click = handlers
                    .remove(key)
                    .unwrap_or_else(|| Box::new(|_, _, _| {}));
                chip_element(index, &cwd, key, chip, on_click, &motion, cx)
            },
        )
        .render(h_flex().w_full().px_1().gap_1p5().flex_wrap());
        crate::motion::SizeGlide::new(&glide, row)
            .on_change(count)
            .into_any_element()
    });
    content = content.child(body);

    content.into_any_element()
}

/// How many files [`changed_file_rows`] lists before it offers the rest.
const RAIL_SHOWN: usize = 3;

/// A turn's changed files one row each, for the chat rail: the file's name
/// then its folder, fading, and its added and removed lines at the right.
/// A row opens the file's diff on a click; past [`RAIL_SHOWN`] a control
/// shows the rest, or fewer.
#[allow(clippy::too_many_arguments)]
pub(crate) fn changed_file_rows(
    index: usize,
    cwd: &Path,
    changes: &[FileChange],
    show_all: bool,
    open_files: Vec<ClickHandler>,
    toggle_more: ClickHandler,
    motion: &crate::motion::owned::Owned,
    cx: &App,
) -> AnyElement {
    let visible = if show_all {
        changes.len()
    } else {
        changes.len().min(RAIL_SHOWN)
    };
    let rows: Vec<(SharedString, RailRow)> = changes
        .iter()
        .take(visible)
        .zip(open_files)
        .map(|(change, open)| {
            let display = crate::workspace_walk::relativize_to_workspace(&change.path, cwd);
            let path = Path::new(&display);
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| display.clone());
            let folder = path
                .parent()
                .map(|parent| parent.to_string_lossy().into_owned())
                .filter(|parent| !parent.is_empty());
            let (added, deleted) = diff_stats(change.diff.as_deref());
            (
                SharedString::from(format!("file-{}", change.path)),
                RailRow {
                    name,
                    folder,
                    display,
                    path: change.path.clone(),
                    added,
                    deleted,
                    open: std::rc::Rc::new(open),
                },
            )
        })
        .collect();
    let more = (changes.len() > RAIL_SHOWN).then(|| {
        let label: SharedString = if show_all {
            crate::tr!("chat.show_fewer_files")
        } else {
            crate::tr!("chat.more_files", count = changes.len() - RAIL_SHOWN)
        }
        .into_owned()
        .into();
        quiet_control(("changed-files-more", index), label, toggle_more, cx)
            .motion(motion)
            .font_family(cx.theme().mono_font_family.clone())
    });
    let cwd = cwd.to_path_buf();
    let motion = motion.clone();
    let rows = crate::motion::Sampled::new(move |window, cx| {
        crate::motion::keyed::keyed_owned(
            &motion,
            format!("changed-file-rows-{index}"),
            crate::motion::keyed::Fold::Down,
            rows,
            ROWS,
            window,
            cx,
            |key, row, _, cx| rail_row(index, &cwd, key, row, cx),
        )
        .render(v_flex().w_full().min_w_0())
    });
    v_flex()
        .w_full()
        .min_w_0()
        .child(rows)
        .children(more)
        .into_any_element()
}

/// One file as [`changed_file_rows`] lists it.
#[derive(Clone)]
struct RailRow {
    name: String,
    /// The folder it is in, relative to the workspace, when it is not at
    /// its root.
    folder: Option<String>,
    display: String,
    path: String,
    added: u32,
    deleted: u32,
    open: std::rc::Rc<ClickHandler>,
}

fn rail_row(index: usize, cwd: &Path, key: &SharedString, row: &RailRow, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let radius = crate::material::radius_card(cx);
    let open = row.open.clone();
    let selector = key.clone();
    crate::material::accessible_clickable(
        h_flex(),
        SharedString::from(format!("changed-file-row-{index}-{key}")),
        Role::Button,
        format!("{}: {}", crate::tr!("chat.view_diff"), row.display),
        cx,
    )
    .debug_selector(move || format!("changed-file-row-{selector}"))
    .relative()
    .w_full()
    .min_w_0()
    .px_1()
    .py_1()
    .gap_2()
    .items_center()
    .rounded(radius)
    .cursor_pointer()
    .text_meta_mono(false, cx)
    .child(crate::material::row_hover(radius, cx))
    .child(
        h_flex()
            .flex_1()
            .min_w_0()
            .gap_1p5()
            .overflow_hidden()
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_color(theme.foreground)
                    .child(crate::widgets::fade_text(row.name.clone())),
            )
            .children(row.folder.clone().map(|folder| {
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .child(crate::widgets::fade_text(folder))
            })),
    )
    .child(
        h_flex()
            .flex_none()
            .gap_1()
            .whitespace_nowrap()
            .child(
                div()
                    .text_color(theme.success)
                    .child(format!("+{}", row.added)),
            )
            .child(
                div()
                    .text_color(theme.danger)
                    .child(format!("-{}", row.deleted)),
            ),
    )
    .on_click(move |event, window, cx| open(event, window, cx))
    .context_menu({
        let path = row.path.clone();
        let cwd = cwd.to_path_buf();
        move |menu, _, _| {
            menu.menu(
                crate::tr!("chat.view_diff").into_owned(),
                Box::new(super::super::OpenFileDiff {
                    turn: index,
                    path: path.clone(),
                }),
            )
            .separator()
            .path_items(&path, Some(&cwd))
        }
    })
    .into_any_element()
}

/// What one place in the files row holds.
#[derive(Clone)]
enum Chip {
    File {
        name: String,
        display: String,
        path: String,
        added: u32,
        deleted: u32,
    },
    /// The control that shows more files, or fewer.
    More(SharedString),
}

fn chip_element(
    index: usize,
    cwd: &Path,
    key: &SharedString,
    chip: &Chip,
    on_click: ClickHandler,
    motion: &crate::motion::owned::Owned,
    cx: &App,
) -> AnyElement {
    match chip {
        Chip::File {
            name,
            display,
            path,
            added,
            deleted,
        } => Button::new(SharedString::from(format!(
            "changed-file-chip-{index}-{key}"
        )))
        .motion(motion)
        .debug_selector({
            let key = key.clone();
            move || format!("changed-file-chip-{key}")
        })
        .ghost()
        .outline()
        .chip()
        .aria_label(format!("{}: {display}", crate::tr!("chat.view_diff")))
        .font_family(cx.theme().mono_font_family.clone())
        .text_token(&cx.theme().tokens.typography.xs)
        .child(name.clone())
        .child(
            div()
                .text_color(cx.theme().success)
                .child(format!("+{added}")),
        )
        .child(
            div()
                .text_color(cx.theme().danger)
                .child(format!("-{deleted}")),
        )
        .on_click(on_click)
        .context_menu({
            let path = path.clone();
            let cwd = cwd.to_path_buf();
            move |menu, _, _| {
                menu.menu(
                    crate::tr!("chat.view_diff").into_owned(),
                    Box::new(super::super::OpenFileDiff {
                        turn: index,
                        path: path.clone(),
                    }),
                )
                .separator()
                .path_items(&path, Some(&cwd))
            }
        })
        .into_any_element(),
        Chip::More(label) => {
            quiet_control(("changed-files-more", index), label.clone(), on_click, cx)
                .motion(motion)
                .font_family(cx.theme().mono_font_family.clone())
                .into_any_element()
        }
    }
}

pub(crate) fn quiet_control(
    id: impl Into<ElementId>,
    label: SharedString,
    on_click: ClickHandler,
    cx: &App,
) -> Button {
    // The label is a child rather than `.label()`: a quiet control stays
    // muted, and `Button` paints its label in the variant's foreground.
    Button::new(id)
        .ghost()
        .small()
        .aria_label(label.clone())
        .text_token(&cx.theme().tokens.typography.xs)
        .on_click(on_click)
        .child(div().text_color(cx.theme().muted_foreground).child(label))
}
