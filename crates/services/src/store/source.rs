//! The older build's `tcode.db` while its threads move into the current one.
//!
//! The first start of this build turns the data dir over in one short step
//! before the store opens ([`switch`]): the older file is checkpointed, its
//! metadata copied into a new schema-3 file staged as `tcode.db.next`, the
//! older file hard-linked to `tcode.db.v1`, and the new file renamed over
//! `tcode.db`. At every instant `tcode.db` is either the complete older file or
//! the complete new one, so a crash anywhere before the rename leaves the older
//! file in place and the next start does the step again. An older build only
//! ever opens `tcode.db`, and refuses it from then on by its version.
//!
//! From then on `tcode.db.v1` is the source: only read, never written, until
//! every thread has moved out of it and it is deleted.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use turso::Connection;

use super::db::{Db, SCHEMA, SCHEMA_VERSION, V1_VERSION, blob, integer, text, wal_path};
use super::{DB_FILE, read_index};
use crate::fs_tree::sync_dir;

pub(super) const SOURCE_FILE: &str = "tcode.db.v1";
const STAGING_FILE: &str = "tcode.db.next";

pub(super) struct Source {
    db: Option<Db>,
    path: PathBuf,
    ends: Mutex<HashMap<String, u64>>,
    /// Every thread in it is registered in the store.
    enumerated: AtomicBool,
    /// Delete the file once the last handle on it drops.
    retired: AtomicBool,
}

impl Source {
    pub(super) fn open(root: &Path) -> io::Result<Option<Self>> {
        let path = root.join(SOURCE_FILE);
        if !path.is_file() {
            return Ok(None);
        }
        let db = Db::open(&path, None)?;
        let version = db.user_version()?;
        if version != V1_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} has schema version {version}; it should be an older build's database",
                    path.display()
                ),
            ));
        }
        Ok(Some(Self {
            db: Some(db),
            path,
            ends: Mutex::default(),
            enumerated: AtomicBool::new(false),
            retired: AtomicBool::new(false),
        }))
    }

    fn db(&self) -> &Db {
        self.db.as_ref().expect("open until dropped")
    }

    pub(super) fn cancel_reads(&self) {
        self.db().cancel_reads();
    }

    pub(super) fn enumerated(&self) {
        self.enumerated.store(true, Ordering::Release);
    }

    pub(super) fn is_enumerated(&self) -> bool {
        self.enumerated.load(Ordering::Acquire)
    }

    pub(super) fn retire(&self) {
        self.retired.store(true, Ordering::Release);
    }

    /// The position after `id`'s last row; it never changes.
    pub(super) fn end(&self, id: &str) -> io::Result<u64> {
        if let Some(end) = self.ends.lock().unwrap_or_else(|p| p.into_inner()).get(id) {
            return Ok(*end);
        }
        let end = self
            .db()
            .read(|db, connection| end_of(db, connection, id))?;
        self.ends
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id.to_owned(), end);
        Ok(end)
    }

    pub(super) fn visit(
        &self,
        id: &str,
        positions: Range<u64>,
        backwards: bool,
        each: &mut dyn FnMut(u64, Vec<u8>) -> io::Result<bool>,
    ) -> io::Result<()> {
        let bound = |position: u64| i64::try_from(position).unwrap_or(i64::MAX);
        self.db().read(|db, connection| {
            db.query_while(
                connection,
                if backwards {
                    "SELECT position, line FROM events WHERE session_id = ?1 \
                     AND position >= ?2 AND position < ?3 ORDER BY position DESC"
                } else {
                    "SELECT position, line FROM events WHERE session_id = ?1 \
                     AND position >= ?2 AND position < ?3 ORDER BY position"
                },
                (id, bound(positions.start), bound(positions.end)),
                |row| each(integer(row, 0)? as u64, blob(row, 1)?),
            )
        })
    }

    /// The first row in `positions` containing `tag`.
    pub(super) fn find(
        &self,
        id: &str,
        positions: Range<u64>,
        tag: &[u8],
    ) -> io::Result<Option<(u64, Vec<u8>)>> {
        if positions.is_empty() {
            return Ok(None);
        }
        self.db().read(|db, connection| {
            let mut found = None;
            db.query(
                connection,
                "SELECT position, line FROM events WHERE session_id = ?1 AND position >= ?2 \
                 AND position < ?3 AND instr(line, ?4) > 0 ORDER BY position LIMIT 1",
                (id, positions.start as i64, positions.end as i64, tag),
                |row| {
                    found = Some((integer(row, 0)? as u64, blob(row, 1)?));
                    Ok(())
                },
            )?;
            Ok(found)
        })
    }

    /// The thread after `after` in id order, found through the primary key
    /// without scanning rows.
    pub(super) fn next_thread(&self, after: Option<&str>) -> io::Result<Option<String>> {
        self.db().read(|db, connection| {
            let mut next = None;
            db.query(
                connection,
                "SELECT session_id FROM events WHERE session_id > ?1 \
                 ORDER BY session_id LIMIT 1",
                [after.unwrap_or("")],
                |row| {
                    next = Some(text(row, 0)?);
                    Ok(())
                },
            )?;
            Ok(next)
        })
    }

    /// Up to `count` rows spread over the file.
    pub(super) fn sample(&self, count: usize) -> io::Result<Vec<Vec<u8>>> {
        self.db().read(|db, connection| {
            let mut last = 0;
            db.query(connection, "SELECT MAX(rowid) FROM events", (), |row| {
                last = integer(row, 0).unwrap_or(0);
                Ok(())
            })?;
            let mut rows = Vec::new();
            if last <= 0 {
                return Ok(rows);
            }
            let step = (last as u64 / count.max(1) as u64).max(1);
            let mut rowid = 1;
            while rowid <= last as u64 && rows.len() < count {
                db.query(
                    connection,
                    "SELECT line FROM events WHERE rowid >= ?1 LIMIT 1",
                    [rowid as i64],
                    |row| {
                        rows.push(blob(row, 0)?);
                        Ok(())
                    },
                )?;
                rowid += step;
            }
            Ok(rows)
        })
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        drop(self.db.take());
        if self.retired.load(Ordering::Acquire) {
            remove_source(&self.path);
        }
    }
}

