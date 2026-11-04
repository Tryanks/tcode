//! Where motion keeps its clocks: in element state, or in a store a view
//! holds, so it survives the element tree moving the view.
//!
//! gpui-base's `transition` (and so every sampler built on it) keeps its
//! clock in keyed element state, keyed by the full id path from the window
//! root. A view drawn under another parent starts all of it over at its
//! target: gpui-base's dock re-parents a tab group whenever its tree changes
//! (see `panes::ThreadPanes`), so a pane's fade under way when its group
//! moved snapped to its end. [`Owned`] keeps the same clocks in a store the
//! view holds, sampled with gpui-base's `Timing` against the executor's
//! clock, timing a glide and a reversal by the rule gpui-base's `transition`
//! uses, and settled at once under reduced motion as `transition` settles.
//! Every sampler is written once over a [`Clock`]; its `Element` arm is
//! gpui-base's own. Remove [`Owned`] once gpui-base's samplers can key their
//! state by an owner instead of an element path.

use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;
#[cfg(not(target_family = "wasm"))]
use std::time::Instant;

use gpui::{AnyElement, App, ElementId, SharedString, Window};
use gpui_base::motion::{
    Interpolate, MotionStatus, Presence, PresencePhase, PresenceSample, Timing,
};
#[cfg(target_family = "wasm")]
use web_time::Instant;

use super::{CrossFade, Curve, Measure, Shown, child_id};

/// Where a sampler keeps its clocks and what it remembers between frames.
#[derive(Clone, Copy)]
pub(crate) enum Clock<'a> {
    /// In keyed element state under the sampling element's id path, as
    /// gpui-base keeps its own.
    Element,
    /// In a view's store, so motion under way carries on when the view is
    /// drawn under a new parent.
    Owned(&'a Owned),
}

impl<'a> From<&'a Owned> for Clock<'a> {
    fn from(owned: &'a Owned) -> Self {
        Self::Owned(owned)
    }
}

impl<'a> From<Option<&'a Owned>> for Clock<'a> {
    fn from(owned: Option<&'a Owned>) -> Self {
        owned.map_or(Self::Element, Self::Owned)
    }
}

