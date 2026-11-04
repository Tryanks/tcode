use crate::icon::{Icon, IconName};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::spinner::Spinner;
use agent::{PlanStep, PlanStepStatus};
use gpui::{
    AnyElement, App, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, ScrollHandle, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{InteractiveElementExt as _, h_flex, v_flex};

use crate::material;
use crate::sizing::Sizable as _;
use crate::store::WorkspaceStore;

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a row drawn here takes to fold in or out, or to move to a new place.
const ROWS: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

pub struct PlanPanel {
    store: Entity<WorkspaceStore>,
    session_id: Option<String>,
    vscroll: ScrollHandle,
    /// The explanation and steps last shown, drawn while an emptied plan
    /// fades out.
    last: (Option<String>, Vec<PlanStep>),
}

impl PlanPanel {
    pub fn new(store: Entity<WorkspaceStore>, session_id: Option<String>) -> Self {
        Self {
            store,
            session_id,
            vscroll: ScrollHandle::new(),
            last: (None, Vec::new()),
        }
    }

    pub(crate) fn set_session(&mut self, session_id: Option<String>, cx: &mut Context<Self>) {
        if self.session_id != session_id {
            self.session_id = session_id;
            cx.notify();
        }
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        material::empty_state(
            Icon::new(IconName::ListOrdered),
            crate::tr!("tasks.empty_title"),
            crate::tr!("tasks.empty_desc"),
            cx,
        )
        .into_any_element()
    }
}

impl Render for PlanPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let plan = self
            .store
            .read(cx)
            .session_plan_for(self.session_id.as_deref(), cx)
            .filter(|plan| !plan.steps.is_empty())
            .map(|plan| (explanation(plan), plan.steps.clone()));
        // The plan and the empty state cross-fade; a plan that empties
        // leaves as it last stood.
        let empty = plan.is_none();
        if let Some(plan) = plan {
            self.last = plan;
        }
        let fade =
            crate::motion::CrossFade::new("plan-panel", usize::from(!empty), 2, CHANGE, window, cx)
                .fill();
        let mut layers = Vec::new();
        if fade.shows(0) {
            layers.push((0, self.render_empty(cx)));
        }
        if fade.shows(1) {
            // A compact page is inset from the window edges; the right column keeps
            // its denser padding.
            let compact = crate::window_seam::window_is_compact(window, cx);
            let inset = if compact {
                material::COMPACT_PAGE_INSET
            } else {
                material::CARD_INSET
            };
            let (explanation, steps) = self.last.clone();
            let column = v_flex()
                .w_full()
                .min_w_0()
                .px(px(inset))
                .py_3()
                .children(render_explanation(
                    "plan-explanation",
                    explanation,
                    compact,
                    window,
                    cx,
                ))
                .child(
                    v_flex()
                        .w_full()
                        .gap_1()
                        .child(
                            div()
                                .pt_1()
                                .text_caption(cx)
                                .child(crate::tr!("tasks.steps")),
                        )
                        .child(material::rail_detail(
                            render_steps(
                                "plan-steps",
                                &steps,
                                compact,
                                StepLayout::Marked,
                                window,
                                cx,
                            ),
                            cx,
                        )),
                );
            layers.push((
                1,
                v_flex()
                    .size_full()
                    .child(crate::scroll::page_viewport(
                        "plan-scroll-bounce",
                        &self.vscroll,
                        div()
                            .id("plan-scroll")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .lock_scroll_axis()
                            .track_scroll(&self.vscroll)
                            .child(column),
                    ))
                    .into_any_element(),
            ));
        }
        v_flex().size_full().child(fade.render(layers).size_full())
    }
}

/// What the agent said of `plan` when it set it, when it said something.
pub(crate) fn explanation(plan: &tcode_protocol::SessionPlan) -> Option<String> {
    plan.explanation
        .as_deref()
        .map(str::trim)
        .filter(|explanation| !explanation.is_empty())
        .map(str::to_owned)
}

/// What the agent said of a plan, drawn above its steps: it folds in when
/// the agent first says something and away once a plan comes without it.
pub(crate) fn render_explanation(
    id: &'static str,
    explanation: Option<String>,
    compact: bool,
    window: &mut Window,
    cx: &mut App,
) -> Option<AnyElement> {
    let shown = crate::motion::shown(id, explanation, SHOW, window, cx)?;
    Some(
        shown.fold(
            div()
                .w_full()
                .pb(cx.theme().tokens.spacing.sm)
                .text_secondary(compact, cx)
                .text_color(cx.theme().muted_foreground)
                .child(shown.value.clone()),
        ),
    )
}

