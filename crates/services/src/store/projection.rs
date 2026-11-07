use std::io;

use turso::Connection;

use super::SessionStore;
use super::db::Db;

pub(super) fn create_cache(db: &Db, connection: &Connection) -> io::Result<()> {
    db.execute(
        connection,
        "CREATE TABLE IF NOT EXISTS event_preview (\
         key INTEGER PRIMARY KEY, version TEXT NOT NULL, token TEXT NOT NULL, body BLOB)",
        (),
    )?;
    for (name, event, affected) in [
        ("insert", "INSERT", "key = NEW.key"),
        ("update", "UPDATE", "key = OLD.key OR key = NEW.key"),
        ("delete", "DELETE", "key = OLD.key"),
    ] {
        db.execute(
            connection,
            &format!(
                "CREATE TRIGGER IF NOT EXISTS event_preview_{name} AFTER {event} ON events \
                 BEGIN DELETE FROM event_preview WHERE {affected}; END"
            ),
            (),
        )?;
    }
    Ok(())
}

pub(super) struct Preview {
    pub key: i64,
    /// The row's stored bytes the preview was made from.
    pub canonical: Vec<u8>,
    pub version: String,
    pub body: Vec<u8>,
}

/// Publication compares the stored row before assigning a fresh token.
/// Replacement, diff stripping and rows still in an older file cannot publish
/// a projection of other bytes.
pub struct ProjectionPublication {
    pub(super) store: SessionStore,
    pub(super) rows: Vec<Preview>,
}

impl ProjectionPublication {
    pub fn publish(self) -> io::Result<()> {
        if self.rows.is_empty() {
            return Ok(());
        }
        let token = uuid::Uuid::new_v4().to_string();
        self.store.run("publish event previews", |st| {
            st.write(|st, connection| {
                for row in self.rows {
                    st.execute(
                        connection,
                        "INSERT INTO event_preview (key, version, token, body) \
                         SELECT ?1, ?2, ?3, ?4 WHERE EXISTS \
                         (SELECT 1 FROM events WHERE key = ?1 AND line = ?5) \
                         ON CONFLICT (key) DO UPDATE SET version = excluded.version, \
                         token = excluded.token, body = excluded.body",
                        (
                            row.key,
                            row.version,
                            token.as_str(),
                            row.body,
                            row.canonical,
                        ),
                    )?;
                }
                Ok(())
            })
        })
    }
}
