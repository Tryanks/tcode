//! Vertical mouse-wheel easing for Tcode's vertical scroll views, over stock
//! GPUI handles. Only line-delta wheel notches take this path: trackpad pixels
//! and touch pans stay with each element's own scroll handler. The registry
//! exists so a notch finds the innermost eased viewport under the pointer and
//! hands off to the registered parent at a limit.
use std::{collections::HashMap, panic::Location, time::Duration};

use gpui::{
    App, Bounds, Element, ElementId, Global, GlobalElementId, HitboxBehavior, HitboxId,
    InspectorElementId, IntoElement, KeyDownEvent, LayoutId, ListState, MouseDownEvent, Pixels,
    ScrollDelta, ScrollHandle, ScrollWheelEvent, Window, WindowId, point, px,
};
#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
#[cfg(target_family = "wasm")]
use web_time::Instant;

const DURATION: Duration = Duration::from_millis(125);

#[derive(Clone)]
pub(crate) enum Handle {
    Scroll(ScrollHandle),
    List(ListState),
}

impl Handle {
    fn apply(&self, delta: Pixels) {
        match self {
            Self::Scroll(handle) => {
                let mut offset = handle.offset();
                offset.y = (offset.y + delta).clamp(-handle.max_offset().y.max(px(0.)), px(0.));
                handle.set_offset(offset);
            }
            Self::List(list) => {
                // A following bottom-aligned list uses an end-of-list logical
                // anchor. scroll_by would subtract from that anchor, swallowing
                // movement shorter than the viewport when layout clamps to the
                // end. Let ListState clamp its own padded extent.
                let old = list.scroll_px_offset_for_scrollbar();
                list.set_offset_from_scrollbar(point(px(0.), old.y + delta));
            }
        }
    }

    /// The clamped vertical offset and its maximum.
    fn extent(&self) -> (Pixels, Pixels) {
        let (offset, max) = match self {
            Self::Scroll(handle) => (handle.offset().y, handle.max_offset().y),
            Self::List(list) => (
                list.scroll_px_offset_for_scrollbar().y,
                list.max_offset_for_scrollbar().y,
            ),
        };
        let max = max.max(px(0.));
        (offset.clamp(-max, px(0.)), max)
    }
}

#[derive(Clone)]
struct Entry {
    id: GlobalElementId,
    parent: Option<GlobalElementId>,
    bounds: Bounds<Pixels>,
    line_height: Pixels,
    handle: Handle,
    hitbox: Option<HitboxId>,
}

struct Motion {
    id: GlobalElementId,
    expected: Position,
    started: Instant,
    duration: Duration,
    travel: Pixels,
    applied: Pixels,
}

// Logical list anchors survive row remeasurement; absolute pixel targets don't.
enum Position {
    Scroll(Pixels),
    List {
        item: usize,
        offset: Pixels,
        count: usize,
    },
}

impl Position {
    fn read(handle: &Handle) -> Self {
        match handle {
            Handle::Scroll(handle) => Self::Scroll(handle.offset().y),
            Handle::List(list) => {
                let top = list.logical_scroll_top();
                Self::List {
                    item: top.item_ix,
                    offset: top.offset_in_item,
                    count: list.item_count(),
                }
            }
        }
    }

    fn matches(&self, handle: &Handle) -> bool {
        match (self, Self::read(handle)) {
            (Self::Scroll(old), Self::Scroll(new)) => *old == new,
            (
                Self::List {
                    item,
                    offset,
                    count,
                },
                Self::List {
                    item: new_item,
                    offset: new_offset,
                    count: new_count,
                },
            ) => {
                // Prepending history shifts both indices equally without moving
                // the reading anchor. Explicit positioning cancels the animation.
                *offset == new_offset
                    && (*item == new_item || *count - *item == new_count - new_item)
            }
            _ => false,
        }
    }
}

#[derive(Default)]
struct Registry {
    entries: Vec<Entry>,
    parents: Vec<GlobalElementId>,
    motion: Option<Motion>,
    frame_pending: bool,
}

