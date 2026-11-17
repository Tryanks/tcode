use super::*;

impl Mapper {
    pub(super) fn on_control_response(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let Some(response) = msg.get("response") else {
            return Vec::new();
        };
        let Some(request_id) = response.get("request_id").and_then(Value::as_str) else {
            return Vec::new();
        };
        if let Some(reply) = self.pending_usage_reads.remove(request_id) {
            let _ = reply.try_send(usage_response(response).map_err(|error| error.to_string()));
            return Vec::new();
        }
        if self.pending_initialize.as_deref() == Some(request_id) {
            self.pending_initialize = None;
            if response.get("subtype").and_then(Value::as_str) != Some("success") {
                log::warn!("claude: the worktree hooks were not registered: {response}");
            }
            return self
                .observe_fast_mode(response.pointer("/response/fast_mode_state"))
                .into_iter()
                .collect();
        }
        if self.pending_settings_read.as_deref() == Some(request_id) {
            self.pending_settings_read = None;
            return self.on_settings_read(response);
        }
        if self.pending_edit_diff.as_deref() == Some(request_id) {
            self.pending_edit_diff = None;
            if response.get("subtype").and_then(Value::as_str) == Some("success") {
                return Vec::new();
            }
            let error = response
                .get("error")
                .and_then(Value::as_str)
                .map_or_else(|| response.to_string(), str::to_owned);
            return vec![AgentEvent::Warning {
                message: format!(
                    "claude: could not turn on bashEditDiffEnabled, so the files a command changes are not shown: {error}"
                ),
            }];
        }
        if self.pending_background_moves.remove(request_id) {
            let moved = response.get("subtype").and_then(Value::as_str) == Some("success")
                && response
                    .pointer("/response/backgrounded")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
            if moved {
                return Vec::new();
            }
            let error = response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("the call is no longer running in the foreground");
            return vec![AgentEvent::Warning {
                message: format!("claude: could not move the call to the background: {error}"),
            }];
        }
        if self
            .pending_settings
            .as_ref()
            .is_some_and(|pending| pending.request_id == request_id)
        {
            return self.on_settings_response(response);
        }
        if let Some(mode) = self.pending_permission_modes.remove(request_id) {
            if response.get("subtype").and_then(Value::as_str) == Some("success") {
                self.applied_permission_mode = response
                    .pointer("/response/permissionMode")
                    .or_else(|| response.pointer("/response/mode"))
                    .and_then(Value::as_str)
                    .unwrap_or(&mode)
                    .to_owned();
                if self.applied_permission_mode == "auto" {
                    self.auto_unavailable = None;
                }
                return vec![self.permission_options()];
            }
            let error = response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("Claude Code rejected the permission-mode request");
            let warning = AgentEvent::Warning {
                message: format!("claude: failed to switch permission mode: {error}"),
            };
            if mode == "auto"
                && response
                    .get("error_code")
                    .and_then(Value::as_str)
                    .is_some_and(|code| code.starts_with("auto_mode_"))
            {
                self.auto_unavailable = Some(error.to_owned());
                return vec![warning, self.permission_options()];
            }
            return vec![warning];
        }
        let Some(pending) = self.pending_rewinds.remove(request_id) else {
            return Vec::new();
        };
        self.on_rewind_response(pending, response)
    }

