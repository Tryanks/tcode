//! Block renderer adapted from gpui-component's Apache-2.0 `text/node.rs` and
//! `text/document.rs`, with rushdown IR and syntect highlighting.

use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    ops::Range,
    sync::Arc,
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, Element, ElementId, Entity, FontStyle, FontWeight,
    GlobalElementId, HighlightStyle, InspectorElementId, InteractiveElement as _, IntoElement,
    LayoutId, ListSizingBehavior, ListState, ObjectFit, ParentElement as _, Pixels, ScrollHandle,
    ScrollWheelEvent, SharedString, StatefulInteractiveElement as _, Style, StyleRefinement,
    Styled as _, StyledImage as _, TouchPhase, Window, div, img, prelude::FluentBuilder as _, px,
    relative, rems, size,
};
use gpui_component::{
    ActiveTheme as _, StyledExt as _, h_flex, highlighter::HighlightTheme, tooltip::Tooltip, v_flex,
};

use crate::highlight;

use super::{
    inline::{Inline, InlineState},
    inline_flow::{InlineCodeStyle, InlineFlow, InlineFlowItem},
    nodes::{BlockNode, CodeBlock, ColumnumnAlign, Paragraph, Table, TextMark},
    state::MarkdownState,
    style::TextViewStyle,
    utils::list_item_prefix,
    window_selection,
};

const CODE_CACHE_CAPACITY: usize = 64;
const BLOCK_OVERDRAW: Pixels = px(300.);
const SCROLL_GESTURE_TIMEOUT: Duration = Duration::from_millis(250);
const TABLE_BORDER_PX: f32 = 1.;
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

#[derive(Clone, Hash, PartialEq, Eq)]
struct CodeWidthCacheKey {
    code: String,
    font_size_bits: u32,
    font_family: SharedString,
}

#[derive(Default)]
struct CodeWidthCache {
    entries: HashMap<CodeWidthCacheKey, Pixels>,
    order: VecDeque<CodeWidthCacheKey>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScrollGestureAxis {
    Horizontal,
    Vertical,
}

#[derive(Default)]
struct HorizontalScrollState {
    scroll: ScrollHandle,
    locked_axis: Option<ScrollGestureAxis>,
    last_event_at: Option<Instant>,
}

impl HorizontalScrollState {
    fn route_gesture(
        &mut self,
        delta_x: f32,
        delta_y: f32,
        phase: TouchPhase,
        now: Instant,
    ) -> ScrollGestureAxis {
        let continuous = self
            .last_event_at
            .is_some_and(|last| now.saturating_duration_since(last) <= SCROLL_GESTURE_TIMEOUT);
        if phase == TouchPhase::Started || !continuous || self.locked_axis.is_none() {
            self.locked_axis = Some(dominant_scroll_axis(delta_x, delta_y));
        }

        let axis = self.locked_axis.unwrap_or(ScrollGestureAxis::Vertical);
        if matches!(phase, TouchPhase::Ended | TouchPhase::Cancelled) {
            self.locked_axis = None;
            self.last_event_at = None;
        } else {
            self.last_event_at = Some(now);
        }
        axis
    }
}

fn dominant_scroll_axis(delta_x: f32, delta_y: f32) -> ScrollGestureAxis {
    if delta_x.abs() > delta_y.abs() {
        ScrollGestureAxis::Horizontal
    } else {
        ScrollGestureAxis::Vertical
    }
}

thread_local! {
    static CODE_HIGHLIGHTS: RefCell<CodeHighlightCache> = RefCell::new(CodeHighlightCache::default());
    static CODE_WIDTHS: RefCell<CodeWidthCache> = RefCell::new(CodeWidthCache::default());
}

#[derive(Clone)]
struct RenderOptions {
    path: String,
    in_list: bool,
    ordered: bool,
    depth: usize,
    is_last: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            path: "root".to_string(),
            in_list: false,
            ordered: false,
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
}

pub(super) struct RootMeasurements {
    pub(super) width: Pixels,
    pub(super) content_height: Option<Pixels>,
}

pub(super) fn render_root(
    node: &BlockNode,
    list_state: ListState,
    measurements: RootMeasurements,
    state: &Entity<MarkdownState>,
    style: &TextViewStyle,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let BlockNode::Root { children, .. } = node else {
        return render_block(node, RenderOptions::default(), state, style, window, cx);
    };
    if list_state.item_count() != children.len() {
        list_state.reset(children.len());
    }

    if let Some(content_height) = measurements.content_height {
        return div()
            .id("root")
            .w_full()
            .child(VirtualizedBlockList {
                blocks: children.clone(),
                list_state,
                content_height,
                state: state.clone(),
                style: style.clone(),
            })
            .into_any_element();
    }

    let blocks = children.clone();
    let state = state.clone();
    let style = style.clone();
    // `Infer` asks for min-content width during request-layout. Pin the list and
    // its blocks to the last real parent width so that intrinsic probes cannot
    // invalidate ListState at a narrow width while this frame measures height.
    let list = gpui::list(list_state, move |ix, window, cx| {
        render_root_block(&blocks, ix, measurements.width, &state, &style, window, cx)
    })
    .with_sizing_behavior(ListSizingBehavior::Infer)
    .w_full()
    .when(measurements.width > px(0.), |list| {
        list.w(measurements.width)
    });
    div().id("root").w_full().child(list).into_any_element()
}

fn render_root_block(
    blocks: &[BlockNode],
    ix: usize,
    width: Pixels,
    state: &Entity<MarkdownState>,
    style: &TextViewStyle,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let is_last = ix + 1 == blocks.len();
    let block = div()
        .w_full()
        .when(width > px(0.), |block| block.w(width))
        .child(render_block(
            &blocks[ix],
            RenderOptions::default().child(ix, is_last),
            state,
            style,
            window,
            cx,
        ));
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
    blocks: Vec<BlockNode>,
    list_state: ListState,
    content_height: Pixels,
    state: Entity<MarkdownState>,
    style: TextViewStyle,
}

impl IntoElement for VirtualizedBlockList {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for VirtualizedBlockList {
    type RequestLayoutState = ();
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
        _: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let content_height = self.content_height;
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
        (layout_id, ())
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

