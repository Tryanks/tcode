use super::*;

impl AppState {
    // -- conversation-owned terminal resources -----------------------------

    pub(super) fn restore_terminal_workspace(&mut self, active: &mut ActiveSession) -> bool {
        let destination = conversation_destination(active);
        let Some(workspace) = self.terminal_workspaces.remove(&destination) else {
            return false;
        };
        active.terminal_workspace = workspace;
        true
    }

    pub(super) fn park_terminal_workspace(&mut self, active: &mut ActiveSession) {
        let destination = conversation_destination(active);
        let workspace = std::mem::take(&mut active.terminal_workspace);
        self.terminal_workspaces.insert(destination, workspace);
    }

    pub(super) fn terminal_preferences_for(
        &self,
        active: &ActiveSession,
    ) -> Option<TerminalPreferences> {
        self.terminal_preferences
            .get(&conversation_destination(active).preference_key())
            .copied()
    }

    pub(super) fn write_terminal_preferences(&mut self, cx: &mut HostCx) {
        match serde_json::to_vec_pretty(&self.terminal_preferences) {
            Ok(bytes) => self.enqueue_store_write(StoreWrite::WriteTerminalUi(bytes), cx),
            Err(error) => log::warn!("failed to encode terminal UI state: {error}"),
        }
    }

    pub(super) fn terminal_prefs_mut(
        &mut self,
        key: String,
        count: usize,
    ) -> &mut TerminalPreferences {
        self.terminal_preferences
            .entry(key)
            .or_insert(TerminalPreferences {
                open: false,
                height: 240.,
                count,
            })
    }

    pub(super) fn reopen_persisted_terminals(
        &mut self,
        preferences: Option<TerminalPreferences>,
        cx: &mut HostCx,
    ) {
        if !preferences.is_some_and(|preferences| preferences.open) {
            return;
        }
        self.open_terminal_panel(cx);
        let count = preferences
            .map(|preferences| preferences.count.clamp(1, MAX_TERMINALS_PER_SESSION))
            .unwrap_or(1);
        for _ in 1..count {
            self.new_terminal(cx);
        }
    }

    // -- terminal drawer ---------------------------------------------------

    pub(super) fn persist_terminal_resource_count(&mut self, cx: &mut HostCx) {
        if let Some(active) = self.active.as_ref() {
            let key = conversation_destination(active).preference_key();
            let count = active.terminal_workspace.terminals.len();
            self.terminal_prefs_mut(key, count).count = count;
        }
        self.write_terminal_preferences(cx);
    }

    pub fn set_terminal_height(&mut self, height: f32, cx: &mut HostCx) {
        if let Some((key, count)) = self.active.as_ref().map(|active| {
            (
                conversation_destination(active).preference_key(),
                active.terminal_workspace.terminals.len(),
            )
        }) {
            self.terminal_prefs_mut(key, count).height = height;
            self.write_terminal_preferences(cx);
        }
    }

    pub(crate) fn terminal_panel_open(&self) -> bool {
        self.active
            .as_ref()
            .and_then(|active| self.terminal_preferences_for(active))
            .is_some_and(|preferences| preferences.open)
    }

    pub fn toggle_terminal_panel(&mut self, cx: &mut HostCx) {
        if self.terminal_panel_open() {
            self.close_terminal_panel(cx);
        } else {
            self.open_terminal_panel(cx);
        }
    }

