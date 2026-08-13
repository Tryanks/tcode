use super::*;

impl AppState {
    /// Build the complete provider read projection. This is the sole
    /// constructor for the replicated providers domain.
    pub fn providers_status_snapshot(&self) -> ProvidersStatus {
        ProvidersStatus {
            model_catalogs: self.model_catalogs.clone(),
            models_loading: self.models_loading.clone(),
            provider_versions: self
                .provider_versions
                .iter()
                .map(|(&provider, status)| {
                    (
                        provider,
                        ProtocolProviderVersionStatus {
                            installed: status.installed.clone(),
                            latest: status.latest.clone(),
                            update_available: status.update_available,
                            checking: status.checking,
                            updating: status.updating,
                            update_command: update_command_string(provider, status.install_source),
                        },
                    )
                })
                .collect(),
            provider_snapshots: self.provider_snapshots.clone(),
            acp_marketplace_items: self.acp_marketplace_items(),
            acp_registry_loading: self.acp_registry_loading,
            acp_registry_error: self.acp_registry_error.clone(),
            acp_installing: self.acp_installing.clone(),
            providers_checked_at: self.providers_checked_at(),
            providers_checking: self.providers_checking(),
            secret_names: self.provider_secret_names.clone(),
        }
    }

    pub(super) fn emit_providers_status(&mut self, cx: &mut HostCx) {
        self.emit_domain(
            Topic::Providers,
            ServerEvent::ProvidersReplaced(self.providers_status_snapshot()),
            cx,
        );
    }

    /// Build the complete active-workspace Git projection.
    pub fn git_status_snapshot(&self) -> GitStatusStatus {
        GitStatusStatus {
            status: self.git_status.clone(),
            busy: self.git_busy,
            generation: self.git_status_generation,
        }
    }

    /// Reconcile the deliberate local live-terminal handle registry after one
    /// host mailbox turn. Only opaque `Arc<Terminal>` values cross this path;
    /// layout and context data are emitted in `SessionStatus`.
    pub(crate) fn sync_terminal_handles(&self) {
        self.terminal_registry.replace_from(
            self.active
                .iter()
                .map(|session| &session.terminal_workspace)
                .chain(
                    self.background
                        .values()
                        .map(|session| &session.terminal_workspace),
                )
                .chain(self.terminal_workspaces.values()),
        );
    }

