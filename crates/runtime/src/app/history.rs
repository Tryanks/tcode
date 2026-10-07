use super::*;
use agent::DeltaKind;
use std::borrow::Cow;
use std::ops::Range;
use tcode_core::session::{TurnSnapshots, drop_turn_diffs};
use tcode_protocol::{
    CommandResponse, HostMessage, MAX_SESSION_HISTORY_BYTES, OUTPUT_PREVIEW_BYTES,
    SESSION_HISTORY_RECORDS, SESSION_WINDOW_BYTES, Subscription,
};
use tcode_services::store::{EventLog, TurnIndex};

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
                output,
                image_reads,
                ..
            } => {
                image_reads
                    .iter()
                    .any(|image| !image.data_base64.is_empty())
                    || output
                        .as_ref()
                        .is_some_and(|output| output.len() > OUTPUT_PREVIEW_BYTES)
            }
            ItemContent::ImageRead {
                image: Some(image), ..
            } => !image.data_base64.is_empty(),
            ItemContent::CommandExecution { output, .. } => output.len() > OUTPUT_PREVIEW_BYTES,
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
                output,
                image_reads,
                ..
            } => {
                let full = output.as_ref().map(|output| output.len() as u64);
                let mut removed = false;
                for image in image_reads {
                    if !image.data_base64.is_empty() {
                        if let Some(output) = output {
                            let preview = output.replace(&image.data_base64, "[image]");
                            removed |= preview.len() != output.len();
                            *output = preview;
                        }
                        image.data_base64.clear();
                    }
                }
                let shortened = output.as_mut().and_then(|output| shorten(output, false));
                if removed || shortened.is_some() {
                    full
                } else {
                    None
                }
            }
            ItemContent::ImageRead {
                image: Some(image), ..
            } => {
                image.data_base64.clear();
                None
            }
            ItemContent::CommandExecution { output, .. } => shorten(output, true),
            _ => None,
        },
        AgentEvent::Delta { text, .. } => shorten(text, true),
        _ => None,
    };
    Cow::Owned(record)
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
        let size = wire_len(&wire_record(&records[index])) + 1;
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
        merged.push(record.clone());
    }
    let records = merged
        .iter()
        .map(|record| wire_record(record).into_owned())
        .collect();
    Ok(Window { range, records })
}

/// A stored turn-changes snapshot that an append supersedes.
pub(super) struct SupersededRow {
    pub(super) position: u64,
    pub(super) turn_id: String,
}

/// What an appended record does to the session's other stored rows, as the
/// fold of the log it joins tells.
pub(super) enum Joined {
    /// It joined a resident log.
    Folded {
        superseded: Option<SupersededRow>,
        /// The turn index after it, when it changed the turns.
        turn_index: Option<TurnIndex>,
    },
    /// No log was resident to fold it into, so neither is known.
    Unfolded,
}

/// What a window is cut from: the records of the rows before `end` of a log,
/// all of them or only its last ones, with the turns of the whole log.
///
/// Cursors are row positions. A blank or undecodable row holds no record, so
/// a range of cursors can hold fewer records than it spans.
struct LogView<'a> {
    records: &'a [SessionEventRecord],
    /// The row each record is in, ascending.
    rows: &'a [u64],
    end: u64,
    turn_starts: &'a [u64],
    turns: u64,
}

