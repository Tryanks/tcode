use super::*;

impl AppState {
    /// Attach the serializable registration half of the preview MCP server.
    /// Its non-serializable broker receiver is moved exactly once into the
    /// client handle by `pipe::spawn_host`.
    pub fn attach_preview_mcp(&mut self, url: String, tokens: preview_mcp::TokenRegistry) {
        self.preview_url = Some(url);
        self.preview_tokens = Some(tokens);
    }

    pub fn attach_orchestrate_mcp(&mut self, server: orchestrate_mcp::OrchestrateMcpServer) {
        self.orchestrate_url = Some(server.url);
        self.orchestrate_tokens = Some(server.tokens);
        self.orchestrate_requests = Some(server.requests);
    }

    pub fn attach_computer_use_mcp(&mut self, url: String, token: String) {
        self.computer_use_registration = Some(agent::McpRegistration {
            name: agent::McpRegistration::SERVER_NAME_COMPUTER_USE.into(),
            url,
            bearer_token: token,
        });
    }

    /// Pump orchestrator requests through the runtime on the host executor.
    ///
    /// Taking the receiver makes repeated calls harmless: exactly one pump can
    /// own the request stream.
    pub fn pump_orchestrate_requests(&mut self, cx: &mut HostCx) {
        let Some(requests) = self.orchestrate_requests.take() else {
            return;
        };
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            while let Ok(request) = requests.recv().await {
                let orchestrate_mcp::BrokerRequest { op, reply } = request;
                host_cx.enqueue(move |state, cx| state.handle_orchestrate_op(op, reply, cx));
            }
        });
    }

    /// Persistently opt a session into native orchestration. Callers restart a
    /// currently-live provider so its next spawn receives the MCP registration.
    pub(crate) fn enable_orchestrate(
        &mut self,
        session_id: &str,
        cx: &mut HostCx,
    ) -> Result<(), String> {
        let Some(mut meta) = self.find_meta(session_id) else {
            return Err("unknown session".into());
        };
        meta.orchestrate_enabled = true;
        meta.updated_at = now_secs();
        if let Some(live_meta) = self.meta_mut(session_id) {
            live_meta.orchestrate_enabled = true;
            live_meta.updated_at = meta.updated_at;
        }
        self.persist_meta(&meta, cx);
        let _ = self.orchestrate_registration_for(&meta);
        Ok(())
    }

    /// Enable orchestration on first use, restart so the MCP registration is
    /// present, and submit the provider-specific guidance plus the user's text.
    pub fn orchestrate_turn(
        &mut self,
        text: String,
        attachment_paths: Vec<PathBuf>,
        cx: &mut HostCx,
    ) {
        let (text, attachments) = self.assemble_user_message(text, attachment_paths);
        self.orchestrate_turn_assembled(text, attachments, cx);
        self.clear_consumed_draft_context(cx);
    }

    pub(super) fn orchestrate_turn_assembled(
        &mut self,
        text: String,
        attachments: Vec<Attachment>,
        cx: &mut HostCx,
    ) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        let provider = active.meta.provider;
        let model = active.meta.model.clone();
        let enabling = !active.meta.orchestrate_enabled;
        let session_id = active.meta.id.clone();
        // The composed text is [guidance?] + [configuration] + [user text] joined
        // by "\n\n", with the user's words last. `context_len` is the byte length
        // of everything before them (prefix + its trailing "\n\n") — the split the
        // timeline records so it can show the prefix as a disclosure and the
        // bubble only the user's words. The provider still receives all of `text`.
        let user_len = text.len();
        let text = compose_orchestrate_text(
            provider,
            model.as_deref(),
            enabling,
            &self.settings.orchestrate,
            &text,
        );
        let context_len = text.len().saturating_sub(user_len);

        if enabling {
            if let Err(message) = self.enable_orchestrate(&session_id, cx) {
                self.report_error(RuntimeError::External(message), cx);
                return;
            }
            if let Some(active) = self.active.as_mut() {
                active.shutdown_to_idle();
            }
        }

        // Stage the split so the next `push_queued` records it on the user
        // message. (A mid-turn steer clears it instead — see `steer` — so the
        // annotation never leaks onto an unrelated later message.)
        if let Some(active) = self.active.as_mut() {
            active.pending_context_len = Some(context_len);
        }

        // `steer` sends ordinarily when idle and injects into a live turn. On
        // first enable the restart above intentionally makes this an ordinary
        // queued send for the resumed, MCP-enabled process.
        self.steer_assembled(text, attachments, cx);
    }

    pub(super) fn orchestrate_registration_for(
        &mut self,
        meta: &SessionMeta,
    ) -> Option<agent::McpRegistration> {
        if !meta.orchestrate_enabled {
            return None;
        }
        if let Some(registration) = self.orchestrate_registrations.get(&meta.id) {
            return Some(registration.clone());
        }
        let token = self.orchestrate_tokens.as_ref()?.register(&meta.id);
        let registration = agent::McpRegistration {
            name: agent::McpRegistration::SERVER_NAME_ORCHESTRATE.into(),
            url: self.orchestrate_url.clone()?,
            bearer_token: token,
        };
        self.orchestrate_registrations
            .insert(meta.id.clone(), registration.clone());
        Some(registration)
    }

    pub(super) fn preview_registration_for(
        &mut self,
        meta: &SessionMeta,
    ) -> Option<agent::McpRegistration> {
        if let Some(registration) = self.preview_registrations.get(&meta.id) {
            return Some(registration.clone());
        }
        let token = self.preview_tokens.as_ref()?.register(&meta.id);
        let registration = agent::McpRegistration {
            name: agent::McpRegistration::SERVER_NAME_PREVIEW.into(),
            url: self.preview_url.clone()?,
            bearer_token: token,
        };
        self.preview_registrations
            .insert(meta.id.clone(), registration.clone());
        Some(registration)
    }

    #[allow(clippy::too_many_arguments)] // mirrors the MCP dispatch schema
    pub(crate) fn create_child_session(
        &mut self,
        parent_id: &str,
        provider: ProviderKind,
        model: Option<String>,
        effort: Option<String>,
        profile_id: Option<String>,
        approval_mode: ApprovalMode,
        title: String,
        cwd: Option<PathBuf>,
        brief: String,
        archive_on_complete: bool,
        result_max_chars: Option<u32>,
        cx: &mut HostCx,
    ) -> Result<String, String> {
        let parent = self
            .find_meta(parent_id)
            .ok_or_else(|| "unknown parent session".to_string())?;
        let cwd = cwd.unwrap_or_else(|| parent.cwd.clone());
        let mut meta = build_child_meta(
            &parent,
            provider,
            model,
            effort,
            profile_id,
            approval_mode,
            cwd,
            archive_on_complete,
            result_max_chars,
        );
        meta.title = title;
        self.enqueue_store_write(
            StoreWrite::UpsertMeta {
                meta: Box::new(meta.clone()),
                initial: true,
            },
            cx,
        );
        self.upsert_session_in_memory(meta.clone());
        let id = meta.id.clone();
        let provider_commands =
            self.cached_provider_commands(meta.provider, meta.acp_agent_id.as_deref());
        let mut child = Self::build_draft_session(
            meta.project_id.clone().unwrap_or_default(),
            meta.cwd.clone(),
            meta.provider,
            meta.model.clone(),
            None,
            provider_commands,
        );
        child.meta = meta;
        child.draft = false;
        child.push_queued(brief, Vec::new());
        self.background.insert(id.clone(), child);
        self.ensure_session_started(&id, cx);
        Ok(id)
    }

    /// Resolve one MCP operation on the host owner thread.
    pub(crate) fn handle_orchestrate_op(
        &mut self,
        op: orchestrate_mcp::OrchestrateOp,
        reply: smol::channel::Sender<Result<serde_json::Value, String>>,
        cx: &mut HostCx,
    ) {
        use orchestrate_mcp::OrchestrateOp;

        match op {
            orchestrate_mcp::OrchestrateOp::Status {
                parent_id,
                thread_id,
            } => self.handle_orchestrate_status(parent_id, thread_id, reply, cx),
            orchestrate_mcp::OrchestrateOp::Result {
                parent_id,
                thread_id,
            } => self.handle_orchestrate_result(parent_id, thread_id, reply, cx),
            orchestrate_mcp::OrchestrateOp::Dispatch {
                parent_id,
                provider,
                model,
                effort,
                profile,
                access,
                title,
                brief,
                cwd,
                archive_on_complete,
                result_max_chars,
            } => {
                let resolved = (|| {
                    let (provider, model, effort, profile_id) = resolve_orchestrate_dispatch(
                        &self.settings.orchestrate,
                        &provider,
                        model.as_deref(),
                        effort.as_deref(),
                        profile.as_deref(),
                    )?;
                    if let Some(id) = profile_id.as_deref()
                        && self.settings.resolved_profile(id).is_none()
                    {
                        return Err(format!("unknown profile: {id}"));
                    }
                    let approval_mode = resolve_dispatch_access(access.as_deref())?;
                    Ok((provider, model, effort, profile_id, approval_mode))
                })();
                let (provider, model, effort, profile_id, approval_mode) = match resolved {
                    Ok(resolved) => resolved,
                    Err(err) => {
                        let _ = reply.try_send(Err(err));
                        return;
                    }
                };
                let archive_on_complete =
                    archive_on_complete.unwrap_or(self.settings.orchestrate.archive_on_complete);
                let Some(cwd) = cwd else {
                    let result = self
                        .create_child_session(
                            &parent_id,
                            provider,
                            Some(model),
                            effort,
                            profile_id,
                            approval_mode,
                            title,
                            None,
                            brief,
                            archive_on_complete,
                            result_max_chars,
                            cx,
                        )
                        .map(|id| serde_json::json!({ "thread_id": id }));
                    let _ = reply.try_send(result);
                    return;
                };
                let Some(parent_cwd) = self.find_meta(&parent_id).map(|meta| meta.cwd) else {
                    let _ = reply.try_send(Err("unknown parent session".to_string()));
                    return;
                };
                let path = PathBuf::from(cwd);
                let path = if path.is_absolute() {
                    path
                } else {
                    parent_cwd.join(path)
                };
                let host_cx = cx.clone();
                HostCx::spawn_detached(cx, async move {
                    let resolved_cwd = host_cx
                        .unblock(move || {
                            let canonical = path
                                .canonicalize()
                                .map_err(|_| format!("invalid cwd: {}", path.display()))?;
                            if !canonical.is_dir() {
                                return Err(format!("invalid cwd: {}", canonical.display()));
                            }
                            Ok(canonical)
                        })
                        .await;
                    let result = match resolved_cwd {
                        Ok(cwd) => host_cx
                            .enqueue_and_wait(move |state, cx| {
                                state.create_child_session(
                                    &parent_id,
                                    provider,
                                    Some(model),
                                    effort,
                                    profile_id,
                                    approval_mode,
                                    title,
                                    Some(cwd),
                                    brief,
                                    archive_on_complete,
                                    result_max_chars,
                                    cx,
                                )
                            })
                            .await
                            .unwrap_or_else(|_| Err("application closed".to_string()))
                            .map(|id| serde_json::json!({ "thread_id": id })),
                        Err(err) => Err(err),
                    };
                    let _ = reply.try_send(result);
                });
            }
            OrchestrateOp::Send {
                parent_id,
                thread_id,
                message,
            } => {
                let result = (|| {
                    let archived = self
                        .require_child(&parent_id, &thread_id)?
                        .archived_at
                        .is_some();
                    // A follow-up revives an archived child: it returns to the
                    // sidebar so the user can watch the retry it just received.
                    if archived {
                        self.unarchive_session(&thread_id, cx);
                    }
                    // A live turn accepts the message right away — same routing as
                    // parent callbacks. Queueing a mid-turn correction until the
                    // turn ends would deliver it after the work it was meant to
                    // redirect (and never, if the turn hangs).
                    let can_steer = self
                        .resident(&thread_id)
                        .is_some_and(|child| child.turn_in_flight && child.supports_steering());
                    if can_steer {
                        let request_id = self.record_steer_request(&thread_id, &message, &[], cx);
                        let sent = self.resident_mut(&thread_id).is_some_and(|child| {
                            child
                                .steer_now(request_id, message.clone(), Vec::new())
                                .is_ok()
                        });
                        if sent {
                            return Ok(serde_json::json!({ "ok": true, "delivery": "steered" }));
                        }
                        // Provider channel gone: fall through so the text survives
                        // in the queue for the wake-up path.
                    }
                    if self.active_session_id() == Some(&thread_id) {
                        let child = self.active.as_mut().unwrap();
                        child.push_queued(message, Vec::new());
                        let idle = matches!(child.runtime, Runtime::Idle);
                        if self.dispatch_next_queued(cx).is_err() {
                            return Err("child provider is unavailable".into());
                        }
                        if idle {
                            self.ensure_started(cx);
                        }
                        return Ok(serde_json::json!({ "ok": true, "delivery": "queued" }));
                    }
                    self.ensure_child_loaded(&thread_id, cx)?;
                    let child = self.background.get_mut(&thread_id).unwrap();
                    child.push_queued(message, Vec::new());
                    let idle = matches!(child.runtime, Runtime::Idle);
                    if !idle && !child.turn_in_flight {
                        self.on_background_turn_completed(&thread_id, cx);
                    }
                    if idle {
                        self.ensure_session_started(&thread_id, cx);
                    }
                    Ok(serde_json::json!({ "ok": true, "delivery": "queued" }))
                })();
                let _ = reply.try_send(result);
            }
            OrchestrateOp::Cancel {
                parent_id,
                thread_id,
            } => {
                let result = (|| {
                    self.require_child(&parent_id, &thread_id)?;
                    self.clear_approvals(&thread_id);
                    if self.active_session_id() == Some(&thread_id) {
                        if let Some(child) = self.active.as_mut() {
                            child.queue.clear();
                            child.timeline.mark_idle();
                            child.shutdown_to_idle();
                        }
                    } else {
                        self.drop_background(&thread_id, cx);
                    }
                    Ok(serde_json::json!({ "ok": true }))
                })();
                let _ = reply.try_send(result);
            }
            OrchestrateOp::Archive {
                parent_id,
                thread_ids,
            } => {
                let result = (|| {
                    if thread_ids.is_empty() {
                        return Err("thread_ids must not be empty".into());
                    }
                    let invalid: Vec<_> = thread_ids
                        .iter()
                        .filter(|thread_id| self.require_child(&parent_id, thread_id).is_err())
                        .cloned()
                        .collect();
                    if !invalid.is_empty() {
                        return Err(format!(
                            "unknown threads or not children of this parent: {}",
                            invalid.join(", ")
                        ));
                    }
                    self.archive_session_ids(&thread_ids, now_secs(), cx);
                    Ok(serde_json::json!({
                        "ok": true,
                        "archived": thread_ids.len(),
                        "thread_ids": thread_ids,
                    }))
                })();
                let _ = reply.try_send(result);
            }
            OrchestrateOp::Approve {
                parent_id,
                thread_id,
                request_id,
                decision,
            } => {
                let result = (|| {
                    self.require_child(&parent_id, &thread_id)?;
                    let pending = self.approval_requests(&thread_id);
                    let request = match request_id {
                        Some(request_id) => pending
                            .iter()
                            .find(|request| request.id == request_id)
                            .cloned()
                            .ok_or_else(|| {
                                "no pending approval with that request_id".to_string()
                            })?,
                        None => match pending {
                            [request] => request.clone(),
                            [] => return Err("no pending approval".into()),
                            _ => {
                                return Err(
                                    "multiple pending approvals; request_id is required".into()
                                );
                            }
                        },
                    };
                    let decision = resolve_approval_decision(&decision)?;
                    let request_id = request.id;
                    self.respond_session_approval(&thread_id, request_id.clone(), decision)?;
                    Ok(serde_json::json!({ "ok": true, "request_id": request_id }))
                })();
                let _ = reply.try_send(result);
            }
        }
    }

    pub(super) fn require_child(
        &self,
        parent_id: &str,
        thread_id: &str,
    ) -> Result<&SessionMeta, String> {
        self.sessions
            .iter()
            .find(|meta| {
                meta.id == thread_id && meta.parent_session_id.as_deref() == Some(parent_id)
            })
            .ok_or_else(|| "unknown thread or not a child of this parent".into())
    }

    pub(super) fn ensure_child_loaded(
        &mut self,
        thread_id: &str,
        cx: &mut HostCx,
    ) -> Result<(), String> {
        if self.background.contains_key(thread_id) {
            return Ok(());
        }
        let meta = self
            .sessions
            .iter()
            .find(|meta| meta.id == thread_id)
            .cloned()
            .ok_or_else(|| "unknown thread".to_string())?;
        if self.active_session_id() == Some(thread_id) {
            return Err("child thread is currently open in the foreground".into());
        }
        self.load_background_session(meta, cx);
        Ok(())
    }

    pub(super) fn load_background_session(&mut self, meta: SessionMeta, cx: &mut HostCx) {
        let thread_id = meta.id.clone();
        let commands = self.cached_provider_commands(meta.provider, meta.acp_agent_id.as_deref());
        let mut child = Self::build_draft_session(
            meta.project_id.clone().unwrap_or_default(),
            meta.cwd.clone(),
            meta.provider,
            meta.model.clone(),
            meta.acp_agent_id.clone(),
            commands,
        );
        child.meta = meta;
        child.draft = false;
        self.background.insert(thread_id.clone(), child);
        self.schedule_timeline_load(
            thread_id,
            TimelineLoadTarget::Background { mark_idle: true },
            cx,
        );
    }

    pub(super) fn handle_orchestrate_status(
        &mut self,
        parent_id: String,
        thread_id: Option<String>,
        reply: smol::channel::Sender<Result<serde_json::Value, String>>,
        cx: &mut HostCx,
    ) {
        let children: Vec<_> = self
            .sessions
            .iter()
            .filter(|meta| meta.parent_session_id.as_deref() == Some(&parent_id))
            .filter(|meta| thread_id.as_ref().is_none_or(|id| id == &meta.id))
            .cloned()
            .collect();
        if thread_id.is_some() && children.is_empty() {
            let _ = reply.try_send(Err("unknown thread or not a child of this parent".into()));
            return;
        }
        let unloaded: Vec<_> = children
            .iter()
            .filter(|meta| self.loaded_child_timeline(&meta.id).is_none())
            .map(|meta| meta.id.clone())
            .collect();
        if unloaded.is_empty() {
            let result = self.orchestrate_status_json(&children, &HashMap::new());
            let _ = reply.try_send(Ok(result));
            return;
        }
        let store = self.store.clone();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let timelines = host_cx
                .unblock(move || {
                    unloaded
                        .into_iter()
                        .map(|id| {
                            let timeline = Timeline::fold_events(store.read_events(&id));
                            (id, timeline)
                        })
                        .collect::<HashMap<_, _>>()
                })
                .await;
            let result = host_cx
                .enqueue_and_wait(move |state, _| {
                    state.orchestrate_status_json(&children, &timelines)
                })
                .await
                .map_err(|_| "tcode orchestrator is not available".to_string());
            let _ = reply.send(result).await;
        });
    }

    pub(super) fn handle_orchestrate_result(
        &mut self,
        parent_id: String,
        thread_id: String,
        reply: smol::channel::Sender<Result<serde_json::Value, String>>,
        cx: &mut HostCx,
    ) {
        let meta = match self.require_child(&parent_id, &thread_id) {
            Ok(meta) => meta.clone(),
            Err(error) => {
                let _ = reply.try_send(Err(error));
                return;
            }
        };
        if let Some(timeline) = self.loaded_child_timeline(&thread_id) {
            let result = self.orchestrate_result_json(&meta, timeline);
            let _ = reply.try_send(result);
            return;
        }
        let store = self.store.clone();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let read_id = thread_id.clone();
            let timeline = host_cx
                .unblock(move || Timeline::fold_events(store.read_events(&read_id)))
                .await;
            let result = host_cx
                .enqueue_and_wait(move |state, _| {
                    let timeline = state.loaded_child_timeline(&thread_id).unwrap_or(&timeline);
                    state.orchestrate_result_json(&meta, timeline)
                })
                .await
                .unwrap_or_else(|_| Err("tcode orchestrator is not available".to_string()));
            let _ = reply.send(result).await;
        });
    }

    pub(super) fn loaded_child_timeline(&self, session_id: &str) -> Option<&Timeline> {
        self.resident(session_id).map(|child| &child.timeline)
    }

    pub(super) fn child_result(
        &self,
        meta: &SessionMeta,
        timeline: &Timeline,
    ) -> (&'static str, String, Option<agent::TokenUsage>) {
        let running = self.resident(&meta.id).is_some_and(|child| {
            child.turn_in_flight
                || child.delivery_in_flight.is_some()
                || !child.queue.is_empty()
                || child.background_task_count > 0
                || matches!(child.runtime, Runtime::Starting { .. })
        });
        let state = if running {
            "running"
        } else {
            match timeline.last_turn_status {
                Some(TurnStatus::Completed) => "completed",
                Some(TurnStatus::Failed | TurnStatus::Interrupted) => "failed",
                None => "idle",
            }
        };
        (state, final_assistant_message(timeline), timeline.usage)
    }

    pub(super) fn orchestrate_result_json(
        &self,
        meta: &SessionMeta,
        timeline: &Timeline,
    ) -> Result<serde_json::Value, String> {
        let (state, final_message, usage) = self.child_result(meta, timeline);
        if state == "running" {
            return Err("thread is still running".into());
        }
        let mut result = serde_json::json!({
            "state": state,
            "final_message": final_message,
        });
        if let Some(usage) = usage.as_ref() {
            result["tokens"] = token_usage_json(usage);
        }
        Ok(result)
    }

    pub(super) fn orchestrate_status_json(
        &self,
        children: &[SessionMeta],
        disk_timelines: &HashMap<String, Timeline>,
    ) -> serde_json::Value {
        let mut children: Vec<_> = children
            .iter()
            .filter_map(|meta| {
                self.loaded_child_timeline(&meta.id)
                    .or_else(|| disk_timelines.get(&meta.id))
                    .map(|timeline| self.child_status_json(meta, timeline))
            })
            .collect();
        children.sort_by_key(|value| value["updated_at"].as_u64().unwrap_or_default());
        children.reverse();
        serde_json::Value::Array(children)
    }

    pub(super) fn child_status_json(
        &self,
        meta: &SessionMeta,
        timeline: &Timeline,
    ) -> serde_json::Value {
        let (state, final_message, usage) = self.child_result(meta, timeline);
        let approval = self.first_approval(&meta.id);
        let waiting_approval = approval.map(approval_request_summary);
        let approval_request_id = approval.map(|request| request.id.as_str());
        let mut status = serde_json::json!({
            "thread_id": meta.id,
            "title": meta.title,
            "provider": provider_name(meta.provider),
            "state": state,
            "archived": meta.archived_at.is_some(),
            "waiting_approval": waiting_approval,
            "approval_request_id": approval_request_id,
            "last_output_tail": tail_chars(&final_message, 600),
            "updated_at": meta.updated_at,
        });
        if let Some(usage) = usage.as_ref() {
            status["tokens"] = token_usage_json(usage);
        }
        status
    }

    pub(super) fn deliver_child_callback(
        &mut self,
        child_id: &str,
        status: TurnStatus,
        cx: &mut HostCx,
    ) {
        let Some(child) = self
            .sessions
            .iter()
            .find(|meta| meta.id == child_id && meta.parent_session_id.is_some())
            .cloned()
        else {
            return;
        };
        if self
            .resident(child_id)
            .is_some_and(|child| !child.queue.is_empty())
        {
            return;
        }
        let child_id = child_id.to_string();
        let parent_id = child.parent_session_id.clone().unwrap();
        let title = child.title;
        // Failed and interrupted children stay visible: they are retry
        // candidates, and archiving would hide exactly the threads that need
        // attention.
        let auto_archive = child.archive_on_complete && matches!(status, TurnStatus::Completed);
        let result_max_chars = child.result_max_chars;
        let store = self.store.clone();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let read_id = child_id.clone();
            let timeline = host_cx
                .unblock(move || Timeline::fold_events(store.read_events(&read_id)))
                .await;
            host_cx.enqueue(move |state, cx| {
                let child_still_exists = state.sessions.iter().any(|meta| {
                    meta.id == child_id
                        && meta.parent_session_id.as_deref() == Some(parent_id.as_str())
                });
                if !child_still_exists {
                    return;
                }
                let turn = timeline.turns.len();
                if state.callback_last_turn.get(&child_id).copied() == Some(turn) {
                    return;
                }
                let text = assemble_callback_text(
                    &child_id,
                    &title,
                    status,
                    &final_assistant_message(&timeline),
                    timeline.usage.as_ref(),
                    result_max_chars,
                    auto_archive,
                );
                state.callback_last_turn.insert(child_id.clone(), turn);
                state.deliver_orchestrate_callback_to_parent(&parent_id, text, cx);
                if auto_archive {
                    state.archive_session_ids(&[child_id], now_secs(), cx);
                }
            });
        });
    }

    pub(super) fn deliver_child_approval_callback(
        &mut self,
        child_id: &str,
        request_id: &str,
        cx: &mut HostCx,
    ) {
        let Some(child) = self
            .sessions
            .iter()
            .find(|meta| meta.id == child_id && meta.parent_session_id.is_some())
            .cloned()
        else {
            return;
        };
        let Some(request) = self
            .approval_requests(child_id)
            .iter()
            .find(|request| request.id == request_id)
            .cloned()
        else {
            return;
        };
        if self.settings.orchestrate.child_approval == ChildApprovalMode::AlwaysAllow {
            if let Err(err) = self.respond_session_approval(
                child_id,
                request.id.clone(),
                ApprovalDecision::ApproveForSession,
            ) {
                log::warn!("failed to auto-approve child {child_id}: {err}");
            }
            return;
        }
        if !self
            .callback_approval_requests
            .insert((child_id.to_string(), request.id.clone()))
        {
            return;
        }
        let parent_id = child.parent_session_id.as_deref().unwrap();
        let text = match self.settings.orchestrate.child_approval {
            ChildApprovalMode::Orchestrator => format!(
                "[orchestrate] thread {child_id} (\"{}\") is waiting for approval: {} (request_id: {}). You are the approver: decide with the approve tool (decision: approve | approve_for_session | deny); deny anything outside the brief's scope.",
                child.title,
                approval_request_summary(&request),
                request.id
            ),
            ChildApprovalMode::Manual => format!(
                "[orchestrate] thread {child_id} (\"{}\") is waiting for approval: {}.",
                child.title,
                approval_request_summary(&request)
            ),
            ChildApprovalMode::AlwaysAllow => unreachable!(),
        };
        self.deliver_orchestrate_callback_to_parent(parent_id, text, cx);
    }

    /// Deliver a child result into the orchestrator's current reasoning turn.
    ///
    /// A foreground parent already used `steer`, but a parked parent used to put
    /// callbacks into its ordinary queue. Parallel children could therefore
    /// leave results stranded while the orchestrator planned from only the first
    /// completion. Steering is session lifecycle behavior, not UI focus behavior,
    /// so foreground and parked parents follow the same routing here.
    pub(super) fn deliver_orchestrate_callback_to_parent(
        &mut self,
        parent_id: &str,
        text: String,
        cx: &mut HostCx,
    ) {
        let can_steer = self
            .resident(parent_id)
            .is_some_and(|parent| parent.turn_in_flight && parent.supports_steering());
        if can_steer {
            // A steered callback is already part of this turn, so persist it just
            // like a user-triggered steer before handing it to the provider.
            let request_id = self.record_steer_request(parent_id, &text, &[], cx);
            let sent = self
                .resident_mut(parent_id)
                .is_some_and(|parent| parent.steer_now(request_id, text, Vec::new()).is_ok());
            if !sent {
                self.report_error(RuntimeError::ProcessGone, cx);
            }
            return;
        }

        if self.active_session_id() == Some(parent_id) {
            let parent = self.active.as_mut().unwrap();
            parent.push_or_merge_orchestrate_callback(text);

            // Match ordinary sends when a launch-time selection changed while
            // the provider was live. Background work keeps the old process
            // alive; its final follow-up completion performs the restart.
            let settings_changed = parent.launch_settings_changed_while_live();
            let restart_deferred = parent.settings_restart_deferred();
            if settings_changed && !restart_deferred {
                parent.shutdown_to_idle();
            }
            let should_start = matches!(parent.runtime, Runtime::Idle);
            if !restart_deferred && self.dispatch_next_queued(cx).is_err() {
                self.report_error(RuntimeError::ProcessGone, cx);
            }
            if should_start {
                self.ensure_started(cx);
            }
            return;
        }

        if !self.background.contains_key(parent_id)
            && let Some(parent) = self
                .sessions
                .iter()
                .find(|meta| meta.id == parent_id)
                .cloned()
        {
            self.load_background_session(parent, cx);
        }
        if let Some(parent) = self.background.get_mut(parent_id) {
            parent.push_or_merge_orchestrate_callback(text);
            let idle_runtime = matches!(parent.runtime, Runtime::Idle);
            let can_dispatch = !parent.turn_in_flight && matches!(parent.runtime, Runtime::Live(_));
            if can_dispatch {
                self.on_background_turn_completed(parent_id, cx);
            }
            if idle_runtime {
                self.ensure_session_started(parent_id, cx);
            }
        }
    }
}

