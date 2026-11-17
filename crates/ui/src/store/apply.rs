use super::*;

impl WorkspaceStore {
    /// Apply one envelope from the host and tell whoever it concerns.
    pub(super) fn receive(&mut self, envelope: EventEnvelope, cx: &mut Context<Self>) {
        let event = match envelope.event {
            // Presented by whoever subscribes; nothing the store holds changes.
            ServerEvent::Runtime(event) => return cx.emit(event),
            event => event,
        };
        let change = StoreChange {
            topic: TopicKind::from(&envelope.topic),
        };
        match self.apply_domain_event(EventEnvelope { event, ..envelope }, cx) {
            Reach::Store => cx.notify(),
            Reach::Thread(session_id, parts) => {
                if let Some(thread) = self.threads.get(&session_id) {
                    for part in parts {
                        thread.bump(*part, cx);
                    }
                }
            }
            // Its grid and label revisions are moved on as it applies.
            Reach::Terminal(terminal_id) => cx.emit(TerminalChange { terminal_id }),
            Reach::Preview(session_ids) => {
                for session_id in session_ids {
                    self.bump_preview(&session_id, cx);
                }
            }
        }
        cx.emit(change);
    }

    /// Apply `envelope` to the replicas: what it reached.
    pub(super) fn apply_domain_event(
        &mut self,
        envelope: EventEnvelope,
        cx: &mut Context<Self>,
    ) -> Reach {
        if !self.host.subscription_reply_is_current(&envelope) {
            return Reach::Store;
        }
        if matches!(
            TopicKind::from(&envelope.topic),
            TopicKind::Index | TopicKind::Settings
        ) {
            self.thread_list_revision += 1;
        }
        if envelope.topic == Topic::Scope {
            if let ServerEvent::ScopeSnapshot { host_unix_ms, .. } = &envelope.event {
                self.host_clock_offset_ms =
                    *host_unix_ms as i64 - crate::time::now_millis(cx) as i64;
            }
            if let ServerEvent::ScopeSnapshot { scope, .. } = &envelope.event
                && envelope.request_id.is_some()
                && envelope.request_id != self.applied_scope_request
                && !matches!(
                    self.host.connection_state(),
                    ConnectionState::Reconnecting { .. } | ConnectionState::Offline { .. }
                )
            {
                self.baseline_topics.clear();
                self.apply_scope(scope, cx);
                self.applied_scope_request = envelope.request_id;
            }
            return Reach::Store;
        }
        if matches!(envelope.topic, Topic::Index | Topic::SpaceIndex { .. })
            && envelope.topic != self.index_topic()
        {
            return Reach::Store;
        }
        if !self.scope.is_full()
            && matches!(
                envelope.topic,
                Topic::Settings
                    | Topic::Providers
                    | Topic::RuntimeEvents
                    | Topic::Preview { .. }
                    | Topic::PreviewSession { .. }
                    | Topic::ExternalImport { .. }
                    | Topic::Hosting
                    | Topic::StorageMigration
                    | Topic::DiskUsage
            )
        {
            return Reach::Store;
        }
        if matches!(envelope.topic, Topic::DevServers { .. })
            && !self.space_allows(|policy| policy.preview)
        {
            return Reach::Store;
        }
        let index_topic = self.index_topic();
        let index = Topic::Index;
        let topic = if envelope.topic == index_topic {
            &index
        } else {
            &envelope.topic
        };
        match (topic, &envelope.event) {
            (Topic::SessionStatus { session_id }, ServerEvent::SessionStatusReplaced(status)) => {
                for message in &status.queued_messages {
                    if let Some(key) = &message.delivery_key {
                        self.retire_delivery_for(session_id, key);
                    }
                }
            }
            (Topic::SessionEvents { session_id }, ServerEvent::SessionEvent(record)) => {
                self.retire_record_delivery(session_id, record);
            }
            (Topic::SessionEvents { session_id }, ServerEvent::SessionSnapshot { records, .. }) => {
                for record in records {
                    self.retire_record_delivery(session_id, record);
                }
            }
            _ => {}
        }
        let index_removal = match &envelope.event {
            ServerEvent::IndexRemoveSession { session_id } if envelope.topic == index_topic => {
                Some(session_id.clone())
            }
            _ => None,
        };
        let EventEnvelope {
            request_id,
            topic: envelope_topic,
            event,
        } = envelope;
        let topic = if envelope_topic == index_topic {
            &index
        } else {
            &envelope_topic
        };
        let mut reach = Reach::Store;
        // What the replicas keep is moved into them rather than copied.
        let event = match (topic, event) {
            (
                Topic::SessionEvents { session_id },
                ServerEvent::SessionSnapshot {
                    from,
                    end,
                    records,
                    total,
                    total_turns,
                    ..
                },
            ) => {
                let window = SnapshotWindow {
                    from,
                    end,
                    total,
                    total_turns: total_turns as usize,
                };
                if !self.apply_session_snapshot(
                    session_id,
                    &envelope_topic,
                    request_id,
                    window,
                    records,
                ) {
                    return Reach::Store;
                }
                None
            }
            (Topic::SessionEvents { session_id }, ServerEvent::SessionEvent(record)) => {
                // A turn's start and end change what reaches past the thread
                // (its conversation state, its recovery card); a record in
                // between changes the thread's log alone.
                let within_turn = !matches!(
                    record.event,
                    agent::AgentEvent::TurnStarted { .. }
                        | agent::AgentEvent::TurnCompleted { .. }
                        | agent::AgentEvent::RewindCompleted { .. }
                        | agent::AgentEvent::PlanUpdated { .. }
                );
                let Some(items) = self.apply_live_record(session_id, &envelope_topic, record)
                else {
                    return Reach::Store;
                };
                if within_turn {
                    reach = Reach::Thread(
                        session_id.clone(),
                        if items {
                            &[Part::Timeline, Part::Entries]
                        } else {
                            &[Part::Timeline]
                        },
                    );
                }
                None
            }
            (Topic::Terminal { terminal_id }, ServerEvent::TerminalFrame { frame, .. }) => {
                self.apply_terminal_frame(*terminal_id, *frame, cx);
                reach = Reach::Terminal(*terminal_id);
                None
            }
            (
                Topic::CommandOutput {
                    session_id,
                    item_id,
                    cols,
                },
                ServerEvent::CommandOutputReplaced(frame),
            ) => {
                self.apply_command_output(session_id, item_id, *cols, *frame);
                reach = Reach::Thread(session_id.clone(), &[Part::Outputs]);
                None
            }
            (_, event) => Some(event),
        };
        if let Some(event) = &event
            && let Some(changed) = self.apply_preview_event(topic, event, cx)
        {
            reach = Reach::Preview(changed);
        }
        if let Some(mut event) = event {
            match (topic, &mut event) {
                (
                    Topic::Preview { .. },
                    ServerEvent::PreviewRequest { .. }
                    | ServerEvent::PreviewRequestCancelled { .. },
                ) => {
                    let _ = self.remote_preview.0.try_send(EventEnvelope {
                        request_id,
                        topic: envelope_topic.clone(),
                        event,
                    });
                }
                (Topic::Terminal { terminal_id }, ServerEvent::TerminalDelta { delta, .. }) => {
                    self.apply_terminal_delta(*terminal_id, delta, cx);
                    reach = Reach::Terminal(*terminal_id);
                }
                (
                    Topic::CommandOutput {
                        session_id,
                        item_id,
                        cols,
                    },
                    ServerEvent::CommandOutputDelta(delta),
                ) => {
                    self.apply_command_output_delta(session_id, item_id, *cols, delta);
                    reach = Reach::Thread(session_id.clone(), &[Part::Outputs]);
                }
                (Topic::Index, ServerEvent::IndexUpsertSession(meta)) => {
                    title_draft(meta);
                    if let Some(archived) = &mut self.archived_replica {
                        archived.sessions.retain(|archived| archived.id != meta.id);
                    }
                    match self
                        .index_replica
                        .0
                        .iter_mut()
                        .find(|existing| existing.id == meta.id)
                    {
                        Some(existing) => *existing = (**meta).clone(),
                        None => self.index_replica.0.push((**meta).clone()),
                    }
                    self.index_replica
                        .0
                        .sort_by_key(|meta| std::cmp::Reverse(meta.updated_at));
                    self.reindex_sessions();
                    self.close_unshown_pull_requests();
                }
                (Topic::Index, ServerEvent::IndexUpsertProject(project)) => {
                    images::invalidate_project_icon(
                        self.image_host,
                        project,
                        images::IconReset::Changed,
                        cx,
                    );
                    match self
                        .index_replica
                        .1
                        .iter_mut()
                        .find(|existing| existing.id == project.id)
                    {
                        Some(existing) => *existing = project.clone(),
                        None => self.index_replica.1.push(project.clone()),
                    }
                }
                (Topic::Index, ServerEvent::IndexRemoveSession { session_id }) => {
                    if let Some(position) = self
                        .index_replica
                        .0
                        .iter()
                        .position(|meta| meta.id == *session_id)
                    {
                        self.removed_session = Some(self.index_replica.0.remove(position));
                        self.reindex_sessions();
                    }
                    self.native_rewind_prefills.remove(session_id);
                    self.conversation_ui
                        .remove(&ConversationDestination::Thread(session_id.clone()));
                    self.open_pull_requests.remove(session_id);
                }
                (Topic::Index, ServerEvent::IndexRemoveProject { project_id }) => {
                    self.index_replica
                        .1
                        .retain(|project| project.id != *project_id);
                    self.import_statuses.remove(project_id);
                    self.conversation_ui
                        .remove(&ConversationDestination::ProjectDraft(project_id.clone()));
                }
                (
                    Topic::ExternalImport { project_id },
                    ServerEvent::ExternalImportStatusReplaced { status, .. },
                ) => {
                    self.import_statuses
                        .insert(project_id.clone(), status.clone());
                }
                (Topic::Index, ServerEvent::IndexSnapshot(snapshot)) => {
                    self.index_hydrated = true;
                    let fresh_baseline = self.baseline_topics.insert(index_topic.clone());
                    for project in &snapshot.projects {
                        let changed =
                            self.index_replica.1.iter().any(|old| {
                                old.id == project.id && old.icon_path != project.icon_path
                            });
                        if changed {
                            images::invalidate_project_icon(
                                self.image_host,
                                project,
                                images::IconReset::Changed,
                                cx,
                            );
                        } else if fresh_baseline {
                            images::invalidate_project_icon(
                                self.image_host,
                                project,
                                images::IconReset::Baseline,
                                cx,
                            );
                        }
                    }
                    snapshot.sessions.iter_mut().for_each(title_draft);
                    self.index_replica = (
                        std::mem::take(&mut snapshot.sessions),
                        std::mem::take(&mut snapshot.projects),
                    );
                    self.reindex_sessions();
                    self.close_unshown_pull_requests();
                    // Client state for a conversation the index no longer lists has
                    // nothing left to return to: a deleted project takes its draft's
                    // state, a deleted or archived thread its own.
                    let (sessions, projects) = &self.index_replica;
                    self.conversation_ui
                        .retain(|destination, _| match destination {
                            ConversationDestination::ProjectDraft(project_id) => {
                                projects.iter().any(|project| project.id == *project_id)
                            }
                            ConversationDestination::Thread(session_id) => {
                                sessions.iter().any(|meta| meta.id == *session_id)
                            }
                        });
                    self.threads.retain(|session_id, thread| {
                        thread.open || sessions.iter().any(|meta| meta.id == *session_id)
                    });
                    self.restore_index_selection(cx);
                    self.apply_index_summary(&snapshot.summary, cx);
                    if self.archived_holds > 0 {
                        self.load_archived_sessions(cx);
                    }
                }
                (
                    Topic::Index,
                    ServerEvent::IndexUnchanged {
                        fingerprint,
                        archived_revision,
                    },
                ) => {
                    if self.cached_index_fingerprint().as_ref() == Some(fingerprint) {
                        self.index_summary.archived_revision = *archived_revision;
                        if self.baseline_topics.insert(index_topic.clone()) {
                            for project in &self.index_replica.1 {
                                images::invalidate_project_icon(
                                    self.image_host,
                                    project,
                                    images::IconReset::Baseline,
                                    cx,
                                );
                            }
                        }
                        self.restore_index_selection(cx);
                        if self.archived_holds > 0 {
                            self.load_archived_sessions(cx);
                        }
                    } else {
                        let _ = self.host.subscribe(Subscription {
                            topic: index_topic.clone(),
                            after: None,
                            known_index: None,
                        });
                    }
                }
                (Topic::Index, ServerEvent::IndexSummaryReplaced(summary)) => {
                    self.apply_index_summary(summary, cx);
                }
                (Topic::Index, ServerEvent::IndexActivitiesChanged { upserts, removed }) => {
                    if self.baseline_topics.contains(&index_topic) {
                        for id in removed {
                            self.index_summary.activity.remove(id);
                        }
                        self.index_summary.activity.extend(
                            upserts
                                .iter()
                                .map(|(id, activity)| (id.clone(), activity.clone())),
                        );
                    }
                }
                (Topic::Settings, ServerEvent::LastVisitedChanged(visits)) => {
                    self.settings_replica
                        .last_visited
                        .extend(visits.iter().map(|(id, at)| (id.clone(), *at)));
                }
                (Topic::Settings, ServerEvent::SettingsReplaced(settings)) => {
                    self.settings_replica = std::mem::take(settings);
                    self.settings_hydrated = true;
                }
                (Topic::Settings, ServerEvent::SettingsSnapshot(settings)) => {
                    self.settings_replica = std::mem::take(settings);
                    self.settings_hydrated = true;
                    self.baseline_topics.insert(Topic::Settings);
                }
                (Topic::Providers, ServerEvent::ProvidersReplaced(status)) => {
                    self.providers_replica = std::mem::take(status);
                    self.baseline_topics.insert(Topic::Providers);
                }
                (Topic::Hosting, ServerEvent::HostingReplaced(state)) => cx.emit((**state).clone()),
                (Topic::StorageMigration, ServerEvent::StorageMigrationReplaced(migration))
                    if self.storage_migration_holds > 0 =>
                {
                    self.storage_migration.clone_from(migration);
                }
                (Topic::DiskUsage, ServerEvent::DiskUsageReplaced(usage))
                    if self.disk_usage_holds > 0 =>
                {
                    self.disk_usage.clone_from(usage);
                }
                (
                    Topic::Providers,
                    ServerEvent::ProviderUsageChanged {
                        provider_usage,
                        usage_checking,
                    },
                ) if self.baseline_topics.contains(&envelope_topic) => {
                    self.providers_replica
                        .provider_usage
                        .clone_from(provider_usage);
                    self.providers_replica
                        .usage_checking
                        .clone_from(usage_checking);
                }
                (Topic::GitStatus { session_id }, ServerEvent::GitStatusReplaced(status)) => {
                    if let Some(thread) = self.threads.get_mut(session_id) {
                        thread.git = Some(std::mem::take(status));
                        reach = Reach::Thread(session_id.clone(), &[Part::Status]);
                    }
                }
                (
                    Topic::SessionStatus { session_id },
                    ServerEvent::SessionStatusReplaced(status),
                ) => {
                    let loaded = self.baseline_topics.insert(envelope_topic.clone());
                    if let Some(thread) = self.threads.get_mut(session_id) {
                        let status = status.as_ref().clone();
                        let drawer = (status.terminal_open, status.terminal_height);
                        thread.set_status(status, cx.background_executor().now());
                        if thread.open {
                            thread.resettle();
                            let destination = thread.destination.clone();
                            let conversations = self.conversation_ui.len();
                            self.sync_terminal_topics(cx);
                            self.sync_conversation_ui(session_id);
                            // A status that moves the thread's conversation
                            // state, or first brings it, reaches past the
                            // thread.
                            if !loaded
                                && self.conversation_ui.len() == conversations
                                && self.threads[session_id].destination == destination
                            {
                                reach = Reach::Thread(
                                    session_id.clone(),
                                    &[Part::Status, Part::Timeline, Part::Entries],
                                );
                            }
                            // The host owns the drawer, which lives only with its
                            // running terminals: a conversation that loads shows it
                            // as the host has it, not as this client last left it.
                            if loaded
                                && let Some(ui) = self.thread_conversation_ui_mut(Some(session_id))
                            {
                                (ui.terminal_open, ui.terminal_height) = drawer;
                            }
                        }
                    }
                }
                (
                    Topic::SessionStatus { session_id },
                    ServerEvent::SessionUsageChanged {
                        usage,
                        context_window,
                    },
                ) if self.baseline_topics.contains(&envelope_topic) => {
                    if let Some(status) = self
                        .threads
                        .get_mut(session_id)
                        .and_then(|thread| thread.status.as_mut())
                    {
                        status.usage = *usage;
                        status.context_window = *context_window;
                        reach = Reach::Thread(session_id.clone(), &[Part::Status]);
                    }
                }
                (Topic::SessionPlan { session_id }, ServerEvent::SessionPlanReplaced(plan)) => {
                    let loaded = self.baseline_topics.insert(envelope_topic.clone());
                    if let Some(thread) = self.threads.get_mut(session_id) {
                        thread.plan = Some(plan.clone());
                        if !loaded {
                            reach = Reach::Thread(session_id.clone(), &[Part::Status]);
                        }
                    }
                }
                (Topic::SessionEvents { session_id }, ServerEvent::SessionHistoryError(error)) => {
                    if let Some(thread) = self.open_thread_mut(session_id) {
                        thread.history_error = Some(error.message.clone());
                        thread.catching_up = false;
                    }
                }
                (
                    Topic::SessionStatus { session_id },
                    ServerEvent::RewindPrefill {
                        client_id,
                        text,
                        attachments,
                        ..
                    },
                ) if *client_id == self.client_id() => {
                    self.native_rewind_prefills.insert(
                        session_id.clone(),
                        RewindPrefill {
                            text: text.clone(),
                            attachments: attachments.clone(),
                        },
                    );
                }
                _ => {}
            }
        }
        self.load_pending_chat_history(cx);
        if envelope_topic == index_topic {
            self.host
                .remember_known_index(&index_topic, self.cached_index_fingerprint());
            self.reconcile_destination(cx);
            self.removed_session = None;
            // A thread kept on screen (its failed send still offers Retry)
            // keeps its replicas until the user leaves it. So does a draft:
            // one holding nothing leaves the index as it leaves the screen,
            // and is still the page its project's next new thread opens on.
            if let Some(session_id) = index_removal
                && !self.threads.get(&session_id).is_some_and(|thread| {
                    thread.open || thread.status.as_ref().is_some_and(|status| status.draft)
                })
            {
                self.threads.remove(&session_id);
            }
        }
        self.acknowledge_read(cx);
        reach
    }

