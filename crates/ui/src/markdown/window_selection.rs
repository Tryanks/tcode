//! Window-level selection controller adapted from gpui-component's Apache-2.0
//! `text/window_selection.rs` implementation.
//!
//! gpui-component keeps this state on its private `Root`; tcode stores the same
//! registrations in a GPUI global keyed by `WindowId` so it can be mounted from
//! the application shell.

use std::{collections::HashMap, time::Duration};

use gpui::{
    AnyWindowHandle, App, AppContext as _, BorrowAppContext as _, Bounds, Element, ElementId,
    Entity, EntityId, Global, GlobalElementId, Hitbox, InspectorElementId, IntoElement, LayoutId,
    Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, PlatformInput,
    Point, ScrollDelta, ScrollWheelEvent, Style, Task, WeakEntity, Window, WindowId, point, px,
};

use super::state::MarkdownState;

#[derive(Default)]
struct SelectionGlobal {
    windows: HashMap<WindowId, WindowSelectionState>,
}

impl Global for SelectionGlobal {}

#[derive(Default)]
struct WindowSelectionState {
    selection: WindowTextSelection,
    views: HashMap<EntityId, (WeakEntity<MarkdownState>, Hitbox)>,
    inlines: HashMap<EntityId, Vec<Bounds<Pixels>>>,
    auto_scroll_delta: Option<Pixels>,
    auto_scroll_position: Point<Pixels>,
    auto_scroll_task: Option<Task<()>>,
}

#[derive(Default)]
struct WindowTextSelection {
    anchor: Option<SelectionEndpoint>,
    cursor: Option<SelectionEndpoint>,
    is_selecting: bool,
    did_hit_text: bool,
}

#[derive(Clone)]
struct SelectionEndpoint {
    view: Option<WeakEntity<MarkdownState>>,
    point: Point<Pixels>,
    inside: bool,
    inside_text: bool,
}

impl SelectionEndpoint {
    fn resolve(&self, cx: &App) -> Option<Point<Pixels>> {
        match &self.view {
            Some(view) => {
                let view = view.upgrade()?;
                Some(self.point + view.read(cx).bounds.origin)
            }
            None => Some(self.point),
        }
    }

    fn view_id(&self) -> Option<EntityId> {
        self.view.as_ref().map(|view| view.entity_id())
    }
}

impl WindowTextSelection {
    fn resolved_points(&self, cx: &App) -> Option<(Point<Pixels>, Point<Pixels>)> {
        if !self.did_hit_text {
            return None;
        }
        let anchor = self.anchor.as_ref()?.resolve(cx)?;
        let cursor = self.cursor.as_ref()?.resolve(cx)?;
        (anchor != cursor).then_some((anchor, cursor))
    }

    fn single_view(&self) -> Option<EntityId> {
        let anchor = self.anchor.as_ref()?.view_id()?;
        let cursor = self.cursor.as_ref()?.view_id()?;
        (anchor == cursor).then_some(anchor)
    }

    fn involves(&self, id: EntityId) -> bool {
        self.anchor.as_ref().and_then(SelectionEndpoint::view_id) == Some(id)
            || self.cursor.as_ref().and_then(SelectionEndpoint::view_id) == Some(id)
    }

    fn reset(&mut self) {
        self.anchor = None;
        self.cursor = None;
        self.is_selecting = false;
        self.did_hit_text = false;
    }
}

fn window_id(window: &Window) -> WindowId {
    window.window_handle().window_id()
}

pub(super) fn init_global(cx: &mut App) {
    if !cx.has_global::<SelectionGlobal>() {
        cx.set_global(SelectionGlobal::default());
    }
}