// Named fable.md: on case-insensitive filesystems a claude.md here collides
// with the CLAUDE.md project-memory convention and gets auto-ingested by
// Claude Code sessions working on this repo.
pub(super) const FABLE_ORCHESTRATE_GUIDANCE: &str =
    include_str!("../../../../assets/orchestrate/fable.md");
pub(super) const CODEX_ORCHESTRATE_GUIDANCE: &str =
    include_str!("../../../../assets/orchestrate/codex.md");
pub(super) const GENERIC_ORCHESTRATE_GUIDANCE: &str =
    include_str!("../../../../assets/orchestrate/generic.md");

pub(super) fn compose_orchestrate_text(
    provider: ProviderKind,
    model: Option<&str>,
    enabling: bool,
    settings: &OrchestrateSettings,
    user_text: &str,
) -> String {
    let base_guidance = match provider {
        ProviderKind::ClaudeCode => FABLE_ORCHESTRATE_GUIDANCE,
        ProviderKind::Codex => CODEX_ORCHESTRATE_GUIDANCE,
        ProviderKind::Pi | ProviderKind::OpenCode => GENERIC_ORCHESTRATE_GUIDANCE,
        ProviderKind::Acp => GENERIC_ORCHESTRATE_GUIDANCE,
    };
    let configuration = render_orchestrate_configuration(settings, provider, model);
    let mut sections = Vec::with_capacity(3);
    if enabling {
        sections.push(base_guidance.trim());
    }
    sections.push(configuration.trim());
    if !user_text.is_empty() {
        sections.push(user_text);
    }
    sections.join("\n\n")
}

