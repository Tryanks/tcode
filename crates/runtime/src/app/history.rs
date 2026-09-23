use super::*;
use agent::DeltaKind;
use std::borrow::Cow;
use std::ops::Range;
use tcode_protocol::{
    HostMessage, MAX_SESSION_HISTORY_BYTES, OUTPUT_PREVIEW_BYTES, SESSION_HISTORY_RECORDS,
    SESSION_WINDOW_BYTES,
};

/// Count serialized bytes without allocating a copy of a large record.
struct ByteCount(usize);

impl std::io::Write for ByteCount {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn wire_len(record: &SessionEventRecord) -> usize {
    let mut count = ByteCount(0);
    serde_json::to_writer(&mut count, record).expect("serializable record");
    count.0
}

/// Keep [`OUTPUT_PREVIEW_BYTES`] of `text`, returning its full length when it
/// was longer. A command keeps its tail: the end of a run is what its panel
/// shows, and the host renders the whole output on request.
fn shorten(text: &mut String, keep_tail: bool) -> Option<u64> {
    if text.len() <= OUTPUT_PREVIEW_BYTES {
        return None;
    }
    let full = text.len() as u64;
    if keep_tail {
        let mut start = text.len() - OUTPUT_PREVIEW_BYTES;
        while !text.is_char_boundary(start) {
            start += 1;
        }
        text.drain(..start);
    } else {
        text.truncate(text.floor_char_boundary(OUTPUT_PREVIEW_BYTES));
    }
    Some(full)
}

fn oversized_output(event: &AgentEvent) -> bool {
    match event {
        AgentEvent::ItemStarted(item)
        | AgentEvent::ItemUpdated(item)
        | AgentEvent::ItemCompleted(item) => match &item.content {
            ItemContent::ToolCall {
                output: Some(output),
                ..
            }
            | ItemContent::CommandExecution { output, .. } => output.len() > OUTPUT_PREVIEW_BYTES,
            _ => false,
        },
        AgentEvent::Delta {
            kind: DeltaKind::CommandOutput,
            text,
            ..
        } => text.len() > OUTPUT_PREVIEW_BYTES,
        _ => false,
    }
}

/// A record as clients receive it: tool and command output beyond
/// [`OUTPUT_PREVIEW_BYTES`] stays on the host, read back with
/// `Query::ReadItemOutput`.
pub(super) fn wire_record(record: &SessionEventRecord) -> Cow<'_, SessionEventRecord> {
    if !oversized_output(&record.event) {
        return Cow::Borrowed(record);
    }
    let mut record = record.clone();
    record.elided = match &mut record.event {
        AgentEvent::ItemStarted(item)
        | AgentEvent::ItemUpdated(item)
        | AgentEvent::ItemCompleted(item) => match &mut item.content {
            ItemContent::ToolCall {
                output: Some(output),
                ..
            } => shorten(output, false),
            ItemContent::CommandExecution { output, .. } => shorten(output, true),
            _ => None,
        },
        AgentEvent::Delta { text, .. } => shorten(text, true),
        _ => None,
    };
    Cow::Owned(record)
}

/// Indices from `from` on of turn-change records that a later record for the
/// same turn replaces. Both land on the same turn and nothing reads a turn's
/// diffs between them, so the earlier one crosses the wire without diffs.
fn superseded_turn_changes(records: &[SessionEventRecord], from: usize) -> HashSet<usize> {
    let mut later = HashSet::new();
    let mut superseded = HashSet::new();
    for index in (from..records.len()).rev() {
        if let AgentEvent::TurnChangesUpdated { turn_id, .. } = &records[index].event
            && !turn_id.is_empty()
            && !later.insert(turn_id.as_str())
        {
            superseded.insert(index);
        }
    }
    superseded
}

fn without_diffs(mut record: SessionEventRecord) -> SessionEventRecord {
    if let AgentEvent::TurnChangesUpdated { changes, .. } = &mut record.event {
        for change in changes {
            change.diff = None;
        }
    }
    record
}

/// The records that stand for the log cursors in `range`.
struct Window {
    range: Range<usize>,
    records: Vec<SessionEventRecord>,
}

