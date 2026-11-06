use std::{
    borrow::Cow,
    io,
    ops::Range,
    sync::{Arc, Mutex},
};

use serde::Deserialize;
use tcode_core::session::StoredEvent;
use turso::Connection;

use super::codec::Mode;
use super::rows::{self, Stored, Thread};
use super::{EventLog, SessionStore, TurnIndex, blob, decode_row, invalid_data};

#[derive(Default, Deserialize)]
struct EventAddress {
    event: Option<Box<EventAddress>>,
    #[serde(rename = "type", default)]
    pub event_type: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub item_id: Option<String>,
}

/// A writer-ordered cut. The end, turn index and payload reads share a database
/// snapshot; appends, diff stripping and replacement cannot move that cut.
pub struct LogRead {
    store: SessionStore,
    connection: Arc<Mutex<Option<Connection>>>,
    thread: Option<Thread>,
    previews: std::sync::Mutex<Vec<super::projection::Preview>>,
    pub end: u64,
    pub index: Option<TurnIndex>,
}

impl SessionStore {
    pub fn snapshot_log(&self, id: &str) -> io::Result<LogRead> {
        self.run("pin event log", |st| {
            let connection = st.connect_reader()?;
            st.execute(&connection, "BEGIN", ())?;
            let cut = (|| {
                let Some(thread) = rows::thread(st, &connection, id)? else {
                    return Ok::<_, io::Error>((None, 0, None));
                };
                let end = rows::end(st, &connection, &thread)?;
                let index = super::turn_index::read(st, &connection, &thread)?;
                Ok((Some(thread), end, index))
            })();
            let (thread, end, index) = match cut {
                Ok(cut) => cut,
                Err(error) => {
                    let _ = st.execute(&connection, "ROLLBACK", ());
                    return Err(error);
                }
            };
            Ok(LogRead {
                store: self.clone(),
                connection: st.pin_reader(connection),
                thread,
                previews: std::sync::Mutex::new(Vec::new()),
                end,
                index,
            })
        })
    }
}

impl Drop for LogRead {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.lock().unwrap().take() {
            let _ = futures_lite::future::block_on(connection.execute("ROLLBACK", ()));
        }
    }
}

impl LogRead {
    pub fn read_item(&self, item_id: &str) -> io::Result<Option<EventLog>> {
        let quoted_id = serde_json::to_vec(item_id).map_err(invalid_data)?;
        let id = memchr::memmem::Finder::new(&quoted_id);
        let rewind = memchr::memmem::Finder::new(br#""rewind_completed""#);
        self.store.run("locate item", |st| {
            let guard = self.connection.lock().unwrap();
            let connection = guard
                .as_ref()
                .ok_or_else(|| io::Error::other("event read cancelled"))?;
            let (Some(index), Some(thread)) = (&self.index, &self.thread) else {
                return Ok(None);
            };
            let codec = st.codec();
            let mut decoder = codec.decoder();
            let mut turn = None;
            let mut last_rewind = None;
            let mut ambiguous = false;
            let mut log = EventLog {
                next_row: self.end,
                ..EventLog::default()
            };
            rows::visit(
                st,
                connection,
                thread,
                0..self.end,
                false,
                |position, row| {
                    let bytes = st.decode(&mut decoder, &row, Mode::Bare)?.bytes;
                    // Escaped JSON addresses can spell the same id differently.
                    if id.find(&bytes).is_none()
                        && rewind.find(&bytes).is_none()
                        && memchr::memchr(b'\\', &bytes).is_none()
                    {
                        return Ok(true);
                    }
                    let Ok(envelope) = serde_json::from_slice::<EventAddress>(&bytes) else {
                        return Ok(true);
                    };
                    let address = envelope.event.as_deref().unwrap_or(&envelope);
                    if address.event_type == "rewind_completed" {
                        last_rewind = Some(position);
                    }
                    if address.id.as_deref() == Some(item_id)
                        || address.item_id.as_deref() == Some(item_id)
                    {
                        let physical_turn =
                            index.starts.partition_point(|start| *start <= position);
                        if turn.is_some_and(|turn| turn != physical_turn) {
                            ambiguous = true;
                        }
                        turn = Some(physical_turn);
                        let line = st.decode(&mut decoder, &row, Mode::Full)?.bytes;
                        if let Ok(Some(record)) = decode_row(&line) {
                            log.rows.push(position);
                            log.records.push(record.into_stored(None));
                        }
                    }
                    Ok(true)
                },
            )?;
            let start = turn
                .and_then(|turn| turn.checked_sub(1))
                .map_or(0, |turn| index.starts[turn]);
            // A rewind changes item visibility without mentioning its id.
            if ambiguous || last_rewind.is_some_and(|rewind| rewind >= start) {
                return Ok(None);
            }
            Ok(Some(log))
        })
    }

    /// Stops at the consumer's byte budget. Cached projections and canonical
    /// fallbacks are read in the same transaction, in either cursor direction.
    pub fn visit_projected_rows(
        &self,
        positions: Range<u64>,
        backwards: bool,
        version: &str,
        cache_bytes: usize,
        project: impl FnMut(&StoredEvent) -> Cow<'_, StoredEvent>,
        mut visit: impl FnMut(u64, StoredEvent) -> bool,
    ) -> io::Result<()> {
        self.store.run("read event previews", |st| {
            let guard = self.connection.lock().unwrap();
            let connection = guard
                .as_ref()
                .ok_or_else(|| io::Error::other("event read cancelled"))?;
            let Some(thread) = &self.thread else {
                return Ok(());
            };
            let codec = st.codec();
            let mut decoder = codec.decoder();
            let mut project = project;
            let mut previews = Vec::new();
            let range = positions.start.min(self.end)..positions.end.min(self.end);
            rows::visit(st, connection, thread, range, backwards, |position, row| {
                let key = thread.row(position)?;
                if let Stored::New(_) = row {
                    let mut cached = None;
                    st.query(
                        connection,
                        "SELECT body FROM event_preview WHERE key = ?1 AND version = ?2",
                        (key, version),
                        |preview| {
                            cached = Some(blob(preview, 0)?);
                            Ok(())
                        },
                    )?;
                    if let Some(body) = cached {
                        let record = serde_json::from_slice(&body).map_err(invalid_data)?;
                        return Ok(visit(position, record));
                    }
                }
                let line = st.decode(&mut decoder, &row, Mode::Full)?.bytes;
                let record = match decode_row(&line) {
                    Ok(Some(record)) => record,
                    _ => return Ok(true),
                };
                if record.legacy_file_diff {
                    return Err(io::Error::other("legacy diff needs full hydration"));
                }
                let record = record.into_stored(None);
                let record = match project(&record) {
                    Cow::Borrowed(_) => record,
                    Cow::Owned(preview) => {
                        if let Stored::New(stored) = row
                            && line.len() >= cache_bytes
                            && let Ok(body) = serde_json::to_vec(&preview)
                            && body.len() < cache_bytes
                        {
                            previews.push(super::projection::Preview {
                                key,
                                canonical: stored,
                                version: version.to_owned(),
                                body,
                            });
                        }
                        preview
                    }
                };
                Ok(visit(position, record))
            })?;
            self.previews.lock().unwrap().extend(previews);
            Ok(())
        })
    }

    pub fn take_publication(&self) -> super::ProjectionPublication {
        super::ProjectionPublication {
            store: self.store.clone(),
            rows: self
                .previews
                .lock()
                .map(|mut rows| std::mem::take(&mut *rows))
                .unwrap_or_default(),
        }
    }
}
