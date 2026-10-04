//! The host's index of projects and sessions: `tcode.redb` in the data dir.
//!
//! One row per project and per session, keyed by id. A value is the row's
//! JSON, so a row written by another build decodes with the same defaults and
//! unknown-field tolerance the types have on the wire; a row this build cannot
//! decode is skipped and left as it is.
//!
//! Only a host opens the index, and only one at a time: it is opened in
//! redb's single-writer mode, where the writing process holds byte-range
//! locks for as long as it has the file open and other processes may open it
//! read-only meanwhile.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use redb::{
    Builder, ConcurrencyMode, Database, DatabaseError, ReadableDatabase, ReadableTable,
    TableDefinition,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tcode_core::project::{IndexFile, Project, SessionMeta, migrate_index};

use super::SessionStore;

const PROJECTS: TableDefinition<&str, &[u8]> = TableDefinition::new("projects");
const SESSIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("sessions");
/// The index's own bookkeeping, keyed by the names below.
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
/// The file's size when it was last compacted.
const COMPACTED_SIZE: &str = "compacted_size";

const DATABASE_FILE: &str = "tcode.redb";
/// Where a new index is built. redb does not create a file atomically, so the
/// index only takes [`DATABASE_FILE`]'s name once it is complete.
const NEW_DATABASE_FILE: &str = "tcode.redb.tmp";
const LEGACY_INDEX: &str = "sessions.json";
const MIGRATED_INDEX: &str = "sessions.json.migrated";

/// How often a host waiting for the index tries again.
const OPEN_RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// One change to the index.
#[derive(Debug, Clone)]
pub enum IndexWrite {
    UpsertSessions(Vec<SessionMeta>),
    UpsertProject(Project),
    RemoveSessions(Vec<String>),
    RemoveProject(String),
}

/// Every row the index holds that this build can read.
#[derive(Debug)]
pub struct LoadedIndex {
    pub projects: Vec<Project>,
    pub sessions: Vec<SessionMeta>,
}

/// The host's handle to the index. Clones share one open database, which
/// closes when the last clone is dropped.
#[derive(Clone)]
pub struct SessionIndex {
    db: Arc<Database>,
    store: SessionStore,
}

fn other(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::other(error)
}

fn encode<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(other)
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> serde_json::Result<T> {
    serde_json::from_slice(bytes)
}

fn builder() -> Builder {
    let mut builder = Builder::new();
    builder.set_concurrency_mode(ConcurrencyMode::SingleWriter);
    builder
}

impl SessionStore {
    fn database_path(&self) -> PathBuf {
        self.root().join(DATABASE_FILE)
    }

    /// Open the index for this host, importing `sessions.json` the first
    /// time. While another process has it open, try again until `wait` has
    /// passed, then fail with [`io::ErrorKind::ResourceBusy`].
    pub fn open_index(&self, wait: Duration) -> io::Result<SessionIndex> {
        let path = self.database_path();
        let deadline = Instant::now() + wait;
        let (mut db, size) = loop {
            let size = fs::metadata(&path).map_or(0, |metadata| metadata.len());
            let opened = if size == 0 && !path.exists() {
                self.import().and_then(|()| builder().open(&path))
            } else {
                builder().open(&path)
            };
            match opened {
                Ok(db) => break (db, size),
                Err(DatabaseError::DatabaseAlreadyOpen) if Instant::now() < deadline => {
                    std::thread::sleep(OPEN_RETRY_INTERVAL);
                }
                Err(DatabaseError::DatabaseAlreadyOpen) => {
                    return Err(io::Error::new(
                        io::ErrorKind::ResourceBusy,
                        format!(
                            "{} is open in another Tcode process; quit it or start this one with another data directory",
                            path.display()
                        ),
                    ));
                }
                Err(error) => {
                    return Err(io::Error::other(format!(
                        "could not open {}: {error}",
                        path.display()
                    )));
                }
            }
        };
        let legacy = self.root().join(LEGACY_INDEX);
        if legacy.exists() {
            log::warn!(
                "ignoring {}: the index in {DATABASE_FILE} is the one in use",
                legacy.display()
            );
        }
        if let Err(error) = compact_if_grown(&mut db, &path, size) {
            log::warn!("could not compact {}: {error}", path.display());
        }
        Ok(SessionIndex {
            db: Arc::new(db),
            store: self.clone(),
        })
    }

