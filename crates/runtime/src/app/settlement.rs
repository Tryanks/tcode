use super::*;
use tcode_core::settlement::{SettlementBlockers, ThreadActivity, automatic_settlement_at};

impl AppState {
    pub(crate) fn start_settlement_sweeps(&mut self, cx: &mut HostCx) {
        self.request_settlement_sweep(cx);
    }

    /// Evaluate every thread: project activity for threads that have none
    /// from the tail of their logs, then settle the ones that are due. Repeats
    /// one minute after it drains.
    pub(super) fn request_settlement_sweep(&mut self, cx: &mut HostCx) {
        if self.settlement_sweep_running {
            self.settlement_sweep_pending = true;
            return;
        }
        if self.store_failed {
            return;
        }
        self.settlement_sweep_running = true;
        self.settlement_timer_generation += 1;
        let started = Instant::now();
        let missing: Vec<_> = self
            .sessions
            .iter()
            .filter(|meta| {
                meta.archived_at.is_none() && !self.thread_activity.contains_key(&meta.id)
            })
            .map(|meta| {
                (
                    meta.id.clone(),
                    meta.parent_session_id.is_some(),
                    self.decision_revision(&meta.id),
                )
            })
            .collect();
        let store = self.store.clone();
        let (flushed, flush) = smol::channel::bounded(1);
        self.enqueue_store_write(StoreWrite::Flush(flushed), cx);
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let projections = if matches!(flush.recv().await, Ok(Ok(()))) {
                host_cx
                    .unblock(move || {
                        missing
                            .into_iter()
                            .map(|(id, has_parent, revision)| {
                                let activity = read_activity_tail(&store, &id).map(|records| {
                                    ThreadActivity::fold_stored(&records, has_parent)
                                });
                                (id, revision, activity)
                            })
                            .collect::<Vec<_>>()
                    })
                    .await
            } else {
                vec![]
            };
            let projected = started.elapsed();
            host_cx.enqueue(move |state, cx| {
                for (id, revision, activity) in projections {
                    if state.find_meta(&id).is_none() || state.thread_activity.contains_key(&id) {
                        continue;
                    }
                    // A record appended while the log was read is missing from it.
                    if state.decision_revision(&id) != revision {
                        state.settlement_sweep_pending = true;
                        continue;
                    }
                    match activity {
                        Ok(activity) => {
                            state.thread_activity.insert(id, activity);
                        }
                        Err(error) => log::warn!("settlement activity read for {id}: {error}"),
                    }
                }
                let now = now_millis();
                let due: Vec<_> = state
                    .sessions
                    .iter()
                    .filter_map(|meta| state.settlement_due(meta, now))
                    .collect();
                for (id, at) in due {
                    state.settle_session_at(&id, at / 1000, cx);
                }
                log::info!(
                    "settlement sweep: {} threads, {} activity projections, read {} ms, total {} ms",
                    state.sessions.len(),
                    state.thread_activity.len(),
                    projected.as_millis(),
                    started.elapsed().as_millis()
                );
                state.settlement_sweep_running = false;
                if std::mem::take(&mut state.settlement_sweep_pending) {
                    state.request_settlement_sweep(cx);
                    return;
                }
                let generation = state.settlement_timer_generation;
                let timer_cx = cx.clone();
                HostCx::spawn_detached(cx, async move {
                    smol::Timer::after(Duration::from_secs(60)).await;
                    timer_cx.enqueue(move |state, cx| {
                        if state.settlement_timer_generation == generation {
                            state.request_settlement_sweep(cx);
                        }
                    });
                });
            });
        });
    }

    /// Settle one thread now if it is due; for changes that concern only it.
    pub(super) fn evaluate_thread_settlement(&mut self, id: &str, cx: &mut HostCx) {
        let due = self
            .sessions
            .iter()
            .find(|meta| meta.id == id)
            .and_then(|meta| self.settlement_due(meta, now_millis()));
        if let Some((id, at)) = due {
            self.settle_session_at(&id, at / 1000, cx);
        }
    }

    fn settlement_due(&self, meta: &SessionMeta, now: u64) -> Option<(String, u64)> {
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
                || self.children_hold(&meta.id),
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
        automatic_settlement_at(meta, activity, blockers, now, days, on_merge)
            .map(|at| (meta.id.clone(), at))
    }
}

/// The end of a log from its latest record that moves an activity clock:
/// read backwards a page at a time until a page holds one. The clocks need
/// no earlier record.
fn read_activity_tail(store: &SessionStore, id: &str) -> std::io::Result<Vec<SessionEventRecord>> {
    let mut low = store.next_row(id)?;
    let mut records = Vec::new();
    while low > 0 {
        let from = low.saturating_sub(history::TAIL_ROWS);
        let page = store.read_rows(id, from..low)?.records;
        let found = page
            .iter()
            .any(|record| ThreadActivity::moves_clock(&record.event));
        records.splice(0..0, page);
        low = from;
        if found {
            break;
        }
    }
    Ok(records)
}
