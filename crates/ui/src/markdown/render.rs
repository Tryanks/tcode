//! Block renderer adapted from gpui-component's Apache-2.0 `text/node.rs` and
//! `text/document.rs`, with rushdown IR and syntect highlighting.

use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    ops::Range,
    rc::Rc,
    sync::{Arc, Mutex},
};

use crate::highlight::HighlightTheme;
use crate::scroll::ScrollableElement as _;
use crate::theme::ActiveTheme as _;
use crate::widgets::tooltip::Tooltip;
use gpui::{
    AnyElement, App, AvailableSpace, Bounds, Element, ElementId, Entity, FontStyle, FontWeight,
    GlobalElementId, HighlightStyle, InspectorElementId, InteractiveElement as _, IntoElement,
    LayoutId, ListState, ObjectFit, ParentElement as _, Pixels, Rems, Role, SharedString,
    StatefulInteractiveElement as _, Style, Styled as _, StyledImage as _, Window, div, img,
    prelude::FluentBuilder as _, px, relative, rems, size,
};
use gpui_base::{h_flex, v_flex};

use crate::{diff::model::sub_runs, highlight};

use super::{
    inline::{Inline, InlineState},
    inline_flow::{InlineCodeStyle, InlineFlow, InlineFlowItem},
    link_target::LinkTarget,
    nodes::{BlockNode, CodeBlock, ColumnumnAlign, Paragraph, Table, TextMark},
    state::MarkdownState,
    utils::list_item_prefix,
};

const CODE_CACHE_CAPACITY: usize = 64;
const BLOCK_OVERDRAW: Pixels = px(300.);
const CODE_LINES_PER_ITEM: usize = 32;
const LIST_ITEMS_PER_ITEM: usize = 8;
const TABLE_BORDER_PX: f32 = 1.;
const HEADING_BASE_FONT_SIZE: Pixels = px(15.);
const INLINE_CODE_FONT_SIZE: Pixels = px(13.);
const INLINE_CODE_RADIUS: Pixels = px(4.);
type HighlightRuns = Vec<(Range<usize>, HighlightStyle)>;
type SharedHighlightRuns = Arc<HighlightRuns>;
type LinkRuns = Vec<(Range<usize>, super::nodes::LinkMark)>;
type FontOverrides = Vec<(Range<usize>, SharedString)>;
type ParagraphTextStyle = (String, LinkRuns, HighlightRuns, FontOverrides);
type MarkRuns = (LinkRuns, HighlightRuns, FontOverrides);

#[derive(Clone, Hash, PartialEq, Eq)]
struct CodeCacheKey {
    code: String,
    lang: String,
    theme: HighlightTheme,
}

#[derive(Default)]
struct CodeHighlightCache {
    entries: HashMap<CodeCacheKey, SharedHighlightRuns>,
    order: VecDeque<CodeCacheKey>,
}

thread_local! {
    static CODE_HIGHLIGHTS: RefCell<CodeHighlightCache> = RefCell::new(CodeHighlightCache::default());
}

#[derive(Clone)]
struct RenderOptions {
    path: String,
    in_list: bool,
    ordered: bool,
    list_start: u32,
    depth: usize,
    is_last: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            path: "root".to_string(),
            in_list: false,
            ordered: false,
            list_start: 1,
            depth: 0,
            is_last: true,
        }
    }
}

impl RenderOptions {
    fn child(&self, ix: usize, is_last: bool) -> Self {
        Self {
            path: format!("{}-{ix}", self.path),
            is_last,
            ..self.clone()
        }
    }

    fn gap(&self) -> Rems {
        if self.in_list || self.is_last {
            rems(0.)
        } else {
            rems(1.)
        }
    }
}

/// One item of the virtualized root list: a whole root block, or a run of the
/// lines of a long code block or the items of a long list, so a block taller
/// than the viewport is only built where it is visible.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RootItem {
    pub(super) block: usize,
    /// The lines or list items of a split block that this item renders.
    pub(super) span: Option<Range<usize>>,
}

/// Append the root list's items for `blocks`, the root blocks from index
/// `first_block`.
pub(super) fn push_root_items(blocks: &[BlockNode], first_block: usize, items: &mut Vec<RootItem>) {
    for (offset, block) in blocks.iter().enumerate() {
        let block_ix = first_block + offset;
        let (len, per_item) = match block {
            BlockNode::CodeBlock(code) => (code_lines(&code.code).len(), CODE_LINES_PER_ITEM),
            BlockNode::List { children, .. } => (children.len(), LIST_ITEMS_PER_ITEM),
            _ => (0, 1),
        };
        if len <= per_item {
            items.push(RootItem {
                block: block_ix,
                span: None,
            });
            continue;
        }
        items.extend((0..len).step_by(per_item).map(|start| RootItem {
            block: block_ix,
            span: Some(start..(start + per_item).min(len)),
        }));
    }
}

