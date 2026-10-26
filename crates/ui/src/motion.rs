use std::borrow::BorrowMut;

use gpui::{
    AnyElement, App, Div, ElementId, InteractiveElement as _, IntoElement, ParentElement as _,
    Pixels, Stateful, Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::motion::{
    MotionReveal, Presence, PresenceSample, Transition, TransitionId, transition,
};

pub(crate) use gpui_base::motion::PresencePhase;

/// How far a surface rises while it appears, and sinks while it leaves.
pub(crate) const LIFT: Pixels = px(6.);

/// Mount and unmount motion for a transient surface: a dialog, a popover,
/// a menu, a tooltip. The sample's `progress` drives opacity and
/// [`lift`]; the surface stays mounted while `should_render()`.
pub(crate) fn appear(
    id: impl Into<TransitionId>,
    present: bool,
    window: &mut Window,
    cx: &mut App,
) -> PresenceSample {
    Presence::new(id, present)
        .transition(Transition::new(crate::edge_back::SETTLE))
        .sample(window, cx)
}

/// The vertical offset at `progress`: [`LIFT`] away from rest when absent,
/// zero when present.
pub(crate) fn lift(progress: f32) -> Pixels {
    LIFT * (1. - progress.clamp(0., 1.))
}

/// The house transition: [`crate::edge_back::SETTLE`].
pub(crate) fn settle() -> Transition {
    Transition::new(crate::edge_back::SETTLE)
}

/// The house spring, for a position that follows the pointer or a reorder
/// and may be retargeted mid-flight: [`crate::edge_back::SETTLE`], at rest a
/// quarter pixel away.
pub(crate) const FOLLOW: gpui_base::motion::Spring =
    gpui_base::motion::Spring::new(crate::edge_back::SETTLE).with_epsilon(0.25);

fn child_id(id: &ElementId, channel: &'static str) -> ElementId {
    ElementId::NamedChild(std::sync::Arc::new(id.clone()), channel.into())
}

/// Something a page shows only some of the time, sampled by [`shown`]: what
/// to draw, and how far in it is.
pub(crate) struct Shown<T> {
    id: ElementId,
    pub value: T,
    progress: f32,
    leaving: bool,
}

/// Follow `value` in and out of a page over `SETTLE`. Once it is `None` the
/// last value it had is kept until it has gone, so whatever leaves is drawn
/// as it was. A page that opens with it shows it at rest. The caller draws
/// the result through [`Shown::fold`] or [`Shown::fade`] whenever this is
/// `Some`, at the same place in the tree, so the element keeps its ids while
/// it comes and goes.
pub(crate) fn shown<T: Clone + PartialEq + 'static>(
    id: impl Into<ElementId>,
    value: Option<T>,
    window: &mut Window,
    cx: &mut App,
) -> Option<Shown<T>> {
    let id = id.into();
    let progress = transition(
        child_id(&id, "shown"),
        if value.is_some() { 1. } else { 0. },
        settle(),
        window,
        cx,
    );
    let kept = window.use_keyed_state(child_id(&id, "shown-value"), cx, |_, _| None::<T>);
    match value {
        Some(value) => {
            if kept.read(cx).as_ref() != Some(&value) {
                kept.update(cx, |kept, _| *kept = Some(value.clone()));
            }
            Some(Shown {
                id,
                value,
                progress,
                leaving: false,
            })
        }
        None if progress > 0. => kept.read(cx).clone().map(|value| Shown {
            id,
            value,
            progress,
            leaving: true,
        }),
        None => {
            if kept.read(cx).is_some() {
                kept.update(cx, |kept, _| *kept = None);
            }
            None
        }
    }
}

impl<T> Shown<T> {
    /// How far in it is, from absent to at rest.
    pub(crate) fn progress(&self) -> f32 {
        self.progress
    }

    /// Whether it is on its way out, drawn from the last value it had.
    pub(crate) fn leaving(&self) -> bool {
        self.leaving
    }

    /// In a column: grows to its height and fades in, shrinks and fades out.
    pub(crate) fn fold(&self, content: impl IntoElement) -> AnyElement {
        MotionReveal::new(
            child_id(&self.id, "fold"),
            self.progress,
            self.frame(content).w_full().into_any_element(),
        )
        .into_any_element()
    }

    /// Beside other content in a row: fades and rises in place, as
    /// [`appear`] does, without folding the row's height.
    pub(crate) fn fade(&self, content: impl IntoElement) -> AnyElement {
        self.frame(content)
            .flex_none()
            .top(lift(self.progress))
            .into_any_element()
    }

    /// What is leaving takes no clicks.
    fn frame(&self, content: impl IntoElement) -> Div {
        div()
            .relative()
            .opacity(self.progress)
            .child(content)
            .when(self.leaving, |frame| {
                frame.child(div().absolute().inset_0().occlude())
            })
    }
}

