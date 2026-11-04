//! Hover: whether the pointer is over an element, and the fill that glides
//! as it comes and goes.
//!
//! GPUI's `hover` and `group_hover` are styles that switch at once, with no
//! transition, so a hover that glides is read at render instead
//! ([`hovered`]) and its fill sampled on a [`Clock`]. Every hover fill —
//! an item's ([`hover_fill`], [`hover_layer`]), a list's
//! ([`super::ListHover`]), a button's — paints the one [`blend`] of its rest
//! and hovered fills.

use gpui::{
    App, ElementId, Entity, Hsla, InteractiveElement as _, IntoElement as _, Pixels, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div,
};
use gpui_base::motion::Interpolate as _;

use super::{Clock, Colour, Curve, Sampled, child_id, owned::Owned};

/// The id a [`hover_layer`] and the hover it keeps go by, under its item's.
const LAYER: &str = "hover-layer";

/// The fill of an element `reveal` of the way from `rest`, its fill while
/// the pointer is away (a highlight, or nothing), to `hover`: mixed over
/// premultiplied alpha, as every glide of a colour is ([`Colour`]).
pub(crate) fn blend(rest: Hsla, hover: Hsla, reveal: f32) -> Hsla {
    Colour(rest).interpolate(&Colour(hover), reveal).0
}

/// `listener`, for an element's `on_hover`, hearing only a pointer come in.
/// gpui-fast's `on_hover` hit-tests the last touch as a pointer at rest, so
/// the element a finger tapped or panned over is reported entered and stays
/// so until another touch lands elsewhere; its own `hover` styles skip that
/// report through `Window::last_input_was_touch`, and so does this. Leaving
/// is always passed on.
pub(crate) fn pointer_hover(
    listener: impl Fn(&bool, &mut Window, &mut App) + 'static,
) -> impl Fn(&bool, &mut Window, &mut App) + 'static {
    move |entered, window, cx| {
        if !(*entered && window.last_input_was_touch()) {
            listener(entered, window, cx);
        }
    }
}

/// An element's hover, kept under its id.
#[derive(Clone, Copy, Default)]
struct Hover {
    /// Whether the pointer is over it, as its `on_hover` last heard.
    pointer: bool,
    /// Whether it keeps its hover fill with the pointer gone
    /// ([`hold_hover_layer`]).
    held: bool,
}

fn hover_state(id: &ElementId, window: &mut Window, cx: &mut App) -> Entity<Hover> {
    window.use_keyed_state(child_id(id, "hovered"), cx, |_, _| Hover::default())
}

/// The element `id`'s hover, and the handler for its `on_hover`.
fn tracked_hover(
    id: &ElementId,
    window: &mut Window,
    cx: &mut App,
) -> (
    Hover,
    impl Fn(&bool, &mut Window, &mut App) + 'static + use<>,
) {
    let state = hover_state(id, window, cx);
    let now = *state.read(cx);
    let on_hover = pointer_hover(move |is_hovered: &bool, _: &mut Window, cx: &mut App| {
        state.update(cx, |hover, cx| {
            if hover.pointer != *is_hovered {
                hover.pointer = *is_hovered;
                cx.notify();
            }
        });
    });
    (now, on_hover)
}

/// Whether the pointer is over the element `id`, as its `on_hover` last
/// heard, and that handler: for an element whose hover moves more than a
/// fill (a label's colour too), each part gliding to its hovered value.
pub(crate) fn hovered<I: Into<ElementId>>(
    id: I,
    window: &mut Window,
    cx: &mut App,
) -> (
    bool,
    impl Fn(&bool, &mut Window, &mut App) + 'static + use<I>,
) {
    let (hover, on_hover) = tracked_hover(&id.into(), window, cx);
    (hover.pointer, on_hover)
}

/// Keeps the [`hover_layer`] of the element `owner` hovered while `held`,
/// wherever the pointer goes: the layer's one fill is what stays, so a pointer
/// over the element adds nothing to it, and the fill never leaves to come
/// back as the pointer moves off. Called from above the element, before it
/// renders. An element with no hover layer of its own (a message, a
/// terminal) has no hover to keep, and shows nothing.
pub(crate) fn hold_hover_layer(owner: ElementId, held: bool, window: &mut Window, cx: &mut App) {
    // gpui-fast keeps what a `RenderOnce` renders under its type's name.
    let layer = ElementId::Name(std::any::type_name::<Sampled>().into());
    let state = window.with_id(owner, |window| {
        window.with_id(layer, |window| hover_state(&LAYER.into(), window, cx))
    });
    state.update(cx, |hover, _| hover.held = held);
}

/// A hover fill gliding along `curve` on `clock`: the fill to paint now,
/// from `rest` to `hover` (a change of `rest` glides too), and the handler
/// for the element's `on_hover` that moves it.
pub(crate) fn hover_fill<I: Into<ElementId>>(
    clock: Clock<'_>,
    id: I,
    rest: Hsla,
    hover: Hsla,
    curve: &Curve,
    window: &mut Window,
    cx: &mut App,
) -> (
    Hsla,
    impl Fn(&bool, &mut Window, &mut App) + 'static + use<I>,
) {
    let id = id.into();
    let (hovered, on_hover) = hovered(id.clone(), window, cx);
    let target = Colour(if hovered { hover } else { rest });
    let fill = clock.transition(child_id(&id, "hover-fill"), target, curve, window, cx);
    (fill.0, on_hover)
}

/// The fill of an item built with `&App` only (a menu item, a selectable
/// row), for its frame's first child, under the item's content:
/// [`hover_fill`] from `rest`, rounded by `radius`, sampled when the item
/// renders. Its clock is kept under the item's id, or by `owner` under its
/// key for an item its view draws under another parent (a list on a page
/// the window re-parents as it turns compact or wide), whose fill would
/// otherwise start over there. It hears the pointer through its own hitbox,
/// which covers the item and leaves the pointer to the item and its
/// content, so the item's own `on_hover` stays the caller's. The item must
/// be positioned and have an id.
pub(crate) fn hover_layer(
    owner: Option<(&Owned, SharedString)>,
    rest: Hsla,
    hover: Hsla,
    radius: Pixels,
    curve: Curve,
) -> Sampled {
    let owner = owner.map(|(motion, key)| (motion.clone(), key));
    Sampled::new(move |window, cx| {
        let (state, on_hover) = tracked_hover(&LAYER.into(), window, cx);
        let (clock, id) = match &owner {
            Some((motion, key)) => (Clock::Owned(motion), ElementId::from(key.clone())),
            None => (Clock::Element, ElementId::from(LAYER)),
        };
        let target = Colour(if state.pointer || state.held {
            hover
        } else {
            rest
        });
        let fill = clock.transition(child_id(&id, "hover-fill"), target, &curve, window, cx);
        div()
            .id(LAYER)
            .absolute()
            .inset_0()
            .rounded(radius)
            .bg(fill.0)
            .on_hover(on_hover)
            .into_any_element()
    })
}