/// Delete a source every thread has moved out of. A failure (another handle
/// still open on Windows) is retried by the next start.
pub(super) fn remove_source(path: &Path) {
    for file in [wal_path(path), path.to_path_buf()] {
        match fs::remove_file(&file) {
            Ok(()) => log::info!("removed {}", file.display()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => log::warn!("could not remove {}: {error}", file.display()),
        }
    }
    if let Some(dir) = path.parent()
        && let Err(error) = sync_dir(dir)
    {
        log::warn!("could not sync {}: {error}", dir.display());
    }
}

fn end_of(db: &Db, connection: &Connection, id: &str) -> io::Result<u64> {
    let mut end = 0;
    db.query(
        connection,
        "SELECT MAX(position) FROM events WHERE session_id = ?1",
        [id],
        |row| {
            if let Ok(turso::Value::Integer(last)) = row.get_value(0) {
                end = last as u64 + 1;
            }
            Ok(())
        },
    )?;
    Ok(end)
}

fn tables(db: &Db, connection: &Connection) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    db.query(
        connection,
        "SELECT name FROM sqlite_master WHERE type = 'table'",
        (),
        |row| {
            names.push(text(row, 0)?);
            Ok(())
        },
    )?;
    Ok(names)
}

/// A turn index row: thread, fold version, turns, starts, state.
type CarriedIndex = (String, i64, i64, Vec<u8>, Vec<u8>);

/// What the new file starts with: the metadata, and the derived per-thread
/// state that stays true while the rows it was derived from are only moved.
struct Carried {
    index: Vec<(String, Vec<u8>)>,
    projects: Vec<(String, Vec<u8>)>,
    kept_worktrees: Vec<String>,
    /// Every thread with a log or metadata, with the position after its last
    /// row.
    threads: Vec<(String, u64)>,
    turn_index: Vec<CarriedIndex>,
    activity: Vec<(String, i64, i64, Vec<u8>)>,
}