#[derive(Clone)]
struct Swap<T> {
    current: T,
    generation: u64,
    previous: Option<(T, u64)>,
}

/// Content that changes to another of its states — a status, a QR for a new
/// link — cross-fading from the one it showed. Each state is drawn under
/// its own slot, which it keeps while it fades out on top of the next.
/// `cx` is the caller's own context, so a view's `build` can make listeners.
pub(crate) fn cross_fade<T: Clone + PartialEq + 'static, C: BorrowMut<App>>(
    id: impl Into<ElementId>,
    value: T,
    window: &mut Window,
    cx: &mut C,
    mut build: impl FnMut(&T, &mut Window, &mut C) -> AnyElement,
) -> AnyElement {
    let id = id.into();
    let app: &mut App = cx.borrow_mut();
    let state = window.use_keyed_state(child_id(&id, "cross-fade"), app, |_, _| Swap {
        current: value.clone(),
        generation: 0,
        previous: None,
    });
    if state.read(app).current != value {
        state.update(app, |swap, _| {
            let current = std::mem::replace(&mut swap.current, value.clone());
            swap.previous = Some((current, swap.generation));
            swap.generation += 1;
        });
    }
    let swap = state.read(app).clone();
    let slot = |generation: u64| {
        ElementId::NamedChild(
            std::sync::Arc::new(id.clone()),
            generation.to_string().into(),
        )
    };
    let progress = match swap.previous {
        Some(_) => {
            let sample = gpui_base::motion::Presence::new(slot(swap.generation), true)
                .transition(settle())
                .sample(window, app);
            if sample.phase == PresencePhase::Present {
                state.update(app, |swap, _| swap.previous = None);
                1.
            } else {
                sample.progress
            }
        }
        None => 1.,
    };
    let current = build(&swap.current, window, cx);
    let previous = swap
        .previous
        .filter(|_| progress < 1.)
        .map(|(value, generation)| (build(&value, window, cx), generation));
    div()
        .relative()
        .child(
            div()
                .id(slot(swap.generation))
                .opacity(progress)
                .child(current),
        )
        .when_some(previous, |frame, (previous, generation)| {
            frame.child(
                div()
                    .id(slot(generation))
                    .absolute()
                    .top_0()
                    .left_0()
                    // As wide as the frame at least, so a state that fills its
                    // row keeps filling it while it leaves.
                    .min_w_full()
                    .opacity(1. - progress)
                    .child(previous)
                    .child(div().absolute().inset_0().occlude()),
            )
        })
        .into_any_element()
}

/// A hover fill that glides over `SETTLE`: the fill to paint now, and the
/// handler for the element's `on_hover` that moves it. Pressing has no
/// fill and stays immediate.
pub(crate) fn hover_fill(
    id: impl Into<ElementId>,
    hover: gpui::Hsla,
    window: &mut Window,
    cx: &mut App,
) -> (gpui::Hsla, impl Fn(&bool, &mut Window, &mut App) + 'static) {
    let id = id.into();
    let hovered = window.use_keyed_state(child_id(&id, "hovered"), cx, |_, _| false);
    let fill = transition(
        child_id(&id, "hover-fill"),
        if *hovered.read(cx) {
            hover
        } else {
            hover.opacity(0.)
        },
        settle(),
        window,
        cx,
    );
    let on_hover = move |is_hovered: &bool, _: &mut Window, cx: &mut App| {
        hovered.update(cx, |hovered, cx| {
            if *hovered != *is_hovered {
                *hovered = *is_hovered;
                cx.notify();
            }
        });
    };
    (fill, on_hover)
}

/// Mutually exclusive states of one region (loading, loaded, failed) that
/// cross-fade: each state draws in its own layer, so its element tree holds
/// while it fades. The current layer is laid out; a leaving one is drawn
/// under it from the top, inert. With [`CrossFade::fill`] every layer fills
/// the region, which then needs a definite size. Unlike [`cross_fade`],
/// which gives each new value a fresh slot, a state keeps its layer however
/// its content changes, so a list's scroll or a page's inputs hold; and only
/// the layers on screen are built.
pub(crate) struct CrossFade {
    id: ElementId,
    current: usize,
    opacity: Vec<f32>,
    fill: bool,
}

impl CrossFade {
    pub(crate) fn new(
        id: impl Into<ElementId>,
        current: usize,
        count: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        let id = id.into();
        let opacity = (0..count)
            .map(|layer| {
                transition(
                    (id.clone(), format!("layer-{layer}")),
                    if layer == current { 1. } else { 0. },
                    Transition::new(crate::edge_back::SETTLE),
                    window,
                    cx,
                )
            })
            .collect();
        Self {
            id,
            current,
            opacity,
            fill: false,
        }
    }

    pub(crate) fn fill(mut self) -> Self {
        self.fill = true;
        self
    }

