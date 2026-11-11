//! A control in a row that clips what overflows it, drawn whole or not at
//! all.
//!
//! gpui-base has no row that lets go of the controls it has no room for. A
//! row with hidden overflow cuts the one at its edge through the middle,
//! which reads as broken and can still be pressed by the sliver left of it.
//! This keeps the row's layout as it is and only stops drawing a control the
//! row's clip would cut across.

use gpui::{
    AnyElement, App, Bounds, ContentMask, Element, ElementId, GlobalElementId, InspectorElementId,
    IntoElement, LayoutId, Pixels, Window,
};

/// `content`, laid out as itself; drawn, and pressable, only while the clip
/// it is drawn under holds its whole width.
pub(crate) fn whole(content: impl IntoElement) -> Whole {
    Whole {
        content: content.into_any_element(),
        cut: false,
    }
}

pub(crate) struct Whole {
    content: AnyElement,
    cut: bool,
}

impl IntoElement for Whole {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Whole {
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
        (self.content.request_layout(window, cx), ())
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
        let clip = window.content_mask().bounds;
        // Layout places on fractions of a pixel that the clip rounds.
        let slack = gpui::px(0.5);
        self.cut = bounds.left() < clip.left() - slack || bounds.right() > clip.right() + slack;
        // Prepainted under an empty clip rather than skipped, so it keeps its
        // state, and takes no presses while it is not drawn.
        window.with_content_mask(self.cut.then(nothing), |window| {
            self.content.prepaint(window, cx)
        });
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
        window.with_content_mask(self.cut.then(nothing), |window| {
            self.content.paint(window, cx)
        });
    }
}

fn nothing() -> ContentMask<Pixels> {
    ContentMask {
        bounds: Bounds::default(),
    }
}
