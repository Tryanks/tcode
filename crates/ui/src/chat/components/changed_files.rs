use std::path::Path;

use crate::icon::{Icon, IconName};
use crate::theme::ActiveTheme as _;
use crate::widgets::tooltip::Tooltip;
use agent::{ChangeCompleteness, FileChange};
use gpui::{
    AnyElement, App, ClickEvent, Div, ElementId, HighlightStyle, InteractiveElement as _,
    IntoElement as _, ParentElement as _, Role, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled as _, StyledText, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};

use super::super::model::{LiveEditRow, diff_stats};
use crate::diff::model::{DiffColors, FileDiffInput, RenderedRow, build_file};
use crate::diff::parse::RowKind;

pub(crate) type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

pub(crate) struct ChangedFilesHandlers {
    pub(crate) view_diff: ClickHandler,
    pub(crate) toggle_more: ClickHandler,
    pub(crate) open_files: Vec<ClickHandler>,
}

/// The changed-file evidence: a quiet header line and a chip row, sitting bare
/// in the flow under a hairline that separates it from the trace above.
pub(crate) fn changed_files(
    index: usize,
    cwd: &Path,
    changes: &[FileChange],
    completeness: ChangeCompleteness,
    show_all: bool,
    handlers: ChangedFilesHandlers,
    cx: &App,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
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
                .text_size(px(11.5))
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
        .child({
            let tooltip = crate::tr!("chat.view_diff_tooltip").into_owned();
            quiet_control(
                ("view-diff", index),
                crate::tr!("chat.view_diff").into_owned().into(),
                view_diff,
                cx,
            )
            .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
        });

    // Whitespace and the quiet header are separation enough; a full-width
    // rule here reads as a section divider, not evidence grouping.
    let mut content = v_flex().w_full().gap_1p5().pt(px(2.)).child(header);

    let visible = if show_all {
        changes.len()
    } else {
        changes.len().min(3)
    };
    let mut body = h_flex().w_full().px_1().gap_1p5().flex_wrap();
    for (file_index, (change, on_click)) in changes.iter().zip(open_files).take(visible).enumerate()
    {
        let display = crate::workspace_walk::relativize_to_workspace(&change.path, cwd);
        let name = Path::new(&display)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| display.clone());
        let (added, deleted) = diff_stats(change.diff.as_deref());
        let chip = h_flex()
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
            );
        body = body.child(
            crate::material::accessible_clickable(
                chip,
                SharedString::from(format!("changed-file-chip-{index}-{file_index}")),
                Role::Button,
                format!("{}: {display}", crate::tr!("chat.view_diff")),
                cx,
            )
            .cursor_pointer()
            .hover(|chip| chip.bg(cx.theme().accent))
            .on_click(on_click)
            .into_any_element(),
        );
    }
    if changes.len() > 3 {
        let hidden = changes.len() - 3;
        body = body.child(
            quiet_control(
                ("changed-files-more", index),
                if show_all {
                    crate::tr!("chat.show_fewer_files")
                } else {
                    crate::tr!("chat.more_files", count = hidden)
                }
                .into_owned()
                .into(),
                toggle_more,
                cx,
            )
            .font_family(cx.theme().mono_font_family.clone())
            .into_any_element(),
        );
    }
    content = content.child(body);

    content.into_any_element()
}

