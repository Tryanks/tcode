use super::*;

impl Actor {
    pub(super) async fn handle_subagent_activity(&mut self, activity: &Value) {
        let Some(activity_id) = activity.get("id").and_then(Value::as_str) else {
            return;
        };
        let Some(thread_id) = activity.get("agentThreadId").and_then(Value::as_str) else {
            return;
        };
        // Codex 0.154 also reports the root thread's own activity (`agentPath`
        // "/root"); that is not a child and would otherwise become a capsule
        // that never completes.
        if thread_id == self.thread_id {
            return;
        }
        let kind = activity
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("started");
        let dedupe_key = format!("{activity_id}\0{thread_id}\0{kind}");
        if !self.seen_subagent_activities.insert(dedupe_key) {
            return;
        }
        let parent_id = if kind == "started" {
            self.subagent_parent_by_thread
                .entry(thread_id.to_owned())
                .or_insert_with(|| activity_id.to_owned())
                .clone()
        } else {
            self.subagent_parent_by_thread
                .entry(thread_id.to_owned())
                .or_insert_with(|| format!("codex-subagent:{thread_id}"))
                .clone()
        };
        let path = activity
            .get("agentPath")
            .and_then(Value::as_str)
            .unwrap_or("subagent");
        let spawn_input = self
            .items
            .get(&parent_id)
            .or_else(|| self.items.get(activity_id))
            .and_then(|item| match &item.content {
                ItemContent::ToolCall { input, .. } => Some(input),
                _ => None,
            });
        let (agent_type, description) = spawn_input
            .map(|input| {
                (
                    input
                        .get("agent_type")
                        .and_then(Value::as_str)
                        .unwrap_or_else(|| path.rsplit('/').next().unwrap_or("subagent"))
                        .to_owned(),
                    input
                        .get("description")
                        .or_else(|| input.get("prompt"))
                        .and_then(Value::as_str)
                        .unwrap_or(path)
                        .to_owned(),
                )
            })
            .unwrap_or_else(|| {
                (
                    path.rsplit('/').next().unwrap_or("subagent").to_owned(),
                    path.to_owned(),
                )
            });
        // Activity notifications can arrive without a spawn ToolCall. Missing
        // arguments do not establish inheritance; read the child's own metadata.
        let override_str = |key: &str| {
            spawn_input
                .and_then(|input| input.get(key))
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
        };
        let model = override_str("model");
        let effort = override_str("reasoning_effort").or_else(|| override_str("reasoningEffort"));
        let needs_metadata = !self.subagents.contains_key(&parent_id);
        let subagent = self
            .subagents
            .entry(parent_id.clone())
            .or_insert_with(|| CodexSubagent {
                agent_type,
                description,
                model,
                effort,
            })
            .clone();
        let status = match kind {
            "completed" => ItemStatus::Completed,
            "interrupted" => ItemStatus::Interrupted,
            // Messaging a finished child does not start another turn; only a
            // started activity establishes that it owns work again.
            "interacted" => self
                .items
                .get(&parent_id)
                .map(|item| item_status(&item.content))
                .unwrap_or(ItemStatus::InProgress),
            _ => ItemStatus::InProgress,
        };
        let parent = ThreadItem {
            id: parent_id.clone(),
            parent_item_id: None,
            content: ItemContent::Subagent {
                agent_type: subagent.agent_type.clone(),
                description: subagent.description.clone(),
                status,
                summary: None,
                model: subagent.model.clone(),
                effort: subagent.effort.clone(),
            },
        };
        self.items.insert(parent_id.clone(), parent.clone());
        let parent_event = if status == ItemStatus::InProgress {
            AgentEvent::ItemUpdated(parent)
        } else {
            AgentEvent::ItemCompleted(parent)
        };
        self.events.emit(parent_event).await;
        if needs_metadata
            && let Err(error) = self.request(
                "thread/read",
                json!({ "threadId": thread_id, "includeTurns": false }),
                PendingRequest::SubagentMetadata { parent_id },
            )
        {
            log::warn!("Could not read Codex child metadata: {error}");
        }
    }

    pub(super) async fn update_subagent_metadata(&mut self, parent_id: &str, thread: &Value) {
        let Some(subagent) = self.subagents.get_mut(parent_id) else {
            return;
        };
        subagent.model = thread
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_owned);
        subagent.effort = thread
            .get("reasoningEffort")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let Some(parent) = self.items.get_mut(parent_id) else {
            return;
        };
        let ItemContent::Subagent { model, effort, .. } = &mut parent.content else {
            return;
        };
        model.clone_from(&subagent.model);
        effort.clone_from(&subagent.effort);
        // A metadata reply can follow completion. Update the label without
        // reopening the child or replacing its terminal status and summary.
        self.events
            .emit(AgentEvent::ItemUpdated(parent.clone()))
            .await;
    }

    /// Deliver `request_user_input_async` answers into the running turn. The
    /// model reads them like any steer, so the reply is a `turn/steer` whose
    /// text is Codex's own question-reply envelope; the timeline shows the
    /// same plain rendering the Codex TUI uses for that envelope.
    pub(super) async fn steer_async_question_reply(
        &mut self,
        request_id: &str,
        questions: &[UserInputQuestion],
        answers: &serde_json::Map<String, Value>,
    ) -> Result<(), String> {
        let Some((wire, display)) = async_question_reply(questions, answers) else {
            return Ok(());
        };
        let Some(turn_id) = self.active_turn.clone() else {
            self.events
                .emit(AgentEvent::Warning {
                    message: "cannot answer: the Codex turn that asked has ended".into(),
                })
                .await;
            return Ok(());
        };
        let steer_id = format!("codex-question-reply-{request_id}");
        self.events
            .emit(AgentEvent::SteerRequested {
                request_id: steer_id.clone(),
                text: display,
                attachments: Vec::new(),
            })
            .await;
        let thread_id = self.thread_id.clone();
        self.request(
            "turn/steer",
            json!({
                "threadId": thread_id,
                "expectedTurnId": turn_id,
                "input": user_input(&wire, &[]),
            }),
            PendingRequest::Steer {
                request_id: steer_id,
                text: wire,
            },
        )
    }

    /// A completed `agentMessage` carrying `questions` is the
    /// `request_user_input_async` tool's output: the message text already
    /// lists the questions, and the turn keeps running. Open questions of the
    /// turn accumulate under one request so a later ask does not hide an
    /// earlier unanswered one.
    pub(super) async fn request_async_questions(&mut self, item: &Value) {
        if item.get("type").and_then(Value::as_str) != Some("agentMessage") {
            return;
        }
        let Some(message_id) = item.get("id").and_then(Value::as_str) else {
            return;
        };
        let questions = parse_codex_async_questions(message_id, item);
        if questions.is_empty() {
            return;
        }
        let mut open = self
            .async_questions
            .take()
            .map(|(_, questions)| questions)
            .unwrap_or_default();
        open.extend(questions);
        self.async_questions = Some((message_id.to_owned(), open.clone()));
        self.events
            .emit(AgentEvent::UserInputRequested {
                request_id: message_id.to_owned(),
                questions: open,
                delivery: UserInputDelivery::Async,
            })
            .await;
    }
}
