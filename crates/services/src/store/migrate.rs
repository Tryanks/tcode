//! Creating `tcode.db`, once: empty in the current schema on a fresh install,
//! or from the JSON index (`sessions.json`) and the per-thread JSONL logs
//! older builds wrote. A migrated file is built in the layout of
//! `user_version` 1, which the store then turns over to its own like any
//! older build's ([`super::source`]).
//!
//! The database is built under a staging name and published by a rename, so
//! a file at the final name is always complete:
//!
//! 1. `tcode.db.migrating` is created with the schema and `user_version` 0,
//!    and the index is imported.
//! 2. Every `*.jsonl` in the data dir (orphans included) is imported in
//!    bounded transactions.
//! 3. `user_version` is set to 1 and the WAL is checkpointed into the main
//!    file.
//! 4. The staged file is synced, the list of sources to archive is written to
//!    `tcode.db.archive`, and the staged file is renamed to `tcode.db`; the
//!    directory is synced around both.
//! 5. The sources move into `legacy/`, and the archive list is removed.
//!    `legacy/` is deleted once the store has opened `tcode.db`
//!    ([`remove_legacy`]), so a published database that does not open still
//!    has its sources beside it.
//!
//! Nothing before step 4's rename writes, renames or removes a source: the
//! steps before it only read them. A migration cancelled or failed before
//! the rename, or a start after one interrupted there, discards the staging
//! files and leaves no `tcode.db`, so the next run begins again from the
//! untouched sources. Cancellation is honoured only up to the rename; after
//! it the next start finishes step 5 from the list.

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use tcode_core::project::{IndexFile, SessionMeta, migrate_index};

use super::DB_FILE;
use super::db::{Db, SCHEMA, SCHEMA_VERSION, V1_SCHEMA, V1_VERSION, wal_path};

const STAGING_FILE: &str = "tcode.db.migrating";
const ARCHIVE_LIST: &str = "tcode.db.archive";
const LEGACY_INDEX: &str = "sessions.json";
const LEGACY_DIR: &str = "legacy";
/// A thread's log is committed in transactions of about this size; one
/// transaction for a multi-hundred-megabyte log holds every dirty page in
/// memory and doubles the WAL.
pub(super) use crate::fs_tree::CHUNK_BYTES;

