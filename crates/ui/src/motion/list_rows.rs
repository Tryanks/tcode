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
//!    input. While either moves it asks for frames.
//! 3. Each render the owner calls [`ListRows::settle`] and remeasures the
//!    rows it names, whose folds have gone.
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

use std::{collections::HashMap, ops::Range, rc::Rc};

#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
#[cfg(target_family = "wasm")]
use web_time::Instant;

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, ContentMask, Element, ElementId, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, ParentElement as _, Pixels, Style, Styled as _,
    Window, div, point, prelude::FluentBuilder as _, px, relative, size,
};
use gpui_base::motion::{Easing, MotionStatus, Timing};

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
}

impl<A> Arrived<A> {
    /// How far in the row is, from absent to at rest, `now`.
    pub(crate) fn progress(&self, now: Instant) -> f32 {
        eased(now.saturating_duration_since(self.at))
    }

    pub(crate) fn settled(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.at) >= crate::edge_back::SETTLE
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
}

impl<A, V> Default for ListRows<A, V> {
    fn default() -> Self {
        Self {
            arrivals: HashMap::new(),
            leaving: Vec::new(),
        }
    }
}

impl<A: Copy, V> ListRows<A, V> {
    /// Row `key` came in `at`. A row still folding away turns back from
    /// where it is.
    pub(crate) fn arrive(&mut self, key: u64, arrival: Arrival, note: A, at: Instant) {
        let mut at = at;
        if let Some(index) = self.leaving.iter().position(|left| left.key == key) {
            let left = self.leaving.remove(index);
            let shown = 1. - eased(at.saturating_duration_since(left.at));
            at = at.checked_sub(elapsed_for(shown)).unwrap_or(at);
        }
        self.arrivals.insert(key, Arrived { at, arrival, note });
    }

