//! An element that moves from one place in a view to another — another row
//! of a list, another container — travelling between them.
//!
//! gpui-base has no shared-element transition, and gpui draws an element
//! built under another parent as a new element in its new place at once,
//! clipped by its new parent. [`SharedMoves`] is the view's record of where
//! each shared element was last drawn, and in which site; the frame an
//! element is drawn in another site than the one it was drawn in on the
//! last frame, it travels there from where it was ([`Shared`]). While it
//! travels it is drawn on top of the view, clipped only to the view's
//! scope ([`SharedMoves::begin_frame`]), so it can cross rows that clip
//! their own content as they fold; its layout is in its new place all the
//! while, so nothing around it waits for it.
//!
//! An element whose old place was not drawn on the last frame (a list row
//! scrolled out of its rendered window) has nowhere to travel from, and is
//! simply drawn in its new place.

use std::{cell::RefCell, collections::HashMap, rc::Rc};

#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
#[cfg(target_family = "wasm")]
use web_time::Instant;

use gpui::{
    AnyElement, App, Bounds, ContentMask, Element, ElementId, GlobalElementId, InspectorElementId,
    IntoElement, LayoutId, Pixels, Point, SharedString, Window,
};

/// How long a shared element takes to travel to its new place.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// Where a view's shared elements were drawn, by key; kept by the view and
/// cloned into whatever draws them. Every clone is the same record.
#[derive(Clone, Default)]
pub(crate) struct SharedMoves {
    moves: Rc<RefCell<Moves>>,
    /// The scene of the view whose elements these are: a new one's are
    /// drawn in their places at once, as those of a view's first frame
    /// are, however their keys match the last scene's.
    scene: super::InScene,
}

#[derive(Default)]
struct Moves {
    /// Counts the view's renders, so a place drawn on the last one can be
    /// told from an older one.
    frame: u64,
    /// The scope a travelling element is clipped to, in window coordinates.
    clip: Option<Bounds<Pixels>>,
    placed: HashMap<SharedString, Placed>,
}

struct Placed {
    site: SharedString,
    /// Where it was laid out when last drawn, in window coordinates.
    bounds: Option<Bounds<Pixels>>,
    frame: u64,
    travel: Option<Travel>,
}

#[derive(Clone)]
struct Travel {
    from_site: SharedString,
    /// Where it started, in window coordinates.
    from: Point<Pixels>,
    /// How far its new place is from where it started, once laid out there.
    offset: Option<Point<Pixels>>,
    at: Instant,
}

/// A shared element on its way from another site.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Transit {
    /// The site it comes from.
    pub(crate) from: SharedString,
    /// How far it has come, from 0 to 1.
    pub(crate) progress: f32,
}

impl SharedMoves {
    /// The record of a view in `epoch`.
    pub(crate) fn in_scene(epoch: &super::Epoch) -> Self {
        Self {
            moves: Rc::default(),
            scene: super::InScene::new(epoch),
        }
    }

