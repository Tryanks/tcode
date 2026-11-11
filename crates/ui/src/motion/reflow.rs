//! Parts of a layout that jump when it reflows: a window turning compact
//! restacks a form, a row too narrow for its field wraps the field under its
//! label.
//!
//! gpui lays a layout out afresh each frame, so a part a reflow moves is
//! simply drawn in its new place; gpui-base has no layout animation. A
//! [`Reflow`] scope records where each of its parts was laid out, relative to
//! itself, and on a frame its width changed draws a part that moved further
//! than that change accounts for from where it was, gliding to where it now
//! is. A part that only follows the width (a control held at the row's end)
//! follows it, as it does on every frame the width did not change, so a fold
//! or a list pushing a part along is never glided twice. A part keeps its
//! glide when the reflow moves it under another parent, because the glide is
//! kept by the scope, keyed by the part's name, not in its element state.

#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    time::Duration,
};

use gpui::{AnyElement, App, Bounds, ElementId, IntoElement, Pixels, Point, SharedString, Window};
#[cfg(target_family = "wasm")]
use web_time::Instant;

use super::keyed::FlipBounds;

/// How long a part takes to glide from where a reflow found it to where it
/// was laid out.
const GLIDE: Duration = Duration::from_millis(200);

/// A part's glide: the offset it was drawn at when the last reflow moved it,
/// easing out to nothing from then.
#[derive(Clone, Copy)]
struct Glide {
    from: Point<Pixels>,
    started_at: Instant,
}

#[derive(Default)]
struct State {
    bounds: FlipBounds,
    /// The scope's width on the last frame it was laid out.
    width: Option<Pixels>,
    /// The layout an owned scope was last drawn with.
    layout: Option<u64>,
    glides: HashMap<SharedString, Glide>,
}

/// A reflow scope's state kept by its owner rather than in the element tree;
/// see [`ReflowState::scope`].
#[derive(Clone, Default)]
pub(crate) struct ReflowState(Rc<RefCell<State>>);

impl ReflowState {
    /// The scope this state keeps, drawing the layout `layout` names. A
    /// frame that draws another layout than the last one drawn is a
    /// reflow, under whatever parent the scope is drawn: gpui keys element
    /// state by its whole id path, and a page the window re-parents as it
    /// turns compact or wide would otherwise start its glides over. The
    /// width alone never makes a reflow here, since the scope may not have
    /// been drawn while the window changed.
    pub(crate) fn scope(&self, layout: u64) -> Reflow {
        let frame = Frame::default();
        frame.layout.set(Some(layout));
        Reflow {
            state: self.0.clone(),
            frame: Rc::new(frame),
        }
    }

    /// [`Self::scope`] whose width changing also makes a reflow (a field
    /// wrapping under its label as the row narrows), on frames the scope
    /// `id` was drawn on the frame before under the same parent.
    pub(crate) fn scope_in(
        &self,
        id: impl Into<ElementId>,
        layout: u64,
        window: &mut Window,
        cx: &mut App,
    ) -> Reflow {
        let fresh = Cell::new(false);
        window.use_keyed_state(super::child_id(&id.into(), "reflow-drawn"), cx, |_, _| {
            fresh.set(true)
        });
        let reflow = self.scope(layout);
        reflow.frame.follows_width.set(!fresh.get());
        reflow
    }
}

/// One layout whose parts glide when it reflows: made each frame by
/// [`reflow`], its parts drawn through [`Reflow::part`] and the layout
/// through [`Reflow::render`].
#[derive(Clone)]
pub(crate) struct Reflow {
    state: Rc<RefCell<State>>,
    frame: Rc<Frame>,
}

/// What the scope measured this frame, for its parts.
#[derive(Default)]
struct Frame {
    origin: Cell<Point<Pixels>>,
    /// How much the scope's width changed this frame, when it did.
    resized: Cell<Option<Pixels>>,
    /// Whether its owner changed the layout this frame.
    reflowed: Cell<bool>,
    /// The layout an owned scope draws.
    layout: Cell<Option<u64>>,
    /// Whether a change of the scope's width alone can make a reflow: the
    /// scope was drawn last frame where it is drawn now, so the width it
    /// measured then is this layout's.
    follows_width: Cell<bool>,
    /// Whether parts are placed against the window rather than the scope.
    in_window: Cell<bool>,
    /// Whether parts are measured by their top-right corner against the
    /// scope's.
    at_end: Cell<bool>,
}

/// The reflow scope `id` of the view rendering it.
pub(crate) fn reflow(id: impl Into<ElementId>, window: &mut Window, cx: &mut App) -> Reflow {
    let id = id.into();
    let state = window.use_keyed_state(super::child_id(&id, "reflow"), cx, |_, _| {
        Rc::new(RefCell::new(State::default()))
    });
    let frame = Frame::default();
    frame.follows_width.set(true);
    Reflow {
        state: state.read(cx).clone(),
        frame: Rc::new(frame),
    }
}

