//! A surface that grows out of what was pressed to open it, and shrinks
//! back into it as it closes: a container transform.
//!
//! gpui-base has no shared-element transition, and [`super::shared`] only
//! moves an element of one size between sites. Here what is pressed (an
//! origin, [`Origins::mark`]) records where it was drawn, by key, and the
//! surface opened from it ([`Morph`]) is drawn from that rectangle, with its
//! corners, to its own place. Only the surface's frame moves: what it holds
//! is laid out at a size of its own throughout, clipped to the frame, so
//! nothing in it is measured again while it moves.
//!
//! GPUI's opacity multiplies down through an element's children, so the
//! frame and what it holds are siblings: each fades on its own.

use std::{cell::RefCell, collections::HashMap, rc::Rc};

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, Div, Element, ElementId, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, ParentElement as _, Pixels, SharedString,
    Styled as _, Window, div, px, size,
};

/// Where the origins of a view were drawn, by key; kept by the view and
/// cloned into whatever draws them. Every clone is the same record.
#[derive(Clone, Default)]
pub(crate) struct Origins {
    origins: Rc<RefCell<Marks>>,
    /// The scene of the view whose origins these are: a new one forgets the
    /// last one's.
    scene: super::InScene,
}

#[derive(Default)]
struct Marks {
    /// Counts the view's renders, so an origin drawn on the last one can be
    /// told from one no longer drawn.
    frame: u64,
    marks: HashMap<SharedString, Mark>,
}

#[derive(Clone, Copy)]
struct Mark {
    /// The part of it on screen when last drawn, in window coordinates, and
    /// the radius of its corners; `None` when it was drawn wholly clipped
    /// away (a row scrolled out of its list).
    shown: Option<(Bounds<Pixels>, Pixels)>,
    frame: u64,
}

impl Origins {
    /// The record of a view in `epoch`.
    pub(crate) fn in_scene(epoch: &super::Epoch) -> Self {
        Self {
            origins: Rc::default(),
            scene: super::InScene::new(epoch),
        }
    }

    fn marks(&self) -> std::cell::RefMut<'_, Marks> {
        let mut marks = self.origins.borrow_mut();
        if self.scene.entered() {
            marks.marks.clear();
        }
        marks
    }

    /// Begins one render of the view. Origins not drawn for a while are
    /// forgotten.
    pub(crate) fn begin_frame(&self) {
        let mut marks = self.marks();
        marks.frame += 1;
        let frame = marks.frame;
        marks.marks.retain(|_, mark| mark.frame + 2 >= frame);
    }

    /// Where origin `key` is on screen, with its corner radius: drawn on the
    /// view's last frame, and not wholly clipped away there.
    pub(crate) fn resolve(&self, key: &str) -> Option<(Bounds<Pixels>, Pixels)> {
        let marks = self.marks();
        marks
            .marks
            .get(key)
            .filter(|mark| mark.frame + 1 >= marks.frame)
            .and_then(|mark| mark.shown)
    }

    /// Records that `key` was drawn at `bounds`, with corners `radius` round,
    /// as much of it as the window's content mask shows.
    fn record(&self, key: SharedString, bounds: Bounds<Pixels>, radius: Pixels, window: &Window) {
        let visible = bounds.intersect(&window.content_mask().bounds);
        let shown = (visible.size.width > px(0.) && visible.size.height > px(0.))
            .then_some((visible, radius));
        let mut marks = self.marks();
        let frame = marks.frame;
        marks.marks.insert(key, Mark { shown, frame });
    }

    /// `child`, the origin `key`, its corners `radius` round: where it is
    /// drawn is recorded each frame it is.
    pub(crate) fn mark(
        &self,
        key: impl Into<SharedString>,
        radius: Pixels,
        child: impl IntoElement,
    ) -> Origin {
        Origin {
            key: key.into(),
            radius,
            origins: self.clone(),
            child: child.into_any_element(),
        }
    }
}

/// An origin, laid out and drawn as its child is.
pub(crate) struct Origin {
    key: SharedString,
    radius: Pixels,
    origins: Origins,
    child: AnyElement,
}

