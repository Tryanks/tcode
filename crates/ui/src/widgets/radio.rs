//! A single-choice list whose options each say what they mean: a styled
//! composition of `gpui_base::RadioGroup`/`Radio`, one row per option with
//! its description under the title. [`super::choice`] is the same control
//! drawn as a segmented track, for options whose names say everything.
use crate::{theme::ActiveTheme as _, typography::TextRole as _};
use gpui::{
    AnyElement, App, ElementId, IntoElement, ParentElement as _, RenderOnce, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, prelude::FluentBuilder, px,
};
use gpui_base::motion::{Transition, transition};
use gpui_base::{Radio, RadioGroup, StyledExt as _, v_flex};
use std::rc::Rc;

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// The indicator's diameter; a glyph, not text, so no type token sizes it.
const INDICATOR: f32 = 16.;

type SelectHandler = Rc<dyn Fn(&mut Window, &mut App)>;

/// One option of a [`RadioList`].
pub struct RadioOption {
    id: ElementId,
    title: SharedString,
    description: Option<SharedString>,
    badge: Option<AnyElement>,
    selected: bool,
    on_select: Option<SelectHandler>,
}

impl RadioOption {
    pub fn new(id: impl Into<ElementId>, title: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            description: None,
            badge: None,
            selected: false,
            on_select: None,
        }
    }

    pub fn description(mut self, description: impl Into<SharedString>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Drawn after the title, such as the mark naming the factory value.
    pub fn badge(mut self, badge: impl IntoElement) -> Self {
        self.badge = Some(badge.into_any_element());
        self
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    /// Called when the option is chosen, including when it is chosen again
    /// while already selected, so an option can open what it stands for.
    pub fn on_select(mut self, handler: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_select = Some(Rc::new(handler));
        self
    }
}

impl FluentBuilder for RadioOption {}

/// Options in grouped-card rows, a hairline between each.
#[derive(IntoElement)]
pub struct RadioList {
    id: ElementId,
    compact: bool,
    disabled: bool,
    options: Vec<RadioOption>,
}

impl RadioList {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            compact: false,
            disabled: false,
            options: Vec::new(),
        }
    }

    pub fn compact(mut self, compact: bool) -> Self {
        self.compact = compact;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn options(mut self, options: impl IntoIterator<Item = RadioOption>) -> Self {
        self.options.extend(options);
        self
    }
}

impl RenderOnce for RadioList {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let size = self.options.len();
        let last = size.saturating_sub(1);
        let mut group = RadioGroup::new(self.id.clone()).w_full();
        for (index, option) in self.options.into_iter().enumerate() {
            let row = option_row(option, index, size, self.compact, self.disabled, window, cx);
            group = group.child(row);
            if index != last {
                group = group.child(crate::material::row_hairline(
                    px(crate::material::CARD_INSET),
                    cx,
                ));
            }
        }
        group
    }
}

fn option_row(
    option: RadioOption,
    index: usize,
    size: usize,
    compact: bool,
    disabled: bool,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let settle = || Transition::new(GLIDE);
    let id = option.id.clone();
    let primary = cx.theme().primary;
    let border = transition(
        (id.clone(), "radio-border"),
        if option.selected {
            primary
        } else {
            cx.theme().input
        },
        settle(),
        window,
        cx,
    );
    let dot = transition(
        (id.clone(), "radio-dot"),
        if option.selected { 1. } else { 0. },
        settle(),
        window,
        cx,
    );
    let focus = crate::motion::focus_ring::handle(&id, window, cx);
    let indicator = crate::motion::focus_ring::Ringed::new(
        div()
            .flex_none()
            .size(px(INDICATOR))
            .rounded_full()
            .border_1()
            .border_color(border)
            .bg(cx.theme().background)
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .size(px(INDICATOR / 2. * (0.4 + 0.6 * dot)))
                    .rounded_full()
                    .bg(primary.opacity(dot)),
            ),
        &focus,
        crate::motion::focus_ring::corners(px(INDICATOR / 2.)),
        cx,
    );
    let on_select = option.on_select.clone();
    let title_line = cx.theme().tokens.typography.md.line_height;
    Radio::new(id)
        .track_focus(&focus)
        .checked(option.selected)
        .disabled(disabled)
        .set_position(index + 1, size)
        .accessibility_label(option.title.clone())
        .when_some(on_select.filter(|_| !disabled), |radio, handler| {
            radio.on_change(move |_, _, window, cx| handler(window, cx))
        })
        // A radio cannot deselect itself, so gpui-base reports no change for
        // the selected one; the click still reaches the row.
        .when_some(
            option.on_select.filter(|_| option.selected && !disabled),
            |radio, handler| radio.on_click(move |_, window, cx| handler(window, cx)),
        )
        .h_flex()
        .w_full()
        .min_h(px(crate::material::TOUCH_TARGET))
        .px(px(crate::material::CARD_INSET))
        .py(px(10.))
        .gap(cx.theme().tokens.spacing.md)
        .items_start()
        .relative()
        .when(!disabled, |row| {
            row.cursor_pointer()
                .child(crate::material::row_hover(px(0.), cx))
        })
        .when(disabled, |row| row.opacity(0.5))
        .child(div().h(title_line).flex().items_center().child(indicator))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    gpui_base::h_flex()
                        .gap_2()
                        .items_center()
                        .child(div().min_w_0().text_body(compact, cx).child(option.title))
                        .children(option.badge),
                )
                .children(
                    option
                        .description
                        .map(|text| div().text_secondary(compact, cx).child(text)),
                ),
        )
        .into_any_element()
}
