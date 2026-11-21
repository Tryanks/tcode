//! Translates a parsed [`Document`] + [`Stylesheet`] into native GPUI elements.

use std::ops::Range;

use gpui::{
    AnyElement, App, DefiniteLength, Entity, HighlightStyle, InteractiveElement as _,
    IntoElement as _, Length, ObjectFit, Overflow, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, StrikethroughStyle, Styled as _, StyledImage as _, StyledText,
    TextAlign, UnderlineStyle, div, img, px, relative,
};
use gpui_base::{h_flex, v_flex};

use super::HtmlState;
use super::css::{MediaEnv, PseudoElement, Stylesheet};
use super::dom::{Document, ElementData, NodeId, NodeKind};
use super::style::{
    AccentBorder, AccentBorderSide, ComputedStyle, DisplayKind, GridColumns, GridTrack,
    ListStyleType, TextTransform, WhiteSpaceMode, compute_element_style, compute_pseudo_style,
};
use super::svg::{RasterState, data_image_key, prepare_svg};
use crate::theme::{ActiveTheme as _, Theme};

/// Pending async raster job discovered during element tree construction.
pub(crate) enum PendingMedia {
    Svg {
        key: u64,
        xml: String,
        natural: gpui::Size<f32>,
        target_width: f32,
    },
    DataImage {
        key: u64,
        uri: String,
    },
}

pub(crate) struct RenderCtx<'a> {
    pub(crate) doc: &'a Document,
    pub(crate) sheet: &'a Stylesheet,
    pub(crate) state: &'a Entity<HtmlState>,
    pub(crate) theme: &'a Theme,
    pub(crate) env: MediaEnv,
    /// Remaining available content width inside nested padded/bordered boxes.
    pub(crate) avail_width: f32,
    pub(crate) pending_media: Vec<PendingMedia>,
    pub(crate) id_seq: usize,
}

impl<'a> RenderCtx<'a> {
    fn next_id(&mut self, prefix: &str) -> SharedString {
        self.id_seq += 1;
        SharedString::from(format!("html-{prefix}-{}", self.id_seq))
    }
}

