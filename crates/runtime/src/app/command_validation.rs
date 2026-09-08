use super::*;

impl AppState {
    /// Validate retained conversation writes before the pipe acknowledges ownership.
    pub(crate) fn validate_command_target(
        &self,
        command: &tcode_protocol::Command,
    ) -> Result<(), tcode_protocol::ProtocolError> {
        use tcode_protocol::Command;
        let error = |code: &str, message: &str| tcode_protocol::ProtocolError {
            code: code.into(),
            message: message.into(),
        };
        match command {
            Command::DeleteProfile { profile_id }
                if Settings::is_builtin_profile_id(profile_id) =>
            {
                return Err(error(
                    "builtin_profile",
                    "Built-in provider profiles cannot be deleted.",
                ));
            }
            Command::TerminalInput { terminal_id, .. }
            | Command::ClearTerminal { terminal_id }
            | Command::ResizeTerminal { terminal_id, .. }
                if self.terminal_handle(*terminal_id).is_none() =>
            {
                return Err(error(
                    "unknown_terminal",
                    "This terminal is no longer available.",
                ));
            }
            Command::ToggleProjectCollapsed { project_id }
            | Command::DeleteProject { project_id }
                if !self
                    .projects
                    .iter()
                    .any(|project| &project.id == project_id) =>
            {
                return Err(error(
                    "unknown_project",
                    "This project is no longer available.",
                ));
            }
            Command::SetProfileSecret { profile_id, .. }
            | Command::UpdateProfileSettings { profile_id, .. }
            | Command::DeleteProfile { profile_id }
                if self.settings.resolved_profile(profile_id).is_none() =>
            {
                return Err(error(
                    "unknown_profile",
                    "This provider profile is no longer available.",
                ));
            }
            _ => {}
        }
        match command {
            Command::InstallAcpAgent { id } if self.acp_installing.contains(id) => {
                return Err(error(
                    "acp_installing",
                    "This agent is already being installed.",
                ));
            }
            Command::InstallAcpAgent { id }
                if !self.acp_marketplace().iter().any(|agent| &agent.id == id) =>
            {
                return Err(error(
                    "unknown_acp_agent",
                    "This agent is not in the registry.",
                ));
            }
            Command::UpdateAcpAgent { id, .. } | Command::SetActiveAcpAgent { id, .. }
                if !self.settings.acp_agents.contains_key(id) =>
            {
                return Err(error("unknown_acp_agent", "This agent is not installed."));
            }
            Command::MergeWorktree { session_id } => {
                return match self.find_meta(session_id) {
                    None => Err(error(
                        "unknown_session",
                        "This thread is no longer available on the host.",
                    )),
                    Some(meta) if meta.worktree.is_none() => Err(error(
                        "no_worktree",
                        "This thread has no worktree to merge.",
                    )),
                    Some(_) => Ok(()),
                };
            }
            _ => {}
        }
        let Some(session_id) = command.session_id() else {
            return Ok(());
        };
        // Index mutations operate on stored sessions, without requiring a live provider.
        if matches!(
            command,
            Command::ArchiveSession { .. }
                | Command::UnarchiveSession { .. }
                | Command::RenameSession { .. }
                | Command::DeleteSession { .. }
                | Command::MarkSessionUnread { .. }
                | Command::ForkThread { .. }
        ) {
            return if self.sessions.iter().any(|meta| meta.id == session_id)
                || self.resident(session_id).is_some()
            {
                Ok(())
            } else {
                Err(error(
                    "unknown_session",
                    "This thread is no longer available on the host.",
                ))
            };
        }
        let active = self.resident(session_id).ok_or_else(|| {
            error(
                "unknown_session",
                "This thread is no longer available on the host.",
            )
        })?;
        match command {
            Command::SetDraftWorkspace { .. } if !active.draft => return Err(error("not_a_draft", "Only a draft can change workspace.")),
            Command::CheckoutBranch { .. } if active.timeline.turn_running => return Err(error("turn_running", "Wait for the running turn before changing branches.")),
            Command::CaptureTerminalSelection { selection: None, .. } => return Err(error("no_selection", "Select terminal text first.")),
            Command::NewTerminal { .. } | Command::SplitTerminal { .. }
                if active.terminal_workspace.terminals.len() + self.pending_terminal_spawns.get(session_id).map_or(0, HashMap::len) >= MAX_TERMINALS_PER_SESSION => return Err(error("terminal_limit", "This thread has reached its terminal limit.")),
            Command::SplitTerminal { .. } if active.terminal_workspace.active_id.is_none_or(|first| active.terminal_workspace.split_for(first).is_some() || self.pending_terminal_spawns.get(session_id).is_some_and(|spawns| spawns.values().any(|action| matches!(action, TerminalSpawnAction::Split { first: pending, .. } if *pending == first)))) => return Err(error("terminal_split_unavailable", "There is no terminal available to split.")),
            Command::RewindTurn { turn, mode, .. }
                if !active.meta.provider.caps().native_rewind || active.turn_in_flight || active.delivery_in_flight.is_some() || active.background_task_count > 0 || active.timeline.turn_running || !active.queue.is_empty() || active.timeline.turns.get(*turn).and_then(|turn| turn.provider_checkpoint_id.as_ref()).is_none() || (*turn == 0 && mode.includes_conversation()) || self.pending_native_rewinds.contains_key(session_id) => return Err(error("rewind_unavailable", "This turn cannot be rewound now.")),
            Command::ActivateTerminal { terminal_id, .. }
            | Command::CloseTerminal { terminal_id, .. }
            | Command::CaptureTerminalSelection { terminal_id, .. }
                if active.terminal_workspace.terminal(*terminal_id).is_none() =>
            {
                return Err(error(
                    "unknown_terminal",
                    "This terminal is no longer available.",
                ));
            }
            Command::RemoveTerminalContext { context_id, .. }
                if !active
                    .terminal_workspace
                    .contexts
                    .iter()
                    .any(|context| context.id == *context_id) =>
            {
                return Err(error(
                    "unknown_terminal_context",
                    "This terminal selection is no longer available.",
                ));
            }
            Command::RemoveReviewComment { index, .. }
                if *index >= self.review_comments(session_id).len() =>
            {
                return Err(error(
                    "unknown_review_comment",
                    "This review comment is no longer available.",
                ));
            }
            Command::ImplementPlan { .. }
            | Command::DismissPlan { .. }
            | Command::ImplementPlanInNewThread { .. }
                if active.timeline.plan_ready().is_none() =>
            {
                return Err(error("unknown_plan", "There is no pending plan."));
            }
            Command::RunGitAction { .. } if self.git_busy.contains(session_id) => {
                return Err(error("git_busy", "A Git operation is already running."));
            }
            Command::RespondApproval { request_id, .. } => {
                if !self
                    .approval_requests(session_id)
                    .iter()
                    .any(|request| &request.id == request_id)
                {
                    return Err(error(
                        "unknown_approval",
                        "This approval is no longer pending.",
                    ));
                }
            }
            Command::RespondUserInput { request_id, .. } => {
                if !active
                    .timeline
                    .pending_user_input
                    .as_ref()
                    .is_some_and(|request| &request.0 == request_id)
                {
                    return Err(error(
                        "unknown_user_input",
                        "This question is no longer pending.",
                    ));
                }
            }
            Command::SteerQueued { id, .. } | Command::DropQueued { id, .. }
                if !active.queue.iter().any(|message| message.id == *id)
                    || active.delivery_in_flight == Some(*id) =>
            {
                return Err(error(
                    "unknown_queued_message",
                    "This message is no longer editable in the queue.",
                ));
            }
            _ => {}
        }
        if matches!(command, Command::Interrupt { .. })
            && !active.turn_in_flight
            && !active.timeline.turn_running
            && active.background_task_count == 0
        {
            return Err(error(
                "no_running_turn",
                "There is no running turn to interrupt.",
            ));
        }
        if matches!(
            command,
            Command::Interrupt { .. }
                | Command::RespondApproval { .. }
                | Command::RespondUserInput { .. }
        ) {
            match &active.runtime {
                Runtime::Live(commands) if !commands.is_closed() => {}
                _ => {
                    return Err(error(
                        "no_running_turn",
                        "The provider is no longer running.",
                    ));
                }
            }
        }
        if matches!(
            command,
            Command::SendTurn { .. }
                | Command::ScheduleTurn { .. }
                | Command::ConfirmRelayAndSend { .. }
                | Command::OrchestrateTurn { .. }
                | Command::Steer { .. }
                | Command::ImplementPlan { .. }
        ) {
            if active.meta.native_subagent.is_some() {
                return Err(error(
                    "read_only_session",
                    "This thread is a read-only mirror.",
                ));
            }
            if !matches!(command, Command::ConfirmRelayAndSend { .. })
                && self.relay_confirmation(session_id).is_some()
            {
                return Err(error(
                    "relay_confirmation_required",
                    "Confirm the provider change before sending.",
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::{TestAppContext, TestStore};
    use tcode_protocol::{ClientMessage, ClientPayload, Command, HostMessage};

    #[test]
    fn writes_blocked_by_host_state_receive_rejected_acks() {
        let store = TestStore::new("write-preconditions");
        let mut state = AppState::new((*store).clone());
        let mut context = TestAppContext::default();
        let mut cx = context.host_cx();
        let id = state.start_draft("fixture".into(), std::env::temp_dir(), &mut cx);
        for (index, expected) in [
            "read_only_session",
            "relay_confirmation_required",
            "git_busy",
            "not_a_draft",
            "turn_running",
            "acp_installing",
            "terminal_limit",
        ]
        .into_iter()
        .enumerate()
        {
            let active = state.resident_mut(&id).unwrap();
            active.meta.native_subagent = (index == 0).then(|| "mirror".into());
            if index == 1 {
                active.timeline.apply_at(
                    None,
                    &AgentEvent::ItemCompleted(agent::ThreadItem {
                        id: "history".into(),
                        parent_item_id: None,
                        content: agent::ItemContent::UserMessage {
                            text: "Earlier conversation".into(),
                            context_len: None,
                            attachments: Vec::new(),
                        },
                    }),
                );
            }
            if index == 1 {
                active.timeline.apply_at(
                    None,
                    &AgentEvent::TurnCompleted {
                        turn_id: "earlier-turn".into(),
                        status: agent::TurnStatus::Completed,
                        usage: None,
                    },
                );
            }
            active.pending_relay = (index == 1).then_some(PendingRelay {
                from_provider: ProviderKind::ClaudeCode,
                from_model: None,
                from_profile: None,
            });
            let command = match index {
                2 => {
                    state.git_busy.insert(id.clone());
                    Command::RunGitAction {
                        session_id: id.clone(),
                        action: GitAction::Push,
                        message: None,
                        included: None,
                        feature_branch: None,
                    }
                }
                3 => {
                    state.resident_mut(&id).unwrap().draft = false;
                    Command::SetDraftWorkspace {
                        session_id: id.clone(),
                        mode: WorkspaceMode::LocalCheckout,
                    }
                }
                4 => {
                    state.resident_mut(&id).unwrap().timeline.turn_running = true;
                    Command::CheckoutBranch {
                        session_id: id.clone(),
                        branch: "main".into(),
                    }
                }
                5 => {
                    state.acp_installing.insert("already-installing".into());
                    Command::InstallAcpAgent {
                        id: "already-installing".into(),
                    }
                }
                6 => {
                    state.pending_terminal_spawns.insert(
                        id.clone(),
                        (0..MAX_TERMINALS_PER_SESSION as u64)
                            .map(|id| (id, TerminalSpawnAction::New))
                            .collect(),
                    );
                    Command::NewTerminal {
                        session_id: id.clone(),
                    }
                }
                _ => Command::SendTurn {
                    session_id: id.clone(),
                    text: "must not vanish".into(),
                    attachment_paths: Vec::new(),
                },
            };
            crate::pipe::handle_client_message(
                &mut state,
                &mut cx,
                ClientMessage {
                    id: 42,
                    key: Some(format!("write-{index}")),
                    payload: ClientPayload::Command(command),
                },
            );
            assert!(
                context
                    .drain_outgoing()
                    .iter()
                    .any(|message| matches!(message,
                        HostMessage::Ack { id: 42, result: Err(error) } if error.code == expected
                    )),
                "expected rejected Ack: {expected}"
            );
            assert!(state.resident(&id).unwrap().queue.is_empty());
        }
    }
    #[test]
    fn provider_channel_rejection_is_not_acknowledged_as_success() {
        let store = TestStore::new("provider-write-rejection");
        let mut state = AppState::new((*store).clone());
        let mut context = TestAppContext::default();
        let mut cx = context.host_cx();
        let id = state.start_draft("fixture".into(), std::env::temp_dir(), &mut cx);
        let (commands, _receiver) = smol::channel::bounded(1);
        commands.try_send(SessionCommand::Interrupt).unwrap();
        let active = state.resident_mut(&id).unwrap();
        active.runtime = Runtime::Live(commands);
        active.turn_in_flight = true;
        active.timeline.pending_user_input = Some(("question".into(), Vec::new()));
        state.record_approval_event(
            &id,
            &AgentEvent::ApprovalRequested(agent::ApprovalRequest {
                id: "approval".into(),
                turn_id: None,
                options: Vec::new(),
                kind: agent::ApprovalKind::ExecCommand {
                    command: "echo fixture".into(),
                    cwd: None,
                    reason: None,
                },
            }),
        );
        for (index, command) in [
            Command::Interrupt {
                session_id: id.clone(),
            },
            Command::RespondApproval {
                session_id: id.clone(),
                request_id: "approval".into(),
                decision: ApprovalDecision::Approve,
            },
            Command::RespondUserInput {
                session_id: id.clone(),
                request_id: "question".into(),
                answers: Default::default(),
            },
        ]
        .into_iter()
        .enumerate()
        {
            crate::pipe::handle_client_message(
                &mut state,
                &mut cx,
                ClientMessage {
                    id: 42,
                    key: Some(format!("blocked-{index}")),
                    payload: ClientPayload::Command(command),
                },
            );
            assert!(context.drain_outgoing().iter().any(|message| matches!(message,
                HostMessage::Ack { id: 42, result: Err(error) } if error.code == "provider_unavailable"
            )));
        }
        assert!(
            state.has_approval(&id),
            "failed delivery must leave the approval pending"
        );
    }
}
