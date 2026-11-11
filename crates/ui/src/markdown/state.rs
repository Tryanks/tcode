//! Synchronous Markdown state, structurally adapted from gpui-component's
//! Apache-2.0 `text/state.rs` implementation.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
};

use crate::theme::ActiveTheme as _;
use crate::widgets::copy::CopiedMark;
use gpui::{
    Bounds, ClipboardItem, Context, FocusHandle, ImageSource, IntoElement, ListAlignment,
    ListState, ParentElement as _, Pixels, Point, Render, SharedString, SharedUri, Size,
    Styled as _, Window, px,
};
use gpui_base::{ElementExt as _, v_flex};

#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
#[cfg(target_family = "wasm")]
use web_time::Instant;

use super::{
    link_target::LinkTarget,
    mermaid,
    nodes::{BlockNode, TextPosition},
    render::{self, RootItem},
    selection::{Membership, RunOrigin, SelectionDocument},
    stream_fade::{FadeFrame, Fades, StreamFade},
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
    Image {
        url: SharedUri,
        title: String,
    },
    Math {
        path: String,
        latex: String,
        /// `None` when the formula does not lay out, so only its source exists.
        show_source: Option<bool>,
    },
}

pub type ImageSourceFn = Rc<dyn Fn(&str) -> Option<ImageSource>>;
pub type ImageStandInFn =
    Rc<dyn Fn(&str, &str, &mut gpui::Window, &mut gpui::App) -> Option<gpui::AnyElement>>;

/// Images a view reads through the host rather than by URL. The chat supplies none.
#[derive(Clone)]
pub struct ImageResolver {
    /// The source to draw a URL from, for a URL this resolver owns.
    pub source: ImageSourceFn,
    /// What stands in for an owned URL until it can be drawn: a loading box, or why it cannot.
    pub pending: ImageStandInFn,
}

