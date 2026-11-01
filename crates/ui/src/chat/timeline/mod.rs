use super::*;

mod expansion;
mod render;
pub(super) mod residency;

pub(super) use expansion::{
    AutoActivityExpansions, AutoActivityRecency, auto_activity_snapshot_keys,
};
pub(super) use render::FullOutput;
pub(super) use residency::{
    PendingMarkdownBuild, tail_row_window, transcript_selection, viewport_row_window,
};

/// Vertical rhythm between turns. Turns are separated by space and typographic
/// hierarchy alone — there is deliberately no rule/divider under the user bubble.
pub(super) const TURN_GAP: f32 = 32.;

/// Vertical rhythm between the segments of one turn.
pub(super) const SEGMENT_GAP: f32 = 10.;

// ListState retains measured row heights, so variable-height turns use the same
// pixel geometry as scrolling rather than treating a long turn as one short row.
pub(super) fn history_screens_covered(list: &ListState, leading: gpui::Pixels) -> Option<f32> {
    let height = list.viewport_bounds().size.height;
    (height > px(0.)).then(|| {
        let above = -list.scroll_px_offset_for_scrollbar().y - leading;
        f32::from(above.max(px(0.))) / f32::from(height)
    })
}

pub(super) fn jump_to_latest_visible(list: &ListState) -> bool {
    let height = list.viewport_bounds().size.height;
    // The scrollbar geometry retains size hints for invalidated rows. Unlike
    // is_scrolled_to_end(), it does not require every offscreen row to have
    // been measured. Prepending shifts the extent and offset together.
    height > px(0.)
        && list.max_offset_for_scrollbar().y + list.scroll_px_offset_for_scrollbar().y > height
}

/// Move an anchor whose offset passes the end of its row onto the row that
/// offset lands in. A remeasure of the anchor row would otherwise clamp the
/// offset to that row's height. False while a row it passes is unmeasured.
pub(super) fn settle_carried_anchor(list: &ListState) -> bool {
    if list.is_following_tail() {
        return true;
    }
    let mut anchor = list.logical_scroll_top();
    let start = anchor.item_ix;
    let settled = loop {
        if anchor.item_ix + 1 >= list.item_count() {
            break true;
        }
        let Some(bounds) = list.bounds_for_item(anchor.item_ix) else {
            break false;
        };
        if anchor.offset_in_item < bounds.size.height {
            break true;
        }
        anchor.offset_in_item -= bounds.size.height;
        anchor.item_ix += 1;
    };
    if anchor.item_ix != start {
        list.scroll_to(anchor);
    }
    settled
}

/// Pixels between the top of the content and the top of the viewport. A list
/// anchored past its last row reports the whole content height; on screen it
/// is scrolled no further than the end.
pub(super) fn capture_scrolled(list: &ListState) -> gpui::Pixels {
    (-list.scroll_px_offset_for_scrollbar().y).min(list.max_offset_for_scrollbar().y)
}

pub(super) struct CaptureSession {
    anchor: ListOffset,
    following: bool,
    /// The row at the top of the viewport when the capture began; capture
    /// offsets are measured from it. A row, not a pixel count: rows measured
    /// for the first time during the capture change every pixel count above
    /// them.
    origin: ListOffset,
}

/// One tile's scroll, in pixels from the capture's starting top.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaptureScroll {
    /// The offset reached: smaller than asked past the end of the measured
    /// content, larger above its start.
    pub reached: gpui::Pixels,
    /// Whether the scroll changed the list, so the frame on screen is stale.
    pub moved: bool,
}

/// The timeline as the surface of the platform's scrolling screenshot.
///
/// The capturer works in pixels relative to the viewport at the start of the
/// capture: it asks for the content that would sit `offset` below that top,
/// the timeline scrolls there as far as measured content allows, and the
/// platform reads back the next painted frame. The history reservation above
/// the first turn is blank and never captured.
impl ChatView {
    /// The timeline's on-screen rectangle, when it has something to capture.
    pub(crate) fn capture_viewport(&self) -> Option<gpui::Bounds<gpui::Pixels>> {
        let bounds = self.list_state.viewport_bounds();
        (!self.rows.is_empty() && bounds.size.height > px(0.) && bounds.size.width > px(0.))
            .then_some(bounds)
    }