impl Clock<'_> {
    /// The value now of a glide toward `target` along `curve`, and whether
    /// it is at rest: the first target is taken at once; a later one is
    /// glided to from wherever the value is when it changes.
    pub(crate) fn glide<T>(
        self,
        id: impl Into<ElementId>,
        target: T,
        curve: &Curve,
        window: &mut Window,
        cx: &mut App,
    ) -> (T, bool)
    where
        T: Interpolate + PartialEq + 'static,
    {
        match self {
            Self::Element => {
                let sample = gpui_base::motion::transition_with_status(
                    id.into(),
                    target,
                    curve.transition(),
                    window,
                    cx,
                );
                let moving = matches!(sample.status, MotionStatus::Delayed | MotionStatus::Running);
                (sample.value, !moving)
            }
            Self::Owned(owned) => owned.glide(owned.key(id), target, curve, window, cx),
        }
    }

    /// [`Self::glide`]'s value alone.
    pub(crate) fn transition<T>(
        self,
        id: impl Into<ElementId>,
        target: T,
        curve: &Curve,
        window: &mut Window,
        cx: &mut App,
    ) -> T
    where
        T: Interpolate + PartialEq + 'static,
    {
        self.glide(id, target, curve, window, cx).0
    }

    /// gpui-base's presence: how far in something that comes and goes is,
    /// coming in from nothing the first time it is present.
    pub(crate) fn appear(
        self,
        id: impl Into<ElementId>,
        present: bool,
        curve: &Curve,
        window: &mut Window,
        cx: &mut App,
    ) -> PresenceSample {
        match self {
            Self::Element => Presence::new(id.into(), present)
                .transition(curve.transition())
                .sample(window, cx),
            Self::Owned(owned) => {
                let key = owned.key(id);
                owned.seed(key.clone(), 0f32, cx.background_executor().now());
                let target = if present { 1f32 } else { 0. };
                let (progress, settled) = owned.glide(key, target, curve, window, cx);
                let phase = match (present, settled) {
                    (true, true) => PresencePhase::Present,
                    (true, false) => PresencePhase::Entering,
                    (false, false) => PresencePhase::Exiting,
                    (false, true) => PresencePhase::Absent,
                };
                PresenceSample {
                    phase,
                    progress,
                    status: if settled {
                        MotionStatus::Finished
                    } else {
                        MotionStatus::Running
                    },
                }
            }
        }
    }

    /// The handle kept under `id`: made the first time it is asked for, the
    /// same one every time after, for state a sampler shares through clones.
    pub(crate) fn keep<T: Clone + Default + 'static>(
        self,
        id: impl Into<ElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> T {
        match self {
            Self::Element => window
                .use_keyed_state(id.into(), cx, |_, _| T::default())
                .read(cx)
                .clone(),
            Self::Owned(owned) => owned.kept(owned.key(id)),
        }
    }

    /// gpui-base's `spring` for a position in pixels, responding over
    /// `response` and at rest a quarter pixel away: the value now of a
    /// position springing toward `target`, which a new target redirects with
    /// the velocity it has. The first target is taken at once; reduced
    /// motion takes every target at once.
    pub(crate) fn follow(
        self,
        id: impl Into<ElementId>,
        target: f32,
        response: Duration,
        window: &mut Window,
        cx: &mut App,
    ) -> f32 {
        match self {
            Self::Element => gpui_base::motion::spring(
                id.into(),
                target,
                gpui_base::motion::Spring::new(response).with_epsilon(FOLLOW_EPSILON),
                window,
                cx,
            ),
            Self::Owned(owned) => owned.follow(owned.key(id), target, response, window, cx),
        }
    }

    /// Something a page shows only some of the time: what to draw, and how
    /// far in it is, along `curve`. Once `value` is `None` the last value it
    /// had is kept until it has gone, so whatever leaves is drawn as it was.
    /// A page that opens with it shows it at rest. The caller draws the
    /// result through [`Shown::fold`] or [`Shown::fade`] whenever this is
    /// `Some`, at the same place in the tree, so the element keeps its ids
    /// while it comes and goes.
    pub(crate) fn shown<T: Clone + PartialEq + 'static>(
        self,
        id: impl Into<ElementId>,
        value: Option<T>,
        curve: &Curve,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Shown<T>> {
        let id = id.into();
        let target = if value.is_some() { 1f32 } else { 0. };
        let progress = self.transition(child_id(&id, "shown"), target, curve, window, cx);
        let kept: Rc<RefCell<Option<T>>> = self.keep(child_id(&id, "shown-value"), window, cx);
        let measure = match self {
            Self::Element => None,
            Self::Owned(_) => Some(self.keep::<Measure>(child_id(&id, "measure"), window, cx)),
        };
        let mut kept = kept.borrow_mut();
        let (value, leaving) = match value {
            Some(value) => {
                if kept.as_ref() != Some(&value) {
                    *kept = Some(value.clone());
                }
                (value, false)
            }
            None if progress > 0. => (kept.clone()?, true),
            None => {
                *kept = None;
                return None;
            }
        };
        Some(Shown {
            id,
            value,
            progress,
            leaving,
            measure,
        })
    }

    /// How far in, from 0 to 1, what `value` names is: a value other than
    /// the last one asked about comes in along `curve`, the first is in at
    /// once. For something drawn in another's place (a section, a page) that
    /// fades in.
    pub(crate) fn entered<T: PartialEq + 'static>(
        self,
        id: impl Into<ElementId>,
        value: T,
        curve: &Curve,
        window: &mut Window,
        cx: &mut App,
    ) -> f32 {
        let id = id.into();
        let kept: Rc<RefCell<Option<(T, u64)>>> =
            self.keep(child_id(&id, "entered-value"), window, cx);
        let (generation, changed) = {
            let mut kept = kept.borrow_mut();
            match &mut *kept {
                Some((held, generation)) if *held != value => {
                    *held = value;
                    *generation += 1;
                    (*generation, true)
                }
                Some((_, generation)) => (*generation, false),
                None => {
                    *kept = Some((value, 0));
                    (0, false)
                }
            }
        };
        let clock = |generation: u64| child_id(&id, format!("entered-{generation}"));
        if changed {
            // Each value has a clock of its own, which starts from nothing.
            if let Self::Owned(owned) = self {
                owned.forget_exact(owned.key(clock(generation - 1)));
            }
            self.transition(clock(generation), 0f32, curve, window, cx);
        }
        self.transition(clock(generation), 1f32, curve, window, cx)
    }

    /// Layer `current` of `count` coming in along `curve` while the others
    /// fade out: a [`CrossFade`] over this clock. `id` is also the region's
    /// element id.
    pub(crate) fn layers(
        self,
        id: impl Into<ElementId>,
        current: usize,
        count: usize,
        curve: &Curve,
        window: &mut Window,
        cx: &mut App,
    ) -> CrossFade {
        let id = id.into();
        let opacity = (0..count)
            .map(|layer| {
                let target = if layer == current { 1. } else { 0. };
                self.transition(
                    child_id(&id, format!("layer-{layer}")),
                    target,
                    curve,
                    window,
                    cx,
                )
            })
            .collect();
        let at_rest = self.keep(child_id(&id, "at-rest"), window, cx);
        CrossFade::sampled(id, current, opacity).at_rest(at_rest)
    }

    /// Content that changes to another of its states (a status, a QR for a
    /// new link) cross-fading along `curve` from the one it showed. Each
    /// state is drawn under its own slot, which it keeps while it fades out
    /// on top of the next, taking no input. `cx` is the caller's own
    /// context, so a view's `build` can make listeners.
    pub(crate) fn cross_fade<T: Clone + PartialEq + 'static, C: std::borrow::BorrowMut<App>>(
        self,
        id: impl Into<ElementId>,
        value: T,
        curve: &Curve,
        window: &mut Window,
        cx: &mut C,
        build: impl FnMut(&T, &mut Window, &mut C) -> AnyElement,
    ) -> AnyElement {
        self.swap(id.into(), value, true, curve, window, cx, build)
    }

    /// [`Self::cross_fade`] for a part of a control (its glyph, its status),
    /// which takes no input of its own: what leaves covers nothing, so a
    /// press during the fade still reaches the control.
    pub(crate) fn cross_fade_part<
        T: Clone + PartialEq + 'static,
        C: std::borrow::BorrowMut<App>,
    >(
        self,
        id: impl Into<ElementId>,
        value: T,
        curve: &Curve,
        window: &mut Window,
        cx: &mut C,
        build: impl FnMut(&T, &mut Window, &mut C) -> AnyElement,
    ) -> AnyElement {
        self.swap(id.into(), value, false, curve, window, cx, build)
    }

    #[allow(clippy::too_many_arguments)]
    fn swap<T: Clone + PartialEq + 'static, C: std::borrow::BorrowMut<App>>(
        self,
        id: ElementId,
        value: T,
        shield: bool,
        curve: &Curve,
        window: &mut Window,
        cx: &mut C,
        mut build: impl FnMut(&T, &mut Window, &mut C) -> AnyElement,
    ) -> AnyElement {
        let app: &mut App = std::borrow::BorrowMut::borrow_mut(cx);
        let state: Rc<RefCell<Option<Change<T>>>> =
            self.keep(child_id(&id, "cross-fade"), window, app);
        let at_rest: super::AtRest = self.keep(child_id(&id, "cross-fade-at-rest"), window, app);
        let generation = {
            let mut state = state.borrow_mut();
            let change = state.get_or_insert_with(|| Change {
                current: value.clone(),
                generation: 0,
                previous: None,
            });
            if change.current != value {
                let current = std::mem::replace(&mut change.current, value);
                change.previous = Some((current, change.generation));
                change.generation += 1;
            }
            change.generation
        };
        let progress = self.entered(
            child_id(&id, "cross-fade-in"),
            generation,
            curve,
            window,
            app,
        );
        let (current, previous) = {
            let mut state = state.borrow_mut();
            let change = state.as_mut().expect("kept above");
            if progress >= 1. {
                change.previous = None;
            }
            (change.current.clone(), change.previous.clone())
        };
        let current = build(&current, window, cx);
        let previous = previous.map(|(value, generation)| (build(&value, window, cx), generation));
        super::draw_change(
            id,
            (current, generation),
            previous,
            progress,
            shield,
            &at_rest,
        )
    }
}