/// State backing a [`super::MarkdownView`].
pub struct MarkdownState {
    pub(super) focus_handle: FocusHandle,
    pub(super) bounds: Bounds<Pixels>,
    pub(super) compact_headings: bool,
    pub(super) base_dir: Option<PathBuf>,
    pub(super) image_host: crate::store::ImageHost,
    pub(super) image_resolver: Option<ImageResolver>,
    /// The block the pointer is over, whose actions (a formula's, a
    /// diagram's) fade in.
    pub(super) hover: crate::motion::ListHover,
    pub(super) pending_context: Option<PendingContextTarget>,
    /// Keyed by the copied code block's root path.
    pub(super) copied: CopiedMark,
    pub(super) math_sources: std::collections::HashSet<String>,
    /// The Mermaid fences a reader switched to their source.
    pub(super) diagram_sources: std::collections::HashSet<String>,
    /// The Mermaid fences' diagrams and the renders on their way.
    pub(super) diagrams: mermaid::Diagrams,
    /// The text is still arriving: an open fence is not yet its whole source.
    pub(super) streaming: bool,
    /// The latest append was not watched arriving word by word.
    last_append_burst: bool,
    /// Window position of the last left mouse-down that landed on a link;
    /// a mouse-up nearby is a click, anything farther is a drag-selection.
    pub(super) link_press_origin: Option<Point<Pixels>>,
    text: String,
    parsed: Rc<BlockNode>,
    items: Rc<[RootItem]>,
    root_block_starts: Option<Vec<usize>>,
    /// The text defines a link reference, so an append is parsed with the
    /// whole text: a definition reaches links in blocks the tail does not hold.
    defines_links: bool,
    pub(super) list_state: ListState,
    item_heights: Rc<RefCell<render::ItemHeights>>,
    image_sizes: HashMap<String, Size<Pixels>>,
    /// The glides of parts whose size changes at rest (a picture that
    /// decoded, a formula shown as its source), by render path. Kept here,
    /// not in element state, because the document measures its blocks under
    /// other element ids than it paints them, and both must size alike.
    glides: RefCell<HashMap<String, crate::motion::GlideState>>,
    /// The clocks of the document's other motion (a picture fading in over
    /// its stand-in), by render path: kept with the document, not in element
    /// state, so they carry on when its view is drawn under a new parent.
    motion: crate::motion::owned::Owned,
    layout_revision: usize,
    /// The document this view's text is selected in, when it is selectable.
    pub(super) selection: Option<Membership>,
    /// Whether `selection` is the view's own document rather than one an
    /// owner placed it in.
    owns_selection: bool,
    /// Clears the view's own selection when the focus leaves the view.
    own_selection_focus_out: Option<gpui::Subscription>,
    /// The runs of text painted last frame, which selection ends hit-test.
    pub(super) runs: Rc<RefCell<Vec<PaintedRun>>>,
    /// Whether the document has been drawn in its scene: text appended
    /// before then was never seen arriving, so it does not fade in.
    drawn: bool,
    /// The scene the document is drawn in: in a new one it starts at rest,
    /// as one never drawn does, whatever it was revealing.
    scene: crate::motion::InScene,
    stream_fade: StreamFade,
    /// The fades this frame paints, sampled when the document renders.
    fade_frame: Option<Rc<FadeFrame>>,
    /// The document's height below the bottom of the line the reveal has
    /// reached, as last placed.
    unrevealed: Cell<Pixels>,
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
        let owner = cx.entity_id();
        cx.on_release(move |_, cx| {
            render::clear_code_measurements(owner, cx);
        })
        .detach();
        let parsed = parsed_document.root;
        let items = root_items(&parsed);
        Self {
            focus_handle: cx.focus_handle(),
            bounds: Bounds::default(),
            compact_headings: false,
            base_dir: None,
            image_host: crate::store::ImageHost::NONE,
            image_resolver: None,
            hover: crate::motion::ListHover::new(crate::material::HOVER),
            pending_context: None,
            copied: CopiedMark::default(),
            math_sources: Default::default(),
            diagram_sources: Default::default(),
            diagrams: mermaid::Diagrams::default(),
            streaming: false,
            last_append_burst: false,
            link_press_origin: None,
            text: text.to_string(),
            parsed: Rc::new(parsed),
            root_block_starts: parsed_document.root_starts,
            defines_links: parsed_document.defines_links,
            // Measure every item once so the list has a stable total height,
            // then construct/layout/paint only the visible items on warm frames.
            list_state: ListState::new(items.len(), ListAlignment::Top, px(1000.)).measure_all(),
            item_heights: Rc::new(RefCell::new(render::ItemHeights::new(items.len()))),
            items,
            image_sizes: HashMap::new(),
            glides: RefCell::default(),
            motion: Default::default(),
            layout_revision: 0,
            selection: None,
            owns_selection: false,
            own_selection_focus_out: None,
            runs: Rc::default(),
            drawn: false,
            scene: Default::default(),
            stream_fade: StreamFade::default(),
            fade_frame: None,
            unrevealed: Cell::new(px(0.)),
        }
    }

    /// Draw the document in `epoch`'s scenes.
    pub(crate) fn set_scene(&mut self, epoch: &crate::motion::Epoch) {
        self.scene = crate::motion::InScene::new(epoch);
    }

    /// A new scene started since the document was last reached: it has not
    /// been drawn in it.
    fn enter_scene(&mut self) {
        if self.scene.entered() {
            self.drawn = false;
            self.stream_fade.clear();
            self.fade_frame = None;
        }
    }

    /// Append text, synchronously reparse the one canonical source, and
    /// repaint. While the text is streaming into a document that has been
    /// drawn in its scene, what this adds fades in; anything else -- a thread opening, a
    /// client catching up, a message rebuilt -- appears as it is.
    pub fn push_str(&mut self, text: &str, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        self.text.push_str(text);
        self.reparse_append(cx);
    }

    /// Whether the text is still arriving. While it is, appends fade in and a
    /// Mermaid fence that has not closed is drawn from its whole statements
    /// so far; once it is not, every fence is drawn from its whole source.
    pub fn set_streaming(&mut self, streaming: bool, cx: &mut Context<Self>) {
        if self.streaming == streaming {
            return;
        }
        self.streaming = streaming;
        cx.notify();
    }

    /// Replace the canonical source and synchronously reparse it.
    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.text == text {
            return;
        }
        self.math_sources.clear();
        self.diagram_sources.clear();
        self.diagrams = mermaid::Diagrams::default();
        self.text.clear();
        self.text.push_str(text);
        self.reparse_reset(cx);
    }

    /// Return all rendered text, including blocks that were never painted.
    pub fn rendered_text(&self) -> String {
        with_trailing_newline(self.parsed.text())
    }

    /// Enable or disable selection of this view's text in a document of
    /// its own.
    pub fn set_selectable(&mut self, selectable: bool, cx: &mut Context<Self>) {
        // A view an owner placed in its document stays there.
        if selectable == self.selection.is_some() {
            return;
        }
        self.selection = selectable.then(|| Membership {
            document: super::selection::own_document(cx.entity().downgrade(), cx),
            entry: 0,
        });
        self.owns_selection = selectable;
        self.own_selection_focus_out = None;
        cx.notify();
    }

    /// Clear the view's own selection when the focus leaves the view, as a
    /// transcript clears its selection when the focus leaves it.
    pub(super) fn clear_own_selection_on_focus_out(
        &mut self,
        window: &mut Window,
        cx: &mut gpui::App,
    ) {
        if !self.wants_focus_out_watch() {
            return;
        }
        let Some(document) = self.own_document() else {
            return;
        };
        self.own_selection_focus_out = Some(window.on_focus_out(
            &self.focus_handle,
            cx,
            move |_, _, cx| document.update(cx, |document, cx| document.clear(cx)),
        ));
    }

    /// Whether the view selects on its own and does not yet clear that
    /// selection when the focus leaves it.
    pub(super) fn wants_focus_out_watch(&self) -> bool {
        self.own_selection_focus_out.is_none() && self.owns_selection && self.selection.is_some()
    }

    /// Whether the view's text is entry `entry` of `document`'s selection.
    pub(crate) fn is_selection_entry(
        &self,
        document: &gpui::Entity<SelectionDocument>,
        entry: u64,
    ) -> bool {
        self.selection
            .as_ref()
            .is_some_and(|m| m.document == *document && m.entry == entry)
    }

    /// Make this view's text entry `entry` of `document`'s selection.
    pub fn set_selection_entry(
        &mut self,
        document: &gpui::Entity<SelectionDocument>,
        entry: u64,
        cx: &mut Context<Self>,
    ) {
        if self.is_selection_entry(document, entry) {
            return;
        }
        self.selection = Some(Membership {
            document: document.clone(),
            entry,
        });
        self.owns_selection = false;
        self.own_selection_focus_out = None;
        cx.notify();
    }

    /// The selection document this view wraps itself in, when it selects on
    /// its own.
    pub(super) fn own_document(&self) -> Option<gpui::Entity<SelectionDocument>> {
        self.owns_selection
            .then(|| self.selection.as_ref().map(|m| m.document.clone()))
            .flatten()
    }

    pub(crate) fn parsed_root(&self) -> Rc<BlockNode> {
        self.parsed.clone()
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

    pub(crate) fn set_image_host(&mut self, host: crate::store::ImageHost, cx: &mut Context<Self>) {
        if self.image_host != host {
            self.image_host = host;
            cx.notify();
        }
    }

    pub fn set_image_resolver(&mut self, resolver: Option<ImageResolver>, cx: &mut Context<Self>) {
        self.image_resolver = resolver;
        cx.notify();
    }

    pub(super) fn base_dir(&self) -> Option<&Path> {
        self.base_dir.as_deref()
    }

    pub(super) fn resolve_link(&self, url: &str) -> LinkTarget {
        super::link_target::resolve_link(url, self.base_dir())
    }

    /// The image drawn in the text. A host file is drawn from its largest preview, whose key
    /// does not depend on the width it lays out at.
    pub(super) fn image_source(&self, uri: &SharedUri) -> ImageSource {
        self.resolve_image(uri, |path| {
            crate::store::image_preview(
                self.image_host,
                path,
                crate::store::MAX_PREVIEW_PIXELS as f32,
                false,
            )
        })
    }

    /// The image the viewer opens, which reads a host file's original.
    pub(super) fn opened_image_source(&self, uri: &SharedUri) -> crate::image_viewer::ViewerSource {
        self.resolve_image(uri, |path| {
            crate::image_viewer::ViewerSource::original(
                self.image_host,
                tcode_protocol::OriginalTarget::File { path },
                self.image_source(uri),
            )
        })
    }

    /// Markdown file images belong to the attached host, including relative paths.
    /// Do not check the client's filesystem before asking that host for the bytes.
    fn resolve_image<T: From<ImageSource>>(
        &self,
        uri: &SharedUri,
        host_file: impl FnOnce(PathBuf) -> T,
    ) -> T {
        if let Some(source) = self
            .image_resolver
            .as_ref()
            .and_then(|resolver| (resolver.source)(uri.as_ref()))
        {
            return source.into();
        }
        let path = PathBuf::from(uri.as_ref());
        if path.is_absolute() {
            return host_file(path);
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
                return host_file(PathBuf::from(path));
            }
            return crate::store::remote_image(uri.clone()).into();
        }
        let path = match self.base_dir() {
            Some(base_dir) => base_dir.join(path),
            None => path,
        };
        host_file(path)
    }

    /// The glide of the part at render `path`, whose size changes at rest.
    pub(super) fn glide(&self, path: &str) -> crate::motion::GlideState {
        self.glides
            .borrow_mut()
            .entry(path.to_string())
            .or_default()
            .clone()
    }

    /// The clocks of the document's motion, kept with the document.
    pub(super) fn motion(&self) -> crate::motion::owned::Owned {
        self.motion.clone()
    }

    pub(super) fn image_layout(&self, path: &str) -> (usize, Option<Size<Pixels>>) {
        (self.layout_revision, self.image_sizes.get(path).copied())
    }

    pub(super) fn image_resized(
        &mut self,
        path: String,
        revision: usize,
        size: Size<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if revision != self.layout_revision {
            return;
        }
        let items = self.items_of(&path);
        // The first size seen is the one the image's item was measured with.
        if self
            .image_sizes
            .insert(path, size)
            .is_none_or(|old| old == size)
        {
            return;
        }
        self.remeasure(items, cx);
    }

    pub(super) fn set_math_source(&mut self, path: &str, source: bool, cx: &mut Context<Self>) {
        // The formula's text becomes its source or its glyphs, so a
        // selection's offsets in it would land elsewhere.
        if let Some(membership) = &self.selection {
            membership
                .document
                .update(cx, |document, cx| document.clear(cx));
        }
        self.layout_revision = self.layout_revision.wrapping_add(1);
        if source {
            self.math_sources.insert(path.into());
        } else {
            self.math_sources.remove(path);
        }
        let items = self.items_of(path);
        self.remeasure(items, cx);
    }

    /// The root items that draw what lies at render `path`: every item when
    /// the path is not in a root block.
    fn items_of(&self, path: &str) -> Range<usize> {
        let Some(block) = render::root_block_of(path) else {
            return 0..self.items.len();
        };
        self.items.partition_point(|item| item.block < block)
            ..self.items.partition_point(|item| item.block <= block)
    }

    /// Switch Mermaid fence `path` between its diagram and its source.
    pub(super) fn set_diagram_source(&mut self, path: &str, source: bool, cx: &mut Context<Self>) {
        let showing_source = self.diagram_sources.contains(path);
        if showing_source == source {
            return;
        }
        // The fence's text becomes its code or its picture, so a selection's
        // runs in it would land elsewhere.
        if let Some(membership) = &self.selection {
            membership
                .document
                .update(cx, |document, cx| document.clear(cx));
        }
        self.diagrams.toggle(
            path,
            showing_source,
            cx.background_executor().now(),
            cx.reduce_motion(),
        );
        if source {
            self.diagram_sources.insert(path.into());
        } else {
            self.diagram_sources.remove(path);
        }
        let items = self.items_of(path);
        self.remeasure(items, cx);
    }

    /// Measure `items` again; the blocks after them move.
    fn remeasure(&mut self, items: Range<usize>, cx: &mut Context<Self>) {
        self.item_heights.borrow_mut().invalidate(items.clone());
        self.list_state.remeasure_items(items);
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

    fn reparse_append(&mut self, cx: &mut Context<Self>) {
        let mut reparse_start = self.incremental_reparse_start();
        let mut parsed_document = super::parse::parse_document(&self.text[reparse_start..]);
        if reparse_start > 0 && parsed_document.defines_links {
            reparse_start = 0;
            parsed_document = super::parse::parse_document(&self.text);
        }
        if reparse_start == 0 {
            self.defines_links = parsed_document.defines_links;
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
            // The blocks that parsed alike stay, with what they prepared.
            let mut blocks = old_blocks;
            blocks.extend(tail_children.into_iter().skip(unchanged));
            (blocks, unchanged, old_suffix)
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

        for (old, new) in old_suffix.iter().zip(&blocks[unchanged..]) {
            new.reuse_highlights(old);
        }
        self.enter_scene();
        self.last_append_burst = if self.drawn && self.streaming {
            self.stream_fade.record(
                &old_suffix,
                &blocks[unchanged..],
                unchanged,
                cx.background_executor().now(),
            )
        } else {
            self.stream_fade.clear();
            self.drawn
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
        self.layout_revision = self.layout_revision.wrapping_add(1);

        if invalid < old_item_count || invalid < self.items.len() {
            self.list_state
                .splice(invalid..old_item_count, self.items.len() - invalid);
            // `splice` creates unmeasured items but does not re-arm measure_all.
            self.list_state.remeasure_items(invalid..self.items.len());
            self.item_heights
                .borrow_mut()
                .splice(invalid, self.items.len());
        }
        cx.notify();
    }

    fn incremental_reparse_start(&self) -> usize {
        if self.defines_links {
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
        self.stream_fade.clear();
        self.image_sizes.clear();
        self.glides.borrow_mut().clear();
        render::clear_code_measurements(cx.entity_id(), cx);
        let parsed_document = super::parse::parse_document(&self.text);
        self.items = root_items(&parsed_document.root);
        self.parsed = Rc::new(parsed_document.root);
        self.root_block_starts = parsed_document.root_starts;
        self.defines_links = parsed_document.defines_links;
        self.layout_revision = self.layout_revision.wrapping_add(1);
        // Even an edit that preserves the number of root blocks can change
        // their heights, so every reparse must invalidate the cached sizes.
        self.list_state.reset(self.items.len());
        self.item_heights.borrow_mut().reset(self.items.len());
        cx.notify();
    }

    /// Whether a Mermaid fence's first picture is still being drawn: the
    /// document shows the fence's code until it is.
    pub(crate) fn drawing_first_diagrams(&self) -> bool {
        self.diagrams.opening()
    }

    pub(super) fn is_selectable(&self) -> bool {
        self.selection.is_some()
    }

    /// The part of the run at `origin`, `len` bytes long, that the
    /// selection covers.
    pub(super) fn selected_range(
        &self,
        origin: &RunOrigin,
        len: usize,
        cx: &gpui::App,
    ) -> Option<Range<usize>> {
        let membership = self.selection.as_ref()?;
        membership
            .document
            .read(cx)
            .run_range(membership.entry, origin, len)
    }

    /// The fades the run of `len` bytes at `origin` paints this frame.
    pub(super) fn fades(&self, origin: &RunOrigin, len: usize) -> Fades {
        self.fade_frame.as_ref().map_or_else(Vec::new, |frame| {
            frame.run(&origin.path, origin.line, origin.base, len)
        })
    }

    /// Whether the text is revealing: words fading in, or waiting to.
    pub(crate) fn revealing(&self) -> bool {
        !self.stream_fade.is_empty() || self.fade_frame.is_some()
    }

    /// Show the text as it is from the next frame: a reader who did not
    /// watch it arrive is not shown it arriving. What is appended after
    /// reveals as ever.
    pub(crate) fn settle(&mut self, cx: &mut Context<Self>) {
        self.stream_fade.clear();
        self.fade_frame = None;
        cx.notify();
    }

    /// While the reveal has words waiting: the document's height it has not
    /// reached, as of the last frame, and when it will have.
    pub(crate) fn unrevealed(&self, now: Instant) -> Option<(Pixels, Instant)> {
        let drains_at = self.stream_fade.drains_at().filter(|at| *at > now)?;
        Some((self.unrevealed.get(), drains_at))
    }

    /// Note, once the frame's text is placed, how far below the reveal the
    /// document ends. The frontier's run gives the bottom of its line; the
    /// reveal overtakes what is painted only for a frame or two, when the
    /// top of its item still bounds it from above, so its row never opens
    /// past the text. A diagram or a formula has no line: it comes whole.
    pub(super) fn measure_unrevealed(&self) {
        let unrevealed = self
            .fade_frame
            .as_ref()
            .and_then(|frame| frame.frontier())
            .map_or(px(0.), |(path, line, offset)| {
                let heights = self.item_heights.borrow();
                let runs = self.runs.borrow();
                let painted = runs
                    .iter()
                    .filter(|run| *run.origin.path == *path && run.origin.line == line)
                    .filter(|run| {
                        (run.origin.base..=run.origin.base + run.text.len()).contains(&offset)
                    })
                    .find_map(|run| {
                        let top = run.layout.position_for_index(offset - run.origin.base)?.y;
                        Some(top + run.layout.line_height() - self.bounds.top())
                    });
                let item = render::item_at(&self.items, path, line);
                let bottom = painted.unwrap_or_else(|| {
                    let whole = render::draws_no_text(&self.parsed, &self.items[item]);
                    heights.top_of(item + usize::from(whole))
                });
                (heights.total() - bottom).max(px(0.))
            });
        self.unrevealed.set(unrevealed);
    }

    /// Forget the runs painted when the view was last built: it is being
    /// built again and paints them anew. A view drawn again from last frame
    /// paints nothing, and its runs stand where they were painted.
    fn begin_frame(&self) {
        self.runs.borrow_mut().clear();
    }

    /// Tell the selection's document where its ends are, in the runs the view
    /// painted: each frame the view is drawn, built again or not.
    pub(super) fn register_edges(&self, cx: &gpui::App) {
        let Some(membership) = &self.selection else {
            return;
        };
        let document = membership.document.read(cx);
        for run in self.runs.borrow().iter() {
            if let Some(selected) = self.selected_range(&run.origin, run.text.len(), cx) {
                document.register_edge(
                    membership.entry,
                    &run.origin,
                    &selected,
                    &run.layout,
                    run.visible,
                );
            }
        }
    }

    pub(super) fn register_run(&self, run: PaintedRun) {
        self.runs.borrow_mut().push(run);
    }

    /// The character under `window_point` in the text painted last frame;
    /// none off the text.
    pub(super) fn char_at(&self, window_point: Point<Pixels>) -> Option<TextPosition> {
        let runs = self.runs.borrow();
        let run = runs.iter().find(|run| run.bounds.contains(&window_point))?;
        let offset = run.layout.index_for_position(window_point).ok()?;
        Some(TextPosition {
            path: run.origin.path.to_vec(),
            line: run.origin.line,
            offset: run.origin.base + offset,
        })
    }

    /// The position a selection end at `window_point` falls on, from the text
    /// painted last frame: on the run nearest to it, at the character
    /// boundary under it, or at the end of a run above it or the start of one
    /// below.
    pub(super) fn position_at(&self, window_point: Point<Pixels>) -> Option<TextPosition> {
        let runs = self.runs.borrow();
        let distance = |bounds: &Bounds<Pixels>| {
            let dx = (bounds.left() - window_point.x)
                .max(window_point.x - bounds.right())
                .max(px(0.));
            let dy = (bounds.top() - window_point.y)
                .max(window_point.y - bounds.bottom())
                .max(px(0.));
            dx.as_f32().hypot(dy.as_f32())
        };
        let run = runs
            .iter()
            .min_by(|a, b| distance(&a.bounds).total_cmp(&distance(&b.bounds)))?;
        let offset = if window_point.y < run.bounds.top() {
            0
        } else if window_point.y >= run.bounds.bottom() {
            run.text.len()
        } else {
            super::selection::nearest_boundary(&run.text, &run.layout, window_point)
        };
        Some(TextPosition {
            path: run.origin.path.to_vec(),
            line: run.origin.line,
            offset: run.origin.base + offset,
        })
    }

    /// Record where the document was placed.
    fn update_layout(&mut self, bounds: Bounds<Pixels>) {
        self.bounds = bounds;
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

impl Render for MarkdownState {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.begin_frame();
        self.enter_scene();
        let first_draw = !self.drawn;
        self.drawn = true;
        let root = self.parsed.clone();
        let diagrams = self.diagrams.sync(
            &root,
            &mermaid::Env {
                streaming: self.streaming,
                // The mode a glide is heading for, from its first frame, so
                // the new picture is drawn while the palette still glides
                // and cross-fades in as it does.
                dark: cx.theme().mode.is_dark(),
                now: cx.background_executor().now(),
                still: first_draw || cx.reduce_motion(),
                burst: self.last_append_burst,
                sources: &self.diagram_sources,
            },
            cx,
        );
        for path in diagrams.remeasure {
            let items = self.items_of(&path);
            self.item_heights.borrow_mut().invalidate(items.clone());
            self.list_state.remeasure_items(items);
        }
        // A part on its way to a new size is measured again each frame, so
        // the blocks after it follow it down.
        let now = cx.background_executor().now();
        let gliding: Vec<String> = self
            .glides
            .borrow()
            .iter()
            .filter(|(_, glide)| glide.moving(now))
            .map(|(path, _)| path.clone())
            .collect();
        for path in &gliding {
            let items = self.items_of(path);
            self.item_heights.borrow_mut().invalidate(items.clone());
            self.list_state.remeasure_items(items);
        }
        if diagrams.moving || !gliding.is_empty() {
            window.request_animation_frame();
        }
        self.fade_frame = self
            .stream_fade
            .frame(cx.background_executor().now(), cx.reduce_motion());
        if self.fade_frame.is_some() {
            window.request_animation_frame();
        }
        let state = cx.entity();
        v_flex()
            .w_full()
            .child(render::render_root(
                self.parsed.clone(),
                self.items.clone(),
                self.list_state.clone(),
                self.item_heights.clone(),
                &state,
                window,
                cx,
            ))
            .on_prepaint(move |bounds, _, cx| {
                state.update(cx, |state, _| state.update_layout(bounds));
            })
    }
}

/// A run of text as it was painted, for hit-testing selection ends.
pub(super) struct PaintedRun {
    pub(super) origin: RunOrigin,
    pub(super) text: SharedString,
    pub(super) layout: gpui::TextLayout,
    pub(super) bounds: Bounds<Pixels>,
    /// The clip it was painted under, out of which an end gets no handle.
    pub(super) visible: Bounds<Pixels>,
}

#[cfg(test)]
mod tests {
    use gpui::{
        AppContext as _, Context, Entity, IntoElement, Render, TestAppContext, VisualTestContext,
        Window, div, px,
    };

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
            div()
                .w(px(400.))
                .child(super::super::MarkdownView::new(&self.markdown).selectable(true))
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
            "An earlier [reference] is defined in a quote.\n\n> [reference]: https://example.com\n",
            "```python\ndef f(x) -> list[str]:\n    return x\n```\n\nA [label] after code.\n\nMore.\n",
            "A [gone] link.\n\n[gone]: /url \"title\" stops being a definition\n",
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

    /// Drag from `start` to `end` and read what the view's document holds.
    fn drag(
        view: &Entity<SelectAllRoot>,
        start: gpui::Point<gpui::Pixels>,
        end: gpui::Point<gpui::Pixels>,
        cx: &mut VisualTestContext,
    ) -> String {
        use gpui::{Modifiers, MouseButton};
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));
        cx.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));
        cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));
        view.read_with(cx, |root, cx| {
            let state = root.markdown.read(cx);
            let document = &state.selection.as_ref().unwrap().document;
            document.read(cx).selected_text(cx)
        })
    }

    fn open<'a>(
        text: &str,
        cx: &'a mut TestAppContext,
    ) -> (Entity<SelectAllRoot>, &'a mut VisualTestContext) {
        cx.update(crate::theme::init);
        cx.update(super::super::init);
        let text = text.to_string();
        let (view, cx) = cx.add_window_view(|_, cx| SelectAllRoot::new(&text, cx));
        cx.update(|window, cx| _ = window.draw(cx));
        (view, cx)
    }

    #[gpui::test]
    fn a_drag_through_a_table_takes_each_cell_whole_before_the_next(cx: &mut TestAppContext) {
        use gpui::point;

        let (view, cx) = open(
            "| name | value |\n| --- | --- |\n\
             | a long first cell whose words wrap over several rows of the table, \
             because the window is far narrower than it | x |",
            cx,
        );
        let first = cx.debug_bounds("markdown-table-cell-content-1-0").unwrap();
        let second = cx.debug_bounds("markdown-table-cell-content-1-1").unwrap();
        // The end lies on the first cell's first row, beside its wrapped rest.
        let start = point(first.left() + px(1.), first.top() + px(4.));
        let end = point(second.right() - px(1.), second.top() + px(4.));
        assert_eq!(
            drag(&view, start, end, cx),
            "a long first cell whose words wrap over several rows of the table, because the \
             window is far narrower than it\tx"
        );
    }
}
