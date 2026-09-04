use super::super::*;

impl Composer {
    /// The active session's pending user-input request, if any.
    pub(in super::super) fn pending_user_input(
        &self,
        cx: &App,
    ) -> Option<(String, Vec<UserInputQuestion>)> {
        self.workspace_store
            .read(cx)
            .composer_state()
            .pending_user_input
    }

    /// Keep the per-request question state in sync: reset the index/selections
    /// when a new request arrives (or the pending one resolves).
    pub(in super::super) fn sync_user_input_state(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self
            .workspace_store
            .read(cx)
            .composer_state()
            .pending_user_input;
        let current_id = current.as_ref().map(|(id, _)| id.clone());
        if current_id != self.ui_request_id {
            self.ui_request_id = current_id;
            self.ui_question_index = 0;
            self.ui_selections.clear();
            let prefill = current
                .as_ref()
                .and_then(|(_, questions)| questions.first())
                .and_then(|question| question.prefill.as_deref())
                .unwrap_or_default();
            self.user_input_custom.update(cx, |state, cx| {
                state.set_value(prefill, window, cx);
            });
        }
    }

    pub(in super::super) fn render_user_input_panel(
        &self,
        request_id: String,
        questions: Vec<UserInputQuestion>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let primary = cx.theme().primary;
        let total = questions.len();
        let index = self.ui_question_index.min(total.saturating_sub(1));
        let Some(question) = questions.get(index).cloned() else {
            return div().into_any_element();
        };
        let multi = question.multi_select;
        let selected = self
            .ui_selections
            .get(&question.id)
            .cloned()
            .unwrap_or_default();

        // Header: question header + "N/total" when multiple.
        let header = h_flex()
            .w_full()
            .gap_2()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .text_size(px(13.))
                    .font_medium()
                    .child(question.header.clone()),
            )
            .when(total > 1, |this| {
                this.child(div().text_size(px(11.)).text_color(muted).child(crate::tr!(
                    "userinput.question_count",
                    index = index + 1,
                    total = total
                )))
            });

