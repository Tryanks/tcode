//! An element whose layout size glides to its content's new size.
//!
//! gpui-base's `MotionReveal` scales one measurement by a progress and its
//! `transition` animates values; neither moves an element's layout from one
//! size to its content's new one, and GPUI has no layout (FLIP) transition.
//! So what is beside or below content that changes size at rest — a body
//! whose output arrived, a picture that decoded, a chip that widened — moves
//! as a consequence of [`SizeGlide`]'s layout, frame by frame, with no
//! bookkeeping of its own.

use std::{cell::RefCell, rc::Rc};

#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
#[cfg(target_family = "wasm")]
use web_time::Instant;

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, ContentMask, Element, ElementId, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, Pixels, Size, Style, Window, relative, size,
};

/// How long a size glide on its own clock takes.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// The sides of an element a [`SizeGlide`] moves; the others are the
/// content's own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum GlideAxes {
    /// In a column: the element spans the width it is given.
    #[default]
    Height,
    /// In a row: the element is as tall as its content.
    Width,
    Both,
}

/// What a [`SizeGlide`] keeps between frames, for as long as the content it
/// glides lives.
///
/// A site without an owner keeps it in the window with
/// [`GlideState::keyed`]. Content laid out in a measuring pass under other
/// element ids than the painted pass (a Markdown document measuring its
/// blocks) keeps it with the owner of both passes, so they size alike.
#[derive(Clone, Default)]
pub(crate) struct GlideState(Rc<RefCell<Glide>>);

#[derive(Default)]
struct Glide {
    /// The size the element was last painted at, mid-glide included.
    painted: Option<Size<Pixels>>,
    /// The content's own size, measured while it glides.
    content: Option<Size<Pixels>>,
    /// The content key last seen.
    key: Option<u64>,
    /// A glide the state runs on its own clock: where it started, and when.
    run: Option<(Size<Pixels>, Instant)>,
}

impl GlideState {
    /// The state of the glide `id` names, kept in the window while the
    /// element is drawn every frame.
    pub(crate) fn keyed(id: impl Into<ElementId>, window: &mut Window, cx: &mut App) -> Self {
        window
            .use_keyed_state(id.into(), cx, |_, _| GlideState::default())
            .read(cx)
            .clone()
    }

    /// Where an owner-clocked glide that starts now starts: the size last
    /// painted, or `None` before the content was painted, when there is
    /// nothing to glide from.
    pub(crate) fn begin(&self) -> Option<Size<Pixels>> {
        let mut glide = self.0.borrow_mut();
        glide.content = None;
        glide.painted
    }

    /// Whether the content has been painted, and so has a size a glide could
    /// start from.
    pub(crate) fn painted(&self) -> bool {
        self.0.borrow().painted.is_some()
    }

    /// Whether a glide the state runs on its own clock is under way `now`,
    /// so an owner that caches its content's size measures it again: from
    /// the frame it starts to the one that lays the content out at rest. A
    /// glide whose content is no longer drawn stops counting a beat later.
    pub(crate) fn moving(&self, now: Instant) -> bool {
        self.0
            .borrow()
            .run
            .is_some_and(|(_, at)| now.saturating_duration_since(at) <= GLIDE * 2)
    }

    /// The size content with `key` takes this frame when its own size is
    /// `target`: `target` at rest, and on its way to it from the size last
    /// painted once `key` changes. For an element that computes its own size
    /// (a measured layout), which knows its target in the frame it changes;
    /// it reports what it painted with [`Self::painted_at`].
    pub(crate) fn toward(&self, key: u64, target: Size<Pixels>, cx: &App) -> Size<Pixels> {
        let now = cx.background_executor().now();
        let mut glide = self.0.borrow_mut();
        glide.retarget(key, now, cx);
        match glide.run {
            Some((from, at)) => {
                let progress =
                    crate::motion::Curve::new(GLIDE).progress(now.saturating_duration_since(at));
                if progress >= 1. {
                    glide.run = None;
                }
                lerp(from, target, progress, GlideAxes::Both)
            }
            None => target,
        }
    }

    /// Record the size the element was painted at, and keep frames coming
    /// while it glides.
    pub(crate) fn painted_at(&self, painted: Size<Pixels>, window: &mut Window) {
        let mut glide = self.0.borrow_mut();
        glide.painted = Some(painted);
        if glide.run.is_some() {
            window.request_animation_frame();
        }
    }
}

impl Glide {
    /// A new key starts a glide from where the content was painted; reduced
    /// motion, or nothing painted yet, takes the new size at once.
    fn retarget(&mut self, key: u64, now: Instant, cx: &App) {
        if self.key.is_some_and(|seen| seen != key)
            && let Some(painted) = self.painted
            && !cx.reduce_motion()
        {
            self.run = Some((painted, now));
            self.content = None;
        }
        self.key = Some(key);
        if cx.reduce_motion() {
            self.run = None;
        }
    }
}

