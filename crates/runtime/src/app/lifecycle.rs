use super::*;

impl AppState {
    /// Request a provider-owned restore point. No local Git or transcript
    /// operation is performed: the canonical timeline changes only after the
    /// provider confirms the native request.
    pub fn rewind_turn(&mut self, turn: usize, mode: RewindMode, cx: &mut HostCx) {
        let Some(active) = self.residents.active.as_ref() else {
            return;
        };
        if active.meta.provider != ProviderKind::ClaudeCode
            || active.turn_in_flight
            || active.delivery_in_flight.is_some()
            || active.background_task_count > 0
            || active.timeline.turn_running
            || !active.queue.is_empty()
        {
            self.report_error(RuntimeError::NativeRewindBlocked, cx);
            return;
        }
        let Some(checkpoint_id) = active
            .timeline
            .turns
            .get(turn)
            .and_then(|turn| turn.provider_checkpoint_id.clone())
        else {
            self.report_error(RuntimeError::NativeRewindBlocked, cx);
            return;
        };
        // Claude Code 2.1.214 cannot persist a conversation rewind before the
        // first assistant anchor. File-only rewind remains valid there.
        if turn == 0 && mode.includes_conversation() {
            self.report_error(RuntimeError::NativeRewindBlocked, cx);
            return;
        }
        let session_id = active.meta.id.clone();
        if self.pending_native_rewinds.contains_key(&session_id) {
            self.report_error(RuntimeError::NativeRewindBlocked, cx);
            return;
        }
        let live_commands = match &active.runtime {
            Runtime::Live(commands) => Some(commands.clone()),
            Runtime::Idle | Runtime::Starting { .. } => None,
        };
        self.pending_native_rewinds
            .insert(session_id.clone(), (checkpoint_id.clone(), mode));
        if let Some(commands) = live_commands {
            if commands
                .try_send(SessionCommand::Rewind {
                    checkpoint_id,
                    mode,
                })
                .is_err()
            {
                self.pending_native_rewinds.remove(&session_id);
                self.report_error(RuntimeError::ProcessGone, cx);
            }
        } else {
            self.ensure_started(cx);
        }
    }

    #[cfg(test)]
    pub(super) fn native_rewind_pending(&self) -> bool {
        self.active_session_id()
            .is_some_and(|id| self.pending_native_rewinds.contains_key(id))
    }

    /// Spawn the provider process for the active session if it isn't running.
    pub(super) fn ensure_started(&mut self, cx: &mut HostCx) {
        let Some(session_id) = self.active_session_id().map(str::to_owned) else {
            return;
        };
        self.ensure_session_started(&session_id, cx);
    }

