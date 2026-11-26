//! A thread here working on another machine of the group: its `tcode_control` calls that
//! name that machine, the threads it starts there, and what comes back from them.
use control_mcp::params::RunOp;
use control_mcp::{
    BatchResponse, ControlOp, ControlThreadParams, ControlThreadResponse, ListMachinesParams,
    Outcome, RunChange, RunResult, StartThreadOnMachineParams, StartThreadResponse,
};
use tcode_client::machines::{MACHINE_UNREACHABLE, MachineInfo, MachineLink, NO_GROUP};
use tcode_core::control::{
    ControlWatch, RemoteChild, RemoteChildOp, RemoteChildState, WatchEvent, action,
};
use tcode_protocol::{MachineCaller, MachineRequest, MachineResponse};

use super::{MachinePeer, permission_mode_of};
use crate::app::control::{Answer, Caller, finish_on_host};
use crate::app::*;

/// A [`MachineRequest::Callback`] as it arrives.
pub(super) struct Callback {
    pub(super) thread_id: String,
    pub(super) from_thread_id: String,
    pub(super) from_title: String,
    pub(super) text: String,
    pub(super) report: Option<String>,
    pub(super) state: Option<RemoteChildState>,
    pub(super) watch: Option<String>,
}

/// What a call answered from another machine, and the machine.
type Forwarded = Result<serde_json::Value, ProtocolError>;

/// `thread_id` on `machine`, as a link and in a message: the form without a machine is
/// this machine's.
fn elsewhere(thread_id: &str, machine: &str) -> String {
    format!("{thread_id}@{machine}")
}

fn machine_named(info: &MachineInfo) -> String {
    info.label.clone().unwrap_or_else(|| info.name.clone())
}

/// The other machine's refusal or failure, as the calling thread reads it.
fn from_machine(info: &MachineInfo, error: ProtocolError) -> String {
    format!(
        "machine \"{}\" ({}): {} ({})",
        machine_named(info),
        info.id,
        error.message,
        error.code
    )
}

/// `answer` with the machine that gave it named in it.
fn naming_machine(answer: serde_json::Value, info: &MachineInfo) -> serde_json::Value {
    let machine = serde_json::json!({ "id": info.id, "name": machine_named(info) });
    match answer {
        serde_json::Value::Object(mut fields) => {
            fields.insert("machine".into(), machine);
            serde_json::Value::Object(fields)
        }
        other => serde_json::json!({ "machine": machine, "answer": other }),
    }
}

/// The action key a write records, by what it did.
fn run_action(op: &RunOp) -> &'static str {
    match op {
        RunOp::Interrupt => action::INTERRUPT,
        RunOp::Cancel => action::CANCEL,
        RunOp::SteerQueued { .. } => action::STEER_QUEUED,
        RunOp::DropQueued { .. } => action::DROP_QUEUED,
        RunOp::Retry { .. } => action::RETRY,
        RunOp::Compact => action::COMPACT,
        RunOp::SetModel { .. } => action::SET_MODEL,
        RunOp::SetOption { .. } => action::SET_OPTION,
        RunOp::Fork | RunOp::ForkAt { .. } => action::FORK,
    }
}

impl AppState {
    /// Why `caller` may not reach another machine, if it may not.
    fn may_reach_machines(&self, caller: &Caller) -> Result<MachineLink, String> {
        if !self.settings.control.machines {
            return Err(
                "refused: Settings → Control does not let this machine's threads reach the \
                 other machines of its group"
                    .into(),
            );
        }
        if caller.space.is_some() {
            return Err("refused: a turn in a space reaches no other machine".into());
        }
        self.machine_link()
            .cloned()
            .map_err(|_| "refused: this machine is in no group".to_owned())
    }

