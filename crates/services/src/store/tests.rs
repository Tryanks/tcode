use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;

use agent::{ProviderCommand, ProviderCommandKind, TurnStatus};
use tcode_core::project::SettledOverride;

use std::sync::atomic::AtomicBool;

use super::*;

/// A data dir that is removed when the test ends.
struct DataDir(PathBuf);

impl DataDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("tcode-store-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn store(&self) -> SessionStore {
        SessionStore::open_at(self.0.clone()).unwrap()
    }

    /// A store over this data dir after the migration a start runs first.
    fn migrated(&self) -> SessionStore {
        let store = self.store();
        assert_eq!(migrate(&store).unwrap(), Migration::Completed);
        store
    }
}

fn migrate(store: &SessionStore) -> io::Result<Migration> {
    store.migrate(|_| {}, &AtomicBool::new(false))
}

impl Drop for DataDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A log as older builds wrote it, with every irregularity a real one can
/// hold: a legacy bare event, CRLF, a blank line, an unparseable line, a line
/// that is not UTF-8, and a torn last line without its newline.
const MIXED_LOG: &[u8] = b"{\"type\":\"turn_started\",\"turn_id\":\"legacy\"}\n\
{\"ts\":2000,\"event\":{\"type\":\"turn_completed\",\"turn_id\":\"legacy\",\"status\":\"completed\",\"usage\":null}}\r\n\
\n\
{not valid json}\n\
\xff\xfe{\"type\":\"turn_started\",\"turn_id\":\"not-utf8\"}\n\
{\"ts\":2500,\"event\":{\"type\":\"turn_started\",\"turn_id\":\"second\"}}\n\
{\"type\":\"turn_started";

const ORPHAN_LOG: &[u8] =
    b"{\"ts\":1,\"event\":{\"type\":\"turn_started\",\"turn_id\":\"orphan\"}}\n";

/// `sessions.json` in the current object schema, with one session that
/// carries fields older builds wrote and this one dropped, and one that has
/// no project yet and was settled before settling recorded an override.
fn legacy_index() -> serde_json::Value {
    serde_json::json!({
        "projects": [
            {"id": "p1", "name": "alpha", "root": "/work/alpha", "created_at": 1}
        ],
        "sessions": [
            {
                "id": "mixed", "title": "Mixed", "provider": "codex", "cwd": "/work/alpha",
                "project_id": "p1", "created_at": 1, "updated_at": 30,
                "forked_from": "source",
                "checkpoints": [{"turn": 2, "commit": "deadbeef", "event_offset": 7}]
            },
            {
                "id": "empty", "title": "Empty", "provider": "claude_code",
                "cwd": "/work/beta", "created_at": 2, "updated_at": 20, "settled_at": 15
            }
        ]
    })
}

fn write_legacy_fixture(root: &Path) {
    fs::write(root.join("sessions.json"), legacy_index().to_string()).unwrap();
    fs::write(root.join("mixed.jsonl"), MIXED_LOG).unwrap();
    fs::write(root.join("empty.jsonl"), b"").unwrap();
    fs::write(root.join("orphan.jsonl"), ORPHAN_LOG).unwrap();
}

const LEGACY_FILES: [&str; 4] = [
    "sessions.json",
    "mixed.jsonl",
    "empty.jsonl",
    "orphan.jsonl",
];

fn turn_ids(events: &[StoredEvent]) -> Vec<(Option<u64>, String)> {
    events
        .iter()
        .map(|stored| {
            let id = match &stored.event {
                AgentEvent::TurnStarted { turn_id } => format!("started {turn_id}"),
                AgentEvent::TurnCompleted {
                    turn_id,
                    status: TurnStatus::Completed,
                    ..
                } => format!("completed {turn_id}"),
                other => format!("{other:?}"),
            };
            (stored.ts, id)
        })
        .collect()
}

/// What the fixture must read back as, wherever it was migrated.
fn assert_fixture_migrated(store: &SessionStore, root: &Path) {
    let file = store.read_file().unwrap();
    assert_eq!(file.projects.len(), 2);
    assert_eq!(file.projects[0].id, "p1");
    let derived = &file.projects[1];
    assert_eq!(derived.root, Path::new("/work/beta"));
    assert_eq!(derived.name, "beta");
    assert_eq!(
        file.sessions
            .iter()
            .map(|meta| (meta.id.as_str(), meta.project_id.as_deref()))
            .collect::<Vec<_>>(),
        [("mixed", Some("p1")), ("empty", Some(derived.id.as_str()))]
    );
    let mixed = &file.sessions[0];
    assert_eq!(mixed.title, "Mixed");
    assert_eq!(mixed.archived_at, None);
    assert_eq!(mixed.settled_override, None);
    let empty = &file.sessions[1];
    assert_eq!(empty.settled_at, Some(15));
    assert_eq!(empty.settled_override, Some(SettledOverride::Settled));

    assert_eq!(store.read_event_log("mixed").unwrap(), MIXED_LOG);
    assert_eq!(store.read_event_log("orphan").unwrap(), ORPHAN_LOG);
    assert!(store.read_event_log("empty").unwrap().is_empty());
    assert_eq!(
        turn_ids(&store.read_events("mixed").unwrap()),
        [
            (None, "started legacy".to_owned()),
            (Some(2000), "completed legacy".to_owned()),
            (Some(2500), "started second".to_owned()),
        ]
    );
    assert_eq!(
        turn_ids(&store.read_events("orphan").unwrap()),
        [(Some(1), "started orphan".to_owned())]
    );

    for name in LEGACY_FILES {
        assert!(!root.join(name).exists(), "{name} left in the data dir");
    }
    for leftover in [
        LEGACY_DIR_NAME,
        "tcode.db.migrating",
        "tcode.db.migrating-wal",
        "tcode.db.archive",
    ] {
        assert!(!root.join(leftover).exists(), "{leftover} left behind");
    }
}

#[test]
fn pinned_history_cuts_survive_mutation_and_close_releases_readers() {
    use std::borrow::Cow;
    let dir = DataDir::new();
    let store = dir.store();
    let old = AgentEvent::Warning {
        message: "old".repeat(1000),
    };
    store
        .apply(&[Mutation::append_event("thread", 1, &old).unwrap()])
        .unwrap();
    let cut = store.snapshot_log("thread").unwrap();
    store
        .append_event(
            "thread",
            2,
            &AgentEvent::Warning {
                message: "appended".into(),
            },
        )
        .unwrap();
    assert_eq!(cut.end, 1);
    assert_eq!(
        cut.index,
        Some(TurnIndex {
            turns: 0,
            starts: vec![]
        })
    );
    let mut seen = Vec::new();
    cut.visit_projected_rows(
        0..u64::MAX,
        false,
        "fixture-preview",
        128,
        |record| {
            Cow::Owned(StoredEvent {
                event: AgentEvent::Warning {
                    message: "old-preview".into(),
                },
                ..record.clone()
            })
        },
        |row, record| {
            seen.push((row, record.event));
            true
        },
    )
    .unwrap();
    assert_eq!(
        seen,
        [(
            0,
            AgentEvent::Warning {
                message: "old-preview".into()
            }
        )]
    );
    let publication = cut.take_publication();
    store
        .apply(&[Mutation::replace_event_log(
            "thread",
            b"{\"type\":\"warning\",\"message\":\"replacement\"}\n".to_vec(),
        )])
        .unwrap();
    publication.publish().unwrap();
    let current = store.snapshot_log("thread").unwrap();
    let mut seen = Vec::new();
    current
        .visit_projected_rows(
            0..current.end,
            false,
            "fixture-preview",
            128,
            |record| Cow::Borrowed(record),
            |_, record| {
                seen.push(record.event);
                true
            },
        )
        .unwrap();
    assert_eq!(
        seen,
        [AgentEvent::Warning {
            message: "replacement".into()
        }]
    );
    store.close().unwrap();
    assert!(
        current
            .visit_projected_rows(
                0..current.end,
                false,
                "fixture-preview",
                128,
                |record| Cow::Borrowed(record),
                |_, _| true
            )
            .is_err()
    );
}

