use std::io;

use agent::{AgentEvent, ItemContent};
use serde::{Deserialize, Serialize};
use tcode_core::session::{
    StoredEvent, TURN_INDEX_FOLD_VERSION, provider_turn_index, record_opens_turn,
    synthetic_entry_id,
};
use turso::Connection;

use super::db::{Db, blob, integer, text};
use super::{SessionStore, decode_row, invalid_data};

pub(super) fn create_tables(db: &Db, connection: &Connection) -> io::Result<()> {
    db.execute(connection, "DROP TABLE IF EXISTS turn_index", ())?;
    db.execute(
        connection,
        "CREATE TABLE IF NOT EXISTS turn_index_v2 (\
        session_id TEXT PRIMARY KEY, fold_version INTEGER NOT NULL, \
        turns INTEGER NOT NULL, starts BLOB NOT NULL, state BLOB NOT NULL)",
        (),
    )?;
    db.execute(
        connection,
        "CREATE TABLE IF NOT EXISTS turn_index_entries_v2 (\
        session_id TEXT NOT NULL, id TEXT NOT NULL, turn INTEGER NOT NULL, \
        pending INTEGER NOT NULL)",
        (),
    )?;
    db.execute(connection, "CREATE INDEX IF NOT EXISTS turn_index_entry_ids_v2 ON turn_index_entries_v2 (session_id, id, turn)", ())?;
    Ok(())
}

/// Turn count after replay, and every physical row that opened a turn.
/// Rewinds remove turns but retain the starts of their ended lifetimes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnIndex {
    pub turns: u64,
    pub starts: Vec<u64>,
}

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
struct Boundary {
    provider: Option<String>,
    checkpoint: Option<String>,
    closed: bool,
    running: bool,
    snapshot: Option<u64>,
}

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
struct State {
    turns: Vec<Boundary>,
    synthetic: u64,
    compacting: bool,
    model: Option<String>,
    served_model: Option<String>,
    undecodable: bool,
}

impl SessionStore {
    pub fn turn_index(&self, id: &str) -> io::Result<Option<TurnIndex>> {
        self.run("read a turn index", |db| {
            db.read(|db, connection| read(db, connection, id))
        })
    }

    pub fn threads_without_turn_index(&self) -> io::Result<Vec<String>> {
        self.run("list threads without a turn index", |db| {
            db.read(|db, connection| {
                let mut ids = Vec::new();
                db.query(
                    connection,
                    "SELECT id FROM sessions WHERE id NOT IN \
                     (SELECT session_id FROM turn_index_v2 WHERE fold_version = ?1)",
                    [i64::from(TURN_INDEX_FOLD_VERSION)],
                    |row| {
                        ids.push(text(row, 0)?);
                        Ok(())
                    },
                )?;
                Ok(ids)
            })
        })
    }
}

