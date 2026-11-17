use super::*;

impl WorkspaceStore {
    pub(crate) fn message_byline_for(
        &self,
        session_id: Option<&str>,
        entry_id: &str,
        cx: &App,
    ) -> Option<String> {
        let held = self
            .watched(session_id, Part::Timeline, cx)?
            .history
            .as_ref()?;
        let attribution = &held.attribution;
        // The lead's messages to a child carry no author: they are no member's.
        if attribution.origins.get(entry_id) == Some(&tcode_core::session::MessageOrigin::Agent) {
            return Some(crate::tr!("chat.from_orchestrator").into_owned());
        }
        let author = attribution.authors.get(entry_id)?;
        // A device whose id cannot be read reaches no other machine, so no
        // message there is its own.
        let own_id = self
            .client_host
            .as_ref()
            .and_then(|host| host.device_id().ok());
        // The host attributes only space members' messages. An unattributed
        // one is the owner's, which is someone else only from inside a space.
        let other = author.as_ref().map_or(!self.scope.is_full(), |author| {
            own_id.as_deref() != Some(author.device_id.as_str())
        });
        (attribution.distinct_authors.len() > 1 || other).then(|| {
            author
                .as_ref()
                .map(|author| author.name.clone())
                .unwrap_or_else(|| crate::tr!("member.owner").into_owned())
        })
    }

    pub fn with_timeline_for<R>(
        &self,
        session_id: Option<&str>,
        read: impl FnOnce(&Timeline) -> R,
        cx: &App,
    ) -> Option<R> {
        self.thread_timeline(session_id, cx).map(read)
    }

    /// Reads `session_id`'s timeline as a view depending on its items alone
    /// ([`Part::Entries`]): text streamed into an item does not draw such a
    /// view again, so `read` must not look at a message's, a thought's or a
    /// command output's text.
    pub fn with_entries_for<R>(
        &self,
        session_id: Option<&str>,
        read: impl FnOnce(&Timeline) -> R,
        cx: &App,
    ) -> Option<R> {
        self.watched(session_id, Part::Entries, cx)?
            .timeline
            .as_ref()
            .map(read)
    }

    /// Moves on with every change to `session_id`'s timeline items, read as
    /// a view depending on them: see [`Self::with_entries_for`].
    pub(crate) fn entries_revision_for(&self, session_id: Option<&str>, cx: &App) -> Option<u64> {
        Some(self.thread(session_id)?.watch(Part::Entries, cx))
    }

    /// Moves on with every change to `session_id`'s timeline, read as a view
    /// depending on it: the store is not notified of a record streamed in.
    pub(crate) fn timeline_revision_for(&self, session_id: Option<&str>, cx: &App) -> Option<u64> {
        Some(self.thread(session_id)?.watch(Part::Timeline, cx))
    }

    /// Changes each time `session_id`'s timeline is folded whole from the
    /// records held for it (opening it, catching up, a history page, pages
    /// dropped above the tail) instead of extended by a live record.
    pub(crate) fn timeline_folds_for(&self, session_id: Option<&str>) -> u64 {
        self.thread(session_id).map_or(0, |thread| thread.folds)
    }

    pub(crate) fn pending_chat_turn(&self, session_id: &str) -> Option<usize> {
        let turn_offset = self.threads.get(session_id)?.turn_offset;
        self.pending_chat_turn
            .as_ref()
            .filter(|(id, _)| id == session_id)
            .and_then(|(_, turn)| turn.checked_sub(turn_offset))
    }

    pub(crate) fn take_pending_chat_turn(&mut self, session_id: &str, turn: usize) {
        let turn_offset = self
            .threads
            .get(session_id)
            .map_or(0, |thread| thread.turn_offset);
        if self.pending_chat_turn.as_ref() == Some(&(session_id.to_string(), turn + turn_offset)) {
            self.pending_chat_turn = None;
        }
    }

    pub fn with_composer_destination_for<R>(
        &self,
        session_id: Option<&str>,
        read: impl FnOnce(bool, &str, Option<&str>) -> R,
        cx: &App,
    ) -> Option<R> {
        self.thread_status(session_id, cx).map(|status| {
            read(
                status.draft,
                &status.session_id,
                status.project_id.as_deref(),
            )
        })
    }

    pub(crate) fn native_subagent_readonly_for(&self, session_id: Option<&str>, cx: &App) -> bool {
        self.thread_status(session_id, cx)
            .is_some_and(|status| status.conversation_read_only)
    }

    /// The composer of `session_id`'s thread, or of no thread.
    pub fn composer_state_for(&self, session_id: Option<&str>, cx: &App) -> ComposerState {
        let mut state = snapshots::composer_state(
            self.thread_status(session_id, cx),
            &self.settings_replica,
            &self.providers_replica,
        );
        state.space_policy = self.space_policy.clone();
        state
    }

