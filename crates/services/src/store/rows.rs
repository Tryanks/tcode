//! Where a thread's rows are and how they are read and written.
//!
//! Every row lives in `events` under the key `thread << 32 | position`, except
//! while a store moves out of an older build's file: then a thread may still
//! have rows in that file, the source, at positions `moved..old_end` of the
//! thread the source knows as `old_id`. The source is never written, so a read
//! that takes `moved` and `old_end` from this file's snapshot and the rest from
//! the source sees one consistent log. Writes only ever go to this file; a row
//! here at a position the source also has is the newer one.

use std::collections::BTreeMap;
use std::io;
use std::ops::{Deref, Range};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use turso::Connection;

use super::blobs::Blobs;
use super::codec::{Codec, Decoded, Decoder, Encoded, Encoder, Mode};
use super::db::{Db, blob, integer, text};
use super::source::Source;

/// Positions a thread's key leaves room for.
const POSITIONS: u64 = 1 << 32;

/// The database file with everything its rows need: the codec, the blobs
/// they reference, and the older file still being moved out of, if any.
pub(super) struct Storage {
    db: Db,
    pub(super) root: PathBuf,
    codec: RwLock<Arc<Codec>>,
    pub(super) blobs: Blobs,
    source: RwLock<Option<Arc<Source>>>,
}

impl Deref for Storage {
    type Target = Db;

    fn deref(&self) -> &Db {
        &self.db
    }
}

impl Storage {
    pub(super) fn new(db: Db, root: &Path, codec: Codec, source: Option<Source>) -> Self {
        Self {
            db,
            root: root.to_path_buf(),
            codec: RwLock::new(Arc::new(codec)),
            blobs: Blobs::new(root),
            source: RwLock::new(source.map(Arc::new)),
        }
    }

    pub(super) fn into_db(self) -> Db {
        self.db
    }

    pub(super) fn write<R>(
        &self,
        body: impl FnOnce(&Self, &Connection) -> io::Result<R>,
    ) -> io::Result<R> {
        self.db.write(|_, connection| body(self, connection))
    }

    pub(super) fn read<R>(
        &self,
        body: impl FnOnce(&Self, &Connection) -> io::Result<R>,
    ) -> io::Result<R> {
        self.db.read(|_, connection| body(self, connection))
    }

    /// Fail every read in flight on this file and the source, for closing.
    pub(super) fn cancel_reads(&self) {
        self.db.cancel_reads();
        if let Some(source) = self.source() {
            source.cancel_reads();
        }
    }

