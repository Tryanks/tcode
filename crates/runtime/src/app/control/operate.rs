use super::*;
use control_mcp::{
    ControlThreadParams, CreateProjectParams, GitParams, PullRequestParams, SendMessageParams,
    StartThreadParams,
};

impl AppState {
    pub(super) fn op_start_thread(
        &mut self,
        _caller: &Caller,
        _params: StartThreadParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        // Phase 2
        Err("not implemented yet".into())
    }

    pub(super) fn op_send_message(
        &mut self,
        _caller: &Caller,
        _params: SendMessageParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        // Phase 2
        Err("not implemented yet".into())
    }

    pub(super) fn op_control_thread(
        &mut self,
        _caller: &Caller,
        _params: ControlThreadParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        // Phase 2
        Err("not implemented yet".into())
    }

    pub(super) fn op_pull_request(
        &mut self,
        _caller: &Caller,
        _params: PullRequestParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        // Phase 2
        Err("not implemented yet".into())
    }

    pub(super) fn op_git(
        &mut self,
        _caller: &Caller,
        _params: GitParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        // Phase 2
        Err("not implemented yet".into())
    }

    pub(super) fn op_create_project(
        &mut self,
        _caller: &Caller,
        _params: CreateProjectParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        // Phase 2
        Err("not implemented yet".into())
    }
}
