use crate::widgets::TooltipTrigger as _;
use crate::{
    icon::Icon,
    sizing::{Sizable, Size},
    theme::ActiveTheme as _,
    widgets::{Spinner, Tooltip},
};
use gpui::{
    AnyElement, App, ClickEvent, Corners, ElementId, InteractiveElement, Interactivity,
    IntoElement, ParentElement, Pixels, RenderOnce, SharedString, StatefulInteractiveElement,
    StyleRefinement, Styled, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::motion::Interpolate;
use std::rc::Rc;

/// How long a button's colours, dim, spinner and glyph take to glide.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

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
    Chip,
    Size(Pixels),
    /// One radius per corner, for a button that fills the end of a bar and
    /// takes the bar's own corners on that side.
    Corners(Corners<Pixels>),
}

impl From<Pixels> for ButtonRounded {
    fn from(value: Pixels) -> Self {
        Self::Size(value)
    }
}

#[derive(IntoElement)]
pub struct Button {
    id: ElementId,
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
    /// Owes a finger a [`TOUCH_TARGET`](crate::material::TOUCH_TARGET)
    /// square whatever the button's size draws.
    touch: bool,
    /// Of that square, only the height: one of a row of actions
    /// ([`Button::touch_in_row`]).
    touch_in_row: bool,
    /// The composer's chip control: 28 tall at the chip radius.
    chip: bool,
    /// How small the label may scale when the button is given less width
    /// than it asks for ([`Button::shrink_label_to`]).
    label_shrinks_to: Option<f32>,
    loading: bool,
    size: Size,
    tooltip: Option<SharedString>,
    /// Accessible name for icon-only buttons, which have no visible label to
    /// derive one from.
    aria_label: Option<SharedString>,
    on_click: Option<ClickHandler>,
    on_hover: Option<super::ToggleHandler>,
    press_then: Option<super::popover::SheetToggle>,
    /// The handle the caller tracks, when it keeps its own.
    focus: Option<gpui::FocusHandle>,
    /// The view's store its clocks are kept in, when its view keeps them.
    motion: Option<crate::motion::owned::Owned>,
}

pub(crate) type ClickHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

/// Height of a [`Button::chip`].
const CHIP_HEIGHT: f32 = 28.;

