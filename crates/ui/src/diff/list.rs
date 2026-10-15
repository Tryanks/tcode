//! Tcode review slots and file navigation around Kit's virtualized Diff.
use super::model::PreparedFile;
use crate::{
    icon::{Icon, IconName},
    material,
    sizing::Sizable as _,
    theme::ActiveTheme as _,
    widgets::{
        button::{Button, ButtonVariants as _},
        menu::{DropdownMenu as _, PopupMenu},
    },
};
use agent::FileChangeKind;
use gpui::{
    Action, AnyElement, App, AppContext as _, Context, Entity, Hsla, InteractiveElement as _,
    IntoElement, ParentElement as _, SharedString, Styled as _, Subscription, Window, canvas, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};
use gpui_component::diff::{
    Diff, DiffAnnotation, DiffEvent, DiffLinePosition, DiffLineRange, DiffMode, DiffSide, DiffState,
};
use serde::Deserialize;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use tcode_core::session::ReviewSide;

#[derive(Action, Clone, Copy, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_diff, no_json)]
pub(crate) enum DiffSelectionMenu {
    AddComment,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum FileLayout {
    #[default]
    Rows,
    Header,
    Placeholder,
}

#[derive(Clone)]
pub(crate) struct LineSelection {
    pub file: String,
    pub row_start: usize,
    pub row_end: usize,
    pub line_start: u32,
    pub line_end: u32,
    pub side: ReviewSide,
    pub start_index: usize,
    pub end_index: usize,
}

pub(crate) struct DiffList {
    pub files: Vec<PreparedFile>,
    layouts: Vec<FileLayout>,
    pub selection: Option<LineSelection>,
    pub state: Entity<DiffState>,
    dirty: Cell<bool>,
    visible_file: Rc<Cell<Option<usize>>>,
    source_anchor: Rc<RefCell<Option<DiffLinePosition>>>,
    subscription: RefCell<Option<Subscription>>,
}
impl DiffList {
    pub fn new(files: Vec<PreparedFile>, layouts: Vec<FileLayout>, cx: &mut App) -> Self {
        let state = cx.new(|cx| DiffState::new(files.iter().map(|file| file.document.clone()), cx));
        Self {
            files,
            layouts,
            state,
            selection: None,
            dirty: Cell::new(false),
            visible_file: Rc::new(Cell::new(None)),
            source_anchor: Rc::new(RefCell::new(None)),
            subscription: RefCell::new(None),
        }
    }
    pub fn layout(&self, index: usize) -> FileLayout {
        self.layouts.get(index).copied().unwrap_or_default()
    }
    pub fn relayout(&mut self) {
        let range = self.selection.as_ref().map(|selection| {
            DiffLineRange::new(
                selection.file.clone(),
                if selection.side == ReviewSide::Old {
                    DiffSide::Original
                } else {
                    DiffSide::Modified
                },
                selection.line_start as usize,
                selection.line_end as usize,
            )
        });
        self.select(range.as_ref());
        self.dirty.set(true);
    }
    pub fn set_layouts(&mut self, layouts: Vec<FileLayout>) {
        self.layouts = layouts;
    }
    pub fn scroll_to_file(&self, file: usize, cx: &mut App) {
        if let Some(document) = self.files.get(file) {
            self.visible_file.set(Some(file));
            self.source_anchor.borrow_mut().take();
            self.state
                .update(cx, |state, cx| state.scroll_to_file(&document.path, cx));
        }
    }
    pub fn top_file(&self, _split: bool) -> Option<usize> {
        self.visible_file.get()
    }
    pub fn clear_selection(&mut self, cx: &mut App) {
        self.selection = None;
        self.state
            .update(cx, |state, cx| state.set_selected_lines(None, cx));
    }
    fn select(&mut self, range: Option<&DiffLineRange>) {
        self.selection = range.and_then(|range| {
            // Host review anchors address one side of one file.
            if range.side() != range.end_side() { return None; }
            let file = self.files.iter().find(|file| file.path == range.path().as_str())?;
            if file.parse_error { return None; }
            let side = if range.side() == DiffSide::Original { ReviewSide::Old } else { ReviewSide::New };
            let index = |line: usize| file.all_rows.iter().position(|row| match side { ReviewSide::Old => row.old, ReviewSide::New => row.new } == Some(line as u32));
            let start = index(range.start())?;
            let end = index(range.end())?;
            let (start, end) = (start.min(end), start.max(end));
            Some(LineSelection { file: file.path.clone(), row_start: start, row_end: end, line_start: range.start().min(range.end()) as u32, line_end: range.start().max(range.end()) as u32, side, start_index: start, end_index: end })
        });
    }
}
pub(crate) enum GapState {
    Expandable,
    Loading,
    Fixed(Option<String>),
}
pub(crate) type FileMenu = Rc<dyn Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu>;
pub(crate) trait DiffListHost: Sized + 'static {
    fn diff_list(&self) -> Option<&DiffList>;
    fn diff_list_mut(&mut self) -> Option<&mut DiffList>;
    fn load_context(&mut self, _file: usize, _cx: &mut Context<Self>) {}
    fn has_more_files(&self, _cx: &App) -> bool {
        false
    }
    fn file_menu(&self, file: usize, cx: &App) -> FileMenu;
    fn gap_state(&self, _file: usize, _expandable: bool) -> GapState {
        GapState::Fixed(None)
    }
    fn header_note(&self, _file: usize, _cx: &mut Context<Self>) -> Option<AnyElement> {
        None
    }
    fn header_trailing(&self, _file: usize, _cx: &mut Context<Self>) -> Option<AnyElement> {
        None
    }
    fn header_clicked(&mut self, _file: usize, _cx: &mut Context<Self>) {}
    fn annotation_positions(&self, _cx: &App) -> Vec<(String, ReviewSide, u32)> {
        Vec::new()
    }
    fn row_extras(
        &self,
        _file: usize,
        _old: Option<u32>,
        _new: Option<u32>,
        _cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        Vec::new()
    }
    fn placeholder(&self, _file: usize, cx: &mut Context<Self>) -> AnyElement {
        render_notice(crate::tr!("diff.no_text").into_owned(), cx).into_any_element()
    }
    fn review_comment_menu(&self, _cx: &App) -> Option<(bool, SharedString)> {
        None
    }
}