    /// Run `op` on `machine` for `caller`, and `finish` with its answer on the host thread
    /// whether or not the caller still waits: what the other machine did is recorded here
    /// even when the call was cancelled meanwhile.
    fn forward(
        &mut self,
        caller: &Caller,
        machine: String,
        op: ControlOp,
        request_id: Option<String>,
        cx: &mut HostCx,
        finish: impl FnOnce(
            &mut AppState,
            &MachineInfo,
            Forwarded,
            &mut HostCx,
        ) -> Result<serde_json::Value, String>
        + Send
        + 'static,
    ) -> Answer {
        let link = match self.may_reach_machines(caller) {
            Ok(link) => link,
            Err(refusal) => return Answer::Now(Err(refusal)),
        };
        let request = MachineRequest::Control {
            caller: self.machine_caller(&caller.thread_id, request_id),
            tool: op.tool_name().to_owned(),
            args: op.args(),
        };
        let host_cx = cx.clone();
        let (sent, answer) = smol::channel::bounded(1);
        HostCx::spawn_detached(cx, async move {
            let info = match member(&link, &machine).await {
                Ok(info) => info,
                Err(refusal) => {
                    let _ = sent.send(Err(refusal)).await;
                    return;
                }
            };
            let answer = link.call(&info.id, request).await;
            let finished = finish_on_host(host_cx, move |state, cx| {
                let reached = answer.is_ok();
                let left = answer
                    .as_ref()
                    .is_err_and(|error| error.code == tcode_client::machines::NOT_MEMBER);
                let answer = answer.and_then(|response| match response {
                    MachineResponse::Control(value) => Ok(value),
                    other => Err(ProtocolError {
                        code: "unexpected_reply".into(),
                        message: format!("the machine answered {other:?}"),
                    }),
                });
                let finished = finish(state, &info, answer, cx);
                state.machine_answered(&info.id, reached, left, true, cx);
                finished.map(|value| naming_machine(value, &info))
            })
            .await;
            let _ = sent.send(finished).await;
        });
        Answer::Later(cx.spawn_background(async move {
            answer
                .recv()
                .await
                .unwrap_or_else(|_| Err("the host stopped before the call finished".into()))
        }))
    }

    /// One of the tools that take `machine`, run on that machine.
    pub(in crate::app) fn forward_control_op(
        &mut self,
        caller: &Caller,
        machine: String,
        op: ControlOp,
        cx: &mut HostCx,
    ) -> Answer {
        let plain = |_: &mut AppState, info: &MachineInfo, answer: Forwarded, _: &mut HostCx| {
            answer.map_err(|error| from_machine(info, error))
        };
        let thread = caller.thread_id.clone();
        match op {
            op @ (ControlOp::ListProjects(_)
            | ControlOp::ListThreads(_)
            | ControlOp::GetThread(_)
            | ControlOp::Outline(_)
            | ControlOp::Read(_)
            | ControlOp::SearchInItem(_)
            | ControlOp::ListProviders(_)) => self.forward(caller, machine, op, None, cx, plain),
            ControlOp::SendMessage(params) => {
                let request_id = params.request_id.clone();
                let queues = !matches!(params.mode, control_mcp::SendMode::Schedule { .. });
                let key = match params.mode {
                    control_mcp::SendMode::Queue => action::SEND_QUEUED,
                    control_mcp::SendMode::Steer => action::SEND_STEERED,
                    control_mcp::SendMode::Schedule { .. } => action::SEND_SCHEDULED,
                };
                self.forward(
                    caller,
                    machine,
                    ControlOp::SendMessage(params),
                    request_id,
                    cx,
                    move |state, info, answer, cx| {
                        let value = answer.map_err(|error| from_machine(info, error))?;
                        if let Ok(batch) = serde_json::from_value::<BatchResponse>(value.clone()) {
                            for result in batch
                                .results
                                .iter()
                                .filter(|result| result.outcome == Outcome::Applied)
                            {
                                state.audit_elsewhere(&thread, info, &result.target, key, None, cx);
                                // A message to one's own subagent opens a new report it owes.
                                if queues {
                                    state.remote_child_heard(&thread, &info.id, &result.target, cx);
                                }
                            }
                        }
                        Ok(value)
                    },
                )
            }
            ControlOp::ControlThread(params) => {
                self.forward_control_thread(caller, machine, params, cx)
            }
            ControlOp::NotifyWhen(params) => {
                let request_id = params.request_id.clone();
                let events = params.events.clone();
                self.forward(
                    caller,
                    machine,
                    ControlOp::NotifyWhen(params),
                    request_id,
                    cx,
                    move |state, info, answer, cx| {
                        let value = answer.map_err(|error| from_machine(info, error))?;
                        state.watching_elsewhere(&thread, info, &value, events, cx);
                        Ok(value)
                    },
                )
            }
            ControlOp::CancelNotify(params) => self.forward(
                caller,
                machine,
                ControlOp::CancelNotify(params),
                None,
                cx,
                move |state, info, answer, cx| {
                    let value = answer.map_err(|error| from_machine(info, error))?;
                    let removed: Vec<String> = value["removed"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|id| id.as_str().map(str::to_owned))
                        .collect();
                    if !removed.is_empty() {
                        state.change_meta(&thread, cx, |meta| {
                            meta.control_watches.retain(|watch| {
                                watch.machine.as_deref() != Some(info.id.as_str())
                                    || !removed.contains(&watch.id)
                            });
                        });
                        let detail = removed.join(", ");
                        state.audit_elsewhere(
                            &thread,
                            info,
                            "",
                            action::CANCEL_NOTIFY,
                            Some(detail),
                            cx,
                        );
                    }
                    Ok(value)
                },
            ),
            ControlOp::RespondInput(params) => {
                let target = params.thread_id.clone();
                self.forward(
                    caller,
                    machine,
                    ControlOp::RespondInput(params),
                    None,
                    cx,
                    move |state, info, answer, cx| {
                        let value = answer.map_err(|error| from_machine(info, error))?;
                        let detail = value["request_id"].as_str().map(str::to_owned);
                        state.audit_elsewhere(
                            &thread,
                            info,
                            &target,
                            action::RESPOND_INPUT,
                            detail,
                            cx,
                        );
                        Ok(value)
                    },
                )
            }
            ControlOp::RespondApproval(params) => {
                let target = params.thread_id.clone();
                self.forward(
                    caller,
                    machine,
                    ControlOp::RespondApproval(params),
                    None,
                    cx,
                    move |state, info, answer, cx| {
                        let value = answer.map_err(|error| from_machine(info, error))?;
                        let detail = value["approval_id"].as_str().map(str::to_owned);
                        state.audit_elsewhere(
                            &thread,
                            info,
                            &target,
                            action::RESPOND_APPROVAL,
                            detail,
                            cx,
                        );
                        Ok(value)
                    },
                )
            }
            other => Answer::Now(Err(format!(
                "`{}` runs on this machine only: omit machine",
                other.tool_name()
            ))),
        }
    }

