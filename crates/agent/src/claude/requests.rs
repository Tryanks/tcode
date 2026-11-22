use super::*;

impl Mapper {
    /// Allocate the next synthesized turn id and mark it in-flight.
    pub(super) fn start_turn(&mut self) -> String {
        self.current_message_id = None;
        self.usage_message_id = None;
        self.request_usage = json!({});
        if self.latest_usage.freshness == crate::ContextFreshness::Current {
            self.latest_usage.freshness = crate::ContextFreshness::LastKnown;
        }
        self.turn_counter += 1;
        let id = format!("turn-{}", self.turn_counter);
        self.current_turn_id = Some(id.clone());
        self.awaiting_turn_checkpoint = true;
        self.warned_stop_reason = None;
        id
    }

    pub(super) fn next_control_id(&mut self) -> String {
        self.control_counter += 1;
        format!("tcode-ctrl-{}", self.control_counter)
    }

    pub(super) fn begin_rewind(&mut self, checkpoint_id: String, mode: RewindMode) -> Value {
        let step = if !mode.includes_files() {
            RewindStep::Conversation
        } else if self.current_turn_id.is_some() {
            RewindStep::Interrupt
        } else {
            RewindStep::Files
        };
        self.rewind_request(PendingRewind {
            checkpoint_id,
            mode,
            step,
            files: None,
            files_failure: None,
        })
    }

    /// A dry run of the files a rewind would restore; `None` when `mode`
    /// restores none, which needs no preview.
    pub(super) fn begin_preview(
        &mut self,
        checkpoint_id: String,
        mode: RewindMode,
    ) -> Option<Value> {
        mode.includes_files().then(|| {
            self.rewind_request(PendingRewind {
                checkpoint_id,
                mode,
                step: RewindStep::Preview,
                files: None,
                files_failure: None,
            })
        })
    }

