//! Builds a frame's GPUI elements from a parsed [`Document`] and its
//! [`Computed`] styles.

use std::collections::HashSet;
use std::ops::Range;

use gpui::{
    AbsoluteLength, AlignItems, AnyElement, App, AvailableSpace, DefiniteLength, HighlightStyle,
    Hsla, InteractiveElement as _, IntoElement as _, Length, ObjectFit, Overflow,
    ParentElement as _, Position, StatefulInteractiveElement as _, StrikethroughStyle, Styled as _,
    StyledImage as _, TextAlign, UnderlineStyle, Window, div, img, linear_color_stop,
    linear_gradient, prelude::FluentBuilder as _, px, relative, size,
};
use gpui_base::{h_flex, v_flex};

use super::dom::{Document, ElementData, MAX_DEPTH, MAX_NODES, NodeId, NodeKind};
use super::style::{
    ComputedStyle, DisplayKind, GridColumns, GridTrack, ListStyleType, TextTransform,
    VerticalAlign, WhiteSpaceMode,
};
use super::svg::{MediaSlots, RasterKey, RasterState};
use super::text::{PageText, TextSink};
use super::{Computed, HtmlState, PictureOf};
use crate::scroll::ScrollableElement as _;
use crate::theme::{ActiveTheme as _, Theme};
use crate::typography::TextRole as _;
use crate::widgets::checkbox::Checkbox;
use crate::widgets::progress::Progress;
use crate::widgets::tooltip::{Tooltip, TooltipTrigger as _};

/// HTML's own limit on `colspan`.
const MAX_COLSPAN: usize = 1000;
/// The most columns a table is laid out in.
const MAX_COLUMNS: usize = 256;
/// The most boxes the text of a page's lines is cut into, a box for a word.
/// Past it a text is one box, which wraps inside itself and not around the
/// boxes beside it.
const MAX_WORDS: usize = 20_000;
/// The most cells whose widths are measured for a page: each is laid out
/// once more than it is drawn.
const MAX_MEASURED: usize = 4000;

/// A picture a frame had no drawing of.
pub(crate) enum Wanted {
    Svg {
        node: NodeId,
        key: RasterKey,
        scale: f32,
    },
    DataImage {
        node: NodeId,
        key: RasterKey,
    },
}

/// What a frame's elements need of the state once they are built.
#[derive(Default)]
pub(crate) struct Frame {
    pub(crate) wanted: Vec<Wanted>,
    /// Every drawing the frame shows or waits for.
    pub(crate) used: HashSet<RasterKey>,
    /// The text the frame draws.
    pub(crate) text: PageText,
}

struct RenderCtx<'a> {
    doc: &'a Document,
    computed: &'a Computed,
    media: &'a MediaSlots,
    theme: &'a Theme,
    viewport_width: f32,
    frame: Frame,
    text: TextSink,
    /// The next box built fills the height of the grid area it is put in.
    stretch: bool,
    /// The next box built is in a column that does not stretch it across,
    /// where GPUI makes it as wide as what it holds: it is no wider than
    /// the column.
    capped: bool,
    /// The boxes the text of lines has been cut into, of [`MAX_WORDS`].
    words: usize,
}

pub(crate) fn render_document(
    state: &HtmlState,
    computed: &Computed,
    viewport_width: f32,
    cx: &App,
) -> (AnyElement, Frame) {
    let doc = &state.doc;
    let root_style = &computed.root;
    let mut ctx = RenderCtx {
        doc,
        computed,
        media: &state.media,
        theme: cx.theme(),
        viewport_width,
        frame: Frame::default(),
        text: TextSink::selecting(state.selecting()),
        stretch: false,
        capped: false,
        words: 0,
    };

    let children = render_children(&doc.roots, root_style, None, &mut ctx, cx);
    let root = v_flex()
        .w_full()
        .min_w_0()
        .text_color(root_style.color)
        .font_family(root_style.font_family.clone())
        .text_size(px(root_style.font_size))
        .children(children)
        .children(left_out_notes(state, computed, cx));

    ctx.frame.text = ctx.text.text;
    (root.into_any_element(), ctx.frame)
}

/// What the visual was written with and is drawn without, said under it.
fn left_out_notes(state: &HtmlState, computed: &Computed, cx: &App) -> Vec<AnyElement> {
    let notes = [
        (
            state.doc.left_out,
            "html-left-out-nodes",
            crate::tr!("html.left_out_nodes", count = MAX_NODES),
        ),
        (
            state.doc.flattened,
            "html-left-out-depth",
            crate::tr!("html.left_out_depth", count = MAX_DEPTH),
        ),
        (
            state.sheet.left_out || computed.left_out,
            "html-left-out-styles",
            crate::tr!("html.left_out_styles"),
        ),
    ];
    notes
        .into_iter()
        .filter(|(said, _, _)| *said)
        .map(|(_, selector, note)| {
            div()
                .mt_2()
                .text_secondary(false, cx)
                .child(note.into_owned())
                .debug_selector(|| selector.to_owned())
                .into_any_element()
        })
        .collect()
}

#[derive(Clone, Copy)]
struct ListContext {
    ordered: bool,
    index: usize,
    style: ListStyleType,
}

fn render_children(
    child_ids: &[NodeId],
    parent_style: &ComputedStyle,
    list_ctx: Option<ListContext>,
    ctx: &mut RenderCtx<'_>,
    cx: &App,
) -> Vec<AnyElement> {
    let computed = ctx.computed;
    let mut out = Vec::new();
    let mut inline_run: Vec<NodeId> = Vec::new();
    let mut list_counter = list_ctx.map_or(1, |l| l.index);

    let is_grid = matches!(
        parent_style.display,
        DisplayKind::Grid | DisplayKind::InlineGrid
    );
    let is_flex = matches!(
        parent_style.display,
        DisplayKind::Flex | DisplayKind::InlineFlex
    );
    let is_flex_or_grid = is_flex || is_grid;
    // A row that sets its items on one baseline: GPUI knows no box's
    // baseline and sets them on their bottom edges, which is right between
    // lines of text and puts a box with no text below the line.
    let in_column = is_flex
        && matches!(
            parent_style.flex_direction,
            Some(gpui::FlexDirection::Column | gpui::FlexDirection::ColumnReverse)
        );
    let on_baseline =
        is_flex && !in_column && parent_style.align_items == Some(AlignItems::Baseline);
    // A grid sets each item across its column; GPUI's grid has only the
    // setting down its row, so a box around the item sets it across.
    let across = |item: AnyElement| match parent_style.justify_items.filter(|_| is_grid) {
        Some(AlignItems::Center) => div()
            .flex()
            .min_w_0()
            .justify_center()
            .child(item)
            .into_any_element(),
        Some(AlignItems::FlexEnd) => div()
            .flex()
            .min_w_0()
            .justify_end()
            .child(item)
            .into_any_element(),
        Some(AlignItems::FlexStart) => div().flex().min_w_0().child(item).into_any_element(),
        _ => item,
    };

    for &cid in child_ids {
        match &ctx.doc.nodes[cid].kind {
            NodeKind::Text(t) => {
                if is_flex_or_grid {
                    // White space around the text of an item is not part of it.
                    let normalized = normalize_text(t, parent_style.white_space);
                    let text = normalized.trim();
                    if !text.is_empty() {
                        let transformed = apply_text_transform(text, parent_style.text_transform);
                        let mut item = text_box(&transformed, parent_style.font_size);
                        if parent_style.white_space == WhiteSpaceMode::NoWrap {
                            item = item.whitespace_nowrap();
                        }
                        let item = item.child(ctx.text.run(transformed, Vec::new()));
                        out.push(across(item.into_any_element()));
                    }
                } else {
                    inline_run.push(cid);
                }
            }
            NodeKind::Element(el) => {
                let Some(style) = computed.shown(cid) else {
                    continue;
                };
                if !is_flex_or_grid && (style.display.is_inline_like() || el.tag == "br") {
                    inline_run.push(cid);
                    continue;
                }
                if !inline_run.is_empty() {
                    if let Some(el) = render_inline_run(&inline_run, parent_style, ctx, cx) {
                        out.push(el);
                    }
                    inline_run.clear();
                }
                let item_list = list_ctx.map(|mut l| {
                    l.index = list_counter;
                    if el.tag == "li" {
                        list_counter = list_counter.saturating_add(1);
                    }
                    l
                });
                let held_across = style.align_self.or(parent_style.align_items);
                ctx.capped =
                    in_column && held_across.is_some_and(|across| across != AlignItems::Stretch);
                if let Some(rendered) = render_element(cid, el, style, item_list, ctx, cx) {
                    let lifted = on_baseline
                        && style.position != Position::Absolute
                        && !has_text(ctx.doc, cid);
                    out.push(if lifted {
                        div()
                            .flex_none()
                            .relative()
                            .top(px(-baseline_from_bottom(parent_style)))
                            .child(rendered)
                            .into_any_element()
                    } else if style.position == Position::Absolute {
                        rendered
                    } else {
                        across(rendered)
                    });
                }
            }
        }
    }

    if !inline_run.is_empty()
        && let Some(el) = render_inline_run(&inline_run, parent_style, ctx, cx)
    {
        out.push(el);
    }
    out
}

