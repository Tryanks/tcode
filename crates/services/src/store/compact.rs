//! Rows a thread's fold no longer needs: the diffs of a turn-changes snapshot
//! a later one supersedes, and the streamed deltas an item's completion
//! repeats. A snapshot row keeps its position without its diffs; superseded
//! deltas leave the log, except the one that created its item's entry, which
//! stays with its text emptied. Both are decided by the fold
//! ([`TurnSnapshots`], [`ItemStreams`]) and, for a whole thread, checked by
//! folding the thread with and without them.
//!
//! An append a resident fold followed drops what it supersedes in its own
//! transaction; every other thread is marked and passed in the background, a
//! bounded batch of rows per writer transaction, each change made only if the
//! row still holds the bytes it was decided on.

use std::collections::{BTreeSet, HashSet};
use std::io;

use agent::{AgentEvent, ItemContent};
use tcode_core::session::{
    ItemStreams, StoredEvent, SupersededStream, Timeline, TurnSnapshots, drop_turn_diffs,
};
use turso::Connection;

use super::codec::{self, Encoder, Mode};
use super::rows::{self, Storage, Stored, Thread};
use super::{EventEnvelopeRef, SessionStore, decode_row, invalid_data};

/// Rows a write batch of the pass changes at most.
const BATCH_ROWS: usize = 2048;

/// What the compaction pass did to one thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compaction {
    /// `deleted` rows left the log and `rewritten` rows lost their diffs or
    /// streamed text; together they went from `before` to `after` bytes.
    Compacted {
        deleted: usize,
        rewritten: usize,
        before: u64,
        after: u64,
    },
    Unchanged,
    /// The row at `position` holds no record this build reads, so the fold
    /// is unknown; no row was changed.
    Undecodable {
        position: u64,
    },
    /// The thread still has rows to move, or changed while it was read; it
    /// stays marked.
    Deferred,
}

/// Whether an item of this kind is the kind deltas stream into.
pub(super) fn streamed(content: &ItemContent) -> bool {
    matches!(
        content,
        ItemContent::AssistantMessage { .. }
            | ItemContent::Reasoning { .. }
            | ItemContent::Plan { .. }
            | ItemContent::CommandExecution { .. }
    )
}

/// What the fold of a thread decides about its rows.
#[derive(Debug, Default, Clone)]
pub(super) struct Decisions {
    pub delete: BTreeSet<u64>,
    pub empty: BTreeSet<u64>,
    pub drop_diffs: BTreeSet<u64>,
}

impl Decisions {
    pub(super) fn is_empty(&self) -> bool {
        self.delete.is_empty() && self.empty.is_empty() && self.drop_diffs.is_empty()
    }

    /// The canonical bytes the row at `position` is kept as, or `None` when
    /// it leaves the log.
    pub(super) fn apply(&self, position: u64, line: Vec<u8>) -> io::Result<Option<Vec<u8>>> {
        if self.delete.contains(&position) {
            return Ok(None);
        }
        if self.empty.contains(&position) {
            return Ok(Some(rewrite(&line, empty_delta)?.unwrap_or(line)));
        }
        if self.drop_diffs.contains(&position) {
            return Ok(Some(rewrite(&line, drop_turn_diffs)?.unwrap_or(line)));
        }
        Ok(Some(line))
    }
}

/// Folds a thread's rows in order to decide what [`Decisions`] it allows.
#[derive(Default)]
pub(super) struct Planner {
    fold: Timeline,
    snapshots: TurnSnapshots<u64>,
    streams: ItemStreams<u64>,
    records: Vec<(u64, StoredEvent)>,
    groups: Vec<SupersededStream<u64>>,
    /// Rows naming blobs: the bare form a decision is made on is not all
    /// they hold, so a stream with one keeps all of its deltas.
    referencing: HashSet<u64>,
    drop_diffs: BTreeSet<u64>,
    undecodable: Option<u64>,
}

pub(super) enum Plan {
    Decided(Decisions),
    Undecodable(u64),
}

