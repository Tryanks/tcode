//! How faint what is painted now is drawn, as far as Tcode's own fades go.
//!
//! Gap: GPUI multiplies the opacity of every element an element is painted
//! under into what it paints, and keeps that opacity to itself
//! (`Window::element_opacity` is crate-private). A part that must paint
//! differently while it is faint, a raised surface's shadow
//! ([`crate::material::Raised`]), reads it here instead, from each fade that
//! declares its content faint through [`faded`] (a raised surface declares
//! its own opacity for what it holds).

use std::cell::Cell;

use gpui::{AnyElement, App, Bounds, ElementId, IntoElement, LayoutId, Pixels, Window};

thread_local! {
    static OPACITY: Cell<f32> = const { Cell::new(1.) };
}

/// How opaque what is painted now is drawn, by every fade it is painted under
/// that declared itself.
pub(crate) fn opacity() -> f32 {
    OPACITY.with(Cell::get)
}

/// Runs `paint`, which paints what is drawn `opacity` opaque, so declared.
pub(crate) fn within<R>(opacity: f32, paint: impl FnOnce() -> R) -> R {
    let outer = self::opacity();
    OPACITY.with(|cell| cell.set(outer * opacity.clamp(0., 1.)));
    let painted = paint();
    OPACITY.with(|cell| cell.set(outer));
    painted
}

/// `content`, which the element it is a child of draws `opacity` opaque,
/// declared so to what it paints.
pub(crate) fn faded(opacity: f32, content: impl IntoElement) -> Faded {
    Faded {
        content: content.into_any_element(),
        opacity: opacity.clamp(0., 1.),
    }
}

/// See [`faded`].
pub(crate) struct Faded {
    content: AnyElement,
    opacity: f32,
}

impl IntoElement for Faded {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl gpui::Element for Faded {
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
    ) -> (LayoutId, ()) {
        (self.content.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.content.prepaint(window, cx);
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
        within(self.opacity, || self.content.paint(window, cx));
    }
}
