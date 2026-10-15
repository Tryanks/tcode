use super::*;
use tcode_core::settlement::{SettlementBlockers, SettlementDue, automatic_settlement_due};

const ACTIVITY_BATCH_SIZE: usize = 16;

impl AppState {
    pub(crate) fn start_activity(&mut self, cx: &mut HostCx) {
        self.evaluate_all_settlement(cx);
        self.load_next_activity_batch(cx);
    }

    pub(super) fn queue_activity_load(&mut self, id: &str, cx: &mut HostCx) {
        self.activity_pending.insert(id.to_owned());
        self.load_next_activity_batch(cx);
    }

    fn load_next_activity_batch(&mut self, cx: &mut HostCx) {
        if self.activity_loading || self.activity_pending.is_empty() || self.store_failed {
            return;
        }
        let ids: Vec<_> = self
            .activity_pending
            .iter()
            .take(ACTIVITY_BATCH_SIZE)
            .cloned()
            .collect();
        let revisions: Vec<_> = ids
            .iter()
            .map(|id| {
                self.activity_pending.remove(id);
                (id.clone(), self.decision_revision(id))
            })
            .collect();
        self.activity_loading = true;
        let store = self.store.clone();
        let flush = self.store_write_barrier(cx);
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let result = match flush.recv().await {
                Ok(Ok(())) => {
                    host_cx
                        .unblock(move || store.backfill_thread_activity(&ids))
                        .await
                }
                error => Err(std::io::Error::other(format!(
                    "activity write barrier failed: {error:?}"
                ))),
            };
            host_cx.enqueue(move |state, cx| {
                state.activity_loading = false;
                match result {
                    Ok(mut activities) => {
                        for (id, revision) in revisions {
                            if state.find_meta(&id).is_none() {
                                continue;
                            }
                            if state.decision_revision(&id) != revision {
                                state.activity_pending.insert(id);
                            } else if let Some(activity) = activities.remove(&id) {
                                state.thread_activity.insert(id.clone(), activity);
                                state.evaluate_thread_settlement(&id, cx);
                            }
                        }
                    }
                    Err(error) => log::warn!("thread activity backfill: {error}"),
                }
                state.load_next_activity_batch(cx);
            });
        });
    }

    pub(super) fn evaluate_all_settlement(&mut self, cx: &mut HostCx) {
        let now = now_millis();
        let projection = super::snapshots::ActivityProjection::new(self);
        let decisions: Vec<_> = self
            .sessions
            .iter()
            .map(|meta| (meta.id.clone(), self.settlement_due(meta, now, &projection)))
            .collect();
        for (id, due) in decisions {
            self.arm_settlement(&id, due, now, cx);
        }
    }

    pub(super) fn evaluate_thread_settlement(&mut self, id: &str, cx: &mut HostCx) {
        let mut next = Some(id.to_owned());
        let mut visited = HashSet::new();
        while let Some(id) = next.take() {
            if !visited.insert(id.clone()) {
                break;
            }
            let meta = self.find_meta(&id);
            next = meta
                .as_ref()
                .and_then(|meta| meta.parent_session_id.clone());
            let now = now_millis();
            let projection = super::snapshots::ActivityProjection::new(self);
            let due = meta
                .as_ref()
                .and_then(|meta| self.settlement_due(meta, now, &projection));
            self.arm_settlement(&id, due, now, cx);
        }
    }

    fn arm_settlement(&mut self, id: &str, due: Option<SettlementDue>, now: u64, cx: &mut HostCx) {
        let Some(due) = due else {
            self.settlement_timers.remove(id);
            return;
        };
        if due.due_at <= now {
            self.settlement_timers.remove(id);
            self.settle_session_at(id, due.activity_at / 1000, cx);
            return;
        }
        if self
            .settlement_timers
            .get(id)
            .is_some_and(|(deadline, _)| *deadline == due.due_at)
        {
            return;
        }
        let id = id.to_owned();
        let timer_id = id.clone();
        let host_cx = cx.clone();
        let task = cx.spawn_background(async move {
            smol::Timer::after(Duration::from_millis(due.due_at.saturating_sub(now))).await;
            host_cx.enqueue(move |state, cx| {
                if state
                    .settlement_timers
                    .get(&timer_id)
                    .is_some_and(|(deadline, _)| *deadline == due.due_at)
                {
                    state.settlement_timers.remove(&timer_id);
                    state.evaluate_thread_settlement(&timer_id, cx);
                }
            });
        });
        self.settlement_timers.insert(id, (due.due_at, task));
    }

    fn settlement_due(
        &self,
        meta: &SessionMeta,
        now: u64,
        projection: &super::snapshots::ActivityProjection<'_>,
    ) -> Option<SettlementDue> {
        let activity = self.thread_activity.get(&meta.id)?;
        let resident = self.resident(&meta.id);
        let blockers = SettlementBlockers {
            pending_input: resident.is_some_and(|session| {
                !session.timeline.pending_approvals.is_empty()
                    || session.timeline.pending_user_input.is_some()
            }),
            live_run: resident.is_some_and(|session| {
                session.preparing_worktree
                    || matches!(session.runtime, Runtime::Starting { .. })
                    || session.turn_in_flight
                    || session.delivery_in_flight.is_some()
                    || session.timeline.turn_running
            }),
            completion_holding_work: resident
                .is_some_and(|session| session.background_task_count > 0)
                || projection.children_hold(&meta.id)
                || tcode_core::pull_request::watched(&meta.pull_requests)
                    .next()
                    .is_some(),
            pending_human_message: resident.is_some_and(|session| {
                session
                    .queue
                    .iter()
                    .any(|message| message.origin == MessageOrigin::Human)
            }),
            scheduled_wake: resident.is_some_and(|session| {
                session
                    .queue
                    .iter()
                    .any(|message| message.not_before.is_some())
            }),
        };
        let project = meta
            .project_id
            .as_deref()
            .and_then(|id| self.settings.project_settlement_overrides.get(id));
        let days = project
            .and_then(|settings| settings.auto_settle_after_days)
            .unwrap_or(self.settings.auto_settle_after_days);
        let on_merge = project
            .and_then(|settings| settings.auto_settle_on_merge)
            .unwrap_or(self.settings.auto_settle_on_merge);
        automatic_settlement_due(meta, activity, blockers, now, days, on_merge)
    }
}