    /// A window of the thread's log, answering its subscription or a page:
    /// whether anything after it in the envelope still applies.
    pub(super) fn apply_session_snapshot(
        &mut self,
        session_id: &str,
        topic: &Topic,
        request_id: Option<u64>,
        window: SnapshotWindow,
        records: Vec<StoredEvent>,
    ) -> bool {
        let SnapshotWindow {
            from,
            end,
            total,
            total_turns,
        } = window;
        if !self
            .threads
            .get(session_id)
            .is_some_and(|thread| thread.open)
        {
            return false;
        }
        let after = match request_id {
            Some(_) => match self.host.consume_session_snapshot(topic, request_id) {
                Some(after) => after,
                None => return false,
            },
            None => None,
        };
        let continuation = after.filter(|after| *after <= total);
        let thread = self.threads.get_mut(session_id).expect("open thread");
        let retain_timeline = continuation == Some(from)
            && records.is_empty()
            && thread.total_turns.is_some()
            && from != 0
            && thread.history.as_ref().is_some_and(|held| held.end == from)
            && thread.timeline.is_some();
        thread.total_turns = None;
        let held = &mut thread.history;
        match held {
            // Continues the held cursor.
            Some(held) if continuation == Some(from) && held.end == from => {
                held.extend(records, end);
            }
            _ => *held = Some(history::HeldHistory::new(from, end, records)),
        }
        // The window reaches the end of the log: the replica is at the
        // host's position, and live records continue it.
        thread.catching_up = false;
        self.host.remember_after(topic, end);
        thread.total_turns = Some(total_turns);
        self.baseline_topics.insert(topic.clone());
        if retain_timeline {
            let timeline = thread.timeline.as_ref().expect("retained timeline");
            thread.turn_offset = total_turns.saturating_sub(timeline.turns.len());
            return false;
        }
        thread.refold(total_turns);
        true
    }

