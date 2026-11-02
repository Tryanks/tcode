//! Keyed siblings in ordinary flow: rows that arrive, leave and change places.
//!
//! gpui lays a container's children out afresh each frame and has no notion of
//! a child that arrives, leaves or moves; gpui-base has `Presence` for one
//! element but nothing for a set of keyed siblings, and no FLIP. A row that is
//! no longer built cannot leave, and one whose place changed is simply drawn
//! there. [`keyed`] keeps each row's last value so a removed row is rebuilt
//! while it folds away, folds new rows in, and glides survivors from where they
//! were laid out on the last frame ([`FlipBounds`], [`flip_offset`]).

use std::{
    borrow::BorrowMut,
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::Arc,
};

use gpui::{
    AnyElement, App, Bounds, Div, ElementId, IntoElement, ParentElement as _, Pixels, Point,
    SharedString, Window,
};
use gpui_base::motion::{MotionStatus, spring, transition, transition_with_status};

use super::{FOLLOW, Shown, settle};

/// Where each participant of one scope was laid out on the last frame that
/// drew it, relative to the scope's own origin, by a row key that is stable
/// for as long as the row means the same thing (a project id, a machine id).
///
/// Relative to the scope, so a scroll or a move of the scope as a whole is not
/// a jump. Whoever owns the handle decides what the scope is: [`keyed`] keeps
/// one per list; an owner that shares one handle between several scopes drawn
/// in one coordinate space can follow a key from one to another.
#[derive(Clone, Default)]
pub(crate) struct FlipBounds(Rc<RefCell<HashMap<SharedString, Bounds<Pixels>>>>);

impl FlipBounds {
    /// Records that `key` is laid out at `bounds` under a scope at `origin`,
    /// and returns how far it moved since it was last recorded: from where it
    /// was to where it is, zero the first time.
    pub(crate) fn measure(
        &self,
        key: &SharedString,
        origin: Point<Pixels>,
        bounds: Bounds<Pixels>,
    ) -> Point<Pixels> {
        let now = Bounds::new(bounds.origin - origin, bounds.size);
        RefCell::borrow_mut(&self.0)
            .insert(key.clone(), now)
            .map_or_else(Point::default, |last| last.origin - now.origin)
    }

    /// Where `key` was laid out when last recorded, relative to its scope.
    pub(crate) fn get(&self, key: &SharedString) -> Option<Bounds<Pixels>> {
        self.0.borrow().get(key).copied()
    }

    /// Forgets every key `keep` rejects.
    pub(crate) fn retain(&self, keep: impl Fn(&SharedString) -> bool) {
        RefCell::borrow_mut(&self.0).retain(|key, _| keep(key));
    }
}

/// How far from its laid-out place to draw a participant whose layout just
/// jumped by `jump` (old place minus new; zero on a frame it did not): the
/// jump is added to whatever offset it was still carrying, which then springs
/// back to zero on [`FOLLOW`], keeping its velocity through a later jump.
/// Keyed by `id` under the element sampling it; reduced motion draws it in
/// place at once.
pub(crate) fn flip_offset(
    id: impl Into<ElementId>,
    jump: Point<Pixels>,
    window: &mut Window,
    cx: &mut App,
) -> Point<Pixels> {
    let id = id.into();
    // The spring tracks minus the sum of every jump, so a new jump moves its
    // target and leaves its position, which is the offset still to travel.
    let jumps = window.use_keyed_state(super::child_id(&id, "flip-jumps"), cx, |_, _| {
        Point::<Pixels>::default()
    });
    if jump != Point::default() {
        jumps.update(cx, |jumps, _| *jumps += jump);
    }
    let jumps = *jumps.read(cx);
    let x = spring((id.clone(), "flip-x"), -jumps.x, FOLLOW, window, cx);
    let y = spring((id, "flip-y"), -jumps.y, FOLLOW, window, cx);
    Point::new(x + jumps.x, y + jumps.y)
}

struct ListState<T> {
    /// The keys drawn last frame, in order, those leaving included.
    drawn: Vec<SharedString>,
    /// Each drawn key's last value, from which a leaving row is rebuilt.
    values: HashMap<SharedString, T>,
    bounds: FlipBounds,
    /// Whether the list has been drawn, so a key it opens with is at rest.
    started: bool,
}

