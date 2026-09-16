use super::*;
use std::ops::Range;
use tcode_protocol::{HostMessage, MAX_SESSION_HISTORY_BYTES, SESSION_HISTORY_RECORDS};

/// Count serialized bytes without allocating a second copy of a large record.
struct ByteBudget(usize);

impl std::io::Write for ByteBudget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.checked_sub(bytes.len()).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "history byte budget exhausted",
            )
        })?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Preserve contiguous absolute cursors while fitting the complete wire envelope.
fn bounded_range(
    records: &[SessionEventRecord],
    requested: Range<usize>,
    backwards: bool,
    overhead: usize,
) -> Result<Range<usize>, tcode_protocol::ProtocolError> {
    let mut budget = ByteBudget(MAX_SESSION_HISTORY_BYTES.saturating_sub(overhead));
    let mut count = 0;
    for offset in 0..requested.len() {
        let index = if backwards {
            requested.end - 1 - offset
        } else {
            requested.start + offset
        };
        if budget.0 == 0 {
            break;
        }
        budget.0 -= 1; // Record separator; also leaves room for an empty array.
        if serde_json::to_writer(&mut budget, &records[index]).is_err() {
            break;
        }
        count += 1;
    }
    if count == 0 && !requested.is_empty() {
        return Err(tcode_protocol::ProtocolError {
            code: "history_record_too_large".into(),
            message: "A history record exceeds the 8 MiB response limit.".into(),
        });
    }
    Ok(if backwards {
        requested.end - count..requested.end
    } else {
        requested.start..requested.start + count
    })
}

/// The complete event log of one session, held in memory while the session
/// is resident so history windows cost the page rather than a re-parse of the
/// JSONL, plus the fold that decides where each window may start.
///
/// Memory policy: [`AppState::event_records`] holds a log for every live or
/// parked session (bounded by the resident LRU) and for nothing else. A log
/// is loaded when a client opens the session or when the session first
/// appends in this process, and is dropped once the session leaves residency
/// and the store writer has flushed every append queued from it
/// ([`AppState::release_stale_session_logs`]): until then the log, not the
/// JSONL, is the whole conversation.
#[derive(Clone)]
pub(super) struct SessionLog {
    records: Vec<SessionEventRecord>,
    /// Indices of the records that opened a turn in `fold`, ascending.
    turn_starts: Vec<usize>,
    /// `Timeline::fold_events(records)`, extended by every push so an append
    /// can tell whether it opens a turn, and cloned by timeline loads instead
    /// of folding the records again.
    fold: Timeline,
    /// The length flushed by the release barrier in flight, if any.
    release_barrier: Option<usize>,
}

impl SessionLog {
    pub(super) fn load(store: &SessionStore, session_id: &str) -> Self {
        Self::from_records(store.read_events(session_id))
    }

    pub(super) fn from_records(records: impl IntoIterator<Item = SessionEventRecord>) -> Self {
        let mut log = Self {
            records: Vec::new(),
            turn_starts: Vec::new(),
            fold: Timeline::default(),
            release_barrier: None,
        };
        for record in records {
            log.push(record);
        }
        log
    }

    pub(super) fn push(&mut self, record: SessionEventRecord) {
        let turns = self.fold.turns.len();
        self.fold.apply_at(record.ts, &record.event);
        if self.fold.turns.len() > turns {
            self.turn_starts.push(self.records.len());
        }
        self.records.push(record);
    }

    pub(super) fn records(&self) -> &[SessionEventRecord] {
        &self.records
    }

    /// The pure fold of every record, before any `mark_idle`.
    pub(super) fn fold(&self) -> &Timeline {
        &self.fold
    }

    /// Move a window start back to the record that opens its turn. The client
    /// folds only the records it holds, so a window that begins mid-turn
    /// renders a partial first turn whose entries shift as earlier pages
    /// arrive, while a window that begins where a turn begins opens its turns
    /// exactly where the full log does. Records that open no turn leave the
    /// start unchanged.
    fn turn_aligned_start(&self, start: usize) -> usize {
        let opened_before = self.turn_starts.partition_point(|&index| index <= start);
        opened_before
            .checked_sub(1)
            .map_or(start, |last| self.turn_starts[last])
    }
}

