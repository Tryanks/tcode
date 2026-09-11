//! Exercise Tcode viewport registration with unmodified GPUI.
use crate::touch_scroll::{Handle, register, root};

use gpui::{
    Context, FollowMode, InteractiveElement, IntoElement, ListAlignment, ListState, ParentElement,
    Pixels, Render, ScrollDelta, ScrollHandle, ScrollWheelEvent, StatefulInteractiveElement,
    Styled, TestAppContext, VisualTestContext, Window, WindowHandle, div, list, point, px,
};
use std::time::Duration;
use std::{cell::Cell, rc::Rc};

#[derive(Clone)]
enum Area {
    Div(ScrollHandle),
    List(ListState),
}

impl Area {
    fn offset(&self) -> Pixels {
        match self {
            Self::Div(handle) => handle.offset().y,
            Self::List(state) => state.scroll_px_offset_for_scrollbar().y,
        }
    }

    fn set_offset(&self, y: Pixels) {
        match self {
            Self::Div(handle) => handle.set_offset(point(px(0.), y)),
            Self::List(state) => {
                state.scrollbar_drag_started();
                state.set_offset_from_scrollbar(point(px(0.), y));
                state.scrollbar_drag_ended();
            }
        }
    }
}

struct ScrollView {
    area: Area,
    tail_height: Rc<Cell<Pixels>>,
}

impl Render for ScrollView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let element = match &self.area {
            Area::Div(handle) => div()
                .id("scroll")
                .w(px(100.))
                .h(px(100.))
                .overflow_y_scroll()
                .track_scroll(handle)
                .child(div().h(px(1000.)).w_full().flex_none())
                .into_any_element(),
            Area::List(state) => {
                let last = state.item_count() - 1;
                let tail_height = self.tail_height.clone();
                list(state.clone(), move |ix, _, _| {
                    let height = if ix == last {
                        tail_height.get()
                    } else {
                        px(100.)
                    };
                    div().h(height).w_full().into_any_element()
                })
                .w(px(100.))
                .h(px(100.))
                .into_any_element()
            }
        };
        let handle = match &self.area {
            Area::Div(handle) => Handle::Scroll(handle.clone()),
            Area::List(state) => Handle::List(state.clone()),
        };
        root(register(element, handle))
    }
}

fn frame(cx: &mut TestAppContext, window: WindowHandle<ScrollView>, millis: u64) {
    cx.executor().advance_clock(Duration::from_millis(millis));
    VisualTestContext::from_window(window.into(), cx).update(|window, cx| {
        window.simulate_next_frame(cx);
        window.draw(cx).clear(cx);
    });
    cx.executor().run_until_parked();
}

fn scroll(cx: &mut TestAppContext, window: WindowHandle<ScrollView>, delta: ScrollDelta) {
    VisualTestContext::from_window(window.into(), cx).simulate_event(ScrollWheelEvent {
        position: point(px(5.), px(5.)),
        delta,
        ..Default::default()
    });
    cx.executor().run_until_parked();
}

fn assert_near(actual: Pixels, expected: f32) {
    assert!(
        (f32::from(actual) - expected).abs() < 0.01,
        "{actual:?} != {expected}"
    );
}

#[gpui::test]
fn wheel_scroll_accumulates_and_yields_to_direct_input(cx: &mut TestAppContext) {
    for area in [
        Area::Div(ScrollHandle::new()),
        Area::List(ListState::new(10, ListAlignment::Top, px(0.)).measure_all()),
    ] {
        let window = cx.add_window(|_, _| ScrollView {
            area: area.clone(),
            tail_height: Rc::new(Cell::new(px(100.))),
        });
        // Div uses the window's line height, while List uses 20px per line.
        let distance = window
            .update(cx, |_, window, _| match &area {
                Area::Div(_) => f32::from(window.line_height()) * 6.,
                Area::List(_) => 120.,
            })
            .unwrap();
        scroll(cx, window, ScrollDelta::Lines(point(0., -3.)));
        assert_eq!(area.offset(), px(0.));
        frame(cx, window, 32);
        assert!(area.offset() < px(0.) && area.offset() > px(-distance / 2.));
        scroll(cx, window, ScrollDelta::Lines(point(0., -3.)));
        frame(cx, window, 150);
        assert_near(area.offset(), -distance);

        scroll(cx, window, ScrollDelta::Lines(point(0., -3.)));
        scroll(cx, window, ScrollDelta::Pixels(point(px(0.), px(7.))));
        assert_near(area.offset(), -distance + 7.);
        frame(cx, window, 150);
        assert_near(area.offset(), -distance + 7.);

        scroll(cx, window, ScrollDelta::Lines(point(0., -3.)));
        area.set_offset(px(-200.));
        window.update(cx, |_, _, cx| cx.notify()).unwrap();
        frame(cx, window, 150);
        assert_near(area.offset(), -200.);

        scroll(cx, window, ScrollDelta::Lines(point(0., -3.)));
        VisualTestContext::from_window(window.into(), cx).simulate_event(gpui::MouseDownEvent {
            position: point(px(95.), px(50.)),
            button: gpui::MouseButton::Left,
            ..Default::default()
        });
        frame(cx, window, 150);
        assert_near(area.offset(), -200.);
    }
}

