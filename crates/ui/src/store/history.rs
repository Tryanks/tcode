use super::*;

pub(crate) const HISTORY_WINDOW_SCREENS: f32 = 6.;

/// How long the reader stays at the tail before the pages fetched above it
/// are dropped, so a glance down and back up does not fetch them again.
pub(super) const HISTORY_TRIM_DELAY: std::time::Duration = std::time::Duration::from_secs(30);

/// The records of one thread's log the client holds: they stand for the
/// cursors `from..end` of layout `epoch`. The host may merge records, so the
/// range can be longer than the records.
pub(super) struct HeldHistory {
    pub(super) epoch: u64,
    pub(super) from: u64,
    pub(super) end: u64,
    pub(super) records: Vec<StoredEvent>,
    /// The earlier pages put in front of the window since it arrived, in the
    /// order they were fetched, so the oldest records come from the last.
    pages: Vec<HeldPage>,
}

struct HeldPage {
    records: usize,
    /// Where the window started before the page: `from` again once the page
    /// is dropped.
    end: u64,
    /// Fetched while the reader followed the tail, which therefore needs it.
    tail: bool,
}

impl HeldHistory {
    pub(super) fn new(window: &SessionWindow) -> Self {
        Self {
            epoch: window.epoch,
            from: window.from,
            end: window.end,
            records: window.records.clone(),
            pages: Vec::new(),
        }
    }

    /// Records that continue the held cursor.
    pub(super) fn extend(&mut self, window: &SessionWindow) {
        self.records.extend(window.records.iter().cloned());
        self.end = window.end;
    }

    fn prepend(&mut self, records: Vec<StoredEvent>, from: u64, tail: bool) {
        self.pages.push(HeldPage {
            records: records.len(),
            end: self.from,
            tail,
        });
        self.records.splice(0..0, records);
        self.from = from;
    }

    fn holds_pages_above_tail(&self) -> bool {
        self.pages.last().is_some_and(|page| !page.tail)
    }

    /// Drop the pages fetched while the reader was away from the tail, back
    /// to the last page the tail needed. What remains is a window the client
    /// already held and showed, plus what arrived live since, so it folds as
    /// that window did; the tail's own pages keep the prefetch satisfied, so
    /// no page dropped here is fetched again while the reader stays there.
    pub(super) fn drop_pages_above_tail(&mut self) -> bool {
        let mut dropped = 0;
        while let Some(page) = self.pages.pop_if(|page| !page.tail) {
            dropped += page.records;
            self.from = page.end;
        }
        self.records.drain(..dropped);
        dropped > 0
    }
}

impl WorkspaceStore {
    fn selected_history(&self) -> Option<&HeldHistory> {
        self.threads
            .get(self.selected_session_id.as_ref()?)?
            .history
            .as_ref()
    }

    pub(crate) fn history_available(&self) -> bool {
        self.selected_history().is_some_and(|held| held.from > 0)
    }

    pub(crate) fn history_loading(&self) -> bool {
        self.history_task.is_some() && self.history_error.is_none()
    }
    pub(crate) fn history_error(&self) -> Option<&str> {
        self.history_error.as_deref()
    }

    /// Chat tests that run frame callbacks would otherwise request a page from
    /// the seeded host, whose reply wakes the test scheduler from a thread it
    /// does not control. A failed page holds prefetch until its retry cooldown.
    #[cfg(test)]
    pub(crate) fn suppress_history_prefetch_for_test(&mut self) {
        self.history_error = Some("prefetch suppressed".into());
    }

    pub(super) fn load_pending_chat_history(&mut self, cx: &mut Context<Self>) {
        if self.history_error.is_none()
            && self.pending_chat_turn.as_ref().is_some_and(|(id, turn)| {
                self.selected_session_id.as_ref() == Some(id) && *turn < self.session_turn_offset
            })
        {
            self.load_earlier_messages(cx);
        }
    }