    /// Row `key` left `at`, drawn from `value` against `attach` until it has
    /// folded away. A row still arriving folds from where it is; rows that
    /// were folding against it fold against `attach` now.
    pub(crate) fn leave(&mut self, key: u64, value: V, attach: Attach, at: Instant) {
        let mut start = at;
        if let Some(arrived) = self.arrivals.remove(&key) {
            let shown = arrived.progress(at);
            start = at.checked_sub(elapsed_for(1. - shown)).unwrap_or(at);
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
        let arrived = self.arrivals.get(&key)?;
        (!arrived.settled(now)).then(|| (*arrived, arrived.progress(now)))
    }

    /// The rows folding away against row `key`: those above its content,
    /// then those below, each in the order they stood.
    pub(crate) fn leaving(&self, key: u64, now: Instant) -> (Vec<Left<'_, V>>, Vec<Left<'_, V>>) {
        let left = |side: fn(u64) -> Attach| {
            self.leaving
                .iter()
                .filter(|left| left.attach == side(key))
                .map(|left| Left {
                    key: left.key,
                    value: &left.value,
                    progress: 1. - eased(now.saturating_duration_since(left.at)),
                })
                .filter(|left| left.progress > 0.)
                .collect::<Vec<_>>()
        };
        (left(Attach::Above), left(Attach::Below))
    }

    /// Lets go of the arrivals that have come in and the rows that have
    /// folded away, returning the rows those folded against: their heights
    /// changed, so the list measures them again.
    pub(crate) fn settle(&mut self, now: Instant) -> Vec<u64> {
        self.arrivals.retain(|_, arrived| !arrived.settled(now));
        let mut folded = Vec::new();
        self.leaving.retain(|left| {
            let gone = now.saturating_duration_since(left.at) >= crate::edge_back::SETTLE;
            if gone {
                folded.push(left.attach.key());
            }
            !gone
        });
        folded
    }

    /// Everything in place at once: a list that changed without the reader
    /// seeing it happen.
    pub(crate) fn clear(&mut self) {
        self.arrivals.clear();
        self.leaving.clear();
    }
}

fn curve() -> Timing {
    Timing::new(crate::edge_back::SETTLE).ease(Easing::Custom(Rc::new(
        gpui_base::animation::ease_out_cubic,
    )))
}

fn eased(elapsed: std::time::Duration) -> f32 {
    curve().sample(elapsed).directed_progress
}

/// How long the glide takes to come `progress` of the way: the inverse of
/// its ease-out cubic.
fn elapsed_for(progress: f32) -> std::time::Duration {
    let progress = progress.clamp(0., 1.);
    crate::edge_back::SETTLE.mul_f32(1. - (1. - progress).cbrt())
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
    /// Arriving, or leaving: its height is its measurement scaled by the
    /// progress, and it fades and rises in, or sinks and fades out.
    Enter(f32),
    /// The live end of a list the reader follows: each time its `content`
    /// changes, its height glides to the new measurement. At rest it is laid
    /// out as a plain child, so height that changes on its own (a fold
    /// inside it, an image that loaded) moves once, by its own motion.
    Tail { content: u64 },
}

/// A list row's content, gliding its layout height.
///
/// The same element in every [`Glide`], so what it draws keeps its element
/// ids, and with them its state, when a glide starts or ends.
pub(crate) struct RowGlide {
    id: ElementId,
    glide: Glide,
    child: AnyElement,
}

impl RowGlide {
    pub(crate) fn new(id: impl Into<ElementId>, glide: Glide, child: impl IntoElement) -> Self {
        let child = div()
            .w_full()
            .flex()
            .flex_col()
            .when_some(
                match glide {
                    Glide::Enter(progress) if progress < 1. => Some(progress),
                    _ => None,
                },
                |child, progress| child.opacity(progress),
            )
            .child(child)
            .into_any_element();
        Self {
            id: id.into(),
            glide,
            child,
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
            Glide::Enter(progress),
            div()
                .relative()
                .w_full()
                .child(child)
                .child(super::shield()),
        )
    }
}

#[derive(Clone, Copy, Default)]
struct Measured {
    /// The child's height as last laid out.
    height: Option<Pixels>,
    /// The tail's content when the glide last took aim.
    content: Option<u64>,
    /// The measurement the tail glides to: the one taken after its content
    /// last changed.
    target: Option<Pixels>,
    /// The content changed; the next measurement is the new target.
    retarget: bool,
    /// The height is on its way to `target`.
    gliding: bool,
}

pub(crate) enum GlideLayout {
    /// The child is this element's layout.
    Plain,
    /// This element is laid out at the glided height; the child is laid out
    /// on its own and clipped to it, `lift` below its top.
    Clipped { lift: Pixels },
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
            Glide::Enter(progress) => {
                let measured = window.with_element_state(global_id, |state, _| {
                    let state: Measured = state.unwrap_or_default();
                    (state.height, state)
                });
                Some((
                    measured.unwrap_or_default() * progress,
                    super::lift(progress),
                ))
            }
            Glide::Tail { content } => {
                let state = window.with_element_state(global_id, |state, _| {
                    let mut state: Measured = state.unwrap_or_default();
                    if state.content.is_some_and(|seen| seen != content) {
                        state.retarget = true;
                    }
                    state.content = Some(content);
                    (state, state)
                });
                match (state.height, state.target) {
                    // The frame the content changes still shows the height it
                    // had; its measurement becomes the target the next frame
                    // glides to. Height that changes during a glide rides on
                    // it, a frame late.
                    (Some(measured), Some(target)) if state.retarget || state.gliding => {
                        let glided = gpui_base::motion::transition_with_status(
                            "tail",
                            target,
                            super::settle(),
                            window,
                            cx,
                        );
                        let gliding =
                            matches!(glided.status, MotionStatus::Delayed | MotionStatus::Running);
                        window.with_element_state(global_id, |state, _| {
                            let mut state: Measured = state.unwrap_or_default();
                            state.gliding = gliding;
                            ((), state)
                        });
                        Some(((glided.value + measured - target).max(px(0.)), px(0.)))
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
                    GlideLayout::Clipped { lift },
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
                if matches!(self.glide, Glide::Tail { .. }) {
                    let height = bounds.size.height;
                    window.with_element_state(
                        global_id.expect("RowGlide has an id"),
                        |state, _| {
                            let mut state: Measured = state.unwrap_or_default();
                            state.height = Some(height);
                            state.target = Some(height);
                            ((), state)
                        },
                    );
                }
                return;
            }
            GlideLayout::Clipped { lift } => *lift,
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
        let moved =
            window.with_element_state(global_id.expect("RowGlide has an id"), |state, _| {
                let mut state: Measured = state.unwrap_or_default();
                let mut moved = state.height != Some(measured);
                state.height = Some(measured);
                if state.retarget {
                    moved |= state.target != Some(measured);
                    state.target = Some(measured);
                    state.retarget = false;
                    state.gliding = true;
                }
                (moved, state)
            });
        if moved {
            window.request_animation_frame();
        }
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
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
            GlideLayout::Clipped { .. } => {
                window.with_content_mask(Some(ContentMask { bounds }), |window| {
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