        // Option rows.
        let mut options_content = v_flex().w_full().gap_1();
        for (opt_index, option) in question.options.iter().enumerate() {
            let is_selected = selected.iter().any(|l| l == &option.label);
            let label = option.label.clone();
            let question_for_click = question.clone();
            let questions_for_click = questions.clone();
            let request_for_click = request_id.clone();
            let mark = div()
                .size(px(16.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(if multi {
                    px(5.)
                } else {
                    crate::material::radius_input()
                })
                .border_1()
                .border_color(if is_selected { primary } else { muted })
                .when(is_selected, |mark| {
                    mark.bg(primary)
                        .child(div().size(px(6.)).rounded_full().bg(gpui::white()))
                });
            options_content = options_content.child(
                h_flex()
                    .id(("ui-opt", opt_index))
                    .flex_none()
                    .w_full()
                    .min_h(px(28.))
                    .px_2()
                    .py_1()
                    .gap_2()
                    .items_start()
                    .rounded(crate::material::radius_chip())
                    .cursor_pointer()
                    .when(is_selected, |s| s.bg(cx.theme().list_active))
                    .hover(|s| s.bg(cx.theme().muted))
                    .child(div().flex_none().pt(px(2.)).child(mark))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .gap_1p5()
                                    .items_center()
                                    .text_size(px(13.))
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_color(muted)
                                            .child(format!("{}", opt_index + 1)),
                                    )
                                    .child(div().font_medium().child(option.label.clone())),
                            )
                            .when(!option.description.is_empty(), |this| {
                                this.child(
                                    div()
                                        .text_size(px(13.))
                                        .text_color(muted)
                                        .child(option.description.clone()),
                                )
                            }),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.ui_toggle_option(&question_for_click, label.clone(), cx);
                        // Single-select answers flow onward by themselves: next
                        // unanswered question, or submission when none remain.
                        if !multi {
                            this.ui_advance_or_submit(
                                &questions_for_click,
                                request_for_click.clone(),
                                window,
                                cx,
                            );
                        }
                    })),
            );
        }
        let options = div()
            .id("user-input-options-scroll")
            .w_full()
            .max_h(px(240.))
            .overflow_y_scroll()
            .child(options_content);

        let custom_input = self.user_input_custom.clone();
        let custom_has_text = !custom_input.read(cx).value().trim().is_empty();
        let custom_answer = h_flex()
            .w_full()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .rounded(px(8.))
            .border_1()
            .border_color(cx.theme().input)
            .bg(cx.theme().popover)
            .child(
                div().flex_1().min_w_0().child(
                    Textarea::new(&self.user_input_custom)
                        .appearance(false)
                        // Match the 13px option rows around it.
                        .text_size(px(13.)),
                ),
            )
            .child(
                crate::material::accessible_clickable(
                    div(),
                    "ui-custom-answer-submit",
                    Role::Button,
                    crate::tr!("userinput.submit_custom"),
                    cx,
                )
                .size(px(28.))
                .rounded(crate::material::radius_input())
                .flex()
                .items_center()
                .justify_center()
                .bg(if custom_has_text {
                    cx.theme().primary
                } else {
                    cx.theme().muted
                })
                .cursor_pointer()
                .when(custom_has_text, |this| this.hover(|this| this.opacity(0.9)))
                .child(
                    Icon::new(IconName::ArrowUp)
                        .xsmall()
                        .text_color(if custom_has_text {
                            cx.theme().primary_foreground
                        } else {
                            cx.theme().muted_foreground
                        }),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.submit_custom_user_input(&custom_input, window, cx);
                })),
            );

        // Actions row: navigation only. Answers submit themselves — a
        // single-select click that completes the set submits it, so the only
        // finishing affordance left is Done on a multi-select question (clicks
        // there cannot signal "I'm finished").
        let is_last = index + 1 >= total;
        let all_answered = user_input_all_answered(&questions, &self.ui_selections);
        let questions_submit = questions.clone();
        let request_submit = request_id.clone();
        let mut actions = h_flex().w_full().gap_2().items_center();
        if index > 0 {
            let questions_previous = questions.clone();
            actions = actions.child(
                Button::new("ui-prev")
                    .ghost()
                    .small()
                    .h(px(28.))
                    .rounded(crate::material::radius_input())
                    .label(crate::tr!("userinput.previous"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.ui_go(-1, &questions_previous, window, cx)
                    })),
            );
        }
        actions = actions.child(div().flex_1());
        if !is_last {
            let questions_next = questions.clone();
            actions = actions.child(
                Button::new("ui-next")
                    .outline()
                    .small()
                    .h(px(28.))
                    .rounded(crate::material::radius_input())
                    .label(crate::tr!("userinput.next_question"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.ui_go(1, &questions_next, window, cx)
                    })),
            );
        }
        if multi && all_answered {
            actions = actions.child(
                Button::new("ui-done")
                    .primary()
                    .small()
                    .h(px(28.))
                    .rounded(crate::material::radius_input())
                    .label(crate::tr!("userinput.done"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.ui_submit(&questions_submit, request_submit.clone(), window, cx);
                    })),
            );
        }

        let pager = h_flex()
            .w_full()
            .h(px(12.))
            .gap_1()
            .items_center()
            .justify_center()
            .children((0..total).map(|page| {
                let questions = questions.clone();
                div()
                    .id(("ui-page", page))
                    .size(px(if page == index { 9. } else { 7. }))
                    .rounded_full()
                    .bg(if page == index {
                        primary
                    } else {
                        muted.opacity(0.35)
                    })
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let delta = page as i32 - this.ui_question_index as i32;
                        this.ui_go(delta, &questions, window, cx);
                    }))
            }));

        v_flex()
            .w_full()
            .gap_2()
            .p(px(14.))
            .rounded(crate::material::radius_card())
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .shadow_sm()
            .child(header)
            .child(div().text_size(px(13.)).child(question.question.clone()))
            .child(options)
            .child(custom_answer)
            .when(multi, |this| {
                this.child(
                    div()
                        .text_size(px(11.))
                        .text_color(muted)
                        .child(crate::tr!("userinput.multi_hint")),
                )
            })
            .child(actions)
            .when(total > 1, |this| this.child(pager))
            .into_any_element()
    }

    /// Number keys 1-9 pressed in the (empty) main composer input select the
    /// matching option of the pending question. Returns true when consumed.
    /// Deliberately NOT wired to the panel itself: the only focusable child
    /// there is the custom-answer textarea, where digits must stay literal.
    pub(in super::super) fn handle_user_input_digit(
        &mut self,
        ev: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if ev.keystroke.modifiers.modified() || !self.input.read(cx).value().is_empty() {
            return false;
        }
        let Some((request_id, questions)) = self.pending_user_input(cx) else {
            return false;
        };
        let index = self
            .ui_question_index
            .min(questions.len().saturating_sub(1));
        let Some(question) = questions.get(index).cloned() else {
            return false;
        };
        let Ok(n) = ev.keystroke.key.parse::<usize>() else {
            return false;
        };
        if n < 1 || n > question.options.len() {
            return false;
        }
        let label = question.options[n - 1].label.clone();
        self.ui_toggle_option(&question, label, cx);
        if !question.multi_select {
            self.ui_advance_or_submit(&questions, request_id, window, cx);
        }
        true
    }

    /// Toggle an option label for a question: single-select replaces, multi
    /// toggles membership.
    pub(in super::super) fn ui_toggle_option(
        &mut self,
        question: &UserInputQuestion,
        label: String,
        cx: &mut Context<Self>,
    ) {
        let entry = self.ui_selections.entry(question.id.clone()).or_default();
        if question.multi_select {
            if let Some(pos) = entry.iter().position(|l| l == &label) {
                entry.remove(pos);
            } else {
                entry.push(label);
            }
        } else {
            *entry = vec![label];
        }
        cx.notify();
    }

    pub(in super::super) fn submit_custom_user_input(
        &mut self,
        input: &Entity<TextareaState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((request_id, questions)) = self.pending_user_input(cx) else {
            return;
        };
        let text = input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        let index = self
            .ui_question_index
            .min(questions.len().saturating_sub(1));
        if let Some(question) = questions.get(index) {
            let entry = self.ui_selections.entry(question.id.clone()).or_default();
            if question.multi_select {
                entry.push(text);
            } else {
                *entry = vec![text];
            }
        }
        input.update(cx, |state, cx| state.set_value("", window, cx));
        self.ui_advance_or_submit(&questions, request_id, window, cx);
        cx.notify();
    }

    pub(in super::super) fn ui_go(
        &mut self,
        delta: i32,
        questions: &[UserInputQuestion],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let next = self.ui_question_index as i32 + delta;
        if next >= 0 {
            self.ui_question_index = next as usize;
            self.seed_user_input_prefill(questions, window, cx);
            cx.notify();
        }
    }

    pub(in super::super) fn seed_user_input_prefill(
        &mut self,
        questions: &[UserInputQuestion],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prefill = questions
            .get(self.ui_question_index)
            .and_then(|question| question.prefill.as_deref())
            .unwrap_or_default();
        self.user_input_custom
            .update(cx, |state, cx| state.set_value(prefill, window, cx));
    }

    /// After answering: jump to the next unanswered question, or — when the
    /// answer completed the whole set — submit without any button press
    /// (S1 §7). The ~200ms pause lets the selection mark register visually
    /// before the panel moves on.
    pub(in super::super) fn ui_advance_or_submit(
        &mut self,
        questions: &[UserInputQuestion],
        request_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let questions = questions.to_vec();
        let at = self.ui_question_index;
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(200))
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                // A newer request or manual navigation invalidates this hop.
                if this.ui_question_index != at
                    || this.ui_request_id.as_deref() != Some(&request_id)
                {
                    return;
                }
                if user_input_all_answered(&questions, &this.ui_selections) {
                    this.ui_submit(&questions, request_id, window, cx);
                } else if let Some(next) =
                    next_unanswered_question(&questions, &this.ui_selections, at)
                {
                    this.ui_question_index = next;
                    this.seed_user_input_prefill(&questions, window, cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(in super::super) fn ui_submit(
        &mut self,
        questions: &[UserInputQuestion],
        request_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let custom = self.user_input_custom.read(cx).value().trim().to_string();
        let custom = if custom.is_empty() {
            None
        } else {
            Some(custom.as_str())
        };
        let answers = assemble_user_input_answers(
            questions,
            &self.ui_selections,
            self.ui_question_index,
            custom,
        );
        self.user_input_custom
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.ui_selections.clear();
        self.ui_question_index = 0;
        self.workspace_store.update(cx, |store, _cx| {
            store.respond_user_input(request_id, answers)
        });
        cx.notify();
    }
}
