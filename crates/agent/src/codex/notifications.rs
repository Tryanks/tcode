use super::*;

impl Actor {
    pub(super) async fn handle_notification(&mut self, method: &str, params: &Value) {
        if matches!(
            method,
            "item/autoApprovalReview/started" | "item/autoApprovalReview/completed"
        ) {
            self.handle_auto_review(params).await;
            return;
        }
        if method == "serverRequest/resolved" {
            self.on_server_request_resolved(params).await;
            return;
        }
        let thread_id = params.get("threadId").and_then(Value::as_str);
        let is_child = thread_id.is_some_and(|id| id != self.thread_id);
        if let Some(activity) = find_subagent_activity(params) {
            self.handle_subagent_activity(activity).await;
            return;
        }
        if is_child {
            // Child snapshots belong in the subagent mirror. Keep their lifecycle,
            // deltas and usage out of the parent turn and its item cache.
            if matches!(method, "item/started" | "item/updated" | "item/completed")
                && let Some(value) = params.get("item")
                && !matches!(
                    value.get("type").and_then(Value::as_str),
                    Some("contextCompaction")
                )
                && let Some(mut item) = lifecycle_item(method, value)
            {
                self.capture_read_image(&mut item).await;
                let thread_id = thread_id.expect("child notification has a thread id");
                let parent_id = self
                    .subagent_parent_by_thread
                    .entry(thread_id.to_owned())
                    .or_insert_with(|| format!("codex-subagent:{thread_id}"));
                item.parent_item_id = Some(parent_id.clone());
                let event = match method {
                    "item/started" => AgentEvent::ItemStarted(item),
                    "item/updated" => AgentEvent::ItemUpdated(item),
                    _ => AgentEvent::ItemCompleted(item),
                };
                self.events.emit(event).await;
            }
            return;
        }
        match method {
            "thread/settings/updated" => {
                if let Some(settings) = params.get("threadSettings") {
                    if let Some(PendingRequest::TurnStart {
                        settings: pending, ..
                    }) = self
                        .pending_requests
                        .values_mut()
                        .find(|request| matches!(request, PendingRequest::TurnStart { .. }))
                    {
                        *pending = Some(settings.clone());
                    } else {
                        self.effective_permission = effective_permission(settings);
                        self.emit_permissions(Some(settings)).await;
                        self.emit_thread_settings(settings).await;
                    }
                }
            }
            "turn/started" => {
                if let Some(id) = params.pointer("/turn/id").and_then(Value::as_str) {
                    if std::mem::take(&mut self.continuing) {
                        self.continuations.insert(id.to_owned());
                    }
                    self.active_turn = Some(id.into());
                    self.retry = None;
                    if self.interrupted {
                        let _ = self.request(
                            "turn/interrupt",
                            json!({"threadId":self.thread_id,"turnId":id}),
                            PendingRequest::Interrupt,
                        );
                    }
                    self.events
                        .emit(AgentEvent::TurnStarted { turn_id: id.into() })
                        .await;
                }
            }
            "turn/completed" => {
                self.background_terminal_read =
                    Some(self.clock.now() + BACKGROUND_TERMINAL_TURN_END_READ);
                let turn = params.get("turn").unwrap_or(&Value::Null);
                let id = turn
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let status = match turn.get("status").and_then(Value::as_str) {
                    Some("interrupted") => TurnStatus::Interrupted,
                    Some("failed") => TurnStatus::Failed,
                    _ => TurnStatus::Completed,
                };
                if status == TurnStatus::Failed
                    && !self.interrupted
                    && turn
                        .pointer("/error/codexErrorInfo")
                        .and_then(Value::as_str)
                        == Some("serverOverloaded")
                {
                    // Only Retry, with no countdown: ChatGPT.app 26.1103 continues on
                    // its own only for an app-server older than 0.162.0-alpha.18.
                    self.retry = Some(crate::TurnRetry {
                        turn_id: id.clone(),
                        retry_at: None,
                    });
                    self.events
                        .emit(AgentEvent::TurnRetry {
                            retry: self.retry.clone(),
                        })
                        .await;
                } else {
                    self.clear_retry().await;
                }
                self.active_turn = None;
                // An async question is answered by steering the turn that
                // asked it, so it ends with that turn: its card goes, and
                // nothing is sent to Codex, which waited on no answer.
                if let Some((request_id, _)) = self.async_questions.take() {
                    self.events
                        .emit(AgentEvent::UserInputResolved {
                            request_id,
                            answers: serde_json::Map::new(),
                        })
                        .await;
                }
                if status == TurnStatus::Completed {
                    // The turn ended without waiting on a subagent still at
                    // work: it goes on in the background.
                    let running: Vec<String> = self
                        .subagents
                        .keys()
                        .filter(|id| {
                            !self.backgrounded_subagents.contains(*id)
                                && self.items.get(*id).is_some_and(|item| {
                                    item.parent_item_id.is_none()
                                        && item_status(&item.content) == ItemStatus::InProgress
                                })
                        })
                        .cloned()
                        .collect();
                    for item_id in running {
                        self.backgrounded_subagents.insert(item_id.clone());
                        self.events
                            .emit(AgentEvent::ItemBackgrounded {
                                item_id,
                                parent_item_id: None,
                            })
                            .await;
                    }
                }
                let mut usage = self.usage_by_turn.remove(&id);
                if let Some(usage) = usage.as_mut() {
                    usage.duration_ms = turn.get("durationMs").and_then(Value::as_u64);
                }
                let checkpoint =
                    (!id.is_empty() && !self.continuations.remove(&id)).then(|| id.clone());
                self.events
                    .emit(AgentEvent::TurnCompleted {
                        turn_id: id,
                        status,
                        usage,
                    })
                    .await;
                // A finished turn's prompt is a point `thread/revert` can go
                // back to; a steer inside it is not.
                if let Some(turn_id) = checkpoint {
                    self.events
                        .emit(AgentEvent::TurnCheckpoint {
                            checkpoint_id: turn_id.clone(),
                            turn_id,
                        })
                        .await;
                }
            }
            "turn/diff/updated" => {
                let turn_id = notification_turn_id(params, &self.active_turn).unwrap_or_default();
                let diff = params
                    .get("diff")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let read = file_changes_from_unified_diff(diff);
                // What Tcode could not read as a file's section reaches the
                // user as Codex wrote it, and the snapshot no longer claims
                // to be the turn's whole change.
                for unread in &read.unread {
                    self.events
                        .emit(AgentEvent::Warning {
                            message: format!(
                                "Codex sent part of the turn's diff that names no file:\n{unread}"
                            ),
                        })
                        .await;
                }
                self.events
                    .emit(AgentEvent::TurnChangesUpdated {
                        turn_id,
                        changes: read.changes,
                        completeness: if read.unread.is_empty() {
                            ChangeCompleteness::Exact
                        } else {
                            ChangeCompleteness::Partial
                        },
                    })
                    .await;
            }
            "item/started" | "item/updated" | "item/completed" => {
                let item_value = params.get("item");
                match item_value
                    .and_then(|item| item.get("type"))
                    .and_then(Value::as_str)
                {
                    Some("userMessage") => {
                        if let Some(item) = item_value {
                            self.accept_echoed_steer(item).await;
                        }
                        return;
                    }
                    Some("contextCompaction") => {
                        if method == "item/started" {
                            self.events
                                .emit(AgentEvent::ContextCompacted(crate::Compaction {
                                    in_progress: true,
                                    ..Default::default()
                                }))
                                .await;
                        } else if method == "item/completed" {
                            self.restore_instructions();
                            self.events
                                .emit(AgentEvent::ContextCompacted(Default::default()))
                                .await;
                        }
                        return;
                    }
                    _ => {}
                }
                if let Some(mut item) = item_value.and_then(|value| lifecycle_item(method, value)) {
                    self.capture_read_image(&mut item).await;
                    // A command the user stopped ends as Codex reports a
                    // killed process: failed, with exit code -1, its
                    // `unwrap_or(-1)` for a process that left no code
                    // (`core/src/unified_exec/async_watcher.rs`). It was
                    // stopped, not failed, and it gave no code.
                    if method == "item/completed"
                        && self.stopped_commands.remove(&item.id)
                        && let ItemContent::CommandExecution {
                            exit_code, status, ..
                        } = &mut item.content
                        && *status == ItemStatus::Failed
                    {
                        *status = ItemStatus::Interrupted;
                        if *exit_code == Some(-1) {
                            *exit_code = None;
                        }
                    }
                    // Codex's item never carries what was typed into it.
                    if let ItemContent::CommandExecution { input, .. } = &mut item.content
                        && let Some(ItemContent::CommandExecution { input: typed, .. }) =
                            self.items.get(&item.id).map(|known| &known.content)
                    {
                        input.clone_from(typed);
                    }
                    if method == "item/completed" {
                        self.terminal_input.remove(&item.id);
                    }
                    if let Some(subagent) = self.subagents.get(&item.id) {
                        let status = if method == "item/completed" {
                            item_status(&item.content)
                        } else {
                            ItemStatus::InProgress
                        };
                        let summary = if method == "item/completed" {
                            item_summary(&item.content)
                        } else {
                            None
                        };
                        item.content = ItemContent::Subagent {
                            agent_type: subagent.agent_type.clone(),
                            description: subagent.description.clone(),
                            status,
                            summary,
                            model: subagent.model.clone(),
                            effort: subagent.effort.clone(),
                        };
                    }
                    self.items.insert(item.id.clone(), item.clone());
                    let event = match method {
                        "item/started" => AgentEvent::ItemStarted(item),
                        "item/updated" => AgentEvent::ItemUpdated(item),
                        _ => AgentEvent::ItemCompleted(item),
                    };
                    self.events.emit(event).await;
                }
                if method == "item/completed"
                    && let Some(item) = item_value
                {
                    self.request_async_questions(item).await;
                }
            }
            "turn/plan/updated" => {
                let turn_id = notification_turn_id(params, &self.active_turn);
                let explanation = params
                    .get("explanation")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                let steps = params
                    .get("plan")
                    .and_then(Value::as_array)
                    .map(|steps| steps.iter().filter_map(map_plan_step).collect())
                    .unwrap_or_default();
                self.events
                    .emit(AgentEvent::PlanUpdated {
                        turn_id,
                        explanation,
                        steps,
                    })
                    .await;
            }
            "item/plan/delta" | "item/agentMessage/delta" => {
                self.emit_delta(params, DeltaKind::AssistantText).await
            }
            "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
                self.emit_delta(params, DeltaKind::ReasoningText).await
            }
            "item/commandExecution/outputDelta" | "command/exec/outputDelta" => {
                self.emit_delta(params, DeltaKind::CommandOutput).await
            }
            "item/commandExecution/terminalInteraction" => {
                self.on_terminal_interaction(params).await
            }
            "thread/tokenUsage/updated" => {
                if let Some(usage) = map_usage(params.get("tokenUsage").unwrap_or(&Value::Null)) {
                    if let Some(turn_id) = params.get("turnId").and_then(Value::as_str) {
                        self.usage_by_turn.insert(turn_id.into(), usage);
                    }
                    self.events.emit(AgentEvent::TokenUsage(usage)).await;
                }
            }
            "model/rerouted" => {
                if let Some(event) = model_reroute(params) {
                    self.events.emit(event).await;
                }
            }
            "error" => {
                // No message field → show the raw notification: a summary like
                // "unknown error" leaves nothing to diagnose with.
                let message = params
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("Codex reported an error: {params}"));
                let fatal = !params
                    .get("willRetry")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                self.events.emit(AgentEvent::Error { message, fatal }).await;
            }
            "warning" | "guardianWarning" => {
                if let Some(message) = params.get("message").and_then(Value::as_str)
                    && !self.config_warnings.contains(message)
                {
                    self.events
                        .emit(AgentEvent::Warning {
                            message: message.into(),
                        })
                        .await;
                }
            }
            "configWarning" | "deprecationNotice" => {
                if let Some(summary) = params.get("summary").and_then(Value::as_str) {
                    if method == "configWarning" {
                        self.config_warnings.insert(summary.to_owned());
                    }
                    self.events
                        .emit(AgentEvent::Warning {
                            message: notice_text(summary, params),
                        })
                        .await;
                }
            }
            // A server's other states have no place in the canonical stream.
            "mcpServer/startupStatus/updated" => {
                if params.get("status").and_then(Value::as_str) == Some("failed")
                    && let Some(error) = params.get("error").and_then(Value::as_str)
                {
                    self.events
                        .emit(AgentEvent::Warning {
                            message: error.into(),
                        })
                        .await;
                }
            }
            "thread/goal/updated" => {
                if let Some(goal) = params.get("goal").and_then(map_goal) {
                    self.events
                        .emit(AgentEvent::GoalUpdated { goal: Some(goal) })
                        .await;
                }
            }
            "thread/goal/cleared" => {
                self.events
                    .emit(AgentEvent::GoalUpdated { goal: None })
                    .await;
            }
            // The `thread/revert` response settles the rewind.
            "thread/reverted" => {}
            "thread/closed" => {
                self.events
                    .emit(AgentEvent::Warning {
                        message: "Codex thread was closed by the server".into(),
                    })
                    .await;
            }
            _ => log::trace!("ignored Codex notification {method}: {params}"),
        }
    }
}
