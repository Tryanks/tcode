//! Persistence for tcode threads.
//!
//! Projects, thread metadata and every thread's event log live in one Turso
//! database, `tcode.db`, in the platform data dir (e.g.
//! `~/Library/Application Support/tcode/`):
//!   * `projects` and `sessions` hold each [`Project`] / [`SessionMeta`] as the
//!     JSON serde produces for it.
//!   * `events` holds each thread's log as raw byte segments, one per line
//!     including its `\n`, densely numbered from 0: the bytes of a `{ ts, event }`
//!     record, or whatever a migrated or imported log contained.
//!
//! Model and command caches stay as JSON files beside it.
//!
//! Replay accepts timestamped records and legacy bare [`AgentEvent`] lines,
//! then folds [`StoredEvent`]s into a [`tcode_core::session::Timeline`].

mod db;
mod migrate;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::fs::{self, File};
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use agent::{AgentEvent, ModelSpec, ProviderCommand, ProviderKind};
use serde::{Deserialize, Serialize};

use tcode_core::project::{IndexFile, Project, SessionMeta};
use tcode_core::session::StoredEvent;

use db::{Broken, Db, SCHEMA_VERSION, blob, integer, is_broken};

const DB_FILE: &str = "tcode.db";
/// Held with an OS file lock for as long as a host owns the data dir, which
/// covers the migration as well as the open database. It is never deleted:
/// unlinking a lock file while another process waits on it would let two
/// owners in.
const LOCK_FILE: &str = "tcode.lock";
/// A macOS permission relaunch starts the new instance while the old one is
/// still closing its store.
const RELAUNCH_WAIT: Duration = Duration::from_secs(15);

/// On-disk envelope wrapping each event with its record time. Kept private:
/// callers deal in [`StoredEvent`] (which tolerates the legacy bare form).
#[derive(Serialize, Deserialize)]
struct EventEnvelope {
    ts: u64,
    event: AgentEvent,
}

#[derive(Serialize)]
struct EventEnvelopeRef<'a> {
    ts: u64,
    event: &'a AgentEvent,
}

/// Cheap, cloneable handle to the data directory. Every handle for the same
/// directory in this process shares one database, writer and lifecycle.
#[derive(Clone)]
pub struct SessionStore {
    root: PathBuf,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for SessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionStore")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

struct Shared {
    state: Mutex<State>,
    /// Signalled whenever an operation finishes, for [`SessionStore::close`].
    idle: Condvar,
    /// Per-session event-log generation, advanced after every committed
    /// change to that log; see [`SessionStore::event_generation`].
    generations: Mutex<HashMap<String, u64>>,
    next_generation: AtomicU64,
    /// Full event-log parses, shared by every clone of this store so tests can
    /// prove a caller served history from memory.
    #[cfg(any(test, feature = "test-support"))]
    event_reads: std::sync::atomic::AtomicUsize,
}

enum State {
    /// Opened on first use, so a handle that only needs the directory (a
    /// subcommand, a settings file) never takes the data dir.
    Unopened,
    Open(Live),
    /// [`SessionStore::close`] is waiting for in-flight operations.
    Closing(Live),
    Closed,
    /// A panic or an unrecoverable connection: nothing is served again, and
    /// the ownership lock stays held while any handle remains, so no other
    /// process opens a database this one may have left mid-operation.
    Failed {
        reason: String,
        _ownership: Option<File>,
    },
}

struct Live {
    db: Arc<Db>,
    ownership: File,
    in_flight: usize,
}

/// Every open handle by canonical data dir, so a second
/// [`SessionStore::open_at`] in one process shares the first one's database
/// instead of contending for its locks.
static STORES: LazyLock<Mutex<HashMap<PathBuf, Weak<Shared>>>> = LazyLock::new(Default::default);

/// One change to the store. [`SessionStore::apply`] commits a sequence of them
/// in a single transaction.
#[derive(Debug, Clone)]
pub struct Mutation(Op);

#[derive(Debug, Clone)]
enum Op {
    AppendEvent { session_id: String, line: Vec<u8> },
    ReplaceEventLog { session_id: String, bytes: Vec<u8> },
    CloneEvents { src: String, dst: String },
    UpsertMeta(Box<SessionMeta>),
    UpsertProject(Box<Project>),
    RemoveSession(String),
    RemoveProject(String),
}

impl Mutation {
    /// Append one event, wrapped in a timestamped envelope
    /// (`{"ts": <unix_ms>, "event": {…}}`).
    pub fn append_event(session_id: &str, ts: u64, event: &AgentEvent) -> io::Result<Self> {
        let mut line = serde_json::to_vec(&EventEnvelopeRef { ts, event }).map_err(invalid_data)?;
        line.push(b'\n');
        Ok(Self(Op::AppendEvent {
            session_id: session_id.to_owned(),
            line,
        }))
    }

