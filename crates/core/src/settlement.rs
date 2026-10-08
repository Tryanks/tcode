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
    pending_requests: std::collections::BTreeMap<String, bool>,
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

    pub fn apply(&mut self, record: &StoredEvent, has_parent: bool) {
        match &record.event {
            AgentEvent::ApprovalRequested(request) => {
                self.pending_requests.insert(request.id.clone(), true);
            }
            AgentEvent::UserInputRequested {
                request_id,
                delivery,
                ..
            } => {
                self.pending_requests
                    .insert(request_id.clone(), delivery.is_blocking());
            }
            AgentEvent::ApprovalResolved { request_id, .. }
            | AgentEvent::UserInputResolved { request_id, .. } => {
                self.pending_requests.remove(request_id);
            }
            AgentEvent::TurnCompleted { .. } | AgentEvent::SessionClosed { .. } => {
                self.pending_requests.clear()
            }
            _ => {}
        }
        let Some(ts) = record.ts else { return };
        match &record.event {
            AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::UserMessage { .. },
                ..
            })
            | AgentEvent::SteerRequested { .. }
            | AgentEvent::MessageAdmitted => {
                self.last_message_at = self.last_message_at.max(Some(ts));
                if record
                    .origin
                    .unwrap_or_else(|| MessageOrigin::legacy(has_parent))
                    == MessageOrigin::Human
                {
                    self.last_human_message_at = self.last_human_message_at.max(Some(ts));
                }
            }
            AgentEvent::RunRequested => {
                self.last_run_requested_at = self.last_run_requested_at.max(Some(ts));
            }
            AgentEvent::TurnStarted { .. } => {
                self.last_run_started_at = self.last_run_started_at.max(Some(ts));
                self.failed = false;
            }
            AgentEvent::TurnCompleted { status, .. } => {
                self.last_run_completed_at = self.last_run_completed_at.max(Some(ts));
                self.failed = *status == agent::TurnStatus::Failed;
            }
            AgentEvent::ProviderStartFailed { .. } => self.failed = true,
            _ => {}
        }
    }

    pub fn has_pending_input(&self) -> bool {
        !self.pending_requests.is_empty()
    }

    pub fn has_blocking_input(&self) -> bool {
        self.pending_requests.values().any(|blocking| *blocking)
    }

    pub fn asynchronous_inputs(&self) -> Vec<String> {
        self.pending_requests
            .iter()
            .filter(|(_, blocking)| !**blocking)
            .map(|(id, _)| id.clone())
            .collect()
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

use crate::project::SessionMeta;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullRequestState {
    Open,
    Closed,
    Merged,
}

pub struct SettlementPullRequest {
    pub state: Option<PullRequestState>,
    pub terminal_at_ms: Option<u64>,
}

pub struct SettlementInput<'a> {
    pub meta: &'a SessionMeta,
    pub activity: &'a ThreadActivity,
    pub pending_input: bool,
    pub live_run: bool,
    pub completion_holding_work: bool,
    pub pending_human_message: bool,
    pub pull_requests: &'a [SettlementPullRequest],
}

