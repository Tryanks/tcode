use gpui::{
    App, ElementId, IntoElement, RenderOnce, StyleRefinement, Styled, Window,
    prelude::FluentBuilder as _,
};
use gpui_base::motion::{Transition, transition};

use crate::icon::{Icon, IconName};

/// A [`gpui_base::Collapsible`] whose content grows open and shrinks shut
/// over `SETTLE` instead of snapping.
#[derive(IntoElement)]
pub struct Collapsible {
    id: ElementId,
    open: bool,
    base: gpui_base::Collapsible,
}

impl Collapsible {
    pub fn new(id: impl Into<ElementId>, open: bool) -> Self {
        Self {
            id: id.into(),
            open,
            base: gpui_base::Collapsible::new(),
        }
    }

    pub fn content(mut self, content: impl IntoElement) -> Self {
        self.base = self.base.content(content);
        self
    }
}

impl Styled for Collapsible {
    fn style(&mut self) -> &mut StyleRefinement {
        self.base.style()
    }
}

impl RenderOnce for Collapsible {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let progress = transition(
            self.id.clone(),
            if self.open { 1. } else { 0. },
            Transition::new(crate::edge_back::SETTLE),
            window,
            cx,
        );
        // A shut section unmounts its content rather than lay it out, hidden,
        // every frame.
        self.base
            .open(self.open)
            .when(self.open || progress > 0., |base| {
                base.reveal(self.id, progress)
            })
    }
}

/// A disclosure chevron: it points right while its section is shut and turns
/// to point down as the section opens.
#[derive(IntoElement)]
pub struct Chevron {
    id: ElementId,
    open: bool,
    icon: Icon,
}

impl Chevron {
    pub fn new(id: impl Into<ElementId>, open: bool) -> Self {
        Self {
            id: id.into(),
            open,
            icon: Icon::new(IconName::ChevronRight),
        }
    }

    /// The chevron's icon as it stands this frame, for a control that draws
    /// its own icon (a button's).
    pub fn icon(id: impl Into<ElementId>, open: bool, window: &mut Window, cx: &mut App) -> Icon {
        Self::new(id, open).turned(window, cx)
    }

    fn turned(self, window: &mut Window, cx: &mut App) -> Icon {
        let turn = transition(
            (self.id, "chevron"),
            if self.open { 1. } else { 0. },
            Transition::new(crate::edge_back::SETTLE),
            window,
            cx,
        );
        self.icon
            .rotate(gpui::radians(turn * std::f32::consts::FRAC_PI_2))
    }
}

impl Styled for Chevron {
    fn style(&mut self) -> &mut StyleRefinement {
        self.icon.style()
    }

    fn text_color(mut self, color: impl Into<gpui::Hsla>) -> Self {
        self.icon = self.icon.text_color(color);
        self
    }
}

impl RenderOnce for Chevron {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        self.turned(window, cx)
    }
}