    /// Replace a session's whole native event log with `bytes`, kept exactly.
    pub fn replace_event_log(session_id: &str, bytes: Vec<u8>) -> Self {
        Self(Op::ReplaceEventLog {
            session_id: session_id.to_owned(),
            bytes,
        })
    }

    /// Give `dst` a copy of `src`'s event log. A missing source is an empty
    /// transcript.
    pub fn clone_events(src: &str, dst: &str) -> Self {
        Self(Op::CloneEvents {
            src: src.to_owned(),
            dst: dst.to_owned(),
        })
    }

    /// Insert or replace a meta (by id).
    pub fn upsert_meta(meta: SessionMeta) -> Self {
        Self(Op::UpsertMeta(Box::new(meta)))
    }

    /// Insert or replace a project; its previous managed icon is removed once
    /// the change has committed.
    pub fn upsert_project(project: Project) -> Self {
        Self(Op::UpsertProject(Box::new(project)))
    }

    /// Remove a session's meta and its event log.
    pub fn remove_session(id: &str) -> Self {
        Self(Op::RemoveSession(id.to_owned()))
    }

    /// Remove a project; its managed icon is removed once the change has
    /// committed. Sessions are removed separately.
    pub fn remove_project(id: &str) -> Self {
        Self(Op::RemoveProject(id.to_owned()))
    }

    /// Approximate bytes this change writes, for bounding a batch.
    pub fn payload_len(&self) -> usize {
        match &self.0 {
            Op::AppendEvent { line, .. } => line.len(),
            Op::ReplaceEventLog { bytes, .. } => bytes.len(),
            _ => 0,
        }
    }

    /// The session whose event log this change rewrites, if any.
    fn event_log(&self) -> Option<&str> {
        match &self.0 {
            Op::AppendEvent { session_id, .. } | Op::ReplaceEventLog { session_id, .. } => {
                Some(session_id)
            }
            Op::CloneEvents { dst, .. } => Some(dst),
            Op::RemoveSession(id) => Some(id),
            Op::UpsertMeta(_) | Op::UpsertProject(_) | Op::RemoveProject(_) => None,
        }
    }
}

impl SessionStore {
    /// Open (creating if needed) the store under the platform data dir, or under
    /// `TCODE_DATA_DIR` when it is set — which gives a throwaway profile (its own
    /// sessions, settings and installed ACP agents) for demos and screenshots.
    pub fn open_default() -> io::Result<Self> {
        let root = match std::env::var_os("TCODE_DATA_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => dirs::data_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("tcode"),
        };
        Self::open_at(root)
    }

    /// A handle to `root`, created if needed. The database is not opened until
    /// [`SessionStore::open`] or the first operation that needs it.
    pub fn open_at(root: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&root)?;
        let key = fs::canonicalize(&root)?;
        let mut stores = STORES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        stores.retain(|_, shared| shared.strong_count() > 0);
        let shared = match stores.get(&key).and_then(Weak::upgrade) {
            Some(shared) if !shared.finished() => shared,
            _ => {
                let shared = Arc::new(Shared {
                    state: Mutex::new(State::Unopened),
                    idle: Condvar::new(),
                    generations: Mutex::default(),
                    next_generation: AtomicU64::new(1),
                    #[cfg(any(test, feature = "test-support"))]
                    event_reads: Default::default(),
                });
                stores.insert(key, Arc::downgrade(&shared));
                shared
            }
        };
        Ok(Self { root, shared })
    }