/// A content key for content whose size is all that changes about it: a
/// picture that decoded, for [`GlideState::toward`].
pub(crate) fn size_key(size: Size<Pixels>) -> u64 {
    (u64::from(f32::from(size.width).to_bits()) << 32) | u64::from(f32::from(size.height).to_bits())
}

fn lerp(from: Size<Pixels>, to: Size<Pixels>, progress: f32, axes: GlideAxes) -> Size<Pixels> {
    let progress = progress.clamp(0., 1.);
    let glide = |from: Pixels, to: Pixels| from + (to - from) * progress;
    size(
        if axes == GlideAxes::Height {
            to.width
        } else {
            glide(from.width, to.width)
        },
        if axes == GlideAxes::Width {
            to.height
        } else {
            glide(from.height, to.height)
        },
    )
}

/// Content whose layout size glides from the size it was painted at to its
/// own new size, clipped to the glided size on the way.
///
/// Two clocks drive it. With [`SizeGlide::on_change`] it keeps its own: a
/// new content key starts a glide over [`GLIDE`], and the site says nothing
/// more. With [`SizeGlide::gliding`] its owner keeps one, for a glide that
/// must stay in step with other motion (a cross-fade): the owner starts it
/// with [`GlideState::begin`] and samples the progress each frame. Either
/// way, at rest or before the content was ever painted the element is the
/// content's own layout, a glide that starts during another starts where
/// that one had got to, and reduced motion takes the new size at once.
///
/// The content's new size is measured while it is painted, a frame behind,
/// so the frame its key changes shows the size it had.
pub(crate) struct SizeGlide {
    state: GlideState,
    axes: GlideAxes,
    key: Option<u64>,
    owner: Option<(Option<Size<Pixels>>, f32)>,
    child: AnyElement,
}

impl SizeGlide {
    /// `child` at rest, at its own size.
    pub(crate) fn new(state: &GlideState, child: impl IntoElement) -> Self {
        Self {
            state: state.clone(),
            axes: GlideAxes::Height,
            key: None,
            owner: None,
            child: child.into_any_element(),
        }
    }

    pub(crate) fn axes(mut self, axes: GlideAxes) -> Self {
        self.axes = axes;
        self
    }

    /// Glide on the state's own clock whenever `key`, which names what the
    /// content shows, changes.
    pub(crate) fn on_change(mut self, key: u64) -> Self {
        self.key = Some(key);
        self
    }

    /// Glide on the owner's clock from `from`, which [`GlideState::begin`]
    /// gave, to the content's own size; `progress` runs from 0 to 1.
    pub(crate) fn gliding(mut self, from: Option<Size<Pixels>>, progress: f32) -> Self {
        self.owner = Some((from, progress));
        self
    }

    /// The size to lay the element out at this frame, while it glides.
    fn glided(&self, cx: &App) -> Option<Size<Pixels>> {
        let now = cx.background_executor().now();
        let mut glide = self.state.0.borrow_mut();
        if let Some(key) = self.key {
            glide.retarget(key, now, cx);
        }
        let (from, progress) = match self.owner {
            Some((from, progress)) => (from?, progress),
            None => {
                let (from, at) = glide.run?;
                let progress =
                    crate::motion::Curve::new(GLIDE).progress(now.saturating_duration_since(at));
                // The run ends once it has played and its content was
                // measured; the content then lays itself out.
                if progress >= 1. && glide.content.is_some() {
                    glide.run = None;
                    return None;
                }
                (from, progress)
            }
        };
        if progress >= 1. {
            return None;
        }
        Some(lerp(
            from,
            glide.content.unwrap_or(from),
            progress,
            self.axes,
        ))
    }
}

