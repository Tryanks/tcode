use std::{collections::HashMap, io};

use tcode_core::{
    session::StoredEvent,
    settlement::{THREAD_ACTIVITY_FOLD_VERSION, ThreadActivity},
};
use turso::Connection;

use super::codec::Mode;
use super::db::{Db, blob, integer, text};
use super::rows::{self, Storage, Thread};
use super::{SessionStore, decode_row, invalid_data};

pub(super) fn create_table(db: &Db, connection: &Connection) -> io::Result<()> {
    db.execute(
        connection,
        "CREATE TABLE IF NOT EXISTS thread_activity (\
         thread INTEGER PRIMARY KEY, fold_version INTEGER NOT NULL, \
         has_parent INTEGER NOT NULL, body BLOB NOT NULL)",
        (),
    )?;
    Ok(())
}

fn session_parent(st: &Storage, connection: &Connection, id: &str) -> io::Result<Option<bool>> {
    let mut parent = None;
    st.query(
        connection,
        "SELECT json_extract(body, '$.parent_session_id') IS NOT NULL FROM sessions WHERE id = ?1",
        [id],
        |row| {
            parent = Some(integer(row, 0)? != 0);
            Ok(())
        },
    )?;
    Ok(parent)
}

fn read(
    st: &Storage,
    connection: &Connection,
    thread: &Thread,
    parent: bool,
) -> io::Result<Option<ThreadActivity>> {
    let mut activity = None;
    st.query(
        connection,
        "SELECT body FROM thread_activity WHERE thread = ?1 AND fold_version = ?2 AND has_parent = ?3",
        (thread.key, i64::from(THREAD_ACTIVITY_FOLD_VERSION), i64::from(parent)),
        |row| {
            match serde_json::from_slice(&blob(row, 0)?) {
                Ok(value) => activity = Some(value),
                Err(error) => log::warn!("thread activity for {} will be rebuilt: {error}", thread.id),
            }
            Ok(())
        },
    )?;
    Ok(activity)
}

fn save(
    st: &Storage,
    connection: &Connection,
    thread: &Thread,
    parent: bool,
    activity: &ThreadActivity,
) -> io::Result<()> {
    st.execute(
        connection,
        "INSERT INTO thread_activity (thread, fold_version, has_parent, body) VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT (thread) DO UPDATE SET fold_version = excluded.fold_version, \
         has_parent = excluded.has_parent, body = excluded.body",
        (thread.key, i64::from(THREAD_ACTIVITY_FOLD_VERSION), i64::from(parent), serde_json::to_vec(activity).map_err(invalid_data)?),
    )?;
    Ok(())
}

fn fold(
    st: &Storage,
    connection: &Connection,
    thread: Option<&Thread>,
    parent: bool,
) -> io::Result<ThreadActivity> {
    let mut records = Vec::new();
    if let Some(thread) = thread {
        let codec = st.codec();
        let mut decoder = codec.decoder();
        rows::visit(st, connection, thread, 0..u64::MAX, false, |_, row| {
            if let Ok(Some(record)) = decode_row(&st.decode(&mut decoder, &row, Mode::Bare)?.bytes)
                && ThreadActivity::moves_clock(&record.event)
            {
                records.push(record);
            }
            Ok(true)
        })?;
    }
    Ok(ThreadActivity::fold_stored(&records, parent))
}

pub(super) fn recompute(
    st: &Storage,
    connection: &Connection,
    thread: &Thread,
) -> io::Result<ThreadActivity> {
    let parent = session_parent(st, connection, &thread.id)?.unwrap_or(false);
    let activity = fold(st, connection, Some(thread), parent)?;
    save(st, connection, thread, parent, &activity)?;
    Ok(activity)
}

pub(super) fn append(
    st: &Storage,
    connection: &Connection,
    thread: &Thread,
    record: &StoredEvent,
) -> io::Result<()> {
    let mut stored = None;
    st.query(
        connection,
        "SELECT has_parent, body FROM thread_activity WHERE thread = ?1 AND fold_version = ?2",
        (thread.key, i64::from(THREAD_ACTIVITY_FOLD_VERSION)),
        |row| {
            if let Ok(activity) = serde_json::from_slice::<ThreadActivity>(&blob(row, 1)?) {
                stored = Some((integer(row, 0)? != 0, activity));
            }
            Ok(())
        },
    )?;
    if let Some((parent, mut activity)) = stored {
        activity.apply(record, parent);
        save(st, connection, thread, parent, &activity)?;
    }
    Ok(())
}

pub(super) fn remove(st: &Storage, connection: &Connection, thread: &Thread) -> io::Result<()> {
    st.execute(
        connection,
        "DELETE FROM thread_activity WHERE thread = ?1",
        [thread.key],
    )?;
    Ok(())
}

