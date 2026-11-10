use crate::widgets::TooltipTrigger as _;
use crate::{theme::ActiveTheme as _, widgets::Tooltip};
use gpui::{
    App, ElementId, IntoElement, ParentElement as _, RenderOnce, SharedString, StyleRefinement,
    Styled, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::StyledExt as _;
use gpui_base::motion::{Transition, transition};
use gpui_base::{SwitchThumb, SwitchTrack};
use std::rc::Rc;

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

#[derive(IntoElement)]
pub struct Switch {
    id: ElementId,
    style: StyleRefinement,
    checked: bool,
    disabled: bool,
    label: Option<SharedString>,
    aria_label: Option<SharedString>,
    on_click: Option<super::ToggleHandler>,
    tooltip: Option<SharedString>,
}

impl Switch {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            style: StyleRefinement::default(),
            checked: false,
            disabled: false,
            label: None,
            aria_label: None,
            on_click: None,
            tooltip: None,
        }
    }
    pub fn checked(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }
    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }
    pub fn aria_label(mut self, label: impl Into<SharedString>) -> Self {
        self.aria_label = Some(label.into());
        self
    }
    pub fn on_click(mut self, handler: impl Fn(&bool, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }
    pub fn tooltip(mut self, tooltip: impl Into<SharedString>) -> Self {
        self.tooltip = Some(tooltip.into());
        self
    }
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}
impl Styled for Switch {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for Switch {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let checked = self.checked;
        let (width, height, thumb) = (px(36.), px(20.), px(16.));
        let inset = px(2.);
        let checked_bg = cx.theme().primary;
        let unchecked_bg = cx.theme().muted;
        let thumb_bg = cx.theme().background;
        let settle = || Transition::new(GLIDE);
        let track_bg = transition(
            (self.id.clone(), "track-bg"),
            if checked { checked_bg } else { unchecked_bg },
            settle(),
            window,
            cx,
        );
        let thumb_left = transition(
            (self.id.clone(), "thumb-left"),
            if checked {
                width - thumb - inset * 2
            } else {
                px(0.)
            },
            settle(),
            window,
            cx,
        );
        let opacity = transition(
            (self.id.clone(), "dim"),
            if self.disabled { 0.5 } else { 1. },
            settle(),
            window,
            cx,
        );
        // gpui-base's Switch keeps its own focus handle, leaving only GPUI's
        // `focus_visible`, which switches at once, to show the keyboard's
        // focus: it is handed this handle instead, and the ring fades in and
        // out over the track (`motion::focus_ring`).
        let focus = crate::motion::focus_ring::handle(&self.id, window, cx);
        gpui_base::Switch::new(self.id.clone())
            .track_focus(&focus)
            .checked(checked)
            .disabled(self.disabled)
            .when_some(
                self.aria_label.or_else(|| self.label.clone()),
                |this, label| this.accessibility_label(label),
            )
            .when_some(self.on_click, |this, handler| {
                this.on_change(move |next, _, window, cx| handler(&next, window, cx))
            })
            .h_flex()
            .gap_2()
            .items_center()
            .refine_style(&self.style)
            .child(crate::motion::focus_ring::Ringed::new(
                SwitchTrack::new((self.id, "track"))
                    .checked(checked)
                    .disabled(self.disabled)
                    .w(width)
                    .h(height)
                    .rounded(height)
                    .flex()
                    .items_center()
                    .border(inset)
                    .border_color(cx.theme().background.opacity(0.))
                    .bg(track_bg)
                    .opacity(opacity)
                    .child(
                        SwitchThumb::new(checked)
                            .disabled(self.disabled)
                            .size(thumb)
                            .rounded(height)
                            .bg(thumb_bg)
                            .left(thumb_left),
                    )
                    .when_some(self.tooltip, |this, text| {
                        this.with_tooltip(move |_, _| Tooltip::new(text.clone()))
                    }),
                &focus,
                crate::motion::focus_ring::corners(height),
                cx,
            ))
            .when_some(self.label, |this, label| {
                this.child(div().line_height(height).child(label))
            })
    }
}