    /// How many times [`SessionStore::read_events`] parsed a log through this
    /// store or any of its clones.
    #[cfg(any(test, feature = "test-support"))]
    pub fn event_reads(&self) -> usize {
        self.shared
            .event_reads
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    /// Take ownership of the data dir and open its database now, migrating the
    /// JSON index and JSONL logs on first start. Fails with
    /// [`io::ErrorKind::ResourceBusy`] while another host owns the directory.
    pub fn open(&self) -> io::Result<()> {
        self.run("open", |_| Ok(()))
    }

    /// Whether the store stopped serving after a panic or a broken connection.
    pub fn is_failed(&self) -> bool {
        matches!(
            *self
                .shared
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            State::Failed { .. }
        )
    }

    /// Wait for every in-flight operation, checkpoint the WAL into the main
    /// file and release the database and the data dir. Every handle of this
    /// store fails afterwards; a store that already failed is not
    /// checkpointed.
    pub fn close(&self) -> io::Result<()> {
        let mut state = self.shared.lock_state()?;
        loop {
            match std::mem::replace(&mut *state, State::Closed) {
                State::Unopened | State::Closed => return Ok(()),
                State::Open(live) => *state = State::Closing(live),
                State::Closing(live) if live.in_flight > 0 => {
                    *state = State::Closing(live);
                    state = self
                        .shared
                        .idle
                        .wait(state)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                }
                State::Closing(live) => {
                    drop(state);
                    let Live { db, ownership, .. } = live;
                    let result = match Arc::try_unwrap(db) {
                        Ok(db) => catch_unwind(AssertUnwindSafe(|| db.checkpoint()))
                            .unwrap_or_else(|panic| {
                                Err(io::Error::other(format!(
                                    "session store panicked during the closing checkpoint: {}",
                                    panic_message(panic.as_ref())
                                )))
                            }),
                        Err(_) => Err(io::Error::other(
                            "session store is still referenced after its last operation",
                        )),
                    };
                    drop(ownership);
                    return result;
                }
                failed @ State::Failed { .. } => {
                    let error = failed_error(&failed);
                    *state = failed;
                    return Err(error);
                }
            }
        }
    }

    /// Run `body` against the open database. A panic inside it (Turso asserts
    /// its invariants by panicking) becomes an error naming `operation` and
    /// fails the store for every handle.
    fn run<R>(&self, operation: &str, body: impl FnOnce(&Db) -> io::Result<R>) -> io::Result<R> {
        let db = self.acquire(operation)?;
        let outcome = catch_unwind(AssertUnwindSafe(|| body(&db)));
        drop(db);
        let result = match outcome {
            Ok(Err(error)) if is_broken(&error) => {
                self.shared.fail(format!("{operation}: {error}"));
                Err(error)
            }
            Ok(result) => result,
            Err(panic) => {
                let reason = format!(
                    "session store panicked during {operation}: {}",
                    panic_message(panic.as_ref())
                );
                log::error!("{reason}");
                self.shared.fail(reason.clone());
                Err(io::Error::other(reason))
            }
        };
        self.shared.release();
        result
    }

    fn acquire(&self, operation: &str) -> io::Result<Arc<Db>> {
        let mut state = self.shared.lock_state()?;
        if matches!(*state, State::Unopened) {
            match catch_unwind(AssertUnwindSafe(|| open_live(&self.root))) {
                Ok(Ok(live)) => *state = State::Open(live),
                Ok(Err(error)) => return Err(error),
                Err(panic) => {
                    let reason = format!(
                        "session store panicked while opening {}: {}",
                        self.root.display(),
                        panic_message(panic.as_ref())
                    );
                    log::error!("{reason}");
                    *state = State::Failed {
                        reason: reason.clone(),
                        _ownership: None,
                    };
                    return Err(io::Error::other(reason));
                }
            }
        }
        match &mut *state {
            State::Open(live) => {
                live.in_flight += 1;
                Ok(live.db.clone())
            }
            State::Closing(_) | State::Closed => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!("cannot {operation}: the session store is closed"),
            )),
            failed @ State::Failed { .. } => Err(failed_error(failed)),
            State::Unopened => unreachable!("opened above"),
        }
    }

    /// Commit `mutations` in one transaction, then run their deferred side
    /// effects. Either every change is committed or none is.
    pub fn apply(&self, mutations: &[Mutation]) -> io::Result<()> {
        if mutations.is_empty() {
            return Ok(());
        }
        let icons = self.run("write", |db| {
            db.write(|db, connection| {
                let mut icons = Vec::new();
                for mutation in mutations {
                    apply_op(db, connection, &mutation.0, &mut icons)?;
                }
                Ok(icons)
            })
        })?;
        {
            let mut generations = self
                .shared
                .generations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for id in mutations.iter().filter_map(Mutation::event_log) {
                let generation = self.shared.next_generation.fetch_add(1, Ordering::Relaxed);
                generations.insert(id.to_owned(), generation);
            }
        }
        for icon in icons {
            self.remove_project_icon(icon);
        }
        Ok(())
    }

    /// A value that changes whenever a committed write changes `id`'s event
    /// log through this store, so a cache of something derived from the log
    /// can tell it is stale. 0 means unchanged since the store was opened.
    pub fn event_generation(&self, id: &str) -> u64 {
        self.shared
            .generations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .copied()
            .unwrap_or(0)
    }

    /// Read a native event log back as the exact bytes it holds.
    pub fn read_event_log(&self, id: &str) -> io::Result<Vec<u8>> {
        self.run("read event log", |db| {
            db.read(|db, connection| {
                let mut bytes = Vec::new();
                db.query(
                    connection,
                    "SELECT line FROM events WHERE session_id = ?1 ORDER BY position",
                    (id,),
                    |row| {
                        bytes.extend_from_slice(&blob(row, 0)?);
                        Ok(())
                    },
                )?;
                Ok(bytes)
            })
        })
    }

    fn models_path(&self, provider: ProviderKind) -> PathBuf {
        let name = match provider {
            ProviderKind::Codex => "codex",
            ProviderKind::ClaudeCode => "claude",
            ProviderKind::Pi => "pi",
            ProviderKind::OpenCode => "opencode",
            ProviderKind::Cursor => "cursor",
            ProviderKind::Grok => "grok",
            // ACP agents publish their models over the wire at session start
            // (`AgentEvent::ProviderOptions`), so there is no catalog to cache.
            ProviderKind::Acp => "acp",
        };
        self.root.join(format!("models-{name}.json"))
    }

    fn commands_path(&self, provider: ProviderKind, acp_agent_id: Option<&str>) -> Option<PathBuf> {
        let name = match provider {
            ProviderKind::Codex => "codex".to_string(),
            ProviderKind::ClaudeCode => "claude".to_string(),
            ProviderKind::Pi => "pi".to_string(),
            ProviderKind::OpenCode => "opencode".to_string(),
            ProviderKind::Cursor => "cursor".to_string(),
            ProviderKind::Grok => "grok".to_string(),
            ProviderKind::Acp => {
                let id = acp_agent_id?;
                // Registry ids are external input and may contain path separators.
                // Hex keeps the filename reversible and collision-free without
                // allowing an id to escape the data directory.
                let encoded = id
                    .as_bytes()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
                format!("acp-{encoded}")
            }
        };
        Some(self.root.join(format!("commands-{name}.json")))
    }

    /// Load the last-fetched model catalog for `provider` so the picker is
    /// instant offline. Empty when never fetched / unreadable.
    pub fn load_models(&self, provider: ProviderKind) -> Vec<ModelSpec> {
        let Ok(bytes) = fs::read(self.models_path(provider)) else {
            return Vec::new();
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    /// Persist the freshly fetched model catalog for `provider`.
    pub fn save_models(&self, provider: ProviderKind, models: &[ModelSpec]) -> io::Result<()> {
        let path = self.models_path(provider);
        let tmp = path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(models).map_err(invalid_data)?;
        fs::write(&tmp, data)?;
        fs::rename(&tmp, path)
    }

    /// Load the most recently reported command/skill list for a native provider
    /// or one specific ACP agent. Empty when missing, unreadable, or when an ACP
    /// agent id was not supplied.
    pub fn load_commands(
        &self,
        provider: ProviderKind,
        acp_agent_id: Option<&str>,
    ) -> Vec<ProviderCommand> {
        let Some(path) = self.commands_path(provider, acp_agent_id) else {
            return Vec::new();
        };
        let Ok(bytes) = fs::read(path) else {
            return Vec::new();
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    /// Atomically persist the complete command/skill list reported by a native
    /// provider or one specific ACP agent. Empty lists are meaningful: they
    /// replace a stale non-empty cache.
    pub fn save_commands(
        &self,
        provider: ProviderKind,
        acp_agent_id: Option<&str>,
        commands: &[ProviderCommand],
    ) -> io::Result<()> {
        let path = self.commands_path(provider, acp_agent_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "ACP command cache requires an agent id",
            )
        })?;
        let tmp = path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(commands).map_err(invalid_data)?;
        fs::write(&tmp, data)?;
        fs::rename(&tmp, path)
    }

    /// Load every project and session, in the order they were first stored.
    /// A row this build cannot decode is skipped with a warning and left in
    /// the database untouched.
    pub fn read_file(&self) -> io::Result<IndexFile> {
        self.run("read index", |db| {
            db.read(|db, connection| {
                // One read transaction, so projects and sessions come from the
                // same snapshot.
                db.execute(connection, "BEGIN", ())?;
                let file = read_index(db, connection);
                db.execute(connection, "COMMIT", ())?;
                file
            })
        })
    }

    /// Load the session index (newest first).
    pub fn load_index(&self) -> io::Result<Vec<SessionMeta>> {
        let mut metas = self.read_file()?.sessions;
        metas.sort_by_key(|b| std::cmp::Reverse(b.updated_at));
        Ok(metas)
    }

    /// Insert or replace a meta in the index (by id).
    pub fn upsert_meta(&self, meta: &SessionMeta) -> io::Result<()> {
        self.apply(&[Mutation::upsert_meta(meta.clone())])
    }

    /// Insert or replace a project, then remove its previous managed icon.
    pub fn upsert_project(&self, project: &Project) -> io::Result<()> {
        self.apply(&[Mutation::upsert_project(project.clone())])
    }

    /// Remove a project and its managed icon. Sessions are removed separately.
    pub fn remove_project(&self, id: &str) -> io::Result<()> {
        self.apply(&[Mutation::remove_project(id)])
    }

    fn remove_project_icon(&self, path: Option<PathBuf>) {
        if let Some(path) = path
            && path.parent() == Some(self.root.join("project-icons").as_path())
            && let Err(error) = fs::remove_file(&path)
            && error.kind() != io::ErrorKind::NotFound
        {
            log::warn!("could not remove project icon {}: {error}", path.display());
        }
    }

    /// Append one event to the session's log, wrapped in a timestamped
    /// envelope (`{"ts": <unix_ms>, "event": {…}}`).
    pub fn append_event(&self, id: &str, ts: u64, event: &AgentEvent) -> io::Result<()> {
        self.apply(&[Mutation::append_event(id, ts, event)?])
    }

    /// Read and parse every persisted event for a session, skipping lines that
    /// do not parse.
    ///
    /// Each line is tolerantly parsed as either a timestamped envelope
    /// (`{"ts":…,"event":…}`) or a legacy bare event (`{"type":…}`), so logs
    /// written before the envelope format still replay (with `ts == None`).
    pub fn read_events(&self, id: &str) -> io::Result<Vec<StoredEvent>> {
        #[cfg(any(test, feature = "test-support"))]
        self.shared
            .event_reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.run("read events", |db| {
            db.read(|db, connection| {
                let mut events = Vec::new();
                db.query(
                    connection,
                    "SELECT position, line FROM events WHERE session_id = ?1 ORDER BY position",
                    (id,),
                    |row| {
                        let position = integer(row, 0)?;
                        let line = blob(row, 1)?;
                        let Ok(line) = std::str::from_utf8(&line) else {
                            log::warn!("skipping event {position} of {id}: not UTF-8");
                            return Ok(());
                        };
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            return Ok(());
                        }
                        match parse_stored_line(trimmed) {
                            Ok(stored) => events.push(stored),
                            Err(err) => {
                                log::warn!("skipping unparseable event {position} of {id}: {err}")
                            }
                        }
                        Ok(())
                    },
                )?;
                Ok(events)
            })
        })
    }

    /// Remove a session from the index and delete its event log.
    pub fn remove_session(&self, id: &str) -> io::Result<()> {
        self.apply(&[Mutation::remove_session(id)])
    }
}

