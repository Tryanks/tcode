//! Selectable rich-text element adapted from gpui-component's Apache-2.0
//! `text/inline.rs` implementation.

use std::{
    ops::Range,
    rc::Rc,
    sync::{Arc, Mutex},
};

use crate::theme::ActiveTheme as _;
use crate::widgets::tooltip::Tooltip;
use gpui::{
    App, BorderStyle, Bounds, CursorStyle, Edges, Element, ElementId, Entity, GlobalElementId,
    HighlightStyle, Hitbox, HitboxBehavior, InspectorElementId, InteractiveText, LayoutId,
    LongPressEvent, MouseButton, MouseDownEvent, MouseUpEvent, Pixels, Point, SharedString,
    StyledText, TextLayout, TextRun, TextStyle, TouchPhase, Window, point, px, quad,
};

use super::{
    nodes::LinkMark,
    selection::RunOrigin,
    state::{MarkdownState, PaintedRun, PendingContextTarget, PendingLinkMenu},
};

/// The text a run paints, retained by the parsed IR.
#[derive(Debug, Default, PartialEq)]
pub(super) struct InlineState {
    pub(super) text: SharedString,
}

impl InlineState {
    pub(super) fn shared(text: SharedString) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self { text }))
    }

    pub(super) fn set_text(&mut self, text: SharedString) {
        self.text = text;
    }
}

/// All selectable text, including code-block lines, is painted through this element.
pub(super) struct Inline {
    id: ElementId,
    math_glyph: Option<(gpui::GlyphId, Pixels)>,
    view: Entity<MarkdownState>,
    text: SharedString,
    links: Rc<Vec<(Range<usize>, LinkMark)>>,
    highlights: Vec<(Range<usize>, HighlightStyle)>,
    font_overrides: Vec<(Range<usize>, SharedString)>,
    interactive_text: InteractiveText,
    text_layout: TextLayout,
    /// Where the text sits in the document; text without one is not
    /// selectable.
    origin: Option<RunOrigin>,
    /// The part of the text selected, as of prepaint.
    selected: Option<Range<usize>>,
    /// Each row of each link, as of prepaint.
    link_rows: Rc<Vec<LinkRow>>,
}

/// A row of a link: which link, its hitbox over the characters of the link
/// on the row, and where the link's tooltip is placed beside.
struct LinkRow {
    link: usize,
    hitbox: Hitbox,
    placement: Bounds<Pixels>,
}

impl Inline {
    pub(super) fn new(
        id: impl Into<ElementId>,
        view: Entity<MarkdownState>,
        state: Arc<Mutex<InlineState>>,
        links: Vec<(Range<usize>, LinkMark)>,
        highlights: Vec<(Range<usize>, HighlightStyle)>,
        font_overrides: Vec<(Range<usize>, SharedString)>,
    ) -> Self {
        let id = id.into();
        let text = state.lock().unwrap().text.clone();
        let styled_text = StyledText::new(text.clone());
        let text_layout = styled_text.layout().clone();
        Self {
            id: id.clone(),
            math_glyph: None,
            view,
            text: text.clone(),
            links: Rc::new(links),
            highlights,
            font_overrides,
            interactive_text: InteractiveText::new(id, styled_text),
            text_layout,
            origin: None,
            selected: None,
            link_rows: Rc::default(),
        }
    }

    pub(super) fn origin(mut self, origin: Option<RunOrigin>) -> Self {
        self.origin = origin;
        self
    }

    pub(super) fn math_glyph(mut self, glyph: gpui::GlyphId, baseline: Pixels) -> Self {
        self.math_glyph = Some((glyph, baseline));
        self
    }

    fn link_for_position(
        layout: &TextLayout,
        links: &[(Range<usize>, LinkMark)],
        position: Point<Pixels>,
    ) -> Option<LinkMark> {
        Self::link_and_range_for_position(layout, links, position).map(|(_, link)| link)
    }

