//! Persistence for tcode threads.
//!
//! Projects, thread metadata and every thread's event log live in one Turso
//! database, `tcode.db`, in the data dir ([`data_dir`]):
//!   * `projects` and `sessions` hold each [`Project`] / [`SessionMeta`] as the
//!     JSON serde produces for it.
//!   * `threads` gives every thread a key; `events` holds each thread's log
//!     as one row per line, keyed by the thread's key and the line's position
//!     ([`rows`]). A row's canonical bytes are a `{ ts, event }` record, or
//!     whatever a migrated or imported log contained, with its `\n`; it is
//!     stored compressed, with its base64 images in files under `blobs/`
//!     ([`codec`], [`blobs`]). Positions only grow, and a gap is a row that
//!     held nothing a fold needs: a turn-changes snapshot that a later one
//!     supersedes is kept without its diffs
//!     ([`tcode_core::session::TurnSnapshots`]), and the streamed deltas an
//!     item's completion repeats are not kept at all
//!     ([`tcode_core::session::ItemStreams`]).
//!   * `turn_index` holds append-maintained turn boundaries.
//!   * `kept_worktrees` holds the path of every worktree whose thread was
//!     deleted with the worktree kept.
//!
//! An older build's `tcode.db` is moved into this layout in the background,
//! a thread at a time, while the store serves ([`source`], [`moving`]).
//!
//! Model and command caches stay as JSON files beside it.
//!
//! Replay accepts timestamped records and legacy bare [`AgentEvent`] lines,
//! reads any other row as it is stored ([`read_row`]), then folds
//! [`StoredEvent`]s into a [`tcode_core::session::Timeline`].

mod activity;
mod blobs;
mod codec;
mod compact;
mod db;
mod log_read;
mod migrate;
mod moving;
mod projection;
mod relocate;
mod rows;
mod search;
mod source;
#[cfg(test)]
mod tests;
mod turn_index;

pub use compact::{Compaction, CompactionPlan};
pub use log_read::LogRead;
pub use migrate::{Migration, MigrationPhase, MigrationProgress};
pub use moving::{ChunkWait, MovePlan, StepOutcome, StorageStep, StorageWork};
pub use projection::ProjectionPublication;
pub use search::{IndexedEntry, SearchMatch, SearchSource};
pub use turn_index::{CloneBound, TurnIndex};

use std::collections::HashMap;
use std::fs::{self, File};
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use agent::{AgentEvent, ModelSpec, ProviderCommand, ProviderKind};
use serde::{Deserialize, Serialize};

use tcode_core::project::{IndexFile, Project, SessionMeta};
use tcode_core::session::{Author, StoredEvent, SupersededStream};

use codec::{Decoder, Encoder, Mode};
use db::{Broken, Db, SCHEMA_VERSION, blob, integer, is_broken};
use rows::{Storage, Stored};

const DATA_DIR_ENV: &str = "TCODE_DATA_DIR";
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
    #[serde(default)]
    origin: Option<tcode_core::session::MessageOrigin>,
    #[serde(default)]
    author: Option<Author>,
    ts: u64,
    event: AgentEvent,
}

/// Stamped on every item row, as `file_diff_version` is on every file
/// change: the releases from v0.2.1 to v0.4.0 open this database and convert
/// a row without them from an older shape.
const ITEM_VERSION: u8 = 1;

#[derive(Serialize)]
struct EventEnvelopeRef<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    origin: Option<tcode_core::session::MessageOrigin>,
    #[serde(skip_serializing_if = "Option::is_none")]
    author: Option<&'a Author>,
    ts: u64,
    event: &'a AgentEvent,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_diff_version: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    item_version: Option<u8>,
}

impl<'a> EventEnvelopeRef<'a> {
    fn new(
        ts: u64,
        event: &'a AgentEvent,
        author: Option<&'a Author>,
        origin: Option<tcode_core::session::MessageOrigin>,
    ) -> Self {
        let (file_change, item) = match event {
            AgentEvent::ItemStarted(item)
            | AgentEvent::ItemUpdated(item)
            | AgentEvent::ItemCompleted(item) => (
                matches!(item.content, agent::ItemContent::FileChange { .. }),
                true,
            ),
            AgentEvent::ApprovalRequested(request) => (
                matches!(request.kind, agent::ApprovalKind::FileChange { .. }),
                false,
            ),
            _ => (false, false),
        };
        Self {
            origin,
            author,
            ts,
            event,
            file_diff_version: file_change.then_some(1),
            item_version: item.then_some(ITEM_VERSION),
        }
    }
}

