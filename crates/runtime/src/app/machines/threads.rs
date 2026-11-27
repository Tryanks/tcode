//! Threads one machine of the group starts on another, and the replies that
//! come back to the machine that started them.
use control_mcp::ControlOp;
use control_mcp::params::RunOp;
use tcode_core::control::ControlScope;
use tcode_protocol::{MachineCaller, MachineRequest, MachineResponse};

use super::MachinePeer;
use crate::app::control::{Answer, Caller, RemoteCaller};
use crate::app::*;

fn refused(message: impl Into<String>) -> ProtocolError {
    ProtocolError {
        code: "refused".into(),
        message: message.into(),
    }
}

fn failed(message: String) -> ProtocolError {
    ProtocolError {
        code: "control_failed".into(),
        message,
    }
}

/// What another machine's thread brings about here is done under the host's authority and
/// as an agent's, never the user's: it answers no approval and is no human activity.
fn agent_cx(cx: &HostCx) -> HostCx {
    let mut cx = cx.clone();
    cx.principal = tcode_protocol::Principal::Full;
    cx.origin = Some(MessageOrigin::Agent);
    cx.author = None;
    cx.space = None;
    cx
}

impl AppState {
    /// Run one `tcode_control` tool here for `caller`, a thread on `peer`. The transport
    /// has let the request through this machine's switches; what a machine may ask is
    /// decided here.
    pub(super) fn machine_control(
        &mut self,
        peer: &MachinePeer,
        caller: MachineCaller,
        tool: String,
        args: serde_json::Value,
        cx: &mut HostCx,
    ) -> HostTask<Result<MachineResponse, ProtocolError>> {
        let answer = (|| {
            if !self.settings.control.enabled {
                return Err(refused(
                    "tcode_control is turned off in Settings → Control on this machine",
                ));
            }
            let op = ControlOp::parse(&tool, args).map_err(failed)?;
            let caller = Caller {
                thread_id: caller.thread_id,
                project_id: None,
                space: None,
                machine: Some(RemoteCaller {
                    peer: peer.clone(),
                    title: caller.title,
                }),
            };
            let op = self.machine_may_ask(&caller, op)?;
            self.authorize_control(&caller, &op).map_err(failed)?;
            // Decided above: the call runs as a thread's call here would.
            Ok(self.dispatch_control_op(&caller, op, &mut agent_cx(cx)))
        })();
        let answer = match answer {
            Ok(answer) => answer,
            Err(error) => return cx.spawn_background(async move { Err(error) }),
        };
        cx.spawn_background(async move {
            let result = match answer {
                Answer::Now(result) => result,
                Answer::Later(task) => task.await,
            };
            result.map(MachineResponse::Control).map_err(failed)
        })
    }

