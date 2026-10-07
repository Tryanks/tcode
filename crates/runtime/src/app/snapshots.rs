use super::*;

/// Last emitted values for every replace-style replica domain.
///
/// The host turn is the single seam that reconciles these projections. Timeline
/// appends and one-shot events deliberately remain on their existing paths.
pub(crate) struct DomainDiff {
    index: IndexSnapshot,
    settings: Settings,
    providers: ProvidersStatus,
    git_status: HashMap<String, GitStatusStatus>,
    session_statuses: HashMap<String, SessionStatus>,
    session_plans: HashMap<String, SessionPlan>,
}

impl DomainDiff {
    pub(crate) fn new(state: &AppState) -> Self {
        Self {
            index: state.index_snapshot(),
            settings: state.settings_snapshot(),
            providers: state.providers_status_snapshot(),
            git_status: HashMap::new(),
            session_statuses: state.resident_session_status_snapshots(),
            session_plans: state.resident_session_plan_snapshots(),
        }
    }

    pub(crate) fn emit_changes(&mut self, state: &AppState, cx: &mut HostCx) {
        let index = state.index_snapshot();
        if self.index != index {
            for event in index_changes(&self.index, &index) {
                emit_replacement(Topic::Index, event, cx);
            }
            self.index = index;
        }

        if self.settings != state.settings {
            let settings = state.settings_snapshot();
            emit_replacement(
                Topic::Settings,
                settings_change(&self.settings, &settings),
                cx,
            );
            self.settings = settings;
        }

        let providers = state.providers_status_snapshot();
        if self.providers != providers {
            self.providers = providers.clone();
            emit_replacement(
                Topic::Providers,
                ServerEvent::ProvidersReplaced(providers),
                cx,
            );
        }

        let git_status: HashMap<_, _> = state
            .residents
            .ids()
            .map(|id| (id.to_string(), state.git_status_snapshot(id)))
            .collect();
        for (id, status) in &git_status {
            if self.git_status.get(id) != Some(status) {
                emit_replacement(
                    Topic::GitStatus {
                        session_id: id.clone(),
                    },
                    ServerEvent::GitStatusReplaced(status.clone()),
                    cx,
                );
            }
        }
        self.git_status = git_status;

        let session_statuses = state.resident_session_status_snapshots();
        let mut changed_ids: Vec<_> = session_statuses
            .iter()
            .filter_map(|(id, status)| {
                (self.session_statuses.get(id) != Some(status)).then_some(id.as_str())
            })
            .collect();
        changed_ids.sort_unstable();
        for id in changed_ids {
            let status = session_statuses[id].clone();
            emit_replacement(
                Topic::SessionStatus {
                    session_id: id.to_string(),
                },
                ServerEvent::SessionStatusReplaced(Box::new(status)),
                cx,
            );
        }
        self.session_statuses = session_statuses;
        let session_plans = state.resident_session_plan_snapshots();
        let mut changed_ids: Vec<_> = session_plans
            .iter()
            .filter_map(|(id, plan)| {
                (self.session_plans.get(id) != Some(plan)).then_some(id.as_str())
            })
            .collect();
        changed_ids.sort_unstable();
        for id in changed_ids {
            emit_replacement(
                Topic::SessionPlan {
                    session_id: id.to_string(),
                },
                ServerEvent::SessionPlanReplaced(session_plans[id].clone()),
                cx,
            );
        }
        self.session_plans = session_plans;
    }
}

/// The events that turn a client's `old` index into `new`: the changed
/// threads and projects one by one, and the summary whole.
fn index_changes(old: &IndexSnapshot, new: &IndexSnapshot) -> Vec<ServerEvent> {
    let mut events = Vec::new();
    let old_projects: HashMap<_, _> = old.projects.iter().map(|p| (p.id.as_str(), p)).collect();
    let new_projects: HashSet<_> = new.projects.iter().map(|p| p.id.as_str()).collect();
    for project in &new.projects {
        if old_projects.get(project.id.as_str()) != Some(&project) {
            events.push(ServerEvent::IndexUpsertProject(project.clone()));
        }
    }
    let old_sessions: HashMap<_, _> = old.sessions.iter().map(|m| (m.id.as_str(), m)).collect();
    let new_sessions: HashSet<_> = new.sessions.iter().map(|m| m.id.as_str()).collect();
    for meta in &new.sessions {
        if old_sessions.get(meta.id.as_str()) != Some(&meta) {
            events.push(ServerEvent::IndexUpsertSession(meta.clone()));
        }
    }
    for meta in &old.sessions {
        if !new_sessions.contains(meta.id.as_str()) {
            events.push(ServerEvent::IndexRemoveSession {
                session_id: meta.id.clone(),
            });
        }
    }
    for project in &old.projects {
        if !new_projects.contains(project.id.as_str()) {
            events.push(ServerEvent::IndexRemoveProject {
                project_id: project.id.clone(),
            });
        }
    }
    if old.summary != new.summary {
        events.push(ServerEvent::IndexSummaryReplaced(new.summary.clone()));
    }
    events
}

