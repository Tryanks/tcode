//! Rows of a gpui `list` that arrive, leave and change height.
//!
//! gpui's `list` has no insert, remove or item-height motion: a splice or a
//! remeasure changes the rows' heights in one frame, and every row below
//! jumps. gpui-base's `MotionReveal` scales its previous frame's measurement
//! by a progress, and that measurement, like every keyed element state, ends
//! when a row leaves the list's rendered window. So the motion of a list's
//! rows is kept by the list's owner, by row key, until gpui or gpui-base
//! moves list rows themselves:
//!
//! - [`ListRows`] holds the stamps: when each row that arrived came in, and
//!   each row that left, with the value it was last drawn from.
//! - [`RowGlide`] lays a row out at a gliding height: an arriving row from
//!   nothing to its measurement, a leaving one back to nothing, the live end
//!   of a followed list to each new measurement ([`Glide::Tail`]).
//!
//! # Using it with a `list`
//!
//! The owner keeps a `ListRows` beside its `ListState` and its row model,
//! each row known by a key that stays the same for as long as the row means
//! the same thing.
//!
//! 1. When the model changes, the owner splices the `ListState` as before.
//!    For a change the reader sees happen (a live record, the reader's own
//!    toggle), it stamps what the splices brought with [`arrived_rows`] and
//!    [`ListRows::arrive`], and gives each removed row to
//!    [`ListRows::leave`] with what it was drawn from and the surviving row
//!    it folds against ([`Attach`]). A change the reader did not see happen
//!    (opening the list, a page of history, a reset, catching up) stamps
//!    nothing, or calls [`ListRows::clear`], so it is in place at once.
//! 2. The list's row renderer draws each row through a [`RowGlide`]: its
//!    [`ListRows::entering`] progress as [`Glide::Enter`], and the rows that
//!    left against it ([`ListRows::leaving`]) above or below its content, each
//!    rebuilt from its value through [`RowGlide::leaving`], which takes no
//!    input. While either moves it asks for frames. A row whose content opens
//!    from the top over time keeps the rest out ([`RowGlide::withheld`]) and
//!    says until when ([`ListRows::hold`]).
//! 3. Each render the owner calls [`ListRows::settle`] and remeasures the
//!    rows it names, whose motion has ended: `list` measures only the rows
//!    in and near its view, so a row its motion moved out of view would keep,
//!    and the list's height and scroll range with it, the height it had there.
//!
//! The list itself is never spliced late: a leaving row is out of the model
//! and the `ListState` at once, so indices, the reader's anchor and the
//! measured heights of every other row stay the owner's as they were. It is
//! drawn by the row it folds against, which the list measures each frame.
//! A reader following the tail stays on it as rows grow and fold, because
//! the list keeps its end on the viewport's bottom edge; one who paused is
//! not moved by rows arriving above the row at the top of the viewport if
//! the owner stamps only rows below it.
//!
//! A list whose rows are replaced whole (a sidebar re-sorted, filtered and
//! grown in one change) finds what arrived and what left from the keys
//! before and after with [`changes`] instead of splices, and hands
//! [`super::list_flip::ListFlip::replace`] the same keys, so the rows that
//! stayed glide to their new places while the others fold in and out:
//! `ListFlip::row(index, id, RowGlide::new(id, glide, content))`.

use std::{collections::HashMap, ops::Range, rc::Rc, time::Duration};

#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
#[cfg(target_family = "wasm")]
use web_time::Instant;

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, ContentMask, Element, ElementId, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, ParentElement as _, Pixels, Style, Styled as _,
    Window, div, point, prelude::FluentBuilder as _, px, relative, size,
};
use gpui_base::motion::{Easing, Timing};

/// How long a row's height, and a list's tail, take to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// How a row that arrived while the reader was looking came in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Arrival {
    /// After every row the list had: the row above gives up the end's
    /// padding, which the glide hands over.
    Appended,
    /// Between rows the list keeps.
    Inserted,
    /// In place of a row that went.
    Replaced,
}