/// Where a running migration is. Importing goes through every log once;
/// publishing and archiving report everything done. While relocating, the
/// thread counts are the older data dir's entries and the byte counts what a
/// copy across filesystems has written; a move by rename reports no bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrationProgress {
    pub phase: MigrationPhase,
    pub threads_done: usize,
    pub threads_total: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationPhase {
    /// Moving an older build's data dir into this one, before anything else.
    Relocating,
    /// Listing the logs and importing the index; the totals are not known
    /// until it ends.
    Scanning,
    Importing,
    /// Past the last point a cancellation is honoured.
    Publishing,
    Archiving,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Migration {
    /// The data dir is in place and `tcode.db` is complete.
    Completed,
    /// Stopped before either was: entries already moved into the data dir
    /// stay there and the rest stay in the older one, and a JSONL migration
    /// left no staging file, no `tcode.db` and every source where it was. The
    /// next start continues.
    Cancelled,
}

/// Whether the data dir holds an older build's files and no `tcode.db`.
pub(super) fn needed(root: &Path) -> io::Result<bool> {
    Ok(!root.join(DB_FILE).exists() && !legacy_sources(root)?.is_empty())
}

/// Make sure a complete `tcode.db` exists for opening, creating an empty one
/// on a fresh install, and finish an archival a previous start left half
/// done. A data dir that still needs migrating is refused: that runs only
/// through [`run`], which reports progress and can be cancelled. Called with
/// the data dir's ownership lock held.
pub(super) fn prepare(root: &Path) -> io::Result<()> {
    if needed(root)? {
        return Err(io::Error::other(format!(
            "{} holds sessions.json or *.jsonl files from an older Tcode that have not been \
             migrated into {DB_FILE} yet",
            root.display()
        )));
    }
    run(root, &mut |_| {}, &AtomicBool::new(false)).map(drop)
}

/// Migrate the older build's files into `tcode.db`, or create an empty one,
/// reporting progress after every chunk and every log. `cancel` is checked
/// at the same points until the staged database is complete; once it is set
/// there, the staging files are removed and nothing else is changed. Called
/// with the data dir's ownership lock held.
pub(super) fn run(
    root: &Path,
    report: &mut dyn FnMut(MigrationProgress),
    cancel: &AtomicBool,
) -> io::Result<Migration> {
    if root.join(DB_FILE).exists() {
        archive_sources(root)?;
        return Ok(Migration::Completed);
    }
    remove_staging(root)?;
    if legacy_sources(root)?.is_empty() {
        create_empty(root)?;
        return Ok(Migration::Completed);
    }
    let started = Instant::now();
    let mut progress = Progress {
        report,
        cancel,
        current: MigrationProgress {
            phase: MigrationPhase::Scanning,
            threads_done: 0,
            threads_total: 0,
            bytes_done: 0,
            bytes_total: 0,
        },
    };
    (progress.report)(progress.current);
    let summary = match build_staging(root, &mut progress) {
        Ok(summary) => summary,
        Err(error) => {
            // `build_staging` has dropped every handle on the staging files.
            if let Err(cleanup) = remove_staging(root) {
                log::warn!("could not discard the staged database: {cleanup}");
            }
            if is_cancelled(&error) {
                log::info!(
                    "migration into {} cancelled; the sources are unchanged",
                    root.join(DB_FILE).display()
                );
                return Ok(Migration::Cancelled);
            }
            return Err(error);
        }
    };
    progress.finish(MigrationPhase::Publishing);
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
    progress.finish(MigrationPhase::Archiving);
    archive_sources(root)?;
    Ok(Migration::Completed)
}

/// A fresh install's `tcode.db`, in the current schema, staged and published
/// like a migrated one.
fn create_empty(root: &Path) -> io::Result<()> {
    let staging = root.join(STAGING_FILE);
    let db = Db::open(&staging, Some(SCHEMA))?;
    db.write(|db, connection| {
        db.execute(
            connection,
            &format!("PRAGMA user_version = {SCHEMA_VERSION}"),
            (),
        )
        .map(drop)
    })?;
    seal(db, &staging)?;
    publish(root, &[])
}

#[derive(Debug)]
pub(super) struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the migration was cancelled")
    }
}

impl std::error::Error for Cancelled {}

pub(super) struct Progress<'a> {
    pub(super) report: &'a mut dyn FnMut(MigrationProgress),
    pub(super) cancel: &'a AtomicBool,
    pub(super) current: MigrationProgress,
}

impl Progress<'_> {
    pub(super) fn check(&self) -> io::Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::other(Cancelled));
        }
        Ok(())
    }

    /// Begin a cancellable phase over `threads` logs of `bytes` in all.
    pub(super) fn start(
        &mut self,
        phase: MigrationPhase,
        threads: usize,
        bytes: u64,
    ) -> io::Result<()> {
        self.check()?;
        self.current = MigrationProgress {
            phase,
            threads_done: 0,
            threads_total: threads,
            bytes_done: 0,
            bytes_total: bytes,
        };
        (self.report)(self.current);
        Ok(())
    }

    pub(super) fn advance(&mut self, threads: usize, bytes: u64) -> io::Result<()> {
        self.current.threads_done += threads;
        self.current.bytes_done += bytes;
        (self.report)(self.current);
        self.check()
    }

    /// Enter a phase that runs to the end whatever `cancel` says.
    fn finish(&mut self, phase: MigrationPhase) {
        self.current.phase = phase;
        self.current.threads_done = self.current.threads_total;
        self.current.bytes_done = self.current.bytes_total;
        (self.report)(self.current);
    }
}

