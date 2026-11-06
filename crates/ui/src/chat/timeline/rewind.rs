//! The rewind panel a message opens, and the undo its divider offers.
use super::*;
use crate::chat::components::rewind::{Panel, PanelActions, PreviewState};
use agent::{
    RewindFailure, RewindFailureReason, RewindMode, RewindPreviewKind, RewindPreviewResult,
};

/// The open panel: the message it rewinds to, the scope chosen, and what the
/// Harness said each file scope would restore.
pub(in crate::chat) struct RewindPanelState {
    /// The message's entry id.
    pub(in crate::chat) key: String,
    turn: usize,
    steer: Option<String>,
    selected: Option<RewindMode>,
    previews: HashMap<RewindMode, PreviewState>,
    files_expanded: bool,
    _previews: Vec<Task<()>>,
}

impl TimelineView {
    /// Open the panel for the message `key`, the prompt that opened `turn`
    /// or the steer `steer` in it, and ask the Harness what each file scope
    /// would restore where it can tell.
    pub(in crate::chat) fn open_rewind(
        &mut self,
        key: String,
        turn: usize,
        steer: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(session_id) = self.session_id.clone() else {
            return;
        };
        let Some(target) = self.workspace_store.read(cx).chat_rewind_target_for(
            Some(&session_id),
            turn,
            steer.as_deref(),
            cx,
        ) else {
            return;
        };
        let mut previews = HashMap::new();
        let mut tasks = Vec::new();
        if target.rewind.preview != RewindPreviewKind::None {
            for mode in target
                .modes
                .iter()
                .copied()
                .filter(|mode| mode.includes_files())
            {
                previews.insert(mode, PreviewState::Checking);
                let preview = self.workspace_store.update(cx, |store, cx| {
                    store.preview_rewind_for(session_id.clone(), turn, steer.clone(), mode, cx)
                });
                let key = key.clone();
                tasks.push(cx.spawn(async move |this, cx| {
                    let state = match preview.await {
                        Ok(RewindPreviewResult::Ready(preview)) => PreviewState::Ready(preview),
                        Ok(RewindPreviewResult::Refused(failure)) => PreviewState::Refused(failure),
                        Err(detail) => PreviewState::Refused(RewindFailure::new(
                            RewindFailureReason::Other,
                            detail,
                        )),
                    };
                    let _ = this.update(cx, |this, cx| {
                        if let Some(panel) =
                            this.rewind_panel.as_mut().filter(|panel| panel.key == key)
                        {
                            panel.previews.insert(mode, state);
                            cx.notify();
                        }
                    });
                }));
            }
        }
        self.rewind_panel = Some(RewindPanelState {
            key,
            turn,
            steer,
            selected: None,
            previews,
            files_expanded: false,
            _previews: tasks,
        });
        cx.notify();
    }

    pub(in crate::chat) fn close_rewind(&mut self, cx: &mut Context<Self>) {
        if self.rewind_panel.take().is_some() {
            cx.notify();
        }
    }

    /// What the open panel for `key` shows, while it is open and its message
    /// is still a target.
    pub(in crate::chat) fn rewind_panel_for(&self, key: &str, cx: &App) -> Option<Panel> {
        let state = self
            .rewind_panel
            .as_ref()
            .filter(|state| state.key == key)?;
        let target = self.workspace_store.read(cx).chat_rewind_target_for(
            self.session_id.as_deref(),
            state.turn,
            state.steer.as_deref(),
            cx,
        )?;
        Some(panel_from(state, target))
    }

    fn select_rewind_mode(&mut self, mode: RewindMode, cx: &mut Context<Self>) {
        if let Some(panel) = self.rewind_panel.as_mut() {
            panel.selected = Some(mode);
            cx.notify();
        }
    }

    fn toggle_rewind_files(&mut self, cx: &mut Context<Self>) {
        if let Some(panel) = self.rewind_panel.as_mut() {
            panel.files_expanded = !panel.files_expanded;
            cx.notify();
        }
    }

    /// Ask the host to make the rewind the panel shows, and close it.
    fn confirm_rewind(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.rewind_panel.take() else {
            return;
        };
        let Some(session_id) = self.session_id.clone() else {
            return;
        };
        let Some(target) = self.workspace_store.read(cx).chat_rewind_target_for(
            Some(&session_id),
            state.turn,
            state.steer.as_deref(),
            cx,
        ) else {
            cx.notify();
            return;
        };
        let mode = panel_from(&state, target).selected;
        self.workspace_store.update(cx, |store, _| {
            store.rewind_turn_for(session_id, state.turn, state.steer, mode)
        });
        cx.notify();
    }

    pub(in crate::chat) fn undo_rewind(&mut self, checkpoint_id: String, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_id.clone() else {
            return;
        };
        self.workspace_store.update(cx, |store, _| {
            store.undo_rewind_for(session_id, checkpoint_id)
        });
    }

    /// The panel's handlers, which reach this timeline from the popover.
    pub(in crate::chat) fn rewind_panel_actions(&self, cx: &mut Context<Self>) -> PanelActions {
        let this = cx.entity().downgrade();
        PanelActions {
            select: Rc::new({
                let this = this.clone();
                move |mode, _, cx| {
                    let _ = this.update(cx, |timeline, cx| timeline.select_rewind_mode(mode, cx));
                }
            }),
            toggle_files: Rc::new({
                let this = this.clone();
                move |_, cx| {
                    let _ = this.update(cx, |timeline, cx| timeline.toggle_rewind_files(cx));
                }
            }),
            confirm: Rc::new({
                let this = this.clone();
                move |_, cx| {
                    let _ = this.update(cx, |timeline, cx| timeline.confirm_rewind(cx));
                }
            }),
            cancel: Rc::new(move |_, cx| {
                let _ = this.update(cx, |timeline, cx| timeline.close_rewind(cx));
            }),
        }
    }
}

/// The panel as the open state and the target make it: file scopes go once
/// the Harness found no files for them, unless nothing else would be left.
fn panel_from(state: &RewindPanelState, target: crate::store::RewindTarget) -> Panel {
    let offered_files = target.modes.iter().any(|mode| mode.includes_files());
    let mut choices: Vec<RewindMode> = target
        .modes
        .iter()
        .copied()
        .filter(|mode| {
            !mode.includes_files()
                || !state
                    .previews
                    .get(mode)
                    .is_some_and(PreviewState::finds_no_files)
        })
        .collect();
    if choices.is_empty() {
        choices = target.modes.clone();
    }
    let no_files = offered_files && !choices.iter().any(|mode| mode.includes_files());
    let selected = state
        .selected
        .filter(|mode| choices.contains(mode))
        .unwrap_or(choices[0]);
    let preview = (target.rewind.preview != RewindPreviewKind::None && selected.includes_files())
        .then(|| {
            state
                .previews
                .get(&selected)
                .cloned()
                .unwrap_or(PreviewState::Checking)
        });
    Panel {
        steer: state.steer.is_some(),
        choices,
        selected,
        preview,
        no_files,
        files_expanded: state.files_expanded,
        target,
    }
}
