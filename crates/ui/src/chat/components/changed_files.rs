use std::path::Path;

use crate::theme::ActiveTheme as _;
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use agent::{ChangeCompleteness, FileChange};
use gpui::{
    AnyElement, App, ClickEvent, Div, InteractiveElement as _, IntoElement as _,
    ParentElement as _, Role, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};

use super::super::model::{LiveEditRow, diff_stats};

pub(crate) type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

pub(crate) struct ChangedFilesState {
    pub(crate) collapsed: bool,
    pub(crate) show_all: bool,
}

pub(crate) struct ChangedFilesHandlers {
    pub(crate) toggle_collapsed: ClickHandler,
    pub(crate) view_diff: ClickHandler,
    pub(crate) toggle_more: ClickHandler,
    pub(crate) open_files: Vec<ClickHandler>,
}

/// The CHANGED FILES card: header with totals + file chips.
pub(crate) fn changed_files(
    index: usize,
    cwd: &Path,
    changes: &[FileChange],
    completeness: ChangeCompleteness,
    state: ChangedFilesState,
    handlers: ChangedFilesHandlers,
    cx: &App,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let ChangedFilesState {
        collapsed,
        show_all,
    } = state;
    let ChangedFilesHandlers {
        toggle_collapsed,
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
        .py_1()
        .gap_2()
        .items_center()
        .child(
            h_flex()
                .flex_1()
                .min_w_0()
                .gap_1p5()
                .items_center()
                .text_size(px(11.))
                .font_medium()
                .text_color(muted)
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
            Button::new(("collapse-all", index))
                .ghost()
                .xsmall()
                .label(if collapsed {
                    crate::tr!("chat.expand_all")
                } else {
                    crate::tr!("chat.collapse_all")
                })
                .on_click(toggle_collapsed),
        )
        .child(
            Button::new(("view-diff", index))
                .outline()
                .xsmall()
                .label(crate::tr!("chat.view_diff"))
                .tooltip(crate::tr!("chat.view_diff_tooltip"))
                .on_click(view_diff),
        );

    let mut content = v_flex().w_full().child(header);

    if !collapsed {
        let visible = if show_all {
            changes.len()
        } else {
            changes.len().min(3)
        };
        let mut body = h_flex().w_full().px_1().pb_1().gap_1p5().flex_wrap();
        for (file_index, (change, on_click)) in
            changes.iter().zip(open_files).take(visible).enumerate()
        {
            let display = tcode_services::user_files::relativize_to_workspace(&change.path, cwd);
            let name = Path::new(&display)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| display.clone());
            let (added, deleted) = diff_stats(change.diff.as_deref());
            body = body.child(
                crate::material::accessible_clickable(
                    h_flex(),
                    SharedString::from(format!("changed-file-chip-{index}-{file_index}")),
                    Role::Button,
                    format!("{}: {display}", crate::tr!("chat.view_diff")),
                    cx,
                )
                .h(px(22.))
                .px_2()
                .gap_1()
                .items_center()
                .rounded(crate::material::radius_chip())
                .border_1()
                .border_color(cx.theme().border)
                .bg(crate::material::content_surface(cx))
                .font_family(cx.theme().mono_font_family.clone())
                .text_size(px(11.5))
                .cursor_pointer()
                .hover(|chip| chip.bg(cx.theme().accent))
                .on_click(on_click)
                .child(name)
                .child(
                    div()
                        .text_color(cx.theme().success)
                        .child(format!("+{added}")),
                )
                .child(
                    div()
                        .text_color(cx.theme().danger)
                        .child(format!("-{deleted}")),
                ),
            );
        }
        if changes.len() > 3 {
            let hidden = changes.len() - 3;
            body = body.child(
                crate::material::accessible_clickable(
                    h_flex(),
                    ("changed-files-more", index),
                    Role::Button,
                    crate::tr!("chat.more_files", count = hidden),
                    cx,
                )
                .h(px(22.))
                .px_2()
                .items_center()
                .rounded(crate::material::radius_chip())
                .text_size(px(11.5))
                .font_family(cx.theme().mono_font_family.clone())
                .text_color(muted)
                .cursor_pointer()
                .hover(|chip| chip.bg(cx.theme().accent))
                .on_click(toggle_more)
                .child(if show_all {
                    crate::tr!("chat.show_fewer_files")
                } else {
                    crate::tr!("chat.more_files", count = hidden)
                }),
            );
        }
        content = content.child(body);
    }

    content.into_any_element()
}

/// The `+N -N` pair, `flex_none` so it never gives ground to a long path.
fn diff_counts_colored(
    added: u32,
    deleted: u32,
    added_color: gpui::Hsla,
    deleted_color: gpui::Hsla,
    mono: SharedString,
) -> Div {
    h_flex()
        .flex_none()
        .gap_2()
        .text_size(px(11.5))
        .font_family(mono)
        .child(div().text_color(added_color).child(format!("+{added}")))
        .child(div().text_color(deleted_color).child(format!("-{deleted}")))
}