/// Whether the element at `id` may be made narrower than what it holds: it
/// clips what it holds, or holds text that wraps. GPUI takes a text's
/// narrowest width to be its whole line, so a box of text that wraps is
/// told it may be narrower; a box of none, or of text that does not wrap,
/// is as wide as what it holds at the least, as a browser has it.
fn narrows(doc: &Document, computed: &Computed, id: NodeId, style: &ComputedStyle) -> bool {
    if matches!(style.overflow_x, Some(Overflow::Hidden | Overflow::Scroll)) {
        return true;
    }
    let wraps =
        style.white_space != WhiteSpaceMode::NoWrap && style.white_space != WhiteSpaceMode::Pre;
    doc.nodes[id]
        .children
        .iter()
        .any(|&child| match &doc.nodes[child].kind {
            NodeKind::Text(text) => wraps && can_wrap(text),
            NodeKind::Element(_) => computed
                .shown(child)
                .is_some_and(|style| narrows(doc, computed, child, style)),
        })
}

/// Whether the element at `id` holds any text that is drawn.
fn has_text(doc: &Document, id: NodeId) -> bool {
    doc.nodes[id]
        .children
        .iter()
        .any(|&child| match &doc.nodes[child].kind {
            NodeKind::Text(text) => !text.trim().is_empty(),
            NodeKind::Element(_) => has_text(doc, child),
        })
}

/// How far above the bottom of a line of `style`'s text its baseline is,
/// reckoned from the shape of the interface font: GPUI gives a box no
/// baseline to set another box by.
fn baseline_from_bottom(style: &ComputedStyle) -> f32 {
    let size = style.font_size;
    let line = match style.line_height {
        Some(DefiniteLength::Fraction(share)) => share * size,
        Some(DefiniteLength::Absolute(AbsoluteLength::Pixels(pixels))) => f32::from(pixels),
        _ => 1.5 * size,
    };
    (line - 1.19 * size) / 2.0 + 0.24 * size
}

/// What a box is drawn with that GPUI's own background and border cannot
/// draw: a gradient of more than two stops, and borders of other colours
/// than the box's. Each is a box laid over the box's own.
fn decorations(style: &ComputedStyle) -> Vec<AnyElement> {
    let mut out = Vec::new();
    if let Some(gradient) = &style.gradient {
        let mut under = div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .overflow_hidden();
        if !gradient.horizontal {
            under = under.flex_col();
        }
        under.style().corner_radii.top_left = style.radius_tl;
        under.style().corner_radii.top_right = style.radius_tr;
        under.style().corner_radii.bottom_right = style.radius_br;
        under.style().corner_radii.bottom_left = style.radius_bl;
        let angle = if gradient.horizontal { 90.0 } else { 180.0 };
        let along = |share: f32| {
            let part = div().flex_none();
            if gradient.horizontal {
                part.h_full().w(relative(share))
            } else {
                part.w_full().h(relative(share))
            }
        };
        let stops = &gradient.stops;
        if let Some(first) = stops.first().filter(|first| first.1 > 0.0) {
            under = under.child(along(first.1).bg(first.0));
        }
        for pair in stops.windows(2) {
            let (from, to) = (pair[0], pair[1]);
            if to.1 > from.1 {
                under = under.child(along(to.1 - from.1).bg(linear_gradient(
                    angle,
                    linear_color_stop(from.0, 0.0),
                    linear_color_stop(to.0, 1.0),
                )));
            }
        }
        if let Some(last) = stops.last().filter(|last| last.1 < 1.0) {
            under = under.child(along(1.0 - last.1).bg(last.0));
        }
        out.push(under.into_any_element());
    }
    for (side, width, color) in style.border_plan().strips {
        let strip = div().absolute().bg(color);
        let strip = match side {
            0 => strip.top_0().left_0().w_full().h(px(width)),
            1 => strip.top_0().right_0().h_full().w(px(width)),
            2 => strip.bottom_0().left_0().w_full().h(px(width)),
            _ => strip.top_0().left_0().h_full().w(px(width)),
        };
        out.push(strip.into_any_element());
    }
    out
}

fn render_element(
    node_id: NodeId,
    el: &ElementData,
    style: &ComputedStyle,
    list_ctx: Option<ListContext>,
    ctx: &mut RenderCtx<'_>,
    cx: &App,
) -> Option<AnyElement> {
    let stretch = std::mem::take(&mut ctx.stretch);
    let capped = std::mem::take(&mut ctx.capped);
    // Specialized semantic & media elements.
    match el.tag.as_str() {
        "svg" => return render_svg_element(node_id, el, style, ctx, cx),
        "img" => return render_img_element(node_id, el, style, ctx, cx),
        "hr" => return Some(render_hr(style, ctx)),
        "br" => return Some(div().h(px(style.font_size * 0.7)).into_any_element()),
        "progress" => return Some(render_progress(node_id, el, style, ctx)),
        "meter" => return Some(render_meter(node_id, el, style, ctx)),
        "input" => return Some(render_static_input(node_id, el, style, ctx)),
        "table" => return Some(render_table(node_id, style, ctx, cx)),
        "li" => return Some(render_list_item(node_id, el, style, list_ctx, ctx, cx)),
        "a" => return Some(render_link_element(node_id, el, style, ctx, cx)),
        _ => {}
    }

    // Non-uniform CSS Grid track lists (e.g. `160px 1fr` or `1fr 2fr`).
    if matches!(style.display, DisplayKind::Grid | DisplayKind::InlineGrid)
        && let Some(GridColumns::Tracks(tracks)) = &style.grid_columns
    {
        ctx.stretch = stretch;
        return Some(render_track_grid(node_id, style, tracks, ctx, cx));
    }

    let mut container = div();
    if narrows(ctx.doc, ctx.computed, node_id, style) {
        container = container.min_w_0();
    }
    style.apply_to(container.style(), ctx.viewport_width);
    if stretch {
        container.style().flex_grow = Some(1.0);
    }
    if capped && style.max_width.is_none() {
        container.style().max_size.width = Some(relative(1.0).into());
    }
    if matches!(style.grid_columns, Some(GridColumns::AutoFit { .. }))
        && let Some(columns) = &mut container.style().grid_cols
    {
        // `auto-fit` leaves no empty column: a row of three items is three
        // columns wide however many would fit.
        let items = ctx.doc.nodes[node_id]
            .children
            .iter()
            .filter(|&&child| match &ctx.doc.nodes[child].kind {
                NodeKind::Text(text) => !text.trim().is_empty(),
                NodeKind::Element(_) => ctx.computed.shown(child).is_some(),
            })
            .count();
        columns.repeat = columns.repeat.min(items.max(1) as u16);
    }

    let next_list_ctx = match el.tag.as_str() {
        "ul" => Some(ListContext {
            ordered: false,
            index: 1,
            style: style.list_style,
        }),
        "ol" => {
            let start = el
                .attr("start")
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(1);
            Some(ListContext {
                ordered: true,
                index: start,
                style: style.list_style,
            })
        }
        _ => None,
    };

    if matches!(el.tag.as_str(), "ul" | "ol") && style.list_style == ListStyleType::None {
        // Collapse UA list indentation when author explicitly disables bullets without setting padding.
        if el
            .inline_style
            .as_deref()
            .is_none_or(|s| !s.contains("padding"))
        {
            container.style().padding.left = Some(DefiniteLength::Absolute(px(0.0).into()));
        }
    }

    let mut children = decorations(style);
    if let Some(before) = ctx.computed.before(node_id) {
        children.push(render_pseudo(before, ctx));
    }
    // What the box holds is laid out in the width left inside its padding
    // and border, which is what a nested grid counts its columns from and a
    // nested picture is fitted to.
    let outer_width = ctx.viewport_width;
    ctx.viewport_width = (outer_width - horizontal_chrome(style, outer_width)).max(120.0);
    children.extend(render_children(
        &ctx.doc.nodes[node_id].children,
        style,
        next_list_ctx,
        ctx,
        cx,
    ));
    ctx.viewport_width = outer_width;
    if let Some(after) = ctx.computed.after(node_id) {
        children.push(render_pseudo(after, ctx));
    }

    // A box scrolls on an axis it asks to and can overflow on: sideways
    // always, and down only when something bounds its height. `crate::scroll`
    // says why these are its areas and not GPUI's scroll containers.
    let scrolls_x = style.overflow_x == Some(Overflow::Scroll);
    let scrolls_y = style.overflow_y == Some(Overflow::Scroll)
        && (style.height.is_some() || style.max_height.is_some());
    let container = match (scrolls_x, scrolls_y) {
        (false, false) => container.children(children).into_any_element(),
        (true, false) => container
            .id(("html-scroll-x", node_id))
            .overflow_x_scroll_area()
            .children(children)
            .into_any_element(),
        (false, true) => container
            .id(("html-scroll-y", node_id))
            .overflow_y_scroll_area()
            .children(children)
            .into_any_element(),
        (true, true) => {
            // An area scrolls one way: the box scrolls down, and a strip
            // inside it, laid out as the box is, scrolls sideways.
            let mut strip = div().id(("html-scroll-x", node_id)).w_full().min_w_0();
            style.apply_flow_to(strip.style(), ctx.viewport_width);
            container
                .id(("html-scroll-y", node_id))
                .overflow_y_scroll_area()
                .child(strip.overflow_x_scroll_area().children(children))
                .into_any_element()
        }
    };
    Some(container)
}

fn render_pseudo(style: &ComputedStyle, ctx: &mut RenderCtx<'_>) -> AnyElement {
    let text = style.content.clone().unwrap_or_default();
    let transformed = apply_text_transform(&text, style.text_transform);
    let mut el = div().flex_shrink_0();
    style.apply_to(el.style(), ctx.viewport_width);
    el = el.children(decorations(style));
    if !transformed.is_empty() {
        el = el.child(transformed);
    }
    el.into_any_element()
}

