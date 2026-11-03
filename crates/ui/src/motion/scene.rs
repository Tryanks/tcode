//! A whole surface changing to another scene: a route of the window.
//!
//! [`super::CrossFade`] fades both of its layers, which suits a region whose
//! states share nothing; a window's scenes share most of what they draw —
//! the canvas, a sidebar both routes keep — and two layers each at half
//! opacity show what they share at three quarters, a visible dip. Here the
//! scene left stays at full opacity beneath the one arriving, which rises
//! over it, so what the two share holds still and only what differs
//! dissolves. That holds only where the scene arriving covers the one it
//! leaves: over a translucent canvas, a column one scene draws where the
//! other draws a different one shows through it at full strength, so the
//! owner fades such a part out beneath the scene rising over it
//! ([`SceneFrame::unshared`]). gpui-base has no such transition, and GPUI
//! cannot draw a scene into a texture to fade a snapshot of it, so both
//! scenes are drawn while the new one rises.
//!
//! What a surface shows changing to another of its kind (a pane showing
//! another thread) is a new scene too, which [`Epoch`] names.

use std::{
    cell::{Cell, RefCell},
    hash::{DefaultHasher, Hash, Hasher as _},
    rc::Rc,
};

use gpui::{
    AnyElement, App, Div, ElementId, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, Stateful, Styled as _, Window, div, prelude::FluentBuilder as _,
};
use gpui_base::motion::{Presence, PresencePhase};

/// How long a change of scene takes.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// The scenes a surface has been showing, kept by the surface's owner: one
/// at rest, and while it changes, every scene still on screen.
pub(crate) struct Scene<K> {
    /// Bottom to top. The bottom scene is drawn opaque; each above it at the
    /// opacity it has risen to, or fallen to on its way out.
    stack: Vec<Layer<K>>,
    generation: u64,
}

struct Layer<K> {
    key: K,
    /// Names this layer's rise, so a scene that left and comes back rises
    /// afresh.
    generation: u64,
}

impl<K> Default for Scene<K> {
    fn default() -> Self {
        Self {
            stack: Vec::new(),
            generation: 0,
        }
    }
}

/// One frame of a [`Scene`]: the scenes to draw, bottom to top, and how.
pub(crate) struct SceneFrame<K> {
    id: ElementId,
    current: K,
    layers: Vec<(K, f32)>,
}

impl<K: Clone + Eq + Hash> Scene<K> {
    /// This frame of the surface showing `current`. A scene not on screen
    /// rises over the ones that are; going back to one still on screen
    /// lowers the scenes above it. The first scene ever shown is at rest.
    pub(crate) fn sample(
        &mut self,
        id: impl Into<ElementId>,
        current: K,
        window: &mut Window,
        cx: &mut App,
    ) -> SceneFrame<K> {
        let id = id.into();
        if !self.stack.iter().any(|layer| layer.key == current) {
            self.generation += 1;
            self.stack.push(Layer {
                key: current.clone(),
                generation: self.generation,
            });
        }
        let at = self
            .stack
            .iter()
            .position(|layer| layer.key == current)
            .expect("pushed above");
        let mut layers = Vec::with_capacity(self.stack.len());
        let mut gone = Vec::new();
        for (index, layer) in self.stack.iter().enumerate() {
            if index == 0 {
                layers.push((layer.key.clone(), 1.));
                continue;
            }
            let sample = Presence::new(
                ElementId::NamedChild(
                    std::sync::Arc::new(id.clone()),
                    format!("scene-{}", layer.generation).into(),
                ),
                index <= at,
            )
            .transition(gpui_base::motion::Transition::new(GLIDE))
            .sample(window, cx);
            match sample.phase {
                PresencePhase::Absent => gone.push(index),
                PresencePhase::Present => layers.push((layer.key.clone(), 1.)),
                PresencePhase::Entering | PresencePhase::Exiting => {
                    layers.push((layer.key.clone(), sample.progress))
                }
            }
        }
        for index in gone.into_iter().rev() {
            self.stack.remove(index);
        }
        // Risen and uncovered, the scene hides every one beneath it.
        if self.stack.last().is_some_and(|layer| layer.key == current)
            && layers
                .last()
                .is_some_and(|(key, opacity)| *key == current && *opacity >= 1.)
        {
            self.stack.drain(..self.stack.len() - 1);
            layers.drain(..layers.len() - 1);
        }
        SceneFrame {
            id,
            current,
            layers,
        }
    }
}

impl<K: Clone + Eq + Hash> SceneFrame<K> {
    /// The opacity of a part of `scene` that the scenes above it, where
    /// `shares` says they do not share it, draw differently: it fades out as
    /// such a scene rises over it and back in as that one falls, while the
    /// rest of `scene` stays opaque beneath.
    pub(crate) fn unshared(&self, scene: &K, shares: impl Fn(&K) -> bool) -> f32 {
        self.layers
            .iter()
            .skip_while(|(key, _)| key != scene)
            .skip(1)
            .filter(|(above, _)| !shares(above))
            .map(|(_, opacity)| 1. - opacity)
            .product()
    }