impl Shared {
    fn lock_state(&self) -> io::Result<MutexGuard<'_, State>> {
        // The state is replaced whole under the lock, so a poisoned guard
        // still holds a consistent value.
        Ok(self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()))
    }

    fn finished(&self) -> bool {
        matches!(
            *self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            State::Closed | State::Failed { .. }
        )
    }

    fn release(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let State::Open(live) | State::Closing(live) = &mut *state {
            live.in_flight -= 1;
        }
        self.idle.notify_all();
    }

    fn fail(&self, reason: String) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let ownership = match std::mem::replace(&mut *state, State::Closed) {
            State::Open(live) | State::Closing(live) => Some(live.ownership),
            State::Failed { _ownership, .. } => _ownership,
            State::Unopened | State::Closed => None,
        };
        *state = State::Failed {
            reason,
            _ownership: ownership,
        };
        self.idle.notify_all();
    }
}

fn failed_error(state: &State) -> io::Error {
    let State::Failed { reason, .. } = state else {
        unreachable!("only called for a failed store")
    };
    io::Error::other(Broken(format!(
        "the session store stopped after an internal failure ({reason}); restart Tcode"
    )))
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

/// Take the data dir, bring `tcode.db` up to date (migrating on first start)
/// and open it.
fn open_live(root: &Path) -> io::Result<Live> {
    let ownership = acquire_ownership(root)?;
    migrate::prepare(root)?;
    let path = root.join(DB_FILE);
    let db = Db::open(&path, false)?;
    match db.user_version()? {
        SCHEMA_VERSION => {}
        0 => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} was never completed (user_version 0). It is kept as it is and can be \
                     examined with the sqlite3 command-line tool.",
                    path.display()
                ),
            ));
        }
        version => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} has schema version {version}, written by a newer Tcode; this build \
                     understands version {SCHEMA_VERSION}",
                    path.display()
                ),
            ));
        }
    }
    migrate::warn_about_stray_sources(root);
    Ok(Live {
        db: Arc::new(db),
        ownership,
        in_flight: 0,
    })
}

