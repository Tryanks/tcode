//! Motion whose state a view keeps, so it survives the element tree moving
//! the view.
//!
//! gpui-base's `transition` (and so `shown`, `CrossFade::new` and every
//! sampler built on it) keeps its clock in keyed element state, keyed by the
//! full id path from the window root. A view drawn under another parent
//! starts all of it over at its target: gpui-base's dock re-parents a tab
//! group whenever its tree changes (see `panes::ThreadPanes`), so a pane's
//! fade under way when its group moved snapped to its end. [`Owned`] keeps the same clocks in a
//! store the view holds, sampled with gpui-base's `Timing` against the
//! executor's clock as `transition` samples its own, and settled at once
//! under reduced motion as `transition` settles. Remove it once gpui-base's
//! samplers can key their state by an owner instead of an element path.

use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;
#[cfg(not(target_family = "wasm"))]
use std::time::Instant;

use gpui::{App, SharedString, Window};
use gpui_base::motion::{Easing, Interpolate, Timing};
#[cfg(target_family = "wasm")]
use web_time::Instant;

use super::CrossFade;

/// How a value owned by an [`Owned`] store travels to a new target.
#[derive(Clone)]
pub(crate) struct Curve {
    duration: Duration,
    easing: Easing,
}

impl Curve {
    pub(crate) fn new(duration: Duration, easing: Easing) -> Self {
        Self { duration, easing }
    }

    /// [`super::settle`]'s curve: `SETTLE`, easing out.
    pub(crate) fn settle() -> Self {
        Self::new(
            crate::edge_back::SETTLE,
            Easing::Custom(Rc::new(gpui_base::animation::ease_out_cubic)),
        )
    }
}

/// The motion state of one view, by key: clone it into whatever samples it
/// (a renderer drawing the view's frame); every clone is the same store.
#[derive(Clone, Default)]
pub(crate) struct Owned(Rc<RefCell<HashMap<Key, Box<dyn Any>>>>);

/// A value's key and, for a cross-fade, its layer.
type Key = (SharedString, usize);

/// The channel of a key [`Owned::keep`] holds a handle under, apart from
/// every layer of a glide.
const KEPT: usize = usize::MAX;

struct Glide<T> {
    from: T,
    target: T,
    started_at: Instant,
    duration: Duration,
    /// The share of a full glide this one takes: below one when it turned
    /// back part of the way, so it returns in the time it had travelled.
    share: f32,
}

impl Owned {
    /// The value now of a glide toward `target` over `curve`: the first
    /// target is taken at once; a later one is glided to from wherever the
    /// value is when it changes.
    pub(crate) fn transition<T>(
        &self,
        key: impl Into<SharedString>,
        target: T,
        curve: &Curve,
        window: &mut Window,
        cx: &App,
    ) -> T
    where
        T: Interpolate + PartialEq + 'static,
    {
        self.sample((key.into(), 0), target, curve, window, cx)
    }

    /// [`CrossFade`] with its layers' opacities kept here: layer `current`
    /// of `count` fades in over `curve` while the others fade out. `key`
    /// is also the region's element id.
    pub(crate) fn cross_fade(
        &self,
        key: impl Into<SharedString>,
        current: usize,
        count: usize,
        curve: &Curve,
        window: &mut Window,
        cx: &App,
    ) -> CrossFade {
        let key = key.into();
        let opacity = (0..count)
            .map(|layer| {
                let target = if layer == current { 1. } else { 0. };
                self.sample((key.clone(), layer), target, curve, window, cx)
            })
            .collect();
        CrossFade::sampled(key, current, opacity)
    }