/// Rows keyed by identity, for [`Keyed::render`] to lay out: a key the list
/// opens with is at rest; a new key folds in; a key that goes is rebuilt from
/// its last value by `build` and folds away in its old place, taking no input;
/// a row whose place changes glides there from where it was. `build` draws
/// every row, arriving, present or leaving, with the same element ids, so a
/// row keeps its state through all three. `cx` is the caller's own context,
/// so a view's `build` can make listeners.
pub(crate) fn keyed<T: Clone + 'static, C: BorrowMut<App>>(
    id: impl Into<ElementId>,
    items: impl IntoIterator<Item = (SharedString, T)>,
    window: &mut Window,
    cx: &mut C,
    mut build: impl FnMut(&SharedString, &T, &mut Window, &mut C) -> AnyElement,
) -> Keyed {
    let id = id.into();
    let items: Vec<(SharedString, T)> = items.into_iter().collect();
    let app: &mut App = cx.borrow_mut();
    let state = window.use_keyed_state(super::child_id(&id, "keyed"), app, |_, _| ListState {
        drawn: Vec::new(),
        values: HashMap::new(),
        bounds: FlipBounds::default(),
        started: false,
    });
    let (drawn, started, bounds) = {
        let state = state.read(app);
        (state.drawn.clone(), state.started, state.bounds.clone())
    };
    let present: HashSet<&SharedString> = items.iter().map(|(key, _)| key).collect();

    // A key that goes stays after the key it followed.
    let mut order: Vec<SharedString> = items.iter().map(|(key, _)| key.clone()).collect();
    for (index, key) in drawn.iter().enumerate() {
        if present.contains(key) {
            continue;
        }
        let at = drawn[..index]
            .iter()
            .rev()
            .find_map(|before| order.iter().position(|key| key == before))
            .map_or(0, |at| at + 1);
        order.insert(at, key.clone());
    }

    let row_id = |key: &SharedString| ElementId::NamedChild(Arc::new(id.clone()), key.clone());
    let mut rows = Vec::with_capacity(order.len());
    for key in order {
        let here = present.contains(&key);
        let presence = (row_id(&key), "presence");
        if here && started && !drawn.contains(&key) {
            // Seeded absent, so a key that arrives after the list opened comes
            // in rather than starting at rest.
            transition(presence.clone(), 0f32, settle(), window, app);
        }
        let sample = transition_with_status(
            presence,
            if here { 1f32 } else { 0. },
            settle(),
            window,
            app,
        );
        let gone = !here
            && sample.value <= 0.
            && matches!(sample.status, MotionStatus::Finished | MotionStatus::Idle);
        if !gone {
            rows.push((key, here, sample.value));
        }
    }

    let now: Vec<SharedString> = rows.iter().map(|(key, ..)| key.clone()).collect();
    let jumped = now != drawn;
    let leaving: Vec<(SharedString, T)> = state.update(app, |state, _| {
        for (key, value) in &items {
            state.values.insert(key.clone(), value.clone());
        }
        state.values.retain(|key, _| now.contains(key));
        state.drawn = now.clone();
        state.started = true;
        rows.iter()
            .filter(|(_, here, _)| !here)
            .filter_map(|(key, ..)| Some((key.clone(), state.values.get(key)?.clone())))
            .collect()
    });
    bounds.retain(|key| now.contains(key));

    let origin = Rc::new(Cell::new(Point::default()));
    let mut values: HashMap<SharedString, T> = items.into_iter().chain(leaving).collect();
    let rows = rows
        .into_iter()
        .filter_map(|(key, here, progress)| {
            let value = values.remove(&key)?;
            let id = row_id(&key);
            let content = build(&key, &value, window, cx);
            let shown = Shown {
                id: id.clone(),
                value: (),
                progress,
                leaving: !here,
            };
            Some(
                FlipRow {
                    id,
                    key,
                    jumped,
                    bounds: bounds.clone(),
                    origin: origin.clone(),
                    child: shown.fold(content),
                }
                .into_any_element(),
            )
        })
        .collect();
    Keyed { origin, rows }
}