/// A row's arrival, stamped when the list learns of it, with what the
/// owner notes about it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Arrived<A> {
    pub(crate) at: Instant,
    pub(crate) arrival: Arrival,
    pub(crate) note: A,
    duration: Duration,
}

impl<A> Arrived<A> {
    /// How far in the row is, from absent to at rest, `now`.
    pub(crate) fn progress(&self, now: Instant) -> f32 {
        eased(now.saturating_duration_since(self.at), self.duration)
    }

    pub(crate) fn settled(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.at) >= self.duration
    }
}

/// The surviving row a leaving one folds against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Attach {
    /// Drawn below this row's content: it was below it.
    Below(u64),
    /// Drawn above this row's content: nothing survived above it.
    Above(u64),
}

impl Attach {
    fn key(self) -> u64 {
        match self {
            Attach::Below(key) | Attach::Above(key) => key,
        }
    }
}

struct Leaving<V> {
    key: u64,
    attach: Attach,
    value: V,
    at: Instant,
}

/// A row that left, as its survivor draws it: its key, what it was drawn
/// from, and how far in it still is.
pub(crate) struct Left<'a, V> {
    pub(crate) key: u64,
    pub(crate) value: &'a V,
    pub(crate) progress: f32,
}

/// The arrivals and departures of one list's rows, by row key, kept by the
/// list's owner. `A` is what the owner notes about an arrival; `V` is what
/// a leaving row is drawn from.
pub(crate) struct ListRows<A, V> {
    arrivals: HashMap<u64, Arrived<A>>,
    leaving: Vec<Leaving<V>>,
    /// Rows laid out short of their content, and when that ends.
    held: HashMap<u64, Instant>,
    /// Rows whose motion was let go of before it ended, which the next
    /// [`Self::settle`] names.
    ended: Vec<u64>,
    /// How long a row takes to come in or fold away.
    duration: Duration,
    /// The scene of the list's surface: a new one's rows, whose keys may
    /// match the last one's, are in place at once.
    scene: super::InScene,
}

impl<A, V> ListRows<A, V> {
    /// Rows that come in and fold away over `duration`: shorter than the
    /// default for a list a keystroke refilters, so the rows keep pace with
    /// the typing.
    pub(crate) fn over(duration: Duration) -> Self {
        Self {
            arrivals: HashMap::new(),
            leaving: Vec::new(),
            held: HashMap::new(),
            ended: Vec::new(),
            duration,
            scene: Default::default(),
        }
    }

    /// The rows of a list in `epoch`'s scene.
    pub(crate) fn in_scene(mut self, epoch: &super::Epoch) -> Self {
        self.scene = super::InScene::new(epoch);
        self
    }

    /// Forgets the last scene's stamps, once a new scene reaches the rows.
    fn enter(&mut self) {
        if self.scene.entered() {
            self.let_go();
        }
    }

    /// Everything in place at once, the rows it moved named by the next
    /// [`Self::settle`].
    fn let_go(&mut self) {
        self.ended.extend(self.arrivals.drain().map(|(key, _)| key));
        self.ended.extend(self.held.drain().map(|(key, _)| key));
        self.ended
            .extend(self.leaving.drain(..).map(|left| left.attach.key()));
    }

    /// Row `key` is laid out short of its content until `until`: a row moved
    /// out of view meanwhile is measured again then, as an arrival is.
    pub(crate) fn hold(&mut self, key: u64, until: Instant) {
        self.held.insert(key, until);
    }
}

