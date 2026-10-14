use std::io;

use tcode_core::settlement::{THREAD_ACTIVITY_FOLD_VERSION, ThreadActivity};
use turso::Connection;

use super::db::{Db, blob};
use super::{SessionStore, invalid_data};

fn cache_table() -> String {
    format!("activity_cache_v{THREAD_ACTIVITY_FOLD_VERSION}")
}

pub(super) fn create_cache(db: &Db, connection: &Connection) -> io::Result<()> {
    let table = cache_table();
    db.execute(
        connection,
        &format!(
            "CREATE TABLE IF NOT EXISTS {table} (\
         session_id TEXT PRIMARY KEY, has_parent INTEGER NOT NULL, \
         token TEXT NOT NULL, body BLOB)"
        ),
        (),
    )?;
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
                "CREATE TRIGGER IF NOT EXISTS {table}_{name} \
                 AFTER {event} ON events BEGIN \
                 DELETE FROM {table} WHERE {affected}; END"
            ),
            (),
        )?;
    }
    db.execute(
        connection,
        &format!(
            "CREATE TRIGGER IF NOT EXISTS {table}_remove \
         AFTER DELETE ON sessions BEGIN \
         DELETE FROM {table} WHERE session_id = OLD.id; END"
        ),
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
        let table = cache_table();
        let mut cached = self.run("read activity cache", |db| {
            db.read(|db, connection| {
                threads
                    .iter()
                    .map(|(id, has_parent)| {
                        let mut activity = None;
                        db.query(
                            connection,
                            &format!(
                                "SELECT body FROM {table} \
                             WHERE session_id = ?1 AND has_parent = ?2 AND body IS NOT NULL"
                            ),
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
                        &format!(
                            "INSERT INTO {table} (session_id, has_parent, token, body) \
                         SELECT ?1, ?2, ?3, NULL \
                         WHERE EXISTS (SELECT 1 FROM sessions WHERE id = ?1) \
                         ON CONFLICT (session_id) DO UPDATE SET \
                         has_parent = excluded.has_parent, token = excluded.token, body = NULL"
                        ),
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
                    publish_activity(db, connection, &threads[index].0, &token, activity)?;
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

fn publish_activity(
    db: &Db,
    connection: &Connection,
    id: &str,
    token: &str,
    activity: &ThreadActivity,
) -> io::Result<u64> {
    let body = serde_json::to_vec(activity).map_err(invalid_data)?;
    db.execute(
        connection,
        &format!(
            "UPDATE {} SET body = ?3 WHERE session_id = ?1 AND token = ?2",
            cache_table()
        ),
        (id, token, body),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::{AgentEvent, ProviderKind};
    use tcode_core::project::SessionMeta;

    #[test]
    fn activity_cache_rejects_old_folds_and_publication_after_a_log_write() {
        let root = std::env::temp_dir().join(format!("activity-cache-{}", uuid::Uuid::new_v4()));
        {
            let store = SessionStore::open_at(root.clone()).unwrap();
            let meta = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
            store.upsert_meta(&meta).unwrap();
            let event = AgentEvent::TurnStarted {
                turn_id: "run".into(),
            };
            store.append_event(&meta.id, 100, &event).unwrap();
            let old_fold = ThreadActivity {
                last_run_started_at: Some(999),
                ..Default::default()
            };
            store
                .run("seed persisted old fold", |db| {
                    db.write(|db, connection| {
                        db.execute(
                            connection,
                            "CREATE TABLE IF NOT EXISTS activity_cache_v1 (session_id TEXT PRIMARY KEY, \
                     has_parent INTEGER NOT NULL, token TEXT NOT NULL, body BLOB)",
                            (),
                        )?;
                        db.execute(
                            connection,
                            "INSERT INTO activity_cache_v1 VALUES (?1, 0, 'old-fold', ?2)",
                            (meta.id.as_str(), serde_json::to_vec(&old_fold).unwrap()),
                        )?;
                        Ok(())
                    })
                })
                .unwrap();

            let threads = [(meta.id.clone(), false)];
            let mut clocks = store.thread_activities(&threads).unwrap();
            let cached = clocks.remove(0).unwrap();
            assert_eq!(cached.last_run_started_at, Some(100));
            let cache_row = || {
                store
                    .run("inspect activity cache", |db| {
                        db.read(|db, connection| {
                            let mut row = None;
                            db.query(
                                connection,
                                &format!(
                                    "SELECT token, body FROM {} WHERE session_id = ?1",
                                    cache_table()
                                ),
                                [meta.id.as_str()],
                                |record| {
                                    row = Some((
                                        super::super::db::text(record, 0)?,
                                        serde_json::from_slice::<ThreadActivity>(&blob(record, 1)?)
                                            .unwrap(),
                                    ));
                                    Ok(())
                                },
                            )?;
                            Ok(row)
                        })
                    })
                    .unwrap()
            };
            let (token, body) = cache_row().expect("recovery publishes the clock");
            assert_eq!(body, cached);

            store.append_event(&meta.id, 200, &event).unwrap();
            assert!(
                cache_row().is_none(),
                "appending invalidates the cached clock"
            );
            let publish = || {
                store
                    .run("publish stale activity", |db| {
                        db.write(|db, connection| {
                            publish_activity(db, connection, &meta.id, &token, &cached)
                        })
                    })
                    .unwrap()
            };
            assert_eq!(publish(), 0, "a deleted read token cannot publish");
            let mut clocks = store.thread_activities(&threads).unwrap();
            assert_eq!(clocks.remove(0).unwrap().last_run_started_at, Some(200));
            assert_eq!(
                publish(),
                0,
                "an old reader cannot overwrite a newer recovery"
            );
            assert_eq!(cache_row().unwrap().1.last_run_started_at, Some(200));
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
