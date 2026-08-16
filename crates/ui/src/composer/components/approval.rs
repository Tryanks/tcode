use super::super::*;

impl Composer {
    pub(in super::super) fn render_approval_panel(
        &self,
        request: &ApprovalRequest,
        count: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let summary = match &request.kind {
            ApprovalKind::ExecCommand { .. } => crate::tr!("approval.command_requested"),
            ApprovalKind::FileRead { .. } => crate::tr!("approval.file_read_requested"),
            ApprovalKind::FileChange { .. } => crate::tr!("approval.file_requested"),
            ApprovalKind::ToolUse { .. } => crate::tr!("approval.tool_requested"),
        };
        let muted = cx.theme().muted_foreground;

        let detail: AnyElement = match &request.kind {
            ApprovalKind::ExecCommand { command, cwd, .. } => v_flex()
                .gap_1()
                .child(
                    div()
                        .text_size(px(13.))
                        .font_family(cx.theme().mono_font_family.clone())
                        .child(command.clone()),
                )
                .when_some(cwd.clone(), |this, cwd| {
                    this.child(
                        div()
                            .text_size(px(11.))
                            .text_color(muted)
                            .child(crate::tr!("approval.in_directory", cwd = cwd)),
                    )
                })
                .into_any_element(),
            ApprovalKind::FileChange { changes, .. } => v_flex()
                .gap_0p5()
                .children(changes.iter().map(|change| {
                    div()
                        .text_size(px(13.))
                        .font_family(cx.theme().mono_font_family.clone())
                        .child(format!(
                            "{} {}",
                            file_change_kind_label(change.kind),
                            change.path
                        ))
                }))
                .into_any_element(),
            ApprovalKind::FileRead { detail } => div()
                .text_size(px(13.))
                .font_family(cx.theme().mono_font_family.clone())
                .child(detail.clone())
                .into_any_element(),
            ApprovalKind::ToolUse { name, input, .. } => div()
                .text_size(px(13.))
                .font_family(cx.theme().mono_font_family.clone())
                .child(format!("{name} {input}"))
                .into_any_element(),
        };

        let expanded = self.approval_expanded;
        let approve_id = request.id.clone();
        let always_id = request.id.clone();
        let deny_id = request.id.clone();
        let cancel_id = request.id.clone();

        v_flex()
            .w_full()
            .gap_2()
            .p(px(14.))
            .rounded(crate::material::radius_card())
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .shadow_sm()
            .child(
                h_flex()
                    .id("approval-header")
                    .w_full()
                    .gap_2()
                    .items_center()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.approval_expanded = !this.approval_expanded;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .text_size(px(11.))
                            .font_medium()
                            .text_color(muted)
                            .child(crate::tr!("approval.pending")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(13.))
                            .font_medium()
                            .child(summary),
                    )
                    .when(count > 1, |this| {
                        this.child(
                            div()
                                .text_size(px(11.))
                                .text_color(muted)
                                .child(format!("1/{count}")),
                        )
                    })
                    .child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .xsmall()
                        .text_color(muted),
                    ),
            )
            .when(expanded, |this| {
                this.child(
                    div()
                        .id("approval-detail-scroll")
                        .w_full()
                        .max_h(px(240.))
                        .overflow_y_scroll()
                        .p_2()
                        .rounded(px(8.))
                        .bg(cx.theme().muted)
                        .child(detail),
                )
            })
            .when(!request.options.is_empty(), |this| {
                // An ACP agent sends its own option list: render exactly those
                // buttons (the labels are the agent's), ordered rejections-first
                // like our fixed four, and answer with the chosen option id.
                let mut row = h_flex().w_full().gap_2().items_center().flex_wrap();
                let mut options = request.options.clone();
                options.sort_by_key(|option| match option.kind {
                    ApprovalOptionKind::RejectAlways => 0,
                    ApprovalOptionKind::RejectOnce => 1,
                    ApprovalOptionKind::AllowAlways => 2,
                    ApprovalOptionKind::AllowOnce => 3,
                });
                let last = options.len().saturating_sub(1);
                for (index, option) in options.into_iter().enumerate() {
                    let request_id = request.id.clone();
                    let option_id = option.id.clone();
                    let rejects = matches!(
                        option.kind,
                        ApprovalOptionKind::RejectOnce | ApprovalOptionKind::RejectAlways
                    );
                    let button = Button::new(gpui::SharedString::from(format!(
                        "approval-option-{}",
                        option.id
                    )))
                    .small()
                    .h(px(28.))
                    .rounded(crate::material::radius_input())
                    .label(option.label.clone())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.respond(
                            request_id.clone(),
                            ApprovalDecision::Option(option_id.clone()),
                            cx,
                        );
                    }));
                    // The agent's preferred (last) option is the primary action.
                    let button = if index == last {
                        button.primary()
                    } else if rejects {
                        button.ghost().text_color(cx.theme().danger)
                    } else {
                        button.ghost()
                    };
                    if index == 1 {
                        row = row.child(div().flex_1());
                    }
                    row = row.child(button);
                }
                this.child(row)
            })
            .when(request.options.is_empty(), |this| {
                this.child(
                    // T3 order (S2 §4): Cancel turn, Decline, Always allow this
                    // session, Approve once.
                    h_flex()
                        .w_full()
                        .gap_2()
                        .items_center()
                        .flex_wrap()
                        .child(
                            Button::new("approval-cancel")
                                .ghost()
                                .small()
                                .h(px(28.))
                                .rounded(crate::material::radius_input())
                                .label(crate::tr!("approval.cancel_turn"))
                                .text_color(cx.theme().muted_foreground)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.respond(cancel_id.clone(), ApprovalDecision::Cancel, cx);
                                })),
                        )
                        .child(div().flex_1())
                        .child(
                            Button::new("approval-deny")
                                .ghost()
                                .small()
                                .h(px(28.))
                                .rounded(crate::material::radius_input())
                                .label(crate::tr!("approval.decline"))
                                .text_color(cx.theme().danger)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.respond(deny_id.clone(), ApprovalDecision::Deny, cx);
                                })),
                        )
                        .child(
                            Button::new("approval-always")
                                .ghost()
                                .small()
                                .h(px(28.))
                                .rounded(crate::material::radius_input())
                                .label(crate::tr!("approval.always_allow_session"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.respond(
                                        always_id.clone(),
                                        ApprovalDecision::ApproveForSession,
                                        cx,
                                    );
                                })),
                        )
                        .child(
                            Button::new("approval-approve")
                                .primary()
                                .small()
                                .h(px(28.))
                                .rounded(crate::material::radius_input())
                                .label(crate::tr!("approval.approve_once"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.respond(approve_id.clone(), ApprovalDecision::Approve, cx);
                                })),
                        ),
                )
            })
            .into_any_element()
    }

    pub(in super::super) fn respond(
        &mut self,
        request_id: String,
        decision: ApprovalDecision,
        cx: &mut Context<Self>,
    ) {
        self.approval_expanded = false;
        self.workspace_store.update(cx, |store, _cx| {
            store.respond_approval(request_id, decision)
        });
    }
}