pub(crate) fn render_document(
    state: &Entity<HtmlState>,
    viewport_width: f32,
    cx: &App,
) -> (AnyElement, Vec<PendingMedia>) {
    let theme = cx.theme().clone();
    let env = MediaEnv {
        dark: theme.mode.is_dark(),
        viewport_width: viewport_width.max(240.0),
    };
    let read = state.read(cx);
    let doc = &read.doc;
    let sheet = &read.sheet;
    let root_style = ComputedStyle::root(&theme, sheet, env);

    let mut ctx = RenderCtx {
        doc,
        sheet,
        state,
        theme: &theme,
        env,
        avail_width: env.viewport_width,
        pending_media: Vec::new(),
        id_seq: 0,
    };

    let mut root = v_flex()
        .w_full()
        .min_w_0()
        .text_color(root_style.color)
        .font_family(root_style.font_family.clone())
        .text_size(px(root_style.font_size));

    let children = render_children(&doc.roots, &root_style, None, &mut ctx, cx);
    root = root.children(children);

    (root.into_any_element(), ctx.pending_media)
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
                let style = compute_element_style(
                    ctx.doc,
                    cid,
                    el,
                    parent_style,
                    ctx.sheet,
                    ctx.theme,
                    ctx.env,
                );
                if style.display == DisplayKind::None {
                    continue;
                }
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
                        list_counter += 1;
                    }
                    l
                });
                if let Some(rendered) = render_element(cid, el, &style, item_list, ctx, cx) {
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
    if style.display == DisplayKind::None {
        return None;
    }

    // Specialized semantic & media elements.
    match el.tag.as_str() {
        "svg" => return Some(render_svg_element(el, style, ctx, cx)),
        "img" => return render_img_element(el, style, ctx, cx),
        "hr" => return Some(render_hr(style, ctx)),
        "br" => return Some(div().h(px(style.font_size * 0.7)).into_any_element()),
        "progress" => return Some(render_progress(el, style, ctx)),
        "meter" => return Some(render_meter(el, style, ctx)),
        "input" => return Some(render_static_input(el, style, ctx)),
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
    style.apply_to(container.style(), ctx.avail_width);
    let accent_border = style.split_accent_border();
    if let Some(accent) = accent_border {
        // Clear the accent side on the outer box so the inner overlay strip
        // paints that side's distinct color without altering the other sides.
        let s = container.style();
        match accent.side {
            AccentBorderSide::Top => s.border_widths.top = Some(px(0.0).into()),
            AccentBorderSide::Right => s.border_widths.right = Some(px(0.0).into()),
            AccentBorderSide::Bottom => s.border_widths.bottom = Some(px(0.0).into()),
            AccentBorderSide::Left => s.border_widths.left = Some(px(0.0).into()),
        }
        s.overflow.x.get_or_insert(Overflow::Hidden);
        s.overflow.y.get_or_insert(Overflow::Hidden);
    }

    // Enable scrollable element ID when CSS explicitly requests overflow scrolling.
    let needs_scroll_id = matches!(style.overflow_x, Some(Overflow::Scroll))
        || matches!(style.overflow_y, Some(Overflow::Scroll));

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

    if let Some(before) = compute_pseudo_style(
        ctx.doc,
        node_id,
        style,
        ctx.sheet,
        ctx.theme,
        ctx.env,
        PseudoElement::Before,
    ) {
        container = container.child(render_pseudo(&before, ctx));
    }

    let prev_avail = ctx.avail_width;
    let horiz_chrome = horizontal_chrome_px(style, ctx.avail_width);
    ctx.avail_width = (ctx.avail_width - horiz_chrome).max(120.0);

    let children = render_children(
        &ctx.doc.nodes[node_id].children,
        style,
        next_list_ctx,
        ctx,
        cx,
    );
    ctx.avail_width = prev_avail;
    // CSS Grid `auto-fit` collapses empty tracks so fewer items than max columns
    // still stretch across the full container width (`auto-fill` keeps empty tracks).
    if matches!(style.display, DisplayKind::Grid | DisplayKind::InlineGrid)
        && matches!(style.grid_columns, Some(GridColumns::AutoFit { .. }))
        && !children.is_empty()
        && let Some(cols) = container.style().grid_cols.as_mut()
    {
        cols.repeat = cols.repeat.min(children.len() as u16).max(1);
    }
    container = container.children(children);

    if let Some(after) = compute_pseudo_style(
        ctx.doc,
        node_id,
        style,
        ctx.sheet,
        ctx.theme,
        ctx.env,
        PseudoElement::After,
    ) {
        container = container.child(render_pseudo(&after, ctx));
    }

    if needs_scroll_id {
        let id = ctx.next_id("scroll");
        Some(wrap_accent_border(
            container.id(id).into_any_element(),
            accent_border,
            style,
        ))
    } else {
        Some(wrap_accent_border(
            container.into_any_element(),
            accent_border,
            style,
        ))
    }
}

fn wrap_accent_border(
    inner: AnyElement,
    accent: Option<AccentBorder>,
    style: &ComputedStyle,
) -> AnyElement {
    let Some(accent) = accent else {
        return inner;
    };
    let mut shell = div().w_full().min_w_0().border_color(accent.color);
    let s = shell.style();
    match accent.side {
        AccentBorderSide::Top => s.border_widths.top = Some(accent.width),
        AccentBorderSide::Right => s.border_widths.right = Some(accent.width),
        AccentBorderSide::Bottom => s.border_widths.bottom = Some(accent.width),
        AccentBorderSide::Left => s.border_widths.left = Some(accent.width),
    }
    s.corner_radii.top_left = style.radius_tl;
    s.corner_radii.top_right = style.radius_tr;
    s.corner_radii.bottom_right = style.radius_br;
    s.corner_radii.bottom_left = style.radius_bl;
    shell.overflow_hidden().child(inner).into_any_element()
}

fn render_pseudo(style: &ComputedStyle, ctx: &mut RenderCtx<'_>) -> AnyElement {
    let text = style.content.clone().unwrap_or_default();
    let transformed = apply_text_transform(&text, style.text_transform);
    let mut el = div().flex_shrink_0();
    style.apply_to(el.style(), ctx.env.viewport_width);
    if !transformed.is_empty() {
        el = el.child(transformed);
    }
    el.into_any_element()
}

fn render_hr(style: &ComputedStyle, ctx: &RenderCtx<'_>) -> AnyElement {
    let mut el = div();
    style.apply_to(el.style(), ctx.env.viewport_width);
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
        style.apply_to(item.style(), ctx.env.viewport_width);
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
    style.apply_to(row.style(), ctx.env.viewport_width);
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
    style.apply_to(node.style(), ctx.env.viewport_width);
    if matches!(style.display, DisplayKind::Inline) {
        node.style().flex_direction = Some(gpui::FlexDirection::Row);
    }
    node = node.children(children);

    if let Some(url) = href {
        let id = ctx.next_id("link");
        node.id(id)
            .cursor_pointer()
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                cx.open_url(&url);
            })
            .into_any_element()
    } else {
        node.into_any_element()
    }
}

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

