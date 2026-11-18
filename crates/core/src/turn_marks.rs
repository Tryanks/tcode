//! A thread's turns as a reader finds their way among them: what the user
//! said to open each and inside it, and how much work it was.

use agent::{ItemContent, TurnStatus};
use serde::{Deserialize, Serialize};

use crate::session::{EntryContent, Timeline};

/// How much of a message a mark carries: a line of it, to recognize it by.
const HEAD_CHARS: usize = 200;

/// One turn the user spoke in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnMark {
    /// The turn's index among its timeline's turns.
    pub turn: usize,
    /// The head of the message that opened the turn; `None` for a turn the
    /// user only steered, or one whose opening the timeline does not hold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The messages the user put into the turn while it ran, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steers: Vec<SteerMark>,
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

/// A message put into a running turn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteerMark {
    /// The timeline entry that holds it.
    pub entry_id: String,
    pub text: String,
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

/// The marks of the turns of `timeline` the user spoke in, in order.
pub fn turn_marks(timeline: &Timeline) -> Vec<TurnMark> {
    let mut marks: Vec<TurnMark> = timeline
        .turns
        .iter()
        .enumerate()
        .map(|(turn, meta)| TurnMark {
            turn,
            started_at: meta.start_ts,
            ended_at: meta.end_ts,
            status: meta.status,
            running: meta.running,
            ..TurnMark::default()
        })
        .collect();
    for entry in &timeline.entries {
        let Some(mark) = marks.get_mut(entry.turn) else {
            continue;
        };
        match &entry.content {
            EntryContent::Item(ItemContent::UserMessage { text, .. }) => {
                if mark.message.is_none() && mark.steers.is_empty() {
                    mark.message = Some(head(text));
                }
            }
            EntryContent::Steer { text, .. } => mark.steers.push(SteerMark {
                entry_id: entry.id.clone(),
                text: head(text),
            }),
            EntryContent::Item(
                ItemContent::CommandExecution { .. }
                | ItemContent::FileChange { .. }
                | ItemContent::ToolCall { .. }
                | ItemContent::Subagent { .. }
                | ItemContent::ImageRead { .. }
                | ItemContent::WebSearch { .. }
                | ItemContent::Other { .. },
            ) => mark.work += 1,
            _ => {}
        }
    }
    marks.retain(|mark| mark.message.is_some() || !mark.steers.is_empty());
    marks
}