/// How a plan's steps say where each stands.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum StepLayout {
    /// A marker column before each step's text.
    Marked,
    /// No marker column: the text's colour says it, and the step under way
    /// ends with a small spinner.
    Dense,
}

/// A plan's steps, keyed by their text: a plan the agent rewrites folds the
/// steps it dropped away as they stood and folds new ones in, and the steps
/// it kept glide to their new places, carrying their state.
pub(crate) fn render_steps(
    id: &'static str,
    steps: &[PlanStep],
    compact: bool,
    layout: StepLayout,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let mut seen = std::collections::HashMap::<&str, usize>::new();
    let keyed = steps.iter().map(|step| {
        let repeat = seen.entry(step.step.as_str()).or_default();
        let key = SharedString::from(format!("{repeat}:{}", step.step));
        *repeat += 1;
        (key, step.clone())
    });
    crate::motion::keyed(
        id,
        keyed,
        ROWS,
        window,
        cx,
        |key, step, _, cx| match layout {
            StepLayout::Marked => render_step(key, step, compact, cx),
            StepLayout::Dense => render_dense_step(key, step, compact, cx),
        },
    )
    .render(v_flex().w_full().min_w_0().gap_1())
}

fn render_step(key: &SharedString, step: &PlanStep, compact: bool, cx: &App) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let foreground = cx.theme().foreground;
    let primary = cx.theme().primary;
    let status = step.status;
    // The marker cross-fades as the step moves on, and a finished step's
    // text fades to muted as it is struck through.
    let marker_id = SharedString::from(format!("plan-step-marker-{key}"));
    let text_id = SharedString::from(format!("plan-step-text-{key}"));
    let marker = crate::motion::Sampled::new(move |window, cx| {
        crate::motion::cross_fade_part(
            marker_id,
            status,
            CHANGE,
            window,
            cx,
            move |status, _, _| match status {
                PlanStepStatus::Completed => Icon::new(IconName::CircleCheck)
                    .text_color(muted)
                    .into_any_element(),
                PlanStepStatus::InProgress => Spinner::new().color(primary).into_any_element(),
                PlanStepStatus::Pending => Icon::new(IconName::CircleDot)
                    .text_color(muted)
                    .into_any_element(),
            },
        )
    });
    let completed = status == PlanStepStatus::Completed;
    let label = step.step.clone();
    let text = crate::motion::Sampled::new(move |window, cx| {
        let color = gpui_base::motion::transition(
            text_id,
            if completed { muted } else { foreground },
            gpui_base::motion::Transition::new(GLIDE),
            window,
            cx,
        );
        div()
            .flex_1()
            .min_w_0()
            .text_color(color)
            .when(completed, |text| text.line_through())
            .child(label)
            .into_any_element()
    });

    let key = key.clone();
    h_flex()
        .id(SharedString::from(format!("plan-step-{key}")))
        .debug_selector(move || format!("plan-step-{key}"))
        .w_full()
        .py_1()
        .gap_2()
        .items_start()
        .text_body(compact, cx)
        .child(
            // One body line tall, so the marker centres on the first line.
            h_flex()
                .flex_none()
                .h(if compact {
                    cx.theme().tokens.typography.md.line_height
                } else {
                    cx.theme().tokens.typography.sm.line_height
                })
                .child(marker),
        )
        .child(text)
        .into_any_element()
}