/// How near its target, in pixels, [`Clock::follow`]'s spring comes to rest.
const FOLLOW_EPSILON: f32 = 0.25;

/// What [`Clock::cross_fade`] keeps: the value shown, and the one it
/// replaced while that fades out.
struct Change<T> {
    current: T,
    generation: u64,
    previous: Option<(T, u64)>,
}

/// The motion state of one view, by key: clone it into whatever samples it
/// (a renderer drawing the view's frame); every clone is the same store.
#[derive(Clone, Default)]
pub(crate) struct Owned {
    values: Rc<RefCell<HashMap<SharedString, Entry>>>,
    /// What this handle's keys are kept under: nothing for the store's own,
    /// a part's name for [`Self::scope`]'s.
    prefix: SharedString,
    /// The scene the store keeps the motion of, when it is a surface's.
    scene: super::InScene,
}

struct Entry {
    value: Box<dyn Any>,
    /// Sampled or kept since the last [`Owned::sweep`].
    seen: bool,
}

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
    /// gpui-base's `reversing_factor`: the share of a full glide this one
    /// takes, below one when it turned back part of the way, so it returns
    /// in the time it had travelled.
    share: f32,
}

impl Owned {
    /// The store of a surface in `epoch`: what it draws in a new scene
    /// takes its first target at once, as the first thing a view draws
    /// does, rather than gliding from what the last scene drew under the
    /// same key.
    pub(crate) fn in_scene(epoch: &super::Epoch) -> Self {
        Self {
            scene: super::InScene::new(epoch),
            ..Self::default()
        }
    }

