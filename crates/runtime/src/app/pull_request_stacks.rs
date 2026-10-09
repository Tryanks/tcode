//! The host's writes to a native stack: an asynchronous merge it follows until GitHub answers or
//! five minutes pass, and a rebase it runs layer by layer. Either one is the stack's operation,
//! recorded with every thread that shows the stack so each client, and a restarted host, knows
//! it; one runs per stack at a time.

use super::pull_requests::failure;
use super::*;
use tcode_core::pull_request::{
    self, PullRequestKey, PullRequestStackOperation, PullRequestStackState, PullRequestState,
    StackOperationKind, StackRebaseLayer, StackRebaseStep,
};
use tcode_protocol::{
    CommandResponse, ProtocolError, PullRequestAction, PullRequestActionResult as Outcome,
    PullRequestRejection, PullRequestStackHead, RuntimeToast,
};
use tcode_services::github::stack_actions::{
    MergeStatus, MergeSubmission, merge_outcome, poll_schedule,
};

/// An unconfirmed merge is kept this long when no sync reads what became of it.
const UNCONFIRMED_KEPT_SECS: u64 = 24 * 60 * 60;

/// A native stack, by host, repository and number.
pub(super) type StackKey = (String, String, u64);

fn answer(outcome: Outcome) -> Result<CommandResponse, ProtocolError> {
    Ok(CommandResponse::PullRequestAction(outcome))
}

fn stack_key(key: &PullRequestKey, stack: u64) -> StackKey {
    (key.host.clone(), key.repository.clone(), stack)
}

fn shows_stack(meta: &SessionMeta, (host, repository, number): &StackKey) -> bool {
    meta.pull_requests.iter().any(|link| {
        link.visible()
            && link.key.host == *host
            && link.key.repository == *repository
            && matches!(&link.stack, PullRequestStackState::Native(stack) if stack.number == *number)
    })
}

impl AppState {
    /// The stack's operation as the threads record it.
    fn stack_operation_record(&self, stack: &StackKey) -> Option<PullRequestStackOperation> {
        self.sessions.iter().find_map(|meta| {
            self.find_meta(&meta.id)?
                .pull_request_operations
                .into_iter()
                .find(|operation| operation.is_for(&stack.0, &stack.1, stack.2))
        })
    }

    /// Records the stack's operation with every thread that shows the stack, or clears it from
    /// every thread that has it.
    fn put_stack_operation(
        &mut self,
        stack: &StackKey,
        operation: Option<PullRequestStackOperation>,
        cx: &mut HostCx,
    ) {
        let ids: Vec<_> = self.sessions.iter().map(|meta| meta.id.clone()).collect();
        for id in ids {
            let Some(mut meta) = self.find_meta(&id) else {
                continue;
            };
            let held = meta
                .pull_request_operations
                .iter()
                .any(|record| record.is_for(&stack.0, &stack.1, stack.2));
            let shows = meta.archived_at.is_none() && shows_stack(&meta, stack);
            if !held && !(shows && operation.is_some()) {
                continue;
            }
            meta.pull_request_operations
                .retain(|record| !record.is_for(&stack.0, &stack.1, stack.2));
            if let Some(operation) = operation.clone().filter(|_| shows) {
                meta.pull_request_operations.push(operation);
            }
            self.save_pull_request_meta(meta, cx);
        }
    }

    /// The thread a stack's result opens into, and the base its layers merge into.
    fn stack_context(&self, stack: &StackKey) -> Option<(String, String)> {
        self.sessions.iter().find_map(|meta| {
            let meta = self.find_meta(&meta.id)?;
            meta.pull_requests
                .iter()
                .find_map(|link| match &link.stack {
                    PullRequestStackState::Native(native)
                        if link.visible()
                            && link.key.host == stack.0
                            && link.key.repository == stack.1
                            && native.number == stack.2 =>
                    {
                        Some((meta.id.clone(), native.base.clone()))
                    }
                    _ => None,
                })
        })
    }

    fn report_stack(
        &self,
        stack: &StackKey,
        target: u64,
        layers: Vec<u64>,
        result: Outcome,
        late: bool,
        cx: &mut HostCx,
    ) {
        let Some((session_id, base)) = self.stack_context(stack) else {
            return;
        };
        emit_runtime(
            cx,
            RuntimeEvent::Toast(RuntimeToast::PullRequestStack {
                session_id,
                target: PullRequestKey::new(&stack.0, &stack.1, target),
                stack: stack.2,
                layers,
                base,
                result,
                late,
            }),
        );
    }

