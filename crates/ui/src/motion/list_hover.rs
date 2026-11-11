//! Hover reveals that fade across a list's items, one item hovered at a time.
//!
//! GPUI's `hover` and `group_hover` are styles that switch at once, with no
//! transition, and an item's `on_hover` hears the pointer leave as it moves
//! onto a part of the item that blocks it (its actions over it). [`ListHover`]
//! is the list's one hovered item, hovered while the pointer is over the
//! item or any of its parts or while a [`HoverHold`] lives, with the fades
//! of the items the pointer just entered or left on [`Owned`]'s glide: at
//! rest it holds one key, whatever the list's length.

use std::{
    cell::RefCell,
    collections::HashSet,
    rc::{Rc, Weak},
};

use gpui::{App, EntityId, SharedString, Window};
#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
#[cfg(target_family = "wasm")]
use web_time::Instant;

use super::{Curve, owned::Owned};

/// The item of a list the pointer is over, owned by the list, and how far
/// each item's hover reveal (a message's actions, a formula's tools) is in.
///
/// An item hands its `on_hover` to [`ListHover::on_hover`] and draws what it
/// reveals at [`ListHover::reveal`]. Only the items whose reveal is moving
/// hold a fade, on the glide every sampler uses; one at rest is 1 when
/// hovered and 0 otherwise.
#[derive(Clone)]
pub(crate) struct ListHover {
    hover: Rc<RefCell<Hover>>,
    /// The scene of the list's surface: a new one's items, whose keys may
    /// match the last one's, start with no reveal moving. The pointer gives
    /// the hover to an item again as it next comes onto one.
    scene: super::InScene,
}

struct Hover {
    /// The item the pointer is over, and over how many of its parts: an item
    /// whose part blocks the pointer from the item (its actions over it)
    /// hands those parts the same handler, and the item stays hovered while
    /// the pointer is over any of them, whichever order they hear it in.
    hovered: Option<(SharedString, usize)>,
    /// The items kept in with the pointer gone, each for as long as its
    /// [`HoverHold`] lives.
    held: Vec<(SharedString, Weak<()>)>,
    /// The reveals on their way in or out, by item, their glides in `fades`.
    fading: HashSet<SharedString>,
    fades: Owned,
    curve: Curve,
    /// The views that drew the list's items since the hover last moved.
    drawn_by: Vec<EntityId>,
}

impl Hover {
    fn is_held(&self, item: &str) -> bool {
        self.held
            .iter()
            .any(|(held, hold)| held == item && hold.strong_count() > 0)
    }

    /// Whether `item`'s reveal is in or on its way there.
    fn is_shown(&self, item: &str) -> bool {
        self.is_held(item) || self.hovered.as_ref().is_some_and(|(held, _)| held == item)
    }

    /// Starts the glide of `item`'s reveal from `from`, unless it is moving.
    fn fade(&mut self, item: SharedString, from: f32, now: Instant) {
        if self.fading.insert(item.clone()) {
            self.fades.seed(item, from, now);
        }
    }

    /// Forgets the holds let go, the reveals only they kept in fading out.
    /// Notes that `window`'s current view draws the list.
    fn drawn_in(&mut self, window: &Window) {
        let view = window.current_view();
        if !self.drawn_by.contains(&view) {
            self.drawn_by.push(view);
        }
    }

    fn release(&mut self, now: Instant) {
        let (live, released): (Vec<_>, Vec<_>) = std::mem::take(&mut self.held)
            .into_iter()
            .partition(|(_, hold)| hold.strong_count() > 0);
        self.held = live;
        for (item, _) in released {
            if !self.is_shown(&item) {
                self.fade(item, 1., now);
            }
        }
    }
}

