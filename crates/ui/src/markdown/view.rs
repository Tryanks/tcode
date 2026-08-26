//! Markdown view element adapted from gpui-component's Apache-2.0
//! `text/text_view.rs` implementation.

use std::path::{Path, PathBuf};

use crate::overlay::{Notification, OverlayExt as _};
use crate::widgets::input::{Copy, SelectAll};
use crate::widgets::menu::ContextMenuExt as _;
use gpui::{
    Action, AnyElement, App, Bounds, ClipboardItem, Element, ElementId, Entity, GlobalElementId,
    Hitbox, HitboxBehavior, InspectorElementId, InteractiveElement as _, IntoElement, LayoutId,
    MouseButton, MouseDownEvent, ParentElement as _, Pixels, StyleRefinement, Styled, Window, div,
    prelude::FluentBuilder as _,
};
use gpui_base::StyledExt as _;
use serde::Deserialize;

use super::{link_target::LinkTarget, state::MarkdownState};

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_markdown_link, no_json)]
struct OpenLink(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_markdown_link, no_json)]
struct CopyLinkAddress(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_markdown_link, no_json)]
struct CopyLinkText(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_markdown_link, no_json)]
struct OpenPath(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_markdown_link, no_json)]
struct OpenPathInZed(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_markdown_link, no_json)]
struct RevealPath(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_markdown_link, no_json)]
struct CopyPath(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_markdown_link, no_json)]
struct CopyRelativePath(String);

/// A GPUI element that renders an [`Entity<MarkdownState>`].
#[derive(Clone)]
pub struct MarkdownView {
    id: ElementId,
    state: Entity<MarkdownState>,
    style: StyleRefinement,
    selectable: Option<bool>,
    compact_headings: Option<bool>,
    base_dir: Option<PathBuf>,
}

impl MarkdownView {
    /// Create a view for an existing Markdown state entity.
    pub fn new(state: &Entity<MarkdownState>) -> Self {
        Self {
            id: ElementId::Name(state.entity_id().to_string().into()),
            state: state.clone(),
            style: StyleRefinement::default(),
            selectable: None,
            compact_headings: None,
            base_dir: None,
        }
    }

    /// Render headings at chat scale (h1 17px / h2 15px / h3+ 13.5px) instead
    /// of document scale.
    pub fn compact_headings(mut self, compact: bool) -> Self {
        self.compact_headings = Some(compact);
        self
    }

    /// Set whether text participates in window-level selection.
    pub fn selectable(mut self, selectable: bool) -> Self {
        self.selectable = Some(selectable);
        self
    }

    /// Set the directory used to resolve relative Markdown links.
    pub fn base_dir(mut self, base_dir: impl Into<PathBuf>) -> Self {
        self.base_dir = Some(base_dir.into());
        self
    }
}

