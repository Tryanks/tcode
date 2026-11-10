use std::rc::Rc;

use crate::overlay::{TooltipBuilder, TooltipHost, TooltipOwner};
use crate::{theme::ActiveTheme as _, typography::TextRole as _};
use gpui::{
    AnyElement, App, Bounds, Element, ElementId, Entity, GlobalElementId, Hitbox, HitboxBehavior,
    InspectorElementId, IntoElement, LayoutId, MouseExitEvent, MouseMoveEvent, ParentElement,
    Pixels, SharedString, StatefulInteractiveElement, StyleRefinement, Styled, Window, canvas, div,
};
use gpui_base::StyledExt as _;

enum TooltipContent {
    Text(SharedString),
    Element(ElementBuilder),
}

type ElementBuilder = Box<dyn Fn(&mut Window, &mut App) -> AnyElement>;

/// A tooltip's content. It is shown through the window's tooltip layer by
/// [`TooltipTrigger::with_tooltip`] or [`tooltip_area`], which builds it when
/// it shows and keeps it drawn while it leaves.
pub struct Tooltip {
    style: StyleRefinement,
    content: TooltipContent,
}

impl Tooltip {
    pub fn new(text: impl Into<SharedString>) -> Self {
        Self {
            style: StyleRefinement::default(),
            content: TooltipContent::Text(text.into()),
        }
    }

    pub fn element<E, F>(builder: F) -> Self
    where
        E: IntoElement,
        F: Fn(&mut Window, &mut App) -> E + 'static,
    {
        Self {
            style: StyleRefinement::default(),
            content: TooltipContent::Element(Box::new(move |window, cx| {
                builder(window, cx).into_any_element()
            })),
        }
    }

    /// The popup at `progress` of its appear motion.
    pub(crate) fn surface(&self, progress: f32, window: &mut Window, cx: &mut App) -> AnyElement {
        crate::material::raised(gpui_base::Tooltip::new("tooltip-popup"))
            .h_flex()
            .relative()
            .top(crate::motion::lift(progress))
            .opacity(progress)
            .font_family(cx.theme().font_family.clone())
            .m_1()
            .bg(cx.theme().popover)
            .text_color(cx.theme().foreground)
            .border_1()
            .border_color(cx.theme().border)
            .shadow_md()
            .rounded(crate::material::radius_chip(cx))
            .py_0p5()
            .px_2()
            .text_token(&cx.theme().tokens.typography.sm)
            .refine_style(&self.style)
            .child(div().child(match &self.content {
                TooltipContent::Text(text) => text.clone().into_any_element(),
                TooltipContent::Element(builder) => builder(window, cx),
            }))
            .into_any_element()
    }
}

impl Styled for Tooltip {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

/// An element that shows a tooltip while the pointer rests on it.
pub trait TooltipTrigger: ParentElement + StatefulInteractiveElement + Sized {
    /// Shows `build`'s tooltip beside this element while the pointer rests on
    /// it, with gpui-base's show delay and grace period.
    fn with_tooltip(self, build: impl Fn(&mut Window, &mut App) -> Tooltip + 'static) -> Self {
        let build: TooltipBuilder = Rc::new(build);
        self.child(
            canvas(
                |bounds, window, cx| {
                    let owner = window.use_keyed_state("tcode-tooltip-trigger", cx, |_, _| {
                        TooltipOwner::default()
                    });
                    (owner, window.insert_hitbox(bounds, HitboxBehavior::Normal))
                },
                move |bounds, (owner, hitbox), window, cx| {
                    track(owner, hitbox, bounds, build, window, cx)
                },
            )
            // Without an inset, an absolute child starts after the padding but
            // is still sized to the padding box, overhanging the far edges.
            .absolute()
            .top_0()
            .left_0()
            .size_full(),
        )
    }
}

impl<E: ParentElement + StatefulInteractiveElement> TooltipTrigger for E {}

/// `child`, showing `build`'s tooltip while the pointer rests on it, for an
/// element that takes no children (an image).
pub fn tooltip_area(
    id: impl Into<ElementId>,
    child: impl IntoElement,
    build: impl Fn(&mut Window, &mut App) -> Tooltip + 'static,
) -> impl IntoElement {
    TooltipArea {
        id: id.into(),
        child: child.into_any_element(),
        build: Rc::new(build),
    }
}

struct TooltipArea {
    id: ElementId,
    child: AnyElement,
    build: TooltipBuilder,
}

impl IntoElement for TooltipArea {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TooltipArea {
    type RequestLayoutState = ();
    type PrepaintState = (Entity<TooltipOwner>, Hitbox);

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.child.prepaint(window, cx);
        let owner =
            window.use_keyed_state("tcode-tooltip-trigger", cx, |_, _| TooltipOwner::default());
        (owner, window.insert_hitbox(bounds, HitboxBehavior::Normal))
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        (owner, hitbox): &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
        track(
            owner.clone(),
            hitbox.clone(),
            bounds,
            self.build.clone(),
            window,
            cx,
        );
    }
}

/// A part of a trigger the pointer is over: which one, the bounds its
/// tooltip is placed beside, and the tooltip.
pub(crate) type TooltipPart = (usize, Bounds<Pixels>, TooltipBuilder);

fn track(
    owner: Entity<TooltipOwner>,
    hitbox: Hitbox,
    bounds: Bounds<Pixels>,
    build: TooltipBuilder,
    window: &mut Window,
    cx: &mut App,
) {
    track_parts(
        owner,
        move |window, cx| {
            (!cx.has_active_drag() && hitbox.is_hovered(window)).then(|| (0, bounds, build.clone()))
        },
        window,
        cx,
    );
}

/// Tell the tooltip layer which part of a trigger the pointer rests on, and
/// when it leaves, the way GPUI reports hover: on a pointer move, on the
/// pointer leaving the window, and when the trigger is drawn under (or away
/// from) a pointer that did not move. Call while painting the trigger.
pub(crate) fn track_parts(
    owner: Entity<TooltipOwner>,
    part_at: impl Fn(&Window, &App) -> Option<TooltipPart> + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    // A touch is hit-tested as a pointer at rest; see `motion::pointer_hover`.
    let part_at = move |window: &Window, cx: &App| {
        (!window.last_input_was_touch())
            .then(|| part_at(window, cx))
            .flatten()
    };
    let was = owner.read(cx).hovered.get();
    let update = Rc::new(
        move |part: Option<TooltipPart>, window: &mut Window, cx: &mut App| {
            let index = part.as_ref().map(|(index, _, _)| *index);
            if owner.read(cx).hovered.replace(index) == index {
                return;
            }
            let Some(host) = TooltipHost::of(window, cx) else {
                return;
            };
            host.update(cx, |host, cx| match part {
                Some((index, bounds, build)) => {
                    host.request_show(&owner, index, bounds, build, window, cx)
                }
                None => host.request_hide(&owner, window, cx),
            });
        },
    );
    let now = part_at(window, cx);
    if now.as_ref().map(|(index, _, _)| *index) != was {
        let update = update.clone();
        window.defer(cx, move |window, cx| update(now, window, cx));
    }
    window.on_mouse_event({
        let update = update.clone();
        move |_: &MouseMoveEvent, phase, window, cx| {
            if phase.bubble() {
                update(part_at(window, cx), window, cx);
            }
        }
    });
    window.on_mouse_event(move |_: &MouseExitEvent, phase, window, cx| {
        if phase.bubble() {
            update(None, window, cx);
        }
    });
}