    pub(crate) fn capture_begin(&mut self, cx: &mut Context<Self>) -> bool {
        if self.capture_viewport().is_none() {
            return false;
        }
        if self.capture.is_none() {
            let list = &self.list_state;
            let anchor = list.logical_scroll_top();
            let following = list.is_following_tail();
            // Layout re-arms tail following whenever the bottom comes into
            // view, which the capture's last tile does; `Normal` keeps the
            // list where the capture put it.
            list.set_follow_mode(FollowMode::Normal);
            // An anchor past the last row is laid out from the end, but
            // `scroll_by` counts its pixel offset from the top of every row:
            // a scroll relative to it would land a viewport short. Re-anchor
            // at the row on screen first.
            if anchor.item_ix >= list.item_count() {
                list.scroll_by(-list.viewport_bounds().size.height);
            }
            self.capture = Some(CaptureSession {
                anchor,
                following,
                origin: list.logical_scroll_top(),
            });
            self.reservation_scroll_back = None;
            cx.notify();
        }
        true
    }

    /// Scroll so the content `offset` pixels below the capture's starting top
    /// is at the top of the viewport. `None` outside a capture.
    pub(crate) fn capture_scroll(
        &mut self,
        offset: gpui::Pixels,
        cx: &mut Context<Self>,
    ) -> Option<CaptureScroll> {
        let session = self.capture.as_ref()?;
        let list = &self.list_state;
        let before = list.logical_scroll_top();
        // Pixel counts are only comparable within one measurement of the
        // rows, so the origin's is taken now, along with the target's.
        list.scroll_to(session.origin);
        // `scroll_by` counts from the top of every row even past the end,
        // where layout shows the list scrolled no further than the end.
        let origin = -list.scroll_px_offset_for_scrollbar().y;
        let end = list.max_offset_for_scrollbar().y;
        let target = (origin + offset)
            .min(end)
            .max(self.history_placeholder_height)
            .max(px(0.));
        list.scroll_by(target - origin);
        let after = list.logical_scroll_top();
        let moved =
            after.item_ix != before.item_ix || after.offset_in_item != before.offset_in_item;
        if moved {
            cx.notify();
        }
        Some(CaptureScroll {
            reached: capture_scrolled(list) - origin.min(end),
            moved,
        })
    }

    /// Whether a frame painted now shows the rows' content rather than a
    /// Markdown placeholder still being built.
    pub(crate) fn capture_settled(&self) -> bool {
        self.pending_md_builds.is_empty()
    }

    pub(crate) fn capture_end(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.capture.take() else {
            return;
        };
        let list = &self.list_state;
        list.set_follow_mode(FollowMode::Tail);
        if !session.following {
            list.scroll_to(session.anchor);
        }
        cx.notify();
    }
}

