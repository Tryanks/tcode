//! A drag's ghost: the row a pointer or a finger carries, lifted off its
//! place, and set down again where it lands or back where it came from.
//!
//! GPUI draws the view a drag carries itself, at the pointer from the frame
//! the drag starts and not at all from the frame it ends, so that view can
//! neither lift off its row nor settle anywhere; gpui-base's dock drag
//! carries the same view. So the drag GPUI carries draws nothing
//! ([`Carried`]; it still carries the payload, so drop targets, the dock's
//! included, work as before) and a window overlay draws the ghost instead,
//! from [`lift`] until it has settled into its row's [`slot`], or into the
//! rect a drop target hands it with [`land_in`]. Where a slot is laid out is
//! measured into a [`FlipBounds`] each frame, and the ghost travels there on
//! [`flip_offset`], so it follows a slot that is itself still moving.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
};

use gpui::{
    AnyElement, App, AppContext as _, Bounds, Context, ElementId, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, Pixels, Point, Render, SharedString, Styled, Subscription,
    WeakEntity, Window, WindowId, deferred, div, prelude::FluentBuilder as _,
};
use gpui_base::motion::{MotionStatus, Transition, transition, transition_with_status};

use super::{
    LIFT, child_id,
    keyed::{FlipBounds, flip_offset},
};

/// How long a ghost takes to lift off its row as a drag picks it up, and to
/// settle into its slot as the drag drops it.
const LIFTING: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a row drawn here takes to spring to a new place.
const FLIP: std::time::Duration = std::time::Duration::from_millis(200);

type Content = Rc<dyn Fn(f32, &mut Window, &mut App) -> AnyElement>;

/// Mounts the ghost layer on every window opened after this; after the
/// dialog and toast layers, so a ghost passes over them.
pub(crate) fn init(cx: &mut App) {
    gpui_base::Root::register_plugin(cx, DragGhosts::new);
}

/// What a drag gives GPUI to draw: nothing, as the ghost layer draws it.
pub(crate) struct Carried;