    /// Spawn a provider for either the foreground session or a parked child.
    pub(super) fn ensure_session_started(&mut self, session_id: &str, cx: &mut HostCx) {
        let idle = self
            .residents
            .active
            .as_ref()
            .filter(|active| active.meta.id == session_id)
            .map(|active| matches!(active.runtime, Runtime::Idle))
            .or_else(|| {
                self.residents
                    .parked
                    .get(session_id)
                    .map(|active| matches!(active.runtime, Runtime::Idle))
            })
            .unwrap_or(false);
        if !idle {
            return;
        }
        self.next_start_generation = self
            .next_start_generation
            .checked_add(1)
            .expect("provider start generation overflow");
        let generation = self.next_start_generation;
        let active = self.resident_mut(session_id).unwrap();
        active.runtime = Runtime::Starting { generation };
        active.idle_since = None;
        // Remember the model + approval mode this process is being launched
        // with so a later switch can detect the mismatch and restart.
        active.live_model = active.meta.model.clone();
        active.live_approval_mode = Some(active.meta.approval_mode);
        active.live_option_selections = active.meta.option_selections.clone();

        let meta = active.meta.clone();
        let settings = self.settings.clone();
        let settings_store = self.settings_store.clone();
        let preview_registration = if meta.provider == ProviderKind::Pi {
            None
        } else {
            self.preview_registration_for(&meta)
        };
        let orchestrate_registration = self.orchestrate_registration_for(&meta);
        let computer_use_registration = self.mcp.computer_use_registration.clone();
        let provider_launcher = self.provider_launcher.clone();
        let session_id = meta.id.clone();
        if let Some(cursor) = &meta.resume_cursor {
            log::info!(
                "starting provider {:?} with resume cursor: {}",
                meta.provider,
                cursor.0
            );
        } else {
            log::info!("starting provider {:?} (fresh session)", meta.provider);
        }

        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let env_meta = meta.clone();
            let env_settings = settings.clone();
            let launch_env = host_cx
                .unblock(move || session_launch_env(&env_settings, &settings_store, &env_meta))
                .await;
            let opts = session_options(
                &meta,
                &settings,
                launch_env,
                preview_registration,
                orchestrate_registration,
                computer_use_registration,
            );
            let result = provider_launcher.launch(meta.provider, opts).await;
            host_cx.enqueue(move |state, cx| {
                let matches_active = state.residents.active.as_ref().is_some_and(|active| {
                    active.meta.id == session_id && active.is_starting_generation(generation)
                });
                // The session may have been parked (thread switch) while its
                // start was in flight; the attempt then adopts the parked entry.
                let matches_parked = !matches_active
                    && state
                        .resident(&session_id)
                        .is_some_and(|parked| parked.is_starting_generation(generation));
                match result {
                    Ok(handle) => {
                        if !matches_active && !matches_parked {
                            // Superseded by a newer start, or the session is gone.
                            let _ = handle.commands.try_send(SessionCommand::Shutdown);
                            return;
                        }
                        let commands = handle.commands.clone();
                        let events = handle.events.clone();
                        let pump_session = session_id.clone();
                        let pump_commands = handle.commands.clone();
                        let pump_cx = cx.clone();
                        let pump = HostCx::spawn_background(cx, async move {
                            while let Ok(event) = events.recv().await {
                                let event_session = pump_session.clone();
                                pump_cx.enqueue(move |state, cx| {
                                    state.on_event(&event_session, event, cx);
                                });
                            }
                            pump_cx.enqueue(move |state, cx| {
                                state.on_event_stream_ended(&pump_session, &pump_commands, cx);
                            });
                        });
                        if let Some(resident) = state.resident_mut(&session_id) {
                            resident.runtime = Runtime::Live(commands.clone());
                            resident._pump = Some(pump);
                        }
                        if matches_active {
                            if let Some((checkpoint_id, mode)) =
                                state.pending_native_rewinds.get(&session_id).cloned()
                            {
                                if commands
                                    .try_send(SessionCommand::Rewind {
                                        checkpoint_id,
                                        mode,
                                    })
                                    .is_err()
                                {
                                    state.pending_native_rewinds.remove(&session_id);
                                    state.report_error(RuntimeError::ProcessGone, cx);
                                }
                            } else if state.dispatch_next_queued(cx).is_err() {
                                state.report_error(RuntimeError::ProcessGone, cx);
                            }
                        } else {
                            if let Some((checkpoint_id, mode)) =
                                state.pending_native_rewinds.get(&session_id).cloned()
                            {
                                if commands
                                    .try_send(SessionCommand::Rewind {
                                        checkpoint_id,
                                        mode,
                                    })
                                    .is_err()
                                {
                                    state.pending_native_rewinds.remove(&session_id);
                                    state.report_error(RuntimeError::ProcessGone, cx);
                                }
                            } else {
                                // Work through the parked queue exactly as a
                                // finished background turn would.
                                state.on_background_turn_completed(&session_id, cx);
                            }
                        }
                    }
                    Err(err) => {
                        if matches_active || matches_parked {
                            // The queue is deliberately KEPT in both cases: it
                            // holds text the user typed but that was never
                            // sent. It stays visible in the queue strip and
                            // flushes on the next successful start; clearing it
                            // would destroy their words along with the process
                            // (the T3 bug family this app tests against).
                            if let Some(resident) = state.resident_mut(&session_id) {
                                resident.runtime = Runtime::Idle;
                                resident.delivery_in_flight = None;
                                resident.turn_in_flight = false;
                                resident.background_task_count = 0;
                            }
                            state.pending_native_rewinds.remove(&session_id);
                            let error_event = AgentEvent::ProviderStartFailed {
                                error: err.to_string(),
                            };
                            state.record_event(&session_id, &error_event, cx);
                            let is_child = state.sessions.iter().any(|meta| {
                                meta.id == session_id && meta.parent_session_id.is_some()
                            });
                            if is_child {
                                if let Some(child) = state.resident_mut(&session_id) {
                                    child.queue.clear();
                                }
                                state.deliver_child_callback(&session_id, TurnStatus::Failed, cx);
                            }
                            state.report_error(
                                RuntimeError::ProviderStart {
                                    error: err.to_string(),
                                },
                                cx,
                            );
                        }
                    }
                }
            });
        });
    }

    /// The provider's event stream ended without a `SessionClosed`. If the
    /// session still owns that exact provider (same command channel, still
    /// Live), the adapter died without announcing it — and the lifecycle flags
    /// (`turn_in_flight`, `delivery_in_flight`, `background_task_count`) have
    /// no other reset path, so the thread would sit at "Working" forever.
    /// Synthesize the close so the ordinary teardown runs no matter which
    /// adapter exit path forgot to emit it. The same-channel check keeps a
    /// stale pump (session already closed, restarted, or shut down mid-event)
    /// from tearing down a successor provider.
    pub(super) fn on_event_stream_ended(
        &mut self,
        session_id: &str,
        pump_commands: &smol::channel::Sender<SessionCommand>,
        cx: &mut HostCx,
    ) {
        let still_owned = self
            .resident(session_id)
            .is_some_and(|session| match &session.runtime {
                Runtime::Live(current) => current.same_channel(pump_commands),
                _ => false,
            });
        if !still_owned {
            return;
        }
        log::warn!(
            "provider event stream for {session_id} ended without SessionClosed; synthesizing close"
        );
        self.on_event(
            session_id,
            AgentEvent::SessionClosed {
                reason: Some("provider event stream ended without a close".into()),
            },
            cx,
        );
    }

    pub(super) fn meta_mut(&mut self, session_id: &str) -> Option<&mut SessionMeta> {
        // Parked sessions keep receiving meta updates (resume cursor, updated_at)
        // — losing the cursor while parked would break the next cold resume.
        self.resident_mut(session_id)
            .map(|session| &mut session.meta)
    }

    pub(super) fn persist_meta(&mut self, meta: &SessionMeta, cx: &mut HostCx) {
        // An update landing on the conversation the user is currently viewing
        // is already read: advance the last-visited watermark alongside it so
        // switching away later does not surface a stale unread dot. Threads the
        // user is not viewing keep their watermark (and their dot), as does an
        // explicit "mark unread" (which only rewrites the watermark).
        if self.active_session_id() == Some(meta.id.as_str()) {
            let visited = self.settings.last_visited.entry(meta.id.clone());
            let visited = visited.or_insert(meta.updated_at);
            if *visited < meta.updated_at {
                *visited = meta.updated_at;
                self.persist_settings(cx);
            }
        }
        self.enqueue_store_write(
            StoreWrite::UpsertMeta {
                meta: Box::new(meta.clone()),
                initial: false,
            },
            cx,
        );
        // Reflect the upsert in memory instead of reloading the whole index
        // from disk: `persist_meta` runs on every turn,
        // where re-reading and re-parsing a large sessions.json stalls the UI.
        // `sessions` stays newest-first, matching `load_index`'s order.
        self.upsert_session_in_memory(meta.clone());
    }

    pub(crate) fn shutdown_active(&mut self, _cx: &mut HostCx) {
        if let Some(session_id) = self.active_session_id().map(str::to_string) {
            self.clear_approvals(&session_id);
            self.pending_native_rewinds.remove(&session_id);
        }
        if let Some(active) = self.residents.active.take()
            && let Runtime::Live(commands) = active.runtime
        {
            let _ = commands.try_send(SessionCommand::Shutdown);
        }
    }

    /// Shut down every provider process before the application exits.
    pub fn shutdown_all(&mut self, cx: &mut HostCx) {
        self.shutdown_active(cx);
        for (_, parked) in self.residents.parked.drain() {
            if let Runtime::Live(commands) = parked.runtime {
                let _ = commands.try_send(SessionCommand::Shutdown);
            }
        }
        // Drop every conversation-owned PTY, including those parked while an
        // idle thread was off screen.
        self.terminal_workspaces.clear();
        self.pending_native_rewinds.clear();
    }

    /// Leave the active session without killing its provider or in-memory work.
    /// Every "switch away" path goes through here; only destructive paths use
    /// `shutdown_active` directly.
    pub(super) fn park_active(&mut self, cx: &mut HostCx) {
        let Some(mut active) = self.residents.active.take() else {
            return;
        };
        self.park_terminal_workspace(&mut active);
        let native_rewind_pending = self.pending_native_rewinds.contains_key(&active.meta.id);
        let has_work = active.turn_in_flight
            || active.delivery_in_flight.is_some()
            || !active.queue.is_empty()
            || active.background_task_count > 0
            || native_rewind_pending;
        let parkable = matches!(active.runtime, Runtime::Live(_) | Runtime::Starting { .. });
        if parkable {
            log::info!(
                "parking session {} (turn in flight: {}, queued: {}, background tasks: {})",
                active.meta.id,
                active.turn_in_flight,
                active.queue.len(),
                active.background_task_count
            );
            let session_id = active.meta.id.clone();
            if has_work {
                active.idle_since = None;
            }
            self.residents.park(active);
            self.reschedule_scheduled_wake(cx);
            if !has_work {
                self.mark_resident_idle(&session_id, cx);
            }
        }
    }

    /// Start a fresh idle grace period, enforce the resident LRU bound, and
    /// reap only if no intervening work or re-adoption made the timer stale.
    pub(super) fn mark_resident_idle(&mut self, session_id: &str, cx: &mut HostCx) {
        let fully_idle = self
            .residents
            .parked
            .get(session_id)
            .is_some_and(|session| {
                session.queue.is_empty()
                    && !session.turn_in_flight
                    && session.delivery_in_flight.is_none()
                    && session.background_task_count == 0
            })
            && !self.pending_native_rewinds.contains_key(session_id);
        if !fully_idle {
            return;
        }

        let idle_since = Instant::now();
        self.residents
            .parked
            .get_mut(session_id)
            .unwrap()
            .idle_since = Some(idle_since);

        let oldest = {
            let mut residents: Vec<_> = self
                .residents
                .parked
                .iter()
                .filter(|(id, session)| {
                    session.queue.is_empty()
                        && !session.turn_in_flight
                        && session.delivery_in_flight.is_none()
                        && session.background_task_count == 0
                        && !self.pending_native_rewinds.contains_key(id.as_str())
                })
                .filter_map(|(id, session)| session.idle_since.map(|idle| (id.clone(), idle)))
                .collect();
            if residents.len() > MAX_IDLE_RESIDENTS {
                residents.sort_unstable_by_key(|(_, idle)| *idle);
                residents.first().map(|(id, _)| id.clone())
            } else {
                None
            }
        };
        if let Some(oldest) = oldest {
            log::info!("evicting idle resident {oldest}");
            self.drop_background(&oldest, cx);
        }

        let session_id = session_id.to_string();
        let resident_idle_grace = self.resident_idle_grace;
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            smol::Timer::after(resident_idle_grace).await;
            host_cx.enqueue(move |state, cx| {
                let still_idle = state
                    .residents
                    .parked
                    .get(&session_id)
                    .is_some_and(|session| {
                        session.idle_since == Some(idle_since)
                            && session.queue.is_empty()
                            && !session.turn_in_flight
                            && session.delivery_in_flight.is_none()
                            && session.background_task_count == 0
                    })
                    && !state.pending_native_rewinds.contains_key(&session_id);
                if still_idle {
                    state.drop_background(&session_id, cx);
                }
            });
        });
    }

    /// Shut down and forget a parked session (archive/delete paths).
    pub(super) fn drop_background(&mut self, session_id: &str, cx: &mut HostCx) {
        self.clear_approvals(session_id);
        self.pending_native_rewinds.remove(session_id);
        if let Some(parked) = self.residents.evict(session_id)
            && let Runtime::Live(commands) = parked.runtime
        {
            let _ = commands.try_send(SessionCommand::Shutdown);
        }
        self.reschedule_scheduled_wake(cx);
    }

    pub(super) fn close_orchestrator_children(&mut self, parent_id: &str, cx: &mut HostCx) {
        let child_ids: Vec<_> = self
            .sessions
            .iter()
            .filter(|meta| meta.parent_session_id.as_deref() == Some(parent_id))
            .map(|meta| meta.id.clone())
            .collect();
        for child_id in child_ids {
            self.drop_background(&child_id, cx);
            self.revoke_preview_registration(&child_id);
        }
        self.revoke_preview_registration(parent_id);
        if let Some(registration) = self.mcp.orchestrate_registrations.remove(parent_id)
            && let Some(tokens) = &self.mcp.orchestrate_tokens
        {
            tokens.revoke(&registration.bearer_token);
        }
    }

    pub(super) fn revoke_preview_registration(&mut self, session_id: &str) {
        if let Some(registration) = self.mcp.preview_registrations.remove(session_id)
            && let Some(tokens) = &self.mcp.preview_tokens
        {
            tokens.revoke(&registration.bearer_token);
        }
    }

    pub(super) fn report_error(&mut self, error: RuntimeError, cx: &mut HostCx) {
        log::error!("{error:?}");
        emit_runtime(cx, RuntimeEvent::Error(error));
    }
}
