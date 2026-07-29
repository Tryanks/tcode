//! Client-owned UI state that follows a conversation across surface switches.

use std::collections::HashMap;

use tcode_core::ui::{ConversationDestination, RightTab};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiffFocus {
    pub session: String,
    pub turn: usize,
    pub path: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ConversationUiState {
    pub right_panel_open: bool,
    pub right_panel_expanded: bool,
    pub right_tab: RightTab,
    pub diff_selected_turn: Option<usize>,
    pub diff_split: bool,
    pub diff_wrap: bool,
    pub pending_diff_focus: Option<DiffFocus>,
    pub diff_refresh_generation: u64,
    pub terminal_open: bool,
    pub terminal_height: f32,
    pub auto_open_task_suppressed: bool,
    pub preview_url: Option<String>,
    pub preview_canvas: Option<(u32, u32)>,
}

impl ConversationUiState {
    pub fn new(diff_wrap: bool, terminal_open: bool, terminal_height: f32) -> Self {
        Self {
            right_panel_open: false,
            right_panel_expanded: false,
            right_tab: RightTab::default(),
            diff_selected_turn: None,
            diff_split: false,
            diff_wrap,
            pending_diff_focus: None,
            diff_refresh_generation: 0,
            terminal_open,
            terminal_height,
            auto_open_task_suppressed: false,
            preview_url: None,
            preview_canvas: None,
        }
    }

    pub fn refresh_diff(&mut self) {
        self.diff_refresh_generation = self.diff_refresh_generation.wrapping_add(1);
    }

    pub fn take_diff_focus(&mut self, session: &str, turn: usize) -> Option<DiffFocus> {
        let matches = self
            .pending_diff_focus
            .as_ref()
            .is_some_and(|request| request.session == session && request.turn == turn);
        matches.then(|| self.pending_diff_focus.take()).flatten()
    }

    pub fn discard_diff_focus(&mut self) {
        self.pending_diff_focus = None;
    }
}

#[derive(Default)]
pub(crate) struct ConversationUi {
    entries: HashMap<ConversationDestination, ConversationUiState>,
}

impl ConversationUi {
    pub fn ensure(
        &mut self,
        destination: ConversationDestination,
        diff_wrap: bool,
        terminal_open: bool,
        terminal_height: f32,
    ) {
        self.entries
            .entry(destination)
            .or_insert_with(|| ConversationUiState::new(diff_wrap, terminal_open, terminal_height));
    }

    pub fn get(&self, destination: &ConversationDestination) -> Option<&ConversationUiState> {
        self.entries.get(destination)
    }

    pub fn get_mut(
        &mut self,
        destination: &ConversationDestination,
    ) -> Option<&mut ConversationUiState> {
        self.entries.get_mut(destination)
    }

    pub fn get_by_key(&self, key: &str) -> Option<&ConversationUiState> {
        self.entries
            .iter()
            .find_map(|(destination, state)| (destination.ui_key() == key).then_some(state))
    }

    pub fn get_mut_by_key(&mut self, key: &str) -> Option<&mut ConversationUiState> {
        self.entries
            .iter_mut()
            .find_map(|(destination, state)| (destination.ui_key() == key).then_some(state))
    }

    pub fn remove(&mut self, destination: &ConversationDestination) {
        self.entries.remove(destination);
    }

    pub fn move_entry(&mut self, from: &ConversationDestination, to: ConversationDestination) {
        if let Some(state) = self.entries.remove(from) {
            self.entries.entry(to).or_insert(state);
        }
    }

    pub fn open_preview_for(&mut self, destination: ConversationDestination, diff_wrap: bool) {
        self.ensure(destination.clone(), diff_wrap, false, 240.);
        let state = self
            .get_mut(&destination)
            .expect("ensured conversation UI entry");
        state.right_panel_open = true;
        state.right_tab = RightTab::Preview;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn right_and_bottom_workspaces_follow_their_thread() {
        let first = ConversationDestination::Thread("first".into());
        let second = ConversationDestination::Thread("second".into());
        let mut conversations = ConversationUi::default();
        conversations.ensure(first.clone(), false, false, 240.);
        conversations.ensure(second.clone(), false, false, 240.);

        let first_ui = conversations.get_mut(&first).unwrap();
        first_ui.right_panel_open = true;
        first_ui.right_panel_expanded = true;
        first_ui.diff_selected_turn = Some(3);
        first_ui.right_tab = RightTab::Preview;
        first_ui.terminal_open = true;
        first_ui.terminal_height = 318.;

        let second_ui = conversations.get(&second).unwrap();
        assert!(
            !second_ui.right_panel_open,
            "another thread must start with its own right panel"
        );
        assert!(!second_ui.terminal_open);
        assert_eq!(second_ui.terminal_height, 240.);

        let second_ui = conversations.get_mut(&second).unwrap();
        second_ui.right_panel_open = true;
        second_ui.right_tab = RightTab::Plan;
        second_ui.terminal_height = 402.;

        let first_ui = conversations.get(&first).unwrap();
        assert!(first_ui.right_panel_open);
        assert!(first_ui.right_panel_expanded);
        assert_eq!(first_ui.diff_selected_turn, Some(3));
        assert_eq!(first_ui.right_tab, RightTab::Preview);
        assert!(first_ui.terminal_open);
        assert_eq!(first_ui.terminal_height, 318.);

        let second_ui = conversations.get(&second).unwrap();
        assert!(second_ui.right_panel_open);
        assert_eq!(second_ui.right_tab, RightTab::Plan);
        assert_eq!(second_ui.terminal_height, 402.);
    }

    #[test]
    fn diff_file_focus_request_is_consumed_once_and_discarded_on_scope_change() {
        let destination = ConversationDestination::Thread("session-a".into());
        let mut conversations = ConversationUi::default();
        conversations.ensure(destination.clone(), false, false, 240.);
        conversations
            .get_mut(&destination)
            .unwrap()
            .pending_diff_focus = Some(DiffFocus {
            session: "session-a".into(),
            turn: 3,
            path: "src/second.rs".into(),
        });
        let ui = conversations.get_mut(&destination).unwrap();
        assert_eq!(ui.take_diff_focus("session-a", 2), None);
        assert!(ui.pending_diff_focus.is_some());
        assert_eq!(
            ui.take_diff_focus("session-a", 3),
            Some(DiffFocus {
                session: "session-a".into(),
                turn: 3,
                path: "src/second.rs".into(),
            })
        );
        assert_eq!(ui.take_diff_focus("session-a", 3), None);
        ui.pending_diff_focus = Some(DiffFocus {
            session: "session-a".into(),
            turn: 3,
            path: "src/second.rs".into(),
        });
        ui.discard_diff_focus();
        assert!(ui.pending_diff_focus.is_none());
    }

    #[test]
    fn preview_open_for_background_thread_does_not_switch_the_active_thread() {
        let first = ConversationDestination::Thread("first".into());
        let second = ConversationDestination::Thread("second".into());
        let mut conversations = ConversationUi::default();
        conversations.ensure(first.clone(), false, false, 240.);
        conversations.ensure(second.clone(), false, false, 240.);
        let active = second.clone();
        conversations.open_preview_for(first.clone(), false);

        assert_eq!(active, second);
        assert!(!conversations.get(&active).unwrap().right_panel_open);
        assert!(conversations.get(&first).unwrap().right_panel_open);
        assert_eq!(
            conversations.get(&first).unwrap().right_tab,
            RightTab::Preview
        );
        assert!(
            conversations
                .get(&first)
                .is_some_and(|ui| ui.right_panel_open && ui.right_tab == RightTab::Preview)
        );
    }

    #[test]
    fn draft_workspace_uses_the_same_project_key_as_composer_text() {
        let draft = ConversationDestination::ProjectDraft("project-stable".into());
        let thread = ConversationDestination::Thread("session-2".into());
        let mut conversations = ConversationUi::default();
        conversations.ensure(draft.clone(), false, true, 355.);
        let ui = conversations.get_mut(&draft).unwrap();
        ui.right_panel_open = true;
        ui.right_tab = RightTab::Preview;

        assert_eq!(
            crate::composer::composer_destination(
                true,
                "transient-draft-session-1",
                Some("project-stable")
            ),
            Some(draft.clone())
        );
        assert_eq!(
            crate::composer::composer_destination(
                true,
                "transient-draft-session-2",
                Some("project-stable")
            ),
            Some(draft.clone())
        );
        conversations.move_entry(&draft, thread.clone());
        let ui = conversations.get(&thread).unwrap();
        assert!(ui.right_panel_open);
        assert_eq!(ui.right_tab, RightTab::Preview);
        assert!(ui.terminal_open);
        assert_eq!(ui.terminal_height, 355.);
        assert_eq!(thread.ui_key(), "session-2");
    }
}
