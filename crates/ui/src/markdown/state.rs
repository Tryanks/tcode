//! Synchronous Markdown state, structurally adapted from gpui-component's
//! Apache-2.0 `text/state.rs` implementation.

use gpui::{
    App, Bounds, Context, EntityId, FocusHandle, IntoElement, ListAlignment, ListState,
    ParentElement as _, Pixels, Point, Render, Styled as _, Window, px,
};
use gpui_component::{ElementExt as _, v_flex};

use super::{nodes::BlockNode, render, style::TextViewStyle, window_selection};

/// State backing a [`super::MarkdownView`].
pub struct MarkdownState {
    pub(super) focus_handle: FocusHandle,
    pub(super) entity_id: EntityId,
    pub(super) bounds: Bounds<Pixels>,
    pub(super) selectable: bool,
    pub(super) style: TextViewStyle,
    pub(super) is_selecting: bool,
    multi_click_selection: Option<MarkdownMultiClickSelection>,
    selected_text_override: Option<String>,
    select_all: bool,
    text: String,
    parsed: BlockNode,
    pub(super) list_state: ListState,
    measured_content_height: Option<Pixels>,
}

impl MarkdownState {
    /// Parse `text` immediately and create a Markdown state entity value.
    pub fn new(text: &str, cx: &mut Context<Self>) -> Self {
        let parsed = super::parse(text);
        let block_count = root_block_count(&parsed);
        Self {
            focus_handle: cx.focus_handle(),
            entity_id: cx.entity_id(),
            bounds: Bounds::default(),
            selectable: false,
            style: TextViewStyle::default(),
            is_selecting: false,
            multi_click_selection: None,
            selected_text_override: None,
            select_all: false,
            text: text.to_string(),
            parsed,
            // Measure every block once so the list has a stable total height,
            // then construct/layout/paint only the visible blocks on warm frames.
            list_state: ListState::new(block_count, ListAlignment::Top, px(1000.)).measure_all(),
            measured_content_height: None,
        }
    }

    /// Append text, synchronously reparse the one canonical source, and repaint.
    pub fn push_str(&mut self, text: &str, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        self.text.push_str(text);
        self.reparse(cx);
    }

    /// Replace the canonical source and synchronously reparse it.
    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.text == text {
            return;
        }
        self.text.clear();
        self.text.push_str(text);
        self.reparse(cx);
    }

    /// Return the currently selected rendered text.
    ///
    /// Block selections (select-all, drags) end with a single trailing
    /// newline, matching gpui-component's block-text convention; the
    /// multi-click override carries the exact word/line instead.
    pub fn selected_text(&self) -> String {
        if self.select_all {
            return with_trailing_newline(self.parsed.text());
        }
        if let Some(text) = &self.selected_text_override {
            return text.clone();
        }
        with_trailing_newline(self.parsed.selected_text())
    }

    /// Select all rendered text in this view.
    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        self.parsed.clear_selection();
        self.multi_click_selection = None;
        self.selected_text_override = None;
        self.select_all = true;
        self.is_selecting = false;
        cx.notify();
    }

    /// Clear all local and drag selection state for this view.
    pub fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.reset_selection();
        cx.notify();
    }

    /// Enable or disable selection for this state.
    pub fn set_selectable(&mut self, selectable: bool, cx: &mut Context<Self>) {
        if self.selectable == selectable {
            return;
        }
        self.selectable = selectable;
        if !selectable {
            self.reset_selection();
            window_selection::clear_selection_for_view(self.entity_id, cx);
        }
        cx.notify();
    }

    fn reparse(&mut self, cx: &mut Context<Self>) {
        // Don't interrupt an active drag-selection; the window-level endpoints
        // stay valid for append-only growth and per-inline ranges repaint.
        if !self.is_selecting {
            self.reset_selection();
            window_selection::clear_selection_for_view(self.entity_id, cx);
        }
        self.parsed = super::parse(&self.text);
        let block_count = root_block_count(&self.parsed);
        // Even an edit that preserves the number of root blocks can change
        // their heights, so every reparse must invalidate the cached sizes.
        self.list_state.reset(block_count);
        self.measured_content_height = None;
        cx.notify();
    }

    fn reset_selection(&mut self) {
        self.multi_click_selection = None;
        self.selected_text_override = None;
        self.select_all = false;
        self.is_selecting = false;
        self.parsed.clear_selection();
    }

    pub(super) fn is_selectable(&self) -> bool {
        self.selectable
    }

    pub(super) fn is_all_selected(&self) -> bool {
        self.select_all
    }

    pub(super) fn has_view_selection(&self) -> bool {
        self.select_all
            || self.multi_click_selection.is_some()
            || self.selected_text_override.is_some()
    }

    pub(super) fn has_selection(&self, window: &Window, cx: &App) -> bool {
        self.has_view_selection() || self.selection_points(window, cx).is_some()
    }

    pub(super) fn selection_points(
        &self,
        window: &Window,
        cx: &App,
    ) -> Option<(Point<Pixels>, Point<Pixels>)> {
        if !self.selectable {
            return None;
        }
        window_selection::selection_points(window, self.entity_id, cx)
    }

    pub(super) fn set_multi_click_selection(
        &mut self,
        position: Point<Pixels>,
        kind: MarkdownMultiClickKind,
        selected_text: String,
    ) {
        self.multi_click_selection = Some(MarkdownMultiClickSelection {
            pos: position - self.bounds.origin,
            kind,
        });
        self.selected_text_override = Some(selected_text);
        self.select_all = false;
        self.is_selecting = false;
    }

    pub(super) fn multi_click_selection(&self) -> Option<MarkdownMultiClickSelection> {
        self.multi_click_selection
            .map(|selection| MarkdownMultiClickSelection {
                pos: selection.pos + self.bounds.origin,
                ..selection
            })
    }

    fn update_layout(
        &mut self,
        bounds: Bounds<Pixels>,
        measured_content_height: Option<Pixels>,
        cx: &mut Context<Self>,
    ) -> bool {
        let width_changed = self.bounds.size.width != bounds.size.width;
        let resized = self.bounds.size != bounds.size;
        let had_measured_height = self.measured_content_height.is_some();
        self.bounds = bounds;

        if width_changed {
            self.measured_content_height = None;
            if had_measured_height {
                // The custom warm-frame list only measures its visible slice.
                // Throw away every old-width item size, then run one complete,
                // width-correct measuring frame before caching the new height.
                self.list_state.reset(self.list_state.item_count());
                cx.notify();
                return resized;
            }
        }
        if let Some(height) = measured_content_height
            && self.measured_content_height != Some(height)
        {
            self.measured_content_height = Some(height);
            cx.notify();
        } else if width_changed {
            // Re-enter the width-correct measuring pass on the next frame.
            cx.notify();
        }
        resized
    }

    fn list_content_height(&self) -> Option<Pixels> {
        let count = self.list_state.item_count();
        if count == 0 {
            return Some(px(0.));
        }
        let viewport = self.list_state.viewport_bounds();
        self.list_state.bounds_for_item(count - 1).map(|last| {
            last.bottom() - viewport.top() - self.list_state.scroll_px_offset_for_scrollbar().y
        })
    }

    #[cfg(test)]
    pub(super) fn source(&self) -> &str {
        &self.text
    }
}