    /// The scenes to build this frame, bottom to top.
    pub(crate) fn scenes(&self) -> impl Iterator<Item = &K> {
        self.layers.iter().map(|(key, _)| key)
    }

    /// The surface holding the `scenes`, each filling it, in the order
    /// [`Self::scenes`] gave them. Only the current one takes input.
    pub(crate) fn render(self, scenes: impl IntoIterator<Item = AnyElement>) -> Stateful<Div> {
        let id = self.id;
        div().id(id.clone()).relative().size_full().children(
            self.layers
                .into_iter()
                .zip(scenes)
                .map(|((key, opacity), content)| {
                    let mut hasher = DefaultHasher::new();
                    key.hash(&mut hasher);
                    let leaving = key != self.current;
                    div()
                        .id((id.clone(), format!("scene-layer-{}", hasher.finish())))
                        .absolute()
                        .inset_0()
                        .opacity(opacity)
                        .child(content)
                        .when(leaving, |el| el.child(super::shield()))
                        .into_any_element()
                }),
        )
    }
}

/// The scene a surface is in: what it shows (the thread a pane shows, or
/// the right panel, which shows the focused thread's), counted by its
/// changes. Everything that keeps the surface's motion keeps it under the
/// scene, so a scene starts at rest whatever the last one left moving,
/// however its parts' keys repeat the last one's (a turn's index, a row's
/// place); and the same thing drawn under a new parent (a zoom, the window
/// turning compact, the dock building the panes anew, a pane swap moving a
/// thread) is the same scene, whose motion carries on:
///
/// - element-keyed motion (gpui-base's samplers, `use_keyed_state`) is
///   drawn under [`Self::id`], which names the scene, so its state is the
///   scene's alone;
/// - a store that keeps motion outside element state ([`super::owned::Owned`],
///   [`super::shared::SharedMoves`], [`super::list_rows::ListRows`],
///   [`super::ListHover`], [`super::resize_glide::ResizeGlide`]) is made
///   in the scene ([`InScene`]) and forgets what it kept the first time it
///   is reached in a new one.
///
/// Every clone is the same epoch: the parts of one surface that are views
/// of their own (a conversation's composer, its terminal drawer) share
/// their host's, and each saying what it shows starts one scene.
#[derive(Clone, Default)]
pub(crate) struct Epoch(Rc<EpochState>);

#[derive(Default)]
struct EpochState {
    shown: RefCell<Option<SharedString>>,
    count: Cell<u64>,
}

impl Epoch {
    /// The surface shows `what` from now on: a new scene when it showed
    /// something else, which this returns.
    pub(crate) fn show(&self, what: Option<&str>) -> bool {
        let mut shown = self.0.shown.borrow_mut();
        if shown.as_deref() == what {
            return false;
        }
        *shown = what.map(|what| SharedString::from(what.to_owned()));
        self.0.count.set(self.0.count.get() + 1);
        true
    }

    /// The surface took down what it shows and will put it up anew: a new
    /// scene, for that content to start at rest whatever it left moving.
    pub(crate) fn restart(&self) {
        self.0.count.set(self.0.count.get() + 1);
    }

    /// `id` in this scene: the id of the element the surface's motion is
    /// drawn under.
    pub(crate) fn id(&self, id: impl Into<ElementId>) -> ElementId {
        ElementId::NamedChild(
            std::sync::Arc::new(id.into()),
            format!("scene-{}", self.count()).into(),
        )
    }

    fn count(&self) -> u64 {
        self.0.count.get()
    }
}

/// Where a motion store stands against the [`Epoch`] it belongs to: none
/// for a store no scene holds. Every clone is the same.
#[derive(Clone, Default)]
pub(crate) struct InScene(Option<(Epoch, Rc<Cell<u64>>)>);

impl InScene {
    /// A store in `epoch`'s scene.
    pub(crate) fn new(epoch: &Epoch) -> Self {
        Self(Some((epoch.clone(), Rc::new(Cell::new(epoch.count())))))
    }

    /// Whether a new scene started since the store was last reached, which
    /// then forgets everything it kept: from here on it is reached in this
    /// scene.
    pub(crate) fn entered(&self) -> bool {
        self.0
            .as_ref()
            .is_some_and(|(epoch, seen)| seen.replace(epoch.count()) != epoch.count())
    }

    /// Whether what the store keeps is this scene's, for a reader that
    /// cannot forget it.
    pub(crate) fn current(&self) -> bool {
        self.0
            .as_ref()
            .is_none_or(|(epoch, seen)| seen.get() == epoch.count())
    }
}
