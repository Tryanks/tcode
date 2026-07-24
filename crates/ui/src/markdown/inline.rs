//! Selectable rich-text element adapted from gpui-component's Apache-2.0
//! `text/inline.rs` implementation.

use std::{
    ops::Range,
    rc::Rc,
    sync::{Arc, Mutex},
};

use gpui::{
    App, BorderStyle, Bounds, CursorStyle, Edges, Element, ElementId, Entity, GlobalElementId,
    HighlightStyle, Hitbox, HitboxBehavior, InspectorElementId, IntoElement, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, SharedString, StyledText,
    TextLayout, Window, point, px, quad,
};
use gpui_component::ActiveTheme as _;

use super::{
    link_target::{LinkTarget, resolve_link},
    nodes::LinkMark,
    selection::word_range_at,
    state::{MarkdownMultiClickKind, MarkdownState, PendingLinkMenu},
    window_selection,
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
    styled_text: StyledText,
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
        let text = state
            .lock()
            .map(|state| state.text.clone())
            .unwrap_or_default();
        Self {
            id: id.into(),
            view,
            text: text.clone(),
            links: Rc::new(links),
            highlights,
            font_overrides,
            styled_text: StyledText::new(text),
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

    fn layout_selection(
        &self,
        text_layout: &TextLayout,
        bounds: Bounds<Pixels>,
        window: &Window,
        cx: &App,
    ) -> (bool, bool, Option<Range<usize>>) {
        let state = self.view.read(cx);
        if !state.is_selectable() {
            return (false, false, None);
        }
        if state.is_all_selected() {
            return (true, true, Some(0..self.text.len()));
        }
        if let Some(selection) = state.multi_click_selection() {
            return (
                true,
                true,
                selection_for_multi_click(
                    &self.text,
                    text_layout,
                    bounds,
                    selection.pos,
                    selection.kind,
                ),
            );
        }
        let Some((selection_start, selection_end)) = state.selection_points(window, cx) else {
            return (true, false, None);
        };

        let line_height = text_layout.line_height();
        let mask_bounds = window.content_mask().bounds;
        let mut selection: Option<Range<usize>> = None;
        let mut offset = 0;
        for c in self.text.chars() {
            let next_offset = offset + c.len_utf8();
            let Some(pos) = text_layout.position_for_index(offset) else {
                offset = next_offset;
                continue;
            };
            let mut char_width = line_height / 2.;
            if let Some(next_pos) = text_layout.position_for_index(next_offset)
                && next_pos.y == pos.y
            {
                char_width = next_pos.x - pos.x;
            }
            let center = point(pos.x + char_width / 2., pos.y + line_height / 2.);
            if mask_bounds.contains(&center)
                && point_in_text_selection(
                    pos,
                    char_width,
                    selection_start,
                    selection_end,
                    line_height,
                )
            {
                selection.get_or_insert(offset..offset).end = next_offset;
            }
            offset = next_offset;
        }
        (true, true, selection)
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
        self.styled_text = StyledText::new(self.text.clone())
            .with_runs(runs)
            .with_font_family_overrides(self.font_overrides.clone());
        let (layout, _) = self
            .styled_text
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
        self.styled_text
            .prepaint(id, inspector_id, bounds, &mut (), window, cx);
        window.insert_hitbox(bounds, HitboxBehavior::Normal)
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
        let text_layout = self.styled_text.layout().clone();
        self.styled_text
            .paint(global_id, None, bounds, &mut (), &mut (), window, cx);

        let (selectable, has_selection, selection) =
            self.layout_selection(&text_layout, bounds, window, cx);
        if let Ok(mut state) = self.state.lock() {
            state.selection = selection.clone();
        }
        if selectable || has_selection {
            window.set_cursor_style(CursorStyle::IBeam, hitbox);
        }
        if Self::link_for_position(&text_layout, &self.links, window.mouse_position()).is_some() {
            window.set_cursor_style(CursorStyle::PointingHand, hitbox);
        }
        if let Some(selection) = &selection {
            Self::paint_selection(selection, &text_layout, bounds, window, cx);
        }

        if selectable {
            window_selection::register_selectable_text_inline(
                &self.view,
                self.text_line_bounds(&text_layout, window.content_mask().bounds),
                window,
                cx,
            );
            window.on_mouse_event({
                let hitbox = hitbox.clone();
                let layout = text_layout.clone();
                let inline_state = self.state.clone();
                let text = self.text.clone();
                let view = self.view.clone();
                move |event: &MouseDownEvent, phase, window, cx| {
                    if !phase.bubble()
                        || !hitbox.is_hovered(window)
                        || event.button != MouseButton::Left
                    {
                        return;
                    }
                    let kind = match event.click_count {
                        2 => MarkdownMultiClickKind::Word,
                        3 => MarkdownMultiClickKind::Paragraph,
                        _ => return,
                    };
                    let Some(range) = selection_for_multi_click(
                        &text,
                        &layout,
                        hitbox.bounds,
                        event.position,
                        kind,
                    ) else {
                        return;
                    };
                    let selected_text = text[range.clone()].to_string();
                    if let Ok(mut state) = inline_state.lock() {
                        state.selection = Some(range);
                    }
                    view.update(cx, |state, cx| {
                        state.set_multi_click_selection(event.position, kind, selected_text);
                        cx.notify();
                    });
                    window_selection::finish_drag(window, cx);
                    cx.notify(current_view);
                }
            });
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
                if !phase.bubble()
                    || event.button != MouseButton::Right
                    || !hitbox.is_hovered(window)
                {
                    return;
                }
                let pending = Self::link_and_range_for_position(&layout, &links, event.position)
                    .map(|(range, link)| {
                        let target = resolve_link(&link.url, view.read(cx).base_dir());
                        let text = text.get(range).map(SharedString::from).unwrap_or_default();
                        PendingLinkMenu {
                            target,
                            text,
                            raw_url: link.url,
                        }
                    });
                view.update(cx, |state, cx| state.set_pending_context_link(pending, cx));
            }
        });

        if !has_selection {
            window.on_mouse_event({
                let links = self.links.clone();
                let layout = text_layout;
                let hitbox = hitbox.clone();
                let view = self.view.clone();
                move |event: &MouseUpEvent, phase, window, cx| {
                    if !phase.bubble()
                        || event.button != MouseButton::Left
                        || !hitbox.is_hovered(window)
                        || view.read(cx).has_selection(window, cx)
                    {
                        return;
                    }
                    if let Some(link) = Self::link_for_position(&layout, &links, event.position) {
                        window_selection::finish_drag(window, cx);
                        cx.stop_propagation();
                        match resolve_link(&link.url, view.read(cx).base_dir()) {
                            LinkTarget::Web(url) => cx.open_url(&url),
                            LinkTarget::Local(path) => cx.open_with_system(&path),
                        }
                    }
                }
            });
        }
    }
}

