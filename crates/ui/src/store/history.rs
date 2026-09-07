use super::*;

impl WorkspaceStore {
    pub(crate) fn history_available(&self) -> bool {
        self.selected_session_id
            .as_ref()
            .and_then(|id| self.session_from.get(id))
            .is_some_and(|from| *from > 0)
    }

    pub(crate) fn history_loading(&self) -> bool {
        self.history_task.is_some()
    }
    pub(crate) fn history_error(&self) -> Option<&str> {
        self.history_error.as_deref()
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

    pub(crate) fn load_earlier_messages(&mut self, cx: &mut Context<Self>) {
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
            let _ = this.update(cx, |store, cx| {
                if store.selection_generation != generation {
                    return;
                }
                store.history_task = None;
                match result {
                    Ok(QueryResponse::SessionHistoryPage { records, from, .. })
                        if from < before
                            && from + records.len() as u64 == before
                            && store.session_from.get(&session_id) == Some(&before) =>
                    {
                        let previous_turns = store
                            .session_replica
                            .as_ref()
                            .map_or(0, |(_, timeline)| timeline.turns.len());
                        let held = store.session_records.entry(session_id.clone()).or_default();
                        held.splice(0..0, records);
                        store.session_from.insert(session_id.clone(), from);
                        let mut timeline = Timeline::fold_events(held.iter().cloned());
                        if !store
                            .session_status_replica
                            .as_ref()
                            .is_some_and(|status| status.turn_running)
                        {
                            timeline.mark_idle();
                        }
                        store.session_turn_offset = store
                            .session_turn_offset
                            .saturating_sub(timeline.turns.len().saturating_sub(previous_turns));
                        store.session_replica = Some((session_id, timeline));
                    }
                    Err(error) => store.history_error = Some(history_error_message(error)),
                    _ => {
                        store.history_error =
                            Some(crate::tr!("chat.history_unavailable").into_owned())
                    }
                }
                store.load_pending_chat_history(cx);
                cx.notify();
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
