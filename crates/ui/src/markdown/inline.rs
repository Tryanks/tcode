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
    HighlightStyle, Hitbox, InspectorElementId, InteractiveText, IntoElement, LayoutId,
    MouseButton, MouseDownEvent, MouseUpEvent, Pixels, Point, SharedString, StyledText, TextLayout,
    Window, point, px, quad,
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
        let text = state
            .lock()
            .map(|state| state.text.clone())
            .unwrap_or_default();
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

    /// Where a link's tooltip is placed: the part of `range` on the line
    /// holding the character at `at`.
    fn line_bounds(layout: &TextLayout, range: &Range<usize>, at: usize) -> Bounds<Pixels> {
        let text = layout.bounds();
        let Some(here) = layout.position_for_index(at) else {
            return text;
        };
        let on_line = |index| {
            layout
                .position_for_index(index)
                .filter(|position| position.y == here.y)
        };
        let left = on_line(range.start).map_or(text.left(), |position| position.x);
        let right = on_line(range.end).map_or(text.right(), |position| position.x);
        Bounds::from_corners(
            point(left, here.y),
            point(right.max(left), here.y + layout.line_height()),
        )
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

impl IntoElement for Inline {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
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
            .map(|origin| self.view.read(cx).fades(origin, self.text.len()))
            .unwrap_or_default();
        let faded;
        let highlights = if fades.is_empty() {
            &self.highlights
        } else {
            faded = super::stream_fade::fade_highlights(&self.highlights, &fades);
            &faded
        };
        let mut runs = Vec::new();
        let mut ix = 0;
        for (range, highlight) in highlights {
            if ix < range.start {
                runs.push(text_style.clone().to_run(range.start - ix));
            }
            runs.push(text_style.clone().highlight(*highlight).to_run(range.len()));
            ix = range.end;
        }
        if ix < self.text.len() {
            runs.push(text_style.to_run(self.text.len() - ix));
        }
        let styled_text = StyledText::new(self.text.clone())
            .with_runs(runs)
            .with_font_family_overrides(self.font_overrides.clone());
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
        let view = self.view.read(cx);
        if let (Some(origin), Some(membership)) = (&self.origin, &view.selection) {
            self.selected = view.selected_range(origin, self.text.len(), cx);
            if let Some(selected) = &self.selected {
                membership.document.read(cx).register_edge(
                    membership.entry,
                    origin,
                    selected,
                    &self.text_layout,
                    window.content_mask().bounds,
                );
            }
            view.register_run(PaintedRun {
                origin: origin.clone(),
                text: self.text.clone(),
                layout: self.text_layout.clone(),
                bounds,
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
                let hitbox = hitbox.clone();
                let layout = text_layout.clone();
                let links = self.links.clone();
                let view = self.view.clone();
                move |window, cx| {
                    if cx.has_active_drag() || !hitbox.is_hovered(window) {
                        return None;
                    }
                    let at = layout.index_for_position(window.mouse_position()).ok()?;
                    let (index, (range, link)) = links
                        .iter()
                        .enumerate()
                        .find(|(_, (range, _))| range.contains(&at))?;
                    let (view, url) = (view.clone(), link.url.clone());
                    let build: crate::overlay::TooltipBuilder = Rc::new(move |_, cx| {
                        Tooltip::new(view.read(cx).resolve_link(&url).tooltip_text())
                    });
                    Some((index, Self::line_bounds(&layout, range, at), build))
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
                        let pending =
                            Self::link_and_range_for_position(&layout, &links, event.position).map(
                                |(range, link)| {
                                    let target = view.read(cx).resolve_link(&link.url);
                                    let text =
                                        text.get(range).map(SharedString::from).unwrap_or_default();
                                    PendingContextTarget::Link(PendingLinkMenu {
                                        target,
                                        text,
                                        raw_url: link.url,
                                    })
                                },
                            );
                        // A press beside a link keeps the enclosing block's
                        // target (a code block's), which bubbled first.
                        if pending.is_some() {
                            view.update(cx, |state, cx| state.set_pending_context(pending, cx));
                        }
                    }
                    _ => {}
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
                    view.read(cx).resolve_link(&link.url).open(window, cx);
                }
            }
        });
    }
}