    /// Build the index from `sessions.json`, or from `sessions.json.migrated`
    /// when only that is left, and put it in place. It is written in one
    /// transaction under a temporary name and read back; then the JSON is
    /// renamed away, and only then does the index take its name. An
    /// interruption before the first rename leaves `sessions.json` to import
    /// again, and one before the second leaves `sessions.json.migrated`,
    /// which holds the same index.
    fn import(&self) -> Result<(), DatabaseError> {
        let building = self.root().join(NEW_DATABASE_FILE);
        // What an interrupted import left is rebuilt; a file that is not yet
        // a database at all is started over.
        let db = match builder().create(&building) {
            Err(DatabaseError::DatabaseAlreadyOpen) => {
                return Err(DatabaseError::DatabaseAlreadyOpen);
            }
            Err(_) => {
                fs::remove_file(&building)?;
                builder().create(&building)?
            }
            Ok(db) => db,
        };
        let legacy = self.root().join(LEGACY_INDEX);
        let migrated = self.root().join(MIGRATED_INDEX);
        let (file, source) = match read_legacy_index(&legacy)? {
            Legacy::Parsed(file) => (file, Some(&legacy)),
            Legacy::Corrupt => (IndexFile::default(), None),
            Legacy::Missing => match read_legacy_index(&migrated)? {
                Legacy::Parsed(file) => (file, None),
                Legacy::Missing | Legacy::Corrupt => (IndexFile::default(), None),
            },
        };
        let started = Instant::now();
        write_index(&db, &file)?;
        drop(db);
        if let Some(legacy) = source {
            fs::rename(legacy, &migrated)?;
        }
        fs::rename(&building, self.database_path())?;
        log::info!(
            "imported {} project(s) and {} session(s) into {DATABASE_FILE} in {:?}",
            file.projects.len(),
            file.sessions.len(),
            started.elapsed()
        );
        Ok(())
    }
}

fn read_meta<T: DeserializeOwned>(db: &Database, key: &str) -> io::Result<Option<T>> {
    let txn = db.begin_read().map_err(other)?;
    let table = match txn.open_table(META) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(error) => return Err(other(error)),
    };
    let Some(value) = table.get(key).map_err(other)? else {
        return Ok(None);
    };
    decode(value.value()).map(Some).map_err(other)
}

fn write_meta<T: Serialize>(db: &Database, key: &str, value: &T) -> io::Result<()> {
    let txn = db.begin_write().map_err(other)?;
    txn.open_table(META)
        .map_err(other)?
        .insert(key, encode(value)?.as_slice())
        .map_err(other)?;
    txn.commit().map_err(other)
}

/// What a legacy JSON index file held.
enum Legacy {
    Missing,
    /// It did not parse and was kept beside itself as `<name>.corrupt-<ns>`.
    Corrupt,
    Parsed(IndexFile),
}

/// The legacy index, parsed as the JSON store parsed it: the current object
/// or the older bare array, projects derived for orphan sessions.
fn read_legacy_index(path: &Path) -> io::Result<Legacy> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Legacy::Missing),
        Err(error) => return Err(error),
    };
    let parsed = serde_json::from_slice::<IndexFile>(&bytes).or_else(|_| {
        serde_json::from_slice::<Vec<SessionMeta>>(&bytes).map(|sessions| IndexFile {
            projects: Vec::new(),
            sessions,
        })
    });
    match parsed {
        Ok(file) => Ok(Legacy::Parsed(migrate_index(file, &HashSet::new()))),
        Err(err) => {
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0);
            let mut corrupt = path.as_os_str().to_owned();
            corrupt.push(format!(".corrupt-{timestamp}"));
            fs::rename(path, &corrupt)?;
            log::warn!(
                "failed to parse {}: {err}; preserved it as {}",
                path.display(),
                Path::new(&corrupt).display()
            );
            Ok(Legacy::Corrupt)
        }
    }
}

