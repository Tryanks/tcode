use super::*;
use control_mcp::{FileHistoryParams, OutlineParams, ReadParams, SearchInItemParams};

impl AppState {
    pub(super) fn op_outline(
        &mut self,
        _caller: &Caller,
        _params: OutlineParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        // Phase 2
        Err("not implemented yet".into())
    }

    pub(super) fn op_read(
        &mut self,
        _caller: &Caller,
        _params: ReadParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        // Phase 2
        Err("not implemented yet".into())
    }

    pub(super) fn op_search_in_item(
        &mut self,
        _caller: &Caller,
        _params: SearchInItemParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        // Phase 2
        Err("not implemented yet".into())
    }

    pub(super) fn op_file_history(
        &mut self,
        _caller: &Caller,
        _params: FileHistoryParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        // Phase 2
        Err("not implemented yet".into())
    }
}
