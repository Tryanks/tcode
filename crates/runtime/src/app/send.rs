use super::*;

impl AppState {
    /// Submit a user turn. Starts the provider lazily if needed.
    pub fn send_turn(&mut self, text: String, attachment_paths: Vec<PathBuf>, cx: &mut HostCx) {
        let (text, attachments) = self.assemble_user_message(text, attachment_paths);
        self.send_turn_assembled(text, attachments, cx);
        self.clear_consumed_draft_context(cx);
    }

    /// Schedule an in-memory user turn for this session. It captures the same
    /// provider options and composer context as an immediate send, but remains
    /// invisible to the persisted transcript until provider acceptance.
    pub fn schedule_turn(
        &mut self,
        text: String,
        attachment_paths: Vec<PathBuf>,
        fire_at_unix_secs: u64,
        cx: &mut HostCx,
    ) {
        let not_before = UNIX_EPOCH + Duration::from_secs(fire_at_unix_secs);
        let (text, attachments) = self.assemble_user_message(text, attachment_paths);
        if self.relay_confirmation().is_some() {
            log::warn!("scheduled send deferred because a conversation relay needs confirmation");
            return;
        }

        let commit_draft = self.active.as_ref().is_some_and(|active| {
            active.draft && !matches!(active.draft_workspace, WorkspaceMode::NewWorktree { .. })
        });
        if commit_draft && let Err(err) = self.commit_draft(cx) {
            self.report_error(
                RuntimeError::PersistSession {
                    error: err.to_string(),
                },
                cx,
            );
            return;
        }

        let Some(active) = self.active.as_mut() else {
            return;
        };
        active.push_scheduled(text, attachments, not_before);
        let should_start = matches!(active.runtime, Runtime::Idle)
            && !(active.draft
                && matches!(active.draft_workspace, WorkspaceMode::NewWorktree { .. }));
        if should_start {
            // Starting now keeps the in-memory session parkable/resident across
            // navigation. Eligibility prevents the future turn from being sent
            // when provider startup completes.
            self.ensure_started(cx);
        }
        self.clear_consumed_draft_context(cx);
        self.emit_active_session_status(cx);
        cx.notify();
        self.reschedule_scheduled_wake(cx);
    }