/// Replace every row with `file`'s in one transaction, then read them back.
fn write_index(db: &Database, file: &IndexFile) -> io::Result<()> {
    let projects: HashMap<&str, &Project> = file
        .projects
        .iter()
        .map(|project| (project.id.as_str(), project))
        .collect();
    let sessions: HashMap<&str, &SessionMeta> = file
        .sessions
        .iter()
        .map(|meta| (meta.id.as_str(), meta))
        .collect();
    let txn = db.begin_write().map_err(other)?;
    for table in [PROJECTS, SESSIONS] {
        txn.delete_table(table).map_err(other)?;
    }
    {
        let mut table = txn.open_table(PROJECTS).map_err(other)?;
        for (id, project) in &projects {
            table
                .insert(*id, encode(project)?.as_slice())
                .map_err(other)?;
        }
        let mut table = txn.open_table(SESSIONS).map_err(other)?;
        for (id, meta) in &sessions {
            table.insert(*id, encode(meta)?.as_slice()).map_err(other)?;
        }
    }
    txn.commit().map_err(other)?;

    let txn = db.begin_read().map_err(other)?;
    let stored_projects = read_rows::<Project>(&txn, PROJECTS)?;
    let stored_sessions = read_rows::<SessionMeta>(&txn, SESSIONS)?;
    let matches = stored_projects.undecodable.is_empty()
        && stored_sessions.undecodable.is_empty()
        && stored_projects.rows.len() == projects.len()
        && stored_sessions.rows.len() == sessions.len()
        && stored_projects
            .rows
            .iter()
            .all(|row| projects.get(row.id.as_str()) == Some(&row))
        && stored_sessions
            .rows
            .iter()
            .all(|row| sessions.get(row.id.as_str()) == Some(&row));
    if matches {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the imported index does not read back as it was written",
        ))
    }
}

struct Rows<T> {
    rows: Vec<T>,
    /// Ids of the rows that did not decode.
    undecodable: Vec<String>,
}

trait Row: DeserializeOwned {
    fn id(&self) -> &str;
}

impl Row for Project {
    fn id(&self) -> &str {
        &self.id
    }
}

impl Row for SessionMeta {
    fn id(&self) -> &str {
        &self.id
    }
}

fn read_rows<T: Row>(
    txn: &redb::ReadTransaction,
    definition: TableDefinition<&str, &[u8]>,
) -> io::Result<Rows<T>> {
    let mut rows = Rows {
        rows: Vec::new(),
        undecodable: Vec::new(),
    };
    let table = match txn.open_table(definition) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(rows),
        Err(error) => return Err(other(error)),
    };
    for entry in table.iter().map_err(other)? {
        let (key, value) = entry.map_err(other)?;
        let id = key.value();
        match decode::<T>(value.value()) {
            Ok(row) if row.id() == id => rows.rows.push(row),
            Ok(_) => {
                log::warn!("skipping {definition} row {id}: its value names another id");
                rows.undecodable.push(id.to_owned());
            }
            Err(error) => {
                log::warn!("skipping {definition} row {id}: {error}");
                rows.undecodable.push(id.to_owned());
            }
        }
    }
    Ok(rows)
}

/// Compact the file once it has grown past twice its size after the last
/// compaction. redb reuses freed pages and trims free space off the end of
/// the file when it closes, but only `compact` moves pages to return free
/// space in the middle, and it cannot tell cheaply when that is worth doing.
/// Sizes are taken as a close left the file, before it is opened again, so a
/// compaction's size is recorded by the open after it.
fn compact_if_grown(db: &mut Database, path: &Path, size: u64) -> io::Result<()> {
    match read_meta::<u64>(db, COMPACTED_SIZE)? {
        Some(compacted) if size <= compacted.saturating_mul(2) => Ok(()),
        Some(_) => {
            let started = Instant::now();
            db.compact().map_err(other)?;
            log::info!(
                "compacted {} of {size} bytes in {:?}",
                path.display(),
                started.elapsed()
            );
            let txn = db.begin_write().map_err(other)?;
            txn.open_table(META)
                .map_err(other)?
                .remove(COMPACTED_SIZE)
                .map_err(other)?;
            txn.commit().map_err(other)
        }
        // A new file has no size a close left yet.
        None if size == 0 => Ok(()),
        None => write_meta(db, COMPACTED_SIZE, &size),
    }
}