/// Choose the contiguous cursors a reply covers and the records it sends for
/// them. Starting from the `backwards` end of `requested`, records are taken
/// while the reply fits [`SESSION_WINDOW_BYTES`], and always at least one.
/// Consecutive deltas of one item are merged into one, which folds the same
/// because nothing between them moves the turn.
fn wire_window(
    records: &[SessionEventRecord],
    requested: Range<usize>,
    backwards: bool,
    overhead: usize,
) -> Result<Window, tcode_protocol::ProtocolError> {
    let superseded = superseded_turn_changes(records, requested.start);
    let prepared = |index: usize| -> Cow<'_, SessionEventRecord> {
        if superseded.contains(&index) {
            Cow::Owned(without_diffs(wire_record(&records[index]).into_owned()))
        } else {
            wire_record(&records[index])
        }
    };
    let budget = SESSION_WINDOW_BYTES.saturating_sub(overhead);
    let mut used = 0;
    let mut count = 0;
    for offset in 0..requested.len() {
        let index = if backwards {
            requested.end - 1 - offset
        } else {
            requested.start + offset
        };
        // Separator; also leaves room for an empty array.
        let size = wire_len(&prepared(index)) + 1;
        if count == 0 && size > MAX_SESSION_HISTORY_BYTES.saturating_sub(overhead) {
            return Err(tcode_protocol::ProtocolError {
                code: "history_record_too_large".into(),
                message: "A history record exceeds the 8 MiB response limit.".into(),
            });
        }
        if count > 0 && used + size > budget {
            break;
        }
        used += size;
        count += 1;
    }
    let range = if backwards {
        requested.end - count..requested.end
    } else {
        requested.start..requested.start + count
    };
    let mut merged: Vec<SessionEventRecord> = Vec::with_capacity(range.len());
    for index in range.clone() {
        let record = &records[index];
        if let AgentEvent::Delta {
            item_id,
            kind,
            text,
        } = &record.event
            && index > range.start
            && let Some(AgentEvent::Delta {
                item_id: last_id,
                kind: last_kind,
                text: last_text,
            }) = merged.last_mut().map(|last| &mut last.event)
            && matches!(&records[index - 1].event, AgentEvent::Delta { .. })
            && last_id == item_id
            && last_kind == kind
        {
            last_text.push_str(text);
            continue;
        }
        merged.push(if superseded.contains(&index) {
            without_diffs(record.clone())
        } else {
            record.clone()
        });
    }
    let records = merged
        .iter()
        .map(|record| wire_record(record).into_owned())
        .collect();
    Ok(Window { range, records })
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
                end: u64::MAX,
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
        match wire_window(records, from..total, after.is_none(), overhead) {
            Ok(window) => ServerEvent::SessionSnapshot {
                from: window.range.start as u64,
                end: window.range.end as u64,
                truncated: window.range.len() < total - from,
                records: window.records,
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
                end: u64::MAX,
                truncated: false,
            }),
        };
        let overhead = serde_json::to_vec(&empty).expect("serializable page").len() + 1;
        let window = wire_window(records, requested.clone(), true, overhead)?;
        Ok(QueryResponse::SessionHistoryPage {
            from: window.range.start as u64,
            end: window.range.end as u64,
            truncated: window.range.len() < requested.len(),
            records: window.records,
        })
    }

    /// The whole output of one item, as the full log folds it.
    pub(crate) fn item_output(
        &mut self,
        session_id: &str,
        item_id: &str,
    ) -> Result<QueryResponse, tcode_protocol::ProtocolError> {
        let log = self.history_log(session_id);
        let output = log
            .fold()
            .entries
            .iter()
            .rev()
            .find(|entry| entry.id == item_id)
            .and_then(|entry| match &entry.content {
                EntryContent::Item(ItemContent::ToolCall { output, .. }) => output.clone(),
                EntryContent::Item(ItemContent::CommandExecution { output, .. }) => {
                    Some(output.clone())
                }
                _ => None,
            })
            .ok_or_else(|| tcode_protocol::ProtocolError {
                code: "unknown_item_output".into(),
                message: format!("no output for item {item_id} in {session_id}"),
            })?;
        if output.len() > MAX_SESSION_HISTORY_BYTES {
            return Err(tcode_protocol::ProtocolError {
                code: "item_output_too_large".into(),
                message: "The output exceeds the 8 MiB response limit.".into(),
            });
        }
        Ok(QueryResponse::ItemOutput(output))
    }
}
