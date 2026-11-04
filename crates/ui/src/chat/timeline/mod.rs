use super::*;

#[cfg(test)]
mod catch_up;
mod expansion;
mod glide;
mod list;
mod render;
mod residency;
pub(super) mod rows;

use crate::motion::list_rows::{Arrival, Glide, RowGlide};
pub(super) use expansion::{
    AutoActivityExpansions, AutoActivityRecency, auto_activity_snapshot_keys,
};
pub(super) use render::{ActivityRow, FullOutput};
pub(super) use residency::{
    PendingMarkdownBuild, tail_row_window, transcript_selection, viewport_row_window,
};

/// A row that left while the reader looked: what it was drawn from, kept
/// until it has folded away against the row above it.
#[derive(Clone)]
pub(super) struct LeftRow {
    pub(super) row: TimelineRow,
    pub(super) turn: TurnMeta,
    pub(super) own: Vec<Arc<TimelineEntry>>,
}

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
            // Rows still rising settle at once: a tile shows each row whole.
            self.rows_motion.clear();
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
        let old_len = self.rows.len();
        let old_last_turn = self.rows.last().map(|row| row.turn);
        let folds = self
            .workspace_store
            .read(cx)
            .timeline_folds_for(self.session_id.as_deref());
        // Rows arrive live only from records streamed into the timeline the
        // reader is looking at. A fold (opening the thread, catching up on
        // what it missed, a page of history) brings rows that were already
        // there; the store holds the timeline still while it catches up, and
        // folds once it has.
        let regrouped = std::mem::take(&mut self.rows_regrouped);
        // The reader sees the rows change: a live record, or a Work Log they
        // expanded or collapsed.
        let seen = !session_changed
            && folds == self.timeline_folds
            && self.capture.is_none()
            && !cx.reduce_motion();
        self.timeline_folds = folds;
        // A collapsed Work Log's rows fold away from what they showed.
        let old_rows = (seen && regrouped).then(|| self.rows.clone());
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
                self.screen.sync(&timeline.entries);
                self.commands.sync(timeline);
                let activity_snapshot_keys = awaiting_activity_snapshot
                    .then(|| auto_activity_snapshot_keys(&timeline.entries));
                (list_sync, activity_snapshot_keys)
            })
            .unwrap_or_else(|| {
                self.screen.sync(&[]);
                self.commands = Default::default();
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
            self.start_scene(cx);
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

        if !matches!(list_sync, ListSync::None) {
            self.list_scroll.settle(&self.list_state);
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
                let arrived = if seen {
                    crate::motion::list_rows::arrived_rows(&splices, &carried, old_len)
                } else {
                    Vec::new()
                };
                let left = old_rows
                    .map(|old| self.rows_leaving(&old, cx))
                    .unwrap_or_default();
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
                let arrived = if regrouped {
                    // An expanded live window was the collapsed run's, on
                    // screen already.
                    arrived
                        .into_iter()
                        .filter(|(index, _)| {
                            self.rows
                                .get(*index)
                                .is_some_and(|row| row.part != RowPart::WorkLogLive)
                        })
                        .collect()
                } else {
                    arrived
                };
                self.stamp_arrivals(arrived, old_last_turn, cx);
                self.fold_away(left, cx);
            }
        }

        if self.list_state.is_following_tail() {
            self.markdown_visible_rows = self.markdown_tail_window();
            self.markdown_scroll_top = Some(self.rows.len());
        }

        let requested_row = requested_turn.and_then(|turn| rows_of_turn(&self.rows, turn).next());
        if let (Some(turn), Some(row)) = (requested_turn, requested_row) {
            // Within the thread on screen the jump glides there; a thread
            // opened at a turn starts there.
            self.jump_to_row(row, !session_changed, cx);
            self.highlighted_turn = Some(
                self.workspace_store
                    .read(cx)
                    .absolute_turn_for(self.session_key.as_deref(), turn),
            );
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
    pub(in crate::chat) fn remeasure_activity(&self, turn: usize, entry_id: &str, cx: &App) {
        match self.row_of_activity(turn, entry_id, cx) {
            Some(row) => self.list_state.remeasure_items(row..row + 1),
            None => self.remeasure_turn(turn),
        }
    }

    /// The row that renders the activity `entry_id` of `turn`.
    fn row_of_activity(&self, turn: usize, entry_id: &str, cx: &App) -> Option<usize> {
        self.workspace_store
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
            .flatten()
    }

    /// Brings the row that renders the activity `entry_id` of `turn` to the
    /// top of the timeline, gliding there.
    pub(in crate::chat) fn scroll_to_activity(
        &mut self,
        turn: usize,
        entry_id: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.row_of_activity(turn, entry_id, cx) else {
            return;
        };
        self.jump_to_row(row, true, cx);
        self.sync_markdown_residency(Some(row), cx);
        cx.notify();
    }

    /// Puts `row` at the top of the timeline, gliding there or at once, and
    /// stops following the tail.
    fn jump_to_row(&mut self, row: usize, glide: bool, cx: &mut Context<Self>) {
        self.reservation_scroll_back = None;
        self.list_state.pause_following_tail();
        let target = ListOffset {
            item_ix: row,
            offset_in_item: px(0.),
        };
        if glide {
            self.list_scroll.scroll_to(
                &self.list_state,
                crate::motion::ScrollTarget::Row(target),
                cx,
            );
        } else {
            self.list_scroll.cancel();
            self.list_state.scroll_to(target);
        }
        self.markdown_visible_rows = viewport_row_window(row, self.rows.len());
        self.markdown_scroll_top = Some(row);
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

/// The breathing room between the timeline and the composer. A phone has
/// none: its rows already inset themselves and the screen is short. The
/// header gets none on any screen: it has no divider, so the eye measures the
/// room under its title to the first thing below, and only
/// [`TIMELINE_EDGE_PADDING`] keeps the first row off it.
pub(super) fn timeline_bottom_inset(cx: &App) -> gpui::Pixels {
    if crate::window_seam::is_mobile(cx) {
        px(0.)
    } else {
        px(TIMELINE_BOTTOM_INSET)
    }
}

/// Desktop timeline inset below the list, in pixels.
pub(super) const TIMELINE_BOTTOM_INSET: f32 = 8.;

/// Padding inside the list on its first and last row, in pixels. Unlike
/// `TIMELINE_BOTTOM_INSET` it scrolls with the content: the header and composer still
/// clip the timeline flush, but a reader at either end sees a little air
/// between the edge turn and the chrome. It lives on the rows rather than on
/// the List element, so the scrollbar, the wheel and tail-resume all measure
/// the same extent.
pub(super) const TIMELINE_EDGE_PADDING: f32 = 8.;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::tests::*;
    use crate::test_frames::{draw, draw_settled};
    use agent::ItemStatus;
    use gpui::TestAppContext;
    use std::sync::atomic::Ordering;
    use tcode_core::session::TurnMeta;

    #[gpui::test]
    fn a_paused_reader_on_the_live_window_stays_put_as_it_folds(cx: &mut TestAppContext) {
        use gpui::{ListOffset, Modifiers, VisualTestContext, px, size};
        use tcode_core::session::SteeringStatus;

        let tool = |id: &str| {
            entry(
                id,
                EntryContent::Item(ItemContent::ToolCall {
                    image_reads: Vec::new(),
                    name: "read".into(),
                    input: serde_json::json!({ "path": id }),
                    output: None,
                    status: ItemStatus::Completed,
                }),
            )
        };
        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta {
            running: true,
            ..TurnMeta::default()
        }];
        timeline.entries.push(entry("user", user_item("go")));
        for index in 0..20 {
            timeline.entries.push(tool(&format!("tool-{index}")));
        }
        // Steers queued behind the live window keep content below it.
        for index in 0..12 {
            timeline.entries.push(entry(
                &format!("steer-{index}"),
                EntryContent::Steer {
                    text: format!("queued steer {index}"),
                    status: SteeringStatus::Pending,
                    context_len: None,
                    attachments: Vec::new(),
                },
            ));
        }

        let (store, window_state, session_id) = seed_chat(cx, timeline.clone());
        cx.update(|cx| cx.set_reduce_motion(true));
        let (view, cx) = cx.add_window_view(|window, cx| {
            ChatView::new(
                store.clone(),
                window_state,
                Some(session_id.clone()),
                window,
                cx,
            )
        });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(700.)));
        draw_settled(cx);
        view.update(cx, |chat, cx| {
            chat.list_state.scroll_to(ListOffset::default());
            cx.notify();
        });
        draw_settled(cx);
        let header = cx
            .debug_bounds("worklog-header-0-tool-0")
            .expect("collapsed work log");
        cx.simulate_click(header.center(), Modifiers::default());
        draw_settled(cx);
        // The reader rests two activities into the live window.
        view.update(cx, |chat, cx| {
            let live = chat
                .rows
                .iter()
                .position(|row| row.part == RowPart::WorkLogLive)
                .expect("an expanded live work log");
            chat.list_state.scroll_to(ListOffset {
                item_ix: live,
                offset_in_item: px(70.),
            });
            cx.notify();
        });
        draw_settled(cx);
        let reading = cx
            .debug_bounds("activity-row-tool-18")
            .expect("the activity under the reader")
            .top();
        let top = |cx: &mut VisualTestContext| {
            cx.debug_bounds("activity-row-tool-18")
                .map(|bounds| bounds.top())
        };

        let update = |timeline: &Timeline, cx: &mut VisualTestContext| {
            store.update(cx, |store, cx| {
                store.set_session_replica_for_test(session_id.clone(), timeline.clone(), cx);
                cx.notify();
            });
            draw_settled(cx);
            draw_settled(cx);
        };
        timeline.entries.push(tool("tool-20"));
        timeline.entries.push(tool("tool-21"));
        update(&timeline, cx);
        assert_eq!(
            top(cx),
            Some(reading),
            "new activities folding the live window moved the reader"
        );

        timeline.turns[0].running = false;
        update(&timeline, cx);
        // Settled, the activity takes the folded rows' indent but keeps its
        // line.
        assert_eq!(
            top(cx),
            Some(reading),
            "the live window settling moved the reader"
        );
    }

    #[gpui::test]
    fn collapsing_work_log_preserves_following_message_bounds(cx: &mut TestAppContext) {
        use gpui::{Modifiers, px, size};

        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta {
            start_ts: Some(1_000),
            end_ts: Some(2_600),
            ..TurnMeta::default()
        }];
        timeline.entries = vec![
            entry("user", user_item("Check the layout.")),
            entry(
                "command",
                EntryContent::Item(ItemContent::CommandExecution {
                    command: "printf 'layout checked\\n'".into(),
                    output: "layout checked\n".into(),
                    exit_code: Some(0),
                    status: ItemStatus::Completed,
                }),
            ),
            entry(
                "answer",
                assistant(
                    "The layout check is complete. The final assistant message stays in the centred content column.\n\nThe footer belongs below this last line.",
                ),
            ),
        ];
        let (store, window_state, session_id) = seed_chat(cx, timeline);
        let (_view, cx) = cx.add_window_view(|window, cx| {
            ChatView::new(store, window_state, Some(session_id), window, cx)
        });
        for (mode, width) in [
            (crate::theme::ThemeMode::Light, 1_400.),
            (crate::theme::ThemeMode::Light, 393.),
            (crate::theme::ThemeMode::Dark, 1_400.),
            (crate::theme::ThemeMode::Dark, 393.),
        ] {
            cx.update(|window, cx| crate::theme::change_mode(mode, Some(window), cx));
            cx.simulate_resize(size(px(width), px(900.)));
            draw_settled(cx);
            let before = [
                cx.debug_bounds("markdown-block-0").expect("answer start"),
                cx.debug_bounds("markdown-block-1")
                    .expect("answer last line"),
            ];
            let footer_before = cx.debug_bounds("turn-time-row").expect("turn footer");
            for expanded in [true, false] {
                let header = cx
                    .debug_bounds("worklog-header-0-command")
                    .expect("work log toggle");
                cx.simulate_click(header.center(), Modifiers::default());
                draw_settled(cx);
                assert_eq!(
                    cx.debug_bounds("activity-row-command").is_some(),
                    expanded,
                    "the click must toggle the work log"
                );
            }
            let after = [
                cx.debug_bounds("markdown-block-0").expect("answer start"),
                cx.debug_bounds("markdown-block-1")
                    .expect("answer last line"),
            ];
            let footer_after = cx.debug_bounds("turn-time-row").expect("turn footer");
            assert_eq!(
                after, before,
                "collapsing moved or resized the answer at {width}px in {mode:?}"
            );
            assert_eq!(footer_after, footer_before, "collapsing moved the footer");
            assert!(
                footer_after.top() >= after[1].bottom(),
                "the footer overlaps the answer's last line: {after:?}, {footer_after:?}"
            );
        }
    }

    /// A `tcode_control` anchor opens its thread at the turn of the entry it
    /// names: here the user message of a turn above the window the thread
    /// opens with, in a log whose streamed replies the host merges on the
    /// wire, so no held record index stands for a log position.
    #[gpui::test]
    fn an_anchor_link_opens_its_thread_at_the_anchored_turn(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px, size};

        const TURNS: u64 = 12;
        const DELTAS: u64 = 40;
        let (host, session_id, _) = seeded_chat_host(cx, Timeline::default(), false);
        let seeded_id = session_id.clone();
        // Turn `t` takes the rows from `t * (DELTAS + 5)`; its user message is
        // the second, and the position a `tcode_control` anchor names it by.
        let rows = smol::block_on(host.update_state_for_test(move |state, cx| {
            let mut events = Vec::new();
            for turn in 0..TURNS {
                let answer = format!("answer-{turn}");
                events.push(agent::AgentEvent::TurnStarted {
                    turn_id: turn.to_string(),
                });
                events.push(agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                    id: format!("user-{turn}"),
                    parent_item_id: None,
                    content: ItemContent::UserMessage {
                        text: format!("Question {turn}"),
                        context_len: None,
                        attachments: Vec::new(),
                    },
                }));
                events.push(agent::AgentEvent::ItemStarted(agent::ThreadItem {
                    id: answer.clone(),
                    parent_item_id: None,
                    content: ItemContent::AssistantMessage {
                        text: String::new(),
                        summarized: false,
                    },
                }));
                events.extend((0..DELTAS).map(|_| agent::AgentEvent::Delta {
                    item_id: answer.clone(),
                    kind: agent::DeltaKind::AssistantText,
                    text: "word ".into(),
                }));
                events.push(agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                    id: answer,
                    parent_item_id: None,
                    content: ItemContent::AssistantMessage {
                        text: "word ".repeat(DELTAS as usize),
                        summarized: false,
                    },
                }));
                events.push(agent::AgentEvent::TurnCompleted {
                    turn_id: turn.to_string(),
                    status: agent::TurnStatus::Completed,
                    usage: None,
                });
            }
            for (ts, event) in events.iter().enumerate() {
                state.record_event_for_replica_test(&seeded_id, ts as u64, event, cx);
            }
            events.len() as u64
        }))
        .expect("record the streamed turns");
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        driver.wait_until(cx, &store, "workspace baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });
        let link = driver.link.clone();
        let page = driver.await_with_events(
            cx,
            &store,
            link.query(tcode_protocol::Query::SessionHistoryPage {
                session_id: session_id.clone(),
                before: u64::MAX,
                limit: 1,
            }),
        );
        assert!(
            matches!(
                page,
                Ok(tcode_protocol::QueryResponse::SessionHistoryPage { end, .. }) if end == rows
            ),
            "the recorded events are the log's only rows: {page:?}"
        );
        let window_state = cx.new(|_| WindowState::new(false));
        let (chat, cx) = cx.add_window_view(|window, cx| {
            ChatView::new(
                store.clone(),
                window_state,
                Some(session_id.clone()),
                window,
                cx,
            )
        });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(700.)));
        crate::store::tests::place_shown_threads(&store, cx);
        // The shell binds its pane to the thread the link opens.
        chat.update(cx, |chat, cx| {
            chat.set_session(Some(session_id.clone()), cx)
        });

        let opened = |cx: &mut VisualTestContext| {
            let view = cx.update(|window, _| window.root::<ChatView>().flatten())?;
            let (row, highlighted, first) = view.read_with(cx, |chat, cx| {
                let row = chat.list_state.logical_scroll_top().item_ix;
                (
                    chat.rows.get(row).map(|row| row.turn),
                    chat.highlighted_turn,
                    chat.workspace_store
                        .read(cx)
                        .absolute_turn_for(chat.session_id.as_deref(), 0),
                )
            });
            let turn = row.filter(|turn| highlighted == Some(first + *turn))?;
            store
                .read_with(cx, |store, _| {
                    store.with_timeline_for(store.sole_thread().as_deref(), |timeline| {
                        timeline
                            .entries
                            .iter()
                            .find(|entry| {
                                entry.turn == turn
                                    && matches!(
                                        entry.content,
                                        EntryContent::Item(ItemContent::UserMessage { .. })
                                    )
                            })
                            .map(|entry| entry.id.clone())
                    })
                })
                .flatten()
        };
        // Turn 1 is above the opening window; turn 9 is inside it, in the
        // thread already open.
        for turn in [1, 9] {
            let expected = format!("user-{turn}");
            store.update(cx, |store, cx| {
                let position = turn * (DELTAS + 5) + 1;
                store.show_thread_at_position(session_id.clone(), position, cx);
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while opened(cx).as_deref() != Some(expected.as_str()) {
                assert!(Instant::now() < deadline, "waiting for {expected}");
                driver.step(cx, &store);
                draw_settled(cx);
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        driver.shutdown(cx, &store, &host);
    }

    #[gpui::test]
    fn timeline_scrollbar_drag_pauses_tail_following(cx: &mut TestAppContext) {
        use gpui::{
            Context, IntoElement, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, Render,
            Window, point, px,
        };

        struct ChatRoot(Entity<ChatView>);
        impl Render for ChatRoot {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.0.clone()
            }
        }

        // A phone and a tablet: the shell's layout rule, applied by hand since
        // no shell is mounted here.
        cx.update(|cx| crate::window_seam::override_mobile_for_test(cx, true));
        for (width, height) in [(393., 852.), (1024., 768.)] {
            let (store, window_state, session_id) = seed_chat(cx, synthetic_markdown_timeline(30));
            let (view, cx) = cx.add_window_view(|window, cx| {
                ChatRoot(cx.new(|cx| {
                    ChatView::new(store, window_state.clone(), Some(session_id), window, cx)
                }))
            });
            cx.simulate_resize(gpui::size(px(width), px(height)));
            cx.update(|window, cx| {
                let compact = crate::window_seam::window_is_compact(window, cx);
                window_state.update(cx, |state, _| state.compact = compact);
            });
            cx.run_until_parked();
            draw_settled(cx);
            let list = view.read_with(cx, |view, cx| view.0.read(cx).list_state.clone());
            let viewport = list.viewport_bounds();
            let tail = list.scroll_px_offset_for_scrollbar().y;
            assert!(list.is_following_tail());
            assert!(list.max_offset_for_scrollbar().y > viewport.size.height);

            let thumb = point(viewport.right() - px(5.), viewport.bottom() - px(8.));
            cx.simulate_mouse_move(thumb, None, Modifiers::default());
            draw_settled(cx);
            cx.simulate_event(MouseDownEvent {
                position: thumb,
                button: MouseButton::Left,
                ..Default::default()
            });
            let target = point(thumb.x, viewport.center().y);
            cx.simulate_mouse_move(target, Some(MouseButton::Left), Modifiers::default());
            cx.simulate_event(MouseUpEvent {
                position: target,
                button: MouseButton::Left,
                ..Default::default()
            });
            draw_settled(cx);

            assert!(
                list.scroll_px_offset_for_scrollbar().y > tail + viewport.size.height,
                "dragging the visible scrollbar must scroll the conversation at width {width}"
            );
            assert!(!list.is_following_tail());

            // A finger on the thumb drags the scrollbar too, instead of
            // panning the conversation underneath it.
            list.set_follow_mode(gpui::FollowMode::Tail);
            list.scroll_to_end();
            draw_settled(cx);
            assert!(list.is_following_tail());
            cx.simulate_mouse_move(thumb, None, Modifiers::default());
            draw_settled(cx);
            for (phase, position) in [
                (gpui::TouchPhase::Started, thumb),
                (gpui::TouchPhase::Moved, target),
                (gpui::TouchPhase::Ended, target),
            ] {
                cx.simulate_event(gpui::TouchDragEvent {
                    phase,
                    start_position: thumb,
                    position,
                });
            }
            draw_settled(cx);
            assert!(
                list.scroll_px_offset_for_scrollbar().y > tail + viewport.size.height,
                "touch-dragging the scrollbar must scroll the conversation at width {width}"
            );
            assert!(!list.is_following_tail());
        }
    }

    #[gpui::test]
    fn touch_pan_pauses_tail_following_and_shows_the_pill(cx: &mut TestAppContext) {
        use gpui::{
            Context, IntoElement, PlatformInput, Render, TouchEvent, TouchId, TouchPhase, Window,
            point, px,
        };
        struct TouchChat(Entity<ChatView>);
        impl Render for TouchChat {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.0.clone()
            }
        }
        let (store, window_state, session_id) = seed_chat(cx, synthetic_markdown_timeline(30));
        window_state.update(cx, |state, _| state.compact = true);
        let (root, cx) = cx.add_window_view(|window, cx| {
            TouchChat(cx.new(|cx| ChatView::new(store, window_state, Some(session_id), window, cx)))
        });
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        cx.run_until_parked();
        draw_settled(cx);
        let list = root.read_with(cx, |root, cx| root.0.read(cx).list_state.clone());
        let viewport = list.viewport_bounds();
        let height = viewport.size.height;
        assert!(list.is_following_tail());
        assert!(cx.debug_bounds("scroll-to-end").is_none());
        let touch = |phase, y: gpui::Pixels, cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                window.dispatch_event(
                    PlatformInput::Touch(TouchEvent {
                        id: TouchId(1),
                        phase,
                        position: point(viewport.center().x, y),
                        predicted_position: None,
                        force: None,
                    }),
                    cx,
                );
            });
            draw_settled(cx);
        };

        // The finger moves down: earlier content comes into view and following
        // pauses on the first step. Cancel avoids release momentum.
        let top = viewport.top() + px(10.);
        touch(TouchPhase::Started, top, cx);
        touch(TouchPhase::Moved, top + px(40.), cx);
        assert!(!list.is_following_tail(), "the first step pauses following");
        touch(TouchPhase::Moved, top + height * 3., cx);
        touch(TouchPhase::Cancelled, top + height * 3., cx);
        assert!(!list.is_following_tail());
        assert!(
            cx.debug_bounds("scroll-to-end").is_some(),
            "the pill follows the list geometry after a touch pan"
        );

        // Panning back to the end resumes following and hides the pill.
        let bottom = viewport.bottom() - px(10.);
        touch(TouchPhase::Started, bottom, cx);
        touch(TouchPhase::Moved, bottom - height * 4., cx);
        touch(TouchPhase::Cancelled, bottom - height * 4., cx);
        assert!(list.is_following_tail());
        assert!(cx.debug_bounds("scroll-to-end").is_none());
    }

    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "android"))]
    #[gpui::test]
    fn timeline_bounces_at_its_edges_without_moving_the_list(cx: &mut TestAppContext) {
        use gpui::{
            Context, IntoElement, PlatformInput, Render, TouchEvent, TouchId, TouchPhase, Window,
            point, px,
        };
        struct TouchChat(Entity<ChatView>);
        impl Render for TouchChat {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.0.clone()
            }
        }
        let (store, window_state, session_id) = seed_chat(cx, synthetic_markdown_timeline(30));
        window_state.update(cx, |state, _| state.compact = true);
        let (root, cx) = cx.add_window_view(|window, cx| {
            TouchChat(cx.new(|cx| ChatView::new(store, window_state, Some(session_id), window, cx)))
        });
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        draw_settled(cx);
        let list = root.read_with(cx, |root, cx| root.0.read(cx).list_state.clone());
        assert!(list.max_offset_for_scrollbar().y > px(0.));
        for at_end in [false, true] {
            draw_settled(cx);
            list.set_follow_mode(gpui::FollowMode::Normal);
            if at_end {
                list.scroll_to_end();
            } else {
                list.scroll_to(gpui::ListOffset {
                    item_ix: 0,
                    offset_in_item: px(0.),
                });
            }
            draw_settled(cx);
            let resting = list.viewport_bounds();
            let offset = list.scroll_px_offset_for_scrollbar().y;
            let row_selector = if at_end {
                "timeline-row-89"
            } else {
                "timeline-row-0"
            };
            let row = cx.debug_bounds(row_selector).expect("edge row");
            let composer = cx.debug_bounds("chat-composer").expect("composer");
            let touch = |phase, y: gpui::Pixels, cx: &mut gpui::VisualTestContext| {
                cx.update(|window, cx| {
                    window.dispatch_event(
                        PlatformInput::Touch(TouchEvent {
                            id: TouchId(1),
                            phase,
                            position: point(resting.center().x, y),
                            predicted_position: None,
                            force: None,
                        }),
                        cx,
                    );
                    let _ = window.draw(cx);
                });
            };
            let direction = if at_end { -1. } else { 1. };
            let start = resting.center().y;
            touch(TouchPhase::Started, start, cx);
            touch(TouchPhase::Moved, start + px(200. * direction), cx);
            let stretched = list.viewport_bounds();
            assert!(
                (stretched.top() - resting.top()) * direction > px(0.),
                "an outward pan stretches the timeline: {stretched:?} vs {resting:?}"
            );
            assert_eq!(stretched.size, resting.size);
            assert_eq!(
                list.scroll_px_offset_for_scrollbar().y,
                offset,
                "the stretch is a displacement, not a scroll"
            );
            let stretched_row = cx.debug_bounds(row_selector).expect("edge row");
            assert_eq!(
                stretched_row.top() - row.top(),
                stretched.top() - resting.top()
            );
            assert_eq!(
                cx.debug_bounds("chat-composer").expect("composer"),
                composer,
                "fixed chrome stays outside the stretch"
            );
            touch(TouchPhase::Cancelled, start + px(200. * direction), cx);
            assert_eq!(list.scroll_px_offset_for_scrollbar().y, offset);
        }
    }

    #[gpui::test]
    fn a_drag_right_after_a_flick_scrolls_the_timeline(cx: &mut TestAppContext) {
        use gpui::{point, px};
        let (store, window_state, session_id) = seed_chat(cx, synthetic_markdown_timeline(30));
        let (view, cx) = cx.add_window_view(|window, cx| {
            ChatView::new(store, window_state, Some(session_id), window, cx)
        });
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        draw_settled(cx);
        let list = view.read_with(cx, |chat, _| chat.list_state.clone());
        list.set_follow_mode(gpui::FollowMode::Normal);
        list.scroll_to(gpui::ListOffset {
            item_ix: 0,
            offset_in_item: px(0.),
        });
        draw_settled(cx);
        let resting = list.viewport_bounds();
        // One trackpad packet per displayed frame.
        let packet = |phase, dy: f32, cx: &mut gpui::VisualTestContext| {
            cx.simulate_event(gpui::ScrollEvent {
                position: resting.center(),
                delta: gpui::ScrollDelta::Pixels(point(px(0.), px(dy))),
                touch_phase: phase,
                ..Default::default()
            });
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            cx.update(|window, cx| window.simulate_next_frame(cx));
        };
        packet(gpui::TouchPhase::Started, 0., cx);
        for _ in 0..6 {
            packet(gpui::TouchPhase::Moved, -70., cx);
        }
        packet(gpui::TouchPhase::Ended, 0., cx);
        let flicked = list.scroll_px_offset_for_scrollbar().y;
        assert!(flicked < px(0.) && flicked > -list.max_offset_for_scrollbar().y);
        packet(gpui::TouchPhase::Started, 0., cx);
        for _ in 0..4 {
            packet(gpui::TouchPhase::Moved, -50., cx);
        }
        assert_eq!(
            list.scroll_px_offset_for_scrollbar().y,
            flicked - px(200.),
            "the drag scrolls the conversation"
        );
        assert_eq!(list.viewport_bounds(), resting, "nothing is displaced");
    }

    #[gpui::test]
    fn timeline_tail_has_one_composer_gap(cx: &mut TestAppContext) {
        use gpui::{Context, IntoElement, Render, Window, px};
        struct ChatRoot(Entity<ChatView>);
        impl Render for ChatRoot {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.0.clone()
            }
        }

        for (mobile, running) in [(false, false), (false, true), (true, false), (true, true)] {
            for (width, height) in [(393., 852.), (1024., 768.)] {
                cx.update(|cx| crate::window_seam::override_mobile_for_test(cx, mobile));
                let expected_gap = if mobile {
                    0.
                } else {
                    super::TIMELINE_BOTTOM_INSET
                };
                let mut timeline = synthetic_markdown_timeline(30);
                timeline.turn_running = running;
                timeline.turns.last_mut().unwrap().running = running;
                let (store, window_state, session_id) = seed_chat(cx, timeline);
                cx.update(|cx| cx.set_reduce_motion(true));
                let (view, cx) = cx.add_window_view(|window, cx| {
                    ChatRoot(cx.new(|cx| {
                        ChatView::new(store, window_state.clone(), Some(session_id), window, cx)
                    }))
                });
                cx.simulate_resize(gpui::size(px(width), px(height)));
                // The shell's layout rule, applied by hand since no shell is
                // mounted here: only the mobile phone is compact.
                cx.update(|window, cx| {
                    let compact = crate::window_seam::window_is_compact(window, cx);
                    window_state.update(cx, |state, _| state.compact = compact);
                });
                let list = view.read_with(cx, |view, cx| view.0.read(cx).list_state.clone());
                list.scroll_to_end();
                cx.run_until_parked();
                draw_settled(cx);
                let last = cx
                    .debug_bounds("timeline-row-89")
                    .expect("last timeline row");
                assert!(
                    f32::from(last.bottom() - list.viewport_bounds().bottom()).abs() <= 1.,
                    "the gap must stay outside the list's viewport"
                );
                let composer = cx.debug_bounds("chat-composer").expect("composer");
                let gap = f32::from(composer.top() - last.bottom());
                assert!(
                    (gap - expected_gap).abs() <= 1.,
                    "{width}×{height}, running={running}, mobile={mobile}: expected {expected_gap}px gap, got {gap}px"
                );
                let card = cx
                    .debug_bounds("composer-card")
                    .expect("visible composer card");
                assert_eq!(card.top(), composer.top(), "no extra composer top inset");
                if running {
                    let status = cx
                        .debug_bounds("working-status-29")
                        .expect("running status");
                    // The row's own edge padding sits below the status.
                    assert_eq!(
                        status.bottom(),
                        last.bottom() - px(super::TIMELINE_EDGE_PADDING)
                    );
                }
            }
        }
    }

    #[gpui::test]
    fn wheel_event_between_a_prepend_and_its_frame_keeps_the_reading_position(
        cx: &mut TestAppContext,
    ) {
        use gpui::{Context, IntoElement, Render, VisualTestContext, Window, point, px};

        struct TouchChat(Entity<ChatView>);
        impl Render for TouchChat {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.0.clone()
            }
        }
        let full = synthetic_markdown_timeline(120);
        let mut tail = full.clone();
        tail.turns.drain(..20);
        tail.entries.retain(|entry| entry.turn >= 20);
        for entry in &mut tail.entries {
            Arc::make_mut(entry).turn -= 20;
        }
        let (store, window_state, session_id) = seed_chat(cx, tail);
        let (root, cx) = cx.add_window_view(|window, cx| {
            TouchChat(cx.new(|cx| {
                ChatView::new(
                    store.clone(),
                    window_state,
                    Some(session_id.clone()),
                    window,
                    cx,
                )
            }))
        });
        let view = root.read_with(cx, |root, _| root.0.clone());
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        draw_settled(cx);
        let list = view.read_with(cx, |chat, _| chat.list_state.clone());
        let height = list.viewport_bounds().size.height;
        let scroll = |distance, cx: &mut VisualTestContext| {
            cx.simulate_event(gpui::ScrollEvent {
                position: list.viewport_bounds().center(),
                delta: gpui::ScrollDelta::Pixels(point(px(0.), distance)),
                touch_phase: gpui::TouchPhase::Moved,
                ..Default::default()
            });
        };
        for _ in 0..12 {
            scroll(height / 4., cx);
            draw_settled(cx);
        }
        assert!(!list.is_following_tail());
        let before = list.logical_scroll_top();

        // An earlier history page arrives from the host between two frames,
        // while a trackpad pan is still delivering packets. The test app
        // redraws dirty windows whenever effects flush, so the store update,
        // its observers and the packet share one flush: the deferred packet
        // runs after the observers and before the frame, as on a display link.
        let wheel = gpui::ScrollEvent {
            position: list.viewport_bounds().center(),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(40.))),
            touch_phase: gpui::TouchPhase::Moved,
            ..Default::default()
        };
        cx.update(|window, cx| {
            store.update(cx, |store, cx| {
                store.set_session_replica_for_test(session_id, full, cx);
                cx.notify();
            });
            window.defer(cx, move |window, cx| {
                window.dispatch_event(gpui::InputEvent::to_platform_input(wheel), cx);
            });
        });
        draw_settled(cx);

        // The row the reader was on sits 60 rows lower and exactly 40px
        // further down the viewport: both the page and the pan applied.
        let row_top = list
            .bounds_for_item(before.item_ix + 60)
            .expect("the reader's row stays on screen")
            .top();
        assert_eq!(
            row_top - list.viewport_bounds().top(),
            px(40.) - before.offset_in_item,
            "a 40px pan and a 60-row page both apply (offset {:?} -> {:?})",
            before.offset_in_item,
            list.logical_scroll_top().offset_in_item
        );
    }

    #[gpui::test]
    fn late_updates_preserve_user_scroll_intent(cx: &mut TestAppContext) {
        use gpui::{Context, IntoElement, Render, Window, px};
        struct TouchChat(Entity<ChatView>);
        impl Render for TouchChat {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.0.clone()
            }
        }
        for phase in [gpui::TouchPhase::Started, gpui::TouchPhase::Moved] {
            let mut timeline = synthetic_markdown_timeline(30);
            timeline.mark_idle();
            let (store, window_state, session_id) = seed_chat(cx, timeline.clone());
            let (root, cx) = cx.add_window_view(|window, cx| {
                TouchChat(cx.new(|cx| {
                    ChatView::new(
                        store.clone(),
                        window_state,
                        Some(session_id.clone()),
                        window,
                        cx,
                    )
                }))
            });
            cx.simulate_resize(gpui::size(px(393.), px(852.)));
            draw_settled(cx);
            let list = root.read_with(cx, |root, cx| root.0.read(cx).list_state.clone());
            let bottom = list.scroll_px_offset_for_scrollbar().y;
            let scroll = |delta, cx: &mut gpui::VisualTestContext| {
                cx.simulate_event(gpui::ScrollEvent {
                    position: list.viewport_bounds().center(),
                    delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), delta)),
                    touch_phase: phase,
                    ..Default::default()
                });
            };
            scroll(px(120.), cx);
            draw_settled(cx);
            assert_eq!(list.scroll_px_offset_for_scrollbar().y, bottom + px(120.));
            assert!(!list.is_following_tail());
            assert!(cx.debug_bounds("scroll-to-end").is_none());
            let anchor = list.logical_scroll_top();
            for update in 1..=30 {
                cx.executor().advance_clock(Duration::from_millis(100));
                Arc::make_mut(timeline.entries.last_mut().unwrap()).content =
                    assistant(&"Late output frame.\n\n".repeat(update));
                store.update(cx, |store, cx| {
                    store.set_session_replica_for_test(session_id.clone(), timeline.clone(), cx);
                    cx.notify();
                });
                draw_settled(cx);
                assert!(!list.is_following_tail(), "late update {update}, {phase:?}");
                assert_eq!(list.logical_scroll_top().item_ix, anchor.item_ix);
                assert_eq!(
                    list.logical_scroll_top().offset_in_item,
                    anchor.offset_in_item
                );
            }
            scroll(px(-100000.), cx);
            draw_settled(cx);
            assert!(
                list.is_following_tail(),
                "return to bottom {phase:?}: offset {:?}, max {:?}, anchor {:?}",
                list.scroll_px_offset_for_scrollbar(),
                list.max_offset_for_scrollbar(),
                list.logical_scroll_top()
            );
            store.update(cx, |store, cx| {
                store.set_session_replica_for_test(session_id, synthetic_markdown_timeline(31), cx);
                cx.notify();
            });
            draw_settled(cx);
            assert!(list.is_following_tail());
            assert_eq!(list.logical_scroll_top().item_ix, list.item_count());
        }
    }

    /// A reader following the tail stays pinned to the bottom while the last
    /// message grows in place: each delta the host streams into it lands
    /// with the message's end on the timeline's bottom edge.
    #[gpui::test]
    fn a_following_reader_stays_pinned_while_the_last_message_grows(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px, size};

        let (host, session_id, _) = seeded_chat_host(cx, Timeline::default(), false);
        // The streaming turn's working indicator rests.
        cx.update(|cx| cx.set_reduce_motion(true));
        let ts = std::cell::Cell::new(0);
        let record = |events: Vec<agent::AgentEvent>| {
            let session_id = session_id.clone();
            let from = ts.replace(ts.get() + events.len() as u64);
            smol::block_on(host.update_state_for_test(move |state, cx| {
                for (offset, event) in events.iter().enumerate() {
                    state.record_event_for_replica_test(
                        &session_id,
                        from + offset as u64,
                        event,
                        cx,
                    );
                }
            }))
            .expect("record on the host");
        };
        let user = |turn: usize| {
            agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                id: format!("user-{turn}"),
                parent_item_id: None,
                content: ItemContent::UserMessage {
                    text: format!("Question {turn}"),
                    context_len: None,
                    attachments: Vec::new(),
                },
            })
        };
        let seeded_id = session_id.clone();
        smol::block_on(host.update_state_for_test(move |state, _| {
            state.residents.live.get_mut(&seeded_id).unwrap().timeline = Timeline::default();
        }))
        .expect("clear the seeded timeline");
        // Earlier turns fill more than a screen above the streaming one.
        let mut events = Vec::new();
        for turn in 0..4 {
            events.extend([
                agent::AgentEvent::TurnStarted {
                    turn_id: turn.to_string(),
                },
                user(turn),
                agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                    id: format!("answer-{turn}"),
                    parent_item_id: None,
                    content: ItemContent::AssistantMessage {
                        text: "An earlier answer.\n\n".repeat(8),
                        summarized: false,
                    },
                }),
                agent::AgentEvent::TurnCompleted {
                    turn_id: turn.to_string(),
                    status: agent::TurnStatus::Completed,
                    usage: None,
                },
            ]);
        }
        events.extend([
            agent::AgentEvent::TurnStarted {
                turn_id: "4".into(),
            },
            user(4),
            agent::AgentEvent::ItemStarted(agent::ThreadItem {
                id: "streaming".into(),
                parent_item_id: None,
                content: ItemContent::AssistantMessage {
                    text: String::new(),
                    summarized: false,
                },
            }),
        ]);
        record(events);

        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        driver.wait_until(cx, &store, "workspace baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });
        store.update(cx, |store, cx| store.show_alone(session_id.clone(), cx));
        driver.wait_until(cx, &store, "chat baseline", |cx| {
            store.read_with(cx, |store, _| {
                !store.chat_loading_for(store.sole_thread().as_deref())
            })
        });
        let window_state = cx.new(|_| WindowState::new(false));
        let (view, cx) = cx.add_window_view(|window, cx| {
            ChatView::new(
                store.clone(),
                window_state,
                Some(session_id.clone()),
                window,
                cx,
            )
        });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(700.)));
        draw_settled(cx);
        let list = view.read_with(cx, |chat, _| chat.list_state.clone());
        assert!(list.is_following_tail());
        assert!(
            list.max_offset_for_scrollbar().y > list.viewport_bounds().size.height,
            "the thread scrolls"
        );

        let streamed = |cx: &mut VisualTestContext| {
            store.read_with(cx, |store, _| {
                store
                    .with_timeline_for(store.sole_thread().as_deref(), |timeline| {
                        timeline
                            .entries
                            .last()
                            .and_then(|entry| match &entry.content {
                                EntryContent::Item(ItemContent::AssistantMessage {
                                    text, ..
                                }) => Some(text.len()),
                                _ => None,
                            })
                    })
                    .flatten()
                    .unwrap_or(0)
            })
        };
        let mut heights = Vec::new();
        for delta in 0..12 {
            let before = streamed(cx);
            record(vec![agent::AgentEvent::Delta {
                item_id: "streaming".into(),
                kind: agent::DeltaKind::AssistantText,
                text: format!("Paragraph {delta} of the streamed answer.\n\n"),
            }]);
            let deadline = Instant::now() + Duration::from_secs(5);
            while streamed(cx) == before {
                assert!(Instant::now() < deadline, "waiting for delta {delta}");
                driver.step(cx, &store);
                std::thread::sleep(Duration::from_millis(1));
            }
            draw_settled(cx);

            assert!(list.is_following_tail(), "delta {delta} paused following");
            let last = cx
                .debug_bounds(format!("timeline-row-{}", list.item_count() - 1).leak())
                .expect("the streaming message is on screen");
            let bottom = list.viewport_bounds().bottom();
            assert!(
                (last.bottom() - bottom).abs() <= px(1.),
                "delta {delta}: the message ends at {:?}, the timeline at {bottom:?}",
                last.bottom()
            );
            heights.push(last.size.height);
        }
        assert!(
            heights.windows(2).all(|pair| pair[1] > pair[0]),
            "the message grew with every delta: {heights:?}"
        );
        driver.shutdown(cx, &store, &host);
    }

    #[gpui::test]
    fn scroll_pill_over_inline_image_does_not_open_lightbox(cx: &mut TestAppContext) {
        use gpui::{Modifiers, MouseButton, ScrollDelta, ScrollEvent, TouchPhase, point, px};

        let path = std::env::temp_dir().join(format!(
            "tcode-pill-image-{}-{}.png",
            std::process::id(),
            NEXT_RESIDENCY_TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        image::RgbaImage::from_pixel(400, 1000, image::Rgba([40, 80, 120, 255]))
            .save(&path)
            .unwrap();
        let mut timeline = synthetic_markdown_timeline(30);
        Arc::make_mut(timeline.entries.last_mut().unwrap()).content = assistant(&format!(
            "![shot]({})\n\n{}",
            path.display(),
            "Text after the screenshot.\n\n".repeat(80)
        ));
        let (host, session_id, timeline) = seeded_chat_host(cx, timeline, false);
        let seeded_id = session_id.clone();
        smol::block_on(host.update_state_for_test(move |state, cx| {
            state.residents.live.get_mut(&seeded_id).unwrap().timeline = Timeline::default();
            let mut ts = 0;
            for turn in 0..timeline.turns.len() {
                let mut events = vec![agent::AgentEvent::TurnStarted {
                    turn_id: turn.to_string(),
                }];
                events.extend(
                    timeline
                        .entries
                        .iter()
                        .filter(|entry| entry.turn == turn)
                        .map(|entry| {
                            let EntryContent::Item(content) = &entry.content else {
                                panic!("image fixture contains provider items");
                            };
                            agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                                id: entry.id.clone(),
                                parent_item_id: None,
                                content: content.clone(),
                            })
                        }),
                );
                events.push(agent::AgentEvent::TurnCompleted {
                    turn_id: turn.to_string(),
                    status: agent::TurnStatus::Completed,
                    usage: None,
                });
                for event in events {
                    state.record_event_for_replica_test(&seeded_id, ts, &event, cx);
                    ts += 1;
                }
            }
        }))
        .expect("record inline image conversation");
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        store.update(cx, |store, cx| store.show_alone(session_id.clone(), cx));
        driver.wait_until(cx, &store, "chat baseline", |cx| {
            store.read_with(cx, |store, _| {
                store.baseline_ready() && !store.chat_loading_for(store.sole_thread().as_deref())
            })
        });
        let window_state = cx.new(|_| WindowState::new(false));
        struct LoadedImageChat {
            chat: Entity<ChatView>,
            sources: Vec<gpui::ImageSource>,
            ready: std::rc::Rc<std::cell::Cell<bool>>,
        }
        impl gpui::Render for LoadedImageChat {
            fn render(
                &mut self,
                window: &mut gpui::Window,
                cx: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                let mut loaded = true;
                for source in &self.sources {
                    let gpui::ImageSource::Custom(load) = source else {
                        unreachable!("host images load through the query plane");
                    };
                    match load(window, cx) {
                        Some(Ok(_)) => {}
                        Some(Err(error)) => panic!("inline host image load failed: {error:?}"),
                        None => loaded = false,
                    }
                }
                self.ready.set(loaded);
                self.chat.clone()
            }
        }
        let ready = std::rc::Rc::new(std::cell::Cell::new(false));
        let image_sources = vec![crate::store::image_preview(
            path.clone(),
            crate::store::MAX_PREVIEW_PIXELS as f32,
            false,
        )];
        let (root, cx) = cx.add_window_view(|window, cx| {
            let chat = cx.new(|cx| {
                ChatView::new(
                    store.clone(),
                    window_state,
                    Some(session_id.clone()),
                    window,
                    cx,
                )
            });
            let surface = cx.new(|_| LoadedImageChat {
                chat,
                sources: image_sources,
                ready: ready.clone(),
            });
            gpui_base::Root::new(surface, window, cx)
        });
        let view = root.read_with(cx, |root, cx| {
            root.view()
                .clone()
                .downcast::<LoadedImageChat>()
                .unwrap()
                .read(cx)
                .chat
                .clone()
        });
        cx.simulate_resize(gpui::size(px(393.), px(600.)));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            driver.step(cx, &store);
            draw_settled(cx);
            if ready.get() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "inline host image finishes loading"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let list = view.read_with(cx, |chat, _| chat.list_state.clone());
        let row = cx.debug_bounds("timeline-row-89").unwrap_or_else(|| {
            panic!(
                "last assistant row: selected={:?}, list_items={}",
                store.read_with(cx, |store, _| store.sole_thread()),
                list.item_count()
            )
        });
        cx.simulate_event(ScrollEvent {
            position: list.viewport_bounds().center(),
            delta: ScrollDelta::Pixels(point(px(0.), list.viewport_bounds().top() - row.top())),
            touch_phase: TouchPhase::Moved,
            ..Default::default()
        });
        draw_settled(cx);
        let pill = cx
            .debug_bounds("scroll-to-end")
            .expect("pill above the image");
        let click = |position, cx: &mut gpui::VisualTestContext| {
            cx.simulate_mouse_move(position, None, Modifiers::default());
            cx.simulate_mouse_down(position, MouseButton::Left, Modifiers::default());
            cx.simulate_mouse_up(position, MouseButton::Left, Modifiers::default());
            draw_settled(cx);
        };
        click(point(pill.center().x, pill.top() - px(40.)), cx);
        let deadline = Instant::now() + Duration::from_secs(5);
        while cx.debug_bounds("image-viewer-image").is_none() {
            driver.step(cx, &store);
            draw_settled(cx);
            assert!(
                Instant::now() < deadline,
                "exposed image finishes its explicit read"
            );
            std::thread::yield_now();
        }
        assert!(
            cx.debug_bounds("image-viewer-image").is_some(),
            "exposed image opens the lightbox"
        );
        cx.simulate_keystrokes("escape");
        draw_settled(cx);
        assert!(cx.debug_bounds("image-viewer-image").is_none());
        assert!(!list.is_following_tail());

        let pill = cx.debug_bounds("scroll-to-end").unwrap();
        click(pill.center(), cx);
        assert!(list.is_following_tail(), "pill click must reach the tail");
        assert!(
            cx.debug_bounds("image-viewer-image").is_none(),
            "pill click opened the image underneath"
        );
        driver.shutdown(cx, &store, &host);
        std::fs::remove_file(path).unwrap();
    }

    /// The jump to the latest glides the timeline there: the conversation
    /// moves toward the end over several frames, one way, and lands
    /// following the tail exactly; the reader's own scroll part way stops it
    /// where they took it; with motion reduced it is at the end at once.
    #[gpui::test]
    fn the_jump_to_latest_glides_there_and_the_reader_can_take_over(cx: &mut TestAppContext) {
        use gpui::{point, px};
        for reduce in [false, true] {
            let (store, window_state, session_id) = seed_chat(cx, synthetic_markdown_timeline(30));
            cx.update(|cx| cx.set_reduce_motion(reduce));
            let (view, cx) = cx.add_window_view(|window, cx| {
                ChatView::new(store, window_state, Some(session_id), window, cx)
            });
            cx.simulate_resize(gpui::size(px(800.), px(700.)));
            draw_settled(cx);
            let list = view.read_with(cx, |chat, _| chat.list_state.clone());
            let height = list.viewport_bounds().size.height;
            let wheel = |distance: gpui::Pixels, cx: &mut gpui::VisualTestContext| {
                // The pointer is over the conversation it scrolls, in a
                // gesture of its own, as a trackpad sends it.
                cx.simulate_mouse_move(
                    list.viewport_bounds().center(),
                    None,
                    gpui::Modifiers::default(),
                );
                for (phase, delta) in [
                    (gpui::TouchPhase::Started, px(0.)),
                    (gpui::TouchPhase::Moved, distance),
                ] {
                    cx.simulate_event(gpui::ScrollEvent {
                        position: list.viewport_bounds().center(),
                        delta: gpui::ScrollDelta::Pixels(point(px(0.), delta)),
                        touch_phase: phase,
                        ..Default::default()
                    });
                }
            };
            let offset = || -list.scroll_px_offset_for_scrollbar().y;
            let frame = |cx: &mut gpui::VisualTestContext| {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.update(|window, cx| window.simulate_next_frame(cx));
                draw(cx);
            };
            for interrupt in [false, true] {
                wheel(height * 1.5, cx);
                draw_settled(cx);
                assert!(!list.is_following_tail());
                let away = offset();
                let pill = cx.debug_bounds("scroll-to-end").expect("the jump");
                cx.simulate_click(pill.center(), gpui::Modifiers::default());
                draw(cx);
                if reduce {
                    draw_settled(cx);
                    assert!(list.is_following_tail(), "reduced motion jumps at once");
                    break;
                }
                let mut seen = vec![away, offset()];
                for _ in 0..(if interrupt { 4 } else { 20 }) {
                    frame(cx);
                    seen.push(offset());
                }
                assert!(
                    seen.windows(2).all(|pair| pair[1] >= pair[0]),
                    "the glide went back: {seen:?}"
                );
                assert!(
                    seen.windows(2).all(|pair| pair[1] - pair[0] < height * 0.6),
                    "the glide jumped: {seen:?}"
                );
                if interrupt {
                    let taken = offset();
                    assert!(taken > away && !list.is_following_tail(), "{seen:?}");
                    wheel(px(120.), cx);
                    draw_settled(cx);
                    assert!(!list.is_following_tail(), "the glide outlived the wheel");
                    assert!(
                        (offset() - (taken - px(120.))).abs() <= px(1.),
                        "the glide moved the list after the reader took it: {taken:?} -> {:?}",
                        offset()
                    );
                    // Back to the end for the next case.
                    list.set_follow_mode(gpui::FollowMode::Tail);
                    list.scroll_to_end();
                    draw_settled(cx);
                } else {
                    assert!(
                        seen.windows(2).filter(|pair| pair[1] > pair[0]).count() >= 6,
                        "the jump moved in one step: {seen:?}"
                    );
                    draw_settled(cx);
                    assert!(list.is_following_tail(), "the glide ends on the tail");
                    assert!(cx.debug_bounds("scroll-to-end").is_none());
                    assert!(
                        (offset() - list.max_offset_for_scrollbar().y).abs() <= px(1.),
                        "the glide ended short of the end"
                    );
                }
            }
        }
    }

    #[gpui::test]
    fn jump_to_latest_survives_unmeasured_history(cx: &mut TestAppContext) {
        use gpui::{Context, IntoElement, Render, VisualTestContext, Window, point, px};

        struct TouchChat(Entity<ChatView>);
        impl Render for TouchChat {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.0.clone()
            }
        }
        let full = synthetic_markdown_timeline(120);
        let mut tail = full.clone();
        tail.turns.drain(..20);
        tail.entries.retain(|entry| entry.turn >= 20);
        for entry in &mut tail.entries {
            Arc::make_mut(entry).turn -= 20;
        }
        assert_eq!(tail.entries.len(), 300);
        let (store, window_state, session_id) = seed_chat(cx, tail);
        let (root, cx) = cx.add_window_view(|window, cx| {
            TouchChat(cx.new(|cx| {
                ChatView::new(
                    store.clone(),
                    window_state,
                    Some(session_id.clone()),
                    window,
                    cx,
                )
            }))
        });
        let view = root.read_with(cx, |root, _| root.0.clone());
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        draw_settled(cx);
        let list = view.read_with(cx, |chat, _| chat.list_state.clone());
        let height = list.viewport_bounds().size.height;
        assert!(height > px(0.));
        assert!(cx.debug_bounds("scroll-to-end").is_none());
        let scroll = |distance, phase, cx: &mut VisualTestContext| {
            cx.simulate_event(gpui::ScrollEvent {
                position: list.viewport_bounds().center(),
                delta: gpui::ScrollDelta::Pixels(point(px(0.), distance)),
                touch_phase: phase,
                ..Default::default()
            });
            draw_settled(cx);
        };
        // Small wheel deltas must accumulate, rather than snapping back while
        // the viewport is still inside the one-screen follow threshold.
        for _ in 0..12 {
            scroll(height / 4., gpui::TouchPhase::Moved, cx);
        }
        assert!(
            cx.debug_bounds("scroll-to-end").is_some(),
            "jump to latest must appear three screens above the tail (position {:?}, end {:?})",
            list.logical_scroll_top(),
            list.is_scrolled_to_end()
        );
        assert!(!list.is_following_tail());
        // Reach a nonzero intra-row offset through the same real wheel path;
        // a row-boundary anchor would miss prepend regressions that reset it.
        scroll(
            list.logical_scroll_top().offset_in_item - px(7.),
            gpui::TouchPhase::Moved,
            cx,
        );
        let before = list.logical_scroll_top();
        assert!(before.item_ix > 0);
        assert_eq!(before.offset_in_item, px(7.));
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id.clone(), full, cx);
            cx.notify();
        });
        draw_settled(cx);
        assert!(
            cx.debug_bounds("scroll-to-end").is_some(),
            "prepend hides jump"
        );
        assert_eq!(list.logical_scroll_top().item_ix, before.item_ix + 60);
        assert_eq!(
            list.logical_scroll_top().offset_in_item,
            before.offset_in_item
        );

        let button = cx.debug_bounds("scroll-to-end").unwrap();
        cx.simulate_click(button.center(), gpui::Modifiers::default());
        draw_settled(cx);
        assert!(cx.debug_bounds("scroll-to-end").is_none());
        assert!(list.is_following_tail());
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(
                session_id.clone(),
                synthetic_markdown_timeline(121),
                cx,
            );
            cx.notify();
        });
        draw_settled(cx);
        assert!(cx.debug_bounds("scroll-to-end").is_none());
        assert!(list.is_following_tail());
        assert_eq!(
            list.logical_scroll_top().item_ix,
            list.item_count(),
            "new turn did not move the tail anchor"
        );

        // A touch pan reaches the list's own wheel handler. No store
        // notification drives this UI.
        scroll(height * 3., gpui::TouchPhase::Started, cx);
        assert!(
            cx.debug_bounds("scroll-to-end").is_some(),
            "a touch pan hides jump"
        );
        assert!(!list.is_following_tail());
        let before = list.logical_scroll_top();
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(
                session_id.clone(),
                synthetic_markdown_timeline(122),
                cx,
            );
            cx.notify();
        });
        draw_settled(cx);
        assert!(cx.debug_bounds("scroll-to-end").is_some());
        assert_eq!(
            list.logical_scroll_top().item_ix,
            before.item_ix,
            "new turn steals reading position"
        );
        assert_eq!(
            list.logical_scroll_top().offset_in_item,
            before.offset_in_item
        );
        assert!(!list.is_following_tail());

        let mut streaming = synthetic_markdown_timeline(122);
        let tail = Arc::make_mut(streaming.entries.last_mut().unwrap());
        tail.content = assistant(&"Streaming paragraph.\n\n".repeat(20));
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id.clone(), streaming, cx);
            cx.notify();
        });
        draw_settled(cx);
        assert!(cx.debug_bounds("scroll-to-end").is_some());
        assert_eq!(list.logical_scroll_top().item_ix, before.item_ix);
        assert_eq!(
            list.logical_scroll_top().offset_in_item,
            before.offset_in_item
        );
        assert!(
            !list.is_following_tail(),
            "streaming steals reading position"
        );

        scroll(-height * 100., gpui::TouchPhase::Moved, cx);
        assert!(cx.debug_bounds("scroll-to-end").is_none());
        // Reaching the bottom resumes following, independently of pill visibility.
        assert!(list.is_following_tail());
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id, synthetic_markdown_timeline(123), cx);
            cx.notify();
        });
        draw_settled(cx);
        assert!(list.is_following_tail());
        assert!(cx.debug_bounds("scroll-to-end").is_none());
    }

    /// A scrolling screenshot moves the timeline tile by tile from wherever it
    /// was, learns how far it really moved once layout has settled, and leaves
    /// it where it was.
    #[gpui::test]
    fn scrolling_capture_moves_the_timeline_within_its_content_and_restores_it(
        cx: &mut TestAppContext,
    ) {
        use gpui::{FollowMode, ListOffset, px};
        let (store, window_state, session_id) = seed_chat(cx, synthetic_markdown_timeline(60));
        let (view, cx) = cx.add_window_view(|window, cx| {
            ChatView::new(store, window_state, Some(session_id), window, cx)
        });
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        let draw = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
        };
        // The rows on screen are built and measured before a capture starts.
        for _ in 0..4 {
            draw(cx);
            cx.run_until_parked();
        }
        assert!(view.read_with(cx, |chat, _| chat.capture_settled()));
        let list = view.read_with(cx, |chat, _| chat.list_state.clone());
        assert!(list.is_following_tail());
        assert!(
            super::capture_scrolled(&list) > px(852.),
            "sixty turns scroll well past one screen"
        );
        // Rows measured for the first time move the list; the capturer scrolls
        // again after each frame until nothing moves.
        let settle = |offset: gpui::Pixels, cx: &mut gpui::VisualTestContext| {
            for _ in 0..8 {
                let scroll = view
                    .update(cx, |chat, cx| chat.capture_scroll(offset, cx))
                    .expect("a capture is in progress");
                if !scroll.moved {
                    return scroll.reached;
                }
                draw(cx);
            }
            panic!("the timeline kept moving");
        };

        // Following the tail: the capture starts at the bottom.
        assert!(view.update(cx, |chat, cx| chat.capture_begin(cx)));
        let viewport = view.read_with(cx, |chat, _| chat.capture_viewport());
        assert!(viewport.is_some_and(|bounds| bounds.size.height > px(0.)));
        assert!(
            !list.is_following_tail(),
            "layout must not snap back to the end"
        );
        assert_eq!(
            settle(px(0.), cx),
            px(0.),
            "the first tile is the screen itself"
        );
        assert_eq!(settle(px(-400.), cx), px(-400.));
        assert!(
            list.logical_scroll_top().item_ix < 176,
            "a screen above the last rows"
        );
        assert_eq!(
            settle(px(400.), cx),
            px(0.),
            "nothing lies below the end of the content"
        );
        let top = settle(px(-1.0e6), cx);
        assert!(
            top < px(-852.) && top > px(-1.0e6),
            "the tile above the first turn stops at the top: {top:?}"
        );
        assert_eq!(list.logical_scroll_top().item_ix, 0);
        view.update(cx, |chat, cx| chat.capture_end(cx));
        assert!(list.is_following_tail());
        assert_eq!(
            view.update(cx, |chat, cx| chat.capture_scroll(px(0.), cx)),
            None,
            "tiles after the end are refused"
        );

        // Reading in the middle: the anchor comes back exactly, still paused.
        list.set_follow_mode(FollowMode::Normal);
        let anchor = ListOffset {
            item_ix: 10,
            offset_in_item: px(7.),
        };
        list.scroll_to(anchor);
        view.update(cx, |_, cx| cx.notify());
        draw(cx);
        assert!(view.update(cx, |chat, cx| chat.capture_begin(cx)));
        assert_eq!(settle(px(0.), cx), px(0.));
        assert_eq!(settle(px(300.), cx), px(300.));
        assert!(list.logical_scroll_top().item_ix >= 10);
        view.update(cx, |chat, cx| chat.capture_end(cx));
        let restored = list.logical_scroll_top();
        assert_eq!(restored.item_ix, anchor.item_ix);
        assert_eq!(restored.offset_in_item, anchor.offset_in_item);
        assert!(!list.is_following_tail());
    }

    /// A long streamed conversation opens with one partial turn: the snapshot
    /// holds only its last records. The first page completes that turn and
    /// adds an earlier one; the reader's position must survive rather than
    /// resetting to the tail.
    #[gpui::test]
    fn completing_the_only_partial_turn_keeps_the_reading_anchor(cx: &mut TestAppContext) {
        use gpui::{FollowMode, ListOffset, px};
        let mut full = synthetic_markdown_timeline(2);
        // Tall enough that the reader can scroll into the loaded content.
        let long_answer = "A paragraph of the answer.\n\n".repeat(80);
        full.entries.pop();
        full.entries
            .push(at_turn(entry("assistant-1", assistant(&long_answer)), 1));
        let mut tail = Timeline::default();
        tail.turns = vec![TurnMeta::default()];
        tail.entries = vec![entry("assistant-1", assistant(&long_answer))];
        let (store, window_state, session_id, _driver) = seed_chat_with_history(cx, tail, true);
        cx.update(|cx| cx.set_reduce_motion(true));
        let (view, cx) = cx.add_window_view(|window, cx| {
            ChatView::new(
                store.clone(),
                window_state,
                Some(session_id.clone()),
                window,
                cx,
            )
        });
        cx.simulate_resize(gpui::size(px(1024.), px(700.)));
        for _ in 0..2 {
            view.update(cx, |_, cx| cx.notify());
            draw_settled(cx);
        }
        let (list, placeholder, leading) = view.read_with(cx, |chat, _| {
            (
                chat.list_state.clone(),
                chat.history_placeholder_height,
                chat.leading_space(),
            )
        });
        assert!(placeholder > px(0.), "earlier history is still unloaded");
        list.set_follow_mode(FollowMode::Normal);
        list.scroll_to(ListOffset {
            item_ix: 0,
            offset_in_item: leading + px(20.),
        });
        view.update(cx, |_, cx| cx.notify());
        draw_settled(cx);
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id, full, cx);
            cx.notify();
        });
        draw_settled(cx);
        let anchor = list.logical_scroll_top();
        // The page adds the earlier turn's three rows and the user bubble
        // above the answer.
        assert_eq!(
            (anchor.item_ix, anchor.offset_in_item),
            (5, px(20.)),
            "the completed turn keeps its reader at the same content offset"
        );
        assert!(
            !list.is_following_tail(),
            "the page must not reset the list to its tail"
        );
    }

    /// A reader who scrolls far up a long thread and comes back sees rows at
    /// every step. Once they have stayed at the tail, the pages fetched above
    /// it are released without moving the tail: the conversation is again the
    /// one the tail needed, nothing is fetched back while they stay, and the
    /// next scroll up asks for the page above that window.
    #[gpui::test]
    fn pages_read_far_up_are_released_once_the_reader_stays_at_the_tail(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, point, px};
        use tcode_protocol::{ClientPayload, EventEnvelope, HostMessage, ServerEvent, Topic};

        // A 400-turn log, four records a turn; the window holds the last ten.
        let events = (0..400)
            .flat_map(|turn| {
                [
                    agent::AgentEvent::TurnStarted {
                        turn_id: turn.to_string(),
                    },
                    user_event(&format!("user-{turn}"), &format!("Question {turn}")),
                    answer_event(
                        &format!("answer-{turn}"),
                        &format!("Answer {turn}.\n\n").repeat(3),
                    ),
                    agent::AgentEvent::TurnCompleted {
                        turn_id: turn.to_string(),
                        status: agent::TurnStatus::Completed,
                        usage: None,
                    },
                ]
            })
            .collect();
        let mut thread = ScriptedThread::new(cx, events, 4 * 390, false);
        let store = thread.store.clone();
        let (view, cx) = thread.open_at(gpui::size(px(393.), px(852.)), cx);

        // Present a frame, as the display link does, and answer the page it
        // asked for, ten turns above `before`; then let the store's single
        // request gate reopen.
        let frame = |thread: &mut ScriptedThread, cx: &mut VisualTestContext| -> Option<u64> {
            store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            draw(cx);
            cx.update(|window, cx| {
                window.simulate_next_frame(cx);
            });
            cx.run_until_parked();
            let before = thread.answer_page(40)?;
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(250));
            Some(before)
        };
        let settle = |thread: &mut ScriptedThread, cx: &mut VisualTestContext| -> Vec<u64> {
            let pages: Vec<u64> = std::iter::from_fn(|| frame(thread, cx)).take(40).collect();
            assert!(frame(thread, cx).is_none(), "the prefetch settles");
            pages
        };
        let replica = |cx: &mut VisualTestContext| {
            store.read_with(cx, |store, _| {
                store.with_timeline_for(store.sole_thread().as_deref(), Timeline::clone)
            })
        };
        let list = view.read_with(cx, |chat, _| chat.list_state.clone());
        // One trackpad gesture.
        let scroll = |distance: gpui::Pixels, cx: &mut VisualTestContext| {
            let position = list.viewport_bounds().center();
            for (touch_phase, distance) in [
                (gpui::TouchPhase::Started, px(0.)),
                (gpui::TouchPhase::Moved, distance),
                (gpui::TouchPhase::Ended, px(0.)),
            ] {
                cx.simulate_event(gpui::ScrollEvent {
                    position,
                    delta: gpui::ScrollDelta::Pixels(point(px(0.), distance)),
                    touch_phase,
                    ..Default::default()
                });
            }
        };

        let tail_pages = settle(&mut thread, cx);
        assert!(list.is_following_tail());
        assert!(!tail_pages.is_empty(), "ten turns do not fill six screens");
        let tail = replica(cx).expect("the tail window");
        let tail_from = tail_pages.last().unwrap() - 40;

        let height = list.viewport_bounds().size.height;
        let mut pages_away = 0;
        while pages_away < 6 {
            scroll(height * 3., cx);
            while frame(&mut thread, cx).is_some() {
                pages_away += 1;
                view.read_with(cx, |chat, cx| {
                    assert!(
                        !chat
                            .workspace_store
                            .read(cx)
                            .chat_loading_for(chat.session_id.as_deref())
                    );
                    assert!(!chat.rows.is_empty(), "the conversation stays on screen");
                });
            }
            assert!(!list.is_following_tail());
        }
        let read = replica(cx).unwrap();
        assert!(read.turns.len() > tail.turns.len());

        let back = |thread: &mut ScriptedThread, cx: &mut VisualTestContext| {
            // The pill shows once a frame has seen the reader away.
            draw(cx);
            let pill = cx.debug_bounds("scroll-to-end").expect("jump to latest");
            cx.simulate_click(pill.center(), gpui::Modifiers::none());
            // The jump glides there over its frames.
            cx.executor().advance_clock(crate::edge_back::SETTLE);
            assert!(frame(thread, cx).is_none());
            assert!(list.is_following_tail());
        };
        back(&mut thread, cx);
        cx.executor().advance_clock(Duration::from_secs(20));
        assert!(frame(&mut thread, cx).is_none());
        assert_eq!(
            replica(cx).unwrap(),
            read,
            "a glance at the tail keeps them"
        );
        scroll(height * 1.5, cx);
        assert!(frame(&mut thread, cx).is_none());
        assert!(!list.is_following_tail());
        cx.executor().advance_clock(Duration::from_secs(20));
        assert!(frame(&mut thread, cx).is_none());
        assert_eq!(replica(cx).unwrap(), read, "reading above keeps them");

        back(&mut thread, cx);
        let last_row = |cx: &mut VisualTestContext| {
            cx.debug_bounds(format!("timeline-row-{}", list.item_count() - 1).leak())
                .expect("the last row is on screen")
        };
        let rows = list.item_count();
        let bottom = last_row(cx).bottom();
        cx.executor().advance_clock(Duration::from_secs(30));
        assert!(
            frame(&mut thread, cx).is_none(),
            "the tail's own pages are still held"
        );
        assert_eq!(replica(cx).unwrap(), tail);
        assert!(list.is_following_tail());
        assert!(list.item_count() < rows);
        assert_eq!(last_row(cx).bottom(), bottom, "the tail stays where it was");
        assert!(settle(&mut thread, cx).is_empty());

        scroll(height * 3., cx);
        assert_eq!(
            frame(&mut thread, cx),
            Some(tail_from),
            "the page above the tail window"
        );

        // Left with that page held, the thread reopens from its cursor with
        // the window its tail needed.
        while thread.outgoing.try_recv().is_ok() {}
        store.update(cx, |store, cx| {
            store.show_alone("other".into(), cx);
            store.show_alone(THREAD.into(), cx);
        });
        let events_topic = Topic::SessionEvents {
            session_id: THREAD.into(),
        };
        let mut resumed_id = None;
        let resumed: Vec<_> = std::iter::from_fn(|| thread.outgoing.try_recv().ok())
            .filter_map(|line| {
                let message = tcode_protocol::decode_client_line(&line).unwrap();
                match message.payload {
                    ClientPayload::Subscribe(subscription)
                        if subscription.topic == events_topic =>
                    {
                        resumed_id = Some(message.id);
                        Some(subscription.after)
                    }
                    _ => None,
                }
            })
            .collect();
        assert_eq!(resumed, [Some(4 * 400)]);
        thread
            .incoming
            .try_send(
                tcode_protocol::encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: resumed_id,
                    topic: events_topic,
                    event: ServerEvent::SessionSnapshot {
                        from: 4 * 400,
                        end: 4 * 400,
                        records: vec![],
                        total: 4 * 400,
                        total_turns: 400,
                        truncated: false,
                    },
                }))
                .unwrap(),
            )
            .unwrap();
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        assert_eq!(replica(cx).unwrap(), tail);
    }

    #[gpui::test]
    fn pan_packet_after_a_page_lands_keeps_the_walk_back_into_it(cx: &mut TestAppContext) {
        use gpui::{FollowMode, ListOffset, point, px};
        for pan_delta in [0., 40.] {
            let full = synthetic_markdown_timeline(60);
            let mut tail = synthetic_markdown_timeline(60);
            tail.turns.drain(..20);
            tail.entries.retain(|entry| entry.turn >= 20);
            for entry in &mut tail.entries {
                std::sync::Arc::make_mut(entry).turn -= 20;
            }
            let (store, window_state, session_id, _driver) = seed_chat_with_history(cx, tail, true);
            cx.update(|cx| cx.set_reduce_motion(true));
            let (view, cx) = cx.add_window_view(|window, cx| {
                ChatView::new(
                    store.clone(),
                    window_state,
                    Some(session_id.clone()),
                    window,
                    cx,
                )
            });
            cx.simulate_resize(gpui::size(px(393.), px(852.)));
            draw_settled(cx);
            let (list, placeholder, leading) = view.read_with(cx, |chat, _| {
                (
                    chat.list_state.clone(),
                    chat.history_placeholder_height,
                    chat.leading_space(),
                )
            });
            assert!(placeholder > px(100.));
            list.set_follow_mode(FollowMode::Normal);
            list.scroll_to(ListOffset {
                item_ix: 0,
                offset_in_item: leading - px(100.),
            });
            draw_settled(cx);
            let content_top = list.bounds_for_item(0).unwrap().top() + leading;
            assert!(content_top > list.viewport_bounds().top());
            store.update(cx, |store, cx| {
                store.set_session_replica_for_test(session_id, full, cx);
                store.suppress_history_prefetch_for_test();
                cx.notify();
            });
            // The frame that lands the page measures it.
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            // The finger is still moving: a pan packet resolves against that
            // frame's anchor before the next frame walks back into the page.
            if pan_delta != 0. {
                cx.simulate_event(gpui::ScrollEvent {
                    position: list.viewport_bounds().center(),
                    delta: gpui::ScrollDelta::Pixels(point(px(0.), px(pan_delta))),
                    touch_phase: gpui::TouchPhase::Moved,
                    ..Default::default()
                });
            }
            cx.update(|window, cx| {
                window.simulate_next_frame(cx);
                let _ = window.draw(cx);
            });
            draw_settled(cx);
            let anchor = list.logical_scroll_top();
            assert!(
                anchor.item_ix < 60 && anchor.offset_in_item >= px(0.),
                "pan {pan_delta}: {anchor:?}"
            );
            let after = list
                .bounds_for_item(60)
                .expect("previous first turn remains on screen")
                .top();
            assert!(
                (after - (content_top + px(pan_delta))).abs() < px(1.),
                "the walk-back and the pan both apply: {content_top:?} + {pan_delta} -> {after:?}"
            );
            assert!(!list.is_following_tail());
        }
    }

    #[gpui::test]
    fn upward_scroll_moves_only_by_the_input_distance(cx: &mut TestAppContext) {
        use gpui::{ScrollDelta, ScrollEvent, point, px, size};

        for width in [393., 1024.] {
            let mut timeline = synthetic_markdown_timeline(3);
            for entry in &mut timeline.entries {
                if let EntryContent::Item(ItemContent::AssistantMessage { text, .. }) =
                    &mut Arc::make_mut(entry).content
                {
                    *text = (0..40)
                        .map(|n| {
                            format!(
                                "## Section {n}\n\nA paragraph with **bold** text and `code`.\n\n"
                            )
                        })
                        .collect();
                }
            }
            let (store, window_state, session_id) = seed_chat(cx, timeline);
            let (view, cx) = cx.add_window_view(|window, cx| {
                ChatView::new(store, window_state, Some(session_id), window, cx)
            });
            cx.simulate_resize(size(px(width), px(700.)));
            draw_settled(cx);
            let list = view.read_with(cx, |chat, _| chat.list_state.clone());
            list.set_offset_from_scrollbar(point(
                px(0.),
                list.scroll_px_offset_for_scrollbar().y + px(40.),
            ));
            view.update(cx, |_, cx| cx.notify());
            draw_settled(cx);
            for step in 0..160 {
                let item = list.logical_scroll_top().item_ix;
                let before = list.bounds_for_item(item).unwrap().top();
                cx.simulate_event(ScrollEvent {
                    position: list.viewport_bounds().center(),
                    delta: ScrollDelta::Pixels(point(px(0.), px(40.))),
                    touch_phase: gpui::TouchPhase::Moved,
                    ..Default::default()
                });
                draw_settled(cx);
                let after = list
                    .bounds_for_item(item)
                    .expect("reading anchor stays visible")
                    .top();
                assert!(
                    (f32::from(after - before) - 40.).abs() < 1.,
                    "step {step}: upward input of 40px moved from {before:?} to {after:?}"
                );
            }
        }
    }

    /// Unloaded history reserves a screen above the first turn from the first
    /// frame, so the timeline's extent is settled by it.
    #[gpui::test]
    fn the_history_reservation_is_settled_by_the_first_frame(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px};
        let (store, window_state, session_id, _driver) =
            seed_chat_with_history(cx, synthetic_markdown_timeline(1), true);
        // Opening the window draws its first frame.
        let (view, cx) = cx.add_window_view(|window, cx| {
            ChatView::new(store, window_state, Some(session_id), window, cx)
        });
        let cx: &mut VisualTestContext = cx;
        let list = view.read_with(cx, |chat, _| chat.list_state.clone());
        let first = list.max_offset_for_scrollbar().y;
        view.update(cx, |_, cx| cx.notify());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert_eq!(list.max_offset_for_scrollbar().y, first);
        assert!(
            first > px(0.),
            "one short turn scrolls into the reservation above it"
        );
    }
}