    /// One record streamed live into an open thread: whether it applied.
    /// Apply a record streamed into an open thread's log: `None` when it was
    /// not, else whether it changed the fold's items ([`Part::Entries`]).
    pub(super) fn apply_live_record(
        &mut self,
        session_id: &str,
        topic: &Topic,
        mut record: StoredEvent,
    ) -> Option<bool> {
        let baseline = self.baseline_topics.contains(topic);
        let thread = self.open_thread_mut(session_id)?;
        // Until its window arrives the thread may hold an earlier
        // visit's records, which the window continues: a record sent
        // before the window is part of it.
        if thread.catching_up || !baseline || thread.timeline.is_none() {
            return None;
        }
        let held = thread.history.as_ref()?;
        // A record withheld live does not say its row: it is the one that
        // takes the held log's end.
        if let agent::AgentEvent::RecordWithheld {
            row: row @ None, ..
        } = &mut record.event
        {
            *row = Some(held.end);
        }
        let after = held.end + 1;
        self.host.remember_after(topic, after);
        self.apply_conversation_event(session_id, &record.event);
        let thread = self.threads.get_mut(session_id).expect("open thread");
        // Text streamed into an item it continues changes no item; a delta
        // that brings its item first does.
        let mut items = !matches!(record.event, agent::AgentEvent::Delta { .. });
        if let Some(mut timeline) = thread.timeline.take() {
            let entries = timeline.entries.len();
            timeline.apply_stored(&record);
            items |= timeline.entries.len() != entries;
            thread.settle_running_turn(&mut timeline);
            thread.total_turns = Some(thread.turn_offset + timeline.turns.len());
            thread.timeline = Some(timeline);
        }
        thread
            .history
            .as_mut()
            .expect("held above")
            .extend(vec![record], after);
        Some(items)
    }
}

/// A draft has no title of its own: the lists name it by the first line of
/// what its composer holds, or as the new thread it is while that is nothing.
fn title_draft(meta: &mut SessionMeta) {
    const SHOWN: usize = 120;
    if let Some(text) = &meta.draft_text {
        meta.title = match text.lines().map(str::trim).find(|line| !line.is_empty()) {
            Some(line) => line.chars().take(SHOWN).collect(),
            None => crate::tr!("chat.new_thread").into_owned(),
        };
    }
}