pub(crate) fn render_list<H: DiffListHost>(
    host: &H,
    id: &'static str,
    split: bool,
    wrap: bool,
    cx: &mut Context<H>,
) -> AnyElement {
    let Some(list) = host.diff_list() else {
        return div().into_any_element();
    };
    let state = list.state.clone();
    if list.dirty.replace(false) {
        let top = list
            .visible_file
            .get()
            .and_then(|index| list.files.get(index))
            .map(|file| file.path.clone());
        state.update(cx, |state, cx| {
            state.set_files(list.files.iter().map(|file| file.document.clone()), cx);
            state.set_selected_lines(
                list.selection.as_ref().map(|selection| {
                    DiffLineRange::new(
                        selection.file.clone(),
                        if selection.side == ReviewSide::Old {
                            DiffSide::Original
                        } else {
                            DiffSide::Modified
                        },
                        selection.line_start as usize,
                        selection.line_end as usize,
                    )
                }),
                cx,
            );
            // Kit 0.7.3 has no viewport/append API. Restore the reader's last
            // observed source line when rebuilding after a host page arrives.
            if let Some(anchor) = list.source_anchor.borrow().clone() {
                state.scroll_to_line(anchor, cx);
            } else if let Some(path) = top {
                state.scroll_to_file(&path, cx);
            }
        });
    }
    state.update(cx, |state, cx| {
        state.set_mode(
            if split {
                DiffMode::Split
            } else {
                DiffMode::Unified
            },
            cx,
        );
        for (index, file) in list.files.iter().enumerate() {
            // Kit has no notice slot. Keep host-only binary/oversized details
            // in the file annotation, suppressing its empty-document notice.
            if file.all_rows.is_empty() && !file.full_source && !file.parse_error {
                state.set_file_collapsed(&file.path, true, cx);
            } else if let Some(layout) = list.layouts.get(index) {
                state.set_file_collapsed(&file.path, *layout == FileLayout::Header, cx);
            }
        }
    });
    if list.subscription.borrow().is_none() {
        *list.subscription.borrow_mut() = Some(cx.subscribe(&state, |host, _, event, cx| {
            match event {
                DiffEvent::SelectionChanged(range) => {
                    if let Some(list) = host.diff_list_mut() {
                        list.select(range.as_ref());
                    }
                }
                DiffEvent::FileCollapsed(path) | DiffEvent::FileExpanded(path) => {
                    let collapsed = matches!(event, DiffEvent::FileCollapsed(_));
                    let changed = host.diff_list().and_then(|list| {
                        list.files
                            .iter()
                            .position(|file| file.path == path.as_str())
                            .filter(|index| {
                                (list.layout(*index) == FileLayout::Header) != collapsed
                            })
                    });
                    if let Some(index) = changed {
                        host.header_clicked(index, cx);
                    }
                }
                _ => {}
            }
            cx.notify();
        }));
    }
    let paths: Rc<Vec<String>> = Rc::new(list.files.iter().map(|file| file.path.clone()).collect());
    let mut positions = host.annotation_positions(cx);
    if let Some(selection) = &list.selection {
        positions.push((selection.file.clone(), selection.side, selection.line_end));
    }
    positions.sort_by(|a, b| (&a.0, a.1 as u8, a.2).cmp(&(&b.0, b.1 as u8, b.2)));
    positions.dedup();
    let mut annotations = positions
        .into_iter()
        .map(|(path, side, line)| {
            let id = SharedString::from(format!("tcode-review:{side:?}:{line}:{path}"));
            DiffAnnotation::line(
                id,
                DiffLinePosition::new(
                    path,
                    if side == ReviewSide::Old {
                        DiffSide::Original
                    } else {
                        DiffSide::Modified
                    },
                    line as usize,
                ),
            )
        })
        .collect::<Vec<_>>();
    for (index, file) in list.files.iter().enumerate() {
        if list.layout(index) == FileLayout::Placeholder
            || file.all_rows.is_empty()
            || file.parse_error
        {
            annotations.push(DiffAnnotation::file(
                ("tcode-file-notice", index),
                file.path.clone(),
            ));
        }
    }
    // A page-end annotation also notices a scrollbar jump past the last
    // virtualized header, so host pagination cannot stall on a long file.
    if host.has_more_files(cx)
        && let Some((index, file)) = list.files.iter().enumerate().next_back()
    {
        let position = file
            .all_rows
            .iter()
            .rev()
            .find(|row| row.kind != super::parse::RowKind::Context)
            .and_then(|row| {
                row.new
                    .map(|line| (DiffSide::Modified, line))
                    .or_else(|| row.old.map(|line| (DiffSide::Original, line)))
            });
        let id = ("tcode-file-tail", index);
        annotations.push(if let Some((side, line)) = position {
            DiffAnnotation::line(
                id,
                DiffLinePosition::new(file.path.clone(), side, line as usize),
            )
        } else {
            DiffAnnotation::file(id, file.path.clone())
        });
    }
    let entity = cx.entity();
    let headers = Rc::new(RefCell::new(Vec::new()));
    let top = list.visible_file.clone();
    let header_paths = paths.clone();
    let header_geometry = headers.clone();
    let annotation_entity = entity.clone();
    let annotation_paths = paths.clone();
    let annotation_geometry = headers.clone();
    let source_anchor = list.source_anchor.clone();
    let diff = Diff::new(&state)
        .soft_wrap(wrap)
        .on_line_hover({
            let top = top.clone();
            let source_anchor = list.source_anchor.clone();
            let paths = paths.clone();
            let entity = entity.clone();
            move |position, _, cx| {
                if let Some(position) = position {
                    *source_anchor.borrow_mut() = Some(position.clone());
                }
                if let Some(index) = position.and_then(|position| {
                    paths
                        .iter()
                        .position(|path| path == position.path().as_str())
                }) && top.replace(Some(index)) != Some(index)
                {
                    cx.notify(entity.entity_id());
                }
            }
        })
        .render_header_prefix(move |file, _, _| {
            let index = header_paths
                .iter()
                .position(|path| path == file.path().as_str())
                .unwrap_or_default();
            let headers = header_geometry.clone();
            canvas(
                move |bounds, _, _| headers.borrow_mut().push((index, bounds.origin.y)),
                |_, _, _, _| {},
            )
            .absolute()
            .inset_0()
        })
        .render_header_title_suffix({
            let entity = entity.clone();
            let paths = paths.clone();
            move |file, _, cx| {
                let index = paths
                    .iter()
                    .position(|path| path == file.path().as_str())
                    .unwrap_or_default();
                entity.update(cx, |host, cx| {
                    h_flex().children(host.header_note(index, cx))
                })
            }
        })
        .render_header_suffix({
            let entity = entity.clone();
            let paths = paths.clone();
            move |file, _, cx| {
                let index = paths
                    .iter()
                    .position(|path| path == file.path().as_str())
                    .unwrap_or_default();
                entity.update(cx, |host, cx| {
                    let menu = host.file_menu(index, cx);
                    let mut suffix = h_flex()
                        .gap_2()
                        .children(host.header_trailing(index, cx))
                        .child(
                            Button::new(("diff-file-menu", index))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Ellipsis)
                                .aria_label(crate::tr!("diff.file_actions"))
                                .dropdown_menu(move |popup, window, cx| menu(popup, window, cx)),
                        );
                    if let Some(file) = host.diff_list().and_then(|list| list.files.get(index))
                        && !file.full_source
                    {
                        match host.gap_state(index, false) {
                            GapState::Expandable => {
                                suffix = suffix.child(
                                    Button::new(("diff-load-context", index))
                                        .ghost()
                                        .xsmall()
                                        .icon(crate::icon::IconName::Plus)
                                        .tooltip(crate::tr!("diff.expand_gap_all"))
                                        .on_click(cx.listener(move |host, _, _, cx| {
                                            host.load_context(index, cx)
                                        })),
                                )
                            }
                            GapState::Loading => {
                                suffix =
                                    suffix.child(crate::tr!("pull_requests.files.loading_lines"))
                            }
                            GapState::Fixed(reason) => suffix = suffix.children(reason),
                        }
                    }
                    suffix.into_any_element()
                })
            }
        })
        .annotations(annotations)
        .render_annotation(move |annotation, _, cx| {
            let index = annotation_paths
                .iter()
                .position(|path| path == annotation.path().as_str())
                .unwrap_or_default();
            if annotation.id() == &("tcode-file-tail", index).into() {
                let geometry = annotation_geometry.clone();
                return div()
                    .relative()
                    .text_color(cx.theme().muted_foreground)
                    .child(crate::tr!("pull_requests.files.loading_more").into_owned())
                    .child(
                        canvas(
                            move |bounds, _, _| {
                                geometry.borrow_mut().push((index, bounds.origin.y))
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0(),
                    )
                    .into_any_element();
            }
            annotation_entity.update(cx, |host, cx| {
                if host
                    .diff_list()
                    .and_then(|list| list.files.get(index))
                    .is_some_and(|file| file.parse_error)
                {
                    return render_notice(crate::tr!("diff.invalid_patch").into_owned(), cx)
                        .into_any_element();
                }
                if let Some(position) = annotation.position() {
                    let (old, new) = if position.side() == DiffSide::Original {
                        (Some(position.line() as u32), None)
                    } else {
                        (None, Some(position.line() as u32))
                    };
                    v_flex()
                        .children(host.row_extras(index, old, new, cx))
                        .into_any_element()
                } else {
                    host.placeholder(index, cx)
                }
            })
        });
    let diff = diff.when(
        host.review_comment_menu(cx)
            .is_some_and(|(enabled, _)| enabled),
        |diff| {
            let entity = entity.clone();
            diff.on_add_annotation(move |range, window, cx| {
                let selected = entity.update(cx, |host, _| {
                    let Some(list) = host.diff_list_mut() else {
                        return false;
                    };
                    list.select(Some(range));
                    list.selection.is_some()
                });
                if selected {
                    window.dispatch_action(Box::new(DiffSelectionMenu::AddComment), cx);
                }
            })
        },
    );
    // Diff has no public viewport query. Slots supply file visibility for
    // host paging and the sidebar without owning the list's geometry.
    div()
        .id(id)
        .debug_selector(move || id.into())
        .relative()
        .flex_1()
        .min_h_0()
        .min_w_0()
        .child(diff)
        .child(
            canvas(
                move |bounds, _, _| bounds,
                move |_, viewport, _, cx| {
                    let headers = headers.borrow();
                    let closest = headers
                        .iter()
                        .filter(|(_, y)| *y <= viewport.origin.y)
                        .max_by_key(|(_, y)| *y)
                        .or_else(|| headers.iter().min_by_key(|(_, y)| *y))
                        .map(|(index, _)| *index);
                    if let Some(index) = closest
                        && top.replace(Some(index)) != Some(index)
                    {
                        if source_anchor
                            .borrow()
                            .as_ref()
                            .is_some_and(|anchor| paths[index] != anchor.path().as_str())
                        {
                            source_anchor.borrow_mut().take();
                        }
                        cx.notify(entity.entity_id());
                    }
                },
            )
            .absolute()
            .inset_0(),
        )
        .into_any_element()
}

pub(crate) fn kind_rail(kind: FileChangeKind, cx: &App) -> Option<Hsla> {
    match kind {
        FileChangeKind::Create => Some(cx.theme().success),
        FileChangeKind::Delete => Some(cx.theme().danger),
        FileChangeKind::Rename => Some(cx.theme().info),
        FileChangeKind::Modify => None,
    }
}
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