/// An item of a [`ListHover`] kept hovered ([`ListHover::hold`]) until this
/// is dropped: a weak count, so a holder that goes away with its row lets go
/// with it.
pub(crate) struct HoverHold(#[allow(dead_code, reason = "held for its drop")] Rc<()>);

impl ListHover {
    /// The hover of one list, nothing hovered, its reveals moving along
    /// `curve`.
    pub(crate) fn new(curve: impl Into<Curve>) -> Self {
        Self {
            hover: Rc::new(RefCell::new(Hover {
                hovered: None,
                held: Vec::new(),
                fading: HashSet::new(),
                fades: Owned::default(),
                curve: curve.into(),
                drawn_by: Vec::new(),
            })),
            scene: Default::default(),
        }
    }

    /// The list's hover in `epoch`'s scene.
    pub(crate) fn in_scene(mut self, epoch: &super::Epoch) -> Self {
        self.scene = super::InScene::new(epoch);
        self
    }

    /// The hover, the last scene's forgotten.
    fn hover(&self) -> std::cell::RefMut<'_, Hover> {
        let mut hover = self.hover.borrow_mut();
        if self.scene.entered() {
            hover.hovered = None;
            hover.held.clear();
            hover.fading.clear();
            hover.fades = Owned::default();
        }
        hover
    }

    /// The item kept hovered, or else the one the pointer is over.
    pub(crate) fn hovered(&self) -> Option<SharedString> {
        let hover = self.hover();
        hover
            .held
            .iter()
            .find(|(_, hold)| hold.strong_count() > 0)
            .map(|(item, _)| item)
            .or(hover.hovered.as_ref().map(|(item, _)| item))
            .cloned()
    }

    /// The handler for the `on_hover` of `item`, or of one of its parts: the
    /// item takes the list's hover as the pointer comes in, and gives it up
    /// once it has left the item and all its parts. Only one item holds the
    /// hover; another taking it lets the first go.
    pub(crate) fn on_hover(
        &self,
        item: impl Into<SharedString>,
    ) -> impl Fn(&bool, &mut Window, &mut App) + 'static {
        let (hover, item) = (self.clone(), item.into());
        super::pointer_hover(move |entered, window, cx| {
            let (previous, next) = {
                let mut hover = hover.hover();
                let previous = hover.hovered.as_ref().map(|(item, _)| item.clone());
                hover.hovered = match hover.hovered.take() {
                    Some((held, parts)) if held == item => {
                        let parts = if *entered {
                            parts + 1
                        } else {
                            parts.saturating_sub(1)
                        };
                        (parts > 0).then_some((held, parts))
                    }
                    _ if *entered => Some((item.clone(), 1)),
                    other => other,
                };
                (
                    previous,
                    hover.hovered.as_ref().map(|(item, _)| item.clone()),
                )
            };
            if previous != next {
                hover.moved(previous, next, window, cx);
            }
        })
    }

    fn moved(
        &self,
        previous: Option<SharedString>,
        next: Option<SharedString>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let now = cx.background_executor().now();
        let drawn_by = {
            let mut hover = self.hover();
            hover.release(now);
            for (item, from) in [(previous, 1f32), (next, 0.)] {
                let Some(item) = item else { continue };
                // A reveal at rest starts its glide from where it rests; one
                // still moving turns from where it is. A held one stays.
                if !hover.is_held(&item) {
                    hover.fade(item, from, now);
                }
            }
            std::mem::take(&mut hover.drawn_by)
        };
        // Nothing an entity holds changed, so the views that drew the list
        // are drawn again, each noting itself as it does. A window refresh
        // would draw every view of the window from scratch.
        if drawn_by.is_empty() {
            window.refresh();
        }
        for view in drawn_by {
            cx.notify(view);
        }
    }

    /// Keeps `item` hovered, wherever the pointer goes, until the hold is
    /// dropped.
    pub(crate) fn hold(&self, item: impl Into<SharedString>, cx: &App) -> HoverHold {
        let item = item.into();
        let hold = Rc::new(());
        let mut hover = self.hover();
        if !hover.is_shown(&item) {
            hover.fade(item.clone(), 0., cx.background_executor().now());
        }
        hover.held.push((item, Rc::downgrade(&hold)));
        HoverHold(hold)
    }

    /// How far `item`'s hover reveal is in, from 0 to 1, keeping frames
    /// coming while it moves.
    pub(crate) fn reveal(&self, item: &str, window: &mut Window, cx: &App) -> f32 {
        let mut hover = self.hover();
        hover.drawn_in(window);
        hover.release(cx.background_executor().now());
        let target = if hover.is_shown(item) { 1f32 } else { 0. };
        if !hover.fading.contains(item) {
            return target;
        }
        let key = SharedString::from(item.to_owned());
        let (value, settled) = hover
            .fades
            .glide(key.clone(), target, &hover.curve, window, cx);
        if settled {
            hover.fading.remove(item);
            hover.fades.forget_exact(key);
        }
        value
    }

    /// What hears the pointer over `item` for an item that takes clicks: a
    /// layer over the item, for the first children of its frame, under its
    /// content, with a hitbox of its own that takes no clicks. GPUI's
    /// `on_hover` reports the pointer gone from an element with click
    /// listeners as soon as it moves while pressing it, so an item hearing
    /// its own would fade its reveal back as a quick click lands and replay
    /// it on release. The item must be positioned.
    pub(crate) fn hitbox(&self, item: impl Into<SharedString>) -> gpui::Stateful<gpui::Div> {
        use gpui::{InteractiveElement as _, StatefulInteractiveElement as _, Styled as _};
        gpui::div()
            .id("list-hover-layer")
            .absolute()
            .inset_0()
            .on_hover(self.on_hover(item))
    }

    /// The hover fill of `item`, for the first child of its frame, under the
    /// item's content: `fill` [blended](super::blend) in at the item's
    /// reveal, rounded by `radius`, over its [`Self::hitbox`], so the item's
    /// own `on_hover` stays the caller's. [`super::hover_layer`] for an item
    /// of a list: the fill holds no state of its own.
    pub(crate) fn layer(
        &self,
        item: impl Into<SharedString>,
        fill: gpui::Hsla,
        radius: gpui::Pixels,
    ) -> super::Sampled {
        use gpui::{IntoElement as _, Styled as _, prelude::FluentBuilder as _};
        let (hover, item) = (self.clone(), item.into());
        super::Sampled::new(move |window, cx| {
            let reveal = hover.reveal(&item, window, cx);
            hover
                .hitbox(item.clone())
                .rounded(radius)
                .when(reveal > 0., |layer| {
                    layer.bg(super::blend(fill.opacity(0.), fill, reveal))
                })
                .into_any_element()
        })
    }
}