/// Whether `item` of block `new` paints exactly as the same item of `old`
/// did, so the height measured for it still holds. `old_is_last` and
/// `new_is_last` say whether each block ends its document: the gap below a
/// block's final item depends on it.
pub(super) fn item_renders_alike(
    old: &BlockNode,
    old_is_last: bool,
    new: &BlockNode,
    new_is_last: bool,
    item: &RootItem,
) -> bool {
    let Some(span) = item.span.clone() else {
        return old == new && old_is_last == new_is_last;
    };
    let same_end = |old_len: usize, new_len: usize| {
        let (old_final, new_final) = (span.end == old_len, span.end == new_len);
        old_final == new_final && (!old_final || old_is_last == new_is_last)
    };
    match (old, new) {
        (BlockNode::CodeBlock(old), BlockNode::CodeBlock(new)) => {
            let (old_lines, new_lines) = (code_lines(&old.code), code_lines(&new.code));
            old.lang == new.lang
                && same_end(old_lines.len(), new_lines.len())
                && old_lines.get(span.clone()) == new_lines.get(span)
        }
        (
            BlockNode::List {
                children: old_items,
                ordered: old_ordered,
                start: old_start,
            },
            BlockNode::List {
                children: new_items,
                ordered: new_ordered,
                start: new_start,
            },
        ) => {
            old_ordered == new_ordered
                && old_start == new_start
                && same_end(old_items.len(), new_items.len())
                && old_items.get(span.clone()) == new_items.get(span)
        }
        _ => false,
    }
}

/// The part of `block` from line or list item `start` to `end`, sharing its
/// selection state, for extracting the text selected across root items.
pub(super) fn block_span(block: &BlockNode, start: Option<usize>, end: Option<usize>) -> BlockNode {
    if start.is_none() && end.is_none() {
        return block.clone();
    }
    match block {
        BlockNode::CodeBlock(code) => {
            let lines = code_lines(&code.code);
            let span = start.unwrap_or(0)..end.unwrap_or(lines.len());
            let states = code
                .line_states
                .lock()
                .ok()
                .and_then(|states| states.get(span.clone()).map(<[_]>::to_vec))
                .unwrap_or_default();
            BlockNode::CodeBlock(CodeBlock {
                code: lines.get(span).unwrap_or_default().join("\n").into(),
                lang: code.lang.clone(),
                line_states: Arc::new(Mutex::new(states)),
            })
        }
        BlockNode::List {
            children,
            ordered,
            start: list_start,
        } => BlockNode::List {
            children: children
                .get(start.unwrap_or(0)..end.unwrap_or(children.len()))
                .map_or_else(Vec::new, <[_]>::to_vec),
            ordered: *ordered,
            start: *list_start,
        },
        _ => block.clone(),
    }
}

pub(super) fn render_root(
    node: Rc<BlockNode>,
    items: Rc<[RootItem]>,
    list_state: ListState,
    content_height: Option<Pixels>,
    state: &Entity<MarkdownState>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    if !matches!(*node, BlockNode::Root { .. }) {
        return render_block(&node, RenderOptions::default(), state, window, cx);
    }
    if list_state.item_count() != items.len() {
        list_state.reset(items.len());
    }

    div()
        .id("root")
        .w_full()
        .child(VirtualizedBlockList {
            root: node,
            items,
            list_state,
            content_height,
            state: state.clone(),
        })
        .into_any_element()
}

fn render_root_item(
    root: &BlockNode,
    items: &[RootItem],
    ix: usize,
    width: Pixels,
    state: &Entity<MarkdownState>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let BlockNode::Root { children } = root else {
        unreachable!("the root list renders the children of a root")
    };
    let item = &items[ix];
    let options = RenderOptions::default().child(item.block, item.block + 1 == children.len());
    let content = match (&children[item.block], item.span.clone()) {
        (BlockNode::CodeBlock(code), Some(span)) => {
            render_code_block(code, Some(span), &options, state, cx)
        }
        (
            BlockNode::List {
                children,
                ordered,
                start,
            },
            Some(span),
        ) => render_list(children, *ordered, *start, span, options, state, window, cx),
        (block, _) => render_block(block, options, state, window, cx),
    };
    let block = div()
        .w_full()
        .when(width > px(0.), |block| block.w(width))
        .child(content);
    #[cfg(test)]
    {
        block
            .debug_selector(move || format!("markdown-block-{ix}"))
            .into_any_element()
    }
    #[cfg(not(test))]
    {
        block.into_any_element()
    }
}