fn selection_for_multi_click(
    text: &str,
    layout: &TextLayout,
    bounds: Bounds<Pixels>,
    pos: Point<Pixels>,
    kind: MarkdownMultiClickKind,
) -> Option<Range<usize>> {
    if !bounds.contains(&pos) {
        return None;
    }
    let offset = layout.index_for_position(pos).ok()?;
    match kind {
        MarkdownMultiClickKind::Word => word_range_at(text, offset),
        MarkdownMultiClickKind::Paragraph => (!text.is_empty()).then_some(0..text.len()),
    }
}

fn point_in_text_selection(
    pos: Point<Pixels>,
    char_width: Pixels,
    selection_start: Point<Pixels>,
    selection_end: Point<Pixels>,
    line_height: Pixels,
) -> bool {
    let point_in_line = |point: Point<Pixels>| point.y >= pos.y && point.y < pos.y + line_height;
    let top = selection_start.y.min(selection_end.y);
    let bottom = selection_start.y.max(selection_end.y);
    let x = pos.x + char_width / 2.;
    if pos.y + line_height <= top || pos.y > bottom {
        return false;
    }
    if point_in_line(selection_start) && point_in_line(selection_end) {
        let left = selection_start.x.min(selection_end.x);
        let right = selection_start.x.max(selection_end.x);
        return x >= left && x <= right;
    }
    let (top_point, bottom_point) = if selection_start.y < selection_end.y {
        (selection_start, selection_end)
    } else {
        (selection_end, selection_start)
    };
    if point_in_line(top_point) {
        x >= top_point.x
    } else if point_in_line(bottom_point) {
        x <= bottom_point.x
    } else {
        true
    }
}