/// Which installation a cached command list belongs to. Native commands
/// depend on the provider's home (its plugins, skills and settings), so two
/// profiles with different homes never seed each other's menus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandsCacheKey {
    /// `home` is the profile's home override; `None` is the CLI's default.
    Native {
        provider: ProviderKind,
        home: Option<PathBuf>,
    },
    Acp {
        agent_id: String,
    },
}

impl CommandsCacheKey {
    /// `None` for an ACP session without an agent id.
    pub fn new(
        provider: ProviderKind,
        home: Option<PathBuf>,
        acp_agent_id: Option<&str>,
    ) -> Option<Self> {
        match provider {
            ProviderKind::Acp => acp_agent_id.map(|agent_id| Self::Acp {
                agent_id: agent_id.to_string(),
            }),
            provider => Some(Self::Native { provider, home }),
        }
    }
}

/// 64-bit FNV-1a: a short, stable file-name segment for a home path.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Cheap, cloneable handle to the data directory. Every handle for the same
/// directory in this process shares one database, writer and lifecycle.
#[derive(Clone)]
pub struct SessionStore {
    root: PathBuf,
    /// An older build's data dir that moves into `root` before the store
    /// opens ([`SessionStore::migrate`]).
    previous: Option<PathBuf>,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for SessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionStore")
            .field("root", &self.root)
            .field("previous", &self.previous)
            .finish_non_exhaustive()
    }
}

/// Whether the entry `name` directly inside a data dir belongs to the thread
/// store: its database and the files beside it, an older build's database
/// while its threads move, and the blobs cut out of stored rows.
pub fn holds_conversations(name: &std::ffi::OsStr) -> bool {
    name == blobs::DIR || name.to_str().is_some_and(|name| name.starts_with(DB_FILE))
}

/// The data dir: `TCODE_DATA_DIR` when it is set — a throwaway profile (its
/// own sessions, settings and installed ACP agents) for demos and screenshots —
/// else `~/.tcode` on every platform.
pub fn data_dir() -> io::Result<PathBuf> {
    if let Some(dir) = std::env::var_os(DATA_DIR_ENV).filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    dirs::home_dir()
        .map(|home| home.join(".tcode"))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "no home directory to keep Tcode's data in (~/.tcode); set {DATA_DIR_ENV} to \
                     choose one"
                ),
            )
        })
}

struct Shared {
    state: Mutex<State>,
    /// Signalled whenever an operation finishes, for [`SessionStore::close`].
    idle: Condvar,
    /// Per-session event-log generation, advanced after every committed
    /// change to that log; see [`SessionStore::event_generation`].
    generations: Mutex<HashMap<String, u64>>,
    next_generation: AtomicU64,
}

enum State {
    /// Opened on first use, so a handle that only needs the directory (a
    /// subcommand, a settings file) never takes the data dir.
    Unopened,
    /// The data dir is owned, for a migration and then the database, but the
    /// database is not open yet.
    Owned(File),
    /// [`SessionStore::migrate`] holds the ownership lock while it runs.
    Migrating,
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
    db: Arc<Storage>,
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
    AppendEvent {
        session_id: String,
        line: Vec<u8>,
        projection: Option<(String, Vec<u8>)>,
        record: Box<StoredEvent>,
        /// `None` when no fold followed the log up to this record.
        streams: Option<Option<SupersededStream<u64>>>,
    },
    ReplaceEventLog {
        session_id: String,
        bytes: Vec<u8>,
    },
    CloneEvents {
        src: String,
        dst: String,
        bound: Option<CloneBound>,
    },
    UpsertMeta(Box<SessionMeta>),
    UpsertProject(Box<Project>),
    RemoveSession(String),
    RemoveProject(String),
    KeepWorktree(PathBuf),
}

impl Mutation {
    /// Append one event, wrapped in a timestamped envelope
    /// (`{"ts": <unix_ms>, "event": {…}}`).
    pub fn append_event(session_id: &str, ts: u64, event: &AgentEvent) -> io::Result<Self> {
        Self::append_authored_event(session_id, ts, event, None, None, |_| None)
    }

    /// `project` returns a derived body using the same policy/version as
    /// [`LogRead::visit_projected_rows`]; it never replaces the canonical event.
    pub fn append_authored_event(
        session_id: &str,
        ts: u64,
        event: &AgentEvent,
        author: Option<&Author>,
        origin: Option<tcode_core::session::MessageOrigin>,
        project: impl FnOnce(usize) -> Option<(String, Vec<u8>)>,
    ) -> io::Result<Self> {
        let mut line = serde_json::to_vec(&EventEnvelopeRef::new(ts, event, author, origin))
            .map_err(invalid_data)?;
        line.push(b'\n');
        let projection = project(line.len());
        Ok(Self(Op::AppendEvent {
            session_id: session_id.to_owned(),
            line,
            projection,
            record: Box::new(StoredEvent {
                ts: Some(ts),
                event: event.clone(),
                author: author.cloned(),
                origin,
                elided: None,
            }),
            streams: None,
        }))
    }

