use super::*;

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
            let failed = this.update(cx, |store, cx| {
                if store.selection_generation != generation {
                    return false;
                }
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
                if !failed {
                    store.history_task = None;
                    store.load_pending_chat_history(cx);
                }
                cx.notify();
                failed
            }).unwrap_or(false);
            if failed {
                // Retain the task as the request gate during cooldown, but hide
                // activity. Selection changes cancel both the request and cooldown.
                cx.background_executor().timer(std::time::Duration::from_secs(5)).await;
                let _ = this.update(cx, |store, cx| {
                    if store.selection_generation == generation {
                        store.history_task = None;
                        cx.notify();
                    }
                });
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, TestAppContext};
    use tcode_runtime::pipe::{HostServices, spawn_host};
    use tcode_services::store::SessionStore;
    #[gpui::test]
    fn failed_prefetch_is_silent_and_retries_only_after_cooldown(cx: &mut TestAppContext) {
        let root = std::env::temp_dir().join(format!(
            "tcode-prefetch-retry-test-{}",
            tcode_services::store::now_millis()
        ));
        let host = spawn_host(
            SessionStore::open_at(root.clone()).unwrap(),
            HostServices::default(),
        )
        .unwrap();
        let status = smol::block_on(host.update_state_for_test(|state, cx| {
            let id = state.start_draft("history".into(), std::env::temp_dir(), cx);
            state.session_status_snapshot(&id).unwrap()
        }))
        .unwrap();
        host.shutdown_blocking().unwrap();
        std::fs::remove_dir_all(root).unwrap();

        let (to_host, outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump_link = link.clone();
        let _pump = cx
            .background_executor
            .spawn(async move { pump_link.pump().await });
        let workspace = cx.new(|cx| {
            WorkspaceStore::new_attached(link, WorkspaceAttachment::Local, None, false, cx)
        });
        workspace.update(cx, |store, _| {
            store.selected_session_id = Some("large".into());
            store.session_from.insert("large".into(), 1800);
            store.session_replica = Some(("large".into(), Default::default()));
            store.session_status_replica = Some(status);
        });
        while outgoing.try_recv().is_ok() {}
        workspace.update(cx, |store, cx| store.load_earlier_messages(cx));
        cx.run_until_parked();
        let request = tcode_protocol::decode_client_line(&outgoing.try_recv().unwrap()).unwrap();
        assert!(matches!(
            request.payload,
            tcode_protocol::ClientPayload::Query(tcode_protocol::Query::SessionHistoryPage {
                before: 1800,
                limit: 200,
                ..
            })
        ));
        workspace.update(cx, |store, cx| {
            assert!(store.history_loading());
            store.load_earlier_messages(cx);
        });
        cx.run_until_parked();
        assert!(
            outgoing.try_recv().is_err(),
            "scrolling while loading must not queue another page"
        );
        incoming
            .try_send(
                tcode_protocol::encode_line(&tcode_protocol::HostMessage::QueryResult {
                    id: request.id,
                    result: Err(tcode_protocol::ProtocolError::decode("offline")),
                })
                .unwrap(),
            )
            .unwrap();
        cx.run_until_parked();
        workspace.update(cx, |store, cx| {
            assert!(!store.history_loading(), "failed pages hide activity");
            store.load_earlier_messages(cx);
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(4));
        workspace.update(cx, |store, cx| store.load_earlier_messages(cx));
        cx.run_until_parked();
        assert!(
            outgoing.try_recv().is_err(),
            "retry is throttled for five seconds"
        );
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        workspace.update(cx, |store, cx| store.load_earlier_messages(cx));
        cx.run_until_parked();
        let retry = tcode_protocol::decode_client_line(&outgoing.try_recv().unwrap()).unwrap();
        assert_eq!(
            retry.payload, request.payload,
            "retry requests the same bounded page"
        );
        workspace.update(cx, |store, cx| {
            assert!(store.history_loading());
            store.load_earlier_messages(cx);
        });
        cx.run_until_parked();
        assert!(
            outgoing.try_recv().is_err(),
            "retry also permits only one page in flight"
        );
    }
}