impl Styled for MarkdownView {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl IntoElement for MarkdownView {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for MarkdownView {
    type RequestLayoutState = (Entity<MarkdownState>, AnyElement);
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let state = self.state.clone();
        state.update(cx, |state, cx| {
            if let Some(selectable) = self.selectable {
                state.set_selectable(selectable, cx);
            }
            if let Some(compact) = self.compact_headings {
                state.set_compact_headings(compact, cx);
            }
            if let Some(base_dir) = &self.base_dir {
                state.set_base_dir(Some(base_dir.clone()), cx);
            }
        });
        let focus_handle = state.read(cx).focus_handle.clone();
        let mut element = div()
            .key_context(super::CONTEXT)
            .track_focus(&focus_handle)
            .w_full()
            .relative()
            .on_action(move |_: &Copy, window, cx| {
                let text = gpui_base::TextSelection::selected_text(window, cx)
                    .trim()
                    .to_string();
                if text.is_empty() {
                    cx.propagate();
                } else {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            })
            .on_action({
                let state = state.clone();
                move |_: &SelectAll, window, cx| {
                    if !state.read(cx).is_selectable() {
                        cx.propagate();
                        return;
                    }
                    gpui_base::TextSelection::clear(window, cx);
                    let selection = state.read(cx).selection_handle().clone();
                    selection.set_local_selection(true, cx);
                    state.update(cx, |_, cx| cx.notify());
                }
            })
            .on_action(|action: &OpenLink, _, cx| cx.open_url(&action.0))
            .on_action(|action: &CopyLinkAddress, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(action.0.clone()))
            })
            .on_action(|action: &CopyLinkText, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(action.0.clone()))
            })
            .on_action(|action: &OpenPath, _, cx| cx.open_with_system(Path::new(&action.0)))
            .on_action(|action: &OpenPathInZed, window, cx| {
                open_in_zed(Path::new(&action.0), window, cx)
            })
            .on_action(|action: &RevealPath, _, cx| cx.reveal_path(Path::new(&action.0)))
            .on_action(|action: &CopyPath, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(action.0.clone()))
            })
            .on_action(|action: &CopyRelativePath, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(action.0.clone()))
            })
            .child(state.clone())
            .refine_style(&self.style)
            .context_menu({
                let state = state.clone();
                move |menu, _window, cx| {
                    let markdown = state.read(cx);
                    let Some(pending) = markdown.pending_context_link.clone() else {
                        return menu;
                    };
                    match pending.target {
                        LinkTarget::Web(url) => menu
                            .menu(
                                crate::tr!("markdown.link_open").into_owned(),
                                Box::new(OpenLink(url)),
                            )
                            .separator()
                            .menu(
                                crate::tr!("markdown.link_copy_address").into_owned(),
                                Box::new(CopyLinkAddress(pending.raw_url.to_string())),
                            )
                            .menu(
                                crate::tr!("markdown.link_copy_text").into_owned(),
                                Box::new(CopyLinkText(pending.text.to_string())),
                            ),
                        LinkTarget::Local(path) => {
                            let path = path.to_string_lossy().into_owned();
                            let relative_path = markdown.base_dir().map(|base_dir| {
                                tcode_services::user_files::relativize_to_workspace(&path, base_dir)
                            });
                            menu.menu(
                                crate::tr!("chat.open").into_owned(),
                                Box::new(OpenPath(path.clone())),
                            )
                            .menu(
                                crate::tr!("chat.open_zed").into_owned(),
                                Box::new(OpenPathInZed(path.clone())),
                            )
                            .menu(
                                crate::tr!("chat.reveal_in_file_manager").into_owned(),
                                Box::new(RevealPath(path.clone())),
                            )
                            .separator()
                            .menu(
                                crate::tr!("chat.copy_path").into_owned(),
                                Box::new(CopyPath(path)),
                            )
                            .when_some(
                                relative_path,
                                |menu, relative_path| {
                                    menu.menu(
                                        crate::tr!("markdown.path_copy_relative").into_owned(),
                                        Box::new(CopyRelativePath(relative_path)),
                                    )
                                },
                            )
                        }
                    }
                }
            })
            .into_any_element();
        let layout_id = element.request_layout(window, cx);
        (layout_id, (state, element))
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        request_layout.1.prepaint(window, cx);
        window.insert_hitbox(bounds, HitboxBehavior::Normal)
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let selectable = request_layout.0.read(cx).is_selectable();
        if selectable {
            request_layout.0.read(cx).selection_adapter.begin_frame();
        }
        // Capture-phase so this runs before the Inline children's bubble-phase
        // handlers repopulate the pending link: a right-click on a non-link
        // area must not resurface the previous link's context menu.
        window.on_mouse_event({
            let hitbox = hitbox.clone();
            let state = request_layout.0.clone();
            move |event: &MouseDownEvent, phase, window, cx| {
                if phase.capture()
                    && event.button == MouseButton::Right
                    && hitbox.is_hovered(window)
                {
                    state.update(cx, |state, cx| state.set_pending_context_link(None, cx));
                }
            }
        });
        request_layout.1.paint(window, cx);
        if selectable {
            let (adapter, content_bounds, scroll_offset) = {
                let state = request_layout.0.read(cx);
                (
                    state.selection_adapter.clone(),
                    state.bounds,
                    state.list_state.scroll_px_offset_for_scrollbar(),
                )
            };
            let y = hitbox.bounds.origin.y.as_f32().max(0.).to_bits() as u64;
            let x = hitbox.bounds.origin.x.as_f32().max(0.).to_bits() as u64;
            adapter.register(
                hitbox.clone(),
                content_bounds,
                scroll_offset,
                (y << 32) | x,
                window,
                cx,
            );
        }
    }
}