    /// Build the serialized snapshot associated with one subscription.
    pub(crate) fn subscription_snapshot(&self, topic: &Topic) -> Option<EventEnvelope> {
        let (seq, event) = match topic {
            Topic::Index => (
                self.index_event_seq,
                ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                    sessions: self.sessions.clone(),
                    projects: self.projects.clone(),
                }),
            ),
            Topic::Settings => (
                self.settings_event_seq,
                ServerEvent::SettingsSnapshot(self.settings.clone()),
            ),
            Topic::Providers => (
                self.providers_event_seq,
                ServerEvent::ProvidersReplaced(self.providers_status_snapshot()),
            ),
            Topic::GitStatus => (
                self.git_status_event_seq,
                ServerEvent::GitStatusReplaced(self.git_status_snapshot()),
            ),
            Topic::ActiveSession => (
                0,
                ServerEvent::ActiveSessionReplaced(
                    self.active_session_id()
                        .and_then(|id| self.session_status_snapshot(id)),
                ),
            ),
            Topic::SessionStatus { session_id } => (
                self.session_status_event_seqs
                    .get(session_id)
                    .copied()
                    .unwrap_or_default(),
                ServerEvent::SessionStatusReplaced(self.session_status_snapshot(session_id)?),
            ),
            Topic::SessionEvents { session_id } => (
                self.session_event_seqs
                    .get(session_id)
                    .copied()
                    .unwrap_or_default(),
                ServerEvent::SessionSnapshot(
                    self.store
                        .read_events(session_id)
                        .into_iter()
                        .map(|stored| SessionEventRecord {
                            ts: stored.ts,
                            event: stored.event,
                        })
                        .collect(),
                ),
            ),
            Topic::RuntimeEvents => (
                0,
                ServerEvent::RuntimeSnapshot(tcode_protocol::RuntimeSnapshot),
            ),
            // Raw terminal bytes never travel through JSON in the local
            // transport. The construction-time handle registry carries the
            // split term channels instead.
            Topic::Terminal { .. } => return None,
            _ => return None,
        };
        Some(EventEnvelope {
            topic: topic.clone(),
            seq,
            event,
        })
    }

    pub(super) fn emit_git_status(&mut self, cx: &mut HostCx) {
        self.emit_domain(
            Topic::GitStatus,
            ServerEvent::GitStatusReplaced(self.git_status_snapshot()),
            cx,
        );
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn emit_provider_and_git_replicas_for_test(&mut self, cx: &mut HostCx) {
        self.emit_providers_status(cx);
        self.emit_git_status(cx);
    }

    /// Build the complete non-event-stream status projection for one resident
    /// session. This is the sole constructor for the replicated status domain.
    pub fn session_status_snapshot(&self, session_id: &str) -> Option<SessionStatus> {
        let session = self.resident(session_id)?;
        let meta = &session.meta;
        let provider_option_descriptors = if meta.provider == ProviderKind::Acp {
            session.provider_options.clone()
        } else {
            meta.model
                .as_deref()
                .and_then(|model| {
                    self.models_for(meta.provider)
                        .iter()
                        .find(|spec| spec.id == model)
                })
                .map(|spec| spec.options.clone())
                .unwrap_or_default()
        };
        let relay_confirmation = session.pending_relay.as_ref().and_then(|pending| {
            has_meaningful_history(&session.timeline).then(|| {
                (
                    self.provider_label(pending.from_provider, pending.from_profile.as_deref()),
                    self.provider_label(meta.provider, meta.profile_id.as_deref()),
                )
            })
        });
        let pending_approval = if self
            .active
            .as_ref()
            .is_some_and(|active| active.meta.id == session_id)
        {
            !session.timeline.pending_approvals.is_empty()
        } else {
            self.sessions_awaiting_approval
                .get(session_id)
                .is_some_and(|requests| !requests.is_empty())
        };
        let terminal_preferences = self.terminal_preferences_for(session);
        Some(SessionStatus {
            session_id: session_id.to_string(),
            title: meta.title.clone(),
            cwd: meta.cwd.clone(),
            attachments_dir: self.attachments_dir_for(session_id),
            provider: meta.provider,
            requested_model: meta.model.clone(),
            requested_profile_id: meta.profile_id.clone(),
            acp_agent_id: meta.acp_agent_id.clone(),
            project_id: meta.project_id.clone(),
            approval_mode: meta.approval_mode,
            interaction_mode: meta.interaction_mode,
            queued_messages: session
                .queue
                .iter()
                .map(|message| QueuedMessageStatus {
                    id: message.id,
                    text: message.text.clone(),
                    fire_at_unix_secs: message.not_before.and_then(|time| {
                        time.duration_since(UNIX_EPOCH)
                            .ok()
                            .map(|duration| duration.as_secs())
                    }),
                })
                .collect(),
            review_comment_drafts: self
                .review_comment_drafts
                .get(session_id)
                .cloned()
                .unwrap_or_default(),
            terminals: session
                .terminal_workspace
                .terminals
                .iter()
                .map(|terminal| TerminalStatus { id: terminal.id })
                .collect(),
            active_terminal_id: session.terminal_workspace.active_id,
            terminal_splits: session
                .terminal_workspace
                .splits
                .iter()
                .map(|split| TerminalSplitStatus {
                    first: split.first,
                    second: split.second,
                    direction: split.direction,
                })
                .collect(),
            terminal_contexts: session
                .terminal_workspace
                .contexts
                .iter()
                .map(|context| TerminalContextStatus {
                    id: context.id,
                    terminal_label: context.terminal_label.clone(),
                    line_start: context.line_start,
                    line_end: context.line_end,
                    text: context.text.clone(),
                })
                .collect(),
            terminal_open: terminal_preferences.is_some_and(|preferences| preferences.open),
            terminal_height: terminal_preferences
                .map(|preferences| preferences.height.clamp(120., 600.))
                .unwrap_or(240.),
            delivery_in_flight: session.delivery_in_flight,
            turn_running: session.turn_in_flight,
            working: session.has_work(),
            pending_approval,
            pending_user_input: session.timeline.pending_user_input.is_some(),
            supports_steering: session.supports_steering(),
            provider_option_descriptors,
            provider_option_selections: meta.option_selections.clone(),
            provider_commands: session.provider_commands.clone(),
            git_branch: session.git_branch.clone(),
            branches: session.branches.clone(),
            draft: session.draft,
            draft_workspace: session.draft_workspace.clone(),
            worktree: meta.worktree.clone(),
            preparing_worktree: session.preparing_worktree,
            relay_confirmation,
            native_rewind_pending: self.pending_native_rewinds.contains_key(session_id),
            // The one-shot value is transferred to the client as a dedicated
            // serialized event; availability is client-replica state after
            // that point, not a host-side consuming read.
            native_rewind_prefill_available: false,
            model_pending_restart: session.model_changed_while_live(),
            options_pending_restart: session.options_changed_while_live(),
            approval_pending_restart: session.approval_mode_changed_while_live(),
            ultrathink_armed: session.pending_ultrathink,
        })
    }

    pub(super) fn emit_session_status(&mut self, session_id: &str, cx: &mut HostCx) {
        if let Some(status) = self.session_status_snapshot(session_id) {
            self.emit_domain(
                Topic::SessionStatus {
                    session_id: session_id.to_string(),
                },
                ServerEvent::SessionStatusReplaced(status),
                cx,
            );
        }
    }

    pub(super) fn emit_active_session_status(&mut self, cx: &mut HostCx) {
        if let Some(session_id) = self.active_session_id().map(str::to_owned) {
            self.emit_session_status(&session_id, cx);
        }
    }

    pub(super) fn upsert_session_in_memory(&mut self, meta: SessionMeta) {
        match self
            .sessions
            .iter_mut()
            .find(|existing| existing.id == meta.id)
        {
            Some(existing) => *existing = meta,
            None => self.sessions.push(meta),
        }
        self.sessions
            .sort_by_key(|meta| std::cmp::Reverse(meta.updated_at));
    }

    /// Enqueue a FIFO barrier used by the application quit hook. The returned
    /// receiver resolves only after every earlier store write has completed.
    pub fn store_write_barrier(&mut self, cx: &mut HostCx) -> smol::channel::Receiver<()> {
        let (completion, barrier) = smol::channel::bounded(1);
        self.enqueue_store_write(StoreWrite::Flush(completion), cx);
        barrier
    }
}