impl IntoElement for Origin {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Origin {
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
        self.origins
            .record(self.key.clone(), bounds, self.radius, window);
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

/// A surface on its way from an origin to its place, which is the bounds it
/// is laid out at, or there at rest.
pub(crate) struct Morph {
    /// The origin it comes from or goes back to, and its corner radius.
    from: Option<(Bounds<Pixels>, Pixels)>,
    /// How far from the origin it is, from 0 (on it) to 1 (in its place).
    growth: f32,
    /// Where its frame rests instead of its place, and the radius of its
    /// corners there: what it holds is still laid out at its place.
    held: Option<(Bounds<Pixels>, Pixels)>,
    /// Where what it holds is laid out instead of its place.
    laid: Option<Bounds<Pixels>>,
    /// How far in the frame's fill, border and shadow are.
    surface: f32,
    /// The keys the frame and what it holds are recorded under each frame,
    /// as an origin is.
    mark: Option<(Origins, SharedString, SharedString)>,
    /// The surface's frame, drawn at the size it has now, its corners
    /// `radius` round at rest, inside a border `border` wide.
    frame: Option<crate::material::Raised<Div>>,
    radius: Pixels,
    border: Pixels,
    content: Option<AnyElement>,
}

impl Morph {
    /// `content` on `frame`, at rest in its place.
    pub(crate) fn new(
        frame: crate::material::Raised<Div>,
        radius: Pixels,
        border: Pixels,
        content: AnyElement,
    ) -> Self {
        Self {
            from: None,
            growth: 1.,
            held: None,
            laid: None,
            surface: 1.,
            mark: None,
            frame: Some(frame),
            radius,
            border,
            content: Some(content),
        }
    }

    /// `growth` of the way from `from`, a rectangle with corners that round.
    pub(crate) fn from(mut self, from: Option<(Bounds<Pixels>, Pixels)>, growth: f32) -> Self {
        self.from = from;
        self.growth = growth;
        self
    }

    /// Its frame rests at `held` rather than its place, where it was drawn
    /// as it stopped moving there.
    pub(crate) fn held(mut self, held: Option<(Bounds<Pixels>, Pixels)>) -> Self {
        self.held = held;
        self
    }

    /// What it holds stays laid out at `laid`, in window coordinates, rather
    /// than at its place.
    pub(crate) fn laid(mut self, laid: Option<Bounds<Pixels>>) -> Self {
        self.laid = laid;
        self
    }

    /// Its fill, border and shadow are `surface` of the way in; what it
    /// holds is not.
    pub(crate) fn surface(mut self, surface: f32) -> Self {
        self.surface = surface;
        self
    }

    /// Where its frame and what it holds are drawn each frame are recorded
    /// in `origins` as `frame` and `content`, so what it does next can start
    /// from there.
    pub(crate) fn mark(
        mut self,
        origins: &Origins,
        frame: impl Into<SharedString>,
        content: impl Into<SharedString>,
    ) -> Self {
        self.mark = Some((origins.clone(), frame.into(), content.into()));
        self
    }
}

impl IntoElement for Morph {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

fn lerp(from: Pixels, to: Pixels, t: f32) -> Pixels {
    from + (to - from) * t
}

impl Element for Morph {
    type RequestLayoutState = ();
    type PrepaintState = Option<AnyElement>;

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
        let mut style = gpui::Style::default();
        style.size.width = gpui::relative(1.).into();
        style.size.height = gpui::relative(1.).into();
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        place: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        let t = self.growth.clamp(0., 1.);
        let (rest, rest_radius) = self.held.unwrap_or((place, self.radius));
        let (now, radius) = match self.from.filter(|_| t < 1.) {
            Some((from, from_radius)) => (
                Bounds::from_corners(
                    gpui::point(
                        lerp(from.left(), rest.left(), t),
                        lerp(from.top(), rest.top(), t),
                    ),
                    gpui::point(
                        lerp(from.right(), rest.right(), t),
                        lerp(from.bottom(), rest.bottom(), t),
                    ),
                ),
                lerp(from_radius, rest_radius, t),
            ),
            None => (rest, rest_radius),
        };
        let laid = self.laid.unwrap_or(place);
        if let Some((origins, frame, content)) = &self.mark {
            origins.record(frame.clone(), now, radius, window);
            origins.record(content.clone(), laid, px(0.), window);
        }
        let border = self.border;
        let content = self.content.take()?;
        let frame = self.frame.take()?.size_full().rounded(radius);
        let held = div()
            .absolute()
            .left(border)
            .top(border)
            .w((now.size.width - border * 2.).max(px(0.)))
            .h((now.size.height - border * 2.).max(px(0.)))
            .rounded((radius - border).max(px(0.)))
            .overflow_hidden()
            .child(
                div()
                    .absolute()
                    .left(laid.left() - now.left())
                    .top(laid.top() - now.top())
                    .w((laid.size.width - border * 2.).max(px(0.)))
                    .h((laid.size.height - border * 2.).max(px(0.)))
                    .child(content),
            );
        let mut surface = div()
            .relative()
            .w(now.size.width)
            .h(now.size.height)
            .child(frame.opacity(self.surface.clamp(0., 1.)))
            .child(held)
            .into_any_element();
        surface.layout_as_root(
            size(
                AvailableSpace::Definite(now.size.width),
                AvailableSpace::Definite(now.size.height),
            ),
            window,
            cx,
        );
        surface.prepaint_at(now.origin, window, cx);
        Some(surface)
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        surface: &mut Option<AnyElement>,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(surface) = surface {
            surface.paint(window, cx);
        }
    }
}