    /// Puts `text` into the thread's composer after whatever is typed there; nothing is sent.
    pub fn append_to_composer(&mut self, session_id: String, text: String, cx: &mut Context<Self>) {
        let pending = self.composer_appends.entry(session_id).or_default();
        if !pending.is_empty() {
            pending.push_str("\n\n");
        }
        pending.push_str(&text);
        cx.emit(StoreChange {
            topic: TopicKind::SessionStatus,
        });
        cx.notify();
    }

    /// Whether text from [`Self::append_to_composer`] or a native rewind's
    /// prefill waits for the thread's composer.
    pub fn composer_text_waiting_for(&self, session_id: Option<&str>, cx: &App) -> bool {
        self.thread_status(session_id, cx).is_some_and(|status| {
            self.composer_appends.contains_key(&status.session_id)
                || self.native_rewind_prefills.contains_key(&status.session_id)
        })
    }

    /// Consume the thread's text from [`Self::append_to_composer`].
    pub fn take_composer_append_for(
        &mut self,
        session_id: Option<&str>,
        cx: &App,
    ) -> Option<String> {
        let session_id = self.thread_status(session_id, cx)?.session_id.clone();
        self.composer_appends.remove(&session_id)
    }

    /// Consume the prompt a rewind this client asked for gave back.
    pub fn take_native_rewind_prefill_for(
        &mut self,
        session_id: Option<&str>,
        cx: &App,
    ) -> Option<RewindPrefill> {
        let session_id = self.thread_status(session_id, cx)?.session_id.clone();
        self.native_rewind_prefills.remove(&session_id)
    }

    /// Build renderer handles from replicated layout. Local affordances keep
    /// direct PTY/grid access; otherwise the handles wrap client emulators.
    pub fn with_terminal_workspace_for<R>(
        &self,
        session_id: Option<&str>,
        read: impl FnOnce(&TerminalWorkspace) -> R,
        cx: &App,
    ) -> Option<R> {
        let status = self.thread_status(session_id, cx)?;
        let workspace = TerminalWorkspace::from_replica(status, |id| self.client_terminal(id));
        Some(read(&workspace))
    }

    /// Whether `session_id`'s timeline is held whole: its log and status
    /// are here and it is not catching up on records it missed, so what
    /// draws it now draws what it will go on drawing.
    pub(crate) fn timeline_settled_for(&self, session_id: &str) -> bool {
        self.threads.get(session_id).is_some_and(|thread| {
            thread.timeline.is_some() && thread.status.is_some() && !thread.catching_up
        })
    }

    pub(crate) fn session_loading_for(&self, session_id: &str) -> bool {
        self.threads
            .get(session_id)
            .is_none_or(|thread| thread.timeline.is_none() || thread.status.is_none())
    }

    /// The title, working directory and draft flag a conversation showing
    /// `session_id`, or no thread, draws in its header.
    pub fn chat_active_session_for(
        &self,
        session_id: Option<&str>,
        cx: &App,
    ) -> Option<(String, PathBuf, bool)> {
        self.thread_status(session_id, cx)
            .map(|status| (status.title.clone(), status.cwd.clone(), status.draft))
            .or_else(|| {
                let id = session_id?;
                self.index_replica
                    .0
                    .iter()
                    .find(|meta| meta.id == id)
                    .map(|meta| (meta.title.clone(), meta.cwd.clone(), false))
            })
            .or_else(|| {
                (!self.delivery_messages_for(session_id, cx).is_empty()).then(|| {
                    (
                        crate::tr!("chat.waiting_connection").into_owned(),
                        PathBuf::new(),
                        false,
                    )
                })
            })
    }

    pub fn chat_requested_model_for(&self, session_id: Option<&str>, cx: &App) -> Option<String> {
        self.thread_status(session_id, cx)
            .and_then(|status| status.requested_model.clone())
    }

    pub fn chat_turn_changes_for(
        &self,
        session_id: Option<&str>,
        turn: usize,
        cx: &App,
    ) -> (Vec<agent::FileChange>, agent::ChangeCompleteness) {
        self.with_timeline_for(
            session_id,
            |timeline| {
                timeline
                    .turns
                    .get(turn)
                    .and_then(|turn| turn.changes.as_ref())
                    .map(|changes| (changes.changes.clone(), changes.completeness))
            },
            cx,
        )
        .flatten()
        .unwrap_or((Vec::new(), agent::ChangeCompleteness::Partial))
    }

