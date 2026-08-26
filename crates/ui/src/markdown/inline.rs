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
    StyledText, TextLayout, Window, point, px, quad,
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
        &self,
        text_layout: &TextLayout,
        mask_bounds: Bounds<Pixels>,
    ) -> Vec<Bounds<Pixels>> {
        let line_height = text_layout.line_height();
        let mut lines = Vec::new();
        let mut current_y = None;
        let mut current: Option<Bounds<Pixels>> = None;
        let mut offset = 0;
        for c in self.text.chars() {
            let next_offset = offset + c.len_utf8();
            let Some(pos) = text_layout.position_for_index(offset) else {
                offset = next_offset;
                continue;
            };
            let mut width = line_height / 2.;
            if let Some(next_pos) = text_layout.position_for_index(next_offset)
                && next_pos.y == pos.y
            {
                width = next_pos.x - pos.x;
            }
            let bounds = Bounds::from_corners(pos, point(pos.x + width, pos.y + line_height))
                .intersect(&mask_bounds);
            if bounds.size.width > px(0.) && bounds.size.height > px(0.) {
                if current_y == Some(pos.y) {
                    if let Some(current) = current.as_mut() {
                        *current = current.union(&bounds);
                    }
                } else {
                    if let Some(current) = current.take() {
                        lines.push(current);
                    }
                    current_y = Some(pos.y);
                    current = Some(bounds);
                }
            }
            offset = next_offset;
        }
        if let Some(current) = current {
            lines.push(current);
        }
        lines
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
                let text_bounds = self.text_line_bounds(&text_layout, window.content_mask().bounds);
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
