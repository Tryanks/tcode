use std::{collections::HashMap, io};

use serde::{Deserialize, Serialize};
use tcode_core::session::{
    StoredEvent, TURN_INDEX_FOLD_VERSION, TurnBoundaries, TurnSnapshots, TurnStarts,
};
use turso::Connection;

use super::codec::{Encoder, Mode};
use super::db::{Db, blob, integer};
use super::rows::{self, Storage, Thread};
use super::{SessionStore, decode_row, invalid_data, read_row};

pub(super) fn create_tables(db: &Db, connection: &Connection) -> io::Result<()> {
    db.execute(
        connection,
        "CREATE TABLE IF NOT EXISTS turn_index (\
        thread INTEGER PRIMARY KEY, fold_version INTEGER NOT NULL, \
        turns INTEGER NOT NULL, starts BLOB NOT NULL, state BLOB NOT NULL)",
        (),
    )?;
    Ok(())
}

/// Turn count after replay, and the rows that opened the turns it holds
/// ([`TurnStarts`]): a rewind drops the starts of the turns it removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnIndex {
    pub turns: u64,
    pub starts: Vec<u64>,
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    boundaries: TurnBoundaries,
    snapshots: TurnSnapshots<u64>,
    starts: TurnStarts,
    undecodable: bool,
}

#[derive(Default)]
pub(super) struct Pending {
    pub index: TurnIndex,
    state: State,
    dirty: bool,
}

/// Where a partial copy of a log ends, by the row that opened a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloneBound {
    /// Before that row.
    BeforeTurn(usize),
    /// After it, for a turn a rewind's divider opened: the rows before the
    /// divider hold what the rewind took out.
    ThroughTurnOpening(usize),
}

impl SessionStore {
    pub fn turn_index(&self, id: &str) -> io::Result<Option<TurnIndex>> {
        self.run("read a turn index", |st| {
            st.read(|st, connection| match rows::thread(st, connection, id)? {
                Some(thread) => read(st, connection, &thread),
                None => Ok(None),
            })
        })
    }

    pub fn threads_without_turn_index(&self) -> io::Result<Vec<String>> {
        self.run("list threads without a turn index", |st| {
            st.read(|st, connection| {
                let mut ids = Vec::new();
                st.query(
                    connection,
                    "SELECT id FROM sessions WHERE id NOT IN \
                    (SELECT t.id FROM threads t JOIN turn_index i ON i.thread = t.key \
                     WHERE i.fold_version = ?1)",
                    [i64::from(TURN_INDEX_FOLD_VERSION)],
                    |row| {
                        ids.push(super::db::text(row, 0)?);
                        Ok(())
                    },
                )?;
                Ok(ids)
            })
        })
    }
}

pub(super) fn read(
    st: &Storage,
    connection: &Connection,
    thread: &Thread,
) -> io::Result<Option<TurnIndex>> {
    let mut index = None;
    st.query(
        connection,
        "SELECT turns, starts FROM turn_index WHERE thread = ?1 AND fold_version = ?2",
        (thread.key, i64::from(TURN_INDEX_FOLD_VERSION)),
        |row| {
            index = Some(TurnIndex {
                turns: integer(row, 0)? as u64,
                starts: serde_json::from_slice(&blob(row, 1)?).map_err(invalid_data)?,
            });
            Ok(())
        },
    )?;
    Ok(index)
}

fn load(st: &Storage, connection: &Connection, key: i64) -> io::Result<Option<Pending>> {
    let mut pending = None;
    st.query(
        connection,
        "SELECT turns, starts, state FROM turn_index WHERE thread = ?1 AND fold_version = ?2",
        (key, i64::from(TURN_INDEX_FOLD_VERSION)),
        |row| {
            pending = Some(Pending {
                index: TurnIndex {
                    turns: integer(row, 0)? as u64,
                    starts: serde_json::from_slice(&blob(row, 1)?).map_err(invalid_data)?,
                },
                state: serde_json::from_slice(&blob(row, 2)?).map_err(invalid_data)?,
                dirty: false,
            });
            Ok(())
        },
    )?;
    Ok(pending)
}

pub(super) fn save(
    st: &Storage,
    connection: &Connection,
    key: i64,
    pending: &Pending,
) -> io::Result<()> {
    st.execute(connection, "INSERT INTO turn_index (thread, fold_version, turns, starts, state) \
        VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (thread) DO UPDATE SET \
        fold_version = excluded.fold_version, turns = excluded.turns, starts = excluded.starts, state = excluded.state",
        (key, i64::from(TURN_INDEX_FOLD_VERSION), pending.index.turns as i64,
         serde_json::to_vec(&pending.index.starts).map_err(invalid_data)?,
         serde_json::to_vec(&pending.state).map_err(invalid_data)?))?;
    Ok(())
}