pub(super) fn register_selectable_text_view(
    state: &Entity<MarkdownState>,
    hitbox: &Hitbox,
    window: &Window,
    cx: &mut App,
) {
    let id = window_id(window);
    let entity_id = state.entity_id();
    let weak = state.downgrade();
    let hitbox = hitbox.clone();
    cx.update_default_global::<SelectionGlobal, _>(|global, _| {
        let selection = global.windows.entry(id).or_default();
        selection
            .views
            .retain(|_, (view, _)| view.upgrade().is_some());
        selection.views.insert(entity_id, (weak, hitbox));
        selection.inlines.remove(&entity_id);
    });
}

pub(super) fn register_selectable_text_inline(
    state: &Entity<MarkdownState>,
    bounds: Vec<Bounds<Pixels>>,
    window: &Window,
    cx: &mut App,
) {
    if bounds.is_empty() {
        return;
    }
    let window_id = window_id(window);
    let entity_id = state.entity_id();
    cx.update_default_global::<SelectionGlobal, _>(|global, _| {
        global
            .windows
            .entry(window_id)
            .or_default()
            .inlines
            .entry(entity_id)
            .or_default()
            .extend(bounds);
    });
}

pub(super) fn selection_points(
    window: &Window,
    view_id: EntityId,
    cx: &App,
) -> Option<(Point<Pixels>, Point<Pixels>)> {
    let state = cx
        .try_global::<SelectionGlobal>()?
        .windows
        .get(&window_id(window))?;
    if state
        .selection
        .single_view()
        .is_some_and(|id| id != view_id)
    {
        return None;
    }
    state.selection.resolved_points(cx)
}

