use super::*;

pub(crate) const HISTORY_WINDOW_SCREENS: f32 = 6.;

impl WorkspaceStore {
    pub(crate) fn history_available(&self) -> bool {
        self.selected_session_id
            .as_ref()
            .and_then(|id| self.session_from.get(id))
            .is_some_and(|from| *from > 0)
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
    pub(crate) fn update_history_window(&mut self, screens: f32, cx: &mut Context<Self>) {
        if self.session_loading() || !screens.is_finite() {
            return;
        }
        let records = self
            .selected_session_id
            .as_ref()
            .and_then(|id| self.session_records.get(id))
            .map_or(0, Vec::len);
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
            self.load_earlier_messages(cx);
        }
    }

    pub(crate) fn load_earlier_messages(&mut self, cx: &mut Context<Self>) {
        self.load_history_pages(cx);
    }

    fn load_history_pages(&mut self, cx: &mut Context<Self>) {
        if self.history_task.is_some() || !self.history_available() || self.session_loading() {
            return;
        }
        let session_id = self.selected_session_id.clone().expect("selected history");
        let before = self.session_from[&session_id];
        let generation = self.selection_generation;
        let host = self.host.clone();
        self.history_error = None;
        self.history_task = Some(cx.spawn(async move |this, cx| {
            let result = host
                .query(Query::SessionHistoryPage {
                    session_id: session_id.clone(),
                    before,
                    limit: tcode_protocol::SESSION_HISTORY_RECORDS as u32,
                })
                .await;
            let failed = this.update(cx, |store, cx| {
                if store.selection_generation != generation {
                    return false;
                }
                match result {
                    Ok(QueryResponse::SessionHistoryPage {
                        records, from, end, ..
                    }) if from < before
                            && end == before
                            && store.session_from.get(&session_id) == Some(&before) =>
                    {
                        store.history_pages_fetched += 1;
                        let previous_turns = store
                            .session_replica
                            .as_ref()
                            .map_or(0, |(_, timeline)| timeline.turns.len());
                        let held = store.session_records.entry(session_id.clone()).or_default();
                        held.splice(0..0, records);
                        store.session_from.insert(session_id.clone(), from);
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
