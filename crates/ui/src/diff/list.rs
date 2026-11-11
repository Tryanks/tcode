//! The diff list both diff views draw: file headers, gaps, unified and split code rows and
//! the line selection. A view supplies its files and the parts that differ through
//! [`DiffListHost`]; the rows themselves are drawn here only.

use crate::widgets::TooltipTrigger as _;
use std::ops::Range;
use std::path::Path;
use std::rc::Rc;

/// How long a diff row takes to come in, or to fold away as it leaves.
const ROWS_IN: std::time::Duration = std::time::Duration::from_millis(200);

use crate::icon::{Icon, IconName};
use crate::material;
use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::menu::{ContextMenuExt as _, CopyText, PopupMenu};
use crate::workspace_walk::relativize_to_workspace;
use agent::FileChangeKind;
use gpui::{
    Action, AnyElement, App, ClipboardItem, Context, Entity, FocusHandle, Hsla,
    InteractiveElement as _, IntoElement, ListAlignment, ListOffset, ListState, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, ScrollHandle,
    StatefulInteractiveElement as _, Styled as _, StyledText, Window, canvas, div, list,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{InteractiveElementExt as _, StyledExt as _, h_flex, v_flex};
use serde::Deserialize;
use tcode_core::session::ReviewSide;

use super::model::{ExpandDir, FileNotice, PairedRow, RenderedFile, display_columns};
use super::parse::RowKind;
use super::rows::{DisplayRow as DiffListItem, expand, project_file};
use crate::motion::list_rows::{Arrival, Attach, Glide, ListRows, RowGlide, changes};

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a surface drawn here takes to appear, and to leave.
const APPEAR: std::time::Duration = std::time::Duration::from_millis(200);

/// A code row's context-menu action on the line selection.
#[derive(Action, Clone, Copy, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_diff, no_json)]
pub(crate) enum DiffSelectionMenu {
    CopyLines,
    AddComment,
}

/// How a file sits in the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum FileLayout {
    #[default]
    Rows,
    /// Collapsed by the reader: the header alone.
    Header,
    /// No rows to draw: the header and the view's notice.
    Notice,
}

pub(crate) struct BuiltListItems {
    pub(crate) unified: Vec<DiffListItem>,
    pub(crate) split: Vec<DiffListItem>,
}

pub(crate) fn build_list_items_with(
    files: &[RenderedFile],
    layouts: &[FileLayout],
) -> BuiltListItems {
    let mut unified = Vec::new();
    let mut split = Vec::new();
    for (file_index, file) in files.iter().enumerate() {
        for (rows, is_split) in [(&mut unified, false), (&mut split, true)] {
            rows.push(DiffListItem::Header(file_index));
            match layouts.get(file_index).copied().unwrap_or_default() {
                FileLayout::Header => {}
                FileLayout::Notice => rows.push(DiffListItem::Notice(file_index)),
                FileLayout::Rows if file.all_rows.is_empty() => {
                    rows.push(DiffListItem::Notice(file_index))
                }
                FileLayout::Rows => project_file(rows, file_index, file, is_split),
            }
        }
    }
    BuiltListItems { unified, split }
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
    pub(crate) unified_items: Vec<DiffListItem>,
    pub(crate) split_items: Vec<DiffListItem>,
    width_candidates: Vec<String>,
    pub(crate) unified_list: ListState,
    pub(crate) split_list: ListState,
    /// A scroll to a file on its way, in each list.
    unified_scroll: crate::motion::ListScroll,
    split_scroll: crate::motion::ListScroll,
    /// Rows coming into and going out of each list.
    unified_motion: RowMotion,
    split_motion: RowMotion,
    pub(crate) selection: Option<LineSelection>,
    /// Whether line numbers are being dragged over to select their lines, in
    /// the split list or the unified one.
    line_drag: Option<bool>,
    /// Scrolls the list while that drag is past its top or bottom.
    drag_scroll: gpui_base::AutoScroll,
    /// The viewport that scrolls the rows sideways; a file header spans it and stays in it.
    horizontal: ScrollHandle,
    focus: FocusHandle,
}

impl DiffList {
    pub(crate) fn new(files: Vec<RenderedFile>, layouts: Vec<FileLayout>, cx: &mut App) -> Self {
        let items = build_list_items_with(&files, &layouts);
        let width_candidates = width_candidates(&files);
        Self {
            unified_list: ListState::new(items.unified.len(), ListAlignment::Top, px(180.)),
            split_list: ListState::new(items.split.len(), ListAlignment::Top, px(180.)),
            unified_scroll: Default::default(),
            split_scroll: Default::default(),
            unified_motion: RowMotion::new(&files, &items.unified),
            split_motion: RowMotion::new(&files, &items.split),
            files,
            layouts,
            unified_items: items.unified,
            split_items: items.split,
            width_candidates,
            selection: None,
            line_drag: None,
            drag_scroll: gpui_base::AutoScroll::default(),
            horizontal: ScrollHandle::new(),
            focus: cx.focus_handle().tab_stop(true),
        }
    }

    pub(crate) fn layout(&self, file: usize) -> FileLayout {
        self.layouts.get(file).copied().unwrap_or_default()
    }

    pub(crate) fn append(&mut self, files: Vec<RenderedFile>, layouts: Vec<FileLayout>) {
        self.files.extend(files);
        self.layouts = layouts;
        self.relayout();
    }