/// The theme tokens a file-edit row needs.
#[derive(Clone)]
pub(crate) struct FileEditRowStyle {
    muted: gpui::Hsla,
    added: gpui::Hsla,
    deleted: gpui::Hsla,
    mono: SharedString,
}

impl FileEditRowStyle {
    pub(crate) fn from_theme(cx: &App) -> Self {
        Self {
            muted: cx.theme().muted_foreground,
            added: cx.theme().success,
            deleted: cx.theme().danger,
            mono: cx.theme().mono_font_family.clone(),
        }
    }
}

/// One live file-edit row: "Code edit  src/foo.rs  +12  -3".
pub(crate) fn file_edit_row(row: &LiveEditRow, style: &FileEditRowStyle) -> Div {
    h_flex()
        .w_full()
        .min_h(px(28.))
        .gap_2()
        .items_center()
        .text_size(px(12.5))
        .debug_selector(|| "file-edit-row".into())
        .child(
            Icon::new(IconName::File)
                .size(px(13.))
                .text_color(style.muted),
        )
        .child(
            h_flex()
                .min_w_0()
                .flex_1()
                .gap_1()
                .overflow_hidden()
                .child(
                    div()
                        .flex_none()
                        .whitespace_nowrap()
                        .child(crate::tr!("chat.file_edit")),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(style.muted)
                        .font_family(style.mono.clone())
                        .debug_selector(|| "file-edit-path".into())
                        .child(row.path.clone()),
                ),
        )
        .when_some(row.counts, |element, (added, deleted)| {
            element.child(
                diff_counts_colored(
                    added,
                    deleted,
                    style.added,
                    style.deleted,
                    style.mono.clone(),
                )
                .debug_selector(|| "file-edit-counts".into()),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::model::LiveEditRow;
    use gpui::TestAppContext;

    struct FileEditRowProbe {
        row: LiveEditRow,
    }

    impl gpui::Render for FileEditRowProbe {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            // The Work Log stacks rows in a full-width column, so the row is
            // content-height there; reproduce that rather than letting the
            // window stretch the row and mask a wrap.
            use gpui::{ParentElement as _, Styled as _};
            gpui_component::v_flex()
                .size_full()
                .child(file_edit_row(&self.row, &FileEditRowStyle::from_theme(cx)))
        }
    }

    #[gpui::test]
    fn long_edit_path_and_counts_stay_inside_the_row_at_narrow_widths(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px, size};

        cx.update(gpui_component::init);
        let (_, cx) = cx.add_window_view(|_, _| FileEditRowProbe {
            row: LiveEditRow {
                path: "crates/ui/src/deeply/nested/module/tree/with/an/absurdly/long/name/live_file_edit_row.rs".into(),
                counts: Some((128, 96)),
            },
        });
        let cx: &mut VisualTestContext = cx;
        let draw = |cx: &mut VisualTestContext| {
            cx.run_until_parked();
            cx.update(|window, cx| {
                _ = window.draw(cx);
            });
        };

        // A comfortable width the path fits in: the single-line baseline.
        cx.simulate_resize(size(px(900.), px(80.)));
        draw(cx);
        let baseline = cx.debug_bounds("file-edit-row").expect("row bounds");
        let baseline_counts = cx.debug_bounds("file-edit-counts").expect("count bounds");

        // The chat column gets narrow when the sidebar and a right panel are
        // both open; sweep down to well below any practical chat width.
        for width in 280..=900 {
            cx.simulate_resize(size(px(width as f32), px(80.)));
            draw(cx);

            let row = cx.debug_bounds("file-edit-row").expect("row bounds");
            let path = cx.debug_bounds("file-edit-path").expect("path bounds");
            let counts = cx.debug_bounds("file-edit-counts").expect("count bounds");

            assert!(
                path.left() >= row.left() && path.right() <= row.right(),
                "path escaped the row at {width}px: row={row:?}, path={path:?}"
            );
            assert!(
                counts.left() >= row.left() && counts.right() <= row.right(),
                "+/- counts escaped the row at {width}px: row={row:?}, counts={counts:?}"
            );
            assert!(
                counts.left() >= path.right(),
                "+/- counts overlapped the path at {width}px: path={path:?}, counts={counts:?}"
            );
            // `flex_none`: the counts never give up width to the path.
            assert_eq!(
                counts.size.width, baseline_counts.size.width,
                "+/- counts were squeezed at {width}px: {counts:?}"
            );
            // The path truncates instead of wrapping the row onto a second line.
            assert_eq!(
                row.size.height, baseline.size.height,
                "row grew taller at {width}px, so the long path wrapped: row={row:?}"
            );
        }
    }

    #[test]
    fn live_edit_row_label_is_exact_in_both_locales() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::set_locale(crate::LANGUAGE_ENGLISH);
        assert_eq!(crate::tr!("chat.file_edit"), "Code edit");

        crate::set_locale(crate::LANGUAGE_SIMPLIFIED_CHINESE);
        assert_eq!(crate::tr!("chat.file_edit"), "编辑代码");
        crate::set_locale(crate::LANGUAGE_ENGLISH);
    }
}
