use super::*;

impl WorkspaceStore {
    /// Open the diff of `session_id`'s `turn`, at `path` if one is given.
    pub fn open_diff(
        &mut self,
        session_id: &str,
        turn: usize,
        path: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(ui) = self.thread_conversation_ui_mut(Some(session_id)) {
            ui.right_panel_open = true;
            ui.right_tab = RightTab::Diff;
            ui.diff_selected_turn = Some(turn);
            ui.pending_diff_focus = Some(DiffFocus {
                session: session_id.to_owned(),
                turn,
                path,
            });
            ui.refresh_diff();
            cx.notify();
        }
    }

    pub fn select_diff_turn_for(
        &mut self,
        session_id: Option<&str>,
        turn: usize,
        cx: &mut Context<Self>,
    ) {
        if let Some(ui) = self.thread_conversation_ui_mut(session_id) {
            ui.pending_diff_focus = None;
            ui.diff_selected_turn = Some(turn);
            ui.refresh_diff();
            cx.notify();
        }
    }

    pub fn discard_diff_focus_for(&mut self, session_id: Option<&str>, cx: &mut Context<Self>) {
        if let Some(ui) = self.thread_conversation_ui_mut(session_id) {
            ui.discard_diff_focus();
            cx.notify();
        }
    }

    /// Close `session_id`'s right panel, whichever tab it shows.
    pub fn close_right_panel_for(&mut self, session_id: Option<&str>, cx: &mut Context<Self>) {
        if let Some(ui) = self.thread_conversation_ui_mut(session_id) {
            ui.pending_diff_focus = None;
            ui.right_panel_open = false;
        }
        cx.notify();
    }

    pub fn toggle_diff_expanded_for(&mut self, session_id: Option<&str>, cx: &mut Context<Self>) {
        if let Some(ui) = self.thread_conversation_ui_mut(session_id) {
            ui.right_panel_expanded = !ui.right_panel_expanded;
            cx.notify();
        }
    }

    pub fn set_right_tab_for(
        &mut self,
        session_id: Option<&str>,
        tab: RightTab,
        cx: &mut Context<Self>,
    ) {
        if tab == RightTab::Preview && !self.offers_preview() {
            return;
        }
        if let Some(ui) = self.thread_conversation_ui_mut(session_id) {
            if tab == RightTab::Diff && ui.right_tab != RightTab::Diff {
                ui.refresh_diff();
            }
            ui.right_tab = tab;
            cx.notify();
        }
    }

    /// Show `tab` of `session_id`'s right panel, or close the panel while it
    /// shows that tab.
    pub fn toggle_tab_panel_for(
        &mut self,
        session_id: &str,
        tab: RightTab,
        cx: &mut Context<Self>,
    ) {
        if tab == RightTab::Preview && !self.offers_preview() {
            return;
        }
        let showing = self
            .thread_conversation_ui(Some(session_id))
            .is_some_and(|ui| ui.right_panel_open && ui.right_tab == tab);
        if showing {
            self.close_right_panel_for(Some(session_id), cx);
        } else {
            self.open_tab_for(session_id, tab, cx);
        }
    }
    pub fn close_preview_panel_for(&mut self, session_id: Option<&str>, cx: &mut Context<Self>) {
        if let Some(ui) = self
            .thread_conversation_ui_mut(session_id)
            .filter(|ui| ui.right_panel_open && ui.right_tab == RightTab::Preview)
        {
            ui.right_panel_open = false;
            cx.notify();
        }
    }

    pub fn open_preview_panel_for(&mut self, session_id: Option<&str>, cx: &mut Context<Self>) {
        if !self.offers_preview() {
            return;
        }
        if let Some(ui) = self.thread_conversation_ui_mut(session_id)
            && !(ui.right_panel_open && ui.right_tab == RightTab::Preview)
        {
            ui.right_panel_open = true;
            ui.right_tab = RightTab::Preview;
            cx.notify();
        }
    }

    pub fn open_tab_for(&mut self, session_id: &str, tab: RightTab, cx: &mut Context<Self>) {
        if tab == RightTab::Preview && !self.offers_preview() {
            return;
        }
        let destination = self
            .threads
            .get(session_id)
            .filter(|thread| thread.open)
            .and_then(|thread| thread.destination.clone())
            .unwrap_or_else(|| ConversationDestination::Thread(session_id.to_string()));
        let ui = self.conversation_ui.entry(destination).or_insert_with(|| {
            ConversationUiState::new(self.settings_replica.word_wrap_diffs, false, 240.)
        });
        ui.right_panel_open = true;
        ui.right_tab = tab;
        if tab == RightTab::Diff {
            ui.refresh_diff();
        }
        cx.notify();
    }

    pub(crate) fn commit_dialog_state_for(
        &self,
        session_id: Option<&str>,
        cx: &App,
    ) -> CommitDialogState {
        let git = self.thread_git(session_id, cx);
        let status = git.and_then(|git| git.status.as_ref());
        CommitDialogState {
            files: status
                .map(|status| status.changed_files.clone())
                .unwrap_or_default(),
            branch: status.and_then(|status| status.branch.clone()),
            on_default_branch: status.is_some_and(|status| status.is_default_branch),
            error: git.and_then(|git| git.error.clone()),
        }
    }

