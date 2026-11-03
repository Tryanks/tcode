use crate::{
    icon::{Icon, IconName},
    sizing::{Sizable, Size},
    theme::ActiveTheme as _,
};
use gpui::{
    AnyElement, App, ElementId, InteractiveElement, Interactivity, IntoElement, ParentElement,
    RenderOnce, SharedString, StatefulInteractiveElement, StyleRefinement, Styled, Transformation,
    Window, div, prelude::FluentBuilder as _, px, relative,
};
use gpui_base::CheckboxIndicator;
use gpui_base::StyledExt as _;
use gpui_base::motion::{Transition, transition};
use std::rc::Rc;

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

#[derive(IntoElement)]
pub struct Checkbox {
    id: ElementId,
    base: gpui_base::Checkbox,
    style: StyleRefinement,
    label: Option<SharedString>,
    /// The name assistive technology reads when the visible text is in the children.
    aria_label: Option<SharedString>,
    children: Vec<AnyElement>,
    checked: bool,
    disabled: bool,
    size: Size,
    on_click: Option<super::ToggleHandler>,
}

impl Checkbox {
    pub fn new(id: impl Into<ElementId>) -> Self {
        let id = id.into();
        Self {
            base: gpui_base::Checkbox::new(id.clone()),
            id,
            style: StyleRefinement::default(),
            label: None,
            aria_label: None,
            children: Vec::new(),
            checked: false,
            disabled: false,
            size: Size::Medium,
            on_click: None,
        }
    }
    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }
    pub fn aria_label(mut self, label: impl Into<SharedString>) -> Self {
        self.aria_label = Some(label.into());
        self
    }
    pub fn checked(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
    pub fn on_click(mut self, handler: impl Fn(&bool, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }
}
impl Sizable for Checkbox {
    fn with_size(mut self, size: impl Into<Size>) -> Self {
        self.size = size.into();
        self
    }
}
impl Styled for Checkbox {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}
impl ParentElement for Checkbox {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}
impl InteractiveElement for Checkbox {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.base.interactivity()
    }
}
impl StatefulInteractiveElement for Checkbox {}

impl RenderOnce for Checkbox {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let checked = self.checked;
        let indicator_size = match self.size {
            Size::XSmall => px(12.),
            Size::Small => px(14.),
            Size::Large => px(18.),
            Size::Size(v) => v,
            Size::Medium => px(16.),
        };
        let label = self.label.clone();
        let settle = || Transition::new(GLIDE);
        let (rest_bg, rest_border, primary) =
            (cx.theme().background, cx.theme().input, cx.theme().primary);
        let bg = transition(
            (self.id.clone(), "box-bg"),
            if checked { primary } else { rest_bg },
            settle(),
            window,
            cx,
        );
        let border = transition(
            (self.id.clone(), "box-border"),
            if checked { primary } else { rest_border },
            settle(),
            window,
            cx,
        );
        // The check stays drawn while it fades and shrinks away.
        let check = transition(
            (self.id.clone(), "check"),
            if checked { 1. } else { 0. },
            settle(),
            window,
            cx,
        );
        let check_scale = 0.5 + 0.5 * check;
        let dim = transition(
            (self.id.clone(), "dim"),
            if self.disabled { 1. } else { 0. },
            settle(),
            window,
            cx,
        );
        let label_color = transition(
            (self.id.clone(), "label"),
            if self.disabled {
                cx.theme().muted_foreground
            } else {
                cx.theme().foreground
            },
            settle(),
            window,
            cx,
        );
        // gpui-base's Checkbox keeps its own focus handle, leaving only GPUI's
        // `focus_visible`, which switches at once, to show the keyboard's
        // focus: it is handed this handle instead, and the ring fades in and
        // out over the box (`motion::focus_ring`).
        let focus = crate::motion::focus_ring::handle(&self.id, window, cx);
        let radius = cx.theme().tokens.radius.sm;
        self.base
            .track_focus(&focus)
            .checked(checked)
            .disabled(self.disabled)
            .when_some(label.clone().or(self.aria_label), |this, label| {
                this.accessibility_label(label)
            })
            .when_some(self.on_click, |this, handler| {
                this.on_change(move |state, _, window, cx| {
                    let value = matches!(state, gpui_base::CheckboxState::Checked);
                    handler(&value, window, cx);
                })
            })
            .h_flex()
            .gap_2()
            .items_start()
            .line_height(relative(1.))
            .text_color(label_color)
            .refine_style(&self.style)
            .child(crate::motion::focus_ring::Ringed::new(
                CheckboxIndicator::new()
                    .checked(checked)
                    .disabled(self.disabled)
                    .size(indicator_size)
                    .flex_shrink_0()
                    .border_1()
                    .rounded(radius)
                    .bg(bg)
                    .border_color(border)
                    .opacity(1. - 0.5 * dim)
                    .when(check > 0., |this| {
                        this.child(
                            Icon::new(IconName::Check)
                                .with_size(Size::Size(indicator_size))
                                .text_color(cx.theme().primary_foreground.opacity(check))
                                .transform(Transformation::scale(gpui::size(
                                    check_scale,
                                    check_scale,
                                ))),
                        )
                    }),
                &focus,
                crate::motion::focus_ring::corners(radius),
                cx,
            ))
            .when_some(label, |this, label| this.child(div().child(label)))
            .children(self.children)
    }
}
