use crate::{
    icon::Icon,
    sizing::{Sizable, Size},
    theme::ActiveTheme as _,
    widgets::{Spinner, Tooltip},
};
use gpui::{
    AnyElement, App, ClickEvent, ElementId, InteractiveElement, Interactivity, IntoElement,
    ParentElement, Pixels, RenderOnce, SharedString, StatefulInteractiveElement, StyleRefinement,
    Styled, Window, prelude::FluentBuilder as _, px,
};
use std::rc::Rc;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ButtonVariant {
    #[default]
    Default,
    Primary,
    Secondary,
    Danger,
    Success,
    Ghost,
    Text,
}

pub trait ButtonVariants: Sized {
    fn with_variant(self, variant: ButtonVariant) -> Self;
    fn primary(self) -> Self {
        self.with_variant(ButtonVariant::Primary)
    }
    fn secondary(self) -> Self {
        self.with_variant(ButtonVariant::Secondary)
    }
    fn danger(self) -> Self {
        self.with_variant(ButtonVariant::Danger)
    }
    fn success(self) -> Self {
        self.with_variant(ButtonVariant::Success)
    }
    fn ghost(self) -> Self {
        self.with_variant(ButtonVariant::Ghost)
    }
    fn text(self) -> Self {
        self.with_variant(ButtonVariant::Text)
    }
}

#[derive(Clone, Copy, Default)]
enum ButtonRounded {
    #[default]
    Medium,
    Size(Pixels),
}

impl From<Pixels> for ButtonRounded {
    fn from(value: Pixels) -> Self {
        Self::Size(value)
    }
}

#[derive(IntoElement)]
pub struct Button {
    base: gpui_base::Button,
    icon: Option<Icon>,
    label: Option<SharedString>,
    children: Vec<AnyElement>,
    disabled: bool,
    selected: bool,
    variant: ButtonVariant,
    rounded: ButtonRounded,
    outline: bool,
    compact: bool,
    loading: bool,
    size: Size,
    tooltip: Option<SharedString>,
    on_click: Option<ClickHandler>,
    on_hover: Option<super::ToggleHandler>,
}

type ClickHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

impl Button {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            base: gpui_base::Button::new(id),
            icon: None,
            label: None,
            children: Vec::new(),
            disabled: false,
            selected: false,
            variant: ButtonVariant::Default,
            rounded: ButtonRounded::Medium,
            outline: false,
            compact: false,
            loading: false,
            size: Size::Medium,
            tooltip: None,
            on_click: None,
            on_hover: None,
        }
    }
    pub fn outline(mut self) -> Self {
        self.outline = true;
        self
    }
    pub fn rounded(mut self, rounded: impl Into<Pixels>) -> Self {
        self.rounded = ButtonRounded::Size(rounded.into());
        self
    }
    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }
    pub fn icon(mut self, icon: impl Into<Icon>) -> Self {
        self.icon = Some(icon.into());
        self
    }
    pub fn tooltip(mut self, tooltip: impl Into<SharedString>) -> Self {
        self.tooltip = Some(tooltip.into());
        self
    }
    pub fn compact(mut self) -> Self {
        self.compact = true;
        self
    }
    pub fn loading(mut self, loading: bool) -> Self {
        self.loading = loading;
        self
    }
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }
    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }
    pub fn on_hover(mut self, handler: impl Fn(&bool, &mut Window, &mut App) + 'static) -> Self {
        self.on_hover = Some(Rc::new(handler));
        self
    }
}

impl ButtonVariants for Button {
    fn with_variant(mut self, variant: ButtonVariant) -> Self {
        self.variant = variant;
        self
    }
}

impl gpui_base::Selectable for Button {
    fn selected(self, selected: bool) -> Self {
        Button::selected(self, selected)
    }
    fn is_selected(&self) -> bool {
        self.selected
    }
}

impl Sizable for Button {
    fn with_size(mut self, size: impl Into<Size>) -> Self {
        self.size = size.into();
        self
    }
}

impl Styled for Button {
    fn style(&mut self) -> &mut StyleRefinement {
        self.base.style()
    }
}

impl ParentElement for Button {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl InteractiveElement for Button {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.base.interactivity()
    }
}

