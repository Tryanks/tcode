//! "The" thread, which a window with several panes does not have. Everything
//! in the store that reads or writes it is here, and nothing is added: every
//! form has an explicit one taking the thread, and its callers move to that.

use std::path::PathBuf;

use gpui::Context;
use tcode_core::{
    session::{ReviewComment, Timeline},
    ui::RightTab,
};
use tcode_protocol::SessionStatus;

#[cfg(test)]
use super::PanelState;
use super::{ComposerState, WorkspaceStore};

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

    #[cfg(test)]
    pub(crate) fn session_loading(&self) -> bool {
        self.focus().is_some_and(|id| self.session_loading_for(id))
    }

    pub fn chat_active_session(&self) -> Option<(String, PathBuf, bool)> {
        self.chat_active_session_for(self.focus())
    }

    pub fn chat_loading(&self) -> bool {
        self.chat_loading_for(self.focus())
    }

    pub fn composer_state(&self) -> ComposerState {
        self.composer_state_for(self.focus())
    }

    pub fn with_active_timeline<R>(&self, read: impl FnOnce(&Timeline) -> R) -> Option<R> {
        self.with_timeline_for(self.focus(), read)
    }

    #[cfg(test)]
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

    #[cfg(test)]
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
    #[cfg(test)]
    pub(crate) fn leave_session(&mut self, cx: &mut Context<Self>) {
        self.selection_generation = self.selection_generation.wrapping_add(1);
        self.draft_fallback_pending = false;
        if let Some(session_id) = self.focused_session_id.take() {
            self.close_thread(&session_id, cx);
        }
        cx.notify();
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

    /// What store-only tests act on a thread with, there being no shell for
    /// [`Self::show_thread`] to reach: it replaces the focused thread, or
    /// focuses one already open.
    #[cfg(test)]
    pub(crate) fn select_session(&mut self, session_id: String, cx: &mut Context<Self>) {
        cx.emit(super::ShowThread {
            session_id: session_id.clone(),
            replacing: None,
            asked: false,
        });
        self.select_for_test(session_id, cx);
    }

    #[cfg(test)]
    pub(crate) fn select_for_test(&mut self, session_id: String, cx: &mut Context<Self>) {
        self.creation_request_generation = self.creation_request_generation.wrapping_add(1);
        self.draft_fallback_pending = false;
        if self.focused_session_id.as_ref() == Some(&session_id) {
            return;
        }
        self.remember_project_of(&session_id);
        if self.is_thread_open(&session_id) {
            self.focus_thread(&session_id, cx);
            return;
        }
        self.leave_session(cx);
        self.focused_session_id = Some(session_id.clone());
        self.open_thread(&session_id);
    }
}
