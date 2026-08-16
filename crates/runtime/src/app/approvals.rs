use super::*;

impl AppState {
    /// Apply the approval-state transition carried by a canonical provider event.
    pub(super) fn record_approval_event(&mut self, session_id: &str, event: &AgentEvent) {
        match event {
            AgentEvent::ApprovalRequested(request) => {
                let requests = self.approvals.entry(session_id.to_string()).or_default();
                if !requests.iter().any(|pending| pending.id == request.id) {
                    requests.push(request.clone());
                }
            }
            AgentEvent::ApprovalResolved { request_id, .. } => {
                self.clear_approval(session_id, request_id);
            }
            AgentEvent::TurnCompleted { .. } => {
                self.clear_approvals(session_id);
            }
            _ => {}
        }
    }

    pub(super) fn approval_requests(&self, session_id: &str) -> &[agent::ApprovalRequest] {
        self.approvals
            .get(session_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub(super) fn first_approval(&self, session_id: &str) -> Option<&agent::ApprovalRequest> {
        self.approval_requests(session_id).first()
    }

    pub(super) fn has_approval(&self, session_id: &str) -> bool {
        !self.approval_requests(session_id).is_empty()
    }

    pub(super) fn clear_approvals(&mut self, session_id: &str) {
        self.approvals.remove(session_id);
    }

    fn clear_approval(&mut self, session_id: &str, request_id: &str) {
        let Some(requests) = self.approvals.get_mut(session_id) else {
            return;
        };
        requests.retain(|request| request.id != request_id);
        if requests.is_empty() {
            self.approvals.remove(session_id);
        }
    }

    /// Send a response to any resident session and clear the same request from
    /// the host authority once the provider command has accepted it.
    pub(super) fn respond_session_approval(
        &mut self,
        session_id: &str,
        request_id: String,
        decision: ApprovalDecision,
    ) -> Result<(), String> {
        let commands = match &self
            .resident(session_id)
            .ok_or_else(|| "session is not loaded".to_string())?
            .runtime
        {
            Runtime::Live(commands) => commands.clone(),
            _ => return Err("session is not live".into()),
        };
        commands
            .try_send(SessionCommand::RespondApproval {
                request_id: request_id.clone(),
                decision,
            })
            .map_err(|err| format!("failed to respond to approval: {err}"))?;
        self.clear_approval(session_id, &request_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::TestStore;
    use super::*;

    fn request(id: &str) -> agent::ApprovalRequest {
        agent::ApprovalRequest {
            id: id.into(),
            turn_id: None,
            kind: agent::ApprovalKind::ExecCommand {
                command: "cargo test".into(),
                cwd: None,
                reason: None,
            },
            options: Vec::new(),
        }
    }

    fn live_session(id: &str, commands: smol::channel::Sender<SessionCommand>) -> ActiveSession {
        let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp"), None);
        meta.id = id.into();
        let mut session = ActiveSession::new(meta, false, Vec::new());
        session.runtime = Runtime::Live(commands);
        session
    }

    fn state(name: &str) -> (TestStore, AppState) {
        let store = TestStore::new(name);
        let state = AppState::new((*store).clone());
        (store, state)
    }

    #[test]
    fn live_session_pending_approval_sets_and_clears() {
        let (_store, mut state) = state("approval-live-test");
        let (commands, _receiver) = smol::channel::unbounded();
        state.active = Some(live_session("live", commands));

        let request = request("approval-live");
        state.record_approval_event("live", &AgentEvent::ApprovalRequested(request.clone()));
        assert_eq!(state.first_approval("live"), Some(&request));
        assert!(state.has_approval("live"));

        state.record_approval_event(
            "live",
            &AgentEvent::ApprovalResolved {
                request_id: request.id,
                decision: ApprovalDecision::Approve,
            },
        );
        assert!(!state.has_approval("live"));
    }

    #[test]
    fn parked_session_reports_pending_from_the_same_authority() {
        let (_store, mut state) = state("approval-parked-test");
        let (commands, _receiver) = smol::channel::unbounded();
        state
            .background
            .insert("parked".into(), live_session("parked", commands));

        state.record_approval_event(
            "parked",
            &AgentEvent::ApprovalRequested(request("approval-parked")),
        );

        assert!(state.active.is_none());
        assert!(
            state
                .session_status_snapshot("parked")
                .unwrap()
                .pending_approval
        );
    }

    #[test]
    fn orchestrated_child_reads_pending_from_the_same_authority() {
        let (_store, mut state) = state("approval-child-test");
        let mut child = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp"), None);
        child.id = "child".into();
        child.parent_session_id = Some("parent".into());
        state.sessions.push(child.clone());
        state.record_approval_event(
            "child",
            &AgentEvent::ApprovalRequested(request("approval-child")),
        );

        let status = state.child_status_json(&child, &Timeline::default());
        assert_eq!(status["approval_request_id"], "approval-child");
        assert_eq!(status["waiting_approval"], "command `cargo test`");
    }

    #[test]
    fn responding_clears_all_host_views_at_once() {
        let (_store, mut state) = state("approval-response-test");
        let (commands, receiver) = smol::channel::unbounded();
        let mut child = live_session("child", commands);
        child.meta.parent_session_id = Some("parent".into());
        state.sessions.push(child.meta.clone());
        state.background.insert("child".into(), child);
        state.record_approval_event(
            "child",
            &AgentEvent::ApprovalRequested(request("approval-response")),
        );

        assert!(
            state
                .session_status_snapshot("child")
                .unwrap()
                .pending_approval
        );
        assert_ne!(
            state.child_status_json(&state.sessions[0], &Timeline::default())["approval_request_id"],
            serde_json::Value::Null
        );

        state
            .respond_session_approval(
                "child",
                "approval-response".into(),
                ApprovalDecision::Approve,
            )
            .unwrap();

        assert!(matches!(
            receiver.try_recv(),
            Ok(SessionCommand::RespondApproval { request_id, .. })
                if request_id == "approval-response"
        ));
        assert!(!state.has_approval("child"));
        assert!(
            !state
                .session_status_snapshot("child")
                .unwrap()
                .pending_approval
        );
        assert_eq!(
            state.child_status_json(&state.sessions[0], &Timeline::default())["approval_request_id"],
            serde_json::Value::Null
        );
    }
}