        let blocks = self.blocks.clone();
        let state = self.state.clone();
        let style = self.style.clone();
        let width = viewport.size.width;
        let mut list = gpui::list(self.list_state.clone(), move |ix, window, cx| {
            render_root_block(&blocks, ix, width, &state, &style, window, cx)
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
    style: &TextViewStyle,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let gap = if options.in_list || options.is_last {
        rems(0.)
    } else {
        style.paragraph_gap
    };
    match node {
        BlockNode::Root { children, .. } => {
            let len = children.len();
            v_flex()
                .id(options.path.clone())
                .w_full()
                .children(children.iter().enumerate().map(|(ix, child)| {
                    render_block(
                        child,
                        options.child(ix, ix + 1 == len),
                        state,
                        style,
                        window,
                        cx,
                    )
                }))
                .into_any_element()
        }
        BlockNode::Paragraph(paragraph) => div()
            .id(options.path.clone())
            .w_full()
            .pb(gap)
            .whitespace_normal()
            .child(render_paragraph(paragraph, &options.path, state, style, cx))
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
            let mut size = style.heading_base_font_size * scale;
            if let Some(resolve) = &style.heading_font_size {
                size = resolve(*level, style.heading_base_font_size);
            }
            div()
                .id(options.path.clone())
                .pb(rems(0.3))
                .whitespace_normal()
                .text_size(size)
                .font_weight(weight)
                .child(render_paragraph(children, &options.path, state, style, cx))
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
                            render_block(
                                child,
                                options.child(ix, ix + 1 == len),
                                state,
                                style,
                                window,
                                cx,
                            )
                        })),
                )
                .into_any_element()
        }
        BlockNode::List {
            children, ordered, ..
        } => {
            let len = children.len();
            v_flex()
                .id(options.path.clone())
                .w_full()
                .pb(gap)
                .children(children.iter().enumerate().map(|(ix, item)| {
                    render_list_item(
                        item,
                        ix,
                        RenderOptions {
                            ordered: *ordered,
                            is_last: ix + 1 == len,
                            path: format!("{}-{ix}", options.path),
                            ..options.clone()
                        },
                        state,
                        style,
                        window,
                        cx,
                    )
                }))
                .into_any_element()
        }
        BlockNode::ListItem { .. } => render_list_item(node, 0, options, state, style, window, cx),
        BlockNode::CodeBlock(code) => render_code_block(code, &options, state, style, window, cx),
        BlockNode::Table(table) => render_table(table, &options, state, style, window, cx),
        BlockNode::HorizontalRule { .. } => div()
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
    style: &TextViewStyle,
    cx: &mut App,
) -> AnyElement {
    let has_image = paragraph.children.iter().any(|child| child.image.is_some());
    let has_text = paragraph
        .children
        .iter()
        .any(|child| !child.text.is_empty());
    if has_image && has_text {
        return InlineFlow::new(
            id.to_string(),
            view.clone(),
            inline_flow_items(paragraph, style, cx),
        )
        .into_any_element();
    }
    if has_image {
        let images = paragraph
            .children
            .iter()
            .filter_map(|child| child.image.as_ref());
        return h_flex()
            .id(id.to_string())
            .flex_wrap()
            .gap_1()
            .children(images.enumerate().map(|(ix, image)| {
                let title = image.title();
                img(image.url.clone())
                    .id(ix)
                    .object_fit(ObjectFit::Contain)
                    .max_w(relative(1.))
                    .min_w(px(15.))
                    .min_h(px(15.))
                    .when_some(image.width, |this, width| this.w(width))
                    .when_some(image.height, |this, height| this.h(height))
                    .when_some(image.link.clone(), |this, link| {
                        let title = title.clone();
                        this.cursor_pointer()
                            .tooltip(move |window, cx| {
                                Tooltip::new(title.clone()).build(window, cx)
                            })
                            .on_click(move |_, window, cx| {
                                window_selection::finish_drag(window, cx);
                                cx.stop_propagation();
                                cx.open_url(&link.url);
                            })
                    })
            }))
            .into_any_element();
    }

    let (text, links, highlights, fonts) = paragraph_text_style(paragraph, style, cx);
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

fn paragraph_text_style(
    paragraph: &Paragraph,
    style: &TextViewStyle,
    cx: &mut App,
) -> ParagraphTextStyle {
    let mut text = String::new();
    let mut links = Vec::new();
    let mut highlights = Vec::new();
    let mut fonts = Vec::new();
    for child in &paragraph.children {
        let offset = text.len();
        text.push_str(&child.text);
        let (node_links, node_highlights, node_fonts) =
            marks_for_node(&child.marks, offset, style, cx);
        links.extend(node_links);
        highlights = gpui::combine_highlights(highlights, node_highlights).collect();
        fonts.extend(node_fonts);
    }
    (text, links, highlights, fonts)
}

fn marks_for_node(
    marks: &[(Range<usize>, TextMark)],
    offset: usize,
    _style: &TextViewStyle,
    cx: &mut App,
) -> MarkRuns {
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
        if mark.underline {
            highlight.underline = Some(gpui::UnderlineStyle {
                thickness: px(1.),
                ..Default::default()
            });
        }
        if mark.code {
            highlight.background_color = Some(*cx.theme().tokens.muted);
            fonts.push((range.clone(), cx.theme().mono_font_family.clone()));
        }
        if let Some(color) = mark.highlight {
            highlight.background_color = Some(color);
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

fn inline_flow_items(
    paragraph: &Paragraph,
    style: &TextViewStyle,
    cx: &mut App,
) -> Vec<InlineFlowItem> {
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
                width: image.width,
                height: image.height,
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
            let (code_links, code_highlights, code_fonts) =
                marks_for_node(&child.marks, 0, style, cx);
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
                    font_size: style.inline_code_font_size,
                    background: *cx.theme().tokens.muted,
                    radius: style.inline_code_radius,
                }),
            });
            continue;
        }

        if text.is_empty() {
            segment_state = Some(child.state.clone());
        }
        let offset = text.len();
        text.push_str(&child.text);
        let (node_links, node_highlights, node_fonts) =
            marks_for_node(&child.marks, offset, style, cx);
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