fn render_progress(el: &ElementData, style: &ComputedStyle, ctx: &RenderCtx<'_>) -> AnyElement {
    let max = el
        .attr("max")
        .and_then(|s| s.parse::<f32>().ok())
        .filter(|n| *n > 0.0)
        .unwrap_or(100.0);
    let value = el
        .attr("value")
        .and_then(|s| s.parse::<f32>().ok())
        .unwrap_or(0.0);
    let ratio = (value / max).clamp(0.0, 1.0);
    render_bar(ratio, style, ctx.theme.primary, ctx)
}

fn render_meter(el: &ElementData, style: &ComputedStyle, ctx: &RenderCtx<'_>) -> AnyElement {
    let min = el
        .attr("min")
        .and_then(|s| s.parse::<f32>().ok())
        .unwrap_or(0.0);
    let max = el
        .attr("max")
        .and_then(|s| s.parse::<f32>().ok())
        .filter(|n| *n > min)
        .unwrap_or(1.0);
    let value = el
        .attr("value")
        .and_then(|s| s.parse::<f32>().ok())
        .unwrap_or(min);
    let ratio = ((value - min) / (max - min)).clamp(0.0, 1.0);
    let fill = if let Some(low) = el.attr("low").and_then(|s| s.parse::<f32>().ok())
        && value < low
    {
        ctx.theme.warning
    } else if let Some(high) = el.attr("high").and_then(|s| s.parse::<f32>().ok())
        && value > high
    {
        ctx.theme.danger
    } else {
        ctx.theme.success
    };
    render_bar(ratio, style, fill, ctx)
}

fn render_bar(
    ratio: f32,
    style: &ComputedStyle,
    default_fill: gpui::Hsla,
    ctx: &RenderCtx<'_>,
) -> AnyElement {
    let mut track = div()
        .w(px(120.0))
        .h(px(8.0))
        .rounded(px(999.0))
        .bg(ctx.theme.tokens.colors.muted)
        .overflow_hidden();
    style.apply_to(track.style(), ctx.env.viewport_width);
    let fill_color = style.color;
    let bar_color = if fill_color == ctx.theme.foreground {
        default_fill
    } else {
        fill_color
    };
    track
        .child(
            div()
                .h_full()
                .w(relative(ratio))
                .rounded(px(999.0))
                .bg(bar_color),
        )
        .into_any_element()
}

