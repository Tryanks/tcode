//! The diff list both diff views draw: file headers, gaps, unified and split code rows and
//! the line selection. A view supplies its files and the parts that differ through
//! [`DiffListHost`]; the rows themselves are drawn here only.

use std::ops::Range;
use std::path::Path;
use std::rc::Rc;

use crate::icon::{Icon, IconName};
use crate::material;
use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::menu::{ContextMenuExt as _, CopyText, PopupMenu};
use crate::workspace_walk::relativize_to_workspace;
use agent::FileChangeKind;
use gpui::{
    Action, AnyElement, App, Context, Hsla, InteractiveElement as _, IntoElement, ListAlignment,
    ListOffset, ListState, MouseButton, MouseDownEvent, MouseMoveEvent, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, StyledText, Window, div, list,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{InteractiveElementExt as _, StyledExt as _, h_flex, v_flex};
use serde::Deserialize;
use tcode_core::session::ReviewSide;

use super::model::{
    ExpandDir, PairedRow, RenderedFile, VisibleItem, VisibleSplitItem, diff_content_widths, expand,
    visible_split, visible_unified,
};
use super::parse::RowKind;

/// A code row's context-menu action on the line selection.
#[derive(Action, Clone, Copy, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_diff, no_json)]
pub(crate) enum DiffSelectionMenu {
    CopyLines,
    AddComment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffListItem {
    Header(usize),
    /// A file drawn without rows: the view explains why below its header.
    Placeholder(usize),
    UnifiedRow {
        file: usize,
        row: usize,
    },
    SplitRow {
        file: usize,
        row: usize,
    },
}

/// How a file sits in the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum FileLayout {
    #[default]
    Rows,
    /// Collapsed by the reader: the header alone.
    Header,
    /// No rows to draw: the header and the view's placeholder.
    Placeholder,
}

pub(crate) struct BuiltListItems {
    pub(crate) unified_visible: Vec<Vec<VisibleItem>>,
    pub(crate) split_visible: Vec<Vec<VisibleSplitItem>>,
    pub(crate) unified: Vec<DiffListItem>,
    pub(crate) split: Vec<DiffListItem>,
}

/// A file missing from `layouts` takes its rows.
pub(crate) fn build_list_items_with(
    files: &[RenderedFile],
    layouts: &[FileLayout],
) -> BuiltListItems {
    let unified_visible = files.iter().map(visible_unified).collect::<Vec<_>>();
    let split_visible = files.iter().map(visible_split).collect::<Vec<_>>();
    let unified_capacity = files.len() + unified_visible.iter().map(Vec::len).sum::<usize>();
    let split_capacity = files.len() + split_visible.iter().map(Vec::len).sum::<usize>();
    let mut unified = Vec::with_capacity(unified_capacity);
    let mut split = Vec::with_capacity(split_capacity);
    for (file_index, _) in files.iter().enumerate() {
        unified.push(DiffListItem::Header(file_index));
        split.push(DiffListItem::Header(file_index));
        match layouts.get(file_index).copied().unwrap_or_default() {
            FileLayout::Header => continue,
            FileLayout::Placeholder => {
                unified.push(DiffListItem::Placeholder(file_index));
                split.push(DiffListItem::Placeholder(file_index));
                continue;
            }
            FileLayout::Rows => {}
        }
        unified.extend((0..unified_visible[file_index].len()).map(|row| {
            DiffListItem::UnifiedRow {
                file: file_index,
                row,
            }
        }));
        split.extend(
            (0..split_visible[file_index].len()).map(|row| DiffListItem::SplitRow {
                file: file_index,
                row,
            }),
        );
    }
    BuiltListItems {
        unified_visible,
        split_visible,
        unified,
        split,
    }
}

pub(crate) fn file_header_index(
    files: &[RenderedFile],
    items: &[DiffListItem],
    path: &str,
    cwd: &Path,
) -> Option<usize> {
    let display_path = relativize_to_workspace(path, cwd);
    let file_index = files.iter().position(|file| file.path == display_path)?;
    items
        .iter()
        .position(|item| matches!(item, DiffListItem::Header(index) if *index == file_index))
}

#[derive(Clone)]
pub(crate) struct LineSelection {
    pub(crate) file: String,
    pub(crate) row_start: usize,
    pub(crate) row_end: usize,
    pub(crate) line_start: u32,
    pub(crate) line_end: u32,
    pub(crate) side: ReviewSide,
    pub(crate) start_index: usize,
    pub(crate) end_index: usize,
}

/// Rendered files laid out as list items for both the unified and the split view.
pub(crate) struct DiffList {
    pub(crate) files: Vec<RenderedFile>,
    layouts: Vec<FileLayout>,
    pub(crate) unified_visible: Vec<Vec<VisibleItem>>,
    pub(crate) split_visible: Vec<Vec<VisibleSplitItem>>,
    pub(crate) unified_items: Vec<DiffListItem>,
    pub(crate) split_items: Vec<DiffListItem>,
    pub(crate) unified_content_width: f32,
    pub(crate) split_content_width: f32,
    pub(crate) unified_list: ListState,
    pub(crate) split_list: ListState,
    pub(crate) selection: Option<LineSelection>,
}

impl DiffList {
    pub(crate) fn new(files: Vec<RenderedFile>, layouts: Vec<FileLayout>) -> Self {
        let items = build_list_items_with(&files, &layouts);
        let (unified_content_width, split_content_width) = diff_content_widths(&files);
        Self {
            unified_list: ListState::new(items.unified.len(), ListAlignment::Top, px(180.)),
            split_list: ListState::new(items.split.len(), ListAlignment::Top, px(180.)),
            files,
            layouts,
            unified_visible: items.unified_visible,
            split_visible: items.split_visible,
            unified_items: items.unified,
            split_items: items.split,
            unified_content_width,
            split_content_width,
            selection: None,
        }
    }

    pub(crate) fn layout(&self, file: usize) -> FileLayout {
        self.layouts.get(file).copied().unwrap_or_default()
    }

    /// Lays the list out again after its files or layouts changed, keeping both scroll
    /// positions where they were.
    pub(crate) fn relayout(&mut self) {
        let unified_top = self.unified_list.logical_scroll_top();
        let split_top = self.split_list.logical_scroll_top();
        let items = build_list_items_with(&self.files, &self.layouts);
        let (unified_content_width, split_content_width) = diff_content_widths(&self.files);
        let restore = |len: usize, top: ListOffset| {
            let state = ListState::new(len, ListAlignment::Top, px(180.));
            if len > 0 {
                state.scroll_to(ListOffset {
                    item_ix: top.item_ix.min(len - 1),
                    offset_in_item: top.offset_in_item,
                });
            }
            state
        };
        self.unified_list = restore(items.unified.len(), unified_top);
        self.split_list = restore(items.split.len(), split_top);
        self.unified_visible = items.unified_visible;
        self.split_visible = items.split_visible;
        self.unified_items = items.unified;
        self.split_items = items.split;
        self.unified_content_width = unified_content_width;
        self.split_content_width = split_content_width;
        self.selection = None;
    }

    pub(crate) fn set_layouts(&mut self, layouts: Vec<FileLayout>) {
        if self.layouts != layouts {
            self.layouts = layouts;
            self.relayout();
        }
    }

    pub(crate) fn expand(&mut self, file: usize, lines: Range<u32>, direction: ExpandDir) {
        let Some(rendered) = self.files.get_mut(file) else {
            return;
        };
        expand(rendered, lines, direction, 20);
        self.relayout();
    }

    pub(crate) fn remeasure(&self) {
        self.unified_list.remeasure();
        self.split_list.remeasure();
    }

    /// Scrolls both lists so the file's header is at the top.
    pub(crate) fn scroll_to_file(&self, file: usize) {
        for (items, state) in [
            (&self.unified_items, &self.unified_list),
            (&self.split_items, &self.split_list),
        ] {
            if let Some(index) = items
                .iter()
                .position(|item| matches!(item, DiffListItem::Header(index) if *index == file))
            {
                state.scroll_to(ListOffset {
                    item_ix: index,
                    offset_in_item: px(0.),
                });
            }
        }
    }

    /// The file whose rows are at the top of the viewport.
    pub(crate) fn top_file(&self, split: bool) -> Option<usize> {
        let (items, state) = if split {
            (&self.split_items, &self.split_list)
        } else {
            (&self.unified_items, &self.unified_list)
        };
        items
            .get(state.logical_scroll_top().item_ix)
            .map(|item| match *item {
                DiffListItem::Header(file) | DiffListItem::Placeholder(file) => file,
                DiffListItem::UnifiedRow { file, .. } | DiffListItem::SplitRow { file, .. } => file,
            })
    }

    pub(crate) fn select_line(
        &mut self,
        file: String,
        row: usize,
        line: u32,
        side: ReviewSide,
        drag: bool,
    ) {
        if drag
            && let Some(selection) = self.selection.as_mut()
            && selection.file == file
            && selection.side == side
        {
            selection.row_end = row;
            selection.line_end = line;
            selection.end_index = row;
        } else {
            self.selection = Some(LineSelection {
                file,
                row_start: row,
                row_end: row,
                line_start: line,
                line_end: line,
                side,
                start_index: row,
                end_index: row,
            });
        }
        self.remeasure();
    }

    /// The selected rows' text, without diff markers, in file order.
    pub(crate) fn selected_lines(&self) -> String {
        let Some(selection) = &self.selection else {
            return String::new();
        };
        let Some(file) = self.files.iter().find(|file| file.path == selection.file) else {
            return String::new();
        };
        let start = selection.row_start.min(selection.row_end);
        let end = selection.row_start.max(selection.row_end);
        file.all_rows
            .iter()
            .skip(start)
            .take(end + 1 - start)
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// What a gap row offers.
pub(crate) enum GapState {
    Expandable,
    Loading,
    /// Not expandable, with the reason as a tooltip when there is one.
    Fixed(Option<String>),
}

pub(crate) type FileMenu = Rc<dyn Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu>;

/// The parts of the list a diff view decides.
pub(crate) trait DiffListHost: Sized + 'static {
    fn diff_list(&self) -> Option<&DiffList>;
    fn diff_list_mut(&mut self) -> Option<&mut DiffList>;
    fn expand_gap(
        &mut self,
        file: usize,
        lines: Range<u32>,
        direction: ExpandDir,
        cx: &mut Context<Self>,
    );
    fn file_menu(&self, file: usize, cx: &App) -> FileMenu;

    fn gap_state(&self, _file: usize, expandable: bool) -> GapState {
        if expandable {
            GapState::Expandable
        } else {
            GapState::Fixed(None)
        }
    }
    /// The header's path; by default the rendered path.
    fn header_title(&self, file: usize, cx: &mut Context<Self>) -> AnyElement {
        let path = self
            .diff_list()
            .and_then(|list| list.files.get(file))
            .map(|file| file.path.clone())
            .unwrap_or_default();
        div()
            .text_size(px(13.))
            .line_height(px(18.))
            .font_medium()
            .text_color(cx.theme().foreground)
            .child(path)
            .into_any_element()
    }
    fn header_leading(&self, _file: usize, _cx: &mut Context<Self>) -> Option<AnyElement> {
        None
    }
    fn header_note(&self, _file: usize, _cx: &mut Context<Self>) -> Option<AnyElement> {
        None
    }
    fn header_trailing(&self, _file: usize, _cx: &mut Context<Self>) -> Option<AnyElement> {
        None
    }
    /// A click on the header's empty area.
    fn header_clicked(&mut self, _file: usize, _cx: &mut Context<Self>) {}
    /// Rows drawn under the code row ending on these lines.
    fn row_extras(
        &self,
        _file: usize,
        _old: Option<u32>,
        _new: Option<u32>,
        _cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        Vec::new()
    }
    fn placeholder(&self, _file: usize, _cx: &mut Context<Self>) -> AnyElement {
        div().into_any_element()
    }
    /// Whether the selection menu offers a comment on the selected lines, enabled or not, and
    /// under what label.
    fn review_comment_menu(&self, _cx: &App) -> Option<(bool, gpui::SharedString)> {
        None
    }
}

/// The row under the last selected line that starts a comment on the selection; the Diff tab's
/// comment for the agent and Files' review comment are both started from it. `disabled` names
/// why it cannot be started now.
pub(crate) fn selection_row(
    id: &'static str,
    label: gpui::SharedString,
    disabled: Option<gpui::SharedString>,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    h_flex()
        .min_w_full()
        .px_3()
        .py_1()
        .bg(cx.theme().muted)
        .rounded(material::radius_card(cx))
        .font_family(cx.theme().font_family.clone())
        .child(
            Button::new(id)
                .ghost()
                .small()
                .label(label)
                .disabled(disabled.is_some())
                .when_some(disabled, |button, reason| button.tooltip(reason))
                .on_click(on_click),
        )
        .into_any_element()
}

/// The scrolling list of files, horizontally scrollable when lines do not wrap.
pub(crate) fn render_list<H: DiffListHost>(
    host: &H,
    id: &'static str,
    split: bool,
    wrap: bool,
    cx: &mut Context<H>,
) -> AnyElement {
    let Some(diff) = host.diff_list() else {
        return div().into_any_element();
    };
    let (list_state, content_width) = if split {
        (diff.split_list.clone(), diff.split_content_width)
    } else {
        (diff.unified_list.clone(), diff.unified_content_width)
    };
    let entity = cx.entity();
    let mut rows = list(list_state.clone(), move |index, _, cx| {
        entity.update(cx, |host, cx| render_item(host, index, split, wrap, cx))
    })
    .flex_1()
    .min_h_0()
    .h_full()
    .text_size(px(13.))
    .font_family(cx.theme().mono_font_family.clone());
    if wrap {
        rows = rows.w_full();
    } else {
        rows = rows.min_w(px(content_width));
    }
    // Do not let this horizontal overflow container translate ordinary
    // vertical wheel input into horizontal movement. The event can then
    // bubble to the List's vertical scroll handler; explicit horizontal
    // wheel/trackpad deltas (or Shift-wheel) still scroll this viewport.
    div()
        .id(id)
        .debug_selector(move || id.into())
        .flex_1()
        .min_h_0()
        .overflow_x_scroll()
        .lock_scroll_axis()
        .child(crate::scroll::page_viewport(
            "diff-body-bounce",
            crate::wheel_easing::Handle::List(list_state),
            rows,
        ))
        .into_any_element()
}

fn render_item<H: DiffListHost>(
    host: &H,
    index: usize,
    split: bool,
    wrap: bool,
    cx: &mut Context<H>,
) -> AnyElement {
    let Some(diff) = host.diff_list() else {
        return div().into_any_element();
    };
    let item = if split {
        diff.split_items.get(index)
    } else {
        diff.unified_items.get(index)
    };
    let Some(item) = item.copied() else {
        return div().into_any_element();
    };
    match item {
        DiffListItem::Header(file) => render_file_header(host, file, cx),
        DiffListItem::Placeholder(file) => host.placeholder(file, cx),
        DiffListItem::UnifiedRow {
            file: file_index,
            row,
        } => {
            let file = &diff.files[file_index];
            let (rendered, lines) = match &diff.unified_visible[file_index][row] {
                VisibleItem::Gap {
                    count,
                    new_lines,
                    expandable,
                } => (
                    render_gap(host, file_index, *count, new_lines.clone(), *expandable, cx),
                    None,
                ),
                VisibleItem::Row(row_index) => {
                    let row = &file.all_rows[*row_index];
                    (
                        render_code_row(host, file, *row_index, row.kind, None, wrap, cx),
                        Some((row.old, row.new)),
                    )
                }
            };
            v_flex()
                .min_w_full()
                .child(rendered)
                .children(
                    lines
                        .into_iter()
                        .flat_map(|(old, new)| host.row_extras(file_index, old, new, cx)),
                )
                .into_any_element()
        }
        DiffListItem::SplitRow {
            file: file_index,
            row,
        } => {
            let file = &diff.files[file_index];
            let (rendered, extras) = match &diff.split_visible[file_index][row] {
                VisibleSplitItem::Gap {
                    count,
                    new_lines,
                    expandable,
                } => (
                    render_gap(host, file_index, *count, new_lines.clone(), *expandable, cx),
                    Vec::new(),
                ),
                VisibleSplitItem::Pair(pair_index) => {
                    let pair = file.all_split[*pair_index];
                    let rendered = render_split_row(host, file, pair, wrap, cx);
                    let old = pair.left.and_then(|index| file.all_rows[index].old);
                    let new = pair.right.and_then(|index| file.all_rows[index].new);
                    (rendered, host.row_extras(file_index, old, new, cx))
                }
            };
            v_flex()
                .min_w_full()
                .child(rendered)
                .children(extras)
                .into_any_element()
        }
    }
}

pub(crate) fn kind_rail(kind: FileChangeKind, cx: &App) -> Option<Hsla> {
    match kind {
        FileChangeKind::Create => Some(cx.theme().success),
        FileChangeKind::Delete => Some(cx.theme().danger),
        FileChangeKind::Rename => Some(cx.theme().info),
        FileChangeKind::Modify => None,
    }
}

fn render_file_header<H: DiffListHost>(
    host: &H,
    file_index: usize,
    cx: &mut Context<H>,
) -> AnyElement {
    let Some(file) = host.diff_list().and_then(|list| list.files.get(file_index)) else {
        return div().into_any_element();
    };
    let (kind, added, removed) = (file.kind, file.added, file.removed);
    let muted = cx.theme().muted_foreground;
    let kind_label = match kind {
        FileChangeKind::Create => Some((crate::tr!("diff.created"), cx.theme().success_foreground)),
        FileChangeKind::Delete => Some((crate::tr!("diff.deleted"), cx.theme().danger_foreground)),
        FileChangeKind::Rename => Some((crate::tr!("diff.renamed"), cx.theme().info_foreground)),
        FileChangeKind::Modify => None,
    };
    let menu = host.file_menu(file_index, cx);
    h_flex()
        .id(("diff-file-header", file_index))
        .min_w_full()
        .h(px(34.))
        .px_3()
        .gap_2()
        .items_center()
        .bg(cx.theme().secondary)
        .rounded(material::radius_card(cx))
        .relative()
        .when_some(kind_rail(kind, cx), |this, color| {
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
        .children(host.header_leading(file_index, cx))
        .child(Icon::new(IconName::File).xsmall().text_color(muted))
        .child(host.header_title(file_index, cx))
        .when_some(kind_label, |this, (label, foreground)| {
            this.child(
                div()
                    .text_size(px(11.))
                    .line_height(px(18.))
                    .text_color(foreground)
                    .child(label),
            )
        })
        .children(host.header_note(file_index, cx))
        .child(div().flex_1())
        .child(
            h_flex()
                .flex_none()
                .gap_2()
                .text_size(px(13.))
                .child(
                    div()
                        .text_color(cx.theme().success)
                        .child(format!("+{added}")),
                )
                .child(
                    div()
                        .text_color(cx.theme().danger)
                        .child(format!("-{removed}")),
                ),
        )
        .children(host.header_trailing(file_index, cx))
        .on_click(cx.listener(move |host, _, _, cx| host.header_clicked(file_index, cx)))
        .context_menu(move |popup, window, cx| (menu)(popup, window, cx))
        .into_any_element()
}

fn render_gap<H: DiffListHost>(
    host: &H,
    file_index: usize,
    count: u32,
    new_lines: Range<u32>,
    expandable: bool,
    cx: &mut Context<H>,
) -> AnyElement {
    let row = h_flex()
        .min_w_full()
        .h(px(24.))
        .px_3()
        .items_center()
        .bg(cx.theme().muted)
        .text_size(px(11.))
        .text_color(cx.theme().muted_foreground)
        .font_family(cx.theme().font_family.clone());
    let label = crate::tr!("diff.unmodified_lines", count = count);
    match host.gap_state(file_index, expandable) {
        GapState::Expandable => {}
        GapState::Loading => {
            return row
                .child(crate::tr!("pull_requests.files.loading_lines"))
                .into_any_element();
        }
        GapState::Fixed(reason) => {
            let start = new_lines.start;
            return row
                .child(
                    div()
                        .id(("diff-gap-fixed", file_index * 100_000 + start as usize))
                        .child(label)
                        .when_some(reason, |this, reason| {
                            this.tooltip(move |window, cx| {
                                crate::widgets::tooltip::Tooltip::new(reason.clone())
                                    .build(window, cx)
                            })
                        }),
                )
                .into_any_element();
        }
    }
    let start = new_lines.start;
    let up_lines = new_lines.clone();
    let all_lines = new_lines.clone();
    row.gap_1()
        .child(
            Button::new(format!("diff-gap-up-{file_index}-{start}"))
                .ghost()
                .small()
                .compact()
                .icon(IconName::ChevronUp)
                .tooltip(crate::tr!("diff.expand_gap_up"))
                .on_click(cx.listener(move |host, _, _, cx| {
                    host.expand_gap(file_index, up_lines.clone(), ExpandDir::Up, cx);
                })),
        )
        .child(
            Button::new(format!("diff-gap-all-{file_index}-{start}"))
                .ghost()
                .small()
                .compact()
                .label(label)
                .tooltip(crate::tr!("diff.expand_gap_all"))
                .on_click(cx.listener(move |host, _, _, cx| {
                    host.expand_gap(file_index, all_lines.clone(), ExpandDir::All, cx);
                })),
        )
        .child(
            Button::new(format!("diff-gap-down-{file_index}-{start}"))
                .ghost()
                .small()
                .compact()
                .icon(IconName::ChevronDown)
                .tooltip(crate::tr!("diff.expand_gap_down"))
                .on_click(cx.listener(move |host, _, _, cx| {
                    host.expand_gap(file_index, new_lines.clone(), ExpandDir::Down, cx);
                })),
        )
        .into_any_element()
}

/// A notice is prose, not code: it wraps inside the panel instead of running off the right
/// edge with the sentence cut in half.
pub(crate) fn render_notice(message: impl Into<gpui::SharedString>, cx: &App) -> gpui::Div {
    h_flex()
        .min_w_full()
        .px(px(material::CARD_INSET))
        .py_2()
        .gap_1p5()
        .items_start()
        .bg(cx.theme().warning.opacity(0.12))
        .rounded(material::radius_card(cx))
        .text_size(px(11.))
        .text_color(cx.theme().warning_foreground)
        .font_family(cx.theme().font_family.clone())
        .child(
            div()
                .flex_none()
                .child(Icon::new(IconName::TriangleAlert).xsmall()),
        )
        .child(div().flex_1().min_w_0().child(message.into()))
}

fn render_split_row<H: DiffListHost>(
    host: &H,
    file: &RenderedFile,
    pair: PairedRow,
    wrap: bool,
    cx: &mut Context<H>,
) -> AnyElement {
    let paired_as_context = pair
        .left
        .zip(pair.right)
        .is_some_and(|(left, right)| file.all_rows[left].text == file.all_rows[right].text);
    let cell = |row_index: Option<usize>, side: ReviewSide, cx: &mut Context<H>| {
        let Some(index) = row_index else {
            return div().flex_1().min_w_0().min_h(px(18.)).into_any_element();
        };
        render_code_row(
            host,
            file,
            index,
            if paired_as_context {
                RowKind::Context
            } else {
                file.all_rows[index].kind
            },
            Some(side),
            wrap,
            cx,
        )
    };
    h_flex()
        .min_w_full()
        .items_stretch()
        .child(cell(pair.left, ReviewSide::Old, cx))
        .child(div().w_px().bg(cx.theme().border.opacity(0.)))
        .child(cell(pair.right, ReviewSide::New, cx))
        .into_any_element()
}

fn render_code_row<H: DiffListHost>(
    host: &H,
    file: &RenderedFile,
    row_index: usize,
    kind: RowKind,
    split_side: Option<ReviewSide>,
    wrap: bool,
    cx: &mut Context<H>,
) -> AnyElement {
    let row = &file.all_rows[row_index];
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
    let split = split_side.is_some();
    let gutter = |side: ReviewSide, cx: &mut Context<H>| {
        let line = match side {
            ReviewSide::Old => row.old,
            ReviewSide::New => row.new,
        };
        let file_down = file.path.clone();
        let file_move = file.path.clone();
        div()
            .flex_none()
            .w(px(if split { 42. } else { 44. }))
            .px_1()
            .text_right()
            .text_size(px(11.))
            .text_color(cx.theme().muted_foreground)
            .child(line.map(|value| value.to_string()).unwrap_or_default())
            .cursor_pointer()
            .when_some(line, |gutter, line| {
                gutter
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |host, _: &MouseDownEvent, _, cx| {
                            if let Some(list) = host.diff_list_mut() {
                                list.select_line(file_down.clone(), row_index, line, side, false);
                            }
                            cx.notify();
                        }),
                    )
                    .on_mouse_move(cx.listener(move |host, event: &MouseMoveEvent, _, cx| {
                        if event.dragging() {
                            if let Some(list) = host.diff_list_mut() {
                                list.select_line(file_move.clone(), row_index, line, side, true);
                            }
                            cx.notify();
                        }
                    }))
            })
    };
    let code = div()
        .flex_1()
        .px_2()
        .text_color(cx.theme().foreground)
        .child(StyledText::new(row.text.clone()).with_highlights(row.runs.iter().cloned()))
        .when(split || wrap, |code| code.min_w_0())
        .when(!wrap, |code| code.whitespace_nowrap());
    let mut cell = h_flex()
        .min_h(px(18.))
        .items_start()
        .when_some(bg, |cell, color| cell.bg(color));
    if let Some(side) = split_side {
        cell = cell.flex_1().min_w_0().child(gutter(side, cx));
    } else {
        cell = cell
            .min_w_full()
            .border_l_2()
            .border_color(accent.unwrap_or(gpui::transparent_black()))
            .child(gutter(ReviewSide::Old, cx))
            .child(gutter(ReviewSide::New, cx));
    }
    let line = row.text.clone();
    let selected_here = host
        .diff_list()
        .and_then(|list| list.selection.as_ref())
        .is_some_and(|selection| selection.file == file.path);
    let review_comment = host.review_comment_menu(cx);
    cell.child(code)
        .context_menu(move |menu, _, _| {
            let menu = menu
                .menu(
                    crate::tr!("diff.copy_line").into_owned(),
                    Box::new(CopyText(line.clone())),
                )
                .menu_with_enable(
                    crate::tr!("diff.copy_selected_lines").into_owned(),
                    Box::new(DiffSelectionMenu::CopyLines),
                    selected_here,
                );
            let Some((enabled, label)) = review_comment.clone() else {
                return menu;
            };
            menu.separator().menu_with_enable(
                label,
                Box::new(DiffSelectionMenu::AddComment),
                selected_here && enabled,
            )
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::model::RenderedRow;

    #[test]
    fn resolves_file_headers_independently_for_unified_and_split_lists() {
        let code_row = |text: &str| RenderedRow {
            kind: RowKind::Added,
            old: None,
            new: Some(1),
            text: text.into(),
            runs: Vec::new(),
        };
        let first_rows = vec![code_row("one"), code_row("two"), code_row("three")];
        let second_rows = vec![code_row("replacement")];
        let outside_rows = vec![code_row("outside")];
        let files = vec![
            RenderedFile {
                path: "src/first.rs".into(),
                kind: FileChangeKind::Modify,
                added: 3,
                removed: 0,
                all_split: vec![PairedRow {
                    left: None,
                    right: Some(0),
                }],
                all_rows: first_rows,
                collapsed: Vec::new(),
                expandable: false,
            },
            RenderedFile {
                path: "tests/second.rs".into(),
                kind: FileChangeKind::Modify,
                added: 1,
                removed: 0,
                all_split: vec![PairedRow {
                    left: None,
                    right: Some(0),
                }],
                all_rows: second_rows,
                collapsed: Vec::new(),
                expandable: false,
            },
            RenderedFile {
                path: "/tmp/outside.rs".into(),
                kind: FileChangeKind::Modify,
                added: 1,
                removed: 0,
                all_split: vec![PairedRow {
                    left: None,
                    right: Some(0),
                }],
                all_rows: outside_rows,
                collapsed: Vec::new(),
                expandable: false,
            },
        ];
        let items = build_list_items_with(&files, &[]);
        let (unified, split) = (items.unified, items.split);
        let cwd = Path::new("/workspace/repository");

        assert_eq!(
            file_header_index(&files, &unified, "tests/second.rs", cwd),
            Some(4)
        );
        assert_eq!(
            file_header_index(&files, &split, "tests/second.rs", cwd),
            Some(2)
        );
        assert_eq!(
            file_header_index(
                &files,
                &unified,
                "/workspace/repository/tests/second.rs",
                cwd,
            ),
            Some(4)
        );
        assert_eq!(
            file_header_index(&files, &unified, "/tmp/outside.rs", cwd),
            Some(6)
        );
        assert_eq!(file_header_index(&files, &unified, "missing.rs", cwd), None);
    }
}
