//! Markdown view element adapted from gpui-component's Apache-2.0
//! `text/text_view.rs` implementation.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::theme::ActiveTheme as _;
use crate::touch_selection::SelectAllTouched;
use crate::widgets::input::SelectAll;
use crate::widgets::menu::{ContextMenuExt as _, CopyText, OpenUrl, PopupMenu};
use gpui::{
    Action, AnyElement, App, Bounds, Element, ElementId, Entity, GlobalElementId, Hitbox,
    HitboxBehavior, InspectorElementId, InteractiveElement as _, IntoElement, LayoutId,
    MouseButton, MouseDownEvent, ParentElement as _, Pixels, StyleRefinement, Styled, Window, div,
};
use gpui_base::{StyledExt as _, TouchHandleLayout};
use serde::Deserialize;

use super::{
    link_target::LinkTarget,
    state::{MarkdownState, PendingContextTarget},
};

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_markdown, no_json)]
struct OpenImage {
    url: String,
    title: String,
}

/// Items a surface adds below the view's own: a message offers to copy or
/// rewind itself, a plan to download itself.
pub type MenuExtension = Rc<dyn Fn(PopupMenu, &mut Window, &mut App) -> PopupMenu>;

/// A GPUI element that renders an [`Entity<MarkdownState>`].
#[derive(Clone)]
pub struct MarkdownView {
    id: ElementId,
    state: Entity<MarkdownState>,
    style: StyleRefinement,
    selectable: Option<bool>,
    compact_headings: Option<bool>,
    base_dir: Option<PathBuf>,
    menu_extension: Option<MenuExtension>,
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
            menu_extension: None,
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

