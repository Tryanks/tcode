use std::borrow::BorrowMut;

use gpui::{
    AnyElement, App, Axis, Div, ElementId, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, Size, Stateful, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::motion::{PresenceSample, Transition, TransitionId, transition};

pub(crate) use gpui_base::motion::PresencePhase;
pub(crate) use scene::{Epoch, InScene, Scene};

pub(crate) mod expand;
mod scene;
pub(crate) mod shared;

pub(crate) mod focus_ring;

pub(crate) mod drag_ghost;
pub(crate) mod faded;
mod hover;
mod image_fade;
pub(crate) mod keyed;
mod list_flip;
mod list_hover;
pub(crate) mod list_rows;
mod list_scroll;
mod size_glide;

pub(crate) use hover::{blend, hold_hover_layer, hover_fill, hover_layer, hovered, pointer_hover};
pub(crate) use image_fade::{image_arrival, image_load};
pub(crate) use keyed::{keyed, keyed_across};
pub(crate) use list_flip::ListFlip;
pub(crate) use list_hover::{HoverHold, ListHover};
pub(crate) use list_scroll::{ListScroll, ScrollTarget};
pub(crate) use size_glide::{GlideAxes, GlideState, SizeGlide, size_key};
pub(crate) mod bounce;
#[cfg(all(preview_backend, any(target_os = "macos", target_os = "ios")))]
pub(crate) mod native_fade;
pub(crate) mod reflow;
pub(crate) use reflow::reflow;
pub(crate) mod resize_glide;

pub(crate) mod owned;
pub(crate) use owned::Clock;

/// How a value travels to a new target: over a duration, along gpui-base's
/// `Transition::new` easing. gpui-base has no default duration ("base
/// components do not install default motion"), so whoever owns a behaviour
/// names the duration it takes and hands it to the sampler.
#[derive(Clone)]
pub(crate) struct Curve {
    duration: std::time::Duration,
}

impl Curve {
    pub(crate) fn new(duration: std::time::Duration) -> Self {
        Self { duration }
    }

    pub(crate) fn duration(&self) -> std::time::Duration {
        self.duration
    }

    /// The curve as gpui-base's transition policy, its easing gpui-base's
    /// default.
    pub(crate) fn transition(&self) -> Transition {
        Transition::new(self.duration)
    }

    /// How far the curve has come `elapsed` after it started, from 0 to 1,
    /// for motion that keeps its own clock. Gap: gpui-base's `Transition`
    /// keeps its sampling to itself, so this samples its default easing
    /// (`ease_out_cubic`) over the duration with gpui-base's `Timing`.
    pub(crate) fn progress(&self, elapsed: std::time::Duration) -> f32 {
        gpui_base::motion::Timing::new(self.duration)
            .ease(gpui_base::motion::Easing::Custom(std::rc::Rc::new(
                gpui_base::animation::ease_out_cubic,
            )))
            .sample(elapsed)
            .directed_progress
    }
}

impl From<std::time::Duration> for Curve {
    fn from(duration: std::time::Duration) -> Self {
        Self::new(duration)
    }
}

impl From<&Curve> for Curve {
    fn from(curve: &Curve) -> Self {
        curve.clone()
    }
}

/// How far a surface rises while it appears, and sinks while it leaves.
pub(crate) const LIFT: Pixels = px(6.);

/// Mount and unmount motion for a transient surface: a dialog, a popover,
/// a menu, a tooltip. The sample's `progress` drives opacity and
/// [`lift`]; the surface stays mounted while `should_render()`.
pub(crate) fn appear(
    id: impl Into<TransitionId>,
    present: bool,
    curve: impl Into<Curve>,
    window: &mut Window,
    cx: &mut App,
) -> PresenceSample {
    let id: ElementId = id.into().into();
    Clock::Element.appear(id, present, &curve.into(), window, cx)
}

/// The vertical offset at `progress`: [`LIFT`] away from rest when absent,
/// zero when present.
pub(crate) fn lift(progress: f32) -> Pixels {
    LIFT * (1. - progress.clamp(0., 1.))
}

/// How long a list a keystroke refilters takes to fold its rows in and out:
/// short enough to keep pace with typing.
pub(crate) const KEYSTROKE: std::time::Duration = std::time::Duration::from_millis(120);

/// A colour as a glide carries it: mixed over premultiplied alpha
/// ([`crate::theme::mix`]), so every frame paints between what its ends
/// paint. gpui-base interpolates `Hsla` channel by channel, which between a
/// near-opaque fill and a faint tint of another colour (a button at rest and
/// under the pointer) passes through a colour darker than either; remove
/// this once gpui-base's colour interpolation is premultiplied.
#[derive(Clone, Copy, PartialEq)]
pub(crate) struct Colour(pub(crate) gpui::Hsla);

impl gpui_base::motion::Interpolate for Colour {
    fn interpolate(&self, target: &Self, progress: f32) -> Self {
        Self(crate::theme::mix(self.0, target.0, progress))
    }
}

fn child_id(id: &ElementId, channel: impl Into<gpui::SharedString>) -> ElementId {
    ElementId::NamedChild(std::sync::Arc::new(id.clone()), channel.into())
}

/// Something a page shows only some of the time, sampled by [`shown`]: what
/// to draw, and how far in it is.
#[derive(Clone, PartialEq)]
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

/// Follow `value` in and out of a page along `curve`. Once it is `None` the
/// last value it had is kept until it has gone, so whatever leaves is drawn
/// as it was. A page that opens with it shows it at rest. The caller draws
/// the result through [`Shown::fold`] or [`Shown::fade`] whenever this is
/// `Some`, at the same place in the tree, so the element keeps its ids while
/// it comes and goes.
pub(crate) fn shown<T: Clone + PartialEq + 'static>(
    id: impl Into<ElementId>,
    value: Option<T>,
    curve: impl Into<Curve>,
    window: &mut Window,
    cx: &mut App,
) -> Option<Shown<T>> {
    Clock::Element.shown(id, value, &curve.into(), window, cx)
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
        self.reveal("fold", Axis::Vertical, px(0.), content)
    }

    /// Beside other content in a row: grows to its width and fades in,
    /// shrinks and fades out, so what is beside it moves over as it does.
    pub(crate) fn fold_across(&self, content: impl IntoElement) -> AnyElement {
        self.fold_across_gap(px(0.), content)
    }

    /// [`Self::fold_across`] in a row with a `gap` between its children: the
    /// gap before it comes and goes with it, so nothing beside it jumps by
    /// the gap as it starts or finishes. At rest it is its content in the
    /// row: it shrinks as far as its content does and no further.
    pub(crate) fn fold_across_gap(&self, gap: Pixels, content: impl IntoElement) -> AnyElement {
        self.reveal("fold-across", Axis::Horizontal, gap, content)
    }

    fn reveal(
        &self,
        channel: &'static str,
        axis: Axis,
        gap: Pixels,
        content: impl IntoElement,
    ) -> AnyElement {
        Reveal {
            id: child_id(&self.id, channel),
            progress: self.progress,
            leaving: self.leaving,
            measure: self.measure.clone(),
            axis,
            gap,
            content: Some(content.into_any_element()),
            child: None,
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

    fn frame(&self, content: impl IntoElement) -> Div {
        frame(self.progress, self.leaving, content)
    }
}

/// `content` drawn `progress` in. What is leaving takes no clicks.
fn frame(progress: f32, leaving: bool, content: impl IntoElement) -> Div {
    div()
        .relative()
        .opacity(progress)
        .child(faded::faded(progress, content))
        .when(leaving, |frame| {
            frame.child(div().absolute().inset_0().occlude())
        })
}

/// A fold in a column at `progress` its owner drives (a list's row under
/// the disclosures around it): [`Shown::fold`] for a progress that is not
/// one value's coming and going. What it folds keeps its place in the tree
/// at every progress, so it keeps its element state as its folds move.
pub(crate) fn fold(
    id: impl Into<ElementId>,
    progress: f32,
    content: impl IntoElement,
) -> AnyElement {
    Shown {
        id: id.into(),
        value: (),
        progress,
        leaving: false,
        measure: None,
    }
    .fold(content)
}

/// A fold's clipped, measured height, or width: every fold Tcode draws.
///
/// gpui-base's `MotionReveal` lays a child it has not measured yet out at
/// zero height, so a section open on the frame that first draws it (a
/// message's actions, an open disclosure) comes in a frame late and moves
/// what is below; and it folds only heights. This one lays its child out as
/// itself while it is at rest or unmeasured, and clips it to its measurement
/// scaled by the progress only while it folds. Its content keeps its place
/// in the tree in both.
///
/// In a row at rest it lays out its content alone, as if it were not there:
/// a box of its own around the content would let the row shrink the box
/// below what the content can shrink to, and the content would be drawn at
/// its own width over what is beside it.
struct Reveal {
    id: ElementId,
    progress: f32,
    leaving: bool,
    /// Its measurement, when an owner keeps it.
    measure: Option<Measure>,
    /// The extent it folds: height in a column, width in a row.
    axis: Axis,
    /// In a row, the gap before it that it brings and takes away.
    gap: Pixels,
    /// What it draws, until it is laid out as [`Self::child`].
    content: Option<AnyElement>,
    /// The content as this frame lays it out: framed with its fade, or alone.
    child: Option<AnyElement>,
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
                    // Content at rest is no shorter than itself in a column
                    // that overflows; a box with only a height would be
                    // shrunk to make room, to nothing in a full scroll
                    // column, and the fold would jump open at its end.
                    style.flex_shrink = 0.;
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
        let content = self.content.take().expect("a Reveal is laid out once");
        let child = self.child.insert(match self.axis {
            Axis::Horizontal if self.progress >= 1. && !self.leaving => content,
            Axis::Horizontal => frame(self.progress, self.leaving, content)
                .min_w_0()
                .into_any_element(),
            Axis::Vertical => frame(self.progress, self.leaving, content)
                .w_full()
                .into_any_element(),
        });
        match measured {
            Some(size) if self.progress < 1. => (
                window.request_layout(folded(size, self.progress), None, cx),
                RevealLayout::Clipped,
            ),
            None if self.progress <= 0. => (
                window.request_layout(folded(Size::default(), 0.), None, cx),
                RevealLayout::Clipped,
            ),
            _ => (child.request_layout(window, cx), RevealLayout::Plain),
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
        let child = self.child.as_mut().expect("laid out");
        let size = match layout {
            RevealLayout::Plain => {
                child.prepaint(window, cx);
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
                let size = child.layout_as_root(space, window, cx);
                window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
                    child.prepaint_at(bounds.origin, window, cx);
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
        let child = self.child.as_mut().expect("laid out");
        match layout {
            RevealLayout::Plain => child.paint(window, cx),
            RevealLayout::Clipped => {
                window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
                    child.paint(window, cx);
                });
            }
        }
    }
}

/// Content that changes to another of its states — a status, a QR for a new
/// link — cross-fading from the one it showed. Each state is drawn under
/// its own slot, which it keeps while it fades out on top of the next.
/// `cx` is the caller's own context, so a view's `build` can make listeners.
pub(crate) fn cross_fade<T: Clone + PartialEq + 'static, C: BorrowMut<App>>(
    id: impl Into<ElementId>,
    value: T,
    curve: impl Into<Curve>,
    window: &mut Window,
    cx: &mut C,
    build: impl FnMut(&T, &mut Window, &mut C) -> AnyElement,
) -> AnyElement {
    Clock::Element.cross_fade(id, value, &curve.into(), window, cx, build)
}

