//! The thread's plan, as the host reports it for this thread.

use std::cell::RefCell;
use std::rc::Rc;

use agent::{PlanStep, PlanStepStatus};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, Context, IntoElement as _, ParentElement as _, SharedString, Styled as _,
    Window,
};
use gpui_base::v_flex;
use tcode_protocol::SessionPlan;

use super::RailView;

fn plan<'a>(rail: &RailView, cx: &'a App) -> Option<&'a SessionPlan> {
    rail.workspace_store
        .read(cx)
        .session_plan_for(rail.session_id.as_deref(), cx)
        .filter(|plan| crate::plan_panel::shown(plan))
}

fn steps<'a>(rail: &RailView, cx: &'a App) -> Option<&'a [PlanStep]> {
    plan(rail, cx)
        .map(|plan| plan.steps.as_slice())
        .filter(|steps| !steps.is_empty())
}

/// Whether the card has something to show for `rail`'s thread now.
pub(super) fn present(rail: &RailView, cx: &App) -> bool {
    plan(rail, cx).is_some()
}

/// The line the card's header shows beside its title: how far
/// through the plan the agent is, and the step it is on.
pub(super) fn summary(rail: &RailView, cx: &App) -> Option<SharedString> {
    let steps = steps(rail, cx)?;
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

/// What the card shows below its header, laid out at the rail's width: the
/// plan's steps without the Plan tab's marker column, what the agent said
/// of it over them. A plan that empties while the card leaves is drawn as
/// it last stood.
pub(super) fn body(
    rail: &RailView,
    window: &mut Window,
    cx: &mut Context<RailView>,
) -> Option<AnyElement> {
    let last = rail
        .motion
        .keep::<Rc<RefCell<(Option<String>, Vec<PlanStep>)>>>("rail-plan-steps");
    if let Some(plan) = plan(rail, cx) {
        last.replace((crate::plan_panel::explanation(plan), plan.steps.clone()));
    }
    let (explanation, steps) = last.borrow().clone();
    if steps.is_empty() && explanation.is_none() {
        return None;
    }
    Some(
        v_flex()
            .w_full()
            .min_w_0()
            .children(crate::plan_panel::render_explanation(
                "rail-plan-explanation",
                explanation,
                false,
                window,
                cx,
            ))
            .when(!steps.is_empty(), |body| {
                body.child(crate::plan_panel::render_steps(
                    "rail-plan-steps",
                    &steps,
                    false,
                    crate::plan_panel::StepLayout::Dense,
                    window,
                    cx,
                ))
            })
            .into_any_element(),
    )
}