impl Planner {
    /// Follow the row at `position`, given in its bare form.
    pub(super) fn push(&mut self, position: u64, bare: &[u8], references: bool) {
        if self.undecodable.is_some() {
            return;
        }
        if references {
            self.referencing.insert(position);
        }
        match decode_row(bare) {
            Ok(Some(record)) => {
                let stored = record.into_stored(None);
                self.drop_diffs.extend(
                    self.snapshots
                        .apply_at(&mut self.fold, stored.ts, &stored.event, position)
                        .1,
                );
                self.groups.extend(self.streams.follow(
                    &mut self.fold,
                    stored.ts,
                    &stored.event,
                    position,
                ));
                self.records.push((position, stored));
            }
            Ok(None) => {}
            Err(_) => self.undecodable = Some(position),
        }
    }

    /// The decisions, kept only if folding the thread with them applied
    /// gives the fold of the thread as it is.
    pub(super) fn finish(self) -> Plan {
        if let Some(position) = self.undecodable {
            return Plan::Undecodable(position);
        }
        let mut decisions = Decisions {
            drop_diffs: self.drop_diffs,
            ..Decisions::default()
        };
        for group in self.groups {
            if group
                .delete
                .iter()
                .chain(&group.empty)
                .any(|position| self.referencing.contains(position))
            {
                continue;
            }
            decisions.delete.extend(group.delete);
            decisions.empty.extend(group.empty);
        }
        if decisions.is_empty() {
            return Plan::Decided(decisions);
        }
        let mut reduced = Timeline::default();
        for (position, record) in &self.records {
            if decisions.delete.contains(position) {
                continue;
            }
            let mut record = record.clone();
            if decisions.empty.contains(position) {
                empty_delta(&mut record.event);
            }
            if decisions.drop_diffs.contains(position) {
                drop_turn_diffs(&mut record.event);
            }
            reduced.apply_stored(&record);
        }
        if reduced != self.fold {
            log::warn!(
                "a thread's fold changes without its superseded rows; keeping them (first row {:?})",
                self.records.first().map(|(position, _)| position)
            );
            return Plan::Decided(Decisions::default());
        }
        Plan::Decided(decisions)
    }
}

fn empty_delta(event: &mut AgentEvent) -> bool {
    match event {
        AgentEvent::Delta { text, .. } if !text.is_empty() => {
            text.clear();
            true
        }
        _ => false,
    }
}

/// `line` with its record changed by `change`, in the form it was stored in;
/// `None` when `change` changes nothing.
pub(super) fn rewrite(
    line: &[u8],
    change: impl FnOnce(&mut AgentEvent) -> bool,
) -> io::Result<Option<Vec<u8>>> {
    let Ok(Some(record)) = decode_row(line) else {
        return Ok(None);
    };
    let mut stored = record.into_stored(None);
    if !change(&mut stored.event) {
        return Ok(None);
    }
    let mut rewritten = match stored.ts {
        Some(ts) => serde_json::to_vec(&EventEnvelopeRef::new(
            ts,
            &stored.event,
            stored.author.as_ref(),
            stored.origin,
        )),
        None => serde_json::to_vec(&stored.event),
    }
    .map_err(invalid_data)?;
    if line.ends_with(b"\n") {
        rewritten.push(b'\n');
    }
    if !matches!(decode_row(&rewritten), Ok(Some(read)) if read.stored == stored) {
        return Err(invalid_data("a rewritten row does not read back as itself"));
    }
    Ok(Some(rewritten))
}

/// Drop the diffs of the snapshot at `position`, which a later one
/// supersedes. A row still in the source is replaced by its rewrite here.
pub(super) fn drop_row_diffs(
    st: &Storage,
    connection: &Connection,
    encoder: &mut Encoder<'_>,
    thread: &Thread,
    position: u64,
) -> io::Result<()> {
    let Some(row) = rows::get(st, connection, thread, position)? else {
        return Ok(());
    };
    let codec = st.codec();
    let decoded = st.decode(&mut codec.decoder(), &row, Mode::Full)?;
    if decoded.missing {
        return Ok(());
    }
    if let Some(rewritten) = rewrite(&decoded.bytes, drop_turn_diffs)? {
        rows::put(st, connection, encoder, thread, position, &rewritten)?;
    }
    Ok(())
}