impl<A: Copy, V> ListRows<A, V> {
    /// Row `key` came in `at`. A row still folding away turns back from
    /// where it is.
    pub(crate) fn arrive(&mut self, key: u64, arrival: Arrival, note: A, at: Instant) {
        self.enter();
        let mut at = at;
        if let Some(index) = self.leaving.iter().position(|left| left.key == key) {
            let left = self.leaving.remove(index);
            self.ended.push(left.attach.key());
            let shown = 1. - eased(at.saturating_duration_since(left.at), self.duration);
            at = at
                .checked_sub(elapsed_for(shown, self.duration))
                .unwrap_or(at);
        }
        self.arrivals.insert(
            key,
            Arrived {
                at,
                arrival,
                note,
                duration: self.duration,
            },
        );
    }

    /// Row `key` left `at`, drawn from `value` against `attach` until it has
    /// folded away. A row still arriving folds from where it is; rows that
    /// were folding against it fold against `attach` now.
    pub(crate) fn leave(&mut self, key: u64, value: V, attach: Attach, at: Instant) {
        self.enter();
        let mut start = at;
        if let Some(arrived) = self.arrivals.remove(&key) {
            let shown = arrived.progress(at);
            start = at
                .checked_sub(elapsed_for(1. - shown, self.duration))
                .unwrap_or(at);
        }
        for left in &mut self.leaving {
            if left.attach.key() == key {
                left.attach = attach;
            }
        }
        self.leaving.retain(|left| left.key != key);
        self.leaving.push(Leaving {
            key,
            attach,
            value,
            at: start,
        });
    }

    /// Row `key`'s arrival and how far in it is, while it comes in.
    pub(crate) fn entering(&self, key: u64, now: Instant) -> Option<(Arrived<A>, f32)> {
        let arrived = self.arrivals.get(&key).filter(|_| self.scene.current())?;
        (!arrived.settled(now)).then(|| (*arrived, arrived.progress(now)))
    }

    /// The rows folding away against row `key`: those above its content,
    /// then those below, each in the order they stood.
    pub(crate) fn leaving(&self, key: u64, now: Instant) -> (Vec<Left<'_, V>>, Vec<Left<'_, V>>) {
        let current = self.scene.current();
        let left = |side: fn(u64) -> Attach| {
            self.leaving
                .iter()
                .filter(|left| current && left.attach == side(key))
                .map(|left| Left {
                    key: left.key,
                    value: &left.value,
                    progress: 1. - eased(now.saturating_duration_since(left.at), self.duration),
                })
                .filter(|left| left.progress > 0.)
                .collect::<Vec<_>>()
        };
        (left(Attach::Above), left(Attach::Below))
    }

    /// Lets go of the arrivals that have come in and the rows that have
    /// folded away, returning the rows whose height their motion changed
    /// and changes no more: those that came in, those the folded rows were
    /// drawn against, and those whose motion was let go of since the last
    /// settle. The list measures them again. While rows still move it asks
    /// for frames: a row moved out of view is not drawn, so asks for none.
    pub(crate) fn settle(&mut self, window: &mut Window, cx: &App) -> Vec<u64> {
        let now = cx.background_executor().now();
        self.enter();
        let mut ended = std::mem::take(&mut self.ended);
        self.arrivals.retain(|key, arrived| {
            let settled = arrived.settled(now);
            if settled {
                ended.push(*key);
            }
            !settled
        });
        self.leaving.retain(|left| {
            let gone = now.saturating_duration_since(left.at) >= self.duration;
            if gone {
                ended.push(left.attach.key());
            }
            !gone
        });
        self.held.retain(|key, until| {
            let released = now >= *until;
            if released {
                ended.push(*key);
            }
            !released
        });
        if !self.arrivals.is_empty() || !self.leaving.is_empty() || !self.held.is_empty() {
            window.request_animation_frame();
        }
        ended
    }

    /// Everything in place at once: a list that changed without the reader
    /// seeing it happen.
    pub(crate) fn clear(&mut self) {
        self.let_go();
    }