pub(super) fn render_orchestrate_configuration(
    settings: &OrchestrateSettings,
    provider: ProviderKind,
    model: Option<&str>,
) -> String {
    let identity = settings.identity_for(provider, model).trim();
    let mut text = String::from("## Current orchestrator configuration\n\n### Your role\n\n");
    if identity.is_empty() {
        text.push_str("No additional model-specific identity is configured.");
    } else {
        text.push_str(identity);
    }
    text.push_str(
        "\n\n### Allowed child models\n\nProfiles pin the effort they dispatch at. A dispatch must name `model` and `effort` exactly as listed; both may be omitted, in which case tcode picks the first enabled profile for the provider. When an entry names a `profile`, pass it exactly as listed. The definitions below are user-configured routing guidance.\n",
    );
    if !settings.child_models.iter().any(|child| child.enabled) {
        text.push_str("No child models are enabled. Work without dispatching until the user enables one in Settings → Orchestrate.");
        return text;
    }
    for child in settings.child_models.iter().filter(|child| child.enabled) {
        let provider = provider_name(child.provider);
        let effort = child.effort.as_deref().unwrap_or("provider default");
        if let Some(profile_id) = child.profile_id.as_deref() {
            text.push_str(&format!(
                "\n#### `{}` / `{}` — effort `{}` — profile `{}`\n\n{}\n",
                escape_markdown_inline(provider),
                escape_markdown_inline(&child.model),
                escape_markdown_inline(effort),
                escape_markdown_inline(profile_id),
                child.description.trim(),
            ));
        } else {
            text.push_str(&format!(
                "\n#### `{}` / `{}` — effort `{}`\n\n{}\n",
                escape_markdown_inline(provider),
                escape_markdown_inline(&child.model),
                escape_markdown_inline(effort),
                child.description.trim(),
            ));
        }
    }
    text
}

