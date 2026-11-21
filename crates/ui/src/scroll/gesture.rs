//! Which scroll area a scroll gesture belongs to.
//!
//! Gap: gpui-fast delivers a touch pan and the momentum after it, and macOS
//! a trackpad gesture and its momentum, at one point of the window while the
//! content scrolls under it, and hit-tests every packet; gpui-base's
//! `ScrollableMask` and `ScrollBounce` decide packet by packet which area
//! scrolls. A nested area that scrolls under that point takes the rest of a
//! page's gesture, an area at its end hands the rest of its own to the page,
//! and a page whose stretched edge loses the gesture's release to a nested
//! area holds the stretch until something else reaches it. Here a gesture
//! belongs to the first area that moves for it, or to the page when no nested
//! area does, until its last momentum packet. Remove it once gpui-base keeps
//! a gesture with one area.
//!
//! Gap: gpui-fast plays a fling's momentum to the end of its curve, up to
//! some seconds, whether or not anything still moves for it, and spends the
//! next touch anywhere in the window on catching it: that touch is neither a
//! tap nor a drag an element can claim. A fling ends here once it runs into
//! the end of the area it belongs to ([`run_out`]), where UIKit and Android
//! end theirs. Remove it once gpui-fast ends momentum nothing moves for.
//!
//! A mouse wheel's notches are not a gesture: each goes to the area under
//! the pointer, as before.

#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
#[cfg(target_family = "wasm")]
use web_time::Instant;

use std::hash::{Hash, Hasher};
use std::time::Duration;

use gpui::{
    App, DispatchPhase, GlobalElementId, HitboxId, IntoElement, IsZero as _, ScrollDelta,
    ScrollEvent, ScrollSource, Styled as _, TouchPhase, Window, WindowId, canvas, point, px,
};
use gpui_base::ScrollbarHandle;

/// A silence longer than this ends a gesture. Momentum arrives once a
/// frame, and a precise stream with no phases (Linux, a browser) has no
/// other end; gpui-base ends such a stream after the same pause.
const GESTURE_GAP: Duration = Duration::from_millis(250);

/// A nested scroll area, the same from frame to frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct AreaKey(u64);

impl AreaKey {
    pub(crate) fn of_element(id: &GlobalElementId) -> Self {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        id.hash(&mut hasher);
        Self(hasher.finish())
    }
}

/// What a nested area may do with the packet being dispatched.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Turn {
    /// No area has the gesture yet: the area scrolls if it can, and the
    /// gesture is then its own ([`take`]).
    Open,
    /// The gesture is the area's: it keeps every packet, at its ends too.
    Mine,
    /// The gesture is another area's or the page's: the area leaves it.
    Theirs,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Owner {
    #[default]
    Open,
    Area(AreaKey),
    /// A packet that could move something went past every nested area.
    Page,
}

#[derive(Default)]
struct Gesture {
    window: Option<WindowId>,
    last_at: Option<Instant>,
    owner: Owner,
    /// The previous packet could move something and no nested area took it.
    passed: bool,
    /// The finger has lifted: the packets that follow are its momentum.
    lifted: bool,
}

impl gpui::Global for Gesture {}

/// A wheel's notches; every other packet is part of a gesture.
fn is_wheel(event: &ScrollEvent) -> bool {
    matches!(event.delta, ScrollDelta::Lines(_))
}

/// Follows the window's scroll gestures. It must be the first element of the
/// window to listen to scroll events, so that it hears each packet before any
/// area acts on it; a window without it scrolls packet by packet.
pub(crate) fn recorder() -> impl IntoElement {
    canvas(
        |_, _, _| {},
        |_, _, window, _| {
            window.on_mouse_event(|event: &ScrollEvent, phase, window, cx| {
                if phase == DispatchPhase::Capture {
                    record(event, window, cx);
                }
            });
        },
    )
    .absolute()
    .size(px(0.))
}