impl Pending {
    fn apply(&mut self, position: u64, record: &StoredEvent) -> Option<u64> {
        let (change, superseded) = self.state.snapshots.apply_boundaries(
            &mut self.state.boundaries,
            &record.event,
            position,
        );
        if change.opened || change.rewind.is_some() || change.undone {
            self.state.starts.apply(&change, position);
            self.index.starts = self.state.starts.rows().to_vec();
        }
        self.index.turns = self.state.boundaries.len() as u64;
        self.dirty |= change.changed;
        superseded.filter(|_| !self.state.undecodable)
    }
}

/// Costs a fold of the whole log: the backfill calls it under its read
/// snapshot, and only a user's partial copy under the writer lock.
pub(super) fn candidate(
    st: &Storage,
    connection: &Connection,
    thread: &Thread,
) -> io::Result<Option<Pending>> {
    if read(st, connection, thread)?.is_some() {
        return Ok(None);
    }
    let mut pending = Pending::default();
    let codec = st.codec();
    let mut decoder = codec.decoder();
    rows::visit(
        st,
        connection,
        thread,
        0..u64::MAX,
        false,
        |position, row| {
            let line = st.decode(&mut decoder, &row, Mode::Bare)?.bytes;
            if decode_row(&line).is_err() {
                pending.state.undecodable = true;
            }
            if let Some(record) = read_row(&line) {
                pending.apply(position, &record);
            }
            Ok(true)
        },
    )?;
    Ok(Some(pending))
}

pub(super) type Batch = HashMap<i64, Option<Pending>>;

pub(super) fn append(
    st: &Storage,
    connection: &Connection,
    encoder: &mut Encoder<'_>,
    batch: &mut Batch,
    thread: &Thread,
    position: u64,
    record: &StoredEvent,
) -> io::Result<()> {
    if let std::collections::hash_map::Entry::Vacant(entry) = batch.entry(thread.key) {
        entry.insert(
            load(st, connection, thread.key)?.or_else(|| (position == 0).then(Pending::default)),
        );
    }
    if let Some(pending) = batch.get_mut(&thread.key).and_then(Option::as_mut) {
        if let Some(superseded) = pending.apply(position, record) {
            super::compact::drop_row_diffs(st, connection, encoder, thread, superseded)?;
        }
        if position == 0 {
            pending.dirty = true;
        }
    } else if matches!(record.event, agent::AgentEvent::TurnChangesUpdated { .. }) {
        rows::needs_compaction(st, connection, thread.key)?;
    }
    Ok(())
}

pub(super) fn flush(st: &Storage, connection: &Connection, batch: &Batch) -> io::Result<()> {
    for (key, pending) in batch {
        if let Some(pending) = pending
            && pending.dirty
        {
            save(st, connection, *key, pending)?;
        }
    }
    Ok(())
}

pub(super) fn copy(
    st: &Storage,
    connection: &Connection,
    src: Option<&Thread>,
    dst: &Thread,
) -> io::Result<()> {
    forget(st, connection, dst)?;
    if let Some(src) = src {
        st.execute(
            connection,
            "INSERT INTO turn_index (thread, fold_version, turns, starts, state) \
            SELECT ?2, fold_version, turns, starts, state FROM turn_index WHERE thread = ?1",
            (src.key, dst.key),
        )?;
    }
    Ok(())
}

/// The position `bound` names in `thread`'s log.
pub(super) fn bound_row(
    st: &Storage,
    connection: &Connection,
    thread: &Thread,
    bound: CloneBound,
) -> io::Result<u64> {
    let starts = match read(st, connection, thread)? {
        Some(index) => index.starts,
        None => candidate(st, connection, thread)?
            .map(|pending| pending.index.starts)
            .unwrap_or_default(),
    };
    let (CloneBound::BeforeTurn(turn) | CloneBound::ThroughTurnOpening(turn)) = bound;
    let row = starts
        .get(turn)
        .copied()
        .ok_or_else(|| io::Error::other(format!("{} has no turn {turn}", thread.key)))?;
    Ok(match bound {
        CloneBound::BeforeTurn(_) => row,
        CloneBound::ThroughTurnOpening(_) => row + 1,
    })
}

/// Fold `thread`'s log into its index now. A partial copy cannot take the
/// source's, and appends only follow an index that exists.
pub(super) fn rebuild(st: &Storage, connection: &Connection, thread: &Thread) -> io::Result<()> {
    forget(st, connection, thread)?;
    match candidate(st, connection, thread)? {
        Some(pending) => save(st, connection, thread.key, &pending),
        None => Ok(()),
    }
}

pub(super) fn forget(st: &Storage, connection: &Connection, thread: &Thread) -> io::Result<()> {
    st.execute(
        connection,
        "DELETE FROM turn_index WHERE thread = ?1",
        [thread.key],
    )?;
    Ok(())
}