    /// Record the link at `position`, if any, for the view's context menu.
    /// A press beside a link keeps the enclosing block's target (a code
    /// block's).
    fn record_link(
        view: &Entity<MarkdownState>,
        layout: &TextLayout,
        links: &[(Range<usize>, LinkMark)],
        text: &SharedString,
        position: Point<Pixels>,
        cx: &mut App,
    ) {
        let Some((range, link)) = Self::link_and_range_for_position(layout, links, position) else {
            return;
        };
        let pending = PendingContextTarget::Link(PendingLinkMenu {
            target: view.read(cx).resolve_link(&link.url, cx),
            text: text.get(range).map(SharedString::from).unwrap_or_default(),
            raw_url: link.url,
        });
        view.update(cx, |state, cx| state.set_pending_context(Some(pending), cx));
    }

    /// The rows `range` is laid out on: on each, where
    /// [`TextLayout::index_for_position`] finds a character of the range, and
    /// where its tooltip is placed, which runs on to the text's edge on a row
    /// the range continues past.
    fn rows(layout: &TextLayout, range: &Range<usize>) -> Vec<(Bounds<Pixels>, Bounds<Pixels>)> {
        let text = layout.bounds();
        let line_height = layout.line_height();
        let (Some(start), Some(end)) = (
            layout.position_for_index(range.start),
            layout.position_for_index(range.end),
        ) else {
            return Vec::new();
        };
        let last = ((end.y - start.y) / line_height).round().max(0.) as usize;
        (0..=last)
            .filter_map(|row| {
                let top = start.y + line_height * row as f32;
                let left = if row == 0 { start.x } else { text.left() };
                let (right, edge) = if row == last {
                    (end.x, end.x)
                } else {
                    // Past a row's end, `index_for_position` finds the index
                    // the row ends at, which `position_for_index` puts at the
                    // end of that row.
                    let ends_at = layout
                        .index_for_position(point(Pixels::MAX, top + line_height / 2.))
                        .unwrap_or_else(|ix| ix);
                    let right = layout
                        .position_for_index(ends_at)
                        .map_or(text.right(), |position| position.x);
                    (right, text.right())
                };
                let rect =
                    |right| Bounds::from_corners(point(left, top), point(right, top + line_height));
                // A range that starts where a row wraps is placed at the end
                // of the row above, which holds none of it.
                (right > left).then(|| (rect(right), rect(edge.max(left))))
            })
            .collect()
    }

    fn link_and_range_for_position(
        layout: &TextLayout,
        links: &[(Range<usize>, LinkMark)],
        position: Point<Pixels>,
    ) -> Option<(Range<usize>, LinkMark)> {
        let offset = layout.index_for_position(position).ok()?;
        links
            .iter()
            .find(|(range, _)| range.contains(&offset))
            .map(|(range, link)| (range.clone(), link.clone()))
    }

    fn paint_selection(
        selection: &Range<usize>,
        text_layout: &TextLayout,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let (start, end) = if selection.start <= selection.end {
            (selection.start, selection.end)
        } else {
            (selection.end, selection.start)
        };
        let (Some(start_position), Some(end_position)) = (
            text_layout.position_for_index(start),
            text_layout.position_for_index(end),
        ) else {
            return;
        };
        let line_height = text_layout.line_height();
        let color = cx.theme().selection;
        let paint = |bounds, window: &mut Window| {
            window.paint_quad(quad(
                bounds,
                px(0.),
                color,
                Edges::default(),
                gpui::transparent_black(),
                BorderStyle::default(),
            ));
        };
        if start_position.y == end_position.y {
            paint(
                Bounds::from_corners(
                    start_position,
                    point(end_position.x, end_position.y + line_height),
                ),
                window,
            );
            return;
        }
        paint(
            Bounds::from_corners(
                start_position,
                point(bounds.right(), start_position.y + line_height),
            ),
            window,
        );
        if end_position.y > start_position.y + line_height {
            paint(
                Bounds::from_corners(
                    point(bounds.left(), start_position.y + line_height),
                    point(bounds.right(), end_position.y),
                ),
                window,
            );
        }
        paint(
            Bounds::from_corners(
                point(bounds.left(), end_position.y),
                point(end_position.x, end_position.y + line_height),
            ),
            window,
        );
    }
}