    /// Replace the detached scheduler wake with one for the earliest deadline
    /// across active and parked sessions. The generation check makes every
    /// superseded timer harmless without needing cancellation support.
    pub(super) fn reschedule_scheduled_wake(&mut self, cx: &mut HostCx) {
        self.scheduler_generation = self.scheduler_generation.wrapping_add(1);
        let generation = self.scheduler_generation;
        let earliest = self
            .active
            .iter()
            .chain(self.background.values())
            .flat_map(|session| {
                session
                    .queue
                    .iter()
                    .filter_map(|message| message.not_before)
            })
            .min();
        let Some(fire_at) = earliest else {
            return;
        };
        let delay = fire_at
            .duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO);
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            smol::Timer::after(delay).await;
            host_cx.enqueue(move |state, cx| {
                if state.scheduler_generation != generation {
                    return;
                }
                state.fire_due_scheduled(cx);
            });
        });
    }

    /// Promote every due scheduled entry back through the ordinary send path.
    /// Active messages are removed first so draft/worktree/restart policy is
    /// centralized in `send_turn_assembled`; parked messages instead clear the
    /// deadline in place to preserve their captured options before dispatch.
    pub(super) fn fire_due_scheduled(&mut self, cx: &mut HostCx) {
        let now = SystemTime::now();
        let active_due: Vec<u64> = self
            .active
            .iter()
            .flat_map(|active| active.queue.iter())
            .filter(|message| message.not_before.is_some_and(|time| time <= now))
            .map(|message| message.id)
            .collect();
        for id in active_due {
            let message = self
                .active
                .as_mut()
                .and_then(|active| active.take_queued(id));
            let Some(message) = message else {
                continue;
            };
            if let Some(active) = self.active.as_mut() {
                active.pending_ultrathink = message.ultrathink;
            }
            self.send_turn_assembled(message.text, message.attachments, cx);
        }

        let parked_ids: Vec<String> = self.background.keys().cloned().collect();
        for session_id in parked_ids {
            let mut had_due = false;
            if let Some(parked) = self.background.get_mut(&session_id) {
                for message in &mut parked.queue {
                    if message.not_before.is_some_and(|time| time <= now) {
                        // Parked turns cannot re-enter the active-only send
                        // pipeline. Clearing in place makes the captured turn
                        // options eligible without reconstructing the entry.
                        message.not_before = None;
                        had_due = true;
                    }
                }
            }
            if !had_due {
                continue;
            }

            let (settings_changed, restart_deferred, is_live, has_queue) = self
                .background
                .get(&session_id)
                .map(|parked| {
                    (
                        parked.launch_settings_changed_while_live(),
                        parked.background_task_count > 0,
                        matches!(parked.runtime, Runtime::Live(_)),
                        !parked.queue.is_empty(),
                    )
                })
                .unwrap_or((false, false, false, false));
            if settings_changed {
                if restart_deferred {
                    log::info!(
                        "parked session {session_id}: deferring scheduled-send settings restart"
                    );
                } else {
                    if let Some(parked) = self.background.get_mut(&session_id) {
                        parked.shutdown_to_idle();
                    }
                    self.ensure_session_started(&session_id, cx);
                }
            } else if is_live {
                if self
                    .background
                    .get_mut(&session_id)
                    .is_some_and(|parked| parked.dispatch_next_pending().is_err())
                {
                    log::warn!(
                        "parked session {session_id}: scheduled dispatch failed (process gone)"
                    );
                }
            } else if has_queue {
                self.ensure_session_started(&session_id, cx);
            }
            self.emit_session_status(&session_id, cx);
        }
        cx.notify();
        self.reschedule_scheduled_wake(cx);
    }

    /// Deterministic queue mutation for cross-crate replica consistency tests.
    /// It deliberately skips provider dispatch so GPUI's test scheduler never
    /// observes a real adapter thread.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-support"))]
    pub fn queue_message_for_replica_test(&mut self, text: String, cx: &mut HostCx) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        active.push_queued(text, Vec::new());
        self.emit_active_session_status(cx);
        cx.notify();
    }

    pub(super) fn send_turn_assembled(
        &mut self,
        text: String,
        attachments: Vec<Attachment>,
        cx: &mut HostCx,
    ) {
        if self.relay_confirmation().is_some() {
            log::warn!("send deferred until the pending conversation relay is confirmed");
            return;
        }
        // Group C: a draft in worktree mode creates its worktree in the
        // background on first send, then re-enters send_turn once ready.
        if let Some(active) = self.active.as_ref()
            && active.draft
            && !active.preparing_worktree
            && let WorkspaceMode::NewWorktree { base } = active.draft_workspace.clone()
        {
            self.begin_worktree_prep(text, attachments, base, cx);
            return;
        }

        // The first send on a draft materializes it into a real (persisted)
        // session so the sidebar row appears; the provider then starts below.
        if self.active_is_draft()
            && let Err(err) = self.commit_draft(cx)
        {
            self.report_error(
                RuntimeError::PersistSession {
                    error: err.to_string(),
                },
                cx,
            );
            return;
        }

        let Some(active) = self.active.as_mut() else {
            return;
        };

        // Every send goes through the queue; what differs is whether it can
        // leave it right now. With a turn in flight the message simply waits
        // (Enter → QUEUE) and shows up in the composer's queue strip; nothing is
        // written to the transcript yet, so dropping it there leaves no trace.
        // See `on_turn_accepted`, which records the user message only after the
        // adapter confirms provider submission.
        active.push_queued(text, attachments);

        // If the user switched models — or a provider that can't switch its
        // approval mode live (Codex) had its mode changed, or a launch-time
        // option changed — while the provider is live, restart it first: the
        // queued turn then flushes on the fresh process, resumed from the stored
        // cursor with the current model + options + mode.
        let model_changed = active.model_changed_while_live();
        let approval_changed = active.approval_mode_changed_while_live();
        let options_changed = active.options_changed_while_live();
        let restart_deferred = active.settings_restart_deferred();
        if model_changed || approval_changed || options_changed {
            if restart_deferred {
                log::info!(
                    "deferring provider settings restart (background tasks: {}, delivery pending: {})",
                    active.background_task_count,
                    active.delivery_in_flight.is_some()
                );
            } else {
                if model_changed {
                    log::info!(
                        "model changed to {:?} while live; restarting provider before next turn",
                        active.meta.model
                    );
                } else if approval_changed {
                    log::info!(
                        "approval mode changed to {:?} while live; restarting provider before next turn",
                        active.meta.approval_mode
                    );
                } else {
                    log::info!(
                        "launch-time option changed while live; restarting provider before next turn"
                    );
                }
                active.shutdown_to_idle();
            }
        }
        let should_start = matches!(active.runtime, Runtime::Idle);
        let dispatch_failed = !restart_deferred && self.dispatch_next_queued(cx).is_err();
        if should_start {
            self.ensure_started(cx);
        }
        if dispatch_failed {
            self.report_error(RuntimeError::ProcessGone, cx);
        }
        self.emit_active_session_status(cx);
        cx.notify();
    }

    /// Display labels (from, to) for the confirmation dialog, when the current
    /// selection needs a canonical-timeline handoff before it can be sent.
    /// Custom profiles show their card title so a same-kind profile switch
    /// (e.g. official Claude → an Anthropic-compatible endpoint) reads as the
    /// backend change it is.
    pub(super) fn provider_label(&self, provider: ProviderKind, profile: Option<&str>) -> String {
        match profile {
            Some(id) => self.settings.profile_display_name(id),
            None => provider.display_name().to_string(),
        }
    }

    pub(super) fn relay_confirmation(&self) -> Option<(String, String)> {
        let active = self.active.as_ref()?;
        let pending = active.pending_relay.as_ref()?;
        if !has_meaningful_history(&active.timeline) {
            return None;
        }
        Some((
            self.provider_label(pending.from_provider, pending.from_profile.as_deref()),
            self.provider_label(active.meta.provider, active.meta.profile_id.as_deref()),
        ))
    }

    /// Confirm the pending provider handoff and send the user's clean message.
    /// The queue carries the provider-only transcript separately, so replay and
    /// chat rendering never expose the injected preamble as user-authored text.
    pub fn confirm_relay_and_send(
        &mut self,
        text: String,
        attachment_paths: Vec<PathBuf>,
        cx: &mut HostCx,
    ) {
        let (text, attachments) = self.assemble_user_message(text, attachment_paths);
        self.confirm_relay_and_send_assembled(text, attachments, cx);
        self.clear_consumed_draft_context(cx);
    }

    pub(super) fn confirm_relay_and_send_assembled(
        &mut self,
        text: String,
        attachments: Vec<Attachment>,
        cx: &mut HostCx,
    ) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        let Some(pending) = active.pending_relay.take() else {
            self.send_turn_assembled(text, attachments, cx);
            return;
        };
        let transcript = render_relay_transcript(
            &active.timeline,
            &active.meta.cwd,
            pending.from_provider,
            pending.from_model.as_deref(),
            RELAY_TRANSCRIPT_MAX_CHARS,
        );
        let event = AgentEvent::ProviderRelay {
            from_provider: pending.from_provider,
            from_model: pending.from_model,
            to_provider: active.meta.provider,
            to_model: active.meta.model.clone(),
        };
        let session_id = active.meta.id.clone();
        active.shutdown_to_idle();
        active.meta.resume_cursor = None;
        active.meta.pending_fork = false;
        active.meta.updated_at = now_secs();
        let meta = active.meta.clone();

        self.persist_meta(&meta, cx);
        self.record_event(&session_id, &event, cx);

        let Some(active) = self.active.as_mut() else {
            return;
        };
        active.push_queued(text, attachments);
        if let Some(message) = active.queue.last_mut() {
            message.relay_transcript = Some(transcript);
        }
        let dispatch_failed = self.dispatch_next_queued(cx).is_err();
        self.ensure_started(cx);
        if dispatch_failed {
            self.report_error(RuntimeError::ProcessGone, cx);
        }
        self.emit_active_session_status(cx);
        cx.notify();
    }

    /// Submit the first eligible queue entry when the live provider can accept
    /// a turn. It remains queued until the adapter emits its correlated `TurnAccepted`;
    /// only that provider-boundary acknowledgement persists the user bubble.
    pub(super) fn dispatch_next_queued(&mut self, _cx: &mut HostCx) -> Result<bool, ()> {
        let Some(active) = self.active.as_ref() else {
            return Ok(false);
        };
        if active.turn_in_flight || !matches!(active.runtime, Runtime::Live(_)) {
            return Ok(false);
        }
        self.active.as_mut().ok_or(())?.dispatch_next_pending()
    }

    /// Finalize one submitted queue entry. Queue-id correlation makes duplicate
    /// acceptance events idempotent, including after a provider close.
    pub(super) fn on_turn_accepted(&mut self, session_id: &str, delivery_id: u64, cx: &mut HostCx) {
        let is_active = self.active_session_id() == Some(session_id);
        let accepted = if is_active {
            self.active
                .as_mut()
                .and_then(|active| active.accept_turn_delivery(delivery_id))
        } else {
            self.background
                .get_mut(session_id)
                .and_then(|parked| parked.accept_turn_delivery(delivery_id))
        };
        let Some(message) = accepted else {
            log::debug!(
                "ignoring stale or duplicate turn acceptance {delivery_id} for {session_id}"
            );
            return;
        };
        self.record_user_message(
            session_id,
            &message.text,
            message.context_len,
            &message.attachments,
            cx,
        );
        if is_active {
            self.maybe_generate_title(&message.text, &message.attachments, cx);
        }
        self.emit_session_status(session_id, cx);
        cx.notify();
    }

    /// A parked session finished a turn: keep working through its queue, then
    /// retain its provider for the bounded resident-idle grace period.
    pub(super) fn on_background_turn_completed(&mut self, session_id: &str, cx: &mut HostCx) {
        let Some(parked) = self.background.get_mut(session_id) else {
            return;
        };
        parked.turn_in_flight = false;
        if !parked.queue.is_empty() && parked.launch_settings_changed_while_live() {
            if parked.background_task_count > 0 {
                log::info!(
                    "parked session {session_id}: deferring settings restart for {} background task(s)",
                    parked.background_task_count
                );
                self.emit_session_status(session_id, cx);
                cx.notify();
                return;
            }
            parked.shutdown_to_idle();
            self.ensure_session_started(session_id, cx);
            self.emit_session_status(session_id, cx);
            cx.notify();
            return;
        }
        if parked.queue.is_empty() {
            if parked.background_task_count > 0 {
                log::info!(
                    "retaining parked session {session_id} for {} background task(s)",
                    parked.background_task_count
                );
                self.emit_session_status(session_id, cx);
                cx.notify();
                return;
            }
            self.mark_resident_idle(session_id, cx);
            self.emit_session_status(session_id, cx);
            cx.notify();
            return;
        }
        match self
            .background
            .get_mut(session_id)
            .unwrap()
            .dispatch_next_pending()
        {
            Ok(true) => {}
            Ok(false) => {}
            Err(()) => {
                // The process is gone; the queue (with its unsent text)
                // survives for the user to find when they reopen the thread.
                log::warn!("parked session {session_id}: dispatch failed (process gone)");
            }
        }
        self.emit_session_status(session_id, cx);
        cx.notify();
    }

    /// Append a user message to the session transcript. Providers don't echo
    /// user input, so we record it as a synthetic canonical event and replay
    /// renders it identically.
    pub(super) fn record_user_message(
        &mut self,
        session_id: &str,
        text: &str,
        context_len: Option<usize>,
        attachments: &[Attachment],
        cx: &mut HostCx,
    ) {
        let user_event = AgentEvent::ItemCompleted(ThreadItem {
            id: format!("local-user-{}", uuid::Uuid::new_v4()),
            parent_item_id: None,
            content: ItemContent::UserMessage {
                text: text.to_owned(),
                context_len,
                attachments: attachment_paths(attachments),
            },
        });
        self.record_event(session_id, &user_event, cx);
    }

    /// Persist a pending steering bubble and return the exact id providers must
    /// echo in `SteerAccepted` after real delivery succeeds.
    pub(super) fn record_steer_request(
        &mut self,
        session_id: &str,
        text: &str,
        attachments: &[Attachment],
        cx: &mut HostCx,
    ) -> String {
        let request_id = format!("local-steer-{}", uuid::Uuid::new_v4());
        self.record_event(
            session_id,
            &AgentEvent::SteerRequested {
                request_id: request_id.clone(),
                text: text.to_owned(),
                attachments: attachment_paths(attachments),
            },
            cx,
        );
        request_id
    }

    /// Cmd+Enter: inject `text` into the turn that is ALREADY running, so the
    /// model picks it up at its next opportunity (typically its next tool call).
    ///
    /// Degrades honestly:
    ///   * no turn running → there is nothing to steer into, so just send;
    ///   * turn running, provider can't steer (ACP) → queue it and say so.
    ///
    /// A steered message IS part of the conversation, so it is recorded to the
    /// session JSONL as a user message (unlike a merely queued one).
    pub fn steer(&mut self, text: String, attachment_paths: Vec<PathBuf>, cx: &mut HostCx) {
        let (text, attachments) = self.assemble_user_message(text, attachment_paths);
        self.steer_assembled(text, attachments, cx);
        self.clear_consumed_draft_context(cx);
    }

    pub(super) fn steer_assembled(
        &mut self,
        text: String,
        attachments: Vec<Attachment>,
        cx: &mut HostCx,
    ) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        match active.route(true) {
            // Nothing is running, so there is nothing to steer into: an ordinary
            // send is exactly the right thing.
            SendRouting::Send | SendRouting::Queue => {
                self.send_turn_assembled(text, attachments, cx)
            }
            SendRouting::QueueUnsupported => {
                let agent = active.meta.provider.display_name();
                self.send_turn_assembled(text, attachments, cx);
                self.report_error(
                    RuntimeError::SteerUnsupported {
                        agent: agent.to_string(),
                    },
                    cx,
                );
            }
            SendRouting::Steer => {
                let session_id = active.meta.id.clone();
                let wire_text = wire_text_with_placeholder(text.clone(), &attachments);
                let wire_text = if active.pending_ultrathink {
                    format!("Ultrathink:\n{wire_text}")
                } else {
                    wire_text
                };
                // The steered message joins the running turn, so it belongs in
                // the transcript exactly like any other user message. (A merely
                // *queued* message does not — see `dispatch_next_queued`.)
                let request_id = self.record_steer_request(&session_id, &text, &attachments, cx);

                let Some(active) = self.active.as_mut() else {
                    return;
                };
                active.pending_ultrathink = false;
                // A steered orchestrate turn joins a turn already in flight and is
                // logged via `record_steer_request`, which carries no split, so it
                // renders as a plain bubble. Drop any staged split rather than let
                // it attach to a later queued message.
                active.pending_context_len = None;
                if active
                    .steer_now(request_id, wire_text, attachments)
                    .is_err()
                {
                    self.report_error(RuntimeError::ProcessGone, cx);
                }
                self.emit_session_status(&session_id, cx);
                cx.notify();
            }
        }
    }

    /// Queue strip: convert an already-queued message into a steering message —
    /// pull it out of the queue and inject it into the running turn.
    pub fn steer_queued(&mut self, id: u64, cx: &mut HostCx) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        let Some(message) = active.take_queued(id) else {
            return;
        };
        // `steer` consumes the session's armed Ultrathink flag, but this
        // message captured its own at queue time — re-arm so it rides along.
        active.pending_ultrathink = message.ultrathink;
        self.steer_assembled(message.text, message.attachments, cx);
        self.reschedule_scheduled_wake(cx);
    }

    /// Queue strip: drop a queued message (the row's ✕). It was never recorded,
    /// so nothing needs undoing. A submitted head cannot be removed until its
    /// correlated provider acknowledgement commits it.
    pub fn drop_queued(&mut self, id: u64, cx: &mut HostCx) {
        if let Some(active) = self.active.as_mut() {
            let _ = active.take_queued(id);
        }
        self.emit_active_session_status(cx);
        cx.notify();
        self.reschedule_scheduled_wake(cx);
    }

    pub fn interrupt(&mut self, cx: &mut HostCx) {
        if let Some(ActiveSession {
            runtime: Runtime::Live(commands),
            ..
        }) = &self.active
        {
            let _ = commands.try_send(SessionCommand::Interrupt);
        }
        cx.notify();
    }

    pub fn respond_approval(
        &mut self,
        request_id: String,
        decision: ApprovalDecision,
        cx: &mut HostCx,
    ) {
        if let Some(ActiveSession {
            runtime: Runtime::Live(commands),
            ..
        }) = &self.active
        {
            let _ = commands.try_send(SessionCommand::RespondApproval {
                request_id,
                decision,
            });
        }
        cx.notify();
    }

    /// Answer a pending user-input request (Claude `AskUserQuestion` / Codex
    /// `requestUserInput`). `answers` is keyed by [`UserInputQuestion::id`] with
    /// string (single-select / free text) or string-array (multi-select) values.
    pub fn respond_user_input(
        &mut self,
        request_id: String,
        answers: serde_json::Map<String, serde_json::Value>,
        cx: &mut HostCx,
    ) {
        if let Some(ActiveSession {
            runtime: Runtime::Live(commands),
            ..
        }) = &self.active
        {
            let _ = commands.try_send(SessionCommand::RespondUserInput {
                request_id,
                answers,
            });
        }
        cx.notify();
    }
}
