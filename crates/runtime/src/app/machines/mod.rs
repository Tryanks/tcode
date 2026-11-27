//! What this machine answers the other machines of its group, and its way to
//! them. Requests arrive as [`tcode_protocol::Query::Machine`] from a
//! [`tcode_protocol::Principal::Machine`], after the transport has checked
//! the machine is a member and that this machine's switches take the kind of
//! request; requests go out through the [`MachineLink`] the composition root
//! gave the host.
mod arrivals;
mod leads;
#[cfg(test)]
mod move_tests;
pub(crate) mod moves;
#[cfg(test)]
mod real_group;
mod remote;
#[cfg(test)]
mod tests;
mod threads;

use tcode_client::machines::{MachineLink, NO_GROUP, NOT_MEMBER};
use tcode_core::control::{RemoteChild, RemoteChildState};
use tcode_protocol::{MachineRequest, MachineResponse};

use super::*;

/// The machine of the group a request came from, as the transport admitted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MachinePeer {
    pub id: String,
    pub name: String,
}

/// How often this machine sends again what the other machines of its group have not taken.
/// A machine may be asleep or away for days, and nothing else would tell this one it is
/// back when neither side has anything new for the other; a machine that comes back sooner
/// hears it on the first exchange with this one, in either direction.
const SEND_AGAIN_EVERY: Duration = Duration::from_secs(10 * 60);

/// What this machine still sends the other machines of its group, beyond what thread
/// metadata keeps of it.
#[derive(Default)]
pub(crate) struct MachineWork {
    /// What is being sent now, by what it is, so a second reason to send it does not send
    /// it twice; with whether such a reason came meanwhile, which looks again once the
    /// first send is answered: the machine may have come back while that send was failing.
    in_flight: HashMap<String, bool>,
    /// The reports of this machine's children that their lead's machine has queued since
    /// this host started. They wait there for the lead's harness to take them, so only the
    /// slow check sends them again, in case that machine stopped before its lead took them.
    queued_there: HashSet<String>,
    /// The report of each child here this host last sent its lead on another machine.
    reports_sent: HashMap<String, String>,
    check: Option<HostTask<()>>,
    watching: bool,
}

/// The subagents `lead` started on other machines that still run.
pub(crate) fn running_remote_children(lead: &SessionMeta) -> impl Iterator<Item = &RemoteChild> {
    lead.remote_children
        .iter()
        .filter(|child| child.subagent && child.state == RemoteChildState::Running)
}

impl AppState {
    pub(crate) fn attach_machine_link(&mut self, link: Option<MachineLink>) {
        self.machine_link = link;
    }

    /// This machine's way to the other machines of its group; [`NO_GROUP`]
    /// where the composition gave the host none.
    pub(crate) fn machine_link(&self) -> Result<&MachineLink, ProtocolError> {
        self.machine_link.as_ref().ok_or_else(|| ProtocolError {
            code: NO_GROUP.into(),
            message: "this machine has no way to the machines of a group".into(),
        })
    }

    pub(crate) fn handle_machine_request(
        &mut self,
        peer: &MachinePeer,
        request: MachineRequest,
        cx: &mut HostCx,
    ) -> HostTask<Result<MachineResponse, ProtocolError>> {
        // A machine pings this one as it starts and on its slow check: what it had queued of
        // this machine's reports may have gone with a restart, so they go again.
        if matches!(request, MachineRequest::Ping) {
            self.forget_reports_queued_on(&peer.id);
        }
        self.send_machine_work(Some(&peer.id), false, cx);
        match request {
            MachineRequest::Ping => cx.spawn_background(async { Ok(MachineResponse::Pong) }),
            MachineRequest::Control { caller, tool, args } => {
                self.machine_control(peer, caller, tool, args, cx)
            }
            request @ (MachineRequest::Callback { .. } | MachineRequest::ReportTaken { .. }) => {
                self.machine_reply(peer, request, cx)
            }
            request @ (MachineRequest::MovePreflight { .. }
            | MachineRequest::MoveBegin { .. }
            | MachineRequest::MoveChunk { .. }
            | MachineRequest::MoveCommit { .. }
            | MachineRequest::MoveAbort { .. }
            | MachineRequest::MoveStatus { .. }) => self.machine_move(peer, request, cx),
        }
    }