#[test]
fn first_start_migrates_the_json_index_and_every_log_byte_for_byte() {
    let dir = DataDir::new();
    write_legacy_fixture(dir.path());
    let store = dir.store();
    assert!(store.needs_migration().unwrap());
    let error = store.open().unwrap_err();
    assert!(error.to_string().contains("not been migrated"), "{error}");
    assert_eq!(migrate(&store).unwrap(), Migration::Completed);
    assert!(!store.needs_migration().unwrap());
    store.open().unwrap();
    assert_fixture_migrated(&store, dir.path());

    // Appending to a log that lost its final newline terminates that line
    // first, as appending to the file did.
    store
        .append_event(
            "mixed",
            3000,
            &AgentEvent::TurnStarted {
                turn_id: "next".into(),
            },
        )
        .unwrap();
    let mut expected = MIXED_LOG.to_vec();
    expected.extend_from_slice(
        b"\n{\"ts\":3000,\"event\":{\"type\":\"turn_started\",\"turn_id\":\"next\"}}\n",
    );
    assert_eq!(store.read_event_log("mixed").unwrap(), expected);
    store.close().unwrap();
    assert!(store.read_file().is_err(), "a closed store serves nothing");

    // A later start opens the database as it is: the data dir now holds only
    // what an older build would write, and that is ignored.
    fs::write(dir.path().join("sessions.json"), b"[]").unwrap();
    fs::write(dir.path().join("mixed.jsonl"), b"").unwrap();
    let reopened = dir.store();
    assert!(!reopened.needs_migration().unwrap());
    assert_eq!(reopened.read_event_log("mixed").unwrap(), expected);
    assert_eq!(reopened.read_file().unwrap().sessions.len(), 2);
    assert_eq!(fs::read(dir.path().join("sessions.json")).unwrap(), b"[]");
}

#[test]
fn legacy_bare_array_index_migrates_with_one_derived_project_per_root() {
    let dir = DataDir::new();
    // Old-format file: a bare JSON array with no project_id fields.
    let legacy = serde_json::json!([
        {
            "id": "s1", "title": "One", "provider": "claude_code",
            "cwd": "/work/alpha", "created_at": 1, "updated_at": 10
        },
        {
            "id": "s2", "title": "Two", "provider": "codex",
            "cwd": "/work/alpha", "created_at": 2, "updated_at": 20
        },
        {
            "id": "s3", "title": "Three", "provider": "codex",
            "cwd": "/work/beta", "created_at": 3, "updated_at": 30
        }
    ]);
    fs::write(dir.path().join("sessions.json"), legacy.to_string()).unwrap();

    let file = dir.migrated().read_file().unwrap();
    // Two distinct roots -> two derived projects, deduped by root.
    assert_eq!(file.projects.len(), 2);
    let alpha = file
        .projects
        .iter()
        .find(|p| p.root == Path::new("/work/alpha"))
        .unwrap();
    assert_eq!(alpha.name, "alpha");
    let project_of = |id: &str| {
        file.sessions
            .iter()
            .find(|session| session.id == id)
            .unwrap()
            .project_id
            .clone()
    };
    assert_eq!(project_of("s1"), Some(alpha.id.clone()));
    assert_eq!(project_of("s2"), project_of("s1"));
    assert_ne!(project_of("s3"), project_of("s1"));
    // Newest first.
    assert_eq!(
        dir.store()
            .load_index()
            .unwrap()
            .iter()
            .map(|meta| meta.id.as_str())
            .collect::<Vec<_>>(),
        ["s3", "s2", "s1"]
    );
}

/// An index that parses as no schema stops the migration before anything is
/// written, so its threads' metadata is never migrated away.
#[test]
fn an_unparseable_index_stops_the_migration_and_changes_nothing() {
    let dir = DataDir::new();
    let corrupt = b"not valid session json";
    fs::write(dir.path().join("sessions.json"), corrupt).unwrap();
    fs::write(dir.path().join("orphan.jsonl"), ORPHAN_LOG).unwrap();
    let store = dir.store();
    let mut before = snapshot(dir.path());

    let error = migrate(&store).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{error}");
    let mut after = snapshot(dir.path());
    before.remove(LOCK_FILE);
    after.remove(LOCK_FILE);
    assert_eq!(after, before);
}

const LEGACY_DIR_NAME: &str = "legacy";

fn archive_list(names: &[&str]) -> Vec<u8> {
    serde_json::to_vec(names).unwrap()
}

#[test]
fn a_start_after_an_interrupted_migration_discards_the_staging_files_and_starts_over() {
    // Killed while building: a torn staging file and WAL, no final database.
    let dir = DataDir::new();
    write_legacy_fixture(dir.path());
    fs::write(dir.path().join("tcode.db.migrating"), b"partial").unwrap();
    fs::write(dir.path().join("tcode.db.migrating-wal"), b"torn frames").unwrap();
    let store = dir.migrated();
    assert_fixture_migrated(&store, dir.path());
    store.close().unwrap();

    // Killed after the staged database was complete and the archive list was
    // written, but before the rename was durable: the sources are all still
    // in the data dir.
    fs::rename(
        dir.path().join(DB_FILE),
        dir.path().join("tcode.db.migrating"),
    )
    .unwrap();
    write_legacy_fixture(dir.path());
    fs::write(
        dir.path().join("tcode.db.archive"),
        archive_list(&LEGACY_FILES),
    )
    .unwrap();
    let store = dir.migrated();
    assert_fixture_migrated(&store, dir.path());
}

/// Every file in `root`, by name, with its bytes; `legacy/` as a marker.
/// The store's own files are recorded by name only: Windows locks the whole
/// of a file the store holds, so even the empty `tcode.lock` cannot be read.
fn snapshot(root: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    fs::read_dir(root)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            let bytes = if entry.file_type().unwrap().is_dir() {
                b"<dir>".to_vec()
            } else if name == LOCK_FILE || name.starts_with(DB_FILE) {
                b"<store file>".to_vec()
            } else {
                fs::read(entry.path()).unwrap()
            };
            (name, bytes)
        })
        .collect()
}

#[test]
fn a_cancelled_migration_changes_no_source_and_the_next_one_completes() {
    let dir = DataDir::new();
    write_legacy_fixture(dir.path());
    // Longer than one chunk, so a cancel can land inside a log.
    let bulk_line = format!(
        "{{\"ts\":1,\"event\":{{\"type\":\"turn_started\",\"turn_id\":\"{}\"}}}}\n",
        "x".repeat(4000)
    );
    let bulk = bulk_line.repeat(migrate::CHUNK_BYTES / bulk_line.len() + 1);
    assert!(bulk.len() > migrate::CHUNK_BYTES);
    fs::write(dir.path().join("bulk.jsonl"), &bulk).unwrap();
    let store = dir.store();
    assert!(store.needs_migration().unwrap());
    let mut sources = snapshot(dir.path());
    // Owned for the whole start, and kept from the first check.
    sources.remove(LOCK_FILE);

    type Stop = fn(&MigrationProgress) -> bool;
    let stops: [(&str, Stop); 3] = [
        ("scanning", |progress| {
            progress.phase == MigrationPhase::Scanning
        }),
        ("inside a log", |progress| {
            progress.phase == MigrationPhase::Importing
                && progress.bytes_done > 0
                && progress.threads_done == 0
        }),
        ("between logs", |progress| {
            progress.phase == MigrationPhase::Importing && progress.threads_done == 2
        }),
    ];
    for (point, stop) in stops {
        let cancel = AtomicBool::new(false);
        let mut seen = None;
        let outcome = store
            .migrate(
                |progress| {
                    if !cancel.load(Ordering::Relaxed) && stop(&progress) {
                        seen = Some(progress);
                        cancel.store(true, Ordering::Relaxed);
                    }
                },
                &cancel,
            )
            .unwrap();
        assert_eq!(outcome, Migration::Cancelled, "{point}");
        assert!(seen.is_some(), "{point}: never reached");
        let mut after = snapshot(dir.path());
        assert!(after.remove(LOCK_FILE).is_some());
        assert_eq!(after, sources, "{point}");
        assert!(store.needs_migration().unwrap(), "{point}");
    }

    let mut reports = Vec::new();
    let outcome = store
        .migrate(|progress| reports.push(progress), &AtomicBool::new(false))
        .unwrap();
    assert_eq!(outcome, Migration::Completed);
    let mut phases = reports
        .iter()
        .map(|progress| progress.phase)
        .collect::<Vec<_>>();
    phases.dedup();
    assert_eq!(
        phases,
        [
            MigrationPhase::Scanning,
            MigrationPhase::Importing,
            MigrationPhase::Publishing,
            MigrationPhase::Archiving
        ]
    );
    let log_bytes = (MIXED_LOG.len() + ORPHAN_LOG.len() + bulk.len()) as u64;
    let last = reports
        .iter()
        .rfind(|progress| progress.phase == MigrationPhase::Importing)
        .unwrap();
    assert_eq!(
        (
            last.threads_done,
            last.threads_total,
            last.bytes_done,
            last.bytes_total
        ),
        (4, 4, log_bytes, log_bytes)
    );
    let last = reports.last().unwrap();
    assert_eq!(
        (last.threads_done, last.bytes_done),
        (last.threads_total, last.bytes_total)
    );
    assert_eq!(store.read_event_log("bulk").unwrap(), bulk.as_bytes());
    assert_fixture_migrated(&store, dir.path());
}

