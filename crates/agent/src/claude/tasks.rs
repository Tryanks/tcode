use super::*;

impl Mapper {
    pub(super) fn on_task_started(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let Some(tool_use_id) = msg.get("tool_use_id").and_then(Value::as_str) else {
            return Vec::new();
        };
        let mut parent_id = tool_use_id.to_owned();
        if let Some(task_id) = msg.get("task_id").and_then(Value::as_str) {
            parent_id = self
                .task_tools
                .entry(task_id.to_owned())
                .or_insert(parent_id)
                .clone();
            if msg.get("task_type").and_then(Value::as_str) != Some("local_bash") {
                self.tailed.insert(parent_id.clone());
                self.tail_requests.push(TailRequest::Start {
                    parent_id: parent_id.clone(),
                    task_id: task_id.to_owned(),
                    session_id: msg
                        .get("session_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                });
            }
        }
        if self.deferred_background_moves.remove(tool_use_id)
            && msg.get("is_backgrounded").and_then(Value::as_bool) != Some(true)
        {
            let request = self.move_to_background_request(tool_use_id.to_owned());
            self.outgoing.push(request);
        }
        // An agent a subagent starts is reported here too, naming that
        // subagent's task. Its call is in the subagent's transcript, which
        // may reach us later or never, so the call is known from this event
        // and is the subagent's from the start.
        let spawner = msg
            .get("parent_task_id")
            .and_then(Value::as_str)
            .and_then(|task_id| self.task_tools.get(task_id))
            .filter(|_| msg.get("task_type").and_then(Value::as_str) == Some("local_agent"))
            .cloned();
        if let Some(spawner) = &spawner {
            match self.tool_items.get_mut(&parent_id) {
                Some(ToolItem::Subagent { parent_item_id, .. }) => {
                    *parent_item_id = Some(spawner.clone());
                }
                Some(_) => {}
                None => {
                    if let Some(tool) =
                        ToolItem::subagent(&spawned_subagent(msg), Some(spawner.clone()))
                    {
                        self.tool_items.insert(parent_id.clone(), tool);
                    }
                }
            }
        }
        let mut events = self.update_subagent(&parent_id, ItemStatus::InProgress, None);
        if let Some((from, to)) = msg
            .get("task_id")
            .and_then(Value::as_str)
            .and_then(|task_id| self.subagent_worktrees.remove(task_id))
        {
            events.push(AgentEvent::SessionMoved {
                from,
                to,
                parent_item_id: Some(parent_id.clone()),
            });
        }
        // The launch acknowledgement that returns the call is only in the
        // subagent's transcript, which both feeds replay; this comes once.
        if spawner.is_some() && msg.get("is_backgrounded").and_then(Value::as_bool) == Some(true) {
            events.push(AgentEvent::ItemBackgrounded {
                item_id: parent_id,
                parent_item_id: spawner,
            });
        }
        events.extend(self.link_background_tasks());
        events
    }

    pub(super) fn on_background_tasks_changed(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let now = crate::now_unix_millis();
        let current = msg
            .get("tasks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|task| {
                let id = task.get("task_id").and_then(Value::as_str)?;
                let known = self.background_tasks.iter().find(|known| known.id == id);
                Some(crate::BackgroundTask {
                    id: id.to_owned(),
                    name: task
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or(id)
                        .to_owned(),
                    kind: match task.get("task_type").and_then(Value::as_str) {
                        Some("local_bash") => crate::BackgroundKind::Command,
                        Some("local_agent" | "remote_agent" | "in_process_teammate") => {
                            crate::BackgroundKind::Agent
                        }
                        _ => crate::BackgroundKind::Other,
                    },
                    item_id: self.task_tools.get(id).cloned(),
                    started_at: known.map_or(now, |known| known.started_at),
                    started_reported: false,
                })
            })
            .collect::<Vec<_>>();
        let had_background_tasks = !self.background_tasks.is_empty();
        self.background_tasks = current;
        let list = AgentEvent::BackgroundTaskList {
            tasks: self.background_tasks.clone(),
        };
        self.background_drain_withheld = self.background_tasks.is_empty() && had_background_tasks;
        if self.background_drain_withheld {
            // `background_tasks_changed []` arrives next to the final
            // task_notification (recorded traces have it on either side) and
            // before the provider's self-invoked follow-up turn. Keep the last
            // non-zero liveness until that turn's result so the runtime cannot
            // park/restart the process in this handoff window. A task stopped
            // through `stop_task` starts no such turn; the CLI goes idle
            // instead, which `session_state_changed` reports (2.1.318).
            vec![list]
        } else {
            vec![
                list,
                AgentEvent::BackgroundTasksChanged {
                    count: self.background_tasks.len(),
                },
            ]
        }
    }

    pub(super) fn on_task_updated(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let Some(task_id) = msg.get("task_id").and_then(Value::as_str) else {
            return Vec::new();
        };
        let Some(tool_use_id) = self.task_tools.get(task_id).cloned() else {
            return Vec::new();
        };
        let Some(status) = msg.pointer("/patch/status").and_then(Value::as_str) else {
            return Vec::new();
        };
        let status = match status {
            "completed" => ItemStatus::Completed,
            "failed" => ItemStatus::Failed,
            // Stopped, by the user or by its session's end: it did not fail.
            "killed" => ItemStatus::Interrupted,
            _ => ItemStatus::InProgress,
        };
        // The tail is stopped by the task_notification that follows a
        // terminal patch, so the notification's `output_file` reaches it first.
        self.update_subagent(&tool_use_id, status, None)
    }

    pub(super) fn on_task_notification(&mut self, msg: &Value) -> Vec<AgentEvent> {
        let task_id = msg
            .get("task_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let tool_use_id = task_id
            .as_ref()
            .and_then(|task_id| self.task_tools.get(task_id).cloned())
            .or_else(|| {
                msg.get("tool_use_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
        let Some(tool_use_id) = tool_use_id else {
            return Vec::new();
        };
        // The task stays in `background_tasks` until the CLI's own
        // `background_tasks_changed` drops it: removing it here lets a drain
        // that follows this notification publish zero before the follow-up
        // turn (see `on_background_tasks_changed`).
        // Claude Code sends "" for a task with no output file.
        if let Some(path) = msg
            .get("output_file")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
        {
            self.tail_requests.push(TailRequest::PreferPath {
                parent_id: tool_use_id.clone(),
                path: PathBuf::from(path),
            });
        }
        self.tail_requests.push(TailRequest::Stop {
            parent_id: tool_use_id.clone(),
        });
        // A notification is the task's terminal frame: `completed`, `failed`
        // or `stopped`.
        let status = match msg.get("status").and_then(Value::as_str) {
            Some("completed") => ItemStatus::Completed,
            Some("stopped") => ItemStatus::Interrupted,
            _ => ItemStatus::Failed,
        };
        let summary = handback_summary(
            msg.get("handback"),
            msg.get("handback_report"),
            msg.get("summary")
                .and_then(Value::as_str)
                .map(str::to_owned),
        );
        // A Command still held here is a background Bash: foreground ones
        // leave `tool_items` with their tool_result.
        if matches!(
            self.tool_items.get(&tool_use_id),
            Some(ToolItem::Command { .. })
        ) {
            self.complete_background_command(&tool_use_id, status, summary)
        } else {
            self.update_subagent(&tool_use_id, status, summary)
        }
    }

    pub(super) fn complete_background_command(
        &mut self,
        tool_use_id: &str,
        status: ItemStatus,
        summary: Option<String>,
    ) -> Vec<AgentEvent> {
        let Some(ToolItem::Command {
            command,
            mut output,
        }) = self.tool_items.remove(tool_use_id)
        else {
            return Vec::new();
        };
        if output.trim().is_empty()
            && let Some(summary) = summary
        {
            output = summary;
        }
        vec![AgentEvent::ItemCompleted(ThreadItem {
            id: tool_use_id.to_owned(),
            parent_item_id: None,
            content: ItemContent::CommandExecution {
                command,
                input: Vec::new(),
                output,
                exit_code: (status == ItemStatus::Completed).then_some(0),
                status,
            },
        })]
    }

    pub(super) fn update_subagent(
        &mut self,
        tool_use_id: &str,
        status: ItemStatus,
        summary: Option<String>,
    ) -> Vec<AgentEvent> {
        let Some(ToolItem::Subagent {
            agent_type,
            description,
            status: saved_status,
            summary: saved_summary,
            model,
            effort,
            parent_item_id,
        }) = self.tool_items.get_mut(tool_use_id)
        else {
            return Vec::new();
        };
        if summary.is_some() {
            *saved_summary = summary;
        }
        *saved_status = status;
        let event = AgentEvent::ItemUpdated(ThreadItem {
            id: tool_use_id.to_owned(),
            parent_item_id: parent_item_id.clone(),
            content: ItemContent::Subagent {
                agent_type: agent_type.clone(),
                description: description.clone(),
                status,
                summary: saved_summary.clone(),
                model: model.clone(),
                effort: effort.clone(),
            },
        });
        if status == ItemStatus::InProgress {
            vec![event]
        } else {
            self.gate_subagent_terminal(tool_use_id, event)
        }
    }

    /// Hold a terminal Subagent snapshot while its transcript tail is still
    /// flushing; the tail's Stop acknowledgement releases the latest one.
    pub(super) fn gate_subagent_terminal(
        &mut self,
        tool_use_id: &str,
        event: AgentEvent,
    ) -> Vec<AgentEvent> {
        if self.tailed.contains(tool_use_id) {
            self.pending_subagent_terminals
                .insert(tool_use_id.to_owned(), event);
            Vec::new()
        } else {
            vec![event]
        }
    }

    /// Fold one feed's child items into the canonical stream: drop the stage
    /// the other feed already delivered, and give a nested spawn the same
    /// `tool_items` lifecycle as a top-level one so its task events and tail
    /// settle it.
    pub(super) fn fold_child_events(&mut self, events: Vec<AgentEvent>) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        for event in events {
            if !self.advance_child_item(&event) {
                continue;
            }
            match &event {
                AgentEvent::ItemStarted(item)
                    if matches!(item.content, ItemContent::Subagent { .. }) =>
                {
                    if let Some(tool) =
                        ToolItem::subagent(&item.content, item.parent_item_id.clone())
                    {
                        self.tool_items.entry(item.id.clone()).or_insert(tool);
                    }
                    out.push(event);
                }
                AgentEvent::ItemCompleted(item)
                    if matches!(item.content, ItemContent::Subagent { .. })
                        && matches!(
                            self.tool_items.get(&item.id),
                            Some(ToolItem::Subagent { .. })
                        ) =>
                {
                    let mut item = item.clone();
                    let id = item.id.clone();
                    // A task_notification outranks the tool_result's text
                    // here as at the top level: this record never says
                    // whether that text is a report.
                    if let (
                        Some(ToolItem::Subagent {
                            summary: Some(notified),
                            ..
                        }),
                        ItemContent::Subagent { summary, .. },
                    ) = (self.tool_items.remove(&id), &mut item.content)
                    {
                        *summary = Some(notified);
                    }
                    out.extend(self.gate_subagent_terminal(&id, AgentEvent::ItemCompleted(item)));
                }
                _ => out.push(event),
            }
        }
        out
    }

    /// Record a child item's stage; false when this stage was already delivered.
    pub(super) fn advance_child_item(&mut self, event: &AgentEvent) -> bool {
        let (item, stage) = match event {
            AgentEvent::ItemStarted(item) => (item, ChildItemStage::Started),
            AgentEvent::ItemUpdated(item) => (item, ChildItemStage::Updated(item.content.clone())),
            AgentEvent::ItemCompleted(item) => (item, ChildItemStage::Completed),
            _ => return true,
        };
        match (self.child_items.get(&item.id), &stage) {
            (Some(ChildItemStage::Completed), _) | (Some(_), ChildItemStage::Started) => false,
            (Some(ChildItemStage::Updated(previous)), ChildItemStage::Updated(content))
                if previous == content =>
            {
                false
            }
            _ => {
                self.child_items.insert(item.id.clone(), stage);
                true
            }
        }
    }

    /// Fold one batch from a subagent transcript tail: its child items, the
    /// model it observed, and — once it has stopped — the terminal Subagent
    /// snapshot that was waiting on its final flush.
    pub(crate) fn on_tail_notice(&mut self, notice: SubagentTailNotice) -> Vec<AgentEvent> {
        let mut events = match notice.model {
            Some((model, effort)) => self.note_subagent_model(&notice.parent_id, model, effort),
            None => Vec::new(),
        };
        events.extend(self.fold_child_events(notice.events));
        if notice.stopped {
            self.tailed.remove(&notice.parent_id);
            events.extend(self.pending_subagent_terminals.remove(&notice.parent_id));
        }
        events
    }

    /// Terminal snapshots whose tail acknowledgement will never arrive
    /// (session teardown).
    pub(super) fn take_pending_subagent_terminals(&mut self) -> Vec<AgentEvent> {
        self.tailed.clear();
        std::mem::take(&mut self.pending_subagent_terminals)
            .into_values()
            .collect()
    }

    /// Record the model/effort a child transcript reports for its spawn item
    /// and re-emit the item when either changed.
    pub(crate) fn note_subagent_model(
        &mut self,
        tool_use_id: &str,
        model: Option<String>,
        effort: Option<String>,
    ) -> Vec<AgentEvent> {
        let Some(ToolItem::Subagent {
            status,
            model: saved_model,
            effort: saved_effort,
            ..
        }) = self.tool_items.get_mut(tool_use_id)
        else {
            return Vec::new();
        };
        let model = model.or_else(|| saved_model.clone());
        let effort = effort.or_else(|| saved_effort.clone());
        if model == *saved_model && effort == *saved_effort {
            return Vec::new();
        }
        *saved_model = model;
        *saved_effort = effort;
        let status = *status;
        self.update_subagent(tool_use_id, status, None)
    }
}
