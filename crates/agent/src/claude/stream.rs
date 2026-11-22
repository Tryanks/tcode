use super::*;

impl Mapper {
    pub(super) fn on_stream_event(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let event = match msg.get("event") {
            Some(e) => e,
            None => return Vec::new(),
        };
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                self.current_message_id = event
                    .get("message")
                    .and_then(|m| m.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let message = &event["message"];
                let mut events = self.observe_served_model(message, None);
                events.extend(self.observe_request_usage(
                    message.get("id").and_then(Value::as_str),
                    message.get("usage"),
                    true,
                ));
                events
            }
            // Claude 5 models stream a thinking block whose deltas carry no
            // text, so the block's start is the only sign the model is
            // thinking: a placeholder reasoning item renders as "Thinking…"
            // until text or the next block arrives.
            Some("content_block_start")
                if event.pointer("/content_block/type").and_then(Value::as_str)
                    == Some("thinking") =>
            {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                vec![AgentEvent::ItemStarted(ThreadItem {
                    id: self.block_item_id(index),
                    parent_item_id: None,
                    content: ItemContent::Reasoning {
                        text: String::new(),
                    },
                })]
            }
            Some("content_block_delta") => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                let delta = match event.get("delta") {
                    Some(d) => d,
                    None => return Vec::new(),
                };
                let (kind, text) = match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => (
                        DeltaKind::AssistantText,
                        delta.get("text").and_then(Value::as_str),
                    ),
                    Some("thinking_delta") => (
                        DeltaKind::ReasoningText,
                        delta
                            .get("thinking")
                            .and_then(Value::as_str)
                            .or_else(|| delta.get("text").and_then(Value::as_str)),
                    ),
                    // input_json_delta and friends: tool input is reconstructed
                    // from the (complete) `assistant` message instead.
                    _ => return Vec::new(),
                };
                let text = match text {
                    Some(t) if !t.is_empty() => t,
                    _ => return Vec::new(),
                };
                vec![AgentEvent::Delta {
                    item_id: self.block_item_id(index),
                    kind,
                    text: text.to_string(),
                }]
            }
            Some("message_delta") => {
                let mut events = self.observe_stop_reason(
                    event.pointer("/delta/stop_reason").and_then(Value::as_str),
                );
                if let Some(usage) = event.get("usage") {
                    let id = self.current_message_id.clone();
                    events.extend(self.observe_request_usage(id.as_deref(), Some(usage), false));
                }
                events
            }
            _ => Vec::new(),
        }
    }

    /// Stream deltas are cumulative fields within one request, never increments.
    /// Assistant messages can repeat an ID for parallel tool blocks.
    pub(super) fn observe_request_usage(
        &mut self,
        id: Option<&str>,
        usage: Option<&Value>,
        start: bool,
    ) -> Vec<AgentEvent> {
        let Some(id) = id else {
            return Vec::new();
        };
        if self.usage_message_id.as_deref() != Some(id) {
            if !self.usage_message_ids.insert(id.to_owned()) {
                return Vec::new();
            }
            self.usage_message_id = Some(id.to_owned());
            self.request_usage = json!({});
        }
        if let Some(fields) = usage.and_then(Value::as_object) {
            for key in [
                "input_tokens",
                "cache_read_input_tokens",
                "cache_creation_input_tokens",
                "output_tokens",
            ] {
                if let Some(value) = fields.get(key).filter(|v| v.is_u64()) {
                    // Non-streaming assistant output is a placeholder; never regress
                    // the cumulative output already observed in the stream.
                    if key != "output_tokens" || value.as_u64() >= self.request_usage[key].as_u64()
                    {
                        self.request_usage[key] = value.clone();
                    }
                }
            }
        }
        self.latest_usage.turn_processed_tokens = None;
        let mapped = map_usage(&self.request_usage, None);
        self.latest_usage.input_tokens = mapped.input_tokens;
        self.latest_usage.cached_input_tokens = mapped.cached_input_tokens;
        self.latest_usage.output_tokens = mapped.output_tokens;
        // Claude's context observation counts the latest request's input and caches.
        // Output deltas describe generated traffic, not a new input observation.
        self.latest_usage.used_tokens = mapped.input_tokens.map(|input| {
            input
                .saturating_add(mapped.cached_input_tokens.unwrap_or(0))
                .saturating_add(
                    self.request_usage["cache_creation_input_tokens"]
                        .as_u64()
                        .unwrap_or(0),
                )
        });
        self.latest_usage.freshness = if self.latest_usage.used_tokens.is_some() {
            crate::ContextFreshness::Current
        } else {
            crate::ContextFreshness::AwaitingObservation
        };
        if usage.is_some() || start {
            vec![AgentEvent::TokenUsage(self.latest_usage)]
        } else {
            Vec::new()
        }
    }

    pub(super) fn block_item_id(&self, index: u64) -> String {
        match &self.current_message_id {
            Some(id) => format!("{id}:{index}"),
            None => format!("msg:{index}"),
        }
    }

    /// `message`'s model, when it is not the one Claude Code last reported;
    /// `from` is the model Claude Code says it switched away from, where it
    /// says one.
    pub(super) fn observe_served_model(
        &mut self,
        message: &Value,
        from: Option<&Value>,
    ) -> Vec<AgentEvent> {
        let Some(model) = message
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty() && *model != "<synthetic>")
        else {
            return Vec::new();
        };
        if self.last_served_model.as_deref() == Some(model) {
            return Vec::new();
        }
        let last = self.last_served_model.replace(model.to_owned());
        vec![AgentEvent::ServedModel {
            model: model.to_owned(),
            from: from.and_then(Value::as_str).map(str::to_owned).or(last),
            reason: None,
        }]
    }

    pub(super) fn on_assistant(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let message = match msg.get("message") {
            Some(m) => m,
            None => return Vec::new(),
        };
        let mut out = self.observe_request_usage(
            message.get("id").and_then(Value::as_str),
            message.get("usage"),
            false,
        );
        let msg_id = message
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("msg")
            .to_string();
        let state = self.assistant_messages.entry(msg_id.clone()).or_default();
        let model = message.get("model").and_then(Value::as_str);
        let model_changed = state.model.as_deref() != model;
        if model_changed {
            state.model = model.map(str::to_owned);
        }
        let content = message.get("content").and_then(Value::as_array);
        let first_index = state.blocks_seen;
        state.blocks_seen += content.map_or(0, Vec::len);
        // Split assistant blocks retain the message_start model even after a
        // fallback boundary; observing an unchanged header would undo the switch.
        if model_changed {
            out.extend(self.observe_served_model(message, None));
        }
        out.extend(self.observe_stop_reason(message.get("stop_reason").and_then(Value::as_str)));
        let Some(content) = content else { return out };
        // Thinking blocks the server tagged as narration carry its summary of
        // the prose the model wrote to the user between tool calls; Claude Code
        // shows them as messages, not as reasoning.
        let narration: Vec<u64> = msg
            .get("narration_block_indexes")
            .and_then(Value::as_array)
            .map(|indexes| indexes.iter().filter_map(Value::as_u64).collect())
            .unwrap_or_default();

        for (offset, block) in content.iter().enumerate() {
            let index = first_index + offset;
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                    out.push(AgentEvent::ItemCompleted(ThreadItem {
                        id: format!("{msg_id}:{index}"),
                        parent_item_id: None,
                        content: ItemContent::AssistantMessage {
                            text: text.to_string(),
                            summarized: false,
                        },
                    }));
                }
                Some("thinking") => {
                    let text = block
                        .get("thinking")
                        .and_then(Value::as_str)
                        .or_else(|| block.get("text").and_then(Value::as_str))
                        .unwrap_or("");
                    // The CLI redacts thinking content in this line even when it
                    // streamed the deltas; completing with "" would blank the
                    // reasoning the user just watched arrive.
                    let content = if text.trim().is_empty() {
                        None
                    } else if narration.contains(&(offset as u64)) {
                        Some(ItemContent::AssistantMessage {
                            text: text.to_string(),
                            summarized: true,
                        })
                    } else {
                        Some(ItemContent::Reasoning {
                            text: text.to_string(),
                        })
                    };
                    if let Some(content) = content {
                        out.push(AgentEvent::ItemCompleted(ThreadItem {
                            id: format!("{msg_id}:{index}"),
                            parent_item_id: None,
                            content,
                        }));
                    }
                }
                Some("tool_use") => {
                    out.extend(self.on_tool_use(block));
                }
                Some("fallback") => {
                    out.extend(self.observe_served_model(&block["to"], block["from"].get("model")))
                }
                _ => {}
            }
        }
        out
    }

    pub(super) fn observe_stop_reason(&mut self, reason: Option<&str>) -> Vec<AgentEvent> {
        let Some(reason) = reason.filter(|reason| !reason.is_empty()) else {
            return Vec::new();
        };
        let message = match reason {
            "refusal" => Some("Request declined by safety classifiers (stop_reason: refusal)"),
            "max_tokens" => Some("Response truncated: max_tokens limit reached"),
            _ => None,
        };
        if message.is_none() || self.warned_stop_reason.as_deref() == Some(reason) {
            return Vec::new();
        }
        self.warned_stop_reason = Some(reason.to_owned());
        vec![AgentEvent::Warning {
            message: message.unwrap().to_owned(),
        }]
    }

    pub(super) fn on_tool_use(&mut self, block: &Value) -> Vec<AgentEvent> {
        let tool_use_id = match block.get("id").and_then(Value::as_str) {
            Some(id) => id.to_string(),
            None => return Vec::new(),
        };
        let name = block
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let input = block.get("input").cloned().unwrap_or_else(|| json!({}));

        // TodoWrite drives the structured plan/task sidebar, not the timeline.
        if is_todo_tool(&name) {
            if let Some(steps) = todo_plan(&name, &input) {
                return vec![AgentEvent::PlanUpdated {
                    turn_id: self.current_turn_id.clone(),
                    explanation: None,
                    steps,
                }];
            }
            return Vec::new();
        }

        let (item, content) = if is_agent_tool(&name.to_lowercase()) {
            let content = spawned_subagent(&input);
            let item = ToolItem::subagent(&content, None).expect("a spawn snapshot is a Subagent");
            (item, content)
        } else {
            ToolItem::started(name, input)
        };

        self.tool_items.insert(tool_use_id.clone(), item);
        vec![AgentEvent::ItemStarted(ThreadItem {
            id: tool_use_id,
            parent_item_id: None,
            content,
        })]
    }

    pub(super) fn on_user(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let content = msg
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array);
        let mut out = Vec::new();
        let has_tool_result = content.is_some_and(|content| {
            content
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        });
        if let Some(uuid) = msg.get("uuid").and_then(Value::as_str)
            && let Some(request_id) = self.steer_uuids.remove(uuid)
        {
            out.push(AgentEvent::SteerCheckpoint {
                request_id,
                checkpoint_id: uuid.to_owned(),
            });
        } else if self.awaiting_turn_checkpoint
            && !has_tool_result
            && let (Some(turn_id), Some(checkpoint_id)) = (
                self.current_turn_id.clone(),
                msg.get("uuid").and_then(Value::as_str),
            )
        {
            self.awaiting_turn_checkpoint = false;
            out.push(AgentEvent::TurnCheckpoint {
                turn_id,
                checkpoint_id: checkpoint_id.to_owned(),
            });
        }
        for block in content.into_iter().flatten() {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let tool_use_id = match block.get("tool_use_id").and_then(Value::as_str) {
                Some(id) => id.to_string(),
                None => continue,
            };
            let launched_in_background = msg.pointer("/tool_use_result/backgroundTaskId").is_some();
            self.deferred_background_moves.remove(&tool_use_id);
            let item = match self.tool_items.remove(&tool_use_id) {
                Some(i) => i,
                None => continue,
            };
            // A background Agent's tool_result only acknowledges the launch;
            // the subagent keeps running until its task_notification.
            if matches!(item, ToolItem::Subagent { .. })
                && msg
                    .pointer("/tool_use_result/status")
                    .and_then(Value::as_str)
                    == Some("async_launched")
            {
                if let Some(agent_id) = msg
                    .pointer("/tool_use_result/agentId")
                    .and_then(Value::as_str)
                {
                    self.task_tools
                        .insert(agent_id.to_owned(), tool_use_id.clone());
                    out.extend(self.link_background_tasks());
                }
                let ToolItem::Subagent { parent_item_id, .. } = &item else {
                    unreachable!("matched as a subagent");
                };
                let parent_item_id = parent_item_id.clone();
                self.tool_items.insert(tool_use_id.clone(), item);
                out.extend(self.update_subagent(&tool_use_id, ItemStatus::InProgress, None));
                out.push(AgentEvent::ItemBackgrounded {
                    item_id: tool_use_id.clone(),
                    parent_item_id,
                });
                continue;
            }
            let is_error = block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            // Where the session works after it entered or left a worktree:
            // the worktree, and on leaving it the directory it worked in
            // before, which `restoredCwd` names only as the shell's last cd.
            let result_path = |key: &str| {
                msg.pointer(&format!("/tool_use_result/{key}"))
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
            };
            let moved_to = match &item {
                ToolItem::Tool { name, .. } if !is_error => match name.as_str() {
                    "EnterWorktree" => {
                        let to = result_path("worktreePath");
                        if to.is_some() {
                            self.before_worktree = self.cwd.clone();
                        }
                        to
                    }
                    "ExitWorktree" => self
                        .before_worktree
                        .take()
                        .or_else(|| result_path("restoredCwd")),
                    _ => None,
                },
                _ => None,
            };
            let output =
                ToolOutput::from_content(block.get("content").unwrap_or(&Value::Null)).text;
            let status = tool_result_status(is_error, &output);
            let mut parent_item_id = None;
            let backgrounded = launched_in_background && matches!(item, ToolItem::Command { .. });
            if let Some(task_id) = msg
                .pointer("/tool_use_result/backgroundTaskId")
                .and_then(Value::as_str)
                .filter(|_| backgrounded)
            {
                self.task_tools
                    .entry(task_id.to_owned())
                    .or_insert_with(|| tool_use_id.clone());
                out.extend(self.link_background_tasks());
            }
            let content = match item {
                ToolItem::Command { command, .. } if backgrounded => {
                    self.tool_items.insert(
                        tool_use_id.clone(),
                        ToolItem::Command {
                            command: command.clone(),
                            output: output.clone(),
                        },
                    );
                    ItemContent::CommandExecution {
                        command,
                        input: Vec::new(),
                        output,
                        exit_code: None,
                        status: ItemStatus::InProgress,
                    }
                }
                item @ (ToolItem::Command { .. }
                | ToolItem::File { .. }
                | ToolItem::Tool { .. }
                | ToolItem::Plan { .. }) => item.finished(block, msg.get("tool_use_result")),
                ToolItem::Subagent {
                    agent_type,
                    description,
                    summary,
                    model,
                    effort,
                    parent_item_id: parent,
                    ..
                } => {
                    parent_item_id = parent;
                    let result = msg.get("tool_use_result");
                    ItemContent::Subagent {
                        agent_type,
                        description,
                        status,
                        summary: handback_summary(
                            result.and_then(|result| result.get("handback")),
                            result.and_then(|result| result.get("handbackReport")),
                            summary.or_else(|| (!output.trim().is_empty()).then_some(output)),
                        ),
                        model,
                        effort,
                    }
                }
            };
            let event = if matches!(
                &content,
                ItemContent::CommandExecution {
                    status: ItemStatus::InProgress,
                    ..
                }
            ) {
                AgentEvent::ItemUpdated
            } else {
                AgentEvent::ItemCompleted
            };
            let is_subagent = matches!(content, ItemContent::Subagent { .. });
            let changed = matches!(content, ItemContent::CommandExecution { .. })
                .then(|| command_changes(&tool_use_id, msg.get("tool_use_result")))
                .flatten();
            let event = event(ThreadItem {
                id: tool_use_id.clone(),
                parent_item_id,
                content,
            });
            if is_subagent {
                out.extend(self.gate_subagent_terminal(&tool_use_id, event));
            } else {
                out.push(event);
            }
            out.extend(changed.map(AgentEvent::ItemCompleted));
            if backgrounded {
                out.push(AgentEvent::ItemBackgrounded {
                    item_id: tool_use_id.clone(),
                    parent_item_id: None,
                });
            }
            out.extend(moved_to.and_then(|to| self.move_session(to)));
        }
        out
    }
}
