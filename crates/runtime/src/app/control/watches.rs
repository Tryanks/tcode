use super::*;
use control_mcp::{CancelNotifyParams, NotifyWhenParams};

impl AppState {
    pub(super) fn op_notify_when(
        &mut self,
        _caller: &Caller,
        _params: NotifyWhenParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        // Phase 2
        Err("not implemented yet".into())
    }

    pub(super) fn op_cancel_notify(
        &mut self,
        _caller: &Caller,
        _params: CancelNotifyParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        // Phase 2
        Err("not implemented yet".into())
    }
}
