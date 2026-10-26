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
pub(crate) fn cross_fade<T: Clone + PartialEq + 'static>(
    id: impl Into<ElementId>,
    value: T,
    window: &mut Window,
    cx: &mut App,
    build: impl Fn(&T, &mut Window, &mut App) -> AnyElement,
) -> AnyElement {
    let id = id.into();
    let state = window.use_keyed_state(child_id(&id, "cross-fade"), cx, |_, _| Swap {
        current: value.clone(),
        generation: 0,
        previous: None,
    });
    if state.read(cx).current != value {
        state.update(cx, |swap, _| {
            let current = std::mem::replace(&mut swap.current, value.clone());
            swap.previous = Some((current, swap.generation));
            swap.generation += 1;
        });
    }
    let swap = state.read(cx).clone();
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
                .sample(window, cx);
            if sample.phase == PresencePhase::Present {
                state.update(cx, |swap, _| swap.previous = None);
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
#[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
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

/// An inline element that comes and goes with `value`: mounted while its
/// [`appear`] sample should render, it keeps showing the last value it had
/// while it leaves. `None` once it is gone.
pub(crate) fn transient<T: Clone + PartialEq + 'static>(
    id: impl Into<ElementId>,
    value: Option<T>,
    window: &mut Window,
    cx: &mut App,
) -> Option<(T, Transient)> {
    let id = id.into();
    let sample = appear(id.clone(), value.is_some(), window, cx);
    let value = retain((id.clone(), "retained"), value, window, cx)?;
    sample
        .should_render()
        .then_some((value, Transient { id, sample }))
}

/// The present-or-leaving state of a [`transient`] element.
pub(crate) struct Transient {
    id: ElementId,
    sample: PresenceSample,
}

impl Transient {
    /// `child` faded and lifted at the sample's progress; a shield takes the
    /// pointer while it leaves.
    pub(crate) fn wrap(self, child: impl IntoElement) -> Stateful<Div> {
        let progress = self.sample.progress;
        div()
            .id(self.id)
            .relative()
            .top(lift(progress))
            .opacity(progress)
            .child(child)
            .when(self.sample.phase == PresencePhase::Exiting, |el| {
                el.child(shield())
            })
    }
}

/// `value`, or while it is `None` the last `Some` it had, so content that
/// leaves can still be drawn from it.
pub(crate) fn retain<T: Clone + PartialEq + 'static>(
    id: impl Into<ElementId>,
    value: Option<T>,
    window: &mut Window,
    cx: &mut App,
) -> Option<T> {
    let kept = window.use_keyed_state(id, cx, |_, _| None::<T>);
    match value {
        Some(value) => {
            if kept.read(cx).as_ref() != Some(&value) {
                kept.update(cx, |kept, _| *kept = Some(value.clone()));
            }
            Some(value)
        }
        None => kept.read(cx).clone(),
    }
}

/// Mutually exclusive states of one region (loading, loaded, failed) that
/// cross-fade: each state draws in its own layer, so its element tree holds
/// while it fades. The current layer is laid out; a leaving one is drawn
/// under it from the top, inert. With [`CrossFade::fill`] every layer fills
/// the region, which then needs a definite size.
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

/// Content that follows `value` and cross-fades when it changes: a status
/// line, a chip's label. Two layers take turns, the leaving one keeping the
/// value it was drawn from.
pub(crate) fn swap<T: Clone + PartialEq + 'static>(
    id: impl Into<ElementId>,
    value: T,
    window: &mut Window,
    cx: &mut App,
) -> SwapLayers<T> {
    let id = id.into();
    let slots = window.use_keyed_state((id.clone(), "slots"), cx, |_, _| SwapSlots {
        values: [Some(value.clone()), None],
        current: 0,
    });
    if slots.read(cx).values[slots.read(cx).current].as_ref() != Some(&value) {
        slots.update(cx, |slots, _| {
            slots.current ^= 1;
            slots.values[slots.current] = Some(value);
        });
    }
    let SwapSlots { values, current } = slots.read(cx).clone();
    SwapLayers {
        fade: CrossFade::new(id, current, 2, window, cx),
        values,
    }
}

#[derive(Clone)]
struct SwapSlots<T> {
    values: [Option<T>; 2],
    current: usize,
}

/// The two layers of a [`swap`].
pub(crate) struct SwapLayers<T> {
    fade: CrossFade,
    values: [Option<T>; 2],
}

impl<T> SwapLayers<T> {
    pub(crate) fn render(self, mut build: impl FnMut(T) -> AnyElement) -> Stateful<Div> {
        let fade = self.fade;
        let layers = self
            .values
            .into_iter()
            .enumerate()
            .filter(|(layer, _)| fade.shows(*layer))
            .filter_map(|(layer, value)| Some((layer, build(value?))))
            .collect::<Vec<_>>();
        fade.render(layers)
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