    /// `control_thread` on another machine. A cancel or interrupt of this thread's own
    /// subagent there that cannot reach it now is kept and sent when it can.
    fn forward_control_thread(
        &mut self,
        caller: &Caller,
        machine: String,
        params: ControlThreadParams,
        cx: &mut HostCx,
    ) -> Answer {
        let thread = caller.thread_id.clone();
        let changes = params.changes.clone();
        let request_id = params.request_id.clone();
        self.forward(
            caller,
            machine,
            ControlOp::ControlThread(params),
            request_id,
            cx,
            move |state, info, answer, cx| match answer {
                Ok(value) => {
                    if let Ok(response) =
                        serde_json::from_value::<ControlThreadResponse>(value.clone())
                    {
                        for (change, result) in changes.iter().zip(&response.results) {
                            if result.result.outcome != Outcome::Applied {
                                continue;
                            }
                            state.audit_elsewhere(
                                &thread,
                                info,
                                &change.thread_id,
                                run_action(&change.op),
                                None,
                                cx,
                            );
                            if change.op == RunOp::Cancel {
                                state.remote_child_done(
                                    &thread,
                                    &info.id,
                                    &change.thread_id,
                                    RemoteChildOp::Cancel,
                                    cx,
                                );
                            }
                        }
                    }
                    Ok(value)
                }
                Err(error) if error.code == MACHINE_UNREACHABLE => {
                    let results = changes
                        .into_iter()
                        .map(|change| state.keep_for_later(&thread, info, change, &error, cx))
                        .collect();
                    serde_json::to_value(ControlThreadResponse { results })
                        .map_err(|error| error.to_string())
                }
                Err(error) => Err(from_machine(info, error)),
            },
        )
    }

