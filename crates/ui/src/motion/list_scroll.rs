//! A gpui `list` scrolled to a row, or to its end, gliding there.
//!
//! `ListState::scroll_to` and `scroll_to_end` move at once, and gpui-base has
//! no animated scroll. [`ListScroll`] moves the list a step each frame, from
//! where it was to the target as the list measures them that frame, so rows
//! measured on the way do not throw it off; it ends exactly at the target.

use std::{cell::RefCell, rc::Rc};

#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
#[cfg(target_family = "wasm")]
use web_time::Instant;

use gpui::{App, FollowMode, ListOffset, ListState, Pixels, Window, px};

/// How long a list takes to scroll a row into view.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// Where a [`ListScroll`] takes its list.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ScrollTarget {
    /// This place in a row at the top of the viewport.
    Row(ListOffset),
    /// The end, following the tail from there.
    End,
}

/// How far, in viewports, a glide travels at most: a target farther away is
/// first brought this near at once, so the glide reads as a move to it
/// rather than a blur of everything in between.
const REACH: f32 = 2.;

/// An animated scroll of one list, kept by the list's owner.
///
/// The owner starts it with [`ListScroll::scroll_to`] and moves it with
/// [`ListScroll::step`] each time it renders, before the list lays out.
/// Anything else that moves the list between two steps — the reader's
/// wheel, pan or scrollbar, which gpui-base's bounce and scrollbar apply
/// without the list's scroll handler hearing of it — stops it where it was
/// taken; [`ListScroll::cancel`] stops it too. Reduced motion, or a list not
/// yet laid out, goes to the target at once.
#[derive(Clone, Default)]
pub(crate) struct ListScroll(Rc<RefCell<Option<Run>>>);

#[derive(Clone, Copy)]
struct Run {
    target: ScrollTarget,
    /// Where the glide starts, as a place in a row, so the rows the list
    /// measures above it on the way do not move it.
    origin: ListOffset,
    at: Instant,
    /// Where the last step left the list.
    placed: ListOffset,
}

impl ListScroll {
    /// Glide `list` to `target` from where it is.
    pub(crate) fn scroll_to(&self, list: &ListState, target: ScrollTarget, cx: &App) {
        let viewport = list.viewport_bounds().size.height;
        if cx.reduce_motion() || viewport <= px(0.) {
            self.cancel();
            finish(list, target);
            return;
        }
        list.pause_following_tail();
        let here = scrolled(list);
        let to = top_of(list, target);
        let reach = viewport * REACH;
        if (to - here).abs() > reach {
            place(list, if to > here { to - reach } else { to + reach });
        } else {
            place(list, here);
        }
        let origin = list.logical_scroll_top();
        *self.0.borrow_mut() = Some(Run {
            target,
            origin,
            at: cx.background_executor().now(),
            placed: origin,
        });
    }

    /// Glide `list` just far enough that row `index` is in view, to where
    /// `ListState::scroll_to_reveal_item` would put it at once.
    pub(crate) fn reveal(&self, list: &ListState, index: usize, cx: &App) {
        let here = list.logical_scroll_top();
        list.scroll_to_reveal_item(index);
        let to = list.logical_scroll_top();
        list.scroll_to(here);
        if to != here {
            self.scroll_to(list, ScrollTarget::Row(to), cx);
        }
    }

    /// Move `list` to where the glide has got to, asking for the next frame
    /// while it is under way; at its end, the list is at the target.
    pub(crate) fn step(&self, list: &ListState, window: &mut Window, cx: &App) {
        let Some(run) = *self.0.borrow() else {
            return;
        };
        if list.is_scrollbar_dragging() || list.logical_scroll_top() != run.placed {
            self.cancel();
            return;
        }
        let progress = if cx.reduce_motion() {
            1.
        } else {
            crate::motion::Curve::new(GLIDE).progress(
                cx.background_executor()
                    .now()
                    .saturating_duration_since(run.at),
            )
        };
        if progress >= 1. {
            self.cancel();
            finish(list, run.target);
            return;
        }
        // Both ends in this frame's measurement of the rows.
        list.scroll_to(run.origin);
        let from = scrolled(list);
        let to = top_of(list, run.target);
        place(list, from + (to - from) * progress);
        if let Some(run) = self.0.borrow_mut().as_mut() {
            run.placed = list.logical_scroll_top();
        }
        window.request_animation_frame();
    }

    /// Go to the target at once: the list's rows are about to change under
    /// the glide, which names its places by row.
    pub(crate) fn settle(&self, list: &ListState) {
        if let Some(run) = self.0.borrow_mut().take() {
            finish(list, run.target);
        }
    }

    /// Stop where the list is: the reader took it over.
    pub(crate) fn cancel(&self) {
        self.0.borrow_mut().take();
    }

    /// Whether a glide is under way.
    pub(crate) fn running(&self) -> bool {
        self.0.borrow().is_some()
    }
}

/// Where `list` is scrolled to, from the top of its content, no further than
/// its end.
fn scrolled(list: &ListState) -> Pixels {
    (-list.scroll_px_offset_for_scrollbar().y).min(list.max_offset_for_scrollbar().y)
}

/// Where `target` scrolls `list` to, from the top of its content.
fn top_of(list: &ListState, target: ScrollTarget) -> Pixels {
    let end = list.max_offset_for_scrollbar().y;
    match target {
        ScrollTarget::End => end,
        ScrollTarget::Row(offset) => {
            let here = list.logical_scroll_top();
            list.scroll_to(offset);
            let top = scrolled(list);
            list.scroll_to(here);
            top.min(end)
        }
    }
}

/// Scroll `list` to `top` pixels below the top of its content. `scroll_by`
/// counts from the top of every row, so it is measured from the first.
fn place(list: &ListState, top: Pixels) {
    list.scroll_to(ListOffset::default());
    list.scroll_by(top.max(px(0.)));
}

fn finish(list: &ListState, target: ScrollTarget) {
    match target {
        ScrollTarget::Row(offset) => list.scroll_to(offset),
        ScrollTarget::End => {
            list.set_follow_mode(FollowMode::Tail);
            list.scroll_to_end();
        }
    }
}
