//! An edge bounce that carries on when its view is drawn under a new parent.
//!
//! gpui-base's `ScrollBounce` keeps its stretch and the spring that returns
//! it in element state keyed by the whole id path, and its physics is
//! private: a view drawn under another parent (a pane zoomed over the column,
//! a window turning compact) starts the bounce over at rest, so an edge on
//! its way back snapped home. [`Bounce`] watches the displacement the bounce
//! draws each frame and, on the frame it finds it drawn under a new parent,
//! takes what was left home itself on the view's clock: gpui-base's own
//! spring (critically damped at the bounce's response) from the place and
//! speed the edge had. Remove it once gpui-base can key a bounce's state by
//! an owner instead of an element path.

#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use gpui::{
    AnyElement, App, Bounds, ContentMask, Element, ElementId, GlobalElementId, InspectorElementId,
    IntoElement, LayoutId, Pixels, Window, point, px,
};
use gpui_base::{ScrollBounce, ScrollBounceMotion};
#[cfg(target_family = "wasm")]
use web_time::Instant;

/// A bounce's displacement kept by the view that draws it; see [`Bounce`].
#[derive(Clone, Default)]
pub(crate) struct BounceState(Rc<RefCell<State>>);

#[derive(Default)]
struct State {
    /// The id path the bounce was last drawn under.
    parent: Option<GlobalElementId>,
    /// What gpui-base displaced the content by on the last frame and the one
    /// before it, and when.
    seen: [Option<(f32, Instant)>; 2],
    /// A displacement left behind under the old parent, on its way home.
    carried: Option<Carried>,
}

#[derive(Clone, Copy)]
struct Carried {
    position: f32,
    velocity: f32,
    at: Instant,
}

/// gpui-base's return spring's rate, for the motion `page_viewport` gives
/// its bounce.
fn omega() -> Option<f32> {
    let seconds = ScrollBounceMotion::default().response().as_secs_f32();
    (seconds > 0.).then(|| std::f32::consts::TAU / seconds)
}

impl Carried {
    /// Where an edge that was at `last` after `before` is heading: the
    /// position and speed of the critically damped spring through both, or
    /// let go at rest from `last` when there is only the one.
    fn from(last: (f32, Instant), before: Option<(f32, Instant)>, omega: f32) -> Self {
        let (position, at) = last;
        let released = -omega * position;
        let velocity = before
            .filter(|(earlier, _)| *earlier != 0. && earlier.signum() == position.signum())
            .and_then(|(earlier, then)| {
                let span = at.saturating_duration_since(then).as_secs_f32();
                (span > 0.).then(|| {
                    let decay = (-omega * span).exp();
                    let c = (position / decay - earlier) / span;
                    (c - omega * earlier - omega * c * span) * decay
                })
            })
            .unwrap_or(released);
        Self {
            position,
            velocity,
            at,
        }
    }

    /// Advanced to `now`, as gpui-base steps its spring; `None` once home.
    fn step(self, now: Instant, omega: f32) -> Option<Self> {
        let seconds = now.saturating_duration_since(self.at).as_secs_f32();
        let decay = (-omega * seconds).exp();
        let c = self.velocity + omega * self.position;
        let position = (self.position + c * seconds) * decay;
        let velocity = (self.velocity - omega * c * seconds) * decay;
        (position.abs() >= 0.1 || velocity.abs() >= 1.).then_some(Self {
            position,
            velocity,
            at: now,
        })
    }
}

/// A bounce whose displacement, when it is drawn under a new parent, is
/// carried home; made by [`BounceState::render`]. Clipped to its own bounds,
/// as the bounce clips its content, so what is carried uncovers the edge it
/// stretched rather than the page past it.
pub(crate) struct Bounce {
    id: ElementId,
    state: BounceState,
    probe: Rc<Cell<Option<Pixels>>>,
    bounce: AnyElement,
}

impl BounceState {
    /// The bounce `bounce` makes around `child`, which it must take as its
    /// own child as it is handed over: what it draws `child` displaced by is
    /// read from where it draws it.
    pub(crate) fn render(
        &self,
        id: impl Into<ElementId>,
        child: impl IntoElement,
        bounce: impl FnOnce(Probe) -> ScrollBounce,
    ) -> Bounce {
        let probe = Rc::new(Cell::new(None));
        Bounce {
            id: super::child_id(&id.into(), "kept"),
            state: self.clone(),
            probe: probe.clone(),
            bounce: bounce(Probe {
                at: probe,
                child: child.into_any_element(),
            })
            .into_any_element(),
        }
    }
}

/// A bounce's child, recording where the bounce drew it.
pub(crate) struct Probe {
    at: Rc<Cell<Option<Pixels>>>,
    child: AnyElement,
}

impl IntoElement for Probe {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Probe {
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
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.at.set(Some(bounds.top()));
        self.child.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
}

impl IntoElement for Bounce {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Bounce {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
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
    ) -> (LayoutId, ()) {
        (self.bounce.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let now = cx.background_executor().now();
        let carry = {
            let mut state = self.state.0.borrow_mut();
            let moved = id.is_some_and(|id| state.parent.as_ref().is_some_and(|last| last != id));
            state.parent = id.cloned();
            match omega().filter(|_| !cx.reduce_motion()) {
                None => {
                    *state = State {
                        parent: state.parent.take(),
                        ..State::default()
                    };
                }
                Some(omega) => {
                    if moved
                        && let [Some(last), before] = state.seen
                        && last.0 != 0.
                    {
                        // What the old parent still drew, plus what it was
                        // still carrying, goes home from here.
                        let carried = state
                            .carried
                            .and_then(|carried| carried.step(last.1, omega))
                            .map_or(0., |carried| carried.position);
                        let mut from = Carried::from(last, before, omega);
                        from.position += carried;
                        state.carried = Some(from);
                        state.seen = [None, None];
                    }
                    state.carried = state.carried.and_then(|carried| carried.step(now, omega));
                }
            }
            state.carried.map_or(0., |carried| carried.position)
        };
        if carry != 0. {
            window.request_animation_frame();
        }
        let mask = ContentMask { bounds };
        window.with_content_mask(Some(mask), |window| {
            window.with_element_offset(point(px(0.), px(carry)), |window| {
                self.bounce.prepaint(window, cx)
            })
        });
        let drawn = self
            .probe
            .get()
            .map_or(0., |top| f32::from(top - bounds.top()) - carry);
        let drawn = if drawn.abs() < 0.1 { 0. } else { drawn };
        let mut state = self.state.0.borrow_mut();
        state.seen = [Some((drawn, now)), state.seen[0]];
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            self.bounce.paint(window, cx)
        });
    }
}