impl RenderOnce for Button {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let transparent = theme.background.opacity(0.);
        let (bg, fg, hover, border) = match self.variant {
            ButtonVariant::Default => {
                (theme.background, theme.foreground, theme.muted, theme.input)
            }
            ButtonVariant::Primary => (
                theme.primary,
                theme.primary_foreground,
                theme.primary.opacity(0.85),
                theme.primary,
            ),
            ButtonVariant::Secondary => (
                theme.secondary,
                theme.foreground,
                theme.secondary_active,
                theme.border,
            ),
            ButtonVariant::Danger => (
                theme.danger.opacity(0.2),
                theme.danger,
                theme.danger.opacity(0.3),
                theme.danger,
            ),
            ButtonVariant::Success => (
                theme.success.opacity(0.2),
                theme.success,
                theme.success.opacity(0.3),
                theme.success,
            ),
            // Transparent at rest; the hairline only shows with `.outline()`
            // (border width is 0 otherwise), which is how select/dropdown
            // triggers read as interactive controls.
            ButtonVariant::Ghost => (transparent, theme.foreground, theme.muted, theme.border),
            ButtonVariant::Text => (
                transparent,
                theme.foreground.opacity(0.9),
                theme.muted,
                transparent,
            ),
        };
        let (bg, fg) = if self.outline {
            let outline_fg = match self.variant {
                ButtonVariant::Primary => theme.primary,
                ButtonVariant::Danger => theme.danger,
                ButtonVariant::Success => theme.success,
                _ => fg,
            };
            (bg.opacity(0.1), outline_fg)
        } else {
            (bg, fg)
        };
        let radius = match self.rounded {
            ButtonRounded::Medium => theme.radius,
            ButtonRounded::Size(value) => value,
        };
        let icon_only = self.label.is_none() && self.children.is_empty();
        let icon_size = match self.size {
            Size::Size(v) => Size::Size(v * 0.75),
            size => size,
        };
        let accessibility_label = self.label.clone();
        let content = gpui_base::h_flex()
            .size_full()
            .items_center()
            .justify_center()
            .map(|this| match self.size {
                Size::XSmall | Size::Small => this.gap_1(),
                _ => this.gap_2(),
            })
            .when_some(self.icon, |this, icon| {
                this.child(if self.loading {
                    Spinner::new().with_size(icon_size).into_any_element()
                } else {
                    icon.with_size(icon_size).into_any_element()
                })
            })
            .when_some(self.label, |this, label| this.child(label))
            .children(self.children);
        self.base
            .disabled(self.disabled || self.loading)
            .selected(self.selected)
            .when_some(accessibility_label, |this, label| {
                this.accessibility_label(label)
            })
            .styles(|styles| {
                styles
                    .selected(|style| style.bg(theme.muted).border_color(theme.primary))
                    .disabled(|style| style.opacity(0.5))
            })
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .rounded(radius)
            .when(icon_only, |this| match self.size {
                Size::XSmall => this.size_5(),
                Size::Small => this.size_6(),
                Size::Size(v) => this.size(v),
                _ => this.size_8(),
            })
            .when(
                !icon_only && !matches!(self.variant, ButtonVariant::Text),
                |this| match self.size {
                    Size::XSmall => this.h_5().px_1(),
                    Size::Small => this.h_6().px_2(),
                    Size::Size(v) => this.px(v * 0.2),
                    _ => this.h_8().px(if self.compact { px(8.) } else { px(10.) }),
                },
            )
            .when(
                matches!(self.variant, ButtonVariant::Default) || self.outline,
                |this| this.border_1(),
            )
            .border_color(border)
            .bg(bg)
            .text_color(fg)
            .hover(move |this| this.bg(hover))
            .when_some(self.on_click, |this, handler| {
                this.on_click(move |event, window, cx| handler(event, window, cx))
            })
            .when_some(self.on_hover, |this, handler| {
                this.on_hover(move |hovered, window, cx| handler(hovered, window, cx))
            })
            .child(content)
            .when_some(self.tooltip, |this, text| {
                this.tooltip(move |window, cx| Tooltip::new(text.clone()).build(window, cx))
            })
    }
}