    /// Every value, the last scene's forgotten.
    fn values(&self) -> std::cell::RefMut<'_, HashMap<SharedString, Entry>> {
        let mut values = self.values.borrow_mut();
        if self.scene.entered() {
            values.clear();
        }
        values
    }

    /// This store as the clock its samplers keep.
    pub(crate) fn clock(&self) -> Clock<'_> {
        Clock::Owned(self)
    }

    /// The same store, its keys under `part`: for parts of a view drawn
    /// more than once, or by code that names its keys without knowing its
    /// neighbours' (a message's Markdown, a row drawn again as it leaves).
    pub(crate) fn scope(&self, part: impl AsRef<str>) -> Self {
        Self {
            values: self.values.clone(),
            prefix: format!("{}{}/", self.prefix, part.as_ref()).into(),
            scene: self.scene.clone(),
        }
    }

    /// Forgets every value not sampled or kept since the last sweep, as
    /// element state forgets an element not drawn: for a view whose parts
    /// come and go by key (a timeline's rows scrolled away), which sweeps
    /// as it starts to render.
    /// Something drawn again after a sweep starts at rest, as one drawn for
    /// the first time does.
    pub(crate) fn sweep(&self) {
        self.values().retain(|_, entry| {
            let seen = entry.seen;
            entry.seen = false;
            seen
        });
    }

    /// Forgets `key`'s values, every layer's and channel's: what it draws
    /// next takes its target at once, for a part whose owner shows its
    /// changes some other way (a view a host holds unseen, to fade it in
    /// itself).
    pub(crate) fn forget(&self, key: impl Into<SharedString>) {
        let key = self.key(key.into());
        let channel = format!("{key}-");
        self.values()
            .retain(|held, _| *held != key && !held.starts_with(&channel));
    }

    pub(super) fn forget_exact(&self, key: SharedString) {
        self.values().remove(&key);
    }

    /// The key `id` is kept under in this handle's scope.
    fn key(&self, id: impl Into<ElementId>) -> SharedString {
        let id = id.into();
        if self.prefix.is_empty() {
            id.to_string().into()
        } else {
            format!("{}{id}", self.prefix).into()
        }
    }

    /// [`Clock::transition`] kept here.
    pub(crate) fn transition<T>(
        &self,
        key: impl Into<SharedString>,
        target: T,
        curve: impl Into<Curve>,
        window: &mut Window,
        cx: &App,
    ) -> T
    where
        T: Interpolate + PartialEq + 'static,
    {
        self.glide(self.key(key.into()), target, &curve.into(), window, cx)
            .0
    }

    /// [`Clock::layers`] kept here.
    pub(crate) fn layers(
        &self,
        key: impl Into<SharedString>,
        current: usize,
        count: usize,
        curve: impl Into<Curve>,
        window: &mut Window,
        cx: &mut App,
    ) -> CrossFade {
        self.clock()
            .layers(key.into(), current, count, &curve.into(), window, cx)
    }

    /// [`Clock::cross_fade`] kept here.
    pub(crate) fn cross_fade<T, C>(
        &self,
        key: impl Into<SharedString>,
        value: T,
        curve: impl Into<Curve>,
        window: &mut Window,
        cx: &mut C,
        build: impl FnMut(&T, &mut Window, &mut C) -> AnyElement,
    ) -> AnyElement
    where
        T: Clone + PartialEq + 'static,
        C: std::borrow::BorrowMut<App>,
    {
        self.clock()
            .cross_fade(key.into(), value, &curve.into(), window, cx, build)
    }

    /// [`Clock::cross_fade_part`] kept here.
    pub(crate) fn cross_fade_part<T, C>(
        &self,
        key: impl Into<SharedString>,
        value: T,
        curve: impl Into<Curve>,
        window: &mut Window,
        cx: &mut C,
        build: impl FnMut(&T, &mut Window, &mut C) -> AnyElement,
    ) -> AnyElement
    where
        T: Clone + PartialEq + 'static,
        C: std::borrow::BorrowMut<App>,
    {
        self.clock()
            .cross_fade_part(key.into(), value, &curve.into(), window, cx, build)
    }

    /// [`Clock::shown`] kept here, its fold's measurement too.
    pub(crate) fn shown<T: Clone + PartialEq + 'static>(
        &self,
        key: impl Into<SharedString>,
        value: Option<T>,
        curve: impl Into<Curve>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Shown<T>> {
        self.clock()
            .shown(key.into(), value, &curve.into(), window, cx)
    }

    /// [`Clock::appear`]'s progress kept here.
    pub(crate) fn appear(
        &self,
        key: impl Into<SharedString>,
        present: bool,
        curve: impl Into<Curve>,
        window: &mut Window,
        cx: &mut App,
    ) -> f32 {
        self.clock()
            .appear(key.into(), present, &curve.into(), window, cx)
            .progress
    }

    /// [`Clock::entered`] kept here.
    pub(crate) fn entered<T: PartialEq + 'static>(
        &self,
        key: impl Into<SharedString>,
        value: T,
        curve: impl Into<Curve>,
        window: &mut Window,
        cx: &mut App,
    ) -> f32 {
        self.clock()
            .entered(key.into(), value, &curve.into(), window, cx)
    }

    /// The handle kept under `key`: made the first time it is asked for,
    /// the same one every time after, for state a sampler shares through
    /// clones (a [`super::GlideState`], a [`super::reflow::ReflowState`]).
    pub(crate) fn keep<T: Clone + Default + 'static>(&self, key: impl Into<SharedString>) -> T {
        self.kept(self.key(key.into()))
    }

    fn kept<T: Clone + Default + 'static>(&self, key: SharedString) -> T {
        let mut values = self.values();
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
        curve: impl Into<Curve>,
        window: &mut Window,
        cx: &mut App,
    ) -> Swap {
        let key = key.into();
        Swap {
            fade: self
                .layers(key.clone(), current, count, curve, window, cx)
                .span(),
            height: self.glide_state(key),
            current,
            axes: super::GlideAxes::Height,
        }
    }

    /// [`Clock::follow`] kept here. gpui-base keeps a spring's state by
    /// element path and builds its physics privately, so this steps GPUI's
    /// spring with the parameters gpui-base's `Spring::new(response)` names
    /// (its response, critically damped), as gpui-base steps it.
    fn follow(
        &self,
        key: SharedString,
        target: f32,
        response: Duration,
        window: &mut Window,
        cx: &App,
    ) -> f32 {
        let now = cx.background_executor().now();
        let mut values = self.values();
        let at_rest = |target: f32| Follow {
            state: gpui::SpringState {
                position: target,
                velocity: 0.,
            },
            target,
            updated_at: now,
        };
        let entry = Self::entry(&mut values, key, || at_rest(target));
        if !entry.is::<Follow>() {
            *entry = Box::new(at_rest(target));
        }
        let follow = entry.downcast_mut::<Follow>().expect("checked above");
        let resting = follow.state.position == target && follow.state.velocity == 0.;
        if resting || cx.reduce_motion() {
            *follow = at_rest(target);
            return target;
        }
        let frequency = std::f32::consts::TAU / response.as_secs_f32();
        let config = gpui::SpringConfig::new(frequency * frequency, 2. * frequency, 1.);
        let elapsed = now
            .saturating_duration_since(follow.updated_at)
            .as_secs_f32();
        let stepped = config.step(follow.state, follow.target, elapsed);
        if config.is_settled(stepped, target, FOLLOW_EPSILON) {
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
        values: &mut HashMap<SharedString, Entry>,
        key: SharedString,
        init: impl FnOnce() -> T,
    ) -> &mut Box<dyn Any> {
        let entry = values.entry(key).or_insert_with(|| Entry {
            value: Box::new(init()),
            seen: true,
        });
        entry.seen = true;
        &mut entry.value
    }

    /// Starts `key`'s glide at `from` when it has none, so its first target
    /// is glided to rather than taken at once.
    pub(super) fn seed<T: Interpolate + PartialEq + 'static>(
        &self,
        key: SharedString,
        from: T,
        now: Instant,
    ) {
        Self::entry(&mut self.values(), key, || Glide {
            from: from.clone(),
            target: from,
            started_at: now,
            duration: Duration::ZERO,
            share: 1.,
        });
    }

    pub(super) fn glide<T>(
        &self,
        key: SharedString,
        target: T,
        curve: &Curve,
        window: &mut Window,
        cx: &App,
    ) -> (T, bool)
    where
        T: Interpolate + PartialEq + 'static,
    {
        let now = cx.background_executor().now();
        let mut values = self.values();
        let at_rest = |target: &T| Glide {
            from: target.clone(),
            target: target.clone(),
            started_at: now,
            duration: curve.duration(),
            share: 1.,
        };
        let entry = Self::entry(&mut values, key, || at_rest(&target));
        if !entry.is::<Glide<T>>() {
            *entry = Box::new(at_rest(&target));
        }
        let glide = entry.downcast_mut::<Glide<T>>().expect("checked above");
        if cx.reduce_motion() || curve.duration().is_zero() {
            if glide.from != target || glide.target != target {
                *glide = at_rest(&target);
            }
            return (target, true);
        }
        let sample = Timing::new(glide.duration)
            .ease(curve.easing())
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
            // gpui-base's reversal: turning back toward where the glide
            // started takes the share of a full glide it had travelled.
            let share = if target == glide.from {
                (sample.directed_progress * glide.share + (1. - glide.share)).clamp(0., 1.)
            } else {
                1.
            };
            *glide = Glide {
                from: value.clone(),
                target,
                started_at: now,
                duration: curve.duration().mul_f32(share),
                share,
            };
            window.request_animation_frame();
            return (value, false);
        }
        if glide.from != glide.target {
            if sample.finished {
                glide.from = glide.target.clone();
            } else {
                window.request_animation_frame();
                return (value, false);
            }
        }
        (value, true)
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

    /// How long the page's note takes to swap for the other layout's.
    const NOTE: std::time::Duration = std::time::Duration::from_millis(200);

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
                .swap("note", usize::from(compact), 2, NOTE, window, cx);
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