/// A full-height layout leaf backed by a viewport-sized GPUI list.
///
/// The outer chat list must measure the whole Markdown document as one row, but
/// the inner list must only construct and paint the slice admitted by the
/// outer row's content mask. A regular `Infer` list uses its full layout bounds
/// as its viewport, so nesting it in a virtualized row defeats block culling.
struct VirtualizedBlockList {
    root: Rc<BlockNode>,
    items: Rc<[RootItem]>,
    list_state: ListState,
    content_height: Option<Pixels>,
    state: Entity<MarkdownState>,
}

impl IntoElement for VirtualizedBlockList {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for VirtualizedBlockList {
    type RequestLayoutState = Option<AnyElement>;
    type PrepaintState = Option<AnyElement>;

    fn id(&self) -> Option<ElementId> {
        None
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
        let Some(content_height) = self.content_height else {
            // Outer-list overdraw lays out rows without prepainting them. A
            // regular column measures cold blocks at the actual parent width;
            // an Infer list would cache their min-content heights instead.
            let mut column = v_flex()
                .w_full()
                .children((0..self.items.len()).map(|ix| {
                    render_root_item(&self.root, &self.items, ix, px(0.), &self.state, window, cx)
                }))
                .into_any_element();
            let layout = column.request_layout(window, cx);
            return (layout, Some(column));
        };
        let layout_id = window.request_measured_layout(
            Style::default(),
            move |known_dimensions, available_space, _, _| {
                let width = known_dimensions
                    .width
                    .unwrap_or(match available_space.width {
                        AvailableSpace::Definite(width) => width,
                        AvailableSpace::MinContent | AvailableSpace::MaxContent => px(0.),
                    });
                size(width, content_height)
            },
        );
        (layout_id, None)
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let visible = bounds.intersect(&window.content_mask().bounds);
        if visible.size.width <= px(0.) || visible.size.height <= px(0.) {
            return None;
        }

        let viewport = Bounds::from_corners(
            gpui::point(
                bounds.left(),
                (visible.top() - BLOCK_OVERDRAW).max(bounds.top()),
            ),
            gpui::point(
                bounds.right(),
                (visible.bottom() + BLOCK_OVERDRAW).min(bounds.bottom()),
            ),
        );
        let desired_offset = viewport.top() - bounds.top();
        let current_offset = -self.list_state.scroll_px_offset_for_scrollbar().y;
        self.list_state.scroll_by(desired_offset - current_offset);

        let root = self.root.clone();
        let items = self.items.clone();
        let state = self.state.clone();
        let width = viewport.size.width;
        let mut list = gpui::list(self.list_state.clone(), move |ix, window, cx| {
            render_root_item(&root, &items, ix, width, &state, window, cx)
        })
        .size_full()
        .into_any_element();
        list.layout_as_root(
            size(
                AvailableSpace::Definite(viewport.size.width),
                AvailableSpace::Definite(viewport.size.height),
            ),
            window,
            cx,
        );
        list.prepaint_at(viewport.origin, window, cx);
        Some(list)
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        list: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(list) = list {
            list.paint(window, cx);
        }
    }
}

fn render_block(
    node: &BlockNode,
    options: RenderOptions,
    state: &Entity<MarkdownState>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let gap = options.gap();
    match node {
        BlockNode::Root { children, .. } => {
            let len = children.len();
            v_flex()
                .id(options.path.clone())
                .w_full()
                .children(children.iter().enumerate().map(|(ix, child)| {
                    render_block(child, options.child(ix, ix + 1 == len), state, window, cx)
                }))
                .into_any_element()
        }
        BlockNode::Paragraph(paragraph) => div()
            .id(options.path.clone())
            .w_full()
            .pb(gap)
            .whitespace_normal()
            .child(render_paragraph(paragraph, &options.path, state, cx))
            .into_any_element(),
        BlockNode::Heading {
            level, children, ..
        } => {
            let (scale, weight) = match level {
                1 => (2., FontWeight::BOLD),
                2 => (1.5, FontWeight::SEMIBOLD),
                3 => (1.25, FontWeight::SEMIBOLD),
                4 => (1.125, FontWeight::SEMIBOLD),
                5 => (1., FontWeight::SEMIBOLD),
                6 => (1., FontWeight::MEDIUM),
                _ => (1., FontWeight::NORMAL),
            };
            // In a chat message a heading is a section label, not a title page.
            let size = if state.read(cx).compact_headings {
                match level {
                    1 => px(17.),
                    2 => px(15.),
                    _ => px(13.5),
                }
            } else {
                HEADING_BASE_FONT_SIZE * scale
            };
            div()
                .id(options.path.clone())
                .pb(rems(0.3))
                .whitespace_normal()
                .text_size(size)
                .font_weight(weight)
                .child(render_paragraph(children, &options.path, state, cx))
                .into_any_element()
        }
        BlockNode::Blockquote { children, .. } => {
            let len = children.len();
            div()
                .id(options.path.clone())
                .w_full()
                .pb(gap)
                .child(
                    v_flex()
                        .w_full()
                        .text_color(cx.theme().muted_foreground)
                        .border_l_3()
                        .border_color(cx.theme().secondary_active)
                        .px_4()
                        .children(children.iter().enumerate().map(|(ix, child)| {
                            render_block(child, options.child(ix, ix + 1 == len), state, window, cx)
                        })),
                )
                .into_any_element()
        }
        BlockNode::List {
            children,
            ordered,
            start,
        } => render_list(
            children,
            *ordered,
            *start,
            0..children.len(),
            options,
            state,
            window,
            cx,
        ),
        BlockNode::ListItem { .. } => render_list_item(node, 0, options, state, window, cx),
        BlockNode::CodeBlock(code) => render_code_block(code, None, &options, state, cx),
        BlockNode::Table(table) => render_table(table, &options, state, window, cx),
        BlockNode::HorizontalRule => div()
            .id(options.path)
            .pb(gap)
            .child(div().h(px(2.)).w_full().bg(cx.theme().border))
            .into_any_element(),
        BlockNode::Unknown => div().into_any_element(),
    }
}

fn render_paragraph(
    paragraph: &Paragraph,
    id: &str,
    view: &Entity<MarkdownState>,
    cx: &mut App,
) -> AnyElement {
    let has_image = paragraph.children.iter().any(|child| child.image.is_some());
    let has_text = paragraph
        .children
        .iter()
        .any(|child| !child.text.is_empty());
    let has_image_link = paragraph
        .children
        .iter()
        .any(|child| super::image_link::for_node(child).is_some());
    if (has_image && has_text) || has_image_link {
        return InlineFlow::new(
            id.to_string(),
            view.clone(),
            inline_flow_items(paragraph, cx),
        )
        .into_any_element();
    }
    if has_image {
        let images = paragraph
            .children
            .iter()
            .filter_map(|child| child.image.as_ref());
        let view = view.clone();
        return h_flex()
            .id(id.to_string())
            .flex_wrap()
            .gap_1()
            .children(images.enumerate().map(move |(ix, image)| {
                let title = image.title();
                let tooltip_title = title.clone();
                let view = view.clone();
                let source = view.read(cx).image_source(&image.url);
                let link = image.link.clone();
                let label = if !title.trim().is_empty() {
                    title.clone()
                } else if let Some(link) = &link {
                    link.url.to_string()
                } else {
                    crate::tr!("markdown.open_image").into_owned()
                };
                let role = if link.is_some() {
                    Role::Link
                } else {
                    Role::Button
                };
                crate::material::accessible_clickable(img(source.clone()), ix, role, label, cx)
                    .object_fit(ObjectFit::Contain)
                    .max_w(relative(1.))
                    .max_h(px(720.))
                    .min_w(px(15.))
                    .min_h(px(15.))
                    .cursor_pointer()
                    .when(link.is_some(), |image| {
                        image.tooltip(move |window, cx| {
                            Tooltip::new(tooltip_title.clone()).build(window, cx)
                        })
                    })
                    .on_click(move |_, window, cx| {
                        gpui_base::TextSelection::end(window, cx);
                        cx.stop_propagation();
                        if let Some(link) = &link {
                            match view.read(cx).resolve_link(&link.url) {
                                LinkTarget::Web(url) => cx.open_url(&url),
                                LinkTarget::Local(path) => cx.open_with_system(&path),
                            }
                        } else {
                            crate::attachments::open_image_lightbox(
                                source.clone(),
                                title.clone(),
                                window,
                                cx,
                            );
                        }
                    })
            }))
            .into_any_element();
    }

    let (text, links, highlights, fonts) = paragraph_text_style(paragraph, cx);
    if let Ok(mut inline_state) = paragraph.state.lock() {
        inline_state.set_text(text.into());
    }
    Inline::new(
        id.to_string(),
        view.clone(),
        paragraph.state.clone(),
        links,
        highlights,
        fonts,
    )
    .into_any_element()
}

fn paragraph_text_style(paragraph: &Paragraph, cx: &mut App) -> ParagraphTextStyle {
    let mut text = String::new();
    let mut links = Vec::new();
    let mut highlights = Vec::new();
    let mut fonts = Vec::new();
    for child in &paragraph.children {
        let offset = text.len();
        text.push_str(&child.text);
        let (node_links, node_highlights, node_fonts) = marks_for_node(&child.marks, offset, cx);
        links.extend(node_links);
        highlights = gpui::combine_highlights(highlights, node_highlights).collect();
        fonts.extend(node_fonts);
    }
    (text, links, highlights, fonts)
}

fn marks_for_node(marks: &[(Range<usize>, TextMark)], offset: usize, cx: &mut App) -> MarkRuns {
    let mut links = Vec::new();
    let mut highlights = Vec::new();
    let mut fonts = Vec::new();
    for (range, mark) in marks {
        let range = (offset + range.start)..(offset + range.end);
        let mut highlight = HighlightStyle::default();
        if mark.bold {
            highlight.font_weight = Some(FontWeight::BOLD);
        }
        if mark.italic {
            highlight.font_style = Some(FontStyle::Italic);
        }
        if mark.strikethrough {
            highlight.strikethrough = Some(gpui::StrikethroughStyle {
                thickness: px(1.),
                ..Default::default()
            });
        }
        if mark.code {
            highlight.background_color = Some(cx.theme().tokens.colors.muted);
            fonts.push((range.clone(), cx.theme().mono_font_family.clone()));
        }
        if let Some(link) = mark.link.clone() {
            highlight.color = Some(cx.theme().link);
            highlight.underline = Some(gpui::UnderlineStyle {
                thickness: px(1.),
                ..Default::default()
            });
            links.push((range.clone(), link));
        }
        highlights.push((range, highlight));
    }
    (links, highlights, fonts)
}

fn inline_flow_items(paragraph: &Paragraph, cx: &mut App) -> Vec<InlineFlowItem> {
    let mut items = Vec::new();
    let mut text = String::new();
    let mut links = Vec::new();
    let mut highlights = Vec::new();
    let mut fonts = Vec::new();
    let mut segment_state: Option<Arc<std::sync::Mutex<InlineState>>> = None;
    let flush_text =
        |items: &mut Vec<InlineFlowItem>,
         text: &mut String,
         links: &mut Vec<(Range<usize>, super::nodes::LinkMark)>,
         highlights: &mut Vec<(Range<usize>, HighlightStyle)>,
         fonts: &mut Vec<(Range<usize>, SharedString)>,
         segment_state: &mut Option<Arc<std::sync::Mutex<InlineState>>>| {
            if text.is_empty() {
                return;
            }
            let state = segment_state
                .take()
                .unwrap_or_else(|| paragraph.state.clone());
            if let Ok(mut inline_state) = state.lock() {
                inline_state.set_text(text.clone().into());
            }
            items.push(InlineFlowItem::Text {
                state,
                text: std::mem::take(text).into(),
                links: std::mem::take(links),
                highlights: std::mem::take(highlights),
                font_overrides: std::mem::take(fonts),
                code_style: None,
            });
        };
    for child in &paragraph.children {
        if let Some(link) = super::image_link::for_node(child) {
            flush_text(
                &mut items,
                &mut text,
                &mut links,
                &mut highlights,
                &mut fonts,
                &mut segment_state,
            );
            items.push(InlineFlowItem::ImageLink {
                url: link.url.clone(),
                label: child.text.clone(),
            });
            continue;
        }
        if let Some(image) = &child.image {
            flush_text(
                &mut items,
                &mut text,
                &mut links,
                &mut highlights,
                &mut fonts,
                &mut segment_state,
            );
            items.push(InlineFlowItem::Image {
                url: image.url.clone(),
                link: image.link.clone(),
                title: image.title(),
            });
            continue;
        }

        let is_code = child.marks.iter().any(|(_, mark)| mark.code);
        if is_code {
            flush_text(
                &mut items,
                &mut text,
                &mut links,
                &mut highlights,
                &mut fonts,
                &mut segment_state,
            );
            let (code_links, code_highlights, code_fonts) = marks_for_node(&child.marks, 0, cx);
            if let Ok(mut inline_state) = child.state.lock() {
                inline_state.set_text(child.text.clone());
            }
            items.push(InlineFlowItem::Text {
                state: child.state.clone(),
                text: child.text.clone(),
                links: code_links,
                highlights: code_highlights,
                font_overrides: code_fonts,
                code_style: Some(InlineCodeStyle {
                    font_family: cx.theme().mono_font_family.clone(),
                    font_size: INLINE_CODE_FONT_SIZE,
                    background: cx.theme().tokens.colors.muted,
                    radius: INLINE_CODE_RADIUS,
                }),
            });
            continue;
        }

        if text.is_empty() {
            segment_state = Some(child.state.clone());
        }
        let offset = text.len();
        text.push_str(&child.text);
        let (node_links, node_highlights, node_fonts) = marks_for_node(&child.marks, offset, cx);
        links.extend(node_links);
        highlights = gpui::combine_highlights(highlights, node_highlights).collect();
        fonts.extend(node_fonts);
    }
    flush_text(
        &mut items,
        &mut text,
        &mut links,
        &mut highlights,
        &mut fonts,
        &mut segment_state,
    );
    items
}

/// The id of the element rendering `span` of a block. Parts after the first
/// are its siblings in the root list, and a shared id would share their
/// element state.
fn span_id(path: &str, span: &Range<usize>) -> SharedString {
    if span.start == 0 {
        path.to_string().into()
    } else {
        format!("{path}@{}", span.start).into()
    }
}

#[allow(clippy::too_many_arguments)]
fn render_list(
    items: &[BlockNode],
    ordered: bool,
    start: u32,
    span: Range<usize>,
    options: RenderOptions,
    state: &Entity<MarkdownState>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let len = items.len();
    v_flex()
        .id(span_id(&options.path, &span))
        .w_full()
        .when(span.end == len, |list| list.pb(options.gap()))
        .children(items[span.clone()].iter().zip(span).map(|(item, ix)| {
            render_list_item(
                item,
                ix,
                RenderOptions {
                    ordered,
                    list_start: start,
                    is_last: ix + 1 == len,
                    path: format!("{}-{ix}", options.path),
                    ..options.clone()
                },
                state,
                window,
                cx,
            )
        }))
        .into_any_element()
}

fn render_list_item(
    item: &BlockNode,
    item_ix: usize,
    options: RenderOptions,
    state: &Entity<MarkdownState>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let BlockNode::ListItem {
        children,
        spread,
        checked,
        ..
    } = item
    else {
        return div().into_any_element();
    };
    let mut rows = Vec::new();
    for (ix, child) in children.iter().enumerate() {
        let child_options = RenderOptions {
            path: format!("{}-{ix}", options.path),
            in_list: true,
            depth: options.depth + 1,
            is_last: true,
            ..options.clone()
        };
        match child {
            BlockNode::Paragraph(_) if ix == 0 => {
                let content = div().flex_1().min_w_0().child(render_block(
                    child,
                    child_options,
                    state,
                    window,
                    cx,
                ));
                #[cfg(test)]
                let content = {
                    let path = options.path.clone();
                    content.debug_selector(move || format!("markdown-list-item-text-{path}"))
                };
                rows.push(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .items_start()
                        .when(checked.is_none(), |this| {
                            this.child(list_item_prefix(
                                item_ix,
                                options.ordered,
                                options.list_start,
                                options.depth,
                            ))
                        })
                        .when_some(*checked, |this, checked| {
                            this.child(
                                div()
                                    .mt(rems(0.35))
                                    .mr_1p5()
                                    .size(rems(0.875))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(3.))
                                    .border_1()
                                    .border_color(cx.theme().primary)
                                    .when(checked, |this| {
                                        this.bg(cx.theme().tokens.colors.primary)
                                            .text_color(cx.theme().primary_foreground)
                                            .text_xs()
                                            .child("✓")
                                    }),
                            )
                        })
                        .child(content),
                );
            }
            BlockNode::List { .. } => rows.push(div().ml(rems(1.)).child(render_block(
                child,
                child_options,
                state,
                window,
                cx,
            ))),
            _ => rows.push(div().w_full().pl(rems(1.25)).child(render_block(
                child,
                child_options,
                state,
                window,
                cx,
            ))),
        }
    }
    let item = v_flex()
        .id(options.path.clone())
        .w_full()
        .min_w_0()
        .when(*spread, |this| this.gap_2())
        .children(rows);
    #[cfg(test)]
    let item = item.debug_selector(move || format!("markdown-list-item-{}", options.path));
    item.into_any_element()
}