pub(super) fn escape_markdown_inline(value: &str) -> String {
    value.replace('`', "\\`").replace(['\r', '\n'], " ")
}

/// Validate an MCP dispatch against the configured child-model allow list and
/// fill in its model/default effort. The main model is unrestricted; this gate
/// applies only to newly-created child sessions.
pub(super) fn resolve_orchestrate_dispatch(
    settings: &OrchestrateSettings,
    provider: &str,
    model: Option<&str>,
    effort: Option<&str>,
    profile: Option<&str>,
) -> Result<(ProviderKind, String, Option<String>, Option<String>), String> {
    let provider = match provider.trim().to_ascii_lowercase().as_str() {
        "claude" | "claude_code" | "claude-code" => ProviderKind::ClaudeCode,
        "codex" => ProviderKind::Codex,
        "pi" => ProviderKind::Pi,
        "opencode" | "open_code" | "open-code" => ProviderKind::OpenCode,
        "acp" => {
            return Err(
                "ACP child dispatch is not available yet; configure a native-provider child model"
                    .into(),
            );
        }
        other => return Err(format!("unknown provider: {other}")),
    };
    let requested_model = model.map(str::trim).filter(|model| !model.is_empty());
    let requested_effort = effort.map(str::trim).filter(|effort| !effort.is_empty());
    let requested_profile = profile.map(str::trim).filter(|profile| !profile.is_empty());
    let candidates: Vec<_> = settings
        .enabled_child_profiles(provider, requested_model, requested_effort)
        .filter(|entry| {
            requested_profile.is_none_or(|requested| {
                entry
                    .profile_id
                    .as_deref()
                    .is_some_and(|id| id.eq_ignore_ascii_case(requested))
            })
        })
        .collect();
    let child = if requested_profile.is_some() {
        candidates.first().copied()
    } else {
        candidates
            .iter()
            .find(|entry| entry.profile_id.is_none())
            .copied()
            .or_else(|| candidates.first().copied())
    }
    .ok_or_else(|| {
        let enabled = settings
            .child_models
            .iter()
            .filter(|entry| entry.enabled && entry.provider == provider)
            .map(|entry| {
                let mut option = format!(
                    "{} (effort {})",
                    entry.model,
                    entry.effort.as_deref().unwrap_or("provider default")
                );
                if let Some(profile_id) = entry.profile_id.as_deref() {
                    option.push_str(&format!(", profile {profile_id}"));
                }
                option
            })
            .collect::<Vec<_>>()
            .join(", ");
        let requested = requested_model.unwrap_or("provider default model");
        let effort = requested_effort
            .map(|effort| format!(" (effort {effort})"))
            .unwrap_or_default();
        let profile = requested_profile
            .map(|profile| format!(" under profile {profile}"))
            .unwrap_or_default();
        format!(
            "no enabled child profile matches {requested}{effort}{profile} under {}; enabled profiles: {}",
            provider_name(provider),
            if enabled.is_empty() { "none" } else { &enabled }
        )
    })?;
    Ok((
        provider,
        child.model.clone(),
        child.effort.clone(),
        child.profile_id.clone(),
    ))
}

