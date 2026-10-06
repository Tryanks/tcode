use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;

use agent::{ApprovalMode, ProviderCommand, ProviderCommandKind, TurnStatus};

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
/// no project yet.
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
                "cwd": "/work/beta", "created_at": 2, "updated_at": 20
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
    assert_eq!(mixed.approval_mode, ApprovalMode::FullAccess);
    assert_eq!(mixed.archived_at, None);

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
    assert_eq!(
        fs::read(root.join("legacy/sessions.json")).unwrap(),
        legacy_index().to_string().as_bytes()
    );
    assert_eq!(
        fs::read(root.join("legacy/mixed.jsonl")).unwrap(),
        MIXED_LOG
    );
    assert_eq!(
        fs::read(root.join("legacy/orphan.jsonl")).unwrap(),
        ORPHAN_LOG
    );
    assert!(
        fs::read(root.join("legacy/empty.jsonl"))
            .unwrap()
            .is_empty()
    );
    for leftover in [
        "tcode.db.migrating",
        "tcode.db.migrating-wal",
        "tcode.db.archive",
    ] {
        assert!(!root.join(leftover).exists(), "{leftover} left behind");
    }
}

#[test]
fn first_start_migrates_the_json_index_and_every_log_byte_for_byte() {
    let dir = DataDir::new();
    write_legacy_fixture(dir.path());
    let store = dir.store();
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
    assert_eq!(reopened.read_event_log("mixed").unwrap(), expected);
    assert_eq!(reopened.read_file().unwrap().sessions.len(), 2);
    assert_eq!(fs::read(dir.path().join("sessions.json")).unwrap(), b"[]");
    assert_eq!(
        fs::read(dir.path().join("legacy/mixed.jsonl")).unwrap(),
        MIXED_LOG
    );
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

    let file = dir.store().read_file().unwrap();
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

#[test]
fn an_unparseable_index_is_preserved_and_the_logs_still_migrate() {
    let dir = DataDir::new();
    let corrupt = b"not valid session json";
    fs::write(dir.path().join("sessions.json"), corrupt).unwrap();
    fs::write(dir.path().join("orphan.jsonl"), ORPHAN_LOG).unwrap();
    let store = dir.store();

    let file = store.read_file().unwrap();
    assert!(file.projects.is_empty() && file.sessions.is_empty());
    assert_eq!(store.read_event_log("orphan").unwrap(), ORPHAN_LOG);
    let backups: Vec<_> = fs::read_dir(dir.path())
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
fn duplicate_index_ids_fail_the_migration_and_leave_the_sources_alone() {
    let dir = DataDir::new();
    let session = serde_json::json!({
        "id": "twice", "title": "One", "provider": "codex",
        "cwd": "/work", "created_at": 1, "updated_at": 1
    });
    let index = serde_json::json!({"projects": [], "sessions": [session, session]}).to_string();
    fs::write(dir.path().join("sessions.json"), &index).unwrap();
    fs::write(dir.path().join("twice.jsonl"), ORPHAN_LOG).unwrap();

    let error = dir.store().open().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("twice"), "{error}");
    assert!(!dir.path().join(DB_FILE).exists());
    assert!(!dir.path().join(LEGACY_DIR_NAME).exists());
    assert_eq!(
        fs::read(dir.path().join("sessions.json")).unwrap(),
        index.as_bytes()
    );
    assert_eq!(
        fs::read(dir.path().join("twice.jsonl")).unwrap(),
        ORPHAN_LOG
    );
}

const LEGACY_DIR_NAME: &str = "legacy";

/// Put the migrated sources back where an interruption would have left them.
fn restore_sources(root: &Path) {
    for name in LEGACY_FILES {
        fs::rename(root.join("legacy").join(name), root.join(name)).unwrap();
    }
}

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
    let store = dir.store();
    store.open().unwrap();
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
    restore_sources(dir.path());
    fs::write(
        dir.path().join("tcode.db.archive"),
        archive_list(&LEGACY_FILES),
    )
    .unwrap();
    let store = dir.store();
    store.open().unwrap();
    assert_fixture_migrated(&store, dir.path());
}

