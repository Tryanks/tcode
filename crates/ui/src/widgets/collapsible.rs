use gpui::{
    App, ElementId, IntoElement, RenderOnce, StyleRefinement, Styled, Window,
    prelude::FluentBuilder as _,
};
use gpui_base::motion::{Transition, transition};

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