    /// Geometry is reported after layout, including the first frame on restore.
    /// Event counts cannot predict the height of folded turns.
    pub(crate) fn update_history_window(
        &mut self,
        screens: f32,
        following_tail: bool,
        cx: &mut Context<Self>,
    ) {
        if self.session_loading() || !screens.is_finite() {
            return;
        }
        let records = self.selected_history().map_or(0, |held| held.records.len());
        if self.history_logged_records != Some(records) {
            log::debug!(
                "history-window session={:?} records_loaded={} screens_covered={:.2} pages_fetched={}",
                self.selected_session_id,
                records,
                screens.max(0.),
                self.history_pages_fetched
            );
            self.history_logged_records = Some(records);
        }
        if screens < HISTORY_WINDOW_SCREENS && self.history_error.is_none() {
            self.load_history_pages(following_tail, cx);
        }
        if !following_tail {
            self.history_trim = None;
        } else if self.history_trim.is_none()
            && self
                .selected_history()
                .is_some_and(HeldHistory::holds_pages_above_tail)
        {
            let generation = self.selection_generation;
            self.history_trim = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(HISTORY_TRIM_DELAY).await;
                let _ = this.update(cx, |store, cx| {
                    if store.selection_generation == generation {
                        store.history_trim = None;
                        store.drop_selected_pages_above_tail();
                        cx.notify();
                    }
                });
            }));
        }
    }

    /// The rows the dropped pages rendered are above the reader, who follows
    /// the tail, and the list removes them without moving what is on screen.
    fn drop_selected_pages_above_tail(&mut self) {
        // A page in flight continues the cursor it was asked from; the next
        // report from the tail schedules the drop again.
        if self.history_task.is_some() || self.session_catching_up {
            return;
        }
        let Some(session_id) = self.selected_session_id.clone() else {
            return;
        };
        let Some(held) = self
            .threads
            .get_mut(&session_id)
            .and_then(|thread| thread.history.as_mut())
        else {
            return;
        };
        if !held.drop_pages_above_tail() {
            return;
        }
        let previous_turns = self
            .session_replica
            .as_ref()
            .map_or(0, |(_, timeline)| timeline.turns.len());
        let mut timeline = self.fold_held_records(&session_id);
        self.session_turn_offset += previous_turns.saturating_sub(timeline.turns.len());
        self.settle_running_turn(&mut timeline);
        self.session_replica = Some((session_id, timeline));
    }

    pub(crate) fn load_earlier_messages(&mut self, cx: &mut Context<Self>) {
        self.load_history_pages(false, cx);
    }

    fn load_history_pages(&mut self, tail: bool, cx: &mut Context<Self>) {
        if self.history_task.is_some() || !self.history_available() || self.session_loading() {
            return;
        }
        let session_id = self.selected_session_id.clone().expect("selected history");
        let held = self.selected_history().expect("selected history");
        let (before, epoch) = (held.from, held.epoch);
        let generation = self.selection_generation;
        let host = self.host.clone();
        self.history_error = None;
        self.history_task = Some(cx.spawn(async move |this, cx| {
            let result = host
                .query(Query::SessionHistoryPage {
                    session_id: session_id.clone(),
                    epoch,
                    before,
                    limit: tcode_protocol::SESSION_HISTORY_RECORDS as u32,
                })
                .await;
            let failed = this.update(cx, |store, cx| {
                if store.selection_generation != generation {
                    return false;
                }
                let held_epoch = store.selected_history().map(|held| held.epoch);
                match result {
                    // The held records moved to another layout while the
                    // request was out; its positions mean nothing there.
                    Ok(QueryResponse::SessionHistoryPage { epoch, .. }) if Some(epoch) != held_epoch => {}
                    Ok(QueryResponse::SessionHistoryReset(window)) => {
                        if Some(window.epoch) != held_epoch {
                            let topic = tcode_protocol::Topic::SessionEvents {
                                session_id: session_id.clone(),
                            };
                            store.apply_session_window(&topic, &window);
                        }
                    }
                    Ok(QueryResponse::SessionHistoryPage {
                        records, from, end, ..
                    }) if from < before
                            && end == before
                            && store.selected_history().map(|held| held.from) == Some(before) =>
                    {
                        store.history_pages_fetched += 1;
                        let previous_turns = store
                            .session_replica
                            .as_ref()
                            .map_or(0, |(_, timeline)| timeline.turns.len());
                        store
                            .threads
                            .get_mut(&session_id)
                            .and_then(|thread| thread.history.as_mut())
                            .expect("held window checked above")
                            .prepend(records, from, tail);
                        let mut timeline = store.fold_held_records(&session_id);
                        store.session_turn_offset = store
                            .session_turn_offset
                            .saturating_sub(timeline.turns.len().saturating_sub(previous_turns));
                        store.settle_running_turn(&mut timeline);
                        store.session_replica = Some((session_id, timeline));
                    }
                    Err(error) => {
                        log::warn!("Earlier history page failed for {session_id} before {before}: {error:?}");
                        store.history_error = Some(error.message);
                    }
                    _ => {
                        log::warn!("Invalid earlier history page response for {session_id} before {before}");
                        store.history_error = Some("Invalid history page response".into());
                    }
                }
                let failed = store.history_error.is_some();
                cx.notify();
                failed
            }).unwrap_or(false);
            // Keep the single request gate reserved while layout measures the
            // prepended rows. The next request uses that fresh geometry.
            cx.background_executor().timer(if failed {
                std::time::Duration::from_secs(5)
            } else {
                std::time::Duration::from_millis(250)
            }).await;
            let _ = this.update(cx, |store, cx| {
                if store.selection_generation == generation {
                    store.history_task = None;
                    if failed {
                        store.history_error = None;
                    }
                    store.load_pending_chat_history(cx);
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }
}

pub(super) fn history_error_message(error: tcode_protocol::ProtocolError) -> String {
    if error.code == "history_record_too_large" {
        crate::tr!("chat.history_record_too_large").into_owned()
    } else {
        error.message
    }
}