fn render_hr(style: &ComputedStyle, ctx: &RenderCtx<'_>) -> AnyElement {
    let mut el = div();
    style.apply_to(el.style(), ctx.viewport_width);
    el.into_any_element()
}

fn render_list_item(
    node_id: NodeId,
    _el: &ElementData,
    style: &ComputedStyle,
    list_ctx: Option<ListContext>,
    ctx: &mut RenderCtx<'_>,
    cx: &App,
) -> AnyElement {
    let list_style = match style.list_style {
        ListStyleType::Default => list_ctx.map_or(ListStyleType::Disc, |l| {
            if l.style != ListStyleType::Default {
                l.style
            } else if l.ordered {
                ListStyleType::Decimal
            } else {
                ListStyleType::Disc
            }
        }),
        explicit => explicit,
    };

    let children = render_children(&ctx.doc.nodes[node_id].children, style, None, ctx, cx);
    if list_style == ListStyleType::None {
        let mut item = div().min_w_0();
        style.apply_to(item.style(), ctx.viewport_width);
        return item.children(children).into_any_element();
    }

    let marker = match list_style {
        ListStyleType::Decimal => format!("{}.", list_ctx.map_or(1, |l| l.index)),
        ListStyleType::Circle => "◦".to_string(),
        ListStyleType::Square => "▪".to_string(),
        ListStyleType::Disc | ListStyleType::Default => "•".to_string(),
        ListStyleType::None => String::new(),
    };

    let mut content = v_flex().flex_1().min_w_0();
    content = content.children(children);

    let mut row = h_flex().w_full().min_w_0().items_start().gap(px(6.0));
    style.apply_to(row.style(), ctx.viewport_width);
    row.style().display = Some(gpui::Display::Flex);
    row.style().flex_direction = Some(gpui::FlexDirection::Row);
    row.child(
        div()
            .flex_shrink_0()
            .text_color(ctx.theme.muted_foreground)
            .child(marker),
    )
    .child(content)
    .into_any_element()
}

fn render_link_element(
    node_id: NodeId,
    el: &ElementData,
    style: &ComputedStyle,
    ctx: &mut RenderCtx<'_>,
    cx: &App,
) -> AnyElement {
    let href = el.attr("href").and_then(safe_href);
    let outer = ctx.text.enter_link();
    let children = render_children(&ctx.doc.nodes[node_id].children, style, None, ctx, cx);
    ctx.text.leave(outer);
    let mut node = h_flex().items_center().flex_wrap();
    style.apply_to(node.style(), ctx.viewport_width);
    if matches!(style.display, DisplayKind::Inline) {
        node.style().flex_direction = Some(gpui::FlexDirection::Row);
    }
    node = node.children(children);

    let Some(url) = href else {
        return node.into_any_element();
    };
    let shown = url.clone();
    let document = ctx
        .text
        .selecting
        .as_ref()
        .map(|selecting| selecting.membership.document.clone());
    node.id(("html-link", node_id))
        .relative()
        .cursor_pointer()
        .on_click(move |_, _, cx| {
            // A drag that selected the link's text ends on it too.
            if document
                .as_ref()
                .is_some_and(|document| document.read(cx).has_selection())
            {
                return;
            }
            cx.stop_propagation();
            cx.open_url(&url);
        })
        .with_tooltip(move |_, _| Tooltip::new(shown.clone()))
        .into_any_element()
}

/// The address a link opens, when it is one a browser or a mail client
/// opens. A visual is written by a model: it does not get to open a file, a
/// thread or another application's scheme on the user's machine.
fn safe_href(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:")
    {
        Some(trimmed.to_string())
    } else {
        None
    }
}

fn render_progress(
    node_id: NodeId,
    el: &ElementData,
    style: &ComputedStyle,
    ctx: &RenderCtx<'_>,
) -> AnyElement {
    let number = |name: &str| el.attr(name).and_then(super::css::number);
    let max = number("max").filter(|n| *n > 0.0).unwrap_or(1.0);
    let ratio = number("value").unwrap_or(0.0) / max;
    render_bar(node_id, ratio, style, ctx.theme.primary, ctx)
}

fn render_meter(
    node_id: NodeId,
    el: &ElementData,
    style: &ComputedStyle,
    ctx: &RenderCtx<'_>,
) -> AnyElement {
    let number = |name: &str| el.attr(name).and_then(super::css::number);
    let min = number("min").unwrap_or(0.0);
    let max = number("max").filter(|n| *n > min).unwrap_or(min + 1.0);
    let value = number("value").unwrap_or(min);
    let fill = if number("low").is_some_and(|low| value < low) {
        ctx.theme.warning
    } else if number("high").is_some_and(|high| value > high) {
        ctx.theme.danger
    } else {
        ctx.theme.success
    };
    render_bar(node_id, (value - min) / (max - min), style, fill, ctx)
}

/// A `<progress>` or `<meter>`: Tcode's progress bar, as wide as the element
/// is styled and filled with its `color` when it sets one.
fn render_bar(
    node_id: NodeId,
    ratio: f32,
    style: &ComputedStyle,
    default_fill: Hsla,
    ctx: &RenderCtx<'_>,
) -> AnyElement {
    let fill = if style.color == ctx.theme.foreground {
        default_fill
    } else {
        style.color
    };
    let mut frame = div().w(px(120.0));
    style.apply_to(frame.style(), ctx.viewport_width);
    frame
        .child(
            Progress::new(("html-progress", node_id))
                .value(ratio.clamp(0.0, 1.0) * 100.0)
                .color(fill),
        )
        .into_any_element()
}

fn render_static_input(
    node_id: NodeId,
    el: &ElementData,
    style: &ComputedStyle,
    ctx: &RenderCtx<'_>,
) -> AnyElement {
    let kind = el.attr("type").unwrap_or("text").to_ascii_lowercase();
    let checked = el.attr("checked").is_some();
    let number = |name: &str| el.attr(name).and_then(super::css::number);
    match kind.as_str() {
        // Nothing here runs, so no control can be changed: each is drawn as
        // its widget draws one that cannot.
        "checkbox" => Checkbox::new(("html-checkbox", node_id))
            .checked(checked)
            .disabled(true)
            .into_any_element(),
        // Gap: `widgets::radio` draws a list of titled option rows and
        // gpui-base's `Radio` draws no indicator of its own, so a lone
        // `<input type="radio">` has no widget; this is the indicator alone.
        "radio" => div()
            .size(px(16.0))
            .flex_none()
            .rounded_full()
            .border_1()
            .border_color(if checked {
                ctx.theme.primary
            } else {
                ctx.theme.input
            })
            .bg(ctx.theme.background)
            .opacity(0.5)
            .flex()
            .items_center()
            .justify_center()
            .children(checked.then(|| div().size(px(8.0)).rounded_full().bg(ctx.theme.primary)))
            .into_any_element(),
        // Gap: `widgets::slider` is a stepped control over a `SliderState`
        // entity, with a dot for every step and a thumb that snaps to one; a
        // continuous range of a hundred steps has no form there. This is its
        // track, filled to the value.
        "range" => {
            let min = number("min").unwrap_or(0.0);
            let max = number("max").filter(|n| *n > min).unwrap_or(min + 100.0);
            let value = number("value").unwrap_or((min + max) * 0.5);
            let mut track = div()
                .w(px(120.0))
                .h(px(8.0))
                .rounded_full()
                .bg(ctx.theme.muted)
                .overflow_hidden();
            style.apply_to(track.style(), ctx.viewport_width);
            track
                .child(
                    div()
                        .h_full()
                        .w(relative(((value - min) / (max - min)).clamp(0.0, 1.0)))
                        .rounded_full()
                        .bg(ctx.theme.primary),
                )
                .into_any_element()
        }
        _ => {
            let value = el
                .attr("value")
                .or_else(|| el.attr("placeholder"))
                .unwrap_or("")
                .to_string();
            let mut field = div()
                .px(px(8.0))
                .py(px(4.0))
                .rounded(px(4.0))
                .border_1()
                .border_color(ctx.theme.border)
                .bg(ctx.theme.input)
                .text_color(if el.attr("value").is_some() {
                    ctx.theme.foreground
                } else {
                    ctx.theme.muted_foreground
                });
            style.apply_to(field.style(), ctx.viewport_width);
            field.child(value).into_any_element()
        }
    }
}

/// Render a CSS Grid whose `grid-template-columns` specifies non-uniform tracks
/// (e.g. `180px 1fr`, `1fr 2fr`, `120px 1fr 120px`).
/// What a grid of named tracks lays out, in the order it places them: each
/// item with the track it starts in and how many it spans, row by row.
fn track_items(
    doc: &Document,
    computed: &Computed,
    node_id: NodeId,
    col_count: usize,
) -> Vec<(NodeId, usize, usize)> {
    let mut items = Vec::new();
    let mut cursor = 0usize;
    for &cid in &doc.nodes[node_id].children {
        let span = match &doc.nodes[cid].kind {
            NodeKind::Text(text) if text.trim().is_empty() => continue,
            NodeKind::Text(_) => 1,
            NodeKind::Element(_) => {
                let Some(style) = computed.shown(cid) else {
                    continue;
                };
                if style.position == Position::Absolute {
                    continue;
                }
                style.grid_location.as_ref().map_or(1, |loc| {
                    match (&loc.column.start, &loc.column.end) {
                        (gpui::GridPlacement::Span(n), _) | (_, gpui::GridPlacement::Span(n)) => {
                            (*n as usize).clamp(1, col_count)
                        }
                        (gpui::GridPlacement::Line(s), gpui::GridPlacement::Line(e)) if *e > *s => {
                            ((*e - *s) as usize).clamp(1, col_count)
                        }
                        _ => 1,
                    }
                })
            }
        };
        if cursor > 0 && cursor + span > col_count {
            cursor = 0;
        }
        items.push((cid, cursor, span));
        cursor = (cursor + span) % col_count;
    }
    items
}