/// The rows [`keyed`] built, for a container to lay out.
pub(crate) struct Keyed {
    origin: Rc<Cell<Point<Pixels>>>,
    rows: Vec<AnyElement>,
}

impl Keyed {
    /// The rows as the children of `container`, which lays them out; they
    /// glide relative to it.
    pub(crate) fn render(self, container: Div) -> AnyElement {
        Scope {
            origin: self.origin,
            child: container.children(self.rows).into_any_element(),
        }
        .into_any_element()
    }
}

/// The frame of reference a list's rows measure their places in.
struct Scope {
    origin: Rc<Cell<Point<Pixels>>>,
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
        self.origin.set(bounds.origin);
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

/// One row, drawn (and hit) offset from its laid-out place while it glides;
/// its layout, and so its neighbours', is never moved.
struct FlipRow {
    id: ElementId,
    key: SharedString,
    /// Whether the list's rows changed this frame, so a move is a jump to
    /// glide rather than a fold pushing it along.
    jumped: bool,
    bounds: FlipBounds,
    origin: Rc<Cell<Point<Pixels>>>,
    child: AnyElement,
}

impl IntoElement for FlipRow {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl gpui::Element for FlipRow {
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
        let moved = self.bounds.measure(&self.key, self.origin.get(), bounds);
        let jump = if self.jumped { moved } else { Point::default() };
        let offset = flip_offset(self.id.clone(), jump, window, cx);
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
    use std::{cell::RefCell, rc::Rc};

    use gpui::{
        Context, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
        StatefulInteractiveElement as _, Styled as _, TestAppContext, VisualTestContext, Window,
        div, px,
    };

    use crate::test_frames::{draw, draw_settled};

    struct Rows {
        order: Vec<&'static str>,
        presses: Rc<RefCell<Vec<&'static str>>>,
    }

    impl Render for Rows {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let items = self
                .order
                .iter()
                .map(|name| (SharedString::from(*name), *name));
            let presses = self.presses.clone();
            let rows = super::keyed("rows", items, window, cx, move |_, name, _, _| {
                let (name, presses) = (*name, presses.clone());
                div()
                    .id(name)
                    .debug_selector(move || name.to_string())
                    .w_full()
                    .h(px(20.))
                    .on_click(move |_, _, _| presses.borrow_mut().push(name))
                    .into_any_element()
            });
            div()
                .size_full()
                .child(rows.render(div().w(px(100.)).flex().flex_col()))
        }
    }

    /// Rows that trade places are drawn, and pressed, where they were, then
    /// glide to where they now are; with motion reduced they are there at once.
    #[gpui::test]
    fn rows_that_trade_places_glide_there(cx: &mut TestAppContext) {
        let presses = Rc::new(RefCell::new(Vec::new()));
        let (rows, cx) = cx.add_window_view(|_, _| Rows {
            order: vec!["a", "b"],
            presses: presses.clone(),
        });
        let top = |name: &'static str, cx: &mut VisualTestContext| {
            cx.debug_bounds(name).expect("the row is drawn").top()
        };
        draw_settled(cx);
        let (first, second) = (top("a", cx), top("b", cx));
        let reorder = |order: Vec<&'static str>, cx: &mut VisualTestContext| {
            rows.update(cx, |rows, cx| {
                rows.order = order;
                cx.notify();
            });
            draw(cx);
        };

        reorder(vec!["b", "a"], cx);
        assert_eq!(top("a", cx), first, "a starts from where it was");
        let a = cx.debug_bounds("a").unwrap();
        cx.simulate_click(a.center(), Default::default());
        assert_eq!(
            *presses.borrow(),
            ["a"],
            "a takes presses where it is drawn"
        );

        cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
        draw(cx);
        let gliding = top("a", cx);
        assert!(gliding > first && gliding < second, "a is on its way");

        draw_settled(cx);
        assert_eq!((top("b", cx), top("a", cx)), (first, second));

        cx.update(|_, cx| cx.set_reduce_motion(true));
        reorder(vec!["a", "b"], cx);
        assert_eq!((top("a", cx), top("b", cx)), (first, second));
    }
}