    /// An append whose log a fold followed up to it, with the deltas the fold
    /// found it supersedes: they leave the log in the same transaction. An
    /// append no fold followed leaves its thread to the compaction pass.
    pub fn superseding(mut self, superseded: Option<SupersededStream<u64>>) -> Self {
        if let Op::AppendEvent { streams, .. } = &mut self.0 {
            *streams = Some(superseded);
        }
        self
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
            bound: None,
        })
    }

    /// Give `dst` the rows of `src`'s log up to `bound`.
    pub fn clone_events_to(src: &str, dst: &str, bound: CloneBound) -> Self {
        Self(Op::CloneEvents {
            src: src.to_owned(),
            dst: dst.to_owned(),
            bound: Some(bound),
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

    /// Record that the worktree at `path` outlives its deleted thread.
    pub fn keep_worktree(path: &Path) -> Self {
        Self(Op::KeepWorktree(path.to_owned()))
    }

    /// Approximate bytes this change writes, for bounding a batch.
    pub fn payload_len(&self) -> usize {
        match &self.0 {
            Op::AppendEvent {
                line, projection, ..
            } => line.len() + projection.as_ref().map_or(0, |(_, body)| body.len()),
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
            Op::UpsertMeta(_)
            | Op::UpsertProject(_)
            | Op::RemoveProject(_)
            | Op::KeepWorktree(_) => None,
        }
    }
}

impl SessionStore {
    /// A host's store: in `root` when one is given (`tcode-headless
    /// --data-dir`), else in [`data_dir`]. An older build's data dir moves in
    /// first, from `LEGACY_TCODE_DATA_DIR` when it is set, else — unless the
    /// data dir was chosen explicitly — from the platform data dir an older
    /// build used; see [`SessionStore::needs_migration`].
    pub fn open_host(root: Option<PathBuf>) -> io::Result<Self> {
        let explicit =
            root.is_some() || std::env::var_os(DATA_DIR_ENV).is_some_and(|dir| !dir.is_empty());
        let root = match root {
            Some(root) => root,
            None => data_dir()?,
        };
        let mut store = Self::open_at(root)?;
        store.previous = relocate::source(explicit);
        Ok(store)
    }

    /// A handle to `root`, created if needed, that never moves an older data
    /// dir in. The database is not opened until [`SessionStore::open`] or the
    /// first operation that needs it.
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
                });
                stores.insert(key, Arc::downgrade(&shared));
                shared
            }
        };
        Ok(Self {
            root,
            previous: None,
            shared,
        })
    }

    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    /// Take ownership of the data dir and open its database now. A data dir
    /// that still needs migrating ([`SessionStore::needs_migration`]) is
    /// refused. Fails with
    /// [`io::ErrorKind::ResourceBusy`] while another host owns the directory.
    pub fn open(&self) -> io::Result<()> {
        self.run("open", |_| Ok(()))
    }

    /// Take ownership of the data dir, keeping it for the database opened
    /// later, and say whether [`SessionStore::migrate`] must prepare it before
    /// the store opens: an older build's data dir is still to move in, or the
    /// data dir holds the JSON index or JSONL logs of an older build that must
    /// move into `tcode.db`. Fails with [`io::ErrorKind::ResourceBusy`] while
    /// another host owns the directory.
    pub fn needs_migration(&self) -> io::Result<bool> {
        let mut state = self.shared.lock_state()?;
        match &*state {
            State::Unopened => *state = State::Owned(acquire_ownership(&self.root)?),
            State::Owned(_) => {}
            State::Open(_) => return Ok(false),
            other => return Err(unavailable(other, "check for a migration")),
        }
        Ok(relocate::needed(&self.root, self.previous.as_deref())? || migrate::needed(&self.root)?)
    }

    /// The older data dir still to move into this one, without taking
    /// ownership: whatever is written to the data dir before the move would
    /// stop it with a collision.
    pub fn pending_relocation(&self) -> io::Result<Option<&Path>> {
        Ok(match self.previous.as_deref() {
            Some(previous) if relocate::needed(&self.root, Some(previous))? => Some(previous),
            _ => None,
        })
    }

    /// Prepare the data dir under its ownership lock, which the store keeps
    /// for the database afterwards: first move an older build's data dir in
    /// ([`MigrationPhase::Relocating`]), then migrate an older build's JSON
    /// index and JSONL logs into `tcode.db`. `progress` is called after every
    /// entry moved, every chunk of about 8 MiB copied or imported, and every
    /// log; `cancel` is checked at the same points and between phases until
    /// the migrated database is complete. A cancelled move keeps the entries
    /// it moved and is continued by the next start; a cancelled migration
    /// removes its staging files and changes nothing else, and a later one
    /// starts over.
    pub fn migrate(
        &self,
        mut progress: impl FnMut(MigrationProgress),
        cancel: &AtomicBool,
    ) -> io::Result<Migration> {
        let ownership = {
            let mut state = self.shared.lock_state()?;
            match std::mem::replace(&mut *state, State::Migrating) {
                State::Unopened => match acquire_ownership(&self.root) {
                    Ok(ownership) => ownership,
                    Err(error) => {
                        *state = State::Unopened;
                        return Err(error);
                    }
                },
                State::Owned(ownership) => ownership,
                State::Open(live) => {
                    *state = State::Open(live);
                    return Ok(Migration::Completed);
                }
                other => {
                    let error = unavailable(&other, "migrate");
                    *state = other;
                    return Err(error);
                }
            }
        };
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            match relocate::run(&self.root, self.previous.as_deref(), &mut progress, cancel)? {
                Migration::Cancelled => Ok(Migration::Cancelled),
                Migration::Completed => migrate::run(&self.root, &mut progress, cancel),
            }
        }));
        let mut state = self.shared.lock_state()?;
        let result = match outcome {
            Ok(result) => {
                *state = State::Owned(ownership);
                result
            }
            Err(panic) => {
                let reason = format!(
                    "session store panicked while migrating {}: {}",
                    self.root.display(),
                    panic_message(panic.as_ref())
                );
                log::error!("{reason}");
                *state = State::Failed {
                    reason: reason.clone(),
                    _ownership: Some(ownership),
                };
                Err(io::Error::other(reason))
            }
        };
        self.shared.idle.notify_all();
        result
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

    /// Wait for every in-flight operation and a running migration, checkpoint
    /// the WAL into the main file and release the database and the data dir.
    /// Every handle of this store fails afterwards; a store that already
    /// failed is not checkpointed.
    pub fn close(&self) -> io::Result<()> {
        let mut state = self.shared.lock_state()?;
        loop {
            match std::mem::replace(&mut *state, State::Closed) {
                State::Unopened | State::Closed => return Ok(()),
                State::Owned(ownership) => return ownership.unlock(),
                State::Migrating => {
                    *state = State::Migrating;
                    state = self
                        .shared
                        .idle
                        .wait(state)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                }
                State::Open(live) => {
                    live.db.cancel_reads();
                    *state = State::Closing(live);
                }
                State::Closing(live) if live.in_flight > 0 => {
                    *state = State::Closing(live);
                    state = self
                        .shared
                        .idle
                        .wait(state)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                }
                State::Closing(live) => {
                    let Live { db, ownership, .. } = live;
                    db.close_readers();
                    let result = match Arc::try_unwrap(db) {
                        Ok(storage) => {
                            catch_unwind(AssertUnwindSafe(|| storage.into_db().checkpoint()))
                                .unwrap_or_else(|panic| {
                                    Err(io::Error::other(format!(
                                        "session store panicked during the closing checkpoint: {}",
                                        panic_message(panic.as_ref())
                                    )))
                                })
                        }
                        Err(_) => Err(io::Error::other(
                            "session store is still referenced after its last operation",
                        )),
                    };
                    // A concurrently launching child can inherit the lock descriptor
                    // until exec. Closing our copy alone leaves that child holding it.
                    let result = result.and(ownership.unlock());
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
    fn run<R>(
        &self,
        operation: &str,
        body: impl FnOnce(&Storage) -> io::Result<R>,
    ) -> io::Result<R> {
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

    fn acquire(&self, operation: &str) -> io::Result<Arc<Storage>> {
        let mut state = self.shared.lock_state()?;
        let ownership = match std::mem::replace(&mut *state, State::Unopened) {
            State::Unopened => Some(acquire_ownership(&self.root)?),
            State::Owned(ownership) => Some(ownership),
            other => {
                *state = other;
                None
            }
        };
        if let Some(ownership) = ownership {
            // Dropping the ownership on failure lets the next attempt start over.
            match catch_unwind(AssertUnwindSafe(|| {
                open_live(&self.root, self.previous.as_deref(), ownership)
            })) {
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
            other => Err(unavailable(other, operation)),
        }
    }

    /// Commit `mutations` in one transaction, then run their deferred side
    /// effects. Either every change is committed or none is.
    pub fn apply(&self, mutations: &[Mutation]) -> io::Result<()> {
        if mutations.is_empty() {
            return Ok(());
        }
        let icons = self.run("write", |st| {
            st.write(|st, connection| {
                let codec = st.codec();
                let mut encoder = codec.encoder()?;
                let mut icons = Vec::new();
                let mut indices = turn_index::Batch::new();
                for mutation in mutations {
                    if !matches!(mutation.0, Op::AppendEvent { .. }) {
                        turn_index::flush(st, connection, &indices)?;
                        indices.clear();
                    }
                    apply_op(
                        st,
                        connection,
                        &mut encoder,
                        &mutation.0,
                        &mut icons,
                        &mut indices,
                    )?;
                }
                turn_index::flush(st, connection, &indices)?;
                Ok(icons)
            })
        })?;
        self.advance_event_generations(mutations.iter().filter_map(Mutation::event_log));
        for icon in icons {
            self.remove_project_icon(icon);
        }
        Ok(())
    }

    fn advance_event_generations<'a>(&self, ids: impl IntoIterator<Item = &'a str>) {
        let mut generations = self
            .shared
            .generations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for id in ids {
            let generation = self.shared.next_generation.fetch_add(1, Ordering::Relaxed);
            generations.insert(id.to_owned(), generation);
        }
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
        self.run("read event log", |st| {
            st.read(|st, connection| {
                let mut bytes = Vec::new();
                let Some(thread) = rows::thread(st, connection, id)? else {
                    return Ok(bytes);
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
                        bytes.extend(canonical(st, &mut decoder, &row, id, position)?);
                        Ok(true)
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

    fn commands_path(&self, key: &CommandsCacheKey) -> PathBuf {
        let name = match key {
            CommandsCacheKey::Native { provider, home } => {
                let provider = match provider {
                    ProviderKind::Codex => "codex",
                    ProviderKind::ClaudeCode => "claude",
                    ProviderKind::Pi => "pi",
                    ProviderKind::OpenCode => "opencode",
                    ProviderKind::Cursor => "cursor",
                    ProviderKind::Grok => "grok",
                    ProviderKind::Acp => "acp",
                };
                let home = home.as_deref().map_or_else(
                    || "default".to_string(),
                    |home| format!("{:016x}", fnv1a(home.as_os_str().as_encoded_bytes())),
                );
                format!("{provider}-{home}")
            }
            CommandsCacheKey::Acp { agent_id } => {
                // Registry ids are external input and may contain path separators.
                // Hex keeps the filename reversible and collision-free without
                // allowing an id to escape the data directory.
                let encoded = agent_id
                    .as_bytes()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
                format!("acp-{encoded}")
            }
        };
        self.root.join(format!("commands-{name}.json"))
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

    /// Load the most recently reported command/skill list for one native
    /// installation or ACP agent. Empty when missing or unreadable.
    pub fn load_commands(&self, key: &CommandsCacheKey) -> Vec<ProviderCommand> {
        let Ok(bytes) = fs::read(self.commands_path(key)) else {
            return Vec::new();
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    /// Atomically persist the complete command/skill list reported by one
    /// native installation or ACP agent. Empty lists are meaningful: they
    /// replace a stale non-empty cache.
    pub fn save_commands(
        &self,
        key: &CommandsCacheKey,
        commands: &[ProviderCommand],
    ) -> io::Result<()> {
        let path = self.commands_path(key);
        let tmp = path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(commands).map_err(invalid_data)?;
        fs::write(&tmp, data)?;
        fs::rename(&tmp, path)
    }

    /// Forget a cached command list after the installation changed under it.
    pub fn invalidate_commands(&self, key: &CommandsCacheKey) -> io::Result<()> {
        match fs::remove_file(self.commands_path(key)) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    /// Load every project and session, in the order they were first stored.
    /// A row this build cannot decode is skipped with a warning and left in
    /// the database untouched.
    pub fn read_file(&self) -> io::Result<IndexFile> {
        self.run("read index", |st| {
            st.read(|st, connection| {
                // One read transaction, so projects and sessions come from the
                // same snapshot.
                st.execute(connection, "BEGIN", ())?;
                let file = read_index(st, connection);
                st.execute(connection, "COMMIT", ())?;
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

    /// Read every persisted record of a session ([`read_row`]).
    pub fn read_events(&self, id: &str) -> io::Result<Vec<StoredEvent>> {
        self.read_log(id).map(|log| log.records)
    }

    /// [`SessionStore::read_events`] with the stored row each record came
    /// from and what the read skipped.
    pub fn read_log(&self, id: &str) -> io::Result<EventLog> {
        self.read_log_until(id, u64::MAX)
    }

    /// [`SessionStore::read_log`] of the rows before position `end` alone:
    /// what the log held when [`SessionStore::next_row`] returned `end`, since
    /// rows are only ever appended.
    pub fn read_log_until(&self, id: &str, end: u64) -> io::Result<EventLog> {
        self.read_rows(id, 0..end)
    }

    /// The records of the rows at `positions` of a session's log, without
    /// reading the rest of it.
    pub fn read_rows(&self, id: &str, positions: std::ops::Range<u64>) -> io::Result<EventLog> {
        self.run("read events", |st| {
            st.read(|st, connection| {
                let mut log = EventLog::default();
                let Some(thread) = rows::thread(st, connection, id)? else {
                    return Ok(log);
                };
                let codec = st.codec();
                let mut decoder = codec.decoder();
                rows::visit(
                    st,
                    connection,
                    &thread,
                    positions,
                    false,
                    |position, row| {
                        log.next_row = position + 1;
                        let line = canonical(st, &mut decoder, &row, id, position)?;
                        if let Some(record) = read_row(&line) {
                            log.records.push(record);
                            log.rows.push(position);
                        }
                        Ok(true)
                    },
                )?;
                Ok(log)
            })
        })
    }

    /// Remove a session from the index and delete its event log.
    pub fn remove_session(&self, id: &str) -> io::Result<()> {
        self.apply(&[Mutation::remove_session(id)])
    }

    /// Every worktree recorded by [`Mutation::keep_worktree`].
    pub fn kept_worktrees(&self) -> io::Result<Vec<PathBuf>> {
        self.run("read kept worktrees", |st| {
            st.read(|st, connection| {
                let mut paths = Vec::new();
                st.query(connection, "SELECT path FROM kept_worktrees", (), |row| {
                    paths.push(PathBuf::from(db::text(row, 0)?));
                    Ok(())
                })?;
                Ok(paths)
            })
        })
    }
}

/// The canonical bytes of a row. A row whose images are missing reads
/// without them and says so.
fn canonical(
    st: &Storage,
    decoder: &mut Decoder<'_>,
    row: &Stored,
    id: &str,
    position: u64,
) -> io::Result<Vec<u8>> {
    let decoded = st.decode(decoder, row, Mode::Full)?;
    if decoded.missing {
        log::warn!("event {position} of {id} names an image file that is missing or damaged");
    }
    Ok(decoded.bytes)
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
            State::Owned(ownership) => Some(ownership),
            State::Failed { _ownership, .. } => _ownership,
            State::Unopened | State::Migrating | State::Closed => None,
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

/// Why a store in `state` cannot `operation` now.
fn unavailable(state: &State, operation: &str) -> io::Error {
    match state {
        State::Migrating => io::Error::new(
            io::ErrorKind::ResourceBusy,
            format!("cannot {operation}: the data directory is being migrated"),
        ),
        failed @ State::Failed { .. } => failed_error(failed),
        _ => io::Error::new(
            io::ErrorKind::BrokenPipe,
            format!("cannot {operation}: the session store is closed"),
        ),
    }
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

/// Bring `tcode.db` up to date in the data dir `ownership` holds, and open it.
/// A data dir an older one has still to move into is refused, as opening it
/// would create the data that makes the move look done.
fn open_live(root: &Path, previous: Option<&Path>, ownership: File) -> io::Result<Live> {
    if let Some(previous) = previous
        && relocate::needed(root, Some(previous))?
    {
        return Err(io::Error::other(format!(
            "{} has not been moved into {} yet",
            previous.display(),
            root.display()
        )));
    }
    migrate::prepare(root)?;
    if source::needed(root)? {
        source::switch(root)?;
    }
    let path = root.join(DB_FILE);
    let db = Db::open(&path, None)?;
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
    db.write(|db, connection| {
        turn_index::create_tables(db, connection)?;
        search::create_tables(db, connection)?;
        activity::create_table(db, connection)?;
        projection::create_cache(db, connection)
    })?;
    let codec = moving::load_codec(&db)?;
    let source = source::Source::open(root)?;
    migrate::warn_about_stray_sources(root);
    migrate::remove_legacy(root);
    Ok(Live {
        db: Arc::new(Storage::new(db, root, codec, source)),
        ownership,
        in_flight: 0,
    })
}

/// A session's event log as one read of its rows saw it.
#[derive(Debug, Default)]
pub struct EventLog {
    pub records: Vec<StoredEvent>,
    /// The position of the row each record was read from.
    pub rows: Vec<u64>,
    /// The position after the last row read.
    pub next_row: u64,
}

/// The record a row holds for every reader that folds it: what it decodes
/// to, or else the row as it is stored ([`AgentEvent::UnreadableRecord`]),
/// timed by its envelope's `ts` when it has one. `None` for a blank row.
pub(crate) fn read_row(line: &[u8]) -> Option<StoredEvent> {
    match decode_row(line) {
        Ok(record) => record,
        Err(_) => Some(unreadable_row(line)),
    }
}

fn unreadable_row(line: &[u8]) -> StoredEvent {
    let line = String::from_utf8_lossy(line);
    let line = line.trim_end_matches(['\n', '\r']);
    let ts = serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|row| row.get("ts")?.as_u64());
    StoredEvent {
        origin: None,
        author: None,
        ts,
        event: AgentEvent::UnreadableRecord {
            line: line.to_owned(),
        },
        elided: None,
    }
}

fn decode_row(line: &[u8]) -> Result<Option<StoredEvent>, String> {
    let Ok(line) = std::str::from_utf8(line) else {
        return Err("not UTF-8".into());
    };
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    parse_stored_line(trimmed)
        .map(Some)
        .map_err(|error| format!("unparseable: {error}"))
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
    let mut sessions: Vec<SessionMeta> = rows(db, connection, "sessions")?;
    for meta in &mut sessions {
        meta.migrate_lifecycle();
    }
    Ok(IndexFile {
        projects: rows(db, connection, "projects")?,
        sessions,
    })
}

/// The raw segments of a native log: every line with its `\n`, and an
/// unterminated last line as it is.
fn segments(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.split_inclusive(|byte| *byte == b'\n')
}

fn apply_op(
    st: &Storage,
    connection: &turso::Connection,
    encoder: &mut Encoder<'_>,
    op: &Op,
    icons: &mut Vec<Option<PathBuf>>,
    indices: &mut turn_index::Batch,
) -> io::Result<()> {
    match op {
        Op::AppendEvent {
            session_id,
            line,
            projection,
            record,
            streams,
        } => {
            let thread = rows::ensure_thread(st, connection, session_id)?;
            let next = rows::end(st, connection, &thread)?;
            if let Some(last) = next.checked_sub(1)
                && let Some(row) = rows::get(st, connection, &thread, last)?
            {
                let terminated = match &row {
                    Stored::New(bytes) => codec::header(bytes)?.ends_line(),
                    Stored::Old(bytes) => bytes.ends_with(b"\n"),
                };
                if !terminated {
                    // A log imported without a final newline: terminate its
                    // last line so the new record starts a line.
                    let codec = st.codec();
                    let mut decoder = codec.decoder();
                    let decoded = st.decode(&mut decoder, &row, Mode::Full)?;
                    if decoded.missing {
                        return Err(io::Error::other(format!(
                            "cannot end row {last} of {session_id}: an image it names is missing"
                        )));
                    }
                    let mut unterminated = decoded.bytes;
                    unterminated.push(b'\n');
                    rows::put(st, connection, encoder, &thread, last, &unterminated)?;
                }
            }
            rows::put(st, connection, encoder, &thread, next, line)?;
            if let AgentEvent::ItemCompleted(item) = &record.event {
                let removed = match streams {
                    Some(Some(superseded)) => compact::remove_superseded(
                        st, connection, encoder, &thread, &item.id, superseded,
                    )?,
                    Some(None) => true,
                    None => !compact::streamed(&item.content),
                };
                if !removed {
                    rows::needs_compaction(st, connection, thread.key)?;
                }
            }
            if tcode_core::settlement::ThreadActivity::moves_clock(&record.event) {
                activity::append(st, connection, &thread, record)?;
            }
            turn_index::append(st, connection, encoder, indices, &thread, next, record)?;
            if let Some((version, body)) = projection
                && let Err(error) = st.execute(
                    connection,
                    "INSERT INTO event_preview (key, version, token, body) VALUES (?1, ?2, '', ?3)",
                    (thread.row(next)?, version.as_str(), body.as_slice()),
                )
            {
                log::warn!("could not persist appended history preview: {error}");
            }
        }
        Op::ReplaceEventLog { session_id, bytes } => {
            let thread = rows::ensure_thread(st, connection, session_id)?;
            rows::clear(st, connection, &thread)?;
            let thread = rows::ensure_thread(st, connection, session_id)?;
            for (position, line) in segments(bytes).enumerate() {
                rows::put(st, connection, encoder, &thread, position as u64, line)?;
            }
            activity::recompute(st, connection, &thread)?;
            rows::needs_compaction(st, connection, thread.key)?;
            turn_index::forget(st, connection, &thread)?;
        }
        Op::CloneEvents { src, dst, bound } => {
            let target = rows::ensure_thread(st, connection, dst)?;
            rows::clear(st, connection, &target)?;
            let target = rows::ensure_thread(st, connection, dst)?;
            let source = rows::thread(st, connection, src)?;
            let end = match (&source, bound) {
                (Some(source), Some(bound)) => {
                    Some(turn_index::bound_row(st, connection, source, *bound)?)
                }
                _ => None,
            };
            if let Some(source) = &source {
                rows::copy(st, connection, source, &target, end)?;
            }
            let target = rows::ensure_thread(st, connection, dst)?;
            activity::recompute(st, connection, &target)?;
            rows::needs_compaction(st, connection, target.key)?;
            match end {
                Some(_) => turn_index::rebuild(st, connection, &target)?,
                None => turn_index::copy(st, connection, source.as_ref(), &target)?,
            }
        }
        Op::UpsertMeta(meta) => {
            let mut previous_parent = None;
            st.query(
                connection,
                "SELECT json_extract(body, '$.parent_session_id') IS NOT NULL FROM sessions WHERE id = ?1",
                [meta.id.as_str()],
                |row| {
                    previous_parent = Some(integer(row, 0)? != 0);
                    Ok(())
                },
            )?;
            let body = serde_json::to_vec(meta.as_ref()).map_err(invalid_data)?;
            st.execute(
                connection,
                "INSERT INTO sessions (id, body) VALUES (?1, ?2) \
                 ON CONFLICT (id) DO UPDATE SET body = excluded.body",
                (meta.id.as_str(), body),
            )?;
            if previous_parent != Some(meta.parent_session_id.is_some()) {
                let thread = rows::ensure_thread(st, connection, &meta.id)?;
                activity::recompute(st, connection, &thread)?;
            }
        }
        Op::UpsertProject(project) => {
            if let Some(previous) = stored_project(st, connection, &project.id)?
                && previous.icon_path != project.icon_path
            {
                icons.push(previous.icon_path);
            }
            let body = serde_json::to_vec(project.as_ref()).map_err(invalid_data)?;
            st.execute(
                connection,
                "INSERT INTO projects (id, body) VALUES (?1, ?2) \
                 ON CONFLICT (id) DO UPDATE SET body = excluded.body",
                (project.id.as_str(), body),
            )?;
        }
        Op::RemoveSession(id) => {
            if let Some(thread) = rows::thread(st, connection, id)? {
                activity::remove(st, connection, &thread)?;
                rows::clear(st, connection, &thread)?;
                st.execute(
                    connection,
                    "UPDATE threads SET compacted = 1 WHERE key = ?1",
                    [thread.key],
                )?;
                turn_index::forget(st, connection, &thread)?;
                search::forget(st, connection, &thread)?;
            }
            st.execute(
                connection,
                "DELETE FROM sessions WHERE id = ?1",
                (id.as_str(),),
            )?;
        }
        Op::RemoveProject(id) => {
            if let Some(previous) = stored_project(st, connection, id)? {
                icons.push(previous.icon_path);
            }
            st.execute(
                connection,
                "DELETE FROM projects WHERE id = ?1",
                (id.as_str(),),
            )?;
        }
        Op::KeepWorktree(path) => {
            st.execute(
                connection,
                "INSERT INTO kept_worktrees (path) VALUES (?1) ON CONFLICT (path) DO NOTHING",
                (path.to_string_lossy().as_ref(),),
            )?;
        }
    }
    Ok(())
}

fn stored_project(
    db: &Storage,
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

/// One stored line: a timestamped envelope, or a bare event as builds before
/// the envelope wrote it (with `ts == None`).
pub(crate) fn parse_stored_line(line: &str) -> Result<StoredEvent, serde_json::Error> {
    Ok(match serde_json::from_str::<EventEnvelope>(line) {
        Ok(envelope) => StoredEvent {
            origin: envelope.origin,
            author: envelope.author,
            ts: Some(envelope.ts),
            event: envelope.event,
            elided: None,
        },
        Err(_) => StoredEvent {
            origin: None,
            author: None,
            ts: None,
            event: serde_json::from_str::<AgentEvent>(line)?,
            elided: None,
        },
    })
}

pub use tcode_core::project::now_secs;

/// Current wall-clock time in unix milliseconds (used for event envelopes).
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
