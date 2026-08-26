//! Synchronous Markdown state, structurally adapted from gpui-component's
//! Apache-2.0 `text/state.rs` implementation.

use std::{
    ops::RangeInclusive,
    path::{Path, PathBuf},
};

use gpui::{
    Bounds, Context, FocusHandle, IntoElement, ListAlignment, ListState, ParentElement as _,
    Pixels, Point, Render, SharedString, Styled as _, Window, px,
};
use gpui_base::{ElementExt as _, v_flex};

use super::{
    link_target::{LinkTarget, LinkTargetCache},
    nodes::BlockNode,
    render,
    selection_adapter::MarkdownSelectionAdapter,
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
    link_targets: LinkTargetCache,
    pub(super) pending_context_link: Option<PendingLinkMenu>,
    /// Window position of the last left mouse-down that landed on a link;
    /// a mouse-up nearby is a click, anything farther is a drag-selection.
    pub(super) link_press_origin: Option<Point<Pixels>>,
    pub(super) is_selecting: bool,
    text: String,
    parsed: BlockNode,
    root_block_starts: Option<Vec<usize>>,
    has_potential_link_reference_definition: bool,
    pub(super) list_state: ListState,
    measured_content_height: Option<Pixels>,
    selection_revision: usize,
    pub(super) selection_adapter: MarkdownSelectionAdapter,
    #[cfg(test)]
    last_reparse_bytes: usize,
}

impl MarkdownState {
    /// Parse `text` immediately and create a Markdown state entity value.
    pub fn new(text: &str, cx: &mut Context<Self>) -> Self {
        let parsed_document = super::parse::parse_document(text);
        let parsed = parsed_document.root;
        let block_count = root_block_count(&parsed);
        let selection_adapter = MarkdownSelectionAdapter::new(cx.entity().downgrade(), cx);
        Self {
            focus_handle: cx.focus_handle(),
            bounds: Bounds::default(),
            selectable: false,
            compact_headings: false,
            base_dir: None,
            link_targets: LinkTargetCache::default(),
            pending_context_link: None,
            link_press_origin: None,
            is_selecting: false,
            text: text.to_string(),
            parsed,
            root_block_starts: parsed_document.root_starts,
            has_potential_link_reference_definition: contains_potential_link_reference_definition(
                text,
            ),
            // Measure every block once so the list has a stable total height,
            // then construct/layout/paint only the visible blocks on warm frames.
            list_state: ListState::new(block_count, ListAlignment::Top, px(1000.)).measure_all(),
            measured_content_height: None,
            selection_revision: 0,
            selection_adapter,
            #[cfg(test)]
            last_reparse_bytes: text.len(),
        }
    }

    /// Append text, synchronously reparse the one canonical source, and repaint.
    pub fn push_str(&mut self, text: &str, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        self.has_potential_link_reference_definition |=
            contains_potential_link_reference_definition(text)
                || self.text.ends_with(']') && text.starts_with(':');
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
        self.has_potential_link_reference_definition =
            contains_potential_link_reference_definition(text);
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
        self.link_targets.clear();
        cx.notify();
    }

    pub(super) fn base_dir(&self) -> Option<&Path> {
        self.base_dir.as_deref()
    }

