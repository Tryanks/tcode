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
pub(crate) struct Owned {
    values: Rc<RefCell<HashMap<Key, Entry>>>,
    /// What this handle's keys are kept under: nothing for the store's own,
    /// a part's name for [`Self::scope`]'s.
    prefix: SharedString,
}

/// A value's key and, for a cross-fade, its layer.
type Key = (SharedString, usize);

struct Entry {
    value: Box<dyn Any>,
    /// Sampled or kept since the last [`Owned::sweep`].
    seen: bool,
}

/// The channel of a key [`Owned::keep`] holds a handle under, apart from
/// every layer of a glide.
const KEPT: usize = usize::MAX;

/// The channel of a key [`Owned::cross_fade_value`] keeps its values under.
const CHANGED: usize = usize::MAX - 1;

/// The channel of a key [`Owned::follow`] keeps its spring under.
const FOLLOWED: usize = usize::MAX - 2;

/// A spring [`Owned::follow`] keeps: where it is, how fast it moves, where
/// it was heading over the frame that last advanced it, and when that was.
struct Follow {
    state: gpui::SpringState,
    target: f32,
    updated_at: Instant,
}

struct Glide<T> {
    from: T,
    target: T,
    started_at: Instant,
    duration: Duration,
    /// The share of a full glide this one takes: below one when it turned
    /// back part of the way, so it returns in the time it had travelled.
    share: f32,
}

/// What [`Owned::cross_fade_value`] keeps: the value shown, and the one it
/// replaced while that fades out.
struct Change<T> {
    current: T,
    generation: u64,
    previous: Option<(T, u64)>,
    started_at: Instant,
}

impl Owned {
    /// The same store, its keys under `part`: for parts of a view drawn
    /// more than once, or by code that names its keys without knowing its
    /// neighbours' (a message's Markdown, a row drawn again as it leaves).
    pub(crate) fn scope(&self, part: impl AsRef<str>) -> Self {
        Self {
            values: self.values.clone(),
            prefix: format!("{}{}/", self.prefix, part.as_ref()).into(),
        }
    }

    /// Forgets every value not sampled or kept since the last sweep, as
    /// element state forgets an element not drawn: for a view whose parts
    /// come and go by key (a timeline's rows scrolled away), which sweeps
    /// as it starts to render.
    /// Something drawn again after a sweep starts at rest, as one drawn for
    /// the first time does.
    pub(crate) fn sweep(&self) {
        self.values.borrow_mut().retain(|_, entry| {
            let seen = entry.seen;
            entry.seen = false;
            seen
        });
    }

    /// Forgets every value, whatever scope keeps it: for a view that starts
    /// a new scene (a conversation's view showing another thread), where
    /// what it draws next takes its first target at once, as the first
    /// thing a view draws does, rather than gliding from what the last
    /// scene drew under the same key.
    pub(crate) fn clear(&self) {
        self.values.borrow_mut().clear();
    }

    /// Forgets `key`'s values, every layer's and channel's: what it draws
    /// next takes its target at once, for a part whose owner shows its
    /// changes some other way (a view a host holds unseen, to fade it in
    /// itself).
    pub(crate) fn forget(&self, key: impl Into<SharedString>) {
        let key = self.key(key);
        self.values.borrow_mut().retain(|(held, _), _| *held != key);
    }