impl Element for Inline {
    type RequestLayoutState = ();
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let text_style = window.text_style();
        let fades = self
            .origin
            .as_ref()
            .map(|origin| {
                let fades = self.view.read(cx).fades(origin, self.text.len());
                super::stream_fade::on_chars(&self.text, fades)
            })
            .unwrap_or_default();
        let faded;
        let highlights = if fades.is_empty() {
            &self.highlights
        } else {
            faded = super::stream_fade::fade_highlights(&self.highlights, &fades);
            &faded
        };
        let runs = text_runs(&self.text, &text_style, highlights);
        let len = self.text.len();
        let font_overrides = self
            .font_overrides
            .iter()
            .filter_map(|(range, family)| {
                let start = self.text.floor_char_boundary(range.start.min(len));
                let end = self.text.ceil_char_boundary(range.end.min(len));
                (start < end).then(|| (start..end, family.clone()))
            })
            .collect::<Vec<_>>();
        let styled_text = StyledText::new(self.text.clone())
            .with_runs(runs)
            .with_font_family_overrides(font_overrides);
        self.text_layout = styled_text.layout().clone();
        let interactive_text = InteractiveText::new(self.id.clone(), styled_text);
        self.interactive_text = if self.links.is_empty() {
            interactive_text
        } else {
            // For the hand InteractiveText sets over the links' own hitboxes.
            // Its click is not used: it fires for any button, for a drag that
            // starts and ends on one link and for each press of a double
            // click, and a press released outside the text stays pending for
            // the next release. Selectable text needs the click in `paint`.
            let ranges = self.links.iter().map(|(range, _)| range.clone()).collect();
            interactive_text.on_click(ranges, |_, _, _| {})
        };
        let (layout, _) = self
            .interactive_text
            .request_layout(global_id, inspector_id, window, cx);
        (layout, ())
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let hitbox = self
            .interactive_text
            .prepaint(id, inspector_id, bounds, &mut (), window, cx);
        self.link_rows = Rc::new(
            self.links
                .iter()
                .enumerate()
                .flat_map(|(link, (range, _))| {
                    Self::rows(&self.text_layout, range)
                        .into_iter()
                        .map(move |row| (link, row))
                })
                .map(|(link, (row, placement))| LinkRow {
                    link,
                    hitbox: window.insert_hitbox(row, HitboxBehavior::Normal),
                    placement,
                })
                .collect(),
        );
        let view = self.view.read(cx);
        if let (Some(origin), Some(_)) = (&self.origin, &view.selection) {
            self.selected = view.selected_range(origin, self.text.len(), cx);
            view.register_run(PaintedRun {
                origin: origin.clone(),
                text: self.text.clone(),
                layout: self.text_layout.clone(),
                bounds,
                visible: window.content_mask().bounds,
            });
        }
        hitbox
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let text_layout = self.text_layout.clone();
        // Before the text paints its links' hand, which the window takes
        // over the I-beam because it is set later.
        if self.origin.is_some() && self.view.read(cx).is_selectable() {
            window.set_cursor_style(CursorStyle::IBeam, hitbox);
        }
        if let Some((glyph, baseline)) = self.math_glyph {
            let style = window.text_style();
            let font_id = window.text_system().resolve_font(&style.font());
            let _ = window.paint_glyph(
                bounds.origin + point(px(0.), baseline),
                font_id,
                glyph,
                style.font_size.to_pixels(window.rem_size()),
                style.color,
            );
        } else {
            self.interactive_text
                .paint(global_id, None, bounds, &mut (), hitbox, window, cx);
        }

        if let Some(selected) = &self.selected {
            Self::paint_selection(selected, &text_layout, bounds, window, cx);
        }

        let owner = window.use_keyed_state("tcode-tooltip-trigger", cx, |_, _| {
            crate::overlay::TooltipOwner::default()
        });
        crate::widgets::tooltip::track_parts(
            owner,
            {
                // Asked as the text paints too: through the rows' hover, a
                // pointer move redraws the text only when it enters or
                // leaves a link.
                let rows = self.link_rows.clone();
                let links = self.links.clone();
                let view = self.view.clone();
                move |window, cx| {
                    if cx.has_active_drag() {
                        return None;
                    }
                    let row = rows.iter().find(|row| row.hitbox.is_hovered(window))?;
                    let (view, url) = (view.clone(), links.get(row.link)?.1.url.clone());
                    let build: crate::overlay::TooltipBuilder = Rc::new(move |_, cx| {
                        Tooltip::new(view.read(cx).resolve_link(&url, cx).tooltip_text())
                    });
                    Some((row.link, row.placement, build))
                }
            },
            window,
            cx,
        );