/// A quiet text control in the chip row: no fill, no border, muted until it is
/// hovered. Every switch next to the file chips wears this, so none of them
/// competes with the chips for the eye.
fn quiet_control(
    id: impl Into<ElementId>,
    label: SharedString,
    on_click: ClickHandler,
    cx: &App,
) -> Stateful<Div> {
    let foreground = cx.theme().foreground;
    crate::material::accessible_clickable(h_flex(), id, Role::Button, label.clone(), cx)
        .h(px(22.))
        .px_1p5()
        .items_center()
        .rounded(crate::material::radius_chip())
        .text_size(px(11.5))
        .text_color(cx.theme().muted_foreground)
        .cursor_pointer()
        .hover(move |control| control.text_color(foreground))
        .on_click(on_click)
        .child(label)
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

/// One live file-edit row: "Code edit  src/foo.rs  +12  -3". Provider diffs
/// drill down in place so the active edit can show its actual patch.
pub(crate) fn file_edit_row(
    key: &str,
    row: &LiveEditRow,
    expanded: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let style = FileEditRowStyle::from_theme(cx);
    let expandable = row.counts.is_some();
    let header = file_edit_row_header(row, expanded, expandable, &style);
    let header: AnyElement = if expandable {
        crate::material::accessible_clickable(
            header,
            SharedString::from(format!("file-edit-row-{key}")),
            Role::Button,
            crate::tr!("chat.activity_details"),
            cx,
        )
        .aria_expanded(expanded)
        .rounded(crate::material::radius_chip())
        .cursor_pointer()
        .hover(|header| header.bg(cx.theme().accent))
        .on_click(on_toggle)
        .into_any_element()
    } else {
        header.into_any_element()
    };

    v_flex()
        .w_full()
        .gap_1()
        .child(header)
        .when(expanded && expandable, |content| {
            content.child(render_inline_diff(key, row, cx))
        })
        .into_any_element()
}

fn file_edit_row_header(
    row: &LiveEditRow,
    expanded: bool,
    expandable: bool,
    style: &FileEditRowStyle,
) -> Div {
    h_flex()
        .w_full()
        .min_h(px(28.))
        .px_1()
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
        .when(expandable, |element| {
            element.child(
                Icon::new(if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size(px(13.))
                .text_color(style.muted),
            )
        })
}

fn render_inline_diff(key: &str, row: &LiveEditRow, cx: &App) -> AnyElement {
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
        &cx.theme().highlight_theme,
        &DiffColors {
            added_word_bg: cx.theme().success.opacity(0.30),
            removed_word_bg: cx.theme().danger.opacity(0.28),
        },
        &HighlightStyle::default(),
    );
    let rows = rendered
        .all_rows
        .iter()
        .map(|row| render_inline_diff_row(row, cx))
        .collect::<Vec<_>>();

    div()
        .w_full()
        .ml_2()
        .pl(px(14.))
        .py_0p5()
        .border_l_1()
        .border_color(cx.theme().border)
        .debug_selector(|| "file-edit-diff".into())
        .child(
            div()
                .id(SharedString::from(format!("file-edit-diff-y-{key}")))
                .w_full()
                .max_h(px(240.))
                .overflow_y_scroll()
                .child(
                    div()
                        .id(SharedString::from(format!("file-edit-diff-x-{key}")))
                        .w_full()
                        .overflow_x_scroll()
                        .child(
                            v_flex()
                                .min_w_full()
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_size(px(11.5))
                                .children(rows),
                        ),
                ),
        )
        .into_any_element()
}

fn render_inline_diff_row(row: &RenderedRow, cx: &App) -> AnyElement {
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
            .text_size(px(10.5))
            .text_color(cx.theme().muted_foreground)
            .child(line.map(|line| line.to_string()).unwrap_or_default())
    };

    h_flex()
        .min_w_full()
        .min_h(px(18.))
        .items_start()
        .border_l_2()
        .border_color(accent.unwrap_or(gpui::transparent_black()))
        .when_some(background, |line, background| line.bg(background))
        .child(gutter(row.old))
        .child(gutter(row.new))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::model::LiveEditRow;
    use gpui::TestAppContext;

    struct FileEditRowProbe {
        row: LiveEditRow,
        expanded: bool,
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
            gpui_base::v_flex().size_full().child(file_edit_row(
                "test-file-edit",
                &self.row,
                self.expanded,
                |_, _, _| {},
                cx,
            ))
        }
    }

    #[gpui::test]
    fn long_edit_path_and_counts_stay_inside_the_row_at_narrow_widths(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px, size};

        cx.update(crate::theme::init);
        let (_, cx) = cx.add_window_view(|_, _| FileEditRowProbe {
            row: LiveEditRow {
                path: "crates/ui/src/deeply/nested/module/tree/with/an/absurdly/long/name/live_file_edit_row.rs".into(),
                kind: agent::FileChangeKind::Modify,
                counts: Some((128, 96)),
                diff: None,
            },
            expanded: false,
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

    #[gpui::test]
    fn expanded_file_edit_renders_its_provider_diff(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px, size};

        cx.update(crate::theme::init);
        let (_, cx) = cx.add_window_view(|_, _| FileEditRowProbe {
            row: LiveEditRow {
                path: "src/lib.rs".into(),
                kind: agent::FileChangeKind::Modify,
                counts: Some((1, 1)),
                diff: Some("@@ -1,2 +1,2 @@\n-fn old() {}\n+fn new() {}\n fn stable() {}\n".into()),
            },
            expanded: true,
        });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(640.), px(240.)));
        cx.run_until_parked();
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });

        let diff = cx
            .debug_bounds("file-edit-diff")
            .expect("expanded inline diff bounds");
        assert!(diff.size.height >= px(36.));
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