#[test]
fn a_start_after_an_interrupted_archival_finishes_it_without_overwriting() {
    let dir = DataDir::new();
    write_legacy_fixture(dir.path());
    let store = dir.store();
    store.open().unwrap();
    store.close().unwrap();

    // Killed after the rename while moving the sources: two are still in the
    // data dir. One of them also has a copy in legacy/ already, which must
    // survive untouched while the source stays where it is.
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
        fs::read(dir.path().join("legacy/mixed.jsonl")).unwrap(),
        MIXED_LOG
    );
    assert_eq!(
        fs::read(dir.path().join("orphan.jsonl")).unwrap(),
        ORPHAN_LOG
    );
    assert_eq!(
        fs::read(dir.path().join("legacy/orphan.jsonl")).unwrap(),
        b"older copy"
    );
    assert_eq!(store.read_event_log("mixed").unwrap(), MIXED_LOG);
}

#[test]
fn a_fresh_data_dir_gets_a_completed_database_through_the_staged_path() {
    let dir = DataDir::new();
    let store = dir.store();
    assert!(store.load_index().unwrap().is_empty());
    store.close().unwrap();
    let db = db::Db::open(&dir.path().join(DB_FILE), false).unwrap();
    assert_eq!(db.user_version().unwrap(), SCHEMA_VERSION);
    drop(db);
    assert!(!dir.path().join("legacy").exists());
    assert!(!dir.path().join("tcode.db.migrating").exists());
}

