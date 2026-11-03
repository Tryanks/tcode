use std::path::Path;

use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::menu::ContextMenuExt as _;
use agent::{ChangeCompleteness, FileChange};
use gpui::{
    AnyElement, App, ClickEvent, ElementId, IntoElement as _, ParentElement as _, SharedString,
    Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};

use super::super::model::diff_stats;

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
