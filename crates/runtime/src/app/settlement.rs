use super::*;
use tcode_core::settlement::{SettlementInput, ThreadActivity, automatic_settlement_at};

fn read_activity(
    store: &SessionStore,
    id: &str,
    has_parent: bool,
) -> std::io::Result<ThreadActivity> {
    store
        .read_events(id)
        .map(|records| ThreadActivity::fold_stored(&records, has_parent))
}

enum ManualSettlement {
    Complete(Result<tcode_protocol::CommandResponse, tcode_protocol::ProtocolError>),
    Read {
        revision: u64,
        has_parent: bool,
        flushed: smol::channel::Receiver<Result<(), String>>,
    },
}

impl AppState {
    pub(crate) fn settle_command(
        &mut self,
        id: String,
        cx: &mut HostCx,
    ) -> crate::host::HostTask<Result<tcode_protocol::CommandResponse, tcode_protocol::ProtocolError>>
    {
        let host = cx.clone();
        let store = self.store.clone();
        cx.spawn_background(async move {
            loop {
                let target = id.clone();
                let preparation = host
                    .enqueue_and_wait(move |state, cx| {
                        let command = tcode_protocol::Command::SettleSession {
                            session_id: target.clone(),
                        };
                        if let Err(error) = state.validate_command_target(&command) {
                            return ManualSettlement::Complete(Err(error));
                        }
                        if state.thread_activity.contains_key(&target) {
                            state.settle_session(&target, cx);
                            return ManualSettlement::Complete(Ok(
                                tcode_protocol::CommandResponse::Unit,
                            ));
                        }
                        ManualSettlement::Read {
                            revision: state.decision_revisions.get(&target).copied().unwrap_or(0),
                            has_parent: state
                                .find_meta(&target)
                                .unwrap()
                                .parent_session_id
                                .is_some(),
                            flushed: state.store_write_barrier(cx),
                        }
                    })
                    .await
                    .map_err(|_| settlement_host_closed())?;
                let ManualSettlement::Read {
                    revision,
                    has_parent,
                    flushed,
                } = preparation
                else {
                    let ManualSettlement::Complete(result) = preparation else {
                        unreachable!()
                    };
                    return result;
                };
                flushed
                    .recv()
                    .await
                    .map_err(|_| settlement_host_closed())?
                    .map_err(|message| tcode_protocol::ProtocolError {
                        code: "store_flush_failed".into(),
                        message,
                    })?;
                let source = store.clone();
                let target = id.clone();
                let activity = host
                    .unblock(move || read_activity(&source, &target, has_parent))
                    .await
                    .map_err(|error| tcode_protocol::ProtocolError {
                        code: "activity_read_failed".into(),
                        message: error.to_string(),
                    })?;
                let target = id.clone();
                host.enqueue_and_wait(move |state, _| {
                    if state.find_meta(&target).is_some()
                        && state.decision_revisions.get(&target).copied().unwrap_or(0) == revision
                    {
                        state.thread_activity.insert(target, activity);
                    }
                })
                .await
                .map_err(|_| settlement_host_closed())?;
            }
        })
    }

    pub(crate) fn start_settlement_sweeps(&mut self, cx: &mut HostCx) {
        self.request_settlement_sweep(cx);
    }

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
                    self.decision_revisions.get(&meta.id).copied().unwrap_or(0),
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
                            .map(|(id, parent, revision)| {
                                let activity = read_activity(&store, &id, parent);
                                (id, revision, activity)
                            })
                            .collect::<Vec<_>>()
                    })
                    .await
            } else {
                vec![]
            };
            host_cx.enqueue(move |state, cx| {
                for (id, revision, activity) in projections {
                    if state.find_meta(&id).is_none() {
                        continue;
                    }
                    if state.decision_revisions.get(&id).copied().unwrap_or(0) != revision {
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
                let by_id: HashMap<_, _> = state
                    .sessions
                    .iter()
                    .map(|meta| (meta.id.as_str(), meta))
                    .collect();
                let mut holding_parents = HashSet::new();
                for child in state
                    .sessions
                    .iter()
                    .filter(|child| child.archived_at.is_none())
                {
                    let native = child.native_subagent.is_some();
                    let holds = if native {
                        state
                            .resident(&child.id)
                            .is_some_and(|session| session.timeline.turn_running)
                    } else {
                        !child.is_settled()
                    };
                    if !holds {
                        continue;
                    }
                    let mut parent = child.parent_session_id.as_deref();
                    while let Some(id) = parent {
                        if !holding_parents.insert(id) {
                            break;
                        }
                        let owner = by_id.get(id);
                        if !native || owner.is_none_or(|meta| meta.native_subagent.is_none()) {
                            break;
                        }
                        parent = owner.and_then(|meta| meta.parent_session_id.as_deref());
                    }
                }
                let candidates: Vec<_> = state
                    .sessions
                    .iter()
                    .filter_map(|meta| {
                        let activity = state.thread_activity.get(&meta.id)?;
                        let resident = state.resident(&meta.id);
                        let (days, on_merge) =
                            state.settlement_settings(meta.project_id.as_deref());
                        let input = SettlementInput {
                            meta,
                            activity,
                            pending_input: resident.is_some_and(|session| {
                                !session.timeline.pending_approvals.is_empty()
                                    || session.timeline.pending_user_input.is_some()
                            }) || activity.has_pending_input(),
                            live_run: resident.is_some_and(|session| {
                                session.preparing_worktree
                                    || matches!(session.runtime, Runtime::Starting { .. })
                                    || session.turn_in_flight
                                    || session.delivery_in_flight.is_some()
                                    || session.timeline.turn_running
                            }),
                            // BackgroundTasksChanged currently describes plain
                            // commands; native agent work is held by its mirrors.
                            completion_holding_work: holding_parents.contains(meta.id.as_str()),
                            pending_human_message: resident.is_some_and(|session| {
                                session
                                    .queue
                                    .iter()
                                    .any(|message| message.origin == MessageOrigin::Human)
                            }),
                            // Linked PR facts are introduced by #532. Empty means no cause and no blocker.
                            pull_requests: &[],
                        };
                        automatic_settlement_at(&input, now, days, on_merge).map(|at| {
                            (
                                meta.id.clone(),
                                state.decision_revisions.get(&meta.id).copied().unwrap_or(0),
                                at,
                            )
                        })
                    })
                    .collect();
                for (id, revision, at) in candidates {
                    if state.decision_revisions.get(&id).copied().unwrap_or(0) == revision {
                        state.settle_session_at(&id, at / 1000, cx);
                    }
                }
                log::info!(
                    "settlement sweep: {} threads, {} activity projections, {} ms",
                    state.sessions.len(),
                    state.thread_activity.len(),
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

    fn settlement_settings(&self, project: Option<&str>) -> (Option<f64>, bool) {
        let overrides = project.and_then(|id| self.settings.project_settlement_overrides.get(id));
        (
            overrides
                .and_then(|settings| settings.auto_settle_after_days)
                .unwrap_or(self.settings.auto_settle_after_days),
            overrides
                .and_then(|settings| settings.auto_settle_on_merge)
                .unwrap_or(self.settings.auto_settle_on_merge),
        )
    }
}

fn settlement_host_closed() -> tcode_protocol::ProtocolError {
    tcode_protocol::ProtocolError {
        code: "transport_closed".into(),
        message: "Host stopped before settling the thread.".into(),
    }
}
