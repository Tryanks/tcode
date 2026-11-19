//! A scroll area that brings a focused input into view.
//!
//! gpui-base's input asks the scroll containers around it to reveal it when
//! it gains focus and whenever the fully visible part of the window changes
//! (`Window::request_autoscroll`), which is how the field being typed in
//! stays above a software keyboard: the page is laid out above the keyboard
//! ([`crate::window_seam`]) and scrolls the field into what is left.
//!
//! Gap: in gpui-fast only `List` answers that request; a `Div` that scrolls
//! does not, so a field in one stayed wherever the keyboard covered it. The
//! areas here answer it for the handle they scroll. Remove this once a
//! scrolling `Div` does.

use gpui::{AnyElement, App, Bounds, Element, ElementId, Pixels, Window, point};
use gpui_base::ScrollbarHandle;

/// `element`, the viewport `handle` scrolls, answering the reveal requests
/// made inside it.
pub(super) fn reveals<H: ScrollbarHandle + Clone>(
    id: impl Into<ElementId>,
    handle: &H,
    element: impl gpui::IntoElement,
) -> Reveals<H> {
    Reveals {
        id: id.into(),
        handle: handle.clone(),
        child: element.into_any_element(),
    }
}

pub(super) struct Reveals<H> {
    id: ElementId,
    handle: H,
    child: AnyElement,
}

/// What was asked to be in view, as a span of the content, and the viewport
/// it was last brought into.
///
/// The request comes on the frame the keyboard is known, while the page is
/// still as tall as it was and has nowhere to scroll to; its inset from the
/// keyboard glides in over the frames after. The span is kept and brought
/// into view again until the viewport has stopped changing.
#[derive(Clone, Copy)]
struct Pending {
    top: Pixels,
    bottom: Pixels,
    viewport: Bounds<Pixels>,
}

impl<H: ScrollbarHandle + 'static> Element for Reveals<H> {
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
        id: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        // A request made before this area, outside it, is an ancestor's.
        let outside = window.take_autoscroll();
        self.child.prepaint(window, cx);
        let request = window.take_autoscroll();
        let viewport = self.handle.viewport_bounds();
        let offset = self.handle.offset();
        let id = id.expect("the area has an id");
        let passed_on = window.with_element_state::<Option<Pending>, _>(id, |pending, window| {
            let asked = request.map(|request| Pending {
                top: request.top() - viewport.top() - offset.y,
                bottom: request.bottom() - viewport.top() - offset.y,
                viewport,
            });
            let Some(pending) = asked.or(pending.flatten()) else {
                return (None, None);
            };
            // What of the viewport the window shows: none of it under a
            // keyboard its layout has yet to move above.
            let shown = viewport.intersect(&window.fully_visible_bounds());
            let mut y = offset.y;
            if pending.bottom + y > shown.bottom() - viewport.top() {
                y = shown.bottom() - viewport.top() - pending.bottom;
            }
            if pending.top + y < shown.top() - viewport.top() {
                y = shown.top() - viewport.top() - pending.top;
            }
            if y != offset.y {
                self.handle.set_offset(point(offset.x, y));
            }
            let settled = asked.is_none() && pending.viewport == viewport;
            if !settled {
                let view = window.current_view();
                window.on_next_frame(move |_, cx| cx.notify(view));
            }
            // Where the span is once this area has moved, for the scrollers
            // around it.
            let span = Bounds::from_corners(
                point(viewport.left(), viewport.top() + pending.top + y),
                point(viewport.right(), viewport.top() + pending.bottom + y),
            );
            (
                Some(span),
                (!settled).then_some(Pending {
                    viewport,
                    ..pending
                }),
            )
        });
        if let Some(bounds) = passed_on.or(outside) {
            window.request_autoscroll(bounds);
        }
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