    /// Whether `layer` is drawn this frame, so only those are built.
    pub(crate) fn shows(&self, layer: usize) -> bool {
        layer == self.current || self.opacity.get(layer).is_some_and(|opacity| *opacity > 0.)
    }

    /// The region holding `layers`, each tagged with its index; layers that
    /// are not [`shown`](Self::shows) are skipped.
    pub(crate) fn render(
        self,
        layers: impl IntoIterator<Item = (usize, AnyElement)>,
    ) -> Stateful<Div> {
        let mut layers: Vec<_> = layers
            .into_iter()
            .filter(|(layer, _)| self.shows(*layer))
            .collect();
        // The current layer is drawn last, over the leaving ones' shields.
        layers.sort_by_key(|(layer, _)| *layer == self.current);
        let fill = self.fill;
        div()
            .id(self.id.clone())
            .relative()
            .children(layers.into_iter().map(|(layer, content)| {
                let leaving = layer != self.current;
                div()
                    .id((self.id.clone(), format!("layer-{layer}")))
                    .opacity(self.opacity[layer])
                    .map(|el| {
                        if fill {
                            el.absolute().inset_0()
                        } else if leaving {
                            el.absolute().top_0().left_0()
                        } else {
                            el
                        }
                    })
                    .child(content)
                    .when(leaving, |el| el.child(shield()))
            }))
    }
}

/// Covers a leaving element so it takes no pointer, wheel or focus.
fn shield() -> Div {
    div()
        .absolute()
        .inset_0()
        .occlude()
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
}

/// A status glyph: its colour glides and a new glyph cross-fades over the
/// one it replaces.
pub(crate) fn status_icon(
    id: impl Into<ElementId>,
    icon: crate::icon::IconName,
    color: gpui::Hsla,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    use crate::icon::IconNamed as _;
    let id = id.into();
    let color = transition(child_id(&id, "color"), color, settle(), window, cx);
    cross_fade(
        child_id(&id, "glyph"),
        icon.path(),
        window,
        cx,
        move |path, _, _| {
            crate::icon::Icon::empty()
                .path(path.clone())
                .text_color(color)
                .into_any_element()
        },
    )
}

/// An element built when it renders, for a helper that holds only `&App`
/// and so cannot reach the window a transition samples.
#[derive(IntoElement)]
pub(crate) struct Sampled(Build);

type Build = Box<dyn FnOnce(&mut Window, &mut App) -> AnyElement>;

impl Sampled {
    pub(crate) fn new(build: impl FnOnce(&mut Window, &mut App) -> AnyElement + 'static) -> Self {
        Self(Box::new(build))
    }
}

impl gpui::RenderOnce for Sampled {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        (self.0)(window, cx)
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use gpui::{
        Context, InteractiveElement as _, IntoElement, ParentElement as _, Render,
        StatefulInteractiveElement as _, Styled as _, TestAppContext, VisualTestContext, Window,
        div, px,
    };

    use super::CrossFade;

    struct States {
        current: usize,
        clicks: Rc<Cell<usize>>,
    }

    impl Render for States {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let fade = CrossFade::new("states", self.current, 2, window, cx);
            let clicks = self.clicks.clone();
            let mut layers = Vec::new();
            if fade.shows(0) {
                layers.push((
                    0,
                    div()
                        .id("first")
                        .debug_selector(|| "first".into())
                        .size(px(40.))
                        .on_click(move |_, _, _| clicks.set(clicks.get() + 1))
                        .into_any_element(),
                ));
            }
            if fade.shows(1) {
                layers.push((
                    1,
                    div()
                        .debug_selector(|| "second".into())
                        .size(px(10.))
                        .into_any_element(),
                ));
            }
            div().size_full().child(fade.render(layers))
        }
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            _ = window.draw(cx);
        });
    }

    #[gpui::test]
    fn a_leaving_state_stays_drawn_inert_until_its_fade_has_played(cx: &mut TestAppContext) {
        let clicks = Rc::new(Cell::new(0));
        let (states, cx) = cx.add_window_view(|_, _| States {
            current: 0,
            clicks: clicks.clone(),
        });
        draw(cx);
        let first = cx.debug_bounds("first").expect("current state");
        cx.simulate_click(first.center(), Default::default());
        assert_eq!(clicks.get(), 1, "the current state takes clicks");

        states.update(cx, |states, cx| {
            states.current = 1;
            cx.notify();
        });
        draw(cx);
        assert!(cx.debug_bounds("second").is_some());
        let leaving = cx.debug_bounds("first").expect("a leaving state is drawn");
        cx.simulate_click(leaving.center(), Default::default());
        assert_eq!(clicks.get(), 1, "a leaving state takes no clicks");

        cx.executor().advance_clock(crate::edge_back::SETTLE);
        draw(cx);
        assert!(cx.debug_bounds("first").is_none(), "gone once faded out");
        assert!(cx.debug_bounds("second").is_some());
    }
}