/// Visiting a thread changes only its visit time, so that alone crosses the
/// wire rather than every setting.
fn settings_change(old: &Settings, new: &Settings) -> ServerEvent {
    let only_visits = Settings {
        last_visited: new.last_visited.clone(),
        ..old.clone()
    } == *new
        && old
            .last_visited
            .keys()
            .all(|id| new.last_visited.contains_key(id));
    if !only_visits {
        return ServerEvent::SettingsReplaced(new.clone());
    }
    ServerEvent::LastVisitedChanged(
        new.last_visited
            .iter()
            .filter(|(id, at)| old.last_visited.get(*id) != Some(at))
            .map(|(id, at)| (id.clone(), *at))
            .collect(),
    )
}

fn emit_replacement(topic: Topic, event: ServerEvent, cx: &mut HostCx) {
    cx.emit(HostEvent::Domain(EventEnvelope {
        request_id: None,
        topic,
        event,
    }));
}

impl AppState {
    pub fn index_snapshot(&self) -> IndexSnapshot {
        let mut summary = IndexSummary {
            title_generating: self.title_generating.clone(),
            archived_revision: self.archived_revision,
            ..IndexSummary::default()
        };
        let sharing = self.worktree_sharing();
        let mut sessions = Vec::new();
        for meta in &self.sessions {
            if meta.archived_at.is_none() {
                sessions.push(meta.clone());
                let resident = self.resident(&meta.id);
                summary.activity.insert(
                    meta.id.clone(),
                    self.session_activity(resident.map_or(meta, |session| &session.meta), resident),
                );
                if sharing.is_shared(meta) {
                    summary.worktree_shared.insert(meta.id.clone());
                }
                continue;
            }
            if let Some(project_id) = &meta.project_id {
                *summary
                    .archived_counts
                    .entry(project_id.clone())
                    .or_default() += 1;
            }
        }
        for id in self.residents.ids() {
            let Some(session) = self.resident(id) else {
                continue;
            };
            if session.meta.archived_at.is_none() && !summary.activity.contains_key(id) {
                summary.activity.insert(
                    id.to_string(),
                    self.session_activity(&session.meta, Some(session)),
                );
                if sharing.is_shared(&session.meta) {
                    summary.worktree_shared.insert(id.to_string());
                }
            }
        }
        IndexSnapshot {
            summary,
            sessions,
            projects: self.projects.clone(),
        }
    }

    pub fn archived_sessions(&self) -> ArchivedSessions {
        let sharing = self.worktree_sharing();
        let mut sessions: Vec<_> = self
            .sessions
            .iter()
            .filter(|meta| meta.archived_at.is_some())
            .cloned()
            .collect();
        sessions.sort_by_key(|meta| std::cmp::Reverse(meta.archived_at));
        let worktree_shared = sessions
            .iter()
            .filter(|meta| sharing.is_shared(meta))
            .map(|meta| meta.id.clone())
            .collect();
        ArchivedSessions {
            sessions,
            worktree_shared,
            revision: self.archived_revision,
        }
    }

    pub(super) fn archived_sharing_affected(&self, other: &SessionMeta) -> bool {
        self.sessions
            .iter()
            .any(|meta| meta.archived_at.is_some() && meta.shares_worktree_with(other))
    }