    fn sync_stack_layers(&mut self, stack: &StackKey, layers: &[u64], cx: &mut HostCx) {
        for number in layers {
            self.request_pull_request_sync(PullRequestKey::new(&stack.0, &stack.1, *number), cx);
        }
    }

    /// A merge or rebase of the stack `key` is a layer of. Only a pull request the thread links
    /// writes to its stack, one write per stack at a time.
    pub(super) fn run_stack_action(
        &mut self,
        session_id: &str,
        key: PullRequestKey,
        action: PullRequestAction,
        cx: &mut HostCx,
    ) -> HostTask<Result<CommandResponse, ProtocolError>> {
        let refuse =
            |rejection| cx.spawn_background(async move { answer(Outcome::Rejected(rejection)) });
        let links = self
            .find_meta(session_id)
            .map(|meta| meta.pull_requests)
            .unwrap_or_default();
        if !links.iter().any(|link| link.visible() && link.key == key) {
            return refuse(PullRequestRejection::NotLinked);
        }
        let Some(native) = pull_request::native_stack(&links, &key) else {
            return refuse(PullRequestRejection::StackUnknown);
        };
        let (PullRequestAction::MergeStack { stack, .. }
        | PullRequestAction::RebaseStack { stack, .. }) = &action
        else {
            return refuse(PullRequestRejection::Invalid);
        };
        if native.number != *stack {
            return refuse(PullRequestRejection::StackChanged);
        }
        let stack = stack_key(&key, *stack);
        let busy = self.pull_requests.stack_writes.contains(&stack)
            || self
                .stack_operation_record(&stack)
                .is_some_and(|operation| match operation.kind {
                    StackOperationKind::MergeUnconfirmed { checked, .. } => !checked,
                    _ => true,
                });
        if busy {
            return refuse(PullRequestRejection::OperationRunning);
        }
        self.pull_requests.stack_writes.insert(stack.clone());
        match action {
            PullRequestAction::MergeStack { heads, method, .. } => {
                self.merge_stack(stack, key, heads, method, cx)
            }
            PullRequestAction::RebaseStack { heads, .. } => {
                self.rebase_stack(stack, key, heads, cx)
            }
            _ => unreachable!("matched above"),
        }
    }

    fn merge_stack(
        &mut self,
        stack: StackKey,
        key: PullRequestKey,
        heads: Vec<PullRequestStackHead>,
        method: tcode_core::pull_request::PullRequestMergeMethod,
        cx: &mut HostCx,
    ) -> HostTask<Result<CommandResponse, ProtocolError>> {
        let reads = self.pull_requests.reads.clone();
        let submitting = key.clone();
        let number = stack.2;
        let task = cx.unblock(move || reads.merge_stack(&submitting, number, &heads, method));
        let host = cx.clone();
        cx.spawn_background(async move {
            let submission = task.await;
            let outcome = host
                .enqueue_and_wait(move |state, cx| {
                    state.pull_requests.stack_writes.remove(&stack);
                    match submission {
                        MergeSubmission::Done(outcome) => {
                            if !matches!(outcome, Outcome::Rejected(_)) {
                                state.request_pull_request_sync(key, cx);
                            }
                            outcome
                        }
                        MergeSubmission::Following {
                            id,
                            adopted,
                            layers,
                        } => {
                            let operation = PullRequestStackOperation {
                                host: stack.0.clone(),
                                repository: stack.1.clone(),
                                stack: stack.2,
                                started_at: now_secs(),
                                kind: StackOperationKind::Merging {
                                    id: id.clone(),
                                    target: key.number,
                                    layers,
                                    adopted,
                                },
                            };
                            state.put_stack_operation(&stack, Some(operation.clone()), cx);
                            state.follow_stack_merge(operation, cx);
                            Outcome::Pending { id, adopted }
                        }
                    }
                })
                .await
                .map_err(|_| failure("Host closed."))?;
            answer(outcome)
        })
    }

