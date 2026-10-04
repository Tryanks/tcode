use super::*;
use agent::DeltaKind;
use std::borrow::Cow;
use std::ops::Range;
use tcode_protocol::{
    CommandResponse, HostMessage, MAX_SESSION_HISTORY_BYTES, OUTPUT_PREVIEW_BYTES,
    SESSION_HISTORY_RECORDS, SESSION_WINDOW_BYTES, SessionWindow, Subscription,
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

/// A position in one layout of a session's log. Rewriting the JSONL
/// renumbers its records under a new epoch, so a position alone does not say
/// which record it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct LogCursor {
    pub(super) epoch: u64,
    pub(super) position: usize,
}

/// The complete event log of one session, held in memory while the session
/// is resident so history windows cost the page rather than a re-parse of the
/// JSONL, plus the fold that decides where each window may start.
///
/// Memory policy: [`AppState::event_records`] holds a log for every live or
/// parked session (bounded by the resident LRU) and for nothing else. A log
/// is read off the mailbox ([`Hydration`]) when a client opens the session
/// or when the session first appends in this process, and is dropped once
/// the session leaves residency and the store writer has flushed every
/// append queued from it ([`AppState::release_stale_session_logs`]): until
/// then the log, not the JSONL, is the whole conversation.
///
/// A resident log keeps the layout it was loaded in. Compaction rewrites the
/// JSONL beneath it without touching it, so its positions, its fold and every
/// client cursor into it stay valid; the rewritten layout is served once the
/// log is next loaded, and cursors into the old one get a baseline then.
#[derive(Clone)]
pub(super) struct SessionLog {
    epoch: u64,
    records: Vec<SessionEventRecord>,
    /// Indices of the records that opened a turn in `fold`, ascending.
    turn_starts: Vec<usize>,
    /// `Timeline::fold_events(records)`, extended by every push so an append
    /// can tell whether it opens a turn, and cloned by timeline loads instead
    /// of folding the records again.
    fold: Timeline,
    /// The end flushed by the release barrier in flight, if any.
    release_barrier: Option<LogCursor>,
}