    /// Stamps what replacing the rows keyed `old` with the rows keyed `new`
    /// brought and took away, for a change the reader sees happen: each new
    /// row `arrives` notes comes in, and each row that went, rebuilt from
    /// what `left` gives for its old index, folds away against its survivor.
    /// A row either passes over (`None`) is in place, or gone, at once: a row
    /// a disclosure folds, or one out of view. Reduced motion stamps nothing.
    pub(crate) fn replace(
        &mut self,
        old: &[u64],
        new: &[u64],
        mut arrives: impl FnMut(usize, Arrival) -> Option<A>,
        mut left: impl FnMut(usize) -> Option<V>,
        cx: &App,
    ) {
        if cx.reduce_motion() || old == new {
            return;
        }
        let at = cx.background_executor().now();
        let Changes {
            arrived,
            left: gone,
        } = changes(old, new);
        for (index, arrival) in arrived {
            if let Some(note) = arrives(index, arrival) {
                self.arrive(new[index], arrival, note, at);
            }
        }
        for (index, attach) in gone {
            if let Some(value) = left(index) {
                self.leave(old[index], value, attach, at);
            }
        }
    }

    /// Row `key`'s motion this frame: how far in it is while it arrives, and
    /// the rows folding away against it, each with what it is drawn from.
    pub(crate) fn around(&self, key: u64, cx: &App) -> Around<V>
    where
        V: Clone,
    {
        let now = cx.background_executor().now();
        let (above, below) = self.leaving(key, now);
        let owned = |left: Vec<Left<'_, V>>| {
            left.into_iter()
                .map(|left| (left.key, left.value.clone(), left.progress))
                .collect()
        };
        Around {
            entering: self.entering(key, now).map(|(_, progress)| progress),
            above: owned(above),
            below: owned(below),
        }
    }

    /// [`Self::settle`] for a list drawn from `keys`, which measures again
    /// the rows it names.
    pub(crate) fn settle_list(
        &mut self,
        list: &gpui::ListState,
        keys: &[u64],
        window: &mut Window,
        cx: &App,
    ) {
        for key in self.settle(window, cx) {
            if let Some(index) = keys.iter().position(|row| *row == key) {
                list.remeasure_items(index..index + 1);
            }
        }
    }
}

/// One list row's motion this frame, as [`ListRows::around`] samples it.
pub(crate) struct Around<V> {
    /// How far in the row is, while it arrives.
    pub(crate) entering: Option<f32>,
    /// The rows folding away above its content: key, value, progress.
    pub(crate) above: Vec<(u64, V, f32)>,
    /// The rows folding away below it.
    pub(crate) below: Vec<(u64, V, f32)>,
}

impl<V> Around<V> {
    /// The row's `content` laid out through a [`RowGlide`] named `id` and
    /// `key`, arriving or else gliding as `rest` says, with the rows that
    /// left against it rebuilt by `build` above and below it. Asks for
    /// frames while any of them moves.
    pub(crate) fn draw(
        self,
        id: &'static str,
        key: u64,
        rest: Glide,
        content: impl IntoElement,
        mut build: impl FnMut(&V, &mut Window) -> AnyElement,
        window: &mut Window,
    ) -> AnyElement {
        let glide = self.entering.map_or(rest, Glide::Enter);
        if self.entering.is_none() && self.above.is_empty() && self.below.is_empty() {
            return RowGlide::new((id, key), glide, content).into_any_element();
        }
        window.request_animation_frame();
        let mut fold = |(key, value, progress): (u64, V, f32), window: &mut Window| {
            RowGlide::leaving((id, key), progress, build(&value, window)).into_any_element()
        };
        let above: Vec<_> = self
            .above
            .into_iter()
            .map(|left| fold(left, window))
            .collect();
        let below: Vec<_> = self
            .below
            .into_iter()
            .map(|left| fold(left, window))
            .collect();
        div()
            .w_full()
            .flex()
            .flex_col()
            .children(above)
            .child(RowGlide::new((id, key), glide, content))
            .children(below)
            .into_any_element()
    }
}

fn curve(duration: Duration) -> Timing {
    Timing::new(duration).ease(Easing::Custom(Rc::new(
        gpui_base::animation::ease_out_cubic,
    )))
}