    /// One step of a rewind answered: send the next, or report the outcome.
    pub(super) fn on_rewind_response(
        &mut self,
        mut pending: PendingRewind,
        response: &Value,
    ) -> Vec<AgentEvent> {
        let succeeded = response.get("subtype").and_then(Value::as_str) == Some("success");
        let result = response.get("response").unwrap_or(&Value::Null);
        let error = || {
            response
                .get("error")
                .or_else(|| result.get("error"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        match pending.step {
            RewindStep::Interrupt => {
                pending.step = RewindStep::Files;
                let request = self.rewind_request(pending);
                self.outgoing.push(request);
                Vec::new()
            }
            RewindStep::Preview => {
                let result = if succeeded
                    && result.get("canRewind").and_then(Value::as_bool) == Some(true)
                {
                    let files = rewind_files(result);
                    self.rewind_previews
                        .insert(pending.checkpoint_id.clone(), files.clone());
                    RewindPreviewResult::Ready(RewindPreview {
                        files,
                        conflicts: Vec::new(),
                    })
                } else {
                    RewindPreviewResult::Refused(files_failure(error()))
                };
                vec![AgentEvent::RewindPreviewed {
                    checkpoint_id: pending.checkpoint_id,
                    mode: pending.mode,
                    result,
                }]
            }
            RewindStep::Files => {
                if succeeded && result.get("canRewind").and_then(Value::as_bool) != Some(false) {
                    let mut files = self
                        .rewind_previews
                        .remove(&pending.checkpoint_id)
                        .unwrap_or_default();
                    files.skipped_links = result
                        .get("skippedLinks")
                        .and_then(Value::as_u64)
                        .unwrap_or_default();
                    pending.files = Some(files);
                } else {
                    pending.files_failure = Some(files_failure(error()));
                }
                if pending.mode.includes_conversation() {
                    pending.step = RewindStep::Conversation;
                    let request = self.rewind_request(pending);
                    self.outgoing.push(request);
                    return Vec::new();
                }
                vec![match pending.files_failure {
                    Some(failure) => AgentEvent::RewindFailed {
                        checkpoint_id: pending.checkpoint_id,
                        mode: pending.mode,
                        failure,
                    },
                    None => AgentEvent::RewindCompleted {
                        checkpoint_id: pending.checkpoint_id,
                        mode: pending.mode,
                        prefill: None,
                        prefill_attachments: Vec::new(),
                        later_history: LaterHistory::Truncated,
                        files: pending.files,
                        incomplete: None,
                        undo_token: None,
                    },
                }]
            }
            RewindStep::Conversation => {
                let rewound =
                    succeeded && result.get("rewound").and_then(Value::as_bool) == Some(true);
                if rewound {
                    let restored = if pending.files_failure.is_some() {
                        RewindMode::Conversation
                    } else {
                        pending.mode
                    };
                    return vec![AgentEvent::RewindCompleted {
                        checkpoint_id: pending.checkpoint_id,
                        mode: restored,
                        prefill: result
                            .get("prefillText")
                            .and_then(Value::as_str)
                            .filter(|text| !text.is_empty())
                            .map(str::to_owned),
                        prefill_attachments: Vec::new(),
                        later_history: LaterHistory::Truncated,
                        files: pending.files,
                        incomplete: pending.files_failure.map(|failure| RewindIncomplete {
                            requested: pending.mode,
                            failure,
                        }),
                        undo_token: None,
                    }];
                }
                let failure = RewindFailure::new(
                    if succeeded {
                        conversation_refusal(result.get("reason").and_then(Value::as_str))
                    } else {
                        RewindFailureReason::Other
                    },
                    error(),
                );
                if pending.mode.includes_files() && pending.files_failure.is_none() {
                    // The files were restored before the conversation refused.
                    vec![AgentEvent::RewindCompleted {
                        checkpoint_id: pending.checkpoint_id,
                        mode: RewindMode::Files,
                        prefill: None,
                        prefill_attachments: Vec::new(),
                        later_history: LaterHistory::Truncated,
                        files: pending.files,
                        incomplete: Some(RewindIncomplete {
                            requested: pending.mode,
                            failure,
                        }),
                        undo_token: None,
                    }]
                } else {
                    vec![AgentEvent::RewindFailed {
                        checkpoint_id: pending.checkpoint_id,
                        mode: pending.mode,
                        failure,
                    }]
                }
            }
        }
    }

    /// Claude Code withdrew one of its own prompts, as it does for a pending
    /// `can_use_tool` when the turn is interrupted.
    pub(super) fn on_control_cancel_request(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let Some(request_id) = msg.get("request_id").and_then(Value::as_str) else {
            return Vec::new();
        };
        if self.pending_approvals.remove(request_id).is_some() {
            vec![AgentEvent::ApprovalResolved {
                request_id: request_id.to_owned(),
                decision: ApprovalDecision::Cancel,
            }]
        } else if self.pending_user_input.remove(request_id).is_some() {
            vec![AgentEvent::UserInputResolved {
                request_id: request_id.to_owned(),
                answers: serde_json::Map::new(),
            }]
        } else {
            Vec::new()
        }
    }

    pub(super) fn on_control_request(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let request = match msg.get("request") {
            Some(r) => r,
            None => return Vec::new(),
        };
        let request_id = match msg.get("request_id").and_then(Value::as_str) {
            Some(id) => id.to_string(),
            None => return Vec::new(),
        };
        match request.get("subtype").and_then(Value::as_str) {
            Some("can_use_tool") => {}
            Some("hook_callback") => return self.on_hook_callback(request_id, request),
            Some("elicitation") => return self.on_elicitation(request_id, request),
            // A dialog kind the host never declared is left unanswered: the
            // control schema has Claude Code discard an error and take any
            // other answer for the user's (2.1.323, `request_user_dialog`).
            Some("request_user_dialog") => return Vec::new(),
            // The Agent SDK's answer to a request it has no handler for
            // (0.3.323, `sdk.mjs`), which the control schema names a designed
            // refusal by its `error_code`.
            subtype => {
                let mut refusal = control_error(
                    &request_id,
                    &format!(
                        "Unsupported control request subtype: {}",
                        subtype.unwrap_or_default()
                    ),
                );
                refusal["response"]["error_code"] = json!("unsupported_request");
                self.outgoing.push(refusal);
                return Vec::new();
            }
        }
        let tool_name = request
            .get("tool_name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let input = request.get("input").cloned().unwrap_or_else(|| json!({}));
        // Claude Code's consent line; it may carry terminal escapes.
        let text = |key: &str| {
            request
                .get(key)
                .and_then(Value::as_str)
                .map(crate::strip_ansi)
                .filter(|text| !text.trim().is_empty())
        };
        let reason = text("decision_reason");
        let blocked_path = text("blocked_path");

        if tool_name == "AskUserQuestion" {
            let questions_raw = input.get("questions").cloned().unwrap_or_else(|| json!([]));
            let questions = parse_ask_user_questions(&input);
            self.pending_user_input.insert(
                request_id.clone(),
                PendingUserInput::AskUserQuestion {
                    questions: questions_raw,
                },
            );
            return vec![AgentEvent::UserInputRequested {
                request_id,
                questions,
                delivery: UserInputDelivery::Blocking,
            }];
        }

        if tool_name == EXIT_PLAN_MODE {
            return self.plan_approval(request_id, request, input);
        }

        let request_type = classify_claude_tool(&tool_name);

        let detail = approval_detail(&tool_name, &input);
        let kind = match request_type {
            ClaudeRequestType::FileRead => ApprovalKind::FileRead { detail },
            ClaudeRequestType::ExecCommand => ApprovalKind::ExecCommand {
                command: input
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                cwd: input.get("cwd").and_then(Value::as_str).map(str::to_string),
                description: input
                    .get("description")
                    .and_then(Value::as_str)
                    .filter(|description| !description.trim().is_empty())
                    .map(str::to_string),
            },
            ClaudeRequestType::FileChange => ApprovalKind::FileChange {
                changes: file_changes(&tool_name, &input),
            },
            ClaudeRequestType::ToolUse => ApprovalKind::ToolUse {
                name: tool_name.clone(),
                input: input.clone(),
                detail,
            },
        };

        let mut options = vec![
            ApprovalOption {
                id: "allow".into(),
                label: "Allow".into(),
                kind: ApprovalOptionKind::AllowOnce,
            },
            ApprovalOption {
                id: "deny".into(),
                label: "Deny".into(),
                kind: ApprovalOptionKind::RejectOnce,
            },
        ];
        let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
        for suggestion in request
            .get("permission_suggestions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let scope = suggestion
                .get("destination")
                .and_then(Value::as_str)
                .unwrap_or("session");
            if let Some((_, suggestions)) = groups.iter_mut().find(|(known, _)| known == scope) {
                suggestions.push(suggestion.clone());
            } else {
                groups.push((scope.to_owned(), vec![suggestion.clone()]));
            }
        }
        let mut suggestions = HashMap::new();
        for (index, (scope, group)) in groups.into_iter().enumerate() {
            let id = format!("allow_suggestion:{index}");
            let label = match scope.as_str() {
                "session" => "Allow and remember for this session".to_owned(),
                "userSettings" => "Allow and save to user settings".to_owned(),
                "projectSettings" => "Allow and save to project settings".to_owned(),
                "localSettings" => "Allow and save to local settings".to_owned(),
                _ => format!("Allow and remember in {scope}"),
            };
            options.push(ApprovalOption {
                id: id.clone(),
                label,
                kind: ApprovalOptionKind::AllowAlways,
            });
            suggestions.insert(id, json!(group));
        }
        self.pending_approvals
            .insert(request_id.clone(), PendingApproval { input, suggestions });
        let parent_item_id = request
            .get("agent_id")
            .and_then(Value::as_str)
            .and_then(|agent_id| self.task_tools.get(agent_id))
            .cloned();
        vec![AgentEvent::ApprovalRequested(ApprovalRequest {
            id: request_id,
            turn_id: self.current_turn_id.clone(),
            parent_item_id,
            kind,
            reason,
            blocked_path,
            options,
        })]
    }

    /// `ExitPlanMode`'s permission prompt: the plan item it decides on (from
    /// the tool's own item, emitted here when the prompt arrives first), then
    /// the decision with Claude Code's allow and deny under plan names.
    pub(super) fn plan_approval(
        &mut self,
        request_id: String,
        request: &Value,
        input: Value,
    ) -> Vec<AgentEvent> {
        let markdown = plan_markdown(&input);
        let item_id = request
            .get("tool_use_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("plan-{request_id}"));
        let mut events = Vec::new();
        let known = match self.tool_items.get(&item_id) {
            Some(ToolItem::Plan { markdown: shown }) => Some(*shown == markdown),
            _ => None,
        };
        if known != Some(true) {
            self.tool_items.insert(
                item_id.clone(),
                ToolItem::Plan {
                    markdown: markdown.clone(),
                },
            );
            let item = ThreadItem {
                id: item_id.clone(),
                parent_item_id: None,
                content: ItemContent::Plan {
                    markdown: markdown.clone(),
                },
            };
            events.push(if known.is_some() {
                AgentEvent::ItemUpdated(item)
            } else {
                AgentEvent::ItemStarted(item)
            });
        }
        self.pending_approvals.insert(
            request_id.clone(),
            PendingApproval {
                input,
                suggestions: HashMap::new(),
            },
        );
        let parent_item_id = request
            .get("agent_id")
            .and_then(Value::as_str)
            .and_then(|agent_id| self.task_tools.get(agent_id))
            .cloned();
        events.push(AgentEvent::ApprovalRequested(ApprovalRequest {
            id: request_id,
            turn_id: self.current_turn_id.clone(),
            parent_item_id,
            kind: ApprovalKind::Plan { item_id, markdown },
            reason: None,
            blocked_path: None,
            options: vec![
                ApprovalOption {
                    id: "allow".into(),
                    label: "Approve".into(),
                    kind: ApprovalOptionKind::AllowOnce,
                },
                ApprovalOption {
                    id: "deny".into(),
                    label: "Keep planning".into(),
                    kind: ApprovalOptionKind::RejectOnce,
                },
            ],
        }));
        events
    }
}