pub(super) fn window_selected_text(window: &Window, cx: &App) -> String {
    let Some(state) = cx
        .try_global::<SelectionGlobal>()
        .and_then(|global| global.windows.get(&window_id(window)))
    else {
        return String::new();
    };
    let resolved = state.selection.resolved_points(cx);
    let single_view = state.selection.single_view();
    let mut items = Vec::new();
    for (id, (view, _)) in &state.views {
        let Some(view) = view.upgrade() else {
            continue;
        };
        let markdown = view.read(cx);
        let in_drag_selection = resolved.is_some()
            && markdown.is_selectable()
            && single_view.is_none_or(|selected| selected == *id);
        if !markdown.has_view_selection() && !in_drag_selection {
            continue;
        }
        // Each view's block text carries a trailing newline; strip it so the
        // join below inserts exactly one newline between views.
        let text = markdown.selected_text().trim_end().to_string();
        if !text.is_empty() {
            items.push((markdown.bounds.origin, text));
        }
    }
    items.sort_by(|a, b| {
        a.0.y
            .partial_cmp(&b.0.y)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                a.0.x
                    .partial_cmp(&b.0.x)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });
    items
        .into_iter()
        .map(|(_, text)| text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn clear_window(window_id: WindowId, cx: &mut App) {
    let views = cx.update_default_global::<SelectionGlobal, _>(|global, cx| {
        let state = global.windows.entry(window_id).or_default();
        let had_selection = state.selection.anchor.is_some();
        state.selection.reset();
        state.auto_scroll_delta = None;
        state.auto_scroll_task = None;
        state.views.retain(|_, (view, _)| view.upgrade().is_some());
        state
            .views
            .values()
            .filter_map(|(view, _)| view.upgrade())
            .filter(|view| had_selection || view.read(cx).has_view_selection())
            .collect::<Vec<_>>()
    });
    for view in views {
        view.update(cx, |state, cx| state.clear_selection(cx));
    }
}

pub(super) fn clear_selection_for_view(view_id: EntityId, cx: &mut App) {
    let other_views = cx.update_default_global::<SelectionGlobal, _>(|global, _| {
        let mut views = Vec::new();
        for state in global.windows.values_mut() {
            if state.selection.involves(view_id) {
                state.selection.reset();
                state.auto_scroll_delta = None;
                state.auto_scroll_task = None;
                views.extend(
                    state
                        .views
                        .iter()
                        .filter(|(id, _)| **id != view_id)
                        .filter_map(|(_, (view, _))| view.upgrade()),
                );
            }
        }
        views
    });
    for view in other_views {
        view.update(cx, |state, cx| state.clear_selection(cx));
    }
}

pub(super) fn clear_selection_for_resized_view(view_id: EntityId, cx: &mut App) {
    let windows = cx.update_default_global::<SelectionGlobal, _>(|global, _| {
        global
            .windows
            .iter()
            .filter(|(_, state)| !state.selection.is_selecting && state.selection.involves(view_id))
            .map(|(id, _)| *id)
            .collect::<Vec<_>>()
    });
    for id in windows {
        clear_window(id, cx);
    }
}

pub(super) fn clear_window_selection_for_select_all(
    selected: &Entity<MarkdownState>,
    cx: &mut App,
) {
    let selected_id = selected.entity_id();
    let other_views = cx.update_default_global::<SelectionGlobal, _>(|global, _| {
        let mut views = Vec::new();
        for state in global.windows.values_mut() {
            state.selection.reset();
            state.auto_scroll_delta = None;
            state.auto_scroll_task = None;
            views.extend(
                state
                    .views
                    .iter()
                    .filter(|(id, _)| **id != selected_id)
                    .filter_map(|(_, (view, _))| view.upgrade()),
            );
        }
        views
    });
    for view in other_views {
        view.update(cx, |state, cx| state.clear_selection(cx));
    }
}

fn endpoint_for_position(
    state: &WindowSelectionState,
    position: Point<Pixels>,
    window: &Window,
    cx: &App,
) -> SelectionEndpoint {
    let mut best: Option<(WeakEntity<MarkdownState>, f32)> = None;
    for (view, hitbox) in state.views.values() {
        if view.upgrade().is_none() || !hitbox.is_hovered(window) {
            continue;
        }
        let area = f32::from(hitbox.bounds.size.width) * f32::from(hitbox.bounds.size.height);
        if best.as_ref().is_none_or(|(_, best_area)| area < *best_area) {
            best = Some((view.clone(), area));
        }
    }
    if let Some((weak, view)) = best.and_then(|(weak, _)| weak.upgrade().map(|view| (weak, view))) {
        let markdown = view.read(cx);
        let inside_text = state
            .inlines
            .get(&markdown.entity_id)
            .is_some_and(|bounds| bounds.iter().any(|bounds| bounds.contains(&position)));
        return SelectionEndpoint {
            view: Some(weak),
            point: position - markdown.bounds.origin,
            inside: true,
            inside_text,
        };
    }

    let mut predecessor: Option<(WeakEntity<MarkdownState>, Pixels)> = None;
    let mut first: Option<(WeakEntity<MarkdownState>, Pixels)> = None;
    for (view, _) in state.views.values() {
        let Some(entity) = view.upgrade() else {
            continue;
        };
        let top = entity.read(cx).bounds.top();
        if top <= position.y && predecessor.as_ref().is_none_or(|(_, y)| top > *y) {
            predecessor = Some((view.clone(), top));
        }
        if first.as_ref().is_none_or(|(_, y)| top < *y) {
            first = Some((view.clone(), top));
        }
    }
    if let Some((weak, entity)) = predecessor
        .or(first)
        .and_then(|(weak, _)| weak.upgrade().map(|entity| (weak, entity)))
    {
        let markdown = entity.read(cx);
        SelectionEndpoint {
            view: Some(weak),
            point: position - markdown.bounds.origin,
            inside: false,
            inside_text: false,
        }
    } else {
        SelectionEndpoint {
            view: None,
            point: position,
            inside: false,
            inside_text: false,
        }
    }
}

fn notify_views(state: &mut WindowSelectionState, cx: &mut App) {
    state.views.retain(|_, (view, _)| {
        let Some(view) = view.upgrade() else {
            return false;
        };
        view.update(cx, |_, cx| cx.notify());
        true
    });
}

fn start_selection(position: Point<Pixels>, window: &mut Window, cx: &mut App) {
    let id = window_id(window);
    cx.update_default_global::<SelectionGlobal, _>(|global, cx| {
        let state = global.windows.entry(id).or_default();
        let endpoint = endpoint_for_position(state, position, window, cx);
        // gpui-component's richer control-suppression flag is crate-private.
        // Its public controls also prevent the default mouse-down behavior, so
        // honor that signal outside Markdown hitboxes while still allowing a
        // focused MarkdownView to begin its own selection.
        if window.default_prevented() && !endpoint.inside {
            return;
        }
        if endpoint.inside
            && let Some(view) = endpoint.view.as_ref().and_then(WeakEntity::upgrade)
        {
            view.update(cx, |markdown, cx| {
                markdown.is_selecting = true;
                markdown.focus_handle.focus(window, cx);
            });
        }
        state.selection.anchor = Some(endpoint.clone());
        state.selection.cursor = Some(endpoint.clone());
        state.selection.did_hit_text = endpoint.inside_text;
        state.selection.is_selecting = true;
    });
}

fn compute_auto_scroll(
    state: &WindowSelectionState,
    pointer: Point<Pixels>,
) -> (Option<Pixels>, Point<Pixels>) {
    let mut bounds: Option<Bounds<Pixels>> = None;
    for (view, hitbox) in state.views.values() {
        if view.upgrade().is_none() {
            continue;
        }
        let visible_bounds = hitbox.bounds.intersect(&hitbox.content_mask.bounds);
        if visible_bounds.size.width <= px(0.)
            || visible_bounds.size.height <= px(0.)
            || pointer.x < visible_bounds.left()
            || pointer.x > visible_bounds.right()
        {
            continue;
        }
        bounds = Some(bounds.map_or(visible_bounds, |bounds| bounds.union(&visible_bounds)));
    }
    let Some(bounds) = bounds else {
        return (None, pointer);
    };
    const INNER_ZONE: f32 = 16.;
    const OUTER_RAMP: f32 = 80.;
    const MIN_SPEED: f32 = 12.;
    const MAX_SPEED: f32 = 64.;
    let bottom_trigger = bounds.bottom() - px(INNER_ZONE);
    let top_trigger = bounds.top() + px(INNER_ZONE);
    let delta = if pointer.y > bottom_trigger {
        let t = ((pointer.y - bottom_trigger) / px(INNER_ZONE + OUTER_RAMP)).min(1.);
        Some(px(MIN_SPEED + t * (MAX_SPEED - MIN_SPEED)))
    } else if pointer.y < top_trigger {
        let t = ((top_trigger - pointer.y) / px(INNER_ZONE + OUTER_RAMP)).min(1.);
        Some(px(-(MIN_SPEED + t * (MAX_SPEED - MIN_SPEED))))
    } else {
        None
    };
    let position = point(
        pointer
            .x
            .max(bounds.left() + px(1.))
            .min(bounds.right() - px(1.)),
        pointer
            .y
            .max(bounds.top() + px(1.))
            .min(bounds.bottom() - px(1.)),
    );
    (delta, position)
}

fn ensure_auto_scroll_task(window: &Window, cx: &mut App) {
    let id = window_id(window);
    let needs_task = cx.update_default_global::<SelectionGlobal, _>(|global, _| {
        global
            .windows
            .entry(id)
            .or_default()
            .auto_scroll_task
            .is_none()
    });
    if !needs_task {
        return;
    }
    let handle: AnyWindowHandle = window.window_handle();
    let task = cx.spawn(async move |cx| {
        loop {
            let sample = cx.update(|cx| {
                cx.try_global::<SelectionGlobal>()
                    .and_then(|global| global.windows.get(&id))
                    .and_then(|state| {
                        state
                            .selection
                            .is_selecting
                            .then_some((state.auto_scroll_delta, state.auto_scroll_position))
                    })
            });
            let Some((delta, position)) = sample else {
                break;
            };
            let delay = if delta.is_some() { 16 } else { 50 };
            cx.background_executor()
                .timer(Duration::from_millis(delay))
                .await;
            let Some(delta) = delta else {
                continue;
            };
            if cx
                .update_window(handle, |_, window, cx| {
                    window.dispatch_event(
                        PlatformInput::ScrollWheel(ScrollWheelEvent {
                            position,
                            delta: ScrollDelta::Pixels(point(px(0.), -delta)),
                            modifiers: Modifiers::default(),
                            ..Default::default()
                        }),
                        cx,
                    );
                })
                .is_err()
            {
                break;
            }
        }
    });
    cx.update_default_global::<SelectionGlobal, _>(|global, _| {
        let state = global.windows.entry(id).or_default();
        if state.auto_scroll_task.is_none() {
            state.auto_scroll_task = Some(task);
        }
    });
}

fn update_selection(position: Point<Pixels>, window: &mut Window, cx: &mut App) {
    let id = window_id(window);
    let active = cx.update_default_global::<SelectionGlobal, _>(|global, cx| {
        let state = global.windows.entry(id).or_default();
        if !state.selection.is_selecting || cx.has_active_drag() {
            return false;
        }
        let endpoint = endpoint_for_position(state, position, window, cx);
        state.selection.did_hit_text |= endpoint.inside_text;
        state.selection.cursor = Some(endpoint);
        let (delta, dispatch_position) = compute_auto_scroll(state, position);
        state.auto_scroll_delta = delta;
        state.auto_scroll_position = dispatch_position;
        notify_views(state, cx);
        true
    });
    if active {
        ensure_auto_scroll_task(window, cx);
    }
}

fn end_selection(window: &Window, cx: &mut App) {
    let id = window_id(window);
    cx.update_default_global::<SelectionGlobal, _>(|global, cx| {
        let state = global.windows.entry(id).or_default();
        if !state.selection.is_selecting {
            return;
        }
        state.selection.is_selecting = false;
        state.auto_scroll_delta = None;
        state.auto_scroll_task = None;
        if !state.selection.did_hit_text {
            state.selection.reset();
        }
        if let Some(view) = state
            .selection
            .anchor
            .as_ref()
            .filter(|endpoint| endpoint.inside)
            .and_then(|endpoint| endpoint.view.as_ref())
            .and_then(WeakEntity::upgrade)
        {
            view.update(cx, |markdown, cx| {
                markdown.is_selecting = false;
                cx.notify();
            });
        }
        notify_views(state, cx);
    });
}

pub(super) fn finish_drag(window: &Window, cx: &mut App) {
    end_selection(window, cx);
}

/// A zero-size element that owns the window-global mouse handlers.
pub(crate) struct TextSelectionController;

impl IntoElement for TextSelectionController {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextSelectionController {
    type RequestLayoutState = ();
    type PrepaintState = ();

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
        (window.request_layout(Style::default(), None, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Window,
        _: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        _: &mut App,
    ) {
        window.on_mouse_event(|event: &MouseDownEvent, phase, window, cx| {
            if event.button != MouseButton::Left {
                return;
            }
            if phase.capture() {
                clear_window(window_id(window), cx);
            } else if event.click_count == 1 {
                start_selection(event.position, window, cx);
            }
        });
        window.on_mouse_event(|event: &MouseMoveEvent, phase, window, cx| {
            if phase.bubble() {
                update_selection(event.position, window, cx);
            }
        });
        window.on_mouse_event(|event: &MouseUpEvent, phase, window, cx| {
            if phase.bubble() && event.button == MouseButton::Left {
                end_selection(window, cx);
            }
        });
        window.on_mouse_event(|_: &ScrollWheelEvent, phase, window, cx| {
            if phase.bubble() {
                update_selection(window.mouse_position(), window, cx);
            }
        });
    }
}
