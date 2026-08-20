//! Synchronous Markdown state, structurally adapted from gpui-component's
//! Apache-2.0 `text/state.rs` implementation.

use std::{
    ops::RangeInclusive,
    path::{Path, PathBuf},
};

use gpui::{
    Bounds, Context, FocusHandle, IntoElement, ListAlignment, ListState, ParentElement as _,
    Pixels, Render, SharedString, Styled as _, Window, px,
};
use gpui_base::{ElementExt as _, v_flex};

use super::{
    link_target::LinkTarget, nodes::BlockNode, render, selection_adapter::MarkdownSelectionAdapter,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PendingLinkMenu {
    pub(super) target: LinkTarget,
    pub(super) text: SharedString,
    pub(super) raw_url: SharedString,
}

/// State backing a [`super::MarkdownView`].
pub struct MarkdownState {
    pub(super) focus_handle: FocusHandle,
    pub(super) bounds: Bounds<Pixels>,
    pub(super) selectable: bool,
    pub(super) compact_headings: bool,
    pub(super) base_dir: Option<PathBuf>,
    pub(super) pending_context_link: Option<PendingLinkMenu>,
    pub(super) is_selecting: bool,
    text: String,
    parsed: BlockNode,
    pub(super) list_state: ListState,
    measured_content_height: Option<Pixels>,
    selection_revision: usize,
    pub(super) selection_adapter: MarkdownSelectionAdapter,
}

impl MarkdownState {
    /// Parse `text` immediately and create a Markdown state entity value.
    pub fn new(text: &str, cx: &mut Context<Self>) -> Self {
        let parsed = super::parse(text);
        let block_count = root_block_count(&parsed);
        let selection_adapter = MarkdownSelectionAdapter::new(cx.entity().downgrade(), cx);
        Self {
            focus_handle: cx.focus_handle(),
            bounds: Bounds::default(),
            selectable: false,
            compact_headings: false,
            base_dir: None,
            pending_context_link: None,
            is_selecting: false,
            text: text.to_string(),
            parsed,
            // Measure every block once so the list has a stable total height,
            // then construct/layout/paint only the visible blocks on warm frames.
            list_state: ListState::new(block_count, ListAlignment::Top, px(1000.)).measure_all(),
            measured_content_height: None,
            selection_revision: 0,
            selection_adapter,
        }
    }

    /// Append text, synchronously reparse the one canonical source, and repaint.
    pub fn push_str(&mut self, text: &str, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        self.text.push_str(text);
        self.reparse_append(cx);
    }

    /// Replace the canonical source and synchronously reparse it.
    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.text == text {
            return;
        }
        self.text.clear();
        self.text.push_str(text);
        self.reparse_reset(cx);
    }

    /// Return all rendered text, including blocks that were never painted.
    pub fn rendered_text(&self) -> String {
        with_trailing_newline(self.parsed.text())
    }

    /// The native window-selection participant owned by this renderer.
    pub fn selection_handle(&self) -> &gpui_base::TextSelectionHandle {
        &self.selection_adapter.selection
    }

    /// Enable or disable selection for this state.
    pub fn set_selectable(&mut self, selectable: bool, cx: &mut Context<Self>) {
        if self.selectable == selectable {
            return;
        }
        self.selectable = selectable;
        if !selectable {
            self.reset_selection_projection();
            self.selection_adapter
                .selection
                .set_local_selection(false, cx);
        }
        cx.notify();
    }

    /// Scale headings for a chat message instead of a document. Document scale
    /// (h1 at 2× body) dwarfs a timeline; chat scale keeps them in the flow.
    pub fn set_compact_headings(&mut self, compact: bool, cx: &mut Context<Self>) {
        if self.compact_headings == compact {
            return;
        }
        self.compact_headings = compact;
        cx.notify();
    }

    /// Set the directory used to resolve relative Markdown links.
    pub fn set_base_dir(&mut self, base_dir: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.base_dir == base_dir {
            return;
        }
        self.base_dir = base_dir;
        cx.notify();
    }

    pub(super) fn base_dir(&self) -> Option<&Path> {
        self.base_dir.as_deref()
    }

    pub(super) fn set_pending_context_link(
        &mut self,
        pending_context_link: Option<PendingLinkMenu>,
        cx: &mut Context<Self>,
    ) {
        if self.pending_context_link == pending_context_link {
            return;
        }
        self.pending_context_link = pending_context_link;
        cx.notify();
    }

    fn prepare_reparse(&mut self, cx: &mut Context<Self>) {
        // Don't interrupt an active drag-selection; the window-level endpoints
        // stay valid for append-only growth and per-inline ranges repaint.
        if !self.is_selecting {
            self.reset_selection_projection();
            self.selection_adapter
                .selection
                .set_local_selection(false, cx);
        }
    }

    fn reparse_append(&mut self, cx: &mut Context<Self>) {
        self.prepare_reparse(cx);
        let parsed = super::parse(&self.text);
        let old_blocks = root_blocks(&self.parsed);
        let new_blocks = root_blocks(&parsed);
        let unchanged = old_blocks
            .iter()
            .zip(new_blocks)
            .take_while(|(old, new)| old == new)
            .count();
        let old_count = old_blocks.len();
        let new_count = new_blocks.len();
        self.parsed = parsed;
        self.selection_revision = self.selection_revision.wrapping_add(1);

        if unchanged < old_count || unchanged < new_count {
            // An append can only affect the old trailing block and blocks added
            // after it. Preserve the measured prefix and invalidate that suffix.
            self.list_state
                .splice(unchanged..old_count, new_count - unchanged);
            // `splice` creates unmeasured items but does not re-arm measure_all.
            self.list_state.remeasure_items(unchanged..new_count);
            self.measured_content_height = None;
        }
        cx.notify();
    }

    fn reparse_reset(&mut self, cx: &mut Context<Self>) {
        self.prepare_reparse(cx);
        self.parsed = super::parse(&self.text);
        self.selection_revision = self.selection_revision.wrapping_add(1);
        let block_count = root_block_count(&self.parsed);
        // Even an edit that preserves the number of root blocks can change
        // their heights, so every reparse must invalidate the cached sizes.
        self.list_state.reset(block_count);
        self.measured_content_height = None;
        cx.notify();
    }

    pub(super) fn reset_selection_projection(&mut self) {
        self.is_selecting = false;
        self.parsed.clear_selection();
    }

    pub(super) fn is_selectable(&self) -> bool {
        self.selectable
    }

    pub(super) fn block_count(&self) -> usize {
        root_block_count(&self.parsed)
    }

    pub(super) fn selected_text_in(&self, blocks: Option<RangeInclusive<usize>>) -> String {
        match (&self.parsed, blocks) {
            (BlockNode::Root { children }, Some(blocks)) => {
                let children = children
                    .get(blocks)
                    .map_or_else(Vec::new, |children| children.to_vec());
                BlockNode::Root { children }.selected_text()
            }
            _ => self.parsed.selected_text(),
        }
    }

    pub(super) fn block_ix_at(&self, content_y: Pixels) -> Option<usize> {
        let origin = self.bounds.origin.y + self.list_state.scroll_px_offset_for_scrollbar().y;
        let count = self.list_state.item_count();
        let mut ix = self.list_state.logical_scroll_top().item_ix;
        while ix < count {
            let bounds = self.list_state.bounds_for_item(ix)?;
            if content_y < bounds.bottom() - origin {
                return Some(ix);
            }
            ix += 1;
        }
        count.checked_sub(1)
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

    #[cfg(test)]
    pub(super) fn has_measured_block(&self, index: usize) -> bool {
        self.list_state.bounds_for_item(index).is_some()
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

fn root_blocks(node: &BlockNode) -> &[BlockNode] {
    match node {
        BlockNode::Root { children, .. } => children,
        _ => std::slice::from_ref(node),
    }
}

impl Render for MarkdownState {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = cx.entity();
        let parsed = self.parsed.clone();
        let measured_content_height = self.measured_content_height;
        v_flex()
            .w_full()
            .child(render::render_root(
                &parsed,
                self.list_state.clone(),
                render::RootMeasurements {
                    width: self.bounds.size.width,
                    content_height: measured_content_height,
                },
                &state,
                window,
                cx,
            ))
            .on_prepaint(move |bounds, window, cx| {
                let (selection_involves_view, has_snapshot, is_selecting) = {
                    let state = state.read(cx);
                    (
                        state.selection_adapter.participates_in_selection(cx),
                        state.selection_adapter.has_selection_snapshot(cx),
                        state.is_selecting,
                    )
                };
                let mut revision_changed = false;
                let resized = state.update(cx, |state, cx| {
                    revision_changed = state
                        .selection_adapter
                        .update_layout_revision(state.selection_revision, state.is_selecting);
                    let measured_height = state.list_content_height();
                    state.update_layout(bounds, measured_height, cx)
                });
                if !is_selecting
                    && ((resized && selection_involves_view) || (revision_changed && has_snapshot))
                {
                    gpui_base::TextSelection::clear(window, cx);
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use gpui::{
        AppContext as _, Context, Entity, IntoElement, Render, TestAppContext, VisualTestContext,
        Window, div, px,
    };
    use gpui_base::TextSelectionLayer;

    use super::*;

    struct SelectAllRoot {
        markdown: Entity<MarkdownState>,
    }

    impl SelectAllRoot {
        fn new(text: &str, cx: &mut Context<Self>) -> Self {
            let text = text.to_string();
            Self {
                markdown: cx.new(|cx| MarkdownState::new(&text, cx)),
            }
        }
    }

    impl Render for SelectAllRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().child(TextSelectionLayer).child(
                div()
                    .h(px(24.))
                    .overflow_hidden()
                    .child(super::super::MarkdownView::new(&self.markdown).selectable(true)),
            )
        }
    }

    #[gpui::test]
    fn source_and_parsed_text_stay_coherent(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(super::super::init);
        let state = cx.update(|cx| cx.new(|cx| MarkdownState::new("old", cx)));

        assert_eq!(
            state.read_with(cx, |state, _| state.rendered_text()),
            "old\n"
        );

        state.update(cx, |state, cx| {
            state.set_text("new", cx);
            state.push_str(" **value**", cx);
        });
        state.read_with(cx, |state, _| {
            assert_eq!(state.source(), "new **value**");
            assert_eq!(state.rendered_text(), "new value\n");
        });
    }

    #[gpui::test]
    fn select_all_reads_blocks_that_were_never_painted(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
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
        let (view, cx) = cx.add_window_view(|_, cx| SelectAllRoot::new(&source, cx));
        let cx: &mut VisualTestContext = cx;
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let selection = view.read_with(cx, |root, cx| {
            root.markdown.read(cx).selection_handle().clone()
        });
        cx.update(|_, cx| selection.set_local_selection(true, cx));
        let selected = cx.update(gpui_base::TextSelection::selected_text);
        assert_eq!(selected, expected);
    }

    #[gpui::test]
    fn select_all_includes_code_and_table_text(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(super::super::init);
        let source = "intro\n\n```text\nfirst line\nsecond line\n```\n\n| name | value |\n| --- | --- |\n| alpha | beta |";
        let (view, cx) = cx.add_window_view(|_, cx| SelectAllRoot::new(source, cx));
        let cx: &mut VisualTestContext = cx;
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let selection = view.read_with(cx, |root, cx| {
            root.markdown.read(cx).selection_handle().clone()
        });
        cx.update(|_, cx| selection.set_local_selection(true, cx));
        let selected = cx.update(gpui_base::TextSelection::selected_text);
        assert_eq!(
            selected,
            "intro\nfirst line\nsecond line\nname value\nalpha beta\n"
        );
    }
}
