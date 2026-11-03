//! The thread's plan, as the host reports it for this thread.

use std::cell::RefCell;
use std::rc::Rc;

use agent::{PlanStep, PlanStepStatus};
use gpui::{
    AnyElement, App, Context, IntoElement as _, ParentElement as _, SharedString, Styled as _,
    Window, div,
};
use gpui_base::v_flex;
use tcode_protocol::SessionPlan;

use crate::chat::ChatView;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;

/// How long the plan's explanation takes to fold in or out.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

fn plan<'a>(chat: &ChatView, cx: &'a App) -> Option<&'a SessionPlan> {
    chat.workspace_store
        .read(cx)
        .session_plan_for(chat.session_id.as_deref())
        .filter(|plan| !plan.steps.is_empty())
}

fn steps<'a>(chat: &ChatView, cx: &'a App) -> Option<&'a [PlanStep]> {
    plan(chat, cx).map(|plan| plan.steps.as_slice())
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

/// What the card shows open, laid out at the rail's width: what the agent
/// said of the plan, when it said something, over the plan's steps, drawn
/// as the Plan tab draws them. A plan that empties while the card leaves is
/// drawn as it last stood.
pub(super) fn body(
    chat: &ChatView,
    window: &mut Window,
    cx: &mut Context<ChatView>,
) -> Option<AnyElement> {
    let last = chat
        .motion
        .keep::<Rc<RefCell<(Option<String>, Vec<PlanStep>)>>>("rail-plan-steps");
    if let Some(plan) = plan(chat, cx) {
        let explanation = plan
            .explanation
            .as_deref()
            .map(str::trim)
            .filter(|explanation| !explanation.is_empty())
            .map(str::to_owned);
        last.replace((explanation, plan.steps.clone()));
    }
    let (explanation, steps) = last.borrow().clone();
    if steps.is_empty() {
        return None;
    }
    let explanation = chat
        .motion
        .shown("rail-plan-explanation", explanation, SHOW, window, cx)
        .map(|shown| {
            shown.fold(
                div()
                    .w_full()
                    .pb(cx.theme().tokens.spacing.sm)
                    .text_secondary(false, cx)
                    .text_color(cx.theme().muted_foreground)
                    .child(shown.value.clone()),
            )
        });
    Some(
        v_flex()
            .w_full()
            .min_w_0()
            .children(explanation)
            .child(crate::plan_panel::render_steps(
                "rail-plan-steps",
                &steps,
                false,
                window,
                cx,
            ))
            .into_any_element(),
    )
}
