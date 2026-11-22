//! Antigravity's own record of a session: one SQLite database a conversation,
//! `conversations/<session id>.db` under its state directory, whose `steps`
//! table holds a row for each step of the trajectory, alongside its step log
//! `brain/<session id>/.system_generated/logs/transcript.jsonl` (agy 1.5.14).
//!
//! Over ACP the agent ends a turn whose upstream request failed as it ends any
//! other: the error is a chat message and the stop reason `end_turn`. Only
//! this record says the turn closed on an error step. Likewise, autonomous
//! replies after a prompt ends (from scheduled timers or subagents) arrive over
//! `session/update` with no `session/prompt` response; the step log and record
//! say when the final reply step completed and what tokens it used.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use turso::Value;

use crate::{ContextFreshness, TokenUsage, TurnStatus};

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

/// The completion of an agent-initiated (proactive) turn, read from the
/// session's database and transcript log.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ProactiveEnd {
    pub(super) step_index: i64,
    pub(super) status: TurnStatus,
    pub(super) usage: Option<TokenUsage>,
}

#[derive(Deserialize)]
struct TranscriptStep {
    step_index: i64,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    tool_calls: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
}

/// Where the record of session `session_id` is kept under the agent's state
/// directory `state_dir` ([`super::state_dirs`]).
pub(super) fn path(state_dir: &Path, session_id: &str) -> PathBuf {
    state_dir
        .join("conversations")
        .join(format!("{session_id}.db"))
}

