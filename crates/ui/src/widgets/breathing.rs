use gpui::{App, Hsla, IntoElement, ParentElement as _, RenderOnce, Styled as _, Window, div};
use gpui_base::motion::{Easing, IterationCount, Keyframe, Keyframes, Timing, animate_keyframes};
use std::time::Duration;

/// A dot that slowly breathes — dims and draws in, then returns — for work
/// that goes on without anything waiting on it, where [`super::Spinner`]
/// marks work something waits on. It takes the current font size as its
/// box, like the spinner, so either can stand in the other's place.
#[derive(IntoElement)]
pub struct BreathingDot {
    color: Hsla,
}

impl BreathingDot {
    pub fn new(color: Hsla) -> Self {
        Self { color }
    }
}

impl RenderOnce for BreathingDot {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        // One breath per iteration, from rest back to rest: reduced motion
        // samples the last frame, so the dot rests.
        let breath = animate_keyframes(
            "breathing-dot",
            &Keyframes::try_new([
                Keyframe::new(0., 0.),
                Keyframe::new(0.5, 1.).ease(Easing::EaseInOut),
                Keyframe::new(1., 0.).ease(Easing::EaseInOut),
            ])
            .expect("ordered keyframes from rest to rest"),
            Timing::new(Duration::from_millis(1800)).iterations(IterationCount::Infinite),
            window,
            cx,
        )
        .value;
        let size = window.text_style().font_size.to_pixels(window.rem_size());
        let dot = size * (0.5 - 0.12 * breath);
        div()
            .flex_none()
            .size(size)
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .size(dot)
                    .rounded_full()
                    .bg(self.color)
                    .opacity(1. - 0.6 * breath),
            )
    }
}
