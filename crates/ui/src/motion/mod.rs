use std::borrow::BorrowMut;

use gpui::{
    AnyElement, App, Axis, Div, ElementId, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, Size, Stateful, StatefulInteractiveElement as _, Styled as _,
    Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::motion::{Presence, PresenceSample, Transition, TransitionId, transition};

pub(crate) use gpui_base::motion::PresencePhase;
pub(crate) use scene::Scene;

mod scene;
pub(crate) mod shared;

pub(crate) mod focus_ring;

pub(crate) mod drag_ghost;
mod image_fade;
pub(crate) mod keyed;
mod list_flip;
mod list_hover;
pub(crate) mod list_rows;
mod list_scroll;
mod size_glide;

pub(crate) use image_fade::{image_arrival, image_arrival_owned, image_load};
pub(crate) use keyed::{keyed, keyed_across};
pub(crate) use list_flip::ListFlip;
pub(crate) use list_hover::ListHover;
pub(crate) use list_scroll::{ListScroll, ScrollTarget};
pub(crate) use size_glide::{GlideAxes, GlideState, SizeGlide, size_key};
pub(crate) mod bounce;
#[cfg(any(
    test,
    all(preview_backend, any(target_os = "macos", target_os = "ios"))
))]
pub(crate) mod native_fade;
pub(crate) mod reflow;
pub(crate) use reflow::reflow;
pub(crate) mod resize_glide;

pub(crate) mod owned;

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

/// How long a list a keystroke refilters takes to fold its rows in and out:
/// short enough to keep pace with typing.
pub(crate) const KEYSTROKE: std::time::Duration = std::time::Duration::from_millis(120);

/// The house transition: [`crate::edge_back::SETTLE`].
pub(crate) fn settle() -> Transition {
    Transition::new(crate::edge_back::SETTLE)
}