impl AppState {
    /// The session's log, cached for the resident session it belongs to. A
    /// non-resident session is read cold and not retained, so paging it never
    /// grows the cache.
    fn history_log(&mut self, session_id: &str) -> std::borrow::Cow<'_, SessionLog> {
        if !self.event_records.contains_key(session_id) {
            let log = SessionLog::load(&self.store, session_id);
            if self.resident(session_id).is_none() {
                return std::borrow::Cow::Owned(log);
            }
            self.event_records.insert(session_id.to_string(), log);
        }
        std::borrow::Cow::Borrowed(&self.event_records[session_id])
    }

    /// Queue a store-writer barrier for every cached log whose session left
    /// residency, and drop the log once the barrier echoes: a cold read after
    /// that sees every append the log had accepted. Appends that race the
    /// barrier re-arm it.
    pub(super) fn release_stale_session_logs(&mut self, cx: &mut HostCx) {
        let stale: Vec<(String, usize)> = self
            .event_records
            .iter()
            .filter(|(id, log)| log.release_barrier.is_none() && self.resident(id).is_none())
            .map(|(id, log)| (id.clone(), log.records.len()))
            .collect();
        for (session_id, flushed) in stale {
            self.event_records
                .get_mut(&session_id)
                .expect("stale log collected above")
                .release_barrier = Some(flushed);
            let (completion, completed) = smol::channel::bounded(1);
            self.enqueue_store_write(StoreWrite::Flush(completion), cx);
            let host_cx = cx.clone();
            HostCx::spawn_detached(cx, async move {
                // A stopped writer has dropped its queue as well; nothing
                // later can still land in the JSONL.
                let _ = completed.recv().await;
                host_cx.enqueue(move |state, cx| {
                    let Some(log) = state.event_records.get_mut(&session_id) else {
                        return;
                    };
                    log.release_barrier = None;
                    let appended_since = log.records.len() != flushed;
                    if state.resident(&session_id).is_some() {
                        return;
                    }
                    if appended_since {
                        state.release_stale_session_logs(cx);
                    } else {
                        state.event_records.remove(&session_id);
                    }
                });
            });
        }
    }

    pub(crate) fn session_events_snapshot(
        &mut self,
        subscription: &tcode_protocol::Subscription,
    ) -> ServerEvent {
        let Topic::SessionEvents { session_id } = &subscription.topic else {
            unreachable!()
        };
        let log = self.history_log(session_id);
        let records = log.records();
        let total = records.len();
        let total_turns = log.fold().turns.len() as u64;
        let after = subscription.after.filter(|after| *after <= total as u64);
        let from = after.map_or_else(
            || log.turn_aligned_start(total.saturating_sub(400)),
            |after| after as usize,
        );
        let empty = HostMessage::Event(EventEnvelope {
            request_id: Some(u64::MAX),
            topic: subscription.topic.clone(),
            event: ServerEvent::SessionSnapshot {
                from: u64::MAX,
                records: vec![],
                total: u64::MAX,
                total_turns: u64::MAX,
                truncated: false,
            },
        });
        let overhead = serde_json::to_vec(&empty)
            .expect("serializable snapshot")
            .len()
            + 1;
        match bounded_range(records, from..total, after.is_none(), overhead) {
            Ok(range) => ServerEvent::SessionSnapshot {
                from: range.start as u64,
                truncated: range.len() < total - from,
                records: records[range].to_vec(),
                total: total as u64,
                total_turns,
            },
            Err(error) => ServerEvent::SessionHistoryError(error),
        }
    }

    pub(crate) fn session_history_page(
        &mut self,
        session_id: &str,
        before: u64,
        limit: u32,
    ) -> Result<QueryResponse, tcode_protocol::ProtocolError> {
        let log = self.history_log(session_id);
        let records = log.records();
        let end = before.min(records.len() as u64) as usize;
        let count = (limit as usize).clamp(1, SESSION_HISTORY_RECORDS);
        let requested = log.turn_aligned_start(end.saturating_sub(count))..end;
        let empty = HostMessage::QueryResult {
            id: u64::MAX,
            result: Ok(QueryResponse::SessionHistoryPage {
                records: vec![],
                from: u64::MAX,
                truncated: false,
            }),
        };
        let overhead = serde_json::to_vec(&empty).expect("serializable page").len() + 1;
        let range = bounded_range(records, requested.clone(), true, overhead)?;
        Ok(QueryResponse::SessionHistoryPage {
            from: range.start as u64,
            truncated: range.len() < requested.len(),
            records: records[range].to_vec(),
        })
    }
}