    /// Send `request` to `machine` and hand its answer to `done` on the host thread. `key`
    /// names what is sent, so it is not sent twice at once. An answer from a machine that
    /// counts as contact with it (`contact`) sends what waits for that machine; what that
    /// sending sends does not count, so nothing goes back and forth on its own.
    pub(crate) fn call_machine(
        &mut self,
        machine: &str,
        key: Option<String>,
        request: MachineRequest,
        contact: bool,
        cx: &mut HostCx,
        done: impl FnOnce(&mut AppState, Result<MachineResponse, ProtocolError>, &mut HostCx)
        + Send
        + 'static,
    ) {
        let link = match self.machine_link() {
            Ok(link) => link.clone(),
            Err(error) => return done(self, Err(error), cx),
        };
        if let Some(key) = &key {
            if let Some(again) = self.machine_work.in_flight.get_mut(key) {
                *again = true;
                return;
            }
            self.machine_work.in_flight.insert(key.clone(), false);
        }
        let host_cx = cx.clone();
        let machine = machine.to_owned();
        HostCx::spawn_detached(cx, async move {
            let answer = link.call(&machine, request).await;
            host_cx.enqueue(move |state, cx| {
                let again = key
                    .and_then(|key| state.machine_work.in_flight.remove(&key))
                    .unwrap_or(false);
                let reached = answer.is_ok();
                let left = answer.as_ref().is_err_and(|error| error.code == NOT_MEMBER);
                done(state, answer, cx);
                state.machine_answered(&machine, reached, left, contact, cx);
                if again && !left {
                    state.send_machine_work(Some(&machine), false, cx);
                }
            });
        });
    }

    /// What follows any answer from `machine`, or its refusal to answer.
    pub(crate) fn machine_answered(
        &mut self,
        machine: &str,
        reached: bool,
        left: bool,
        contact: bool,
        cx: &mut HostCx,
    ) {
        if left {
            self.machine_left(machine, cx);
        } else if reached && contact {
            self.send_machine_work(Some(machine), false, cx);
        }
    }