/// A step of [`StepLayout::Dense`]: its text from the left edge, muted and
/// struck through once done, in the primary colour while under way, with
/// a small spinner at the right that folds in and away with it.
fn render_dense_step(key: &SharedString, step: &PlanStep, compact: bool, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let (muted, foreground, primary) = (theme.muted_foreground, theme.foreground, theme.primary);
    let status = step.status;
    let completed = status == PlanStepStatus::Completed;
    let text_id = SharedString::from(format!("plan-step-text-{key}"));
    let label = step.step.clone();
    let text = crate::motion::Sampled::new(move |window, cx| {
        let color = gpui_base::motion::transition(
            text_id,
            match status {
                PlanStepStatus::Completed => muted,
                PlanStepStatus::InProgress => primary,
                PlanStepStatus::Pending => foreground,
            },
            gpui_base::motion::Transition::new(GLIDE),
            window,
            cx,
        );
        div()
            .flex_1()
            .min_w_0()
            .text_color(color)
            .when(completed, |text| text.line_through())
            .child(label)
            .into_any_element()
    });
    let spinner_id = SharedString::from(format!("plan-step-spinner-{key}"));
    let line_height = if compact {
        cx.theme().tokens.typography.md.line_height
    } else {
        cx.theme().tokens.typography.sm.line_height
    };
    let spinner = crate::motion::Sampled::new(move |window, cx| {
        let running = (status == PlanStepStatus::InProgress).then_some(());
        crate::motion::shown(spinner_id, running, CHANGE, window, cx)
            .map(|shown| {
                shown.fold_across(
                    // One body line tall, so the spinner centres on the
                    // first line.
                    h_flex().h(line_height).pl_2().child(
                        Spinner::new()
                            .color(primary)
                            .with_size(crate::sizing::Size::XSmall),
                    ),
                )
            })
            .unwrap_or_else(|| div().into_any_element())
    });

    let key = key.clone();
    h_flex()
        .id(SharedString::from(format!("plan-step-{key}")))
        .debug_selector(move || format!("plan-step-{key}"))
        .w_full()
        .py_1()
        .items_start()
        .text_body(compact, cx)
        .child(text)
        .child(spinner)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use agent::{PlanStep, PlanStepStatus};
    use gpui::{AppContext as _, TestAppContext, VisualTestContext, px, size};
    use tcode_protocol::{EventEnvelope, HostMessage, ServerEvent, Topic, encode_line};

    use super::PlanPanel;
    use crate::store::WorkspaceStore;
    use crate::test_frames::{draw, draw_settled};

    /// A plan the agent rewrites keeps the steps it kept: one dropped folds
    /// away as it stood while the step below it glides up, and one added
    /// folds in and pushes it back down, over several frames; with motion
    /// reduced each change lands at once.
    #[gpui::test]
    fn a_rewritten_plan_folds_its_steps_in_and_out(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let (to_host, _outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let send = |topic, event| {
            incoming
                .try_send(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic,
                        event,
                    }))
                    .unwrap(),
                )
                .unwrap()
        };
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump_link = link.clone();
        let executor = cx.background_executor.clone();
        let _pump = cx.background_executor.spawn(async move {
            pump_link
                .pump_with_timer(|delay| executor.timer(delay))
                .await;
        });
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &incoming, Vec::new(), cx);
        store.update(cx, |store, cx| store.show_alone("thread".into(), cx));
        let plan = |steps: &[(&str, PlanStepStatus)]| {
            send(
                Topic::SessionPlan {
                    session_id: "thread".into(),
                },
                ServerEvent::SessionPlanReplaced(tcode_protocol::SessionPlan {
                    session_id: "thread".into(),
                    steps: steps
                        .iter()
                        .map(|(step, status)| PlanStep {
                            step: step.to_string(),
                            status: *status,
                        })
                        .collect(),
                    explanation: None,
                }),
            )
        };
        // A step in progress shows a spinner, which never rests.
        use PlanStepStatus::{Completed, Pending};
        plan(&[("Read", Completed), ("Plan", Pending), ("Build", Pending)]);
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        let (_, cx) =
            cx.add_window_view(|_, _| PlanPanel::new(store.clone(), Some("thread".to_owned())));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(360.), px(600.)));
        draw_settled(cx);
        let step = |key: &str, cx: &mut VisualTestContext| {
            cx.debug_bounds(format!("plan-step-0:{key}").leak())
        };
        assert!(step("Build", cx).is_some(), "the plan's steps show");

        for reduce in [false, true] {
            cx.update(|_, cx| cx.set_reduce_motion(reduce));
            for (steps, plan_stays) in [
                (&[("Read", Completed), ("Build", Pending)][..], false),
                (
                    &[("Read", Completed), ("Plan", Completed), ("Build", Pending)][..],
                    true,
                ),
            ] {
                plan(steps);
                cx.run_until_parked();
                store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
                draw(cx);
                let mut frames = Vec::new();
                for _ in 0..20 {
                    frames.push((
                        step("Plan", cx).is_some(),
                        step("Build", cx).expect("Build stays").top(),
                    ));
                    cx.executor().advance_clock(Duration::from_millis(16));
                    cx.update(|window, cx| window.simulate_next_frame(cx));
                    draw(cx);
                }
                draw_settled(cx);
                assert_eq!(step("Plan", cx).is_some(), plan_stays);
                let rest = step("Build", cx).unwrap().top();
                let tops: Vec<_> = frames.iter().map(|(_, top)| *top).collect();
                let between = tops
                    .iter()
                    .filter(|top| **top != tops[0] && **top != rest)
                    .count();
                let drawn = frames.iter().filter(|(shown, _)| *shown).count();
                if reduce {
                    assert_eq!(tops[0], rest, "Build glided with motion reduced: {tops:?}");
                    assert!(plan_stays || drawn == 0, "Plan lingered");
                } else {
                    assert!(between >= 3, "Build jumped: {tops:?}");
                    assert!(plan_stays || drawn >= 3, "Plan went at once");
                }
            }
        }
    }
}