fn render_list_item(
    item: &BlockNode,
    item_ix: usize,
    options: RenderOptions,
    state: &Entity<MarkdownState>,
    style: &TextViewStyle,
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
                let content = render_block(child, child_options, state, style, window, cx);
                rows.push(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .items_start()
                        .when(checked.is_none(), |this| {
                            this.child(list_item_prefix(item_ix, options.ordered, options.depth))
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
                                        this.bg(cx.theme().tokens.primary)
                                            .text_color(cx.theme().primary_foreground)
                                            .text_xs()
                                            .child("✓")
                                    }),
                            )
                        })
                        .child(div().flex_1().min_w_0().child(content)),
                );
            }
            BlockNode::List { .. } => rows.push(div().ml(rems(1.)).child(render_block(
                child,
                child_options,
                state,
                style,
                window,
                cx,
            ))),
            _ => rows.push(div().w_full().pl(rems(1.25)).child(render_block(
                child,
                child_options,
                state,
                style,
                window,
                cx,
            ))),
        }
    }
    v_flex()
        .id(options.path)
        .w_full()
        .min_w_0()
        .when(*spread, |this| this.gap_2())
        .children(rows)
        .into_any_element()
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

fn cached_code_width(
    code: &str,
    lines: &[&str],
    font_size: Pixels,
    font_family: &SharedString,
    window: &mut Window,
) -> Pixels {
    let key = CodeWidthCacheKey {
        code: code.to_string(),
        font_size_bits: f32::from(font_size).to_bits(),
        font_family: font_family.clone(),
    };
    CODE_WIDTHS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(width) = cache.entries.get(&key) {
            return *width;
        }

        let mut text_style = window.text_style();
        text_style.font_family = font_family.clone();
        text_style.font_size = gpui::AbsoluteLength::Pixels(font_size);
        let max_width = lines.iter().fold(px(0.), |max_width, line| {
            if line.is_empty() {
                max_width
            } else {
                let width = window
                    .text_system()
                    .layout_line(line, font_size, &[text_style.to_run(line.len())], None)
                    .width;
                max_width.max(width)
            }
        });
        while cache.entries.len() >= CODE_CACHE_CAPACITY {
            let Some(oldest) = cache.order.pop_front() else {
                break;
            };
            cache.entries.remove(&oldest);
        }
        cache.order.push_back(key.clone());
        cache.entries.insert(key, max_width);
        max_width
    })
}