    /// The rewind the thread's Harness offers to before the prompt that
    /// opened `turn` (local to the held window), or the steer `steer` in it;
    /// `None` when that message is no target.
    pub fn chat_rewind_target_for(
        &self,
        session_id: Option<&str>,
        turn: usize,
        steer: Option<&str>,
        cx: &App,
    ) -> Option<RewindTarget> {
        let status = self.thread_status(session_id, cx)?;
        let mut rewind = status.rewind.clone()?;
        let absolute = self.absolute_turn_for(session_id, turn);
        let (checkpoint_id, later_turns, started, returns) = self.with_timeline_for(
            session_id,
            |timeline| {
                // A message of a session a handoff left is rewound to in that
                // session, by its Harness.
                let returns = match steer {
                    None => timeline.rewind_return(turn, absolute),
                    Some(_) => timeline.left_session(turn).map(|_| None),
                }
                .map(|returns| {
                    returns.map(|to| {
                        let harness = self.harness_name(
                            to.session.acp_agent.as_deref(),
                            to.session.profile.as_deref(),
                            to.provider,
                        );
                        // The message a handoff carried has the session it
                        // left for a restore point.
                        let left = (!to.harness_rewinds).then(|| to.session.id.clone());
                        (harness, left)
                    })
                });
                if returns == Some(None) {
                    return None;
                }
                let meta = timeline.turns.get(turn)?;
                let returns = returns.flatten();
                let left = returns.as_ref().and_then(|(_, left)| left.clone());
                let checkpoint = match steer {
                    None => left.or_else(|| meta.provider_checkpoint_id.clone()),
                    Some(steer) => meta
                        .steer_checkpoints
                        .iter()
                        .find(|(id, _)| id == steer)
                        .map(|(_, checkpoint)| checkpoint.clone()),
                }?;
                let opened: HashSet<usize> = timeline
                    .entries
                    .iter()
                    .filter(|entry| {
                        entry.turn >= turn
                            && matches!(
                                entry.content,
                                tcode_core::session::EntryContent::Item(
                                    agent::ItemContent::UserMessage { .. }
                                )
                            )
                    })
                    .map(|entry| entry.turn)
                    .collect();
                let later = opened.len() - usize::from(steer.is_some() && opened.contains(&turn));
                Some((
                    checkpoint,
                    later,
                    meta.start_ts,
                    returns.map(|(harness, _)| harness),
                ))
            },
            cx,
        )??;
        if returns.is_some() {
            rewind = tcode_protocol::RewindStatus {
                modes: vec![tcode_core::session::RewindReturn::MODE],
                first_turn_conversation: true,
                steer_targets: false,
                later_history: agent::LaterHistory::Truncated,
                preview: agent::RewindPreviewKind::None,
                restores_command_edits: false,
                ..rewind
            };
        }
        let modes: Vec<agent::RewindMode> = rewind
            .modes
            .iter()
            .copied()
            .filter(|&mode| rewind.offers(mode, absolute, steer.is_some()))
            .collect();
        if modes.is_empty() {
            return None;
        }
        let since = started.map(|ms| ms / 1000);
        let delegated_after = self
            .index_replica
            .0
            .iter()
            .filter(|meta| {
                meta.parent_session_id.as_deref() == Some(status.session_id.as_str())
                    && since.is_some_and(|since| meta.created_at >= since)
            })
            .count();
        let returning = returns.is_some();
        let harness = returns.unwrap_or_else(|| self.harness_name_for(session_id, cx));
        Some(RewindTarget {
            returns: returning,
            in_progress: rewind
                .pending
                .as_ref()
                .is_some_and(|pending| !pending.undo && pending.checkpoint_id == checkpoint_id),
            checkpoint_id,
            modes,
            later_turns,
            delegated_after,
            queued: status.queued_messages.len(),
            turn_running: status.activity.turn_running,
            worktree: status.worktree.is_some(),
            harness,
            rewind,
        })
    }

    /// The fork the thread's Harness offers through finished `turn` (local to
    /// the held window).
    pub fn chat_fork_target_for(
        &self,
        session_id: Option<&str>,
        turn: usize,
        cx: &App,
    ) -> Option<ForkTarget> {
        let fork = self.thread_status(session_id, cx)?.fork_at?;
        let checkpoint_id = self.with_timeline_for(
            session_id,
            |timeline| {
                if fork.left_only && timeline.left_session(turn).is_none() {
                    return None;
                }
                timeline.fork_edge(turn)?;
                Some(timeline.turns[turn].provider_checkpoint_id.clone())
            },
            cx,
        )??;
        Some(ForkTarget {
            turn: self.absolute_turn_for(session_id, turn),
            checkpoint_id,
            blocked: fork.blocked,
        })
    }

