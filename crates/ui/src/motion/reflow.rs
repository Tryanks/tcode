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
use gpui_base::motion::Timing;
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
            let sample = Timing::new(GLIDE)
                .ease(gpui_base::motion::Easing::Custom(Rc::new(
                    gpui_base::animation::ease_out_cubic,
                )))
                .sample(now.saturating_duration_since(glide.started_at));
            (!sample.finished).then(|| glide.from * (1. - sample.directed_progress))
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

#[cfg(test)]
mod tests {
    use gpui::{
        Context, InteractiveElement as _, IntoElement, ParentElement as _, Render, Styled as _,
        TestAppContext, VisualTestContext, Window, div, px, size,
    };

    use crate::test_frames::{draw, draw_settled};

    /// A row whose button is held at its end, over a label with a field
    /// beside it that wraps under it when the row is too narrow for both.
    struct Form;

    impl Render for Form {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let reflow = super::reflow("form", window, cx);
            let part = |key: &'static str, width: f32| {
                reflow.part(
                    key,
                    div()
                        .debug_selector(move || key.to_string())
                        .flex_none()
                        .w(px(width))
                        .h(px(20.)),
                )
            };
            let layout = gpui_base::v_flex()
                .w_full()
                .child(
                    gpui_base::h_flex()
                        .w_full()
                        .justify_end()
                        .child(part("button", 50.)),
                )
                .child(
                    gpui_base::h_flex()
                        .w_full()
                        .flex_wrap()
                        .child(div().flex_none().w(px(200.)).h(px(20.)))
                        .child(part("field", 150.)),
                );
            div().size_full().child(reflow.render(layout))
        }
    }

    /// Narrowing the window wraps the field under its label: it is drawn
    /// where it was and glides there, while the button at the row's end
    /// follows the width at once; with motion reduced the field is there at
    /// once.
    #[gpui::test]
    fn a_wrapped_field_glides_under_its_label(cx: &mut TestAppContext) {
        let (_, cx) = cx.add_window_view(|_, _| Form);
        let at = |name: &'static str, cx: &mut VisualTestContext| {
            cx.debug_bounds(name).expect("the part is drawn").origin
        };
        cx.simulate_resize(size(px(400.), px(300.)));
        draw_settled(cx);
        let beside = at("field", cx);
        assert_eq!(beside.x, px(200.));

        cx.simulate_resize(size(px(300.), px(300.)));
        draw(cx);
        assert_eq!(at("field", cx), beside, "the field starts where it was");
        assert_eq!(at("button", cx).x, px(250.), "the button follows the width");

        cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
        draw(cx);
        let gliding = at("field", cx);
        assert!(
            gliding.x < beside.x && gliding.x > px(0.) && gliding.y > beside.y,
            "the field is on its way under the label, at {gliding:?}"
        );

        draw_settled(cx);
        let under = at("field", cx);
        assert_eq!(under.x, px(0.));
        assert!(under.y > beside.y);

        cx.update(|_, cx| cx.set_reduce_motion(true));
        cx.simulate_resize(size(px(400.), px(300.)));
        draw(cx);
        assert_eq!(at("field", cx), beside);
    }

    /// A row whose owner moves a part to its other end on a frame the width
    /// does not change, as a layout chosen from last frame's width does.
    struct Folding {
        folded: bool,
        last: Option<bool>,
    }

    impl Render for Folding {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let reflowed = self
                .last
                .replace(self.folded)
                .is_some_and(|last| last != self.folded);
            let reflow = super::reflow("folding", window, cx).reflowed(reflowed);
            let meter = reflow.part(
                "meter",
                div()
                    .debug_selector(|| "meter".to_string())
                    .flex_none()
                    .w(px(40.))
                    .h(px(20.)),
            );
            let spacer = div().flex_1();
            let row = if self.folded {
                gpui_base::h_flex().w_full().child(spacer).child(meter)
            } else {
                gpui_base::h_flex().w_full().child(meter).child(spacer)
            };
            div().size_full().child(reflow.render(row))
        }
    }

    /// The owner's reflow glides the part from where it was, though the width
    /// held still.
    #[gpui::test]
    fn an_owners_reflow_glides_its_parts(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| Folding {
            folded: false,
            last: None,
        });
        cx.simulate_resize(size(px(400.), px(300.)));
        draw_settled(cx);
        let left = |cx: &mut VisualTestContext| cx.debug_bounds("meter").unwrap().left();
        assert_eq!(left(cx), px(0.));
        view.update(cx, |view, cx| {
            view.folded = true;
            cx.notify();
        });
        draw(cx);
        assert_eq!(left(cx), px(0.), "the meter starts where it was");
        cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
        draw(cx);
        let gliding = left(cx);
        assert!(gliding > px(0.) && gliding < px(360.), "{gliding:?}");
        draw_settled(cx);
        assert_eq!(left(cx), px(360.));
    }
}