#[test]
fn an_unfinished_newer_or_corrupt_database_is_refused_and_left_untouched() {
    let set_version = |root: &Path, version: i64| {
        let db = db::Db::open(&root.join(DB_FILE), false).unwrap();
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
        let before = fs::read(dir.path().join(DB_FILE)).unwrap();
        let error = dir.store().open().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains(expected), "{error}");
        assert_eq!(fs::read(dir.path().join(DB_FILE)).unwrap(), before);
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
    let fork = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/w"), None);
    store
        .apply(&[
            Mutation::replace_event_log("source", MIXED_LOG.to_vec()),
            Mutation::clone_events("source", &fork.id),
            Mutation::upsert_meta(fork.clone()),
            Mutation::clone_events("missing", "empty-fork"),
        ])
        .unwrap();
    assert_eq!(store.read_event_log("source").unwrap(), MIXED_LOG);
    assert_eq!(store.read_event_log(&fork.id).unwrap(), MIXED_LOG);
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
fn command_cache_roundtrips_per_provider_and_acp_agent() {
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
    store
        .save_commands(ProviderKind::ClaudeCode, None, &native)
        .unwrap();
    store
        .save_commands(ProviderKind::Acp, Some("vendor/agent"), &acp)
        .unwrap();

    // Reopen the store to prove the values come from disk, not memory.
    let reopened = SessionStore::open_at(root.clone()).unwrap();
    assert_eq!(
        reopened.load_commands(ProviderKind::ClaudeCode, None),
        native
    );
    assert_eq!(
        reopened.load_commands(ProviderKind::Acp, Some("vendor/agent")),
        acp
    );
    assert!(
        reopened
            .load_commands(ProviderKind::Acp, Some("different-agent"))
            .is_empty()
    );
    assert!(root.join("commands-claude.json").is_file());
    assert!(
        root.join("commands-acp-76656e646f722f6167656e74.json")
            .is_file()
    );
}

/// This test binary, re-run as another process that opens `root` and holds
/// it until its stdin closes. It reports `OPEN` or `ERR <kind> <message>`.
struct Child {
    process: std::process::Child,
    stdout: BufReader<std::process::ChildStdout>,
    report: String,
}

impl Child {
    fn spawn(root: &Path) -> std::process::Child {
        crate::process::command(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "store::tests::store_owner_process",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("TCODE_STORE_OWNER_DIR", root)
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
        let report = loop {
            line.clear();
            assert!(
                stdout.read_line(&mut line).unwrap() > 0,
                "the child exited without reporting"
            );
            if let Some((_, report)) = line.trim_end().split_once(OWNER_REPORT) {
                break report.to_owned();
            }
        };
        Self {
            process,
            stdout,
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

#[test]
#[ignore = "the other process of the ownership tests below, which run it"]
fn store_owner_process() {
    let root = PathBuf::from(std::env::var_os("TCODE_STORE_OWNER_DIR").unwrap());
    let store = SessionStore::open_at(root).unwrap();
    match store.open() {
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
    let owner = Child::report(Child::spawn(dir.path()));
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
    std::thread::sleep(Duration::from_millis(300));
    assert!(!waiting.is_finished(), "the relaunch waits for the owner");
    owner.release();
    waiting.join().unwrap().unwrap();
    // Only peeked: the host consumes the marker once it owns the data dir.
    assert!(crate::relaunch::pending(dir.path()));
}

#[test]
fn two_simultaneous_first_launches_migrate_once() {
    let dir = DataDir::new();
    write_legacy_fixture(dir.path());
    let (first, second) = (Child::spawn(dir.path()), Child::spawn(dir.path()));
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

/// Kill the migration at random points — while logs are copied, around the
/// checkpoint, the rename, the directory syncs and the archival — and restart
/// it: there is either no `tcode.db` and every source is where it was, or a
/// complete one, and the finished migration always holds every source byte
/// for byte. `TCODE_MIGRATION_KILLS` sets the number of kills.
#[test]
#[ignore = "spawns and SIGKILLs child processes over a large fixture; run deliberately"]
fn migration_survives_sigkill() {
    let runs: usize = std::env::var("TCODE_MIGRATION_KILLS")
        .ok()
        .and_then(|runs| runs.parse().ok())
        .unwrap_or(20);
    let originals = DataDir::new();
    write_legacy_fixture(originals.path());
    let line = format!(
        "{{\"ts\":1,\"event\":{{\"type\":\"turn_started\",\"turn_id\":\"{}\"}}}}\n",
        "x".repeat(1000)
    );
    for index in 0..24 {
        fs::write(
            originals.path().join(format!("bulk-{index}.jsonl")),
            line.repeat(300 + index * 20),
        )
        .unwrap();
    }
    let names: Vec<String> = fs::read_dir(originals.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
        | 1;
    // Kills spread over the time an uninterrupted migration takes.
    let calibration = DataDir::new();
    for name in &names {
        fs::copy(originals.path().join(name), calibration.path().join(name)).unwrap();
    }
    let started = Instant::now();
    let full = Child::report(Child::spawn(calibration.path()));
    let window = started.elapsed().as_millis() as u64;
    assert_eq!(full.report, "OPEN");
    full.release();
    let mut outcomes = HashMap::<&str, usize>::new();
    for run in 0..runs {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let dir = DataDir::new();
        for name in &names {
            fs::copy(originals.path().join(name), dir.path().join(name)).unwrap();
        }
        let mut owner = Child::spawn(dir.path());
        std::thread::sleep(Duration::from_millis(seed % (window + window / 10)));
        owner.kill().unwrap();
        owner.wait().unwrap();

        // What the kill left: no database and untouched sources, or a
        // completed database and every source in the data dir or legacy/.
        let source = |name: &str| {
            [dir.path().join(name), dir.path().join("legacy").join(name)]
                .into_iter()
                .find(|path| path.exists())
        };
        let outcome = if dir.path().join(DB_FILE).exists() {
            let db = db::Db::open(&dir.path().join(DB_FILE), false).unwrap();
            assert_eq!(db.user_version().unwrap(), SCHEMA_VERSION, "run {run}");
            drop(db);
            if dir.path().join("tcode.db.archive").exists() {
                "published, archival pending"
            } else {
                "complete"
            }
        } else {
            for name in &names {
                assert!(
                    dir.path().join(name).exists(),
                    "run {run}: {name} moved early"
                );
            }
            if dir.path().join("tcode.db.migrating").exists() {
                "staging"
            } else {
                "not started"
            }
        };
        *outcomes.entry(outcome).or_default() += 1;
        for name in &names {
            assert_eq!(
                fs::read(source(name).unwrap_or_else(|| panic!("run {run}: {name} lost"))).unwrap(),
                fs::read(originals.path().join(name)).unwrap(),
                "run {run}: {name}"
            );
        }

        let store = dir.store();
        store.open().unwrap();
        for name in names.iter().filter_map(|name| name.strip_suffix(".jsonl")) {
            assert_eq!(
                store.read_event_log(name).unwrap(),
                fs::read(originals.path().join(format!("{name}.jsonl"))).unwrap(),
                "run {run}: {name}"
            );
        }
        assert_eq!(store.read_file().unwrap().sessions.len(), 2, "run {run}");
        store.close().unwrap();
    }
    println!("{runs} kills over a {window} ms migration: {outcomes:?}");
}
