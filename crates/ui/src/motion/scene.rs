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

use std::hash::{DefaultHasher, Hash, Hasher as _};

use gpui::{
    AnyElement, App, Div, ElementId, InteractiveElement as _, IntoElement, ParentElement as _,
    Stateful, Styled as _, Window, div, prelude::FluentBuilder as _,
};
use gpui_base::motion::{Presence, PresencePhase};

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
            .transition(super::settle())
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