impl LogView<'_> {
    /// Move a window start back to the row that opens its turn. The client
    /// folds only the records it holds, so a window that begins mid-turn
    /// renders a partial first turn whose entries shift as earlier pages
    /// arrive, while a window that begins where a turn begins opens its turns
    /// exactly where the full log does. Rows that open no turn leave the
    /// start unchanged.
    fn turn_aligned_start(&self, start: u64) -> u64 {
        let opened_before = self.turn_starts.partition_point(|&row| row <= start);
        opened_before
            .checked_sub(1)
            .map_or(start, |last| self.turn_starts[last])
    }

    /// The index of the first record at or after row `position`.
    fn index(&self, position: u64) -> usize {
        self.rows.partition_point(|row| *row < position)
    }

    /// The row a window whose first record is `index` starts at, when it
    /// was asked to start at `requested`, the row of record `first`.
    fn start(&self, index: usize, first: usize, requested: u64) -> u64 {
        if index == first {
            requested
        } else {
            self.rows[index]
        }
    }

    /// The window answering a subscription: the records from its cursor on,
    /// or without one a baseline: the tail from a turn start about
    /// [`BASELINE_RECORDS`] rows back, cut to the byte budget from its newest
    /// end.
    fn events_window(&self, subscription: &Subscription) -> ServerEvent {
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
        let total = self.end;
        let after = subscription.after.filter(|after| *after <= total);
        let from = after
            .unwrap_or_else(|| self.turn_aligned_start(total.saturating_sub(BASELINE_RECORDS)));
        let first = self.index(from);
        match wire_window(
            self.records,
            first..self.records.len(),
            after.is_none(),
            wire_overhead(&empty),
        ) {
            Ok(window) => {
                let start = self.start(window.range.start, first, from);
                let end = if window.range.end == self.records.len() {
                    total
                } else {
                    self.rows[window.range.end]
                };
                ServerEvent::SessionSnapshot {
                    from: start,
                    end,
                    truncated: start > from || end < total,
                    records: window.records,
                    total,
                    total_turns: self.turns,
                }
            }
            Err(error) => ServerEvent::SessionHistoryError(error),
        }
    }

    fn history_page(
        &self,
        before: u64,
        limit: u32,
    ) -> Result<QueryResponse, tcode_protocol::ProtocolError> {
        let end = before.min(self.end);
        let count = (limit as u64).clamp(1, SESSION_HISTORY_RECORDS as u64);
        let start = self.turn_aligned_start(end.saturating_sub(count));
        let requested = self.index(start)..self.index(end);
        let empty = HostMessage::QueryResult {
            id: u64::MAX,
            result: Ok(QueryResponse::SessionHistoryPage {
                records: vec![],
                from: u64::MAX,
                end: u64::MAX,
                truncated: false,
            }),
        };
        let window = wire_window(self.records, requested.clone(), true, wire_overhead(&empty))?;
        Ok(QueryResponse::SessionHistoryPage {
            from: self.start(window.range.start, requested.start, start),
            end,
            truncated: window.range.len() < requested.len(),
            records: window.records,
        })
    }
}

/// The bytes a reply spends besides its records, measured on `empty`, a reply
/// without records whose numbers are as long as any reply's.
fn wire_overhead(empty: &HostMessage) -> usize {
    serde_json::to_vec(empty).expect("serializable reply").len() + 1
}

/// How far back a baseline reaches before its turn start and byte budget.
const BASELINE_RECORDS: u64 = 400;

/// The complete event log of one session, held in memory while the session
/// is resident so history windows cost the page rather than a re-read of the
/// stored log, plus the fold that decides where each window may start.
///
/// Memory policy: [`AppState::event_records`] holds a log for every live or
/// parked session (bounded by the resident LRU) and for nothing else. A log
/// is read off the mailbox ([`Hydration`]) when a client opens the session or
/// when the session first appends in this process, and is dropped once the
/// session leaves residency and the store writer has flushed every append
/// queued from it ([`AppState::release_stale_session_logs`]): until then the
/// log, not the store, is the whole conversation.
///
/// A turn-changes snapshot that a later one supersedes is held without its
/// diffs, as the store keeps it, so no window sends them.
#[derive(Clone)]
pub(super) struct SessionLog {
    records: Vec<SessionEventRecord>,
    /// The stored row each record is in, ascending.
    rows: Vec<u64>,
    /// The rows of the records that opened a turn in `fold`, ascending.
    turn_starts: Vec<u64>,
    /// `Timeline::fold_events(records)`, extended by every push so an append
    /// can tell whether it opens a turn, and cloned by timeline loads instead
    /// of folding the records again.
    fold: Timeline,
    /// The snapshot each turn of `fold` holds, as its record index and the
    /// stored row it is in.
    snapshots: TurnSnapshots<(usize, u64)>,
    /// The stored row the next append takes.
    next_row: u64,
    /// Every stored row decoded when the log was read, so `fold` names the
    /// same superseded snapshots as the store's own pass would.
    decoded: bool,
    /// The end flushed by the release barrier in flight, if any.
    release_barrier: Option<u64>,
}

