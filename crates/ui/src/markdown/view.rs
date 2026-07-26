//! Markdown view element adapted from gpui-component's Apache-2.0
//! `text/text_view.rs` implementation.

use std::path::{Path, PathBuf};

use gpui::{
    Action, AnyElement, App, Bounds, ClipboardItem, Element, ElementId, Entity, GlobalElementId,
    Hitbox, HitboxBehavior, InspectorElementId, InteractiveElement as _, IntoElement, LayoutId,
    MouseButton, MouseDownEvent, ParentElement as _, Pixels, StyleRefinement, Styled, Window, div,
    prelude::FluentBuilder as _,
};
use gpui_component::{
    StyledExt as _, WindowExt as _,
    input::{Copy, SelectAll},
    menu::ContextMenuExt as _,
    notification::Notification,
};
use serde::Deserialize;

use super::{
    link_target::LinkTarget, state::MarkdownState, style::TextViewStyle, window_selection,
};

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
    text_view_style: TextViewStyle,
    style: StyleRefinement,
    selectable: Option<bool>,
    base_dir: Option<PathBuf>,
}

impl MarkdownView {
    /// Create a view for an existing Markdown state entity.
    pub fn new(state: &Entity<MarkdownState>) -> Self {
        Self {
            id: ElementId::Name(state.entity_id().to_string().into()),
            state: state.clone(),
            text_view_style: TextViewStyle::default(),
            style: StyleRefinement::default(),
            selectable: None,
            base_dir: None,
        }
    }

    /// Set the Markdown presentation style.
    pub fn style(mut self, style: TextViewStyle) -> Self {
        self.text_view_style = style;
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
            state.style = self.text_view_style.clone();
            if let Some(selectable) = self.selectable {
                state.set_selectable(selectable, cx);
            }
            if let Some(base_dir) = &self.base_dir {
                state.set_base_dir(Some(base_dir.clone()), cx);
            }
        });
        let focus_handle = state.read(cx).focus_handle.clone();
        let copy_state = state.clone();
        let mut element = div()
            .key_context(super::CONTEXT)
            .track_focus(&focus_handle)
            .w_full()
            .relative()
            .on_action(move |_: &Copy, window, cx| {
                let mut text = window_selection::window_selected_text(window, cx);
                if text.is_empty() {
                    text = copy_state.read(cx).selected_text();
                }
                let text = text.trim().to_string();
                if text.is_empty() {
                    cx.propagate();
                } else {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            })
            .on_action({
                let state = state.clone();
                move |_: &SelectAll, _, cx| {
                    if !state.read(cx).is_selectable() {
                        cx.propagate();
                        return;
                    }
                    window_selection::clear_window_selection_for_select_all(&state, cx);
                    state.update(cx, |state, cx| state.select_all(cx));
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
                                tcode_i18n::tr!("markdown.link_open").into_owned(),
                                Box::new(OpenLink(url)),
                            )
                            .separator()
                            .menu(
                                tcode_i18n::tr!("markdown.link_copy_address").into_owned(),
                                Box::new(CopyLinkAddress(pending.raw_url.to_string())),
                            )
                            .menu(
                                tcode_i18n::tr!("markdown.link_copy_text").into_owned(),
                                Box::new(CopyLinkText(pending.text.to_string())),
                            ),
                        LinkTarget::Local(path) => {
                            let path = path.to_string_lossy().into_owned();
                            let relative_path = markdown.base_dir().map(|base_dir| {
                                tcode_runtime::ui_facade::relativize_to_workspace(&path, base_dir)
                            });
                            menu.menu(
                                tcode_i18n::tr!("chat.open").into_owned(),
                                Box::new(OpenPath(path.clone())),
                            )
                            .menu(
                                tcode_i18n::tr!("chat.open_zed").into_owned(),
                                Box::new(OpenPathInZed(path.clone())),
                            )
                            .menu(
                                tcode_i18n::tr!("chat.reveal_in_file_manager").into_owned(),
                                Box::new(RevealPath(path.clone())),
                            )
                            .separator()
                            .menu(
                                tcode_i18n::tr!("chat.copy_path").into_owned(),
                                Box::new(CopyPath(path)),
                            )
                            .when_some(
                                relative_path,
                                |menu, relative_path| {
                                    menu.menu(
                                        tcode_i18n::tr!("markdown.path_copy_relative").into_owned(),
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
        if request_layout.0.read(cx).is_selectable() {
            window_selection::register_selectable_text_view(&request_layout.0, hitbox, window, cx);
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
    }
}

fn open_in_zed(path: &Path, window: &mut Window, cx: &mut App) {
    if tcode_runtime::ui_facade::open_in_zed(path).is_err() {
        window.push_notification(
            Notification::error(tcode_i18n::tr!("errors.zed_cli_missing")),
            cx,
        );
    }
}

#[cfg(test)]
mod tests {
    use gpui::{
        AppContext as _, Context, Entity, IntoElement, ListAlignment, ListState, Modifiers,
        MouseButton, ParentElement as _, Render, Styled as _, TestAppContext, VisualTestContext,
        Window, div, point, px,
    };

    use super::*;
    use crate::markdown::window_selection::TextSelectionController;

    struct TestRoot {
        markdown: Entity<MarkdownState>,
    }

    struct CrossViewRoot {
        first: Entity<MarkdownState>,
        second: Entity<MarkdownState>,
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
                .child(TextSelectionController)
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

    impl Render for CrossViewRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .pt(px(10.))
                .child(TextSelectionController)
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
        cx.update(gpui_component::init);
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
        cx.update(gpui_component::init);
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
        cx.update(gpui_component::init);
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
        cx.update(gpui_component::init);
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
        cx.update(gpui_component::init);
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
        cx.update(gpui_component::init);
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
        cx.update(gpui_component::init);
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
        cx.update(gpui_component::init);
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
        cx.update(gpui_component::init);
        cx.update(crate::markdown::init);
        let (view, cx) =
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
        let selected = view.read_with(cx, |root, cx| root.markdown.read(cx).selected_text());
        assert!(selected.is_empty(), "unexpected selection: {selected:?}");
    }

    #[gpui::test]
    fn cross_view_drag_copies_in_document_order(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
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

        let selected = cx.update(|window, cx| {
            crate::markdown::window_selection::window_selected_text(window, cx)
        });
        assert_eq!(selected, "Hello world\nSecond message");
    }
}
