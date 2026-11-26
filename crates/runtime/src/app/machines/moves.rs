//! A project and its threads moving between the machines of a group: the
//! side that sends it (the commands, the preview and the `ProjectMoves`
//! topic) and the side that stages and commits it.
use tcode_protocol::{MachineRequest, MachineResponse, ProjectMovePreview, ProjectMoveStatus};

use super::{MachinePeer, not_implemented};
use crate::app::*;

impl AppState {
    /// One step of a move from `peer` to this machine.
    pub(super) fn machine_move(
        &mut self,
        _peer: &MachinePeer,
        _request: MachineRequest,
        cx: &mut HostCx,
    ) -> HostTask<Result<MachineResponse, ProtocolError>> {
        cx.spawn_background(async { Err(not_implemented("taking a project moved here")) })
    }

    pub(crate) fn move_project(
        &mut self,
        _project_id: &str,
        _machine: &str,
        _root: &str,
        _include_ignored: bool,
        _cx: &mut HostCx,
    ) -> Result<(), ProtocolError> {
        self.machine_link()?;
        Err(not_implemented("moving a project to another machine"))
    }

    pub(crate) fn cancel_project_move(
        &mut self,
        _project_id: &str,
        _cx: &mut HostCx,
    ) -> Result<(), ProtocolError> {
        self.machine_link()?;
        Err(not_implemented("cancelling a project move"))
    }

    pub(crate) fn project_move_preview(
        &mut self,
        _project_id: &str,
        _machine: &str,
        cx: &mut HostCx,
    ) -> HostTask<Result<ProjectMovePreview, ProtocolError>> {
        let refused = self
            .machine_link()
            .err()
            .unwrap_or_else(|| not_implemented("previewing a project move"));
        cx.spawn_background(async move { Err(refused) })
    }

    /// The projects moving from this machine, as [`tcode_protocol::Topic::ProjectMoves`]
    /// answers.
    pub(crate) fn project_moves_snapshot(&self) -> Vec<ProjectMoveStatus> {
        Vec::new()
    }
}