/// A grid of named tracks, as rows of boxes: GPUI's grid has only tracks of
/// one size. Every row gives a track the same room, which is its length, its
/// share of what is left, or for an `auto` track the width
/// [`measure`] found its widest item to have.
fn render_track_grid(
    node_id: NodeId,
    style: &ComputedStyle,
    tracks: &[GridTrack],
    ctx: &mut RenderCtx<'_>,
    cx: &App,
) -> AnyElement {
    let col_count = tracks.len().max(1);
    let measured = ctx.computed.measured(node_id);
    let gap = match style.col_gap {
        Some(DefiniteLength::Absolute(AbsoluteLength::Pixels(pixels))) => f32::from(pixels),
        _ => 0.0,
    };
    let room = TrackRoom {
        tracks,
        measured: measured
            .as_deref()
            .map(|widths| &widths[..widths.len() / 2]),
        shares: tracks.iter().any(|track| matches!(track, GridTrack::Fr(_))),
        gap,
    };

    let mut outer = v_flex().w_full().min_w_0();
    style.apply_to(outer.style(), ctx.viewport_width);
    outer.style().display = Some(gpui::Display::Flex);
    outer.style().flex_direction = Some(gpui::FlexDirection::Column);
    outer.style().align_items = Some(AlignItems::Stretch);
    outer.style().justify_content = None;
    if std::mem::take(&mut ctx.stretch) {
        outer.style().flex_grow = Some(1.0);
    }
    outer = outer.children(decorations(style));

    let mut row = make_track_row(style);
    let mut filled = 0usize;
    for (cid, start, span) in track_items(ctx.doc, ctx.computed, node_id, col_count) {
        if start == 0 && filled > 0 {
            outer = outer.child(room.close(row, filled));
            row = make_track_row(style);
        }
        let mut cell = div().flex().flex_col();
        let narrows = match &ctx.doc.nodes[cid].kind {
            NodeKind::Text(text) => can_wrap(text),
            NodeKind::Element(_) => ctx
                .computed
                .shown(cid)
                .is_some_and(|child| narrows(ctx.doc, ctx.computed, cid, child)),
        };
        if narrows {
            cell = cell.min_w_0();
        }
        room.apply(cell.style(), start, span);
        let mut down = style.align_items.unwrap_or(AlignItems::Stretch);
        let rendered = match &ctx.doc.nodes[cid].kind {
            NodeKind::Text(text) => {
                let normalized = normalize_text(text, style.white_space);
                let transformed = apply_text_transform(normalized.trim(), style.text_transform);
                Some(
                    text_box(&transformed, style.font_size)
                        .child(ctx.text.run(transformed, Vec::new()))
                        .into_any_element(),
                )
            }
            NodeKind::Element(el) => ctx.computed.shown(cid).and_then(|child| {
                down = child.align_self.unwrap_or(down);
                ctx.stretch = down == AlignItems::Stretch;
                let rendered = render_element(cid, el, child, None, ctx, cx);
                ctx.stretch = false;
                rendered
            }),
        };
        cell = match down {
            AlignItems::Center => cell.justify_center(),
            AlignItems::FlexEnd => cell.justify_end(),
            _ => cell,
        };
        cell = match style.justify_items {
            Some(AlignItems::Center) => cell.items_center(),
            Some(AlignItems::FlexEnd) => cell.items_end(),
            Some(AlignItems::FlexStart | AlignItems::Baseline) => cell.items_start(),
            _ => cell,
        };
        row = row.child(cell.children(rendered));
        filled = start + span;
    }
    if filled > 0 {
        outer = outer.child(room.close(row, filled));
    }
    // What the grid positions itself is no item of it.
    for &cid in &ctx.doc.nodes[node_id].children {
        if let NodeKind::Element(el) = &ctx.doc.nodes[cid].kind
            && let Some(child) = ctx.computed.shown(cid)
            && child.position == Position::Absolute
        {
            outer = outer.children(render_element(cid, el, child, None, ctx, cx));
        }
    }
    outer.into_any_element()
}

fn make_track_row(style: &ComputedStyle) -> gpui::Div {
    let mut row = h_flex().w_full().min_w_0().items_stretch().flex_grow(1.0);
    if let Some(gap) = style.col_gap {
        row.style().gap.width = Some(gap);
    }
    row
}

/// The room each track of a grid has in every one of its rows.
struct TrackRoom<'a> {
    tracks: &'a [GridTrack],
    /// The width of each track's widest item, for the `auto` tracks.
    measured: Option<&'a [f32]>,
    /// Some track takes a share of what is left; else the `auto` tracks do.
    shares: bool,
    gap: f32,
}

impl TrackRoom<'_> {
    /// Give `cell` the room of the `span` tracks from `start`.
    fn apply(&self, cell: &mut gpui::StyleRefinement, start: usize, span: usize) {
        let end = (start + span).min(self.tracks.len());
        let slice = &self.tracks[start.min(end)..end];
        let mut share = 0.0f32;
        let mut pixels = self.gap * slice.len().saturating_sub(1) as f32;
        let mut percent = 0.0f32;
        let mut fits_content = false;
        let mut gives = false;
        for (index, track) in slice.iter().enumerate() {
            match track {
                GridTrack::Fr(fr) => share += *fr,
                GridTrack::Px(px) => pixels += *px,
                GridTrack::Percent(pct) => percent += *pct,
                GridTrack::Auto => {
                    gives = true;
                    match self.measured.and_then(|widths| widths.get(start + index)) {
                        Some(width) => pixels += *width,
                        None => fits_content = true,
                    }
                    if !self.shares {
                        share += 1.0;
                    }
                }
            }
        }
        if percent > 0.0 && share == 0.0 {
            cell.size.width = Some(relative(percent.clamp(0.0, 1.0)).into());
            cell.flex_shrink = Some(0.0);
        } else if fits_content {
            cell.flex_grow = Some(share);
            cell.flex_shrink = Some(1.0);
        } else {
            cell.flex_basis = Some(px(pixels).into());
            cell.flex_grow = Some(share);
            cell.flex_shrink = Some(if share > 0.0 || gives { 1.0 } else { 0.0 });
        }
    }

    /// `row`, which has items in its first `filled` tracks, with the room of
    /// the tracks it has none in left empty.
    fn close(&self, mut row: gpui::Div, filled: usize) -> gpui::Div {
        for start in filled..self.tracks.len() {
            let mut cell = div().min_w_0();
            self.apply(cell.style(), start, 1);
            row = row.child(cell);
        }
        row
    }
}

/// The rows of a table: each with its style, whether it is of the head, and
/// its cells with the column each starts in and how many it spans.
struct TableRows<'a> {
    caption: Option<NodeId>,
    rows: Vec<TableRow<'a>>,
    columns: usize,
}

struct TableRow<'a> {
    style: &'a ComputedStyle,
    head: bool,
    cells: Vec<(NodeId, &'a ComputedStyle, usize, usize)>,
}

fn table_rows<'a>(doc: &Document, computed: &'a Computed, node_id: NodeId) -> TableRows<'a> {
    let mut table = TableRows {
        caption: None,
        rows: Vec::new(),
        columns: 0,
    };
    for &cid in &doc.nodes[node_id].children {
        let (Some(el), Some(_)) = (doc.element(cid), computed.shown(cid)) else {
            continue;
        };
        match el.tag.as_str() {
            "caption" => table.caption = Some(cid),
            "thead" | "tbody" | "tfoot" => {
                for &rid in &doc.nodes[cid].children {
                    if doc.element(rid).is_some_and(|row| row.tag == "tr")
                        && let Some(style) = computed.shown(rid)
                    {
                        let mut cells = Vec::new();
                        let mut column = 0usize;
                        for &cell_id in &doc.nodes[rid].children {
                            if let Some(cell) = doc.element(cell_id)
                                && matches!(cell.tag.as_str(), "td" | "th")
                                && let Some(cell_style) = computed.shown(cell_id)
                                && column < MAX_COLUMNS
                            {
                                let span = cell
                                    .attr("colspan")
                                    .and_then(|s| s.parse::<usize>().ok())
                                    .unwrap_or(1)
                                    .clamp(1, MAX_COLSPAN)
                                    .min(MAX_COLUMNS - column);
                                cells.push((cell_id, cell_style, column, span));
                                column += span;
                            }
                        }
                        table.columns = table.columns.max(column);
                        table.rows.push(TableRow {
                            style,
                            head: el.tag == "thead",
                            cells,
                        });
                    }
                }
            }
            _ => {}
        }
    }
    table
}

/// The room a column of a table has in every row.
#[derive(Clone, Copy)]
enum ColumnRoom {
    /// The width a cell of the column names, in pixels.
    Pixels(f32),
    /// The share of the table a cell of the column names.
    Share(f32),
    /// The width of the column's widest cell, which it gives and takes of
    /// what the table has over or under in that measure.
    Content(f32),
    /// An equal part of what is left.
    Even,
}