#[derive(Default)]
struct WheelEasing(HashMap<WindowId, Registry>);
impl Global for WheelEasing {}

fn registry<'a>(window: &Window, cx: &'a mut App) -> &'a mut Registry {
    cx.default_global::<WheelEasing>()
        .0
        .entry(window.window_handle().window_id())
        .or_default()
}

fn install(window: &mut Window) {
    window.on_mouse_event(|_: &MouseDownEvent, phase, window, cx| {
        if phase.capture() {
            registry(window, cx).motion = None;
        }
    });
    window.on_key_event(|_: &KeyDownEvent, phase, window, cx| {
        if phase.capture() {
            registry(window, cx).motion = None;
        }
    });
    window.on_mouse_event(|event: &ScrollWheelEvent, phase, window, cx| {
        if !phase.capture() {
            return;
        }
        let ScrollDelta::Lines(lines) = event.delta else {
            registry(window, cx).motion = None;
            return;
        };
        if cx.reduce_motion()
            || event.modifiers.shift
            || lines.y == 0.
            || lines.x.abs() >= lines.y.abs()
        {
            registry(window, cx).motion = None;
            return;
        }

        let previous = registry(window, cx).motion.take();
        let entries = &registry(window, cx).entries;
        // Menus and dialogs can occlude an otherwise matching viewport.
        let mut entry = entries.iter().rev().find(|entry| {
            entry.bounds.contains(&event.position)
                && entry
                    .hitbox
                    .is_some_and(|hitbox| hitbox.should_handle_scroll(window))
        });
        // At a nested viewport's limit, let the registered parent take this notch.
        let (entry, offset, max, delta) = loop {
            let Some(target) = entry else { return };
            let (offset, max) = target.handle.extent();
            let line_height = if matches!(target.handle, Handle::List(_)) {
                px(20.)
            } else {
                target.line_height
            };
            let delta = line_height * lines.y;
            if (offset + delta).clamp(-max, px(0.)) != offset {
                break (target.clone(), offset, max, delta);
            }
            entry = target
                .parent
                .as_ref()
                .and_then(|parent| entries.iter().find(|entry| &entry.id == parent));
        };
        let now = cx.background_executor().now();
        let previous = previous
            .filter(|motion| motion.id == entry.id && motion.expected.matches(&entry.handle));
        let mut travel = delta;
        let mut duration = DURATION;
        if let Some(previous) = previous {
            let remaining = previous.travel - previous.applied;
            if f32::from(remaining) * f32::from(delta) > 0. {
                travel += remaining;
                // Preserve speed when retargeting a run of wheel notches (Zed #44827).
                let progress = (now.duration_since(previous.started).as_secs_f32()
                    / previous.duration.as_secs_f32())
                .min(1.);
                let speed = f32::from(previous.travel) * 3. * (1. - progress).powi(2)
                    / previous.duration.as_secs_f32();
                if speed.abs() > 1e-6 {
                    duration = Duration::from_secs_f32(
                        (3. * f32::from(travel) / speed)
                            .clamp(DURATION.as_secs_f32() / 8., DURATION.as_secs_f32()),
                    );
                }
            }
        }
        travel = travel.clamp(-max - offset, -offset);
        if let Handle::List(list) = &entry.handle
            && list.is_following_tail()
            && travel > px(0.)
        {
            // GPUI re-engages tail following within 1px of the end. Move just
            // beyond that threshold now so streaming can't reclaim the viewport
            // before the first animation frame.
            let release = travel.min(px(2.));
            list.set_offset_from_scrollbar(point(px(0.), offset + release));
            list.pause_following_tail();
            travel -= release;
        }
        registry(window, cx).motion = Some(Motion {
            id: entry.id,
            expected: Position::read(&entry.handle),
            started: now,
            duration,
            travel,
            applied: px(0.),
        });
        schedule(window, cx);
        window.refresh();
        cx.stop_propagation();
    });
}

