use crate::{
    sizing::{Sizable, Size},
    theme::ActiveTheme as _,
};
use gpui::{
    Animation, AnimationExt as _, App, ElementId, Hsla, IntoElement, ParentElement as _,
    RenderOnce, StyleRefinement, Styled, Window, prelude::FluentBuilder as _, px, relative,
};
use gpui_base::StyledExt as _;
use gpui_base::motion::{Transition, transition};
use gpui_base::{ProgressIndicator, ProgressTrack};
use std::time::Duration;

#[derive(IntoElement)]
pub struct Progress {
    id: ElementId,
    style: StyleRefinement,
    color: Option<Hsla>,
    value: f32,
    size: Size,
    loading: bool,
}
impl Progress {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            style: StyleRefinement::default(),
            color: None,
            value: 0.,
            size: Size::Medium,
            loading: false,
        }
    }
    pub fn loading(mut self, loading: bool) -> Self {
        self.loading = loading;
        self
    }
    pub fn color(mut self, color: impl Into<Hsla>) -> Self {
        self.color = Some(color.into());
        self
    }
    pub fn value(mut self, value: f32) -> Self {
        self.value = value.clamp(0., 100.);
        self
    }
}
impl Sizable for Progress {
    fn with_size(mut self, size: impl Into<Size>) -> Self {
        self.size = size.into();
        self
    }
}
impl Styled for Progress {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}
impl RenderOnce for Progress {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        // A bar that changes colour with its value (a usage meter crossing a
        // threshold) glides between them with the fill.
        let color = transition(
            (self.id.clone(), "color"),
            self.color.unwrap_or(cx.theme().primary),
            Transition::new(crate::edge_back::SETTLE),
            window,
            cx,
        );
        let height = match self.size {
            Size::XSmall => px(4.),
            Size::Small => px(6.),
            Size::Medium => px(8.),
            Size::Large => px(10.),
            Size::Size(v) => v,
        };
        let value = self.value;
        let shown = transition(
            (self.id.clone(), "value"),
            value,
            Transition::new(crate::edge_back::SETTLE),
            window,
            cx,
        );
        let reduce_motion = cx.reduce_motion();
        gpui_base::Progress::new(self.id)
            .value(value)
            .indeterminate(self.loading)
            .w_full()
            .relative()
            .h(height)
            .rounded(height / 2.)
            .refine_style(&self.style)
            .child(
                ProgressTrack::new()
                    .absolute()
                    .size_full()
                    .rounded(height / 2.)
                    .bg(color.opacity(0.2)),
            )
            .child(
                ProgressIndicator::new()
                    .absolute()
                    .top_0()
                    .left_0()
                    .h_full()
                    .rounded(height / 2.)
                    .bg(color)
                    .map(|this| {
                        if self.loading && reduce_motion {
                            // A still segment in the middle of the track says
                            // "working" without the sweep.
                            this.left(relative(0.35))
                                .w(relative(0.3))
                                .into_any_element()
                        } else if self.loading {
                            this.with_animation(
                                "progress-loading",
                                Animation::new(Duration::from_secs(1)).repeat(),
                                |this, delta| this.left(relative(delta * 0.7)).w(relative(0.3)),
                            )
                            .into_any_element()
                        } else {
                            this.w(relative(shown / 100.)).into_any_element()
                        }
                    }),
            )
    }
}
