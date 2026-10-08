//! Event-derived activity and thread lifecycle policy. All activity clocks use Unix milliseconds.

use agent::{AgentEvent, ItemContent, ThreadItem};
use serde::{Deserialize, Serialize};

use crate::session::{MessageOrigin, StoredEvent};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadActivity {
    pub last_message_at: Option<u64>,
    pub last_human_message_at: Option<u64>,
    pub last_run_requested_at: Option<u64>,
    pub last_run_started_at: Option<u64>,
    pub last_run_completed_at: Option<u64>,
    pub failed: bool,
}

impl ThreadActivity {
    pub fn apply(&mut self, record: &StoredEvent, has_parent: bool) {
        let Some(ts) = record.ts else { return };
        match &record.event {
            AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::UserMessage { .. },
                ..
            })
            | AgentEvent::SteerRequested { .. } => {
                self.last_message_at = self.last_message_at.max(Some(ts));
                if record
                    .origin
                    .unwrap_or_else(|| MessageOrigin::legacy(has_parent))
                    == MessageOrigin::Human
                {
                    self.last_human_message_at = self.last_human_message_at.max(Some(ts));
                }
                // Ordinary messages are recorded on acknowledged submission; a steer joins the live run.
                self.last_run_requested_at = self.last_run_requested_at.max(Some(ts));
            }
            AgentEvent::TurnStarted { .. } => {
                self.last_run_started_at = Some(ts);
                self.failed = false;
            }
            AgentEvent::TurnCompleted { status, .. } => {
                self.last_run_completed_at = Some(ts);
                self.failed = *status == agent::TurnStatus::Failed;
            }
            AgentEvent::ProviderStartFailed { .. } => self.failed = true,
            _ => {}
        }
    }

    pub fn last_activity_at(&self) -> Option<u64> {
        [
            self.last_message_at,
            self.last_run_requested_at,
            self.last_run_started_at,
            self.last_run_completed_at,
        ]
        .into_iter()
        .flatten()
        .max()
    }
}
