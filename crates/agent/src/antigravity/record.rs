//! Antigravity's own record of a session: one SQLite database a conversation,
//! `conversations/<session id>.db` under its state directory, whose `steps`
//! table holds a row for each step of the trajectory (agy 1.5.14).
//!
//! Over ACP the agent ends a turn whose upstream request failed as it ends any
//! other: the error is a chat message and the stop reason `end_turn`. Only
//! this record says the turn closed on an error step.

use std::path::{Path, PathBuf};

use turso::Value;

/// The type of the step a turn ends on when its upstream request failed: the
/// last step of every such turn in the records agy 1.5.14 wrote for prompts
/// its API refused.
const ERROR_STEP: i64 = 17;

/// The last step a session's record holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct LastStep {
    pub(super) index: i64,
    pub(super) error: bool,
}

/// Where the record of session `session_id` is kept under the agent's state
/// directory `state_dir` ([`super::state_dirs`]).
pub(super) fn path(state_dir: &Path, session_id: &str) -> PathBuf {
    state_dir
        .join("conversations")
        .join(format!("{session_id}.db"))
}

/// The last step in the record at `record`; `None` where it holds none yet
/// or cannot be read. The agent has the file open while the session lives, as
/// SQLite shares it; it is opened read-only, which takes no lock on it and
/// reads the steps still in its write-ahead log.
pub(super) fn last_step(record: &Path) -> Option<LastStep> {
    match read_last_step(record) {
        Ok(step) => step,
        Err(err) => {
            log::debug!(
                "antigravity: the session record {} was not read: {err}",
                record.display()
            );
            None
        }
    }
}

fn read_last_step(record: &Path) -> Result<Option<LastStep>, String> {
    // Turso's API is async and drives its own I/O.
    smol::future::block_on(async {
        let failed = |err: turso::Error| err.to_string();
        // Turso locks a file it may write for itself alone, and the agent
        // holds SQLite's shared lock on this one.
        let database = turso::Builder::new_local(&record.to_string_lossy())
            .read_only(true)
            .build()
            .await
            .map_err(failed)?;
        let connection = database.connect().map_err(failed)?;
        let mut statement = connection
            .prepare("SELECT idx, step_type FROM steps ORDER BY idx DESC LIMIT 1")
            .await
            .map_err(failed)?;
        let mut rows = statement.query(()).await.map_err(failed)?;
        let Some(row) = rows.next().await.map_err(failed)? else {
            return Ok(None);
        };
        match (row.get_value(0), row.get_value(1)) {
            (Ok(Value::Integer(index)), Ok(Value::Integer(kind))) => Ok(Some(LastStep {
                index,
                error: kind == ERROR_STEP,
            })),
            other => Err(format!("a step row read as {other:?}")),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record with the `steps` table of agy 1.5.14's conversation
    /// databases (`idx` and `step_type` of its eleven columns), holding one
    /// step of each of `kinds` in order. The step types are invented but for
    /// the error step's.
    fn record(kinds: &[i64]) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tcode-agy-record-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.db");
        smol::future::block_on(async {
            let database = turso::Builder::new_local(&path.to_string_lossy())
                .build()
                .await
                .unwrap();
            let connection = database.connect().unwrap();
            connection
                .execute(
                    "CREATE TABLE steps (idx integer, step_type integer NOT NULL DEFAULT 0, \
                     PRIMARY KEY (idx))",
                    (),
                )
                .await
                .unwrap();
            for (index, kind) in kinds.iter().enumerate() {
                connection
                    .execute(
                        "INSERT INTO steps (idx, step_type) VALUES (?, ?)",
                        (index as i64, *kind),
                    )
                    .await
                    .unwrap();
            }
        });
        path
    }

    /// The record's last step is what a turn closed on: an error step where
    /// its upstream request failed, and any other where the model answered.
    /// A record with no step, or none at all, says nothing.
    #[test]
    fn the_last_step_says_whether_the_turn_closed_on_an_error() {
        let failed = record(&[14, ERROR_STEP]);
        assert_eq!(
            last_step(&failed),
            Some(LastStep {
                index: 1,
                error: true
            })
        );
        let answered = record(&[14, ERROR_STEP, 14, 15]);
        assert_eq!(
            last_step(&answered),
            Some(LastStep {
                index: 3,
                error: false
            })
        );
        let empty = record(&[]);
        assert_eq!(last_step(&empty), None);
        assert_eq!(last_step(&empty.with_file_name("missing.db")), None);
        for path in [failed, answered, empty] {
            std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        }
    }
}