    fn key(&self, key: impl Into<SharedString>) -> SharedString {
        let key = key.into();
        if self.prefix.is_empty() {
            key
        } else {
            format!("{}{key}", self.prefix).into()
        }
    }

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
        self.sample((self.key(key), 0), target, curve, window, cx)
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
        let key = self.key(key);
        let opacity = (0..count)
            .map(|layer| {
                let target = if layer == current { 1. } else { 0. };
                self.sample((key.clone(), layer), target, curve, window, cx)
            })
            .collect();
        CrossFade::sampled(key, current, opacity)
    }

    /// [`super::cross_fade`] with the values it shows and its clock kept
    /// here: content that changes to another of its states fades in over
    /// the one it showed, which takes no input while it goes. `key` also
    /// names its elements.
    pub(crate) fn cross_fade_value<T, C>(
        &self,
        key: impl Into<SharedString>,
        value: T,
        window: &mut Window,
        cx: &mut C,
        build: impl FnMut(&T, &mut Window, &mut C) -> gpui::AnyElement,
    ) -> gpui::AnyElement
    where
        T: Clone + PartialEq + 'static,
        C: std::borrow::BorrowMut<App>,
    {
        self.change(key.into(), value, true, window, cx, build)
    }

    /// [`super::cross_fade_part`] kept here: [`Self::cross_fade_value`] for
    /// a part of a control (its glyph, its status), whose leaving state
    /// covers nothing, so a press during the fade still reaches the control.
    pub(crate) fn cross_fade_part<T, C>(
        &self,
        key: impl Into<SharedString>,
        value: T,
        window: &mut Window,
        cx: &mut C,
        build: impl FnMut(&T, &mut Window, &mut C) -> gpui::AnyElement,
    ) -> gpui::AnyElement
    where
        T: Clone + PartialEq + 'static,
        C: std::borrow::BorrowMut<App>,
    {
        self.change(key.into(), value, false, window, cx, build)
    }

    fn change<T, C>(
        &self,
        key: SharedString,
        value: T,
        shield: bool,
        window: &mut Window,
        cx: &mut C,
        mut build: impl FnMut(&T, &mut Window, &mut C) -> gpui::AnyElement,
    ) -> gpui::AnyElement
    where
        T: Clone + PartialEq + 'static,
        C: std::borrow::BorrowMut<App>,
    {
        let key = self.key(key);
        let (now, reduce) = {
            let app: &mut App = cx.borrow_mut();
            (app.background_executor().now(), app.reduce_motion())
        };
        let (current, generation, previous, progress) = {
            let mut values = self.values.borrow_mut();
            let entry = Self::entry(&mut values, (key.clone(), CHANGED), || Change {
                current: value.clone(),
                generation: 0,
                previous: None,
                started_at: now,
            });
            if !entry.is::<Change<T>>() {
                *entry = Box::new(Change {
                    current: value.clone(),
                    generation: 0,
                    previous: None::<(T, u64)>,
                    started_at: now,
                });
            }
            let change = entry.downcast_mut::<Change<T>>().expect("checked above");
            if change.current != value {
                let current = std::mem::replace(&mut change.current, value.clone());
                change.previous = Some((current, change.generation));
                change.generation += 1;
                change.started_at = now;
            }
            let progress = if change.previous.is_none() || reduce {
                1.
            } else {
                super::settle_progress(now.saturating_duration_since(change.started_at))
            };
            if progress >= 1. {
                change.previous = None;
            }
            (
                change.current.clone(),
                change.generation,
                change.previous.clone(),
                progress,
            )
        };
        if progress < 1. {
            window.request_animation_frame();
        }
        let current = build(&current, window, cx);
        let previous = previous.map(|(value, generation)| (build(&value, window, cx), generation));
        super::draw_change(
            key.into(),
            (current, generation),
            previous,
            progress,
            shield,
        )
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
        let key = self.key(key);
        let target = if value.is_some() { 1f32 } else { 0. };
        let progress = self.sample((key.clone(), 0), target, &Curve::settle(), window, cx);
        let mut values = self.values.borrow_mut();
        let measure = Self::entry(&mut values, (key.clone(), 2), super::Measure::default)
            .downcast_ref::<super::Measure>()
            .expect("a fold's measurement")
            .clone();
        let kept = Self::entry(&mut values, (key.clone(), 1), || None::<T>)
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
        let key = (self.key(key), 0);
        let curve = Curve::settle();
        Self::entry(&mut self.values.borrow_mut(), key.clone(), || Glide {
            from: 0f32,
            target: 0f32,
            started_at: cx.background_executor().now(),
            duration: curve.duration,
            share: 1.,
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
        let key = self.key(key);
        let mut values = self.values.borrow_mut();
        let kept = Self::entry(&mut values, (key.clone(), 1), || None::<T>)
            .downcast_mut::<Option<T>>()
            .expect("an entered value of one type");
        let changed = kept.as_ref().is_some_and(|kept| *kept != value);
        *kept = Some(value);
        if changed {
            *Self::entry(&mut values, (key.clone(), 0), || ()) = Box::new(Glide {
                from: 0f32,
                target: 0f32,
                started_at: cx.background_executor().now(),
                duration: Curve::settle().duration,
                share: 1.,
            });
        }
        drop(values);
        self.sample((key, 0), 1f32, &Curve::settle(), window, cx)
    }

    /// The handle kept under `key`: made the first time it is asked for,
    /// the same one every time after, for state a sampler shares through
    /// clones (a [`super::GlideState`], a [`super::reflow::ReflowState`]).
    pub(crate) fn keep<T: Clone + Default + 'static>(&self, key: impl Into<SharedString>) -> T {
        let key = (self.key(key), KEPT);
        let mut values = self.values.borrow_mut();
        let entry = Self::entry(&mut values, key, T::default);
        if !entry.is::<T>() {
            *entry = Box::new(T::default());
        }
        entry.downcast_ref::<T>().expect("checked above").clone()
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
            .scope_in(self.key(key), layout, window, cx)
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
            axes: super::GlideAxes::Height,
        }
    }

    /// gpui-base's `spring` on [`super::FOLLOW`] with its state kept here:
    /// the value now of a position springing toward `target`, which a new
    /// target redirects with the velocity it has. The first target is taken
    /// at once; reduced motion takes every target at once. gpui-base keeps a
    /// spring's state by element path and builds its physics privately, so
    /// this steps GPUI's spring with the parameters `FOLLOW` names (its
    /// response, critically damped, its tolerance), as gpui-base steps it.
    pub(crate) fn follow(
        &self,
        key: impl Into<SharedString>,
        target: f32,
        window: &mut Window,
        cx: &App,
    ) -> f32 {
        let now = cx.background_executor().now();
        let mut values = self.values.borrow_mut();
        let at_rest = |target: f32| Follow {
            state: gpui::SpringState {
                position: target,
                velocity: 0.,
            },
            target,
            updated_at: now,
        };
        let entry = Self::entry(&mut values, (self.key(key), FOLLOWED), || at_rest(target));
        if !entry.is::<Follow>() {
            *entry = Box::new(at_rest(target));
        }
        let follow = entry.downcast_mut::<Follow>().expect("checked above");
        let resting = follow.state.position == target && follow.state.velocity == 0.;
        if resting || cx.reduce_motion() {
            *follow = at_rest(target);
            return target;
        }
        let frequency = std::f32::consts::TAU / crate::edge_back::SETTLE.as_secs_f32();
        let config = gpui::SpringConfig::new(frequency * frequency, 2. * frequency, 1.);
        let elapsed = now
            .saturating_duration_since(follow.updated_at)
            .as_secs_f32();
        let stepped = config.step(follow.state, follow.target, elapsed);
        if config.is_settled(stepped, target, super::FOLLOW.epsilon()) {
            *follow = at_rest(target);
            return target;
        }
        *follow = Follow {
            state: stepped,
            target,
            updated_at: now,
        };
        window.request_animation_frame();
        stepped.position
    }

    /// The value under `key`, made by `init` the first time, marked seen.
    fn entry<T: 'static>(
        values: &mut HashMap<Key, Entry>,
        key: Key,
        init: impl FnOnce() -> T,
    ) -> &mut Box<dyn Any> {
        let entry = values.entry(key).or_insert_with(|| Entry {
            value: Box::new(init()),
            seen: true,
        });
        entry.seen = true;
        &mut entry.value
    }

    fn sample<T>(&self, key: Key, target: T, curve: &Curve, window: &mut Window, cx: &App) -> T
    where
        T: Interpolate + PartialEq + 'static,
    {
        let now = cx.background_executor().now();
        let mut values = self.values.borrow_mut();
        let at_rest = |target: &T| Glide {
            from: target.clone(),
            target: target.clone(),
            started_at: now,
            duration: curve.duration,
            share: 1.,
        };
        let entry = Self::entry(&mut values, key, || at_rest(&target));
        if !entry.is::<Glide<T>>() {
            *entry = Box::new(at_rest(&target));
        }
        let glide = entry.downcast_mut::<Glide<T>>().expect("checked above");
        if cx.reduce_motion() || curve.duration.is_zero() {
            if glide.from != target || glide.target != target {
                *glide = at_rest(&target);
            }
            return target;
        }
        let sample = Timing::new(glide.duration)
            .ease(curve.easing.clone())
            .sample(now.saturating_duration_since(glide.started_at));
        // A finished glide is at its target exactly, as gpui-base's is, not
        // wherever interpolating all the way rounds to.
        let value = if sample.finished {
            glide.target.clone()
        } else {
            glide
                .from
                .interpolate(&glide.target, sample.directed_progress)
        };
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
    axes: super::GlideAxes,
}

impl Swap {
    /// Whether `layer` is drawn this frame, so only those are built.
    pub(crate) fn shows(&self, layer: usize) -> bool {
        self.fade.shows(layer)
    }

    /// The swap of layouts side by side with others in a row (a toolbar's
    /// controls for their labelled selves): the region's width glides, not
    /// its height, so what is beside it moves over as it does.
    pub(crate) fn across(mut self) -> Self {
        self.axes = super::GlideAxes::Width;
        self
    }

    /// The region holding `layers`, each tagged with its index.
    pub(crate) fn render(
        self,
        layers: impl IntoIterator<Item = (usize, gpui::AnyElement)>,
    ) -> gpui::AnyElement {
        use gpui::{IntoElement as _, Styled as _};
        let region = self.fade.render(layers);
        let region = match self.axes {
            super::GlideAxes::Height => region.w_full(),
            _ => region.flex_none(),
        };
        super::SizeGlide::new(&self.height, region)
            .axes(self.axes)
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