    /// One change of a `control_thread` call that could not reach `info`'s machine.
    fn keep_for_later(
        &mut self,
        lead: &str,
        info: &MachineInfo,
        change: RunChange,
        error: &ProtocolError,
        cx: &mut HostCx,
    ) -> RunResult {
        let op = match change.op {
            RunOp::Cancel => Some(RemoteChildOp::Cancel),
            RunOp::Interrupt => Some(RemoteChildOp::Interrupt),
            _ => None,
        };
        let own = self.find_meta(lead).is_some_and(|meta| {
            meta.remote_children
                .iter()
                .any(|child| child.machine == info.id && child.thread_id == change.thread_id)
        });
        let result = |outcome, reason: String| RunResult {
            result: control_mcp::BatchResult {
                target: change.thread_id.clone(),
                outcome,
                reason: Some(reason),
                side_effects: None,
            },
            new_thread_id: None,
        };
        match op {
            Some(op) if own => {
                self.change_meta(lead, cx, |meta| {
                    for child in &mut meta.remote_children {
                        if child.machine == info.id && child.thread_id == change.thread_id {
                            // A cancel stops it anyway: an interrupt asked later adds nothing.
                            if !(child.pending == Some(RemoteChildOp::Cancel)
                                && op == RemoteChildOp::Interrupt)
                            {
                                child.pending = Some(op);
                            }
                        }
                    }
                });
                result(
                    Outcome::Applied,
                    format!(
                        "queued: machine \"{}\" cannot be reached now; it is sent when it can be",
                        machine_named(info)
                    ),
                )
            }
            _ => result(Outcome::Failed, from_machine(info, error.clone())),
        }
    }

    pub(in crate::app) fn op_list_machines(
        &mut self,
        caller: &Caller,
        _params: ListMachinesParams,
        cx: &mut HostCx,
    ) -> Answer {
        let link = match self.may_reach_machines(caller) {
            Ok(link) => link,
            Err(refusal) => return Answer::Now(Err(refusal)),
        };
        Answer::Later(cx.spawn_background(async move {
            let machines = link.machines(true).await.map_err(no_group)?;
            let machines: Vec<serde_json::Value> = machines
                .iter()
                .map(|info| {
                    serde_json::json!({
                        "id": info.id,
                        "name": machine_named(info),
                        "host_name": info.name,
                        "this_machine": info.this_machine,
                        "reachable": info.reachable,
                        "accepts_threads": info.accepts_threads,
                        "accepts_projects": info.accepts_projects,
                        "same_version": info.protocol_ok,
                    })
                })
                .collect();
            Ok(serde_json::json!({ "machines": machines }))
        }))
    }

    pub(in crate::app) fn op_start_thread_on_machine(
        &mut self,
        caller: &Caller,
        params: StartThreadOnMachineParams,
        cx: &mut HostCx,
    ) -> Answer {
        if params.request_id.trim().is_empty() {
            return Answer::Now(Err("request_id must not be empty".into()));
        }
        let subagent = params.subagent.unwrap_or(false);
        if subagent
            && self
                .find_meta(&caller.thread_id)
                .is_some_and(|meta| meta.collaboration_id.is_some())
        {
            return Answer::Now(Err(
                "refused: this thread runs under a collaboration, whose members are all on \
                 this machine; start its subagents here with start_thread"
                    .into(),
            ));
        }
        let thread = caller.thread_id.clone();
        let request_id = params.request_id.clone();
        let machine = params.machine.clone();
        self.forward(
            caller,
            machine,
            ControlOp::StartThread(params.start_thread()),
            Some(request_id.clone()),
            cx,
            move |state, info, answer, cx| {
                let value = answer.map_err(|error| from_machine(info, error))?;
                let started: StartThreadResponse = serde_json::from_value(value)
                    .map_err(|error| format!("the machine's answer was not understood: {error}"))?;
                if subagent {
                    state.remember_remote_child(&thread, info, &started, &request_id, cx);
                }
                if started.created {
                    state.audit_elsewhere(
                        &thread,
                        info,
                        &started.thread_id,
                        action::START_THREAD,
                        Some(started.title.clone()),
                        cx,
                    );
                }
                Ok(serde_json::json!({
                    "thread_id": started.thread_id,
                    "link": format!("tcode-thread://{}", elsewhere(&started.thread_id, &info.id)),
                    "created": started.created,
                    "title": started.title,
                    "cwd": started.cwd,
                    "subagent": started.subagent,
                    "worktree_branch": started.worktree_branch,
                }))
            },
        )
    }