impl SessionStore {
    pub fn load_thread_activity(&self) -> io::Result<HashMap<String, ThreadActivity>> {
        self.run("load thread activity", |st| {
            st.read(|st, connection| {
                let mut activities = HashMap::new();
                st.query(
                    connection,
                    "SELECT t.id, a.body FROM thread_activity a JOIN threads t ON t.key = a.thread \
                     WHERE a.fold_version = ?1",
                    [i64::from(THREAD_ACTIVITY_FOLD_VERSION)],
                    |row| {
                        let id = text(row, 0)?;
                        match serde_json::from_slice(&blob(row, 1)?) {
                            Ok(activity) => {
                                activities.insert(id, activity);
                            }
                            Err(error) => {
                                log::warn!("thread activity for {id} will be rebuilt: {error}")
                            }
                        }
                        Ok(())
                    },
                )?;
                Ok(activities)
            })
        })
    }

    /// A `None` activity must be retried because its log changed during the fold.
    /// Sessions absent from the store are omitted from the result.
    pub fn backfill_thread_activity(
        &self,
        ids: &[String],
    ) -> io::Result<HashMap<String, Option<ThreadActivity>>> {
        self.run("backfill thread activity", |st| {
            let mut activities = HashMap::new();
            for id in ids {
                let candidate = st.read(|st, connection| {
                    st.execute(connection, "BEGIN", ())?;
                    let result = (|| {
                        let Some(parent) = session_parent(st, connection, id)? else {
                            return Ok(None);
                        };
                        let thread = rows::thread(st, connection, id)?;
                        let (end, revision) = match &thread {
                            Some(thread) => (
                                rows::end(st, connection, thread)?,
                                rows::revision(st, connection, id)?,
                            ),
                            None => (0, Vec::new()),
                        };
                        let activity = match &thread {
                            Some(thread) => match read(st, connection, thread, parent)? {
                                Some(activity) => activity,
                                None => fold(st, connection, Some(thread), parent)?,
                            },
                            None => fold(st, connection, None, parent)?,
                        };
                        let index = match &thread {
                            Some(thread) => super::turn_index::candidate(st, connection, thread)?,
                            None => Some(super::turn_index::Pending::default()),
                        };
                        Ok(Some((parent, end, revision, activity, index)))
                    })();
                    st.execute(connection, "ROLLBACK", ())?;
                    result
                })?;
                let Some((parent, end, revision, activity, index)) = candidate else {
                    continue;
                };
                let published = st.write(|st, connection| {
                    if session_parent(st, connection, id)? != Some(parent) {
                        return Ok(None);
                    }
                    let thread = rows::ensure_thread(st, connection, id)?;
                    if rows::end(st, connection, &thread)? != end
                        || (end > 0 && rows::revision(st, connection, id)? != revision)
                    {
                        return Ok(None);
                    }
                    if let Some(index) = index
                        && super::turn_index::read(st, connection, &thread)?.is_none()
                    {
                        super::turn_index::save(st, connection, thread.key, &index)?;
                    }
                    if let Some(current) = read(st, connection, &thread, parent)? {
                        return Ok(Some(current));
                    }
                    save(st, connection, &thread, parent, &activity)?;
                    Ok(Some(activity))
                })?;
                activities.insert(id.clone(), published);
            }
            Ok(activities)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Mutation;
    use agent::ProviderKind;
    use tcode_core::project::SessionMeta;

    const LOG: &str = concat!(
        "{\"ts\":100,\"event\":{\"type\":\"item_completed\",\"id\":\"human\",\"content\":{\"kind\":\"user_message\",\"text\":\"request\"}}}\n",
        "{\"ts\":101,\"event\":{\"type\":\"turn_started\",\"turn_id\":\"one\"}}\n",
        "{\"ts\":102,\"origin\":\"human\",\"event\":{\"type\":\"steer_requested\",\"request_id\":\"steer\",\"text\":\"change\"}}\n",
        "{\"ts\":103,\"event\":{\"type\":\"turn_completed\",\"turn_id\":\"one\",\"status\":\"failed\",\"usage\":null}}\n",
        "{\"ts\":104,\"event\":{\"type\":\"turn_started\",\"turn_id\":\"two\"}}\n",
        "{\"ts\":105,\"event\":{\"type\":\"turn_completed\",\"turn_id\":\"two\",\"status\":\"interrupted\",\"usage\":null}}\n",
        "{\"ts\":106,\"origin\":\"agent\",\"event\":{\"type\":\"steer_requested\",\"request_id\":\"agent\",\"text\":\"continue\"}}\n",
        "{\"ts\":107,\"event\":{\"type\":\"provider_start_failed\",\"error\":\"cannot start\"}}\n",
        "{\"ts\":108,\"event\":{\"type\":\"turn_started\",\"turn_id\":\"three\"}}\n",
        "{\"ts\":109,\"event\":{\"type\":\"turn_completed\",\"turn_id\":\"three\",\"status\":\"completed\",\"usage\":null}}\n",
        "{\"ts\":90,\"event\":{\"type\":\"item_completed\",\"id\":\"late\",\"content\":{\"kind\":\"user_message\",\"text\":\"older stamp\"}}}\n",
    );

    #[test]
    fn committed_activity_matches_replay_through_append_import_parent_changes_and_deletion() {
        let root = std::env::temp_dir().join(format!("thread-activity-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::open_at(root.clone()).unwrap();
        let records: Vec<_> = LOG
            .lines()
            .map(|line| super::super::parse_stored_line(line).unwrap())
            .collect();
        for parent in [false, true] {
            let mut meta = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
            meta.parent_session_id = parent.then(|| "lead".into());
            store.upsert_meta(&meta).unwrap();
            for (index, record) in records.iter().enumerate() {
                store
                    .apply(&[Mutation::append_authored_event(
                        &meta.id,
                        record.ts.unwrap(),
                        &record.event,
                        None,
                        record.origin,
                        |_| None,
                    )
                    .unwrap()])
                    .unwrap();
                assert_eq!(
                    store.load_thread_activity().unwrap()[&meta.id],
                    ThreadActivity::fold_stored(&records[..=index], parent)
                );
                if index == 0 {
                    meta.parent_session_id = (!parent).then(|| "lead".into());
                    store.upsert_meta(&meta).unwrap();
                    assert_eq!(
                        store.load_thread_activity().unwrap()[&meta.id],
                        ThreadActivity::fold_stored(&records[..=index], !parent)
                    );
                    meta.parent_session_id = parent.then(|| "lead".into());
                    store.upsert_meta(&meta).unwrap();
                }
            }
            meta.parent_session_id = (!parent).then(|| "lead".into());
            store.upsert_meta(&meta).unwrap();
            assert_eq!(
                store.load_thread_activity().unwrap()[&meta.id],
                ThreadActivity::fold_stored(&records, !parent)
            );
            store
                .apply(&[Mutation::replace_event_log(
                    &meta.id,
                    LOG.lines().next().unwrap().as_bytes().to_vec(),
                )])
                .unwrap();
            assert_eq!(
                store.load_thread_activity().unwrap()[&meta.id],
                ThreadActivity::fold_stored(&records[..1], !parent)
            );
            let mut fork = meta.clone();
            fork.id = uuid::Uuid::new_v4().to_string();
            store
                .apply(&[
                    Mutation::clone_events(&meta.id, &fork.id),
                    Mutation::upsert_meta(fork.clone()),
                ])
                .unwrap();
            assert_eq!(
                store.load_thread_activity().unwrap()[&fork.id],
                ThreadActivity::fold_stored(&records[..1], !parent)
            );
            store.remove_session(&meta.id).unwrap();
            assert!(!store.load_thread_activity().unwrap().contains_key(&meta.id));
            store.remove_session(&fork.id).unwrap();
        }
        store.close().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn activity_backfill_resumes_per_session_and_rebuilds_a_new_fold_version() {
        let root =
            std::env::temp_dir().join(format!("activity-migration-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::open_at(root.clone()).unwrap();
        let mut ids = Vec::new();
        for _ in 0..3 {
            let meta = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
            store.upsert_meta(&meta).unwrap();
            store
                .apply(&[Mutation::replace_event_log(
                    &meta.id,
                    LOG.as_bytes().to_vec(),
                )])
                .unwrap();
            ids.push(meta.id);
        }
        store
            .run("seed old activity version", |st| {
                st.write(|st, connection| {
                    st.execute(
                        connection,
                        "UPDATE thread_activity SET fold_version = 0",
                        (),
                    )?;
                    st.execute(
                        connection,
                        "UPDATE thread_activity SET fold_version = ?2, body = ?3 \
                        WHERE thread = (SELECT key FROM threads WHERE id = ?1)",
                        (
                            ids[2].as_str(),
                            i64::from(THREAD_ACTIVITY_FOLD_VERSION),
                            b"not-json".as_slice(),
                        ),
                    )?;
                    Ok(())
                })
            })
            .unwrap();
        assert!(store.load_thread_activity().unwrap().is_empty());
        store.backfill_thread_activity(&ids[..1]).unwrap();
        store.close().unwrap();
        let store = SessionStore::open_at(root.clone()).unwrap();
        assert_eq!(
            store
                .load_thread_activity()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            vec![&ids[0]]
        );
        store
            .append_event(
                &ids[1],
                200,
                &agent::AgentEvent::ProviderStartFailed {
                    error: "start failed".into(),
                },
            )
            .unwrap();
        assert!(!store.load_thread_activity().unwrap().contains_key(&ids[1]));
        store.backfill_thread_activity(&ids).unwrap();
        let activities = store.load_thread_activity().unwrap();
        for id in ids {
            assert_eq!(
                activities[&id],
                ThreadActivity::fold_stored(&store.read_events(&id).unwrap(), false)
            );
        }
        store.close().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }
}