pub(super) fn resolve_dispatch_access(access: Option<&str>) -> Result<ApprovalMode, String> {
    let Some(access) = access.map(str::trim).filter(|access| !access.is_empty()) else {
        return Ok(ApprovalMode::FullAccess);
    };
    match access.to_ascii_lowercase().as_str() {
        "full" => Ok(ApprovalMode::FullAccess),
        "read_only" => Ok(ApprovalMode::ReadOnly),
        "workspace_write" => Ok(ApprovalMode::AutoAcceptEdits),
        _ => Err(format!(
            "unknown access: {access}; expected read_only, workspace_write, or full"
        )),
    }
}

pub(super) fn resolve_approval_decision(decision: &str) -> Result<ApprovalDecision, String> {
    let decision = decision.trim();
    match decision.to_ascii_lowercase().as_str() {
        "approve" => Ok(ApprovalDecision::Approve),
        "approve_for_session" => Ok(ApprovalDecision::ApproveForSession),
        "deny" => Ok(ApprovalDecision::Deny),
        _ => Err(format!(
            "unknown decision: {decision}; expected approve, approve_for_session, or deny"
        )),
    }
}

pub(super) fn provider_name(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Codex => "codex",
        ProviderKind::ClaudeCode => "claude",
        ProviderKind::Pi => "pi",
        ProviderKind::OpenCode => "opencode",
        ProviderKind::Acp => "acp",
    }
}