    pub(super) fn codec(&self) -> Arc<Codec> {
        self.codec.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub(super) fn set_codec(&self, codec: Codec) {
        *self.codec.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(codec);
    }

    pub(super) fn source(&self) -> Option<Arc<Source>> {
        self.source
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Stop reading the source; it is deleted once the last read holding it
    /// ends.
    pub(super) fn retire_source(&self) -> Option<Arc<Source>> {
        self.source
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }

    /// Decode a row to its canonical (or bare) bytes.
    pub(super) fn decode(
        &self,
        decoder: &mut Decoder<'_>,
        row: &Stored,
        mode: Mode,
    ) -> io::Result<Decoded> {
        match row {
            Stored::Old(bytes) => Ok(Decoded {
                bytes: match mode {
                    Mode::Full => bytes.clone(),
                    Mode::Bare => super::codec::split(bytes).bare,
                },
                missing: false,
            }),
            Stored::New(bytes) => decoder.decode(bytes, mode, &mut |hash| self.blobs.read(hash)),
        }
    }

    /// Encode `canonical` and put the blobs it names on disk.
    pub(super) fn encode(
        &self,
        encoder: &mut Encoder<'_>,
        canonical: &[u8],
    ) -> io::Result<Encoded> {
        let encoded = encoder.encode(canonical)?;
        self.blobs.put(&encoded.blobs)?;
        Ok(encoded)
    }
}

/// A stored row: format 2 in this file, or an older build's canonical bytes
/// still in the source.
#[derive(Debug, Clone)]
pub(super) enum Stored {
    New(Vec<u8>),
    Old(Vec<u8>),
}

/// One thread's place in the store.
#[derive(Clone)]
pub(super) struct Thread {
    pub key: i64,
    pub id: String,
    /// The source's name for this thread's rows not yet moved, if any.
    pub old_id: Option<String>,
    pub moved: u64,
    pub old_end: u64,
    /// Taken before the row above was read: a source retired after that
    /// still serves this read, and one retired before it had every row
    /// moved by then.
    source: Option<Arc<Source>>,
}

impl Thread {
    pub(super) fn row(&self, position: u64) -> io::Result<i64> {
        if position >= POSITIONS {
            return Err(io::Error::other(format!(
                "thread {} has no room for row {position}",
                self.id
            )));
        }
        Ok(self.base() + position as i64)
    }

    pub(super) fn base(&self) -> i64 {
        self.key << 32
    }

    /// The keys of rows `positions` of this thread.
    pub(super) fn keys(&self, positions: Range<u64>) -> (i64, i64) {
        let end = positions.end.min(POSITIONS);
        let start = positions.start.min(end);
        (self.base() + start as i64, self.base() + end as i64)
    }

    /// The positions still read from the source.
    pub(super) fn unmoved(&self) -> Range<u64> {
        match self.old_id {
            Some(_) => self.moved..self.old_end.max(self.moved),
            None => 0..0,
        }
    }
}

pub(super) fn thread(
    st: &Storage,
    connection: &Connection,
    id: &str,
) -> io::Result<Option<Thread>> {
    let source = st.source();
    let mut found = None;
    st.query(
        connection,
        "SELECT key, old_id, moved, old_end FROM threads WHERE id = ?1",
        [id],
        |row| {
            found = Some(Thread {
                key: integer(row, 0)?,
                id: id.to_owned(),
                old_id: match row.get_value(1) {
                    Ok(turso::Value::Text(old)) => Some(old),
                    _ => None,
                },
                moved: integer(row, 2)? as u64,
                old_end: integer(row, 3)? as u64,
                source: source.clone(),
            });
            Ok(())
        },
    )?;
    Ok(found)
}

/// The thread named `id`, registered if it is new. A thread the source has
/// rows for is registered with them still to move.
pub(super) fn ensure_thread(st: &Storage, connection: &Connection, id: &str) -> io::Result<Thread> {
    if let Some(thread) = thread(st, connection, id)? {
        return Ok(thread);
    }
    let old_end = match st.source() {
        Some(source) => source.end(id)?,
        None => 0,
    };
    register(st, connection, id, old_end)?;
    thread(st, connection, id)?.ok_or_else(|| io::Error::other("a registered thread is missing"))
}

/// Register `id`, whose first `old_end` rows are still in the source.
pub(super) fn register(
    st: &Storage,
    connection: &Connection,
    id: &str,
    old_end: u64,
) -> io::Result<()> {
    st.execute(
        connection,
        "INSERT INTO threads (key, id, revision, compacted, old_id, old_end, moved) \
         VALUES ((SELECT COALESCE(MAX(key), 0) + 1 FROM threads), ?1, randomblob(16), ?2, ?3, ?4, 0) \
         ON CONFLICT (id) DO NOTHING",
        (
            id,
            i64::from(old_end == 0),
            (old_end > 0).then_some(id),
            old_end as i64,
        ),
    )?;
    Ok(())
}

/// The position after the thread's last row.
pub(super) fn end(st: &Storage, connection: &Connection, thread: &Thread) -> io::Result<u64> {
    let (lo, hi) = thread.keys(0..POSITIONS);
    let mut last = None;
    st.query(
        connection,
        "SELECT MAX(key) FROM events WHERE key >= ?1 AND key < ?2",
        (lo, hi),
        |row| {
            if let Ok(turso::Value::Integer(key)) = row.get_value(0) {
                last = Some((key - thread.base()) as u64);
            }
            Ok(())
        },
    )?;
    let end = last.map_or(0, |last| last + 1);
    Ok(match thread.old_id {
        Some(_) => end.max(thread.old_end),
        None => end,
    })
}

/// Visit the thread's rows in `positions`, in order or backwards, while
/// `each` asks for more.
pub(super) fn visit(
    st: &Storage,
    connection: &Connection,
    thread: &Thread,
    positions: Range<u64>,
    backwards: bool,
    mut each: impl FnMut(u64, Stored) -> io::Result<bool>,
) -> io::Result<()> {
    let unmoved = thread.unmoved();
    let mixed = positions.start.max(unmoved.start)..positions.end.min(unmoved.end);
    let source = thread.source.as_ref().filter(|_| !mixed.is_empty());
    let (Some(source), Some(old_id)) = (source, thread.old_id.as_deref()) else {
        return visit_new(st, connection, thread, positions, backwards, &mut each).map(drop);
    };
    let mut segments = [
        (positions.start..mixed.start, false),
        (mixed.clone(), true),
        (mixed.end..positions.end, false),
    ];
    if backwards {
        segments.reverse();
    }
    for (range, mixed) in segments {
        if range.is_empty() {
            continue;
        }
        if !mixed {
            if !visit_new(st, connection, thread, range, backwards, &mut each)? {
                return Ok(());
            }
            continue;
        }
        let mut newer = BTreeMap::new();
        visit_new(
            st,
            connection,
            thread,
            range.clone(),
            false,
            &mut |position, row| {
                newer.insert(position, row);
                Ok(true)
            },
        )?;
        let mut more = true;
        source.visit(old_id, range, backwards, &mut |position, bytes| {
            loop {
                let next = if backwards {
                    newer
                        .last_key_value()
                        .map(|(p, _)| *p)
                        .filter(|p| *p > position)
                } else {
                    newer
                        .first_key_value()
                        .map(|(p, _)| *p)
                        .filter(|p| *p < position)
                };
                let Some(next) = next else { break };
                let row = newer.remove(&next).expect("present");
                if !each(next, row)? {
                    more = false;
                    return Ok(false);
                }
            }
            more = match newer.remove(&position) {
                Some(row) => each(position, row)?,
                None => each(position, Stored::Old(bytes))?,
            };
            Ok(more)
        })?;
        if !more {
            return Ok(());
        }
        let rest: Vec<_> = if backwards {
            std::mem::take(&mut newer).into_iter().rev().collect()
        } else {
            std::mem::take(&mut newer).into_iter().collect()
        };
        for (position, row) in rest {
            if !each(position, row)? {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Returns whether `each` asked for more.
fn visit_new(
    st: &Storage,
    connection: &Connection,
    thread: &Thread,
    positions: Range<u64>,
    backwards: bool,
    each: &mut dyn FnMut(u64, Stored) -> io::Result<bool>,
) -> io::Result<bool> {
    let (lo, hi) = thread.keys(positions);
    let mut more = true;
    st.query_while(
        connection,
        if backwards {
            "SELECT key, line FROM events WHERE key >= ?1 AND key < ?2 ORDER BY key DESC"
        } else {
            "SELECT key, line FROM events WHERE key >= ?1 AND key < ?2 ORDER BY key"
        },
        (lo, hi),
        |row| {
            let position = (integer(row, 0)? - thread.base()) as u64;
            more = each(position, Stored::New(blob(row, 1)?))?;
            Ok(more)
        },
    )?;
    Ok(more)
}

/// The row at `position`, wherever it is.
pub(super) fn get(
    st: &Storage,
    connection: &Connection,
    thread: &Thread,
    position: u64,
) -> io::Result<Option<Stored>> {
    let mut found = None;
    visit(
        st,
        connection,
        thread,
        position..position + 1,
        false,
        |_, row| {
            found = Some(row);
            Ok(false)
        },
    )?;
    Ok(found)
}

/// The first row at or after `from` whose event type is `kind`, wherever it
/// is: `tag` is how an older build's row spells that type.
pub(super) fn find(
    st: &Storage,
    connection: &Connection,
    thread: &Thread,
    from: u64,
    kind: u8,
    tag: &[u8],
) -> io::Result<Option<(u64, Stored)>> {
    let (lo, hi) = thread.keys(from..POSITIONS);
    let mut found = None;
    st.query(
        connection,
        "SELECT key, line FROM events WHERE key >= ?1 AND key < ?2 \
         AND substr(line, 3, 1) = ?3 ORDER BY key LIMIT 1",
        (lo, hi, vec![kind]),
        |row| {
            found = Some((
                (integer(row, 0)? - thread.base()) as u64,
                Stored::New(blob(row, 1)?),
            ));
            Ok(())
        },
    )?;
    let unmoved = thread.unmoved();
    if let (Some(source), Some(old_id)) = (&thread.source, thread.old_id.as_deref())
        && !unmoved.is_empty()
        && from < unmoved.end
        && let Some((position, bytes)) = source.find(
            old_id,
            from.max(unmoved.start)
                ..found
                    .as_ref()
                    .map_or(unmoved.end, |(p, _)| *p)
                    .min(unmoved.end),
            tag,
        )?
    {
        // A newer row at that position replaced the source's.
        return Ok(Some(match get(st, connection, thread, position)? {
            Some(row) => (position, row),
            None => (position, Stored::Old(bytes)),
        }));
    }
    Ok(found)
}

/// Store `encoded` at the key, replacing a row already there when `replace`
/// is set; returns whether it was written.
pub(super) fn insert(
    st: &Storage,
    connection: &Connection,
    key: i64,
    encoded: &Encoded,
    replace: bool,
) -> io::Result<bool> {
    let written = st.execute(
        connection,
        if replace {
            "INSERT INTO events (key, line) VALUES (?1, ?2) \
             ON CONFLICT (key) DO UPDATE SET line = excluded.line"
        } else {
            "INSERT INTO events (key, line) VALUES (?1, ?2) ON CONFLICT (key) DO NOTHING"
        },
        (key, encoded.bytes.as_slice()),
    )? > 0;
    if written {
        if encoded.blobs.is_empty() {
            if replace {
                st.execute(connection, "DELETE FROM event_refs WHERE key = ?1", [key])?;
            }
        } else {
            st.execute(
                connection,
                "INSERT INTO event_refs (key, hashes) VALUES (?1, ?2) \
                 ON CONFLICT (key) DO UPDATE SET hashes = excluded.hashes",
                (key, encoded.hashes()),
            )?;
        }
    }
    Ok(written)
}

/// Encode `canonical` and store it at `position`, replacing what is there.
pub(super) fn put(
    st: &Storage,
    connection: &Connection,
    encoder: &mut Encoder<'_>,
    thread: &Thread,
    position: u64,
    canonical: &[u8],
) -> io::Result<()> {
    let encoded = st.encode(encoder, canonical)?;
    insert(st, connection, thread.row(position)?, &encoded, true).map(drop)
}

/// Replace the row at `key` with `encoded` if it still holds `line`;
/// returns whether it did.
pub(super) fn replace_if(
    st: &Storage,
    connection: &Connection,
    key: i64,
    line: &[u8],
    encoded: &Encoded,
) -> io::Result<bool> {
    let replaced = st.execute(
        connection,
        "UPDATE events SET line = ?3 WHERE key = ?1 AND line = ?2",
        (key, line, encoded.bytes.as_slice()),
    )? > 0;
    if replaced {
        if encoded.blobs.is_empty() {
            st.execute(connection, "DELETE FROM event_refs WHERE key = ?1", [key])?;
        } else {
            st.execute(
                connection,
                "INSERT INTO event_refs (key, hashes) VALUES (?1, ?2) \
                 ON CONFLICT (key) DO UPDATE SET hashes = excluded.hashes",
                (key, encoded.hashes()),
            )?;
        }
    }
    Ok(replaced)
}

/// Remove the row at `key` if it still holds `line`; returns whether it did.
pub(super) fn delete_if(
    st: &Storage,
    connection: &Connection,
    key: i64,
    line: &[u8],
) -> io::Result<bool> {
    let deleted = st.execute(
        connection,
        "DELETE FROM events WHERE key = ?1 AND line = ?2",
        (key, line),
    )? > 0;
    if deleted {
        st.execute(connection, "DELETE FROM event_refs WHERE key = ?1", [key])?;
    }
    Ok(deleted)
}

/// Remove every row of the thread from this file and forget its unmoved
/// rows in the source.
pub(super) fn clear(st: &Storage, connection: &Connection, thread: &Thread) -> io::Result<()> {
    let (lo, hi) = thread.keys(0..POSITIONS);
    st.execute(
        connection,
        "DELETE FROM events WHERE key >= ?1 AND key < ?2",
        (lo, hi),
    )?;
    st.execute(
        connection,
        "DELETE FROM event_refs WHERE key >= ?1 AND key < ?2",
        (lo, hi),
    )?;
    st.execute(
        connection,
        "UPDATE threads SET old_id = NULL, old_end = 0, moved = 0, \
         revision = randomblob(16) WHERE key = ?1",
        [thread.key],
    )?;
    Ok(())
}

/// Give `dst` (cleared) a copy of `src`'s rows before position `end`, or all
/// of them: those in this file, and the same unmoved rows of the source.
pub(super) fn copy(
    st: &Storage,
    connection: &Connection,
    src: &Thread,
    dst: &Thread,
    end: Option<u64>,
) -> io::Result<()> {
    let (lo, hi) = src.keys(0..end.unwrap_or(POSITIONS));
    let shift = dst.base() - src.base();
    st.execute(
        connection,
        "INSERT INTO events (key, line) SELECT key + ?3, line FROM events \
         WHERE key >= ?1 AND key < ?2",
        (lo, hi, shift),
    )?;
    st.execute(
        connection,
        "INSERT INTO event_refs (key, hashes) SELECT key + ?3, hashes FROM event_refs \
         WHERE key >= ?1 AND key < ?2",
        (lo, hi, shift),
    )?;
    st.execute(
        connection,
        "UPDATE threads SET old_id = ?2, old_end = ?3, moved = ?4 WHERE key = ?1",
        (
            dst.key,
            src.old_id.as_deref(),
            end.map_or(src.old_end, |end| src.old_end.min(end)) as i64,
            src.moved as i64,
        ),
    )?;
    Ok(())
}

pub(super) fn revision(st: &Storage, connection: &Connection, id: &str) -> io::Result<Vec<u8>> {
    let mut revision = Vec::new();
    st.query(
        connection,
        "SELECT revision FROM threads WHERE id = ?1",
        [id],
        |row| {
            revision = blob(row, 0)?;
            Ok(())
        },
    )?;
    Ok(revision)
}

/// Leave the thread for the compaction pass.
pub(super) fn needs_compaction(st: &Storage, connection: &Connection, key: i64) -> io::Result<()> {
    st.execute(
        connection,
        "UPDATE threads SET compacted = 0 WHERE key = ?1",
        [key],
    )
    .map(drop)
}

pub(super) fn thread_ids(
    st: &Storage,
    connection: &Connection,
    sql: &str,
) -> io::Result<Vec<String>> {
    let mut ids = Vec::new();
    st.query(connection, sql, (), |row| {
        ids.push(text(row, 0)?);
        Ok(())
    })?;
    Ok(ids)
}