pub(super) fn read(db: &Db, connection: &Connection, id: &str) -> io::Result<Option<TurnIndex>> {
    let mut index = None;
    db.query(
        connection,
        "SELECT turns, starts FROM turn_index_v2 WHERE session_id = ?1 AND fold_version = ?2",
        (id, i64::from(TURN_INDEX_FOLD_VERSION)),
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

fn save(
    db: &Db,
    connection: &Connection,
    id: &str,
    index: &TurnIndex,
    state: &State,
) -> io::Result<()> {
    db.execute(connection,
        "INSERT INTO turn_index_v2 (session_id, fold_version, turns, starts, state) \
         VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (session_id) DO UPDATE SET \
         fold_version = excluded.fold_version, turns = excluded.turns, starts = excluded.starts, state = excluded.state",
        (id, i64::from(TURN_INDEX_FOLD_VERSION), index.turns as i64,
         serde_json::to_vec(&index.starts).map_err(invalid_data)?,
         serde_json::to_vec(state).map_err(invalid_data)?))?;
    Ok(())
}

/// Hydration supplies the independent full-fold result; the writer reconstructs
/// only the boundary state needed to continue it, if migration is still pending.
pub(super) fn set(db: &Db, connection: &Connection, id: &str, index: &TurnIndex) -> io::Result<()> {
    if read(db, connection, id)?.is_none() {
        rebuild(db, connection, id)?;
    }
    if read(db, connection, id)?.as_ref() != Some(index) {
        return Err(invalid_data(
            "turn index disagrees with the full timeline fold",
        ));
    }
    Ok(())
}

pub(super) fn rebuild(db: &Db, connection: &Connection, id: &str) -> io::Result<()> {
    forget(db, connection, id)?;
    let mut index = TurnIndex {
        turns: 0,
        starts: Vec::new(),
    };
    let mut state = State::default();
    db.query(
        connection,
        "SELECT position, line FROM events WHERE session_id = ?1 ORDER BY position",
        (id,),
        |row| {
            match decode_row(&blob(row, 1)?) {
                Ok(Some(record)) => {
                    apply(
                        db,
                        connection,
                        id,
                        integer(row, 0)? as u64,
                        &record.stored,
                        &mut index,
                        &mut state,
                    )?;
                }
                Ok(None) => {}
                Err(_) => state.undecodable = true,
            }
            Ok(())
        },
    )?;
    save(db, connection, id, &index, &state)
}

pub(super) fn backfill(db: &Db, connection: &Connection, id: &str) -> io::Result<()> {
    if read(db, connection, id)?.is_none() {
        rebuild(db, connection, id)?;
    }
    Ok(())
}

pub(super) fn append(
    db: &Db,
    connection: &Connection,
    id: &str,
    position: u64,
    record: &StoredEvent,
) -> io::Result<()> {
    if position > 0 && fast_append(db, connection, id, &record.event)? {
        return Ok(());
    }
    let mut stored = None;
    db.query(connection,
        "SELECT turns, starts, state FROM turn_index_v2 WHERE session_id = ?1 AND fold_version = ?2",
        (id, i64::from(TURN_INDEX_FOLD_VERSION)), |row| {
            stored = Some((TurnIndex {
                turns: integer(row, 0)? as u64,
                starts: serde_json::from_slice(&blob(row, 1)?).map_err(invalid_data)?,
            }, serde_json::from_slice::<State>(&blob(row, 2)?).map_err(invalid_data)?));
            Ok(())
        })?;
    let (mut index, mut state) = match stored {
        Some(stored) => stored,
        None if position == 0 => (
            TurnIndex {
                turns: 0,
                starts: Vec::new(),
            },
            State::default(),
        ),
        None => {
            if matches!(record.event, AgentEvent::TurnChangesUpdated { .. }) {
                super::superseded::forget_pass(db, connection, id)?;
            }
            return Ok(());
        }
    };
    let before = state.clone();
    let starts = index.starts.len();
    if let Some(superseded) = apply(db, connection, id, position, record, &mut index, &mut state)? {
        super::superseded::drop_row_diffs(db, connection, id, superseded)?;
    }
    if position == 0 || starts != index.starts.len() || before != state {
        save(db, connection, id, &index, &state)?;
    }
    Ok(())
}

fn entry_exists(db: &Db, connection: &Connection, id: &str, item: &str) -> io::Result<bool> {
    let mut exists = false;
    db.query(
        connection,
        "SELECT 1 FROM turn_index_entries_v2 WHERE session_id = ?1 AND id = ?2 LIMIT 1",
        (id, item),
        |_| {
            exists = true;
            Ok(())
        },
    )?;
    Ok(exists)
}

fn remember(
    db: &Db,
    connection: &Connection,
    id: &str,
    item: &str,
    turn: usize,
    pending: bool,
) -> io::Result<()> {
    db.execute(
        connection,
        "INSERT INTO turn_index_entries_v2 (session_id, id, turn, pending) VALUES (?1, ?2, ?3, ?4)",
        (id, item, turn as i64, i64::from(pending)),
    )?;
    Ok(())
}

fn apply_entries(
    db: &Db,
    connection: &Connection,
    id: &str,
    event: &AgentEvent,
    current: Option<usize>,
) -> io::Result<()> {
    let known = match event {
        AgentEvent::ItemStarted(item)
        | AgentEvent::ItemUpdated(item)
        | AgentEvent::ItemCompleted(item) => entry_exists(db, connection, id, &item.id)?,
        AgentEvent::SteerRequested { request_id, .. } => {
            entry_exists(db, connection, id, request_id)?
        }
        _ => false,
    };
    match event {
        AgentEvent::ItemStarted(item)
        | AgentEvent::ItemUpdated(item)
        | AgentEvent::ItemCompleted(item)
            if item.parent_item_id.is_none() =>
        {
            if !known {
                remember(
                    db,
                    connection,
                    id,
                    &item.id,
                    current.expect("turn-opening policy"),
                    false,
                )?;
            } else if !matches!(item.content, ItemContent::UserMessage { .. }) {
                db.execute(connection, "UPDATE turn_index_entries_v2 SET pending = 0 WHERE rowid = (SELECT rowid FROM turn_index_entries_v2 WHERE session_id = ?1 AND id = ?2 ORDER BY rowid LIMIT 1)", (id, item.id.as_str()))?;
            }
        }
        AgentEvent::SteerRequested { request_id, .. } if !known => {
            remember(
                db,
                connection,
                id,
                request_id,
                current.expect("turn-opening policy"),
                true,
            )?;
        }
        AgentEvent::SteerRejected { request_id } => {
            db.execute(connection, "DELETE FROM turn_index_entries_v2 WHERE session_id = ?1 AND id = ?2 AND pending = 1", (id, request_id.as_str()))?;
        }
        AgentEvent::Delta { item_id, .. } | AgentEvent::ProposedPlanDelta { item_id, .. } => {
            let turn = current.expect("turn-opening policy");
            let mut exists = false;
            db.query(connection, "SELECT 1 FROM turn_index_entries_v2 WHERE session_id = ?1 AND id = ?2 AND turn = ?3 LIMIT 1", (id, item_id.as_str(), turn as i64), |_| { exists = true; Ok(()) })?;
            if !exists {
                remember(db, connection, id, item_id, turn, false)?;
            }
        }
        AgentEvent::ProposedPlan { item_id, .. } => {
            if entry_exists(db, connection, id, item_id)? {
                db.execute(connection, "UPDATE turn_index_entries_v2 SET pending = 0 WHERE rowid = (SELECT rowid FROM turn_index_entries_v2 WHERE session_id = ?1 AND id = ?2 ORDER BY rowid LIMIT 1)", (id, item_id.as_str()))?;
            } else {
                remember(
                    db,
                    connection,
                    id,
                    item_id,
                    current.expect("turn-opening policy"),
                    false,
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn fast_append(db: &Db, connection: &Connection, id: &str, event: &AgentEvent) -> io::Result<bool> {
    match event {
        AgentEvent::ItemStarted(_)
        | AgentEvent::ItemUpdated(_)
        | AgentEvent::ItemCompleted(_)
        | AgentEvent::SteerRequested { .. }
        | AgentEvent::SteerRejected { .. }
        | AgentEvent::Delta { .. }
        | AgentEvent::ProposedPlanDelta { .. }
        | AgentEvent::ProposedPlan { .. } => {}
        AgentEvent::SessionStarted { .. }
        | AgentEvent::TokenUsage(_)
        | AgentEvent::TurnStarted { .. }
        | AgentEvent::TurnCompleted { .. }
        | AgentEvent::SessionClosed { .. }
        | AgentEvent::TurnChangesUpdated { .. }
        | AgentEvent::TurnCheckpoint { .. }
        | AgentEvent::RewindCompleted { .. }
        | AgentEvent::ProviderRelay { .. }
        | AgentEvent::SteerAccepted { .. }
        | AgentEvent::ServedModel { .. }
        | AgentEvent::ProviderStartFailed { .. }
        | AgentEvent::Error { .. }
        | AgentEvent::ContextCompacted(_)
        | AgentEvent::ContextWindowChanged { .. } => return Ok(false),
        _ => return Ok(true),
    }
    let mut turns = None;
    db.query(
        connection,
        "SELECT turns FROM turn_index_v2 WHERE session_id = ?1 AND fold_version = ?2",
        (id, i64::from(TURN_INDEX_FOLD_VERSION)),
        |row| {
            turns = Some(integer(row, 0)?);
            Ok(())
        },
    )?;
    let Some(turns) = turns else {
        return Ok(true);
    };
    if turns == 0 {
        return Ok(false);
    }
    if let AgentEvent::ItemStarted(item)
    | AgentEvent::ItemUpdated(item)
    | AgentEvent::ItemCompleted(item) = event
        && item.parent_item_id.is_none()
        && matches!(item.content, ItemContent::UserMessage { .. })
        && !entry_exists(db, connection, id, &item.id)?
    {
        return Ok(false);
    }
    apply_entries(db, connection, id, event, Some(turns as usize - 1))?;
    Ok(true)
}

fn apply(
    db: &Db,
    connection: &Connection,
    id: &str,
    position: u64,
    record: &StoredEvent,
    index: &mut TurnIndex,
    state: &mut State,
) -> io::Result<Option<u64>> {
    let event = &record.event;
    let item = match event {
        AgentEvent::ItemStarted(item)
        | AgentEvent::ItemUpdated(item)
        | AgentEvent::ItemCompleted(item) => Some(item.id.as_str()),
        AgentEvent::SteerRequested { request_id, .. } => Some(request_id.as_str()),
        _ => None,
    };
    let known = item
        .map(|item| entry_exists(db, connection, id, item))
        .transpose()?
        .unwrap_or(false);
    if record_opens_turn(
        event,
        state.turns.last().map(|turn| !turn.closed),
        known,
        state.compacting,
    ) {
        state.turns.push(Boundary::default());
        index.starts.push(position);
    }
    let current = state.turns.len().checked_sub(1);
    let mut superseded = None;
    apply_entries(db, connection, id, event, current)?;
    match event {
        AgentEvent::SessionStarted { model, .. } => {
            if model.is_some() {
                state.model.clone_from(model);
            }
            state.served_model.clone_from(&state.model);
        }
        AgentEvent::TokenUsage(usage) => {
            state.compacting = usage.freshness == agent::ContextFreshness::Compacting;
        }
        AgentEvent::TurnStarted { turn_id } => {
            let turn = state.turns.last_mut().expect("turn-opening policy");
            turn.provider = Some(turn_id.clone());
            turn.running = true;
        }
        AgentEvent::TurnChangesUpdated { turn_id, .. }
        | AgentEvent::TurnCheckpoint { turn_id, .. } => {
            let target = provider_turn_index(
                turn_id,
                state.turns.iter().map(|turn| turn.provider.as_deref()),
                current,
            )
            .expect("turn-opening policy");
            let turn = &mut state.turns[target];
            if matches!(event, AgentEvent::TurnChangesUpdated { .. }) {
                superseded = turn.snapshot.replace(position);
            }
            if !turn_id.is_empty() || matches!(event, AgentEvent::TurnCheckpoint { .. }) {
                turn.provider = Some(turn_id.clone());
            }
            if let AgentEvent::TurnCheckpoint { checkpoint_id, .. } = event {
                turn.checkpoint = Some(checkpoint_id.clone());
            }
        }
        AgentEvent::TurnCompleted { usage, .. } => {
            if let Some(usage) = usage {
                state.compacting = usage.freshness == agent::ContextFreshness::Compacting;
            }
            if let Some(turn) = state.turns.last_mut() {
                turn.closed = true;
                turn.running = false;
            }
        }
        AgentEvent::SessionClosed { .. } => {
            if let Some(turn) = state.turns.last_mut() {
                turn.running = false;
            }
        }
        AgentEvent::RewindCompleted {
            checkpoint_id,
            mode,
            ..
        } if mode.includes_conversation() => {
            if let Some(target) = state
                .turns
                .iter()
                .position(|turn| turn.checkpoint.as_deref() == Some(checkpoint_id))
            {
                state.turns.truncate(target);
                state.compacting = false;
                db.execute(
                    connection,
                    "DELETE FROM turn_index_entries_v2 WHERE session_id = ?1 AND turn >= ?2",
                    (id, target as i64),
                )?;
            }
        }
        AgentEvent::SteerAccepted { request_id } => {
            let running = state.turns.iter().rposition(|turn| turn.running);
            if let Some(running) = running {
                db.execute(connection, "UPDATE turn_index_entries_v2 SET turn = ?3, pending = 0 WHERE rowid = (SELECT rowid FROM turn_index_entries_v2 WHERE session_id = ?1 AND id = ?2 ORDER BY rowid LIMIT 1) AND pending = 1", (id, request_id.as_str(), running as i64))?;
            } else {
                db.execute(connection, "UPDATE turn_index_entries_v2 SET pending = 0 WHERE rowid = (SELECT rowid FROM turn_index_entries_v2 WHERE session_id = ?1 AND id = ?2 ORDER BY rowid LIMIT 1)", (id, request_id.as_str()))?;
            }
        }
        _ => {}
    }
    let prefix = match event {
        AgentEvent::ProviderRelay { .. } => Some("relay"),
        AgentEvent::ServedModel { model, .. } => {
            let changed = state
                .served_model
                .as_deref()
                .map(agent::claude::strip_context_window_suffix)
                != Some(agent::claude::strip_context_window_suffix(model));
            state.served_model = Some(model.clone());
            changed.then_some("model")
        }
        AgentEvent::ProviderStartFailed { .. }
        | AgentEvent::Error { .. }
        | AgentEvent::SessionClosed { reason: Some(_) } => Some("error"),
        AgentEvent::ContextCompacted(compaction) => {
            let duplicate = compaction.in_progress && state.compacting;
            state.compacting = compaction.in_progress;
            (!duplicate).then_some("compacted")
        }
        AgentEvent::ContextWindowChanged { .. } => Some("context-window"),
        _ => None,
    };
    if let Some(prefix) = prefix {
        let mut error = None;
        let item =
            synthetic_entry_id(
                prefix,
                record.ts,
                &mut state.synthetic,
                |item| match entry_exists(db, connection, id, item) {
                    Ok(exists) => exists,
                    Err(failure) => {
                        error = Some(failure);
                        false
                    }
                },
            );
        if let Some(error) = error {
            return Err(error);
        }
        remember(
            db,
            connection,
            id,
            &item,
            current.expect("turn-opening policy"),
            false,
        )?;
    }
    index.turns = state.turns.len() as u64;
    Ok(superseded.filter(|_| !state.undecodable))
}

pub(super) fn forget(db: &Db, connection: &Connection, id: &str) -> io::Result<()> {
    db.execute(
        connection,
        "DELETE FROM turn_index_v2 WHERE session_id = ?1",
        (id,),
    )?;
    db.execute(
        connection,
        "DELETE FROM turn_index_entries_v2 WHERE session_id = ?1",
        (id,),
    )?;
    Ok(())
}
