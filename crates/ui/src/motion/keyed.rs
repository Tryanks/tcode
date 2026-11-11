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

use super::{Clock, Curve, Shown};
use gpui::{
    AnyElement, App, Bounds, ElementId, IntoElement, ParentElement, Pixels, Point, SharedString,
    Window, px,
};

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
/// back to zero over `response`, keeping its velocity through a later jump.
/// Kept by `clock` under `id`; reduced motion draws it in place at once.
pub(crate) fn flip_offset(
    clock: Clock<'_>,
    id: impl Into<ElementId>,
    jump: Point<Pixels>,
    response: std::time::Duration,
    window: &mut Window,
    cx: &mut App,
) -> Point<Pixels> {
    let id = id.into();
    // The spring tracks minus the sum of every jump, so a new jump moves its
    // target and leaves its position, which is the offset still to travel.
    let jumps: Rc<Cell<Point<Pixels>>> = clock.keep(super::child_id(&id, "flip-jumps"), window, cx);
    if jump != Point::default() {
        jumps.set(jumps.get() + jump);
    }
    let jumps = jumps.get();
    let mut follow = |axis: &'static str, jump: Pixels| {
        let id = super::child_id(&id, axis);
        px(clock.follow(id, f32::from(-jump), response, window, cx)) + jump
    };
    Point::new(follow("flip-x", jumps.x), follow("flip-y", jumps.y))
}

thread_local! {
    /// How far the glides of the rows being prepainted draw them from their
    /// places, summed over every gliding row around them.
    static GLIDING: Cell<Point<Pixels>> = Cell::default();
}

/// Prepaints, with `prepaint`, a row drawn `offset` from its laid-out place
/// as it glides there; what it holds can ask [`gliding`] how far.
pub(crate) fn prepaint_gliding<R>(
    offset: Point<Pixels>,
    window: &mut Window,
    prepaint: impl FnOnce(&mut Window) -> R,
) -> R {
    let around = GLIDING.get();
    GLIDING.set(around + offset);
    let prepainted = window.with_element_offset(offset, prepaint);
    GLIDING.set(around);
    prepainted
}

/// How far from where it is laid out what is being prepainted is drawn by
/// the glides of the rows it is in ([`prepaint_gliding`]).
pub(crate) fn gliding() -> Point<Pixels> {
    GLIDING.get()
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

/// A list's state, as its window or its owner keeps it.
struct Kept<T>(Rc<RefCell<ListState<T>>>);

impl<T> Clone for Kept<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> Default for Kept<T> {
    fn default() -> Self {
        Self(Rc::new(RefCell::new(ListState {
            drawn: Vec::new(),
            values: HashMap::new(),
            bounds: FlipBounds::default(),
            started: false,
        })))
    }
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
    curve: impl Into<Curve>,
    window: &mut Window,
    cx: &mut C,
    build: impl FnMut(&SharedString, &T, &mut Window, &mut C) -> AnyElement,
) -> Keyed {
    keyed_along(
        id.into(),
        Clock::Element,
        Fold::Down,
        items,
        curve.into(),
        window,
        cx,
        build,
    )
}

/// [`keyed`] for rows that sit side by side — chips, tabs, buttons in a row
/// or a wrapping row: an arriving or leaving one folds along its width, so
/// its neighbours move over as it does.
pub(crate) fn keyed_across<T: Clone + 'static, C: BorrowMut<App>>(
    id: impl Into<ElementId>,
    items: impl IntoIterator<Item = (SharedString, T)>,
    curve: impl Into<Curve>,
    window: &mut Window,
    cx: &mut C,
    build: impl FnMut(&SharedString, &T, &mut Window, &mut C) -> AnyElement,
) -> Keyed {
    keyed_along(
        id.into(),
        Clock::Element,
        Fold::Across(px(0.)),
        items,
        curve.into(),
        window,
        cx,
        build,
    )
}

/// How the rows of [`keyed_in`] fold in and out.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Fold {
    /// In a column, along their height ([`keyed`]).
    Down,
    /// Side by side, along their width, each bringing the row's `gap` before
    /// it ([`Shown::fold_across_gap`]).
    Across(Pixels),
    /// Side by side in a row that divides its width among them, each
    /// bringing the row's `gap` before it ([`Shown::fold_share`]).
    Share(Pixels),
}

/// [`keyed`] with the rows folding as `fold` says.
pub(crate) fn keyed_in<T: Clone + 'static, C: BorrowMut<App>>(
    id: impl Into<ElementId>,
    fold: Fold,
    items: impl IntoIterator<Item = (SharedString, T)>,
    curve: impl Into<Curve>,
    window: &mut Window,
    cx: &mut C,
    build: impl FnMut(&SharedString, &T, &mut Window, &mut C) -> AnyElement,
) -> Keyed {
    keyed_along(
        id.into(),
        Clock::Element,
        fold,
        items,
        curve.into(),
        window,
        cx,
        build,
    )
}