    /// Keep on `lead` the subagent it started on `info`'s machine; once per request.
    fn remember_remote_child(
        &mut self,
        lead: &str,
        info: &MachineInfo,
        started: &StartThreadResponse,
        request_id: &str,
        cx: &mut HostCx,
    ) {
        let now = cx.clock().unix_secs();
        self.change_meta(lead, cx, |meta| {
            if meta.remote_children.iter().any(|child| {
                child.machine == info.id
                    && (child.request_id == request_id || child.thread_id == started.thread_id)
            }) {
                return;
            }
            meta.remote_children.push(RemoteChild {
                machine: info.id.clone(),
                thread_id: started.thread_id.clone(),
                title: started.title.clone(),
                request_id: request_id.to_owned(),
                subagent: true,
                state: RemoteChildState::Running,
                updated_at: now,
                pending: None,
                report_taken: None,
                report_taken_unsent: false,
            });
        });
    }

    /// Record in `lead`'s own log a write its call made on `info`'s machine, which records
    /// it in the target thread there.
    fn audit_elsewhere(
        &mut self,
        lead: &str,
        info: &MachineInfo,
        target: &str,
        key: &str,
        detail: Option<String>,
        cx: &mut HostCx,
    ) {
        let Some(meta) = self.find_meta(lead) else {
            return;
        };
        let target_title = meta
            .remote_children
            .iter()
            .find(|child| child.machine == info.id && child.thread_id == target)
            .map(|child| child.title.clone())
            .unwrap_or_else(|| target.to_owned());
        let on = if target.is_empty() {
            machine_named(info)
        } else {
            format!("{target_title} — {}", machine_named(info))
        };
        let action = tcode_core::control::ControlAction {
            actor_thread_id: lead.to_owned(),
            actor_title: meta.title,
            actor_machine: None,
            action: key.to_owned(),
            detail: Some(match detail {
                Some(detail) => format!("{on} — {detail}"),
                None => on,
            }),
        };
        self.record_event(lead, &AgentEvent::ControlAction(action), cx);
    }

    /// `lead`'s subagent on `machine` was sent a message: it runs again, and owes a report.
    fn remote_child_heard(&mut self, lead: &str, machine: &str, thread: &str, cx: &mut HostCx) {
        let now = cx.clock().unix_secs();
        self.change_meta(lead, cx, |meta| {
            for child in &mut meta.remote_children {
                if child.machine == machine
                    && child.thread_id == thread
                    && child.subagent
                    && child.state != RemoteChildState::MachineLeft
                {
                    child.state = RemoteChildState::Running;
                    child.updated_at = now;
                }
            }
        });
    }

    /// `op` on `lead`'s subagent on `machine` was carried out there.
    fn remote_child_done(
        &mut self,
        lead: &str,
        machine: &str,
        thread: &str,
        op: RemoteChildOp,
        cx: &mut HostCx,
    ) {
        let now = cx.clock().unix_secs();
        self.change_meta(lead, cx, |meta| {
            for child in &mut meta.remote_children {
                if child.machine == machine && child.thread_id == thread {
                    if child.pending == Some(op) {
                        child.pending = None;
                    }
                    let state = match op {
                        RemoteChildOp::Cancel => Some(RemoteChildState::Cancelled),
                        RemoteChildOp::Settle => Some(RemoteChildState::Settled),
                        RemoteChildOp::Interrupt => None,
                    };
                    if let Some(state) = state {
                        child.state = state;
                        child.updated_at = now;
                    }
                }
            }
        });
    }

    /// The lead settles its children: those on other machines that reported are settled
    /// there, now or when their machine can be reached.
    pub(in crate::app) fn settle_remote_children(&mut self, lead: &str, cx: &mut HostCx) {
        self.change_meta(lead, cx, |meta| {
            for child in &mut meta.remote_children {
                if child.subagent
                    && matches!(
                        child.state,
                        RemoteChildState::Reported | RemoteChildState::Failed
                    )
                    && child.pending.is_none()
                {
                    child.pending = Some(RemoteChildOp::Settle);
                }
            }
        });
        let machines: BTreeSet<String> = self
            .find_meta(lead)
            .into_iter()
            .flat_map(|meta| meta.remote_children)
            .filter(|child| child.pending == Some(RemoteChildOp::Settle))
            .map(|child| child.machine)
            .collect();
        for machine in machines {
            self.send_remote_child_work(Some(&machine), cx);
        }
    }

