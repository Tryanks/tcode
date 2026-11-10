use super::active_session::{QueuedMessageKind, native_skill_invocation};
use super::*;

impl AppState {
    /// Submit a user turn. Starts the provider lazily if needed.
    pub fn send_turn(
        &mut self,
        target_id: &str,
        text: String,
        attachment_paths: Vec<PathBuf>,
        cx: &mut HostCx,
    ) {
        let (text, attachments) = self.assemble_user_message(target_id, text, attachment_paths);
        self.send_turn_assembled(target_id, text, attachments, cx);
        self.clear_consumed_draft_context(target_id, cx);
    }

    /// Schedule an in-memory user turn for this session. It captures the same
    /// provider options and composer context as an immediate send, but remains
    /// invisible to the persisted transcript until provider acceptance.
    pub fn schedule_turn(
        &mut self,
        target_id: &str,
        text: String,
        attachment_paths: Vec<PathBuf>,
        fire_at_unix_secs: u64,
        cx: &mut HostCx,
    ) {
        let not_before = UNIX_EPOCH + Duration::from_secs(fire_at_unix_secs);
        let (text, attachments) = self.assemble_user_message(target_id, text, attachment_paths);

        let commit_draft = self.resident(target_id).is_some_and(|active| {
            active.draft && !matches!(active.draft_workspace, WorkspaceMode::NewWorktree { .. })
        });
        if commit_draft {
            self.commit_draft(target_id, cx);
        }

        let default_window = self.default_context_window(target_id);
        let Some(active) = self.resident_mut(target_id) else {
            return;
        };
        active.push_scheduled(text, attachments, not_before);
        active.stamp_context_window_change(default_window);
        if let Some(message) = active.queue.last_mut() {
            message.delivery_key = cx.delivery_key.clone();
            message.author = cx.author.clone();
            message.space = cx.space.clone();
            message.origin = cx.origin.unwrap_or(message.origin);
        }
        let should_start = matches!(active.runtime, Runtime::Idle)
            && !(active.draft
                && matches!(active.draft_workspace, WorkspaceMode::NewWorktree { .. }));
        self.reactivate_session(target_id, cx);
        if should_start {
            // Starting now keeps the in-memory session parkable/resident across
            // navigation. Eligibility prevents the future turn from being sent
            // when provider startup completes.
            self.ensure_started(target_id, cx);
        }
        self.clear_consumed_draft_context(target_id, cx);
        self.reschedule_scheduled_wake(cx);
    }

