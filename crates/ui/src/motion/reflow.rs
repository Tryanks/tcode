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
const GLIDE: Duration = crate::edge_back::SETTLE;

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
    glides: HashMap<SharedString, Glide>,
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
}

/// The reflow scope `id` of the view rendering it.
pub(crate) fn reflow(id: impl Into<ElementId>, window: &mut Window, cx: &mut App) -> Reflow {
    let id = id.into();
    let state = window.use_keyed_state(super::child_id(&id, "reflow"), cx, |_, _| {
        Rc::new(RefCell::new(State::default()))
    });
    Reflow {
        state: state.read(cx).clone(),
        frame: Rc::default(),
    }
}

impl Reflow {
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
        frame.origin.set(bounds.origin);
        let last = self
            .reflow
            .state
            .borrow_mut()
            .width
            .replace(bounds.size.width);
        frame.resized.set(
            last.map(|last| bounds.size.width - last)
                .filter(|change| *change != Pixels::ZERO),
        );
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
        let moved = state.bounds.measure(&self.key, frame.origin.get(), bounds);
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
        });
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
        window.with_element_offset(offset, |window| self.child.prepaint(window, cx));
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
}
