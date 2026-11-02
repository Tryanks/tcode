use crate::icon::{Icon, IconName};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::spinner::Spinner;
use agent::{PlanStep, PlanStepStatus};
use gpui::{
    AnyElement, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    ScrollHandle, StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{InteractiveElementExt as _, h_flex, v_flex};

use crate::material;
use crate::store::{TopicKind, WorkspaceStore, observe_store_topics};

pub struct PlanPanel {
    store: Entity<WorkspaceStore>,
    vscroll: ScrollHandle,
    /// The steps last shown, drawn while an emptied list fades out.
    last_steps: Vec<PlanStep>,
    _subscriptions: Vec<Subscription>,
}

impl PlanPanel {
    pub fn new(store: Entity<WorkspaceStore>, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![observe_store_topics(
            &store,
            &[
                TopicKind::ActiveSession,
                TopicKind::SessionStatus,
                TopicKind::SessionPlan,
                TopicKind::SessionEvents,
            ],
            cx,
        )];
        Self {
            store,
            vscroll: ScrollHandle::new(),
            last_steps: Vec::new(),
            _subscriptions: subscriptions,
        }
    }

    fn render_steps(
        &self,
        steps: &[PlanStep],
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut steps_col = v_flex().w_full().min_w_0().gap_1();
        for (index, step) in steps.iter().enumerate() {
            steps_col = steps_col.child(self.render_step(index, step, compact, cx));
        }
        let steps_col = material::rail_detail(steps_col, cx);
        v_flex()
            .w_full()
            .gap_1()
            .child(
                div()
                    .pt_1()
                    .text_caption(cx)
                    .child(crate::tr!("tasks.steps")),
            )
            .child(steps_col)
            .into_any_element()
    }

    fn render_step(
        &self,
        index: usize,
        step: &PlanStep,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let foreground = cx.theme().foreground;
        let primary = cx.theme().primary;
        let status = step.status;
        // The marker cross-fades as the step moves on, and a finished step's
        // text fades to muted as it is struck through.
        let marker = crate::motion::Sampled::new(move |window, cx| {
            crate::motion::cross_fade_part(
                ("plan-step-marker", index),
                status,
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
                gpui::ElementId::from(("plan-step-text", index)),
                if completed { muted } else { foreground },
                crate::motion::settle(),
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

        h_flex()
            .id(("plan-step", index))
            .debug_selector(move || format!("plan-step-{index}"))
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
        let steps = self
            .store
            .read(cx)
            .session_plan()
            .map(|plan| plan.steps.clone())
            .unwrap_or_default();
        // The step list and the empty state cross-fade; a list that empties
        // leaves as it last stood.
        let empty = steps.is_empty();
        if !empty {
            self.last_steps = steps;
        }
        let fade =
            crate::motion::CrossFade::new("plan-panel", usize::from(!empty), 2, window, cx).fill();
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
            let steps = self.last_steps.clone();
            let column = v_flex()
                .w_full()
                .min_w_0()
                .px(px(inset))
                .py_3()
                .gap_3()
                .child(self.render_steps(&steps, compact, cx));
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