    pub(super) fn schedule_terminal_spawn(
        &mut self,
        session_id: String,
        cwd: PathBuf,
        action: TerminalSpawnAction,
        cx: &mut HostCx,
    ) {
        self.next_terminal_spawn_id = self
            .next_terminal_spawn_id
            .checked_add(1)
            .expect("terminal spawn id overflow");
        let spawn_id = self.next_terminal_spawn_id;
        self.pending_terminal_spawns
            .entry(session_id.clone())
            .or_default()
            .insert(spawn_id, action);

        // Capture the thread-local cwd override before the work moves to the
        // background executor.
        let cwd = term::Terminal::resolve_spawn_cwd(cwd);
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let result = host_cx.unblock(move || term::Terminal::spawn(cwd)).await;
            host_cx.enqueue(move |state, cx| {
                let pending = state
                    .pending_terminal_spawns
                    .get_mut(&session_id)
                    .and_then(|spawns| spawns.remove(&spawn_id));
                if state
                    .pending_terminal_spawns
                    .get(&session_id)
                    .is_some_and(HashMap::is_empty)
                {
                    state.pending_terminal_spawns.remove(&session_id);
                }
                let Some(action) = pending else {
                    return;
                };
                let active_matches = state
                    .active
                    .as_ref()
                    .is_some_and(|active| active.meta.id == session_id);
                if !active_matches {
                    return;
                }

                let terminal = match result {
                    Ok(terminal) => terminal,
                    Err(error) => {
                        let runtime_error = match action {
                            TerminalSpawnAction::Restart { .. } => RuntimeError::TerminalRestart {
                                error: error.to_string(),
                            },
                            _ => RuntimeError::TerminalStart {
                                error: error.to_string(),
                            },
                        };
                        state.report_error(runtime_error, cx);
                        return;
                    }
                };

                let workspace = &mut state.active.as_mut().unwrap().terminal_workspace;
                let mut followup_split = None;
                let applied = match action {
                    TerminalSpawnAction::Open { split_after } => {
                        if workspace.terminals.len() < MAX_TERMINALS_PER_SESSION {
                            let first = workspace.push(terminal);
                            followup_split = split_after.map(|direction| (first, direction));
                            true
                        } else {
                            false
                        }
                    }
                    TerminalSpawnAction::Restart { terminal_id } => {
                        if let Some(entry) = workspace
                            .terminals
                            .iter_mut()
                            .find(|entry| Some(entry.id) == terminal_id)
                        {
                            entry.terminal = terminal.into();
                            true
                        } else if terminal_id.is_none() && workspace.terminals.is_empty() {
                            workspace.push(terminal);
                            true
                        } else {
                            false
                        }
                    }
                    TerminalSpawnAction::New => {
                        if workspace.terminals.len() < MAX_TERMINALS_PER_SESSION {
                            workspace.push(terminal);
                            true
                        } else {
                            false
                        }
                    }
                    TerminalSpawnAction::Split { first, direction } => {
                        if workspace.terminals.len() < MAX_TERMINALS_PER_SESSION
                            && workspace.terminal(first).is_some()
                            && workspace.split_for(first).is_none()
                        {
                            let second = workspace.push(terminal);
                            workspace.splits.push(TerminalSplit {
                                first,
                                second,
                                direction,
                            });
                            true
                        } else {
                            false
                        }
                    }
                };
                if applied {
                    state.persist_terminal_resource_count(cx);
                }
                if let Some((first, direction)) = followup_split {
                    let cwd = state.active.as_ref().unwrap().meta.cwd.clone();
                    state.schedule_terminal_spawn(
                        session_id,
                        cwd,
                        TerminalSpawnAction::Split { first, direction },
                        cx,
                    );
                }
            });
        });
    }

    pub(super) fn cancel_pending_terminal_spawns(&mut self, session_id: &str) {
        self.pending_terminal_spawns.remove(session_id);
    }

    pub(crate) fn open_terminal_panel(&mut self, cx: &mut HostCx) {
        let Some((session_id, cwd, destination, count, terminals_empty)) =
            self.active.as_ref().map(|active| {
                (
                    active.meta.id.clone(),
                    active.meta.cwd.clone(),
                    conversation_destination(active),
                    active.terminal_workspace.terminals.len(),
                    active.terminal_workspace.terminals.is_empty(),
                )
            })
        else {
            return;
        };
        let key = destination.preference_key();
        self.terminal_prefs_mut(key, count).open = true;
        self.write_terminal_preferences(cx);
        if terminals_empty {
            let already_pending =
                self.pending_terminal_spawns
                    .get(&session_id)
                    .is_some_and(|spawns| {
                        spawns
                            .values()
                            .any(|action| matches!(action, TerminalSpawnAction::Open { .. }))
                    });
            if !already_pending {
                self.schedule_terminal_spawn(
                    session_id,
                    cwd,
                    TerminalSpawnAction::Open { split_after: None },
                    cx,
                );
            }
        }
    }

    pub fn close_terminal_panel(&mut self, cx: &mut HostCx) {
        let session_id = self.active.as_ref().map(|active| active.meta.id.clone());
        if let Some(session_id) = session_id.as_deref() {
            self.cancel_pending_terminal_spawns(session_id);
        }
        if let Some(active) = self.active.as_ref() {
            let key = conversation_destination(active).preference_key();
            let count = active.terminal_workspace.terminals.len();
            self.terminal_prefs_mut(key, count).open = false;
            self.write_terminal_preferences(cx);
        }
    }

    pub fn restart_terminal(&mut self, cx: &mut HostCx) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        let session_id = active.meta.id.clone();
        let cwd = active.meta.cwd.clone();
        let terminal_id = active.terminal_workspace.active_id;
        if let Some(spawns) = self.pending_terminal_spawns.get_mut(&session_id) {
            spawns.retain(|_, action| !matches!(action, TerminalSpawnAction::Restart { .. }));
        }
        self.schedule_terminal_spawn(
            session_id,
            cwd,
            TerminalSpawnAction::Restart { terminal_id },
            cx,
        );
    }

    pub fn new_terminal(&mut self, cx: &mut HostCx) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        let pending = self
            .pending_terminal_spawns
            .get(&active.meta.id)
            .map_or(0, HashMap::len);
        if active.terminal_workspace.terminals.len() + pending >= MAX_TERMINALS_PER_SESSION {
            return;
        }
        self.schedule_terminal_spawn(
            active.meta.id.clone(),
            active.meta.cwd.clone(),
            TerminalSpawnAction::New,
            cx,
        );
    }

    pub fn activate_terminal(&mut self, terminal_id: u64, _cx: &mut HostCx) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if active.terminal_workspace.terminal(terminal_id).is_some() {
            active.terminal_workspace.active_id = Some(terminal_id);
        }
    }

    pub fn close_terminal(&mut self, terminal_id: u64, cx: &mut HostCx) {
        let session_id = self.active.as_ref().map(|active| active.meta.id.clone());
        if let Some(session_id) = session_id.as_deref() {
            self.cancel_pending_terminal_spawns(session_id);
        }
        let Some(active) = self.active.as_mut() else {
            return;
        };
        let workspace = &mut active.terminal_workspace;
        workspace.terminals.retain(|entry| entry.id != terminal_id);
        workspace
            .splits
            .retain(|split| split.first != terminal_id && split.second != terminal_id);
        if workspace.active_id == Some(terminal_id) {
            workspace.active_id = workspace.terminals.last().map(|entry| entry.id);
        }
        let empty = workspace.terminals.is_empty();
        self.persist_terminal_resource_count(cx);
        if empty {
            self.close_terminal_panel(cx);
        }
    }

    pub fn split_terminal(&mut self, direction: TerminalSplitDirection, cx: &mut HostCx) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        let workspace = &active.terminal_workspace;
        let Some(first) = workspace.active_id else {
            return;
        };
        let pending = self
            .pending_terminal_spawns
            .get(&active.meta.id)
            .map_or(0, HashMap::len);
        if workspace.terminals.len() + pending >= MAX_TERMINALS_PER_SESSION
            || workspace.split_for(first).is_some()
            || self
                .pending_terminal_spawns
                .get(&active.meta.id)
                .is_some_and(|spawns| {
                    spawns.values().any(|action| {
                        matches!(
                            action,
                            TerminalSpawnAction::Split {
                                first: pending_first,
                                ..
                            } if *pending_first == first
                        )
                    })
                })
        {
            return;
        }
        self.schedule_terminal_spawn(
            active.meta.id.clone(),
            active.meta.cwd.clone(),
            TerminalSpawnAction::Split { first, direction },
            cx,
        );
    }

    pub fn capture_terminal_selection(&mut self, terminal_id: u64, _cx: &mut HostCx) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        let Some(entry) = active.terminal_workspace.terminal(terminal_id) else {
            return;
        };
        let label = entry.terminal.label();
        let selection = entry.terminal.selected_text();
        if let Some(selection) = selection {
            active.terminal_workspace.add_context(label, selection);
        }
    }

    pub fn remove_terminal_context(&mut self, context_id: u64, _cx: &mut HostCx) {
        if let Some(active) = self.active.as_mut() {
            active
                .terminal_workspace
                .contexts
                .retain(|context| context.id != context_id);
        }
    }

    pub(crate) fn review_comments(&self) -> &[ReviewComment] {
        let Some(id) = self.active.as_ref().map(|active| active.meta.id.as_str()) else {
            return &[];
        };
        self.review_comment_drafts
            .get(id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn add_review_comment(&mut self, comment: ReviewComment, _cx: &mut HostCx) {
        if let Some(id) = self.active.as_ref().map(|active| active.meta.id.clone()) {
            self.review_comment_drafts
                .entry(id.clone())
                .or_default()
                .push(comment);
        }
    }

    pub fn remove_review_comment(&mut self, index: usize, _cx: &mut HostCx) {
        let Some(id) = self.active.as_ref().map(|active| active.meta.id.clone()) else {
            return;
        };
        if let Some(comments) = self.review_comment_drafts.get_mut(&id)
            && index < comments.len()
        {
            comments.remove(index);
        }
    }

    pub(super) fn clear_review_comments(&mut self, _cx: &mut HostCx) {
        if let Some(id) = self.active.as_ref().map(|active| active.meta.id.clone()) {
            self.review_comment_drafts.remove(&id);
        }
    }

    /// Drop the attached terminal contexts once a message consuming them is sent.
    pub(super) fn clear_terminal_contexts(&mut self) {
        if let Some(active) = self.active.as_mut() {
            active.terminal_workspace.contexts.clear();
        }
    }
}