    /// What this machine's leads still owe their subagents on `machine`, or on every
    /// machine: a cancel, interrupt or settle that could not reach it, and the word that a
    /// report was taken.
    pub(super) fn send_remote_child_work(&mut self, machine: Option<&str>, cx: &mut HostCx) {
        let due: Vec<(String, RemoteChild)> = self
            .sessions
            .iter()
            .flat_map(|meta| {
                meta.remote_children
                    .iter()
                    .filter(|child| {
                        machine.is_none_or(|machine| child.machine == machine)
                            && child.state != RemoteChildState::MachineLeft
                            && (child.pending.is_some() || child.report_taken_unsent)
                    })
                    .map(|child| (meta.id.clone(), child.clone()))
            })
            .collect();
        for (lead, child) in due {
            if child.report_taken_unsent
                && let Some(report) = child.report_taken.clone()
            {
                self.send_report_taken(&lead, &child, report, cx);
            }
            if let Some(op) = child.pending {
                self.send_remote_child_op(&lead, &child, op, cx);
            }
        }
    }

    fn send_report_taken(
        &mut self,
        lead: &str,
        child: &RemoteChild,
        report: String,
        cx: &mut HostCx,
    ) {
        let lead = lead.to_owned();
        let (machine, thread) = (child.machine.clone(), child.thread_id.clone());
        self.call_machine(
            &child.machine,
            Some(format!("taken:{machine}:{thread}:{report}")),
            MachineRequest::ReportTaken {
                child_thread_id: thread.clone(),
            },
            false,
            cx,
            move |state, answer, cx| {
                // Answered either way, the other machine has nothing more to keep for it.
                let heard = match &answer {
                    Ok(_) => true,
                    Err(error) => error.code == "out_of_scope",
                };
                if heard {
                    state.change_meta(&lead, cx, |meta| {
                        for child in &mut meta.remote_children {
                            if child.machine == machine
                                && child.thread_id == thread
                                && child.report_taken.as_deref() == Some(report.as_str())
                            {
                                child.report_taken_unsent = false;
                            }
                        }
                    });
                }
            },
        );
    }

    fn send_remote_child_op(
        &mut self,
        lead: &str,
        child: &RemoteChild,
        op: RemoteChildOp,
        cx: &mut HostCx,
    ) {
        let (tool, args) = match op {
            RemoteChildOp::Cancel | RemoteChildOp::Interrupt => (
                "control_thread",
                serde_json::json!({
                    "changes": [{
                        "thread_id": child.thread_id,
                        "op": if op == RemoteChildOp::Cancel { "cancel" } else { "interrupt" },
                    }],
                }),
            ),
            RemoteChildOp::Settle => (
                "update_threads",
                serde_json::json!({
                    "changes": [{ "thread_id": child.thread_id, "op": "settle" }],
                }),
            ),
        };
        let request = MachineRequest::Control {
            caller: self.machine_caller(lead, None),
            tool: tool.into(),
            args,
        };
        let lead = lead.to_owned();
        let (machine, thread) = (child.machine.clone(), child.thread_id.clone());
        self.call_machine(
            &child.machine,
            Some(format!("op:{machine}:{thread}")),
            request,
            false,
            cx,
            move |state, answer, cx| {
                let outcome = answer.as_ref().ok().and_then(|response| match response {
                    MachineResponse::Control(value) => {
                        value["results"][0]["outcome"].as_str().map(str::to_owned)
                    }
                    _ => None,
                });
                match (outcome.as_deref(), &answer) {
                    (Some("applied"), _) => {
                        state.remote_child_done(&lead, &machine, &thread, op, cx);
                    }
                    // The other machine will not do it, now or later: nothing is kept.
                    (Some(_), _) => state.change_meta(&lead, cx, |meta| {
                        for child in &mut meta.remote_children {
                            if child.machine == machine
                                && child.thread_id == thread
                                && child.pending == Some(op)
                            {
                                child.pending = None;
                            }
                        }
                    }),
                    (None, Err(error)) if error.code != MACHINE_UNREACHABLE => {
                        state.change_meta(&lead, cx, |meta| {
                            for child in &mut meta.remote_children {
                                if child.machine == machine
                                    && child.thread_id == thread
                                    && child.pending == Some(op)
                                {
                                    child.pending = None;
                                }
                            }
                        });
                    }
                    // Kept, and sent again on the next contact with that machine.
                    (None, _) => {}
                }
            },
        );
    }

