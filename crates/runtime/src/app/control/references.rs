use super::*;
use control_mcp::ReferenceParams;
use tcode_core::control::{ReferenceOrigin, ThreadReference};

impl AppState {
    pub(super) fn op_reference(
        &mut self,
        _caller: &Caller,
        _params: ReferenceParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        // Phase 2
        Err("not implemented yet".into())
    }

    /// The user named `thread_id` in `session_id`'s composer; a reference across a project or
    /// provider boundary carries the notice the user saw once.
    pub fn add_thread_reference(
        &mut self,
        session_id: &str,
        thread_id: &str,
        cx: &mut HostCx,
    ) -> Result<(), ProtocolError> {
        let target = self.find_meta(thread_id).ok_or_else(|| ProtocolError {
            code: "unknown_session".into(),
            message: "The referenced thread is no longer available.".into(),
        })?;
        self.change_meta(session_id, cx, |meta| {
            if meta.id == target.id
                || meta
                    .references
                    .iter()
                    .any(|reference| reference.thread_id == target.id)
            {
                return;
            }
            meta.references.push(ThreadReference {
                thread_id: target.id.clone(),
                by: ReferenceOrigin::User,
                created_at: now_secs(),
                boundary_notice: meta.project_id != target.project_id
                    || meta.provider != target.provider,
            });
        });
        Ok(())
    }

    pub fn remove_thread_reference(&mut self, session_id: &str, thread_id: &str, cx: &mut HostCx) {
        self.change_meta(session_id, cx, |meta| {
            meta.references
                .retain(|reference| reference.thread_id != thread_id);
        });
    }
}