    /// [`super::shown`] with its clock, the last value it had and its fold's
    /// measurement kept here: something a page shows only some of the time,
    /// drawn through [`Shown::fold`](super::Shown::fold) and its kin, that
    /// carries on coming or going when the view is drawn under a new parent.
    /// `key` is also its element id.
    pub(crate) fn shown<T: Clone + PartialEq + 'static>(
        &self,
        key: impl Into<SharedString>,
        value: Option<T>,
        window: &mut Window,
        cx: &App,
    ) -> Option<super::Shown<T>> {
        let key = key.into();
        let target = if value.is_some() { 1f32 } else { 0. };
        let progress = self.sample((key.clone(), 0), target, &Curve::settle(), window, cx);
        let mut values = self.0.borrow_mut();
        let measure = values
            .entry((key.clone(), 2))
            .or_insert_with(|| Box::new(super::Measure::default()))
            .downcast_ref::<super::Measure>()
            .expect("a fold's measurement")
            .clone();
        let kept = values
            .entry((key.clone(), 1))
            .or_insert_with(|| Box::new(None::<T>))
            .downcast_mut::<Option<T>>()
            .expect("a shown value of one type");
        let (value, leaving) = match value {
            Some(value) => {
                *kept = Some(value.clone());
                (value, false)
            }
            None if progress > 0. => (kept.clone()?, true),
            None => {
                *kept = None;
                return None;
            }
        };
        Some(super::Shown {
            id: key.into(),
            value,
            progress,
            leaving,
            measure: Some(measure),
        })
    }

    /// [`super::appear`]'s progress with its clock kept here: how far in,
    /// from 0 to 1, something that comes and goes is, coming in from nothing
    /// the first time it is present as gpui-base's presence does, and
    /// carrying on when the view is drawn under a new parent.
    pub(crate) fn appear(
        &self,
        key: impl Into<SharedString>,
        present: bool,
        window: &mut Window,
        cx: &App,
    ) -> f32 {
        let key = (key.into(), 0);
        let curve = Curve::settle();
        self.0.borrow_mut().entry(key.clone()).or_insert_with(|| {
            Box::new(Glide {
                from: 0f32,
                target: 0f32,
                started_at: cx.background_executor().now(),
                duration: curve.duration,
                share: 1.,
            })
        });
        self.sample(key, if present { 1. } else { 0. }, &curve, window, cx)
    }

    /// How far in, from 0 to 1, what `value` names is: a value other than
    /// the last one asked about comes in over [`Curve::settle`], the first
    /// is in at once. For something drawn in another's place (a section, a
    /// page) that fades in, and carries on fading when the view is drawn
    /// under a new parent, where [`super::appear`] would start it over.
    pub(crate) fn entered<T: PartialEq + 'static>(
        &self,
        key: impl Into<SharedString>,
        value: T,
        window: &mut Window,
        cx: &App,
    ) -> f32 {
        let key = key.into();
        let mut values = self.0.borrow_mut();
        let kept = values
            .entry((key.clone(), 1))
            .or_insert_with(|| Box::new(None::<T>))
            .downcast_mut::<Option<T>>()
            .expect("an entered value of one type");
        let changed = kept.as_ref().is_some_and(|kept| *kept != value);
        *kept = Some(value);
        if changed {
            values.insert(
                (key.clone(), 0),
                Box::new(Glide {
                    from: 0f32,
                    target: 0f32,
                    started_at: cx.background_executor().now(),
                    duration: Curve::settle().duration,
                    share: 1.,
                }),
            );
        }
        drop(values);
        self.sample((key, 0), 1f32, &Curve::settle(), window, cx)
    }

    /// The handle kept under `key`: made the first time it is asked for,
    /// the same one every time after, for state a sampler shares through
    /// clones (a [`super::GlideState`], a [`super::reflow::ReflowState`]).
    pub(crate) fn keep<T: Clone + Default + 'static>(&self, key: impl Into<SharedString>) -> T {
        let key = (key.into(), KEPT);
        let mut values = self.0.borrow_mut();
        if let Some(kept) = values.get(&key).and_then(|kept| kept.downcast_ref::<T>()) {
            return kept.clone();
        }
        let kept = T::default();
        values.insert(key, Box::new(kept.clone()));
        kept
    }

    /// The reflow scope `key` for the layout `layout` names, kept here so a
    /// layout the window draws under another parent when it changes (a page
    /// re-parented as the window turns compact or wide) still glides its
    /// parts from where the last one drew them.
    pub(crate) fn reflow(
        &self,
        key: impl Into<SharedString>,
        layout: u64,
    ) -> super::reflow::Reflow {
        self.keep::<super::reflow::ReflowState>(key).scope(layout)
    }

    /// [`Self::reflow`] whose width changing also makes a reflow, on frames
    /// it was drawn the frame before where it is now; see
    /// [`super::reflow::ReflowState::scope_in`].
    pub(crate) fn reflow_in(
        &self,
        key: impl Into<SharedString>,
        layout: u64,
        window: &mut Window,
        cx: &mut App,
    ) -> super::reflow::Reflow {
        let key = key.into();
        self.keep::<super::reflow::ReflowState>(key.clone())
            .scope_in(key, layout, window, cx)
    }

    /// A [`super::SizeGlide`]'s state, kept here.
    pub(crate) fn glide_state(&self, key: impl Into<SharedString>) -> super::GlideState {
        self.keep(key)
    }

    /// Layer `current` of `count` coming in over the others, and the
    /// region's height gliding to the new layer's, both on clocks kept
    /// here: one layout swapped for another (an accordion for a dropdown,
    /// a control that only one layout has) without a jump of what is below.
    pub(crate) fn swap(
        &self,
        key: impl Into<SharedString>,
        current: usize,
        count: usize,
        window: &mut Window,
        cx: &App,
    ) -> Swap {
        let key = key.into();
        Swap {
            fade: self
                .cross_fade(key.clone(), current, count, &Curve::settle(), window, cx)
                .span(),
            height: self.glide_state(key),
            current,
        }
    }

    fn sample<T>(&self, key: Key, target: T, curve: &Curve, window: &mut Window, cx: &App) -> T
    where
        T: Interpolate + PartialEq + 'static,
    {
        let now = cx.background_executor().now();
        let mut values = self.0.borrow_mut();
        let at_rest = |target: &T| Glide {
            from: target.clone(),
            target: target.clone(),
            started_at: now,
            duration: curve.duration,
            share: 1.,
        };
        let glide = match values.get_mut(&key) {
            Some(glide) if glide.is::<Glide<T>>() => glide,
            _ => {
                values.insert(key, Box::new(at_rest(&target)));
                return target;
            }
        };
        let glide = glide.downcast_mut::<Glide<T>>().expect("checked above");
        if cx.reduce_motion() || curve.duration.is_zero() {
            if glide.from != target || glide.target != target {
                *glide = at_rest(&target);
            }
            return target;
        }
        let sample = Timing::new(glide.duration)
            .ease(curve.easing.clone())
            .sample(now.saturating_duration_since(glide.started_at));
        let value = glide
            .from
            .interpolate(&glide.target, sample.directed_progress);
        if glide.target != target {
            let share = if target == glide.from {
                (sample.directed_progress * glide.share + (1. - glide.share)).clamp(0., 1.)
            } else {
                1.
            };
            *glide = Glide {
                from: value.clone(),
                target,
                started_at: now,
                duration: curve.duration.mul_f32(share),
                share,
            };
            window.request_animation_frame();
            return value;
        }
        if glide.from != glide.target {
            if sample.finished {
                glide.from = glide.target.clone();
            } else {
                window.request_animation_frame();
            }
        }
        value
    }
}

