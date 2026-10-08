//! Event-derived activity and thread lifecycle policy. All activity clocks use Unix milliseconds.

use agent::{AgentEvent, ItemContent, ThreadItem, TurnStatus};

use crate::project::SessionMeta;
use crate::session::{MessageOrigin, StoredEvent};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThreadActivity {
    pub last_message_at: Option<u64>,
    pub last_human_message_at: Option<u64>,
    pub last_run_started_at: Option<u64>,
    pub last_run_completed_at: Option<u64>,
    /// The latest run ended in an error and nothing has run since.
    pub failed: bool,
}

impl ThreadActivity {
    pub fn fold_stored<'a>(
        records: impl IntoIterator<Item = &'a StoredEvent>,
        has_parent: bool,
    ) -> Self {
        let mut activity = Self::default();
        for record in records {
            activity.apply(record, has_parent);
        }
        activity
    }

    /// Whether the record moves a clock: the latest of these, and what follows
    /// it, is all the fold needs from a log.
    pub fn moves_clock(event: &AgentEvent) -> bool {
        matches!(
            event,
            AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::UserMessage { .. },
                ..
            }) | AgentEvent::SteerRequested { .. }
                | AgentEvent::TurnStarted { .. }
                | AgentEvent::TurnCompleted { .. }
                | AgentEvent::ProviderStartFailed { .. }
        )
    }

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
            }
            AgentEvent::TurnStarted { .. } => {
                self.last_run_started_at = self.last_run_started_at.max(Some(ts));
                self.failed = false;
            }
            AgentEvent::TurnCompleted { status, .. } => {
                self.last_run_completed_at = self.last_run_completed_at.max(Some(ts));
                self.failed = *status == TurnStatus::Failed;
            }
            AgentEvent::ProviderStartFailed { .. } => {
                self.last_run_completed_at = self.last_run_completed_at.max(Some(ts));
                self.failed = true;
            }
            _ => {}
        }
    }

    pub fn last_activity_at(&self) -> Option<u64> {
        [
            self.last_message_at,
            self.last_run_started_at,
            self.last_run_completed_at,
        ]
        .into_iter()
        .flatten()
        .max()
    }
}

/// Host facts that keep a thread out of automatic settlement.
#[derive(Debug, Clone, Copy, Default)]
pub struct SettlementBlockers {
    pub pending_input: bool,
    pub live_run: bool,
    pub completion_holding_work: bool,
    pub pending_human_message: bool,
    pub scheduled_wake: bool,
}

impl SettlementBlockers {
    fn any(self) -> bool {
        self.pending_input
            || self.live_run
            || self.completion_holding_work
            || self.pending_human_message
            || self.scheduled_wake
    }
}

/// The activity stamp, in milliseconds, to record as the settlement time when
/// the thread has been inactive for longer than `days`.
pub fn automatic_settlement_at(
    meta: &SessionMeta,
    activity: &ThreadActivity,
    blockers: SettlementBlockers,
    now_ms: u64,
    days: Option<f64>,
) -> Option<u64> {
    if meta.archived_at.is_some()
        || meta.settled_override.is_some()
        || meta.is_settled()
        || meta.auto_settle_disabled_at.is_some()
        || meta.parent_session_id.is_some()
        || blockers.any()
    {
        return None;
    }
    let days = days?;
    let last_activity = activity.last_activity_at()?;
    ((last_activity as f64) < now_ms as f64 - days * 86_400_000.0).then_some(last_activity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::SettledOverride;

    #[test]
    fn inactivity_settles_at_the_last_activity_unless_blocked() {
        let meta = SessionMeta::new(agent::ProviderKind::Codex, "/sample".into(), None);
        let activity = ThreadActivity {
            last_message_at: Some(100_000),
            last_run_completed_at: Some(90_000),
            ..Default::default()
        };
        let three_days = 3 * 86_400_000;
        let settle = |meta: &SessionMeta, activity: &ThreadActivity, blockers, now| {
            automatic_settlement_at(meta, activity, blockers, now, Some(3.0))
        };
        let blockers = SettlementBlockers::default();
        assert_eq!(
            settle(&meta, &activity, blockers, 100_000 + three_days + 1),
            Some(100_000)
        );
        assert_eq!(
            settle(&meta, &activity, blockers, 100_000 + three_days),
            None,
            "strictly older than the window"
        );
        let now = 100_000 + three_days + 1;
        assert_eq!(
            automatic_settlement_at(&meta, &activity, blockers, now, None),
            None,
            "never"
        );
        assert_eq!(
            settle(&meta, &ThreadActivity::default(), blockers, now),
            None,
            "no activity never ages"
        );
        for blocker in [
            "archived",
            "active",
            "settled",
            "disabled",
            "child",
            "input",
            "run",
            "background",
            "human",
            "wake",
        ] {
            let mut blocked = meta.clone();
            match blocker {
                "archived" => blocked.archived_at = Some(1),
                "active" => blocked.settled_override = Some(SettledOverride::Active),
                "settled" => blocked.settled_override = Some(SettledOverride::Settled),
                "disabled" => blocked.auto_settle_disabled_at = Some(1),
                "child" => blocked.parent_session_id = Some("lead".into()),
                _ => {}
            }
            let blockers = SettlementBlockers {
                pending_input: blocker == "input",
                live_run: blocker == "run",
                completion_holding_work: blocker == "background",
                pending_human_message: blocker == "human",
                scheduled_wake: blocker == "wake",
            };
            assert_eq!(
                settle(&blocked, &activity, blockers, now),
                None,
                "{blocker}"
            );
        }
    }
}