    /// Append the surface's own items to the view's context menu.
    pub fn menu_extension(mut self, extension: MenuExtension) -> Self {
        self.menu_extension = Some(extension);
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

/// What the view laid out for the frame: its hitbox, and the touch handles
/// it owns.
pub struct MarkdownViewPrepaintState {
    hitbox: Hitbox,
    touch_handles: TouchHandleLayout,
}

impl Element for MarkdownView {
    type RequestLayoutState = (Entity<MarkdownState>, AnyElement);
    type PrepaintState = MarkdownViewPrepaintState;

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
            // The edit menu's Select All keeps the window selection the
            // long press made, and with it the handles and the menu: the
            // inlines paint everything selected and report the new ends,
            // which the handles then drag from, back into a point selection.
            .on_action({
                let state = state.clone();
                move |_: &SelectAllTouched, _, cx| {
                    if !state.read(cx).is_selectable() {
                        cx.propagate();
                        return;
                    }
                    let selection = state.read(cx).selection_handle().clone();
                    selection.set_local_selection(true, cx);
                    state.update(cx, |_, cx| cx.notify());
                }
            })
            .on_action({
                let state = state.clone();
                move |action: &OpenImage, window, cx| {
                    let source = state.read(cx).image_source(&action.url.clone().into());
                    crate::image_viewer::open(source, action.title.clone(), window, cx);
                }
            })
            .child(state.clone())
            .refine_style(&self.style)
            .context_menu({
                let state = state.clone();
                let extension = self.menu_extension.clone();
                move |menu, window, cx| {
                    let (selectable, pending, base_dir) = {
                        let markdown = state.read(cx);
                        (
                            markdown.is_selectable(),
                            markdown.pending_context.clone(),
                            markdown.base_dir().map(Path::to_path_buf),
                        )
                    };
                    let menu = if selectable {
                        menu.selection_items(true, window, cx)
                    } else {
                        menu
                    };
                    let menu = match pending {
                        None => menu,
                        Some(PendingContextTarget::Link(pending)) => match pending.target {
                            LinkTarget::Web(url) => menu
                                .separator()
                                .menu(
                                    crate::tr!("markdown.link_open").into_owned(),
                                    Box::new(OpenUrl(url)),
                                )
                                .menu(
                                    crate::tr!("markdown.link_copy_address").into_owned(),
                                    Box::new(CopyText(pending.raw_url.to_string())),
                                )
                                .menu(
                                    crate::tr!("markdown.link_copy_text").into_owned(),
                                    Box::new(CopyText(pending.text.to_string())),
                                ),
                            LinkTarget::Local(path) => menu
                                .separator()
                                .path_items(&path.to_string_lossy(), base_dir.as_deref()),
                        },
                        Some(PendingContextTarget::CodeBlock(code)) => menu.separator().menu(
                            crate::tr!("markdown.code_copy").into_owned(),
                            Box::new(CopyText(code)),
                        ),
                        Some(PendingContextTarget::Image { url, title }) => {
                            let menu = menu.separator().menu(
                                crate::tr!("markdown.open_image").into_owned(),
                                Box::new(OpenImage {
                                    url: url.to_string(),
                                    title,
                                }),
                            );
                            match state.read(cx).resolve_link(&url) {
                                LinkTarget::Web(address) => menu.menu(
                                    crate::tr!("markdown.image_copy_address").into_owned(),
                                    Box::new(CopyText(address)),
                                ),
                                LinkTarget::Local(path) => menu
                                    .separator()
                                    .path_items(&path.to_string_lossy(), base_dir.as_deref()),
                            }
                        }
                    };
                    match &extension {
                        Some(extension) => extension(menu.separator(), window, cx),
                        None => menu,
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
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        // Over the text, so after its hitbox.
        let state = request_layout.0.read(cx);
        let touch_handles = if state.is_selectable() {
            state.selection_adapter.prepaint_touch_handles(window, cx)
        } else {
            TouchHandleLayout::default()
        };
        MarkdownViewPrepaintState {
            hitbox,
            touch_handles,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let hitbox = &prepaint.hitbox;
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
                    state.update(cx, |state, cx| state.set_pending_context(None, cx));
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
            // The handles of a touch selection go over the text, and under
            // whatever is painted over the text after it.
            adapter.paint_touch_handles(
                &prepaint.touch_handles,
                cx.theme().selection.alpha(1.),
                window,
                cx,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use gpui::{
        AppContext as _, Context, Entity, IntoElement, ListAlignment, ListState, Modifiers,
        MouseButton, Render, StatefulInteractiveElement as _, TestAppContext, VisualTestContext,
        Window, div, point, px,
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

    struct SwallowedClickRoot {
        markdown: Entity<MarkdownState>,
    }

    impl Render for SwallowedClickRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(TextSelectionLayer)
                .child(
                    div()
                        .id("swallowing-button")
                        .w_full()
                        .h(px(52.))
                        .on_click(|_, window, cx| {
                            crate::widgets::stop_click_propagation(window, cx)
                        }),
                )
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
        fn new(text: &str, cx: &mut Context<Self>) -> Self {
            let text = text.to_string();
            Self {
                markdown: cx.new(|cx| MarkdownState::new(&text, cx)),
                list_state: ListState::new(1, ListAlignment::Top, px(100.)),
            }
        }
    }

    impl Render for OuterListRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let markdown = self.markdown.clone();
            div().size_full().child(TextSelectionLayer).child(
                gpui::list(self.list_state.clone(), move |_, _, _| {
                    div()
                        .w_full()
                        .child(MarkdownView::new(&markdown).selectable(true))
                        .into_any_element()
                })
                .size_full(),
            )
        }
    }

    #[gpui::test]
    fn long_blocks_in_a_timeline_row_lay_out_only_their_visible_lines(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let code = (0..3_000)
            .map(|ix| format!("let line_{ix} = {ix};"))
            .collect::<Vec<_>>()
            .join("\n");
        let list = (0..1_000)
            .map(|ix| format!("- item {ix}"))
            .collect::<Vec<_>>()
            .join("\n");
        for (source, near, far, last) in [
            (
                format!("```rust\n{code}\n```"),
                "markdown-code-line-5",
                "markdown-code-line-2500",
                "markdown-code-line-2999",
            ),
            (
                list,
                "markdown-list-item-root-0-5",
                "markdown-list-item-root-0-900",
                "markdown-list-item-root-0-999",
            ),
        ] {
            let (view, cx) = cx.add_window_view(|_, cx| OuterListRoot::new(&source, cx));
            let cx: &mut VisualTestContext = cx;
            cx.simulate_resize(gpui::size(px(393.), px(852.)));
            for _ in 0..2 {
                cx.run_until_parked();
                cx.update(|window, cx| _ = window.draw(cx));
            }
            assert!(cx.debug_bounds(near).is_some());
            assert!(
                cx.debug_bounds(far).is_none(),
                "{far} was laid out while the row shows only the top of the block"
            );

            view.update(cx, |root, _| root.list_state.scroll_to_end());
            cx.update(|window, cx| _ = window.draw(cx));
            assert!(cx.debug_bounds(last).is_some());
            assert!(cx.debug_bounds(near).is_none());
        }
    }

    #[gpui::test]
    fn drag_across_a_long_code_block_copies_lines_never_painted(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let lines = (0..3_000)
            .map(|ix| format!("line {ix}"))
            .collect::<Vec<_>>();
        let source = format!("```text\n{}\n```", lines.join("\n"));
        let (view, cx) = cx.add_window_view(|_, cx| OuterListRoot::new(&source, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        for _ in 0..2 {
            cx.run_until_parked();
            cx.update(|window, cx| _ = window.draw(cx));
        }

        let anchor = cx.debug_bounds("markdown-code-line-5").unwrap();
        let first = cx.debug_bounds("markdown-code-line-30").unwrap();
        cx.simulate_mouse_down(
            point(anchor.left() + px(1.), anchor.center().y),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.update(|window, cx| _ = window.draw(cx));
        cx.simulate_mouse_move(
            point(px(380.), first.center().y),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.update(|window, cx| _ = window.draw(cx));
        view.update(cx, |root, _| root.list_state.scroll_to_end());
        cx.update(|window, cx| _ = window.draw(cx));
        let last = cx.debug_bounds("markdown-code-line-2999").unwrap();
        let end = point(px(380.), last.center().y);
        cx.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));
        cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));

        let selected = cx.update(gpui_base::TextSelection::selected_text);
        assert_eq!(selected, lines[5..].join("\n"));
    }

    #[gpui::test]
    fn math_render_source_switch_and_streaming_fallback(cx: &mut TestAppContext) {
        let (root, cx) = open_timeline_row("```latex\n1+2=3\n```", cx);
        assert!(cx.debug_bounds("markdown-math-root-0").is_some());
        assert!(cx.debug_bounds("markdown-code-line-0").is_none());
        let first = cx.debug_bounds("math-glyph-0").unwrap();
        let end = cx.debug_bounds("math-glyph-3").unwrap();
        cx.simulate_mouse_down(
            point(first.left(), first.center().y),
            MouseButton::Left,
            Modifiers::default(),
        );
        drag_to(cx, point(end.left(), end.center().y));
        assert_eq!(release(cx, point(end.left(), end.center().y)), "1+2");
        let source = cx.debug_bounds("math-mode-true-root-0").unwrap();
        cx.simulate_click(source.center(), Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));
        assert!(cx.debug_bounds("markdown-code-line-0").is_some());
        let rendered = cx.debug_bounds("math-mode-false-root-0").unwrap();
        cx.simulate_click(rendered.center(), Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));
        assert!(cx.debug_bounds("markdown-code-line-0").is_none());
        root.update(cx, |root, cx| {
            root.markdown
                .update(cx, |md, cx| md.set_text("```latex\n\\frac{1}{", cx))
        });
        cx.update(|window, cx| _ = window.draw(cx));
        assert!(cx.debug_bounds("markdown-code-line-0").is_some());
        root.update(cx, |root, cx| {
            root.markdown
                .update(cx, |md, cx| md.push_str("2}\n```", cx))
        });
        cx.update(|window, cx| _ = window.draw(cx));
        assert!(cx.debug_bounds("markdown-code-line-0").is_none());
        root.update(cx, |root, cx| {
            root.markdown
                .update(cx, |md, cx| md.set_text("before $1+2=3$ after", cx))
        });
        cx.update(|window, cx| _ = window.draw(cx));
        let first = cx.debug_bounds("math-glyph-0").unwrap();
        let end = point(px(380.), first.center().y);
        cx.simulate_mouse_down(
            point(first.left(), first.center().y),
            MouseButton::Left,
            Modifiers::default(),
        );
        drag_to(cx, end);
        assert_eq!(release(cx, end), "1+2=3 after");
    }

    #[gpui::test]
    fn mermaid_fence_paints_a_diagram_and_unparsable_source_stays_code(cx: &mut TestAppContext) {
        let (_, cx) = open_timeline_row(
            "```mermaid\nflowchart LR\n  A --> B\n```\n\n```mermaid\nnot a diagram\n```",
            cx,
        );
        assert!(cx.debug_bounds("markdown-mermaid-root-0").is_some());
        assert!(cx.debug_bounds("markdown-mermaid-root-1").is_none());
        let fallback = cx.debug_bounds("markdown-code-line-0");
        assert!(
            fallback.is_some(),
            "the fence that does not parse keeps its code"
        );
    }

    #[gpui::test]
    fn a_diagram_wider_than_the_row_shrinks_without_letterboxing(cx: &mut TestAppContext) {
        let nodes = (0..12)
            .map(|ix| format!("N{ix}[node {ix}]"))
            .collect::<Vec<_>>();
        let source = format!("```mermaid\nflowchart LR\n  {}\n```", nodes.join(" --> "));
        let (_, cx) = open_timeline_row(&source, cx);
        let padding = px(12.) * 2.;
        let mut aspects = Vec::new();
        for width in [px(393.), px(600.)] {
            cx.simulate_resize(gpui::size(width, px(852.)));
            cx.update(|window, cx| _ = window.draw(cx));
            let block = cx.debug_bounds("markdown-mermaid-root-0").unwrap();
            aspects.push((block.size.height - padding) / (block.size.width - padding));
        }
        assert!(
            (aspects[0] - aspects[1]).abs() < 0.01,
            "the diagram box keeps the picture's aspect ratio at every row width: {aspects:?}"
        );
    }

    #[gpui::test]
    fn code_block_copy_button_copies_the_whole_fence(cx: &mut TestAppContext) {
        let lines = (0..100).map(|ix| format!("line {ix}")).collect::<Vec<_>>();
        let source = format!("```text\n{}\n```", lines.join("\n"));
        let (_, cx) = open_timeline_row(&source, cx);
        let button = cx.debug_bounds("markdown-code-copy-root-0").unwrap();
        cx.simulate_click(button.center(), Modifiers::default());
        cx.run_until_parked();
        let copied = cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert_eq!(copied.as_deref(), Some(lines.join("\n").as_str()));
    }

    fn open_timeline_row<'a>(
        source: &str,
        cx: &'a mut TestAppContext,
    ) -> (Entity<OuterListRoot>, &'a mut VisualTestContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let source = source.to_string();
        let (view, cx) = cx.add_window_view(|_, cx| OuterListRoot::new(&source, cx));
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        for _ in 0..2 {
            cx.run_until_parked();
            cx.update(|window, cx| _ = window.draw(cx));
        }
        (view, cx)
    }

    fn drag_to(cx: &mut VisualTestContext, to: gpui::Point<gpui::Pixels>) {
        cx.simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));
    }

    fn release(cx: &mut VisualTestContext, at: gpui::Point<gpui::Pixels>) -> String {
        cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));
        cx.update(gpui_base::TextSelection::selected_text)
    }

    fn scroll_to_end(view: &Entity<OuterListRoot>, cx: &mut VisualTestContext) {
        view.update(cx, |root, _| root.list_state.scroll_to_end());
        for _ in 0..2 {
            cx.run_until_parked();
            cx.update(|window, cx| _ = window.draw(cx));
        }
    }

    #[gpui::test]
    fn shift_click_extends_from_an_anchor_scrolled_away(cx: &mut TestAppContext) {
        let lines = (0..3_000)
            .map(|ix| format!("line {ix}"))
            .collect::<Vec<_>>();
        let items = (0..1_000)
            .map(|ix| format!("item {ix}"))
            .collect::<Vec<_>>();
        for (source, expected, first, last) in [
            (
                format!("```text\n{}\n```", lines.join("\n")),
                lines[5..].join("\n"),
                "markdown-code-line-5",
                "markdown-code-line-2999",
            ),
            (
                items
                    .iter()
                    .map(|item| format!("- {item}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                items[5..].join("\n"),
                "markdown-list-item-text-root-0-5",
                "markdown-list-item-root-0-999",
            ),
        ] {
            let (view, cx) = open_timeline_row(&source, cx);
            let first = cx.debug_bounds(first).unwrap();
            cx.simulate_click(
                point(first.left() + px(1.), first.center().y),
                Modifiers::default(),
            );
            cx.update(|window, cx| _ = window.draw(cx));
            scroll_to_end(&view, cx);

            let end = point(px(380.), cx.debug_bounds(last).unwrap().center().y);
            let shift = Modifiers {
                shift: true,
                ..Default::default()
            };
            cx.simulate_mouse_down(end, MouseButton::Left, shift);
            cx.update(|window, cx| _ = window.draw(cx));
            cx.simulate_mouse_up(end, MouseButton::Left, shift);
            cx.update(|window, cx| _ = window.draw(cx));
            let selected = cx.update(gpui_base::TextSelection::selected_text);
            assert_eq!(selected, expected);
        }
    }

    #[gpui::test]
    fn a_scroll_jump_mid_drag_drops_text_left_behind(cx: &mut TestAppContext) {
        let lines = (0..3_000)
            .map(|ix| format!("line {ix}"))
            .collect::<Vec<_>>();
        let items = (0..1_000)
            .map(|ix| format!("item {ix}"))
            .collect::<Vec<_>>();
        // Each drag starts in the second item of the root list and turns
        // back within it before the jump.
        for (source, expected, anchor, back, last) in [
            (
                format!("```text\n{}\n```", lines.join("\n")),
                lines[40..].join("\n"),
                "markdown-code-line-40",
                "markdown-code-line-33",
                "markdown-code-line-2999",
            ),
            (
                items
                    .iter()
                    .map(|item| format!("- {item}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                items[12..].join("\n"),
                "markdown-list-item-text-root-0-12",
                "markdown-list-item-text-root-0-9",
                "markdown-list-item-root-0-999",
            ),
        ] {
            let (view, cx) = open_timeline_row(&source, cx);
            let start = |cx: &mut VisualTestContext, selector| {
                let bounds = cx.debug_bounds(selector).unwrap();
                point(bounds.left() + px(1.), bounds.center().y)
            };
            let (anchor, back) = (start(cx, anchor), start(cx, back));
            cx.simulate_mouse_down(anchor, MouseButton::Left, Modifiers::default());
            cx.update(|window, cx| _ = window.draw(cx));
            drag_to(cx, back);
            scroll_to_end(&view, cx);

            let end = point(px(380.), cx.debug_bounds(last).unwrap().center().y);
            drag_to(cx, end);
            assert_eq!(release(cx, end), expected);
        }
    }

    #[gpui::test]
    fn streamed_append_preserves_earlier_item_measurements(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let code = (0..100)
            .map(|ix| format!("line {ix}"))
            .collect::<Vec<_>>()
            .join("\n");
        // A long streaming code block keeps the items of its settled lines.
        // A block that stops being the last gains a gap below its last item.
        for (source, append, settled) in [
            ("stable block\n\nstreaming block".to_string(), " delta", 0),
            (format!("```text\n{code}"), "\nline 100", 2),
            (
                "stable block\n\nsettled block".to_string(),
                "\n\nnew block",
                0,
            ),
            (format!("```text\n{code}\n```"), "\n\nnew block", 2),
        ] {
            let (view, cx) = cx.add_window_view(|_, cx| TestRoot::new(&source, cx));
            let cx: &mut VisualTestContext = cx;
            cx.run_until_parked();
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });

            assert!(view.read_with(cx, |root, cx| {
                root.markdown.read(cx).has_measured_item(settled + 1)
            }));
            view.update(cx, |root, cx| {
                root.markdown.update(cx, |markdown, cx| {
                    markdown.push_str(append, cx);
                    assert!(markdown.has_measured_item(settled));
                    assert!(!markdown.has_measured_item(settled + 1));
                });
            });
        }
    }

    #[gpui::test]
    fn wide_table_shrinks_to_viewport_and_wraps(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let header = (0..20).map(|ix| format!("column-{ix}")).collect::<Vec<_>>();
        let separator = vec!["---"; header.len()];
        let values = (0..header.len())
            .map(|ix| format!("value-{ix}"))
            .collect::<Vec<_>>();
        let overflowing = format!(
            "| {} |\n| {} |\n| {} |",
            header.join(" | "),
            separator.join(" | "),
            values.join(" | ")
        );
        for (case, markdown, wraps, overflows) in [
            ("wide cells", "| column | value |\n| --- | --- |\n| this-cell-is-deliberately-much-wider-than-the-markdown-viewport | another-wide-value |".to_string(), true, false),
            ("short cells", "| a | b |\n| --- | --- |\n| 1 | 2 |".to_string(), false, false),
            ("many columns", overflowing, false, true),
        ] {
            let (_, cx) = cx.add_window_view(|_, cx| TestRoot::new(&markdown, cx));
            let cx: &mut VisualTestContext = cx;
            cx.run_until_parked();
            cx.update(|window, cx| { let _ = window.draw(cx); });
            let track = cx.debug_bounds("markdown-table-track-root-0").expect("table track was painted");
            if overflows {
                let last_cell = cx.debug_bounds("markdown-table-cell-0-19").expect("last cell was painted");
                assert_eq!(last_cell.right() + px(1.), track.right(), "{case}: {last_cell:?}, {track:?}");
            } else {
                assert_eq!(track.size.width, px(320.), "{case}: table must fit the viewport");
            }
            if wraps {
                assert!(track.size.height > px(68.), "{case}: wrapped height {:?}", track.size.height);
            }
        }
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
    fn click_that_stops_propagation_does_not_leave_selection_following_pointer(
        cx: &mut TestAppContext,
    ) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (_, cx) = cx.add_window_view(|_, cx| SwallowedClickRoot {
            markdown: cx.new(|cx| MarkdownState::new("Hello world", cx)),
        });
        let cx: &mut VisualTestContext = cx;
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        cx.simulate_mouse_down(
            point(px(10.), px(20.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.simulate_mouse_up(
            point(px(10.), px(20.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.simulate_mouse_move(point(px(80.), px(70.)), None, Modifiers::default());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
            assert!(
                gpui_base::TextSelection::selected_text(window, cx).is_empty(),
                "the released pointer kept selecting text"
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
    fn drag_across_mixed_blocks_copies_every_block_kind(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        for (source, expected) in [
            ("Alpha beta\n\nGamma delta", "Alpha beta\nGamma delta"),
            (
                "# Title\n\nIntro para\n\n- item one\n- item two\n\n```\ncode line\n```\n\nTail para",
                "Title\nIntro para\nitem one\nitem two\ncode line\nTail para",
            ),
        ] {
            let (view, cx) = cx.add_window_view(|_, cx| MultiBlockRoot::with_text(source, cx));
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
            assert_eq!(selected, expected, "source: {source:?}");
        }
    }

    #[gpui::test]
    fn drag_across_split_code_and_list_copies_the_lines_between(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let code = (0..40)
            .map(|ix| format!("line {ix}"))
            .collect::<Vec<_>>()
            .join("\n");
        let list = (0..12)
            .map(|ix| format!("- item {ix}"))
            .collect::<Vec<_>>()
            .join("\n");
        let source = format!("```text\n{code}\n```\n\n{list}");
        let (_, cx) = cx.add_window_view(|_, cx| MultiBlockRoot::with_text(&source, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(gpui::size(px(400.), px(1_400.)));
        for _ in 0..2 {
            cx.run_until_parked();
            cx.update(|window, cx| _ = window.draw(cx));
        }

        let start = cx.debug_bounds("markdown-code-line-30").unwrap();
        let end = cx.debug_bounds("markdown-list-item-root-1-10").unwrap();
        let (start, end) = (
            point(start.left() + px(1.), start.center().y),
            point(px(390.), end.center().y),
        );
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));
        cx.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));
        cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));

        let expected = (30..40)
            .map(|ix| format!("line {ix}"))
            .chain((0..=10).map(|ix| format!("item {ix}")))
            .collect::<Vec<_>>()
            .join("\n");
        let selected = cx.update(gpui_base::TextSelection::selected_text);
        assert_eq!(selected, expected);
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
            let link = root
                .markdown
                .read(cx)
                .list_state
                .bounds_for_item(1)
                .unwrap();
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

    /// The view under the window `Root`, which owns Copy for the window
    /// selection, as the shell mounts it.
    fn right_click_window(
        cx: &mut TestAppContext,
    ) -> (Entity<RightClickRoot>, &mut VisualTestContext) {
        let view = Rc::new(std::cell::OnceCell::new());
        let (_, cx) = cx.add_window_view({
            let view = view.clone();
            move |window, cx| {
                let root = cx.new(RightClickRoot::new);
                view.set(root.clone()).ok().unwrap();
                gpui_base::Root::new(root, window, cx)
            }
        });
        (view.get().unwrap().clone(), cx)
    }

    #[gpui::test]
    fn link_context_menu_action_opens_the_url(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        cx.update(crate::widgets::menu::init);
        let (view, cx) = right_click_window(cx);
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let link_pos = view.read_with(cx, |root, cx| {
            let link = root
                .markdown
                .read(cx)
                .list_state
                .bounds_for_item(1)
                .unwrap();
            point(px(5.), link.center().y)
        });
        cx.simulate_mouse_down(link_pos, MouseButton::Right, Modifiers::default());
        cx.simulate_mouse_up(link_pos, MouseButton::Right, Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        // Copy is disabled with nothing selected, so the first Down lands on
        // Select All and the second on "Open link"; the action must dispatch
        // through the trigger's ancestor chain to the OpenUrl handler.
        cx.simulate_keystrokes("down down enter");
        cx.run_until_parked();
        assert_eq!(cx.opened_url().as_deref(), Some("https://example.com"));
    }

    #[gpui::test]
    fn right_click_on_text_opens_the_selection_menu_and_is_swallowed(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        cx.update(crate::widgets::menu::init);
        let (view, cx) = right_click_window(cx);
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

        // Plain (and selectable) text: the view's own menu (Copy, Select
        // All) opens and takes focus, and the press is consumed before the
        // root handler sees it.
        cx.simulate_mouse_down(text_pos, MouseButton::Right, Modifiers::default());
        cx.simulate_mouse_up(text_pos, MouseButton::Right, Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let bubbled = view.read_with(cx, |root, _| *root.bubbled_right_clicks.borrow());
        assert_eq!(bubbled, 0, "right-click on plain text must not propagate");
        let markdown_focus =
            view.read_with(cx, |root, cx| root.markdown.read(cx).focus_handle.clone());
        let focused = cx.update(|window, cx| window.focused(cx));
        assert!(
            focused.is_some_and(|focused| focused != markdown_focus),
            "a plain-text right-click must open and focus the context menu"
        );

        // Select All from that menu selects the whole view, and Copy, which
        // was disabled with nothing selected, then copies it.
        cx.simulate_keystrokes("down enter");
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert_eq!(
            cx.update(gpui_base::TextSelection::selected_text).trim(),
            "Alpha beta\nclick"
        );
        cx.simulate_mouse_down(text_pos, MouseButton::Right, Modifiers::default());
        cx.simulate_mouse_up(text_pos, MouseButton::Right, Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.simulate_keystrokes("down enter");
        cx.run_until_parked();
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("Alpha beta\nclick")
        );

        // A link's menu adds the link items after the view's own; the press
        // is consumed the same way and the menu takes focus.
        cx.simulate_mouse_down(link_pos, MouseButton::Right, Modifiers::default());
        cx.simulate_mouse_up(link_pos, MouseButton::Right, Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let bubbled = view.read_with(cx, |root, _| *root.bubbled_right_clicks.borrow());
        assert_eq!(bubbled, 0, "right-click on a link must not propagate");
        let focused = cx.update(|window, cx| window.focused(cx));
        assert!(
            focused.is_some_and(|focused| focused != markdown_focus),
            "a link right-click must open and focus the context menu"
        );
    }

    struct TouchRoot {
        markdown: Entity<MarkdownState>,
    }

    impl Render for TouchRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(300.))
                .child(TextSelectionLayer)
                .child(MarkdownView::new(&self.markdown).selectable(true))
        }
    }

    fn open_touch_root(cx: &mut TestAppContext) -> (Entity<TouchRoot>, &mut VisualTestContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (view, cx) = cx.add_window_view(|_, cx| TouchRoot {
            markdown: cx.new(|cx| MarkdownState::new("quick select value", cx)),
        });
        cx.run_until_parked();
        draw(cx);
        (view, cx)
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
    }

    fn long_press(cx: &mut VisualTestContext, phases: &[gpui::TouchPhase], at: (f32, f32)) {
        let start_position = point(px(at.0), px(at.1));
        for phase in phases {
            cx.simulate_event(gpui::LongPressEvent {
                phase: *phase,
                start_position,
                position: start_position,
            });
            draw(cx);
        }
    }

    fn selected(view: &Entity<TouchRoot>, cx: &mut VisualTestContext) -> String {
        view.read_with(cx, |root, cx| {
            let markdown = root.markdown.read(cx);
            markdown.selected_text_in(
                Some(0..=markdown.item_count().saturating_sub(1)),
                None,
                None,
            )
        })
        .trim()
        .to_string()
    }

    fn touch_selection(cx: &mut VisualTestContext) -> Option<gpui_base::TouchSelectionSnapshot> {
        cx.update(|window, cx| gpui_base::TextSelection::touch_selection(window, cx))
    }

    #[gpui::test]
    fn long_press_release_keeps_handles_which_drag_the_selection(cx: &mut TestAppContext) {
        use gpui::{TouchDragEvent, TouchPhase};
        use gpui_base::{SelectionEdge, TextSelection, TouchHandle};

        for continue_finger in [true, false] {
            let (view, cx) = open_touch_root(cx);
            if continue_finger {
                let start_position = point(px(10.), px(10.));
                cx.simulate_event(gpui::LongPressEvent {
                    phase: TouchPhase::Started,
                    start_position,
                    position: start_position,
                });
                draw(cx);
                assert_eq!(selected(&view, cx), "quick");
                for phase in [TouchPhase::Moved, TouchPhase::Ended] {
                    cx.simulate_event(gpui::LongPressEvent {
                        phase,
                        start_position,
                        position: point(px(220.), px(10.)),
                    });
                }
                draw(cx);
                assert_eq!(selected(&view, cx), "quick select value");
                continue;
            }
            long_press(cx, &[TouchPhase::Started, TouchPhase::Ended], (70., 10.));
            assert_eq!(selected(&view, cx), "select");
            let snapshot = touch_selection(cx).expect("a released long press keeps its handles");
            assert!(snapshot.is_menu_open());
            assert!(!snapshot.is_empty());
            assert!(snapshot.start().left() < snapshot.end().left());

            // The view painted the handles in place: a touch on the end knob,
            // which hangs below the line, takes it and drags the end along the
            // line, through the window layer, to the end of the text.
            let end = snapshot.end();
            let finger = TouchHandle::hit_bounds(SelectionEdge::End, end).center();
            cx.simulate_event(TouchDragEvent {
                phase: TouchPhase::Started,
                start_position: finger,
                position: finger,
            });
            draw(cx);
            let snapshot = touch_selection(cx).unwrap();
            assert_eq!(snapshot.dragging(), Some(SelectionEdge::End));
            assert!(!snapshot.is_menu_open());
            cx.simulate_event(TouchDragEvent {
                phase: TouchPhase::Moved,
                start_position: finger,
                position: point(px(290.), finger.y),
            });
            draw(cx);
            assert_eq!(selected(&view, cx), "select value");
            cx.simulate_event(TouchDragEvent {
                phase: TouchPhase::Ended,
                start_position: finger,
                position: point(px(290.), finger.y),
            });
            draw(cx);
            let snapshot = touch_selection(cx).unwrap();
            assert!(snapshot.is_menu_open());
            assert_eq!(snapshot.dragging(), None);
            let select_start = snapshot.start().left();

            // Select All from the menu is a view-local selection; its handles
            // still drag, turning it back into a point selection.
            cx.update(|window, cx| {
                window.dispatch_action(Box::new(crate::touch_selection::SelectAllTouched), cx);
            });
            cx.run_until_parked();
            draw(cx);
            assert_eq!(selected(&view, cx), "quick select value");
            assert_eq!(
                cx.update(TextSelection::selected_text).trim(),
                "quick select value"
            );
            let snapshot = touch_selection(cx).expect("select all keeps the touch selection");
            assert!(snapshot.is_menu_open());
            let start = snapshot.start();
            assert!(start.left() < select_start);
            cx.update(|window, cx| {
                TextSelection::begin_edge_drag(SelectionEdge::Start, start.origin, window, cx);
                TextSelection::update_edge_drag(point(select_start, start.origin.y), window, cx);
                TextSelection::end_edge_drag(window, cx);
            });
            draw(cx);
            assert_eq!(selected(&view, cx), "select value");

            // A press on the menu leaves the selection alone; one on the text
            // clears it.
            let menu = gpui::Bounds::new(point(px(0.), px(200.)), gpui::size(px(120.), px(32.)));
            cx.update(|window, cx| TextSelection::register_touch_ui(menu, window, cx));
            cx.simulate_mouse_down(
                point(px(10.), px(210.)),
                MouseButton::Left,
                Modifiers::default(),
            );
            assert_eq!(selected(&view, cx), "select value");
            cx.simulate_click(point(px(10.), px(10.)), Modifiers::default());
            draw(cx);
            assert!(touch_selection(cx).is_none());
        }
    }
}