    /// Record on `lead` the watch `answer` says `info`'s machine keeps for it.
    fn watching_elsewhere(
        &mut self,
        lead: &str,
        info: &MachineInfo,
        answer: &serde_json::Value,
        events: Vec<WatchEvent>,
        cx: &mut HostCx,
    ) {
        let (Some(id), Some(threads)) = (answer["id"].as_str(), answer["thread_ids"].as_array())
        else {
            return;
        };
        let thread_ids: Vec<String> = threads
            .iter()
            .filter_map(|id| id.as_str().map(str::to_owned))
            .collect();
        let watch = ControlWatch {
            id: id.to_owned(),
            thread_ids: thread_ids.clone(),
            events: events.into_iter().collect(),
            space: None,
            machine: Some(info.id.clone()),
        };
        let mut added = false;
        self.change_meta(lead, cx, |meta| {
            if !meta
                .control_watches
                .iter()
                .any(|held| held.id == watch.id && held.machine == watch.machine)
            {
                meta.control_watches.push(watch);
                added = true;
            }
        });
        if added {
            for target in &thread_ids {
                self.audit_elsewhere(
                    lead,
                    info,
                    target,
                    action::NOTIFY_WHEN,
                    Some(id.to_owned()),
                    cx,
                );
            }
        }
    }

    /// `lead` no longer keeps `watch`, which a machine of its group keeps for it: that
    /// machine drops it when it can be reached now, and otherwise its firing finds no
    /// watch here and is dropped then.
    pub(in crate::app) fn unwatch_on_machine(
        &mut self,
        lead: &str,
        watch: ControlWatch,
        cx: &mut HostCx,
    ) {
        let Some(machine) = watch.machine.clone() else {
            return;
        };
        let request = MachineRequest::Control {
            caller: self.machine_caller(lead, None),
            tool: "cancel_notify".into(),
            args: serde_json::json!({ "ids": [watch.id] }),
        };
        self.call_machine(&machine, None, request, true, cx, |_, _, _| {});
    }

    /// Thread `thread` here as the other machine is told of it. Its permission mode is the
    /// one it runs under now, as a space's policy reads it.
    fn machine_caller(&self, thread: &str, request_id: Option<String>) -> MachineCaller {
        let meta = self.current_meta(thread);
        MachineCaller {
            thread_id: thread.to_owned(),
            title: meta.map(|meta| meta.title.clone()).unwrap_or_default(),
            permission_mode: meta.and_then(permission_mode_of),
            request_id,
        }
    }