/// Remove the deltas `superseded` names, all of them or none: each must be
/// a delta of `item` in this file that names no blob. Returns whether they
/// were removed.
pub(super) fn remove_superseded(
    st: &Storage,
    connection: &Connection,
    encoder: &mut Encoder<'_>,
    thread: &Thread,
    item: &str,
    superseded: &SupersededStream<u64>,
) -> io::Result<bool> {
    let codec = st.codec();
    let mut decoder = codec.decoder();
    let unmoved = thread.unmoved();
    let mut checked = Vec::new();
    for position in superseded.delete.iter().chain(&superseded.empty) {
        if unmoved.contains(position) {
            return Ok(false);
        }
        let Some(Stored::New(stored)) = rows::get(st, connection, thread, *position)? else {
            return Ok(false);
        };
        if !codec::header(&stored)?.refs.is_empty() {
            return Ok(false);
        }
        let line = decoder.bare(&stored)?;
        match decode_row(&line) {
            Ok(Some(record)) if matches!(&record.stored.event, AgentEvent::Delta { item_id, .. } if item_id == item) =>
                {}
            _ => return Ok(false),
        }
        checked.push((*position, stored, line));
    }
    for (position, stored, line) in checked {
        let key = thread.row(position)?;
        if superseded.empty == Some(position) {
            if let Some(empty) = rewrite(&line, empty_delta)? {
                let encoded = st.encode(encoder, &empty)?;
                rows::replace_if(st, connection, key, &stored, &encoded)?;
            }
        } else {
            rows::delete_if(st, connection, key, &stored)?;
        }
    }
    Ok(true)
}

/// A thread's compaction, decided on one read of it.
pub struct CompactionPlan {
    key: i64,
    id: String,
    end: u64,
    /// Each row to change: its key, stored bytes, and canonical replacement
    /// (`None` to delete it).
    changes: Vec<(i64, Vec<u8>, Option<Vec<u8>>)>,
    next: usize,
    undecodable: Option<u64>,
    outcome: Option<Compaction>,
    totals: (usize, usize, u64, u64),
}

impl SessionStore {
    /// Threads marked for the compaction pass.
    pub fn threads_to_compact(&self) -> io::Result<Vec<String>> {
        self.run("list threads to compact", |st| {
            st.read(|st, connection| {
                rows::thread_ids(
                    st,
                    connection,
                    "SELECT t.id FROM threads t JOIN sessions s ON s.id = t.id \
                     WHERE t.compacted = 0 ORDER BY t.key",
                )
            })
        })
    }

    /// Read and fold `id` to decide its compaction, without the writer.
    pub fn plan_compaction(&self, id: &str) -> io::Result<CompactionPlan> {
        self.run("plan a compaction", |st| {
            st.read(|st, connection| {
                st.execute(connection, "BEGIN", ())?;
                let plan = plan(st, connection, id);
                st.execute(connection, "ROLLBACK", ())?;
                plan
            })
        })
    }