fn open_in_zed(path: &Path, window: &mut Window, cx: &mut App) {
    if tcode_services::desktop::open_in_zed(path).is_err() {
        window.push_notification(
            Notification::error(crate::tr!("errors.zed_cli_missing")),
            cx,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use gpui::{
        AppContext as _, Context, Entity, IntoElement, ListAlignment, ListState, Modifiers,
        MouseButton, Render, TestAppContext, VisualTestContext, Window, div, point, px,
    };

    use super::*;
    use gpui_base::TextSelectionLayer;

    struct TestRoot {
        markdown: Entity<MarkdownState>,
    }

    struct CrossViewRoot {
        first: Entity<MarkdownState>,
        second: Entity<MarkdownState>,
    }

    struct DragAreaRoot {
        markdown: Entity<MarkdownState>,
    }

    struct OuterListRoot {
        markdown: Entity<MarkdownState>,
        list_state: ListState,
    }

    impl TestRoot {
        fn new(text: &str, cx: &mut Context<Self>) -> Self {
            let text = text.to_string();
            Self {
                markdown: cx.new(|cx| MarkdownState::new(&text, cx)),
            }
        }
    }

    impl Render for TestRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(320.))
                .child(TextSelectionLayer)
                .child(
                    div()
                        .h(px(24.))
                        .overflow_hidden()
                        .child(MarkdownView::new(&self.markdown).selectable(true)),
                )
                .child(div().h(px(40.)).child("footer"))
        }
    }

    impl CrossViewRoot {
        fn new(cx: &mut Context<Self>) -> Self {
            Self {
                first: cx.new(|cx| MarkdownState::new("Hello world", cx)),
                second: cx.new(|cx| MarkdownState::new("Second message", cx)),
            }
        }
    }

    struct MultiBlockRoot {
        markdown: Entity<MarkdownState>,
    }

    struct RightClickRoot {
        markdown: Entity<MarkdownState>,
        bubbled_right_clicks: Rc<RefCell<usize>>,
    }

    impl RightClickRoot {
        fn new(cx: &mut Context<Self>) -> Self {
            Self {
                markdown: cx
                    .new(|cx| MarkdownState::new("Alpha beta\n\n[click](https://example.com)", cx)),
                bubbled_right_clicks: Rc::default(),
            }
        }
    }

    impl Render for RightClickRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let bubbled = self.bubbled_right_clicks.clone();
            div()
                .size_full()
                .child(TextSelectionLayer)
                .child(MarkdownView::new(&self.markdown).selectable(true))
                .on_mouse_down(MouseButton::Right, move |_, _, _| {
                    *bubbled.borrow_mut() += 1;
                })
        }
    }

    impl MultiBlockRoot {
        fn new(cx: &mut Context<Self>) -> Self {
            Self::with_text("Alpha beta\n\nGamma delta", cx)
        }

        fn with_text(text: &str, cx: &mut Context<Self>) -> Self {
            Self {
                markdown: cx.new(|cx| MarkdownState::new(text, cx)),
            }
        }
    }

    impl Render for MultiBlockRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(TextSelectionLayer)
                .child(MarkdownView::new(&self.markdown).selectable(true))
        }
    }

    impl DragAreaRoot {
        fn new(cx: &mut Context<Self>) -> Self {
            Self {
                markdown: cx.new(|cx| MarkdownState::new("Hello world", cx)),
            }
        }
    }

    impl Render for DragAreaRoot {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(TextSelectionLayer)
                .child(crate::window_drag_area(
                    "test-titlebar",
                    div().w_full().h(px(52.)),
                    window,
                    cx,
                ))
                .child(
                    div()
                        .h(px(40.))
                        .child(MarkdownView::new(&self.markdown).selectable(true)),
                )
        }
    }

    impl Render for CrossViewRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .pt(px(10.))
                .child(TextSelectionLayer)
                .child(
                    div()
                        .h(px(40.))
                        .child(MarkdownView::new(&self.first).selectable(true)),
                )
                .child(
                    div()
                        .h(px(40.))
                        .child(MarkdownView::new(&self.second).selectable(true)),
                )
        }
    }

    impl OuterListRoot {
        fn new(cx: &mut Context<Self>) -> Self {
            let text = (0..12)
                .map(|ix| format!("## Section {ix}\n\nParagraph {ix} with enough text to render."))
                .collect::<Vec<_>>()
                .join("\n\n");
            Self {
                markdown: cx.new(|cx| MarkdownState::new(&text, cx)),
                list_state: ListState::new(1, ListAlignment::Top, px(100.)),
            }
        }
    }

    impl Render for OuterListRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let markdown = self.markdown.clone();
            gpui::list(self.list_state.clone(), move |_, _, _| {
                div()
                    .w_full()
                    .child(MarkdownView::new(&markdown))
                    .into_any_element()
            })
            .size_full()
        }
    }

    #[gpui::test]
    fn renders_gfm_document_without_panicking(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (_, cx) = cx.add_window_view(|_, cx| {
            TestRoot::new(
                "# Heading\n\n> quote with `inline code`\n\n- [x] done\n\n| a | b |\n|:-|--:|\n| 1 | 2 |\n\n```rust\nfn main() {}\n```",
                cx,
            )
        });
        let cx: &mut VisualTestContext = cx;
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
    }

    #[gpui::test]
    fn markdown_reports_intrinsic_height_inside_outer_list(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (view, cx) = cx.add_window_view(|_, cx| OuterListRoot::new(cx));
        let cx: &mut VisualTestContext = cx;
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let height = view.read_with(cx, |root, cx| root.markdown.read(cx).bounds.size.height);
        assert!(
            height > px(100.),
            "nested MarkdownView collapsed to {height:?} instead of reporting content height"
        );
    }

    #[gpui::test]
    fn streamed_append_preserves_earlier_block_measurements(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (view, cx) =
            cx.add_window_view(|_, cx| TestRoot::new("stable block\n\nstreaming block", cx));
        let cx: &mut VisualTestContext = cx;
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        assert!(view.read_with(cx, |root, cx| {
            root.markdown.read(cx).has_measured_block(0)
        }));
        view.update(cx, |root, cx| {
            root.markdown.update(cx, |markdown, cx| {
                markdown.push_str(" delta", cx);
                assert!(markdown.has_measured_block(0));
                assert!(!markdown.has_measured_block(1));
            });
        });
    }

    #[gpui::test]
    fn wide_table_shrinks_to_viewport_and_wraps(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (_, cx) = cx.add_window_view(|_, cx| {
            TestRoot::new(
                "| column | value |\n| --- | --- |\n| this-cell-is-deliberately-much-wider-than-the-markdown-viewport | another-wide-value |",
                cx,
            )
        });
        let cx: &mut VisualTestContext = cx;
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let track = cx
            .debug_bounds("markdown-table-track-root-0")
            .expect("table track was painted");
        assert_eq!(
            track.size.width,
            px(320.),
            "wide table did not shrink to the markdown viewport"
        );
        assert!(
            track.size.height > px(68.),
            "wide table did not grow tall enough for wrapped content: {:?}",
            track.size.height
        );
    }

    #[gpui::test]
    fn small_table_stretches_to_viewport_width(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (_, cx) =
            cx.add_window_view(|_, cx| TestRoot::new("| a | b |\n| --- | --- |\n| 1 | 2 |", cx));
        let cx: &mut VisualTestContext = cx;
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let track = cx
            .debug_bounds("markdown-table-track-root-0")
            .expect("table track was painted");
        assert_eq!(
            track.size.width,
            px(320.),
            "small table did not stretch to the markdown viewport"
        );
    }

    #[gpui::test]
    fn right_aligned_column_justifies_cell_content(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (_, cx) = cx.add_window_view(|_, cx| {
            TestRoot::new("| num | label |\n| ---: | --- |\n| 1 | value |", cx)
        });
        let cx: &mut VisualTestContext = cx;
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let cell = cx
            .debug_bounds("markdown-table-cell-1-0")
            .expect("right-aligned cell was painted");
        let content = cx
            .debug_bounds("markdown-table-cell-content-1-0")
            .expect("cell content was painted");
        // The stretched column is far wider than "1"; justify_end must push the
        // content against the cell's right padding edge (px_2 = 8px).
        assert!(
            cell.right() - content.right() <= px(9.),
            "content {content:?} is not right-justified inside cell {cell:?}"
        );
        assert!(
            content.left() - cell.left() > px(20.),
            "content {content:?} hugs the left edge of cell {cell:?}"
        );
    }

    #[gpui::test]
    fn wide_table_track_contains_the_last_cell(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let header = (0..20).map(|ix| format!("column-{ix}")).collect::<Vec<_>>();
        let separator = vec!["---"; header.len()];
        let values = (0..header.len())
            .map(|ix| format!("value-{ix}"))
            .collect::<Vec<_>>();
        let markdown = format!(
            "| {} |\n| {} |\n| {} |",
            header.join(" | "),
            separator.join(" | "),
            values.join(" | ")
        );
        let (_, cx) = cx.add_window_view(|_, cx| TestRoot::new(&markdown, cx));
        let cx: &mut VisualTestContext = cx;
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let track = cx
            .debug_bounds("markdown-table-track-root-0")
            .expect("table track was painted");
        let last_cell = cx
            .debug_bounds("markdown-table-cell-0-19")
            .expect("last table cell was painted");
        assert_eq!(
            last_cell.right() + px(1.),
            track.right(),
            "last cell {:?} did not end at the track's inner right edge {:?}",
            last_cell,
            track
        );
    }

    #[gpui::test]
    fn streaming_table_growth_updates_track_height(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (view, cx) =
            cx.add_window_view(|_, cx| TestRoot::new("| column |\n| --- |\n| streaming", cx));
        let cx: &mut VisualTestContext = cx;
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let initial_height = cx
            .debug_bounds("markdown-table-track-root-0")
            .expect("initial table track was painted")
            .size
            .height;

        view.update(cx, |root, cx| {
            root.markdown.update(cx, |markdown, cx| {
                markdown.push_str(
                    "-cell-that-grows-much-wider-and-keeps-growing-until-it-wraps |",
                    cx,
                );
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let grown_height = cx
            .debug_bounds("markdown-table-track-root-0")
            .expect("grown table track was painted")
            .size
            .height;

        assert!(
            grown_height > initial_height,
            "streamed table height stayed at {initial_height:?} instead of growing: {grown_height:?}"
        );
    }

    #[gpui::test]
    fn clipped_markdown_cannot_start_selection(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (_view, cx) =
            cx.add_window_view(|_, cx| TestRoot::new("visible\n\nhidden selection text", cx));
        let cx: &mut VisualTestContext = cx;
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.simulate_mouse_down(
            point(px(10.), px(34.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.simulate_mouse_move(
            point(px(90.), px(34.)),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.simulate_mouse_up(
            point(px(90.), px(34.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        let selected = cx.update(gpui_base::TextSelection::selected_text);
        assert!(selected.is_empty(), "unexpected selection: {selected:?}");
    }

    #[gpui::test]
    fn press_on_titlebar_drag_area_does_not_start_selection(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (_, cx) = cx.add_window_view(|_, cx| DragAreaRoot::new(cx));
        let cx: &mut VisualTestContext = cx;
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        cx.simulate_mouse_down(
            point(px(10.), px(20.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.simulate_mouse_move(
            point(px(80.), px(70.)),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.simulate_mouse_up(
            point(px(80.), px(70.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.update(|window, cx| {
            let _ = window.draw(cx);
            assert!(
                gpui_base::TextSelection::selected_text(window, cx).is_empty(),
                "a titlebar press started a text selection"
            );
        });
    }

    #[gpui::test]
    fn cross_view_drag_copies_in_document_order(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (_, cx) = cx.add_window_view(|_, cx| CrossViewRoot::new(cx));
        let cx: &mut VisualTestContext = cx;
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let start = point(px(1.), px(15.));
        let end = point(px(300.), px(70.));
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::default());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let selected = cx.update(gpui_base::TextSelection::selected_text);
        assert_eq!(selected, "Hello world\nSecond message");
    }

    #[gpui::test]
    fn drag_across_paragraphs_copies_both_blocks(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (view, cx) = cx.add_window_view(|_, cx| MultiBlockRoot::new(cx));
        let cx: &mut VisualTestContext = cx;
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        // Anchor mid-first-paragraph, cursor mid-second-paragraph, using the
        // measured block bounds so the test tracks real layout.
        let (start, end) = view.read_with(cx, |root, cx| {
            let state = root.markdown.read(cx);
            let first = state.list_state.bounds_for_item(0).unwrap();
            let second = state.list_state.bounds_for_item(1).unwrap();
            (
                point(px(1.), first.center().y),
                point(px(300.), second.center().y),
            )
        });
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::default());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let selected = cx.update(gpui_base::TextSelection::selected_text);
        assert_eq!(selected, "Alpha beta\nGamma delta");
    }

    #[gpui::test]
    fn drag_across_mixed_blocks_copies_every_block_kind(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (view, cx) = cx.add_window_view(|_, cx| {
            MultiBlockRoot::with_text(
                "# Title\n\nIntro para\n\n- item one\n- item two\n\n```\ncode line\n```\n\nTail para",
                cx,
            )
        });
        let cx: &mut VisualTestContext = cx;
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let (start, end) = view.read_with(cx, |root, cx| {
            let state = root.markdown.read(cx);
            let first = state.list_state.bounds_for_item(0).unwrap();
            let count = state.list_state.item_count();
            let last = state.list_state.bounds_for_item(count - 1).unwrap();
            (
                point(px(1.), first.center().y),
                point(px(300.), last.center().y),
            )
        });
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::default());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let selected = cx.update(gpui_base::TextSelection::selected_text);
        for fragment in ["Title", "Intro para", "item one", "item two", "code line"] {
            assert!(
                selected.contains(fragment),
                "missing {fragment:?} in {selected:?}"
            );
        }
    }

    #[gpui::test]
    fn link_click_survives_pixel_jitter_but_not_a_drag(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (view, cx) = cx.add_window_view(|_, cx| RightClickRoot::new(cx));
        let cx: &mut VisualTestContext = cx;
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let link_pos = view.read_with(cx, |root, cx| {
            let link = root.markdown.read(cx).list_state.bounds_for_item(1).unwrap();
            point(px(5.), link.center().y)
        });

        // A drag that starts on the link is a selection, not a click.
        let far = point(link_pos.x + px(60.), link_pos.y);
        cx.simulate_mouse_down(link_pos, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(far, Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_up(far, MouseButton::Left, Modifiers::default());
        assert_eq!(cx.opened_url(), None, "a drag must not open the link");

        // A press-and-release with a pixel of jitter is still a click.
        let jittered = point(link_pos.x + px(1.), link_pos.y + px(1.));
        cx.simulate_mouse_down(link_pos, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(jittered, Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_up(jittered, MouseButton::Left, Modifiers::default());
        assert_eq!(cx.opened_url().as_deref(), Some("https://example.com"));
    }

    #[gpui::test]
    fn link_context_menu_action_opens_the_url(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        cx.update(crate::widgets::menu::init);
        let (view, cx) = cx.add_window_view(|_, cx| RightClickRoot::new(cx));
        let cx: &mut VisualTestContext = cx;
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let link_pos = view.read_with(cx, |root, cx| {
            let link = root.markdown.read(cx).list_state.bounds_for_item(1).unwrap();
            point(px(5.), link.center().y)
        });
        cx.simulate_mouse_down(link_pos, MouseButton::Right, Modifiers::default());
        cx.simulate_mouse_up(link_pos, MouseButton::Right, Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        // Select "Open Link" and confirm; the action must dispatch through
        // the trigger's ancestor chain to the OpenLink handler.
        cx.simulate_keystrokes("down enter");
        cx.run_until_parked();
        assert_eq!(cx.opened_url().as_deref(), Some("https://example.com"));
    }

    #[gpui::test]
    fn right_click_off_link_is_swallowed_so_no_menu_opens(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (view, cx) = cx.add_window_view(|_, cx| RightClickRoot::new(cx));
        let cx: &mut VisualTestContext = cx;
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let (text_pos, link_pos) = view.read_with(cx, |root, cx| {
            let state = root.markdown.read(cx);
            let text = state.list_state.bounds_for_item(0).unwrap();
            let link = state.list_state.bounds_for_item(1).unwrap();
            (
                point(px(5.), text.center().y),
                point(px(5.), link.center().y),
            )
        });

        // Plain (and selectable) text: the press must be swallowed before the
        // context-menu popover — and thus before the root handler — sees it.
        cx.simulate_mouse_down(text_pos, MouseButton::Right, Modifiers::default());
        cx.simulate_mouse_up(text_pos, MouseButton::Right, Modifiers::default());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let bubbled = view.read_with(cx, |root, _| *root.bubbled_right_clicks.borrow());
        assert_eq!(bubbled, 0, "right-click on plain text must not propagate");

        let focused = cx.update(|window, cx| window.focused(cx));
        let markdown_focus =
            view.read_with(cx, |root, cx| root.markdown.read(cx).focus_handle.clone());
        assert!(
            focused.is_none() || focused == Some(markdown_focus.clone()),
            "no menu may take focus on a plain-text right-click"
        );

        // A link still surfaces its context menu: the popover consumes the
        // press (so the root handler stays at zero) and focuses the menu.
        cx.simulate_mouse_down(link_pos, MouseButton::Right, Modifiers::default());
        cx.simulate_mouse_up(link_pos, MouseButton::Right, Modifiers::default());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let focused = cx.update(|window, cx| window.focused(cx));
        assert!(
            focused.is_some_and(|focused| focused != markdown_focus),
            "a link right-click must open and focus the context menu"
        );
    }
}