fn eased(elapsed: Duration, duration: Duration) -> f32 {
    curve(duration).sample(elapsed).directed_progress
}

/// How long the glide takes to come `progress` of the way: the inverse of
/// its ease-out cubic.
fn elapsed_for(progress: f32, duration: Duration) -> Duration {
    let progress = progress.clamp(0., 1.);
    duration.mul_f32(1. - (1. - progress).cbrt())
}

/// The rows a set of splices brings in, by their index among the new rows,
/// and how each came in. `splices` are ranges of the old rows with the
/// number of new rows standing in for each, ascending; those in `carried`
/// hand rows already on screen over to new ones, so nothing in them
/// arrives.
pub(crate) fn arrived_rows(
    splices: &[(Range<usize>, usize)],
    carried: &[Range<usize>],
    old_len: usize,
) -> Vec<(usize, Arrival)> {
    let mut arrived = Vec::new();
    let mut shift = 0isize;
    for (range, count) in splices {
        let start = range.start.saturating_add_signed(shift);
        shift += *count as isize - range.len() as isize;
        if carried.contains(range) {
            continue;
        }
        arrived.extend((0..*count).map(|offset| {
            let arrival = if offset < range.len() {
                Arrival::Replaced
            } else if range.end == old_len {
                Arrival::Appended
            } else {
                Arrival::Inserted
            };
            (start + offset, arrival)
        }));
    }
    arrived
}

/// What replacing a list's rows brought and took away.
#[derive(Debug, PartialEq)]
pub(crate) struct Changes {
    /// Each new row whose key is new, by its index among the new rows, with
    /// how it came in.
    pub(crate) arrived: Vec<(usize, Arrival)>,
    /// Each old row whose key went, by its index among the old rows, with
    /// the surviving row it folds against.
    pub(crate) left: Vec<(usize, Attach)>,
}

/// What replacing the rows keyed `old` with the rows keyed `new` brought
/// and took away.
pub(crate) fn changes(old: &[u64], new: &[u64]) -> Changes {
    let before: std::collections::HashSet<u64> = old.iter().copied().collect();
    let after: std::collections::HashSet<u64> = new.iter().copied().collect();
    let last_kept = new.iter().rposition(|key| before.contains(key));
    let arrived = new
        .iter()
        .enumerate()
        .filter(|(_, key)| !before.contains(key))
        .map(|(index, _)| {
            let arrival = if last_kept.is_some_and(|last| index < last) {
                Arrival::Inserted
            } else {
                Arrival::Appended
            };
            (index, arrival)
        })
        .collect();
    let survives = |index: usize| after.contains(&old[index]);
    let left = (0..old.len())
        .filter(|&index| !survives(index))
        .filter_map(|index| {
            let attach = (0..index)
                .rev()
                .find(|&above| survives(above))
                .map(|above| Attach::Below(old[above]))
                .or_else(|| {
                    (index + 1..old.len())
                        .find(|&below| survives(below))
                        .map(|below| Attach::Above(old[below]))
                })?;
            Some((index, attach))
        })
        .collect();
    Changes { arrived, left }
}

/// What a [`RowGlide`] does this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Glide {
    /// Laid out as a plain child: no measurement, no clip, no frame.
    Rest,
    /// Arriving: its height is its measurement scaled by the progress, and
    /// it fades and rises in. Not yet measured, it starts from nothing.
    Enter(f32),
    /// Leaving: as [`Glide::Enter`] on its way back to nothing. Not yet
    /// measured (a row drawn somewhere new as it leaves), it is laid out as
    /// tall as it is, so it folds from its whole height.
    Leave(f32),
    /// Each time its `content` changes, its height glides to the new
    /// measurement: the live end of a list the reader follows, or a row
    /// whose owner knows what changes its height (a diff row's wrapping).
    /// At rest it is laid out as a plain child, so height that changes on
    /// its own (a fold inside it, an image that loaded) moves once, by its
    /// own motion, and a list of rows at rest costs what it would without.
    Tail { content: u64 },
    /// A row whose own height changes at rest — a card opened under it, an
    /// editor in place of its text — glides to each new measurement, a frame
    /// behind. It is measured apart from the list every frame, so it suits
    /// a few rows (cards in a conversation, what is under a diff line), not
    /// every row of a long list. A new `scope` (the list's viewport
    /// resized) takes the new height at once, so a resize does not trail.
    /// With motion reduced it is laid out as a plain child.
    Height { scope: u64 },
}

