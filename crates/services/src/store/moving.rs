//! The background pass that moves an older build's rows into this file,
//! trains the row dictionary, retires the source and collects blobs nothing
//! names any more.
//!
//! Each step is prepared off the writer (reading, folding, encoding, writing
//! blobs) and committed as one short writer transaction, so a step never holds
//! the writer for more than one bounded batch of rows. A thread moves in
//! chunks: each commit checks the thread still has the rows the chunk was
//! read from (`moved`, `old_id`, `old_end`) and advances `moved` with them, so
//! an interruption anywhere resumes from the last commit, and a write that
//! replaced, forked or removed the thread meanwhile makes the chunk stale
//! instead of bringing old rows back. Blobs a stale or interrupted chunk wrote
//! are named by nothing and go with the next collection.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use turso::Connection;

use super::SessionStore;
use super::codec::{self, Codec, Encoded, Hash};
use super::compact::{Decisions, Plan, Planner};
use super::db::{Db, blob, integer};
use super::rows::{self, Storage};

/// A chunk commits at most this many rows ...
const CHUNK_ROWS: usize = 1024;
/// ... and about this many stored bytes.
const CHUNK_BYTES: usize = 4 << 20;
/// Free space a chunk leaves on the disk beyond what it writes.
const RESERVE_BYTES: u64 = 2 << 30;
const SAMPLES: usize = 20_000;
const MIN_SAMPLES: usize = 1000;
const SAMPLE_BYTES: usize = 8 << 10;
const SWEEP_FILES: usize = 1024;
/// A thread whose rows in the source take no more than this is read once:
/// its plan keeps them for its chunks.
const KEPT_BYTES: u64 = 64 << 20;

pub(super) fn load_codec(db: &Db) -> io::Result<Codec> {
    db.read(|db, connection| {
        let mut stored = Vec::new();
        db.query(
            connection,
            "SELECT id, dict FROM codec_dicts ORDER BY id",
            (),
            |row| {
                stored.push((integer(row, 0)? as u32, blob(row, 1)?));
                Ok(())
            },
        )?;
        Codec::new(stored)
    })
}

/// What the pass has left to do.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StorageWork {
    /// No dictionary compresses rows yet.
    pub dictionary: bool,
    /// Threads with rows still in the source.
    pub moves: Vec<String>,
    /// The source is open; once `moves` is done it can be retired.
    pub source: bool,
    /// Threads marked for the compaction pass.
    pub compactions: Vec<String>,
}

/// One step prepared off the writer, for [`SessionStore::commit_storage_step`].
pub struct StorageStep(Step);

enum Step {
    Dictionary(Vec<u8>),
    Chunk(Chunk),
    Sweep {
        candidates: Vec<(PathBuf, Option<Hash>)>,
        cutoff: SystemTime,
    },
}

struct Chunk {
    id: String,
    key: i64,
    old_id: String,
    old_end: u64,
    start: u64,
    end: u64,
    rows: Vec<(u64, Encoded)>,
    read_bytes: u64,
    compacted: bool,
}

impl StorageStep {
    /// Stored bytes the step adds to the database file.
    pub fn bytes(&self) -> u64 {
        match &self.0 {
            Step::Dictionary(bytes) => bytes.len() as u64,
            Step::Chunk(chunk) => chunk
                .rows
                .iter()
                .map(|(_, row)| row.bytes.len() as u64)
                .sum(),
            Step::Sweep { .. } => 0,
        }
    }
}

/// What committing a step did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    Trained,
    /// A dictionary was already stored; rows keep using it.
    AlreadyTrained,
    Moved {
        rows: usize,
        /// Bytes the moved rows held in the source, and hold here.
        before: u64,
        after: u64,
        /// The thread has no rows left in the source.
        done: bool,
    },
    /// The thread changed since the chunk was read; nothing was written.
    Stale,
    Swept {
        files: usize,
        bytes: u64,
    },
}

/// A thread being moved: what its fold decided, and how far it got.
pub struct MovePlan {
    id: String,
    key: i64,
    old_id: String,
    old_end: u64,
    next: u64,
    decisions: Decisions,
    /// The source rows held no undecodable row, so the moved thread needs no
    /// compaction pass unless something else was appended.
    compacted: bool,
    /// The rows still to move, when they were few enough to keep.
    kept: Option<std::collections::VecDeque<(u64, Vec<u8>)>>,
}

/// Why a chunk was not prepared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkWait {
    /// Every row has moved.
    Done,
    /// The disk has `free` bytes, fewer than the chunk needs.
    NoSpace { needed: u64, free: u64 },
}