/// [`keyed_in`] with every row's clock, value, fold and glide kept by
/// `motion` under `key` rather than in element state, so rows coming,
/// going or changing places carry on when the view is drawn under a new
/// parent. `key` is also the list's element id.
#[allow(clippy::too_many_arguments)]
pub(crate) fn keyed_owned<T: Clone + 'static, C: BorrowMut<App>>(
    motion: &super::owned::Owned,
    key: impl Into<SharedString>,
    fold: Fold,
    items: impl IntoIterator<Item = (SharedString, T)>,
    curve: impl Into<Curve>,
    window: &mut Window,
    cx: &mut C,
    build: impl FnMut(&SharedString, &T, &mut Window, &mut C) -> AnyElement,
) -> Keyed {
    let key: SharedString = key.into();
    let id = ElementId::from(key.clone());
    keyed_along(
        id,
        Clock::Owned(&motion.scope(&key)),
        fold,
        items,
        curve.into(),
        window,
        cx,
        build,
    )
}

#[allow(clippy::too_many_arguments)]
fn keyed_along<T: Clone + 'static, C: BorrowMut<App>>(
    id: ElementId,
    clock: Clock<'_>,
    fold: Fold,
    items: impl IntoIterator<Item = (SharedString, T)>,
    curve: Curve,
    window: &mut Window,
    cx: &mut C,
    mut build: impl FnMut(&SharedString, &T, &mut Window, &mut C) -> AnyElement,
) -> Keyed {
    let items: Vec<(SharedString, T)> = items.into_iter().collect();
    let app: &mut App = cx.borrow_mut();
    let state = clock
        .keep::<Kept<T>>(super::child_id(&id, "keyed"), window, app)
        .0;
    let (drawn, started, bounds) = {
        let state = state.borrow();
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
        // Seeded absent, so a key that arrives after the list opened comes
        // in rather than starting at rest.
        let arrives = here && started && !drawn.contains(&key);
        let target = if here { 1f32 } else { 0. };
        let presence = super::child_id(&row_id(&key), "presence");
        if arrives {
            clock.transition(presence.clone(), 0f32, &curve, window, app);
        }
        let (progress, settled) = clock.glide(presence, target, &curve, window, app);
        let gone = !here && progress <= 0. && settled;
        if !gone {
            rows.push((key, here, progress));
        }
    }

    let now: Vec<SharedString> = rows.iter().map(|(key, ..)| key.clone()).collect();
    let jumped = now != drawn;
    let leaving: Vec<(SharedString, T)> = {
        let mut state = RefCell::borrow_mut(&state);
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
    };
    bounds.retain(|key| now.contains(key));

    let origin = Rc::new(Cell::new(Point::default()));
    let mut values: HashMap<SharedString, T> = items.into_iter().chain(leaving).collect();
    let rows = rows
        .into_iter()
        .filter_map(|(key, here, progress)| {
            let value = values.remove(&key)?;
            let id = row_id(&key);
            let content = build(&key, &value, window, cx);
            let measure = match clock {
                Clock::Element => None,
                Clock::Owned(_) => {
                    Some(clock.keep(super::child_id(&id, "measure"), window, cx.borrow_mut()))
                }
            };
            let shown = Shown {
                id: id.clone(),
                value: (),
                progress,
                leaving: !here,
                measure,
            };
            Some(
                FlipRow {
                    id,
                    motion: match clock {
                        Clock::Element => None,
                        Clock::Owned(motion) => Some(motion.scope(&key)),
                    },
                    response: curve.duration(),
                    key,
                    jumped,
                    wraps: matches!(fold, Fold::Across(_)),
                    bounds: bounds.clone(),
                    origin: origin.clone(),
                    child: match fold {
                        Fold::Down => shown.fold(content),
                        Fold::Across(gap) => shown.fold_across_gap(gap, content),
                        Fold::Share(gap) => shown.fold_share(gap, content),
                    },
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
    /// `elements` drawn after the rows, over them.
    pub(crate) fn followed_by(mut self, elements: impl IntoIterator<Item = AnyElement>) -> Self {
        self.rows.extend(elements);
        self
    }

    /// The rows as the children of `container`, which lays them out; they
    /// glide relative to it.
    pub(crate) fn render(self, container: impl ParentElement + IntoElement) -> AnyElement {
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
    /// How long it springs to a new place over.
    response: std::time::Duration,
    /// The row's own part of its list's owner, which keeps its glide.
    motion: Option<super::owned::Owned>,
    key: SharedString,
    /// Whether the list's rows changed this frame, so a move is a jump to
    /// glide rather than a fold pushing it along.
    jumped: bool,
    /// Rows side by side that may wrap: one a fold pushes onto another line
    /// jumps there, so a move to another line glides whenever it happens.
    wraps: bool,
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
        let jump = if self.jumped || (self.wraps && moved.y != Pixels::ZERO) {
            moved
        } else {
            Point::default()
        };
        let offset = flip_offset(
            Clock::from(self.motion.as_ref()),
            self.id.clone(),
            jump,
            self.response,
            window,
            cx,
        );
        prepaint_gliding(offset, window, |window| self.child.prepaint(window, cx));
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