/// A list row's content, gliding its layout height.
///
/// The same element in every [`Glide`], so what it draws keeps its element
/// ids, and with them its state, when a glide starts or ends.
pub(crate) struct RowGlide {
    id: ElementId,
    glide: Glide,
    /// Where its measurement and its tail's glide are kept when an owner
    /// keeps them; in element state otherwise.
    owner: Option<Owner>,
    /// How much of its content's height, from the bottom, it keeps out.
    withheld: Pixels,
    child: AnyElement,
}

/// A [`RowGlide`]'s state as an owner keeps it.
struct Owner {
    measured: Rc<std::cell::Cell<Measured>>,
    motion: super::owned::Owned,
}

impl RowGlide {
    pub(crate) fn new(id: impl Into<ElementId>, glide: Glide, child: impl IntoElement) -> Self {
        let child = div()
            .w_full()
            .flex()
            .flex_col()
            .when_some(
                match glide {
                    Glide::Enter(progress) | Glide::Leave(progress) if progress < 1. => {
                        Some(progress)
                    }
                    _ => None,
                },
                |child, progress| child.opacity(opacity(progress)),
            )
            .child(child)
            .into_any_element();
        Self {
            id: id.into(),
            glide,
            owner: None,
            withheld: px(0.),
            child,
        }
    }

    /// The row short by `withheld` of its content's height, while it arrives
    /// and on [`Glide::Tail`]: as `withheld` falls the row opens, gliding as
    /// it does to a new measurement. It only opens: a row shown taller stays
    /// so until its content's height has caught up, and it never stands
    /// taller than its content.
    pub(crate) fn withheld(mut self, withheld: Pixels) -> Self {
        self.withheld = withheld;
        self
    }

    /// The row with its measurement and its glide kept by `motion`, so a
    /// row drawn under a new parent (its list's view moved by the window
    /// turning compact or wide, or by the dock) carries on gliding. The
    /// row's id names what it keeps there.
    pub(crate) fn owned(mut self, motion: &super::owned::Owned) -> Self {
        let motion = motion.scope(self.id.to_string());
        self.owner = Some(Owner {
            measured: motion.keep("measured"),
            motion,
        });
        self
    }

    /// The row's state, from its owner or its element.
    fn measured<R>(
        &self,
        global_id: &GlobalElementId,
        window: &mut Window,
        f: impl FnOnce(&mut Measured) -> R,
    ) -> R {
        match &self.owner {
            Some(owner) => {
                let mut state = owner.measured.get();
                let result = f(&mut state);
                owner.measured.set(state);
                result
            }
            None => window.with_element_state(global_id, |state, _| {
                let mut state: Measured = state.unwrap_or_default();
                let result = f(&mut state);
                (result, state)
            }),
        }
    }

    /// A row that left, rebuilt from what it was drawn from, folding away at
    /// `progress`; it takes no pointer, wheel or focus.
    pub(crate) fn leaving(
        id: impl Into<ElementId>,
        progress: f32,
        child: impl IntoElement,
    ) -> Self {
        Self::new(
            id,
            Glide::Leave(progress),
            div()
                .relative()
                .w_full()
                .child(child)
                .child(super::shield()),
        )
    }
}

