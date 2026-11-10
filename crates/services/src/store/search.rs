//! The text global search reads: each thread's searchable entries, extracted
//! from its folded log and kept with the identity of the log they came from,
//! so a search reads text instead of reading and folding every log.

use std::io;

use turso::Connection;

use super::SessionStore;
use super::db::{Db, blob, integer, text};
use super::rows::{self, Storage, Thread};

pub(super) fn create_tables(db: &Db, connection: &Connection) -> io::Result<()> {
    db.execute(
        connection,
        "CREATE TABLE IF NOT EXISTS search_sources (\
         thread INTEGER PRIMARY KEY, version INTEGER NOT NULL, \
         revision BLOB NOT NULL, end_row INTEGER NOT NULL)",
        (),
    )?;
    db.execute(
        connection,
        "CREATE TABLE IF NOT EXISTS search_entries (\
         thread INTEGER NOT NULL, ordinal INTEGER NOT NULL, entry_id TEXT NOT NULL, \
         position INTEGER NOT NULL, turn INTEGER NOT NULL, kind TEXT NOT NULL, \
         text TEXT NOT NULL, folded TEXT, \
         PRIMARY KEY (thread, ordinal))",
        (),
    )?;
    Ok(())
}

pub(super) fn forget(st: &Storage, connection: &Connection, thread: &Thread) -> io::Result<()> {
    st.execute(
        connection,
        "DELETE FROM search_entries WHERE thread = ?1",
        [thread.key],
    )?;
    st.execute(
        connection,
        "DELETE FROM search_sources WHERE thread = ?1",
        [thread.key],
    )?;
    Ok(())
}

/// The identity of a thread's log: a replacement changes its revision, an
/// append its end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchSource {
    revision: Vec<u8>,
    /// The row after the last one the entries were extracted from.
    pub end: u64,
}

/// One searchable entry as it is kept: `text` is what a hit's snippet is cut
/// from, with its whitespace collapsed as a snippet collapses it, and
/// `folded` that text lowercased as a query is; `position` is the log row
/// that first recorded the entry.
pub struct IndexedEntry<'a> {
    pub entry_id: &'a str,
    pub position: u64,
    pub turn: usize,
    pub kind: &'a str,
    pub text: &'a str,
    pub folded: &'a str,
}

/// One stored entry a query matched.
pub struct SearchMatch<'a> {
    pub entry_id: &'a str,
    pub position: u64,
    pub turn: usize,
    pub kind: &'a str,
    pub text: &'a str,
}

fn current_source(st: &Storage, connection: &Connection, id: &str) -> io::Result<SearchSource> {
    Ok(match rows::thread(st, connection, id)? {
        Some(thread) => SearchSource {
            revision: rows::revision(st, connection, id)?,
            end: rows::end(st, connection, &thread)?,
        },
        None => SearchSource {
            revision: Vec::new(),
            end: 0,
        },
    })
}

impl SessionStore {
    /// The identity of `id`'s log as it is now.
    pub fn search_source(&self, id: &str) -> io::Result<SearchSource> {
        self.run("read a search source", |st| {
            st.read(|st, connection| current_source(st, connection, id))
        })
    }

    /// Whether `id`'s searchable entries were extracted under `version` from
    /// its log as it is now.
    pub fn search_entries_current(&self, id: &str, version: u32) -> io::Result<bool> {
        self.run("check search entries", |st| {
            st.read(|st, connection| {
                let mut indexed = None;
                st.query(
                    connection,
                    "SELECT s.revision, s.end_row FROM search_sources s \
                     JOIN threads t ON t.key = s.thread WHERE t.id = ?1 AND s.version = ?2",
                    (id, i64::from(version)),
                    |row| {
                        indexed = Some(SearchSource {
                            revision: blob(row, 0)?,
                            end: integer(row, 1)? as u64,
                        });
                        Ok(())
                    },
                )?;
                Ok(indexed.is_some_and(|indexed| {
                    current_source(st, connection, id).is_ok_and(|current| current == indexed)
                }))
            })
        })
    }

    /// Replace `id`'s searchable entries with those extracted under `version`
    /// from the log `source` names.
    pub fn replace_search_entries<'a>(
        &self,
        id: &str,
        version: u32,
        source: &SearchSource,
        entries: impl IntoIterator<Item = IndexedEntry<'a>>,
    ) -> io::Result<()> {
        self.run("write search entries", |st| {
            st.write(|st, connection| {
                let thread = rows::ensure_thread(st, connection, id)?;
                forget(st, connection, &thread)?;
                for (ordinal, entry) in entries.into_iter().enumerate() {
                    // `lower()` folds ASCII alone, so only text it would fold
                    // differently from a query keeps its folded form.
                    let folded =
                        (entry.folded != entry.text.to_ascii_lowercase()).then_some(entry.folded);
                    st.execute(
                        connection,
                        "INSERT INTO search_entries \
                         (thread, ordinal, entry_id, position, turn, kind, text, folded) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        (
                            thread.key,
                            ordinal as i64,
                            entry.entry_id,
                            entry.position as i64,
                            entry.turn as i64,
                            entry.kind,
                            entry.text,
                            folded,
                        ),
                    )?;
                }
                st.execute(
                    connection,
                    "INSERT INTO search_sources (thread, version, revision, end_row) \
                     VALUES (?1, ?2, ?3, ?4)",
                    (
                        thread.key,
                        i64::from(version),
                        source.revision.clone(),
                        source.end as i64,
                    ),
                )?;
                Ok(())
            })
        })
    }

    /// Visit `id`'s searchable entries whose folded text contains `folded`, in
    /// order, while `each` asks for more.
    pub fn visit_search_matches(
        &self,
        id: &str,
        folded: &str,
        mut each: impl FnMut(SearchMatch<'_>) -> bool,
    ) -> io::Result<()> {
        self.run("search entries", |st| {
            st.read(|st, connection| {
                st.query_while(
                    connection,
                    "SELECT e.entry_id, e.position, e.turn, e.kind, e.text FROM search_entries e \
                     JOIN threads t ON t.key = e.thread WHERE t.id = ?1 \
                     AND instr(COALESCE(e.folded, lower(e.text)), ?2) > 0 ORDER BY e.ordinal",
                    (id, folded),
                    |row| {
                        Ok(each(SearchMatch {
                            entry_id: &text(row, 0)?,
                            position: integer(row, 1)? as u64,
                            turn: integer(row, 2)? as usize,
                            kind: &text(row, 3)?,
                            text: &text(row, 4)?,
                        }))
                    },
                )
            })
        })
    }
}