fn cached_highlights(code: &str, lang: &str, theme: &HighlightTheme) -> SharedHighlightRuns {
    let key = CodeCacheKey {
        code: code.to_string(),
        lang: lang.to_string(),
        theme: theme.clone(),
    };
    CODE_HIGHLIGHTS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(styles) = cache.entries.get(&key) {
            return styles.clone();
        }
        let styles = Arc::new(highlight::highlight_source(code, lang, theme));
        while cache.entries.len() >= CODE_CACHE_CAPACITY {
            let Some(oldest) = cache.order.pop_front() else {
                break;
            };
            cache.entries.remove(&oldest);
        }
        cache.order.push_back(key.clone());
        cache.entries.insert(key, styles.clone());
        styles
    })
}

/// Split code content into display lines. The fence's single terminating
/// newline is a delimiter, not content; genuine trailing blank lines survive
/// (`"a\n\n"` → `["a", ""]`).
fn code_lines(code: &str) -> Vec<&str> {
    let mut lines = code.split('\n').collect::<Vec<_>>();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines
}

fn render_code_block(
    code: &CodeBlock,
    span: Option<Range<usize>>,
    options: &RenderOptions,
    view: &Entity<MarkdownState>,
    cx: &mut App,
) -> AnyElement {
    let lang = code.lang.as_deref().unwrap_or("text");
    // Normalize CRLF so no stray `\r` is measured or painted, and drop the
    // fence's terminating newline (split would otherwise yield a phantom
    // blank last line). Highlight runs, line offsets, and selection states
    // are all derived from this same normalized text.
    let normalized;
    let code_text: &str = if code.code.contains('\r') {
        normalized = code.code.replace("\r\n", "\n");
        &normalized
    } else {
        &code.code
    };
    let all_runs = cached_highlights(code_text, lang, &cx.theme().highlight_theme);
    let lines = code_lines(code_text);
    let span = span.unwrap_or(0..lines.len());
    let (first, last) = (span.start == 0, span.end == lines.len());
    let states = code.states_for_lines(&lines, span.clone());
    let mut offset = lines[..span.start]
        .iter()
        .map(|line| line.len() + 1)
        .sum::<usize>();
    let span_end = offset
        + lines[span.clone()]
            .iter()
            .map(|line| line.len() + 1)
            .sum::<usize>();
    // Highlight runs are ordered and disjoint; only those in the span matter.
    let span_runs = &all_runs[all_runs.partition_point(|(range, _)| range.end <= offset)
        ..all_runs.partition_point(|(range, _)| range.start < span_end)];
    let mut rendered_lines = Vec::with_capacity(span.len());
    let mono_font_family = cx.theme().mono_font_family.clone();
    for ((line, line_state), ix) in lines[span.clone()].iter().zip(states).zip(span.clone()) {
        let end = offset + line.len();
        let runs = sub_runs(span_runs, offset, end);
        let line = div()
            .id(("code-line", ix))
            .min_h(px(18.))
            .whitespace_normal()
            .font_family(mono_font_family.clone())
            .text_size(INLINE_CODE_FONT_SIZE)
            .child(Inline::new(
                ix,
                view.clone(),
                line_state,
                Vec::new(),
                runs,
                Vec::new(),
            ));
        #[cfg(test)]
        let line = line.debug_selector(move || format!("markdown-code-line-{ix}"));
        rendered_lines.push(line);
        offset = end.saturating_add(1);
    }
    let radius = cx.theme().radius;
    div()
        .id(span_id(&options.path, &span))
        .when(last && !options.is_last, |block| block.pb(rems(1.)))
        .child(
            div()
                .px_3()
                .when(first, |block| block.pt_3().rounded_t(radius))
                .when(last, |block| block.pb_3().rounded_b(radius))
                .bg(cx.theme().tokens.colors.muted)
                .child(v_flex().w_full().children(rendered_lines)),
        )
        .into_any_element()
}