/// How opaque a row arriving or leaving at `progress` of its height is. Its
/// content is clipped to that height, so a row whose fade kept pace with
/// its height would show, late in a fold, as a faint sliver of its top; the
/// fade runs ahead instead, and a leaving row is gone while its room is still
/// closing, an arriving one shows once it has room to.
fn opacity(progress: f32) -> f32 {
    const UNSEEN: f32 = 0.3;
    ((progress - UNSEEN) / (1. - UNSEEN)).clamp(0., 1.)
}

#[derive(Clone, Copy, Default)]
struct Measured {
    /// The child's height as last laid out.
    height: Option<Pixels>,
    /// The tail's content when the glide last took aim.
    content: Option<u64>,
    /// The measurement the tail glides to: the one taken after its content
    /// last changed, or the latest while part of it is withheld.
    target: Option<Pixels>,
    /// The height the tail is on its way to: `target` less what is withheld.
    aim: Pixels,
    /// The content changed; the next measurement is the new target.
    retarget: bool,
    /// The height is on its way to `target`.
    gliding: bool,
    /// For [`Glide::Height`]: the scope last seen, and the glide under way,
    /// from the height shown when the content changed, and when.
    scope: Option<u64>,
    run: Option<(Pixels, Instant)>,
}

pub(crate) enum GlideLayout {
    /// The child is this element's layout.
    Plain,
    /// This element is laid out at the glided height; the child is laid out
    /// on its own, `lift` below its top, and clipped to it where `clip`.
    Clipped { lift: Pixels, clip: bool },
}

