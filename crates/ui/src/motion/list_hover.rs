//! Hover reveals that fade, in a list too long for state per item.
//!
//! GPUI's `hover` and `group_hover` are styles that switch at once, with no
//! transition; gpui-base's `transition` animates a value an element samples,
//! but an item that keeps its own hovered flag and transition keeps state for
//! every item of a list that may hold thousands. [`ListHover`] is the list's
//! one hovered item, with the fades of the items the pointer just entered or
//! left: at rest it holds one key, whatever the list's length.

use std::{cell::RefCell, collections::HashMap, rc::Rc};

#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
#[cfg(target_family = "wasm")]
use web_time::Instant;

use gpui::{App, SharedString, Window};

/// The item of a list the pointer is over, owned by the list, and how far
/// each item's hover reveal (a message's actions, a formula's tools) is in.
///
/// An item hands its `on_hover` to [`ListHover::on_hover`] and draws what it
/// reveals at [`ListHover::reveal`]. Only the items whose reveal is moving
/// hold a fade; one at rest is 1 when hovered and 0 otherwise.
#[derive(Clone)]
pub(crate) struct ListHover(Rc<RefCell<Hover>>);

#[derive(Default)]
struct Hover {
    /// The item the pointer is over, and over how many of its parts: an item
    /// whose part blocks the pointer from the item (its actions over it)
    /// hands those parts the same handler, and the item stays hovered while
    /// the pointer is over any of them, whichever order they hear it in.
    hovered: Option<(SharedString, usize)>,
    /// Reveals on their way in or out: from where, to where, since when.
    fading: HashMap<SharedString, (f32, f32, Instant)>,
}

impl ListHover {
    /// The hover of one list, nothing hovered.
    pub(crate) fn new() -> Self {
        Self(Rc::default())
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
        let reduce = cx.reduce_motion();
        {
            let mut hover = self.0.borrow_mut();
            for (item, to) in [(previous, 0f32), (next, 1.)] {
                let Some(item) = item else { continue };
                let from = hover.sample_toward(&item, 1. - to, now);
                hover.fading.remove(&item);
                if !reduce && from != to {
                    hover.fading.insert(item, (from, to, now));
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
        let now = cx.background_executor().now();
        let mut hover = self.0.borrow_mut();
        let value = hover.sample(item, now);
        if hover.fading.contains_key(item) {
            window.request_animation_frame();
        }
        value
    }
}

impl Hover {
    /// `item`'s reveal `now`; a fade that has played is let go.
    fn sample(&mut self, item: &str, now: Instant) -> f32 {
        let rest = if self.hovered.as_ref().is_some_and(|(held, _)| held == item) {
            1.
        } else {
            0.
        };
        self.sample_toward(item, rest, now)
    }

    /// `item`'s reveal `now`, or `rest` when it is not fading.
    fn sample_toward(&mut self, item: &str, rest: f32, now: Instant) -> f32 {
        if let Some(&(from, to, at)) = self.fading.get(item) {
            // A reveal that turns back part way runs the part it has left.
            let span = crate::edge_back::SETTLE.mul_f32((to - from).abs().max(0.01));
            let elapsed = now.saturating_duration_since(at);
            if elapsed < span {
                let progress = super::settle_progress(
                    crate::edge_back::SETTLE.mul_f32(elapsed.as_secs_f32() / span.as_secs_f32()),
                );
                return from + (to - from) * progress;
            }
            self.fading.remove(item);
            return to;
        }
        rest
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
                hover: ListHover::new(),
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
}