fn render_table(
    table: &Table,
    options: &RenderOptions,
    view: &Entity<MarkdownState>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let mut column_count = 0;
    for row in &table.children {
        column_count = column_count.max(row.children.len());
    }
    render_scroll_table(table, column_count, options, view, window, cx)
}

fn table_track_width(column_widths: &[f32]) -> f32 {
    // Column widths cover text and horizontal cell padding. GPUI lays each
    // vertical separator outside that flex basis, while the track's border-box
    // consumes its two outer borders. Include both so the tracked child bounds
    // contain every painted cell: N - 1 separators plus 2 outer borders.
    column_widths.iter().sum::<f32>()
        + TABLE_BORDER_PX * (column_widths.len().saturating_sub(1).saturating_add(2) as f32)
}

#[allow(clippy::too_many_arguments)]
fn render_scroll_table(
    table: &Table,
    column_count: usize,
    options: &RenderOptions,
    view: &Entity<MarkdownState>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    const CELL_PAD_PX: f32 = 16.;
    const CELL_MIN_PX: f32 = 48.;
    const COL_MIN_PX: f32 = 100.;

    let text_style = window.text_style();
    let font_size = text_style.font_size.to_pixels(window.rem_size());
    let mut widths = vec![CELL_MIN_PX; column_count];
    for (row_ix, row) in table.children.iter().enumerate() {
        for (ix, cell) in row.children.iter().enumerate() {
            let Some(slot) = widths.get_mut(ix) else {
                continue;
            };
            let mut width = Pixels::ZERO;
            let mut line_width = Pixels::ZERO;
            for child in &cell.children.children {
                let mut run_style = text_style.clone();
                if row_ix == 0 {
                    run_style.font_weight = FontWeight::SEMIBOLD;
                }
                for (_, mark) in &child.marks {
                    if mark.bold {
                        run_style.font_weight = FontWeight::BOLD;
                    }
                    if mark.italic {
                        run_style.font_style = FontStyle::Italic;
                    }
                    if mark.code {
                        run_style.font_family = cx.theme().mono_font_family.clone();
                    }
                }
                for (line_ix, fragment) in child.text.split('\n').enumerate() {
                    if line_ix > 0 {
                        width = width.max(line_width);
                        line_width = Pixels::ZERO;
                    }
                    if fragment.is_empty() {
                        continue;
                    }
                    let run = run_style.to_run(fragment.len());
                    line_width += window
                        .text_system()
                        .layout_line(fragment, font_size, &[run], None)
                        .width;
                }
            }
            width = width.max(line_width);
            *slot = slot.max(f32::from(width) + CELL_PAD_PX);
        }
    }
    let minimums = widths
        .iter()
        .map(|width| width.min(COL_MIN_PX))
        .collect::<Vec<_>>();
    let total_width = table_track_width(&minimums);
    let row_count = table.children.len();
    let rows = table
        .children
        .iter()
        .enumerate()
        .map(|(row_ix, row)| {
            div()
                .id(("table-row", row_ix))
                .w_full()
                .flex()
                .flex_row()
                .when(row_ix == 0, |this| {
                    this.bg(cx.theme().tokens.colors.muted)
                        .font_weight(FontWeight::SEMIBOLD)
                })
                .when(row_ix + 1 < row_count, |this| {
                    this.border_b_1().border_color(cx.theme().border)
                })
                .children(row.children.iter().enumerate().map(|(ix, cell)| {
                    let align = table.column_align(ix);
                    let width = widths.get(ix).copied().unwrap_or(CELL_MIN_PX);
                    let minimum = minimums.get(ix).copied().unwrap_or(CELL_MIN_PX);
                    let cell_content = div().min_w_0().child(render_paragraph(
                        &cell.children,
                        &format!("{}-{row_ix}-{ix}", options.path),
                        view,
                        cx,
                    ));
                    #[cfg(test)]
                    let cell_content = cell_content.debug_selector(move || {
                        format!("markdown-table-cell-content-{row_ix}-{ix}")
                    });
                    let rendered_cell = div()
                        .id(("table-cell", ix))
                        .flex_basis(px(width))
                        .flex_grow(width)
                        .min_w(px(minimum))
                        .whitespace_normal()
                        .flex()
                        .px_2()
                        .py_1()
                        .when(align == ColumnumnAlign::Center, |this| this.text_center())
                        .when(align == ColumnumnAlign::Right, |this| this.text_right())
                        .when(align == ColumnumnAlign::Center, |this| {
                            this.justify_center()
                        })
                        .when(align == ColumnumnAlign::Right, |this| this.justify_end())
                        .when(ix + 1 < row.children.len(), |this| {
                            this.border_r_1().border_color(cx.theme().border)
                        })
                        .child(cell_content);
                    #[cfg(test)]
                    let rendered_cell = rendered_cell
                        .debug_selector(move || format!("markdown-table-cell-{row_ix}-{ix}"));
                    rendered_cell
                }))
        })
        .collect::<Vec<_>>();
    let track = v_flex()
        .min_w_full()
        .w(px(total_width))
        .border_1()
        .border_color(cx.theme().border)
        .rounded(cx.theme().radius)
        .overflow_hidden()
        .children(rows);
    #[cfg(test)]
    let track = {
        let selector = format!("markdown-table-track-{}", options.path);
        track.debug_selector(move || selector.clone())
    };
    div()
        .id(options.path.clone())
        .w_full()
        .pb(if options.is_last { rems(0.) } else { rems(1.) })
        .child(
            div()
                .id(SharedString::from(format!("{}-viewport", options.path)))
                .w_full()
                .overflow_x_scroll_area()
                .child(track),
        )
        .into_any_element()
}