fn sub_runs(
    runs: &[(Range<usize>, HighlightStyle)],
    start: usize,
    end: usize,
) -> Vec<(Range<usize>, HighlightStyle)> {
    runs.iter()
        .filter(|(range, _)| range.start < end && range.end > start)
        .map(|(range, style)| {
            let clipped_start = range.start.max(start) - start;
            let clipped_end = range.end.min(end) - start;
            (clipped_start..clipped_end, *style)
        })
        .collect()
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
    options: &RenderOptions,
    view: &Entity<MarkdownState>,
    style: &TextViewStyle,
    window: &mut Window,
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
    let states = code.states_for_lines(&lines);
    let mut offset = 0;
    let mut rendered_lines = Vec::with_capacity(lines.len());
    let mono_font_family = cx.theme().mono_font_family.clone();
    let max_width = cached_code_width(
        code_text,
        &lines,
        style.inline_code_font_size,
        &mono_font_family,
        window,
    );
    for (ix, (line, line_state)) in lines.iter().zip(states).enumerate() {
        let end = offset + line.len();
        let runs = sub_runs(&all_runs, offset, end);
        rendered_lines.push(
            div()
                .id(("code-line", ix))
                .min_h(px(18.))
                .whitespace_nowrap()
                .font_family(mono_font_family.clone())
                .text_size(style.inline_code_font_size)
                .child(Inline::new(
                    ix,
                    view.clone(),
                    line_state,
                    Vec::new(),
                    runs,
                    Vec::new(),
                )),
        );
        offset = end.saturating_add(1);
    }
    let scroll_key: SharedString =
        format!("markdown-code-scroll-{}-{}", view.entity_id(), options.path).into();
    let scroll = window.use_keyed_state(scroll_key, cx, |_, _| HorizontalScrollState::default());
    let track = v_flex()
        .min_w_full()
        .w(max_width + px(24.))
        .children(rendered_lines);
    div()
        .id(options.path.clone())
        .pb(if options.is_last {
            rems(0.)
        } else {
            style.paragraph_gap
        })
        .child(horizontal_scroll_area(
            format!("{}-viewport", options.path),
            &scroll,
            &StyleRefinement::default(),
            cx,
            div()
                .p_3()
                .rounded(cx.theme().radius)
                .bg(cx.theme().tokens.muted)
                .refine_style(&style.code_block)
                .child(track),
        ))
        .into_any_element()
}

fn render_table(
    table: &Table,
    options: &RenderOptions,
    view: &Entity<MarkdownState>,
    style: &TextViewStyle,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let mut column_count = 0;
    for row in &table.children {
        column_count = column_count.max(row.children.len());
    }
    render_scroll_table(table, column_count, options, view, style, window, cx)
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
    style: &TextViewStyle,
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
                    this.bg(cx.theme().tokens.muted)
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
                        style,
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
                        .refine_style(&style.table_cell)
                        .child(cell_content);
                    #[cfg(test)]
                    let rendered_cell = rendered_cell
                        .debug_selector(move || format!("markdown-table-cell-{row_ix}-{ix}"));
                    rendered_cell
                }))
        })
        .collect::<Vec<_>>();
    let scroll_key: SharedString = format!(
        "markdown-table-scroll-{}-{}",
        view.entity_id(),
        options.path
    )
    .into();
    let scroll = window.use_keyed_state(scroll_key, cx, |_, _| HorizontalScrollState::default());
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
        .pb(if options.is_last {
            rems(0.)
        } else {
            style.paragraph_gap
        })
        .child(horizontal_scroll_area(
            format!("{}-viewport", options.path),
            &scroll,
            &style.table,
            cx,
            track,
        ))
        .into_any_element()
}

