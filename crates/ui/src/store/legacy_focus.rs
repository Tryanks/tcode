//! "The" thread, which a window with several panes does not have. Everything
//! in the store that reads or writes it is here, and nothing is added: every
//! form has an explicit one taking the thread, and its callers move to that.

use std::path::PathBuf;

use gpui::Context;
use tcode_core::{
    session::{ReviewComment, Timeline},
    ui::RightTab,
};
use tcode_protocol::{SessionStatus, Topic};

use super::{ComposerState, PanelState, WorkspaceStore};

impl WorkspaceStore {
    pub fn legacy_focus(&self) -> Option<String> {
        self.focused_session_id.clone()
    }

    fn focus(&self) -> Option<&str> {
        self.focused_session_id.as_deref()
    }

    pub(super) fn focused_status(&self) -> Option<&SessionStatus> {
        self.thread_status(self.focus())
    }

    pub fn session_status(&self) -> Option<&SessionStatus> {
        self.focused_status()
    }

    pub(crate) fn session_loading(&self) -> bool {
        self.focus().is_some_and(|id| self.session_loading_for(id))
    }

    pub fn chat_active_session(&self) -> Option<(String, PathBuf, bool)> {
        self.chat_active_session_for(self.focus())
    }

    pub fn chat_loading(&self) -> bool {
        self.chat_loading_for(self.focus())
    }

    pub fn baseline_ready(&self) -> bool {
        self.baseline_ready_for(self.focus())
    }

    pub fn composer_state(&self) -> ComposerState {
        self.composer_state_for(self.focus())
    }

    pub fn with_active_timeline<R>(&self, read: impl FnOnce(&Timeline) -> R) -> Option<R> {
        self.with_timeline_for(self.focus(), read)
    }

    pub(crate) fn delivery_messages(&self) -> Vec<(String, String, Option<String>, bool)> {
        self.delivery_messages_for(self.focus())
    }

    pub fn roster_session_id(&self) -> Option<String> {
        self.focus().map(|id| self.roster_row(id))
    }

    pub fn shell_window_title(&self) -> String {
        self.shell_window_title_for(self.focus())
    }

    pub fn active_session_cwd(&self) -> Option<PathBuf> {
        self.session_cwd_for(self.focus())
    }

    pub fn preview_active_identity(&self) -> Option<(String, String)> {
        let session_id = self.focus()?;
        Some((session_id.to_owned(), self.preview_key_for(session_id)?))
    }

    pub(crate) fn panel_state(&self) -> PanelState {
        self.panel_state_for(self.focus())
    }

    pub fn review_comments(&self) -> Vec<ReviewComment> {
        self.review_comments_for(self.focus())
    }

    pub fn write_relaunch_marker(&mut self, reopen_settings: String) {
        self.write_relaunch_marker_for(self.legacy_focus().unwrap_or_default(), reopen_settings);
    }

    pub fn toggle_terminal_panel(&mut self, cx: &mut Context<Self>) {
        self.toggle_terminal_panel_for(self.legacy_focus().unwrap_or_default(), cx);
    }

    pub fn toggle_diff_panel(&mut self, cx: &mut Context<Self>) {
        self.toggle_tab_panel(RightTab::Diff, cx);
    }

    pub fn toggle_pull_requests_panel(&mut self, cx: &mut Context<Self>) {
        self.toggle_tab_panel(RightTab::PullRequests, cx);
    }

    pub fn toggle_plan_panel(&mut self, cx: &mut Context<Self>) {
        self.toggle_tab_panel(RightTab::Plan, cx);
    }

    pub fn toggle_agents_panel(&mut self, cx: &mut Context<Self>) {
        self.toggle_tab_panel(RightTab::Agents, cx);
    }

    pub fn toggle_preview_panel(&mut self, cx: &mut Context<Self>) {
        self.toggle_tab_panel(RightTab::Preview, cx);
    }

    fn toggle_tab_panel(&mut self, tab: RightTab, cx: &mut Context<Self>) {
        if let Some(session_id) = self.legacy_focus() {
            self.toggle_tab_panel_for(&session_id, tab, cx);
        }
    }

    pub fn close_diff_panel(&mut self, cx: &mut Context<Self>) {
        let session_id = self.legacy_focus();
        self.close_right_panel_for(session_id.as_deref(), cx);
    }

    pub fn open_preview_panel(&mut self, cx: &mut Context<Self>) {
        let session_id = self.legacy_focus();
        self.open_preview_panel_for(session_id.as_deref(), cx);
    }

    pub fn set_right_tab(&mut self, tab: RightTab, cx: &mut Context<Self>) {
        let session_id = self.legacy_focus();
        self.set_right_tab_for(session_id.as_deref(), tab, cx);
    }

    pub fn toggle_diff_expanded(&mut self, cx: &mut Context<Self>) {
        let session_id = self.legacy_focus();
        self.toggle_diff_expanded_for(session_id.as_deref(), cx);
    }

    pub fn diff_selected_turn(&self) -> Option<usize> {
        self.diff_selected_turn_for(self.focus())
    }

    /// Closing the focused thread leaves nothing focused.
    pub(super) fn unfocus(&mut self, session_id: &str) {
        if self.focus() == Some(session_id) {
            self.focused_session_id = None;
        }
    }