        window.on_mouse_event({
            let hitbox = hitbox.clone();
            let layout = text_layout.clone();
            let links = self.links.clone();
            let text = self.text.clone();
            let view = self.view.clone();
            move |event: &MouseDownEvent, phase, window, cx| {
                if !phase.bubble() || !hitbox.is_hovered(window) {
                    return;
                }
                match event.button {
                    MouseButton::Left => {
                        let origin = Self::link_for_position(&layout, &links, event.position)
                            .is_some()
                            .then_some(event.position);
                        view.update(cx, |state, _| state.link_press_origin = origin);
                    }
                    MouseButton::Right => {
                        Self::record_link(&view, &layout, &links, &text, event.position, cx);
                    }
                    _ => {}
                }
            }
        });

        // In the capture phase: the view's touch menu decides whether to
        // take the press before the text under it hears it.
        window.on_mouse_event({
            let hitbox = hitbox.clone();
            let layout = text_layout.clone();
            let links = self.links.clone();
            let text = self.text.clone();
            let view = self.view.clone();
            move |event: &LongPressEvent, phase, window, cx| {
                if phase.capture()
                    && event.phase == TouchPhase::Started
                    && hitbox.is_hovered(window)
                {
                    Self::record_link(&view, &layout, &links, &text, event.position, cx);
                }
            }
        });

        window.on_mouse_event({
            let links = self.links.clone();
            let layout = text_layout;
            let hitbox = hitbox.clone();
            let view = self.view.clone();
            move |event: &MouseUpEvent, phase, window, cx| {
                if !phase.bubble()
                    || event.button != MouseButton::Left
                    || !hitbox.is_hovered(window)
                {
                    return;
                }
                let Some(origin) = view.update(cx, |state, _| state.link_press_origin.take())
                else {
                    return;
                };
                // A press-and-release on the link is a click even with the
                // pixel of jitter a real mouse adds; farther apart is a
                // drag-selection. click_count 1 keeps a double-click (which
                // selects the word) from opening the link a second time.
                let moved = event.position - origin;
                if event.click_count != 1 || moved.x.abs() > px(3.) || moved.y.abs() > px(3.) {
                    return;
                }
                if let Some(link) = Self::link_for_position(&layout, &links, event.position) {
                    cx.stop_propagation();
                    MarkdownState::open_link(&view, &link.url, window, cx);
                }
            }
        });
    }
}

/// Partition `0..text.len()` into valid GPUI text runs for `highlights`.
/// Ranges are clamped to UTF-8 boundaries and normalized if unsorted or
/// overlapping, so GPUI's shaper and `StyledText::with_runs` never receive
/// runs whose total length diverges from `text.len()`.
pub(super) fn text_runs(
    text: &str,
    style: &TextStyle,
    highlights: &[(Range<usize>, HighlightStyle)],
) -> Vec<TextRun> {
    let len = text.len();
    if len == 0 {
        return Vec::new();
    }
    let mut prev_end = 0;
    let mut needs_normalize = false;
    let mut clamped = Vec::with_capacity(highlights.len());
    for (range, highlight) in highlights {
        let start = text.floor_char_boundary(range.start.min(len));
        let end = text.ceil_char_boundary(range.end.min(len));
        if start >= end {
            continue;
        }
        if start < prev_end {
            needs_normalize = true;
        }
        prev_end = end;
        clamped.push((start..end, *highlight));
    }
    let normalized = if needs_normalize {
        gpui::combine_highlights(std::iter::empty(), clamped).collect()
    } else {
        clamped
    };
    let mut runs = Vec::new();
    let mut ix = 0;
    for (range, highlight) in normalized {
        if ix < range.start {
            runs.push(style.clone().to_run(range.start - ix));
        }
        runs.push(style.clone().highlight(highlight).to_run(range.len()));
        ix = range.end;
    }
    if ix < len {
        runs.push(style.to_run(len - ix));
    }
    runs
}