/// A table cell's box and what it holds, before its column gives it room.
fn table_cell(
    cell_id: NodeId,
    cell_style: &ComputedStyle,
    ctx: &mut RenderCtx<'_>,
    cx: &App,
) -> gpui::Div {
    let mut cell_div = v_flex().min_w_0();
    cell_style.apply_to(cell_div.style(), ctx.viewport_width);
    cell_div.style().size.width = None;
    cell_div = match cell_style.vertical_align {
        VerticalAlign::Top => cell_div.justify_start(),
        VerticalAlign::Bottom => cell_div.justify_end(),
        _ => cell_div.justify_center(),
    };
    let outer_cell = ctx.text.enter_cell();
    let children = render_children(&ctx.doc.nodes[cell_id].children, cell_style, None, ctx, cx);
    ctx.text.leave(outer_cell);
    cell_div
        .children(decorations(cell_style))
        .children(children)
}

/// Native GPUI HTML `<table>` renderer with header styling, borders, `colspan`,
/// caption support, and horizontal scrolling for wide tables. A column has
/// the same room in every row: the width a cell of it names, else the width
/// [`measure`] found its widest cell to have.
fn render_table(
    node_id: NodeId,
    table_style: &ComputedStyle,
    ctx: &mut RenderCtx<'_>,
    cx: &App,
) -> AnyElement {
    let computed = ctx.computed;
    let parts = table_rows(ctx.doc, computed, node_id);
    let caption_el = parts.caption.and_then(|cid| {
        render_element(
            cid,
            ctx.doc.element(cid)?,
            computed.shown(cid)?,
            None,
            ctx,
            cx,
        )
    });

    let measured = computed.measured(node_id);
    let fills = table_style.width.is_some();
    let mut columns: Vec<Option<ColumnRoom>> = vec![None; parts.columns];
    for row in &parts.rows {
        for &(_, cell_style, column, span) in &row.cells {
            if span != 1 || columns[column].is_some() {
                continue;
            }
            columns[column] = match cell_style.width {
                Some(Length::Definite(DefiniteLength::Absolute(AbsoluteLength::Pixels(w)))) => {
                    let chrome = if cell_style.border_box {
                        0.0
                    } else {
                        cell_style.chrome().0
                    };
                    Some(ColumnRoom::Pixels(f32::from(w) + chrome))
                }
                Some(Length::Definite(DefiniteLength::Fraction(share))) => {
                    Some(ColumnRoom::Share(share))
                }
                _ => None,
            };
        }
    }
    let columns: Vec<ColumnRoom> = columns
        .into_iter()
        .enumerate()
        .map(|(index, named)| {
            named.unwrap_or_else(|| {
                match measured.as_deref().and_then(|widths| widths.get(index)) {
                    Some(width) if !table_style.table_fixed => ColumnRoom::Content(*width),
                    _ => ColumnRoom::Even,
                }
            })
        })
        .collect();
    let table_width = ctx.viewport_width;
    let room = |cell: &mut gpui::StyleRefinement, column: usize, span: usize| {
        let slice = &columns[column..(column + span).min(columns.len())];
        if let [ColumnRoom::Share(share)] = slice {
            cell.size.width = Some(relative(share.clamp(0.0, 1.0)).into());
            cell.flex_shrink = Some(0.0);
            return;
        }
        if slice.len() == columns.len() && slice.len() > 1 {
            cell.size.width = Some(relative(1.0).into());
            return;
        }
        let mut pixels = 0.0f32;
        let mut grows = 0.0f32;
        let mut gives = false;
        let rigid: f32 = (column..(column + span).min(columns.len()))
            .filter_map(|index| measured.as_deref()?.get(columns.len() + index))
            .sum();
        if rigid > 0.0 {
            cell.min_size.width = Some(px(rigid).into());
        }
        for column in slice {
            match *column {
                ColumnRoom::Pixels(width) => pixels += width,
                ColumnRoom::Share(share) => pixels += share * table_width,
                ColumnRoom::Content(width) => {
                    pixels += width;
                    gives = true;
                    if fills {
                        grows += width.max(1.0);
                    }
                }
                ColumnRoom::Even => {
                    gives = true;
                    grows += 1.0;
                }
            }
        }
        cell.flex_basis = Some(px(pixels).into());
        cell.flex_grow = Some(grows);
        cell.flex_shrink = Some(if gives { 1.0 } else { 0.0 });
    };

    let mut table = v_flex().min_w_0();
    table_style.apply_to(table.style(), ctx.viewport_width);
    table.style().display = Some(gpui::Display::Flex);
    table.style().flex_direction = Some(gpui::FlexDirection::Column);

    let row_count = parts.rows.len();
    for (row_idx, row) in parts.rows.into_iter().enumerate() {
        let outer_row = ctx.text.enter_row();
        let mut row_div = h_flex().w_full().min_w_0().items_stretch();
        row.style.apply_to(row_div.style(), ctx.viewport_width);
        row_div.style().display = Some(gpui::Display::Flex);
        row_div.style().flex_direction = Some(gpui::FlexDirection::Row);

        // Subtle default row divider if author did not explicitly clear borders.
        if row.style.border_bottom_w.is_none() && (row.head || row_idx + 1 < row_count) {
            row_div = row_div
                .border_b_1()
                .border_color(ctx.theme.border.opacity(if row.head { 0.9 } else { 0.5 }));
        }
        if row.head && row.style.background.is_none() {
            row_div = row_div.bg(ctx.theme.muted.opacity(0.6));
        }

        for (cell_id, cell_style, column, span) in row.cells {
            let mut cell_div = table_cell(cell_id, cell_style, ctx, cx);
            room(cell_div.style(), column, span);
            row_div = row_div.child(cell_div);
        }
        ctx.text.leave(outer_row);
        table = table.child(row_div);
    }

    v_flex()
        .w_full()
        .min_w_0()
        .items_start()
        .children(caption_el)
        .child(
            div()
                .id(("html-table", node_id))
                .max_w_full()
                .min_w_0()
                .when(fills, |this| this.w_full())
                .overflow_x_scroll_area()
                .child(table),
        )
        .into_any_element()
}

/// Find the widths [`render_track_grid`] and [`render_table`] give their
/// columns by: for each `auto` track of a grid and each column of a table,
/// the width its widest item has when nothing narrows it. GPUI lays a row
/// out by itself, so what the rows of one column share is found first, by
/// laying each item out alone.
///
/// An element is laid out under the styles of what is around the visual,
/// so each item is put in a box with the page's own.
pub(crate) fn measure(
    doc: &Document,
    computed: &Computed,
    viewport_width: f32,
    window: &mut Window,
    cx: &mut App,
) {
    let media = MediaSlots::default();
    let mut left = MAX_MEASURED;
    // Last first: what is inside is measured before what holds it.
    for node_id in (0..doc.nodes.len()).rev() {
        let (Some(el), Some(style)) = (doc.element(node_id), computed.shown(node_id)) else {
            continue;
        };
        // Each item to measure, with its column and whether it is a cell.
        let (count, items): (usize, Vec<(NodeId, usize, bool)>) = if el.tag == "table" {
            let parts = table_rows(doc, computed, node_id);
            let cells = parts
                .rows
                .iter()
                .flat_map(|row| &row.cells)
                .filter(|cell| cell.3 == 1)
                .map(|cell| (cell.0, cell.2, true))
                .collect();
            (parts.columns, cells)
        } else if matches!(style.display, DisplayKind::Grid | DisplayKind::InlineGrid)
            && let Some(GridColumns::Tracks(tracks)) = &style.grid_columns
            && tracks.contains(&GridTrack::Auto)
        {
            let items = track_items(doc, computed, node_id, tracks.len())
                .into_iter()
                .filter(|&(cid, start, span)| {
                    span == 1 && tracks[start] == GridTrack::Auto && doc.element(cid).is_some()
                })
                .map(|(cid, start, _)| (cid, start, false))
                .collect();
            (tracks.len(), items)
        } else {
            continue;
        };
        if items.is_empty() || items.len() > left {
            continue;
        }
        left -= items.len();
        // After the widths, as many more: the width each column cannot be
        // narrower than, which is that of its widest item that does not wrap.
        let mut widths = vec![0.0f32; count * 2];
        for (item, column, is_cell) in items {
            let built = {
                let mut ctx = RenderCtx {
                    doc,
                    computed,
                    media: &media,
                    theme: cx.theme(),
                    viewport_width,
                    frame: Frame::default(),
                    text: TextSink::default(),
                    stretch: false,
                    capped: false,
                    words: 0,
                };
                let (Some(el), Some(style)) = (doc.element(item), computed.shown(item)) else {
                    continue;
                };
                if is_cell {
                    Some(table_cell(item, style, &mut ctx, cx).into_any_element())
                } else {
                    render_element(item, el, style, None, &mut ctx, cx)
                }
            };
            let Some(built) = built else {
                continue;
            };
            let root = &computed.root;
            let mut alone = div()
                .flex()
                .flex_col()
                .items_start()
                .font_family(root.font_family.clone())
                .text_size(px(root.font_size))
                .child(built)
                .into_any_element();
            let natural = alone.layout_as_root(
                size(AvailableSpace::MaxContent, AvailableSpace::MaxContent),
                window,
                cx,
            );
            // A width in whole pixels and one over: a line as wide as its
            // box to a fraction wraps.
            let natural = f32::from(natural.width).ceil() + 1.0;
            widths[column] = widths[column].max(natural);
            if computed
                .shown(item)
                .is_some_and(|style| style.white_space == WhiteSpaceMode::NoWrap)
            {
                widths[count + column] = widths[count + column].max(natural);
            }
        }
        computed.set_measured(node_id, widths);
    }
}