    /// The text a thread on `peer` sends a thread here: a report, approval request or
    /// question of the subagent it started for that thread, or the firing of a watch that
    /// thread put on it.
    pub(super) fn receive_callback(
        &mut self,
        peer: &MachinePeer,
        callback: Callback,
        cx: &mut HostCx,
    ) -> Result<MachineResponse, ProtocolError> {
        let out_of_scope = || {
            Err(ProtocolError::out_of_scope(
                "no thread here waits on that thread of yours",
            ))
        };
        let Some(lead) = self
            .find_meta(&callback.thread_id)
            .filter(|meta| meta.archived_at.is_none())
        else {
            return out_of_scope();
        };
        let child = lead
            .remote_children
            .iter()
            .find(|child| child.machine == peer.id && child.thread_id == callback.from_thread_id)
            .cloned();
        // Its message names the thread as this machine names a thread there.
        let text = callback.text.replacen(
            &format!("thread {} (", callback.from_thread_id),
            &format!("thread {} (", elsewhere(&callback.from_thread_id, &peer.id)),
            1,
        );
        if let Some(id) = &callback.watch {
            let watched = lead.control_watches.iter().any(|watch| {
                watch.id == *id
                    && watch.machine.as_deref() == Some(peer.id.as_str())
                    && watch.thread_ids.contains(&callback.from_thread_id)
            });
            if !watched {
                return out_of_scope();
            }
            let space = lead
                .control_watches
                .iter()
                .find(|watch| watch.id == *id)
                .and_then(|watch| watch.space.clone());
            self.change_meta(&lead.id, cx, |meta| {
                for watch in &mut meta.control_watches {
                    if watch.id == *id && watch.machine.as_deref() == Some(peer.id.as_str()) {
                        watch
                            .thread_ids
                            .retain(|thread| *thread != callback.from_thread_id);
                    }
                }
                meta.control_watches
                    .retain(|watch| !watch.thread_ids.is_empty());
            });
            let mut watch_cx = cx.clone();
            watch_cx.space = space;
            self.deliver_orchestrate_callback_to_parent(&lead.id, text, &mut watch_cx);
            return Ok(MachineResponse::Done);
        }
        let Some(child) = child else {
            return out_of_scope();
        };
        let Some(report) = callback.report else {
            // A subagent archived or deleted there says only that it will not report.
            if let Some(state) = callback.state {
                let now = cx.clock().unix_secs();
                self.change_meta(&lead.id, cx, |meta| {
                    for kept in &mut meta.remote_children {
                        if kept.machine == peer.id && kept.thread_id == child.thread_id {
                            kept.state = state;
                            kept.updated_at = now;
                        }
                    }
                });
            }
            if !text.is_empty() {
                self.deliver_orchestrate_callback_to_parent(&lead.id, text, cx);
            }
            return Ok(MachineResponse::Done);
        };
        if child.report_taken.as_deref() == Some(report.as_str()) {
            // Its lead took it before: the other machine has yet to hear so.
            self.change_meta(&lead.id, cx, |meta| {
                for kept in &mut meta.remote_children {
                    if kept.machine == peer.id && kept.thread_id == child.thread_id {
                        kept.report_taken_unsent = true;
                    }
                }
            });
            self.send_remote_child_work(Some(&peer.id), cx);
            return Ok(MachineResponse::Done);
        }
        let remote = Some((peer.id.clone(), report));
        let queued = self.resident(&lead.id).is_some_and(|session| {
            session
                .queue
                .iter()
                .flat_map(|message| &message.reports)
                .chain(
                    session
                        .pending_steers
                        .iter()
                        .flat_map(|steer| &steer.reports),
                )
                .any(|handover| {
                    handover.child == callback.from_thread_id && handover.remote == remote
                })
        });
        if queued {
            return Ok(MachineResponse::Done);
        }
        let now = cx.clock().unix_secs();
        self.change_meta(&lead.id, cx, |meta| {
            for kept in &mut meta.remote_children {
                if kept.machine == peer.id && kept.thread_id == child.thread_id {
                    if let Some(state) = callback.state {
                        kept.state = state;
                    }
                    kept.title = callback.from_title.clone();
                    kept.updated_at = now;
                }
            }
        });
        self.deliver_callback(
            &lead.id,
            text.clone(),
            Some(ReportHandover {
                child: callback.from_thread_id,
                text,
                remote,
            }),
            cx,
        );
        Ok(MachineResponse::Done)
    }

    /// The lead's harness took `reports`, or the user dropped them: the machines of those
    /// that came from another machine hear so.
    pub(in crate::app) fn reports_taken(&mut self, reports: &[ReportHandover], cx: &mut HostCx) {
        let machines: BTreeSet<String> = reports
            .iter()
            .filter_map(|report| report.remote.as_ref().map(|(machine, _)| machine.clone()))
            .collect();
        for machine in machines {
            self.send_remote_child_work(Some(&machine), cx);
        }
    }
}

/// `machine` as a member of this machine's group, other than this machine.
async fn member(link: &MachineLink, machine: &str) -> Result<MachineInfo, String> {
    let machines = link.machines(false).await.map_err(no_group)?;
    let info = machines
        .into_iter()
        .find(|info| info.id == machine)
        .ok_or_else(|| {
            format!(
                "refused: {machine} is not a machine of this machine's group; list_machines \
                 lists them"
            )
        })?;
    if info.this_machine {
        return Err(format!(
            "{machine} is this machine: omit machine to run the call here"
        ));
    }
    Ok(info)
}

fn no_group(error: ProtocolError) -> String {
    if error.code == NO_GROUP {
        "refused: this machine is in no group".into()
    } else {
        error.message
    }
}