impl SessionStore {
    /// List the work left, first registering every thread the source holds
    /// that this file does not know yet.
    pub fn storage_work(&self) -> io::Result<StorageWork> {
        self.run("list storage work", |st| {
            let source = st.source();
            if let Some(source) = &source {
                let mut unknown = Vec::new();
                let mut after = None;
                while let Some(id) = source.next_thread(after.as_deref())? {
                    if st
                        .read(|st, connection| rows::thread(st, connection, &id))?
                        .is_none()
                    {
                        unknown.push((id.clone(), source.end(&id)?));
                    }
                    after = Some(id);
                }
                if !unknown.is_empty() {
                    st.write(|st, connection| {
                        for (id, end) in &unknown {
                            rows::register(st, connection, id, *end)?;
                        }
                        Ok(())
                    })?;
                }
                source.enumerated();
            }
            st.read(|st, connection| {
                Ok(StorageWork {
                    dictionary: !st.codec().has_dictionary(),
                    moves: rows::thread_ids(
                        st,
                        connection,
                        "SELECT id FROM threads WHERE old_id IS NOT NULL AND moved < old_end \
                         ORDER BY key",
                    )?,
                    source: source.is_some(),
                    compactions: rows::thread_ids(
                        st,
                        connection,
                        "SELECT t.id FROM threads t JOIN sessions s ON s.id = t.id \
                         WHERE t.compacted = 0 ORDER BY t.key",
                    )?,
                })
            })
        })
    }

    /// How far moving the source's rows has come: how many have moved, of
    /// how many its threads hold; `None` once there is no source.
    pub fn move_progress(&self) -> io::Result<Option<(u64, u64)>> {
        self.run("read the move's progress", |st| {
            if st.source().is_none() {
                return Ok(None);
            }
            st.read(|st, connection| {
                let mut progress = (0, 0);
                st.query(
                    connection,
                    "SELECT COALESCE(SUM(MIN(moved, old_end)), 0), COALESCE(SUM(old_end), 0) \
                     FROM threads WHERE old_id IS NOT NULL",
                    (),
                    |row| {
                        progress = (integer(row, 0)? as u64, integer(row, 1)? as u64);
                        Ok(())
                    },
                )?;
                Ok(Some(progress))
            })
        })
    }

    /// Train the dictionary from rows spread over the source, or this file
    /// when there is none; `None` when there are too few rows yet.
    pub fn prepare_dictionary(&self) -> io::Result<Option<StorageStep>> {
        self.run("train a dictionary", |st| {
            let rows = match st.source() {
                Some(source) => source.sample(SAMPLES)?,
                None => st.read(sample)?,
            };
            let mut samples: Vec<Vec<u8>> = rows
                .iter()
                .map(|row| {
                    let mut bare = codec::split(row).bare;
                    bare.truncate(SAMPLE_BYTES);
                    bare
                })
                .filter(|bare| !bare.is_empty())
                .collect();
            samples.dedup();
            if samples.len() < MIN_SAMPLES {
                return Ok(None);
            }
            Ok(Some(StorageStep(Step::Dictionary(codec::train(&samples)?))))
        })
    }

    /// Fold the thread's rows in the source to decide what moving them
    /// drops; `None` when it has none left to move.
    pub fn plan_move(&self, id: &str) -> io::Result<Option<MovePlan>> {
        self.run("plan a move", |st| {
            let Some(thread) = st.read(|st, connection| rows::thread(st, connection, id))? else {
                return Ok(None);
            };
            let (Some(old_id), Some(source)) = (thread.old_id.clone(), st.source()) else {
                return Ok(None);
            };
            if thread.unmoved().is_empty() {
                return Ok(None);
            }
            let mut planner = Planner::default();
            let mut kept = Some(std::collections::VecDeque::new());
            let mut read = 0;
            source.visit(&old_id, 0..thread.old_end, false, &mut |position, line| {
                let split = codec::split(&line);
                planner.push(position, &split.bare, !split.refs.is_empty());
                read += line.len() as u64;
                if read > KEPT_BYTES {
                    kept = None;
                } else if let Some(kept) = kept.as_mut()
                    && position >= thread.moved
                {
                    kept.push_back((position, line));
                }
                Ok(true)
            })?;
            let (decisions, compacted) = match planner.finish() {
                Plan::Decided(decisions) => (decisions, true),
                Plan::Undecodable(position) => {
                    log::warn!(
                        "moving {id} as it is: its row {position} holds no record this build reads"
                    );
                    (Decisions::default(), false)
                }
            };
            Ok(Some(MovePlan {
                id: id.to_owned(),
                key: thread.key,
                old_id,
                old_end: thread.old_end,
                next: thread.moved,
                decisions,
                compacted,
                kept,
            }))
        })
    }