#[allow(clippy::too_many_arguments)] // mirrors the MCP dispatch schema
pub(super) fn build_child_meta(
    parent: &SessionMeta,
    provider: ProviderKind,
    model: Option<String>,
    effort: Option<String>,
    profile_id: Option<String>,
    approval_mode: ApprovalMode,
    cwd: PathBuf,
    archive_on_complete: bool,
    result_max_chars: Option<u32>,
) -> SessionMeta {
    let mut meta = SessionMeta::new(provider, cwd, model);
    meta.project_id = parent.project_id.clone();
    meta.parent_session_id = Some(parent.id.clone());
    meta.profile_id = profile_id;
    meta.approval_mode = approval_mode;
    meta.archive_on_complete = archive_on_complete;
    meta.result_max_chars = result_max_chars;
    if let Some(effort) = effort {
        meta.option_selections.push(OptionSelection {
            id: "reasoningEffort".into(),
            value: serde_json::Value::String(effort),
        });
    }
    meta
}

pub(super) fn final_assistant_message(timeline: &Timeline) -> String {
    let Some((last_index, last)) = timeline
        .entries
        .iter()
        .enumerate()
        .rev()
        .find(|(_, entry)| {
            matches!(
                &entry.content,
                EntryContent::Item(ItemContent::AssistantMessage { .. })
            )
        })
    else {
        return String::new();
    };

    // One provider message may contain several adjacent text blocks. They are
    // separate timeline entries, but together form the final assistant output.
    // Stop at the first non-assistant item so tool preambles from earlier in the
    // turn are not mistaken for part of the final answer.
    let mut parts = Vec::new();
    for entry in timeline.entries[..=last_index].iter().rev() {
        if entry.turn != last.turn {
            break;
        }
        match &entry.content {
            EntryContent::Item(ItemContent::AssistantMessage { text }) => parts.push(text.as_str()),
            _ => break,
        }
    }
    parts.reverse();
    parts.concat()
}