fn acquire_ownership(root: &Path) -> io::Result<File> {
    let path = root.join(LOCK_FILE);
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    let mut deadline = None;
    let mut delay = Duration::from_millis(20);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(fs::TryLockError::Error(error)) => return Err(error),
            Err(fs::TryLockError::WouldBlock) => {
                let deadline = *deadline.get_or_insert_with(|| {
                    // Peek only: the marker is consumed by the host after it
                    // owns the data dir.
                    crate::relaunch::pending(root).then(|| Instant::now() + RELAUNCH_WAIT)
                });
                match deadline {
                    Some(deadline) if Instant::now() < deadline => {
                        std::thread::sleep(delay);
                        delay = (delay * 2).min(Duration::from_millis(250));
                    }
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::ResourceBusy,
                            format!(
                                "another Tcode host is already using the data directory {}; \
                                 quit it first, or set TCODE_DATA_DIR to use another directory",
                                root.display()
                            ),
                        ));
                    }
                }
            }
        }
    }
}

fn read_index(db: &Db, connection: &turso::Connection) -> io::Result<IndexFile> {
    fn rows<T: serde::de::DeserializeOwned>(
        db: &Db,
        connection: &turso::Connection,
        table: &str,
    ) -> io::Result<Vec<T>> {
        let mut values = Vec::new();
        db.query(
            connection,
            &format!("SELECT id, body FROM {table} ORDER BY rowid"),
            (),
            |row| {
                let body = blob(row, 1)?;
                match serde_json::from_slice(&body) {
                    Ok(value) => values.push(value),
                    Err(error) => log::warn!(
                        "skipping {table} row {:?} this build cannot read: {error}",
                        db::text(row, 0).unwrap_or_default()
                    ),
                }
                Ok(())
            },
        )?;
        Ok(values)
    }
    Ok(IndexFile {
        projects: rows(db, connection, "projects")?,
        sessions: rows(db, connection, "sessions")?,
    })
}