/// [`cross_fade`] for a part of a control — its glyph, its status — which
/// takes no input of its own: what leaves covers nothing, so a press during
/// the fade still reaches the control.
pub(crate) fn cross_fade_part<T: Clone + PartialEq + 'static, C: BorrowMut<App>>(
    id: impl Into<ElementId>,
    value: T,
    curve: impl Into<Curve>,
    window: &mut Window,
    cx: &mut C,
    build: impl FnMut(&T, &mut Window, &mut C) -> AnyElement,
) -> AnyElement {
    Clock::Element.cross_fade_part(id, value, &curve.into(), window, cx, build)
}

/// What a cross-fade keeps of the width each of its states, by key, was
/// last laid out at while current, so that one leaving is laid out as it
/// was at rest.
#[derive(Clone, Default)]
struct AtRest(std::rc::Rc<std::cell::RefCell<Vec<(u64, Pixels)>>>);

impl AtRest {
    /// `region`, noting the width of its child at `index`, the state
    /// `current`, and forgetting the states it no longer draws.
    fn noting(&self, region: Div, current: u64, index: usize, drawn: Vec<u64>) -> Div {
        let widths = self.0.clone();
        region.on_children_prepainted(move |bounds, _, _| {
            let mut widths = std::cell::RefCell::borrow_mut(&widths);
            widths.retain(|(key, _)| *key != current && drawn.contains(key));
            if let Some(bounds) = bounds.get(index) {
                widths.push((current, bounds.size.width));
            }
        })
    }

