use super::super::*;
use crate::widgets::menu::{ContextMenuExt as _, CopyText};
use agent::{Goal, GoalStatus};

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

/// The word a goal's status reads as.
pub(crate) fn status_label(status: GoalStatus) -> String {
    match status {
        GoalStatus::Active => crate::tr!("composer.goal_active"),
        GoalStatus::Paused => crate::tr!("composer.goal_paused"),
        GoalStatus::Blocked => crate::tr!("composer.goal_blocked"),
        GoalStatus::UsageLimited => crate::tr!("composer.goal_usage_limited"),
        GoalStatus::BudgetLimited => crate::tr!("composer.goal_budget_limited"),
        GoalStatus::Complete => crate::tr!("composer.goal_complete"),
    }
    .into_owned()
}

/// The glyph and colour a goal's status shows in.
fn status_mark(status: GoalStatus, cx: &App) -> (IconName, Hsla) {
    let theme = cx.theme();
    match status {
        GoalStatus::Active => (IconName::CircleDot, theme.primary),
        GoalStatus::Paused => (IconName::Pause, theme.muted_foreground),
        GoalStatus::Complete => (IconName::CircleCheck, theme.success),
        GoalStatus::Blocked | GoalStatus::UsageLimited | GoalStatus::BudgetLimited => {
            (IconName::TriangleAlert, theme.warning)
        }
    }
}

/// Time, tokens and checks, whichever the harness reports.
pub(crate) fn usage_line(goal: &Goal) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(secs) = goal.time_used_secs.filter(|secs| *secs > 0) {
        parts.push(crate::chat::model::format_elapsed_seconds(secs * 1000));
    }
    match (goal.tokens_used, goal.token_budget) {
        (used, Some(budget)) => parts.push(
            crate::tr!(
                "composer.goal_tokens_of_budget",
                used = crate::context_meter::format_tokens(Some(used.unwrap_or(0))),
                budget = crate::context_meter::format_tokens(Some(budget))
            )
            .into_owned(),
        ),
        (Some(used), None) if used > 0 => parts.push(
            crate::tr!(
                "composer.goal_tokens",
                used = crate::context_meter::format_tokens(Some(used))
            )
            .into_owned(),
        ),
        _ => {}
    }
    if let Some(checks) = goal.checks.filter(|checks| *checks > 0) {
        parts.push(crate::tr!("composer.goal_checks", count = checks).into_owned());
    }
    parts.join(" · ")
}

impl Composer {
    /// The harness's standing goal: what it works toward, how far it got, and
    /// the pause, edit and clear controls the harness offers.
    ///
    /// A new status cross-fades its glyph and glides its colour; the pause
    /// control folds in and away with the statuses that offer it, and the
    /// last check's reason folds in under the objective.
    pub(in super::super) fn render_goal_strip(
        &self,
        goal: &Goal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = self.composer_state(cx);
        let goal = goal.clone();
        let control = state.provider.and_then(|provider| provider.caps().goals);
        let enabled = self.interactive(cx) && goal_offered(&state).is_some();
        let (icon, color) = status_mark(goal.status, cx);
        let theme = cx.theme();
        let pause = match goal.status {
            GoalStatus::Active => Some((GoalStatus::Paused, "composer.goal_pause")),
            GoalStatus::Paused | GoalStatus::Blocked | GoalStatus::UsageLimited => {
                Some((GoalStatus::Active, "composer.goal_resume"))
            }
            GoalStatus::BudgetLimited | GoalStatus::Complete => None,
        }
        .filter(|_| control.is_some_and(|control| control.pause));
        let usage = usage_line(&goal);
        let foreground = theme.foreground;
        let touch = self.compact;
        let last_check = self
            .motion
            .shown(
                self.motion_key("goal-last-check"),
                goal.last_check
                    .clone()
                    .filter(|_| goal.status != GoalStatus::Complete),
                SHOW,
                window,
                cx,
            )
            .map(|shown| {
                shown.fold(
                    div()
                        .w_full()
                        .pt_0p5()
                        .line_clamp(2)
                        .text_secondary(touch, cx)
                        .child(crate::tr!(
                            "composer.goal_last_check",
                            reason = shown.value.as_str()
                        )),
                )
            });
        let objective = goal.objective.clone();
        let status_icon = crate::motion::status_icon(
            self.motion_id("goal-status"),
            icon,
            color,
            CHANGE,
            window,
            cx,
        );
        let pause = self
            .motion
            .shown(self.motion_key("goal-pause"), pause, SHOW, window, cx)
            .map(|shown| {
                let (status, label) = shown.value;
                let icon = if status == GoalStatus::Paused {
                    IconName::Pause
                } else {
                    IconName::Play
                };
                shown.fold_across(
                    Button::new("goal-pause")
                        .motion(&self.button_motion())
                        .debug_selector(|| "goal-pause".into())
                        .ghost()
                        .xsmall()
                        .touch(touch)
                        .icon(icon)
                        .disabled(!enabled)
                        .tooltip(crate::tr!(label))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.workspace_store.update(cx, |store, _cx| {
                                store.set_goal_for(
                                    this.session_id.clone().unwrap_or_default(),
                                    None,
                                    Some(status),
                                )
                            });
                        })),
                )
            });

        let header = h_flex()
            .w_full()
            .gap_1()
            .items_center()
            .text_secondary(touch, cx)
            .child(status_icon)
            .child(div().flex_none().child(crate::tr!(
                "composer.goal_header",
                status = status_label(goal.status)
            )))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .child(crate::widgets::fade_text(usage)),
            )
            .children(pause)
            .child(
                Button::new("goal-edit")
                    .motion(&self.button_motion())
                    .debug_selector(|| "goal-edit".into())
                    .ghost()
                    .xsmall()
                    .touch(touch)
                    .icon(IconName::Pencil)
                    .disabled(!enabled)
                    .tooltip(crate::tr!("composer.goal_edit"))
                    .on_click(cx.listener({
                        let objective = objective.clone();
                        move |this, _, window, cx| this.edit_goal(&objective, window, cx)
                    })),
            )
            .child(
                Button::new("goal-clear")
                    .motion(&self.button_motion())
                    .debug_selector(|| "goal-clear".into())
                    .ghost()
                    .xsmall()
                    .touch(touch)
                    .icon(IconName::Close)
                    .disabled(!enabled)
                    .tooltip(crate::tr!("composer.goal_clear"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.workspace_store.update(cx, |store, _cx| {
                            store.clear_goal_for(this.session_id.clone().unwrap_or_default())
                        });
                    })),
            );

        v_flex()
            .id("goal-strip")
            .w_full()
            .px_1()
            .child(header)
            .child(
                div()
                    .w_full()
                    .pt_0p5()
                    .line_clamp(3)
                    .text_body(touch, cx)
                    .text_color(foreground)
                    .child(objective.clone()),
            )
            .children(last_check)
            .context_menu(move |menu, _, _| {
                menu.menu(
                    crate::tr!("chat.copy_text").into_owned(),
                    Box::new(CopyText(objective.clone())),
                )
            })
            .into_any_element()
    }

    /// Put `/goal <objective>` in the composer with the objective selected.
    pub(in super::super) fn edit_goal(
        &mut self,
        objective: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = format!("/goal {objective}");
        let selection = "/goal ".len()..text.len();
        self.input.update(cx, |state, cx| {
            state.replace_all(text, window, cx);
            state.set_selected_range(selection, cx);
            if !crate::window_seam::is_mobile(cx) {
                state.focus(window, cx);
            }
        });
        self.recompute_trigger(cx);
        cx.notify();
    }
}
