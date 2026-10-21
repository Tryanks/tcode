//! The compact Queue button's touch. A tap queues; holding the finger down,
//! or sliding it up, arms a steer so that lifting steers the running turn.

use super::super::*;
use gpui::{Bounds, Pixels, Point, TouchDragEvent, TouchPhase};

/// How far above where it landed the finger must travel to arm a steer.
const SLIDE_UP: Pixels = px(32.);

pub(in super::super) struct SteerGesture {
    start: Point<Pixels>,
    bounds: Bounds<Pixels>,
    held: bool,
    lifted: bool,
    _hold: Task<()>,
}

impl SteerGesture {
    pub(in super::super) fn armed(&self) -> bool {
        self.held || self.lifted
    }
}

impl Composer {
    /// The touch handlers painted over the compact Queue button. The drag is
    /// claimed on the first touch, before it could become a tap, a long
    /// press or a scroll of the thread behind it. That claim ends the
    /// window's own long-press recognition for this touch, so the hold is
    /// timed here with gpui's long-press duration.
    pub(in super::super) fn render_steer_gesture(&self, cx: &mut Context<Self>) -> AnyElement {
        let composer = cx.entity();
        gpui::canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                window.on_mouse_event(move |event: &TouchDragEvent, phase, window, cx| {
                    if !phase.bubble() {
                        return;
                    }
                    match event.phase {
                        TouchPhase::Started => {
                            if window.default_prevented() || !bounds.contains(&event.position) {
                                return;
                            }
                            window.prevent_default();
                            cx.stop_propagation();
                            composer.update(cx, |this, cx| {
                                this.begin_steer_gesture(bounds, event.position, cx);
                            });
                        }
                        TouchPhase::Moved => composer.update(cx, |this, cx| {
                            this.move_steer_gesture(event.position, cx);
                        }),
                        TouchPhase::Ended | TouchPhase::Cancelled => {
                            composer.update(cx, |this, cx| {
                                this.end_steer_gesture(event.phase, event.position, window, cx);
                            });
                        }
                    }
                });
            },
        )
        .absolute()
        .size_full()
        .into_any_element()
    }

    fn begin_steer_gesture(
        &mut self,
        bounds: Bounds<Pixels>,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let hold = gpui::GestureTuning::default().long_press_duration;
        let hold = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(hold).await;
            _ = this.update(cx, |this, cx| {
                this.update_steer_gesture(cx, |gesture| gesture.held = true);
            });
        });
        self.steer_gesture = Some(SteerGesture {
            start: position,
            bounds,
            held: false,
            lifted: false,
            _hold: hold,
        });
        cx.notify();
    }

    fn move_steer_gesture(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        self.update_steer_gesture(cx, |gesture| {
            gesture.lifted = gesture.start.y - position.y > SLIDE_UP;
        });
    }

    /// The haptic plays the moment a steer is armed: the finger can lift.
    fn update_steer_gesture(
        &mut self,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut SteerGesture),
    ) {
        let Some(gesture) = self.steer_gesture.as_mut() else {
            return;
        };
        let was_armed = gesture.armed();
        change(gesture);
        let armed = gesture.armed();
        if armed && !was_armed {
            crate::haptics::long_press();
        }
        if armed != was_armed {
            cx.notify();
        }
    }

    fn end_steer_gesture(
        &mut self,
        phase: TouchPhase,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(gesture) = self.steer_gesture.take() else {
            return;
        };
        cx.notify();
        if phase == TouchPhase::Cancelled {
            return;
        }
        let steer = gesture.armed();
        if steer || gesture.bounds.contains(&position) {
            let input = self.input.clone();
            self.submit(&input, steer, window, cx);
        }
    }
}