/// The raw segments of a native log: every line with its `\n`, and an
/// unterminated last line as it is.
fn segments(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.split_inclusive(|byte| *byte == b'\n')
}

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

fn apply_op(
    db: &Db,
    connection: &turso::Connection,
    op: &Op,
    icons: &mut Vec<Option<PathBuf>>,
) -> io::Result<()> {
    match op {
        Op::AppendEvent { session_id, line } => {
            let mut last = None;
            db.query(
                connection,
                "SELECT position, substr(line, -1) = x'0a' FROM events \
                 WHERE session_id = ?1 ORDER BY position DESC LIMIT 1",
                (session_id.as_str(),),
                |row| {
                    last = Some((integer(row, 0)?, integer(row, 1)? != 0));
                    Ok(())
                },
            )?;
            let next = match last {
                None => 0,
                Some((position, terminated)) => {
                    if !terminated {
                        // A log imported without a final newline: terminate
                        // its last line so the new record starts a line.
                        let mut unterminated = Vec::new();
                        db.query(
                            connection,
                            "SELECT line FROM events WHERE session_id = ?1 AND position = ?2",
                            (session_id.as_str(), position),
                            |row| {
                                unterminated = blob(row, 0)?;
                                Ok(())
                            },
                        )?;
                        unterminated.push(b'\n');
                        db.execute(
                            connection,
                            "UPDATE events SET line = ?3 WHERE session_id = ?1 AND position = ?2",
                            (session_id.as_str(), position, unterminated),
                        )?;
                    }
                    position + 1
                }
            };
            insert_segments(db, connection, session_id, next, [line])?;
        }
        Op::ReplaceEventLog { session_id, bytes } => {
            db.execute(
                connection,
                "DELETE FROM events WHERE session_id = ?1",
                (session_id.as_str(),),
            )?;
            insert_segments(db, connection, session_id, 0, segments(bytes))?;
        }
        Op::CloneEvents { src, dst } => {
            db.execute(
                connection,
                "DELETE FROM events WHERE session_id = ?1",
                (dst.as_str(),),
            )?;
            db.execute(
                connection,
                "INSERT INTO events (session_id, position, line) \
                 SELECT ?2, position, line FROM events WHERE session_id = ?1",
                (src.as_str(), dst.as_str()),
            )?;
        }
        Op::UpsertMeta(meta) => {
            let body = serde_json::to_vec(meta.as_ref()).map_err(invalid_data)?;
            db.execute(
                connection,
                "INSERT INTO sessions (id, body) VALUES (?1, ?2) \
                 ON CONFLICT (id) DO UPDATE SET body = excluded.body",
                (meta.id.as_str(), body),
            )?;
        }
        Op::UpsertProject(project) => {
            if let Some(previous) = stored_project(db, connection, &project.id)?
                && previous.icon_path != project.icon_path
            {
                icons.push(previous.icon_path);
            }
            let body = serde_json::to_vec(project.as_ref()).map_err(invalid_data)?;
            db.execute(
                connection,
                "INSERT INTO projects (id, body) VALUES (?1, ?2) \
                 ON CONFLICT (id) DO UPDATE SET body = excluded.body",
                (project.id.as_str(), body),
            )?;
        }
        Op::RemoveSession(id) => {
            db.execute(
                connection,
                "DELETE FROM events WHERE session_id = ?1",
                (id.as_str(),),
            )?;
            db.execute(
                connection,
                "DELETE FROM sessions WHERE id = ?1",
                (id.as_str(),),
            )?;
        }
        Op::RemoveProject(id) => {
            if let Some(previous) = stored_project(db, connection, id)? {
                icons.push(previous.icon_path);
            }
            db.execute(
                connection,
                "DELETE FROM projects WHERE id = ?1",
                (id.as_str(),),
            )?;
        }
    }
    Ok(())
}