fn schedule(window: &mut Window, cx: &mut App) {
    let wheel = registry(window, cx);
    if wheel.frame_pending {
        return;
    }
    wheel.frame_pending = true;
    window.on_next_frame(|window, cx| {
        registry(window, cx).frame_pending = false;
        let Some(mut motion) = registry(window, cx).motion.take() else {
            return;
        };
        let Some(handle) = registry(window, cx)
            .entries
            .iter()
            .find(|entry| entry.id == motion.id)
            .map(|entry| entry.handle.clone())
        else {
            return;
        };
        if !motion.expected.matches(&handle) {
            return;
        }
        let progress = if cx.reduce_motion() {
            1.
        } else {
            (cx.background_executor()
                .now()
                .duration_since(motion.started)
                .as_secs_f32()
                / motion.duration.as_secs_f32())
            .min(1.)
        };
        let position = motion.travel * (1. - (1. - progress).powi(3));
        let delta = position - motion.applied;
        if delta != px(0.) {
            handle.apply(delta);
        }
        motion.applied = position;
        motion.expected = Position::read(&handle);
        if progress < 1. {
            registry(window, cx).motion = Some(motion);
            schedule(window, cx);
        }
        window.refresh();
    });
}

/// A transparent element wrapper: registration precedes child prepaint, so the
/// last matching entry is the innermost viewport.
pub(crate) struct Registered<E: Element> {
    element: E,
    id: ElementId,
    handle: Option<Handle>,
    root: bool,
}

/// Register `element` as the viewport that `handle` scrolls, so mouse-wheel
/// notches over it ease instead of jumping.
#[track_caller]
pub(crate) fn register<E: IntoElement>(element: E, handle: Handle) -> Registered<E::Element> {
    let element = element.into_element();
    let id = element
        .id()
        .unwrap_or(ElementId::CodeLocation(*Location::caller()));
    Registered {
        element,
        id,
        handle: Some(handle),
        root: false,
    }
}

/// The window's root: rebuilds the viewport registry each frame and owns the
/// wheel listeners.
pub(crate) fn root<E: IntoElement>(element: E) -> Registered<E::Element> {
    Registered {
        element: element.into_element(),
        id: "wheel-easing-root".into(),
        handle: None,
        root: true,
    }
}

impl<E: Element> IntoElement for Registered<E> {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl<E: Element> Element for Registered<E> {
    type RequestLayoutState = E::RequestLayoutState;
    type PrepaintState = E::PrepaintState;
    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }
    fn source_location(&self) -> Option<&'static Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        self.element.request_layout(id, inspector, window, cx)
    }
    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        if self.root {
            let registry = registry(window, cx);
            if let Some(motion) = &registry.motion
                && let Some(entry) = registry.entries.iter().find(|entry| entry.id == motion.id)
                && !motion.expected.matches(&entry.handle)
            {
                registry.motion = None;
            }
            registry.entries.clear();
            registry.parents.clear();
        }
        let entry_index = registry(window, cx).entries.len();
        if let Some(handle) = &self.handle {
            let bounds = bounds.intersect(&window.content_mask().bounds);
            let registry = registry(window, cx);
            let id = id.expect("registered scroll element has an id").clone();
            registry.entries.push(Entry {
                id: id.clone(),
                parent: registry.parents.last().cloned(),
                bounds,
                line_height: window.line_height(),
                handle: handle.clone(),
                hitbox: None,
            });
            registry.parents.push(id);
        }
        let result = self
            .element
            .prepaint(id, inspector, bounds, layout, window, cx);
        if self.handle.is_some() {
            let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
            let registry = registry(window, cx);
            registry.entries[entry_index].hitbox = Some(hitbox.id);
            registry.parents.pop();
        }
        if self.root {
            // Accept layout's anchor adjustments, and retire animations when
            // their view disappears.
            let registry = registry(window, cx);
            if let Some(motion) = &mut registry.motion {
                if let Some(entry) = registry.entries.iter().find(|entry| entry.id == motion.id) {
                    motion.expected = Position::read(&entry.handle);
                } else {
                    registry.motion = None;
                }
            }
        }
        result
    }
    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if self.root {
            install(window);
        }
        self.element
            .paint(id, inspector, bounds, layout, prepaint, window, cx);
    }
}

#[cfg(test)]
mod tests;
