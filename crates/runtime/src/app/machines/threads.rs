//! Threads one machine of the group starts on another, and the replies that
//! come back to the machine that started them.
use tcode_protocol::{MachineCaller, MachineRequest, MachineResponse};

use super::{MachinePeer, not_implemented};
use crate::app::*;

impl AppState {
    /// Run one `tcode_control` tool here for `caller`, a thread on `peer`.
    pub(super) fn machine_control(
        &mut self,
        _peer: &MachinePeer,
        _caller: MachineCaller,
        _tool: String,
        _args: serde_json::Value,
        cx: &mut HostCx,
    ) -> HostTask<Result<MachineResponse, ProtocolError>> {
        cx.spawn_background(async { Err(not_implemented("starting threads for another machine")) })
    }

    /// A [`MachineRequest::Callback`] or [`MachineRequest::ReportTaken`]: the
    /// answer to something this machine sent `peer`.
    pub(super) fn machine_reply(
        &mut self,
        _peer: &MachinePeer,
        _request: MachineRequest,
        cx: &mut HostCx,
    ) -> HostTask<Result<MachineResponse, ProtocolError>> {
        cx.spawn_background(async { Err(not_implemented("replies from another machine")) })
    }
}