    /// How this client names itself to the host.
    pub(crate) fn client_id(&self) -> &str {
        self.preview.client_id()
    }

    /// The name the thread's Harness goes by: an ACP agent's own, else the
    /// product's with its account.
    pub fn harness_name_for(&self, session_id: Option<&str>, cx: &App) -> String {
        let Some(status) = self.thread_status(session_id, cx) else {
            return crate::tr!("providers.harness.unnamed").into_owned();
        };
        self.harness_name(
            status.acp_agent_id.as_deref(),
            status.requested_profile_id.as_deref(),
            status.provider,
        )
    }

    /// As [`Self::harness_name_for`], for a thread known only by its index row.
    pub fn harness_name_of(&self, meta: &SessionMeta) -> String {
        self.harness_name(
            meta.acp_agent_id.as_deref(),
            meta.profile_id.as_deref(),
            meta.provider,
        )
    }

    pub(super) fn harness_name(
        &self,
        acp_agent_id: Option<&str>,
        profile_id: Option<&str>,
        provider: agent::ProviderKind,
    ) -> String {
        acp_agent_id
            .and_then(|id| self.installed_acp_agent(id))
            .map(|agent| agent.name)
            .unwrap_or_else(|| {
                self.provider_profile_display_name(
                    profile_id
                        .unwrap_or(tcode_core::settings::Settings::builtin_profile_id(provider)),
                )
            })
    }

    /// What a rewind to the target would restore, as its Harness tells
    /// before restoring anything.
    pub fn preview_rewind_for(
        &self,
        session_id: String,
        turn: usize,
        steer: Option<String>,
        checkpoint_id: String,
        mode: agent::RewindMode,
        cx: &mut App,
    ) -> Task<Result<agent::RewindPreviewResult, String>> {
        let command = self.command(
            Command::PreviewRewind {
                turn: self.absolute_turn_for(Some(&session_id), turn),
                session_id,
                steer,
                checkpoint_id: Some(checkpoint_id),
                mode,
            },
            cx,
        );
        cx.spawn(async move |_| match command.await {
            Ok(CommandResponse::RewindPreview(result)) => Ok(result),
            Ok(other) => Err(format!("unexpected answer {other:?}")),
            Err(error) => Err(error.message),
        })
    }

    pub fn chat_git_controls_for(
        &self,
        session_id: Option<&str>,
        cx: &App,
    ) -> Option<(QuickAction, Vec<MenuItem>)> {
        let git = self.thread_git(session_id, cx)?;
        let remote = self.space_allows(|policy| policy.git_remote);
        git.status.as_ref().map(|status| {
            if remote {
                (quick_action(status, git.busy), menu_items(status, git.busy))
            } else {
                tcode_core::git::local_controls(status, git.busy)
            }
        })
    }

    /// git's own error when it could not read the thread's status.
    pub fn chat_git_error_for(&self, session_id: Option<&str>, cx: &App) -> Option<String> {
        self.thread_git(session_id, cx)?.error.clone()
    }

    pub fn generate_commit_message_for(
        &self,
        session_id: Option<&str>,
        included: Option<Vec<String>>,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let session_id = session_id.unwrap_or_default().to_owned();
        let host = self.host.clone();
        cx.spawn(async move |_| {
            match host
                .query(Query::GenerateCommitMessage {
                    session_id,
                    included,
                })
                .await
            {
                Ok(QueryResponse::CommitMessage(message)) => Ok(message),
                Ok(other) => Err(format!("unexpected commit-message response: {other:?}")),
                Err(error) => Err(error.message),
            }
        })
    }

    pub fn absolute_turn_for(&self, session_id: Option<&str>, local: usize) -> usize {
        local
            + self
                .thread(session_id)
                .map_or(0, |thread| thread.turn_offset)
    }

    pub fn session_plan_for(&self, session_id: Option<&str>, cx: &App) -> Option<&SessionPlan> {
        self.watched(session_id, Part::Status, cx)?.plan.as_ref()
    }

    pub fn session_status_for(&self, session_id: Option<&str>, cx: &App) -> Option<&SessionStatus> {
        self.thread_status(session_id, cx)
    }

    /// When this client first saw the stop the thread's status has out, by
    /// the executor's clock: its turn's (`task: None`) or a background
    /// piece's.
    pub(crate) fn stop_seen(
        &self,
        session_id: Option<&str>,
        task: Option<&str>,
        cx: &App,
    ) -> Option<Instant> {
        let thread = self.watched(session_id, Part::Status, cx)?;
        let (_, seen) = thread.stops_seen.get(&task.map(str::to_owned))?;
        Some(*seen)
    }
}