fn record(event: &ScrollEvent, window: &Window, cx: &mut App) {
    if is_wheel(event) {
        return;
    }
    let now = cx.background_executor().now();
    let window_id = window.window_handle().window_id();
    let gesture = cx.default_global::<Gesture>();
    let begins = gesture.window != Some(window_id)
        || event.touch_phase == TouchPhase::Started
        || gesture
            .last_at
            .is_none_or(|at| now.saturating_duration_since(at) >= GESTURE_GAP);
    gesture.window = Some(window_id);
    gesture.last_at = Some(now);
    if begins {
        gesture.owner = Owner::Open;
        gesture.lifted = false;
    } else if gesture.owner == Owner::Open && gesture.passed {
        gesture.owner = Owner::Page;
    }
    gesture.passed = event.delta.pixel_delta(window.line_height()) != Default::default();
    gesture.lifted |= matches!(event.touch_phase, TouchPhase::Ended | TouchPhase::Cancelled);
}

/// What the area `key` may do with `event`, in the capture phase.
pub(crate) fn turn(key: AreaKey, event: &ScrollEvent, window: &Window, cx: &App) -> Turn {
    if is_wheel(event) {
        return Turn::Open;
    }
    let Some(gesture) = cx.try_global::<Gesture>() else {
        return Turn::Open;
    };
    if gesture.window != Some(window.window_handle().window_id()) {
        return Turn::Open;
    }
    match gesture.owner {
        Owner::Open => Turn::Open,
        Owner::Area(owner) if owner == key => Turn::Mine,
        Owner::Area(_) | Owner::Page => Turn::Theirs,
    }
}

/// The area `key` moved for `event` while the gesture was open: the rest of
/// the gesture is its own.
pub(crate) fn take(key: AreaKey, event: &ScrollEvent, window: &Window, cx: &mut App) {
    if is_wheel(event) {
        return;
    }
    let window_id = window.window_handle().window_id();
    if let Some(gesture) = cx.try_global::<Gesture>()
        && gesture.window == Some(window_id)
        && gesture.owner == Owner::Open
    {
        cx.global_mut::<Gesture>().owner = Owner::Area(key);
    }
}

/// The gesture's owner, `area` or the page, has nothing left to move for
/// `event`: if that is a packet of a fling's momentum, the fling ends.
pub(crate) fn run_out(
    area: Option<AreaKey>,
    event: &ScrollEvent,
    window: &mut Window,
    cx: &mut App,
) {
    if event.source != ScrollSource::Touch || event.touch_phase != TouchPhase::Moved {
        return;
    }
    let window_id = window.window_handle().window_id();
    let owner = area.map_or(Owner::Page, Owner::Area);
    if !cx.try_global::<Gesture>().is_some_and(|gesture| {
        gesture.window == Some(window_id) && gesture.lifted && gesture.owner == owner
    }) {
        return;
    }
    cx.global_mut::<Gesture>().lifted = false;
    // Outside the window, where no scroll area is: one under the touch would
    // take it for a finger on its edge and let go of the bounce the fling
    // has just thrown.
    crate::edge_back::catch_fling(point(px(-1.), px(-1.)), window, cx);
}

/// Ends a fling that runs into the end of the page `handle` scrolls, under
/// `hitbox`. Call it while painting, before the page: its listeners take
/// each packet as it is captured.
pub(super) fn end_flings_at_page_end<H: ScrollbarHandle + 'static>(
    hitbox: HitboxId,
    handle: H,
    window: &mut Window,
) {
    window.on_mouse_event(move |event: &ScrollEvent, phase, window, cx| {
        if !(phase.capture() && hitbox.should_handle_scroll(window)) {
            return;
        }
        let delta = event.delta.pixel_delta(window.line_height()).y;
        if delta.is_zero() {
            return;
        }
        let max = (handle.content_size().height - handle.viewport_bounds().size.height).max(px(0.));
        let offset = handle.offset().y.clamp(-max, px(0.));
        if !super::autoscroll::room(&handle).vertical
            || (offset + delta).clamp(-max, px(0.)) == offset
        {
            run_out(None, event, window, cx);
        }
    });
}