impl Button {
    pub fn new(id: impl Into<ElementId>) -> Self {
        let id = id.into();
        Self {
            base: gpui_base::Button::new(id.clone()),
            id,
            icon: None,
            label: None,
            children: Vec::new(),
            disabled: false,
            selected: false,
            variant: ButtonVariant::Default,
            rounded: ButtonRounded::Medium,
            outline: false,
            compact: false,
            touch: false,
            touch_in_row: false,
            chip: false,
            label_shrinks_to: None,
            loading: false,
            size: Size::Medium,
            tooltip: None,
            aria_label: None,
            on_click: None,
            on_hover: None,
            press_then: None,
            focus: None,
            motion: None,
        }
    }
    pub fn outline(mut self) -> Self {
        self.outline = true;
        self
    }
    /// Keep a [`TOUCH_TARGET`](crate::material::TOUCH_TARGET) hit box on a
    /// touch layout; the button's size still decides what it draws.
    pub fn touch(mut self, touch: bool) -> Self {
        self.touch = touch;
        self
    }
    /// [`Button::touch`] for one of a row of icon actions: the full touch
    /// height, but only [`TOUCH_ROW_ACTION_WIDTH`](crate::material::TOUCH_ROW_ACTION_WIDTH)
    /// across, so the row's glyphs read as one group rather than a touch
    /// square apart.
    pub fn touch_in_row(mut self, touch: bool) -> Self {
        self.touch = touch;
        self.touch_in_row = true;
        self
    }
    /// The chip form used by the composer's inline controls: 28pt tall at the
    /// chip radius. gpui-base's `Button` carries no sizes, so this is the one
    /// place the chip's geometry is written.
    pub fn chip(mut self) -> Self {
        self.chip = true;
        self.rounded = ButtonRounded::Chip;
        self
    }
    /// Let a parent narrower than the button shrink it, the label scaling
    /// down to `smallest` of its size to fit before its end fades
    /// ([`super::FadeText::shrink_to`]).
    pub fn shrink_label_to(mut self, smallest: f32) -> Self {
        self.label_shrinks_to = Some(smallest);
        self
    }
    pub fn rounded(mut self, rounded: impl Into<Pixels>) -> Self {
        self.rounded = ButtonRounded::Size(rounded.into());
        self
    }
    pub fn rounded_corners(mut self, corners: Corners<Pixels>) -> Self {
        self.rounded = ButtonRounded::Corners(corners);
        self
    }
    /// Whether pressing the button moves focus onto it; see
    /// [`gpui_base::Button::focusable`]. A menu that acts on a focused
    /// input's selection must leave that focus where it is.
    pub fn focusable(mut self, focusable: bool) -> Self {
        self.base = self.base.focusable(focusable);
        self
    }
    /// Focus the button through the caller's `handle`, so the caller can
    /// sample whether the keyboard has put focus on it: a control shown only
    /// on hover shows while it holds keyboard focus too.
    pub fn track_focus(mut self, handle: &gpui::FocusHandle) -> Self {
        self.focus = Some(handle.clone());
        self
    }
    /// Keep the button's colours, dim, spinner and glyph fade in `motion`,
    /// under the button's id, rather than under its element path: for a
    /// button its view draws under another parent (the composer's, as the
    /// window turns compact or wide), where each would otherwise start over
    /// at its end. Two buttons with one id share their clocks in one store.
    pub(crate) fn motion(mut self, motion: &crate::motion::owned::Owned) -> Self {
        self.motion = Some(motion.scope(format!("button:{}", self.id)));
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
    /// Name an icon-only button for assistive technology. Buttons with a
    /// visible label already announce it and need no override.
    pub fn aria_label(mut self, label: impl Into<SharedString>) -> Self {
        self.aria_label = Some(label.into());
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
    /// What a press from assistive technology does after the button's own click handler, for a
    /// button whose click alone is not all it does: one that opens a popover. Every button with
    /// a click handler presses as it clicks; a disabled or loading one does not. GPUI's default
    /// press clicks the node's centre, which misses a button scrolled or clipped out of view.
    pub(crate) fn press_action(mut self, then: super::popover::SheetToggle) -> Self {
        self.press_then = Some(then);
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

impl StatefulInteractiveElement for Button {
    fn on_a11y_action(
        mut self,
        action: gpui::accesskit::Action,
        listener: impl FnMut(Option<&gpui::accesskit::ActionData>, &mut Window, &mut App) + 'static,
    ) -> Self {
        if action == gpui::accesskit::Action::Click {
            let listener = std::cell::RefCell::new(listener);
            return self.press_action(Rc::new(move |window, cx| {
                listener.borrow_mut()(None, window, cx);
            }));
        }
        self.base = self.base.on_a11y_action(action, listener);
        self
    }
}

impl RenderOnce for Button {
    fn render(mut self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        // Hover is read at render, rather than left to a hover style, so its
        // colour can glide. A button gives pressing no look of its own.
        let (is_hovered, hovered) = crate::motion::hovered(self.id.clone(), window, cx);
        let clocks = Clocks {
            id: self.id.clone(),
            motion: self.motion.take(),
        };
        let spinning = clocks.transition(
            "loading",
            if self.loading && self.icon.is_some() {
                1.
            } else {
                0.
            },
            window,
            cx,
        );
        let dimmed = clocks.transition(
            "opacity",
            if self.disabled || self.loading {
                0.5
            } else {
                1.
            },
            window,
            cx,
        );
        let theme = cx.theme();
        let transparent = theme.background.opacity(0.);
        // `None` hovers as the theme's hover tint over the rest fill.
        let (bg, fg, hover, border) = match self.variant {
            ButtonVariant::Default => (theme.background, theme.foreground, None, theme.input),
            ButtonVariant::Primary => (
                theme.primary,
                theme.primary_foreground,
                Some(theme.primary.opacity(0.85)),
                theme.primary,
            ),
            ButtonVariant::Secondary => (theme.secondary, theme.foreground, None, theme.border),
            ButtonVariant::Danger => (
                theme.danger.opacity(0.2),
                theme.danger,
                Some(theme.danger.opacity(0.3)),
                theme.danger,
            ),
            ButtonVariant::Success => (
                theme.success.opacity(0.2),
                theme.success,
                Some(theme.success.opacity(0.3)),
                theme.success,
            ),
            // Transparent at rest; the hairline only shows with `.outline()`
            // (border width is 0 otherwise), which is how select/dropdown
            // triggers read as interactive controls.
            ButtonVariant::Ghost => (transparent, theme.foreground, None, theme.border),
            ButtonVariant::Text => (
                transparent,
                theme.foreground.opacity(0.9),
                None,
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
        // In place of a nearly opaque rest fill, the faint tint alone would
        // paint almost what the rest fill does.
        let hover = hover.unwrap_or_else(|| tinted(bg, theme.muted));
        let corners = match self.rounded {
            ButtonRounded::Medium => Corners::all(theme.tokens.radius.md),
            ButtonRounded::Chip => Corners::all(crate::material::radius_chip(cx)),
            ButtonRounded::Size(value) => Corners::all(value),
            ButtonRounded::Corners(corners) => corners,
        };
        let icon_only = self.label.is_none() && self.children.is_empty();
        // A custom size sets the hit box, not the glyph: a 44pt touch control
        // keeps the medium icon rather than a 33pt one.
        let icon_size = match self.size {
            Size::Size(_) => Size::Medium,
            size => size,
        };
        // A caller that colours the button's text (a muted disclosure label)
        // keeps that colour; the variant supplies the default.
        let fg = self.base.style().text.color.unwrap_or(fg);
        // A caller's opacity (a control that shows on row hover) is the
        // button's, dimmed further while disabled or loading.
        let dimmed = self.base.style().opacity.unwrap_or(1.) * dimmed;
        let (bg, border) = match (is_hovered, self.selected) {
            (true, _) => (hover, border),
            (false, true) => (theme.muted, theme.primary),
            (false, false) => (bg, border),
        };
        let glide = |channel, colour, window: &mut Window, cx: &mut App| {
            clocks
                .transition(channel, crate::motion::Colour(colour), window, cx)
                .0
        };
        let bg = glide("bg", bg, window, cx);
        let fg = glide("fg", fg, window, cx);
        let border = glide("border", border, window, cx);
        let accessibility_label = self.aria_label.clone().or_else(|| self.label.clone());
        // gpui-base's Button keeps its own focus handle, leaving only GPUI's
        // `focus_visible`, which switches at once, to show the keyboard's
        // focus: it is handed this handle instead, and the ring fades in and
        // out over it (`motion::focus_ring`).
        let focus = match self.focus.take() {
            Some(focus) => focus,
            None => crate::motion::focus_ring::handle(&self.id, window, cx),
        };
        // Every button that acts on a click acts on a press too, so a press
        // never falls to GPUI's default of clicking where the node last was,
        // which a button folding or gliding away has left to its neighbour.
        let press = (!self.disabled
            && !self.loading
            && (self.on_click.is_some() || self.press_then.is_some()))
        .then(|| {
            let (on_click, then) = (self.on_click.clone(), self.press_then.clone());
            move |_: Option<&gpui::accesskit::ActionData>, window: &mut Window, cx: &mut App| {
                if let Some(on_click) = &on_click {
                    on_click(&ClickEvent::default(), window, cx);
                }
                if let Some(then) = &then {
                    then(window, cx);
                }
            }
        });
        // A new glyph (Copy becoming its check) cross-fades over the one it
        // replaces; the icon as given is the current one.
        let icon = self.icon.map(|icon| {
            let icon = icon.with_size(icon_size).opacity(1. - spinning);
            let glyph = icon.glyph().clone();
            let leaving = icon.clone();
            let mut current = Some(icon);
            let shown = glyph.clone();
            let build = move |shown: &SharedString, _: &mut Window, _: &mut App| match current
                .take()
                .filter(|_| *shown == glyph)
            {
                Some(icon) => icon.into_any_element(),
                None => leaving.clone().path(shown.clone()).into_any_element(),
            };
            clocks.clock().cross_fade_part(
                clocks.channel("icon"),
                shown,
                &crate::motion::Curve::new(GLIDE),
                window,
                cx,
                build,
            )
        });
        let shrinks = self.label_shrinks_to.is_some();
        let label = self
            .label
            .map(|label| super::fade_text(label).shrink_to(self.label_shrinks_to.unwrap_or(1.)));
        let content =
            gpui_base::h_flex()
                .size_full()
                .items_center()
                .justify_center()
                .map(|this| match self.size {
                    Size::XSmall | Size::Small => this.gap_1(),
                    _ => this.gap_2(),
                })
                .when_some(icon, |this, icon| {
                    // The spinner cross-fades over the icon, which keeps its
                    // place so the label does not move.
                    this.child(div().relative().flex_none().child(icon).when(
                        spinning > 0.,
                        |this| {
                            this.child(
                                div()
                                    .absolute()
                                    .inset_0()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .opacity(spinning)
                                    .child(Spinner::new().with_size(icon_size)),
                            )
                        },
                    ))
                })
                .when_some(label, |this, label| this.child(label))
                .children(self.children)
                .when(shrinks, |this| this.min_w_0());
        let button = self
            .base
            .track_focus(&focus)
            .disabled(self.disabled || self.loading)
            .selected(self.selected)
            .when_some(accessibility_label, |this, label| {
                this.accessibility_label(label)
            })
            .opacity(dimmed)
            .flex()
            .map(|this| {
                if shrinks {
                    this.flex_shrink(1.).min_w_0()
                } else {
                    this.flex_shrink_0()
                }
            })
            .items_center()
            .justify_center()
            .rounded_tl(corners.top_left)
            .rounded_tr(corners.top_right)
            .rounded_bl(corners.bottom_left)
            .rounded_br(corners.bottom_right)
            .when(self.chip, |this| this.h(px(CHIP_HEIGHT)).px_2())
            .when(!self.chip && icon_only, |this| match self.size {
                Size::XSmall => this.size_5(),
                Size::Small => this.size_6(),
                Size::Size(v) => this.size(v),
                _ => this.size_8(),
            })
            .when(
                !self.chip && !icon_only && !matches!(self.variant, ButtonVariant::Text),
                |this| match self.size {
                    Size::XSmall => this.h_5().px_1(),
                    Size::Small => this.h_6().px_2(),
                    Size::Size(v) => this.h(v).px(v * 0.2),
                    _ => this.h_8().px(if self.compact { px(8.) } else { px(10.) }),
                },
            )
            .when(self.touch, |this| {
                this.min_w(px(if self.touch_in_row {
                    crate::material::TOUCH_ROW_ACTION_WIDTH
                } else {
                    crate::material::TOUCH_TARGET
                }))
                .min_h(px(crate::material::TOUCH_TARGET))
            })
            .when(
                matches!(self.variant, ButtonVariant::Default) || self.outline,
                |this| this.border_1(),
            )
            .border_color(border)
            .bg(bg)
            .text_color(fg)
            .when_some(self.on_click, |this, handler| {
                this.on_click(move |event, window, cx| handler(event, window, cx))
            })
            .when_some(press, |this, press| {
                this.on_a11y_action(gpui::accesskit::Action::Click, press)
            })
            .on_hover({
                let on_hover = self.on_hover;
                move |is_hovered, window, cx| {
                    hovered(is_hovered, window, cx);
                    if let Some(handler) = &on_hover {
                        handler(is_hovered, window, cx);
                    }
                }
            })
            .child(content)
            .when_some(self.tooltip, |this, text| {
                this.with_tooltip(move |_, _| Tooltip::new(text.clone()))
            });
        crate::motion::focus_ring::Ringed::new(
            button,
            &focus,
            crate::motion::focus_ring::corners_of(corners),
            cx,
        )
        .opacity(dimmed)
    }
}

/// The small × at the right end of a header that closes the surface the
/// header belongs to: a pane among several, the wide Machines page.
pub(crate) fn close_button(id: &'static str, tooltip: impl Into<SharedString>) -> Button {
    let tooltip = tooltip.into();
    Button::new(id)
        .debug_selector(move || id.into())
        // A press must not arm a header's window drag region.
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .ghost()
        .small()
        .compact()
        .icon(crate::icon::IconName::Close)
        .aria_label(tooltip.clone())
        .tooltip(tooltip)
}

/// `tint` laid over `fill`, as the window paints the one over the other: the
/// premultiplied mix toward the tint's own colour, as far as its alpha goes.
pub(crate) fn tinted(fill: gpui::Hsla, tint: gpui::Hsla) -> gpui::Hsla {
    crate::motion::Colour(fill)
        .interpolate(&crate::motion::Colour(gpui::Hsla { a: 1., ..tint }), tint.a)
        .0
}

/// Where a [`Button`]'s clocks are kept, and the id its channels are
/// named under.
struct Clocks {
    id: ElementId,
    motion: Option<crate::motion::owned::Owned>,
}

impl Clocks {
    fn clock(&self) -> crate::motion::Clock<'_> {
        crate::motion::Clock::from(self.motion.as_ref())
    }

    fn channel(&self, channel: &'static str) -> ElementId {
        ElementId::NamedChild(std::sync::Arc::new(self.id.clone()), channel.into())
    }

    /// The value now of the button's `channel` gliding to `target`.
    fn transition<T>(
        &self,
        channel: &'static str,
        target: T,
        window: &mut Window,
        cx: &mut App,
    ) -> T
    where
        T: Interpolate + PartialEq + 'static,
    {
        self.clock().transition(
            self.channel(channel),
            target,
            &crate::motion::Curve::new(GLIDE),
            window,
            cx,
        )
    }
}
