//! Rows of a gpui `list` that glide to their new places when the list's rows
//! are replaced: a reorder, a row moved to another section.
//!
//! A `list` lays out only the rows in view and places each one itself, so a
//! row whose index changed is simply drawn at its new place; gpui-base has no
//! layout animation. [`keyed`](super::keyed) glides rows in ordinary flow by
//! measuring every row each frame, which a virtual list (whose rows outside
//! the view are not laid out, and whose layer may replay rows it did not lay
//! out again) cannot do. So [`ListFlip`] asks the list, just before its owner
//! replaces the rows, where it draws each row in view, and hands that place to
//! the row at its new index, which on the next layout is drawn from there and
//! glides on [`flip_offset`] to where it now is. A row that comes into view
//! with the change is drawn where it is.

use std::{cell::RefCell, collections::HashMap, rc::Rc};

use gpui::{AnyElement, App, Bounds, ElementId, IntoElement, ListState, Pixels, Point, Window, px};

use super::keyed::{flip_offset, prepaint_gliding};

/// How long a row drawn here takes to spring to a new place.
const FLIP: std::time::Duration = std::time::Duration::from_millis(200);

/// Where the rows a list showed were drawn before its rows were replaced, by
/// their index in the rows that replaced them; kept by the list's owner.
#[derive(Clone, Default)]
pub(crate) struct ListFlip(Rc<RefCell<HashMap<usize, Point<Pixels>>>>);

impl ListFlip {
    /// Notes where `list` draws each row in view, whose key `old` gives by
    /// its index, before the owner replaces its rows with `new`: a row `new`
    /// still has glides from there to its new place.
    pub(crate) fn replace<K: PartialEq>(
        &self,
        list: &ListState,
        old: impl Fn(usize) -> Option<K>,
        new: &[K],
        window: &mut Window,
    ) {
        let mut from = self.0.borrow_mut();
        from.clear();
        let bottom = list.viewport_bounds().bottom();
        for index in list.logical_scroll_top().item_ix..list.item_count() {
            let Some(bounds) = list.bounds_for_item(index) else {
                break;
            };
            if bounds.top() >= bottom {
                break;
            }
            if let Some(at) = old(index).and_then(|key| new.iter().position(|new| *new == key)) {
                from.insert(at, bounds.origin);
            }
        }
        drop(from);
        // A row the next frame does not lay out (it left the view) does not
        // glide on a later one.
        let flip = self.clone();
        window.on_next_frame(move |_, _| flip.0.borrow_mut().clear());
    }

    /// `child` as the list's row at `index`: on the frame after
    /// [`Self::replace`] it is drawn, and takes the pointer, where the row
    /// was, and glides to where it is laid out. `id` names the row, not its
    /// index, so a row moved again mid-glide carries on from where it is.
    pub(crate) fn row(
        &self,
        index: usize,
        id: impl Into<ElementId>,
        child: impl IntoElement,
    ) -> AnyElement {
        FlipRow {
            id: id.into(),
            index,
            from: self.clone(),
            child: child.into_any_element(),
        }
        .into_any_element()
    }
}

struct FlipRow {
    id: ElementId,
    index: usize,
    from: ListFlip,
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
        // Taken, so a list that lays its rows out twice in a frame does not
        // count the move twice. Under a pixel is the list snapping its rows
        // to the display, not a move.
        let jump = self
            .from
            .0
            .borrow_mut()
            .remove(&self.index)
            .map(|was| was - bounds.origin)
            .filter(|jump| jump.x.abs() >= px(1.) || jump.y.abs() >= px(1.))
            .unwrap_or_default();
        let offset = flip_offset(
            super::Clock::Element,
            self.id.clone(),
            jump,
            FLIP,
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