    /// Leave the focused thread: nothing is focused, and the thread closes.
    pub(crate) fn leave_session(&mut self, cx: &mut Context<Self>) {
        self.selection_generation = self.selection_generation.wrapping_add(1);
        self.draft_fallback_pending = false;
        if let Some(session_id) = self.focused_session_id.take() {
            self.close_thread(&session_id, cx);
        }
        cx.notify();
    }

    /// Focus nothing while every thread stays open, for a surface about to
    /// show a thread that does not exist yet.
    pub(crate) fn release_focus(&mut self, cx: &mut Context<Self>) {
        self.selection_generation = self.selection_generation.wrapping_add(1);
        if self.focused_session_id.take().is_some() {
            cx.notify();
        }
    }

    /// Act on an open thread, which another surface shows, leaving every
    /// other thread as it is. A thread that is not open is not focused.
    pub(crate) fn focus_thread(&mut self, session_id: &str, cx: &mut Context<Self>) {
        if self.focused_session_id.as_deref() == Some(session_id)
            || !self.is_thread_open(session_id)
        {
            return;
        }
        self.selection_generation = self.selection_generation.wrapping_add(1);
        self.focused_session_id = Some(session_id.to_owned());
        cx.notify();
    }

    /// Act on a thread: one another surface shows only takes the focus,
    /// any other replaces the focused thread.
    pub fn select_session(&mut self, session_id: String, cx: &mut Context<Self>) {
        let shown = self.is_thread_open(&session_id);
        self.select(session_id, shown, cx);
    }

    /// Select the focused thread afresh once a new baseline dropped its
    /// subscriptions.
    pub(super) fn reselect_focused(&mut self, cx: &mut Context<Self>) {
        if let Some(session_id) = self.focused_session_id.take() {
            self.select(session_id, false, cx);
        }
    }

    fn select(&mut self, session_id: String, shown: bool, cx: &mut Context<Self>) {
        self.creation_request_generation = self.creation_request_generation.wrapping_add(1);
        self.draft_fallback_pending = false;
        if self.focused_session_id.as_ref() == Some(&session_id) {
            return;
        }
        let project_id = self
            .index_replica
            .0
            .iter()
            .find(|meta| meta.id == session_id)
            .and_then(|meta| meta.project_id.clone());
        self.remember_project(project_id);
        if shown {
            self.focus_thread(&session_id, cx);
            return;
        }
        self.leave_session(cx);
        self.focused_session_id = Some(session_id.clone());
        self.open_thread(&session_id);
    }

    /// Decide what the workspace shows after the index changed.
    ///
    /// Every open thread that left the visible index — archived (alone or
    /// with its lead), deleted — closes, so no surface keeps showing it. Only
    /// the conversation on screen picks what comes next, so archiving a
    /// background thread (an Orchestrate sibling completing, a sweep) never
    /// steals navigation: the workspace follows the focused thread's
    /// still-visible parent; with no such parent it falls back to the last
    /// interacted project's draft, which is also what an empty workspace
    /// opens.
    pub(super) fn reconcile_destination(&mut self, cx: &mut Context<Self>) {
        // A deletion can arrive before the rejected Ack. Keep the authored
        // write on screen so its failure still has Retry and Discard controls.
        if !self.delivery_messages().is_empty() {
            return;
        }
        let left: Vec<String> = self
            .threads
            .iter()
            .filter(|(id, thread)| {
                thread.open
                    && self.focused_session_id.as_ref() != Some(*id)
                    && thread
                        .status
                        .as_ref()
                        .is_some_and(|status| self.left_index(status))
            })
            .map(|(id, _)| id.clone())
            .collect();
        for session_id in left {
            self.close_thread(&session_id, cx);
        }
        match self.focused_status() {
            Some(status) if !self.left_index(status) => return,
            // A draft has no index entry of its own; its project leaving
            // the index leaves nothing to draft into.
            Some(status) if status.draft => self.leave_session(cx),
            Some(status) => {
                let session_id = status.session_id.clone();
                let parent = self
                    .index_replica
                    .0
                    .iter()
                    .chain(&self.removed_session)
                    .find(|meta| meta.id == session_id)
                    .and_then(|meta| meta.parent_session_id.clone())
                    .filter(|parent| self.session_visible(parent));
                if let Some(parent) = parent {
                    self.select_session(parent, cx);
                    return;
                }
                self.leave_session(cx);
            }
            // Focused, but its first status has not arrived: nothing to
            // decide yet. Without a focus this is the empty workspace.
            None if self.focused_session_id.is_some() => return,
            None => {}
        }
        self.open_last_project_draft(cx);
    }

    pub(super) fn restore_index_selection(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.focused_session_id.clone() {
            let visible = self.index_replica.0.iter().any(|meta| meta.id == id)
                || self.focused_status().is_some_and(|status| {
                    status.draft
                        && status.project_id.as_ref().is_some_and(|project| {
                            self.index_replica.1.iter().any(|p| &p.id == project)
                        })
                });
            if visible
                && !self
                    .host
                    .subscribed_topics()
                    .contains(&Topic::SessionStatus {
                        session_id: id.clone(),
                    })
            {
                self.reselect_focused(cx);
            } else if !visible && !self.scope.is_full() {
                self.leave_session(cx);
            }
        }
    }
}