impl ChatView {
    /// Mirror timeline markdown text into synchronous [`MarkdownState`] entities.
    pub(super) fn sync_markdown_states(&mut self, cx: &mut Context<Self>) {
        self.timeline_stale = false;
        // ListState owns follow intent: user scrolling pauses Tail until the
        // bottom is reached again. Content updates must not override that pause.
        let session_key = self.session_id.clone();
        let session_changed = session_key != self.session_key;
        if session_changed {
            self.expanded.clear();
            self.auto_activity_expansions
                .activate_session(session_key.as_deref());
        }
        let awaiting_activity_snapshot = self
            .auto_activity_expansions
            .awaiting_session_snapshot(session_key.as_deref());
        let continuity = if session_changed {
            TimelineContinuity::NewSession
        } else if self
            .workspace_store
            .read(cx)
            .history_available_for(self.session_id.as_deref())
        {
            TimelineContinuity::PartialFirstTurn
        } else {
            TimelineContinuity::Complete
        };
        let (list_sync, activity_snapshot_keys) = self
            .workspace_store
            .read(cx)
            .with_timeline_for(self.session_id.as_deref(), |timeline| {
                let list_sync = self.turn_index_cache.sync(
                    &mut self.rows,
                    &timeline.turns,
                    &timeline.entries,
                    &self.expanded,
                    continuity,
                );
                let activity_snapshot_keys = awaiting_activity_snapshot
                    .then(|| auto_activity_snapshot_keys(&timeline.entries));
                (list_sync, activity_snapshot_keys)
            })
            .unwrap_or_else(|| {
                let list_sync = self.turn_index_cache.sync(
                    &mut self.rows,
                    &[],
                    &[],
                    &self.expanded,
                    continuity,
                );
                (list_sync, None)
            });

        if let Some(session_key) = session_key.as_deref()
            && let Some(keys) = activity_snapshot_keys
        {
            self.auto_activity_expansions
                .hydrate_session_snapshot(session_key, keys);
        }

        let requested_turn = session_key
            .as_deref()
            .and_then(|session_id| self.workspace_store.read(cx).pending_chat_turn(session_id));
        if session_changed {
            self.md_states.clear();
            self.selection
                .update(cx, |selection, cx| selection.clear(cx));
            self.pending_md_builds.clear();
            self.command_panels.borrow_mut().clear();
            self.inline_diffs.clear();
            self.full_outputs.clear();
            self.highlighted_turn = None;
            self.session_key = session_key;
            self.painted_tail_start = None;
            self.markdown_visible_rows = tail_row_window(self.rows.len());
            self.markdown_scroll_top = Some(self.rows.len());
        }

        match list_sync {
            ListSync::None => {}
            ListSync::Prepend { count, remeasure } => {
                let anchor = self.list_state.logical_scroll_top();
                let following = self.list_state.is_following_tail();
                self.list_state.splice(0..0, count);
                // A page requested before a capture began still lands during
                // it; the capture's anchor names the same turn as before.
                if let Some(capture) = &mut self.capture {
                    capture.anchor.item_ix += count;
                    capture.origin.item_ix += count;
                }
                // The edge padding and the reservation move to the new first
                // turn. Remove them from the former first turn without moving
                // its content.
                self.list_state.remeasure_items(count..count + 1);
                if anchor.item_ix == 0 && !following {
                    let into_reservation = self.leading_space() - anchor.offset_in_item;
                    let anchor = ListOffset {
                        item_ix: count,
                        offset_in_item: (-into_reservation).max(px(0.)),
                    };
                    self.list_state.scroll_to(anchor);
                    // A reader inside the padding or the reservation keeps that
                    // pixel position, which now belongs to the page above. A
                    // list anchor cannot precede its row (rows above it stay
                    // unpainted), so the render after the next frame walks
                    // back over the page once layout has measured it.
                    if into_reservation > px(0.) {
                        let distance = self
                            .reservation_scroll_back
                            .take()
                            .map_or(px(0.), |back| back.distance);
                        self.reservation_scroll_back = Some(ReservationScrollBack {
                            distance: distance + into_reservation,
                            measured: false,
                        });
                    }
                }
                for index in remeasure {
                    self.list_state.remeasure_items(index..index + 1);
                }
            }
            ListSync::Reset { count } => {
                self.reservation_scroll_back = None;
                self.list_state.reset(count);
                if session_changed {
                    // Reset also clears stale item focus handles. A newly opened
                    // session always starts actively following its tail.
                    self.list_state.set_follow_mode(FollowMode::Tail);
                }
            }
            ListSync::Incremental {
                splices,
                remeasure,
                carried,
            } => {
                let anchor = self.list_state.logical_scroll_top();
                let carried_offset = (!self.list_state.is_following_tail()
                    && carried.iter().any(|range| range.contains(&anchor.item_ix)))
                .then_some(anchor.offset_in_item);
                for (range, count) in splices.into_iter().rev() {
                    // The row above a splice changes its padding (the former
                    // last row hands its edge padding to the new one); its
                    // cached height must not keep it.
                    if range.start > 0 {
                        self.list_state
                            .remeasure_items(range.start - 1..range.start);
                    }
                    self.list_state.splice(range, count);
                }
                for index in remeasure {
                    self.list_state.remeasure_items(index..index + 1);
                }
                // A splice over the anchor moves it to the first new row at
                // offset zero; the carried content starts where the old row
                // did, so the old offset still names the same pixel. It may
                // pass that row's end until `settle_carried_anchor` measures it.
                if let Some(offset) = carried_offset {
                    self.list_state.scroll_to(ListOffset {
                        item_ix: self.list_state.logical_scroll_top().item_ix,
                        offset_in_item: offset,
                    });
                    self.carried_anchor = true;
                }
            }
        }

        if self.list_state.is_following_tail() {
            self.markdown_visible_rows = self.markdown_tail_window();
            self.markdown_scroll_top = Some(self.rows.len());
        }

        let requested_row = requested_turn.and_then(|turn| rows_of_turn(&self.rows, turn).next());
        if let (Some(turn), Some(row)) = (requested_turn, requested_row) {
            self.reservation_scroll_back = None;
            self.list_state.pause_following_tail();
            self.list_state.scroll_to(ListOffset {
                item_ix: row,
                offset_in_item: px(0.),
            });
            self.highlighted_turn = Some(turn);
            self.markdown_visible_rows = viewport_row_window(row, self.rows.len());
            self.markdown_scroll_top = Some(row);
            if let Some(session_id) = self.session_key.as_deref() {
                self.workspace_store.update(cx, |store, _cx| {
                    store.take_pending_chat_turn(session_id, turn);
                });
            }
        }

        self.sync_markdown_residency(requested_row, cx);
        self.sync_deciding_plan(cx);

        let scheduled_limit_resume = {
            let store = self.workspace_store.read(cx);
            let deadlines: HashSet<u64> = store
                .composer_state_for(self.session_id.as_deref())
                .queue
                .into_iter()
                .flat_map(|queue| queue.messages)
                .filter_map(|message| message.fire_at_unix_secs)
                .collect();
            store
                .with_timeline_for(self.session_id.as_deref(), |timeline| {
                    timeline
                        .turn_retry
                        .as_ref()
                        .is_some_and(|retry| retry.retry_at.is_some())
                        || timeline.entries.iter().any(|entry| {
                            matches!(
                                entry.content,
                                EntryContent::Error {
                                    limit_resets_at: Some(resets_at),
                                    ..
                                } if deadlines.contains(&resets_at)
                            )
                        })
                })
                .unwrap_or(false)
        };
        if scheduled_limit_resume && self._limit_tick.is_none() {
            self._limit_tick = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }));
        } else if !scheduled_limit_resume {
            self._limit_tick = None;
        }
    }

    /// Invalidate the measured heights of every row of `turn`.
    fn remeasure_turn(&self, turn: usize) {
        let rows = rows_of_turn(&self.rows, turn);
        if !rows.is_empty() {
            self.list_state.remeasure_items(rows);
        }
    }

    /// Invalidate the measured height of the row that renders the activity
    /// `entry_id`; an expanded Work Log gives each activity a row of its own.
    fn remeasure_activity(&self, turn: usize, entry_id: &str, cx: &App) {
        let row = self
            .workspace_store
            .read(cx)
            .with_timeline_for(self.session_id.as_deref(), |timeline| {
                let entries = &timeline.entries;
                let start = entries.partition_point(|entry| entry.turn < turn);
                let index = start
                    + entries[start..]
                        .iter()
                        .take_while(|entry| entry.turn == turn)
                        .position(|entry| entry.id == entry_id)?;
                row_of_entry(&self.rows, index, turn)
            })
            .flatten();
        match row {
            Some(row) => self.list_state.remeasure_items(row..row + 1),
            None => self.remeasure_turn(turn),
        }
    }
}