impl Reflow {
    /// Marks this frame as a reflow when `reflowed`, whatever the width did:
    /// for an owner that chooses its layout from a width measured on an
    /// earlier frame, so the new layout comes in on a frame the width does
    /// not change. Every part it moved glides there.
    pub(crate) fn reflowed(self, reflowed: bool) -> Self {
        self.frame.reflowed.set(reflowed);
        self
    }

    /// Measures the parts against the window rather than the scope: for an
    /// overlay placed against the window, whose scope is wherever its host
    /// happens to lay it out, and lays it out elsewhere in another layout.
    pub(crate) fn in_window(self) -> Self {
        self.frame.in_window.set(true);
        self
    }

    /// Measures each part by its top-right corner against the scope's: for
    /// parts held at the end of the row (a block's actions, over it or
    /// beside it), so a reflow that also changes the scope's width or the
    /// part's own glides it only as far as the layout moved its end.
    pub(crate) fn at_end(self) -> Self {
        self.frame.at_end.set(true);
        self
    }

    /// `child`, as the part `key` of this layout: a reflow that moves it
    /// draws it, and hits it, where it was, gliding to where it is.
    pub(crate) fn part(&self, key: impl Into<SharedString>, child: impl IntoElement) -> AnyElement {
        Part {
            key: key.into(),
            reflow: self.clone(),
            child: child.into_any_element(),
        }
        .into_any_element()
    }

    /// `layout` as the scope its parts are measured in.
    pub(crate) fn render(self, layout: impl IntoElement) -> AnyElement {
        Scope {
            reflow: self,
            child: layout.into_any_element(),
        }
        .into_any_element()
    }
}

struct Scope {
    reflow: Reflow,
    child: AnyElement,
}

impl IntoElement for Scope {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl gpui::Element for Scope {
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
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let frame = &self.reflow.frame;
        frame.origin.set(if frame.in_window.get() {
            Point::default()
        } else if frame.at_end.get() {
            bounds.top_right()
        } else {
            bounds.origin
        });
        let mut state = self.reflow.state.borrow_mut();
        let last = state.width.replace(bounds.size.width);
        if let Some(layout) = frame.layout.get() {
            let changed = state
                .layout
                .replace(layout)
                .is_some_and(|last| last != layout);
            frame.reflowed.set(frame.reflowed.get() || changed);
        }
        if frame.follows_width.get() {
            frame.resized.set(
                last.map(|last| bounds.size.width - last)
                    .filter(|change| *change != Pixels::ZERO),
            );
        }
        drop(state);
        self.child.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
}

struct Part {
    key: SharedString,
    reflow: Reflow,
    child: AnyElement,
}

impl IntoElement for Part {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Part {
    /// How far from its laid-out place to draw the part this frame.
    fn offset(&self, bounds: Bounds<Pixels>, window: &mut Window, cx: &App) -> Point<Pixels> {
        let frame = &self.reflow.frame;
        let mut state = self.reflow.state.borrow_mut();
        let corner = if frame.at_end.get() {
            Bounds::new(bounds.top_right(), bounds.size)
        } else {
            bounds
        };
        let moved = state.bounds.measure(&self.key, frame.origin.get(), corner);
        let now = cx.background_executor().now();
        let still = |glide: &Glide| {
            let elapsed = now.saturating_duration_since(glide.started_at);
            (elapsed < GLIDE)
                .then(|| glide.from * (1. - super::Curve::new(GLIDE).progress(elapsed)))
        };
        if cx.reduce_motion() {
            state.glides.remove(&self.key);
            return Point::default();
        }
        let carried = state
            .glides
            .get(&self.key)
            .and_then(still)
            .unwrap_or_default();
        // Only a move the width change does not account for is a reflow: a
        // part held at the end of a row moves by exactly as much.
        let reflowed = frame.resized.get().is_some_and(|change| {
            moved.x.abs() > change.abs() + gpui::px(0.5) || moved.y.abs() > gpui::px(0.5)
        }) || (frame.reflowed.get()
            && (moved.x.abs() > gpui::px(0.5) || moved.y.abs() > gpui::px(0.5)));
        if reflowed {
            let from = carried + moved;
            state.glides.insert(
                self.key.clone(),
                Glide {
                    from,
                    started_at: now,
                },
            );
            window.request_animation_frame();
            return from;
        }
        if carried == Point::default() {
            state.glides.remove(&self.key);
        } else {
            window.request_animation_frame();
        }
        carried
    }
}

impl gpui::Element for Part {
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
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let offset = self.offset(bounds, window, cx);
        super::keyed::prepaint_gliding(offset, window, |window| self.child.prepaint(window, cx));
    }

    fn paint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
}