    /// `layer`, the state `key` leaving, out of flow at its region's top
    /// left.
    fn leaving(&self, key: u64, layer: Stateful<Div>) -> Stateful<Div> {
        let width = self
            .0
            .borrow()
            .iter()
            .find_map(|(kept, width)| (*kept == key).then_some(*width));
        // Out of flow, a state is as wide as its content: one that fills its
        // row would shrink, and a line that fades its end rather than wrap
        // would widen to its whole text and stop fading. It keeps the width
        // it had while current, and is as wide as the region at least.
        layer
            .absolute()
            .top_0()
            .left_0()
            .when_some(width, |layer, width| layer.w(width))
            .min_w_full()
    }
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
    at_rest: &AtRest,
) -> AnyElement {
    let slot = |generation: u64| {
        ElementId::NamedChild(
            std::sync::Arc::new(id.clone()),
            generation.to_string().into(),
        )
    };
    let drawn = std::iter::once(generation)
        .chain(previous.as_ref().map(|(_, generation)| *generation))
        .collect();
    at_rest
        .noting(div(), generation, 0, drawn)
        .relative()
        .child(
            div()
                .id(slot(generation))
                .opacity(progress)
                .child(faded::faded(progress, current)),
        )
        .when_some(previous, |frame, (previous, generation)| {
            frame.child(
                at_rest
                    .leaving(generation, div().id(slot(generation)))
                    .opacity(1. - progress)
                    .child(faded::faded(1. - progress, previous))
                    .when(shield, |previous| {
                        previous.child(div().absolute().inset_0().occlude())
                    }),
            )
        })
        .into_any_element()
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
    staged: Option<usize>,
    keyboard: Option<gpui::FocusHandle>,
    at_rest: AtRest,
}