    /// What a thread on another machine may ask of this one: listing projects and models,
    /// starting threads, reading and writing to the threads its machine started here
    /// (`open_to` confines the threads), and what a lead does for its own subagent.
    /// The match names every tool, so one added later is refused until it is decided here.
    fn machine_may_ask(&self, caller: &Caller, op: ControlOp) -> Result<ControlOp, ProtocolError> {
        let name = op.tool_name();
        let not_for_machines = || {
            Err(refused(format!(
                "a thread on another machine may not ask for `{name}` here"
            )))
        };
        match op {
            ControlOp::ListThreads(mut params) => {
                // A caller elsewhere has no project here.
                params.scope.get_or_insert(ControlScope::Host);
                Ok(ControlOp::ListThreads(params))
            }
            op @ (ControlOp::ListProjects(_)
            | ControlOp::GetThread(_)
            | ControlOp::Outline(_)
            | ControlOp::Read(_)
            | ControlOp::SearchInItem(_)
            | ControlOp::ListProviders(_)
            | ControlOp::StartThread(_)
            | ControlOp::SendMessage(_)
            | ControlOp::NotifyWhen(_)
            | ControlOp::CancelNotify(_)
            | ControlOp::RespondInput(_)) => Ok(op),
            ControlOp::ControlThread(params) => {
                if let Some(change) = params.changes.iter().find(|change| {
                    !matches!(
                        change.op,
                        RunOp::Interrupt
                            | RunOp::Cancel
                            | RunOp::SteerQueued { .. }
                            | RunOp::DropQueued { .. }
                    )
                }) {
                    return Err(refused(format!(
                        "a thread on another machine may only interrupt, cancel, steer a queued \
                         message or drop one here; not {}",
                        serde_json::to_value(&change.op)
                            .ok()
                            .and_then(|op| match op {
                                serde_json::Value::String(name) => Some(name),
                                serde_json::Value::Object(map) => map.keys().next().cloned(),
                                _ => None,
                            })
                            .unwrap_or_default()
                    )));
                }
                Ok(ControlOp::ControlThread(params))
            }
            ControlOp::RespondApproval(params) => {
                if !self
                    .find_meta(&params.thread_id)
                    .is_some_and(|meta| self.own_subagent(caller, &meta))
                {
                    return Err(refused(format!(
                        "thread {} is not a subagent your thread started on this machine; its \
                         approvals are the user's",
                        params.thread_id
                    )));
                }
                Ok(ControlOp::RespondApproval(params))
            }
            // Settling is what a lead does with its own subagent once it accepts the result.
            ControlOp::UpdateThreads(params) => {
                let own = params.changes.iter().all(|change| {
                    matches!(change.op, control_mcp::ThreadOp::Settle)
                        && self
                            .find_meta(&change.thread_id)
                            .is_some_and(|meta| self.own_subagent(caller, &meta))
                });
                if !own || params.dry_run == Some(true) {
                    return not_for_machines();
                }
                Ok(ControlOp::UpdateThreads(params))
            }
            ControlOp::Search(_)
            | ControlOp::FileHistory(_)
            | ControlOp::GetGit(_)
            | ControlOp::GetPullRequest(_)
            | ControlOp::GetSettings(_)
            | ControlOp::ReadTerminal(_)
            | ControlOp::ReviewCandidates(_)
            | ControlOp::SettleSelf(_)
            | ControlOp::PullRequest(_)
            | ControlOp::Git(_)
            | ControlOp::CreateProject(_)
            | ControlOp::MoveThread(_)
            | ControlOp::ReportResult(_)
            | ControlOp::Terminal(_)
            | ControlOp::DeleteThreads(_)
            | ControlOp::Rewind(_)
            | ControlOp::PatchSettings(_)
            | ControlOp::SetModelGuide(_)
            | ControlOp::ListMachines(_)
            | ControlOp::StartThreadOnMachine(_) => not_for_machines(),
        }
    }

    /// A [`MachineRequest::Callback`] or [`MachineRequest::ReportTaken`]: the
    /// answer to something this machine sent `peer`.
    pub(super) fn machine_reply(
        &mut self,
        peer: &MachinePeer,
        request: MachineRequest,
        cx: &mut HostCx,
    ) -> HostTask<Result<MachineResponse, ProtocolError>> {
        let host_cx = cx;
        let cx = &mut agent_cx(host_cx);
        let answer = match request {
            MachineRequest::Callback {
                thread_id,
                from_thread_id,
                from_title,
                text,
                report,
                state,
                watch,
            } => self.receive_callback(
                peer,
                super::remote::Callback {
                    thread_id,
                    from_thread_id,
                    from_title,
                    text,
                    report,
                    state,
                    watch,
                },
                cx,
            ),
            MachineRequest::ReportTaken { child_thread_id } => {
                self.report_taken_by_lead(peer, &child_thread_id, cx)
            }
            other => Err(ProtocolError::out_of_scope(format!(
                "{:?} is not a reply",
                other.kind()
            ))),
        };
        host_cx.spawn_background(async move { answer })
    }

    /// `peer`'s lead took the report of `child_id`, a thread it started here: the report is
    /// delivered. [`MachineRequest::ReportTaken`] does not say which report, so only one this
    /// host has sent is taken; one kept from before a restart goes again first, and `peer`
    /// answers that it took it.
    fn report_taken_by_lead(
        &mut self,
        peer: &MachinePeer,
        child_id: &str,
        cx: &mut HostCx,
    ) -> Result<MachineResponse, ProtocolError> {
        let Some(mut child) = self.find_meta(child_id).filter(|meta| {
            meta.remote_parent
                .as_ref()
                .is_some_and(|parent| parent.machine == peer.id)
        }) else {
            return Err(ProtocolError::out_of_scope(
                "no thread here was started by a thread on that machine",
            ));
        };
        let sent = self.machine_work.reports_sent.get(child_id).cloned();
        let Some(report) = child
            .expectation
            .as_ref()
            .and_then(|expectation| expectation.undelivered_id.clone())
            .filter(|report| sent.as_ref() == Some(report))
        else {
            return Ok(MachineResponse::Done);
        };
        if let Some(expectation) = child.expectation.as_mut() {
            expectation.undelivered = None;
            expectation.undelivered_id = None;
        }
        child.updated_at = cx.clock().unix_secs();
        self.store_meta(child, cx);
        self.machine_work
            .queued_there
            .remove(&super::leads::report_key(child_id, &report));
        Ok(MachineResponse::Done)
    }
}