/// Render a contiguous run of inline nodes.
///
/// Pure text + styled spans (`strong`, `em`, `span`, `mark`, `small`, `br`, etc.)
/// flatten into one native `StyledText` for crisp wrapping and baseline alignment.
/// Mixed runs that contain inline boxes (badges, pills, `<svg>`, links, `<progress>`)
/// render in a wrapping flex flow.
fn render_inline_run(
    nodes: &[NodeId],
    parent_style: &ComputedStyle,
    ctx: &mut RenderCtx<'_>,
    cx: &App,
) -> Option<AnyElement> {
    if can_flatten_inline(nodes, parent_style, ctx) {
        let mut text = String::new();
        let mut highlights: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
        let run = InlineRun {
            base: parent_style,
            ctx,
        };
        run.collect(nodes, parent_style, None, &mut text, &mut highlights);
        if matches!(
            parent_style.white_space,
            WhiteSpaceMode::Normal | WhiteSpaceMode::NoWrap
        ) {
            let trimmed_start = text.len() - text.trim_start().len();
            let trimmed_end = text.trim_end().len();
            if trimmed_start >= trimmed_end {
                return None;
            }
            if trimmed_start > 0 || trimmed_end < text.len() {
                text = text[trimmed_start..trimmed_end].to_string();
                highlights = highlights
                    .into_iter()
                    .filter_map(|(r, h)| {
                        let s = r.start.saturating_sub(trimmed_start);
                        let e = r.end.min(trimmed_end).saturating_sub(trimmed_start);
                        (s < e).then_some((s..e, h))
                    })
                    .collect();
            }
        } else if text.is_empty() {
            return None;
        }
        let styled = text_box(&text, parent_style.font_size).w_full();
        return Some(
            styled
                .child(ctx.text.run(text, highlights))
                .into_any_element(),
        );
    }

    // A line of text with boxes in it: badges, pictures, links, text of
    // another size. GPUI wraps the text of one box and nothing around it, so
    // the line is a wrapping row of the boxes and of the text between them,
    // word by word.
    let mut row = h_flex().w_full().min_w_0().items_baseline();
    if parent_style.white_space != WhiteSpaceMode::NoWrap {
        row = row.flex_wrap();
    }
    match parent_style.text_align {
        Some(TextAlign::Center) => row = row.justify_center(),
        Some(TextAlign::Right) => row = row.justify_end(),
        _ => {}
    }
    let outer = ctx.text.enter_flow();
    let mut flow = InlineFlow {
        block: parent_style,
        items: Vec::new(),
        space: None,
        fresh: true,
    };
    flow.nodes(nodes, parent_style, None, ctx, cx);
    ctx.text.leave(outer);
    (!flow.items.is_empty()).then(|| row.children(flow.items).into_any_element())
}

/// The boxes of a line as it is built, in order.
struct InlineFlow<'a> {
    /// The style of the block the line is in.
    block: &'a ComputedStyle,
    items: Vec<AnyElement>,
    /// White space stands between the last box and the next, in text of this
    /// style and background.
    space: Option<(&'a ComputedStyle, Option<Hsla>)>,
    /// Nothing is on the line yet.
    fresh: bool,
}

impl<'a> InlineFlow<'a> {
    fn nodes<'c: 'a>(
        &mut self,
        nodes: &[NodeId],
        style: &'a ComputedStyle,
        background: Option<Hsla>,
        ctx: &mut RenderCtx<'c>,
        cx: &App,
    ) {
        for &nid in nodes {
            match &ctx.doc.nodes[nid].kind {
                NodeKind::Text(text) => self.text(text, style, background, ctx),
                NodeKind::Element(el) => {
                    if el.tag == "br" {
                        self.line_break(ctx);
                        continue;
                    }
                    let Some(child) = ctx.computed.shown(nid) else {
                        continue;
                    };
                    if flows_as_text(nid, el, child, ctx) {
                        let background = child.background_hsla.or(background);
                        self.nodes(&ctx.doc.nodes[nid].children, child, background, ctx, cx);
                        continue;
                    }
                    self.spaced(ctx);
                    if let Some(rendered) = render_element(nid, el, child, None, ctx, cx) {
                        self.items.push(self.on_line(rendered, nid, child, ctx));
                        self.fresh = false;
                    }
                }
            }
        }
    }

    fn text(
        &mut self,
        text: &str,
        style: &'a ComputedStyle,
        background: Option<Hsla>,
        ctx: &mut RenderCtx<'_>,
    ) {
        let normalized = normalize_text(text, style.white_space);
        let kept = matches!(
            style.white_space,
            WhiteSpaceMode::Pre | WhiteSpaceMode::PreWrap | WhiteSpaceMode::PreLine
        );
        for (index, line) in normalized.split('\n').enumerate() {
            if index > 0 {
                if kept {
                    self.line_break(ctx);
                } else {
                    self.space = Some((style, background));
                }
            }
            match style.white_space {
                // A line of `pre` is one box, its spaces as written.
                WhiteSpaceMode::Pre | WhiteSpaceMode::NoWrap => {
                    if !line.trim().is_empty() || (kept && !line.is_empty()) {
                        let line = if kept {
                            line
                        } else {
                            self.inner(line, style, background)
                        };
                        if !line.is_empty() {
                            self.spaced(ctx);
                            self.piece(line, style, background, ctx);
                        }
                    } else if !line.is_empty() && !self.fresh {
                        self.space = Some((style, background));
                    }
                }
                _ if ctx.words >= MAX_WORDS => {
                    if !line.trim().is_empty() {
                        self.spaced(ctx);
                        let text = apply_text_transform(line.trim(), style.text_transform);
                        let mut piece = text_box(&text, style.font_size);
                        style.apply_text_to(piece.style());
                        self.items.push(
                            piece
                                .child(ctx.text.run(text, Vec::new()))
                                .into_any_element(),
                        );
                        self.fresh = false;
                    }
                }
                _ => {
                    for word in words(line) {
                        ctx.words += 1;
                        if word.trim().is_empty() {
                            if !self.fresh {
                                self.space = Some((style, background));
                            }
                        } else {
                            self.spaced(ctx);
                            self.piece(word, style, background, ctx);
                        }
                    }
                }
            }
        }
    }

    /// `line` without the white space at its ends, which is kept as the
    /// space before the next box.
    fn inner<'t>(
        &mut self,
        line: &'t str,
        style: &'a ComputedStyle,
        background: Option<Hsla>,
    ) -> &'t str {
        if line.starts_with(char::is_whitespace) && !self.fresh {
            self.space = Some((style, background));
        }
        line.trim_start()
    }

    /// The white space before the next box, as a box: it is drawn where a
    /// line goes on, and a line that ends at it loses only its width.
    fn spaced(&mut self, ctx: &mut RenderCtx<'_>) {
        if let Some((style, background)) = self.space.take()
            && !self.fresh
        {
            self.piece(" ", style, background, ctx);
        }
    }

    fn piece(
        &mut self,
        text: &str,
        style: &ComputedStyle,
        background: Option<Hsla>,
        ctx: &mut RenderCtx<'_>,
    ) {
        let text = apply_text_transform(text, style.text_transform);
        let mut piece = div().flex_none().whitespace_nowrap();
        style.apply_text_to(piece.style());
        if let Some(background) = background {
            piece = piece.bg(background);
        }
        // No line breaks before what closes a phrase, whatever box it
        // follows: the two are one box of the row.
        // CoreText measures a line without the blank half of a full-width
        // mark at either end of it, as a line of a paragraph is set. A word
        // is a line to it and is no line's end here, so the half is put back.
        if cfg!(target_vendor = "apple") {
            let half = px(style.font_size / 2.0);
            if text.ends_with([
                '，', '。', '、', '；', '：', '！', '？', '）', '」', '』', '】', '》',
            ]) {
                piece = piece.pr(half);
            }
            if text.starts_with(['（', '「', '『', '【', '《']) {
                piece = piece.pl(half);
            }
        }
        let glued = !self.fresh && self.space.is_none() && text.starts_with(closes_phrase);
        let piece = piece
            .child(ctx.text.run(text, Vec::new()))
            .into_any_element();
        match self.items.pop() {
            Some(before) if glued => self.items.push(
                h_flex()
                    .flex_none()
                    .items_baseline()
                    .child(before)
                    .child(piece)
                    .into_any_element(),
            ),
            before => {
                self.items.extend(before);
                self.items.push(piece);
            }
        }
        self.fresh = false;
    }

    fn line_break(&mut self, ctx: &mut RenderCtx<'_>) {
        // A line with nothing on it is still a line.
        let height = if self.fresh {
            line_height(self.block)
        } else {
            0.0
        };
        self.items
            .push(div().w(relative(1.0)).h(px(height)).into_any_element());
        ctx.text.line_break();
        self.space = None;
        self.fresh = true;
    }

    /// A box on the line, where the line's text puts it: the row sets every
    /// box on its bottom edge, and a box whose own baseline is not as far
    /// above that edge as the line's is drawn that much higher.
    fn on_line(
        &self,
        rendered: AnyElement,
        node: NodeId,
        style: &ComputedStyle,
        ctx: &RenderCtx<'_>,
    ) -> AnyElement {
        // A box as wide as a share of its line is given that width by the
        // line, which a box around it would come between.
        if matches!(
            style.width,
            Some(Length::Definite(DefiniteLength::Fraction(_)))
        ) {
            return rendered;
        }
        let line = baseline_from_bottom(self.block);
        // A box of text stands on its own last line, which is never above
        // the line's: only a box with none is moved up to it.
        let stands = if has_text(ctx.doc, node) {
            (line - baseline_from_bottom(style) - style.chrome_below()).max(0.0)
        } else {
            line
        };
        let lift = match style.vertical_align {
            VerticalAlign::Middle => {
                return div()
                    .flex_none()
                    .self_center()
                    .child(rendered)
                    .into_any_element();
            }
            VerticalAlign::Top => {
                return div()
                    .flex_none()
                    .self_start()
                    .child(rendered)
                    .into_any_element();
            }
            VerticalAlign::Bottom => return rendered,
            VerticalAlign::Baseline => stands,
            VerticalAlign::Raised(by) => stands + by,
        };
        if lift.abs() < 0.5 {
            return rendered;
        }
        div()
            .flex_none()
            .relative()
            .top(px(-lift))
            .child(rendered)
            .into_any_element()
    }
}

