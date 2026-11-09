//! The harness's standing goal for the thread, as it reports it.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use agent::{Goal, GoalStatus};
use gpui::{
    AnyElement, App, Context, IntoElement as _, ParentElement as _, SharedString, Styled as _,
    Window, div,
};
use gpui_base::v_flex;

use super::RailView;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;

/// How long a part of the card takes to cross-fade to another of its
/// states, or to fold in or away.
const CHANGE: Duration = Duration::from_millis(200);

fn goal<'a>(rail: &RailView, cx: &'a App) -> Option<&'a Goal> {
    rail.workspace_store
        .read(cx)
        .session_status_for(rail.session_id.as_deref(), cx)?
        .goal
        .as_ref()
}

/// Whether the card has something to show for `rail`'s thread now.
pub(super) fn present(rail: &RailView, cx: &App) -> bool {
    goal(rail, cx).is_some()
}

/// The line the card's header shows beside its title: the
/// goal's status, then the time, tokens and checks it has taken.
pub(super) fn summary(rail: &RailView, cx: &App) -> Option<SharedString> {
    let goal = goal(rail, cx)?;
    let status = crate::composer::goal_status_label(goal.status);
    let usage = crate::composer::goal_usage_line(goal);
    Some(
        if usage.is_empty() {
            status
        } else {
            format!("{status} · {usage}")
        }
        .into(),
    )
}

/// What the card shows below its header, laid out at the rail's width: the
/// objective, and the latest check's verdict while the goal is not met; the
/// header's summary says its status. A goal cleared while the card leaves is drawn as it stood.
pub(super) fn body(
    rail: &RailView,
    window: &mut Window,
    cx: &mut Context<RailView>,
) -> Option<AnyElement> {
    let last = rail.motion.keep::<Rc<RefCell<Option<Goal>>>>("rail-goal");
    if let Some(goal) = goal(rail, cx) {
        last.replace(Some(goal.clone()));
    }
    let goal = last.borrow().clone()?;
    let verdict = rail
        .motion
        .shown(
            "rail-goal-last-check",
            goal.last_check
                .clone()
                .filter(|_| goal.status != GoalStatus::Complete),
            CHANGE,
            window,
            cx,
        )
        .map(|shown| {
            let reason = shown.value.clone();
            shown.fold(
                div()
                    .w_full()
                    .pt_1()
                    .line_clamp(3)
                    .text_secondary(false, cx)
                    .child(crate::tr!(
                        "composer.goal_last_check",
                        reason = reason.as_str()
                    )),
            )
        });
    let objective = rail.motion.cross_fade_part(
        "rail-goal-objective",
        goal.objective.clone(),
        CHANGE,
        window,
        cx,
        |objective, _, cx| {
            div()
                .w_full()
                .line_clamp(4)
                .text_body(false, cx)
                .text_color(cx.theme().foreground)
                .child(objective.clone())
                .into_any_element()
        },
    );
    Some(
        v_flex()
            .w_full()
            .min_w_0()
            .px_1()
            .child(objective)
            .children(verdict)
            .into_any_element(),
    )
}