impl SessionIndex {
    /// Every row this build can decode, with a project derived for each
    /// session that has none; the derived projects and reassigned sessions
    /// are written back so their ids stay stable. A row that does not decode
    /// is logged and left in the index as it is.
    pub fn load(&self) -> io::Result<LoadedIndex> {
        let txn = self.db.begin_read().map_err(other)?;
        let projects = read_rows::<Project>(&txn, PROJECTS)?;
        let sessions = read_rows::<SessionMeta>(&txn, SESSIONS)?;
        drop(txn);
        let before: HashMap<String, Option<String>> = sessions
            .rows
            .iter()
            .map(|meta| (meta.id.clone(), meta.project_id.clone()))
            .collect();
        let known_projects = projects.rows.len();
        let foreign = projects.undecodable.into_iter().collect();
        let file = migrate_index(
            IndexFile {
                projects: projects.rows,
                sessions: sessions.rows,
            },
            &foreign,
        );
        let reassigned: Vec<SessionMeta> = file
            .sessions
            .iter()
            .filter(|meta| before.get(&meta.id) != Some(&meta.project_id))
            .cloned()
            .collect();
        if !reassigned.is_empty() {
            let mut writes: Vec<IndexWrite> = file.projects[known_projects..]
                .iter()
                .cloned()
                .map(IndexWrite::UpsertProject)
                .collect();
            writes.push(IndexWrite::UpsertSessions(reassigned));
            self.commit(&writes)?;
        }
        Ok(LoadedIndex {
            projects: file.projects,
            sessions: file.sessions,
        })
    }

    /// Apply `writes` in one durable transaction, then delete the managed
    /// icons of projects it removed or gave another icon.
    pub fn commit(&self, writes: &[IndexWrite]) -> io::Result<()> {
        let txn = self.db.begin_write().map_err(other)?;
        let mut replaced_icons = Vec::new();
        {
            let mut projects = txn.open_table(PROJECTS).map_err(other)?;
            let mut sessions = txn.open_table(SESSIONS).map_err(other)?;
            let previous_icon = |bytes: Option<&[u8]>| {
                bytes
                    .and_then(|bytes| decode::<Project>(bytes).ok())
                    .and_then(|project| project.icon_path)
            };
            for write in writes {
                match write {
                    IndexWrite::UpsertSessions(metas) => {
                        for meta in metas {
                            sessions
                                .insert(meta.id.as_str(), encode(meta)?.as_slice())
                                .map_err(other)?;
                        }
                    }
                    IndexWrite::UpsertProject(project) => {
                        let old = projects
                            .insert(project.id.as_str(), encode(project)?.as_slice())
                            .map_err(other)?;
                        if let Some(icon) = previous_icon(old.as_ref().map(|old| old.value()))
                            && project.icon_path.as_ref() != Some(&icon)
                        {
                            replaced_icons.push(icon);
                        }
                    }
                    IndexWrite::RemoveSessions(ids) => {
                        for id in ids {
                            sessions.remove(id.as_str()).map_err(other)?;
                        }
                    }
                    IndexWrite::RemoveProject(id) => {
                        let old = projects.remove(id.as_str()).map_err(other)?;
                        replaced_icons.extend(previous_icon(old.as_ref().map(|old| old.value())));
                    }
                }
            }
        }
        txn.commit().map_err(other)?;
        for icon in replaced_icons {
            self.store.remove_project_icon(icon);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::{ApprovalMode, InteractionMode, ProviderKind};

    struct DataDir(SessionStore);

    impl DataDir {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("tcode-index-test-{}", uuid::Uuid::new_v4()));
            Self(SessionStore::open_at(root).unwrap())
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.root().join(name)
        }

        fn open(&self) -> SessionIndex {
            self.0.open_index(Duration::ZERO).unwrap()
        }

        /// Open the file as another build or a crash would have left it.
        fn raw(&self) -> Database {
            builder().create(self.path(DATABASE_FILE)).unwrap()
        }
    }

