use std::io;

use tcode_core::settlement::ThreadActivity;
use turso::Connection;

use super::db::{Db, blob};
use super::{SessionStore, invalid_data};

pub(super) fn create_cache(db: &Db, connection: &Connection) -> io::Result<()> {
    db.execute(
        connection,
        "CREATE TABLE IF NOT EXISTS activity_cache_v1 (\
         session_id TEXT PRIMARY KEY, has_parent INTEGER NOT NULL, \
         token TEXT NOT NULL, body BLOB)",
        (),
    )?;
    // Database triggers also invalidate the cache when an older build writes
    // this data directory. A log replacement can keep exactly the same length.
    for (name, event, affected) in [
        ("insert", "INSERT", "session_id = NEW.session_id"),
        (
            "update",
            "UPDATE",
            "session_id IN (OLD.session_id, NEW.session_id)",
        ),
        ("delete", "DELETE", "session_id = OLD.session_id"),
    ] {
        db.execute(
            connection,
            &format!(
                "CREATE TRIGGER IF NOT EXISTS activity_cache_v1_{name} \
                 AFTER {event} ON events BEGIN \
                 DELETE FROM activity_cache_v1 WHERE {affected}; END"
            ),
            (),
        )?;
    }
    db.execute(
        connection,
        "CREATE TRIGGER IF NOT EXISTS activity_cache_v1_remove \
         AFTER DELETE ON sessions BEGIN \
         DELETE FROM activity_cache_v1 WHERE session_id = OLD.id; END",
        (),
    )?;
    Ok(())
}

impl SessionStore {
    /// Recover clocks without rereading unchanged logs. Cache registration and
    /// publication are batched; the slow log reads never hold the writer.
    pub fn thread_activities(
        &self,
        threads: &[(String, bool)],
    ) -> io::Result<Vec<io::Result<ThreadActivity>>> {
        let mut cached = self.run("read activity cache", |db| {
            db.read(|db, connection| {
                threads
                    .iter()
                    .map(|(id, has_parent)| {
                        let mut activity = None;
                        db.query(
                            connection,
                            "SELECT body FROM activity_cache_v1 \
                             WHERE session_id = ?1 AND has_parent = ?2 AND body IS NOT NULL",
                            (id.as_str(), i64::from(*has_parent)),
                            |row| {
                                activity = serde_json::from_slice(&blob(row, 0)?).ok();
                                Ok(())
                            },
                        )?;
                        Ok(activity)
                    })
                    .collect::<io::Result<Vec<Option<ThreadActivity>>>>()
            })
        })?;
        let missing: Vec<_> = cached
            .iter()
            .enumerate()
            .filter_map(|(index, activity)| activity.is_none().then_some(index))
            .collect();
        if missing.is_empty() {
            return Ok(cached
                .into_iter()
                .map(|activity| Ok(activity.expect("all activities cached")))
                .collect());
        }
        let token = uuid::Uuid::new_v4().to_string();
        self.run("register activity reads", |db| {
            db.write(|db, connection| {
                for &index in &missing {
                    let (id, has_parent) = &threads[index];
                    db.execute(
                        connection,
                        "INSERT INTO activity_cache_v1 (session_id, has_parent, token, body) \
                         SELECT ?1, ?2, ?3, NULL \
                         WHERE EXISTS (SELECT 1 FROM sessions WHERE id = ?1) \
                         ON CONFLICT (session_id) DO UPDATE SET \
                         has_parent = excluded.has_parent, token = excluded.token, body = NULL",
                        (id.as_str(), i64::from(*has_parent), token.as_str()),
                    )?;
                }
                Ok(())
            })
        })?;
        let activities: Vec<_> = threads
            .iter()
            .zip(cached.iter_mut())
            .map(|((id, has_parent), cached)| match cached.take() {
                Some(activity) => Ok(activity),
                None => self.read_activity(id, *has_parent),
            })
            .collect();
        self.run("publish activity cache", |db| {
            db.write(|db, connection| {
                for &index in &missing {
                    let Ok(activity) = &activities[index] else {
                        continue;
                    };
                    let body = serde_json::to_vec(activity).map_err(invalid_data)?;
                    // Any intervening log write deletes the token. A later
                    // reader has its own token, so an old read cannot publish.
                    db.execute(
                        connection,
                        "UPDATE activity_cache_v1 SET body = ?3 \
                         WHERE session_id = ?1 AND token = ?2",
                        (threads[index].0.as_str(), token.as_str(), body),
                    )?;
                }
                Ok(())
            })
        })?;
        Ok(activities)
    }

    fn read_activity(&self, id: &str, has_parent: bool) -> io::Result<ThreadActivity> {
        let mut low = self.next_row(id)?;
        let mut records = Vec::new();
        loop {
            let activity = ThreadActivity::fold_stored(&records, has_parent);
            if low == 0 || activity.complete(has_parent) {
                return Ok(activity);
            }
            let from = low.saturating_sub(256);
            let page = self.read_rows(id, from..low)?.records;
            records.splice(
                0..0,
                page.into_iter()
                    .filter(|record| ThreadActivity::moves_clock(&record.event)),
            );
            low = from;
        }
    }
}