impl Render for Carried {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// Lifts the row laid out in `key`'s [`slot`] off its place, for the
/// constructor GPUI's `on_drag` calls: the ghost starts where the row is and
/// keeps the pointer where it took hold of it. `grab` is the pointer's offset
/// into the drag's source, which places the ghost when the row has no slot.
/// `content` draws the row as it is carried, filling the slot's width, every
/// frame until the ghost has settled; it is given how far the ghost is
/// lifted, for [`lifted`]. Returns the view GPUI carries.
pub(crate) fn lift(
    key: impl Into<SharedString>,
    grab: Point<Pixels>,
    content: impl Fn(f32, &mut Window, &mut App) -> AnyElement + 'static,
    window: &mut Window,
    cx: &mut App,
) -> Entity<Carried> {
    let key = key.into();
    if let Some(host) = host(window, cx) {
        let pointer = window.mouse_position();
        host.update(cx, |host, cx| {
            for ghost in &mut host.ghosts {
                ghost.release();
            }
            let (at, width) = match host.slots.get(&key) {
                Some(slot) => (slot.origin, Some(slot.size.width)),
                None => (pointer - grab, None),
            };
            host.ghosts.push(Ghost {
                id: host.next,
                key,
                content: Rc::new(content),
                grab: pointer - at,
                width,
                at,
                flight: Flight::Carried,
                fresh: true,
                shown: 1.,
            });
            host.next += 1;
            cx.notify();
        });
    }
    cx.new(|_| Carried)
}

/// The drop target took the drag: the ghost goes into `bounds` (window
/// coordinates) and fades there, instead of back into its row's slot.
pub(crate) fn land_in(bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
    let Some(host) = host(window, cx) else {
        return;
    };
    host.update(cx, |host, cx| {
        if let Some(ghost) = host.ghosts.last_mut()
            && matches!(
                ghost.flight,
                Flight::Carried | Flight::Landing { into: None, .. }
            )
        {
            ghost.flight = Flight::Landing {
                into: Some(bounds),
                toward: None,
            };
            cx.notify();
        }
    });
}

/// Where the row `key` is laid out, for the ghost to lift from and settle
/// into: `element` is measured each frame it is drawn, and is hidden while
/// its ghost is carried and fades in as the ghost settles into it.
pub(crate) fn slot(
    key: impl Into<SharedString>,
    element: impl IntoElement,
    window: &Window,
    cx: &App,
) -> AnyElement {
    let key = key.into();
    let Some(host) = host(window, cx) else {
        return element.into_any_element();
    };
    let host = host.read(cx);
    let cover = host
        .ghosts
        .iter()
        .rev()
        .find(|ghost| ghost.key == key)
        .map(Ghost::cover);
    Slot {
        key,
        slots: host.slots.clone(),
        seen: host.seen.clone(),
        child: div()
            .when_some(cover, |slot, opacity| slot.opacity(opacity))
            .child(element)
            .into_any_element(),
    }
    .into_any_element()
}

/// `el` with the shadow of a lifted surface, as far as `lift` has it off the
/// page.
pub(crate) fn lifted<E: Styled>(el: E, lift: f32) -> E {
    let mut el = el.shadow_lg();
    for shadow in el.style().box_shadow.iter_mut().flatten() {
        shadow.color.a *= lift.clamp(0., 1.);
    }
    el
}

fn host(window: &Window, cx: &App) -> Option<Entity<Layer>> {
    cx.try_global::<Layers>()?
        .0
        .get(&window.window_handle().window_id())?
        .upgrade()
}

/// Each window's ghosts, found without reading gpui-base's `Root` or the
/// plugin that draws them. Root updates every plugin each time it renders,
/// and is updated itself whenever one of them notifies; gpui-fast counts
/// either as a change to what a view read, so a row that read them to find
/// its slot would be drawn again with every frame of a tooltip or a toast.
#[derive(Default)]
struct Layers(HashMap<WindowId, WeakEntity<Layer>>);

impl gpui::Global for Layers {}

/// The overlay drawing a window's ghosts, over the window's other layers.
pub(crate) struct DragGhosts {
    layer: Entity<Layer>,
    _layer_changed: Subscription,
}

/// The window's ghosts: the one being carried, and any still settling.
struct Layer {
    ghosts: Vec<Ghost>,
    next: u64,
    /// Every slot drawn last frame, in window coordinates.
    slots: FlipBounds,
    /// The slots measured since the layer last rendered.
    seen: Rc<RefCell<HashSet<SharedString>>>,
}

struct Ghost {
    id: u64,
    key: SharedString,
    content: Content,
    /// Where the pointer holds the ghost, from its top left.
    grab: Point<Pixels>,
    width: Option<Pixels>,
    /// Where it was last drawn, before it rose.
    at: Point<Pixels>,
    flight: Flight,
    /// Whether it has not been drawn yet, so it lifts from the page.
    fresh: bool,
    /// The opacity it was last drawn at.
    shown: f32,
}

enum Flight {
    Carried,
    /// Let go: on its way into `into`, or its row's slot when that is
    /// `None`; `toward` is what it was headed for last frame, and where.
    Landing {
        into: Option<Bounds<Pixels>>,
        toward: Option<(Toward, Point<Pixels>)>,
    },
}

#[derive(Clone, Copy)]
enum Toward {
    Rect,
    Slot,
    /// Its slot is not drawn, so it fades where it is.
    Here,
}

impl Ghost {
    fn release(&mut self) {
        if matches!(self.flight, Flight::Carried) {
            self.flight = Flight::Landing {
                into: None,
                toward: None,
            };
        }
    }

    /// How opaque its slot is drawn: hidden while it is carried, and taking
    /// over from the ghost as it settles.
    fn cover(&self) -> f32 {
        match self.flight {
            Flight::Carried => 0.,
            Flight::Landing { .. } => 1. - self.shown,
        }
    }
}

impl DragGhosts {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let layer = cx.new(|_| Layer {
            ghosts: Vec::new(),
            next: 0,
            slots: FlipBounds::default(),
            seen: Rc::default(),
        });
        let layers = &mut cx.default_global::<Layers>().0;
        layers.retain(|_, layer| layer.upgrade().is_some());
        layers.insert(window.window_handle().window_id(), layer.downgrade());
        Self {
            _layer_changed: cx.observe(&layer, |_, _, cx| cx.notify()),
            layer,
        }
    }
}

impl gpui_base::RootPlugin for DragGhosts {}