    pub(super) fn worktree_sharing(&self) -> WorktreeSharing<'_> {
        WorktreeSharing::new(
            self.sessions
                .iter()
                .chain(self.residents.live.values().map(|session| &session.meta)),
        )
    }

    pub fn settings_snapshot(&self) -> Settings {
        self.settings.clone()
    }

    /// Build the complete provider read projection. This is the sole
    /// constructor for the replicated providers domain.
    pub fn providers_status_snapshot(&self) -> ProvidersStatus {
        self.providers.status_snapshot(
            self.acp_marketplace_items(),
            self.acp_registry_loading,
            self.acp_registry_error.clone(),
            self.acp_installing.clone(),
            self.provider_plugin_catalogs(),
        )
    }

    /// Build the complete active-workspace Git projection.
    pub fn git_status_snapshot(&self, session_id: &str) -> GitStatusStatus {
        GitStatusStatus {
            status: self.git_status.get(session_id).cloned(),
            busy: self.git_busy.contains(session_id),
        }
    }

    /// Reconcile the deliberate local live-terminal handle registry after one
    /// host mailbox turn. Only opaque `Arc<Terminal>` values cross this path;
    /// layout and context data are emitted in `SessionStatus`.
    pub(crate) fn sync_terminal_handles(&self) {
        self.terminal_registry.replace_from(
            self.residents
                .live
                .values()
                .map(|session| &session.terminal_workspace)
                .chain(
                    self.residents
                        .parked
                        .values()
                        .map(|session| &session.terminal_workspace),
                )
                .chain(self.terminal_workspaces.values()),
        );
    }

    /// Build the serialized snapshot associated with one subscription.
    pub(crate) fn subscription_snapshot(
        &mut self,
        subscription: &tcode_protocol::Subscription,
    ) -> Option<EventEnvelope> {
        let topic = &subscription.topic;
        let event = match topic {
            Topic::Index => ServerEvent::IndexSnapshot(self.index_snapshot()),
            Topic::Settings => ServerEvent::SettingsSnapshot(self.settings_snapshot()),
            Topic::Providers => ServerEvent::ProvidersReplaced(self.providers_status_snapshot()),
            Topic::GitStatus { session_id } => {
                ServerEvent::GitStatusReplaced(self.git_status_snapshot(session_id))
            }
            Topic::SessionStatus { session_id } => ServerEvent::SessionStatusReplaced(Box::new(
                self.session_status_snapshot(session_id)?,
            )),
            Topic::SessionPlan { session_id } => {
                ServerEvent::SessionPlanReplaced(self.session_plan_snapshot(session_id)?)
            }
            // Answered from the session's log by `reply_to_subscription`.
            Topic::SessionEvents { .. } => return None,
            Topic::RuntimeEvents => return None,
            Topic::Preview { .. } => return None,
            // Retained latest-run status, so a client that subscribes after a
            // fast completion still recovers the outcome. `None` means no run
            // has ever started for this project.
            Topic::ExternalImport { project_id } => ServerEvent::ExternalImportStatusReplaced {
                project_id: project_id.clone(),
                status: self.external_imports.get(project_id).cloned(),
            },
            Topic::Terminal { terminal_id } => ServerEvent::TerminalFrame {
                terminal_id: *terminal_id,
                frame: Box::new(self.terminal_frame(*terminal_id)?),
            },
        };
        Some(EventEnvelope {
            request_id: None,
            topic: topic.clone(),
            event,
        })
    }

    /// The requests a session waits on. A provider shut down without a closing
    /// record leaves them open in the timeline; only an in-flight turn waits.
    fn open_requests<'a>(
        &'a self,
        session_id: &str,
        session: &'a ActiveSession,
    ) -> (
        &'a [agent::ApprovalRequest],
        Option<&'a tcode_core::session::PendingUserInput>,
    ) {
        if !session.turn_in_flight {
            return (&[], None);
        }
        (
            self.approval_requests(session_id),
            session.timeline.pending_user_input.as_ref(),
        )
    }

    pub(super) fn session_activity(
        &self,
        meta: &SessionMeta,
        resident: Option<&ActiveSession>,
    ) -> SessionActivity {
        let (approvals, input) = resident.map_or((&[][..], None), |session| {
            self.open_requests(&meta.id, session)
        });
        SessionActivity {
            working: resident.is_some_and(ActiveSession::has_work),
            turn_running: resident.is_some_and(|session| session.turn_in_flight),
            background_only: resident.is_some_and(|session| {
                session.background_task_count > 0
                    && !session.turn_in_flight
                    && session.delivery_in_flight.is_none()
                    && session.queue.is_empty()
            }),
            waiting_for_approval: !approvals.is_empty(),
            waiting_for_input: input.is_some(),
            unread: self.session_unread(meta),
            fork: Self::session_fork_availability(meta, resident),
        }
    }

    pub fn session_plan_snapshot(&self, session_id: &str) -> Option<SessionPlan> {
        let timeline = &self.resident(session_id)?.timeline;
        Some(SessionPlan {
            session_id: session_id.to_string(),
            steps: timeline.plan_steps.clone(),
        })
    }

    fn resident_session_plan_snapshots(&self) -> HashMap<String, SessionPlan> {
        self.residents
            .ids()
            .filter_map(|id| Some((id.to_string(), self.session_plan_snapshot(id)?)))
            .collect()
    }

    pub fn session_status_snapshot(&self, session_id: &str) -> Option<SessionStatus> {
        let session = self.resident(session_id)?;
        let meta = &session.meta;
        let mut provider_option_descriptors = if matches!(
            meta.provider.caps().option_descriptors,
            OptionDescriptors::Wire
        ) {
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
        if let Some(descriptor) = permission_control(meta.provider) {
            provider_option_descriptors.push(descriptor);
        }
        for descriptor in &session.provider_options {
            let id = match descriptor {
                OptionDescriptor::Select { id, .. } | OptionDescriptor::Boolean { id, .. } => id,
            };
            provider_option_descriptors.retain(|existing| match existing {
                OptionDescriptor::Select {
                    id: existing_id, ..
                }
                | OptionDescriptor::Boolean {
                    id: existing_id, ..
                } => existing_id != id,
            });
            provider_option_descriptors.push(descriptor.clone());
        }
        let mut provider_option_selections = meta.option_selections.clone();
        if !matches!(session.runtime, Runtime::Idle)
            && let Some(descriptor) = session.permission_descriptor()
        {
            let id = match descriptor {
                OptionDescriptor::Select { id, .. } | OptionDescriptor::Boolean { id, .. } => id,
            };
            provider_option_selections.retain(|selection| selection.id != id);
            provider_option_selections.extend(
                session
                    .confirmed_option_selections
                    .iter()
                    .filter(|selection| selection.id == id)
                    .cloned(),
            );
        }
        let relay_confirmation = session.pending_relay.as_ref().and_then(|pending| {
            has_meaningful_history(&session.timeline).then(|| {
                (
                    self.provider_label(pending.from_provider, pending.from_profile.as_deref()),
                    self.provider_label(meta.provider, meta.profile_id.as_deref()),
                )
            })
        });
        let terminal_preferences = self.terminal_preferences_for(session);
        let (approvals, user_input) = self.open_requests(session_id, session);
        let context_window = session
            .timeline
            .usage
            .and_then(|usage| usage.context_window)
            .map(|reported| {
                if meta.provider == ProviderKind::ClaudeCode
                    && let Some(model) = meta.model.as_deref()
                {
                    reported.min(agent::claude::resolved_context_window(
                        model,
                        &meta.option_selections,
                    ))
                } else {
                    reported
                }
            });
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
            queued_messages: session
                .queue
                .iter()
                .map(|message| QueuedMessageStatus {
                    delivery_key: message.delivery_key.clone(),
                    id: message.id,
                    editable: Self::queued_message_editable(session, message.id),
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
                .map(|terminal| TerminalStatus {
                    id: terminal.id,
                    title: terminal.terminal.label(),
                    exited: terminal.terminal.exited(),
                })
                .collect(),
            active_terminal_id: session.terminal_workspace.active_id,
            terminal_splits: session.terminal_workspace.splits.clone(),
            terminal_contexts: session.terminal_workspace.contexts.clone(),
            terminal_open: terminal_preferences.is_some_and(|preferences| preferences.open),
            terminal_height: terminal_preferences
                .map(|preferences| preferences.height.clamp(120., 600.))
                .unwrap_or(240.),
            delivery_in_flight: session.delivery_in_flight,
            activity: self.session_activity(meta, Some(session)),
            stopping: session.interrupt_requested,
            native_rewind_blocked: self.native_rewind_blocked(session),
            checkout_blocked: Self::checkout_blocked(session),
            conversation_read_only: Self::conversation_read_only(meta),
            terminal_limit_reached: self.terminal_limit_reached(session),
            terminal_split_available: self.terminal_split_available(session),
            usage: session.timeline.usage,
            context_window,
            // A provider shut down without a closing record leaves its turn
            // running in the timeline; only an in-flight turn is live.
            running_turn: session
                .turn_in_flight
                .then(|| session.timeline.running_turn())
                .flatten(),
            pending_approvals: approvals.to_vec(),
            pending_user_input: user_input.cloned(),
            steering_supported: session.can_steer(),
            provider_option_descriptors,
            provider_option_selections,
            provider_option_requested_selections: meta.option_selections.clone(),
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
            ultrathink_armed: session.pending_ultrathink,
        })
    }

    fn resident_session_status_snapshots(&self) -> HashMap<String, SessionStatus> {
        let mut statuses = HashMap::new();
        for id in self.residents.ids() {
            if let Some(status) = self.session_status_snapshot(id) {
                statuses.insert(id.to_string(), status);
            }
        }
        statuses
    }

    pub(super) fn upsert_session_in_memory(&mut self, meta: SessionMeta) {
        let existing = self.sessions.iter().find(|existing| existing.id == meta.id);
        let archived_metadata_changed = existing != Some(&meta)
            && (meta.archived_at.is_some()
                || existing.is_some_and(|old| old.archived_at.is_some()));
        let sharing_may_change = existing
            .is_none_or(|old| old.cwd != meta.cwd || old.worktree != meta.worktree)
            && (self.archived_sharing_affected(&meta)
                || existing.is_some_and(|old| self.archived_sharing_affected(old)));
        if archived_metadata_changed || sharing_may_change {
            self.archived_revision += 1;
        }
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
    /// receiver resolves only after every earlier store write has committed,
    /// with the store failure that lost one if any did.
    pub fn store_write_barrier(
        &mut self,
        cx: &mut HostCx,
    ) -> smol::channel::Receiver<Result<(), String>> {
        let (completion, barrier) = smol::channel::bounded(1);
        self.enqueue_store_write(StoreWrite::Flush(completion), cx);
        barrier
    }
}
