//! What the store keeps of projects moving between the machines of a group:
//! each move leaving this machine as its journal record (`project_moves`),
//! each move that arrived here and the project it made (`moves_in`), and
//! where each thread that left went (`moved_threads`). A thread's event log
//! leaves in canonical form and arrives through a staged file.
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead as _, BufReader, Write};
use std::path::Path;

use turso::Connection;

use super::codec::Encoder;
use super::db::{Db, blob, text};
use super::rows::{self, Storage};
use super::{SessionStore, activity, canonical, turn_index};

pub(super) fn create_tables(db: &Db, connection: &Connection) -> io::Result<()> {
    for table in [
        "CREATE TABLE IF NOT EXISTS project_moves (id TEXT PRIMARY KEY, body BLOB NOT NULL)",
        "CREATE TABLE IF NOT EXISTS moves_in (id TEXT PRIMARY KEY, project_id TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS moved_threads (id TEXT PRIMARY KEY, machine TEXT NOT NULL)",
    ] {
        db.execute(connection, table, ())?;
    }
    Ok(())
}

/// Replace the log of `session_id` with the lines of the file at `path`,
/// kept exactly, read a line at a time.
pub(super) fn import_event_log(
    st: &Storage,
    connection: &Connection,
    encoder: &mut Encoder<'_>,
    session_id: &str,
    path: &Path,
) -> io::Result<()> {
    let thread = rows::ensure_thread(st, connection, session_id)?;
    rows::clear(st, connection, &thread)?;
    let thread = rows::ensure_thread(st, connection, session_id)?;
    let mut reader = BufReader::new(File::open(path)?);
    let mut line = Vec::new();
    let mut position = 0_u64;
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        rows::put(st, connection, encoder, &thread, position, &line)?;
        position += 1;
    }
    activity::recompute(st, connection, &thread)?;
    rows::needs_compaction(st, connection, thread.key)?;
    turn_index::forget(st, connection, &thread)
}

impl SessionStore {
    /// Write the log of `id` to `out` as the canonical bytes it holds, a row
    /// at a time, images put back in. A row whose image is missing goes
    /// without it, as every read of it does.
    pub fn export_event_log(&self, id: &str, out: &mut dyn Write) -> io::Result<()> {
        self.run("export event log", |st| {
            st.read(|st, connection| {
                let Some(thread) = rows::thread(st, connection, id)? else {
                    return Ok(());
                };
                let codec = st.codec();
                let mut decoder = codec.decoder();
                rows::visit(
                    st,
                    connection,
                    &thread,
                    0..u64::MAX,
                    false,
                    |position, row| {
                        out.write_all(&canonical(st, &mut decoder, &row, id, position)?)?;
                        Ok(true)
                    },
                )
            })
        })
    }

    /// Every move leaving this machine that has not ended, as its journal
    /// record.
    pub fn project_moves(&self) -> io::Result<Vec<Vec<u8>>> {
        self.run("read project moves", |st| {
            st.read(|st, connection| {
                let mut records = Vec::new();
                st.query(
                    connection,
                    "SELECT body FROM project_moves ORDER BY rowid",
                    (),
                    |row| {
                        records.push(blob(row, 0)?);
                        Ok(())
                    },
                )?;
                Ok(records)
            })
        })
    }

    /// Each move that arrived here, by its id, and the project it made.
    pub fn moves_in(&self) -> io::Result<HashMap<String, String>> {
        self.pairs("read arrived moves", "SELECT id, project_id FROM moves_in")
    }

    /// Each thread that moved away, by its id, and the machine it went to.
    pub fn moved_threads(&self) -> io::Result<HashMap<String, String>> {
        self.pairs(
            "read moved threads",
            "SELECT id, machine FROM moved_threads",
        )
    }

    fn pairs(&self, operation: &str, sql: &str) -> io::Result<HashMap<String, String>> {
        self.run(operation, |st| {
            st.read(|st, connection| {
                let mut pairs = HashMap::new();
                st.query(connection, sql, (), |row| {
                    pairs.insert(text(row, 0)?, text(row, 1)?);
                    Ok(())
                })?;
                Ok(pairs)
            })
        })
    }
}

pub(super) fn apply(st: &Storage, connection: &Connection, op: &super::MoveOp) -> io::Result<()> {
    use super::MoveOp;
    match op {
        MoveOp::PutRecord { id, body } => st.execute(
            connection,
            "INSERT INTO project_moves (id, body) VALUES (?1, ?2) \
             ON CONFLICT (id) DO UPDATE SET body = excluded.body",
            (id.as_str(), body.as_slice()),
        )?,
        MoveOp::RemoveRecord(id) => st.execute(
            connection,
            "DELETE FROM project_moves WHERE id = ?1",
            (id.as_str(),),
        )?,
        MoveOp::Arrived { id, project_id } => st.execute(
            connection,
            "INSERT INTO moves_in (id, project_id) VALUES (?1, ?2)",
            (id.as_str(), project_id.as_str()),
        )?,
        MoveOp::Redirect { id, machine } => st.execute(
            connection,
            "INSERT INTO moved_threads (id, machine) VALUES (?1, ?2) \
             ON CONFLICT (id) DO UPDATE SET machine = excluded.machine",
            (id.as_str(), machine.as_str()),
        )?,
        MoveOp::ClearRedirect(id) => st.execute(
            connection,
            "DELETE FROM moved_threads WHERE id = ?1",
            (id.as_str(),),
        )?,
    };
    Ok(())
}
