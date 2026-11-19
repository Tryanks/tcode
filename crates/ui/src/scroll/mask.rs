//! The mask that hands a nested viewport the scroll input it can use.
//!
//! Gap: gpui-base's `ScrollableMask` takes each packet its viewport can use
//! and leaves the rest to the area around it, packet by packet, and has no
//! way to leave alone a packet whose gesture is another area's. This is that
//! mask, without its debug border, its plain-wheel variant and the record of
//! horizontal claims, none of which Tcode uses, and which goes by
//! [`gesture`](super::gesture) as well: it moves only for a gesture that is
//! open or its own, and keeps its own gesture at its ends. Remove it with
//! [`gesture`](super::gesture).

use std::{cell::RefCell, rc::Rc};

use gpui::{
    App, Axis, Bounds, ContentMask, Element, ElementId, GlobalElementId, Hitbox, IsZero as _,
    LayoutId, OngoingScroll, Pixels, Position, ScrollEvent, Style, Window, px, relative,
};
use gpui_base::{OngoingScrollExt as _, ScrollbarHandle};

use super::gesture::{self, AreaKey, Turn};

/// Covers its parent, which must also hold the viewport `handle` scrolls,
/// as a sibling painted before it.
pub(crate) struct AreaMask<H: ScrollbarHandle + Clone> {
    axis: Axis,
    id: ElementId,
    handle: H,
}

impl<H: ScrollbarHandle + Clone> AreaMask<H> {
    /// `id` keeps the mask's gesture, and its axis lock, from frame to frame.
    pub(crate) fn new(axis: Axis, handle: &H, id: impl Into<ElementId>) -> Self {
        Self {
            axis,
            id: id.into(),
            handle: handle.clone(),
        }
    }
}

impl<H: ScrollbarHandle + Clone + 'static> Element for AreaMask<H> {
    type RequestLayoutState = ();
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        let axis = match self.axis {
            Axis::Horizontal => "horizontal",
            Axis::Vertical => "vertical",
        };
        Some((self.id.clone(), axis).into())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        // Pinned to the parent's top-left corner whatever its layout, rather
        // than left where a column or a row would stack it.
        let mut style = Style {
            position: Position::Absolute,
            flex_grow: 1.0,
            flex_shrink: 1.0,
            ..Default::default()
        };
        style.inset.top = px(0.).into();
        style.inset.left = px(0.).into();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        _: &mut App,
    ) -> Hitbox {
        window.insert_hitbox(bounds, gpui::HitboxBehavior::Normal)
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        hitbox: &mut Hitbox,
        window: &mut Window,
        _: &mut App,
    ) {
        let global_id = global_id.expect("an area mask has an id");
        let key = AreaKey::of_element(global_id);
        let ongoing =
            window.with_element_state::<Rc<RefCell<OngoingScroll>>, _>(global_id, |state, _| {
                let state = state.unwrap_or_default();
                (state.clone(), state)
            });
        let horizontal = self.axis == Axis::Horizontal;
        let line_height = window.line_height();
        let bounds = hitbox.bounds;
        let hitbox = hitbox.id;
        let handle = self.handle.clone();
        let view = window.current_view();
        super::autoscroll::listen_for_press(
            hitbox,
            handle.clone(),
            super::autoscroll::Axes {
                horizontal,
                vertical: !horizontal,
            },
            window,
        );
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            window.on_mouse_event(move |event: &ScrollEvent, phase, window, cx| {
                // In the capture phase: the scrollers around a viewport
                // (`gpui::list`, a page's bounce) take a packet as it is
                // captured or as soon as it bubbles. `should_handle_scroll`
                // keeps an occluded mask inert, under a dialog or a menu.
                if !(phase.capture() && hitbox.should_handle_scroll(window)) {
                    return;
                }
                let turn = gesture::turn(key, event, window, cx);
                if turn == Turn::Theirs {
                    return;
                }
                let mut delta = event.delta.pixel_delta(line_height);
                // Lock the gesture to the axis it started on, so a diagonal
                // swipe cannot flip which mask takes it from one packet to the
                // next. Line deltas carry no touch phase.
                if event.delta.precise() {
                    ongoing
                        .borrow_mut()
                        .lock_axis(&mut delta, event.touch_phase);
                }
                // One axis at a time: a trackpad reports both.
                if !delta.x.is_zero() && !delta.y.is_zero() {
                    if delta.x.abs() > delta.y.abs() {
                        delta.y = px(0.);
                    } else {
                        delta.x = px(0.);
                    }
                }
                let mut offset = handle.offset();
                let moved = if horizontal {
                    // `set_offset` does not clamp (the viewport clamps as it
                    // lays out), so a sideways packet is taken even at an
                    // edge rather than handed to the page.
                    offset.x += delta.x;
                    offset != handle.offset()
                } else {
                    // The current offset is clamped too: a packet that bubbled
                    // to the viewport's own listener can leave the offset past
                    // the edge until the next layout, which would read as room.
                    let max = (handle.content_size().height - handle.viewport_bounds().size.height)
                        .max(px(0.));
                    let current = offset.y.clamp(-max, px(0.));
                    offset.y = (current + delta.y).clamp(-max, px(0.));
                    offset.y != current
                };
                if moved {
                    handle.set_offset(offset);
                    cx.notify(view);
                    if turn == Turn::Open {
                        gesture::take(key, event, window, cx);
                    }
                    cx.stop_propagation();
                } else if turn == Turn::Mine {
                    cx.stop_propagation();
                }
            });
        });
    }
}
