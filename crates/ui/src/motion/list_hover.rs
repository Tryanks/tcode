//! Hover reveals that fade, in a list too long for state per item.
//!
//! GPUI's `hover` and `group_hover` are styles that switch at once, with no
//! transition; gpui-base's `transition` animates a value an element samples,
//! but an item that keeps its own hovered flag and transition keeps state for
//! every item of a list that may hold thousands. [`ListHover`] is the list's
//! one hovered item, with the fades of the items the pointer just entered or
//! left: at rest it holds one key, whatever the list's length.

use std::{cell::RefCell, collections::HashSet, rc::Rc};

use gpui::{App, SharedString, Window};

use super::{Curve, owned::Owned};

/// The item of a list the pointer is over, owned by the list, and how far
/// each item's hover reveal (a message's actions, a formula's tools) is in.
///
/// An item hands its `on_hover` to [`ListHover::on_hover`] and draws what it
/// reveals at [`ListHover::reveal`]. Only the items whose reveal is moving
/// hold a fade, on the glide every sampler uses; one at rest is 1 when
/// hovered and 0 otherwise.
#[derive(Clone)]
pub(crate) struct ListHover(Rc<RefCell<Hover>>);

struct Hover {
    /// The item the pointer is over, and over how many of its parts: an item
    /// whose part blocks the pointer from the item (its actions over it)
    /// hands those parts the same handler, and the item stays hovered while
    /// the pointer is over any of them, whichever order they hear it in.
    hovered: Option<(SharedString, usize)>,
    /// The reveals on their way in or out, by item, their glides in `fades`.
    fading: HashSet<SharedString>,
    fades: Owned,
    curve: Curve,
}

impl ListHover {
    /// The hover of one list, nothing hovered, its reveals moving along
    /// `curve`.
    pub(crate) fn new(curve: impl Into<Curve>) -> Self {
        Self(Rc::new(RefCell::new(Hover {
            hovered: None,
            fading: HashSet::new(),
            fades: Owned::default(),
            curve: curve.into(),
        })))
    }

    /// Nothing hovered and no reveal moving: the list shows other items now
    /// (another thread's), whose keys may match the last ones'. The pointer
    /// gives the hover to an item again as it next comes onto one.
    pub(crate) fn clear(&self) {
        let mut hover = self.0.borrow_mut();
        hover.hovered = None;
        hover.fading.clear();
        hover.fades.clear();
    }