fn with_trailing_newline(mut text: String) -> String {
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

fn root_block_count(node: &BlockNode) -> usize {
    match node {
        BlockNode::Root { children, .. } => children.len(),
        _ => 1,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct MarkdownMultiClickSelection {
    pub(super) pos: Point<Pixels>,
    pub(super) kind: MarkdownMultiClickKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MarkdownMultiClickKind {
    Word,
    Paragraph,
}

impl Render for MarkdownState {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = cx.entity();
        let parsed = self.parsed.clone();
        let style = self.style.clone();
        let measured_content_height = self.measured_content_height;
        v_flex()
            .w_full()
            .text_size(style.base_font_size)
            .child(render::render_root(
                &parsed,
                self.list_state.clone(),
                render::RootMeasurements {
                    width: self.bounds.size.width,
                    content_height: measured_content_height,
                },
                &state,
                &style,
                window,
                cx,
            ))
            .on_prepaint(move |bounds, _window, cx| {
                let entity_id = state.entity_id();
                let resized = state.update(cx, |state, cx| {
                    let measured_height = state.list_content_height();
                    state.update_layout(bounds, measured_height, cx)
                });
                if resized {
                    window_selection::clear_selection_for_resized_view(entity_id, cx);
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use gpui::{AppContext as _, TestAppContext};

    use super::*;

    #[gpui::test]
    fn source_and_parsed_text_stay_coherent(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        cx.update(super::super::init);
        let state = cx.update(|cx| cx.new(|cx| MarkdownState::new("old", cx)));

        state.update(cx, |state, cx| state.select_all(cx));
        assert_eq!(
            state.read_with(cx, |state, _| state.selected_text()),
            "old\n"
        );

        state.update(cx, |state, cx| {
            state.set_text("new", cx);
            state.push_str(" **value**", cx);
            state.select_all(cx);
        });
        state.read_with(cx, |state, _| {
            assert_eq!(state.source(), "new **value**");
            assert_eq!(state.selected_text(), "new value\n");
        });
    }

    /// A streamed update must not wipe a drag-selection in progress
    /// (streaming chat pushes deltas while the user may be selecting).
    #[gpui::test]
    fn reparse_preserves_selection_during_active_drag(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        cx.update(super::super::init);
        let state = cx.update(|cx| cx.new(|cx| MarkdownState::new("start", cx)));
        state.update(cx, |state, cx| {
            state.set_multi_click_selection(
                Point::default(),
                MarkdownMultiClickKind::Word,
                "start".to_string(),
            );
            state.is_selecting = true;
            state.push_str(" more", cx);
            assert_eq!(state.selected_text(), "start", "selection survives drag");
            state.is_selecting = false;
            state.push_str(" again", cx);
            assert_eq!(state.selected_text(), "", "settled selection clears");
        });
    }

    #[gpui::test]
    fn select_all_reads_blocks_that_were_never_painted(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        cx.update(super::super::init);
        let source = (0..2_000)
            .map(|ix| format!("block {ix}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let expected = (0..2_000)
            .map(|ix| format!("block {ix}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let state = cx.update(|cx| cx.new(|cx| MarkdownState::new(&source, cx)));

        // No window or MarkdownView is created, so none of the list's blocks
        // can have been painted before selected_text traverses the parsed tree.
        state.update(cx, |state, cx| state.select_all(cx));
        assert_eq!(
            state.read_with(cx, |state, _| state.selected_text()),
            expected
        );
    }
}
