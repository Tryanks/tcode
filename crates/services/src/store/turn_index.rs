//! A thread's turn index: how many turns the fold of its whole log holds and
//! the rows that opened them, so a window can be cut from the log's last rows
//! without reading the rest. It is written whenever the host folds the log
//! and the turns change, in the transaction of the append that changed them,
//! and forgotten by any write the host did not fold; a thread without one is
//! read whole.

use std::io;

use turso::Connection;

use super::db::{Db, blob, integer};
use super::{SessionStore, invalid_data};

pub(super) const TURN_INDEX_TABLE: &str = "CREATE TABLE IF NOT EXISTS turn_index (\
    session_id TEXT PRIMARY KEY, \
    turns INTEGER NOT NULL, \
    starts BLOB NOT NULL\
)";

/// The turns of the fold of a thread's whole log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnIndex {
    /// How many turns the fold holds.
    pub turns: u64,
    /// The position of every row that opened a turn while the log was
    /// folded, ascending. A rewind ends turns without removing their starts:
    /// each is still where a turn began.
    pub starts: Vec<u64>,
}

impl SessionStore {
    /// The session's turn index, if the host has one that covers its whole
    /// log.
    pub fn turn_index(&self, id: &str) -> io::Result<Option<TurnIndex>> {
        self.run("read a turn index", |db| {
            db.read(|db, connection| {
                let mut index = None;
                db.query(
                    connection,
                    "SELECT turns, starts FROM turn_index WHERE session_id = ?1",
                    (id,),
                    |row| {
                        index = Some(TurnIndex {
                            turns: integer(row, 0)? as u64,
                            starts: serde_json::from_slice(&blob(row, 1)?).map_err(invalid_data)?,
                        });
                        Ok(())
                    },
                )?;
                Ok(index)
            })
        })
    }
}

pub(super) fn set(
    db: &Db,
    connection: &Connection,
    session_id: &str,
    index: &TurnIndex,
) -> io::Result<()> {
    let starts = serde_json::to_vec(&index.starts).map_err(invalid_data)?;
    db.execute(
        connection,
        "INSERT INTO turn_index (session_id, turns, starts) VALUES (?1, ?2, ?3) \
         ON CONFLICT (session_id) DO UPDATE SET turns = excluded.turns, starts = excluded.starts",
        (session_id, index.turns as i64, starts),
    )
    .map(drop)
}

pub(super) fn forget(db: &Db, connection: &Connection, session_id: &str) -> io::Result<()> {
    db.execute(
        connection,
        "DELETE FROM turn_index WHERE session_id = ?1",
        (session_id,),
    )
    .map(drop)
}