fn stored_project(
    db: &Db,
    connection: &turso::Connection,
    id: &str,
) -> io::Result<Option<Project>> {
    let mut project = None;
    db.query(
        connection,
        "SELECT body FROM projects WHERE id = ?1",
        (id,),
        |row| {
            project = serde_json::from_slice(&blob(row, 0)?).ok();
            Ok(())
        },
    )?;
    Ok(project)
}

fn invalid_data(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

/// Parse one JSONL line into a [`StoredEvent`], accepting both the timestamped
/// envelope and the legacy bare-event form. Envelope is tried first; a bare
/// event lacks the `ts`/`event` keys so it can't masquerade as one, and an
/// envelope lacks the top-level `type` tag so it can't parse as a bare event.
pub(crate) fn parse_stored_line(line: &str) -> Result<StoredEvent, serde_json::Error> {
    match serde_json::from_str::<EventEnvelope>(line) {
        Ok(envelope) => Ok(StoredEvent {
            ts: Some(envelope.ts),
            event: envelope.event,
            elided: None,
        }),
        Err(_envelope_err) => match serde_json::from_str::<AgentEvent>(line) {
            Ok(event) => Ok(StoredEvent {
                ts: None,
                event,
                elided: None,
            }),
            // Both forms failed: the line is genuinely corrupt. The bare-event
            // error is the more informative one (the envelope attempt always
            // fails on a bare event merely because `ts` is missing).
            Err(bare_err) => Err(bare_err),
        },
    }
}

pub use tcode_core::project::now_secs;

/// Current wall-clock time in unix milliseconds (used for event envelopes).
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