#[test]
fn a_start_after_an_interrupted_archival_finishes_it_and_loses_no_source() {
    let dir = DataDir::new();
    write_legacy_fixture(dir.path());
    dir.migrated().close().unwrap();

    // Killed after the rename while moving the sources: two are still in the
    // data dir. One of them also has a copy in legacy/ already, so the source
    // stays where it is rather than replace it.
    fs::rename(
        dir.path().join("legacy/mixed.jsonl"),
        dir.path().join("mixed.jsonl"),
    )
    .unwrap();
    fs::copy(
        dir.path().join("legacy/orphan.jsonl"),
        dir.path().join("orphan.jsonl"),
    )
    .unwrap();
    fs::write(dir.path().join("legacy/orphan.jsonl"), b"older copy").unwrap();
    fs::write(
        dir.path().join("tcode.db.archive"),
        archive_list(&LEGACY_FILES),
    )
    .unwrap();

    let store = dir.store();
    store.open().unwrap();
    assert!(!dir.path().join("tcode.db.archive").exists());
    assert!(!dir.path().join("mixed.jsonl").exists());
    assert_eq!(
        fs::read(dir.path().join("orphan.jsonl")).unwrap(),
        ORPHAN_LOG
    );
    // Removed with the archived sources once the database opened.
    assert!(!dir.path().join(LEGACY_DIR_NAME).exists());
    assert_eq!(store.read_event_log("mixed").unwrap(), MIXED_LOG);
}

#[test]
fn a_fresh_data_dir_gets_a_completed_database_through_the_staged_path() {
    let dir = DataDir::new();
    let store = dir.store();
    assert!(store.load_index().unwrap().is_empty());
    store.close().unwrap();
    let db = db::Db::open(&dir.path().join(DB_FILE), None).unwrap();
    assert_eq!(db.user_version().unwrap(), SCHEMA_VERSION);
    drop(db);
    assert!(!dir.path().join("legacy").exists());
    assert!(!dir.path().join("tcode.db.migrating").exists());
}

#[test]
fn an_unfinished_newer_or_corrupt_database_is_refused_and_left_untouched() {
    let set_version = |root: &Path, version: i64| {
        let db = db::Db::open(&root.join(DB_FILE), None).unwrap();
        db.write(|db, connection| {
            db.execute(connection, &format!("PRAGMA user_version = {version}"), ())
                .map(drop)
        })
        .unwrap();
        db.checkpoint().unwrap();
    };
    for (version, expected) in [(0, "never completed"), (2, "newer Tcode")] {
        let dir = DataDir::new();
        dir.store()
            .upsert_meta(&SessionMeta::new(ProviderKind::Codex, "/w".into(), None))
            .unwrap();
        dir.store().close().unwrap();
        set_version(dir.path(), version);
        // The sources a migration archived stay until a database opens.
        fs::create_dir(dir.path().join(LEGACY_DIR_NAME)).unwrap();
        fs::write(dir.path().join("legacy/mixed.jsonl"), MIXED_LOG).unwrap();
        let before = fs::read(dir.path().join(DB_FILE)).unwrap();
        let error = dir.store().open().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains(expected), "{error}");
        assert_eq!(fs::read(dir.path().join(DB_FILE)).unwrap(), before);
        assert_eq!(
            fs::read(dir.path().join("legacy/mixed.jsonl")).unwrap(),
            MIXED_LOG
        );
    }

    let dir = DataDir::new();
    let garbage = vec![0x5a_u8; 8192];
    fs::write(dir.path().join(DB_FILE), &garbage).unwrap();
    let error = dir.store().open().unwrap_err();
    assert!(error.to_string().contains("sqlite3"), "{error}");
    assert!(error.to_string().contains(DB_FILE), "{error}");
    assert_eq!(fs::read(dir.path().join(DB_FILE)).unwrap(), garbage);
}

#[test]
fn session_index_upserts_orders_and_removes_only_the_selected_session() {
    let dir = DataDir::new();
    let store = dir.store();
    let mut a = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/a"), None);
    a.updated_at = 100;
    let mut b = SessionMeta::new(ProviderKind::ClaudeCode, PathBuf::from("/b"), None);
    b.updated_at = 200;
    store.upsert_meta(&a).unwrap();
    store.upsert_meta(&b).unwrap();

    let index = store.load_index().unwrap();
    assert_eq!(index.len(), 2);
    // newest first
    assert_eq!(index[0].id, b.id);
    assert_eq!(index[1].id, a.id);

    // upsert replaces
    let mut a2 = a.clone();
    a2.title = "renamed".into();
    store.upsert_meta(&a2).unwrap();
    let index = store.load_index().unwrap();
    assert_eq!(index.len(), 2);
    assert_eq!(
        index.iter().find(|m| m.id == a.id).unwrap().title,
        "renamed"
    );
    for (meta, ts) in [(&a, 1), (&b, 2)] {
        store
            .append_event(
                &meta.id,
                ts,
                &AgentEvent::TurnStarted {
                    turn_id: format!("turn-{ts}"),
                },
            )
            .unwrap();
    }
    store.remove_session(&a.id).unwrap();
    store.remove_session(&a.id).unwrap();
    let remaining = store.load_index().unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, b.id);
    assert!(store.read_event_log(&a.id).unwrap().is_empty());
    assert_eq!(
        store.read_event_log(&b.id).unwrap(),
        b"{\"ts\":2,\"event\":{\"type\":\"turn_started\",\"turn_id\":\"turn-2\"}}\n"
    );
}

#[test]
fn a_cloned_or_imported_log_keeps_its_exact_bytes() {
    let dir = DataDir::new();
    let store = dir.store();
    let authored = b"{\"ts\":3000,\"author\":{\"device_id\":\"phone\",\"name\":\"Alice\"},\"event\":{\"type\":\"turn_started\",\"turn_id\":\"shared\"}}\n";
    let mut log = MIXED_LOG.to_vec();
    log.push(b'\n');
    log.extend_from_slice(authored);
    let fork = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/w"), None);
    store
        .apply(&[
            Mutation::replace_event_log("source", log.clone()),
            Mutation::clone_events("source", &fork.id),
            Mutation::upsert_meta(fork.clone()),
            Mutation::clone_events("missing", "empty-fork"),
        ])
        .unwrap();
    assert_eq!(store.read_event_log("source").unwrap(), log);
    assert_eq!(store.read_event_log(&fork.id).unwrap(), log);
    let records = store.read_events(&fork.id).unwrap();
    assert_eq!(
        records.last().unwrap().author,
        Some(Author {
            device_id: "phone".into(),
            name: "Alice".into()
        })
    );
    assert!(
        records[..records.len() - 1]
            .iter()
            .all(|record| record.author.is_none())
    );
    assert!(store.read_event_log("empty-fork").unwrap().is_empty());
    assert_eq!(store.load_index().unwrap(), [fork]);
}

#[test]
fn removing_project_cleans_only_managed_icons() {
    let dir = DataDir::new();
    let root = dir.path();
    let store = dir.store();
    fs::create_dir(root.join("project-icons")).unwrap();
    let mut project = Project::from_root(root.join("project"));
    for (path, managed) in [
        (root.join("project-icons/icon.png"), true),
        (root.join("original.png"), false),
    ] {
        fs::write(&path, b"stored image").unwrap();
        project.icon_path = Some(path.clone());
        store.upsert_project(&project).unwrap();
        assert_eq!(
            store.read_file().unwrap().projects[0].icon_path.as_ref(),
            Some(&path)
        );
        store.remove_project(&project.id).unwrap();
        assert!(store.read_file().unwrap().projects.is_empty());
        assert_eq!(path.exists(), !managed);
    }
}

#[test]
fn replacing_a_project_icon_deletes_the_superseded_managed_copy() {
    let dir = DataDir::new();
    let root = dir.path();
    let store = dir.store();
    fs::create_dir(root.join("project-icons")).unwrap();
    let mut project = Project::from_root(root.join("project"));
    let first = root.join("project-icons/first.png");
    let second = root.join("project-icons/second.png");
    for path in [&first, &second] {
        fs::write(path, b"stored image").unwrap();
    }
    project.icon_path = Some(first.clone());
    store.upsert_project(&project).unwrap();
    project.icon_path = Some(second.clone());
    store.upsert_project(&project).unwrap();
    assert!(!first.exists());
    assert!(second.exists());
    project.icon_path = None;
    store.upsert_project(&project).unwrap();
    assert!(!second.exists());
    assert_eq!(store.read_file().unwrap().projects[0].icon_path, None);
}

