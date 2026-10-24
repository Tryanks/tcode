use super::*;
use control_mcp::{ReferenceParams, ReferenceResponse};
use tcode_core::control::{ControlTier, ReferenceOrigin, ThreadReference};

impl AppState {
    /// Only a thread inside the caller's observe scope can be referenced; `authorize_control`
    /// has already held the target to the grant's scope.
    pub(super) fn op_reference(
        &mut self,
        caller: &Caller,
        params: ReferenceParams,
        cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        if !caller.grant.allows_tier(ControlTier::Observe) {
            return Err(format!(
                "refused: `reference` reads within tier observe; your grant: {}",
                caller.grant.describe()
            ));
        }
        let target = self
            .find_meta(&params.thread_id)
            .ok_or_else(|| format!("unknown thread: {}", params.thread_id))?;
        if target.id == caller.thread_id {
            return Err("a thread always reads its own log; it needs no reference".into());
        }
        self.add_reference(&caller.thread_id, &target, ReferenceOrigin::Model, cx);
        let boundary_notice = self
            .find_meta(&caller.thread_id)
            .and_then(|meta| {
                meta.references
                    .into_iter()
                    .find(|reference| reference.thread_id == target.id)
            })
            .is_some_and(|reference| reference.boundary_notice);
        let detail = match params.reason {
            Some(reason) => format!("{} ({}): {reason}", target.id, target.title),
            None => format!("{} ({})", target.id, target.title),
        };
        self.audit(caller, &caller.thread_id, "reference", Some(detail), cx);
        serde_json::to_value(ReferenceResponse {
            thread_id: target.id,
            title: target.title,
            boundary_notice,
        })
        .map_err(|error| error.to_string())
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
        self.add_reference(session_id, &target, ReferenceOrigin::User, cx);
        Ok(())
    }

    /// A reference across a project or provider boundary carries a notice; an existing
    /// reference, or one to the thread itself, is left as it is.
    fn add_reference(
        &mut self,
        session_id: &str,
        target: &SessionMeta,
        by: ReferenceOrigin,
        cx: &mut HostCx,
    ) {
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
                by,
                created_at: now_secs(),
                boundary_notice: meta.project_id != target.project_id
                    || meta.provider != target.provider,
            });
        });
    }

    pub fn remove_thread_reference(&mut self, session_id: &str, thread_id: &str, cx: &mut HostCx) {
        self.change_meta(session_id, cx, |meta| {
            meta.references
                .retain(|reference| reference.thread_id != thread_id);
        });
    }
}
