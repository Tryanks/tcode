//! Creating `tcode.db`, once: from the JSON index (`sessions.json`) and the
//! per-thread JSONL logs older builds wrote, or empty on a fresh install.
//!
//! The database is built and verified under a staging name and published by
//! a rename, so a file at the final name is always complete:
//!
//! 1. `tcode.db.migrating` is created with the schema and `user_version` 0.
//! 2. The index is imported, then every `*.jsonl` in the data dir (orphans
//!    included) in bounded transactions; each log is read back and compared
//!    byte for byte with its file, then the metadata field for field.
//! 3. `user_version` is set to 1, the WAL is checkpointed and truncated,
//!    every handle is dropped, and a fresh open re-counts what was committed.
//! 4. The staged file is synced, the list of sources to archive is written to
//!    `tcode.db.archive`, and the staged file is renamed to `tcode.db`; the
//!    directory is synced around both.
//! 5. The sources move into `legacy/`, and the archive list is removed.
//!
//! An interrupted run before step 4's rename leaves no `tcode.db`, and the
//! next start discards the staging files and begins again from the untouched
//! sources. After the rename, the next start finishes step 5 from the list.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Instant;

use tcode_core::project::{IndexFile, SessionMeta, migrate_index};

use super::db::{Db, SCHEMA_VERSION, blob, integer, wal_path};
use super::{DB_FILE, insert_segments, read_index};

const STAGING_FILE: &str = "tcode.db.migrating";
const ARCHIVE_LIST: &str = "tcode.db.archive";
const LEGACY_INDEX: &str = "sessions.json";
const LEGACY_DIR: &str = "legacy";
/// A thread's log is committed in transactions of about this size; one
/// transaction for a multi-hundred-megabyte log holds every dirty page in
/// memory and doubles the WAL.
const CHUNK_BYTES: usize = 8 << 20;
/// At most this many rows, and about [`CHUNK_BYTES`], are compared per read
/// when verifying, so a reader never pins the WAL or memory for a whole log.
const VERIFY_ROWS: i64 = 4096;

/// Make sure a complete `tcode.db` exists, migrating or creating it, and
/// finish an archival a previous start left half done. Called with the data
/// dir's ownership lock held.
pub(super) fn prepare(root: &Path) -> io::Result<()> {
    if root.join(DB_FILE).exists() {
        return archive_sources(root);
    }
    remove_staging(root)?;
    let started = Instant::now();
    let summary = build_staging(root)?;
    publish(root, &summary.sources)?;
    log::info!(
        "migrated {} thread(s) ({} line(s), {} blank, {} bytes) and {} session(s) in {} \
         project(s) into {} in {:.1}s",
        summary.logs,
        summary.lines,
        summary.blank_lines,
        summary.bytes,
        summary.sessions,
        summary.projects,
        root.join(DB_FILE).display(),
        started.elapsed().as_secs_f64()
    );
    archive_sources(root)
}

/// An older build that ran after the migration writes JSON again; this build
/// never reads it back.
pub(super) fn warn_about_stray_sources(root: &Path) {
    let Ok(sources) = legacy_sources(root) else {
        return;
    };
    if !sources.is_empty() {
        log::warn!(
            "ignoring {} file(s) in {} left by an older Tcode build (sessions.json / *.jsonl); \
             threads are read only from {DB_FILE}",
            sources.len(),
            root.display()
        );
    }
}

struct Summary {
    sources: Vec<String>,
    projects: usize,
    sessions: usize,
    logs: usize,
    lines: u64,
    blank_lines: u64,
    bytes: u64,
}