/// How far [`settle`] has come `elapsed` after it started, from 0 to 1, for
/// motion that keeps its own clock: its easing over its duration.
pub(crate) fn settle_progress(elapsed: std::time::Duration) -> f32 {
    gpui_base::motion::Timing::new(crate::edge_back::SETTLE)
        .ease(gpui_base::motion::Easing::Custom(std::rc::Rc::new(
            gpui_base::animation::ease_out_cubic,
        )))
        .sample(elapsed)
        .directed_progress
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
#[derive(PartialEq)]
pub(crate) struct Shown<T> {
    id: ElementId,
    pub value: T,
    progress: f32,
    leaving: bool,
    /// Where its fold keeps its measurement when an owner holds it
    /// ([`owned::Owned::shown`]); in element state otherwise.
    measure: Option<Measure>,
}

/// A fold's measured size, kept by an owner rather than in element state, so
/// a fold under way carries on when its element is drawn under a new parent.
pub(crate) type Measure = std::rc::Rc<std::cell::Cell<Option<Size<Pixels>>>>;

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
                measure: None,
            })
        }
        None if progress > 0. => kept.read(cx).clone().map(|value| Shown {
            id,
            value,
            progress,
            leaving: true,
            measure: None,
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
        Reveal {
            id: child_id(&self.id, "fold"),
            progress: self.progress,
            measure: self.measure.clone(),
            axis: Axis::Vertical,
            gap: px(0.),
            child: self.frame(content).w_full().into_any_element(),
        }
        .into_any_element()
    }

    /// Beside other content in a row: grows to its width and fades in,
    /// shrinks and fades out, so what is beside it moves over as it does.
    pub(crate) fn fold_across(&self, content: impl IntoElement) -> AnyElement {
        self.fold_across_gap(px(0.), content)
    }

    /// [`Self::fold_across`] in a row with a `gap` between its children: the
    /// gap before it comes and goes with it, so nothing beside it jumps by
    /// the gap as it starts or finishes. At rest it shrinks with the row as
    /// its content would.
    pub(crate) fn fold_across_gap(&self, gap: Pixels, content: impl IntoElement) -> AnyElement {
        Reveal {
            id: child_id(&self.id, "fold-across"),
            progress: self.progress,
            measure: self.measure.clone(),
            axis: Axis::Horizontal,
            gap,
            child: self.frame(content).min_w_0().into_any_element(),
        }
        .into_any_element()
    }

    /// [`Self::fold_across`] for content that must keep `min` of its width
    /// at rest however narrow its row gets (a picker that must stay
    /// pressable): the row shrinks it to `min` and no further, so what is
    /// beside it never comes to lie over it.
    pub(crate) fn fold_across_min(&self, min: Pixels, content: impl IntoElement) -> AnyElement {
        Reveal {
            id: child_id(&self.id, "fold-across"),
            progress: self.progress,
            measure: self.measure.clone(),
            axis: Axis::Horizontal,
            gap: px(0.),
            child: self.frame(content).min_w(min).into_any_element(),
        }
        .into_any_element()
    }

    /// In a row that divides its width among its children (a segmented
    /// track): its share grows from nothing and fades in, and gives itself
    /// back to the others as it fades out, with the `gap` before it. At
    /// rest it takes a whole share, and no less than its content.
    pub(crate) fn fold_share(&self, gap: Pixels, content: impl IntoElement) -> AnyElement {
        let progress = self.progress.clamp(0., 1.);
        self.frame(content)
            .flex()
            .flex_grow(progress)
            .flex_basis(px(0.))
            .flex_shrink_0()
            .when(progress < 1., |frame| {
                frame.min_w_0().overflow_hidden().ml(-gap * (1. - progress))
            })
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

/// A fold's clipped, measured height, or width.
///
/// gpui-base's `MotionReveal` lays a child it has not measured yet out at
/// zero height, so a section open on the frame that first draws it (a
/// message's actions, an open disclosure) comes in a frame late and moves
/// what is below; and it folds only heights. This one lays its child out as
/// itself while it is at rest or unmeasured, and clips it to its measurement
/// scaled by the progress only while it folds. Its child keeps its place in
/// the tree in both.
struct Reveal {
    id: ElementId,
    progress: f32,
    /// Its measurement, when an owner keeps it.
    measure: Option<Measure>,
    /// The extent it folds: height in a column, width in a row.
    axis: Axis,
    /// In a row, the gap before it that it brings and takes away.
    gap: Pixels,
    child: AnyElement,
}

/// How a fold lays its content out this frame.
pub(crate) enum RevealLayout {
    /// The content is the element's layout.
    Plain,
    /// The element is laid out at its own size; the content is laid out on
    /// its own and clipped to it.
    Clipped,
}

impl IntoElement for Reveal {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl gpui::Element for Reveal {
    type RequestLayoutState = RevealLayout;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_id: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, RevealLayout) {
        let global_id = global_id.expect("Reveal has an id");
        let measured = window.with_element_state(global_id, |size: Option<Size<Pixels>>, _| {
            (size, size.unwrap_or_default())
        });
        let measured = self
            .measure
            .as_ref()
            .and_then(|measure| measure.get())
            .or(measured);
        let folded = |size: Size<Pixels>, progress: f32| {
            let mut style = gpui::Style::default();
            match self.axis {
                Axis::Vertical => {
                    style.size.width = gpui::relative(1.).into();
                    style.size.height = (size.height * progress.clamp(0., 1.)).into();
                }
                Axis::Horizontal => {
                    let progress = progress.clamp(0., 1.);
                    // It shrinks with the row as its content would at rest.
                    style.min_size.width = px(0.).into();
                    let width = size.width * progress;
                    let pull = self.gap * (1. - progress);
                    // Taffy sizes a row by its children's widths and leaves
                    // out the margin of a child with no width, so a fold
                    // pulling its gap back at (almost) no width would widen
                    // a row sized by its children by the gap. It keeps a
                    // pixel of width, pulled back with the gap.
                    let lead = if pull > px(0.) {
                        (px(1.) - width).max(px(0.))
                    } else {
                        px(0.)
                    };
                    style.size.width = (width + lead).into();
                    style.size.height = size.height.into();
                    style.margin.left = (-(pull + lead)).into();
                }
            }
            style
        };
        match measured {
            Some(size) if self.progress < 1. => (
                window.request_layout(folded(size, self.progress), None, cx),
                RevealLayout::Clipped,
            ),
            None if self.progress <= 0. => (
                window.request_layout(folded(Size::default(), 0.), None, cx),
                RevealLayout::Clipped,
            ),
            _ => (self.child.request_layout(window, cx), RevealLayout::Plain),
        }
    }

    fn prepaint(
        &mut self,
        global_id: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: gpui::Bounds<Pixels>,
        layout: &mut RevealLayout,
        window: &mut Window,
        cx: &mut App,
    ) {
        let global_id = global_id.expect("Reveal has an id");
        let size = match layout {
            RevealLayout::Plain => {
                self.child.prepaint(window, cx);
                bounds.size
            }
            RevealLayout::Clipped => {
                let space = match self.axis {
                    Axis::Vertical => gpui::size(
                        gpui::AvailableSpace::Definite(bounds.size.width),
                        gpui::AvailableSpace::MinContent,
                    ),
                    Axis::Horizontal => gpui::size(
                        gpui::AvailableSpace::MaxContent,
                        gpui::AvailableSpace::MinContent,
                    ),
                };
                let size = self.child.layout_as_root(space, window, cx);
                window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
                    self.child.prepaint_at(bounds.origin, window, cx);
                });
                size
            }
        };
        let changed = window.with_element_state(global_id, |measured: Option<Size<Pixels>>, _| {
            (measured != Some(size), size)
        });
        let changed = match &self.measure {
            Some(measure) => measure.replace(Some(size)) != Some(size),
            None => changed,
        };
        if changed && matches!(layout, RevealLayout::Clipped) {
            window.request_animation_frame();
        }
    }

    fn paint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: gpui::Bounds<Pixels>,
        layout: &mut RevealLayout,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        match layout {
            RevealLayout::Plain => self.child.paint(window, cx),
            RevealLayout::Clipped => {
                window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
                    self.child.paint(window, cx);
                });
            }
        }
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
    build: impl FnMut(&T, &mut Window, &mut C) -> AnyElement,
) -> AnyElement {
    swap(id.into(), value, true, window, cx, build)
}

