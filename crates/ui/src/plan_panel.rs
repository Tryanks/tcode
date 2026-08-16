//! The right-panel "Plan / Tasks" tab: the captured proposed plan (with its
//! Copy / Download / Save actions) plus the latest structured plan steps
//! (S1 §6). Hosted alongside the diff view.

use std::time::Duration;

use crate::theme::ActiveTheme as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::spinner::Spinner;
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use agent::{PlanStep, PlanStepStatus};
use gpui::{
    AnyElement, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, ScrollHandle, StatefulInteractiveElement as _, Styled as _,
    Subscription, Task, Window, div, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};

use tcode_core::session::plan_title;
use tcode_protocol::Command;

use crate::markdown::{MarkdownState, MarkdownView};
use crate::material;
use crate::store::WorkspaceStore;

pub struct PlanPanel {
    store: Entity<WorkspaceStore>,
    /// Cached markdown state for the proposed-plan body (rebuilt when the text
    /// changes) so streaming/replay reparses cheaply.
    md: Option<(String, Entity<MarkdownState>)>,
    /// Whether the "Copied!" confirmation is showing (2s).
    copied: bool,
    _copied_task: Option<Task<()>>,
    vscroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl PlanPanel {
    pub fn new(store: Entity<WorkspaceStore>, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![cx.observe(&store, |_, _, cx| cx.notify())];
        Self {
            store,
            md: None,
            copied: false,
            _copied_task: None,
            vscroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        }
    }

    fn sync_markdown(&mut self, markdown: &str, cx: &mut Context<Self>) -> Entity<MarkdownState> {
        if let Some((cached, state)) = &self.md
            && cached == markdown
        {
            return state.clone();
        }
        let text = markdown.to_string();
        let state = cx.new(|cx| MarkdownState::new(&text, cx));
        self.md = Some((text, state.clone()));
        state
    }

    fn mark_copied(&mut self, cx: &mut Context<Self>) {
        self.copied = true;
        self._copied_task = Some(cx.spawn(async move |this, cx| {
            smol::Timer::after(Duration::from_secs(2)).await;
            let _ = this.update(cx, |this, cx| {
                this.copied = false;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn render_proposed_plan(&mut self, markdown: String, cx: &mut Context<Self>) -> AnyElement {
        let title =
            plan_title(&markdown).unwrap_or_else(|| crate::tr!("plan.proposed_plan").into_owned());
        let md_state = self.sync_markdown(&markdown, cx);
        let copied = self.copied;

        let md_copy = markdown.clone();
        let md_download = markdown.clone();
        let md_save = markdown;

        v_flex()
            .w_full()
            .gap_2()
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(11.))
                            .line_height(px(20.))
                            .font_medium()
                            .text_color(cx.theme().muted_foreground)
                            .child(crate::tr!("plan.badge")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(15.))
                            .line_height(px(20.))
                            .font_medium()
                            .child(title),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .text_size(px(13.))
                    .line_height(px(20.))
                    .child(MarkdownView::new(&md_state).selectable(true)),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .flex_wrap()
                    .child(
                        Button::new("plan-copy")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Copy)
                            .label(if copied {
                                crate::tr!("plan.copied")
                            } else {
                                crate::tr!("plan.copy")
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let md = md_copy.clone();
                                this.store.update(cx, |store, _cx| {
                                    store.dispatch(Command::CopyPlan { markdown: md });
                                });
                                this.mark_copied(cx);
                            })),
                    )
                    .child(
                        Button::new("plan-download")
                            .ghost()
                            .xsmall()
                            .icon(Icon::empty().path("icons/download.svg"))
                            .label(crate::tr!("plan.download"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let md = md_download.clone();
                                let fallback = crate::tr!("plan.proposed_plan").into_owned();
                                this.store.update(cx, |store, _cx| {
                                    store.dispatch(Command::DownloadPlan {
                                        markdown: md,
                                        fallback_title: fallback,
                                    });
                                });
                            })),
                    )
                    .child(
                        Button::new("plan-save")
                            .ghost()
                            .xsmall()
                            .icon(IconName::HardDrive)
                            .label(crate::tr!("plan.save_workspace"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let md = md_save.clone();
                                this.store.update(cx, |store, _cx| {
                                    store.dispatch(Command::SavePlanToWorkspace { markdown: md });
                                });
                            })),
                    ),
            )
            .child(material::faded_hairline(cx))
            .into_any_element()
    }

    fn render_steps(&self, steps: &[PlanStep], cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let mut steps_col = v_flex()
            .w_full()
            .gap_1()
            .ml_2()
            .pl(px(14.))
            .py_0p5()
            .border_l_1()
            .border_color(cx.theme().border);
        for (index, step) in steps.iter().enumerate() {
            steps_col = steps_col.child(self.render_step(index, step, cx));
        }
        v_flex()
            .w_full()
            .gap_1()
            .child(
                div()
                    .pt_1()
                    .text_size(px(11.))
                    .font_medium()
                    .text_color(muted)
                    .child(crate::tr!("plan.steps")),
            )
            .child(steps_col)
            .into_any_element()
    }

    fn render_step(&self, index: usize, step: &PlanStep, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let primary = cx.theme().primary;

        let marker: AnyElement = match step.status {
            PlanStepStatus::Completed => Icon::new(IconName::CircleCheck)
                .xsmall()
                .text_color(muted)
                .into_any_element(),
            PlanStepStatus::InProgress => Spinner::new().xsmall().color(primary).into_any_element(),
            PlanStepStatus::Pending =>
            // An outlined circle with a muted dot.
            {
                div()
                    .size(px(14.))
                    .rounded_full()
                    .border_1()
                    .border_color(muted)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(div().size(px(4.)).rounded_full().bg(muted))
                    .into_any_element()
            }
        };

        let mut text = div()
            .flex_1()
            .min_w_0()
            .text_size(px(13.))
            .child(step.step.clone());
        if step.status == PlanStepStatus::Completed {
            text = text.line_through().text_color(muted);
        }

        h_flex()
            .id(("plan-step", index))
            .w_full()
            .py_1()
            .gap_2()
            .items_start()
            .child(div().flex_none().pt(px(1.)).child(marker))
            .child(text)
            .into_any_element()
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .gap_1()
            .child(
                div()
                    .text_size(px(15.))
                    .font_medium()
                    .child(crate::tr!("plan.empty_title")),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(cx.theme().muted_foreground)
                    .child(crate::tr!("plan.empty_desc")),
            )
            .into_any_element()
    }
}

impl Render for PlanPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (markdown, steps) = self.store.read(cx).plan_panel_state();

        if markdown.is_none() && steps.is_empty() {
            return v_flex().size_full().child(self.render_empty(cx));
        }

        let mut column = v_flex().w_full().p_3().gap_3();
        if let Some(markdown) = markdown {
            column = column.child(self.render_proposed_plan(markdown, cx));
        }
        if !steps.is_empty() {
            column = column.child(self.render_steps(&steps, cx));
        }

        v_flex().size_full().child(
            div()
                .id("plan-scroll")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .track_scroll(&self.vscroll)
                .child(column),
        )
    }
}