    /// From now on, send what this machine owes its group each time the transport starts
    /// serving the link, which a host's start is followed by, and on the slow check.
    pub(crate) fn start_machine_work(&mut self, cx: &mut HostCx) {
        let Ok(link) = self.machine_link().cloned() else {
            return;
        };
        if std::mem::replace(&mut self.machine_work.watching, true) {
            return;
        }
        let serves = link.serves();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            while serves.recv().await.is_ok() {
                host_cx.enqueue(|state, cx| state.send_machine_work(None, true, cx));
            }
        });
        self.arm_machine_check(cx);
    }

    fn arm_machine_check(&mut self, cx: &mut HostCx) {
        let due = cx.clock().timer(SEND_AGAIN_EVERY);
        let host_cx = cx.clone();
        self.machine_work.check = Some(cx.spawn_background(async move {
            due.await;
            host_cx.enqueue(|state, cx| {
                state.check_group_membership(cx);
                state.send_machine_work(None, true, cx);
                state.arm_machine_check(cx);
            });
        }));
    }

    /// Send what waits for `machine`, or for every machine. `again` also sends the reports
    /// the lead's machine has already queued, and asks each machine whose threads work with
    /// threads here whether it has anything for this one: its sending what it holds follows
    /// any request from here.
    pub(crate) fn send_machine_work(
        &mut self,
        machine: Option<&str>,
        again: bool,
        cx: &mut HostCx,
    ) {
        if self.machine_link().is_err() {
            return;
        }
        if again {
            self.machine_work.queued_there.clear();
            self.ping_working_machines(machine, cx);
        }
        self.send_remote_child_work(machine, cx);
        self.send_reports_to_leads(machine, cx);
        self.send_watch_firings_to(machine, cx);
    }

    /// The machines that run a subagent of a thread here, and those whose threads started
    /// one here, hear from this one.
    fn ping_working_machines(&mut self, only: Option<&str>, cx: &mut HostCx) {
        let machines: BTreeSet<String> = self
            .sessions
            .iter()
            .filter(|meta| meta.archived_at.is_none())
            .flat_map(|meta| {
                meta.remote_children
                    .iter()
                    .filter(|child| {
                        child.subagent
                            && !matches!(
                                child.state,
                                RemoteChildState::Cancelled
                                    | RemoteChildState::Settled
                                    | RemoteChildState::MachineLeft
                            )
                    })
                    .map(|child| child.machine.clone())
                    .chain(
                        meta.remote_parent
                            .iter()
                            .map(|parent| parent.machine.clone()),
                    )
            })
            .filter(|machine| only.is_none_or(|only| only == machine))
            .collect();
        for machine in machines {
            self.call_machine(
                &machine.clone(),
                Some(format!("ping:{machine}")),
                MachineRequest::Ping,
                false,
                cx,
                |_, _, _| {},
            );
        }
    }

    fn forget_reports_queued_on(&mut self, machine: &str) {
        let children: Vec<String> = self
            .sessions
            .iter()
            .filter(|meta| {
                meta.remote_parent
                    .as_ref()
                    .is_some_and(|parent| parent.machine == machine)
            })
            .map(|meta| format!("report:{}:", meta.id))
            .collect();
        self.machine_work
            .queued_there
            .retain(|key| !children.iter().any(|child| key.starts_with(child)));
    }

    /// Whether the machines this one's threads deal with are still in its group.
    fn check_group_membership(&mut self, cx: &mut HostCx) {
        let Ok(link) = self.machine_link() else {
            return;
        };
        let machines = link.machines(false);
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let Ok(machines) = machines.await else {
                return;
            };
            let members: HashSet<String> = machines.into_iter().map(|info| info.id).collect();
            host_cx.enqueue(move |state, cx| {
                let known: BTreeSet<String> = state
                    .sessions
                    .iter()
                    .flat_map(|meta| {
                        meta.remote_children
                            .iter()
                            .map(|child| child.machine.clone())
                            .chain(
                                meta.remote_parent
                                    .iter()
                                    .map(|parent| parent.machine.clone()),
                            )
                    })
                    .collect();
                for machine in known.difference(&members.into_iter().collect()) {
                    state.machine_left(machine, cx);
                }
            });
        });
    }

    /// `machine` is no longer in this machine's group: its subagents of threads here are
    /// gone from their leads' reach, and threads here it started carry on as ordinary
    /// threads.
    fn machine_left(&mut self, machine: &str, cx: &mut HostCx) {
        let now = cx.clock().unix_secs();
        let leads: Vec<String> = self
            .sessions
            .iter()
            .filter(|meta| {
                meta.remote_children.iter().any(|child| {
                    child.machine == machine && child.state != RemoteChildState::MachineLeft
                })
            })
            .map(|meta| meta.id.clone())
            .collect();
        for lead in leads {
            self.change_meta(&lead, cx, |meta| {
                for child in &mut meta.remote_children {
                    if child.machine == machine {
                        child.state = RemoteChildState::MachineLeft;
                        child.pending = None;
                        child.report_taken_unsent = false;
                        child.updated_at = now;
                    }
                }
            });
        }
        let children: Vec<String> = self
            .sessions
            .iter()
            .filter(|meta| {
                meta.remote_parent
                    .as_ref()
                    .is_some_and(|parent| parent.machine == machine)
            })
            .map(|meta| meta.id.clone())
            .collect();
        for child in children {
            self.change_meta(&child, cx, |meta| {
                meta.remote_parent = None;
                meta.expectation = None;
            });
        }
        let watched: Vec<String> = self
            .sessions
            .iter()
            .filter(|meta| {
                meta.remote_watches
                    .iter()
                    .any(|watch| watch.machine == machine)
            })
            .map(|meta| meta.id.clone())
            .collect();
        for thread in watched {
            self.change_meta(&thread, cx, |meta| {
                meta.remote_watches.retain(|watch| watch.machine != machine);
            });
        }
        self.refresh_control_access();
    }
}