    /// Asks GitHub what became of the operation on the poll schedule from its submission, and
    /// once more when a restarted host finds its schedule already past. Still pending at the
    /// deadline, it is recorded as unconfirmed and never submitted again.
    fn follow_stack_merge(&mut self, operation: PullRequestStackOperation, cx: &mut HostCx) {
        let StackOperationKind::Merging { id, target, .. } = &operation.kind else {
            return;
        };
        let (id, started_at) = (id.clone(), operation.started_at);
        let key = PullRequestKey::new(&operation.host, &operation.repository, *target);
        let stack = (
            operation.host.clone(),
            operation.repository.clone(),
            operation.stack,
        );
        let reads = self.pull_requests.reads.clone();
        let host = cx.clone();
        let task = cx.spawn_background(async move {
            let elapsed = now_secs().saturating_sub(started_at);
            let mut due: Vec<_> = poll_schedule()
                .into_iter()
                .filter(|offset| *offset >= elapsed)
                .collect();
            if due.is_empty() {
                due.push(elapsed);
            }
            let mut ended = None;
            for offset in due {
                let wait = (started_at + offset).saturating_sub(now_secs());
                smol::Timer::after(Duration::from_secs(wait)).await;
                let (reads, key, id) = (reads.clone(), key.clone(), id.clone());
                match host.unblock(move || reads.merge_status(&key, &id)).await {
                    Ok(MergeStatus::Pending { .. }) => {}
                    Ok(status) => {
                        ended = Some(status);
                        break;
                    }
                    // An unanswered ask is no answer; the next one may be.
                    Err(error) => log::debug!("stack merge status skipped: {error}"),
                }
            }
            host.enqueue(move |state, cx| state.end_stack_merge(&stack, &id, ended, cx));
        });
        self.pull_requests.workers.push(task);
    }

    fn end_stack_merge(
        &mut self,
        stack: &StackKey,
        id: &str,
        ended: Option<MergeStatus>,
        cx: &mut HostCx,
    ) {
        let Some(mut operation) = self.stack_operation_record(stack) else {
            return;
        };
        let StackOperationKind::Merging {
            id: following,
            target,
            layers,
            ..
        } = &operation.kind
        else {
            return;
        };
        if following != id {
            return;
        }
        let (target, layers) = (*target, layers.clone());
        let result = match ended.as_ref().and_then(merge_outcome) {
            Some(result) => {
                self.put_stack_operation(stack, None, cx);
                result
            }
            None => {
                operation.kind = StackOperationKind::MergeUnconfirmed {
                    id: id.to_owned(),
                    target,
                    layers: layers.clone(),
                    checked: false,
                };
                self.put_stack_operation(stack, Some(operation), cx);
                Outcome::MergeUnconfirmed { id: id.to_owned() }
            }
        };
        self.sync_stack_layers(stack, &layers, cx);
        self.report_stack(stack, target, layers, result, false, cx);
    }

    fn rebase_stack(
        &mut self,
        stack: StackKey,
        key: PullRequestKey,
        heads: Vec<PullRequestStackHead>,
        cx: &mut HostCx,
    ) -> HostTask<Result<CommandResponse, ProtocolError>> {
        let reads = self.pull_requests.reads.clone();
        let planning = key.clone();
        let number = stack.2;
        let task = cx.unblock(move || reads.plan_stack_rebase(&planning, number, &heads));
        let host = cx.clone();
        cx.spawn_background(async move {
            let plan = match task.await {
                Ok(plan) => plan,
                Err(rejection) => {
                    let _ = host
                        .enqueue_and_wait(move |state, _| {
                            state.pull_requests.stack_writes.remove(&stack)
                        })
                        .await;
                    return answer(Outcome::Rejected(rejection));
                }
            };
            host.enqueue_and_wait(move |state, cx| state.start_stack_rebase(stack, key, plan, cx))
                .await
                .map_err(|_| failure("Host closed."))?;
            answer(Outcome::RebaseStarted)
        })
    }

    fn start_stack_rebase(
        &mut self,
        stack: StackKey,
        key: PullRequestKey,
        plan: tcode_services::github::stack_actions::RebasePlan,
        cx: &mut HostCx,
    ) {
        let layers: Vec<_> = plan
            .layers
            .iter()
            .map(|layer| StackRebaseLayer {
                number: layer.number,
                branch: layer.branch.clone(),
                step: StackRebaseStep::Waiting,
            })
            .collect();
        let numbers: Vec<_> = layers.iter().map(|layer| layer.number).collect();
        self.put_stack_operation(
            &stack,
            Some(PullRequestStackOperation {
                host: stack.0.clone(),
                repository: stack.1.clone(),
                stack: stack.2,
                started_at: now_secs(),
                kind: StackOperationKind::Rebasing { layers },
            }),
            cx,
        );
        let (progress, steps) = smol::channel::unbounded();
        let run = cx.unblock(move || {
            plan.run(|index, step| {
                let _ = progress.send_blocking((index, step));
            })
        });
        let host = cx.clone();
        let task = cx.spawn_background(async move {
            while let Ok((index, step)) = steps.recv().await {
                let stack = stack.clone();
                host.enqueue(move |state, cx| state.step_stack_rebase(&stack, index, step, cx));
            }
            let outcome = run.await;
            host.enqueue(move |state, cx| {
                state.pull_requests.stack_writes.remove(&stack);
                state.put_stack_operation(&stack, None, cx);
                state.sync_stack_layers(&stack, &numbers, cx);
                state.report_stack(&stack, key.number, numbers, outcome, false, cx);
            });
        });
        self.pull_requests.workers.push(task);
    }