/// Returns the activity stamp, in milliseconds, to record as the settlement time.
pub fn automatic_settlement_at(
    input: &SettlementInput<'_>,
    now_ms: u64,
    days: Option<f64>,
    on_merge: bool,
) -> Option<u64> {
    let meta = input.meta;
    if meta.archived_at.is_some()
        || meta.is_settled()
        || meta.settled_override.is_some()
        || meta.pinned_at.is_some()
        || meta.auto_settle_disabled_at.is_some()
        || meta.parent_session_id.is_some()
        || input.pending_input
        || input.live_run
        || input.completion_holding_work
        || input.pending_human_message
    {
        return None;
    }
    if input
        .pull_requests
        .iter()
        .any(|pr| pr.state.is_none() || pr.state == Some(PullRequestState::Open))
    {
        return None;
    }
    let activity = input.activity;
    if !activity.failed
        && activity.last_human_message_at.is_some_and(|message| {
            now_ms.abs_diff(message) <= 120_000
                && [
                    activity.last_run_requested_at,
                    activity.last_run_started_at,
                    activity.last_run_completed_at,
                ]
                .into_iter()
                .all(|run| run.is_none_or(|run| run < message))
        })
    {
        return None;
    }
    let last_activity = activity.last_activity_at();
    if let Some(pr) = input
        .pull_requests
        .iter()
        .max_by_key(|pr| pr.terminal_at_ms)
        && (pr.state == Some(PullRequestState::Closed)
            || (on_merge && pr.state == Some(PullRequestState::Merged)))
        && pr.terminal_at_ms.is_some_and(|terminal| {
            terminal
                >= meta
                    .created_at
                    .saturating_mul(1000)
                    .max(activity.last_human_message_at.unwrap_or(0))
        })
    {
        return last_activity.or(Some(meta.created_at.saturating_mul(1000)));
    }
    let days = days?;
    let last_activity = last_activity?;
    ((last_activity as f64) < now_ms as f64 - days * 86_400_000.0).then_some(last_activity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::SettledOverride;

    #[test]
    fn settlement_respects_blockers_human_anchors_and_empty_links() {
        let mut meta = SessionMeta::new(agent::ProviderKind::Codex, "/sample".into(), None);
        meta.created_at = 1;
        let activity = ThreadActivity {
            last_message_at: Some(100_000),
            last_human_message_at: Some(1_000),
            ..Default::default()
        };
        fn input<'a>(
            meta: &'a SessionMeta,
            activity: &'a ThreadActivity,
            links: &'a [SettlementPullRequest],
        ) -> SettlementInput<'a> {
            SettlementInput {
                meta,
                activity,
                pending_input: false,
                live_run: false,
                completion_holding_work: false,
                pending_human_message: false,
                pull_requests: links,
            }
        }
        let now = 4 * 86_400_000 + 100_000;
        assert_eq!(
            automatic_settlement_at(&input(&meta, &activity, &[]), now, Some(3.0), true),
            Some(100_000)
        );
        assert_eq!(
            automatic_settlement_at(&input(&meta, &activity, &[]), now, None, true),
            None
        );
        for (state, terminal, on_merge, expected) in [
            (
                Some(PullRequestState::Merged),
                Some(2_000),
                true,
                Some(100_000),
            ),
            (Some(PullRequestState::Merged), Some(2_000), false, None),
            (
                Some(PullRequestState::Closed),
                Some(2_000),
                false,
                Some(100_000),
            ),
            (Some(PullRequestState::Closed), Some(999), true, None),
            (Some(PullRequestState::Closed), None, true, None),
            (Some(PullRequestState::Open), Some(2_000), true, None),
            (None, Some(2_000), true, None),
        ] {
            let links = [SettlementPullRequest {
                state,
                terminal_at_ms: terminal,
            }];
            assert_eq!(
                automatic_settlement_at(&input(&meta, &activity, &links), now, None, on_merge),
                expected
            );
            if state.is_none() || state == Some(PullRequestState::Open) {
                assert_eq!(
                    automatic_settlement_at(
                        &input(&meta, &activity, &links),
                        now,
                        Some(1.0),
                        on_merge
                    ),
                    None
                );
            }
        }
        for blocker in [
            "archived",
            "active",
            "settled",
            "pinned",
            "disabled",
            "child",
            "input",
            "run",
            "background",
            "human",
        ] {
            let mut blocked = meta.clone();
            match blocker {
                "archived" => blocked.archived_at = Some(1),
                "active" => blocked.settled_override = Some(SettledOverride::Active),
                "settled" => blocked.settled_override = Some(SettledOverride::Settled),
                "pinned" => blocked.pinned_at = Some(1),
                "disabled" => blocked.auto_settle_disabled_at = Some(1),
                "child" => blocked.parent_session_id = Some("lead".into()),
                _ => {}
            }
            let mut candidate = input(&blocked, &activity, &[]);
            candidate.pending_input = blocker == "input";
            candidate.live_run = blocker == "run";
            candidate.completion_holding_work = blocker == "background";
            candidate.pending_human_message = blocker == "human";
            assert_eq!(
                automatic_settlement_at(&candidate, now, Some(3.0), true),
                None,
                "{blocker}"
            );
        }
        let empty = ThreadActivity::default();
        let mut candidate = input(&meta, &activity, &[]);
        candidate.activity = &empty;
        assert_eq!(
            automatic_settlement_at(&candidate, now, Some(1.0), true),
            None
        );
        let fresh = ThreadActivity {
            last_human_message_at: Some(now - 10),
            last_message_at: Some(now - 10),
            ..Default::default()
        };
        let merged = [SettlementPullRequest {
            state: Some(PullRequestState::Merged),
            terminal_at_ms: Some(now),
        }];
        candidate.activity = &fresh;
        candidate.pull_requests = &merged;
        assert_eq!(automatic_settlement_at(&candidate, now, None, true), None);
    }
}
