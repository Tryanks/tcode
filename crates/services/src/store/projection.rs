use std::{borrow::Cow, io, ops::Range};

use tcode_core::session::StoredEvent;
use turso::Connection;

use super::db::{Db, blob, integer};
use super::{SessionStore, decode_row, invalid_data, stored_provider_at};

pub(super) fn create_cache(db: &Db, connection: &Connection) -> io::Result<()> {
    db.execute(
        connection,
        "CREATE TABLE IF NOT EXISTS event_preview_v1 (\
         session_id TEXT NOT NULL, position INTEGER NOT NULL, version TEXT NOT NULL, \
         token TEXT NOT NULL, body BLOB, PRIMARY KEY (session_id, position))",
        (),
    )?;
    for (name, event, affected) in [
        (
            "insert",
            "INSERT",
            "session_id = NEW.session_id AND position = NEW.position",
        ),
        (
            "update",
            "UPDATE",
            "(session_id = OLD.session_id AND position = OLD.position) OR (session_id = NEW.session_id AND position = NEW.position)",
        ),
        (
            "delete",
            "DELETE",
            "session_id = OLD.session_id AND position = OLD.position",
        ),
    ] {
        db.execute(
            connection,
            &format!(
                "CREATE TRIGGER IF NOT EXISTS event_preview_v1_{name} AFTER {event} ON events \
             BEGIN DELETE FROM event_preview_v1 WHERE {affected}; END"
            ),
            (),
        )?;
    }
    db.execute(
        connection,
        "CREATE TRIGGER IF NOT EXISTS event_preview_v1_remove AFTER DELETE ON sessions \
         BEGIN DELETE FROM event_preview_v1 WHERE session_id = OLD.id; END",
        (),
    )?;
    Ok(())
}

impl SessionStore {
    /// A borrowed projection must be the original record. Returns uncached
    /// large-row positions eligible for read_log_with_projections.
    pub fn visit_projected_rows_backwards(
        &self,
        id: &str,
        positions: Range<u64>,
        version: &str,
        min_bytes: usize,
        mut project: impl FnMut(&StoredEvent) -> Cow<'_, StoredEvent>,
        mut visit: impl FnMut(u64, StoredEvent) -> bool,
    ) -> io::Result<Vec<u64>> {
        let bound = |position| i64::try_from(position).unwrap_or(i64::MAX);
        let (start, end) = (bound(positions.start), bound(positions.end));
        self.run("read projected event tail", |db| {
            let mut missing = Vec::new();
            db.read(|db, connection| {
                // Cached and canonical rows must describe one snapshot, even if the
                // log is replaced while the caller consumes it.
                db.execute(connection, "BEGIN", ())?;
                let result = (|| {
                    let mut gap = |from, to, visit: &mut dyn FnMut(u64, StoredEvent) -> bool| {
                        if from == to { return Ok(true); }
                        let mut more = true;
                        db.query_while(connection,
                            "SELECT position, line FROM events WHERE session_id = ?1 \
                             AND position >= ?2 AND position < ?3 ORDER BY position DESC",
                            (id, from, to), |row| {
                                let position = integer(row, 0)?;
                                let line = blob(row, 1)?;
                                let record = match decode_row(&line) {
                                    Ok(Some(record)) => record,
                                    Ok(None) => return Ok(true),
                                    Err(reason) => {
                                        log::warn!("skipping event {position} of {id}: {reason}");
                                        return Ok(true);
                                    }
                                };
                                // Legacy diffs depend on later relays and session metadata.
                                let legacy = record.legacy_file_diff;
                                let provider = if legacy {
                                    stored_provider_at(db, connection, id, position)?
                                } else { None };
                                let record = record.into_stored(provider);
                                let projected = match project(&record) {
                                    Cow::Borrowed(_) => record,
                                    Cow::Owned(projected) => {
                                        if !legacy && line.len() >= min_bytes {
                                            missing.push(position as u64);
                                        }
                                        projected
                                    }
                                };
                                more = visit(position as u64, projected);
                                Ok(more)
                            })?;
                        Ok::<_, io::Error>(more)
                    };
                    let mut cursor = end;
                    let mut more = true;
                    db.query_while(connection,
                        "SELECT position, body FROM event_preview_v1 WHERE session_id = ?1 \
                         AND position >= ?2 AND position < ?3 AND version = ?4 AND body IS NOT NULL \
                         ORDER BY position DESC",
                        (id, start, end, version), |row| {
                            let position = integer(row, 0)?;
                            more = gap(position + 1, cursor, &mut visit)?;
                            if !more { return Ok(false); }
                            match serde_json::from_slice::<StoredEvent>(&blob(row, 1)?) {
                                Ok(record) => more = visit(position as u64, record),
                                Err(_) => more = gap(position, position + 1, &mut visit)?,
                            }
                            cursor = position;
                            Ok(more)
                        })?;
                    if more { gap(start, cursor, &mut visit)?; }
                    Ok::<_, io::Error>(())
                })();
                db.execute(connection, "COMMIT", ())?;
                result
            })?;
            Ok(missing)
        })
    }