pub(super) fn tail_chars(text: &str, max: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(max)).collect()
}

pub(super) fn approval_request_summary(request: &agent::ApprovalRequest) -> String {
    let detail = match &request.kind {
        agent::ApprovalKind::ExecCommand { command, .. } => format!("command `{command}`"),
        agent::ApprovalKind::FileRead { detail } => format!("file read `{detail}`"),
        agent::ApprovalKind::FileChange { changes, .. } => match changes.as_slice() {
            [change] => format!("file change `{}`", change.path),
            changes => format!("{} file changes", changes.len()),
        },
        agent::ApprovalKind::ToolUse { name, .. } => format!("tool `{name}`"),
    };
    let one_line = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    let truncated: String = one_line.chars().take(180).collect();
    if one_line.chars().count() > 180 {
        format!("{truncated}…")
    } else {
        truncated
    }
}

pub(super) fn token_usage_json(usage: &agent::TokenUsage) -> serde_json::Value {
    let mut value = serde_json::Map::new();
    for (key, count) in [
        ("input_tokens", usage.input_tokens),
        ("cached_input_tokens", usage.cached_input_tokens),
        ("output_tokens", usage.output_tokens),
        ("used_tokens", usage.used_tokens),
        ("total_processed_tokens", usage.total_processed_tokens),
    ] {
        if let Some(count) = count {
            value.insert(key.into(), count.into());
        }
    }
    serde_json::Value::Object(value)
}