    /// Replace the detached scheduler wake with one for the earliest deadline
    /// across active and parked sessions. The generation check makes every
    /// superseded timer harmless without needing cancellation support.
    pub(super) fn reschedule_scheduled_wake(&mut self, cx: &mut HostCx) {
        self.scheduler_generation = self.scheduler_generation.wrapping_add(1);
        let generation = self.scheduler_generation;
        let earliest = self
            .residents
            .live
            .values()
            .chain(self.residents.parked.values())
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
        let due: Vec<_> = self
            .residents
            .live
            .iter()
            .flat_map(|(session_id, session)| {
                session
                    .queue
                    .iter()
                    .filter(|m| m.not_before.is_some_and(|t| t <= now))
                    .map(|m| (session_id.clone(), m.id))
            })
            .collect();
        for (session_id, id) in due {
            let Some(message) = self
                .resident_mut(&session_id)
                .and_then(|s| s.take_queued(id))
            else {
                continue;
            };
            let author = std::mem::replace(&mut cx.author, message.author);
            let space = std::mem::replace(&mut cx.space, message.space);
            let origin = cx.origin.replace(message.origin);
            let key = std::mem::replace(&mut cx.delivery_key, message.delivery_key);
            self.send_turn_assembled(&session_id, message.text, message.attachments, cx);
            cx.author = author;
            cx.space = space;
            cx.origin = origin;
            cx.delivery_key = key;
        }

        let parked_ids: Vec<String> = self.residents.parked.keys().cloned().collect();
        for session_id in parked_ids {
            let mut had_due = false;
            if let Some(parked) = self.residents.parked.get_mut(&session_id) {
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

            let (restart, is_live, has_queue) = self
                .residents
                .parked
                .get(&session_id)
                .map(|parked| {
                    (
                        parked.restart_required() && parked.background_task_count == 0,
                        matches!(parked.runtime, Runtime::Live(_)),
                        !parked.queue.is_empty(),
                    )
                })
                .unwrap_or((false, false, false));
            if restart {
                if let Some(parked) = self.residents.parked.get_mut(&session_id) {
                    parked.shutdown_to_idle();
                }
                self.ensure_session_started(&session_id, cx);
            } else if is_live {
                if self.dispatch_next_queued(&session_id, cx).is_err() {
                    log::warn!(
                        "parked session {session_id}: scheduled dispatch failed (process gone)"
                    );
                }
            } else if has_queue {
                self.ensure_session_started(&session_id, cx);
            }
        }
        self.reschedule_scheduled_wake(cx);
    }

    pub(super) fn send_turn_assembled(
        &mut self,
        target_id: &str,
        text: String,
        attachments: Vec<Attachment>,
        cx: &mut HostCx,
    ) {
        let relay_pending = self.relay_confirmation(target_id).is_some();
        // A draft in worktree mode creates its worktree in the
        // background on first send, then re-enters send_turn once ready.
        if let Some(active) = self.resident(target_id)
            && active.draft
            && !active.preparing_worktree
            && let WorkspaceMode::NewWorktree { base } = active.draft_workspace.clone()
        {
            self.begin_worktree_prep(target_id, text, attachments, base, cx);
            return;
        }

        // The first send on a draft materializes it into a real (persisted)
        // session so the sidebar row appears; the provider then starts below.
        self.commit_draft(target_id, cx);
        self.reactivate_session(target_id, cx);
        self.resume_pull_request_watches(target_id, cx);

        let default_window = self.default_context_window(target_id);
        let rewinding = self
            .resident(target_id)
            .is_some_and(|active| self.rewind_requests.contains_key(&active.meta.id));
        let Some(active) = self.resident_mut(target_id) else {
            return;
        };

        // Every send goes through the queue; what differs is whether it can
        // leave it right now. With a turn in flight the message simply waits
        // (Enter → QUEUE) and shows up in the composer's queue strip; nothing is
        // written to the transcript yet, so dropping it there leaves no trace.
        // See `on_turn_accepted`, which records the user message only after the
        // adapter confirms provider submission.
        active.push_queued(text, attachments);
        active.stamp_context_window_change(default_window);
        if let Some(message) = active.queue.last_mut() {
            message.delivery_key = cx.delivery_key.clone();
            message.author = cx.author.clone();
            message.space = cx.space.clone();
            message.origin = cx.origin.unwrap_or(message.origin);
        }
        // Nothing goes to the backend the thread was switched to before the
        // relay to it is confirmed; the message waits in the queue for that.
        if relay_pending {
            return;
        }
        // The first message after a rewind goes ahead of the messages the
        // rewind held, which follow it.
        if std::mem::take(&mut active.queue_held)
            && let Some(message) = active.queue.pop()
        {
            active.queue.insert(0, message);
        }

        let model_changed = active.model_changed_while_live();
        // A restart would drop the rewind's answer.
        let restart_deferred = active.settings_restart_deferred() || rewinding;
        if active.restart_required() {
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
                } else {
                    log::info!(
                        "launch-time setting changed while live; restarting provider before next turn"
                    );
                }
                active.shutdown_to_idle();
            }
        }
        let should_start = matches!(active.runtime, Runtime::Idle);
        let dispatch_failed = self.dispatch_next_queued(target_id, cx).is_err();
        if should_start {
            self.ensure_started(target_id, cx);
        }
        if dispatch_failed {
            self.report_error(RuntimeError::ProcessGone, cx);
        }
    }

    /// Queue a turn the host or a child wrote for this thread behind any running turn, loading
    /// and starting its provider in the background when the thread is not resident.
    pub(super) fn queue_automatic_turn(
        &mut self,
        id: &str,
        push: impl FnOnce(&mut ActiveSession),
        cx: &mut HostCx,
    ) {
        if self.residents.live.contains_key(id) {
            let session = self.resident_mut(id).unwrap();
            push(session);

            // Match ordinary sends when a launch-time selection changed while
            // the provider was live.
            if session.restart_required() && !session.settings_restart_deferred() {
                session.shutdown_to_idle();
            }
            let should_start = matches!(session.runtime, Runtime::Idle);
            if self.dispatch_next_queued(id, cx).is_err() {
                self.report_error(RuntimeError::ProcessGone, cx);
            }
            if should_start {
                self.ensure_started(id, cx);
            }
            return;
        }

        if !self.residents.parked.contains_key(id)
            && let Some(meta) = self.sessions.iter().find(|meta| meta.id == id).cloned()
        {
            self.load_background_session(meta, cx);
        }
        if let Some(session) = self.resident_mut(id) {
            push(session);
            let idle_runtime = matches!(session.runtime, Runtime::Idle);
            let can_dispatch =
                !session.turn_in_flight && matches!(session.runtime, Runtime::Live(_));
            if can_dispatch {
                self.on_background_turn_completed(id, cx);
            }
            if idle_runtime {
                self.ensure_session_started(id, cx);
            }
        }
    }

    /// Display labels (from, to) for the confirmation dialog, when the current
    /// selection needs a canonical-timeline handoff before it can be sent.
    /// Custom profiles show their card title so a same-kind profile switch
    /// (e.g. official Claude → an Anthropic-compatible endpoint) reads as the
    /// backend change it is.
    pub(super) fn provider_label(&self, provider: ProviderKind, profile: Option<&str>) -> String {
        match profile {
            Some(id) => self.profile_display_name(id),
            None => provider.display_name().to_string(),
        }
    }

    /// The name a thread's Harness goes by in what the user reads: an ACP
    /// agent's own, else the product's with its account.
    pub(super) fn thread_harness_name(&self, session_id: &str) -> String {
        let Some(meta) = self.find_meta(session_id) else {
            return "Harness".to_string();
        };
        meta.acp_agent_id
            .as_deref()
            .and_then(|id| self.settings.acp_agent(id))
            .map(|agent| agent.name.clone())
            .unwrap_or_else(|| {
                self.profile_display_name(
                    meta.profile_id
                        .as_deref()
                        .unwrap_or(Settings::builtin_profile_id(meta.provider)),
                )
            })
    }

    /// Shutting a provider down drops its event pump, so a turn still running
    /// loses the completion that would close it in the timeline and on the
    /// clients' running indicators. Record that close here so the next turn
    /// opens on its own instead of joining the abandoned one.
    pub(super) fn close_abandoned_turn(&mut self, session_id: &str, cx: &mut HostCx) {
        let Some(turn_id) = self.conversation(session_id).and_then(|conversation| {
            conversation
                .turns
                .last()
                .filter(|turn| turn.status.is_none() && turn.end_ts.is_none())
                .map(|turn| turn.provider_turn_id.clone().unwrap_or_default())
        }) else {
            return;
        };
        self.on_event(
            session_id,
            AgentEvent::TurnCompleted {
                turn_id,
                status: TurnStatus::Interrupted,
                usage: None,
            },
            cx,
        );
    }

    pub(super) fn relay_confirmation(&self, target_id: &str) -> Option<(String, String)> {
        let active = self.resident(target_id)?;
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
        target_id: &str,
        text: String,
        attachment_paths: Vec<PathBuf>,
        cx: &mut HostCx,
    ) {
        let (text, attachments) = self.assemble_user_message(target_id, text, attachment_paths);
        self.confirm_relay_and_send_assembled(target_id, text, attachments, cx);
        self.clear_consumed_draft_context(target_id, cx);
    }

    pub(super) fn confirm_relay_and_send_assembled(
        &mut self,
        target_id: &str,
        text: String,
        attachments: Vec<Attachment>,
        cx: &mut HostCx,
    ) {
        let Some(active) = self.resident_mut(target_id) else {
            return;
        };
        let Some(pending) = active.pending_relay.take() else {
            self.send_turn_assembled(target_id, text, attachments, cx);
            return;
        };
        active.shutdown_to_idle();
        active.meta.resume_cursor = None;
        active.meta.pending_fork = None;
        let Some(transcript) = self.record_relay(target_id, pending, &text, cx) else {
            return;
        };

        let default_window = self.default_context_window(target_id);
        let Some(active) = self.resident_mut(target_id) else {
            return;
        };
        active.push_queued(text, attachments);
        active.stamp_context_window_change(default_window);
        if let Some(message) = active.queue.last_mut() {
            message.delivery_key = cx.delivery_key.clone();
            message.author = cx.author.clone();
            message.space = cx.space.clone();
            message.origin = cx.origin.unwrap_or(message.origin);
        }
        // The message that confirms the relay carries it, so it goes ahead of
        // the messages the relay held, which follow it.
        if let Some(mut message) = active.queue.pop() {
            message.relay_transcript = Some(transcript);
            active.queue.insert(0, message);
        }
        let dispatch_failed = self.dispatch_next_queued(target_id, cx).is_err();
        self.ensure_started(target_id, cx);
        if dispatch_failed {
            self.report_error(RuntimeError::ProcessGone, cx);
        }
    }

    /// Record the handoff that gives a fresh provider session the conversation so far,
    /// opening the turn of `text`, the message it travels with, and return the transcript
    /// that message carries to the provider. `from` is the backend the native history
    /// belongs to; a move to another directory ([`Self::move_session`]) is a relay too,
    /// from the same backend, and the event names the directory it left.
    fn record_relay(
        &mut self,
        target_id: &str,
        from: PendingRelay,
        text: &str,
        cx: &mut HostCx,
    ) -> Option<String> {
        let active = self.resident(target_id)?;
        let mut transcript = render_relay_transcript(
            self.conversation(target_id)?,
            active
                .meta
                .moved_from
                .as_deref()
                .unwrap_or(&active.meta.cwd),
            from.from_provider,
            from.from_model.as_deref(),
            RELAY_TRANSCRIPT_MAX_CHARS,
        );
        let active = self.resident_mut(target_id)?;
        let moved_from = active.meta.moved_from.take();
        active.meta.session_transfer_from = None;
        if let Some(from_dir) = &moved_from {
            transcript.push_str(&format!(
                "The thread has since moved from `{}` to `{}`, the directory you now run in.\n",
                from_dir.display(),
                active.meta.cwd.display()
            ));
        }
        let event = AgentEvent::ProviderRelay {
            from_provider: from.from_provider,
            from_model: from.from_model,
            to_provider: active.meta.provider,
            to_model: active.meta.model.clone(),
            prompt: Some(assemble_relay_prompt(&transcript, text)),
            moved_from,
        };
        active.meta.updated_at = now_secs();
        let meta = active.meta.clone();
        self.persist_meta(&meta, cx);
        self.close_abandoned_turn(&meta.id, cx);
        self.record_event(&meta.id, &event, cx);
        Some(transcript)
    }

    /// The conversation as the host holds it: the thread's log once it is read, its
    /// timeline until then.
    fn conversation(&self, id: &str) -> Option<&Timeline> {
        self.event_records
            .get(id)
            .map(SessionLog::fold)
            .or_else(|| self.resident(id).map(|active| &active.timeline))
    }

    /// Give the first turn of a fresh provider session the conversation so far, when the
    /// thread moved or its native session could not be carried to its new account; a
    /// thread with nothing worth handing over just starts over there. The handoff is
    /// made from the thread's log, so while the log is still being read this returns
    /// false and the message waits; [`Self::finish_hydration`] sends it.
    fn hand_off_conversation(&mut self, target_id: &str, cx: &mut HostCx) -> bool {
        if self
            .resident(target_id)
            .and_then(ActiveSession::handoff_message)
            .is_none()
        {
            return true;
        }
        if !self.event_records.contains_key(target_id) {
            self.hydrate_log(target_id, None, cx);
            return false;
        }
        // The turn a provider that died left open is closed first, since closing it
        // dispatches the next message, which then carries the handoff itself.
        self.close_abandoned_turn(target_id, cx);
        let Some(active) = self.resident(target_id) else {
            return true;
        };
        let Some((message_id, text)) = active.handoff_message() else {
            return true;
        };
        if !self
            .conversation(target_id)
            .is_some_and(has_meaningful_history)
        {
            let active = self.resident_mut(target_id).unwrap();
            active.meta.moved_from = None;
            active.meta.session_transfer_from = None;
            let meta = active.meta.clone();
            self.persist_meta(&meta, cx);
            return true;
        }
        let from_profile = match &active.meta.session_transfer_from {
            Some(home) if active.meta.moved_from.is_none() => {
                (!Settings::is_builtin_profile_id(home)).then(|| home.clone())
            }
            _ => active.meta.profile_id.clone(),
        };
        let from = PendingRelay {
            from_provider: active.meta.provider,
            from_model: active.meta.model.clone(),
            from_profile,
        };
        let transcript = self.record_relay(target_id, from, &text, cx);
        if let Some(message) = self
            .resident_mut(target_id)
            .and_then(|active| active.queue.iter_mut().find(|m| m.id == message_id))
        {
            message.relay_transcript = transcript;
        }
        true
    }

    /// Submit the first eligible queue entry when the live provider can accept
    /// a turn. It remains queued until the adapter emits its correlated `TurnAccepted`;
    /// only that provider-boundary acknowledgement persists the user bubble.
    pub(super) fn dispatch_next_queued(
        &mut self,
        target_id: &str,
        cx: &mut HostCx,
    ) -> Result<bool, ()> {
        self.refresh_policy_holds(target_id);
        let Some(active) = self.resident(target_id) else {
            return Ok(false);
        };
        // A message sent while a rewind is made goes to the conversation the
        // rewind leaves.
        if active.turn_in_flight
            || !matches!(active.runtime, Runtime::Live(_))
            || self.rewind_requests.contains_key(&active.meta.id)
        {
            return Ok(false);
        }
        if !self.hand_off_conversation(target_id, cx) {
            return Ok(false);
        }
        let sent = self
            .resident_mut(target_id)
            .ok_or(())?
            .dispatch_next_pending()?;
        if sent {
            let space = self.turn_space_of(target_id);
            self.enter_turn_space(target_id, space);
        }
        Ok(sent)
    }

    /// Finalize one submitted queue entry. Queue-id correlation makes duplicate
    /// acceptance events idempotent, including after a provider close.
    pub(super) fn on_turn_accepted(&mut self, session_id: &str, delivery_id: u64, cx: &mut HostCx) {
        let accepted = self
            .resident_mut(session_id)
            .and_then(|resident| resident.accept_turn_delivery(delivery_id));
        let Some(message) = accepted else {
            log::debug!(
                "ignoring stale or duplicate turn acceptance {delivery_id} for {session_id}"
            );
            return;
        };
        let author = std::mem::replace(&mut cx.author, message.author.clone());
        let origin = cx.origin.replace(message.origin);
        self.record_user_message(
            session_id,
            &message.text,
            &message.attachments,
            message.delivery_key.as_deref(),
            &message.reports,
            cx,
        );
        cx.author = author;
        cx.origin = origin;
        if let Some(meta) = self.meta_mut(session_id) {
            meta.updated_at = now_secs();
            let meta = meta.clone();
            self.persist_meta(&meta, cx);
        }
        if let QueuedMessageKind::PullRequestWake { key, id } = &message.kind {
            self.acknowledge_pull_request_wake(session_id, key, id, &message.text, cx);
        }
        if message.kept_settings {
            self.record_event(session_id, &AgentEvent::SettingsKept, cx);
        } else if let Some(window) = message.context_window_changed {
            self.record_event(session_id, &AgentEvent::ContextWindowChanged { window }, cx);
        }
        self.maybe_generate_title(session_id, &message.text, &message.attachments, cx);
    }

    /// A parked session finished a turn: keep working through its queue, then
    /// retain its provider for the bounded resident-idle grace period.
    pub(super) fn on_background_turn_completed(&mut self, session_id: &str, cx: &mut HostCx) {
        let Some(parked) = self.residents.parked.get_mut(session_id) else {
            return;
        };
        parked.turn_in_flight = false;
        let waiting = !parked.queue.is_empty() && !parked.queue_held;
        if waiting && parked.restart_required() && parked.background_task_count == 0 {
            parked.shutdown_to_idle();
            self.ensure_session_started(session_id, cx);
            return;
        }
        if !waiting {
            if parked.cwd_changed_while_live() && parked.background_task_count == 0 {
                parked.shutdown_to_idle();
            }
            if parked.background_task_count > 0 {
                log::info!(
                    "retaining parked session {session_id} for {} background task(s)",
                    parked.background_task_count
                );
                return;
            }
            self.mark_resident_idle(session_id, cx);
            return;
        }
        match self.dispatch_next_queued(session_id, cx) {
            Ok(true) => {}
            Ok(false) => {}
            Err(()) => {
                // The process is gone; the queue (with its unsent text)
                // survives for the user to find when they reopen the thread.
                log::warn!("parked session {session_id}: dispatch failed (process gone)");
            }
        }
    }

    /// Append a user message to the session transcript. Providers don't echo
    /// user input, so we record it as a synthetic canonical event and replay
    /// renders it identically.
    pub(super) fn record_user_message(
        &mut self,
        session_id: &str,
        text: &str,
        attachments: &[Attachment],
        delivery_key: Option<&str>,
        reports: &[ReportHandover],
        cx: &mut HostCx,
    ) {
        let user_event = AgentEvent::ItemCompleted(ThreadItem {
            id: format!(
                "local-user-{}",
                delivery_key
                    .map(str::to_owned)
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
            ),
            parent_item_id: None,
            content: ItemContent::UserMessage {
                text: text.to_owned(),
                context_len: None,
                attachments: attachment_paths(attachments),
            },
        });
        self.record_handover(session_id, &user_event, reports, cx);
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
        self.reactivate_session(session_id, cx);
        let request_id = format!(
            "local-steer-{}",
            cx.delivery_key
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
        );
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
    /// Persist the steering request before delivery; its acceptance or rejection
    /// is recorded separately so pending input survives replay.
    pub fn steer(
        &mut self,
        target_id: &str,
        text: String,
        attachment_paths: Vec<PathBuf>,
        cx: &mut HostCx,
    ) {
        let (text, attachments) = self.assemble_user_message(target_id, text, attachment_paths);
        self.steer_assembled(target_id, text, attachments, cx);
        self.clear_consumed_draft_context(target_id, cx);
    }

    pub(super) fn steer_assembled(
        &mut self,
        target_id: &str,
        text: String,
        attachments: Vec<Attachment>,
        cx: &mut HostCx,
    ) {
        let Some(active) = self.resident(target_id) else {
            return;
        };
        match active.route(true) {
            // Nothing is running, so there is nothing to steer into: an ordinary
            // send is exactly the right thing.
            SendRouting::Send | SendRouting::Queue => {
                self.send_turn_assembled(target_id, text, attachments, cx)
            }
            SendRouting::QueueUnsupported => {
                let agent = active.meta.provider.display_name();
                self.send_turn_assembled(target_id, text, attachments, cx);
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
                let wire_text = native_skill_invocation(wire_text, &active.provider_commands);
                // Persist the pending request before handing it to the provider.
                let request_id = self.record_steer_request(&session_id, &text, &attachments, cx);

                let steer = PendingSteer {
                    request_id,
                    text: text.clone(),
                    attachments: attachments.clone(),
                    author: cx.author.clone(),
                    space: cx.space.clone(),
                    origin: cx.origin,
                    reports: Vec::new(),
                };
                if !self.hand_over_steer(target_id, steer, wire_text, cx) {
                    self.report_error(RuntimeError::ProcessGone, cx);
                    self.send_turn_assembled(target_id, text, attachments, cx);
                }
            }
        }
    }

    /// Give the provider a recorded steer, kept until it confirms so the words
    /// survive a rejection or the process going away. `false` when the provider
    /// is gone, with the steer withdrawn for the caller to send another way.
    pub(super) fn hand_over_steer(
        &mut self,
        target_id: &str,
        steer: PendingSteer,
        wire_text: String,
        cx: &mut HostCx,
    ) -> bool {
        let Some(active) = self.resident_mut(target_id) else {
            return false;
        };
        let request_id = steer.request_id.clone();
        let attachments = steer.attachments.clone();
        let space = steer.space.clone();
        active.pending_steers.push(steer);
        if active
            .steer_now(request_id.clone(), wire_text, attachments)
            .is_ok()
        {
            if space.is_some() {
                self.enter_turn_space(target_id, space);
            }
            return true;
        }
        active.pending_steers.pop();
        self.record_event(target_id, &AgentEvent::SteerRejected { request_id }, cx);
        false
    }

    /// Steers the provider took and never confirmed died with its process: the
    /// model did not see them, so they go out as ordinary turns.
    pub(super) fn requeue_unconfirmed_steers(&mut self, session_id: &str, cx: &mut HostCx) {
        let steers = self
            .resident_mut(session_id)
            .map(|resident| std::mem::take(&mut resident.pending_steers))
            .unwrap_or_default();
        for steer in steers {
            self.record_event(
                session_id,
                &AgentEvent::SteerRejected {
                    request_id: steer.request_id.clone(),
                },
                cx,
            );
            if let Some(resident) = self.resident_mut(session_id) {
                resident.requeue_steer(steer);
            }
        }
    }

    /// Queue strip: convert an already-queued message into a steering message —
    /// pull it out of the queue and inject it into the running turn.
    pub fn steer_queued(&mut self, target_id: &str, id: u64, cx: &mut HostCx) {
        // A message its space is holding must not reach the running turn
        // either.
        if self.resident(target_id).is_some_and(|active| {
            active
                .queue
                .iter()
                .any(|message| message.id == id && self.queued_held(active, message))
        }) {
            return;
        }
        let Some(active) = self.resident_mut(target_id) else {
            return;
        };
        let Some(message) = active.take_queued(id) else {
            return;
        };
        let command_key = std::mem::replace(&mut cx.delivery_key, message.delivery_key);
        let author = std::mem::replace(&mut cx.author, message.author);
        let space = std::mem::replace(&mut cx.space, message.space);
        let origin = cx.origin.replace(message.origin);
        self.steer_assembled(target_id, message.text, message.attachments, cx);
        cx.delivery_key = command_key;
        cx.author = author;
        cx.space = space;
        cx.origin = origin;
        self.reschedule_scheduled_wake(cx);
    }

    /// Queue strip: drop a queued message (the row's ✕). It was never recorded,
    /// so nothing needs undoing. A submitted entry cannot be removed until its
    /// correlated provider acknowledgement commits it.
    pub fn drop_queued(&mut self, target_id: &str, id: u64, cx: &mut HostCx) {
        // A report the user drops is not sent again by the next start.
        if let Some(message) = self
            .resident_mut(target_id)
            .and_then(|active| active.take_queued(id))
        {
            self.reports_dropped(&message.reports, cx);
        }
        self.reschedule_scheduled_wake(cx);
        self.evaluate_thread_settlement(target_id, cx);
    }

    /// Queue strip: reorder by dragging a row. Dispatch takes eligible
    /// entries in queue order, so this is the order they are sent in.
    pub fn move_queued(&mut self, target_id: &str, id: u64, before: Option<u64>) {
        if let Some(active) = self.resident_mut(target_id) {
            active.move_queued(id, before);
        }
    }

    pub fn retry_turn(
        &mut self,
        target_id: &str,
        turn_id: String,
        cx: &mut HostCx,
    ) -> Result<(), tcode_protocol::ProtocolError> {
        let active = self
            .resident_mut(target_id)
            .ok_or_else(|| provider_command_error("The thread is no longer available."))?;
        if active.timeline.turn_running
            || !active
                .timeline
                .turn_retry
                .as_ref()
                .is_some_and(|retry| retry.turn_id == turn_id)
        {
            return Err(provider_command_error(
                "This turn can no longer be retried.",
            ));
        }
        let Runtime::Live(commands) = &active.runtime else {
            return Err(provider_command_error("The provider is no longer running."));
        };
        commands
            .try_send(SessionCommand::RetryTurn { turn_id })
            .map_err(provider_command_error)?;
        active.turn_in_flight = true;
        self.enter_member_turn(target_id, cx);
        Ok(())
    }

    /// A turn a member's request starts without the queue (a retry, a
    /// compaction, a goal change) runs under the member's space, as a queued
    /// message from them would.
    fn enter_member_turn(&mut self, target_id: &str, cx: &HostCx) {
        if cx.space.is_some() {
            self.enter_turn_space(target_id, cx.space.clone());
        }
    }

    pub fn interrupt(
        &mut self,
        target_id: &str,
        cx: &mut HostCx,
    ) -> Result<(), tcode_protocol::ProtocolError> {
        self.pause_pull_request_watches(target_id, cx);
        // An idle provider never answers an interrupt, which would leave the thread
        // stopping, and background work takes no interrupt: Claude Code acknowledges it
        // and the task runs on (2.1.318); `stop_background_task` is what stops one.
        let Some(active) = self.resident(target_id).filter(|active| {
            active.turn_in_flight
                || active.delivery_in_flight.is_some()
                || active.timeline.turn_running
        }) else {
            return Ok(());
        };
        let Runtime::Live(commands) = &active.runtime else {
            return Err(provider_command_error("The provider is no longer running."));
        };
        commands
            .try_send(SessionCommand::Interrupt)
            .map_err(provider_command_error)?;
        if let Some(active) = self.resident_mut(target_id) {
            active.interrupt_requested = true;
            active.interrupt_requested_at.get_or_insert_with(now_millis);
        }
        self.cancel_computer_use_feedback(target_id);
        Ok(())
    }

    /// Kill the thread's provider process, for a harness that stops answering:
    /// its running turn ends where it is and the next message starts it again.
    pub fn force_terminate(
        &mut self,
        target_id: &str,
        cx: &mut HostCx,
    ) -> Result<(), tcode_protocol::ProtocolError> {
        let Some(active) = self.resident_mut(target_id) else {
            return Err(provider_command_error("The thread is not open."));
        };
        let session_id = active.meta.id.clone();
        if matches!(active.runtime, Runtime::Idle) {
            return Ok(());
        }
        active.shutdown_to_idle();
        self.requeue_unconfirmed_steers(&session_id, cx);
        self.clear_approvals(&session_id);
        self.close_abandoned_turn(&session_id, cx);
        Ok(())
    }

    /// Move the thread's running call `item_id` into the background.
    pub fn move_to_background(
        &mut self,
        target_id: &str,
        item_id: String,
    ) -> Result<(), tcode_protocol::ProtocolError> {
        let active = self
            .resident(target_id)
            .ok_or_else(|| provider_command_error("The thread is not open."))?;
        if !active
            .meta
            .provider
            .caps()
            .background
            .is_some_and(|control| control.move_running)
        {
            return Err(provider_command_error(
                "This provider cannot move a call into the background.",
            ));
        }
        let Runtime::Live(commands) = &active.runtime else {
            return Err(provider_command_error("The provider is no longer running."));
        };
        commands
            .try_send(SessionCommand::MoveToBackground { item_id })
            .map_err(provider_command_error)
    }

    /// Stop one piece of the thread's background work, or all of it with `None`.
    pub fn stop_background_task(
        &mut self,
        target_id: &str,
        task_id: Option<String>,
    ) -> Result<(), tcode_protocol::ProtocolError> {
        let active = self
            .resident_mut(target_id)
            .ok_or_else(|| provider_command_error("The thread is not open."))?;
        if !active
            .meta
            .provider
            .caps()
            .background
            .is_some_and(|control| control.stop.stops())
        {
            return Err(provider_command_error(
                "This provider cannot stop background work.",
            ));
        }
        let Runtime::Live(commands) = &active.runtime else {
            return Err(provider_command_error("The provider is no longer running."));
        };
        commands
            .try_send(SessionCommand::StopBackgroundTask {
                task_id: task_id.clone(),
            })
            .map_err(provider_command_error)?;
        active.background.stopping(task_id.as_deref(), now_millis());
        Ok(())
    }

    /// Compaction is the provider's own turn: it is not started over one that
    /// is already running or still being delivered. An idle provider is started
    /// first and compacts once it is live.
    pub fn compact_context(
        &mut self,
        target_id: &str,
        cx: &mut HostCx,
    ) -> Result<(), tcode_protocol::ProtocolError> {
        let Some(active) = self.resident_mut(target_id) else {
            return Err(provider_command_error("The thread is not open."));
        };
        if active.turn_in_flight
            || active.delivery_in_flight.is_some()
            || active.timeline.turn_running
        {
            return Err(provider_command_error("A turn is still running."));
        }
        match &active.runtime {
            Runtime::Live(commands) => commands
                .try_send(SessionCommand::Compact)
                .map_err(provider_command_error)?,
            Runtime::Idle | Runtime::Starting { .. } => {
                active.requested_command = Some(SessionCommand::Compact);
                self.ensure_started(target_id, cx);
            }
        }
        self.enter_member_turn(target_id, cx);
        Ok(())
    }

    /// A goal change ([`SessionCommand::SetGoal`] or
    /// [`SessionCommand::ClearGoal`]). A harness whose change is a turn of its
    /// own is held to the same rule as compaction; an idle provider is started
    /// first and takes the change once it is live.
    pub fn change_goal(
        &mut self,
        target_id: &str,
        command: SessionCommand,
        cx: &mut HostCx,
    ) -> Result<(), tcode_protocol::ProtocolError> {
        let Some(active) = self.resident_mut(target_id) else {
            return Err(provider_command_error("The thread is not open."));
        };
        let Some(control) = active.meta.provider.caps().goals else {
            return Err(provider_command_error("This provider has no goal."));
        };
        if !control.live
            && (active.turn_in_flight
                || active.delivery_in_flight.is_some()
                || active.timeline.turn_running)
        {
            return Err(provider_command_error("A turn is still running."));
        }
        let turn = !control.live;
        match &active.runtime {
            Runtime::Live(commands) => {
                commands.try_send(command).map_err(provider_command_error)?
            }
            Runtime::Idle | Runtime::Starting { .. } => {
                active.requested_command = Some(command);
                self.ensure_started(target_id, cx);
            }
        }
        if turn {
            self.enter_member_turn(target_id, cx);
        }
        Ok(())
    }

    pub fn respond_approval(
        &mut self,
        target_id: &str,
        request_id: String,
        decision: ApprovalDecision,
        cx: &mut HostCx,
    ) -> Result<(), tcode_protocol::ProtocolError> {
        self.respond_session_approval(target_id, request_id.clone(), decision)
            .map_err(provider_command_error)?;
        self.decision_authors
            .remove(&(target_id.to_string(), request_id.clone()));
        if let Some(author) = &cx.author {
            self.decision_authors
                .insert((target_id.to_string(), request_id), author.clone());
        }
        Ok(())
    }

    /// Answer the host's pending user-input request, acknowledging only after
    /// the provider command channel has accepted the answer.
    pub fn respond_user_input(
        &mut self,
        target_id: &str,
        request_id: String,
        answers: serde_json::Map<String, serde_json::Value>,
        cx: &mut HostCx,
    ) -> Result<(), tcode_protocol::ProtocolError> {
        let Some(ActiveSession {
            runtime: Runtime::Live(commands),
            ..
        }) = self.resident(target_id)
        else {
            return Err(provider_command_error("The provider is no longer running."));
        };
        commands
            .try_send(SessionCommand::RespondUserInput {
                request_id: request_id.clone(),
                answers,
            })
            .map_err(provider_command_error)?;
        self.decision_authors
            .remove(&(target_id.to_string(), request_id.clone()));
        if let Some(author) = &cx.author {
            self.decision_authors
                .insert((target_id.to_string(), request_id), author.clone());
        }
        Ok(())
    }
}

fn provider_command_error(error: impl std::fmt::Display) -> tcode_protocol::ProtocolError {
    tcode_protocol::ProtocolError {
        code: "provider_unavailable".into(),
        message: error.to_string(),
    }
}