impl SessionLog {
    pub(super) fn new(log: EventLog) -> Self {
        let mut session_log = Self {
            records: Vec::new(),
            rows: Vec::new(),
            turn_starts: Vec::new(),
            fold: Timeline::default(),
            snapshots: TurnSnapshots::default(),
            next_row: log.next_row,
            decoded: log.undecodable == 0,
            release_barrier: None,
        };
        for (record, row) in log.records.into_iter().zip(log.rows) {
            session_log.push_at(record, row);
        }
        session_log
    }

    /// Append a record, returning what the store should change besides
    /// appending it.
    pub(super) fn push(&mut self, record: SessionEventRecord) -> Joined {
        let (turns, starts) = (self.fold.turns.len(), self.turn_starts.len());
        let row = self.next_row;
        self.next_row += 1;
        let superseded =
            self.push_at(record, row)
                .filter(|_| self.decoded)
                .map(|(index, position)| {
                    let AgentEvent::TurnChangesUpdated { turn_id, .. } = &self.records[index].event
                    else {
                        unreachable!("only snapshots are superseded")
                    };
                    SupersededRow {
                        position,
                        turn_id: turn_id.clone(),
                    }
                });
        let turn_index = (self.fold.turns.len() != turns || self.turn_starts.len() != starts)
            .then(|| self.turn_index());
        Joined::Folded {
            superseded,
            turn_index,
        }
    }

    fn push_at(&mut self, record: SessionEventRecord, row: u64) -> Option<(usize, u64)> {
        let turns = self.fold.turns.len();
        let index = self.records.len();
        let superseded =
            self.snapshots
                .apply_at(&mut self.fold, record.ts, &record.event, (index, row));
        if self.fold.turns.len() > turns {
            self.turn_starts.push(row);
        }
        self.records.push(record);
        self.rows.push(row);
        if let Some((earlier, _)) = superseded {
            drop_turn_diffs(&mut self.records[earlier].event);
        }
        superseded
    }

    pub(super) fn records(&self) -> &[SessionEventRecord] {
        &self.records
    }

    /// The records in rows from `position` on.
    pub(super) fn records_from(&self, position: u64) -> &[SessionEventRecord] {
        &self.records[self.rows.partition_point(|row| *row < position)..]
    }

    /// The pure fold of every record, before any `mark_idle`.
    pub(super) fn fold(&self) -> &Timeline {
        &self.fold
    }

    /// The row just past the last record: the log's end cursor.
    pub(super) fn end(&self) -> u64 {
        self.next_row
    }

    pub(super) fn turn_index(&self) -> TurnIndex {
        TurnIndex {
            turns: self.fold.turns.len() as u64,
            starts: self.turn_starts.clone(),
        }
    }