    impl Drop for DataDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(self.0.root());
        }
    }

    fn raw_row(db: &Database, table: TableDefinition<&str, &[u8]>, id: &str) -> Option<Vec<u8>> {
        let txn = db.begin_read().unwrap();
        let table = txn.open_table(table).unwrap();
        table.get(id).unwrap().map(|value| value.value().to_vec())
    }

    fn put_raw(db: &Database, table: TableDefinition<&str, &[u8]>, id: &str, value: &[u8]) {
        let txn = db.begin_write().unwrap();
        txn.open_table(table).unwrap().insert(id, value).unwrap();
        txn.commit().unwrap();
    }

    fn sorted(mut loaded: LoadedIndex) -> LoadedIndex {
        loaded.projects.sort_by(|a, b| a.id.cmp(&b.id));
        loaded.sessions.sort_by(|a, b| a.id.cmp(&b.id));
        loaded
    }

    #[test]
    fn legacy_index_files_import_once_and_the_database_stays_the_truth() {
        let bare_array = r#"[
            {"id": "s1", "title": "One", "provider": "claude_code",
             "cwd": "/work/alpha", "created_at": 1, "updated_at": 10},
            {"id": "s2", "title": "Two", "provider": "codex",
             "cwd": "/work/alpha", "created_at": 2, "updated_at": 20},
            {"id": "s3", "title": "Three", "provider": "codex",
             "cwd": "/work/beta", "created_at": 3, "updated_at": 30}
        ]"#;
        let missing_defaults = r#"{"sessions": [
            {"id": "s1", "title": "One", "provider": "codex", "cwd": "/work/alpha",
             "project_id": "p", "created_at": 1, "updated_at": 10}
        ], "projects": [
            {"id": "p", "name": "alpha", "root": "/work/alpha", "created_at": 1}
        ]}"#;
        let removed_fields = r#"{"projects": [
            {"id": "p", "name": "alpha", "root": "/work/alpha", "created_at": 1}
        ], "sessions": [
            {"id": "s1", "title": "One", "provider": "codex", "cwd": "/work/alpha",
             "project_id": "p", "created_at": 1, "updated_at": 10,
             "checkpoints": [{"turn_id": "t", "ref": "abc"}], "forked_from": "s0"}
        ]}"#;
        for (name, legacy) in [
            ("bare array", bare_array),
            ("missing defaults", missing_defaults),
            ("removed fields", removed_fields),
        ] {
            let dir = DataDir::new();
            fs::write(dir.path(LEGACY_INDEX), legacy).unwrap();
            let index = dir.open();
            let loaded = sorted(index.load().unwrap());
            drop(index);

            assert!(!dir.path(LEGACY_INDEX).exists(), "{name}");
            assert_eq!(
                fs::read_to_string(dir.path(MIGRATED_INDEX)).unwrap(),
                legacy,
                "{name}"
            );
            let titles: Vec<&str> = loaded.sessions.iter().map(|s| s.title.as_str()).collect();
            match name {
                "bare array" => {
                    assert_eq!(titles, ["One", "Two", "Three"]);
                    // Two distinct roots -> two derived projects, deduped by root.
                    let alpha = loaded
                        .projects
                        .iter()
                        .find(|p| p.root == Path::new("/work/alpha"))
                        .unwrap();
                    assert_eq!(alpha.name, "alpha");
                    assert_eq!(loaded.projects.len(), 2);
                    assert_eq!(loaded.sessions[0].project_id.as_ref(), Some(&alpha.id));
                    assert_eq!(loaded.sessions[1].project_id.as_ref(), Some(&alpha.id));
                    assert_ne!(loaded.sessions[2].project_id.as_ref(), Some(&alpha.id));
                }
                _ => {
                    assert_eq!(titles, ["One"], "{name}");
                    let session = &loaded.sessions[0];
                    assert_eq!(session.project_id.as_deref(), Some("p"), "{name}");
                    assert_eq!(session.approval_mode, ApprovalMode::default(), "{name}");
                    assert_eq!(session.interaction_mode, InteractionMode::default());
                    assert!(session.option_selections.is_empty(), "{name}");
                    assert_eq!(loaded.projects.len(), 1, "{name}");
                }
            }

            // An older build run in between writes a sessions.json of its own.
            fs::write(dir.path(LEGACY_INDEX), "[]").unwrap();
            let reloaded = sorted(dir.open().load().unwrap());
            assert_eq!(reloaded.projects, loaded.projects, "{name}");
            assert_eq!(reloaded.sessions, loaded.sessions, "{name}");
            assert_eq!(fs::read(dir.path(LEGACY_INDEX)).unwrap(), b"[]", "{name}");
        }
    }

    #[test]
    fn an_interrupted_import_is_redone_from_the_file_still_holding_the_index() {
        let legacy = r#"{"projects": [
            {"id": "p", "name": "alpha", "root": "/work/alpha", "created_at": 1}
        ], "sessions": [
            {"id": "s1", "title": "One", "provider": "codex", "cwd": "/work/alpha",
             "project_id": "p", "created_at": 1, "updated_at": 10},
            {"id": "s2", "title": "Two", "provider": "codex", "cwd": "/work/alpha",
             "project_id": "p", "created_at": 2, "updated_at": 20}
        ]}"#;
        let dir = DataDir::new();
        fs::write(dir.path(LEGACY_INDEX), legacy).unwrap();
        let imported = sorted(dir.open().load().unwrap());

        let building = dir.path(NEW_DATABASE_FILE);
        let partial = || {
            let db = builder().create(&building).unwrap();
            put_raw(&db, SESSIONS, "stray", b"{}");
        };
        // Each state an interrupted import leaves: a file redb had begun to
        // create, or one holding some rows, beside sessions.json; or the
        // same once sessions.json was renamed away.
        for (state, legacy_left) in [
            ("created", true),
            ("partial", true),
            ("created", false),
            ("partial", false),
        ] {
            fs::remove_file(dir.path(DATABASE_FILE)).unwrap();
            match state {
                "created" => fs::write(&building, b"redb").unwrap(),
                _ => partial(),
            }
            if legacy_left {
                fs::rename(dir.path(MIGRATED_INDEX), dir.path(LEGACY_INDEX)).unwrap();
            }
            let redone = sorted(dir.open().load().unwrap());
            assert_eq!(redone.projects, imported.projects, "{state} {legacy_left}");
            assert_eq!(redone.sessions, imported.sessions, "{state} {legacy_left}");
            assert!(!building.exists());
            assert!(!dir.path(LEGACY_INDEX).exists());
            assert_eq!(
                fs::read_to_string(dir.path(MIGRATED_INDEX)).unwrap(),
                legacy
            );
        }
    }

    #[test]
    fn an_unparseable_sessions_json_is_preserved_and_the_index_starts_empty() {
        let dir = DataDir::new();
        let corrupt = b"not valid session json";
        fs::write(dir.path(LEGACY_INDEX), corrupt).unwrap();
        let loaded = dir.open().load().unwrap();
        assert!(loaded.projects.is_empty() && loaded.sessions.is_empty());
        assert!(!dir.path(LEGACY_INDEX).exists());
        let backups: Vec<_> = fs::read_dir(dir.0.root())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("sessions.json.corrupt-")
            })
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::read(backups[0].path()).unwrap(), corrupt);
    }

    #[test]
    fn rows_this_build_cannot_decode_are_skipped_and_kept() {
        let dir = DataDir::new();
        let project = Project::from_root(PathBuf::from("/work/alpha"));
        let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/work/alpha"), None);
        meta.project_id = Some(project.id.clone());
        let mut child = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/work/beta"), None);
        child.project_id = Some("newer-project".into());
        let index = dir.open();
        index
            .commit(&[
                IndexWrite::UpsertProject(project.clone()),
                IndexWrite::UpsertSessions(vec![meta.clone(), child.clone()]),
            ])
            .unwrap();
        drop(index);

        let newer_session = br#"{"id":"newer","title":"t","provider":"warp_drive","cwd":"/w","created_at":1,"updated_at":2}"#;
        let newer_project = br#"{"id":"newer-project","name":{"localized":true},"root":"/work/beta","created_at":1}"#;
        let db = dir.raw();
        put_raw(&db, SESSIONS, "newer", newer_session);
        put_raw(&db, PROJECTS, "newer-project", newer_project);
        drop(db);

        let index = dir.open();
        let loaded = sorted(index.load().unwrap());
        assert_eq!(loaded.projects, vec![project]);
        let mut expected = vec![meta.clone(), child];
        expected.sort_by(|a, b| a.id.cmp(&b.id));
        // The child of the unreadable project keeps it rather than being
        // given a derived one.
        assert_eq!(loaded.sessions, expected);
        meta.title = "renamed".into();
        index
            .commit(&[IndexWrite::UpsertSessions(vec![meta])])
            .unwrap();
        drop(index);

        let db = dir.raw();
        assert_eq!(raw_row(&db, SESSIONS, "newer").unwrap(), newer_session);
        assert_eq!(
            raw_row(&db, PROJECTS, "newer-project").unwrap(),
            newer_project
        );
    }

    #[test]
    fn a_second_host_is_refused_until_the_first_closes_the_index() {
        let dir = DataDir::new();
        let first = dir.open();
        let refused = dir.0.open_index(Duration::ZERO).err().unwrap();
        assert_eq!(refused.kind(), io::ErrorKind::ResourceBusy);
        // A relaunch waits for the instance it replaces to quit.
        let closing = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(first);
        });
        dir.0.open_index(Duration::from_secs(10)).unwrap();
        closing.join().unwrap();
    }

    #[test]
    fn replacing_or_removing_a_project_deletes_only_its_managed_icon() {
        let dir = DataDir::new();
        let index = dir.open();
        fs::create_dir(dir.path("project-icons")).unwrap();
        let mut project = Project::from_root(dir.path("project"));
        for (path, managed) in [
            (dir.path("project-icons/icon.png"), true),
            (dir.path("original.png"), false),
        ] {
            fs::write(&path, b"stored image").unwrap();
            project.icon_path = Some(path.clone());
            index
                .commit(&[IndexWrite::UpsertProject(project.clone())])
                .unwrap();
            project.icon_path = None;
            index
                .commit(&[IndexWrite::UpsertProject(project.clone())])
                .unwrap();
            assert_eq!(path.exists(), !managed);

            fs::write(&path, b"stored image").unwrap();
            project.icon_path = Some(path.clone());
            index
                .commit(&[
                    IndexWrite::UpsertProject(project.clone()),
                    IndexWrite::RemoveProject(project.id.clone()),
                ])
                .unwrap();
            assert!(index.load().unwrap().projects.is_empty());
            assert_eq!(path.exists(), !managed);
        }
    }

    #[test]
    fn the_file_is_compacted_at_open_once_it_has_doubled() {
        let dir = DataDir::new();
        let size = || fs::metadata(dir.path(DATABASE_FILE)).unwrap().len();
        let project = Project::from_root(PathBuf::from("/work"));
        let session = |id: String, title: String| {
            let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/work"), None);
            meta.id = id;
            meta.project_id = Some(project.id.clone());
            meta.title = title;
            meta
        };
        let index = dir.open();
        index
            .commit(&[IndexWrite::UpsertProject(project.clone())])
            .unwrap();
        drop(index);
        let index = dir.open();
        let compacted = size();
        // Kept rows written between large ones pin the pages the large ones
        // leave free once they are removed.
        let mut kept = Vec::new();
        let mut removed = Vec::new();
        for n in 0..32 {
            let large = session(format!("{n:02}-large"), "x".repeat(64 * 1024));
            let small = session(format!("{n:02}-kept"), "kept".into());
            index
                .commit(&[IndexWrite::UpsertSessions(vec![
                    large.clone(),
                    small.clone(),
                ])])
                .unwrap();
            removed.push(large.id);
            kept.push(small);
        }
        index
            .commit(&[IndexWrite::RemoveSessions(removed)])
            .unwrap();
        drop(index);
        let grown = size();
        assert!(grown > compacted * 2);

        let index = dir.open();
        assert!(size() < grown / 2);
        kept.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(sorted(index.load().unwrap()).sessions, kept);
    }
}
