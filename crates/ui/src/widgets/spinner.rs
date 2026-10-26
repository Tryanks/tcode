use crate::{
    icon::{Icon, IconName},
    sizing::{Sizable, Size},
};
use gpui::{
    App, Hsla, IntoElement, ParentElement as _, RenderOnce, Styled as _, Transformation, Window,
    div, percentage, prelude::FluentBuilder as _,
};
use gpui_base::motion::{Easing, IterationCount, Keyframe, Keyframes, Timing, animate_keyframes};
use std::time::Duration;

/// A rotating loader icon. With no explicit size it takes the current font
/// size, like [`Icon`].
#[derive(IntoElement)]
pub struct Spinner {
    size: Option<Size>,
    icon: Icon,
    color: Option<Hsla>,
}

impl Spinner {
    pub fn new() -> Self {
        Self {
            size: None,
            icon: Icon::new(IconName::Loader),
            color: None,
        }
    }

    pub fn icon(mut self, icon: impl Into<Icon>) -> Self {
        self.icon = icon.into();
        self
    }

    pub fn color(mut self, color: Hsla) -> Self {
        self.color = Some(color);
        self
    }
}

impl Default for Spinner {
    fn default() -> Self {
        Self::new()
    }
}

impl Sizable for Spinner {
    fn with_size(mut self, size: impl Into<Size>) -> Self {
        self.size = Some(size.into());
        self
    }
}

impl RenderOnce for Spinner {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        // Reduced motion samples the last frame, a full turn: the glyph rests.
        let turn = animate_keyframes(
            "spinner-turn",
            &Keyframes::try_new([Keyframe::new(0., 0.), Keyframe::new(1., 1.)])
                .expect("two ordered endpoints"),
            Timing::new(Duration::from_millis(800))
                .iterations(IterationCount::Infinite)
                .ease(Easing::EaseInOut),
            window,
            cx,
        )
        .value;
        div().child(
            self.icon
                .when_some(self.size, |icon, size| icon.with_size(size))
                .when_some(self.color, |icon, color| icon.text_color(color))
                .transform(Transformation::rotate(percentage(turn))),
        )
    }
}