    fn view(&self) -> LogView<'_> {
        LogView {
            records: &self.records,
            rows: &self.rows,
            end: self.next_row,
            turn_starts: &self.turn_starts,
            turns: self.fold.turns.len() as u64,
        }
    }

    pub(super) fn events_window(&self, subscription: &Subscription) -> ServerEvent {
        self.view().events_window(subscription)
    }

    pub(super) fn history_page(
        &self,
        before: u64,
        limit: u32,
    ) -> Result<QueryResponse, tcode_protocol::ProtocolError> {
        self.view().history_page(before, limit)
    }

    pub(super) fn item_image(
        &self,
        item_id: &str,
        image_index: usize,
    ) -> Result<QueryResponse, ProtocolError> {
        use base64::Engine as _;
        let image = self
            .fold
            .entries
            .iter()
            .rev()
            .find(|entry| entry.id == item_id)
            .and_then(|entry| match &entry.content {
                EntryContent::Item(ItemContent::ToolCall { image_reads, .. }) => {
                    image_reads.get(image_index)
                }
                EntryContent::Item(ItemContent::ImageRead { image, .. }) if image_index == 0 => {
                    image.as_ref()
                }
                _ => None,
            })
            .ok_or_else(|| ProtocolError::out_of_scope("image is outside this item"))?;
        if image.data_base64.len() > MAX_SESSION_HISTORY_BYTES {
            return Err(ProtocolError {
                code: "item_image_too_large".into(),
                message: "The image exceeds the response limit.".into(),
            });
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&image.data_base64)
            .map_err(|_| ProtocolError {
                code: "invalid_image".into(),
                message: "Invalid image encoding.".into(),
            })?;
        Ok(QueryResponse::FileBytes(bytes))
    }

    /// The whole output of one item, as the full log folds it.
    pub(super) fn item_output(
        &self,
        session_id: &str,
        item_id: &str,
    ) -> Result<QueryResponse, tcode_protocol::ProtocolError> {
        let output = self
            .fold
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

/// The last rows of a log, as many as a baseline window can use, read with
/// the turn index of the whole log so the window is the one the whole log
/// would give.
pub(super) struct Tail {
    records: Vec<SessionEventRecord>,
    rows: Vec<u64>,
    end: u64,
    index: TurnIndex,
}

/// Rows read per step while a tail is read backwards.
const TAIL_ROWS: u64 = 256;

impl Tail {
    /// Read the rows before `end` backwards from it until they reach the
    /// baseline's turn start or hold more than a window's byte budget, past
    /// which no baseline reaches.
    fn read(
        store: &SessionStore,
        session_id: &str,
        end: u64,
        index: TurnIndex,
    ) -> std::io::Result<Self> {
        let start = LogView {
            records: &[],
            rows: &[],
            end,
            turn_starts: &index.starts,
            turns: index.turns,
        }
        .turn_aligned_start(end.saturating_sub(BASELINE_RECORDS));
        let mut tail = Self {
            records: Vec::new(),
            rows: Vec::new(),
            end,
            index,
        };
        let (mut low, mut bytes) = (end, 0);
        while low > start && bytes <= SESSION_WINDOW_BYTES {
            let from = low.saturating_sub(TAIL_ROWS).max(start);
            let read = store.read_rows(session_id, from..low)?;
            bytes += read
                .records
                .iter()
                .map(|record| wire_len(&wire_record(record)) + 1)
                .sum::<usize>();
            tail.records.splice(0..0, read.records);
            tail.rows.splice(0..0, read.rows);
            low = from;
        }
        Ok(tail)
    }

    fn view(&self) -> LogView<'_> {
        LogView {
            records: &self.records,
            rows: &self.rows,
            end: self.end,
            turn_starts: &self.index.starts,
            turns: self.index.turns,
        }
    }
}