fn render_static_input(el: &ElementData, style: &ComputedStyle, ctx: &RenderCtx<'_>) -> AnyElement {
    let kind = el.attr("type").unwrap_or("text").to_ascii_lowercase();
    let checked = el.attr("checked").is_some();
    match kind.as_str() {
        "checkbox" | "radio" => {
            let radius = if kind == "radio" { px(999.0) } else { px(3.0) };
            let mut box_el = div()
                .w(px(14.0))
                .h(px(14.0))
                .flex_shrink_0()
                .rounded(radius)
                .border_1()
                .border_color(if checked {
                    ctx.theme.primary
                } else {
                    ctx.theme.border
                })
                .bg(if checked {
                    ctx.theme.primary
                } else {
                    ctx.theme.background
                })
                .flex()
                .items_center()
                .justify_center();
            if checked {
                box_el = box_el.child(
                    div()
                        .text_size(px(10.0))
                        .text_color(ctx.theme.primary_foreground)
                        .child(if kind == "radio" { "•" } else { "✓" }),
                );
            }
            box_el.into_any_element()
        }
        "range" => {
            let min = el
                .attr("min")
                .and_then(|s| s.parse::<f32>().ok())
                .unwrap_or(0.0);
            let max = el
                .attr("max")
                .and_then(|s| s.parse::<f32>().ok())
                .filter(|n| *n > min)
                .unwrap_or(100.0);
            let val = el
                .attr("value")
                .and_then(|s| s.parse::<f32>().ok())
                .unwrap_or((min + max) * 0.5);
            render_bar(
                ((val - min) / (max - min)).clamp(0.0, 1.0),
                style,
                ctx.theme.primary,
                ctx,
            )
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
            style.apply_to(field.style(), ctx.env.viewport_width);
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
            let child_style =
                compute_element_style(ctx.doc, cid, el, style, ctx.sheet, ctx.theme, ctx.env);
            if child_style.display == DisplayKind::None {
                continue;
            }
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
            if let Some(rendered) = render_element(cid, el, &child_style, None, ctx, cx) {
                items.push((span, rendered));
            }
        }
    }

    let mut outer = v_flex().w_full().min_w_0();
    style.apply_to(outer.style(), ctx.env.viewport_width);
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
    let mut caption_el: Option<AnyElement> = None;
    let mut rows: Vec<(NodeId, ComputedStyle, bool)> = Vec::new();

    for &cid in &ctx.doc.nodes[node_id].children {
        if let NodeKind::Element(el) = &ctx.doc.nodes[cid].kind {
            let cs =
                compute_element_style(ctx.doc, cid, el, table_style, ctx.sheet, ctx.theme, ctx.env);
            if cs.display == DisplayKind::None {
                continue;
            }
            match el.tag.as_str() {
                "caption" => {
                    caption_el = render_element(cid, el, &cs, None, ctx, cx);
                }
                "thead" | "tbody" | "tfoot" => {
                    let is_head = el.tag == "thead";
                    for &rid in &ctx.doc.nodes[cid].children {
                        if let NodeKind::Element(rel) = &ctx.doc.nodes[rid].kind
                            && rel.tag == "tr"
                        {
                            let rs = compute_element_style(
                                ctx.doc, rid, rel, &cs, ctx.sheet, ctx.theme, ctx.env,
                            );
                            if rs.display != DisplayKind::None {
                                rows.push((rid, rs, is_head));
                            }
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
    table_style.apply_to(table.style(), ctx.env.viewport_width);
    table.style().display = Some(gpui::Display::Flex);
    table.style().flex_direction = Some(gpui::FlexDirection::Column);

    let row_count = rows.len();
    for (row_idx, (rid, row_style, is_head)) in rows.into_iter().enumerate() {
        let mut row_div = h_flex().w_full().min_w_0().items_stretch();
        row_style.apply_to(row_div.style(), ctx.env.viewport_width);
        row_div.style().display = Some(gpui::Display::Flex);
        row_div.style().flex_direction = Some(gpui::FlexDirection::Row);

        // Subtle default row divider if author did not explicitly clear borders.
        if row_style.border_bottom_w.is_none() && (is_head || row_idx + 1 < row_count) {
            row_div = row_div
                .border_b_1()
                .border_color(ctx.theme.border.opacity(if is_head { 0.9 } else { 0.5 }));
        }
        if is_head && row_style.background.is_none() {
            row_div = row_div.bg(ctx.theme.tokens.colors.muted.opacity(0.6));
        }

        for &cell_id in &ctx.doc.nodes[rid].children {
            if let NodeKind::Element(cel) = &ctx.doc.nodes[cell_id].kind
                && matches!(cel.tag.as_str(), "td" | "th")
            {
                let cell_style = compute_element_style(
                    ctx.doc, cell_id, cel, &row_style, ctx.sheet, ctx.theme, ctx.env,
                );
                if cell_style.display == DisplayKind::None {
                    continue;
                }
                let colspan = cel
                    .attr("colspan")
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(1)
                    .clamp(1, 24);
                let mut cell_div = v_flex().min_w_0().justify_center();
                cell_style.apply_to(cell_div.style(), ctx.env.viewport_width);
                if cell_style.width.is_none() {
                    cell_div.style().flex_grow = Some(colspan as f32);
                    cell_div.style().flex_shrink = Some(1.0);
                    cell_div.style().flex_basis = Some(px(0.0).into());
                } else {
                    cell_div.style().flex_shrink = Some(0.0);
                }
                let children =
                    render_children(&ctx.doc.nodes[cell_id].children, &cell_style, None, ctx, cx);
                row_div = row_div.child(cell_div.children(children));
            }
        }
        table = table.child(row_div);
    }

    let scroll_id = ctx.next_id("table");
    let mut wrapper = v_flex().w_full().min_w_0();
    if let Some(cap) = caption_el {
        wrapper = wrapper.child(cap);
    }
    wrapper
        .child(
            div()
                .id(scroll_id)
                .w_full()
                .min_w_0()
                .overflow_x_scroll()
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
        collect_styled_text(nodes, parent_style, ctx, &mut text, &mut highlights);
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
                let style = compute_element_style(
                    ctx.doc,
                    nid,
                    el,
                    parent_style,
                    ctx.sheet,
                    ctx.theme,
                    ctx.env,
                );
                if style.display == DisplayKind::None {
                    continue;
                }
                if let Some(child_el) = render_element(nid, el, &style, None, ctx, cx) {
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
                let style = compute_element_style(
                    ctx.doc,
                    nid,
                    el,
                    parent_style,
                    ctx.sheet,
                    ctx.theme,
                    ctx.env,
                );
                if !style.display.is_pure_inline() || style.has_inline_box() {
                    return false;
                }
                if (style.font_size - parent_style.font_size).abs() > 0.5
                    || style.font_family != parent_style.font_family
                {
                    return false;
                }
                if compute_pseudo_style(
                    ctx.doc,
                    nid,
                    &style,
                    ctx.sheet,
                    ctx.theme,
                    ctx.env,
                    PseudoElement::Before,
                )
                .is_some()
                    || compute_pseudo_style(
                        ctx.doc,
                        nid,
                        &style,
                        ctx.sheet,
                        ctx.theme,
                        ctx.env,
                        PseudoElement::After,
                    )
                    .is_some()
                {
                    return false;
                }
                if !can_flatten_inline(&ctx.doc.nodes[nid].children, &style, ctx) {
                    return false;
                }
            }
        }
    }
    true
}

fn collect_styled_text(
    nodes: &[NodeId],
    parent_style: &ComputedStyle,
    ctx: &RenderCtx<'_>,
    out: &mut String,
    highlights: &mut Vec<(Range<usize>, HighlightStyle)>,
) {
    for &nid in nodes {
        match &ctx.doc.nodes[nid].kind {
            NodeKind::Text(t) => {
                let norm = normalize_text(t, parent_style.white_space);
                let transformed = apply_text_transform(&norm, parent_style.text_transform);
                // Collapse duplicate boundary space across adjacent inline nodes in normal mode.
                if matches!(
                    parent_style.white_space,
                    WhiteSpaceMode::Normal | WhiteSpaceMode::NoWrap
                ) && out.ends_with(' ')
                    && transformed.starts_with(' ')
                {
                    out.push_str(&transformed[1..]);
                } else {
                    out.push_str(&transformed);
                }
            }
            NodeKind::Element(el) => {
                if el.tag == "br" {
                    out.push('\n');
                    continue;
                }
                let style = compute_element_style(
                    ctx.doc,
                    nid,
                    el,
                    parent_style,
                    ctx.sheet,
                    ctx.theme,
                    ctx.env,
                );
                if style.display == DisplayKind::None {
                    continue;
                }
                let start = out.len();
                collect_styled_text(&ctx.doc.nodes[nid].children, &style, ctx, out, highlights);
                let end = out.len();
                if end > start {
                    let mut hl = HighlightStyle::default();
                    let mut any = false;
                    if style.color != parent_style.color {
                        hl.color = Some(style.color);
                        any = true;
                    }
                    if style.font_weight != parent_style.font_weight {
                        hl.font_weight = Some(style.font_weight);
                        any = true;
                    }
                    if style.font_style != parent_style.font_style {
                        hl.font_style = Some(style.font_style);
                        any = true;
                    }
                    if let Some(bg) = style.background_hsla {
                        hl.background_color = Some(bg);
                        any = true;
                    }
                    if style.underline {
                        hl.underline = Some(UnderlineStyle {
                            thickness: px(1.0),
                            color: Some(style.color),
                            wavy: false,
                        });
                        any = true;
                    }
                    if style.line_through {
                        hl.strikethrough = Some(StrikethroughStyle {
                            thickness: px(1.0),
                            color: Some(style.color),
                        });
                        any = true;
                    }
                    if any {
                        highlights.push((start..end, hl));
                    }
                }
            }
        }
    }
}

fn render_svg_element(
    el: &ElementData,
    style: &ComputedStyle,
    ctx: &mut RenderCtx<'_>,
    cx: &App,
) -> AnyElement {
    let raw = el.raw_svg.as_deref().unwrap_or_default();
    let prepared = prepare_svg(
        raw,
        style.color,
        &style.vars,
        &ctx.doc.stylesheets,
        ctx.theme.font_family.as_ref(),
    );
    let key = prepared.key;
    let natural = prepared.natural;

    let raster = ctx.state.read(cx).media.get(key).cloned();
    match raster {
        Some(RasterState::Ready(pic)) => {
            render_raster_box(&pic, style, el.attr("aria-label").unwrap_or("SVG"), ctx)
        }
        Some(RasterState::Failed(err)) => div()
            .p_2()
            .rounded(px(6.0))
            .border_1()
            .border_color(ctx.theme.border)
            .text_size(px(12.0))
            .text_color(ctx.theme.muted_foreground)
            .child(format!("SVG render error: {err}"))
            .into_any_element(),
        None => {
            let (w, h) = fitted_media_size(natural, style, ctx.avail_width);
            ctx.pending_media.push(PendingMedia::Svg {
                key,
                xml: prepared.xml,
                natural,
                target_width: w,
            });
            let mut placeholder = div().max_w_full().w(px(w)).h(px(h)).rounded(px(6.0));
            style.apply_to(placeholder.style(), ctx.env.viewport_width);
            maybe_center_media(placeholder.into_any_element(), style)
        }
    }
}

fn render_img_element(
    el: &ElementData,
    style: &ComputedStyle,
    ctx: &mut RenderCtx<'_>,
    cx: &App,
) -> Option<AnyElement> {
    let src = el.attr("src")?;
    if !src.starts_with("data:") {
        // Static renderer never fetches external network URLs; show alt text if present.
        let alt = el.attr("alt").unwrap_or("");
        if alt.is_empty() {
            return None;
        }
        return Some(
            div()
                .px_2()
                .py_1()
                .rounded(px(4.0))
                .border_1()
                .border_color(ctx.theme.border)
                .text_size(px(12.0))
                .text_color(ctx.theme.muted_foreground)
                .child(alt.to_string())
                .into_any_element(),
        );
    }

    let key = data_image_key(src);
    match ctx.state.read(cx).media.get(key).cloned() {
        Some(RasterState::Ready(pic)) => Some(render_raster_box(
            &pic,
            style,
            el.attr("alt").unwrap_or("Image"),
            ctx,
        )),
        Some(RasterState::Failed(_)) => el.attr("alt").map(|alt| {
            div()
                .text_size(px(12.0))
                .text_color(ctx.theme.muted_foreground)
                .child(alt.to_string())
                .into_any_element()
        }),
        None => {
            ctx.pending_media.push(PendingMedia::DataImage {
                key,
                uri: src.to_string(),
            });
            Some(div().w(px(120.0)).h(px(80.0)).into_any_element())
        }
    }
}

fn render_raster_box(
    pic: &super::svg::RasterPicture,
    style: &ComputedStyle,
    label: &str,
    ctx: &mut RenderCtx<'_>,
) -> AnyElement {
    let natural = gpui::size(f32::from(pic.size().width), f32::from(pic.size().height));
    let (w, h) = fitted_media_size(natural, style, ctx.avail_width);
    let aspect = (w / h).max(0.05);
    let fill_width = matches!(
        style.width,
        Some(Length::Definite(DefiniteLength::Fraction(f))) if (f - 1.0).abs() < 0.01
    );
    let source = pic.source();
    let viewer_source = source.clone();
    let viewer_label = label.to_string();
    let clickable = w >= 64.0 || h >= 64.0;

    let mut image = if fill_width {
        img(source)
            .w_full()
            .max_w_full()
            .aspect_ratio(aspect)
            .object_fit(ObjectFit::Contain)
    } else {
        img(source)
            .w(px(w))
            .max_w_full()
            .h(px(h))
            .object_fit(ObjectFit::Contain)
            .flex_shrink_0()
    };
    if let Some(radius) = style.radius_tl {
        image.style().corner_radii.top_left = Some(radius);
        image.style().corner_radii.top_right = style.radius_tr.or(Some(radius));
        image.style().corner_radii.bottom_right = style.radius_br.or(Some(radius));
        image.style().corner_radii.bottom_left = style.radius_bl.or(Some(radius));
    }

    let mut wrapper = div().min_w_0().max_w_full();
    style.apply_to(wrapper.style(), ctx.env.viewport_width);
    if fill_width {
        wrapper.style().size.width = Some(relative(1.0).into());
        wrapper.style().size.height = None;
    } else {
        wrapper.style().flex_shrink = Some(0.0);
        wrapper.style().size.width = Some(px(w).into());
        wrapper.style().size.height = Some(px(h).into());
    }

    let element = if clickable {
        let id = ctx.next_id("svg");
        wrapper
            .id(id)
            .cursor_pointer()
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                crate::image_viewer::open(viewer_source.clone(), viewer_label.clone(), window, cx);
            })
            .child(image)
            .into_any_element()
    } else {
        wrapper.child(image).into_any_element()
    };
    maybe_center_media(element, style)
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

fn horizontal_chrome_px(style: &ComputedStyle, avail: f32) -> f32 {
    let def_px = |len: Option<DefiniteLength>| match len {
        Some(DefiniteLength::Absolute(gpui::AbsoluteLength::Pixels(p))) => f32::from(p).max(0.0),
        Some(DefiniteLength::Absolute(gpui::AbsoluteLength::Rems(r))) => {
            (r.0 * super::style::BASE_FONT_PX).max(0.0)
        }
        Some(DefiniteLength::Fraction(f)) => (avail * f).max(0.0),
        None => 0.0,
    };
    let abs_px = |len: Option<gpui::AbsoluteLength>| match len {
        Some(gpui::AbsoluteLength::Pixels(p)) => f32::from(p).max(0.0),
        Some(gpui::AbsoluteLength::Rems(r)) => (r.0 * super::style::BASE_FONT_PX).max(0.0),
        None => 0.0,
    };
    def_px(style.padding_left)
        + def_px(style.padding_right)
        + abs_px(style.border_left_w)
        + abs_px(style.border_right_w)
}

fn fitted_media_size(
    natural: gpui::Size<f32>,
    style: &ComputedStyle,
    avail_width: f32,
) -> (f32, f32) {
    let nat_w = natural.width.max(1.0);
    let nat_h = natural.height.max(1.0);
    let aspect = nat_w / nat_h;
    let max_w = avail_width.max(120.0);

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