    /// Lays the files out again after the reader changed what they show (a
    /// file collapsed or expanded, a gap expanded) or more files came: rows
    /// that came in where the reader looks open their room, and rows that
    /// went while in view fold away, on the next frame.
    pub(crate) fn relayout(&mut self) {
        let items = build_list_items_with(&self.files, &self.layouts);
        self.width_candidates = width_candidates(&self.files);
        self.unified_motion.change(
            &self.files,
            &self.unified_items,
            &items.unified,
            &self.unified_list,
        );
        self.split_motion.change(
            &self.files,
            &self.split_items,
            &items.split,
            &self.split_list,
        );
        reconcile_rows(&self.unified_list, &self.unified_items, &items.unified);
        reconcile_rows(&self.split_list, &self.split_items, &items.split);
        self.unified_items = items.unified;
        self.split_items = items.split;
        self.remap_selection();
    }

    /// Shows newer files in place of these with the reader kept on the same file and line.
    /// Rows are identified by index, so an unchanged index can hold different text: unlike
    /// [`Self::relayout`], no measured row is kept, and the gap expansions of the old text
    /// are dropped.
    pub(crate) fn replace_files(&mut self, files: Vec<RenderedFile>) {
        let anchors = [
            row_anchor(&self.files, &self.unified_items, &self.unified_list),
            row_anchor(&self.files, &self.split_items, &self.split_list),
        ];
        let items = build_list_items_with(&files, &[]);
        self.width_candidates = width_candidates(&files);
        self.unified_list = ListState::new(items.unified.len(), ListAlignment::Top, px(180.));
        self.split_list = ListState::new(items.split.len(), ListAlignment::Top, px(180.));
        self.unified_scroll.cancel();
        self.split_scroll.cancel();
        self.unified_motion = RowMotion::new(&files, &items.unified);
        self.split_motion = RowMotion::new(&files, &items.split);
        self.files = files;
        self.layouts.clear();
        self.unified_items = items.unified;
        self.split_items = items.split;
        for (anchor, items, state) in [
            (&anchors[0], &self.unified_items, &self.unified_list),
            (&anchors[1], &self.split_items, &self.split_list),
        ] {
            if let Some(anchor) = anchor {
                state.scroll_to(anchor.locate(&self.files, items));
            }
        }
        self.remap_selection();
    }

    fn remap_selection(&mut self) {
        if let Some(selection) = &mut self.selection {
            let row_for_line = |line| {
                self.files
                    .iter()
                    .find(|file| file.path == selection.file)
                    .and_then(|file| {
                        file.all_rows.iter().position(|row| match selection.side {
                            ReviewSide::Old => row.old == Some(line),
                            ReviewSide::New => row.new == Some(line),
                        })
                    })
            };
            if let Some((start, end)) =
                row_for_line(selection.line_start).zip(row_for_line(selection.line_end))
            {
                selection.row_start = start;
                selection.row_end = end;
                selection.start_index = start;
                selection.end_index = end;
            } else {
                self.selection = None;
            }
        }
    }

    pub(crate) fn set_layouts(&mut self, layouts: Vec<FileLayout>) {
        if self.layouts != layouts {
            self.layouts = layouts;
            self.relayout();
        }
    }

