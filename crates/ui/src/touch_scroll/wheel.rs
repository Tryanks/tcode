//! Vertical wheel easing for Tcode's registered viewports, using stock GPUI handles.
use super::{Handle, registry};
use gpui::{
    App, GlobalElementId, KeyDownEvent, MouseDownEvent, Pixels, ScrollDelta, ScrollWheelEvent,
    Window, point, px,
};
use std::time::Duration;
#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
#[cfg(target_family = "wasm")]
use web_time::Instant;

const DURATION: Duration = Duration::from_millis(125);

#[derive(Default)]
pub(super) struct Wheel {
    motion: Option<Motion>,
    frame_pending: bool,
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
            Handle::Textarea(_) => unreachable!("textareas retain their native wheel handler"),
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

fn extent(handle: &Handle) -> Option<(Pixels, Pixels)> {
    let (offset, max) = match handle {
        Handle::Scroll(handle) => (handle.offset().y, handle.max_offset().y),
        Handle::List(list) => (
            list.scroll_px_offset_for_scrollbar().y,
            list.max_offset_for_scrollbar().y,
        ),
        Handle::Textarea(_) => return None,
    };
    let max = max.max(px(0.));
    Some((offset.clamp(-max, px(0.)), max))
}

pub(super) fn install(window: &mut Window) {
    window.on_mouse_event(|_: &MouseDownEvent, phase, window, cx| {
        if phase.capture() {
            registry(window, cx).wheel.motion = None;
        }
    });
    window.on_key_event(|_: &KeyDownEvent, phase, window, cx| {
        if phase.capture() {
            registry(window, cx).wheel.motion = None;
        }
    });
    window.on_mouse_event(|event: &ScrollWheelEvent, phase, window, cx| {
        if !phase.capture() {
            return;
        }
        let ScrollDelta::Lines(lines) = event.delta else {
            registry(window, cx).wheel.motion = None;
            return;
        };
        if cx.reduce_motion()
            || event.modifiers.shift
            || lines.y == 0.
            || lines.x.abs() >= lines.y.abs()
        {
            registry(window, cx).wheel.motion = None;
            return;
        }

        let previous = registry(window, cx).wheel.motion.take();
        let entries = &registry(window, cx).entries;
        let mut entry = entries.iter().rev().find(|entry| {
            entry.bounds.contains(&event.position)
                && entry
                    .hitbox
                    .is_some_and(|hitbox| hitbox.should_handle_scroll(window))
        });
        // At a nested viewport's limit, let the registered parent take this notch.
        let (entry, offset, max, delta) = loop {
            let Some(target) = entry else { return };
            let Some((offset, max)) = extent(&target.handle) else {
                return;
            };
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
        registry(window, cx).wheel.motion = Some(Motion {
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
    let wheel = &mut registry(window, cx).wheel;
    if wheel.frame_pending {
        return;
    }
    wheel.frame_pending = true;
    window.on_next_frame(|window, cx| {
        registry(window, cx).wheel.frame_pending = false;
        let Some(mut motion) = registry(window, cx).wheel.motion.take() else {
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
            handle.apply(point(px(0.), delta), cx);
        }
        motion.applied = position;
        motion.expected = Position::read(&handle);
        if progress < 1. {
            registry(window, cx).wheel.motion = Some(motion);
            schedule(window, cx);
        }
        window.refresh();
    });
}

pub(super) fn will_prepaint(window: &Window, cx: &mut App) {
    let registry = registry(window, cx);
    if let Some(motion) = &registry.wheel.motion
        && let Some(entry) = registry.entries.iter().find(|entry| entry.id == motion.id)
        && !motion.expected.matches(&entry.handle)
    {
        registry.wheel.motion = None;
    }
}

/// Accept layout's anchor adjustments, and retire animations when their view disappears.
pub(super) fn did_prepaint(window: &Window, cx: &mut App) {
    let registry = registry(window, cx);
    if let Some(motion) = &mut registry.wheel.motion {
        if let Some(entry) = registry.entries.iter().find(|entry| entry.id == motion.id) {
            motion.expected = Position::read(&entry.handle);
        } else {
            registry.wheel.motion = None;
        }
    }
}

#[cfg(test)]
mod tests;