    pub(super) fn rewind_request(&mut self, pending: PendingRewind) -> Value {
        let checkpoint_id = pending.checkpoint_id.as_str();
        if matches!(
            pending.step,
            RewindStep::Interrupt | RewindStep::Conversation
        ) && self.current_turn_id.is_some()
        {
            self.interrupt_pending = true;
        }
        let request = match pending.step {
            RewindStep::Interrupt => json!({ "subtype": "interrupt" }),
            RewindStep::Files => {
                json!({ "subtype": "rewind_files", "user_message_id": checkpoint_id })
            }
            RewindStep::Preview => json!({
                "subtype": "rewind_files",
                "user_message_id": checkpoint_id,
                "dry_run": true,
            }),
            // A target before a compaction boundary resolves to the summary
            // that replaced it, as the native menu allows.
            RewindStep::Conversation => json!({
                "subtype": "rewind_conversation",
                "target_message_uuid": checkpoint_id,
                "interrupt_if_running": true,
                "allow_summarized_target": true,
            }),
        };
        let message = self.control_request(request);
        let request_id = message["request_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        self.pending_rewinds.insert(request_id, pending);
        message
    }

    pub(super) fn control_request(&mut self, request: Value) -> Value {
        json!({
            "type": "control_request",
            "request_id": self.next_control_id(),
            "request": request,
        })
    }

    /// Client → CLI interrupt control request.
    pub(super) fn interrupt_request(&mut self) -> Value {
        if self.current_turn_id.is_some() {
            self.interrupt_pending = true;
        }
        self.control_request(json!({ "subtype": "interrupt" }))
    }

    /// Client → CLI `set_permission_mode` control request. Wire shape verified
    /// against `@anthropic-ai/claude-agent-sdk` v0.3.170 (`browser-sdk.js`):
    /// `request(e)` wraps the payload as
    /// `{request_id, type:"control_request", request:e}`, and
    /// `setPermissionMode(m)` sends `{subtype:"set_permission_mode", mode:m}`.
    pub(super) fn set_permission_mode_request_str(&mut self, mode: &str) -> Value {
        let message = self.control_request(json!({
            "subtype": "set_permission_mode",
            "mode": mode,
        }));
        let request_id = message["request_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        self.pending_permission_modes
            .insert(request_id, mode.to_owned());
        message
    }

    /// One `stop_task` for `task_id`, or for every task the process runs: the
    /// CLI has no way to stop them together.
    pub(super) fn stop_task_requests(&mut self, task_id: Option<String>) -> Vec<Value> {
        let tasks = match task_id {
            Some(id) => vec![id],
            None => self
                .background_tasks
                .iter()
                .map(|task| task.id.clone())
                .collect(),
        };
        tasks
            .into_iter()
            .map(|task_id| {
                self.control_request(json!({ "subtype": "stop_task", "task_id": task_id }))
            })
            .collect()
    }

    /// Ctrl+B for the call `item_id`: Claude returns it to the model and
    /// its command or agent goes on as a background task.
    pub(super) fn move_to_background_request(&mut self, item_id: String) -> Value {
        let message =
            self.control_request(json!({ "subtype": "background_tasks", "tool_use_id": item_id }));
        if let Some(request_id) = message.get("request_id").and_then(Value::as_str) {
            let task_announced = self.task_tools.values().any(|call| *call == item_id);
            self.pending_background_moves.insert(
                request_id.to_owned(),
                PendingBackgroundMove {
                    item_id,
                    task_announced,
                },
            );
        }
        message
    }

    /// Claude refused to move a call: too early, it is asked again once
    /// Claude announces the call's task; too late, there is nothing to move.
    pub(super) fn on_background_move_refused(&mut self, refused: PendingBackgroundMove) {
        if refused.task_announced || !self.tool_items.contains_key(&refused.item_id) {
            return;
        }
        // The task was announced after Claude weighed the request.
        if self
            .task_tools
            .values()
            .any(|call| *call == refused.item_id)
        {
            let request = self.move_to_background_request(refused.item_id);
            self.outgoing.push(request);
        } else {
            self.deferred_background_moves.insert(refused.item_id);
        }
    }

    /// The list again, once the call is known of a listed task that had
    /// none: Claude lists a task before it says which call started it.
    pub(super) fn link_background_tasks(&mut self) -> Option<AgentEvent> {
        let mut linked = false;
        for task in &mut self.background_tasks {
            if task.item_id.is_none()
                && let Some(item_id) = self.task_tools.get(&task.id)
            {
                task.item_id = Some(item_id.clone());
                linked = true;
            }
        }
        linked.then(|| AgentEvent::BackgroundTaskList {
            tasks: self.background_tasks.clone(),
        })
    }

    /// The first request bringing the process to `target`, `None` when it runs
    /// that already. `Err` when the change takes a restart.
    pub(super) fn begin_settings(
        &mut self,
        mut target: ClaudeLaunchOptions,
    ) -> Result<Option<Value>, ()> {
        if self.applied_settings.needs_restart_for(&target) {
            return Err(());
        }
        target.auto_compact_window = self.applied_settings.auto_compact_window;
        let flags = self.applied_settings.flag_changes(&target);
        let flags = (!flags.is_empty()).then(|| {
            json!({
                "subtype": "apply_flag_settings",
                "settings": flags,
            })
        });
        let (request, flags) = if self.applied_settings.model_id != target.model_id {
            (
                json!({ "subtype": "set_model", "model": target.model_id }),
                flags,
            )
        } else if let Some(flags) = flags {
            (flags, None)
        } else {
            self.applied_settings = target;
            return Ok(None);
        };
        let message = self.control_request(request);
        self.pending_settings = Some(PendingSettings {
            request_id: message["request_id"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            target,
            flags,
        });
        Ok(Some(message))
    }

    /// One step of a live settings change answered: send the flags that waited
    /// for the model, or report the outcome.
    pub(super) fn on_settings_response(&mut self, response: &Value) -> Vec<AgentEvent> {
        let Some(mut pending) = self.pending_settings.take() else {
            return Vec::new();
        };
        if response.get("subtype").and_then(Value::as_str) != Some("success") {
            log::warn!("claude: live settings change refused: {response}");
            return vec![AgentEvent::SettingsApplied { applied: false }];
        }
        self.applied_settings.model_id = pending.target.model_id.clone();
        if let Some(flags) = pending.flags.take() {
            let message = self.control_request(flags);
            pending.request_id = message["request_id"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            self.outgoing.push(message);
            self.pending_settings = Some(pending);
            return Vec::new();
        }
        self.applied_settings = pending.target;
        let read = self.settings_read_request();
        self.outgoing.push(read);
        vec![AgentEvent::SettingsApplied { applied: true }]
    }

    /// `get_settings`, whose `applied` is what the next message runs with
    /// (2.1.322): the model, and the effort it resolved, `null` for a model
    /// without one. The stream reports neither until a turn, and the effort
    /// never.
    pub(super) fn settings_read_request(&mut self) -> Value {
        let message = self.control_request(json!({ "subtype": "get_settings" }));
        self.pending_settings_read = message["request_id"].as_str().map(str::to_owned);
        message
    }

    pub(super) fn on_settings_read(&mut self, response: &Value) -> Vec<AgentEvent> {
        let applied = response
            .pointer("/response/applied")
            .filter(|_| response.get("subtype").and_then(Value::as_str) == Some("success"));
        if applied.is_none() {
            log::warn!("claude: get_settings was not answered: {response}");
        }
        let mut events = Vec::new();
        if let Some(model) = applied
            .and_then(|applied| applied.get("model"))
            .and_then(Value::as_str)
        {
            events.extend(self.observe_model(model));
        }
        let effort = applied.map_or(Value::Null, |applied| self.tcode_effort(applied));
        events.push(AgentEvent::ProviderOptions {
            descriptors: Vec::new(),
            selections: vec![OptionSelection {
                id: crate::EFFORT_OPTION_ID.into(),
                value: effort,
            }],
        });
        // Claude Code records the files a Bash command changed (`bashEditDiff`)
        // in default modes only when asked. Asked in the flag layer, which
        // outranks the user's own settings, so only where none of theirs
        // decides it.
        if response
            .pointer("/response/effective")
            .and_then(Value::as_object)
            .is_some_and(|effective| !effective.contains_key("bashEditDiffEnabled"))
        {
            let message = self.control_request(json!({
                "subtype": "apply_flag_settings",
                "settings": { "bashEditDiffEnabled": true },
            }));
            self.pending_edit_diff = message["request_id"].as_str().map(str::to_owned);
            self.outgoing.push(message);
        }
        events
    }

    /// The effort Claude applied, in Tcode's values: the one sent when it
    /// names that level, else the first of the model's that does, else
    /// Claude's own word. `null` when Claude has none, or when the value sent
    /// is one the manifest's `effortMap` launches with no level, so that what
    /// Claude applied is not it.
    pub(super) fn tcode_effort(&self, applied: &Value) -> Value {
        let Some(effort) = applied.get("effort").and_then(Value::as_str) else {
            return Value::Null;
        };
        let ultracode = applied.get("ultracode").and_then(Value::as_bool) == Some(true);
        let catalog = crate::claude_manifest::current();
        let Some(entry) = self
            .applied_settings
            .model
            .as_deref()
            .or(self.reported_model.as_deref())
            .and_then(|model| catalog.model(model))
        else {
            return json!(effort);
        };
        let sent = self.applied_settings.effort_value.as_deref();
        if sent.is_some_and(|sent| entry.cli_effort(sent).is_none()) {
            return Value::Null;
        }
        let names = |value: &str| {
            entry.cli_effort(value).as_deref() == Some(effort)
                && (value == "ultracode") == ultracode
        };
        if let Some(sent) = sent.filter(|sent| names(sent)) {
            return json!(sent);
        }
        entry
            .spec
            .options
            .iter()
            .find_map(|option| match option {
                OptionDescriptor::Select { id, options, .. } if id == crate::EFFORT_OPTION_ID => {
                    options.iter().find(|option| names(&option.value))
                }
                _ => None,
            })
            .map_or_else(|| json!(effort), |option| json!(option.value))
    }

    /// The model Claude names, spelled as Tcode launched it when it is that
    /// one: the CLI resolves an alias and adds the window's suffix.
    pub(super) fn tcode_model(&self, reported: &str) -> String {
        let catalog = crate::claude_manifest::current();
        let canonical = |model: &str| {
            catalog.model(model).map_or_else(
                || strip_context_window_suffix(model).to_owned(),
                |entry| entry.spec.id.clone(),
            )
        };
        match self.applied_settings.model.as_deref() {
            Some(launched) if canonical(launched) == canonical(reported) => launched.to_owned(),
            _ => canonical(reported),
        }
    }

    /// A model Claude reports the session set to, when it is news.
    pub(super) fn observe_model(&mut self, reported: &str) -> Option<AgentEvent> {
        let model = self.tcode_model(reported);
        if self.reported_model.as_deref() == Some(model.as_str()) {
            return None;
        }
        self.reported_model = Some(model.clone());
        Some(AgentEvent::SessionModel { model })
    }

    /// Fast mode as `fast_mode_state` reports it, when it is news: on in a
    /// cooldown too, which only waits out a rate limit (2.1.322).
    pub(super) fn observe_fast_mode(&mut self, state: Option<&Value>) -> Option<AgentEvent> {
        let on = match state?.as_str()? {
            "off" => false,
            "on" | "cooldown" => true,
            _ => return None,
        };
        if self.reported_fast_mode == Some(on) {
            return None;
        }
        self.reported_fast_mode = Some(on);
        Some(AgentEvent::ProviderOptions {
            descriptors: Vec::new(),
            selections: vec![OptionSelection {
                id: "fastMode".into(),
                value: json!(on),
            }],
        })
    }

    /// Build the `control_response` answering a pending `can_use_tool` prompt.
    pub(super) fn build_approval_response(
        &mut self,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> Option<Value> {
        let pending = self.pending_approvals.get(request_id)?;
        let response = match decision {
            ApprovalDecision::Option(id) if id == "allow" => json!({
                "behavior": "allow", "updatedInput": pending.input,
            }),
            ApprovalDecision::Option(id) if id == "deny" => json!({
                "behavior": "deny", "message": "User declined tool execution.",
            }),
            ApprovalDecision::Option(id) => {
                let suggestions = pending.suggestions.get(&id)?;
                json!({ "behavior": "allow", "updatedInput": pending.input, "updatedPermissions": suggestions })
            }
            ApprovalDecision::Feedback(text) => json!({
                "behavior": "deny", "message": text,
            }),
            ApprovalDecision::Cancel => json!({
                "behavior": "deny", "message": "User cancelled tool execution.",
            }),
        };
        self.pending_approvals.remove(request_id);
        Some(control_response(request_id, response))
    }

    /// The `control_response` giving pending questions the user's `answers`;
    /// `None` for an unknown request id. No answers at all is the user
    /// dismissing an elicitation, which MCP calls `cancel`; answers that are
    /// all empty are a form submitted with nothing filled in, an `accept`.
    pub(super) fn build_user_input_response(
        &mut self,
        request_id: &str,
        answers: &serde_json::Map<String, Value>,
    ) -> Option<Value> {
        let response = match self.pending_user_input.remove(request_id)? {
            PendingUserInput::AskUserQuestion { questions } => json!({
                "behavior": "allow",
                "updatedInput": { "questions": questions, "answers": answers }
            }),
            PendingUserInput::Elicitation { .. } if answers.is_empty() => {
                json!({ "action": "cancel" })
            }
            PendingUserInput::Elicitation { fields } => json!({
                "action": "accept",
                "content": elicitation_content(&fields, answers),
            }),
        };
        Some(control_response(request_id, response))
    }

    /// An `elicitation` control request: an MCP server asking the user for
    /// input. A form is asked as questions. What questions cannot ask (a URL
    /// to visit, a field of another shape) is declined, as the Agent SDK
    /// answers for a host with no elicitation handler, and the user is told.
    pub(super) fn on_elicitation(
        &mut self,
        request_id: String,
        request: &Value,
    ) -> Vec<AgentEvent> {
        let Some((questions, fields)) = elicitation_questions(request) else {
            self.outgoing.push(control_response(
                &request_id,
                json!({ "action": "decline" }),
            ));
            let text = |key: &str| request.get(key).and_then(Value::as_str).unwrap_or_default();
            let mut message = format!(
                "The MCP server {} asked for input Tcode cannot show, and was declined: {}",
                text("mcp_server_name"),
                text("message")
            );
            if let Some(url) = request.get("url").and_then(Value::as_str) {
                message.push_str(&format!(" ({url})"));
            }
            return vec![AgentEvent::Warning { message }];
        };
        self.pending_user_input
            .insert(request_id.clone(), PendingUserInput::Elicitation { fields });
        vec![AgentEvent::UserInputRequested {
            request_id,
            questions,
            delivery: UserInputDelivery::Blocking,
        }]
    }

    /// The `initialize` control request registering the worktree hooks.
    pub(super) fn initialize_request(&mut self) -> Value {
        let message = self.control_request(json!({
            "subtype": "initialize",
            "hooks": {
                "WorktreeCreate": [
                    { "hookCallbackIds": [WORKTREE_CREATE_CALLBACK], "onFailure": "block" }
                ],
                "WorktreeRemove": [
                    { "hookCallbackIds": [WORKTREE_REMOVE_CALLBACK], "onFailure": "block" }
                ],
            },
        }));
        self.pending_initialize = message["request_id"].as_str().map(str::to_owned);
        message
    }

    /// A `hook_callback` control request: a worktree hook becomes a request to
    /// the host; any other is answered with no opinion.
    pub(super) fn on_hook_callback(
        &mut self,
        request_id: String,
        request: &Value,
    ) -> Vec<AgentEvent> {
        let input = request.get("input").unwrap_or(&Value::Null);
        let field = |key: &str| input.get(key).and_then(Value::as_str);
        let worktree = match field("hook_event_name") {
            Some("WorktreeCreate") => {
                field("cwd")
                    .zip(field("name"))
                    .map(|(cwd, name)| WorktreeRequest::Create {
                        cwd: PathBuf::from(cwd),
                        name: name.to_owned(),
                    })
            }
            Some("WorktreeRemove") => field("worktree_path").map(|path| WorktreeRequest::Remove {
                path: PathBuf::from(path),
            }),
            _ => None,
        };
        let Some(worktree) = worktree else {
            log::debug!("claude: answering hook_callback {request} with no opinion");
            self.outgoing.push(control_response(&request_id, json!({})));
            return Vec::new();
        };
        self.pending_worktrees
            .insert(request_id.clone(), worktree.clone());
        vec![AgentEvent::WorktreeRequested {
            request_id,
            request: worktree,
        }]
    }

    /// The `control_response` giving a pending worktree hook the host's
    /// `answer`, and the request it answers; `None` for an unknown request id.
    pub(super) fn worktree_response(
        &mut self,
        request_id: &str,
        answer: &WorktreeAnswer,
    ) -> Option<(Value, WorktreeRequest)> {
        let request = self.pending_worktrees.remove(request_id)?;
        let refusal = |error: &str| control_error(request_id, error);
        let response = match (&request, answer) {
            (WorktreeRequest::Create { cwd, name }, WorktreeAnswer::Ready { path, .. }) => {
                self.worktrees.push(path.clone());
                if let Some(agent_id) = name.strip_prefix(SUBAGENT_WORKTREE_PREFIX) {
                    self.subagent_worktrees
                        .insert(agent_id.to_owned(), (cwd.clone(), path.clone()));
                }
                control_response(
                    request_id,
                    json!({ "hookSpecificOutput": {
                        "hookEventName": "WorktreeCreate",
                        "worktreePath": path,
                    }}),
                )
            }
            (WorktreeRequest::Remove { .. }, WorktreeAnswer::Removed) => {
                control_response(request_id, json!({}))
            }
            (_, WorktreeAnswer::Refused { error }) => refusal(error),
            (_, mismatched) => {
                log::warn!("claude: {mismatched:?} does not answer {request:?}");
                refusal("the host answered another kind of worktree request")
            }
        };
        Some((response, request))
    }

    /// The session works in `to` from now on, as Claude reported it: a move, unless
    /// that is where it works already.
    pub(super) fn move_session(&mut self, to: PathBuf) -> Option<AgentEvent> {
        let from = self.cwd.as_ref()?;
        if same_directory(from, &to) {
            return None;
        }
        // Claude reports directories resolved; the one Tcode started it in keeps
        // Tcode's spelling.
        let to = match &self.launch_cwd {
            Some(launch) if same_directory(launch, &to) => launch.clone(),
            _ => to,
        };
        let from = self.cwd.replace(to.clone())?;
        Some(AgentEvent::SessionMoved {
            from,
            to,
            parent_item_id: None,
        })
    }
}