    fn step_stack_rebase(
        &mut self,
        stack: &StackKey,
        index: usize,
        step: StackRebaseStep,
        cx: &mut HostCx,
    ) {
        let Some(mut operation) = self.stack_operation_record(stack) else {
            return;
        };
        let StackOperationKind::Rebasing { layers } = &mut operation.kind else {
            return;
        };
        let Some(layer) = layers.get_mut(index) else {
            return;
        };
        layer.step = step;
        self.put_stack_operation(stack, Some(operation), cx);
    }

    /// After a restart: a merge is followed again from where its schedule stands; a rebase
    /// cannot be, since its Git process ended with the host, so its layers are read again.
    pub(super) fn resume_stack_operations(&mut self, cx: &mut HostCx) {
        let mut seen = HashSet::new();
        let operations: Vec<_> = self
            .sessions
            .iter()
            .filter_map(|meta| self.find_meta(&meta.id))
            .flat_map(|meta| meta.pull_request_operations)
            .filter(|operation| {
                seen.insert((
                    operation.host.clone(),
                    operation.repository.clone(),
                    operation.stack,
                ))
            })
            .collect();
        for operation in operations {
            let stack = (
                operation.host.clone(),
                operation.repository.clone(),
                operation.stack,
            );
            match &operation.kind {
                StackOperationKind::Merging { .. } => self.follow_stack_merge(operation, cx),
                StackOperationKind::Rebasing { layers } => {
                    let numbers: Vec<_> = layers.iter().map(|layer| layer.number).collect();
                    self.put_stack_operation(&stack, None, cx);
                    self.sync_stack_layers(&stack, &numbers, cx);
                }
                StackOperationKind::MergeUnconfirmed { .. } => {}
            }
        }
    }

    /// What a sync read of `key` says of an unconfirmed merge it is the target of: merged or
    /// closed ends it, and still open lets the stack's writes go again, a later attempt
    /// following GitHub's operation should it still run. Past a day it is dropped unread.
    pub(super) fn reconcile_unconfirmed_merges(
        &mut self,
        key: Option<(&PullRequestKey, PullRequestState)>,
        cx: &mut HostCx,
    ) {
        let mut seen = HashSet::new();
        let unconfirmed: Vec<_> = self
            .sessions
            .iter()
            .filter_map(|meta| self.find_meta(&meta.id))
            .flat_map(|meta| meta.pull_request_operations)
            .filter(|operation| {
                matches!(operation.kind, StackOperationKind::MergeUnconfirmed { .. })
                    && seen.insert((
                        operation.host.clone(),
                        operation.repository.clone(),
                        operation.stack,
                    ))
            })
            .collect();
        for mut operation in unconfirmed {
            let stack = (
                operation.host.clone(),
                operation.repository.clone(),
                operation.stack,
            );
            let StackOperationKind::MergeUnconfirmed {
                target,
                layers,
                checked,
                ..
            } = &mut operation.kind
            else {
                continue;
            };
            if now_secs().saturating_sub(operation.started_at) >= UNCONFIRMED_KEPT_SECS {
                self.put_stack_operation(&stack, None, cx);
                continue;
            }
            let Some((_, state)) = key.filter(|(key, _)| {
                key.host == stack.0 && key.repository == stack.1 && key.number == *target
            }) else {
                continue;
            };
            match state {
                PullRequestState::Merged => {
                    let (target, layers) = (*target, layers.clone());
                    self.put_stack_operation(&stack, None, cx);
                    self.report_stack(&stack, target, layers, Outcome::Applied, true, cx);
                }
                PullRequestState::Closed => self.put_stack_operation(&stack, None, cx),
                PullRequestState::Open if !*checked => {
                    *checked = true;
                    self.put_stack_operation(&stack, Some(operation), cx);
                }
                PullRequestState::Open => {}
            }
        }
    }
}