fn horizontal_scroll_area(
    id: impl Into<gpui::ElementId>,
    state: &Entity<HorizontalScrollState>,
    style: &StyleRefinement,
    cx: &App,
    child: impl IntoElement,
) -> impl IntoElement {
    let scroll = state.read(cx).scroll.clone();
    let state = state.clone();
    div()
        .id(id)
        .w_full()
        .relative()
        .refine_style(style)
        .overflow_hidden()
        .track_scroll(&scroll)
        .child(child)
        .on_scroll_wheel(move |event: &ScrollWheelEvent, window, cx| {
            let delta = event.delta.pixel_delta(window.line_height());
            let can_scroll_horizontally = state.read(cx).scroll.max_offset().x > Pixels::ZERO;
            if !can_scroll_horizontally {
                return;
            }

            let axis = state.update(cx, |state, cx| {
                let axis = state.route_gesture(
                    f32::from(delta.x),
                    f32::from(delta.y),
                    event.touch_phase,
                    Instant::now(),
                );
                if axis == ScrollGestureAxis::Horizontal {
                    let mut offset = state.scroll.offset();
                    offset.x += delta.x;
                    if offset != state.scroll.offset() {
                        state.scroll.set_offset(offset);
                        cx.notify();
                    }
                }
                axis
            });
            if axis == ScrollGestureAxis::Horizontal {
                cx.stop_propagation();
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clips_and_rebases_code_runs() {
        let style = HighlightStyle::default();
        let runs = vec![(0..5, style), (5..12, style)];
        assert_eq!(sub_runs(&runs, 4, 10), vec![(0..1, style), (1..6, style)]);
    }

    #[test]
    fn code_lines_drops_only_the_terminating_newline() {
        assert_eq!(code_lines("fn main() {}\n"), ["fn main() {}"]);
        assert_eq!(code_lines("a\nb"), ["a", "b"]);
        // A genuine trailing blank line survives.
        assert_eq!(code_lines("a\n\n"), ["a", ""]);
        // Empty code renders zero lines rather than one phantom.
        assert_eq!(code_lines(""), Vec::<&str>::new());
    }

    #[test]
    fn table_track_width_includes_cell_and_outer_borders() {
        assert_eq!(table_track_width(&[48., 72., 96.]), 220.);
        assert_eq!(table_track_width(&[]), 2.);
    }

    #[test]
    fn scroll_axis_uses_strict_horizontal_dominance() {
        assert_eq!(dominant_scroll_axis(8., 3.), ScrollGestureAxis::Horizontal);
        assert_eq!(dominant_scroll_axis(3., 8.), ScrollGestureAxis::Vertical);
        assert_eq!(dominant_scroll_axis(8., 8.), ScrollGestureAxis::Vertical);
    }

    #[test]
    fn scroll_axis_sticks_for_a_continuous_gesture() {
        let start = Instant::now();
        let mut state = HorizontalScrollState::default();
        assert_eq!(
            state.route_gesture(12., 4., TouchPhase::Started, start),
            ScrollGestureAxis::Horizontal
        );
        assert_eq!(
            state.route_gesture(2., 8., TouchPhase::Moved, start + Duration::from_millis(50)),
            ScrollGestureAxis::Horizontal
        );

        let mut state = HorizontalScrollState::default();
        assert_eq!(
            state.route_gesture(4., 12., TouchPhase::Started, start),
            ScrollGestureAxis::Vertical
        );
        assert_eq!(
            state.route_gesture(8., 2., TouchPhase::Moved, start + Duration::from_millis(50)),
            ScrollGestureAxis::Vertical
        );
    }

    #[test]
    fn scroll_axis_unlocks_after_timeout_or_end() {
        let start = Instant::now();
        let mut state = HorizontalScrollState::default();
        state.route_gesture(12., 4., TouchPhase::Moved, start);
        assert_eq!(
            state.route_gesture(
                2.,
                8.,
                TouchPhase::Moved,
                start + Duration::from_millis(251)
            ),
            ScrollGestureAxis::Vertical
        );

        state.route_gesture(
            2.,
            8.,
            TouchPhase::Ended,
            start + Duration::from_millis(252),
        );
        assert_eq!(
            state.route_gesture(
                8.,
                2.,
                TouchPhase::Moved,
                start + Duration::from_millis(253)
            ),
            ScrollGestureAxis::Horizontal
        );
    }
}