/// [`cross_fade`] for a part of a control — its glyph, its status — which
/// takes no input of its own: what leaves covers nothing, so a press during
/// the fade still reaches the control.
pub(crate) fn cross_fade_part<T: Clone + PartialEq + 'static, C: BorrowMut<App>>(
    id: impl Into<ElementId>,
    value: T,
    window: &mut Window,
    cx: &mut C,
    build: impl FnMut(&T, &mut Window, &mut C) -> AnyElement,
) -> AnyElement {
    swap(id.into(), value, false, window, cx, build)
}

fn swap<T: Clone + PartialEq + 'static, C: BorrowMut<App>>(
    id: ElementId,
    value: T,
    shield: bool,
    window: &mut Window,
    cx: &mut C,
    mut build: impl FnMut(&T, &mut Window, &mut C) -> AnyElement,
) -> AnyElement {
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
    draw_change(id, (current, swap.generation), previous, progress, shield)
}

/// A [`cross_fade`]'s frame: the `current` state, of its generation, coming
/// in at `progress` over the `previous` one, which is shielded when
/// `shield`.
fn draw_change(
    id: ElementId,
    (current, generation): (AnyElement, u64),
    previous: Option<(AnyElement, u64)>,
    progress: f32,
    shield: bool,
) -> AnyElement {
    let slot = |generation: u64| {
        ElementId::NamedChild(
            std::sync::Arc::new(id.clone()),
            generation.to_string().into(),
        )
    };
    div()
        .relative()
        .child(div().id(slot(generation)).opacity(progress).child(current))
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
                    .when(shield, |previous| {
                        previous.child(div().absolute().inset_0().occlude())
                    }),
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
    hover_fill_over(id, hover.opacity(0.), hover, window, cx)
}