/// A session's log being read and folded off the mailbox, and what waits for
/// it. A session has at most one, and none while its log is resident.
///
/// The read covers the rows before the position the store writer found next
/// in its queue order, so it holds every record whose append was queued
/// before the hydration began and none queued after. Every record accepted
/// for the session from then on is held in `pending`, in order, and takes the
/// rows after that end.
///
/// A long log with a turn index first has its tail read, and a baseline
/// window cut from it answers the subscriptions waiting then and made later,
/// while nothing is pending. Once one has, the log is `live`: records it
/// accepts go out as they arrive, and a subscription that has to wait gets a
/// window that ends after them.
pub(super) struct Hydration {
    tail: Option<Tail>,
    live: bool,
    pending: Vec<SessionEventRecord>,
    /// Subscription replies; one without a request id answers no request.
    replies: Vec<(Option<u64>, Subscription)>,
    /// Queries answered from the whole log.
    queries: Vec<LogQuery>,
    /// The timeline load waiting for the log, with how many `pending` records
    /// had been accepted when it was requested.
    timeline: Option<(TimelineLoad, usize)>,
}

type LogQuery = Box<dyn FnOnce(Result<&SessionLog, &str>) + Send>;

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
                    end: snapshot,
                },
                cx,
            );
            let store = self.store.clone();
            let read_id = session_id.to_string();
            let host_cx = cx.clone();
            HostCx::spawn_detached(cx, async move {
                let end = match taken.recv().await {
                    Ok(Ok(end)) => end,
                    Ok(Err(error)) => {
                        host_cx.enqueue(move |state, cx| {
                            state.finish_hydration(read_id, Err(error), cx)
                        });
                        return;
                    }
                    Err(_) => {
                        host_cx.enqueue(move |state, cx| {
                            state.finish_hydration(
                                read_id,
                                Err("the session store writer has stopped".into()),
                                cx,
                            )
                        });
                        return;
                    }
                };
                let tail = {
                    let store = store.clone();
                    let read_id = read_id.clone();
                    host_cx
                        .unblock(move || {
                            if end <= BASELINE_RECORDS {
                                return None;
                            }
                            let index = store.turn_index(&read_id).ok().flatten()?;
                            Tail::read(&store, &read_id, end, index)
                                .inspect_err(|error| {
                                    log::warn!("could not read the tail of {read_id}: {error}")
                                })
                                .ok()
                        })
                        .await
                };
                if let Some(tail) = tail {
                    let tail_id = read_id.clone();
                    host_cx.enqueue(move |state, cx| state.serve_tail(&tail_id, tail, cx));
                }
                let log = {
                    let read_id = read_id.clone();
                    host_cx
                        .unblock(move || {
                            store
                                .read_log_until(&read_id, end)
                                .map(SessionLog::new)
                                .map_err(|error| error.to_string())
                        })
                        .await
                };
                host_cx.enqueue(move |state, cx| state.finish_hydration(read_id, log, cx));
            });
            self.log_hydrations.insert(
                session_id.to_string(),
                Hydration {
                    tail: None,
                    live: false,
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

    /// Answer the baseline subscriptions waiting for the log from its tail,
    /// unless a record arrived since the read began: the tail ends before it.
    fn serve_tail(&mut self, session_id: &str, tail: Tail, cx: &mut HostCx) {
        let Some(hydration) = self.log_hydrations.get_mut(session_id) else {
            return;
        };
        if !hydration.pending.is_empty() {
            return;
        }
        let (baselines, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut hydration.replies)
            .into_iter()
            .partition(|(_, subscription)| subscription.after.is_none());
        hydration.replies = waiting;
        hydration.live |= !baselines.is_empty();
        let view = tail.view();
        for (request_id, subscription) in &baselines {
            self.reply_with(
                view.events_window(subscription),
                *request_id,
                subscription,
                cx,
            );
        }
        if let Some(hydration) = self.log_hydrations.get_mut(session_id) {
            hydration.tail = Some(tail);
        }
    }

    /// Serve everything that waited for the log, in the order of its
    /// records, and keep the log only if its session is resident.
    fn finish_hydration(
        &mut self,
        session_id: String,
        log: Result<SessionLog, String>,
        cx: &mut HostCx,
    ) {
        let Some(hydration) = self.log_hydrations.remove(&session_id) else {
            return;
        };
        let mut log = match log {
            Ok(log) => log,
            Err(error) => {
                log::error!("could not read the log of {session_id}: {error}");
                let failure = ServerEvent::SessionHistoryError(tcode_protocol::ProtocolError {
                    code: "history_unavailable".into(),
                    message: format!("could not read the history of {session_id}: {error}"),
                });
                for (request_id, subscription) in &hydration.replies {
                    self.reply_with(failure.clone(), *request_id, subscription, cx);
                }
                for query in hydration.queries {
                    query(Err(&error));
                }
                if hydration.timeline.is_some() {
                    self.report_error(
                        RuntimeError::External(format!(
                            "could not load thread {session_id}: {error}"
                        )),
                        cx,
                    );
                }
                return;
            }
        };
        let retained = self.resident(&session_id).is_some();
        let topic = Topic::SessionEvents {
            session_id: session_id.clone(),
        };
        // Records a live log accepted are out already: a waiting subscription
        // gets a window that holds them. Otherwise they follow the windows.
        if !hydration.live {
            self.reply_from_log(&log, &hydration.replies, cx);
        }
        let timeline = hydration.timeline.filter(|_| retained);
        let fold_at = timeline.as_ref().map(|(_, accepted)| *accepted);
        let mut folded = None;
        for (index, record) in hydration.pending.into_iter().enumerate() {
            if fold_at == Some(index) {
                folded = Some((log.end(), log.fold().clone()));
            }
            if !hydration.live {
                self.emit_domain(
                    topic.clone(),
                    ServerEvent::SessionEvent(wire_record(&record).into_owned()),
                    cx,
                );
            }
            log.push(record);
        }
        if hydration.live {
            self.reply_from_log(&log, &hydration.replies, cx);
        }
        for query in hydration.queries {
            query(Ok(&log));
        }
        // Queued after every append the log holds, so it covers all of them.
        self.enqueue_store_write(
            StoreWrite::SetTurnIndex {
                id: session_id.clone(),
                index: log.turn_index(),
            },
            cx,
        );
        if !retained {
            return;
        }
        let folded = timeline.map(|(load, _)| {
            let (cursor, fold) = folded.unwrap_or_else(|| (log.end(), log.fold().clone()));
            (load, cursor, fold)
        });
        self.event_records.insert(session_id.clone(), log);
        if let Some((load, cursor, fold)) = folded {
            self.fold_timeline(session_id, load, cursor, fold, cx);
        }
    }

    fn reply_from_log(
        &self,
        log: &SessionLog,
        replies: &[(Option<u64>, Subscription)],
        cx: &mut HostCx,
    ) {
        for (request_id, subscription) in replies {
            self.reply_with(
                log.events_window(subscription),
                *request_id,
                subscription,
                cx,
            );
        }
    }

    /// Send `window` as the reply to a subscription unless nobody is
    /// subscribed any more, and acknowledge the request.
    fn reply_with(
        &self,
        window: ServerEvent,
        request_id: Option<u64>,
        subscription: &Subscription,
        cx: &mut HostCx,
    ) {
        if self.subscriptions.contains(&subscription.topic) {
            cx.emit(HostEvent::Domain(EventEnvelope {
                request_id,
                topic: subscription.topic.clone(),
                event: window,
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
    /// resident, from its tail when a baseline can be and nothing arrived
    /// since the tail was read, otherwise once the log has been read; the
    /// acknowledgement waits with it, so the window is the request's first
    /// reply.
    pub(crate) fn reply_to_subscription(
        &mut self,
        request_id: Option<u64>,
        subscription: Subscription,
        cx: &mut HostCx,
    ) {
        if let Topic::SessionEvents { session_id } = &subscription.topic {
            if let Some(log) = self.event_records.get(session_id) {
                self.reply_with(
                    log.events_window(&subscription),
                    request_id,
                    &subscription,
                    cx,
                );
                return;
            }
            let session_id = session_id.clone();
            let hydration = self.hydrate_log(&session_id, None, cx);
            let window = hydration
                .tail
                .as_ref()
                .filter(|_| hydration.pending.is_empty() && subscription.after.is_none())
                .map(|tail| tail.view().events_window(&subscription));
            match window {
                Some(window) => {
                    hydration.live = true;
                    self.reply_with(window, request_id, &subscription, cx);
                }
                None => hydration.replies.push((request_id, subscription)),
            }
            return;
        }
        if let Some(mut snapshot) = self.subscription_snapshot(&subscription, &cx.principal) {
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

    /// Accept a record for a session whose log is not resident. It is held
    /// while the log is read, since its row comes after whatever the read
    /// holds, and sent at once when the log is already live.
    pub(super) fn hold_record(
        &mut self,
        session_id: &str,
        record: SessionEventRecord,
        cx: &mut HostCx,
    ) {
        let hydration = self.hydrate_log(session_id, None, cx);
        let live = hydration.live;
        let wire = live.then(|| wire_record(&record).into_owned());
        hydration.pending.push(record);
        if let Some(wire) = wire {
            self.emit_domain(
                Topic::SessionEvents {
                    session_id: session_id.to_string(),
                },
                ServerEvent::SessionEvent(wire),
                cx,
            );
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
        let read_id = session_id.to_string();
        self.hydrate_log(session_id, None, cx)
            .queries
            .push(Box::new(move |log| {
                let _ = answer.try_send(match log {
                    Ok(log) => query(log),
                    Err(error) => Err(tcode_protocol::ProtocolError {
                        code: "history_unavailable".into(),
                        message: format!("could not read the history of {read_id}: {error}"),
                    }),
                });
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
        before: u64,
        limit: u32,
        cx: &mut HostCx,
    ) -> HostTask<Result<QueryResponse, tcode_protocol::ProtocolError>> {
        self.query_session_log(session_id, move |log| log.history_page(before, limit), cx)
    }

    pub(crate) fn item_image(
        &mut self,
        session_id: &str,
        item_id: String,
        image_index: usize,
        cx: &mut HostCx,
    ) -> HostTask<Result<QueryResponse, ProtocolError>> {
        self.query_session_log(
            session_id,
            move |log| log.item_image(&item_id, image_index),
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
        let scoped = matches!(cx.principal, tcode_protocol::Principal::Space { .. });
        self.query_session_log(
            session_id,
            move |log| {
                log.item_output(&read_id, &item_id).map_err(|error| {
                    if scoped && error.code == "unknown_item_output" {
                        ProtocolError::out_of_scope("item is outside this session")
                    } else {
                        error
                    }
                })
            },
            cx,
        )
    }

    /// Queue a store-writer barrier for every cached log whose session left
    /// residency, and drop the log once the barrier confirms every earlier
    /// write committed: a cold read after that sees every append the log had
    /// accepted. Appends that race the barrier re-arm it. A barrier that
    /// reports a failed write, or a writer that is gone, keeps the log: it is
    /// then the only complete copy of the conversation.
    pub(super) fn release_stale_session_logs(&mut self, cx: &mut HostCx) {
        let stale: Vec<(String, u64)> = self
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
            let (completion, completed) = smol::channel::bounded(1);
            self.enqueue_store_write(StoreWrite::Flush(completion), cx);
            let host_cx = cx.clone();
            HostCx::spawn_detached(cx, async move {
                let flushed_ok = match completed.recv().await {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(error)) => Err(error),
                    Err(_) => Err("the session store writer has stopped".to_owned()),
                };
                host_cx.enqueue(move |state, cx| {
                    let Some(log) = state.event_records.get_mut(&session_id) else {
                        return;
                    };
                    if let Err(error) = flushed_ok {
                        // The barrier stays armed, so the log is never offered
                        // for release again.
                        log::warn!("keeping the in-memory history of {session_id}: {error}");
                        return;
                    }
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
