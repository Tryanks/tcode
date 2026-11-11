use gpui::{
    AnyElement, App, Div, ElementId, IntoElement, ParentElement as _, RenderOnce, StyleRefinement,
    Styled, Window, div, prelude::FluentBuilder as _,
};

use crate::icon::{Icon, IconName};

/// How long a disclosure takes to fold open or shut, and its chevron to turn.
const FOLD: std::time::Duration = std::time::Duration::from_millis(200);

/// A disclosure's section: its content grows open and shrinks shut through
/// [`crate::motion::Shown::fold`], the one fold Tcode draws, and is not
/// built while it is shut.
///
/// gpui-base's `Collapsible` (0.9) shows and hides its content at once
/// unless its caller supplies a progress, and then reveals it through
/// `MotionReveal`, the reveal `Shown::fold` replaces (it draws an unmeasured
/// child at zero height, does not fade, and keeps the content built while
/// shut). Nor can it keep its clock with a view
/// that the dock moves under another parent, which [`Collapsible::motion`]
/// does.
#[derive(IntoElement)]
pub struct Collapsible {
    id: ElementId,
    open: bool,
    base: Div,
    content: Option<AnyElement>,
    /// Where its fold is kept when its view keeps it; in element state
    /// otherwise.
    motion: Option<crate::motion::owned::Owned>,
}

impl Collapsible {
    pub fn new(id: impl Into<ElementId>, open: bool) -> Self {
        Self {
            id: id.into(),
            open,
            base: div(),
            content: None,
            motion: None,
        }
    }

    pub fn content(mut self, content: impl IntoElement) -> Self {
        self.content = Some(content.into_any_element());
        self
    }

    /// The section with its fold kept by `motion`, under its id, so it
    /// carries on folding when its view is drawn under a new parent.
    pub(crate) fn motion(mut self, motion: &crate::motion::owned::Owned) -> Self {
        self.motion = Some(motion.clone());
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
        let open = self.open.then_some(());
        let fold = crate::motion::Clock::from(self.motion.as_ref()).shown(
            self.id,
            open,
            &crate::motion::Curve::new(FOLD),
            window,
            cx,
        );
        self.base
            .when_some(fold.zip(self.content), |base, (fold, content)| {
                base.child(fold.fold(content))
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
    /// Where its turn is kept when its view keeps it; in element state
    /// otherwise.
    motion: Option<crate::motion::owned::Owned>,
}

impl Chevron {
    pub fn new(id: impl Into<ElementId>, open: bool) -> Self {
        Self {
            id: id.into(),
            open,
            icon: Icon::new(IconName::ChevronRight),
            motion: None,
        }
    }

    /// The chevron with its turn kept by `motion`, under its id, so it
    /// carries on turning when its view is drawn under a new parent.
    pub(crate) fn owned(mut self, motion: &crate::motion::owned::Owned) -> Self {
        self.motion = Some(motion.clone());
        self
    }

    /// [`Self::icon`] with its turn kept by `motion`; see [`Self::owned`].
    pub(crate) fn owned_icon(
        motion: &crate::motion::owned::Owned,
        id: impl Into<ElementId>,
        open: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Icon {
        Self::new(id, open).owned(motion).turned(window, cx)
    }

    /// The chevron's icon as it stands this frame, for a control that draws
    /// its own icon (a button's).
    pub fn icon(id: impl Into<ElementId>, open: bool, window: &mut Window, cx: &mut App) -> Icon {
        Self::new(id, open).turned(window, cx)
    }

    fn turned(self, window: &mut Window, cx: &mut App) -> Icon {
        let target = if self.open { 1. } else { 0. };
        let turn = crate::motion::Clock::from(self.motion.as_ref()).transition(
            (self.id, "chevron"),
            target,
            &crate::motion::Curve::new(FOLD),
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