/// Every staging file a previous run may have left. Never the final name's.
fn remove_staging(root: &Path) -> io::Result<()> {
    let staging = root.join(STAGING_FILE);
    for path in [
        wal_path(&staging),
        sidecar(&staging, "-shm"),
        sidecar(&staging, "-journal"),
        staging,
        root.join(ARCHIVE_LIST),
    ] {
        match fs::remove_file(&path) {
            Ok(()) => log::warn!("discarded {} from an interrupted migration", path.display()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// `sessions.json` and every `<id>.jsonl` directly in the data dir.
fn legacy_sources(root: &Path) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name == LEGACY_INDEX
            || Path::new(&name)
                .extension()
                .is_some_and(|ext| ext == "jsonl")
        {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

fn build_staging(root: &Path) -> io::Result<Summary> {
    let staging = root.join(STAGING_FILE);
    let db = Db::open(&staging, true)?;
    let index = migrate_index(read_legacy_index(root)?);
    reject_duplicates(&index)?;
    db.write(|db, connection| {
        for project in &index.projects {
            let body = serde_json::to_vec(project).map_err(io::Error::other)?;
            db.execute(
                connection,
                "INSERT INTO projects (id, body) VALUES (?1, ?2)",
                (project.id.as_str(), body),
            )?;
        }
        for session in &index.sessions {
            let body = serde_json::to_vec(session).map_err(io::Error::other)?;
            db.execute(
                connection,
                "INSERT INTO sessions (id, body) VALUES (?1, ?2)",
                (session.id.as_str(), body),
            )?;
        }
        Ok(())
    })?;

    let sources = legacy_sources(root)?;
    let mut summary = Summary {
        sources: sources.clone(),
        projects: index.projects.len(),
        sessions: index.sessions.len(),
        logs: 0,
        lines: 0,
        blank_lines: 0,
        bytes: 0,
    };
    let mut tallies = Vec::new();
    for name in &sources {
        let Some(id) = name.strip_suffix(".jsonl") else {
            continue;
        };
        let path = root.join(name);
        let tally = import_log(&db, id, &path)?;
        verify_log(&db, id, &path, &tally)?;
        log::info!(
            "migrated {name}: {} line(s), {} blank, {} bytes, verified byte-identical",
            tally.lines,
            tally.blank_lines,
            tally.bytes
        );
        summary.logs += 1;
        summary.lines += tally.lines;
        summary.blank_lines += tally.blank_lines;
        summary.bytes += tally.bytes;
        tallies.push((id.to_owned(), tally));
    }

    let stored = db.read(read_index)?;
    verify_index(&index, &stored)?;
    db.write(|db, connection| {
        db.execute(
            connection,
            &format!("PRAGMA user_version = {SCHEMA_VERSION}"),
            (),
        )
        .map(drop)
    })?;
    db.checkpoint()?;
    drop(db);

    // A fresh open sees only what reached the main file.
    let reopened = Db::open(&staging, false)?;
    if reopened.user_version()? != SCHEMA_VERSION {
        return Err(io::Error::other(format!(
            "{} lost its completion marker after the checkpoint",
            staging.display()
        )));
    }
    // Per thread, so each count is an index range scan rather than one sort
    // over every row in the database.
    let mut committed_total = 0;
    for (id, tally) in &tallies {
        let (lines, bytes) = reopened.read(|db, connection| {
            let mut counts = (0, 0);
            db.query(
                connection,
                "SELECT COUNT(*), COALESCE(SUM(length(line)), 0) FROM events WHERE session_id = ?1",
                (id.as_str(),),
                |row| {
                    counts = (integer(row, 0)? as u64, integer(row, 1)? as u64);
                    Ok(())
                },
            )?;
            Ok(counts)
        })?;
        if (lines, bytes) != (tally.lines, tally.bytes) {
            return Err(io::Error::other(format!(
                "{}: thread {id} holds {lines} line(s) / {bytes} bytes after reopening, the \
                 source {} / {}",
                staging.display(),
                tally.lines,
                tally.bytes
            )));
        }
        committed_total += lines;
    }
    let mut stored_total = 0;
    reopened.read(|db, connection| {
        db.query(connection, "SELECT COUNT(*) FROM events", (), |row| {
            stored_total = integer(row, 0)? as u64;
            Ok(())
        })
    })?;
    if stored_total != committed_total {
        return Err(io::Error::other(format!(
            "{}: {stored_total} event rows after reopening, {committed_total} migrated",
            staging.display()
        )));
    }
    let stored = reopened.read(read_index)?;
    verify_index(&index, &stored)?;
    drop(reopened);
    let wal = wal_path(&staging);
    match fs::metadata(&wal) {
        Ok(metadata) if metadata.len() > 0 => {
            return Err(io::Error::other(format!(
                "{} holds {} bytes after the staged database was closed",
                wal.display(),
                metadata.len()
            )));
        }
        Ok(_) => fs::remove_file(&wal)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    sync_file(&staging)?;
    Ok(summary)
}

/// Today's tolerance: the object schema or the legacy bare array; an
/// unparseable file is preserved beside itself and the logs still migrate.
fn read_legacy_index(root: &Path) -> io::Result<IndexFile> {
    let path = root.join(LEGACY_INDEX);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(IndexFile::default()),
        Err(error) => return Err(error),
    };
    let parsed = serde_json::from_slice::<IndexFile>(&bytes).or_else(|_| {
        serde_json::from_slice::<Vec<SessionMeta>>(&bytes).map(|sessions| IndexFile {
            projects: Vec::new(),
            sessions,
        })
    });
    match parsed {
        Ok(file) => Ok(file),
        Err(error) => {
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0);
            let corrupt = root.join(format!("{LEGACY_INDEX}.corrupt-{timestamp}"));
            fs::rename(&path, &corrupt)?;
            log::warn!(
                "failed to parse {LEGACY_INDEX}: {error}; preserved it as {} and migrated the \
                 event logs without it",
                corrupt.display()
            );
            Ok(IndexFile::default())
        }
    }
}

/// Upserting by id would silently keep only the last of two entries.
fn reject_duplicates(index: &IndexFile) -> io::Result<()> {
    let mut seen = HashSet::new();
    for id in index.projects.iter().map(|project| &project.id) {
        if !seen.insert(("project", id)) {
            return Err(duplicate("project", id));
        }
    }
    for id in index.sessions.iter().map(|session| &session.id) {
        if !seen.insert(("session", id)) {
            return Err(duplicate("session", id));
        }
    }
    Ok(())
}

fn duplicate(kind: &str, id: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "{LEGACY_INDEX} lists {kind} {id} more than once; nothing was migrated and the file \
             is unchanged"
        ),
    )
}

fn verify_index(expected: &IndexFile, stored: &IndexFile) -> io::Result<()> {
    if stored.projects != expected.projects {
        return Err(io::Error::other(
            "the migrated projects differ from sessions.json when read back",
        ));
    }
    if stored.sessions != expected.sessions {
        let id = expected
            .sessions
            .iter()
            .zip(&stored.sessions)
            .find(|(expected, stored)| expected != stored)
            .map(|(expected, _)| expected.id.as_str())
            .unwrap_or("(count)");
        return Err(io::Error::other(format!(
            "the migrated session {id} differs from sessions.json when read back"
        )));
    }
    Ok(())
}

#[derive(Default)]
struct Tally {
    lines: u64,
    blank_lines: u64,
    bytes: u64,
}

/// Every raw segment of `path`: each line with its `\n`, an unterminated last
/// line as it is.
fn read_segments(path: &Path, mut each: impl FnMut(Vec<u8>) -> io::Result<()>) -> io::Result<()> {
    let mut reader = BufReader::with_capacity(1 << 20, File::open(path)?);
    loop {
        let mut segment = Vec::new();
        if reader.read_until(b'\n', &mut segment)? == 0 {
            return Ok(());
        }
        each(segment)?;
    }
}

fn import_log(db: &Db, id: &str, path: &Path) -> io::Result<Tally> {
    let mut tally = Tally::default();
    let mut pending: Vec<Vec<u8>> = Vec::new();
    let mut pending_bytes = 0;
    let mut position = 0;
    let flush = |pending: &mut Vec<Vec<u8>>, position: &mut i64| -> io::Result<()> {
        if !pending.is_empty() {
            *position = db.write(|db, connection| {
                insert_segments(db, connection, id, *position, pending.iter())
            })?;
            pending.clear();
        }
        Ok(())
    };
    read_segments(path, |segment| {
        tally.lines += 1;
        tally.bytes += segment.len() as u64;
        if segment.trim_ascii().is_empty() {
            tally.blank_lines += 1;
        }
        pending_bytes += segment.len();
        pending.push(segment);
        if pending_bytes >= CHUNK_BYTES {
            flush(&mut pending, &mut position)?;
            pending_bytes = 0;
        }
        Ok(())
    })?;
    flush(&mut pending, &mut position)?;
    Ok(tally)
}

/// Compare the committed rows with the file again, segment for segment.
fn verify_log(db: &Db, id: &str, path: &Path, tally: &Tally) -> io::Result<()> {
    let mismatch = |position: i64| {
        io::Error::other(format!(
            "{}: line {} differs from the migrated copy",
            path.display(),
            position + 1
        ))
    };
    let mut page: Vec<Vec<u8>> = Vec::new();
    let mut page_start = 0_i64;
    let mut position = 0_i64;
    read_segments(path, |segment| {
        if position - page_start == page.len() as i64 {
            page_start = position;
            page = db.read(|db, connection| {
                // Sizes first, so a page holds about CHUNK_BYTES however long
                // the lines are, and at least one line.
                let mut last = position - 1;
                let mut bytes = 0;
                db.query(
                    connection,
                    "SELECT position, length(line) FROM events \
                     WHERE session_id = ?1 AND position >= ?2 ORDER BY position LIMIT ?3",
                    (id, position, VERIFY_ROWS),
                    |row| {
                        if last < position || bytes < CHUNK_BYTES as i64 {
                            last = integer(row, 0)?;
                            bytes += integer(row, 1)?;
                        }
                        Ok(())
                    },
                )?;
                let mut rows = Vec::new();
                db.query(
                    connection,
                    "SELECT position, line FROM events \
                     WHERE session_id = ?1 AND position BETWEEN ?2 AND ?3 ORDER BY position",
                    (id, position, last),
                    |row| {
                        let stored = integer(row, 0)?;
                        if stored != position + rows.len() as i64 {
                            return Err(mismatch(position + rows.len() as i64));
                        }
                        rows.push(blob(row, 1)?);
                        Ok(())
                    },
                )?;
                Ok(rows)
            })?;
        }
        let stored = page
            .get((position - page_start) as usize)
            .ok_or_else(|| mismatch(position))?;
        if *stored != segment {
            return Err(mismatch(position));
        }
        position += 1;
        Ok(())
    })?;
    let mut extra = 0;
    db.read(|db, connection| {
        db.query(
            connection,
            "SELECT COUNT(*) FROM events WHERE session_id = ?1 AND position >= ?2",
            (id, position),
            |row| {
                extra = integer(row, 0)?;
                Ok(())
            },
        )
    })?;
    if extra != 0 || position as u64 != tally.lines {
        return Err(mismatch(position));
    }
    Ok(())
}

fn publish(root: &Path, sources: &[String]) -> io::Result<()> {
    let list = root.join(ARCHIVE_LIST);
    if !sources.is_empty() {
        let temporary = sidecar(&list, ".tmp");
        fs::write(
            &temporary,
            serde_json::to_vec_pretty(sources).map_err(io::Error::other)?,
        )?;
        sync_file(&temporary)?;
        fs::rename(&temporary, &list)?;
    }
    sync_dir(root)?;
    fs::rename(root.join(STAGING_FILE), root.join(DB_FILE))?;
    sync_dir(root)
}

/// Move the sources named by `tcode.db.archive` into `legacy/`. Safe to repeat:
/// a moved file is skipped, and a name already taken in `legacy/` keeps its
/// source where it is.
fn archive_sources(root: &Path) -> io::Result<()> {
    let list = root.join(ARCHIVE_LIST);
    let names: Vec<String> = match fs::read(&list) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is unreadable: {error}", list.display()),
            )
        })?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let legacy = root.join(LEGACY_DIR);
    fs::create_dir_all(&legacy)?;
    let mut moved = 0;
    for name in &names {
        // The list is written by this module, but a name is still never
        // allowed to leave the data dir.
        if Path::new(name).file_name() != Some(std::ffi::OsStr::new(name)) {
            continue;
        }
        let source = root.join(name);
        let destination = legacy.join(name);
        if !source.exists() {
            continue;
        }
        if destination.exists() {
            log::warn!(
                "kept {} in place: {} already exists",
                source.display(),
                destination.display()
            );
            continue;
        }
        fs::rename(&source, &destination)?;
        moved += 1;
    }
    sync_dir(&legacy)?;
    sync_dir(root)?;
    fs::remove_file(&list)?;
    sync_dir(root)?;
    log::info!(
        "moved {moved} migrated source file(s) into {}",
        legacy.display()
    );
    Ok(())
}

/// Windows flushes only a handle opened for writing.
fn sync_file(path: &Path) -> io::Result<()> {
    fs::OpenOptions::new().write(true).open(path)?.sync_all()
}

/// Make a directory's entries (a rename, a new file) durable.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// NTFS journals renames itself, and std cannot open a directory handle on
/// Windows.
#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}
