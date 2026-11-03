//! The thread's plan, as the host reports it for this thread.

use std::cell::RefCell;
use std::rc::Rc;

use agent::{PlanStep, PlanStepStatus};
use gpui::{AnyElement, App, Context, SharedString, Window};

use crate::chat::ChatView;

fn steps<'a>(chat: &ChatView, cx: &'a App) -> Option<&'a [PlanStep]> {
    chat.workspace_store
        .read(cx)
        .session_plan_for(chat.session_id.as_deref())
        .map(|plan| plan.steps.as_slice())
        .filter(|steps| !steps.is_empty())
}

/// Whether the card has something to show for `chat`'s thread now.
pub(super) fn present(chat: &ChatView, cx: &App) -> bool {
    steps(chat, cx).is_some()
}

/// The line the card's header shows beside its title, open or shut: how far
/// through the plan the agent is, and the step it is on.
pub(super) fn summary(chat: &ChatView, cx: &App) -> Option<SharedString> {
    let steps = steps(chat, cx)?;
    let done = steps
        .iter()
        .filter(|step| step.status == PlanStepStatus::Completed)
        .count();
    let progress = crate::tr!("tasks.progress", done = done, total = steps.len());
    Some(
        match steps
            .iter()
            .find(|step| step.status == PlanStepStatus::InProgress)
        {
            Some(step) => format!("{progress} · {}", step.step),
            None => progress.into_owned(),
        }
        .into(),
    )
}

/// What the card shows open, laid out at the rail's width: the plan's
/// steps, drawn as the Plan tab draws them. A plan that empties while the
/// card leaves is drawn as it last stood.
pub(super) fn body(
    chat: &ChatView,
    window: &mut Window,
    cx: &mut Context<ChatView>,
) -> Option<AnyElement> {
    let last = chat
        .motion
        .keep::<Rc<RefCell<Vec<PlanStep>>>>("rail-plan-steps");
    if let Some(steps) = steps(chat, cx) {
        last.replace(steps.to_vec());
    }
    let steps = last.borrow().clone();
    (!steps.is_empty())
        .then(|| crate::plan_panel::render_steps("rail-plan-steps", &steps, false, window, cx))
}
