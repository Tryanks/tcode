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

use gpui::{App, Window};
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
type Key = (&'static str, usize);

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
        key: &'static str,
        target: T,
        curve: &Curve,
        window: &mut Window,
        cx: &App,
    ) -> T
    where
        T: Interpolate + PartialEq + 'static,
    {
        self.sample((key, 0), target, curve, window, cx)
    }

    /// [`CrossFade`] with its layers' opacities kept here: layer `current`
    /// of `count` fades in over `curve` while the others fade out. `key`
    /// is also the region's element id.
    pub(crate) fn cross_fade(
        &self,
        key: &'static str,
        current: usize,
        count: usize,
        curve: &Curve,
        window: &mut Window,
        cx: &App,
    ) -> CrossFade {
        let opacity = (0..count)
            .map(|layer| {
                let target = if layer == current { 1. } else { 0. };
                self.sample((key, layer), target, curve, window, cx)
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
        key: &'static str,
        value: Option<T>,
        window: &mut Window,
        cx: &App,
    ) -> Option<super::Shown<T>> {
        let target = if value.is_some() { 1f32 } else { 0. };
        let progress = self.sample((key, 0), target, &Curve::settle(), window, cx);
        let mut values = self.0.borrow_mut();
        let measure = values
            .entry((key, 2))
            .or_insert_with(|| Box::new(super::Measure::default()))
            .downcast_ref::<super::Measure>()
            .expect("a fold's measurement")
            .clone();
        let kept = values
            .entry((key, 1))
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
