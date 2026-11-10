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

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use gpui::{
        Context, InteractiveElement as _, IntoElement, ListAlignment, ListState,
        ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, TestAppContext,
        VisualTestContext, Window, div, list, px,
    };

    use super::ListFlip;
    use crate::test_frames::{draw, draw_settled};

    struct Rows {
        order: Vec<&'static str>,
        drawn: Vec<&'static str>,
        list: ListState,
        flip: ListFlip,
        presses: Rc<RefCell<Vec<&'static str>>>,
    }

    impl Render for Rows {
        fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            if self.order != self.drawn {
                let drawn = self.drawn.clone();
                self.flip.replace(
                    &self.list,
                    |index| drawn.get(index).copied(),
                    &self.order,
                    window,
                );
                self.list
                    .splice(0..self.list.item_count(), self.order.len());
                self.drawn = self.order.clone();
            }
            let (order, flip, presses) =
                (self.order.clone(), self.flip.clone(), self.presses.clone());
            div().size(px(200.)).child(
                list(self.list.clone(), move |index, _, _| {
                    let (name, presses) = (order[index], presses.clone());
                    flip.row(
                        index,
                        name,
                        div()
                            .id(name)
                            .debug_selector(move || name.to_string())
                            .w_full()
                            .h(px(20.))
                            .on_click(move |_, _, _| presses.borrow_mut().push(name)),
                    )
                })
                .size_full(),
            )
        }
    }

    /// Rows of a virtual list that trade places are drawn, and pressed, where
    /// they were, then glide to where they now are; with motion reduced they
    /// are there at once.
    #[gpui::test]
    fn list_rows_that_trade_places_glide_there(cx: &mut TestAppContext) {
        let presses = Rc::new(RefCell::new(Vec::new()));
        let (rows, cx) = cx.add_window_view(|_, _| Rows {
            order: vec!["a", "b", "c"],
            drawn: vec!["a", "b", "c"],
            list: ListState::new(3, ListAlignment::Top, px(0.)),
            flip: ListFlip::default(),
            presses: presses.clone(),
        });
        cx.update(|_, cx| cx.set_reduce_motion(false));
        let top = |name: &'static str, cx: &mut VisualTestContext| {
            cx.debug_bounds(name).expect("the row is drawn").top()
        };
        draw_settled(cx);
        let (first, third) = (top("a", cx), top("c", cx));
        let reorder = |order: Vec<&'static str>, cx: &mut VisualTestContext| {
            rows.update(cx, |rows, cx| {
                rows.order = order;
                cx.notify();
            });
            draw(cx);
        };

        reorder(vec!["c", "b", "a"], cx);
        assert_eq!(top("a", cx), first, "a starts from where it was");
        let a = cx.debug_bounds("a").unwrap();
        cx.simulate_click(a.center(), Default::default());
        assert_eq!(
            *presses.borrow(),
            ["a"],
            "a takes presses where it is drawn"
        );

        cx.executor().advance_clock(super::FLIP / 2);
        draw(cx);
        let gliding = top("a", cx);
        assert!(gliding > first && gliding < third, "a is on its way");

        draw_settled(cx);
        assert_eq!((top("c", cx), top("a", cx)), (first, third));

        cx.update(|_, cx| cx.set_reduce_motion(true));
        reorder(vec!["a", "b", "c"], cx);
        assert_eq!((top("a", cx), top("c", cx)), (first, third));
    }
}
