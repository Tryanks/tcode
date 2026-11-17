use super::*;
use tcode_core::control::{ReferenceOrigin, ThreadReference};

impl AppState {
    /// A message the user sends in `session_id` links `thread_id`. Whether the client should now
    /// tell the user, once, that the new reference crosses a project or provider boundary.
    pub fn add_thread_reference(
        &mut self,
        session_id: &str,
        thread_id: &str,
        cx: &mut HostCx,
    ) -> Result<bool, ProtocolError> {
        let target = self.find_meta(thread_id).ok_or_else(|| ProtocolError {
            code: "unknown_session".into(),
            message: "The referenced thread is no longer available.".into(),
        })?;
        let added = self.add_reference(session_id, &target, cx);
        Ok(added
            && self.find_meta(session_id).is_some_and(|meta| {
                meta.references
                    .iter()
                    .any(|reference| reference.thread_id == target.id && reference.boundary_notice)
            }))
    }

    /// A reference across a project or provider boundary carries a notice; an existing
    /// reference, or one to the thread itself, is left as it is. Whether one was added.
    fn add_reference(&mut self, session_id: &str, target: &SessionMeta, cx: &mut HostCx) -> bool {
        let now = cx.clock().unix_secs();
        let add = |meta: &mut SessionMeta| {
            if meta.id == target.id
                || meta
                    .references
                    .iter()
                    .any(|reference| reference.thread_id == target.id)
            {
                return false;
            }
            meta.references.push(ThreadReference {
                thread_id: target.id.clone(),
                by: ReferenceOrigin::User,
                created_at: now,
                boundary_notice: meta.project_id != target.project_id
                    || meta.provider != target.provider,
            });
            true
        };
        let mut added = false;
        self.change_references(session_id, cx, |meta| added = add(meta));
        added
    }

    pub fn remove_thread_reference(&mut self, session_id: &str, thread_id: &str, cx: &mut HostCx) {
        self.change_references(session_id, cx, |meta| {
            meta.references
                .retain(|reference| reference.thread_id != thread_id);
        });
    }

    /// Change `session_id`'s references. A draft's change in memory: its first message
    /// stores the draft with them, so naming a thread in a draft never stores the draft.
    fn change_references(
        &mut self,
        session_id: &str,
        cx: &mut HostCx,
        change: impl FnOnce(&mut SessionMeta),
    ) {
        match self.resident_mut(session_id).filter(|active| active.draft) {
            Some(draft) => change(&mut draft.meta),
            None => self.change_meta(session_id, cx, change),
        }
    }
}