/// [`hover_fill`] from `rest`, the fill the element has while the pointer
/// is away (a highlight, or nothing); a change of `rest` glides too.
fn hover_fill_over(
    id: impl Into<ElementId>,
    rest: gpui::Hsla,
    hover: gpui::Hsla,
    window: &mut Window,
    cx: &mut App,
) -> (gpui::Hsla, impl Fn(&bool, &mut Window, &mut App) + 'static) {
    let id = id.into();
    let (hovered, on_hover) = hovered(id.clone(), window, cx);
    let fill = transition(
        child_id(&id, "hover-fill"),
        if hovered { hover } else { rest },
        settle(),
        window,
        cx,
    );
    (fill, on_hover)
}

/// Whether the pointer is over the element `id`, as its `on_hover` last
/// heard, and that handler: for an element whose hover moves more than a
/// fill (a label's colour too), each part gliding to its hovered value.
pub(crate) fn hovered<I: Into<ElementId>>(
    id: I,
    window: &mut Window,
    cx: &mut App,
) -> (
    bool,
    impl Fn(&bool, &mut Window, &mut App) + 'static + use<I>,
) {
    let hovered = window.use_keyed_state(child_id(&id.into(), "hovered"), cx, |_, _| false);
    let now = *hovered.read(cx);
    let on_hover = move |is_hovered: &bool, _: &mut Window, cx: &mut App| {
        hovered.update(cx, |hovered, cx| {
            if *hovered != *is_hovered {
                *hovered = *is_hovered;
                cx.notify();
            }
        });
    };
    (now, on_hover)
}

/// The fill of an item built with `&App` only (a menu item, a selectable
/// row), for its frame's first child, under the item's content: [`hover_fill`]
/// over `rest`, rounded by `radius`, sampled when the item renders and kept
/// under the item's id. It hears the pointer through its own hitbox, which
/// covers the item and leaves the pointer to the item and its content, so
/// the item's own `on_hover` stays the caller's. The item must be
/// positioned and have an id.
pub(crate) fn hover_layer(rest: gpui::Hsla, hover: gpui::Hsla, radius: Pixels) -> Sampled {
    Sampled::new(move |window, cx| {
        let (fill, on_hover) = hover_fill_over("hover-layer", rest, hover, window, cx);
        div()
            .id("hover-layer")
            .absolute()
            .inset_0()
            .rounded(radius)
            .bg(fill)
            .on_hover(on_hover)
            .into_any_element()
    })
}

