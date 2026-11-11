//! Tcode's single-choice form control: a styled composition of
//! `gpui_base::RadioGroup`/`Radio`, drawn as a
//! [`TabBar::segmented`](super::tabs::TabBar::segmented) track.
use crate::widgets::TooltipTrigger as _;
use crate::{
    icon::{Icon, IconName},
    theme::ActiveTheme as _,
    typography::TextRole as _,
    widgets::{
        Tooltip,
        tabs::{Glide, item_key, label_color, segment, segment_list, segmented_track},
    },
};
use gpui::{
    App, Axis, ElementId, InteractiveElement as _, IntoElement, ParentElement as _, RenderOnce,
    SharedString, StatefulInteractiveElement as _, StyleRefinement, Styled, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{Radio, RadioGroup, StyledExt as _};
use std::rc::Rc;

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

type SelectHandler = Rc<dyn Fn(&mut Window, &mut App)>;

/// One option of a [`ChoiceGroup`].
pub struct Choice {
    id: ElementId,
    label: SharedString,
    selected: bool,
    disabled: bool,
    recommended: bool,
    on_select: Option<SelectHandler>,
}

impl Choice {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            selected: false,
            disabled: false,
            recommended: false,
            on_select: None,
        }
    }
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
    /// Marks the option the provider or host recommends.
    pub fn recommended(mut self, recommended: bool) -> Self {
        self.recommended = recommended;
        self
    }
    /// Called when the user picks this option while another is selected.
    pub fn on_select(mut self, handler: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_select = Some(Rc::new(handler));
        self
    }
}

/// A row of mutually exclusive options. It carries no label of its own; the
/// form places one above it.
///
/// Its own styles place the group (width, flex, margins); the track owns
/// everything inside it.
#[derive(IntoElement)]
pub struct ChoiceGroup {
    id: ElementId,
    compact: bool,
    aria_label: Option<SharedString>,
    choices: Vec<Choice>,
    style: StyleRefinement,
}

impl ChoiceGroup {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            compact: false,
            aria_label: None,
            choices: Vec::new(),
            style: StyleRefinement::default(),
        }
    }
    /// Touch geometry, as [`TabBar::compact`](super::tabs::TabBar::compact).
    pub fn compact(mut self, compact: bool) -> Self {
        self.compact = compact;
        self
    }
    pub fn aria_label(mut self, label: impl Into<SharedString>) -> Self {
        self.aria_label = Some(label.into());
        self
    }
    pub fn choice(mut self, choice: Choice) -> Self {
        self.choices.push(choice);
        self
    }
    pub fn choices(mut self, choices: impl IntoIterator<Item = Choice>) -> Self {
        self.choices.extend(choices);
        self
    }
}

impl Styled for ChoiceGroup {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for ChoiceGroup {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            id,
            compact,
            aria_label,
            choices,
            style,
        } = self;
        let count = choices.len();
        let selected = choices.iter().position(|choice| choice.selected);
        let dim = selected.is_some_and(|ix| choices[ix].disabled);
        let glide = Glide::new(
            &id,
            selected.map(|ix| item_key(&choices[ix].id)),
            window,
            cx,
        );
        let radios: Vec<_> = choices
            .into_iter()
            .enumerate()
            .map(|(ix, choice)| render_choice(choice, ix, count, compact, &glide, window, cx))
            .collect();
        let group = RadioGroup::new(id.clone())
            .axis(Axis::Horizontal)
            .when_some(aria_label, |this, label| this.aria_label(label))
            .child(glide.pill(dim, cx))
            .children(radios)
            .relative();
        segmented_track(id.clone(), segment_list(group, cx), compact)
            .debug_selector(|| id.to_string())
            .refine_style(&style)
    }
}

fn render_choice(
    choice: Choice,
    ix: usize,
    count: usize,
    compact: bool,
    glide: &Glide,
    window: &mut Window,
    cx: &mut App,
) -> crate::motion::focus_ring::Ringed {
    let Choice {
        id,
        label,
        selected,
        disabled,
        recommended,
        on_select,
    } = choice;
    let (hovered, on_hover) = crate::motion::hovered((id.clone(), "hover"), window, cx);
    let lit = hovered && !selected && !disabled;
    let target = if selected || lit {
        cx.theme().foreground
    } else {
        cx.theme().muted_foreground
    };
    let fg = label_color(&id, "fg", target, window, cx);
    let opacity = gpui_base::motion::transition(
        (id.clone(), "dim"),
        if disabled { 0.5 } else { 1. },
        gpui_base::motion::Transition::new(GLIDE),
        window,
        cx,
    );
    let muted = cx.theme().muted;
    let hover_fill = label_color(
        &id,
        "hover-fill",
        if lit { muted } else { muted.opacity(0.) },
        window,
        cx,
    );
    // gpui-base's Radio keeps its own focus handle, leaving only GPUI's
    // `focus_visible`, which switches at once, to show the keyboard's focus: it
    // is handed this handle instead, and the ring fades in and out over it
    // (`motion::focus_ring`).
    let focus = crate::motion::focus_ring::handle(&id, window, cx);
    let accessible: SharedString = if recommended {
        crate::tr!("choice.recommended_option", label = label.clone())
            .into_owned()
            .into()
    } else {
        label.clone()
    };
    let selector = id.to_string();
    let key = item_key(&id);
    let radio = Radio::new(id)
        .debug_selector(move || selector)
        .checked(selected)
        .disabled(disabled)
        .set_position(ix + 1, count)
        .accessibility_label(accessible)
        .track_focus(&focus)
        .flex()
        .items_center()
        .px(px(if compact { 14. } else { 12. }))
        .gap(px(if compact { 8. } else { 6. }))
        .text_body(compact, cx)
        .font_medium()
        .whitespace_nowrap()
        .text_color(fg)
        .when(!disabled, |this| this.cursor_pointer())
        .on_hover(on_hover)
        .opacity(opacity)
        .when_some(on_select.filter(|_| !disabled), |this, handler| {
            this.on_change(move |_, _, window, cx| handler(window, cx))
        })
        .child(div().flex_none().child(super::fade_text(label)))
        .when(recommended, |this| {
            this.child(Icon::new(IconName::Star).flex_none())
                .with_tooltip(|_, _| Tooltip::new(crate::tr!("choice.recommended")))
        });
    let mut radio = glide.measure(key, segment(radio, hover_fill, compact, cx));
    let corners = radio.style().corner_radii.clone();
    crate::motion::focus_ring::Ringed::new(radio, &focus, corners, cx)
}