#[gpui::test]
fn wheel_scroll_preserves_chat_anchor_during_streaming_and_prepend(cx: &mut TestAppContext) {
    let state = ListState::new(10, ListAlignment::Bottom, px(2000.)).measure_all();
    state.set_follow_mode(FollowMode::Tail);
    let tail_height = Rc::new(Cell::new(px(100.)));
    let area = Area::List(state.clone());
    let window = cx.add_window(|_, _| ScrollView {
        area: area.clone(),
        tail_height: tail_height.clone(),
    });
    assert_near(area.offset(), -900.);
    scroll(cx, window, ScrollDelta::Lines(point(0., 3.)));
    assert!(!state.is_following_tail());
    // Streaming before the first animation frame must not re-engage follow.
    tail_height.set(px(200.));
    state.remeasure_items(9..10);
    frame(cx, window, 32);
    assert!(area.offset() > px(-900.) && area.offset() < px(-840.));
    assert!(!state.is_following_tail());
    state.splice(0..0, 2);
    window.update(cx, |_, _, cx| cx.notify()).unwrap();
    frame(cx, window, 150);
    assert_near(area.offset(), -1040.);
    assert!(!state.is_following_tail());

    scroll(cx, window, ScrollDelta::Lines(point(0., -100.)));
    frame(cx, window, 150);
    assert_near(area.offset(), -1200.);
    assert!(state.is_following_tail());
}

#[gpui::test]
fn wheel_scroll_reverses_without_overscroll_and_respects_reduced_motion(cx: &mut TestAppContext) {
    for area in [
        Area::Div(ScrollHandle::new()),
        Area::List(ListState::new(10, ListAlignment::Top, px(0.)).measure_all()),
    ] {
        let window = cx.add_window(|_, _| ScrollView {
            area: area.clone(),
            tail_height: Rc::new(Cell::new(px(100.))),
        });
        let line_height = window
            .update(cx, |_, window, _| match area {
                Area::Div(_) => f32::from(window.line_height()),
                Area::List(_) => 20.,
            })
            .unwrap();
        // Reversing before the first frame must also cancel a queued notch.
        scroll(cx, window, ScrollDelta::Lines(point(0., -3.)));
        scroll(cx, window, ScrollDelta::Lines(point(0., 3.)));
        frame(cx, window, 150);
        assert_near(area.offset(), 0.);

        scroll(cx, window, ScrollDelta::Lines(point(0., -100.)));
        frame(cx, window, 32);
        let before = area.offset();
        assert!(before < px(0.) && before > px(-900.));
        scroll(cx, window, ScrollDelta::Lines(point(0., 1.)));
        frame(cx, window, 16);
        assert!(
            area.offset() > before,
            "reversing must discard the old destination"
        );
        frame(cx, window, 150);
        assert_near(area.offset(), f32::from(before) + line_height);

        scroll(cx, window, ScrollDelta::Lines(point(0., -100.)));
        frame(cx, window, 150);
        assert_near(area.offset(), -900.);
        // Extra input against the limit must not accumulate invisible overscroll.
        scroll(cx, window, ScrollDelta::Lines(point(0., -100.)));
        scroll(cx, window, ScrollDelta::Lines(point(0., 1.)));
        frame(cx, window, 150);
        assert_near(area.offset(), -900. + line_height);

        cx.update(|cx| cx.set_reduce_motion(true));
        scroll(cx, window, ScrollDelta::Lines(point(0., 1.)));
        assert_near(area.offset(), -900. + 2. * line_height);
        frame(cx, window, 150);
        assert_near(area.offset(), -900. + 2. * line_height);
        cx.update(|cx| cx.set_reduce_motion(false));
    }
}

#[gpui::test]
fn wheel_scroll_only_moves_the_innermost_scrollable_viewport(cx: &mut TestAppContext) {
    struct Nested {
        inner: ListState,
        outer: ScrollHandle,
    }
    impl Render for Nested {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            root(register(
                div()
                    .id("outer")
                    .w(px(100.))
                    .h(px(100.))
                    .overflow_y_scroll()
                    .track_scroll(&self.outer)
                    .child(register(
                        list(self.inner.clone(), |_, _, _| {
                            div().w_full().h(px(100.)).into_any_element()
                        })
                        .w_full()
                        .h(px(100.))
                        .flex_none(),
                        Handle::List(self.inner.clone()),
                    ))
                    .child(div().h(px(1000.)).flex_none()),
                Handle::Scroll(self.outer.clone()),
            ))
        }
    }
    let inner = ListState::new(10, ListAlignment::Top, px(0.)).measure_all();
    let outer = ScrollHandle::new();
    let window = cx.add_window(|_, _| Nested {
        inner: inner.clone(),
        outer: outer.clone(),
    });
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    let wheel = ScrollWheelEvent {
        position: point(px(5.), px(5.)),
        delta: ScrollDelta::Lines(point(0., -3.)),
        ..Default::default()
    };
    visual.simulate_event(wheel.clone());
    cx.executor().advance_clock(Duration::from_millis(150));
    visual.update(|window, cx| {
        window.simulate_next_frame(cx);
        window.draw(cx).clear(cx);
    });
    assert_near(inner.scroll_px_offset_for_scrollbar().y, -60.);
    assert_eq!(outer.offset().y, px(0.));

    inner.scroll_to_end();
    visual.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    });
    visual.simulate_event(wheel);
    cx.executor().advance_clock(Duration::from_millis(150));
    visual.update(|window, cx| {
        window.simulate_next_frame(cx);
        window.draw(cx).clear(cx);
    });
    assert_near(inner.scroll_px_offset_for_scrollbar().y, -900.);
    assert!(
        outer.offset().y < px(0.),
        "the parent must receive wheel input at the child's limit"
    );
}
