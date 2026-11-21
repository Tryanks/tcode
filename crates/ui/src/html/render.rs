//! Builds a frame's GPUI elements from a parsed [`Document`] and its
//! [`Computed`] styles.

use std::collections::HashSet;
use std::ops::Range;

use gpui::{
    AnyElement, App, DefiniteLength, HighlightStyle, Hsla, InteractiveElement as _,
    IntoElement as _, Length, ObjectFit, Overflow, ParentElement as _,
    StatefulInteractiveElement as _, StrikethroughStyle, Styled as _, StyledImage as _, StyledText,
    TextAlign, UnderlineStyle, div, img, px, relative,
};
use gpui_base::{h_flex, v_flex};

use super::dom::{Document, ElementData, MAX_DEPTH, MAX_NODES, NodeId, NodeKind};
use super::style::{
    ComputedStyle, DisplayKind, GridColumns, GridTrack, ListStyleType, TextTransform,
    WhiteSpaceMode,
};
use super::svg::{MediaSlots, RasterKey, RasterState};
use super::{Computed, HtmlState, PictureOf};
use crate::scroll::ScrollableElement as _;
use crate::theme::{ActiveTheme as _, Theme};
use crate::typography::TextRole as _;
use crate::widgets::checkbox::Checkbox;
use crate::widgets::progress::Progress;
use crate::widgets::tooltip::{Tooltip, TooltipTrigger as _};

/// HTML's own limit on `colspan`.
const MAX_COLSPAN: usize = 1000;

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
}