impl Render for DragGhosts {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let layer = self.layer.read(cx);
        let seen = std::mem::take(&mut *layer.seen.borrow_mut());
        layer.slots.retain(|key| seen.contains(key));
        // Updated only while it holds a ghost: the rows read it for their
        // slots, and an update while the window draws draws them again.
        if layer.ghosts.is_empty() {
            return div()
                .absolute()
                .top_0()
                .left_0()
                .size_0()
                .into_any_element();
        }
        self.layer.update(cx, |layer, cx| layer.render(window, cx))
    }
}

impl Layer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if !cx.has_active_drag() {
            for ghost in &mut self.ghosts {
                ghost.release();
            }
        }
        let pointer = window.mouse_position();
        let mut drawn = Vec::with_capacity(self.ghosts.len());
        let slots = self.slots.clone();
        self.ghosts.retain_mut(|ghost| {
            let id = ElementId::from(("drag-ghost", ghost.id as usize));
            let lift_id = child_id(&id, "lift");
            if std::mem::take(&mut ghost.fresh) {
                transition(lift_id.clone(), 0f32, Transition::new(LIFTING), window, cx);
            }
            let carried = matches!(ghost.flight, Flight::Carried);
            let lift = transition_with_status(
                lift_id,
                if carried { 1f32 } else { 0. },
                Transition::new(LIFTING),
                window,
                cx,
            );
            let slot = slots.get(&ghost.key);
            if let Some(slot) = slot {
                ghost.width = Some(slot.size.width);
            }
            let (at, shown, rest) = match &mut ghost.flight {
                Flight::Carried => {
                    // At rest from the start, so the jump on release is
                    // travelled rather than adopted.
                    flip_offset(
                        super::Clock::Element,
                        child_id(&id, "flight"),
                        Point::default(),
                        FLIP,
                        window,
                        cx,
                    );
                    (pointer - ghost.grab, 1., false)
                }
                Flight::Landing { into, toward } => {
                    let (heading, target) = match (*into, slot) {
                        (Some(rect), _) => (Toward::Rect, rect.center() - ghost.grab),
                        (None, Some(slot)) => (Toward::Slot, slot.origin),
                        (None, None) => (Toward::Here, toward.map_or(ghost.at, |(_, last)| last)),
                    };
                    // Wherever its target moves to, it travels there from
                    // where it is.
                    let jump = toward.map_or(ghost.at, |(_, last)| last) - target;
                    *toward = Some((heading, target));
                    let offset = flip_offset(
                        super::Clock::Element,
                        child_id(&id, "flight"),
                        jump,
                        FLIP,
                        window,
                        cx,
                    );
                    // Over its slot it hands over to the row in the second
                    // half of its descent, by when it is nearly there;
                    // elsewhere it fades as it goes.
                    let shown = match heading {
                        Toward::Slot => (lift.value * 2.).min(1.),
                        Toward::Rect | Toward::Here => lift.value,
                    };
                    let settled = lift.value <= 0.
                        && matches!(lift.status, MotionStatus::Finished | MotionStatus::Idle)
                        && offset == Point::default();
                    (target + offset, shown, settled)
                }
            };
            if rest {
                return false;
            }
            ghost.at = at;
            ghost.shown = shown;
            let key = ghost.key.clone();
            let content = (ghost.content)(lift.value, window, cx);
            drawn.push(
                deferred(
                    div()
                        .id(id)
                        .debug_selector(move || format!("drag-ghost-{key}"))
                        .absolute()
                        .left(at.x)
                        .top(at.y - LIFT / 2. * lift.value)
                        .when_some(ghost.width, |ghost, width| ghost.w(width))
                        .opacity(shown)
                        .child(content),
                )
                .with_priority(1),
            );
            true
        });
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_0()
            .children(drawn)
            .into_any_element()
    }
}

/// A row's place, measured into the ghost layer's slots each frame it is
/// laid out; the row itself is laid out as it would be without it.
struct Slot {
    key: SharedString,
    slots: FlipBounds,
    seen: Rc<RefCell<HashSet<SharedString>>>,
    child: AnyElement,
}

impl IntoElement for Slot {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl gpui::Element for Slot {
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
        self.slots.measure(&self.key, Point::default(), bounds);
        self.seen.borrow_mut().insert(self.key.clone());
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
