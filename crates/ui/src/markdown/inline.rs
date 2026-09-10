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
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, SharedString,
    StyledText, TextLayout, Window, WrappedLineLayout, point, px, quad,
};

use super::{
    link_target::LinkTarget,
    nodes::LinkMark,
    state::{MarkdownState, PendingLinkMenu},
};

/// Mutable paint-time data retained by the parsed IR.
#[derive(Debug, Default, PartialEq)]
pub(super) struct InlineState {
    pub(super) text: SharedString,
    pub(super) selection: Option<Range<usize>>,
}

impl InlineState {
    pub(super) fn shared(text: SharedString) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            text,
            selection: None,
        }))
    }

    pub(super) fn set_text(&mut self, text: SharedString) {
        self.text = text;
    }
}

/// All selectable text, including code-block lines, is painted through this element.
pub(super) struct Inline {
    id: ElementId,
    view: Entity<MarkdownState>,
    text: SharedString,
    links: Rc<Vec<(Range<usize>, LinkMark)>>,
    highlights: Vec<(Range<usize>, HighlightStyle)>,
    font_overrides: Vec<(Range<usize>, SharedString)>,
    interactive_text: InteractiveText,
    text_layout: TextLayout,
    state: Arc<Mutex<InlineState>>,
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
            view,
            text: text.clone(),
            links: Rc::new(links),
            highlights,
            font_overrides,
            interactive_text: InteractiveText::new(id, styled_text),
            text_layout,
            state,
        }
    }

    fn link_for_position(
        layout: &TextLayout,
        links: &[(Range<usize>, LinkMark)],
        position: Point<Pixels>,
    ) -> Option<LinkMark> {
        Self::link_and_range_for_position(layout, links, position).map(|(_, link)| link)
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

    fn text_line_bounds(
        text_layout: &TextLayout,
        mask_bounds: Bounds<Pixels>,
    ) -> Vec<Bounds<Pixels>> {
        let lines = text_layout.line_layouts();
        wrapped_line_bounds(
            lines.iter().map(Arc::as_ref),
            text_layout.bounds().origin,
            text_layout.line_height(),
            mask_bounds,
        )
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
        let mut runs = Vec::new();
        let mut ix = 0;
        for (range, highlight) in &self.highlights {
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
        let links = self.links.clone();
        let view = self.view.clone();
        self.interactive_text = InteractiveText::new(self.id.clone(), styled_text).tooltip(
            move |position, window, cx| {
                let (_, link) = links.iter().find(|(range, _)| range.contains(&position))?;
                let text = view.read(cx).resolve_link(&link.url).tooltip_text();
                Some(Tooltip::new(text).build(window, cx))
            },
        );
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
        self.interactive_text
            .prepaint(id, inspector_id, bounds, &mut (), window, cx)
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
        let current_view = window.current_view();
        let text_layout = self.text_layout.clone();
        self.interactive_text
            .paint(global_id, None, bounds, &mut (), hitbox, window, cx);

        let selectable = self.view.read(cx).is_selectable();
        let selection = selectable
            .then(|| {
                let text_bounds =
                    Self::text_line_bounds(&text_layout, window.content_mask().bounds);
                let adapter = self.view.read(cx).selection_adapter.clone();
                let projection = adapter.update_run(
                    self.text.clone(),
                    text_layout.clone(),
                    bounds,
                    text_bounds,
                    cx,
                );
                if adapter.selection.has_local_selection(cx) {
                    Some(0..self.text.len())
                } else {
                    projection
                }
            })
            .flatten();
        if let Ok(mut state) = self.state.lock() {
            state.selection = selection.clone();
        }
        if selectable {
            window.set_cursor_style(CursorStyle::IBeam, hitbox);
        }
        if Self::link_for_position(&text_layout, &self.links, window.mouse_position()).is_some() {
            window.set_cursor_style(CursorStyle::PointingHand, hitbox);
        }
        if let Some(selection) = &selection {
            Self::paint_selection(selection, &text_layout, bounds, window, cx);
        }

        window.on_mouse_event({
            let hitbox = hitbox.clone();
            let layout = text_layout.clone();
            let links = self.links.clone();
            let mut hovered_link =
                Self::link_for_position(&layout, &links, window.mouse_position()).is_some();
            move |event: &MouseMoveEvent, phase, window, cx| {
                if !phase.bubble() {
                    return;
                }
                let updated = hitbox.is_hovered(window)
                    && Self::link_for_position(&layout, &links, event.position).is_some();
                if updated != hovered_link {
                    hovered_link = updated;
                    cx.notify(current_view);
                }
            }
        });

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
                                    PendingLinkMenu {
                                        target,
                                        text,
                                        raw_url: link.url,
                                    }
                                },
                            );
                        view.update(cx, |state, cx| state.set_pending_context_link(pending, cx));
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
                    gpui_base::TextSelection::end(window, cx);
                    cx.stop_propagation();
                    match view.read(cx).resolve_link(&link.url) {
                        LinkTarget::Web(url) => cx.open_url(&url),
                        LinkTarget::Local(path) => cx.open_with_system(&path),
                    }
                }
            }
        });
    }
}