    /// Publishing is separate so history delivery does not wait for the
    /// cache's durable write.
    pub fn read_log_with_projections(
        &self,
        id: &str,
        end: u64,
        positions: &[u64],
        version: &str,
        max_bytes: usize,
        mut project: impl FnMut(&StoredEvent) -> Cow<'_, StoredEvent>,
    ) -> io::Result<(super::EventLog, ProjectionPublication)> {
        let token = uuid::Uuid::new_v4().to_string();
        let mut publication = ProjectionPublication {
            store: self.clone(),
            id: id.to_owned(),
            version: version.to_owned(),
            token: token.clone(),
            bodies: Vec::new(),
        };
        if !positions.is_empty() {
            let registered = self.run("register event previews", |db| {
                db.write(|db, connection| {
                    for &position in positions {
                        db.execute(
                            connection,
                            "INSERT INTO event_preview_v1 (session_id, position, version, token) \
                         SELECT ?1, ?2, ?3, ?4 WHERE EXISTS \
                         (SELECT 1 FROM events WHERE session_id = ?1 AND position = ?2) \
                         ON CONFLICT (session_id, position) DO UPDATE SET \
                         version = excluded.version, token = excluded.token, body = NULL",
                            (
                                id,
                                i64::try_from(position).map_err(invalid_data)?,
                                version,
                                token.as_str(),
                            ),
                        )?;
                    }
                    Ok(())
                })
            });
            if let Err(error) = registered {
                log::warn!("could not register history previews: {error}");
                return Ok((self.read_log_until(id, end)?, publication));
            }
        }
        let log = self.read_log_until(id, end)?;
        let wanted: std::collections::HashSet<_> = positions.iter().copied().collect();
        for (&position, record) in log.rows.iter().zip(&log.records) {
            if wanted.contains(&position)
                && let Cow::Owned(preview) = project(record)
            {
                match serde_json::to_vec(&preview) {
                    Ok(body) if body.len() < max_bytes => publication.bodies.push((position, body)),
                    Ok(_) => {}
                    Err(error) => log::warn!("could not encode history preview: {error}"),
                }
            }
        }
        Ok((log, publication))
    }
}

/// A canonical mutation invalidates the token, so publishing after delivery
/// cannot revive old data.
pub struct ProjectionPublication {
    store: SessionStore,
    id: String,
    version: String,
    token: String,
    bodies: Vec<(u64, Vec<u8>)>,
}

impl ProjectionPublication {
    pub fn publish(self) -> io::Result<()> {
        if self.bodies.is_empty() {
            return Ok(());
        }
        self.store.run("publish event previews", |db| {
            db.write(|db, connection| {
                for (position, body) in self.bodies {
                    db.execute(
                        connection,
                        "UPDATE event_preview_v1 SET body = ?5 WHERE session_id = ?1 \
                     AND position = ?2 AND version = ?3 AND token = ?4",
                        (
                            self.id.as_str(),
                            i64::try_from(position).map_err(invalid_data)?,
                            self.version.as_str(),
                            self.token.as_str(),
                            body,
                        ),
                    )?;
                }
                Ok(())
            })
        })
    }
}