#[test]
fn command_cache_is_kept_per_native_home_and_acp_agent() {
    let dir = DataDir::new();
    let root = dir.path().to_path_buf();
    let store = dir.store();
    let native = vec![ProviderCommand {
        name: "review".into(),
        description: Some("Review the current changes".into()),
        kind: ProviderCommandKind::Command,
    }];
    let acp = vec![ProviderCommand {
        name: "browser".into(),
        description: None,
        kind: ProviderCommandKind::Skill,
    }];
    let default_home = CommandsCacheKey::Native {
        provider: ProviderKind::ClaudeCode,
        home: None,
    };
    let shadow_home = CommandsCacheKey::Native {
        provider: ProviderKind::ClaudeCode,
        home: Some(PathBuf::from("/tmp/claude-shadow")),
    };
    let agent = CommandsCacheKey::Acp {
        agent_id: "vendor/agent".into(),
    };
    store.save_commands(&default_home, &native).unwrap();
    store.save_commands(&agent, &acp).unwrap();

    // Reopen the store to prove the values come from disk, not memory.
    let reopened = SessionStore::open_at(root.clone()).unwrap();
    assert_eq!(reopened.load_commands(&default_home), native);
    assert!(reopened.load_commands(&shadow_home).is_empty());
    assert_eq!(reopened.load_commands(&agent), acp);
    assert!(
        reopened
            .load_commands(&CommandsCacheKey::Acp {
                agent_id: "different-agent".into()
            })
            .is_empty()
    );
    assert!(
        root.join("commands-acp-76656e646f722f6167656e74.json")
            .is_file()
    );

    reopened.save_commands(&shadow_home, &native).unwrap();
    reopened.invalidate_commands(&default_home).unwrap();
    reopened.invalidate_commands(&default_home).unwrap();
    assert!(reopened.load_commands(&default_home).is_empty());
    assert_eq!(reopened.load_commands(&shadow_home), native);
}

/// This test binary, re-run as another process that starts as a host does —
/// with `TCODE_DATA_DIR` set to `root`, and `LEGACY_TCODE_DATA_DIR` to
/// `previous` if given — and holds the data dir until its stdin closes. It
/// reports the phases its preparation went through, then `OPEN` or
/// `ERR <kind> <message>`.
struct Child {
    process: std::process::Child,
    stdout: BufReader<std::process::ChildStdout>,
    /// Each phase by its `Debug` name.
    phases: Vec<String>,
    report: String,
}

impl Child {
    fn spawn(root: &Path, previous: Option<&Path>) -> std::process::Child {
        let mut command = crate::process::command(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "store::tests::store_owner_process",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("TCODE_DATA_DIR", root)
            .env_remove("LEGACY_TCODE_DATA_DIR");
        if let Some(previous) = previous {
            command.env("LEGACY_TCODE_DATA_DIR", previous);
        }
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    }

    /// Wait for the child to say whether it owns the data dir.
    fn report(mut process: std::process::Child) -> Self {
        let mut stdout = BufReader::new(process.stdout.take().unwrap());
        // libtest prints the test's name on the same line first.
        let mut line = String::new();
        let mut phases = Vec::new();
        let report = loop {
            line.clear();
            assert!(
                stdout.read_line(&mut line).unwrap() > 0,
                "the child exited without reporting"
            );
            if let Some((_, phase)) = line.trim_end().split_once(OWNER_PHASE) {
                phases.push(phase.to_owned());
            }
            if let Some((_, report)) = line.trim_end().split_once(OWNER_REPORT) {
                break report.to_owned();
            }
        };
        Self {
            process,
            stdout,
            phases,
            report,
        }
    }

    /// Close the child's stdin, which makes it close the store and exit.
    fn release(mut self) {
        drop(self.process.stdin.take());
        std::io::copy(&mut self.stdout, &mut std::io::sink()).unwrap();
        assert!(self.process.wait().unwrap().success());
    }
}

const OWNER_REPORT: &str = "store owner: ";
const OWNER_PHASE: &str = "store owner phase: ";

#[test]
#[ignore = "the other process of the ownership tests below, which run it"]
fn store_owner_process() {
    let store = SessionStore::open_host(None).unwrap();
    let opened = store.needs_migration().and_then(|needed| {
        if needed {
            let mut last = None;
            let outcome = store.migrate(
                |progress| {
                    if last != Some(progress.phase) {
                        println!("{OWNER_PHASE}{:?}", progress.phase);
                        last = Some(progress.phase);
                    }
                },
                &AtomicBool::new(false),
            )?;
            assert_eq!(outcome, Migration::Completed);
        }
        store.open()
    });
    match opened {
        Ok(()) => println!("{OWNER_REPORT}OPEN"),
        Err(error) => println!("{OWNER_REPORT}ERR {:?} {error}", error.kind()),
    }
    std::io::stdout().flush().unwrap();
    let _ = std::io::stdin().read(&mut [0_u8; 1]);
    store.close().unwrap();
}

#[test]
fn a_second_process_is_refused_and_a_relaunch_waits_for_the_owner_to_exit() {
    let dir = DataDir::new();
    let owner = Child::report(Child::spawn(dir.path(), None));
    assert_eq!(owner.report, "OPEN");

    let error = dir.store().open().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::ResourceBusy);
    assert!(
        error
            .to_string()
            .contains(&dir.path().display().to_string()),
        "{error}"
    );

    // A permission relaunch starts while the old instance is still closing.
    crate::relaunch::write(
        dir.path(),
        &crate::relaunch::RelaunchMarker {
            reopen_settings: "computer_use".into(),
            active_session: None,
        },
    )
    .unwrap();
    let waiting = std::thread::spawn({
        let store = dir.store();
        move || store.open()
    });
    owner.release();
    waiting.join().unwrap().unwrap();
    // Only peeked: the host consumes the marker once it owns the data dir.
    assert!(crate::relaunch::pending(dir.path()));
}

#[test]
fn two_simultaneous_first_launches_migrate_once() {
    let dir = DataDir::new();
    write_legacy_fixture(dir.path());
    let (first, second) = (
        Child::spawn(dir.path(), None),
        Child::spawn(dir.path(), None),
    );
    let (first, second) = (Child::report(first), Child::report(second));
    let mut reports = [first.report.clone(), second.report.clone()];
    reports.sort();
    assert_eq!(reports[1], "OPEN", "{reports:?}");
    assert!(reports[0].starts_with("ERR ResourceBusy"), "{reports:?}");
    first.release();
    second.release();

    let store = dir.store();
    assert_fixture_migrated(&store, dir.path());
}

/// Every file, directory and link under `root` by relative path: a file's
/// bytes, a link's target, or `<dir>`.
pub(super) fn tree(root: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let kind = fs::symlink_metadata(&path).unwrap().file_type();
            let relative = path.strip_prefix(root).unwrap().to_owned();
            if kind.is_symlink() {
                let target = fs::read_link(&path).unwrap();
                out.insert(relative, target.to_string_lossy().as_bytes().to_vec());
            } else if kind.is_dir() {
                out.insert(relative, b"<dir>".to_vec());
                walk(root, &path, out);
            } else {
                out.insert(relative, fs::read(&path).unwrap());
            }
        }
    }
    let mut out = std::collections::BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// An older build's data dir with every kind of entry it can hold.
