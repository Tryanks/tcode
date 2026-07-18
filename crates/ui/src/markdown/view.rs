//! Markdown view element adapted from gpui-component's Apache-2.0
//! `text/text_view.rs` implementation.

use gpui::{
    AnyElement, App, Bounds, ClipboardItem, Element, ElementId, Entity, GlobalElementId, Hitbox,
    HitboxBehavior, InspectorElementId, InteractiveElement as _, IntoElement, LayoutId,
    ParentElement as _, Pixels, StyleRefinement, Styled, Window, div,
};
use gpui_component::StyledExt as _;
use gpui_component::input::{Copy, SelectAll};

use super::{state::MarkdownState, style::TextViewStyle, window_selection};

/// A GPUI element that renders an [`Entity<MarkdownState>`].
#[derive(Clone)]
pub struct MarkdownView {
    id: ElementId,
    state: Entity<MarkdownState>,
    text_view_style: TextViewStyle,
    style: StyleRefinement,
    selectable: Option<bool>,
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
            .child(state.clone())
            .refine_style(&self.style)
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
        request_layout.1.paint(window, cx);
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
    fn wide_table_keeps_its_intrinsic_width_inside_viewport(cx: &mut TestAppContext) {
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
        assert!(
            track.size.width > px(320.),
            "wide table collapsed to viewport width {:?}",
            track.size.width
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