/// [`hover_layer`] with its fill's clock kept by `motion` under `key`, for
/// an item its view draws under another parent (a list on a page the window
/// re-parents as it turns compact or wide), whose fill would otherwise start
/// over there. Whether the pointer is over it is still heard where it is
/// drawn: the pointer moving is what says so.
pub(crate) fn hover_layer_owned(
    motion: &owned::Owned,
    key: gpui::SharedString,
    rest: gpui::Hsla,
    hover: gpui::Hsla,
    radius: Pixels,
) -> Sampled {
    let motion = motion.clone();
    Sampled::new(move |window, cx| {
        let (hovered, on_hover) = hovered("hover-layer", window, cx);
        let fill = motion.transition(
            format!("{key}/hover-fill"),
            if hovered { hover } else { rest },
            &owned::Curve::settle(),
            window,
            cx,
        );
        div()
            .id("hover-layer")
            .absolute()
            .inset_0()
            .rounded(radius)
            .bg(fill)
            .on_hover(on_hover)
            .into_any_element()
    })
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
    span: bool,
    column: bool,
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
            span: false,
            column: false,
        }
    }

    /// Layers at the opacities their owner sampled from a clock it keeps,
    /// for a region whose fade must stay in step with other motion that
    /// clock drives, and whose owner decides when a change snaps.
    pub(crate) fn sampled(id: impl Into<ElementId>, current: usize, opacity: Vec<f32>) -> Self {
        Self {
            id: id.into(),
            current,
            opacity,
            fill: false,
            span: false,
            column: false,
        }
    }

    pub(crate) fn fill(mut self) -> Self {
        self.fill = true;
        self
    }

    /// A leaving layer keeps the region's width, so content that sizes
    /// itself to the width it is given stays as it was while it leaves.
    pub(crate) fn span(mut self) -> Self {
        self.span = true;
        self
    }

    /// The region is a column its owner bounds (a popover's capped height)
    /// and the current layer shrinks to it, so a list in it scrolls within
    /// the room rather than overflowing it.
    pub(crate) fn column(mut self) -> Self {
        self.column = true;
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
        let (fill, span, column) = (self.fill, self.span, self.column);
        div()
            .id(self.id.clone())
            .relative()
            .when(column, |region| region.flex().flex_col().min_h_0())
            .children(layers.into_iter().map(|(layer, content)| {
                let leaving = layer != self.current;
                div()
                    .id((self.id.clone(), format!("layer-{layer}")))
                    .opacity(self.opacity[layer])
                    .map(|el| {
                        if fill {
                            el.absolute().inset_0()
                        } else if leaving {
                            el.absolute().top_0().left_0().when(span, |el| el.right_0())
                        } else if column {
                            el.flex().flex_col().flex_1().min_h_0()
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
pub(crate) fn shield() -> Div {
    div()
        .absolute()
        .inset_0()
        .occlude()
        .on_scroll(|_, _, cx| cx.stop_propagation())
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
    cross_fade_part(
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

    /// A section that folds under a header, holding a counter its own clicks
    /// keep in element state.
    struct Section {
        open: bool,
    }

    impl Render for Section {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let body = super::shown("section", self.open.then_some(()), window, cx).map(|fold| {
                let clicks = window.use_keyed_state("section-clicks", cx, |_, _| 0usize);
                let count = *clicks.read(cx);
                fold.fold(
                    div()
                        .id("section-body")
                        .debug_selector(move || format!("section-body-{count}"))
                        .h(px(80.))
                        .on_click(move |_, _, cx| clicks.update(cx, |count, _| *count += 1)),
                )
            });
            div()
                .w(px(200.))
                .flex()
                .flex_col()
                .children(body)
                .child(div().debug_selector(|| "below".into()).h(px(10.)))
        }
    }

    /// A fold open when first drawn takes its height on that frame, so what
    /// is below does not move a frame later. Shutting, it is drawn leaving and
    /// takes no clicks; opened again before it has gone, it is the same
    /// section, its state intact. With motion reduced it goes at once.
    #[gpui::test]
    fn a_fold_is_whole_when_first_drawn_and_inert_while_it_leaves(cx: &mut TestAppContext) {
        use crate::test_frames::{draw, draw_settled};
        // Opening the window draws its first frame.
        let (section, cx) = cx.add_window_view(|_, _| Section { open: true });
        let below = cx.debug_bounds("below").expect("below the section");
        assert_eq!(below.top(), px(80.), "the open section has its height");

        let body = cx.debug_bounds("section-body-0").expect("the open section");
        cx.simulate_click(body.center(), Default::default());
        draw(cx);
        assert!(
            cx.debug_bounds("section-body-1").is_some(),
            "a click counts"
        );

        let set = |open: bool, cx: &mut VisualTestContext| {
            section.update(cx, |section, cx| {
                section.open = open;
                cx.notify();
            });
            draw(cx);
        };
        set(false, cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
        draw(cx);
        let leaving = cx
            .debug_bounds("section-body-1")
            .expect("drawn while it leaves");
        let below = cx.debug_bounds("below").unwrap();
        assert!(below.top() > px(0.) && below.top() < px(80.), "it folds");
        cx.simulate_click(leaving.center(), Default::default());
        set(true, cx);
        draw_settled(cx);
        assert!(
            cx.debug_bounds("section-body-1").is_some(),
            "the leaving section took a click, or came back as another"
        );

        cx.update(|_, cx| cx.set_reduce_motion(true));
        set(false, cx);
        assert!(cx.debug_bounds("section-body-1").is_none());
        assert_eq!(cx.debug_bounds("below").unwrap().top(), px(0.));
    }

    /// A chip between two neighbours in a row with a gap.
    struct Pill {
        chip: bool,
        clicks: Rc<Cell<usize>>,
    }

    impl Render for Pill {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let gap = px(8.);
            let clicks = self.clicks.clone();
            let chip = super::shown("pill-chip", self.chip.then_some(()), window, cx).map(|chip| {
                chip.fold_across_gap(
                    gap,
                    div()
                        .id("chip")
                        .debug_selector(|| "chip".into())
                        .w(px(50.))
                        .h(px(20.))
                        .on_click(move |_, _, _| clicks.set(clicks.get() + 1)),
                )
            });
            // A row as wide as its children, at the right end of its parent,
            // so its own width follows what the chip takes.
            div().w(px(400.)).flex().justify_end().child(
                div()
                    .flex()
                    .flex_row()
                    .flex_none()
                    .gap(gap)
                    .child(
                        div()
                            .debug_selector(|| "before".into())
                            .w(px(30.))
                            .h(px(20.)),
                    )
                    .children(chip)
                    .child(
                        div()
                            .debug_selector(|| "after".into())
                            .w(px(30.))
                            .h(px(20.)),
                    ),
            )
        }
    }

    /// A chip that comes into a row widens into it, gap and all, so the
    /// neighbour before it moves aside one way without a jump while the one
    /// after it holds the row's end; going, it narrows out the same way and
    /// takes no clicks.
    #[gpui::test]
    fn a_chip_widens_into_its_row_and_narrows_out(cx: &mut TestAppContext) {
        use crate::test_frames::{draw, draw_settled};
        let clicks = Rc::new(Cell::new(0));
        let (pill, cx) = cx.add_window_view(|_, _| Pill {
            chip: false,
            clicks: clicks.clone(),
        });
        let before = |cx: &mut VisualTestContext| {
            let end = cx.debug_bounds("after").unwrap().right();
            assert_eq!(end, px(400.), "the row's end moved");
            cx.debug_bounds("before").unwrap().left()
        };
        assert_eq!(before(cx), px(332.));
        for (chip, from, to) in [(true, px(332.), px(274.)), (false, px(274.), px(332.))] {
            pill.update(cx, |pill, cx| {
                pill.chip = chip;
                cx.notify();
            });
            draw(cx);
            let mut seen = vec![before(cx)];
            if !chip {
                let leaving = cx.debug_bounds("chip").expect("drawn while it goes");
                cx.simulate_click(leaving.center(), Default::default());
            }
            for _ in 0..16 {
                cx.executor()
                    .advance_clock(std::time::Duration::from_millis(16));
                cx.update(|window, cx| window.simulate_next_frame(cx));
                draw(cx);
                seen.push(before(cx));
            }
            draw_settled(cx);
            seen.push(before(cx));
            assert!((seen[0] - from).abs() <= px(1.), "{chip}: jumped: {seen:?}");
            assert_eq!(*seen.last().unwrap(), to, "{chip}: {seen:?}");
            assert!(
                seen.windows(2).all(|pair| {
                    (pair[1] - pair[0]).abs() < px(24.)
                        && if to > from {
                            pair[1] >= pair[0]
                        } else {
                            pair[1] <= pair[0]
                        }
                }),
                "{chip}: the neighbour jumped or went back: {seen:?}"
            );
        }
        assert_eq!(clicks.get(), 0, "a chip on its way out took a click");
    }
}