/// One of several layouts cross-fading in a region whose height glides; see
/// [`Owned::swap`].
pub(crate) struct Swap {
    fade: CrossFade,
    height: super::GlideState,
    current: usize,
}

impl Swap {
    /// Whether `layer` is drawn this frame, so only those are built.
    pub(crate) fn shows(&self, layer: usize) -> bool {
        self.fade.shows(layer)
    }

    /// The region holding `layers`, each tagged with its index.
    pub(crate) fn render(
        self,
        layers: impl IntoIterator<Item = (usize, gpui::AnyElement)>,
    ) -> gpui::AnyElement {
        use gpui::{IntoElement as _, Styled as _};
        super::SizeGlide::new(&self.height, self.fade.render(layers).w_full())
            .on_change(self.current as u64)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use gpui::{
        Context, InteractiveElement as _, IntoElement, ParentElement as _, Render, Styled as _,
        TestAppContext, VisualTestContext, Window, div, px, size,
    };
    use gpui_base::{h_flex, v_flex};

    use super::Owned;
    use crate::test_frames::{draw, draw_settled};

    /// A row that a window turning compact draws under another parent, as
    /// the shell draws a settings page: a label and a control beside it, or
    /// the control under it, over a neighbour whose place follows the row's
    /// height; the compact layout also swaps a short note for a tall one.
    struct Page {
        motion: Owned,
        compact: bool,
    }

    impl Render for Page {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let compact = self.compact;
            let reflow = self.motion.reflow("row", u64::from(compact));
            let control = reflow.part(
                "control",
                div()
                    .debug_selector(|| "control".into())
                    .flex_none()
                    .w(px(100.))
                    .h(px(20.)),
            );
            let label = div().flex_none().w(px(100.)).h(px(20.));
            let row = if compact {
                v_flex().w_full().child(label).child(control)
            } else {
                h_flex()
                    .w_full()
                    .justify_between()
                    .child(label)
                    .child(control)
            };
            let swap = self
                .motion
                .swap("note", usize::from(compact), 2, window, cx);
            let layers = (0..2)
                .filter(|layer| swap.shows(*layer))
                .map(|layer| {
                    (
                        layer,
                        div()
                            .w_full()
                            .h(px(20. + 60. * layer as f32))
                            .into_any_element(),
                    )
                })
                .collect::<Vec<_>>();
            let column = v_flex()
                .w(px(400.))
                .child(
                    div()
                        .debug_selector(|| "row".into())
                        .child(reflow.render(row)),
                )
                .child(swap.render(layers))
                .child(div().debug_selector(|| "below".into()).h(px(10.)));
            if compact {
                div().id("phone").child(column).into_any_element()
            } else {
                div()
                    .id("tablet")
                    .child(div().id("split").child(column))
                    .into_any_element()
            }
        }
    }

