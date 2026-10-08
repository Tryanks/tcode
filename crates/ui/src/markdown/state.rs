//! Synchronous Markdown state, structurally adapted from gpui-component's
//! Apache-2.0 `text/state.rs` implementation.

use std::{
    ops::RangeInclusive,
    path::{Path, PathBuf},
    rc::Rc,
};

use crate::widgets::copy::CopiedMark;
use gpui::{
    Bounds, ClipboardItem, Context, FocusHandle, ImageSource, IntoElement, ListAlignment,
    ListState, ParentElement as _, Pixels, Point, Render, SharedString, SharedUri, Styled as _,
    Window, px,
};
use gpui_base::{ElementExt as _, v_flex};

use super::{
    link_target::{LinkTarget, LinkTargetCache},
    nodes::{BlockNode, TextPosition},
    render::{self, RootItem},
    selection_adapter::MarkdownSelectionAdapter,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PendingLinkMenu {
    pub(super) target: LinkTarget,
    pub(super) text: SharedString,
    pub(super) raw_url: SharedString,
}

/// What the last right-click landed on, recorded by the inline that was hit
/// and read by the view's context menu, which opens after the press settles.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PendingContextTarget {
    Link(PendingLinkMenu),
    CodeBlock(String),
    Image { url: SharedUri, title: String },
}