    /// The handler for the `on_hover` of `item`, or of one of its parts: the
    /// item takes the list's hover as the pointer comes in, and gives it up
    /// once it has left the item and all its parts. Only one item holds the
    /// hover; another taking it lets the first go.
    pub(crate) fn on_hover(
        &self,
        item: impl Into<SharedString>,
    ) -> impl Fn(&bool, &mut Window, &mut App) + 'static {
        let (hover, item) = (self.clone(), item.into());
        move |entered, window, cx| {
            let (previous, next) = {
                let mut hover = hover.0.borrow_mut();
                let previous = hover.hovered.as_ref().map(|(item, _)| item.clone());
                hover.hovered = match hover.hovered.take() {
                    Some((held, parts)) if held == item => {
                        let parts = if *entered {
                            parts + 1
                        } else {
                            parts.saturating_sub(1)
                        };
                        (parts > 0).then_some((held, parts))
                    }
                    _ if *entered => Some((item.clone(), 1)),
                    other => other,
                };
                (
                    previous,
                    hover.hovered.as_ref().map(|(item, _)| item.clone()),
                )
            };
            if previous != next {
                hover.moved(previous, next, window, cx);
            }
        }
    }

    fn moved(
        &self,
        previous: Option<SharedString>,
        next: Option<SharedString>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let now = cx.background_executor().now();
        {
            let mut hover = self.0.borrow_mut();
            for (item, from) in [(previous, 1f32), (next, 0.)] {
                let Some(item) = item else { continue };
                // A reveal at rest starts its glide from where it rests; one
                // still moving turns from where it is.
                if hover.fading.insert(item.clone()) {
                    hover.fades.seed(item, from, now);
                }
            }
        }
        // The pointer moved over the list, or the list under a still
        // pointer: nothing the list holds changed, so it is redrawn rather
        // than its view notified.
        window.refresh();
    }

    /// How far `item`'s hover reveal is in, from 0 to 1, keeping frames
    /// coming while it moves.
    pub(crate) fn reveal(&self, item: &str, window: &mut Window, cx: &App) -> f32 {
        let mut hover = self.0.borrow_mut();
        let target = if hover.hovered.as_ref().is_some_and(|(held, _)| held == item) {
            1f32
        } else {
            0.
        };
        if !hover.fading.contains(item) {
            return target;
        }
        let key = SharedString::from(item.to_owned());
        let (value, settled) = hover
            .fades
            .glide(key.clone(), target, &hover.curve, window, cx);
        if settled {
            hover.fading.remove(item);
            hover.fades.forget_exact(key);
        }
        value
    }

    /// The hover fill of `item`, for the first child of its frame, under the
    /// item's content: `fill` [blended](super::blend) in at the item's
    /// reveal, rounded by `radius`. It hears the pointer through its own
    /// hitbox over the item, so the item's own `on_hover` stays the
    /// caller's. [`super::hover_layer`] for an item of a list: the fill
    /// holds no state of its own. The item must be positioned.
    pub(crate) fn layer(
        &self,
        item: impl Into<SharedString>,
        fill: gpui::Hsla,
        radius: gpui::Pixels,
    ) -> super::Sampled {
        use gpui::{
            InteractiveElement as _, IntoElement as _, StatefulInteractiveElement as _,
            Styled as _, prelude::FluentBuilder as _,
        };
        let (hover, item) = (self.clone(), item.into());
        super::Sampled::new(move |window, cx| {
            let reveal = hover.reveal(&item, window, cx);
            gpui::div()
                .id("list-hover-layer")
                .absolute()
                .inset_0()
                .rounded(radius)
                .when(reveal > 0., |layer| {
                    layer.bg(super::blend(fill.opacity(0.), fill, reveal))
                })
                .on_hover(hover.on_hover(item))
                .into_any_element()
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::HashMap, rc::Rc, time::Duration};

    use gpui::{
        Context, InteractiveElement as _, IntoElement, Modifiers, ParentElement as _, Render,
        SharedString, StatefulInteractiveElement as _, Styled as _, TestAppContext,
        VisualTestContext, Window, div, point, px,
    };

    use super::ListHover;
    use crate::test_frames::{draw, draw_settled};

    /// Rows whose actions, a part over the row that takes the pointer from
    /// it, show at the row's reveal; what each frame drew, by row.
    struct Rows {
        hover: ListHover,
        drawn: Rc<RefCell<HashMap<usize, Vec<f32>>>>,
    }

    impl Render for Rows {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div().flex().flex_col().children((0..3).map(|row| {
                let key = SharedString::from(format!("row-{row}"));
                let reveal = self.hover.reveal(&key, window, cx);
                self.drawn.borrow_mut().entry(row).or_default().push(reveal);
                div()
                    .id(key.clone())
                    .relative()
                    .w(px(200.))
                    .h(px(40.))
                    .on_hover(self.hover.on_hover(key.clone()))
                    .child(
                        div()
                            .id(("actions", row))
                            .absolute()
                            .top_0()
                            .right_0()
                            .size(px(20.))
                            .block_mouse_except_scroll()
                            .opacity(reveal)
                            .on_hover(self.hover.on_hover(key)),
                    )
            }))
        }
    }

    /// The reveal each row drew, from the frame `since` on.
    fn reveals(rows: &Rc<RefCell<HashMap<usize, Vec<f32>>>>, row: usize, since: usize) -> Vec<f32> {
        rows.borrow()[&row][since..].to_vec()
    }

    fn frames(cx: &mut VisualTestContext) {
        for _ in 0..16 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.update(|window, cx| window.simulate_next_frame(cx));
            draw(cx);
        }
        draw_settled(cx);
    }

    /// The row under the pointer fades its actions in, and the row it left
    /// fades them out, each over several frames; moving onto the actions
    /// keeps them; leaving the list fades them out and the window comes to
    /// rest, holding nothing for the rows it passed over.
    #[gpui::test]
    fn the_hovered_row_fades_its_reveal_in_and_out(cx: &mut TestAppContext) {
        let drawn = Rc::new(RefCell::new(HashMap::new()));
        let (_, cx) = cx.add_window_view({
            let drawn = drawn.clone();
            |_, _| Rows {
                hover: ListHover::new(crate::material::HOVER),
                drawn,
            }
        });
        // The pointer starts away from the list.
        cx.simulate_mouse_move(point(px(300.), px(300.)), None, Modifiers::none());
        draw_settled(cx);
        let mark = |cx: &mut VisualTestContext| {
            let _ = cx;
            drawn.borrow()[&0].len()
        };
        let fades = |values: &[f32], up: bool| {
            values.windows(2).all(|pair| {
                if up {
                    pair[1] >= pair[0]
                } else {
                    pair[1] <= pair[0]
                }
            }) && values
                .iter()
                .filter(|value| **value > 0. && **value < 1.)
                .count()
                >= 6
        };

        let since = mark(cx);
        cx.simulate_mouse_move(point(px(50.), px(10.)), None, Modifiers::none());
        frames(cx);
        let row = reveals(&drawn, 0, since);
        assert!(fades(&row, true), "row 0 popped in: {row:?}");
        assert_eq!(*row.last().unwrap(), 1.);
        assert!(reveals(&drawn, 1, since).iter().all(|value| *value == 0.));

        let since = mark(cx);
        cx.simulate_mouse_move(point(px(190.), px(10.)), None, Modifiers::none());
        frames(cx);
        assert!(
            reveals(&drawn, 0, since).iter().all(|value| *value == 1.),
            "the row's actions faded as the pointer moved onto them"
        );

        let since = mark(cx);
        cx.simulate_mouse_move(point(px(50.), px(50.)), None, Modifiers::none());
        frames(cx);
        let (left, entered) = (reveals(&drawn, 0, since), reveals(&drawn, 1, since));
        assert!(fades(&left, false), "row 0 popped out: {left:?}");
        assert!(fades(&entered, true), "row 1 popped in: {entered:?}");
        assert_eq!((*left.last().unwrap(), *entered.last().unwrap()), (0., 1.));

        let since = mark(cx);
        cx.simulate_mouse_move(point(px(300.), px(300.)), None, Modifiers::none());
        frames(cx);
        let left = reveals(&drawn, 1, since);
        assert!(fades(&left, false), "row 1 popped out: {left:?}");
        assert_eq!(*left.last().unwrap(), 0.);
        assert_eq!(
            cx.update(|window, cx| window.simulate_next_frame(cx)),
            0,
            "a reveal kept asking for frames"
        );
    }

    /// The rows of a virtual list, which reveal at the list's hover.
    struct ListRows {
        hover: ListHover,
        list: gpui::ListState,
        drawn: Rc<RefCell<HashMap<usize, Vec<f32>>>>,
    }

    impl Render for ListRows {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(
                gpui::list(
                    self.list.clone(),
                    cx.processor(|this, row: usize, window, cx| {
                        let key = SharedString::from(format!("row-{row}"));
                        let reveal = this.hover.reveal(&key, window, cx);
                        this.drawn.borrow_mut().entry(row).or_default().push(reveal);
                        div()
                            .id(key.clone())
                            .w(px(200.))
                            .h(px(40.))
                            .on_hover(this.hover.on_hover(key))
                            .child(div().size(px(20.)).opacity(reveal))
                            .into_any_element()
                    }),
                )
                .size_full(),
            )
        }
    }

    /// In a virtual list drawn as the app draws its frames, with GPUI's
    /// scroll layers holding the rows it painted, the reveal of the row the
    /// pointer enters is drawn again each frame of its fade rather than once.
    #[gpui::test]
    fn a_virtual_lists_retained_rows_redraw_their_reveal(cx: &mut TestAppContext) {
        let drawn = Rc::new(RefCell::new(HashMap::new()));
        let (_, cx) = cx.add_window_view({
            let drawn = drawn.clone();
            |_, _| ListRows {
                hover: ListHover::new(crate::material::HOVER),
                list: gpui::ListState::new(30, gpui::ListAlignment::Top, px(200.)),
                drawn,
            }
        });
        cx.simulate_mouse_move(point(px(300.), px(500.)), None, Modifiers::none());
        draw_settled(cx);
        // Frames as the display delivers them: drawn without being refreshed.
        let frame = |cx: &mut VisualTestContext| {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.update(|window, cx| {
                window.simulate_next_frame(cx);
                let _ = window.draw(cx);
            });
        };
        for _ in 0..4 {
            frame(cx);
        }
        let since = drawn.borrow()[&2].len();
        cx.simulate_mouse_move(point(px(50.), px(100.)), None, Modifiers::none());
        for _ in 0..20 {
            frame(cx);
        }
        let row = reveals(&drawn, 2, since);
        assert!(
            row.iter()
                .filter(|value| **value > 0. && **value < 1.)
                .count()
                >= 6,
            "the hovered row's reveal was drawn {row:?}"
        );
        assert_eq!(row.last(), Some(&1.));
    }
}