    /// Commit the next batch of `plan`; the last one marks the thread done
    /// if it has not changed since it was read. Returns the outcome once the
    /// plan is finished.
    pub fn compact(&self, plan: &mut CompactionPlan) -> io::Result<Option<Compaction>> {
        if let Some(outcome) = plan.outcome.clone() {
            return Ok(Some(outcome));
        }
        let changed = self.run("compact a thread", |st| {
            st.write(|st, connection| {
                let codec = st.codec();
                let mut encoder = codec.encoder()?;
                let mut totals = (0, 0, 0, 0);
                let batch = plan.next..(plan.next + BATCH_ROWS).min(plan.changes.len());
                for (key, stored, replacement) in &plan.changes[batch.clone()] {
                    match replacement {
                        None => {
                            if rows::delete_if(st, connection, *key, stored)? {
                                totals.0 += 1;
                                totals.2 += stored.len() as u64;
                            }
                        }
                        Some(canonical) => {
                            let encoded = st.encode(&mut encoder, canonical)?;
                            if rows::replace_if(st, connection, *key, stored, &encoded)? {
                                totals.1 += 1;
                                totals.2 += stored.len() as u64;
                                totals.3 += encoded.bytes.len() as u64;
                            }
                        }
                    }
                }
                let last = batch.end == plan.changes.len();
                let mut done = false;
                if last
                    && let Some(thread) = rows::thread(st, connection, &plan.id)?
                    && thread.key == plan.key
                    && rows::end(st, connection, &thread)? == plan.end
                {
                    st.execute(
                        connection,
                        "UPDATE threads SET compacted = 1 WHERE key = ?1",
                        [plan.key],
                    )?;
                    done = true;
                }
                Ok((batch.end, totals, last, done))
            })
        });
        let (next, totals, last, done) = changed?;
        plan.next = next;
        plan.totals.0 += totals.0;
        plan.totals.1 += totals.1;
        plan.totals.2 += totals.2;
        plan.totals.3 += totals.3;
        if totals.0 + totals.1 > 0 {
            self.advance_event_generations([plan.id.as_str()]);
        }
        if !last {
            return Ok(None);
        }
        let (deleted, rewritten, before, after) = plan.totals;
        let outcome = if !done {
            Compaction::Deferred
        } else if let Some(position) = plan.undecodable {
            Compaction::Undecodable { position }
        } else if deleted + rewritten == 0 {
            Compaction::Unchanged
        } else {
            Compaction::Compacted {
                deleted,
                rewritten,
                before,
                after,
            }
        };
        plan.outcome = Some(outcome.clone());
        Ok(Some(outcome))
    }
}

fn plan(st: &Storage, connection: &Connection, id: &str) -> io::Result<CompactionPlan> {
    let mut plan = CompactionPlan {
        key: 0,
        id: id.to_owned(),
        end: 0,
        changes: Vec::new(),
        next: 0,
        undecodable: None,
        outcome: None,
        totals: (0, 0, 0, 0),
    };
    let Some(thread) = rows::thread(st, connection, id)? else {
        plan.outcome = Some(Compaction::Unchanged);
        return Ok(plan);
    };
    plan.key = thread.key;
    plan.end = rows::end(st, connection, &thread)?;
    if !thread.unmoved().is_empty() {
        plan.outcome = Some(Compaction::Deferred);
        return Ok(plan);
    }
    let codec = st.codec();
    let mut decoder = codec.decoder();
    let mut planner = Planner::default();
    rows::visit(
        st,
        connection,
        &thread,
        0..plan.end,
        false,
        |position, row| {
            let Stored::New(stored) = &row else {
                return Ok(true);
            };
            let references = !codec::header(stored)?.refs.is_empty();
            planner.push(position, &decoder.bare(stored)?, references);
            Ok(true)
        },
    )?;
    let decisions = match planner.finish() {
        Plan::Undecodable(position) => {
            plan.undecodable = Some(position);
            return Ok(plan);
        }
        Plan::Decided(decisions) => decisions,
    };
    for position in decisions
        .delete
        .iter()
        .chain(&decisions.empty)
        .chain(&decisions.drop_diffs)
        .copied()
        .collect::<BTreeSet<_>>()
    {
        let Some(Stored::New(stored)) = rows::get(st, connection, &thread, position)? else {
            continue;
        };
        let key = thread.row(position)?;
        if decisions.delete.contains(&position) {
            plan.changes.push((key, stored, None));
            continue;
        }
        let decoded = st.decode(&mut decoder, &Stored::New(stored.clone()), Mode::Full)?;
        if decoded.missing {
            continue;
        }
        if let Some(line) = decisions.apply(position, decoded.bytes.clone())?
            && line != decoded.bytes
        {
            plan.changes.push((key, stored, Some(line)));
        }
    }
    Ok(plan)
}