/// State backing a [`super::MarkdownView`].
pub struct MarkdownState {
    pub(super) focus_handle: FocusHandle,
    pub(super) bounds: Bounds<Pixels>,
    pub(super) selectable: bool,
    pub(super) compact_headings: bool,
    pub(super) base_dir: Option<PathBuf>,
    link_targets: LinkTargetCache,
    pub(super) pending_context: Option<PendingContextTarget>,
    /// Keyed by the copied code block's root path.
    pub(super) copied: CopiedMark,
    pub(super) math_sources: std::collections::HashSet<String>,
    /// Window position of the last left mouse-down that landed on a link;
    /// a mouse-up nearby is a click, anything farther is a drag-selection.
    pub(super) link_press_origin: Option<Point<Pixels>>,
    pub(super) is_selecting: bool,
    text: String,
    parsed: Rc<BlockNode>,
    items: Rc<[RootItem]>,
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
        Self::from_parsed(text, parsed_document, cx)
    }

    /// Create a Markdown state from a document parsed on another executor.
    pub(crate) fn from_parsed(
        text: &str,
        parsed_document: super::parse::ParsedDocument,
        cx: &mut Context<Self>,
    ) -> Self {
        let parsed = parsed_document.root;
        let items = root_items(&parsed);
        let selection_adapter = MarkdownSelectionAdapter::new(cx.entity().downgrade(), cx);
        Self {
            focus_handle: cx.focus_handle(),
            bounds: Bounds::default(),
            selectable: false,
            compact_headings: false,
            base_dir: None,
            link_targets: LinkTargetCache::default(),
            pending_context: None,
            copied: CopiedMark::default(),
            math_sources: Default::default(),
            link_press_origin: None,
            is_selecting: false,
            text: text.to_string(),
            parsed: Rc::new(parsed),
            root_block_starts: parsed_document.root_starts,
            has_potential_link_reference_definition: contains_potential_link_reference_definition(
                text,
            ),
            // Measure every item once so the list has a stable total height,
            // then construct/layout/paint only the visible items on warm frames.
            list_state: ListState::new(items.len(), ListAlignment::Top, px(1000.)).measure_all(),
            items,
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
        self.math_sources.clear();
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

    /// Markdown file images belong to the attached host, including relative paths.
    /// Do not check the client's filesystem before asking that host for the bytes.
    pub(super) fn image_source(&self, uri: &SharedUri) -> ImageSource {
        let path = PathBuf::from(uri.as_ref());
        if path.is_absolute() {
            return crate::store::host_image(path);
        }
        if let Ok(url) = url::Url::parse(uri.as_ref()) {
            if url.scheme() == "file"
                && let Ok(path) = percent_encoding::percent_decode_str(url.path()).decode_utf8()
            {
                // Decode the host's path without the viewing platform's filesystem rules.
                let path = match url.host_str() {
                    Some(host) => format!("//{host}{path}"),
                    None => path.into_owned(),
                };
                let path = path
                    .strip_prefix('/')
                    .filter(|path| path.as_bytes().get(1) == Some(&b':'))
                    .unwrap_or(&path);
                return crate::store::host_image(PathBuf::from(path));
            }
            return uri.clone().into();
        }
        let path = match self.base_dir() {
            Some(base_dir) => base_dir.join(path),
            None => path,
        };
        crate::store::host_image(path)
    }

    pub(super) fn set_math_source(&mut self, path: &str, source: bool, cx: &mut Context<Self>) {
        self.prepare_reparse(cx);
        self.selection_revision = self.selection_revision.wrapping_add(1);
        self.selection_adapter
            .virtual_blocks
            .borrow_mut()
            .forget_positions();
        if source {
            self.math_sources.insert(path.into());
        } else {
            self.math_sources.remove(path);
        }
        self.list_state.reset(self.items.len());
        self.measured_content_height = None;
        cx.notify();
    }

    pub(super) fn copy_code(&mut self, path: String, code: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(code));
        self.copied.mark(path, |state| &mut state.copied, cx);
    }

    pub(super) fn set_pending_context(
        &mut self,
        pending_context: Option<PendingContextTarget>,
        cx: &mut Context<Self>,
    ) {
        if self.pending_context == pending_context {
            return;
        }
        self.pending_context = pending_context;
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

        let BlockNode::Root {
            children: mut old_blocks,
        } = Rc::unwrap_or_clone(std::mem::replace(
            &mut self.parsed,
            Rc::new(BlockNode::Unknown),
        ))
        else {
            unreachable!("parsed markdown document must have a root")
        };
        let BlockNode::Root {
            children: tail_children,
        } = parsed_document.root
        else {
            unreachable!("parsed markdown tail must have a root")
        };
        let old_block_count = old_blocks.len();
        let (blocks, unchanged, old_suffix) = if reparse_start == 0 {
            // The old last block is compared even when it is unchanged: a
            // block after it changes the gap below it.
            let unchanged = old_blocks
                .iter()
                .zip(&tail_children)
                .take_while(|(old, new)| old == new)
                .count()
                .min(old_block_count.saturating_sub(1));
            self.root_block_starts = parsed_document.root_starts;
            let old_suffix = old_blocks.split_off(unchanged);
            (tail_children, unchanged, old_suffix)
        } else {
            let old_prefix_count = old_blocks.len() - 1;
            let old_suffix = old_blocks.split_off(old_prefix_count);
            let mut blocks = old_blocks;
            blocks.extend(tail_children);

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
            (blocks, old_prefix_count, old_suffix)
        };

        // An append can only affect the old trailing block and blocks added
        // after it. Keep the measured items before it, and those of its items
        // that paint as they did.
        let first_changed = self.items.partition_point(|item| item.block < unchanged);
        let mut items = self.items[..first_changed].to_vec();
        render::push_root_items(&blocks[unchanged..], unchanged, &mut items);
        let kept = self.items[first_changed..]
            .iter()
            .zip(&items[first_changed..])
            .take_while(|(old, new)| {
                old == new
                    && render::item_renders_alike(
                        &old_suffix[old.block - unchanged],
                        old.block + 1 == old_block_count,
                        &blocks[new.block],
                        new.block + 1 == blocks.len(),
                        new,
                    )
            })
            .count();
        let invalid = first_changed + kept;
        let old_item_count = self.items.len();
        self.parsed = Rc::new(BlockNode::Root { children: blocks });
        self.items = items.into();
        self.selection_revision = self.selection_revision.wrapping_add(1);

        if invalid < old_item_count || invalid < self.items.len() {
            self.list_state
                .splice(invalid..old_item_count, self.items.len() - invalid);
            // `splice` creates unmeasured items but does not re-arm measure_all.
            self.list_state.remeasure_items(invalid..self.items.len());
            self.measured_content_height = None;
        }
        cx.notify();
    }

    fn incremental_reparse_start(&self) -> usize {
        if self.has_potential_link_reference_definition {
            return 0;
        }
        let block_count = match &*self.parsed {
            BlockNode::Root { children } => children.len(),
            _ => 1,
        };
        self.root_block_starts
            .as_ref()
            .filter(|starts| starts.len() == block_count)
            .and_then(|starts| starts.last().copied())
            .unwrap_or(0)
    }

    fn reparse_reset(&mut self, cx: &mut Context<Self>) {
        self.prepare_reparse(cx);
        let parsed_document = super::parse::parse_document(&self.text);
        self.items = root_items(&parsed_document.root);
        self.parsed = Rc::new(parsed_document.root);
        self.root_block_starts = parsed_document.root_starts;
        #[cfg(test)]
        {
            self.last_reparse_bytes = self.text.len();
        }
        self.selection_revision = self.selection_revision.wrapping_add(1);
        self.selection_adapter
            .virtual_blocks
            .borrow_mut()
            .forget_positions();
        // Even an edit that preserves the number of root blocks can change
        // their heights, so every reparse must invalidate the cached sizes.
        self.list_state.reset(self.items.len());
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

    pub(super) fn item_count(&self) -> usize {
        self.items.len()
    }

    /// The selected text in the root list's `items`, or in the whole
    /// document when the selection is not bounded to them. A block holding
    /// the selection's `start` or `end` is read from those positions.
    pub(super) fn selected_text_in(
        &self,
        items: Option<RangeInclusive<usize>>,
        start: Option<TextPosition>,
        end: Option<TextPosition>,
    ) -> String {
        let (BlockNode::Root { children }, Some(items)) = (&*self.parsed, items) else {
            return self.parsed.selected_text();
        };
        let (Some(first), Some(last)) =
            (self.items.get(*items.start()), self.items.get(*items.end()))
        else {
            return String::new();
        };
        let children = (first.block..=last.block)
            .map(|block| {
                let from = start.filter(|position| position.block == block);
                let to = end.filter(|position| position.block == block);
                if from.is_some() || to.is_some() {
                    return children[block].selected_between(from, to);
                }
                let span_start = (block == first.block)
                    .then(|| first.span.as_ref().map(|span| span.start))
                    .flatten();
                let span_end = (block == last.block)
                    .then(|| last.span.as_ref().map(|span| span.end))
                    .flatten();
                render::block_span(&children[block], span_start, span_end)
            })
            .collect();
        BlockNode::Root { children }.selected_text()
    }

    /// The text position under a content point, from the text painted in
    /// the last frame.
    pub(super) fn text_position_at(&self, content_point: Point<Pixels>) -> Option<TextPosition> {
        let BlockNode::Root { children } = &*self.parsed else {
            return None;
        };
        let window_point =
            content_point + self.list_state.scroll_px_offset_for_scrollbar() + self.bounds.origin;
        self.selection_adapter
            .runs_at(window_point)
            .into_iter()
            .find_map(|(state, offset)| {
                children
                    .iter()
                    .enumerate()
                    .find_map(|(block, node)| node.position_of(block, &state, offset))
            })
    }

    pub(super) fn item_ix_at(&self, content_y: Pixels) -> Option<usize> {
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
            self.selection_adapter
                .virtual_blocks
                .borrow_mut()
                .forget_positions();
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
    pub(super) fn has_measured_item(&self, index: usize) -> bool {
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

fn root_items(root: &BlockNode) -> Rc<[RootItem]> {
    let mut items = Vec::new();
    if let BlockNode::Root { children } = root {
        render::push_root_items(children, 0, &mut items);
    }
    items.into()
}

fn contains_potential_link_reference_definition(source: &str) -> bool {
    source.contains("]:")
}

impl Render for MarkdownState {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = cx.entity();
        let measured_content_height = self.measured_content_height;
        v_flex()
            .w_full()
            .child(render::render_root(
                self.parsed.clone(),
                self.items.clone(),
                self.list_state.clone(),
                measured_content_height,
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
            "An earlier [reference].\n\n[reference]: https://example.com\n\nTail grows",
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
                    assert_eq!(
                        state.parsed, expected_tree,
                        "{document:?}, chunk size {chunk_size}"
                    );
                    if document.contains("[reference]") {
                        let BlockNode::Root { children } = state.parsed.as_ref() else {
                            panic!("root")
                        };
                        let BlockNode::Paragraph(paragraph) = &children[0] else {
                            panic!("paragraph")
                        };
                        let urls = paragraph
                            .children
                            .iter()
                            .flat_map(|inline| &inline.marks)
                            .filter_map(|(_, mark)| {
                                mark.link.as_ref().map(|link| link.url.as_ref())
                            })
                            .collect::<Vec<_>>();
                        assert_eq!(urls, ["https://example.com"], "chunk size {chunk_size}");
                    }
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
    fn drag_copies_exactly_the_highlighted_text(cx: &mut TestAppContext) {
        use gpui::{Modifiers, MouseButton, point};

        cx.update(crate::theme::init);
        cx.update(super::super::init);
        let (view, cx) = cx.add_window_view(|_, cx| SelectAllRoot::new("Hello world", cx));
        let cx: &mut VisualTestContext = cx;
        cx.update(|window, cx| _ = window.draw(cx));

        // The sweep ends the drag on both halves of every character it crosses.
        for x in 1..120 {
            let (start, end) = (point(px(1.), px(12.)), point(px(x as f32), px(12.)));
            cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
            cx.update(|window, cx| _ = window.draw(cx));
            cx.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::default());
            cx.update(|window, cx| _ = window.draw(cx));
            cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());
            cx.update(|window, cx| _ = window.draw(cx));

            let highlighted =
                view.read_with(cx, |root, cx| root.markdown.read(cx).parsed.selected_text());
            let copied = cx.update(gpui_base::TextSelection::selected_text);
            assert_eq!(copied, highlighted, "drag to x = {x}");
        }
    }
}