    /// Under its new parent the control starts where the other layout drew
    /// it and glides to its place, and what is below the swapped note
    /// glides with the note's height, rather than either starting over;
    /// with motion reduced both are there at once.
    #[gpui::test]
    fn a_layout_drawn_under_a_new_parent_glides_from_the_last(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let (page, cx) = cx.add_window_view(|_, _| Page {
            motion: Owned::default(),
            compact: false,
        });
        cx.simulate_resize(size(px(400.), px(400.)));
        draw_settled(cx);
        let at = |name: &'static str, cx: &mut VisualTestContext| {
            let row = cx.debug_bounds("row").expect("the row is drawn").origin;
            cx.debug_bounds(name).expect("drawn").origin - row
        };
        let beside = at("control", cx);
        let above = at("below", cx);
        let turn = |compact: bool, cx: &mut VisualTestContext| {
            page.update(cx, |page, cx| {
                page.compact = compact;
                cx.notify();
            });
            draw(cx);
        };

        turn(true, cx);
        assert_eq!(at("control", cx), beside, "the control starts where it was");
        cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
        draw(cx);
        let (control, below) = (at("control", cx), at("below", cx));
        draw_settled(cx);
        let (under, lower) = (at("control", cx), at("below", cx));
        assert!(under.y > beside.y && under.x < beside.x);
        assert!(
            control.x < beside.x && control.x > under.x && control.y > beside.y,
            "the control glides: {control:?}"
        );
        assert!(
            below.y > above.y && below.y < lower.y,
            "what is below glides with the note: {below:?}"
        );

        cx.update(|_, cx| cx.set_reduce_motion(true));
        turn(false, cx);
        assert_eq!(at("control", cx), beside);
        assert_eq!(at("below", cx), above);
    }
}