    /// Read, decide and encode the plan's next chunk, putting the blobs it
    /// names on disk. The plan moves past the chunk; a chunk whose commit
    /// comes back [`StepOutcome::Stale`] ends the plan.
    pub fn prepare_move_chunk(
        &self,
        plan: &mut MovePlan,
    ) -> io::Result<Result<StorageStep, ChunkWait>> {
        if plan.next >= plan.old_end {
            return Ok(Err(ChunkWait::Done));
        }
        self.run("prepare a move", |st| {
            let Some(source) = st.source() else {
                return Ok(Err(ChunkWait::Done));
            };
            let mut lines = Vec::new();
            let mut read_bytes = 0;
            let mut end = plan.next;
            let mut take = |position: u64, line: Vec<u8>| {
                if lines.len() >= CHUNK_ROWS || read_bytes >= CHUNK_BYTES as u64 * 4 {
                    return Some((position, line));
                }
                read_bytes += line.len() as u64;
                end = position + 1;
                lines.push((position, line));
                None
            };
            match plan.kept.as_mut() {
                Some(kept) => {
                    while let Some((position, line)) = kept.pop_front() {
                        if let Some(back) = take(position, line) {
                            kept.push_front(back);
                            break;
                        }
                    }
                }
                None => source.visit(
                    &plan.old_id,
                    plan.next..plan.old_end,
                    false,
                    &mut |position, line| Ok(take(position, line).is_none()),
                )?,
            }
            if lines.is_empty() {
                end = plan.old_end;
            }
            let needed = read_bytes + RESERVE_BYTES;
            let free = free_space(&st.root)?;
            if free < needed {
                return Ok(Err(ChunkWait::NoSpace { needed, free }));
            }
            let codec = st.codec();
            let mut encoder = codec.encoder()?;
            let mut rows = Vec::with_capacity(lines.len());
            let mut stored = 0;
            let mut lines = lines.into_iter();
            for (position, line) in lines.by_ref() {
                let Some(line) = plan.decisions.apply(position, line)? else {
                    continue;
                };
                let encoded = encoder.encode(&line)?;
                stored += encoded.bytes.len();
                rows.push((position, encoded));
                if stored >= CHUNK_BYTES {
                    end = position + 1;
                    break;
                }
            }
            let rest: Vec<_> = lines.collect();
            read_bytes -= rest.iter().map(|(_, line)| line.len() as u64).sum::<u64>();
            if let Some(kept) = plan.kept.as_mut() {
                for row in rest.into_iter().rev() {
                    kept.push_front(row);
                }
            }
            let blobs: Vec<_> = rows.iter().flat_map(|(_, row)| row.blobs.clone()).collect();
            st.blobs.put(&blobs)?;
            let start = std::mem::replace(&mut plan.next, end);
            Ok(Ok(StorageStep(Step::Chunk(Chunk {
                id: plan.id.clone(),
                key: plan.key,
                old_id: plan.old_id.clone(),
                old_end: plan.old_end,
                start,
                end,
                rows,
                read_bytes,
                compacted: plan.compacted,
            }))))
        })
    }

    /// The blob files written before `cutoff`, for a sweep; `None` while the
    /// source is open, since moving writes blobs before their rows commit.
    pub fn prepare_sweep(&self, cutoff: SystemTime) -> io::Result<Vec<StorageStep>> {
        self.run("list blobs", |st| {
            if st.source().is_some() {
                return Ok(Vec::new());
            }
            let candidates = st.blobs.older_than(cutoff)?;
            Ok(candidates
                .chunks(SWEEP_FILES)
                .map(|candidates| {
                    StorageStep(Step::Sweep {
                        candidates: candidates.to_vec(),
                        cutoff,
                    })
                })
                .collect())
        })
    }

    /// Commit a prepared step in one writer transaction.
    pub fn commit_storage_step(&self, step: StorageStep) -> io::Result<StepOutcome> {
        self.run("commit a storage step", |st| match step.0 {
            Step::Dictionary(dictionary) => {
                let stored = st.write(|st, connection| {
                    let mut existing = 0;
                    st.query(connection, "SELECT COUNT(*) FROM codec_dicts", (), |row| {
                        existing = integer(row, 0)?;
                        Ok(())
                    })?;
                    if existing > 0 {
                        return Ok(false);
                    }
                    let mut codec = Codec::default();
                    codec.add(1, &dictionary)?;
                    st.execute(
                        connection,
                        "INSERT INTO codec_dicts (id, dict) VALUES (1, ?1)",
                        [dictionary.as_slice()],
                    )?;
                    Ok(true)
                })?;
                st.set_codec(load_codec(st)?);
                Ok(if stored {
                    StepOutcome::Trained
                } else {
                    StepOutcome::AlreadyTrained
                })
            }
            Step::Chunk(chunk) => st.write(|st, connection| commit_chunk(st, connection, chunk)),
            Step::Sweep { candidates, cutoff } => {
                let (files, bytes) = st.write(|st, connection| {
                    let referenced = referenced(st, connection)?;
                    st.blobs.sweep(&candidates, &referenced, cutoff)
                })?;
                Ok(StepOutcome::Swept { files, bytes })
            }
        })
    }

