//! What this machine answers the other machines of its group, and its way to
//! them. Requests arrive as [`tcode_protocol::Query::Machine`] from a
//! [`tcode_protocol::Principal::Machine`], after the transport has checked
//! the machine is a member and that this machine's switches take the kind of
//! request; requests go out through the [`MachineLink`] the composition root
//! gave the host.
mod moves;
mod threads;

use tcode_client::machines::{MachineLink, NO_GROUP};
use tcode_protocol::{MachineRequest, MachineResponse};

use super::*;

/// The machine of the group a request came from, as the transport admitted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MachinePeer {
    pub id: String,
    pub name: String,
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
        match request {
            MachineRequest::Ping => cx.spawn_background(async {
                Ok(MachineResponse::Pong {
                    protocol_version: tcode_protocol::PROTOCOL_VERSION,
                })
            }),
            MachineRequest::Control { caller, tool, args } => {
                self.machine_control(peer, caller, tool, args, cx)
            }
            request @ (MachineRequest::Callback { .. } | MachineRequest::ReportTaken { .. }) => {
                self.machine_reply(peer, request, cx)
            }
            request @ (MachineRequest::MoveBegin { .. }
            | MachineRequest::MoveChunk { .. }
            | MachineRequest::MoveCommit { .. }
            | MachineRequest::MoveAbort { .. }
            | MachineRequest::MoveStatus { .. }) => self.machine_move(peer, request, cx),
        }
    }
}

/// The answer of a part of this feature still to be built.
pub(crate) fn not_implemented(what: &str) -> ProtocolError {
    ProtocolError {
        code: "not_implemented".into(),
        message: format!("{what} is not implemented on this machine yet"),
    }
}