impl IntoElement for SizeGlide {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// How a [`SizeGlide`] lays its content out this frame.
pub(crate) enum GlideLayout {
    /// The content is the element's layout.
    Plain,
    /// The element is laid out at the glided size; the content is laid out
    /// on its own and clipped to it.
    Clipped,
}

impl Element for SizeGlide {
    type RequestLayoutState = GlideLayout;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
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
    ) -> (LayoutId, GlideLayout) {
        match self.glided(cx) {
            Some(glided) => {
                let mut style = Style::default();
                style.size.width = match self.axes {
                    GlideAxes::Height => relative(1.).into(),
                    GlideAxes::Width | GlideAxes::Both => glided.width.into(),
                };
                style.size.height = glided.height.into();
                style.flex_shrink = 0.;
                (window.request_layout(style, None, cx), GlideLayout::Clipped)
            }
            None => (self.child.request_layout(window, cx), GlideLayout::Plain),
        }
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut GlideLayout,
        window: &mut Window,
        cx: &mut App,
    ) {
        match layout {
            GlideLayout::Plain => {
                self.child.prepaint(window, cx);
            }
            GlideLayout::Clipped => {
                let width = match self.axes {
                    GlideAxes::Height => AvailableSpace::Definite(bounds.size.width),
                    GlideAxes::Width | GlideAxes::Both => AvailableSpace::MaxContent,
                };
                let content =
                    self.child
                        .layout_as_root(size(width, AvailableSpace::MinContent), window, cx);
                self.state.0.borrow_mut().content = Some(content);
                window.with_content_mask(Some(ContentMask { bounds }), |window| {
                    self.child.prepaint_at(bounds.origin, window, cx);
                });
            }
        }
        self.state.painted_at(bounds.size, window);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut GlideLayout,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        match layout {
            GlideLayout::Plain => self.child.paint(window, cx),
            GlideLayout::Clipped => {
                window.with_content_mask(Some(ContentMask { bounds }), |window| {
                    self.child.paint(window, cx);
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gpui::{
        Context, InteractiveElement as _, IntoElement, ParentElement as _, Render, Styled as _,
        TestAppContext, VisualTestContext, Window, div, px,
    };
    use gpui_base::{h_flex, v_flex};

    use super::{GlideAxes, GlideState, SizeGlide};
    use crate::test_frames::{draw, draw_settled};

    /// Content beside or above a neighbour, whose size its view sets.
    struct Grows {
        axes: GlideAxes,
        side: f32,
    }

    impl Render for Grows {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let glide = GlideState::keyed("grows", window, cx);
            let content = div()
                .debug_selector(|| "content".into())
                .w(px(self.side))
                .h(px(self.side));
            let glided = SizeGlide::new(&glide, content)
                .axes(self.axes)
                .on_change(self.side as u64);
            let neighbour = div().debug_selector(|| "neighbour".into()).size(px(10.));
            match self.axes {
                GlideAxes::Width => h_flex()
                    .items_start()
                    .child(glided)
                    .child(neighbour)
                    .into_any_element(),
                _ => v_flex()
                    .w(px(400.))
                    .child(glided)
                    .child(neighbour)
                    .into_any_element(),
            }
        }
    }

    /// One display frame: time passes, the frames the window asked for are
    /// delivered, and it draws. How many frames it had asked for.
    fn frame(cx: &mut VisualTestContext) -> usize {
        cx.executor().advance_clock(Duration::from_millis(16));
        let asked = cx.update(|window, cx| window.simulate_next_frame(cx));
        draw(cx);
        asked
    }

    /// Content whose size changes at rest moves what is beside or below it
    /// smoothly: the frame of the change keeps the old place, the neighbour
    /// then moves one way, never by half its travel in a frame, and comes to
    /// rest exactly where the new size puts it. Reduced motion moves it at
    /// once.
    #[gpui::test]
    fn a_size_change_at_rest_moves_the_neighbour_smoothly(cx: &mut TestAppContext) {
        for (axes, reduce) in [
            (GlideAxes::Height, false),
            (GlideAxes::Width, false),
            (GlideAxes::Height, true),
        ] {
            cx.update(|cx| cx.set_reduce_motion(reduce));
            let (view, cx) = cx.add_window_view(|_, _| Grows { axes, side: 40. });
            draw_settled(cx);
            let place = |cx: &mut VisualTestContext| {
                let neighbour = cx.debug_bounds("neighbour").expect("the neighbour");
                if axes == GlideAxes::Width {
                    neighbour.left()
                } else {
                    neighbour.top()
                }
            };
            assert_eq!(place(cx), px(40.));
            view.update(cx, |view, cx| {
                view.side = 120.;
                cx.notify();
            });
            draw(cx);
            if reduce {
                assert_eq!(place(cx), px(120.), "reduced motion moves it at once");
                continue;
            }
            let mut seen = vec![place(cx)];
            assert_eq!(seen[0], px(40.), "{axes:?}: the neighbour jumped");
            while frame(cx) > 0 {
                seen.push(place(cx));
            }
            seen.push(place(cx));
            assert!(
                seen.windows(2).all(|pair| pair[1] >= pair[0]),
                "{axes:?}: the neighbour went back: {seen:?}"
            );
            assert!(
                seen.windows(2).all(|pair| pair[1] - pair[0] < px(40.)),
                "{axes:?}: the neighbour jumped: {seen:?}"
            );
            assert_eq!(*seen.last().unwrap(), px(120.), "{axes:?}: {seen:?}");
            let content = cx.debug_bounds("content").unwrap();
            assert_eq!(content.size, gpui::size(px(120.), px(120.)));
        }
    }
}