pub(super) fn assemble_callback_text(
    child_id: &str,
    title: &str,
    status: TurnStatus,
    final_message: &str,
    usage: Option<&agent::TokenUsage>,
    max_chars: Option<u32>,
    archived: bool,
) -> String {
    let state = match status {
        TurnStatus::Completed if archived => "completed (auto-archived; send revives it)",
        TurnStatus::Completed => "completed",
        TurnStatus::Failed | TurnStatus::Interrupted => "failed",
    };
    let mut token_parts = Vec::new();
    if let Some(usage) = usage {
        if let Some(input) = usage.input_tokens {
            let cached = usage
                .cached_input_tokens
                .filter(|cached| *cached > 0)
                .map(|cached| format!(" (+{cached} cached)"))
                .unwrap_or_default();
            token_parts.push(format!("input {input}{cached}"));
        }
        if let Some(output) = usage.output_tokens {
            token_parts.push(format!("output {output}"));
        }
        if let Some(total) = usage.total_processed_tokens.or(usage.used_tokens) {
            token_parts.push(format!("total {total}"));
        }
    }
    let token_segment = if token_parts.is_empty() {
        String::new()
    } else {
        format!(" tokens: {}.", token_parts.join(", "))
    };
    let body = if final_message.is_empty() {
        "(no assistant output)".to_string()
    } else {
        let count = final_message.chars().count();
        let cap = max_chars.unwrap_or(1200) as usize;
        if cap == 0 || count <= cap {
            final_message.to_string()
        } else {
            format!(
                "Final output tail ({count} chars total; the tail plus the diff is usually enough — result {child_id} has the full text):\n{}",
                tail_chars(final_message, 600.min(cap))
            )
        }
    };
    format!("[orchestrate] thread {child_id} (\"{title}\") {state}.{token_segment}\n{body}")
}