/// The height of a line of `style`'s text.
fn line_height(style: &ComputedStyle) -> f32 {
    match style.line_height {
        Some(DefiniteLength::Fraction(share)) => share * style.font_size,
        Some(DefiniteLength::Absolute(AbsoluteLength::Pixels(pixels))) => f32::from(pixels),
        _ => 1.5 * style.font_size,
    }
}

/// Whether an inline element is drawn as text of its line, which wraps
/// inside it: it has no box of its own, and nothing put before or after it.
fn flows_as_text(
    nid: NodeId,
    el: &ElementData,
    style: &ComputedStyle,
    ctx: &RenderCtx<'_>,
) -> bool {
    style.display.is_pure_inline()
        && !style.has_inline_box()
        && !matches!(
            el.tag.as_str(),
            "a" | "svg" | "img" | "progress" | "meter" | "input" | "button"
        )
        && ctx.computed.before(nid).is_none()
        && ctx.computed.after(nid).is_none()
}

/// Whether `c` closes a phrase, so that no line begins with it.
fn closes_phrase(c: char) -> bool {
    matches!(
        c,
        ',' | '.'
            | ';'
            | ':'
            | '!'
            | '?'
            | ')'
            | ']'
            | '}'
            | '%'
            | '，'
            | '。'
            | '、'
            | '；'
            | '：'
            | '！'
            | '？'
            | '）'
            | '」'
            | '』'
            | '】'
            | '》'
            | '〉'
            | '”'
            | '’'
            | '…'
            | '·'
    )
}

fn is_cjk(c: char) -> bool {
    matches!(
        c,
        '\u{2e80}'..='\u{30ff}'
            | '\u{3400}'..='\u{4dbf}'
            | '\u{4e00}'..='\u{9fff}'
            | '\u{ac00}'..='\u{d7af}'
            | '\u{f900}'..='\u{faff}'
            | '\u{ff00}'..='\u{ffef}'
            | '\u{20000}'..='\u{2fa1f}'
    )
}

/// `text` cut where a line may break: after white space, and on either side
/// of a CJK character, but never before what closes a phrase nor after what
/// opens one. White space after a word stays with it.
fn words(text: &str) -> Vec<&str> {
    let closes = closes_phrase;
    let opens = |c: char| {
        matches!(
            c,
            '(' | '[' | '{' | '（' | '「' | '『' | '【' | '《' | '〈' | '“' | '‘'
        )
    };
    let mut out = Vec::new();
    let mut start = 0;
    let mut before: Option<char> = None;
    for (at, c) in text.char_indices() {
        if let Some(before) = before {
            let breaks = if before.is_whitespace() {
                !c.is_whitespace()
            } else {
                !c.is_whitespace() && !closes(c) && !opens(before) && (is_cjk(before) || is_cjk(c))
            };
            if breaks {
                out.push(&text[start..at]);
                start = at;
            }
        }
        before = Some(c);
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

fn can_flatten_inline(nodes: &[NodeId], parent_style: &ComputedStyle, ctx: &RenderCtx<'_>) -> bool {
    for &nid in nodes {
        match &ctx.doc.nodes[nid].kind {
            NodeKind::Text(_) => {}
            NodeKind::Element(el) => {
                if el.tag == "br" {
                    continue;
                }
                if matches!(
                    el.tag.as_str(),
                    "a" | "svg" | "img" | "progress" | "meter" | "input" | "button"
                ) {
                    return false;
                }
                let Some(style) = ctx.computed.style(nid) else {
                    return false;
                };
                if style.display == DisplayKind::None {
                    continue;
                }
                if !style.display.is_pure_inline() || style.has_inline_box() {
                    return false;
                }
                // One run of text has one background shape: its lines'.
                if style.background.is_some() && style.radius_tl.is_some() {
                    return false;
                }
                if (style.font_size - parent_style.font_size).abs() > 0.5
                    || style.font_family != parent_style.font_family
                {
                    return false;
                }
                if ctx.computed.before(nid).is_some() || ctx.computed.after(nid).is_some() {
                    return false;
                }
                if !can_flatten_inline(&ctx.doc.nodes[nid].children, style, ctx) {
                    return false;
                }
            }
        }
    }
    true
}

/// The text of an inline run and the ranges of it styled apart from `base`,
/// the style the run's own box is drawn in.
struct InlineRun<'a, 'c> {
    base: &'a ComputedStyle,
    ctx: &'a RenderCtx<'c>,
}

impl InlineRun<'_, '_> {
    /// One range for each text node, in order, so that the ranges never
    /// overlap: GPUI builds a run for each as it comes and takes them to be
    /// sorted and apart. `background` is the nearest inline ancestor's, which
    /// a style does not inherit.
    fn collect(
        &self,
        nodes: &[NodeId],
        style: &ComputedStyle,
        background: Option<Hsla>,
        out: &mut String,
        highlights: &mut Vec<(Range<usize>, HighlightStyle)>,
    ) {
        for &nid in nodes {
            match &self.ctx.doc.nodes[nid].kind {
                NodeKind::Text(t) => {
                    let norm = normalize_text(t, style.white_space);
                    let transformed = apply_text_transform(&norm, style.text_transform);
                    let start = out.len();
                    // Collapse duplicate boundary space across adjacent inline nodes in normal mode.
                    if matches!(
                        style.white_space,
                        WhiteSpaceMode::Normal | WhiteSpaceMode::NoWrap
                    ) && out.ends_with(' ')
                        && transformed.starts_with(' ')
                    {
                        out.push_str(&transformed[1..]);
                    } else {
                        out.push_str(&transformed);
                    }
                    if out.len() > start
                        && let Some(highlight) = self.highlight(style, background)
                    {
                        highlights.push((start..out.len(), highlight));
                    }
                }
                NodeKind::Element(el) => {
                    if el.tag == "br" {
                        out.push('\n');
                        continue;
                    }
                    let Some(child) = self.ctx.computed.shown(nid) else {
                        continue;
                    };
                    self.collect(
                        &self.ctx.doc.nodes[nid].children,
                        child,
                        child.background_hsla.or(background),
                        out,
                        highlights,
                    );
                }
            }
        }
    }

    fn highlight(&self, style: &ComputedStyle, background: Option<Hsla>) -> Option<HighlightStyle> {
        if std::ptr::eq(style, self.base) {
            return None;
        }
        let base = self.base;
        let highlight = HighlightStyle {
            color: (style.color != base.color).then_some(style.color),
            font_weight: (style.font_weight != base.font_weight).then_some(style.font_weight),
            font_style: (style.font_style != base.font_style).then_some(style.font_style),
            background_color: background,
            underline: style.underline.then_some(UnderlineStyle {
                thickness: px(1.0),
                color: Some(style.color),
                wavy: false,
            }),
            strikethrough: style.line_through.then_some(StrikethroughStyle {
                thickness: px(1.0),
                color: Some(style.color),
            }),
            ..Default::default()
        };
        (highlight != HighlightStyle::default()).then_some(highlight)
    }
}

fn render_svg_element(
    node_id: NodeId,
    el: &ElementData,
    style: &ComputedStyle,
    ctx: &mut RenderCtx<'_>,
    cx: &App,
) -> Option<AnyElement> {
    let (content, natural) = match ctx.computed.picture(node_id)? {
        PictureOf::Svg { content, natural } => (*content, *natural),
        PictureOf::SvgTooLong => {
            return Some(picture_failed(&crate::tr!("html.svg_too_long"), cx));
        }
        PictureOf::DataImage { .. } => return None,
    };
    let (w, h) = fitted_media_size(natural, style, ctx.viewport_width);
    let shown = (w / natural.width).min(h / natural.height);
    let (key, scale) = RasterKey::svg(content, natural, shown);
    let found = ctx.media.find(key);
    ctx.frame.used.insert(key);
    if !ctx.media.holds(key) {
        ctx.frame.wanted.push(Wanted::Svg {
            node: node_id,
            key,
            scale,
        });
    }
    Some(match found {
        Some((held, RasterState::Ready(pic))) => {
            ctx.frame.used.insert(held);
            let label = match el.attr("aria-label") {
                Some(label) => label.to_owned(),
                None => crate::tr!("html.drawing").into_owned(),
            };
            render_raster_box(node_id, &pic, style, label, ctx)
        }
        Some((_, RasterState::Failed(error))) => picture_failed(&error, cx),
        None => {
            let mut placeholder = div().rounded(px(6.0));
            style.apply_to(placeholder.style(), ctx.viewport_width);
            fit_media_box(placeholder.style(), style, w, h);
            maybe_center_media(placeholder.into_any_element(), style)
        }
    })
}

/// What stands where a picture could not be drawn: the reason.
fn picture_failed(reason: &str, cx: &App) -> AnyElement {
    div()
        .p_2()
        .rounded(crate::material::radius_card(cx))
        .border_1()
        .border_color(cx.theme().border)
        .text_secondary(false, cx)
        .child(reason.to_owned())
        .into_any_element()
}

fn render_img_element(
    node_id: NodeId,
    el: &ElementData,
    style: &ComputedStyle,
    ctx: &mut RenderCtx<'_>,
    cx: &App,
) -> Option<AnyElement> {
    let alt = el.attr("alt").filter(|alt| !alt.is_empty());
    let Some(PictureOf::DataImage { content }) = ctx.computed.picture(node_id) else {
        // Only a picture written into the visual is drawn: nothing here
        // reads a file or an address. Its alternative text stands for it.
        return alt.map(|alt| {
            div()
                .px_2()
                .py_1()
                .rounded(px(4.0))
                .border_1()
                .border_color(ctx.theme.border)
                .text_secondary(false, cx)
                .child(alt.to_string())
                .into_any_element()
        });
    };
    let key = RasterKey::image(*content);
    let found = ctx.media.find(key);
    ctx.frame.used.insert(key);
    if !ctx.media.holds(key) {
        ctx.frame
            .wanted
            .push(Wanted::DataImage { node: node_id, key });
    }
    Some(match found {
        Some((_, RasterState::Ready(pic))) => {
            let label = match alt {
                Some(alt) => alt.to_owned(),
                None => crate::tr!("html.image").into_owned(),
            };
            render_raster_box(node_id, &pic, style, label, ctx)
        }
        Some((_, RasterState::Failed(error))) => picture_failed(&error, cx),
        None => div().w(px(120.0)).h(px(80.0)).into_any_element(),
    })
}

fn render_raster_box(
    node_id: NodeId,
    pic: &super::raster::Picture,
    style: &ComputedStyle,
    label: String,
    ctx: &mut RenderCtx<'_>,
) -> AnyElement {
    let natural = gpui::size(f32::from(pic.size().width), f32::from(pic.size().height));
    let (w, h) = fitted_media_size(natural, style, ctx.viewport_width);
    let source = pic.source();
    let viewer_source = source.clone();
    let clickable = w >= 64.0 || h >= 64.0;

    let mut image = img(source).size_full().object_fit(ObjectFit::Contain);
    if let Some(radius) = style.radius_tl {
        image.style().corner_radii.top_left = Some(radius);
        image.style().corner_radii.top_right = style.radius_tr.or(Some(radius));
        image.style().corner_radii.bottom_right = style.radius_br.or(Some(radius));
        image.style().corner_radii.bottom_left = style.radius_bl.or(Some(radius));
    }

    let mut wrapper = div();
    style.apply_to(wrapper.style(), ctx.viewport_width);
    fit_media_box(wrapper.style(), style, w, h);

    let element = if clickable {
        wrapper
            .id(("html-picture", node_id))
            .cursor_pointer()
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                crate::image_viewer::open(viewer_source.clone(), label.clone(), window, cx);
            })
            .child(image)
            .into_any_element()
    } else {
        wrapper.child(image).into_any_element()
    };
    maybe_center_media(element, style)
}