    pub(crate) fn fold_for_line(&self, file: usize, line: u32) -> Option<Range<u32>> {
        let mut rows = Vec::new();
        project_file(&mut rows, file, self.files.get(file)?, false);
        rows.into_iter().find_map(|row| match row {
            DiffListItem::Fold { new_lines, .. } if new_lines.contains(&line) => Some(new_lines),
            _ => None,
        })
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

    /// Scrolls both lists so the file's header is at the top, gliding there.
    pub(crate) fn scroll_to_file(&self, file: usize, cx: &App) {
        for split in [false, true] {
            let items = if split {
                &self.split_items
            } else {
                &self.unified_items
            };
            if let Some(index) = items
                .iter()
                .position(|item| matches!(item, DiffListItem::Header(index) if *index == file))
            {
                self.glide_to(split, index, cx);
            }
        }
    }

    /// Scrolls the unified or the split list so row `index` is at the top, gliding there.
    pub(crate) fn glide_to(&self, split: bool, index: usize, cx: &App) {
        let (list, scroll) = self.scrolled(split);
        scroll.scroll_to(
            list,
            crate::motion::ScrollTarget::Row(ListOffset {
                item_ix: index,
                offset_in_item: px(0.),
            }),
            cx,
        );
    }

    fn motion(&self, split: bool) -> &RowMotion {
        if split {
            &self.split_motion
        } else {
            &self.unified_motion
        }
    }

    fn scrolled(&self, split: bool) -> (&ListState, &crate::motion::ListScroll) {
        if split {
            (&self.split_list, &self.split_scroll)
        } else {
            (&self.unified_list, &self.unified_scroll)
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
            .map(DiffListItem::file)
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

    /// One frame of a line drag held past the list's top or bottom: the
    /// selection reaches the line at that edge as last laid out (rows
    /// scrolled in this frame are not measured yet), and the list scrolls
    /// `delta` on.
    fn drag_scroll_tick(&mut self, split: bool, delta: gpui::Pixels) {
        let Some(pointer) = self.drag_scroll.last_drag_position else {
            return;
        };
        let at_edge = self.selected_side_line_at(split, pointer.y);
        self.scrolled(split).0.scroll_by(delta);
        if let Some((path, row, line, side)) = at_edge {
            self.select_line(path, row, line, side, true);
        }
    }

    /// The line on the selection's file and side in the row at `y`, clamped
    /// into the list's viewport.
    fn selected_side_line_at(
        &self,
        split: bool,
        y: gpui::Pixels,
    ) -> Option<(String, usize, u32, ReviewSide)> {
        let selection = self.selection.as_ref()?;
        let side = selection.side;
        let (state, _) = self.scrolled(split);
        let viewport = state.viewport_bounds();
        let y = y.clamp(viewport.top() + px(1.), viewport.bottom() - px(1.));
        let items = if split {
            &self.split_items
        } else {
            &self.unified_items
        };
        let (ix, bounds) = (state.logical_scroll_top().item_ix..items.len())
            .map_while(|ix| Some((ix, state.bounds_for_item(ix)?)))
            .find(|(_, bounds)| bounds.bottom() > y)?;
        let &DiffListItem::Code { file, left, right } =
            items.get(ix).filter(|_| bounds.top() <= y)?
        else {
            return None;
        };
        let file = self
            .files
            .get(file)
            .filter(|file| file.path == selection.file)?;
        let row = match (split, side) {
            (false, _) => right.or(left),
            (true, ReviewSide::Old) => left,
            (true, ReviewSide::New) => right,
        }?;
        let line = match side {
            ReviewSide::Old => file.all_rows[row].old,
            ReviewSide::New => file.all_rows[row].new,
        }?;
        Some((file.path.clone(), row, line, side))
    }

    fn end_line_drag(&mut self) {
        self.line_drag = None;
        self.drag_scroll.stop();
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
            .filter(|row| match selection.side {
                ReviewSide::Old => row.old.is_some(),
                ReviewSide::New => row.new.is_some(),
            })
            .map(|row| row.source(selection.side))
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
            .min_w_0()
            .overflow_hidden()
            .text_body(false, cx)
            .font_medium()
            .text_color(cx.theme().foreground)
            .child(crate::widgets::fade_text(path))
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
    fn notice(&self, file: usize, cx: &mut Context<Self>) -> AnyElement {
        let notice = self
            .diff_list()
            .and_then(|list| list.files.get(file))
            .and_then(|file| file.notice);
        let message = match notice {
            Some(FileNotice::Binary) => crate::tr!("diff.binary"),
            Some(FileNotice::Empty) => crate::tr!("diff.empty_file"),
            _ => crate::tr!("diff.no_text"),
        };
        render_notice(message.into_owned(), cx).into_any_element()
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
    material::card(cx)
        .flex_row()
        .min_w_full()
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
    window: &mut Window,
    cx: &mut Context<H>,
) -> AnyElement {
    // The unified and the split list each keep their layer (and their own
    // list), so switching the layout cross-fades one into the other; the
    // list left takes no keyboard or scroll while it fades.
    let fade = crate::motion::CrossFade::new(
        gpui::SharedString::from(format!("{id}-layout")),
        usize::from(split),
        2,
        CHANGE,
        window,
        cx,
    );
    let layers: Vec<(usize, AnyElement)> = [false, true]
        .into_iter()
        .filter(|shown| fade.shows(usize::from(*shown)))
        .map(|shown| {
            let list = render_layout(host, id, shown, wrap, shown != split, window, cx);
            (
                usize::from(shown),
                v_flex().size_full().child(list).into_any_element(),
            )
        })
        .collect();
    fade.fill()
        .render(layers)
        .flex_1()
        .min_h_0()
        .w_full()
        .into_any_element()
}

fn render_layout<H: DiffListHost>(
    host: &H,
    id: &'static str,
    split: bool,
    wrap: bool,
    leaving: bool,
    window: &mut Window,
    cx: &mut Context<H>,
) -> AnyElement {
    let Some(diff) = host.diff_list() else {
        return div().into_any_element();
    };
    let (list_state, scroll) = diff.scrolled(split);
    let list_state = list_state.clone();
    scroll.step(&list_state, window, cx);
    diff.motion(split).stamp(&list_state, window, cx);
    let content_width = if wrap {
        px(0.)
    } else {
        measure_content_width(&diff.width_candidates, split, window, cx)
    };
    let horizontal = diff.horizontal.clone();
    let shown = (horizontal.bounds().size.width, horizontal.offset().x);
    let entity = cx.entity();
    let drag_host = entity.clone();
    // Base's VirtualList requires known item sizes; wrapping and review annotations
    // have dynamic heights, as in GPUI Kit's diff, so GPUI's measured list owns them.
    // A resize of the viewport puts every row at its new height at once.
    let scope = u64::from(f32::from(shown.0).to_bits());
    let mut rows = list(list_state.clone(), move |index, window, cx| {
        entity.update(cx, |host, cx| {
            render_item(host, index, split, wrap, scope, window, cx)
        })
    })
    .flex_1()
    .min_h_0()
    .h_full();
    if wrap {
        rows = rows.w_full();
    } else {
        rows = rows.min_w(content_width);
    }
    // Do not let this horizontal overflow container translate ordinary
    // vertical wheel input into horizontal movement. The event can then
    // bubble to the List's vertical scroll handler; explicit horizontal
    // wheel/trackpad deltas (or Shift-wheel) still scroll this viewport.
    div()
        .id(id)
        .text_size(cx.theme().tokens.typography.mono_md.size)
        .font_family(cx.theme().mono_font_family.clone())
        .debug_selector(move || id.into())
        .key_context("TcodeDiff")
        .when(!leaving, |list| list.track_focus(&diff.focus))
        .on_action(cx.listener(|host, _: &gpui_base::input::Copy, _, cx| {
            if let Some(list) = host.diff_list()
                && list.selection.is_some()
            {
                cx.write_to_clipboard(ClipboardItem::new_string(list.selected_lines()));
            } else {
                cx.propagate();
            }
        }))
        .flex_1()
        .min_h_0()
        .overflow_x_scroll()
        .when(!leaving, |list| list.track_scroll(&diff.horizontal))
        .lock_scroll_axis()
        .child(crate::scroll::page_viewport(
            "diff-body-bounce",
            &list_state,
            rows,
        ))
        // Draws again once the viewport's width or offset settles elsewhere than the headers
        // were placed for.
        .child(
            canvas(
                move |_, window, _| {
                    if (horizontal.bounds().size.width, horizontal.offset().x) != shown {
                        window.request_animation_frame();
                    }
                },
                move |_, _, window, _| listen_for_line_drag(drag_host, split, window),
            )
            .absolute()
            .inset_0(),
        )
        .into_any_element()
}

/// A drag from a line number selects down to the line level with the pointer
/// wherever it is, scrolls the list while held past its top or bottom, inside
/// the window or not, and ends wherever the button comes up. An element's own
/// listeners would hear the pointer only while it is over that element.
fn listen_for_line_drag<H: DiffListHost>(host: Entity<H>, split: bool, window: &mut Window) {
    let dragging = move |host: &Entity<H>, cx: &App| {
        host.read(cx)
            .diff_list()
            .is_some_and(|list| list.line_drag == Some(split))
    };
    window.on_mouse_event({
        let host = host.clone();
        move |event: &MouseMoveEvent, phase, _, cx| {
            if !phase.capture() || !dragging(&host, cx) {
                return;
            }
            host.update(cx, |host, cx| {
                let Some(list) = host.diff_list_mut() else {
                    return;
                };
                if event.pressed_button != Some(MouseButton::Left) {
                    list.end_line_drag();
                    return;
                }
                if let Some((path, row, line, side)) =
                    list.selected_side_line_at(split, event.position.y)
                    && list
                        .selection
                        .as_ref()
                        .is_some_and(|selection| selection.row_end != row)
                {
                    list.select_line(path, row, line, side, true);
                    cx.notify();
                }
                let viewport = list.scrolled(split).0.viewport_bounds();
                let delta = gpui_base::AutoScroll::compute_delta(event.position.y, viewport);
                list.drag_scroll.last_drag_position = Some(event.position);
                list.drag_scroll
                    .set(delta, cx, move |delta, host: &mut H, cx| {
                        if let Some(list) = host.diff_list_mut() {
                            list.drag_scroll_tick(split, delta);
                            cx.notify();
                        }
                    });
            });
        }
    });
    window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
        if phase.capture() && event.button == MouseButton::Left && dragging(&host, cx) {
            host.update(cx, |host, _| {
                if let Some(list) = host.diff_list_mut() {
                    list.end_line_drag();
                }
            });
        }
    });
}

/// The row at the top of a list, by what it shows rather than by its index.
struct RowAnchor {
    path: String,
    /// The code row's old and new line; none for a file's header.
    lines: Option<(Option<u32>, Option<u32>)>,
    offset: gpui::Pixels,
    item_ix: usize,
}

fn code_lines(file: &RenderedFile, item: &DiffListItem) -> Option<(Option<u32>, Option<u32>)> {
    let DiffListItem::Code { left, right, .. } = *item else {
        return None;
    };
    let line = |ix: Option<usize>, side: fn(&super::model::RenderedRow) -> Option<u32>| {
        ix.and_then(|ix| file.all_rows.get(ix)).and_then(side)
    };
    Some((line(left, |row| row.old), line(right, |row| row.new)))
}

fn row_anchor(
    files: &[RenderedFile],
    items: &[DiffListItem],
    state: &ListState,
) -> Option<RowAnchor> {
    let top = state.logical_scroll_top();
    let item = items.get(top.item_ix)?;
    let file_ix = item.file();
    let file = files.get(file_ix)?;
    let (lines, offset) = match item {
        DiffListItem::Header(_) => (None, top.offset_in_item),
        DiffListItem::Code { .. } => (code_lines(file, item), top.offset_in_item),
        // A hunk, gap or notice row is re-found by the first line under it.
        _ => (
            items[top.item_ix..]
                .iter()
                .take_while(|item| item.file() == file_ix)
                .find_map(|item| code_lines(file, item)),
            px(0.),
        ),
    };
    Some(RowAnchor {
        path: file.path.clone(),
        lines,
        offset,
        item_ix: top.item_ix,
    })
}

impl RowAnchor {
    fn locate(&self, files: &[RenderedFile], items: &[DiffListItem]) -> ListOffset {
        let at = |item_ix, offset_in_item| ListOffset {
            item_ix,
            offset_in_item,
        };
        let Some(file_ix) = files.iter().position(|file| file.path == self.path) else {
            return at(self.item_ix, px(0.));
        };
        let file = &files[file_ix];
        let rows = || {
            items
                .iter()
                .enumerate()
                .filter(move |(_, item)| item.file() == file_ix)
        };
        let header = rows()
            .find(|(_, item)| matches!(item, DiffListItem::Header(_)))
            .map(|(ix, _)| ix)
            .unwrap_or(self.item_ix);
        let Some((old, new)) = self.lines else {
            return at(header, self.offset);
        };
        // Compare on the side the anchor has: the new line, or the old one for a removed line.
        let key = |lines: (Option<u32>, Option<u32>)| if new.is_some() { lines.1 } else { lines.0 };
        let Some(wanted) = new.or(old) else {
            return at(header, px(0.));
        };
        let line_of = |item: &DiffListItem| code_lines(file, item).and_then(key);
        if let Some((ix, _)) = rows().find(|(_, item)| line_of(item) == Some(wanted)) {
            return at(ix, self.offset);
        }
        rows()
            .find(|(_, item)| line_of(item).is_some_and(|line| line > wanted))
            .map_or(at(header, px(0.)), |(ix, _)| at(ix, px(0.)))
    }
}

pub(crate) fn reconcile_rows<T: PartialEq>(state: &ListState, old: &[T], new: &[T]) {
    let prefix = old
        .iter()
        .zip(new)
        .take_while(|(old, new)| old == new)
        .count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(old, new)| old == new)
        .count();
    if prefix == old.len() && prefix == new.len() {
        state.remeasure_items(0..new.len());
    } else {
        // Splicing only the changed interval retains measured rows and their exact scroll offset.
        state.splice(prefix..old.len() - suffix, new.len() - prefix - suffix);
    }
}

fn width_candidates(files: &[RenderedFile]) -> Vec<String> {
    let mut candidates: Vec<(usize, &str)> = Vec::new();
    for text in files
        .iter()
        .flat_map(|file| &file.all_rows)
        .flat_map(|row| {
            std::iter::once(row.text.as_str())
                .chain(row.original.iter().map(|original| original.text.as_str()))
        })
    {
        let cells = display_columns(text);
        let ix = candidates.partition_point(|(width, _)| *width >= cells);
        if ix < 8 {
            candidates.insert(ix, (cells, text));
            candidates.truncate(8);
        }
    }
    candidates
        .into_iter()
        .map(|(_, text)| text.to_string())
        .collect()
}

fn measure_content_width(
    candidates: &[String],
    split: bool,
    window: &mut Window,
    cx: &App,
) -> gpui::Pixels {
    let font = gpui::Font {
        family: cx.theme().mono_font_family.clone(),
        fallbacks: crate::i18n::cjk_font_fallbacks(),
        ..Default::default()
    };
    let width = candidates
        .iter()
        .map(|text| {
            window
                .text_system()
                .shape_line(
                    text.clone().into(),
                    cx.theme().tokens.typography.mono_md.size,
                    &[gpui::TextRun {
                        len: text.len(),
                        font: font.clone(),
                        color: cx.theme().foreground,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }],
                    None,
                )
                .width
        })
        .fold(px(0.), gpui::Pixels::max);
    if split {
        (width + px(58.)) * 2. + px(1.)
    } else {
        width + px(106.)
    }
}

fn render_item<H: DiffListHost>(
    host: &H,
    index: usize,
    split: bool,
    wrap: bool,
    scope: u64,
    window: &mut Window,
    cx: &mut Context<H>,
) -> AnyElement {
    let Some(diff) = host.diff_list() else {
        return div().into_any_element();
    };
    let items = if split {
        &diff.split_items
    } else {
        &diff.unified_items
    };
    let Some(item) = items.get(index) else {
        return div().into_any_element();
    };
    let motion = diff.motion(split);
    let key = motion.keys.get(index).copied().unwrap_or_default();
    let (content, extras) = render_row(host, item, split, wrap, Some((key, scope)), cx);
    // The lines wrapping or unwrapping, and something coming under a line
    // or going, change the row's height; it glides to the new one. At rest
    // it is laid out as itself, so a list of rows at rest costs what it did.
    let around = motion.rows.borrow().around(key, cx);
    around.draw(
        "diff-row",
        key,
        Glide::Tail {
            content: u64::from(wrap) | u64::from(extras) << 1,
        },
        content,
        |item, _| render_row(host, item, split, wrap, None, cx).0,
        window,
    )
}

/// One row of the list as `item` describes it, and whether something is
/// drawn under its line. `glide` names the row (its key, and the viewport's
/// scope) when what is under a line (comments, editors) glides to each new
/// height it takes.
fn render_row<H: DiffListHost>(
    host: &H,
    item: &DiffListItem,
    split: bool,
    wrap: bool,
    glide: Option<(u64, u64)>,
    cx: &mut Context<H>,
) -> (AnyElement, bool) {
    let Some(diff) = host.diff_list() else {
        return (div().into_any_element(), false);
    };
    let row = match item {
        DiffListItem::Header(file) => render_file_header(host, *file, cx),
        DiffListItem::Notice(file) => host.notice(*file, cx),
        DiffListItem::Hunk { file, hunk } => div()
            .min_w_full()
            .px_3()
            .py_1()
            .text_secondary(false, cx)
            .bg(cx.theme().muted.opacity(0.45))
            .child(diff.files[*file].hunks[*hunk].header.clone())
            .into_any_element(),
        DiffListItem::Fold {
            file,
            new_lines,
            expandable,
        } => render_gap(
            host,
            *file,
            new_lines.end - new_lines.start,
            new_lines.clone(),
            *expandable,
            cx,
        ),
        DiffListItem::Code { file, left, right } => {
            let document = &diff.files[*file];
            let rendered = if split {
                render_split_row(
                    host,
                    document,
                    PairedRow {
                        left: *left,
                        right: *right,
                    },
                    wrap,
                    cx,
                )
            } else {
                let index = right.or(*left).expect("code has a source line");
                render_code_row(
                    host,
                    document,
                    index,
                    document.all_rows[index].kind,
                    None,
                    wrap,
                    cx,
                )
            };
            let old = left.and_then(|ix| document.all_rows[ix].old);
            let new = right.and_then(|ix| document.all_rows[ix].new);
            let extras = host.row_extras(*file, old, new, cx);
            if extras.is_empty() {
                return (rendered, false);
            }
            // What is under a line changes height on its own (an editor in
            // place of a button, a card's text): measured apart from the
            // list, only while there is something there, it glides.
            let extras = v_flex().w_full().children(extras);
            let extras = match glide {
                Some((key, scope)) => {
                    RowGlide::new(("diff-row-extras", key), Glide::Height { scope }, extras)
                        .into_any_element()
                }
                None => extras.into_any_element(),
            };
            return (
                v_flex()
                    .w_full()
                    .child(rendered)
                    .child(extras)
                    .into_any_element(),
                true,
            );
        }
    };
    (row, false)
}

/// The rows of one of the list's views coming and going, by row key.
///
/// A row is known by what it shows, not its index: a file's header by its
/// path, a code row by its lines, a gap by the gap it was cut from, so a
/// relayout that shifts the rows still knows which came and which went.
struct RowMotion {
    keys: Vec<u64>,
    rows: std::cell::RefCell<ListRows<(), DiffListItem>>,
    /// What the last relayout brought and took, for the next frame to stamp.
    pending: std::cell::RefCell<Option<Pending>>,
}

impl RowMotion {
    fn new(files: &[RenderedFile], items: &[DiffListItem]) -> Self {
        Self {
            keys: row_keys(files, items, &[], &[]),
            rows: std::cell::RefCell::new(ListRows::over(ROWS_IN)),
            pending: Default::default(),
        }
    }

    /// The rows are about to be `new` in place of `old`, which `list` still
    /// lays out: note the rows that come in where the reader looks and those
    /// that go while in view.
    fn change(
        &mut self,
        files: &[RenderedFile],
        old: &[DiffListItem],
        new: &[DiffListItem],
        list: &ListState,
    ) {
        let keys = row_keys(files, new, old, &self.keys);
        let top = list.logical_scroll_top();
        let viewport = list.viewport_bounds();
        let in_view = |index: usize| {
            list.bounds_for_item(index).is_some_and(|bounds| {
                bounds.top() < viewport.bottom() && bounds.bottom() >= viewport.top()
            })
        };
        // As many arrivals as the view showed rows, from the top of the view:
        // past them a file that opens is in place at once, out of view.
        let shown = (top.item_ix..old.len())
            .take_while(|index| in_view(*index))
            .count();
        let reach = top.item_ix..top.item_ix + shown;
        let mut changed = changes(&self.keys, &keys);
        changed.arrived.retain(|(index, _)| reach.contains(index));
        changed.left.retain(|(index, _)| in_view(*index));
        let mut pending = self.pending.borrow_mut();
        let (arrived, left) = pending.get_or_insert_with(Default::default);
        arrived.extend(changed.arrived.iter().map(|(index, _)| keys[*index]));
        left.extend(
            changed
                .left
                .into_iter()
                .map(|(index, attach)| (self.keys[index], old[index].clone(), attach)),
        );
        drop(pending);
        self.keys = keys;
    }

    /// Stamps what the last relayout brought and took, now; and lets go of
    /// what has come in and gone.
    fn stamp(&self, list: &ListState, window: &mut Window, cx: &App) {
        let now = cx.background_executor().now();
        if let Some((arrived, left)) = self.pending.borrow_mut().take()
            && !cx.reduce_motion()
        {
            let mut rows = self.rows.borrow_mut();
            for key in arrived {
                rows.arrive(key, Arrival::Inserted, (), now);
            }
            for (key, item, attach) in left {
                rows.leave(key, item, attach, now);
            }
        }
        self.rows
            .borrow_mut()
            .settle_list(list, &self.keys, window, cx);
    }
}

/// What a relayout brought (by key) and took (by key, with what it was and
/// the row it folds against).
type Pending = (Vec<u64>, Vec<(u64, DiffListItem, Attach)>);

/// The keys of `items`; a gap cut from one of `old`'s gaps keeps its key.
fn row_keys(
    files: &[RenderedFile],
    items: &[DiffListItem],
    old: &[DiffListItem],
    old_keys: &[u64],
) -> Vec<u64> {
    let path = |file: usize| files.get(file).map_or("", |file| file.path.as_str());
    items
        .iter()
        .map(|item| match item {
            DiffListItem::Header(file) => row_key(("header", path(*file))),
            DiffListItem::Notice(file) => row_key(("notice", path(*file))),
            DiffListItem::Hunk { file, hunk } => row_key(("hunk", path(*file), *hunk)),
            DiffListItem::Code { file, left, right } => {
                row_key(("code", path(*file), *left, *right))
            }
            DiffListItem::Fold {
                file, new_lines, ..
            } => old
                .iter()
                .zip(old_keys)
                .find_map(|(was, key)| match was {
                    DiffListItem::Fold {
                        file: was_file,
                        new_lines: was_lines,
                        ..
                    } if was_file == file
                        && was_lines.start <= new_lines.start
                        && new_lines.end <= was_lines.end =>
                    {
                        Some(*key)
                    }
                    _ => None,
                })
                .unwrap_or_else(|| row_key(("gap", path(*file), new_lines.start))),
        })
        .collect()
}

fn row_key(name: impl std::hash::Hash) -> u64 {
    use std::hash::Hasher as _;
    let mut hasher = std::hash::DefaultHasher::new();
    name.hash(&mut hasher);
    hasher.finish()
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
    let (viewport_width, scrolled) = host.diff_list().map_or((px(0.), px(0.)), |list| {
        (
            list.horizontal.bounds().size.width,
            -list.horizontal.offset().x,
        )
    });
    let menu = host.file_menu(file_index, cx);
    let header = material::card_secondary(cx)
        .flex_row()
        .id(("diff-file-header", file_index))
        // A header spans what the list shows and moves with it, not with its lines, so the path,
        // the counts and the menu stay in view however far a long line reaches.
        .map(|header| {
            if viewport_width > px(0.) {
                header.w(viewport_width).ml(scrolled)
            } else {
                header.min_w_full()
            }
        })
        .items_center()
        .relative()
        // The change kind's accent rail; no owner draws an accent edge on a card.
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
        .child(Icon::new(IconName::File).text_color(muted))
        .child(host.header_title(file_index, cx))
        .when_some(kind_label, |this, (label, foreground)| {
            this.child(
                div()
                    .flex_none()
                    .text_secondary(false, cx)
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
                .text_meta_mono(false, cx)
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
        .context_menu(move |popup, window, cx| (menu)(popup, window, cx));
    // A list item is laid out as its own root, so the header's offset is a margin inside it.
    div().min_w_full().child(header).into_any_element()
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
        // A fold row is as tall as its small expand controls; no row owner
        // is this dense.
        .h(px(24.))
        .px_3()
        .items_center()
        .bg(cx.theme().muted)
        .text_secondary(false, cx)
        .font_family(cx.theme().font_family.clone());
    let label = crate::tr!("diff.unmodified_lines", count = count);
    match host.gap_state(file_index, expandable) {
        GapState::Expandable => {}
        GapState::Loading => {
            // The Loading line fades in where the expand controls were clicked.
            let id = gpui::SharedString::from(format!(
                "diff-gap-loading-{file_index}-{}",
                new_lines.start
            ));
            return row
                .child(crate::motion::Sampled::new(move |window, cx| {
                    let shown = crate::motion::appear(id, true, APPEAR, window, cx).progress;
                    div()
                        .opacity(shown)
                        .child(crate::tr!("pull_requests.files.loading_lines"))
                        .into_any_element()
                }))
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
                            this.with_tooltip(move |_, _| {
                                crate::widgets::tooltip::Tooltip::new(reason.clone())
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
    material::notice(cx.theme().warning, cx)
        .items_start()
        .text_token(&cx.theme().tokens.typography.xs)
        .text_color(cx.theme().warning_foreground)
        .font_family(cx.theme().font_family.clone())
        .child(div().flex_none().child(Icon::new(IconName::TriangleAlert)))
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
        .w_full()
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
        let path = file.path.clone();
        let side_name = match side {
            ReviewSide::Old => "old",
            ReviewSide::New => "new",
        };
        div()
            .debug_selector(move || format!("diff-line-{side_name}-{path}-{}", line.unwrap_or(0)))
            .flex_none()
            .w(px(if split { 42. } else { 44. }))
            .px_1()
            .text_right()
            // A five-digit line number overflows the split gutter at `xs`, so
            // the gutter keeps the denser size.
            .text_size(px(11.))
            .text_color(cx.theme().muted_foreground)
            .child(line.map(|value| value.to_string()).unwrap_or_default())
            .cursor_pointer()
            .when_some(line, |gutter, line| {
                gutter.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |host, _: &MouseDownEvent, _, cx| {
                        if let Some(list) = host.diff_list_mut() {
                            list.select_line(file_down.clone(), row_index, line, side, false);
                            list.line_drag = Some(split);
                        }
                        cx.notify();
                    }),
                )
            })
    };
    let (text, runs) = if split_side == Some(ReviewSide::Old)
        && let Some(original) = &row.original
    {
        (&original.text, &original.runs)
    } else {
        (&row.text, &row.runs)
    };
    let code = div()
        .flex_1()
        .px_2()
        .text_color(cx.theme().foreground)
        .child(StyledText::new(text.clone()).with_highlights(runs.iter().cloned()))
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
            .w_full()
            .border_l_2()
            .border_color(accent.unwrap_or(gpui::transparent_black()))
            .child(gutter(ReviewSide::Old, cx))
            .child(gutter(ReviewSide::New, cx));
    }
    let copy_side = split_side.unwrap_or(if row.kind == RowKind::Removed {
        ReviewSide::Old
    } else {
        ReviewSide::New
    });
    let line = row.source(copy_side).to_string();
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
    use crate::diff::model::{DiffColors, FileDiffInput, build_file};
    use gpui::{AppContext as _, HighlightStyle, Render, TestAppContext};

    struct Viewer(DiffList, gpui::Entity<crate::widgets::input::InputState>);

    impl DiffListHost for Viewer {
        fn diff_list(&self) -> Option<&DiffList> {
            Some(&self.0)
        }
        fn diff_list_mut(&mut self) -> Option<&mut DiffList> {
            Some(&mut self.0)
        }
        fn expand_gap(
            &mut self,
            file: usize,
            lines: Range<u32>,
            direction: ExpandDir,
            cx: &mut Context<Self>,
        ) {
            self.0.expand(file, lines, direction);
            cx.notify();
        }
        fn file_menu(&self, _: usize, _: &App) -> FileMenu {
            Rc::new(|menu, _, _| menu)
        }
        fn row_extras(
            &self,
            _: usize,
            _: Option<u32>,
            new: Option<u32>,
            _: &mut Context<Self>,
        ) -> Vec<AnyElement> {
            if new == Some(1) {
                vec![
                    div()
                        .debug_selector(|| "review-editor".into())
                        .h(px(40.))
                        .child(crate::widgets::input::Input::new(&self.1))
                        .into_any_element(),
                ]
            } else {
                Vec::new()
            }
        }
    }

    impl Render for Viewer {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(600.))
                .h(px(250.))
                .flex()
                .flex_col()
                .child(render_list(self, "copy-diff", true, true, window, cx))
        }
    }

    /// Changed files, unified, with a card under the second line of the
    /// file `card` names.
    struct Reviewed {
        list: DiffList,
        card: Option<usize>,
    }

    impl DiffListHost for Reviewed {
        fn diff_list(&self) -> Option<&DiffList> {
            Some(&self.list)
        }
        fn diff_list_mut(&mut self) -> Option<&mut DiffList> {
            Some(&mut self.list)
        }
        fn expand_gap(&mut self, _: usize, _: Range<u32>, _: ExpandDir, _: &mut Context<Self>) {}
        fn file_menu(&self, _: usize, _: &App) -> FileMenu {
            Rc::new(|menu, _, _| menu)
        }
        fn row_extras(
            &self,
            file: usize,
            _: Option<u32>,
            new: Option<u32>,
            _: &mut Context<Self>,
        ) -> Vec<AnyElement> {
            (self.card == Some(file) && new == Some(2))
                .then(|| div().h(px(80.)).into_any_element())
                .into_iter()
                .collect()
        }
    }

    impl Render for Reviewed {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(600.))
                .h(px(600.))
                .flex()
                .flex_col()
                .child(render_list(self, "reviewed-diff", false, true, window, cx))
        }
    }

    fn changed_file(path: &str) -> RenderedFile {
        build_file(
            &FileDiffInput {
                path,
                kind: FileChangeKind::Modify,
                old_text: Some("one\ntwo\nthree\nfour\n"),
                new_text: Some("one\n2\n3\nfour\n"),
                patch: None,
                ignore_whitespace: false,
                show_invisibles: false,
            },
            path.into(),
            "rust",
            &crate::highlight::HighlightTheme::default_dark(),
            &DiffColors {
                added_word_bg: gpui::green(),
                removed_word_bg: gpui::red(),
            },
            &HighlightStyle::default(),
        )
    }

    /// The row at the top of the view stays where the reader has it while a
    /// row above the view or below it in the view glides to a new height.
    #[gpui::test]
    fn the_row_the_reader_is_at_stays_put_while_rows_around_it_glide(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let files = ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs", "f.rs"]
            .into_iter()
            .map(changed_file)
            .collect();
        let (viewer, cx) = cx.add_window_view(|_, cx| Reviewed {
            list: DiffList::new(files, Vec::new(), cx),
            card: None,
        });
        cx.update(|_, cx| cx.set_reduce_motion(false));
        crate::test_frames::draw_settled(cx);
        viewer.update(cx, |viewer, cx| {
            let header = viewer
                .list
                .unified_items
                .iter()
                .position(|item| matches!(item, DiffListItem::Header(2)))
                .expect("c.rs has a header");
            viewer.list.unified_list.scroll_to(ListOffset {
                item_ix: header,
                offset_in_item: px(0.),
            });
            cx.notify();
        });
        crate::test_frames::draw_settled(cx);
        let reader = |cx: &mut gpui::VisualTestContext| {
            cx.debug_bounds("diff-line-new-c.rs-1")
                .expect("the reader's row is on screen")
                .top()
        };
        let at = reader(cx);
        for card in [Some(0), Some(2)] {
            viewer.update(cx, |viewer, cx| {
                viewer.card = card;
                cx.notify();
            });
            crate::test_frames::draw(cx);
            for _ in 0..20 {
                cx.executor()
                    .advance_clock(std::time::Duration::from_millis(16));
                cx.update(|window, cx| {
                    window.simulate_next_frame(cx);
                });
                crate::test_frames::draw(cx);
                assert_eq!(
                    reader(cx),
                    at,
                    "a card under file {card:?} moved the reader"
                );
            }
        }
        let below = cx
            .debug_bounds("diff-line-new-d.rs-1")
            .expect("the next file is on screen")
            .top();
        assert!(below - at > px(80.), "the card below the reader opened");
    }

    #[gpui::test]
    fn wrapped_split_keeps_columns_visible_and_copies_original_text(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::shortcut::init);
        for (old, new, ignore_whitespace) in [
            ("let 名 = 1;\n", "let 名\t= 2;\n", false),
            ("let 名 = 1;\n", "let  名\t=1;\n", true),
            (
                "let description = \"a long line wraps inside the original column while its counterpart stays visible\";\n",
                "let description = \"a long line wraps inside the modified column while its counterpart stays visible\";\n",
                false,
            ),
        ] {
            let (viewer, cx) = cx.add_window_view(|window, cx| {
                let file = build_file(
                    &FileDiffInput {
                        path: "copy.rs",
                        kind: FileChangeKind::Modify,
                        old_text: Some(old),
                        new_text: Some(new),
                        patch: None,
                        ignore_whitespace,
                        show_invisibles: true,
                    },
                    "copy.rs".into(),
                    "rust",
                    &crate::highlight::HighlightTheme::default_dark(),
                    &DiffColors {
                        added_word_bg: gpui::green(),
                        removed_word_bg: gpui::red(),
                    },
                    &HighlightStyle::default(),
                );
                let input = cx.new(|cx| crate::widgets::input::InputState::new(window, cx));
                Viewer(DiffList::new(vec![file], Vec::new(), cx), input)
            });
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            let left = cx.debug_bounds("diff-line-old-copy.rs-1").unwrap();
            let right = cx.debug_bounds("diff-line-new-copy.rs-1").unwrap();
            assert!(
                ((right.origin.x - left.origin.x) - px(300.5)).abs() <= px(1.),
                "wrapped counterparts must start in equal halves of the 600px viewport: {left:?}, {right:?}"
            );
            for (selector, source) in [
                ("diff-line-old-copy.rs-1", old),
                ("diff-line-new-copy.rs-1", new),
            ] {
                let line = cx.debug_bounds(selector).expect("source-side gutter");
                cx.simulate_click(line.center(), gpui::Modifiers::default());
                cx.run_until_parked();
                #[cfg(target_os = "macos")]
                cx.simulate_keystrokes("cmd-c");
                #[cfg(not(target_os = "macos"))]
                cx.simulate_keystrokes("ctrl-c");
                assert_eq!(
                    cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text())),
                    Some(source.trim_end_matches('\n').into())
                );
            }
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            let editor = cx
                .debug_bounds("review-editor")
                .expect("line-anchored review input");
            cx.simulate_click(editor.center(), gpui::Modifiers::default());
            cx.simulate_input("Review text");
            assert_eq!(
                viewer.read_with(cx, |viewer, cx| viewer.1.read(cx).value()),
                "Review text"
            );
        }
    }
}