fn carried(db: &Db, connection: &Connection) -> io::Result<Carried> {
    let tables = tables(db, connection)?;
    let pairs = |table: &str| -> io::Result<Vec<(String, Vec<u8>)>> {
        let mut rows = Vec::new();
        db.query(
            connection,
            &format!("SELECT id, body FROM {table} ORDER BY rowid"),
            (),
            |row| {
                rows.push((text(row, 0)?, blob(row, 1)?));
                Ok(())
            },
        )?;
        Ok(rows)
    };
    let mut carried = Carried {
        index: pairs("sessions")?,
        projects: pairs("projects")?,
        kept_worktrees: Vec::new(),
        threads: Vec::new(),
        turn_index: Vec::new(),
        activity: Vec::new(),
    };
    if tables.iter().any(|name| name == "kept_worktrees") {
        db.query(connection, "SELECT path FROM kept_worktrees", (), |row| {
            carried.kept_worktrees.push(text(row, 0)?);
            Ok(())
        })?;
    }
    if tables.iter().any(|name| name == "turn_index") {
        db.query(
            connection,
            "SELECT session_id, fold_version, turns, starts, state FROM turn_index",
            (),
            |row| {
                carried.turn_index.push((
                    text(row, 0)?,
                    integer(row, 1)?,
                    integer(row, 2)?,
                    blob(row, 3)?,
                    blob(row, 4)?,
                ));
                Ok(())
            },
        )?;
    }
    if tables.iter().any(|name| name == "thread_activity") {
        db.query(
            connection,
            "SELECT session_id, fold_version, has_parent, body FROM thread_activity",
            (),
            |row| {
                carried.activity.push((
                    text(row, 0)?,
                    integer(row, 1)?,
                    integer(row, 2)?,
                    blob(row, 3)?,
                ));
                Ok(())
            },
        )?;
    }
    let mut ids: Vec<String> = carried
        .index
        .iter()
        .map(|(id, _)| id.clone())
        .chain(carried.turn_index.iter().map(|row| row.0.clone()))
        .chain(carried.activity.iter().map(|row| row.0.clone()))
        .collect();
    // Every log, a session's or not, through the primary key alone.
    let mut after = String::new();
    loop {
        let mut next = None;
        db.query(
            connection,
            "SELECT session_id FROM events WHERE session_id > ?1 ORDER BY session_id LIMIT 1",
            [after.as_str()],
            |row| {
                next = Some(text(row, 0)?);
                Ok(())
            },
        )?;
        let Some(next) = next else { break };
        ids.push(next.clone());
        after = next;
    }
    ids.sort();
    ids.dedup();
    for id in ids {
        let end = end_of(db, connection, &id)?;
        carried.threads.push((id, end));
    }
    Ok(carried)
}

/// Whether `tcode.db` is an older build's file still to be turned over.
pub(super) fn needed(root: &Path) -> io::Result<bool> {
    let db = Db::open(&root.join(DB_FILE), None)?;
    Ok(db.user_version()? == V1_VERSION)
}