    pub(crate) fn diff_active_state_for(
        &self,
        session_id: Option<&str>,
        cx: &App,
    ) -> Option<DiffActiveState> {
        self.thread_status(session_id, cx)
            .map(|status| DiffActiveState {
                session: status.session_id.clone(),
                cwd: status.cwd.clone(),
                branches: status.branches.clone(),
            })
    }

    pub fn diff_turns_for(&self, session_id: Option<&str>, cx: &App) -> Vec<usize> {
        self.with_timeline_for(
            session_id,
            |timeline| {
                timeline
                    .turns
                    .iter()
                    .enumerate()
                    .filter_map(|(turn, meta)| {
                        meta.changes
                            .as_ref()
                            .is_some_and(|changes| !changes.changes.is_empty())
                            .then_some(turn)
                    })
                    .collect()
            },
            cx,
        )
        .unwrap_or_default()
    }

    pub fn diff_selected_turn_for(&self, session_id: Option<&str>, cx: &App) -> Option<usize> {
        let turns = self.diff_turns_for(session_id, cx);
        let explicit = self
            .thread_conversation_ui(session_id)
            .and_then(|ui| ui.diff_selected_turn);
        match explicit {
            Some(turn) if turns.contains(&turn) => Some(turn),
            _ => turns.last().copied(),
        }
    }

    pub fn with_diff_turn_changes_for<R>(
        &self,
        session_id: Option<&str>,
        turn: usize,
        read: impl FnOnce(&[agent::FileChange], agent::ChangeCompleteness) -> R,
        cx: &App,
    ) -> Option<R> {
        self.with_timeline_for(
            session_id,
            |timeline| {
                let changes = timeline.turns.get(turn)?.changes.as_ref()?;
                Some(read(&changes.changes, changes.completeness))
            },
            cx,
        )
        .flatten()
    }

    pub(crate) fn pending_diff_focus_for(&self, session_id: Option<&str>) -> Option<DiffFocus> {
        self.thread_conversation_ui(session_id)
            .and_then(|ui| ui.pending_diff_focus.clone())
    }

    /// UI-only consuming selector. The underlying diff focus is replica state;
    /// this does not cross the host boundary.
    pub(crate) fn take_diff_focus_for(
        &mut self,
        session_id: Option<&str>,
        session: &str,
        turn: usize,
    ) -> Option<DiffFocus> {
        self.thread_conversation_ui_mut(session_id)?
            .take_diff_focus(session, turn)
    }

    pub fn diff_refresh_generation_for(&self, session_id: Option<&str>) -> u64 {
        self.thread_conversation_ui(session_id)
            .map(|ui| ui.diff_refresh_generation)
            .unwrap_or(0)
    }

    pub fn diff_word_wrap_for(&self, session_id: Option<&str>) -> bool {
        self.thread_conversation_ui(session_id)
            .map(|ui| ui.diff_wrap)
            .unwrap_or(self.settings_replica.word_wrap_diffs)
    }

    pub fn diff_split_for(&self, session_id: Option<&str>) -> bool {
        self.thread_conversation_ui(session_id)
            .is_some_and(|ui| ui.diff_split)
    }

    pub fn set_diff_split_for(
        &mut self,
        session_id: Option<&str>,
        split: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(ui) = self.thread_conversation_ui_mut(session_id) {
            ui.diff_split = split;
            cx.notify();
        }
    }

    pub fn toggle_diff_wrap_for(&mut self, session_id: Option<&str>, cx: &mut Context<Self>) {
        if let Some(ui) = self.thread_conversation_ui_mut(session_id) {
            ui.diff_wrap = !ui.diff_wrap;
            cx.notify();
        }
    }

    /// The panels of `session_id`'s conversation, or of no thread.
    pub(crate) fn panel_state_for(&self, session_id: Option<&str>) -> PanelState {
        let mut state = snapshots::panel_state(self.thread_conversation_ui(session_id));
        state.terminal_open &= self.space_allows(|policy| policy.terminals);
        state
    }

    pub fn review_comments_for(&self, session_id: Option<&str>, cx: &App) -> Vec<ReviewComment> {
        self.thread_status(session_id, cx)
            .map(|status| status.review_comment_drafts.clone())
            .unwrap_or_default()
    }

    pub fn load_git_diff(
        &self,
        cwd: &std::path::Path,
        scope: GitDiffScope,
        base: Option<&str>,
        ignore_whitespace: bool,
        cx: &mut App,
    ) -> Task<GitDiffResult> {
        let host = self.host.clone();
        let query = Query::LoadGitDiff {
            cwd: cwd.to_path_buf(),
            scope,
            base: base.map(str::to_string),
            ignore_whitespace,
        };
        cx.spawn(async move |_| match host.query(query).await {
            Ok(QueryResponse::GitDiff(diff)) => diff,
            Ok(other) => GitDiffResult {
                error: Some(format!("unexpected git-diff response: {other:?}")),
                ..GitDiffResult::default()
            },
            Err(error) => GitDiffResult {
                error: Some(error.message),
                ..GitDiffResult::default()
            },
        })
    }
}