pub(super) fn is_cancelled(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|inner| inner.is::<Cancelled>())
}

/// Delete `legacy/` and the sources archived in it, once `tcode.db` has
/// opened. A failure is logged and retried by the next start.
pub(super) fn remove_legacy(root: &Path) {
    let legacy = root.join(LEGACY_DIR);
    match fs::remove_dir_all(&legacy) {
        Ok(()) => log::info!("removed {}", legacy.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => log::warn!("could not remove {}: {error}", legacy.display()),
    }
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
            Ok(()) => log::warn!("discarded {} from an unfinished migration", path.display()),
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

fn build_staging(root: &Path, progress: &mut Progress) -> io::Result<Summary> {
    let sources = legacy_sources(root)?;
    let mut logs = Vec::new();
    let mut bytes_total = 0;
    for name in &sources {
        if let Some(id) = name.strip_suffix(".jsonl") {
            bytes_total += fs::metadata(root.join(name))?.len();
            logs.push((id, root.join(name)));
        }
    }
    let mut index = migrate_index(read_legacy_index(root)?);
    for session in &mut index.sessions {
        session.migrate_lifecycle();
    }
    let staging = root.join(STAGING_FILE);
    let db = Db::open(&staging, Some(V1_SCHEMA))?;
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

    let mut summary = Summary {
        sources: sources.clone(),
        projects: index.projects.len(),
        sessions: index.sessions.len(),
        logs: 0,
        lines: 0,
        blank_lines: 0,
        bytes: 0,
    };
    progress.start(MigrationPhase::Importing, logs.len(), bytes_total)?;
    for (id, path) in &logs {
        let tally = import_log(&db, id, path, progress)?;
        summary.logs += 1;
        summary.lines += tally.lines;
        summary.blank_lines += tally.blank_lines;
        summary.bytes += tally.bytes;
    }
    db.write(|db, connection| {
        db.execute(
            connection,
            &format!("PRAGMA user_version = {V1_VERSION}"),
            (),
        )
        .map(drop)
    })?;
    // The last point a cancellation is honoured: what follows publishes.
    progress.check()?;
    seal(db, &staging)?;
    Ok(summary)
}

/// Checkpoint the staged database into its main file, close it and sync it,
/// so the file alone is the database.
fn seal(db: Db, staging: &Path) -> io::Result<()> {
    db.checkpoint()?;
    drop(db);
    match fs::remove_file(wal_path(staging)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    sync_file(staging)
}

/// Today's tolerance: the object schema or the legacy bare array. An
/// unparseable file is archived into `legacy/` as it is, with the logs, which
/// still migrate.
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
    Ok(parsed.unwrap_or_else(|error| {
        log::warn!(
            "failed to parse {LEGACY_INDEX}: {error}; migrating the event logs without it, and \
             archiving it with them"
        );
        IndexFile::default()
    }))
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

fn import_log(db: &Db, id: &str, path: &Path, progress: &mut Progress) -> io::Result<Tally> {
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
            progress.advance(0, pending_bytes as u64)?;
            pending_bytes = 0;
        }
        Ok(())
    })?;
    flush(&mut pending, &mut position)?;
    progress.advance(1, pending_bytes as u64)?;
    Ok(tally)
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
pub(super) fn sync_file(path: &Path) -> io::Result<()> {
    fs::OpenOptions::new().write(true).open(path)?.sync_all()
}

/// Make a directory's entries (a rename, a new file) durable.
pub(super) use crate::fs_tree::sync_dir;

fn insert_segments(
    db: &Db,
    connection: &turso::Connection,
    session_id: &str,
    first: i64,
    lines: impl IntoIterator<Item = impl AsRef<[u8]>>,
) -> io::Result<i64> {
    let mut position = first;
    for line in lines {
        db.execute(
            connection,
            "INSERT INTO events (session_id, position, line) VALUES (?1, ?2, ?3)",
            (session_id, position, line.as_ref()),
        )?;
        position += 1;
    }
    Ok(position)
}