pub(super) fn write_older_data_dir(previous: &Path) {
    fs::create_dir_all(previous.join("attachments/session-1")).unwrap();
    fs::create_dir_all(previous.join("project-icons")).unwrap();
    fs::create_dir_all(previous.join("profile-homes/claude/.claude")).unwrap();
    fs::create_dir_all(previous.join("legacy")).unwrap();
    fs::write(previous.join("settings.json"), br#"{"locale":"en"}"#).unwrap();
    fs::write(previous.join("traverse.json"), br#"{"key":"machine"}"#).unwrap();
    fs::write(
        previous.join("attachments/session-1/a.png"),
        [0x89, b'P', b'N', b'G'],
    )
    .unwrap();
    fs::write(previous.join("project-icons/p1.png"), b"icon").unwrap();
    fs::write(
        previous.join("profile-homes/claude/.claude/settings.json"),
        b"{}",
    )
    .unwrap();
    fs::write(previous.join("legacy/older.jsonl"), ORPHAN_LOG).unwrap();
    fs::write(previous.join(".DS_Store"), b"finder").unwrap();
}

#[test]
fn a_start_moves_an_older_data_dir_in_then_migrates_it_and_leaves_nothing_behind() {
    let home = DataDir::new();
    let previous = home.path().join("Application Support/tcode");
    let root = home.path().join(".tcode");
    write_older_data_dir(&previous);
    write_legacy_fixture(&previous);
    // Orchestrate's worktrees already live under ~/.tcode.
    fs::create_dir_all(root.join("worktrees/thread-1")).unwrap();
    fs::write(root.join("worktrees/thread-1/README"), b"checkout").unwrap();
    let mut expected = tree(&previous);
    expected.retain(|path, _| {
        !path.starts_with("legacy")
            && path != Path::new(".DS_Store")
            && !LEGACY_FILES.iter().any(|name| path == Path::new(name))
    });
    expected.extend(tree(&root));

    let owner = Child::report(Child::spawn(&root, Some(&previous)));
    assert_eq!(owner.report, "OPEN");
    assert_eq!(
        owner.phases,
        [
            MigrationPhase::Relocating,
            MigrationPhase::Scanning,
            MigrationPhase::Importing,
            MigrationPhase::Publishing,
            MigrationPhase::Archiving,
        ]
        .map(|phase| format!("{phase:?}"))
    );
    owner.release();

    assert!(!previous.exists(), "the older data dir is left behind");
    let mut after = tree(&root);
    // The store's own files, `tcode.db` among them.
    after.retain(|path, _| {
        path != Path::new(LOCK_FILE) && !path.to_string_lossy().starts_with(DB_FILE)
    });
    assert_eq!(after, expected);
    assert_fixture_migrated(&SessionStore::open_at(root.clone()).unwrap(), &root);
}

/// A Codex log as stored: every turn-changes snapshot carries the turn's whole
/// diff so far, so each one supersedes the one before it on its turn. One
/// legacy bare snapshot and a blank row are among them.
const SNAPSHOT_LOG: &[u8] = b"{\"ts\":1,\"event\":{\"type\":\"turn_started\",\"turn_id\":\"t1\"}}\n\
{\"type\":\"turn_changes_updated\",\"turn_id\":\"t1\",\"changes\":[{\"path\":\"f\",\"kind\":\"modify\",\"diff\":\"-a\\n+b\\n\"}],\"completeness\":\"exact\"}\n\
\n\
{\"ts\":3,\"event\":{\"type\":\"turn_changes_updated\",\"turn_id\":\"t1\",\"changes\":[{\"path\":\"f\",\"kind\":\"modify\",\"diff\":\"-a\\n+c\\n\"}],\"completeness\":\"exact\"}}\n\
{\"ts\":4,\"event\":{\"type\":\"turn_completed\",\"turn_id\":\"t1\",\"status\":\"completed\",\"usage\":null}}\n\
{\"ts\":5,\"event\":{\"type\":\"turn_started\",\"turn_id\":\"t2\"}}\n\
{\"author\":{\"device_id\":\"phone\",\"name\":\"Alice\"},\"ts\":6,\"event\":{\"type\":\"turn_changes_updated\",\"turn_id\":\"t2\",\"changes\":[{\"path\":\"g\",\"kind\":\"create\",\"diff\":\"+d\\n\"}],\"completeness\":\"exact\"}}\n\
{\"ts\":7,\"event\":{\"type\":\"turn_changes_updated\",\"turn_id\":\"t2\",\"changes\":[{\"path\":\"g\",\"kind\":\"create\",\"diff\":\"+e\\n\"}],\"completeness\":\"exact\"}}\n";

/// Run the compaction pass over one thread to its end.
fn compact(store: &SessionStore, id: &str) -> Compaction {
    let mut plan = store.plan_compaction(id).unwrap();
    loop {
        if let Some(outcome) = store.compact(&mut plan).unwrap() {
            return outcome;
        }
    }
}

fn rows(store: &SessionStore, id: &str) -> Vec<Vec<u8>> {
    store
        .read_event_log(id)
        .unwrap()
        .split_inclusive(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect()
}

/// The pass drops the diffs of exactly the snapshots a later one supersedes,
/// in the form each was stored in, without moving a row; the thread folds as
/// before, and it is not passed again until its rows are replaced or an
/// append leaves a superseded diff it could not name. Rows without a session
/// are never listed.
#[test]
fn superseded_snapshots_lose_their_diffs_in_place_once() {
    let dir = DataDir::new();
    let mut store = dir.store();
    let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/w"), None);
    meta.id = "codex".into();
    store
        .apply(&[
            Mutation::upsert_meta(meta),
            Mutation::replace_event_log("codex", SNAPSHOT_LOG.to_vec()),
            Mutation::replace_event_log("orphan", SNAPSHOT_LOG.to_vec()),
        ])
        .unwrap();
    let before = rows(&store, "codex");
    let folded = tcode_core::session::Timeline::fold_events(store.read_events("codex").unwrap());
    assert_eq!(store.threads_to_compact().unwrap(), ["codex"]);

    let Compaction::Compacted { rewritten, .. } = compact(&store, "codex") else {
        panic!("superseded snapshots lose their diffs")
    };
    assert_eq!(rewritten, 2);
    let after = rows(&store, "codex");
    let mut expected = before.clone();
    expected[1] = b"{\"type\":\"turn_changes_updated\",\"turn_id\":\"t1\",\"changes\":[{\"path\":\"f\",\"kind\":\"modify\",\"diff\":null}],\"completeness\":\"exact\"}\n".to_vec();
    expected[6] = b"{\"author\":{\"device_id\":\"phone\",\"name\":\"Alice\"},\"ts\":6,\"event\":{\"type\":\"turn_changes_updated\",\"turn_id\":\"t2\",\"changes\":[{\"path\":\"g\",\"kind\":\"create\",\"diff\":null}],\"completeness\":\"exact\"}}\n".to_vec();
    assert_eq!(after, expected);
    assert_eq!(
        tcode_core::session::Timeline::fold_events(store.read_events("codex").unwrap()),
        folded
    );
    assert!(store.threads_to_compact().unwrap().is_empty());
    assert_eq!(compact(&store, "codex"), Compaction::Unchanged);

    store.close().unwrap();
    store = dir.store();
    assert!(store.threads_to_compact().unwrap().is_empty());
    store.backfill_thread_activity(&["codex".into()]).unwrap();
    let snapshot = parse_stored_line(
        std::str::from_utf8(
            SNAPSHOT_LOG
                .split_inclusive(|byte| *byte == b'\n')
                .next_back()
                .unwrap(),
        )
        .unwrap(),
    )
    .unwrap()
    .event;
    store.append_event("codex", 8, &snapshot).unwrap();
    let snapshots: Vec<_> = store
        .read_events("codex")
        .unwrap()
        .into_iter()
        .filter_map(|record| match record.event {
            AgentEvent::TurnChangesUpdated { changes, .. } => Some(changes[0].diff.is_some()),
            _ => None,
        })
        .collect();
    assert_eq!(snapshots, [false, true, false, false, true]);
    assert!(store.threads_to_compact().unwrap().is_empty());
    let mut fresh = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/w"), None);
    fresh.id = "fresh".into();
    store.upsert_meta(&fresh).unwrap();
    store
        .append_event(
            "fresh",
            1,
            &AgentEvent::TurnStarted {
                turn_id: "fresh".into(),
            },
        )
        .unwrap();
    assert!(store.threads_to_compact().unwrap().is_empty());
    store
        .apply(&[Mutation::replace_event_log("codex", SNAPSHOT_LOG.to_vec())])
        .unwrap();
    assert_eq!(store.threads_to_compact().unwrap(), ["codex"]);
    compact(&store, "codex");
}

/// A tolerant read folds without a row it cannot decode, so it could name
/// the wrong snapshots: such a thread keeps every row as it is.
#[test]
fn a_thread_with_an_undecodable_row_keeps_every_row() {
    let dir = DataDir::new();
    let store = dir.store();
    let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/w"), None);
    meta.id = "codex".into();
    store.upsert_meta(&meta).unwrap();
    for (label, bad) in [
        ("unparseable", &b"{not valid json}\n"[..]),
        ("not UTF-8", &b"\xff\xfe{}\n"[..]),
    ] {
        let mut log = SNAPSHOT_LOG.to_vec();
        let at: usize = SNAPSHOT_LOG
            .split_inclusive(|byte| *byte == b'\n')
            .take(5)
            .map(<[u8]>::len)
            .sum();
        log.splice(at..at, bad.iter().copied());
        store
            .apply(&[Mutation::replace_event_log("codex", log.clone())])
            .unwrap();
        assert_eq!(store.threads_to_compact().unwrap(), ["codex"]);
        assert_eq!(
            compact(&store, "codex"),
            Compaction::Undecodable { position: 5 },
            "{label}"
        );
        assert_eq!(store.read_event_log("codex").unwrap(), log, "{label}");
        assert!(store.threads_to_compact().unwrap().is_empty(), "{label}");
    }
}

#[test]
fn appended_turn_boundaries_match_replay_across_restarts_imports_and_forks() {
    use tcode_core::session::Timeline;
    const LOG: &str = include_str!("fixtures/turn-index.jsonl");
    let dir = DataDir::new();
    let mut store = dir.store();
    let mut fold = Timeline::default();
    let mut starts = tcode_core::session::TurnStarts::default();
    for (row, line) in LOG.lines().enumerate() {
        let record = parse_stored_line(line).unwrap();
        starts.apply(&fold.apply_stored(&record), row as u64);
        let starts = starts.rows().to_vec();
        store
            .append_event("thread", row as u64, &record.event)
            .unwrap();
        assert_eq!(
            store.turn_index("thread").unwrap(),
            Some(TurnIndex {
                turns: fold.turns.len() as u64,
                starts: starts.clone(),
            }),
            "row {row}"
        );
        if row == 9 {
            store.close().unwrap();
            store = dir.store();
        }
    }
    let expected = TurnIndex {
        turns: 2,
        // The turns the fold holds: the divider's of the rewind at row 60
        // (to before the first turn) and of the one at row 70; a rewind's
        // row opens the turn its divider stands in, and the turns it took
        // out leave their starts behind.
        starts: vec![60, 70],
    };
    assert_eq!(store.turn_index("thread").unwrap(), Some(expected.clone()));
    store
        .apply(&[
            Mutation::replace_event_log("import", LOG.as_bytes().to_vec()),
            Mutation::clone_events("thread", "fork"),
        ])
        .unwrap();
    assert!(store.turn_index("import").unwrap().is_none());
    let mut imported = SessionMeta::new(ProviderKind::Codex, dir.path().to_owned(), None);
    imported.id = "import".into();
    store.upsert_meta(&imported).unwrap();
    store.backfill_thread_activity(&["import".into()]).unwrap();
    for id in ["import", "fork"] {
        assert_eq!(store.turn_index(id).unwrap(), Some(expected.clone()));
    }
    store
        .apply(&[
            Mutation::clone_events_to("thread", "before", CloneBound::BeforeTurn(1)),
            Mutation::clone_events_to("thread", "through", CloneBound::ThroughTurnOpening(1)),
        ])
        .unwrap();
    let source = rows(&store, "thread");
    for (id, end, starts) in [("before", 70, vec![60, 66]), ("through", 71, vec![60, 70])] {
        assert_eq!(rows(&store, id), source[..end], "{id}");
        assert_eq!(
            store.turn_index(id).unwrap(),
            Some(TurnIndex {
                turns: starts.len() as u64,
                starts,
            }),
            "{id}: its own index, which appends go on from"
        );
    }
    store
        .run("seed an older fold version", |st| {
            st.write(|st, connection| {
                st.execute(
                    connection,
                    "UPDATE turn_index SET fold_version = 0 \
                     WHERE thread = (SELECT key FROM threads WHERE id = 'thread')",
                    (),
                )?;
                Ok(())
            })
        })
        .unwrap();
    assert!(store.turn_index("thread").unwrap().is_none());
    let mut meta = SessionMeta::new(ProviderKind::Codex, dir.path().to_owned(), None);
    meta.id = "thread".into();
    store.upsert_meta(&meta).unwrap();
    store.backfill_thread_activity(&["thread".into()]).unwrap();
    assert_eq!(store.turn_index("thread").unwrap(), Some(expected));
    store.remove_session("thread").unwrap();
    assert!(store.turn_index("thread").unwrap().is_none());
}

/// Item rows as builds before v0.4.0 recorded them, in the shape tcode.db
/// holds them (invented content, 1x1 pictures): tool results in their
/// provider's shape, Codex items recorded as `Other`, a Codex file creation
/// whose diff is the whole file, and a Grok MCP call named `server__tool`.
/// Every row is read as a record, enters the timeline and search, and is
/// exported and read back.
#[test]
fn item_rows_recorded_before_canonical_items_are_read() {
    use std::collections::HashSet;
    let legacy = [
        include_str!("fixtures/legacy-tool-output.jsonl"),
        include_str!("fixtures/legacy-codex-items.jsonl"),
        "{\"ts\":1,\"event\":{\"type\":\"item_completed\",\"id\":\"add\",\"content\":{\"kind\":\"file_change\",\"status\":\"completed\",\"changes\":[{\"path\":\"a.md\",\"kind\":\"create\",\"diff\":\"- list\\n\"}]}}}\n",
        "{\"ts\":1,\"event\":{\"type\":\"item_completed\",\"id\":\"call_37_0\",\"content\":{\"kind\":\"tool_call\",\"name\":\"tcode_probe__echo_upper\",\"input\":{\"tool_name\":\"tcode_probe__echo_upper\",\"tool_input\":{\"text\":\"hello mcp\"}},\"output\":\"HELLO MCP\",\"status\":\"completed\"}}}\n",
    ]
    .concat();
    let dir = DataDir::new();
    let store = dir.store();
    let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/w"), None);
    meta.id = "legacy".into();
    store
        .apply(&[
            Mutation::upsert_meta(meta.clone()),
            Mutation::replace_event_log("legacy", legacy.clone().into_bytes()),
        ])
        .unwrap();
    let ids: Vec<String> = legacy
        .lines()
        .map(|line| {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            row["event"]["id"].as_str().unwrap().to_owned()
        })
        .collect();

    let log = store.read_log("legacy").unwrap();
    assert_eq!(log.undecodable, 0);
    let read: Vec<&str> = log
        .records
        .iter()
        .map(|record| match &record.event {
            AgentEvent::ItemCompleted(item) => item.id.as_str(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(read, ids);
    let distinct: HashSet<&str> = ids.iter().map(String::as_str).collect();
    let timeline = tcode_core::session::Timeline::fold_stored(&log.records);
    let shown: HashSet<&str> = timeline
        .entries
        .iter()
        .map(|entry| entry.id.as_str())
        .collect();
    assert_eq!(shown, distinct);
    let searched = crate::session_search::extract_searchable_entries(&log);
    assert_eq!(
        searched
            .iter()
            .map(|entry| entry.entry_id.as_str())
            .collect::<HashSet<_>>(),
        distinct
    );

    crate::export::render_thread(&store, &meta, tcode_protocol::ThreadExportFormat::Markdown)
        .unwrap();
    crate::export::render_thread(&store, &meta, tcode_protocol::ThreadExportFormat::Jsonl)
        .unwrap();
}

/// Every row of `id` as the store reads it: its position and canonical bytes,
/// and how many named a blob that is missing.
fn decoded_rows(store: &SessionStore, id: &str) -> (Vec<(u64, Vec<u8>)>, usize) {
    store
        .run("read rows", |st| {
            st.read(|st, connection| {
                let mut out = (Vec::new(), 0);
                let Some(thread) = rows::thread(st, connection, id)? else {
                    return Ok(out);
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
                        let decoded = st.decode(&mut decoder, &row, Mode::Full)?;
                        out.1 += usize::from(decoded.missing);
                        out.0.push((position, decoded.bytes));
                        Ok(true)
                    },
                )?;
                Ok(out)
            })
        })
        .unwrap()
}

fn fold_rows<'a>(rows: impl IntoIterator<Item = &'a [u8]>) -> tcode_core::session::Timeline {
    let mut fold = tcode_core::session::Timeline::default();
    for line in rows {
        if let Ok(Some(record)) = decode_row(line) {
            fold.apply_stored(&record);
        }
    }
    fold
}

/// Bytes standing in for an image: deterministic, and different per seed.
fn image_bytes(seed: u32, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(2_654_435_761) | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect()
}

/// `fixtures/legacy-images.jsonl` with its placeholders filled: runs long
/// enough to leave their rows (A–F), a short one (G) and a long run that is
/// not base64 (H), which stay.
fn images_log() -> Vec<u8> {
    use base64::Engine as _;
    let mut log = include_str!("fixtures/legacy-images.jsonl").to_owned();
    for (index, (name, len)) in [
        ("A", 10_000),
        ("B", 38_000),
        ("C", 30_000),
        ("D", 21_000),
        ("E", 5_000),
        ("F", 3_100),
        ("G", 600),
    ]
    .into_iter()
    .enumerate()
    {
        let run = base64::engine::general_purpose::STANDARD.encode(image_bytes(index as u32, len));
        log = log.replace(&format!("@{name}@"), &run);
    }
    log.replace("@H@", &"A".repeat(5_001)).into_bytes()
}

/// A thread that streams text and command output: items whose completion
/// repeats their deltas, one whose completion abbreviates them, and one that
/// never completes.
fn stream_log(turns: usize) -> Vec<u8> {
    let mut log = String::new();
    let mut ts = 1;
    let mut line = |event: String| {
        ts += 1;
        log.push_str(&format!("{{\"ts\":{ts},\"event\":{event}}}\n"));
    };
    for turn in 0..turns {
        line(format!(
            "{{\"type\":\"turn_started\",\"turn_id\":\"t{turn}\"}}"
        ));
        for text in ["Hel", "lo ", "world"] {
            line(format!(
                "{{\"type\":\"delta\",\"item_id\":\"a{turn}\",\"kind\":\"assistant_text\",\"text\":\"{text}\"}}"
            ));
        }
        line(format!(
            "{{\"type\":\"item_completed\",\"id\":\"a{turn}\",\"content\":{{\"kind\":\"assistant_message\",\"text\":\"Hello world\"}}}}"
        ));
        line(format!(
            "{{\"type\":\"item_started\",\"id\":\"c{turn}\",\"content\":{{\"kind\":\"command_execution\",\"command\":\"ls\",\"output\":\"\",\"exit_code\":null,\"status\":\"in_progress\"}}}}"
        ));
        for chunk in ["one\\n", "two\\n"] {
            line(format!(
                "{{\"type\":\"delta\",\"item_id\":\"c{turn}\",\"kind\":\"command_output\",\"text\":\"{chunk}\"}}"
            ));
        }
        let output = if turn % 2 == 0 {
            "one\\ntwo\\n"
        } else {
            "one… 4 bytes omitted"
        };
        line(format!(
            "{{\"type\":\"item_completed\",\"id\":\"c{turn}\",\"content\":{{\"kind\":\"command_execution\",\"command\":\"ls\",\"output\":\"{output}\",\"exit_code\":0,\"status\":\"completed\"}}}}"
        ));
        line(format!(
            "{{\"type\":\"delta\",\"item_id\":\"r{turn}\",\"kind\":\"reasoning_text\",\"text\":\"redacted\"}}"
        ));
        line(format!(
            "{{\"type\":\"turn_completed\",\"turn_id\":\"t{turn}\",\"status\":\"completed\",\"usage\":null}}"
        ));
    }
    log.into_bytes()
}

/// A data dir as an older build leaves it: `tcode.db` at `user_version` 1
/// holding `logs`, each thread with a session.
fn older_data_dir(logs: &[(&str, Vec<u8>)]) -> DataDir {
    let dir = DataDir::new();
    let db = db::Db::open(&dir.path().join(DB_FILE), Some(db::V1_SCHEMA)).unwrap();
    db.write(|db, connection| {
        db.execute(
            connection,
            "CREATE TABLE kept_worktrees (path TEXT PRIMARY KEY)",
            (),
        )?;
        for (id, log) in logs {
            let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/w"), None);
            meta.id = (*id).to_owned();
            db.execute(
                connection,
                "INSERT INTO sessions (id, body) VALUES (?1, ?2)",
                (*id, serde_json::to_vec(&meta).unwrap()),
            )?;
            for (position, line) in segments(log).enumerate() {
                db.execute(
                    connection,
                    "INSERT INTO events (session_id, position, line) VALUES (?1, ?2, ?3)",
                    (*id, position as i64, line),
                )?;
            }
        }
        db.execute(connection, "PRAGMA user_version = 1", ())
    })
    .unwrap();
    db.checkpoint().unwrap();
    dir
}

fn copy_data_dir(from: &DataDir) -> DataDir {
    let dir = DataDir::new();
    fs::copy(from.path().join(DB_FILE), dir.path().join(DB_FILE)).unwrap();
    dir
}

/// Run the storage pass's steps in the order the host does, committing at
/// most `limit` of them; returns how many it committed.
fn storage_pass(store: &SessionStore, limit: usize) -> usize {
    let committed = std::cell::Cell::new(0);
    let commit = |step: StorageStep| {
        if committed.get() == limit {
            return None;
        }
        committed.set(committed.get() + 1);
        Some(store.commit_storage_step(step).unwrap())
    };
    let work = store.storage_work().unwrap();
    if work.dictionary
        && let Some(step) = store.prepare_dictionary().unwrap()
        && commit(step).is_none()
    {
        return committed.get();
    }
    for id in &work.moves {
        let Some(mut plan) = store.plan_move(id).unwrap() else {
            continue;
        };
        while let Ok(step) = store.prepare_move_chunk(&mut plan).unwrap() {
            match commit(step) {
                None => return committed.get(),
                Some(StepOutcome::Moved { done: false, .. }) => {}
                Some(_) => break,
            }
        }
    }
    store.retire_source().unwrap();
    for id in store.threads_to_compact().unwrap() {
        let mut plan = store.plan_compaction(&id).unwrap();
        loop {
            if committed.get() == limit {
                return committed.get();
            }
            committed.set(committed.get() + 1);
            if store.compact(&mut plan).unwrap().is_some() {
                break;
            }
        }
    }
    for step in store
        .prepare_sweep(std::time::SystemTime::now() - Duration::from_secs(3600))
        .unwrap()
    {
        if commit(step).is_none() {
            return committed.get();
        }
    }
    committed.get()
}

/// What a client and a model can see of a thread: its records, folded and
/// as each history window holds them, where each entry is anchored, and the
/// entries search holds for it.
#[derive(Debug, PartialEq)]
struct Observed {
    fold: tcode_core::session::Timeline,
    anchors: Vec<(u64, String)>,
    search: Vec<crate::session_search::SearchableEntry>,
    window: Vec<(u64, StoredEvent)>,
}

fn observe(store: &SessionStore, id: &str) -> Observed {
    let log = store.read_log(id).unwrap();
    let indexed = crate::thread_index::IndexedLog::build(&log);
    let read = store.snapshot_log(id).unwrap();
    let mut window = Vec::new();
    read.visit_projected_rows(
        0..read.end,
        true,
        "observe",
        usize::MAX,
        |record| std::borrow::Cow::Borrowed(record),
        |position, record| {
            // What compaction changes; the folds above cover it.
            if !matches!(
                record.event,
                AgentEvent::Delta { .. } | AgentEvent::TurnChangesUpdated { .. }
            ) {
                window.push((position, record));
            }
            true
        },
    )
    .unwrap();
    Observed {
        fold: tcode_core::session::Timeline::fold_stored(&log.records),
        anchors: indexed
            .entries()
            .map(|(position, entry)| (position, entry.id.clone()))
            .collect(),
        search: crate::session_search::extract_searchable_entries(&log),
        window,
    }
}

/// An older build's store turns over on the first open, and its rows move
/// in the background while every read sees the same thread: rows read back
/// byte for byte, from the older file or from this one, except what the
/// move drops — superseded deltas (the one that created an entry emptied)
/// and superseded snapshots' diffs. Images leave the rows for one file each
/// and come back exactly. The older file is gone at the end.
#[test]
fn an_older_store_moves_in_the_background_and_every_thread_reads_back_the_same() {
    let logs = [
        ("images", images_log()),
        ("codex", SNAPSHOT_LOG.to_vec()),
        ("mixed", MIXED_LOG.to_vec()),
        (
            "turns",
            include_bytes!("fixtures/turn-index.jsonl").to_vec(),
        ),
        (
            "tools",
            include_bytes!("fixtures/legacy-tool-output.jsonl").to_vec(),
        ),
        (
            "items",
            include_bytes!("fixtures/legacy-codex-items.jsonl").to_vec(),
        ),
        ("stream", stream_log(2)),
    ];
    let dir = older_data_dir(&logs);
    let store = dir.store();
    store.open().unwrap();
    let version = |path: &Path| db::Db::open(path, None).unwrap().user_version().unwrap();
    assert!(dir.path().join(source::SOURCE_FILE).exists());
    let mut before = HashMap::new();
    for (id, log) in &logs {
        assert_eq!(
            &store.read_event_log(id).unwrap(),
            log,
            "{id} before moving"
        );
        before.insert(*id, observe(&store, id));
    }

    assert!(storage_pass(&store, usize::MAX) > 0);
    store.close().unwrap();
    assert!(!dir.path().join(source::SOURCE_FILE).exists());
    assert_eq!(version(&dir.path().join(DB_FILE)), SCHEMA_VERSION);
    let store = dir.store();
    for (id, log) in &logs {
        let after = observe(&store, id);
        assert_eq!(after, before[id], "{id}");
        let expected: Vec<u8> = match *id {
            "codex" => {
                let mut rows: Vec<&[u8]> = segments(log).collect();
                rows[1] = b"{\"type\":\"turn_changes_updated\",\"turn_id\":\"t1\",\"changes\":[{\"path\":\"f\",\"kind\":\"modify\",\"diff\":null}],\"completeness\":\"exact\"}\n";
                rows[6] = b"{\"author\":{\"device_id\":\"phone\",\"name\":\"Alice\"},\"ts\":6,\"event\":{\"type\":\"turn_changes_updated\",\"turn_id\":\"t2\",\"changes\":[{\"path\":\"g\",\"kind\":\"create\",\"diff\":null}],\"completeness\":\"exact\"}}\n";
                rows.concat()
            }
            "stream" => {
                let text = String::from_utf8(log.clone()).unwrap();
                text.lines()
                    .filter(|line| {
                        !(line.contains("\"text\":\"lo \"")
                            || line.contains("\"text\":\"world\"")
                            || line.contains("\"item_id\":\"c0\""))
                    })
                    .map(|line| line.replace("\"text\":\"Hel\"", "\"text\":\"\"") + "\n")
                    .collect::<String>()
                    .into_bytes()
            }
            _ => log.clone(),
        };
        assert_eq!(store.read_event_log(id).unwrap(), expected, "{id}");
        let (_, missing) = decoded_rows(&store, id);
        assert_eq!(missing, 0, "{id}");
    }
    let blobs = crate::fs_tree::tree_size(&dir.path().join("blobs/sha256")).unwrap();
    let images: u64 = [10_000, 38_000, 30_000, 21_000, 5_000, 3_100].iter().sum();
    assert_eq!(
        blobs, images,
        "one file per image, however often a row names it"
    );
    assert!(store.threads_to_compact().unwrap().is_empty());
    assert_eq!(
        storage_pass(&store, usize::MAX),
        0,
        "a second pass has nothing to do"
    );
}

/// Stopping the pass after any commit and starting it again on a reopened
/// store ends with the store an uninterrupted pass leaves.
#[test]
fn an_interrupted_storage_pass_resumes_and_converges() {
    let logs = [("stream", stream_log(270)), ("images", images_log())];
    let older = older_data_dir(&logs);
    let read_all = |store: &SessionStore| {
        logs.iter()
            .map(|(id, _)| decoded_rows(store, id).0)
            .collect::<Vec<_>>()
    };
    let reference = copy_data_dir(&older);
    let store = reference.store();
    let commits = storage_pass(&store, usize::MAX);
    assert!(
        commits > 4,
        "{commits} commits leave too few places to stop"
    );
    assert!(
        store
            .run("dictionary", |st| Ok(st.codec().has_dictionary()))
            .unwrap(),
        "enough rows to train a dictionary"
    );
    let expected = read_all(&store);
    store.close().unwrap();
    for stop in 0..commits {
        let dir = copy_data_dir(&older);
        let store = dir.store();
        assert_eq!(storage_pass(&store, stop), stop);
        store.close().unwrap();
        let store = dir.store();
        storage_pass(&store, usize::MAX);
        assert_eq!(read_all(&store), expected, "stopped after {stop} commits");
        store.close().unwrap();
        assert!(
            !dir.path().join(source::SOURCE_FILE).exists(),
            "stopped after {stop}"
        );
    }
}

/// Writes to a thread between reading a chunk of it and committing the
/// chunk: an append keeps both, a replacement or removal makes the chunk
/// stale so the older rows never come back, and a fork takes the rows still
/// to move. Blobs only a stale chunk wrote are collected; no blob a row
/// names ever is.
#[test]
fn a_thread_written_while_it_moves_loses_nothing() {
    let logs = [
        ("appended", images_log()),
        ("replaced", images_log()),
        ("forked", stream_log(3)),
    ];
    let dir = older_data_dir(&logs);
    let store = dir.store();
    store.open().unwrap();
    let event = AgentEvent::TurnStarted {
        turn_id: "after".into(),
    };
    let chunk = |id: &str| {
        let mut plan = store.plan_move(id).unwrap().unwrap();
        store.prepare_move_chunk(&mut plan).unwrap().unwrap()
    };

    let appended = chunk("appended");
    store.append_event("appended", 99, &event).unwrap();
    assert!(matches!(
        store.commit_storage_step(appended).unwrap(),
        StepOutcome::Moved { done: true, .. }
    ));
    let mut expected = images_log();
    expected.extend_from_slice(
        b"{\"ts\":99,\"event\":{\"type\":\"turn_started\",\"turn_id\":\"after\"}}\n",
    );
    assert_eq!(store.read_event_log("appended").unwrap(), expected);

    let replaced = chunk("replaced");
    store
        .apply(&[Mutation::replace_event_log(
            "replaced",
            SNAPSHOT_LOG.to_vec(),
        )])
        .unwrap();
    assert_eq!(
        store.commit_storage_step(replaced).unwrap(),
        StepOutcome::Stale
    );

    let forked = chunk("forked");
    store
        .apply(&[
            Mutation::clone_events("forked", "fork"),
            Mutation::clone_events_to("forked", "partial", CloneBound::ThroughTurnOpening(0)),
            Mutation::remove_session("forked"),
        ])
        .unwrap();
    assert_eq!(
        store.commit_storage_step(forked).unwrap(),
        StepOutcome::Stale
    );
    assert_eq!(
        store.read_event_log("fork").unwrap(),
        logs[2].1,
        "the fork reads the rows still to move"
    );
    assert!(
        logs[2]
            .1
            .starts_with(&store.read_event_log("partial").unwrap()),
        "a partial copy reads only its own of the rows still to move"
    );
    assert!(store.read_event_log("partial").unwrap().len() < logs[2].1.len());

    storage_pass(&store, usize::MAX);
    assert!(!dir.path().join(source::SOURCE_FILE).exists());
    assert_eq!(
        fold_rows(segments(&store.read_event_log("replaced").unwrap())),
        fold_rows(segments(SNAPSHOT_LOG)),
        "the replacement, not the rows it replaced"
    );
    assert!(store.read_event_log("forked").unwrap().is_empty());
    let fork = store.read_log("fork").unwrap();
    assert_eq!(
        tcode_core::session::Timeline::fold_stored(&fork.records),
        fold_rows(segments(&logs[2].1))
    );
    let files = || {
        fs::read_dir(dir.path().join("blobs/sha256"))
            .unwrap()
            .count()
    };
    let named = files();
    let swept: usize = store
        .prepare_sweep(std::time::SystemTime::now() + Duration::from_secs(1))
        .unwrap()
        .into_iter()
        .map(|step| match store.commit_storage_step(step).unwrap() {
            StepOutcome::Swept { files, .. } => files,
            other => panic!("{other:?}"),
        })
        .sum();
    assert_eq!(
        swept, 0,
        "every blob the stale chunk wrote is also the appended thread's"
    );
    assert_eq!(files(), named);
    store
        .apply(&[Mutation::remove_session("appended")])
        .unwrap();
    let swept: usize = store
        .prepare_sweep(std::time::SystemTime::now() + Duration::from_secs(1))
        .unwrap()
        .into_iter()
        .map(|step| match store.commit_storage_step(step).unwrap() {
            StepOutcome::Swept { files, .. } => files,
            other => panic!("{other:?}"),
        })
        .sum();
    assert_eq!(swept, 6, "the removed thread's images");
    for id in ["replaced", "fork"] {
        assert_eq!(decoded_rows(&store, id).1, 0, "{id}");
    }
}

/// A switch a crash interrupted is done again from the older file, which
/// stays whole until the new one is in place; a file under the source's name
/// that is not the older store itself is kept aside. An older build only
/// ever opens `tcode.db`, and from the switch on refuses it.
#[test]
fn an_interrupted_switch_starts_over_and_an_older_build_never_opens_a_partial_store() {
    let logs = [("images", images_log())];
    let dir = older_data_dir(&logs);
    fs::hard_link(
        dir.path().join(DB_FILE),
        dir.path().join(source::SOURCE_FILE),
    )
    .unwrap();
    fs::write(dir.path().join("tcode.db.next"), b"half written").unwrap();
    let store = dir.store();
    assert_eq!(store.read_event_log("images").unwrap(), logs[0].1);
    store.close().unwrap();
    let version = db::Db::open(&dir.path().join(DB_FILE), None)
        .unwrap()
        .user_version()
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    assert_ne!(version, db::V1_VERSION, "what an older build opens");
    assert!(!dir.path().join("tcode.db.next").exists());

    let dir = older_data_dir(&logs);
    fs::write(dir.path().join(source::SOURCE_FILE), b"someone else's").unwrap();
    let store = dir.store();
    assert_eq!(store.read_event_log("images").unwrap(), logs[0].1);
    store.close().unwrap();
    let aside: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|entry| entry.unwrap().file_name().into_string().ok())
        .filter(|name| name.starts_with("tcode.db.v1."))
        .collect();
    assert_eq!(aside.len(), 1);
    assert_eq!(
        fs::read(dir.path().join(&aside[0])).unwrap(),
        b"someone else's"
    );
}
