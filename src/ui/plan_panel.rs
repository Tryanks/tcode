//! The right-panel "Plan / Tasks" tab: the captured proposed plan (with its
//! Copy / Download / Save actions) plus the latest structured plan steps
//! (S1 §6). Hosted alongside the diff view by [`crate::ui::diff_panel`].

use std::time::Duration;

use agent::{PlanStep, PlanStepStatus};
use gpui::{
    AnyElement, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, ScrollHandle, StatefulInteractiveElement as _, Styled as _,
    Subscription, Task, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    spinner::Spinner,
    text::{TextView, TextViewState},
    v_flex,
};

use crate::app::AppState;
use crate::session::plan_title;

pub struct PlanPanel {
    app_state: Entity<AppState>,
    /// Cached markdown state for the proposed-plan body (rebuilt when the text
    /// changes) so streaming/replay reparses cheaply.
    md: Option<(String, Entity<TextViewState>)>,
    /// Whether the "Copied!" confirmation is showing (2s).
    copied: bool,
    _copied_task: Option<Task<()>>,
    vscroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl PlanPanel {
    pub fn new(app_state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![cx.observe(&app_state, |_, _, cx| cx.notify())];
        Self {
            app_state,
            md: None,
            copied: false,
            _copied_task: None,
            vscroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        }
    }

    fn sync_markdown(&mut self, markdown: &str, cx: &mut Context<Self>) -> Entity<TextViewState> {
        if let Some((cached, state)) = &self.md
            && cached == markdown
        {
            return state.clone();
        }
        let text = markdown.to_string();
        let state = cx.new(|cx| TextViewState::markdown(&text, cx));
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
        let title = plan_title(&markdown)
            .unwrap_or_else(|| rust_i18n::t!("plan.proposed_plan").into_owned());
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
                            .px_2()
                            .py(px(1.))
                            .rounded(px(4.))
                            .bg(cx.theme().primary)
                            .text_color(cx.theme().primary_foreground)
                            .text_size(px(11.))
                            .font_medium()
                            .child(rust_i18n::t!("plan.badge")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(14.))
                            .font_medium()
                            .child(title),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .text_size(px(13.))
                    .line_height(px(20.))
                    .child(TextView::new(&md_state).selectable(true)),
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
                                rust_i18n::t!("plan.copied")
                            } else {
                                rust_i18n::t!("plan.copy")
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let md = md_copy.clone();
                                this.app_state.update(cx, |s, cx| s.copy_plan(md, cx));
                                this.mark_copied(cx);
                            })),
                    )
                    .child(
                        Button::new("plan-download")
                            .ghost()
                            .xsmall()
                            .icon(Icon::empty().path("icons/download.svg"))
                            .label(rust_i18n::t!("plan.download"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let md = md_download.clone();
                                this.app_state.update(cx, |s, cx| s.download_plan(md, cx));
                            })),
                    )
                    .child(
                        Button::new("plan-save")
                            .ghost()
                            .xsmall()
                            .icon(IconName::HardDrive)
                            .label(rust_i18n::t!("plan.save_workspace"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let md = md_save.clone();
                                this.app_state
                                    .update(cx, |s, cx| s.save_plan_to_workspace(md, cx));
                            })),
                    ),
            )
            .child(div().h_px().w_full().bg(cx.theme().border))
            .into_any_element()
    }

    fn render_steps(&self, steps: &[PlanStep], cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let mut col = v_flex().w_full().gap_1().child(
            div()
                .pt_1()
                .text_size(px(11.))
                .font_medium()
                .text_color(muted)
                .child(rust_i18n::t!("plan.steps")),
        );
        for (index, step) in steps.iter().enumerate() {
            col = col.child(self.render_step(index, step, cx));
        }
        col.into_any_element()
    }

    fn render_step(&self, index: usize, step: &PlanStep, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let success = cx.theme().success;
        let primary = cx.theme().primary;

        let (marker, bg): (AnyElement, Option<gpui::Hsla>) = match step.status {
            PlanStepStatus::Completed => (
                Icon::new(IconName::CircleCheck)
                    .xsmall()
                    .text_color(success)
                    .into_any_element(),
                Some(tint(success)),
            ),
            PlanStepStatus::InProgress => (
                Spinner::new().xsmall().color(primary).into_any_element(),
                Some(tint(primary)),
            ),
            PlanStepStatus::Pending => (
                // An outlined circle with a muted dot.
                div()
                    .size(px(14.))
                    .rounded_full()
                    .border_1()
                    .border_color(muted)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(div().size(px(4.)).rounded_full().bg(muted))
                    .into_any_element(),
                None,
            ),
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
            .px_2()
            .py_1p5()
            .gap_2()
            .items_start()
            .rounded(px(6.))
            .when_some(bg, |this, color| this.bg(color))
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
                    .text_size(px(14.))
                    .font_medium()
                    .child(rust_i18n::t!("plan.empty_title")),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(cx.theme().muted_foreground)
                    .child(rust_i18n::t!("plan.empty_desc")),
            )
            .into_any_element()
    }
}

impl Render for PlanPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let markdown = self.app_state.read(cx).proposed_plan_markdown();
        let steps = self.app_state.read(cx).plan_steps();

        if markdown.is_none() && steps.is_empty() {
            return v_flex()
                .size_full()
                .bg(cx.theme().background)
                .child(self.render_empty(cx));
        }

        let mut column = v_flex().w_full().p_3().gap_3();
        if let Some(markdown) = markdown {
            column = column.child(self.render_proposed_plan(markdown, cx));
        }
        if !steps.is_empty() {
            column = column.child(self.render_steps(&steps, cx));
        }

        v_flex().size_full().bg(cx.theme().background).child(
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

/// A subtle background tint from an accent color (row highlight).
fn tint(color: gpui::Hsla) -> gpui::Hsla {
    gpui::Hsla { a: 0.12, ..color }
}
