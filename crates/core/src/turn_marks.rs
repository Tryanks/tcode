//! A thread's turns as a reader finds their way among them: what the user
//! said to open each, and how much work it was.

use agent::{ItemContent, TurnStatus};
use serde::{Deserialize, Serialize};

use crate::session::{EntryContent, Timeline};

/// How much of a message a mark carries: a line of it, to recognize it by.
const HEAD_CHARS: usize = 200;

/// One turn the user opened.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnMark {
    /// The turn's index among its timeline's turns.
    pub turn: usize,
    /// The head of the message that opened the turn.
    pub message: String,
    /// Unix milliseconds the turn began and ended at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<TurnStatus>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub running: bool,
    /// How many commands, edits and other calls the turn made.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub work: u32,
}

fn is_zero(count: &u32) -> bool {
    *count == 0
}

impl TurnMark {
    /// How long the turn ran: `None` while it runs, or without its start.
    pub fn elapsed_ms(&self) -> Option<u64> {
        Some(self.ended_at?.saturating_sub(self.started_at?))
    }
}

fn head(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match collapsed.char_indices().nth(HEAD_CHARS) {
        Some((cut, _)) => format!("{}…", collapsed[..cut].trim_end()),
        None => collapsed,
    }
}

/// The marks of the turns of `timeline` the user opened, in order. A turn
/// with no opening message (one the harness began, or one whose opening the
/// timeline does not hold) has none.
pub fn turn_marks(timeline: &Timeline) -> Vec<TurnMark> {
    let mut marks: Vec<Option<TurnMark>> = vec![None; timeline.turns.len()];
    let mut work = vec![0u32; timeline.turns.len()];
    for entry in &timeline.entries {
        let Some(meta) = timeline.turns.get(entry.turn) else {
            continue;
        };
        match &entry.content {
            EntryContent::Item(ItemContent::UserMessage { text, .. }) => {
                marks[entry.turn].get_or_insert_with(|| TurnMark {
                    turn: entry.turn,
                    message: head(text),
                    started_at: meta.start_ts,
                    ended_at: meta.end_ts,
                    status: meta.status,
                    running: meta.running,
                    work: 0,
                });
            }
            EntryContent::Item(
                ItemContent::CommandExecution { .. }
                | ItemContent::FileChange { .. }
                | ItemContent::ToolCall { .. }
                | ItemContent::Subagent { .. }
                | ItemContent::ImageRead { .. }
                | ItemContent::WebSearch { .. }
                | ItemContent::Other { .. },
            ) => work[entry.turn] += 1,
            _ => {}
        }
    }
    marks
        .into_iter()
        .flatten()
        .map(|mut mark| {
            mark.work = work[mark.turn];
            mark
        })
        .collect()
}