/// Where the plain-JSONL step transcript of session `session_id` is kept under
/// `state_dir`.
pub(super) fn transcript_path(state_dir: &Path, session_id: &str) -> PathBuf {
    state_dir
        .join("brain")
        .join(session_id)
        .join(".system_generated")
        .join("logs")
        .join("transcript.jsonl")
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

/// Whether the autonomous (proactive) work that began after `after_step` has
/// reached a terminal step: either an `ERROR_STEP` in the session database or
/// a completed `PLANNER_RESPONSE` with no `tool_calls` in `transcript.jsonl`
/// that no newer step has followed.
pub(super) fn proactive_turn_end(
    record: &Path,
    transcript: &Path,
    after_step: i64,
    context: Option<TokenUsage>,
) -> Option<ProactiveEnd> {
    let db_last = last_step(record);
    if let Some(last) = db_last
        && last.error
        && last.index > after_step
    {
        return Some(ProactiveEnd {
            step_index: last.index,
            status: TurnStatus::Failed,
            usage: context,
        });
    }

    let text = std::fs::read_to_string(transcript).ok()?;
    let mut latest: Option<TranscriptStep> = None;
    let mut input_tokens: u64 = 0;
    let mut cached_tokens: u64 = 0;
    let mut output_tokens: u64 = 0;
    let mut saw_usage = false;

    for line in text.lines() {
        let Ok(step) = serde_json::from_str::<TranscriptStep>(line) else {
            continue;
        };
        if step.step_index <= after_step {
            continue;
        }
        if step.kind == "PLANNER_RESPONSE" {
            if let Some(input) = step.input_tokens {
                input_tokens = input_tokens.saturating_add(input);
                saw_usage = true;
            }
            if let Some(cached) = step.cache_read_tokens {
                cached_tokens = cached_tokens.saturating_add(cached);
                saw_usage = true;
            }
            if let Some(output) = step.output_tokens {
                output_tokens = output_tokens.saturating_add(output);
                saw_usage = true;
            }
        }
        if latest
            .as_ref()
            .is_none_or(|current| step.step_index >= current.step_index)
        {
            latest = Some(step);
        }
    }

    let latest = latest?;
    // A newer step in SQLite means the agent has already moved on (e.g. tool
    // execution or another model step) and this turn is not done yet.
    if db_last.is_some_and(|last| last.index > latest.step_index) {
        return None;
    }
    let has_tool_calls = latest
        .tool_calls
        .as_ref()
        .is_some_and(|calls| !calls.is_empty());
    if latest.kind != "PLANNER_RESPONSE" || latest.status != "DONE" || has_tool_calls {
        return None;
    }

    let usage = if saw_usage {
        let total = input_tokens
            .saturating_add(cached_tokens)
            .saturating_add(output_tokens);
        Some(TokenUsage {
            freshness: ContextFreshness::Current,
            turn_processed_tokens: Some(total),
            input_tokens: Some(input_tokens),
            cached_input_tokens: (cached_tokens > 0).then_some(cached_tokens),
            output_tokens: Some(output_tokens),
            used_tokens: context.and_then(|c| c.used_tokens),
            context_window: context.and_then(|c| c.context_window),
            cost_usd: context.and_then(|c| c.cost_usd),
            ..TokenUsage::default()
        })
    } else {
        context
    };

    Some(ProactiveEnd {
        step_index: latest.step_index,
        status: TurnStatus::Completed,
        usage,
    })
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

    #[test]
    fn proactive_turn_ends_only_on_terminal_planner_step_or_error() {
        let db = record(&[14, 15, 101, 15]);
        let transcript = db.with_file_name("transcript.jsonl");
        let context = Some(TokenUsage {
            freshness: ContextFreshness::Current,
            used_tokens: Some(420),
            context_window: Some(1_048_576),
            ..TokenUsage::default()
        });

        // A planner step carrying tool calls is mid-turn, not finished.
        std::fs::write(
            &transcript,
            concat!(
                r#"{"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","input_tokens":50,"output_tokens":10}"#,
                "\n",
                r#"{"step_index":3,"type":"PLANNER_RESPONSE","status":"DONE","input_tokens":100,"cache_read_tokens":20,"output_tokens":15,"tool_calls":[{"name":"read_file","args":{}}]}"#,
                "\n",
            ),
        )
        .unwrap();
        assert_eq!(proactive_turn_end(&db, &transcript, 1, context), None);

        // After the tool step completes and a final planner step answers
        // without tool calls, proactive_turn_end sums all steps after `seen`.
        let db_done = record(&[14, 15, 101, 15, 132, 15]);
        std::fs::write(
            &transcript,
            concat!(
                r#"{"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","input_tokens":50,"output_tokens":10}"#,
                "\n",
                r#"{"step_index":3,"type":"PLANNER_RESPONSE","status":"DONE","input_tokens":100,"cache_read_tokens":20,"output_tokens":15,"tool_calls":[{"name":"read_file","args":{}}]}"#,
                "\n",
                r#"{"step_index":4,"type":"GENERIC","status":"DONE","content":"ok"}"#,
                "\n",
                r#"{"step_index":5,"type":"PLANNER_RESPONSE","status":"DONE","input_tokens":40,"cache_read_tokens":80,"output_tokens":25,"content":"Done"}"#,
                "\n",
            ),
        )
        .unwrap();
        let end = proactive_turn_end(&db_done, &transcript, 1, context).unwrap();
        assert_eq!(end.step_index, 5);
        assert_eq!(end.status, TurnStatus::Completed);
        let usage = end.usage.unwrap();
        assert_eq!(usage.turn_processed_tokens, Some(280));
        assert_eq!(usage.total_processed_tokens, None);
        assert_eq!(usage.input_tokens, Some(140));
        assert_eq!(usage.cached_input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(40));
        assert_eq!(usage.used_tokens, Some(420));
        assert_eq!(usage.context_window, Some(1_048_576));

        // An error step closes the proactive turn as Failed even without a
        // transcript line for it.
        let db_err = record(&[14, 15, 101, ERROR_STEP]);
        let end = proactive_turn_end(&db_err, &transcript, 1, context).unwrap();
        assert_eq!(end.step_index, 3);
        assert_eq!(end.status, TurnStatus::Failed);

        for path in [db, db_done, db_err] {
            std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        }
    }
}