/// The walk back into a page that replaced the reservation the reader had
/// scrolled into. See [`ChatView::apply_reservation_scroll_back`].
#[derive(Debug, Clone, Copy)]
pub(super) struct ReservationScrollBack {
    distance: gpui::Pixels,
    /// A frame has laid the page out since the walk back was requested, so
    /// `scroll_by` can count its rows.
    measured: bool,
}

impl ChatView {
    /// Space above the first turn's content: the edge padding plus the history
    /// reservation. Both move to the new first row when a page lands.
    pub(super) fn leading_space(&self) -> gpui::Pixels {
        px(TIMELINE_EDGE_PADDING) + self.history_placeholder_height
    }

    /// Walk back over a landed page inside the render that follows the frame
    /// which measured it. The walk is a relative scroll, so packets that moved
    /// the reader in between (a finger still panning, or a fling ticking on
    /// the frame) stay applied: those resolved against the painted anchor,
    /// and the anchor painted after this render carries the walk back.
    pub(super) fn apply_reservation_scroll_back(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(back) = self.reservation_scroll_back.as_mut() else {
            return;
        };
        if !back.measured {
            back.measured = true;
            // The next frame may be idle otherwise; make it render.
            let chat = cx.entity().downgrade();
            window.on_next_frame(move |_, cx| {
                let _ = chat.update(cx, |_, cx| cx.notify());
            });
            return;
        }
        let back = self.reservation_scroll_back.take().expect("checked above");
        let list = &self.list_state;
        let anchor = list.logical_scroll_top();
        // Following the tail or resting past the end means the reader left
        // for the bottom in between; the walk back would pull them off it.
        if self.capture.is_none() && !list.is_following_tail() && anchor.item_ix < list.item_count()
        {
            list.scroll_by(-back.distance);
        }
    }
}

/// The breathing room between the timeline and the header/composer. A phone
/// has none: its rows already inset themselves and the screen is short.
pub(super) fn timeline_inset(cx: &App) -> gpui::Pixels {
    if crate::window_seam::is_mobile(cx) {
        px(0.)
    } else {
        px(TIMELINE_INSET)
    }
}

/// Desktop timeline inset above and below the list, in pixels.
pub(super) const TIMELINE_INSET: f32 = 8.;

/// Padding inside the list on its first and last row, in pixels. Unlike
/// `TIMELINE_INSET` it scrolls with the content: the header and composer still
/// clip the timeline flush, but a reader at either end sees a little air
/// between the edge turn and the chrome. It lives on the rows rather than on
/// the List element, so the scrollbar, the wheel and tail-resume all measure
/// the same extent.
pub(super) const TIMELINE_EDGE_PADDING: f32 = 8.;