impl CrossFade {
    pub(crate) fn new(
        id: impl Into<ElementId>,
        current: usize,
        count: usize,
        curve: impl Into<Curve>,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        Clock::Element.layers(id, current, count, &curve.into(), window, cx)
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
            staged: None,
            keyboard: None,
            at_rest: AtRest::default(),
        }
    }

    /// Leaving layers keep the widths `at_rest` noted while they were
    /// current.
    fn at_rest(mut self, at_rest: AtRest) -> Self {
        self.at_rest = at_rest;
        self
    }

    pub(crate) fn fill(mut self) -> Self {
        self.fill = true;
        self
    }

    /// A leaving layer is held to the region's width, not widened by
    /// content wider than the region.
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

    /// Draws `layer` even while it is out: unseen and covered, so it lays
    /// itself out and readies what it shows (a thread's rows measured, its
    /// Markdown built) before it fades in, in the place it will fade in from.
    pub(crate) fn stage(mut self, layer: usize) -> Self {
        self.staged = Some(layer);
        self
    }

    /// A press anywhere in the region gives the keyboard to `focus`, the
    /// handle its owner tracks. A leaving or staged layer's shield occludes
    /// every hitbox painted before it, the owner's own among them, so
    /// GPUI's focus-on-press, which asks the owner's hitbox whether it is
    /// hovered, never sees a press over the shield; the shield takes the
    /// press for the owner instead, so what it covers still gets none.
    pub(crate) fn keyboard(mut self, focus: gpui::FocusHandle) -> Self {
        self.keyboard = Some(focus);
        self
    }

    /// Whether `layer` is drawn this frame, so only those are built.
    pub(crate) fn shows(&self, layer: usize) -> bool {
        layer == self.current
            || self.staged == Some(layer)
            || self.opacity.get(layer).is_some_and(|opacity| *opacity > 0.)
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
        let keyboard = self.keyboard.clone();
        let at_rest = self.at_rest.clone();
        let current = layers.iter().position(|(layer, _)| *layer == self.current);
        let drawn = layers.iter().map(|(layer, _)| *layer as u64).collect();
        let region = match current {
            Some(index) => self
                .at_rest
                .noting(div(), self.current as u64, index, drawn),
            None => div(),
        };
        region
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
                        } else if leaving && span {
                            el.absolute().top_0().left_0().right_0()
                        } else if leaving {
                            at_rest.leaving(layer as u64, el)
                        } else if column {
                            el.flex().flex_col().flex_1().min_h_0()
                        } else {
                            el
                        }
                    })
                    .child(faded::faded(self.opacity[layer], content))
                    .when(leaving, |el| {
                        el.child(shield().when_some(keyboard.clone(), |shield, focus| {
                            // As GPUI's own focus-on-press: a press something
                            // drawn over the shield took is left to it.
                            shield.on_any_mouse_down(move |_, window, cx| {
                                if !window.default_prevented() {
                                    window.focus(&focus, cx);
                                    window.prevent_default();
                                }
                            })
                        }))
                    })
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
    curve: impl Into<Curve>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    status_icon_sized(id, icon, color, None, curve, window, cx)
}

/// A [`status_icon`] drawn at `size` rather than at its text's size.
pub(crate) fn status_icon_sized(
    id: impl Into<ElementId>,
    icon: crate::icon::IconName,
    color: gpui::Hsla,
    size: impl Into<Option<crate::sizing::Size>>,
    curve: impl Into<Curve>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    use crate::icon::IconNamed as _;
    use crate::sizing::Sizable as _;
    let id = id.into();
    let size = size.into();
    let curve = curve.into();
    let color = transition(
        child_id(&id, "color"),
        color,
        curve.transition(),
        window,
        cx,
    );
    cross_fade_part(
        child_id(&id, "glyph"),
        icon.path(),
        curve,
        window,
        cx,
        move |path, _, _| {
            crate::icon::Icon::empty()
                .path(path.clone())
                .text_color(color)
                .when_some(size, |icon, size| icon.with_size(size))
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