struct RenderCtx<'a> {
    doc: &'a Document,
    computed: &'a Computed,
    media: &'a MediaSlots,
    theme: &'a Theme,
    viewport_width: f32,
    frame: Frame,
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

    let is_flex_or_grid = matches!(
        parent_style.display,
        DisplayKind::Flex | DisplayKind::InlineFlex | DisplayKind::Grid | DisplayKind::InlineGrid
    );

    for &cid in child_ids {
        match &ctx.doc.nodes[cid].kind {
            NodeKind::Text(t) => {
                if is_flex_or_grid {
                    let normalized = normalize_text(t, parent_style.white_space);
                    if !normalized.trim().is_empty() {
                        let transformed =
                            apply_text_transform(&normalized, parent_style.text_transform);
                        out.push(div().child(transformed).into_any_element());
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
                if let Some(rendered) = render_element(cid, el, style, item_list, ctx, cx) {
                    out.push(rendered);
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

fn render_element(
    node_id: NodeId,
    el: &ElementData,
    style: &ComputedStyle,
    list_ctx: Option<ListContext>,
    ctx: &mut RenderCtx<'_>,
    cx: &App,
) -> Option<AnyElement> {
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
        return Some(render_track_grid(node_id, style, tracks, ctx, cx));
    }

    let mut container = div().min_w_0();
    style.apply_to(container.style(), ctx.viewport_width);
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

    let mut children = Vec::new();
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
    let children = render_children(&ctx.doc.nodes[node_id].children, style, None, ctx, cx);
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
    node.id(("html-link", node_id))
        .relative()
        .cursor_pointer()
        .on_click(move |_, _, cx| {
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
fn render_track_grid(
    node_id: NodeId,
    style: &ComputedStyle,
    tracks: &[GridTrack],
    ctx: &mut RenderCtx<'_>,
    cx: &App,
) -> AnyElement {
    let col_count = tracks.len().max(1);
    let mut items: Vec<(usize, AnyElement)> = Vec::new();

    for &cid in &ctx.doc.nodes[node_id].children {
        if let NodeKind::Element(el) = &ctx.doc.nodes[cid].kind {
            let Some(child_style) = ctx.computed.shown(cid) else {
                continue;
            };
            let span = child_style.grid_location.as_ref().map_or(1, |loc| {
                match (&loc.column.start, &loc.column.end) {
                    (gpui::GridPlacement::Span(n), _) | (_, gpui::GridPlacement::Span(n)) => {
                        (*n as usize).clamp(1, col_count)
                    }
                    (gpui::GridPlacement::Line(s), gpui::GridPlacement::Line(e)) if *e > *s => {
                        ((*e - *s) as usize).clamp(1, col_count)
                    }
                    _ => 1,
                }
            });
            if let Some(rendered) = render_element(cid, el, child_style, None, ctx, cx) {
                items.push((span, rendered));
            }
        }
    }

    let mut outer = v_flex().w_full().min_w_0();
    style.apply_to(outer.style(), ctx.viewport_width);
    outer.style().display = Some(gpui::Display::Flex);
    outer.style().flex_direction = Some(gpui::FlexDirection::Column);

    let mut cursor = 0usize;
    let mut current_row = make_track_row(style);

    for (span, child) in items {
        let span = span.min(col_count);
        if cursor > 0 && cursor + span > col_count {
            outer = outer.child(current_row);
            current_row = make_track_row(style);
            cursor = 0;
        }
        let mut cell = div().min_w_0();
        apply_track_span(
            cell.style(),
            &tracks[cursor..(cursor + span).min(col_count)],
        );
        current_row = current_row.child(cell.child(child));
        cursor += span;
        if cursor >= col_count {
            outer = outer.child(current_row);
            current_row = make_track_row(style);
            cursor = 0;
        }
    }
    if cursor > 0 {
        outer = outer.child(current_row);
    }

    outer.into_any_element()
}

fn make_track_row(style: &ComputedStyle) -> gpui::Div {
    let mut row = h_flex().w_full().min_w_0();
    if let Some(gap) = style.col_gap {
        row.style().gap.width = Some(gap);
    }
    if let Some(ai) = style.align_items {
        row.style().align_items = Some(ai);
    }
    row
}

fn apply_track_span(style: &mut gpui::StyleRefinement, slice: &[GridTrack]) {
    let mut total_fr = 0.0f32;
    let mut total_px = 0.0f32;
    let mut total_pct = 0.0f32;
    let mut has_auto = false;
    for tr in slice {
        match tr {
            GridTrack::Fr(f) => total_fr += *f,
            GridTrack::Px(p) => total_px += *p,
            GridTrack::Percent(pct) => total_pct += *pct,
            GridTrack::Auto => has_auto = true,
        }
    }
    if total_fr > 0.0 {
        style.flex_grow = Some(total_fr);
        style.flex_shrink = Some(1.0);
        style.flex_basis = Some(px(0.0).into());
    } else if total_pct > 0.0 {
        style.size.width = Some(relative(total_pct.clamp(0.0, 1.0)).into());
        style.flex_shrink = Some(0.0);
    } else if total_px > 0.0 {
        style.size.width = Some(px(total_px).into());
        style.flex_shrink = Some(0.0);
    } else if has_auto {
        style.flex_grow = Some(1.0);
    }
}

/// Native GPUI HTML `<table>` renderer with header styling, borders, `colspan`,
/// caption support, and horizontal scrolling for wide tables.
fn render_table(
    node_id: NodeId,
    table_style: &ComputedStyle,
    ctx: &mut RenderCtx<'_>,
    cx: &App,
) -> AnyElement {
    let computed = ctx.computed;
    let mut caption_el: Option<AnyElement> = None;
    let mut rows: Vec<(NodeId, &ComputedStyle, bool)> = Vec::new();

    for &cid in &ctx.doc.nodes[node_id].children {
        if let NodeKind::Element(el) = &ctx.doc.nodes[cid].kind {
            let Some(cs) = computed.shown(cid) else {
                continue;
            };
            match el.tag.as_str() {
                "caption" => {
                    caption_el = render_element(cid, el, cs, None, ctx, cx);
                }
                "thead" | "tbody" | "tfoot" => {
                    let is_head = el.tag == "thead";
                    for &rid in &ctx.doc.nodes[cid].children {
                        if ctx.doc.element(rid).is_some_and(|row| row.tag == "tr")
                            && let Some(rs) = computed.shown(rid)
                        {
                            rows.push((rid, rs, is_head));
                        }
                    }
                }
                "tr" => {
                    rows.push((cid, cs, false));
                }
                _ => {}
            }
        }
    }

    let mut table = v_flex().w_full().min_w_0();
    table_style.apply_to(table.style(), ctx.viewport_width);
    table.style().display = Some(gpui::Display::Flex);
    table.style().flex_direction = Some(gpui::FlexDirection::Column);

    let row_count = rows.len();
    for (row_idx, (rid, row_style, is_head)) in rows.into_iter().enumerate() {
        let mut row_div = h_flex().w_full().min_w_0().items_stretch();
        row_style.apply_to(row_div.style(), ctx.viewport_width);
        row_div.style().display = Some(gpui::Display::Flex);
        row_div.style().flex_direction = Some(gpui::FlexDirection::Row);

        // Subtle default row divider if author did not explicitly clear borders.
        if row_style.border_bottom_w.is_none() && (is_head || row_idx + 1 < row_count) {
            row_div = row_div
                .border_b_1()
                .border_color(ctx.theme.border.opacity(if is_head { 0.9 } else { 0.5 }));
        }
        if is_head && row_style.background.is_none() {
            row_div = row_div.bg(ctx.theme.muted.opacity(0.6));
        }

        for &cell_id in &ctx.doc.nodes[rid].children {
            if let NodeKind::Element(cel) = &ctx.doc.nodes[cell_id].kind
                && matches!(cel.tag.as_str(), "td" | "th")
                && let Some(cell_style) = computed.shown(cell_id)
            {
                let colspan = cel
                    .attr("colspan")
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(1)
                    .clamp(1, MAX_COLSPAN);
                let mut cell_div = v_flex().min_w_0().justify_center();
                cell_style.apply_to(cell_div.style(), ctx.viewport_width);
                if cell_style.width.is_none() {
                    cell_div.style().flex_grow = Some(colspan as f32);
                    cell_div.style().flex_shrink = Some(1.0);
                    cell_div.style().flex_basis = Some(px(0.0).into());
                } else {
                    cell_div.style().flex_shrink = Some(0.0);
                }
                let children =
                    render_children(&ctx.doc.nodes[cell_id].children, cell_style, None, ctx, cx);
                row_div = row_div.child(cell_div.children(children));
            }
        }
        table = table.child(row_div);
    }

    v_flex()
        .w_full()
        .min_w_0()
        .children(caption_el)
        .child(
            div()
                .id(("html-table", node_id))
                .w_full()
                .min_w_0()
                .overflow_x_scroll_area()
                .child(table),
        )
        .into_any_element()
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
        let styled = if highlights.is_empty() {
            div().w_full().min_w_0().child(text)
        } else {
            div()
                .w_full()
                .min_w_0()
                .child(StyledText::new(text).with_highlights(highlights))
        };
        return Some(styled.into_any_element());
    }

    // Rich inline flow (contains badges, inline-blocks, links, inline SVGs, etc.)
    let mut row = h_flex().w_full().min_w_0().flex_wrap().items_center();
    match parent_style.text_align {
        Some(TextAlign::Center) => row = row.justify_center(),
        Some(TextAlign::Right) => row = row.justify_end(),
        _ => {}
    }
    let mut any_visible = false;
    for &nid in nodes {
        match &ctx.doc.nodes[nid].kind {
            NodeKind::Text(t) => {
                let norm = normalize_text(t, parent_style.white_space);
                if !norm.trim().is_empty() {
                    let transformed = apply_text_transform(&norm, parent_style.text_transform);
                    row = row.child(div().child(transformed));
                    any_visible = true;
                }
            }
            NodeKind::Element(el) => {
                if el.tag == "br" {
                    row = row.child(div().w(relative(1.0)).h(px(0.0)));
                    any_visible = true;
                    continue;
                }
                let Some(style) = ctx.computed.shown(nid) else {
                    continue;
                };
                if let Some(child_el) = render_element(nid, el, style, None, ctx, cx) {
                    row = row.child(child_el);
                    any_visible = true;
                }
            }
        }
    }
    any_visible.then(|| row.into_any_element())
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