/// Turn an older build's `tcode.db` into the source of a new schema-3
/// `tcode.db`. Called with the data dir's ownership lock held and nothing
/// open.
pub(super) fn switch(root: &Path) -> io::Result<()> {
    let path = root.join(DB_FILE);
    let staging = root.join(STAGING_FILE);
    let linked = root.join(SOURCE_FILE);
    if linked.exists() {
        // An unfinished switch leaves a second name for `tcode.db` itself.
        // Anything else there is not ours to delete.
        let (link, file) = (fs::metadata(&linked)?, fs::metadata(&path)?);
        if link.len() != file.len() || link.modified()? != file.modified()? {
            let aside = root.join(format!("{SOURCE_FILE}.{}", uuid::Uuid::new_v4()));
            log::warn!(
                "{} is not {}; keeping it as {}",
                linked.display(),
                path.display(),
                aside.display()
            );
            fs::rename(&linked, aside)?;
        }
    }
    for stale in [
        wal_path(&staging),
        staging.clone(),
        wal_path(&linked),
        linked.clone(),
    ] {
        match fs::remove_file(&stale) {
            Ok(()) => log::warn!("discarded {} from an unfinished switch", stale.display()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    let started = std::time::Instant::now();
    let carried = {
        let old = Db::open(&path, None)?;
        old.checkpoint()?;
        old.read(|db, connection| {
            db.execute(connection, "BEGIN", ())?;
            let carried = carried(db, connection);
            db.execute(connection, "COMMIT", ())?;
            carried
        })?
    };
    {
        let new = Db::open(&staging, Some(SCHEMA))?;
        new.write(|db, connection| {
            for (id, body) in &carried.index {
                db.execute(
                    connection,
                    "INSERT INTO sessions (id, body) VALUES (?1, ?2)",
                    (id.as_str(), body.as_slice()),
                )?;
            }
            for (id, body) in &carried.projects {
                db.execute(
                    connection,
                    "INSERT INTO projects (id, body) VALUES (?1, ?2)",
                    (id.as_str(), body.as_slice()),
                )?;
            }
            for path in &carried.kept_worktrees {
                db.execute(
                    connection,
                    "INSERT INTO kept_worktrees (path) VALUES (?1)",
                    [path.as_str()],
                )?;
            }
            let mut keys = HashMap::new();
            for (key, (id, end)) in carried.threads.iter().enumerate() {
                let key = key as i64 + 1;
                db.execute(
                    connection,
                    "INSERT INTO threads (key, id, revision, compacted, old_id, old_end, moved) \
                     VALUES (?1, ?2, randomblob(16), ?3, ?4, ?5, 0)",
                    (
                        key,
                        id.as_str(),
                        i64::from(*end == 0),
                        (*end > 0).then_some(id.as_str()),
                        *end as i64,
                    ),
                )?;
                keys.insert(id.as_str(), key);
            }
            super::turn_index::create_tables(db, connection)?;
            super::activity::create_table(db, connection)?;
            for (id, version, turns, starts, state) in &carried.turn_index {
                db.execute(
                    connection,
                    "INSERT INTO turn_index (thread, fold_version, turns, starts, state) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    (
                        keys[id.as_str()],
                        *version,
                        *turns,
                        starts.as_slice(),
                        state.as_slice(),
                    ),
                )?;
            }
            for (id, version, parent, body) in &carried.activity {
                db.execute(
                    connection,
                    "INSERT INTO thread_activity (thread, fold_version, has_parent, body) \
                     VALUES (?1, ?2, ?3, ?4)",
                    (keys[id.as_str()], *version, *parent, body.as_slice()),
                )?;
            }
            db.execute(
                connection,
                &format!("PRAGMA user_version = {SCHEMA_VERSION}"),
                (),
            )?;
            Ok(())
        })?;
        new.checkpoint()?;
        // Confirm what a fresh open will see before publishing it.
        if new.read(read_index)?.sessions.len() != carried.index.len() {
            return Err(io::Error::other("the staged database lost sessions"));
        }
    }
    fs::OpenOptions::new()
        .write(true)
        .open(&staging)?
        .sync_all()?;
    fs::hard_link(&path, &linked)?;
    sync_dir(root)?;
    // The checkpoint left the older file's WAL empty; the new file must not
    // take it over.
    match fs::remove_file(wal_path(&path)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    fs::rename(&staging, &path)?;
    sync_dir(root)?;
    let _ = fs::remove_file(wal_path(&staging));
    log::info!(
        "turned {} over to schema {SCHEMA_VERSION} in {:.0?}: {} thread(s) and their metadata \
         carried, their rows to move from {}",
        path.display(),
        started.elapsed(),
        carried.threads.len(),
        linked.display()
    );
    Ok(())
}