    /// Delete the source once every thread has moved out of it and every
    /// thread it holds is known. Returns whether it was retired.
    pub fn retire_source(&self) -> io::Result<bool> {
        self.run("retire the source", |st| {
            let Some(source) = st.source() else {
                return Ok(false);
            };
            if !source.is_enumerated() {
                return Ok(false);
            }
            let left = st.read(|st, connection| {
                rows::thread_ids(
                    st,
                    connection,
                    "SELECT id FROM threads WHERE old_id IS NOT NULL AND moved < old_end",
                )
            })?;
            if !left.is_empty() {
                return Ok(false);
            }
            drop(source);
            if let Some(source) = st.retire_source() {
                source.retire();
            }
            Ok(true)
        })
    }
}

impl MovePlan {
    pub fn id(&self) -> &str {
        &self.id
    }
}

fn commit_chunk(st: &Storage, connection: &Connection, chunk: Chunk) -> io::Result<StepOutcome> {
    let Some(thread) = rows::thread(st, connection, &chunk.id)? else {
        return Ok(StepOutcome::Stale);
    };
    if thread.key != chunk.key
        || thread.old_id.as_deref() != Some(chunk.old_id.as_str())
        || thread.old_end != chunk.old_end
        || thread.moved != chunk.start
    {
        return Ok(StepOutcome::Stale);
    }
    let mut after = 0;
    for (position, row) in &chunk.rows {
        rows::insert(st, connection, thread.row(*position)?, row, false)?;
        after += row.bytes.len() as u64;
    }
    st.execute(
        connection,
        "UPDATE threads SET moved = ?2 WHERE key = ?1",
        (thread.key, chunk.end as i64),
    )?;
    let done = chunk.end >= chunk.old_end;
    if done && chunk.compacted {
        let (lo, hi) = thread.keys(chunk.old_end..u64::MAX);
        let mut appended = false;
        st.query(
            connection,
            "SELECT 1 FROM events WHERE key >= ?1 AND key < ?2 LIMIT 1",
            (lo, hi),
            |_| {
                appended = true;
                Ok(())
            },
        )?;
        if !appended {
            st.execute(
                connection,
                "UPDATE threads SET compacted = 1 WHERE key = ?1",
                [thread.key],
            )?;
        }
    }
    Ok(StepOutcome::Moved {
        rows: chunk.rows.len(),
        before: chunk.read_bytes,
        after,
        done,
    })
}

fn referenced(st: &Storage, connection: &Connection) -> io::Result<HashSet<Hash>> {
    let mut referenced = HashSet::new();
    st.query(connection, "SELECT hashes FROM event_refs", (), |row| {
        for hash in blob(row, 0)?.as_chunks::<32>().0 {
            referenced.insert(*hash);
        }
        Ok(())
    })?;
    Ok(referenced)
}

fn sample(st: &Storage, connection: &Connection) -> io::Result<Vec<Vec<u8>>> {
    let codec = st.codec();
    let mut decoder = codec.decoder();
    let mut rows = Vec::new();
    st.query(
        connection,
        "SELECT line FROM events ORDER BY random() LIMIT ?1",
        [SAMPLES as i64],
        |row| {
            rows.push(decoder.bare(&blob(row, 0)?)?);
            Ok(())
        },
    )?;
    Ok(rows)
}

/// Bytes free to this user on the disk holding `path`.
pub(super) fn free_space(path: &Path) -> io::Result<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::other("a path with a NUL byte"))?;
        let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: `path` is NUL-terminated and `stats` is written in full on
        // success.
        if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: statvfs returned 0.
        let stats = unsafe { stats.assume_init() };
        #[allow(clippy::unnecessary_cast)]
        Ok(stats.f_bavail as u64 * stats.f_frsize as u64)
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        let mut free = 0u64;
        // SAFETY: `wide` is NUL-terminated and `free` outlives the call.
        unsafe {
            windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
                windows::core::PCWSTR(wide.as_ptr()),
                Some(&mut free),
                None,
                None,
            )
        }
        .map_err(io::Error::other)?;
        Ok(free)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(u64::MAX)
    }
}
