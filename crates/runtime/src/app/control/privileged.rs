use super::*;
use control_mcp::{
    DeleteThreadsParams, PatchSettingsParams, RespondInputParams, RewindParams, TerminalParams,
};
use tcode_core::settings::SettingsPatch;

/// Settings no grant lets an agent change: credentials and hosting, and everything that
/// changes authority (grants, the control configuration, the settlement automation tier).
pub(super) fn settings_patch_never_exposed(patch: &SettingsPatch) -> bool {
    matches!(
        patch,
        SettingsPatch::SourceControlHost { .. }
            | SettingsPatch::RemoveSourceControlHost { .. }
            | SettingsPatch::RemoteHostingEnabled(_)
            | SettingsPatch::Traverse(_)
            | SettingsPatch::RemoteHostName(_)
            | SettingsPatch::ControlEnabled(_)
            | SettingsPatch::ControlDefaultGrant(_)
            | SettingsPatch::ControlProjectGrant { .. }
            | SettingsPatch::SettlementAutomation(_)
    )
}

impl AppState {
    pub(super) fn op_respond_input(
        &mut self,
        _caller: &Caller,
        _params: RespondInputParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        // Phase 2
        Err("not implemented yet".into())
    }

    pub(super) fn op_terminal(
        &mut self,
        _caller: &Caller,
        _params: TerminalParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        // Phase 2
        Err("not implemented yet".into())
    }

    pub(super) fn op_delete_threads(
        &mut self,
        _caller: &Caller,
        _params: DeleteThreadsParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        // Phase 2
        Err("not implemented yet".into())
    }

    pub(super) fn op_rewind(
        &mut self,
        _caller: &Caller,
        _params: RewindParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        // Phase 2
        Err("not implemented yet".into())
    }

    pub(super) fn op_patch_settings(
        &mut self,
        _caller: &Caller,
        _params: PatchSettingsParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        // Phase 2
        Err("not implemented yet".into())
    }
}