impl SessionLog {
    pub(super) fn new(epoch: u64, records: impl IntoIterator<Item = SessionEventRecord>) -> Self {
        let mut log = Self {
            epoch,
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

    pub(super) fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The cursor just past the last record.
    pub(super) fn end(&self) -> LogCursor {
        LogCursor {
            epoch: self.epoch,
            position: self.records.len(),
        }
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

    /// The window answering a subscription: the records from its cursor on
    /// when the cursor is in this log's layout, otherwise a baseline that
    /// replaces whatever the client holds.
    pub(super) fn events_window(&self, subscription: &Subscription) -> ServerEvent {
        let after = subscription.after.filter(|after| {
            subscription.epoch == Some(self.epoch) && *after <= self.records.len() as u64
        });
        let empty = HostMessage::Event(EventEnvelope {
            request_id: Some(u64::MAX),
            topic: subscription.topic.clone(),
            event: ServerEvent::SessionSnapshot(empty_window()),
        });
        match session_window(
            self,
            after.map(|after| after as usize),
            wire_overhead(&empty),
        ) {
            Ok(window) => ServerEvent::SessionSnapshot(window),
            Err(error) => ServerEvent::SessionHistoryError(error),
        }
    }

    pub(super) fn history_page(
        &self,
        epoch: u64,
        before: u64,
        limit: u32,
    ) -> Result<QueryResponse, tcode_protocol::ProtocolError> {
        if epoch != self.epoch {
            let empty = HostMessage::QueryResult {
                id: u64::MAX,
                result: Ok(QueryResponse::SessionHistoryReset(empty_window())),
            };
            return session_window(self, None, wire_overhead(&empty))
                .map(QueryResponse::SessionHistoryReset);
        }
        let records = self.records();
        let end = before.min(records.len() as u64) as usize;
        let count = (limit as usize).clamp(1, SESSION_HISTORY_RECORDS);
        let requested = self.turn_aligned_start(end.saturating_sub(count))..end;
        let empty = HostMessage::QueryResult {
            id: u64::MAX,
            result: Ok(QueryResponse::SessionHistoryPage {
                epoch: u64::MAX,
                records: vec![],
                from: u64::MAX,
                end: u64::MAX,
                truncated: false,
            }),
        };
        let window = wire_window(records, requested.clone(), true, wire_overhead(&empty))?;
        Ok(QueryResponse::SessionHistoryPage {
            epoch: self.epoch,
            from: window.range.start as u64,
            end: window.range.end as u64,
            truncated: window.range.len() < requested.len(),
            records: window.records,
        })
    }

    /// The whole output of one item, as the full log folds it.
    pub(super) fn item_output(
        &self,
        session_id: &str,
        item_id: &str,
    ) -> Result<QueryResponse, tcode_protocol::ProtocolError> {
        let output = self
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

/// A session's log being read and folded off the mailbox, and what waits for
/// it. A session has at most one, and none while its log is resident.
///
/// The read is of a [`LogSnapshot`](tcode_services::store::LogSnapshot) the
/// store writer takes in its queue order, so it holds every record whose
/// append was queued before the hydration began and none queued after. Every record accepted for the
/// session from then on is held in `pending`, in order, and takes the
/// positions after the snapshot's end; it reaches subscribers once the log is
/// resident, after the replies that end where the snapshot ends.
pub(super) struct Hydration {
    pub(super) pending: Vec<SessionEventRecord>,
    /// Subscription replies; one without a request id answers no request.
    replies: Vec<(Option<u64>, Subscription)>,
    /// Queries answered from the whole log.
    queries: Vec<LogQuery>,
    /// The timeline load waiting for the log, with how many `pending` records
    /// had been accepted when it was requested.
    timeline: Option<(TimelineLoad, usize)>,
}

type LogQuery = Box<dyn FnOnce(&SessionLog) + Send>;

impl AppState {
    /// The session's hydration, begun unless its log is being read already.
    /// Callers check first that the log is not resident.
    pub(super) fn hydrate_log(
        &mut self,
        session_id: &str,
        timeline: Option<TimelineLoad>,
        cx: &mut HostCx,
    ) -> &mut Hydration {
        if !self.log_hydrations.contains_key(session_id) {
            let (snapshot, taken) = smol::channel::bounded(1);
            self.enqueue_store_write(
                StoreWrite::SnapshotLog {
                    id: session_id.to_string(),
                    snapshot,
                },
                cx,
            );
            let read_id = session_id.to_string();
            let host_cx = cx.clone();
            HostCx::spawn_detached(cx, async move {
                // The store writer stops only with the host.
                let Ok(snapshot) = taken.recv().await else {
                    return;
                };
                let log = host_cx
                    .unblock(move || {
                        let read = snapshot.read();
                        SessionLog::new(read.epoch, read.records)
                    })
                    .await;
                host_cx.enqueue(move |state, cx| state.finish_hydration(read_id, log, cx));
            });
            self.log_hydrations.insert(
                session_id.to_string(),
                Hydration {
                    pending: Vec::new(),
                    replies: Vec::new(),
                    queries: Vec::new(),
                    timeline: None,
                },
            );
        }
        let hydration = self
            .log_hydrations
            .get_mut(session_id)
            .expect("inserted above");
        if let Some(load) = timeline {
            hydration.timeline = Some((load, hydration.pending.len()));
        }
        hydration
    }

    /// Serve everything that waited for the log, in the order of its
    /// records, and keep the log only if its session is resident.
    fn finish_hydration(&mut self, session_id: String, mut log: SessionLog, cx: &mut HostCx) {
        let hydration = self
            .log_hydrations
            .remove(&session_id)
            .expect("a hydration is removed only when it finishes");
        let retained = self.resident(&session_id).is_some();
        for (request_id, subscription) in &hydration.replies {
            self.reply_from_log(&log, *request_id, subscription, cx);
        }
        let topic = Topic::SessionEvents {
            session_id: session_id.clone(),
        };
        let mut pending = hydration.pending.into_iter();
        let accepted_before = hydration.timeline.map_or(0, |(_, accepted)| accepted);
        for record in pending.by_ref().take(accepted_before) {
            self.accept_pending(&mut log, record, &topic, cx);
        }
        // A timeline load folds what had been accepted when it was requested;
        // what came after follows any `mark_idle`, as it arrived.
        let folded = hydration
            .timeline
            .filter(|_| retained)
            .map(|(load, _)| (load, log.end(), log.fold().clone()));
        for record in pending {
            self.accept_pending(&mut log, record, &topic, cx);
        }
        for query in hydration.queries {
            query(&log);
        }
        if !retained {
            return;
        }
        self.withdraw_pass_compaction(&session_id);
        self.event_records.insert(session_id.clone(), log);
        if let Some((load, cursor, fold)) = folded {
            self.fold_timeline(session_id, load, cursor, fold, cx);
        }
    }

    /// Append a record accepted while the log was being read, sending it as
    /// `record_event` would have.
    fn accept_pending(
        &self,
        log: &mut SessionLog,
        record: SessionEventRecord,
        topic: &Topic,
        cx: &mut HostCx,
    ) {
        let end = log.end();
        self.emit_domain(
            topic.clone(),
            ServerEvent::SessionEvent {
                epoch: end.epoch,
                position: end.position as u64,
                record: wire_record(&record).into_owned(),
            },
            cx,
        );
        log.push(record);
    }

    /// Answer a subscription with a window of `log` unless nobody is
    /// subscribed any more, and acknowledge the request.
    fn reply_from_log(
        &self,
        log: &SessionLog,
        request_id: Option<u64>,
        subscription: &Subscription,
        cx: &mut HostCx,
    ) {
        if self.subscriptions.contains(&subscription.topic) {
            cx.emit(HostEvent::Domain(EventEnvelope {
                request_id,
                topic: subscription.topic.clone(),
                event: log.events_window(subscription),
            }));
        }
        if let Some(id) = request_id {
            cx.send_message(HostMessage::Ack {
                id,
                result: Ok(CommandResponse::Unit),
            });
        }
    }

    /// Answer a subscription with its snapshot and acknowledge the request.
    /// A session's events are answered from its log: now when it is
    /// resident, otherwise once it has been read, and the acknowledgement
    /// waits with it, so the window is the request's first reply.
    pub(crate) fn reply_to_subscription(
        &mut self,
        request_id: Option<u64>,
        subscription: Subscription,
        cx: &mut HostCx,
    ) {
        if let Topic::SessionEvents { session_id } = &subscription.topic {
            match self.event_records.get(session_id) {
                Some(log) => self.reply_from_log(log, request_id, &subscription, cx),
                None => {
                    let session_id = session_id.clone();
                    self.hydrate_log(&session_id, None, cx)
                        .replies
                        .push((request_id, subscription));
                }
            }
            return;
        }
        if let Some(mut snapshot) = self.subscription_snapshot(&subscription) {
            snapshot.request_id = request_id;
            cx.emit(HostEvent::Domain(snapshot));
        }
        if let Some(id) = request_id {
            cx.send_message(HostMessage::Ack {
                id,
                result: Ok(CommandResponse::Unit),
            });
        }
    }

    /// Answer a query from the session's log: now when it is resident,
    /// otherwise once it has been read.
    fn query_session_log(
        &mut self,
        session_id: &str,
        query: impl FnOnce(&SessionLog) -> Result<QueryResponse, tcode_protocol::ProtocolError>
        + Send
        + 'static,
        cx: &mut HostCx,
    ) -> HostTask<Result<QueryResponse, tcode_protocol::ProtocolError>> {
        if let Some(log) = self.event_records.get(session_id) {
            let result = query(log);
            return cx.spawn_background(async move { result });
        }
        let (answer, answered) = smol::channel::bounded(1);
        self.hydrate_log(session_id, None, cx)
            .queries
            .push(Box::new(move |log| {
                let _ = answer.try_send(query(log));
            }));
        cx.spawn_background(async move {
            answered.recv().await.unwrap_or_else(|_| {
                Err(tcode_protocol::ProtocolError {
                    code: "host_stopped".into(),
                    message: "the host stopped before the session's log was read".into(),
                })
            })
        })
    }

    pub(crate) fn session_history_page(
        &mut self,
        session_id: &str,
        epoch: u64,
        before: u64,
        limit: u32,
        cx: &mut HostCx,
    ) -> HostTask<Result<QueryResponse, tcode_protocol::ProtocolError>> {
        self.query_session_log(
            session_id,
            move |log| log.history_page(epoch, before, limit),
            cx,
        )
    }

    pub(crate) fn item_output(
        &mut self,
        session_id: &str,
        item_id: String,
        cx: &mut HostCx,
    ) -> HostTask<Result<QueryResponse, tcode_protocol::ProtocolError>> {
        let read_id = session_id.to_string();
        self.query_session_log(
            session_id,
            move |log| log.item_output(&read_id, &item_id),
            cx,
        )
    }

    /// Queue a store-writer barrier for every cached log whose session left
    /// residency, and drop the log once the barrier echoes: a cold read after
    /// that sees every append the log had accepted. Appends that race the
    /// barrier re-arm it. The log is compacted first, so a turn that never
    /// completed is compacted too, and the next load serves that layout.
    pub(super) fn release_stale_session_logs(&mut self, cx: &mut HostCx) {
        let stale: Vec<(String, LogCursor)> = self
            .event_records
            .iter()
            .filter(|(id, log)| log.release_barrier.is_none() && self.resident(id).is_none())
            .map(|(id, log)| (id.clone(), log.end()))
            .collect();
        for (session_id, flushed) in stale {
            self.event_records
                .get_mut(&session_id)
                .expect("stale log collected above")
                .release_barrier = Some(flushed);
            self.schedule_compaction(&session_id, cx);
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
                    let appended_since = log.end() != flushed;
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
}

/// The bytes a reply spends besides its records, measured on `empty`, a reply
/// without records whose numbers are as long as any reply's.
fn wire_overhead(empty: &HostMessage) -> usize {
    serde_json::to_vec(empty).expect("serializable reply").len() + 1
}

/// The records from cursor `after` to the end, or without one a baseline: the
/// tail from a turn start about [`BASELINE_RECORDS`] records back, cut to the
/// byte budget from its newest end.
fn session_window(
    log: &SessionLog,
    after: Option<usize>,
    overhead: usize,
) -> Result<SessionWindow, tcode_protocol::ProtocolError> {
    let total = log.records.len();
    let from =
        after.unwrap_or_else(|| log.turn_aligned_start(total.saturating_sub(BASELINE_RECORDS)));
    let window = wire_window(&log.records, from..total, after.is_none(), overhead)?;
    Ok(SessionWindow {
        epoch: log.epoch,
        from: window.range.start as u64,
        end: window.range.end as u64,
        truncated: window.range.len() < total - from,
        records: window.records,
        total: total as u64,
        total_turns: log.fold.turns.len() as u64,
    })
}

/// How far back a baseline reaches before its turn start and byte budget.
const BASELINE_RECORDS: usize = 400;

/// A window without records whose numbers are as long as any window's.
fn empty_window() -> SessionWindow {
    SessionWindow {
        epoch: u64::MAX,
        from: u64::MAX,
        end: u64::MAX,
        records: vec![],
        total: u64::MAX,
        total_turns: u64::MAX,
        truncated: false,
    }
}