/// One rect per visual (wrapped) line, covering that line's text extent. The
/// selection engine only hit-tests these, so per-character geometry cost
/// O(chars × glyphs) on every paint and bought nothing.
///
/// Mirrors the coordinate model of `TextLayout::position_for_index`: y starts at
/// `origin.y` and advances by `line_height` per visual line, and every visual
/// line starts at `origin.x`.
fn wrapped_line_bounds<'a>(
    lines: impl IntoIterator<Item = &'a WrappedLineLayout>,
    origin: Point<Pixels>,
    line_height: Pixels,
    mask_bounds: Bounds<Pixels>,
) -> Vec<Bounds<Pixels>> {
    let mut rects = Vec::new();
    let mut y = origin.y;
    for line in lines {
        let mut start_x = px(0.);
        let boundary_xs = line.wrap_boundaries.iter().map(|boundary| {
            line.unwrapped_layout.runs[boundary.run_ix].glyphs[boundary.glyph_ix]
                .position
                .x
        });
        for end_x in boundary_xs.chain([line.unwrapped_layout.width]) {
            let rect = Bounds::from_corners(
                point(origin.x, y),
                point(origin.x + end_x - start_x, y + line_height),
            )
            .intersect(&mask_bounds);
            if rect.size.width > px(0.) && rect.size.height > px(0.) {
                rects.push(rect);
            }
            start_x = end_x;
            y += line_height;
        }
    }
    rects
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Render, TestAppContext, TextRun, VisualTestContext, div, size};

    struct Root;

    impl Render for Root {
        fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
            div()
        }
    }

    #[gpui::test]
    fn text_bounds_cover_one_rect_per_visual_line(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let (_, cx) = cx.add_window_view(|_, _| Root);
        let cx: &mut VisualTestContext = cx;

        let text: SharedString =
            "the quick brown fox jumps over the lazy dog again and again\nsecond".into();
        let font_size = px(14.);
        let wrap_width = px(120.);
        let line_height = px(20.);
        let origin = point(px(10.), px(30.));

        let lines = cx.update(|window, _| {
            let run = TextRun {
                len: text.len(),
                font: window.text_style().font(),
                color: gpui::black(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            window
                .text_system()
                .shape_text(text.clone(), font_size, &[run], Some(wrap_width), None)
                .unwrap()
        });

        let visual_lines: usize = lines.iter().map(|l| l.wrap_boundaries.len() + 1).sum();
        assert!(
            visual_lines > lines.len(),
            "test needs at least one wrap boundary, got {visual_lines} visual lines \
             across {} hard lines",
            lines.len()
        );
        assert_eq!(lines.len(), 2, "test needs the '\\n' to split two lines");

        let huge = Bounds::new(point(px(0.), px(0.)), size(px(1000.), px(1000.)));
        let rects =
            wrapped_line_bounds(lines.iter().map(|line| &***line), origin, line_height, huge);
        assert_eq!(rects.len(), visual_lines);
        for (ix, rect) in rects.iter().enumerate() {
            assert_eq!(rect.origin.x, origin.x);
            assert_eq!(rect.origin.y, origin.y + line_height * ix as f32);
            assert_eq!(rect.size.height, line_height);
            assert!(rect.size.width > px(0.));
            assert!(rect.size.width <= wrap_width + px(0.5), "{rect:?}");
        }

        // A mask that excludes the last visual line drops its rect.
        let clipped = Bounds::new(
            point(px(0.), px(0.)),
            size(
                px(1000.),
                origin.y + line_height * (visual_lines - 1) as f32,
            ),
        );
        let rects = wrapped_line_bounds(
            lines.iter().map(|line| &***line),
            origin,
            line_height,
            clipped,
        );
        assert_eq!(rects.len(), visual_lines - 1);
    }
}