/// The width a box's left and right padding and border take of `width`.
fn horizontal_chrome(style: &ComputedStyle, width: f32) -> f32 {
    let padding = |length: Option<DefiniteLength>| match length {
        Some(DefiniteLength::Absolute(gpui::AbsoluteLength::Pixels(pixels))) => f32::from(pixels),
        Some(DefiniteLength::Absolute(gpui::AbsoluteLength::Rems(rems))) => {
            rems.0 * super::style::BASE_FONT_PX
        }
        Some(DefiniteLength::Fraction(share)) => width * share,
        None => 0.0,
    };
    let border = |length: Option<gpui::AbsoluteLength>| match length {
        Some(gpui::AbsoluteLength::Pixels(pixels)) => f32::from(pixels),
        Some(gpui::AbsoluteLength::Rems(rems)) => rems.0 * super::style::BASE_FONT_PX,
        None => 0.0,
    };
    (padding(style.padding_left)
        + padding(style.padding_right)
        + border(style.border_left_w)
        + border(style.border_right_w))
    .max(0.0)
}

/// The box of a picture fitted to `w` by `h`: as wide as the page asks, a
/// share of what holds it where the page gives a percentage, never wider than
/// what holds it, and as tall as its shape then makes it. The fitted size is
/// reckoned from the whole view's width, which a box nested in padding is
/// narrower than.
fn fit_media_box(target: &mut gpui::StyleRefinement, style: &ComputedStyle, w: f32, h: f32) {
    target.size.width = Some(match style.width {
        Some(Length::Definite(DefiniteLength::Fraction(share))) => relative(share).into(),
        _ => px(w).into(),
    });
    target.size.height = Some(Length::Auto);
    target.max_size.width = Some(relative(1.0).into());
    target.aspect_ratio = Some(w / h);
    target.flex_shrink = Some(1.0);
}

fn maybe_center_media(element: AnyElement, style: &ComputedStyle) -> AnyElement {
    if style.margin_auto_x || matches!(style.text_align, Some(TextAlign::Center)) {
        h_flex()
            .w_full()
            .justify_center()
            .child(element)
            .into_any_element()
    } else {
        element
    }
}

fn fitted_media_size(
    natural: gpui::Size<f32>,
    style: &ComputedStyle,
    viewport_width: f32,
) -> (f32, f32) {
    let nat_w = natural.width.max(1.0);
    let nat_h = natural.height.max(1.0);
    let aspect = nat_w / nat_h;
    let max_w = (viewport_width - 8.0).max(120.0);

    let resolve_len = |len: Option<Length>, base: f32| -> Option<f32> {
        match len? {
            Length::Definite(DefiniteLength::Absolute(gpui::AbsoluteLength::Pixels(p))) => {
                Some(f32::from(p))
            }
            Length::Definite(DefiniteLength::Absolute(gpui::AbsoluteLength::Rems(r))) => {
                Some(r.0 * super::style::BASE_FONT_PX)
            }
            Length::Definite(DefiniteLength::Fraction(f)) => Some(base * f),
            Length::Auto => None,
        }
    };

    let explicit_w = resolve_len(style.width, max_w);
    let explicit_h = resolve_len(style.height, 480.0);

    let (mut w, mut h) = match (explicit_w, explicit_h) {
        (Some(w), Some(h)) => (w, h),
        (Some(w), None) => (w, w / aspect),
        (None, Some(h)) => (h * aspect, h),
        (None, None) => (nat_w, nat_h),
    };

    if w > max_w {
        let scale = max_w / w;
        w = max_w;
        h *= scale;
    }
    (w.max(1.0), h.max(1.0))
}

/// The box a run of text set at `font_size` is drawn in. GPUI breaks a
/// word that is wider than its box wherever the box ends, `100%` into `100`
/// and `%`; a browser breaks only at a break opportunity and lets the word
/// overflow. So a run that has none is not wrapped, and one that has is no
/// narrower than its longest word, as near as that is known before the text
/// is set: three fifths of its size for each letter, its size for a CJK character.
fn text_box(text: &str, font_size: f32) -> gpui::Div {
    if !can_wrap(text) {
        return div().whitespace_nowrap();
    }
    let mut longest = 0.0f32;
    let mut word = 0.0f32;
    for c in text.chars() {
        if c.is_whitespace() {
            word = 0.0;
        } else if is_cjk(c) {
            word = 0.0;
            longest = longest.max(1.0);
        } else {
            word += 0.6;
            longest = longest.max(word);
        }
    }
    div().min_w(px(longest * font_size))
}

/// Whether `text` has anywhere a line may break in it.
fn can_wrap(text: &str) -> bool {
    text.trim().chars().any(|c| c.is_whitespace() || is_cjk(c))
}

fn normalize_text(text: &str, mode: WhiteSpaceMode) -> String {
    match mode {
        WhiteSpaceMode::Pre | WhiteSpaceMode::PreWrap => text.to_string(),
        WhiteSpaceMode::PreLine => {
            let mut out = String::with_capacity(text.len());
            let mut prev_space = false;
            for ch in text.chars() {
                if ch == '\n' {
                    out.push('\n');
                    prev_space = false;
                } else if ch.is_whitespace() && ch != '\u{00a0}' {
                    if !prev_space {
                        out.push(' ');
                        prev_space = true;
                    }
                } else {
                    out.push(ch);
                    prev_space = false;
                }
            }
            out
        }
        WhiteSpaceMode::Normal | WhiteSpaceMode::NoWrap => {
            let mut out = String::with_capacity(text.len());
            let mut prev_space = false;
            for ch in text.chars() {
                if ch.is_whitespace() && ch != '\u{00a0}' {
                    if !prev_space {
                        out.push(' ');
                        prev_space = true;
                    }
                } else {
                    out.push(ch);
                    prev_space = false;
                }
            }
            out
        }
    }
}

fn apply_text_transform(text: &str, transform: TextTransform) -> String {
    match transform {
        TextTransform::None => text.to_string(),
        TextTransform::Uppercase => text.to_uppercase(),
        TextTransform::Lowercase => text.to_lowercase(),
        TextTransform::Capitalize => {
            let mut out = String::with_capacity(text.len());
            let mut cap_next = true;
            for ch in text.chars() {
                if ch.is_whitespace() {
                    cap_next = true;
                    out.push(ch);
                } else if cap_next {
                    for uc in ch.to_uppercase() {
                        out.push(uc);
                    }
                    cap_next = false;
                } else {
                    out.push(ch);
                }
            }
            out
        }
    }
}
