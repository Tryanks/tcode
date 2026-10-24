use super::*;
use control_mcp::{RecordReviewParams, SettleSelfParams, UpdateThreadsParams};
use tcode_core::control::FollowUp;

impl AppState {
    pub(super) fn op_update_threads(
        &mut self,
        _caller: &Caller,
        _params: UpdateThreadsParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        // Phase 2
        Err("not implemented yet".into())
    }

    pub(super) fn op_record_review(
        &mut self,
        _caller: &Caller,
        _params: RecordReviewParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        // Phase 2
        Err("not implemented yet".into())
    }

    pub(super) fn op_settle_self(
        &mut self,
        _caller: &Caller,
        _params: SettleSelfParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        // Phase 2
        Err("not implemented yet".into())
    }

    pub fn set_follow_up(
        &mut self,
        session_id: &str,
        follow_up: Option<FollowUp>,
        cx: &mut HostCx,
    ) {
        self.change_meta(session_id, cx, |meta| meta.follow_up = follow_up);
    }

    pub fn set_parked(&mut self, session_id: &str, parked: bool, cx: &mut HostCx) {
        self.change_meta(session_id, cx, |meta| {
            meta.parked_at = parked.then(now_secs);
        });
    }

    pub fn dismiss_settle_suggestion(&mut self, session_id: &str, cx: &mut HostCx) {
        self.change_meta(session_id, cx, |meta| {
            meta.settle_suggestion_dismissed_at = Some(now_secs());
        });
    }

    pub fn revoke_self_settle(&mut self, session_id: &str, cx: &mut HostCx) {
        self.change_meta(session_id, cx, |meta| meta.self_settle = None);
    }

    /// Apply `change` to a stored thread's meta and persist it when it changed anything.
    pub(super) fn change_meta(
        &mut self,
        session_id: &str,
        cx: &mut HostCx,
        change: impl FnOnce(&mut SessionMeta),
    ) {
        let Some(before) = self.find_meta(session_id) else {
            return;
        };
        let mut meta = before.clone();
        change(&mut meta);
        if meta == before {
            return;
        }
        meta.updated_at = now_secs();
        self.store_meta(meta, cx);
    }
}
