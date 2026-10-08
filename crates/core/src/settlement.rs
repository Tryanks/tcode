//! Event-derived activity and thread lifecycle policy. All activity clocks use Unix milliseconds.

use agent::{AgentEvent, ItemContent, ThreadItem, TurnStatus};

use crate::project::SessionMeta;
use crate::pull_request::PullRequestState;
use crate::session::{MessageOrigin, StoredEvent};

/// How long a user-role message waits for its run before it stops blocking.
/// The bound is absolute so clock skew either way cannot block a thread forever.
const QUEUED_TURN_START_GRACE_MS: u64 = 2 * 60 * 1000;

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

    /// A recent user-role message no run has picked up yet, unless the latest run failed.
    fn queued_turn_start(&self, now_ms: u64) -> bool {
        let Some(message_at) = self.last_message_at else {
            return false;
        };
        !self.failed
            && now_ms.abs_diff(message_at) <= QUEUED_TURN_START_GRACE_MS
            && [self.last_run_started_at, self.last_run_completed_at]
                .into_iter()
                .all(|run| run.is_none_or(|run| run < message_at))
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

/// The activity stamp, in milliseconds, to record as the settlement time:
/// when the thread's latest terminal pull request settles it, or when it has
/// been inactive for longer than `days`.
pub fn automatic_settlement_at(
    meta: &SessionMeta,
    activity: &ThreadActivity,
    blockers: SettlementBlockers,
    now_ms: u64,
    days: Option<f64>,
    on_merge: bool,
) -> Option<u64> {
    let mut latest: Option<(PullRequestState, Option<u64>)> = None;
    for link in meta.pull_requests.iter().filter(|link| link.visible()) {
        let snapshot = link.snapshot.as_ref()?;
        let terminal_at = match snapshot.state {
            PullRequestState::Open => return None,
            PullRequestState::Merged => snapshot.merged_at.as_deref(),
            PullRequestState::Closed => snapshot.closed_at.as_deref(),
        }
        .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
        .and_then(|at| u64::try_from(at.timestamp_millis()).ok());
        if latest.is_none_or(|(_, at)| terminal_at > at) {
            latest = Some((snapshot.state, terminal_at));
        }
    }
    if meta.archived_at.is_some()
        || meta.settled_override.is_some()
        || meta.is_settled()
        || meta.auto_settle_disabled_at.is_some()
        || meta.parent_session_id.is_some()
        || blockers.any()
        || activity.queued_turn_start(now_ms)
    {
        return None;
    }
    let last_activity = activity.last_activity_at();
    // Only the human resumes a thread; agent and server messages do not hold a finished PR open.
    let anchor = activity
        .last_human_message_at
        .unwrap_or(0)
        .max(meta.created_at.saturating_mul(1000));
    if let Some((state, Some(terminal_at))) = latest
        && (state == PullRequestState::Closed || on_merge)
        && terminal_at >= anchor
    {
        return Some(last_activity.unwrap_or(meta.created_at.saturating_mul(1000)));
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
    fn inactivity_settles_at_the_last_activity_unless_blocked() {
        let meta = SessionMeta::new(agent::ProviderKind::Codex, "/sample".into(), None);
        let activity = ThreadActivity {
            last_message_at: Some(100_000),
            last_run_completed_at: Some(90_000),
            ..Default::default()
        };
        let three_days = 3 * 86_400_000;
        let settle = |meta: &SessionMeta, activity: &ThreadActivity, blockers, now| {
            automatic_settlement_at(meta, activity, blockers, now, Some(3.0), true)
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
            automatic_settlement_at(&meta, &activity, blockers, now, None, true),
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

    const HOUR: u64 = 3_600_000;
    const NOW: u64 = 1_790_000_000_000;

    fn record(origin: Option<MessageOrigin>, ts: u64, event: AgentEvent) -> StoredEvent {
        StoredEvent {
            origin,
            author: None,
            ts: Some(ts),
            event,
            elided: None,
        }
    }

    fn message(origin: MessageOrigin, ts: u64) -> StoredEvent {
        record(
            Some(origin),
            ts,
            AgentEvent::ItemCompleted(ThreadItem {
                id: format!("message-{ts}"),
                parent_item_id: None,
                content: ItemContent::UserMessage {
                    text: "Sample request".into(),
                    context_len: None,
                    attachments: vec![],
                },
            }),
        )
    }

    fn run(started: u64, completed: u64) -> [StoredEvent; 2] {
        [
            record(
                None,
                started,
                AgentEvent::TurnStarted {
                    turn_id: format!("turn-{started}"),
                },
            ),
            record(
                None,
                completed,
                AgentEvent::TurnCompleted {
                    turn_id: format!("turn-{started}"),
                    status: TurnStatus::Completed,
                    usage: None,
                },
            ),
        ]
    }

    /// A human request at -5h whose run ended at -4h.
    fn worked() -> Vec<StoredEvent> {
        let mut records = vec![message(MessageOrigin::Human, NOW - 5 * HOUR)];
        records.extend(run(NOW - 5 * HOUR + 1, NOW - 4 * HOUR));
        records
    }

    /// A thread created at -10h with one link per entry: `None` is unsynced.
    fn thread(links: &[Option<(PullRequestState, Option<u64>)>]) -> SessionMeta {
        use crate::pull_request::{
            Mergeability, PullRequestKey, PullRequestSnapshot, PullRequestSource, link_pull_request,
        };
        let mut meta = SessionMeta::new(agent::ProviderKind::Codex, "/sample".into(), None);
        meta.created_at = (NOW - 10 * HOUR) / 1000;
        for (index, link) in links.iter().enumerate() {
            let number = index as u64 + 1;
            link_pull_request(
                &mut meta.pull_requests,
                PullRequestKey::new("github.com", "sample/project", number),
                format!("https://github.com/sample/project/pull/{number}"),
                PullRequestSource::Agent,
                1,
                true,
            );
            meta.pull_requests[index].snapshot = link.map(|(state, at)| {
                let at = at.map(|at| {
                    chrono::DateTime::from_timestamp_millis(at as i64)
                        .unwrap()
                        .to_rfc3339()
                });
                PullRequestSnapshot {
                    state,
                    title: format!("Change {number}"),
                    head_branch: format!("change-{number}"),
                    base_branch: "main".into(),
                    is_draft: false,
                    updated_at: "2026-10-08T00:00:00Z".into(),
                    synced_at: 1,
                    closed_at: at.clone(),
                    merged_at: (state == PullRequestState::Merged).then_some(at).flatten(),
                    author: None,
                    additions: 1,
                    deletions: 1,
                    changed_files: 1,
                    review_decision: None,
                    checks_state: None,
                    mergeability: Mergeability::Unknown,
                }
            });
        }
        meta
    }

    fn settle_at(
        meta: &SessionMeta,
        records: &[StoredEvent],
        now: u64,
        on_merge: bool,
    ) -> Option<u64> {
        let activity = ThreadActivity::fold_stored(records, false);
        automatic_settlement_at(
            meta,
            &activity,
            SettlementBlockers::default(),
            now,
            Some(3.0),
            on_merge,
        )
    }

    #[test]
    fn the_latest_terminal_pull_request_settles_at_the_last_activity() {
        use PullRequestState::{Closed, Merged, Open};
        let records = worked();
        let aged = NOW + 3 * 86_400_000;
        let worked_at = Some(NOW - 4 * HOUR);
        let merged = thread(&[Some((Merged, Some(NOW - 3 * HOUR)))]);
        assert_eq!(settle_at(&merged, &records, NOW, true), worked_at);
        assert_eq!(
            settle_at(&merged, &records, NOW, false),
            None,
            "a merge settles only with the merge setting"
        );
        assert_eq!(
            settle_at(&merged, &records, aged, false),
            worked_at,
            "inactivity still settles"
        );
        let closed = thread(&[Some((Closed, Some(NOW - 3 * HOUR)))]);
        assert_eq!(settle_at(&closed, &records, NOW, false), worked_at);
        assert_eq!(
            settle_at(&closed, &[], NOW, false),
            Some(NOW - 10 * HOUR),
            "a thread that never ran settles at its creation"
        );
        for (blocking, label) in [
            (
                thread(&[Some((Closed, Some(NOW - 3 * HOUR))), Some((Open, None))]),
                "open",
            ),
            (
                thread(&[Some((Closed, Some(NOW - 3 * HOUR))), None]),
                "unsynced",
            ),
        ] {
            assert_eq!(settle_at(&blocking, &records, NOW, true), None, "{label}");
            assert_eq!(
                settle_at(&blocking, &records, aged, true),
                None,
                "an {label} link blocks inactivity too"
            );
        }
        let mut dismissed = thread(&[Some((Closed, Some(NOW - 3 * HOUR))), Some((Open, None))]);
        let key = dismissed.pull_requests[1].key.clone();
        crate::pull_request::unlink_pull_request(&mut dismissed.pull_requests, &key);
        assert_eq!(
            settle_at(&dismissed, &records, NOW, true),
            worked_at,
            "a dismissed tombstone is not a link"
        );
        let latest_merged = thread(&[
            Some((Closed, Some(NOW - 4 * HOUR + 1))),
            Some((Merged, Some(NOW - 3 * HOUR))),
        ]);
        assert_eq!(settle_at(&latest_merged, &records, NOW, true), worked_at);
        assert_eq!(
            settle_at(&latest_merged, &records, NOW, false),
            None,
            "an older closed link does not stand in for the latest merged one"
        );
    }

    #[test]
    fn only_a_later_human_message_keeps_a_terminal_pull_request_open() {
        let closed = thread(&[Some((PullRequestState::Closed, Some(NOW - 3 * HOUR)))]);
        let mut resumed = worked();
        resumed.push(message(MessageOrigin::Human, NOW - 2 * HOUR));
        resumed.extend(run(NOW - 2 * HOUR + 1, NOW - HOUR));
        assert_eq!(settle_at(&closed, &resumed, NOW, true), None);
        for origin in [MessageOrigin::Agent, MessageOrigin::Server] {
            let mut woken = worked();
            woken.push(message(origin, NOW - 2 * HOUR));
            woken.extend(run(NOW - 2 * HOUR + 1, NOW - HOUR));
            assert_eq!(
                settle_at(&closed, &woken, NOW, true),
                Some(NOW - HOUR),
                "{origin:?}"
            );
        }
    }

    #[test]
    fn a_fresh_user_message_holds_settlement_until_a_run_takes_it() {
        let closed = thread(&[Some((PullRequestState::Closed, Some(NOW - 3 * HOUR)))]);
        let mut queued = worked();
        queued.push(message(MessageOrigin::Server, NOW - 60_000));
        assert_eq!(settle_at(&closed, &queued, NOW, true), None);
        assert_eq!(
            settle_at(&closed, &queued, NOW + 61_000, true),
            Some(NOW - 60_000),
            "the grace lasts two minutes"
        );
        let mut started = queued.clone();
        started.push(record(
            None,
            NOW - 30_000,
            AgentEvent::TurnStarted {
                turn_id: "taken".into(),
            },
        ));
        assert_eq!(settle_at(&closed, &started, NOW, true), Some(NOW - 30_000));
        let mut failed = worked();
        failed.push(record(
            None,
            NOW - 90_000,
            AgentEvent::ProviderStartFailed {
                error: "sample failure".into(),
            },
        ));
        failed.push(message(MessageOrigin::Agent, NOW - 60_000));
        assert_eq!(settle_at(&closed, &failed, NOW, true), Some(NOW - 60_000));
    }
}