    /// The record, the last scene's places forgotten.
    fn moves(&self) -> std::cell::RefMut<'_, Moves> {
        let mut moves = self.moves.borrow_mut();
        if self.scene.entered() {
            moves.placed.clear();
        }
        moves
    }

    /// Begins one render of the view, whose shared elements travel inside
    /// `clip`. Places not drawn for a while are forgotten.
    pub(crate) fn begin_frame(&self, clip: Bounds<Pixels>, cx: &App) {
        let now = cx.background_executor().now();
        let mut moves = self.moves();
        moves.frame += 1;
        moves.clip = Some(clip);
        let frame = moves.frame;
        moves.placed.retain(|_, placed| {
            placed.frame + 2 >= frame
                || placed
                    .travel
                    .as_ref()
                    .is_some_and(|travel| !settled(travel, now))
        });
    }

    /// Element `key` is drawn in `site` this frame. If it was drawn in
    /// another site on the last frame, it travels here from there; while it
    /// travels, its transit. Reduced motion puts it here at once.
    pub(crate) fn arrive(
        &self,
        key: &SharedString,
        site: &SharedString,
        cx: &App,
    ) -> Option<Transit> {
        let now = cx.background_executor().now();
        let reduce = cx.reduce_motion();
        let mut moves = self.moves();
        let frame = moves.frame;
        let placed = moves.placed.entry(key.clone()).or_insert_with(|| Placed {
            site: site.clone(),
            bounds: None,
            frame,
            travel: None,
        });
        if placed.site != *site {
            let from = placed
                .bounds
                .filter(|_| placed.frame + 1 == frame && !reduce)
                .map(|bounds| {
                    // A move during a travel starts from where it is drawn.
                    let drawn = placed
                        .travel
                        .as_ref()
                        .and_then(|travel| Some(travel.offset? * (1. - progress(travel, now))))
                        .unwrap_or_default();
                    bounds.origin + drawn
                });
            placed.travel = from.map(|from| Travel {
                from_site: placed.site.clone(),
                from,
                offset: None,
                at: now,
            });
            placed.site = site.clone();
        }
        placed.frame = frame;
        if placed
            .travel
            .as_ref()
            .is_some_and(|travel| settled(travel, now))
        {
            placed.travel = None;
        }
        placed.travel.as_ref().map(|travel| Transit {
            from: travel.from_site.clone(),
            progress: progress(travel, now),
        })
    }

    /// Whether element `key`, last drawn in `site`, is now drawn elsewhere:
    /// what stays behind in `site` while it folds away is only its place.
    pub(crate) fn departed(&self, key: &SharedString, site: &SharedString) -> bool {
        let moves = self.moves();
        moves
            .placed
            .get(key)
            .is_some_and(|placed| placed.site != *site && placed.frame + 1 >= moves.frame)
    }

    /// `child`, the element `key` drawn where [`Self::arrive`] placed it.
    pub(crate) fn element(&self, key: impl Into<SharedString>, child: impl IntoElement) -> Shared {
        Shared {
            key: key.into(),
            moves: self.clone(),
            child: Some(child.into_any_element()),
        }
    }
}

fn progress(travel: &Travel, now: Instant) -> f32 {
    crate::motion::Curve::new(GLIDE).progress(now.saturating_duration_since(travel.at))
}

fn settled(travel: &Travel, now: Instant) -> bool {
    now.saturating_duration_since(travel.at) >= GLIDE
}

/// A shared element, laid out in its place and drawn there, or on its way
/// there from where it was.
pub(crate) struct Shared {
    key: SharedString,
    moves: SharedMoves,
    child: Option<AnyElement>,
}

impl IntoElement for Shared {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Shared {
    type RequestLayoutState = ();
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
    ) -> (LayoutId, ()) {
        let child = self.child.as_mut().expect("laid out once");
        (child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let now = cx.background_executor().now();
        let (travel, clip) = {
            let mut moves = self.moves.moves();
            let clip = moves.clip;
            let travel = moves.placed.get_mut(&self.key).and_then(|placed| {
                placed.bounds = Some(bounds);
                let travel = placed.travel.as_mut()?;
                let offset = *travel.offset.get_or_insert(travel.from - bounds.origin);
                Some(offset * (1. - progress(travel, now)))
            });
            (travel, clip)
        };
        let child = self.child.as_mut().expect("prepainted once");
        match travel {
            Some(offset) if offset != Point::default() => {
                window.request_animation_frame();
                let child = self.child.take().expect("prepainted once");
                window.defer_draw(
                    child,
                    window.element_offset() + offset,
                    0,
                    clip.map(|bounds| ContentMask { bounds }),
                );
            }
            _ => {
                child.prepaint(window, cx);
            }
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(child) = &mut self.child {
            child.paint(window, cx);
        }
    }
}