    pub(super) fn resolve_link(&self, url: &str) -> LinkTarget {
        self.link_targets.resolve(url, self.base_dir())
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
        self.link_targets.clear();
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
        let reparse_start = self.incremental_reparse_start();
        let parsed_document = super::parse::parse_document(&self.text[reparse_start..]);
        #[cfg(test)]
        {
            self.last_reparse_bytes = self.text.len() - reparse_start;
        }

        let old_count = root_block_count(&self.parsed);
        let (parsed, unchanged, new_count) = if reparse_start == 0 {
            let old_blocks = root_blocks(&self.parsed);
            let new_blocks = root_blocks(&parsed_document.root);
            let unchanged = old_blocks
                .iter()
                .zip(new_blocks)
                .take_while(|(old, new)| old == new)
                .count();
            let new_count = new_blocks.len();
            self.root_block_starts = parsed_document.root_starts;
            (parsed_document.root, unchanged, new_count)
        } else {
            let old_prefix_count = old_count - 1;
            let BlockNode::Root { mut children } =
                std::mem::replace(&mut self.parsed, BlockNode::Unknown)
            else {
                unreachable!("parsed markdown document must have a root")
            };
            children.truncate(old_prefix_count);
            let BlockNode::Root {
                children: tail_children,
            } = parsed_document.root
            else {
                unreachable!("parsed markdown tail must have a root")
            };
            let new_count = old_prefix_count + tail_children.len();
            children.extend(tail_children);

            self.root_block_starts =
                match (self.root_block_starts.take(), parsed_document.root_starts) {
                    (Some(mut prefix), Some(mut tail)) => {
                        prefix.truncate(old_prefix_count);
                        tail.iter_mut().for_each(|start| *start += reparse_start);
                        prefix.extend(tail);
                        Some(prefix)
                    }
                    _ => None,
                };
            (BlockNode::Root { children }, old_prefix_count, new_count)
        };
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

    fn incremental_reparse_start(&self) -> usize {
        if self.has_potential_link_reference_definition {
            return 0;
        }
        let block_count = root_block_count(&self.parsed);
        self.root_block_starts
            .as_ref()
            .filter(|starts| starts.len() == block_count)
            .and_then(|starts| starts.last().copied())
            .unwrap_or(0)
    }

    fn reparse_reset(&mut self, cx: &mut Context<Self>) {
        self.prepare_reparse(cx);
        let parsed_document = super::parse::parse_document(&self.text);
        self.parsed = parsed_document.root;
        self.root_block_starts = parsed_document.root_starts;
        #[cfg(test)]
        {
            self.last_reparse_bytes = self.text.len();
        }
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

    #[cfg(test)]
    fn last_reparse_bytes(&self) -> usize {
        self.last_reparse_bytes
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

fn contains_potential_link_reference_definition(source: &str) -> bool {
    source.contains("]:")
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
    use std::{fs, process, time::SystemTime};

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
    fn streamed_appends_match_full_parse(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(super::super::init);
        let documents = [
            "plain paragraph\ncontinued line\n\n# atx heading\n\nSetext heading\n===\n",
            "- outer\n  - nested one\n  - nested two\n\n1. ordered\n   1. nested ordered\n\n> quoted\n>\n> - with a list\n",
            "````text\na literal ``` inside the longer fence\n````\n\n```rust\nfn main() {}\n```\n",
            "before\n\n```rust\nlet unfinished = true;\n",
            "| left | center | right |\n| :--- | :---: | ---: |\n| a | b | c |\n",
            "An earlier [reference] is resolved later.\n\n[reference]: https://example.com \"title\"\n",
        ];

        for document in documents {
            let expected = cx.update(|cx| cx.new(|cx| MarkdownState::new(document, cx)));
            let expected_tree = expected.read_with(cx, |state, _| state.parsed.clone());
            for chunk_size in [1, 7, 64] {
                let streamed = cx.update(|cx| cx.new(|cx| MarkdownState::new("", cx)));
                for chunk in document.as_bytes().chunks(chunk_size) {
                    let chunk = std::str::from_utf8(chunk).expect("test corpus is ASCII");
                    streamed.update(cx, |state, cx| state.push_str(chunk, cx));
                }
                streamed.read_with(cx, |state, _| {
                    assert_eq!(state.parsed, expected_tree, "chunk size {chunk_size}");
                });
            }
        }
    }

    #[gpui::test]
    fn append_reparses_only_the_last_root_block(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(super::super::init);
        let document = (0..120)
            .map(|ix| format!("## Closed block {ix}\n\nParagraph {ix}.\n\n"))
            .collect::<String>();
        assert!(document.len() > 2_000);
        let state = cx.update(|cx| cx.new(|cx| MarkdownState::new(&document, cx)));

        state.update(cx, |state, cx| state.push_str("new tail", cx));
        state.read_with(cx, |state, _| {
            assert!(
                state.last_reparse_bytes() < 64,
                "parsed {} of {} bytes",
                state.last_reparse_bytes(),
                state.text.len()
            );
        });
    }

    #[gpui::test]
    fn link_reference_definitions_force_a_full_reparse(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(super::super::init);
        let document = "An earlier [reference].\n\n[reference]: https://example.com\n\nTail";
        let state = cx.update(|cx| cx.new(|cx| MarkdownState::new(document, cx)));

        state.update(cx, |state, cx| state.push_str(" grows", cx));
        state.read_with(cx, |state, _| {
            assert_eq!(state.last_reparse_bytes(), state.text.len());
            assert_eq!(state.parsed, super::super::parse(&state.text));
        });
    }

    #[gpui::test]
    fn link_target_cache_tracks_document_content_and_base_dir(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(super::super::init);
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock should be after Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tcode-markdown-state-link-cache-{}-{nonce}",
            process::id()
        ));
        let first_base = root.join("first");
        let second_base = root.join("second");
        fs::create_dir_all(&first_base).expect("create first base directory");
        fs::create_dir_all(&second_base).expect("create second base directory");
        let path = first_base.join("linked.md");
        fs::write(&path, b"linked").expect("create linked file");

        let state = cx.update(|cx| cx.new(|cx| MarkdownState::new("[link](linked.md)", cx)));
        state.update(cx, |state, cx| {
            state.set_base_dir(Some(first_base.clone()), cx);
            assert_eq!(
                state.resolve_link("linked.md"),
                LinkTarget::Local(path.clone())
            );
        });

        fs::remove_file(&path).expect("remove linked file");
        state.update(cx, |state, cx| {
            assert_eq!(
                state.resolve_link("linked.md"),
                LinkTarget::Local(path.clone())
            );
            state.set_text("changed [link](linked.md)", cx);
            assert_eq!(
                state.resolve_link("linked.md"),
                LinkTarget::Web("linked.md".to_string())
            );
        });

        fs::write(&path, b"linked again").expect("recreate linked file");
        state.update(cx, |state, cx| {
            assert_eq!(
                state.resolve_link("linked.md"),
                LinkTarget::Web("linked.md".to_string())
            );
            state.set_base_dir(Some(second_base), cx);
            assert_eq!(
                state.resolve_link("linked.md"),
                LinkTarget::Web("linked.md".to_string())
            );
            state.set_base_dir(Some(first_base), cx);
            assert_eq!(state.resolve_link("linked.md"), LinkTarget::Local(path));
        });

        fs::remove_dir_all(root).expect("remove temporary directory");
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