impl IntoElement for RowGlide {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for RowGlide {
    type RequestLayoutState = GlideLayout;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, GlideLayout) {
        let global_id = global_id.expect("RowGlide has an id");
        let height = match self.glide {
            Glide::Rest => None,
            Glide::Height { .. } if cx.reduce_motion() => None,
            Glide::Height { scope } => {
                // Its own clock in its element state, not a transition: a list
                // draws many such rows at rest every frame.
                let state = self.measured(global_id, window, |state| {
                    if state.scope != Some(scope) {
                        state.run = None;
                    }
                    state.scope = Some(scope);
                    *state
                });
                let now = cx.background_executor().now();
                state.height.map(|height| {
                    let shown = match state.run {
                        Some((from, at)) => {
                            let progress = crate::motion::Curve::new(GLIDE)
                                .progress(now.saturating_duration_since(at));
                            if progress < 1. {
                                window.request_animation_frame();
                            }
                            from + (height - from) * progress.min(1.)
                        }
                        None => height,
                    };
                    (shown, px(0.))
                })
            }
            Glide::Enter(progress) | Glide::Leave(progress) => {
                let withheld = self.withheld;
                let measured = self.measured(global_id, window, |state| {
                    let shown = state.height.unwrap_or_default() - withheld;
                    state.aim = shown.max(px(0.)) * progress;
                    state.height.map(|_| state.aim)
                });
                match (measured, self.glide) {
                    (None, Glide::Leave(_)) => None,
                    (height, _) => Some((height.unwrap_or_default(), super::lift(progress))),
                }
            }
            Glide::Tail { content } => {
                let withheld = self.withheld;
                let held = withheld > px(0.);
                let state = self.measured(global_id, window, |state| {
                    if state.content.is_some_and(|seen| seen != content) {
                        state.retarget = true;
                    }
                    state.content = Some(content);
                    if held {
                        state.target = state.target.or(state.height);
                    }
                    *state
                });
                match (state.height, state.target) {
                    // The frame the content changes still shows the height it
                    // had; its measurement becomes the target the next frame
                    // glides to. Height that changes during a glide rides on
                    // it, a frame late.
                    (Some(measured), Some(target)) if state.retarget || state.gliding || held => {
                        let aim = (target - withheld).max(state.aim).min(target);
                        let clock =
                            super::Clock::from(self.owner.as_ref().map(|owner| &owner.motion));
                        let (glided, settled) =
                            clock.glide("tail", aim, &super::Curve::new(GLIDE), window, cx);
                        self.measured(global_id, window, |state| {
                            state.aim = aim;
                            state.gliding = !settled || held;
                        });
                        Some(((glided + measured - target).max(px(0.)), px(0.)))
                    }
                    _ => None,
                }
            }
        };
        match height {
            None => (self.child.request_layout(window, cx), GlideLayout::Plain),
            Some((height, lift)) => {
                let mut style = Style::default();
                style.size.width = relative(1.).into();
                style.size.height = height.into();
                (
                    window.request_layout(style, None, cx),
                    GlideLayout::Clipped { lift, clip: true },
                )
            }
        }
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut GlideLayout,
        window: &mut Window,
        cx: &mut App,
    ) {
        let lift = match layout {
            GlideLayout::Plain => {
                self.child.prepaint(window, cx);
                // A row an owner keeps may take up a glide after any rest,
                // and takes it up from the height it shows.
                if self.owner.is_some()
                    || matches!(
                        self.glide,
                        Glide::Tail { .. } | Glide::Leave(_) | Glide::Height { .. }
                    )
                {
                    let size = bounds.size;
                    self.measured(global_id.expect("RowGlide has an id"), window, |state| {
                        state.height = Some(size.height);
                        state.target = Some(size.height);
                        state.aim = size.height;
                        state.retarget = false;
                        state.gliding = false;
                    });
                }
                return;
            }
            GlideLayout::Clipped { lift, .. } => *lift,
        };
        let measured = self
            .child
            .layout_as_root(
                size(
                    AvailableSpace::Definite(bounds.size.width),
                    AvailableSpace::MinContent,
                ),
                window,
                cx,
            )
            .height;
        let now = cx.background_executor().now();
        let height_glide = matches!(self.glide, Glide::Height { .. });
        let held = self.withheld > px(0.);
        let moved = self.measured(global_id.expect("RowGlide has an id"), window, |state| {
            let mut moved = state.height != Some(measured);
            if height_glide && moved && state.height.is_some() {
                state.run = Some((bounds.size.height, now));
            }
            state.height = Some(measured);
            if state.retarget || held {
                moved |= state.target != Some(measured);
                state.target = Some(measured);
                state.retarget = false;
                state.gliding = true;
            }
            moved
        });
        if moved {
            window.request_animation_frame();
        }
        // A row gliding its height at rest is clipped only while its room is
        // short of its content, so what reaches past it (a card's shadow) is
        // cut off no longer than it has to be.
        let clip =
            !matches!(self.glide, Glide::Height { .. }) || bounds.size.height + px(0.5) < measured;
        if let GlideLayout::Clipped { clip: clipped, .. } = layout {
            *clipped = clip;
        }
        window.with_content_mask(clip.then_some(ContentMask { bounds }), |window| {
            self.child
                .prepaint_at(bounds.origin + point(px(0.), lift), window, cx);
        });
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
            GlideLayout::Clipped { clip, .. } => {
                window.with_content_mask(clip.then_some(ContentMask { bounds }), |window| {
                    self.child.paint(window, cx);
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Arrival, Attach, changes};

    /// One replacement that reorders, inserts and removes at once: a new row
    /// between kept ones is inserted and one after them all appended; a row
    /// that went folds below the kept row above it, or above the first kept
    /// row when none was above it. Kept rows, moved or not, are neither.
    #[test]
    fn a_replacement_tells_what_came_and_what_went() {
        let replaced = changes(&[1, 2, 3, 4], &[3, 5, 1, 6]);
        assert_eq!(
            replaced.arrived,
            [(1, Arrival::Inserted), (3, Arrival::Appended)]
        );
        assert_eq!(
            replaced.left,
            [(1, Attach::Below(1)), (3, Attach::Below(3))]
        );
        assert_eq!(
            changes(&[1, 2, 3], &[3]).left,
            [(0, Attach::Above(3)), (1, Attach::Above(3))]
        );
    }
}
