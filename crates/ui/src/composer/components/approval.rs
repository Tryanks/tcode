use super::super::*;
use crate::scroll::ScrollableElement as _;

impl Composer {
    pub(in super::super) fn render_approval_panel(
        &self,
        request: &ApprovalRequest,
        count: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let summary = match &request.kind {
            ApprovalKind::ExecCommand { .. } => {
                if self.compact {
                    crate::tr!("mobile.command_requested")
                } else {
                    crate::tr!("approval.command_requested")
                }
            }
            ApprovalKind::FileRead { .. } => {
                if self.compact {
                    crate::tr!("mobile.read_requested")
                } else {
                    crate::tr!("approval.file_read_requested")
                }
            }
            ApprovalKind::FileChange { .. } => {
                if self.compact {
                    crate::tr!("mobile.files_requested")
                } else {
                    crate::tr!("approval.file_requested")
                }
            }
            ApprovalKind::ToolUse { .. } => {
                if self.compact {
                    crate::tr!("mobile.tool_requested")
                } else {
                    crate::tr!("approval.tool_requested")
                }
            }
        };
        let muted = cx.theme().muted_foreground;

        let detail_bg = cx.theme().muted;
        let detail_area = |content: AnyElement| {
            div()
                .id("approval-detail-scroll")
                .w_full()
                .max_h(px(240.))
                .overflow_y_scroll_area()
                .p_2()
                .rounded(px(8.))
                .bg(detail_bg)
                .child(content)
                .into_any_element()
        };
        let detail: AnyElement = match &request.kind {
            ApprovalKind::ExecCommand { command, cwd, .. } => detail_area(
                v_flex()
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
            ),
            ApprovalKind::FileChange { changes, .. } => {
                let changes = Rc::new(changes.clone());
                let last = changes.len().saturating_sub(1);
                div()
                    .w_full()
                    .rounded(px(8.))
                    .bg(detail_bg)
                    .child(
                        crate::scroll::VirtualList::measured(
                            "approval-detail-scroll",
                            changes.len(),
                            move |index, _, cx| {
                                let change = &changes[index];
                                div()
                                    .debug_selector(move || format!("approval-change-{index}"))
                                    .when(index < last, |row| row.pb_0p5())
                                    .text_size(px(13.))
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .child(format!(
                                        "{} {}",
                                        file_change_kind_label(change.kind),
                                        change.path
                                    ))
                            },
                        )
                        .w_full()
                        .max_h(px(240.))
                        .p_2(),
                    )
                    .into_any_element()
            }
            ApprovalKind::FileRead { detail } => detail_area(
                div()
                    .text_size(px(13.))
                    .font_family(cx.theme().mono_font_family.clone())
                    .child(detail.clone())
                    .into_any_element(),
            ),
            ApprovalKind::ToolUse { name, input, .. } => detail_area(
                div()
                    .text_size(px(13.))
                    .font_family(cx.theme().mono_font_family.clone())
                    .child(format!("{name} {input}"))
                    .into_any_element(),
            ),
        };

        let pending = self
            .workspace_store
            .read(cx)
            .approval_delivery_pending(&request.id);
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
            .when(pending, |card| {
                card.child(div().text_size(px(11.)).text_color(muted).child(crate::tr!(
                    if self
                        .workspace_store
                        .read(cx)
                        .connection_state()
                        .is_connected()
                    {
                        "chat.sending"
                    } else {
                        "chat.waiting_connection"
                    }
                )))
            })
            .when(self.compact && !self.interactive(cx), |card| {
                card.child(
                    div()
                        .text_size(px(13.))
                        .text_color(muted)
                        .child(crate::tr!("mobile.offline_action")),
                )
            })
            .child(
                h_flex()
                    .id("approval-header")
                    .when(self.compact, |header| header.min_h(px(44.)))
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
                            .child(if self.compact {
                                crate::tr!("mobile.approval")
                            } else {
                                crate::tr!("approval.pending")
                            }),
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
            .when(expanded, |this| this.child(detail))
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
                    .when(self.compact, |button| {
                        button
                            .min_h(px(44.))
                            .min_w(px(44.))
                            .w(gpui::relative(0.48))
                            .flex_none()
                    })
                    .disabled(!self.interactive(cx) || pending)
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
                // Keep long localized approval labels on separate compact rows
                // so they cannot overlap the Deny/Allow controls.
                let compact = self.compact;
                let interactive = self.interactive(cx) && !pending;
                let half = |button: Button| {
                    if compact {
                        button
                            .min_h(px(44.))
                            .min_w(px(44.))
                            .w(gpui::relative(0.48))
                            .flex_none()
                    } else {
                        button
                    }
                };
                let full = |button: Button| {
                    if compact {
                        button.min_h(px(44.)).w_full().flex_none()
                    } else {
                        button
                    }
                };
                let cancel = full(
                    Button::new("approval-cancel")
                        .ghost()
                        .small()
                        .h(px(28.))
                        .disabled(!interactive)
                        .rounded(crate::material::radius_input())
                        .label(crate::tr!("approval.cancel_turn"))
                        .text_color(cx.theme().muted_foreground)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.respond(cancel_id.clone(), ApprovalDecision::Cancel, cx);
                        })),
                );
                let deny = half(
                    Button::new("approval-deny")
                        .ghost()
                        .small()
                        .h(px(28.))
                        .disabled(!interactive)
                        .rounded(crate::material::radius_input())
                        .label(if compact {
                            crate::tr!("mobile.deny")
                        } else {
                            crate::tr!("approval.decline")
                        })
                        .text_color(cx.theme().danger)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.respond(deny_id.clone(), ApprovalDecision::Deny, cx);
                        })),
                );
                let always = full(
                    Button::new("approval-always")
                        .ghost()
                        .small()
                        .h(px(28.))
                        .disabled(!interactive)
                        .rounded(crate::material::radius_input())
                        .label(if compact {
                            crate::tr!("mobile.always_allow")
                        } else {
                            crate::tr!("approval.always_allow_session")
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.respond(
                                always_id.clone(),
                                ApprovalDecision::ApproveForSession,
                                cx,
                            );
                        })),
                );
                let approve = half(
                    Button::new("approval-approve")
                        .primary()
                        .small()
                        .h(px(28.))
                        .disabled(!interactive)
                        .rounded(crate::material::radius_input())
                        .label(if compact {
                            crate::tr!("mobile.allow")
                        } else {
                            crate::tr!("approval.approve_once")
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.respond(approve_id.clone(), ApprovalDecision::Approve, cx);
                        })),
                );
                let row = h_flex().w_full().gap_2().items_center().flex_wrap();
                let row = if compact {
                    row.child(deny)
                        .child(div().flex_1())
                        .child(approve)
                        .child(always)
                        .child(cancel)
                } else {
                    row.child(cancel)
                        .child(div().flex_1())
                        .child(deny)
                        .child(always)
                        .child(approve)
                };
                this.child(row)
            })
            .into_any_element()
    }

    pub(in super::super) fn respond(
        &mut self,
        request_id: String,
        decision: ApprovalDecision,
        cx: &mut Context<Self>,
    ) {
        self.workspace_store.update(cx, |store, _cx| {
            store.respond_approval(request_id, decision)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{ScrollDelta, ScrollWheelEvent, TestAppContext, point, size};

    #[gpui::test]
    fn a_long_file_change_request_lays_out_only_the_visible_files(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let host = tcode_runtime::pipe::spawn_host(
            tcode_services::store::SessionStore::open_at(std::env::temp_dir().join(format!(
                "tcode-approval-list-test-{}-{}",
                std::process::id(),
                tcode_services::store::now_millis()
            )))
            .unwrap(),
            tcode_runtime::pipe::HostServices::default(),
        )
        .unwrap();
        let (session_id, timeline) = smol::block_on(host.update_state_for_test(|state, cx| {
            let id = state.start_draft("approval-list".into(), std::env::temp_dir(), cx);
            let active = state.residents.live.get_mut(&id).unwrap();
            active.timeline.pending_approvals.push(ApprovalRequest {
                id: "patch".into(),
                turn_id: None,
                kind: ApprovalKind::FileChange {
                    changes: (0..300)
                        .map(|index| agent::FileChange {
                            path: format!("src/file-{index}.rs"),
                            kind: agent::FileChangeKind::Modify,
                            diff: None,
                        })
                        .collect(),
                    reason: None,
                },
                options: Vec::new(),
            });
            (id, active.timeline.clone())
        }))
        .unwrap();
        let store = cx.new(|cx| WorkspaceStore::new(host.link(), cx));
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id, timeline, cx);
        });
        let (_composer, cx) = cx.add_window_view(|window, cx| {
            Composer::new_with_layout(store.clone(), true, window, cx)
        });
        cx.simulate_resize(size(px(393.), px(852.)));
        cx.update(|window, cx| _ = window.draw(cx));
        let first = cx
            .debug_bounds("approval-change-0")
            .expect("first change row");
        assert!(
            cx.debug_bounds("approval-change-250").is_none(),
            "the approval card must not lay out all 300 file rows"
        );

        // Rows are measured as they come into view, so the list reaches its
        // end over successive gestures.
        for _ in 0..20 {
            cx.simulate_event(ScrollWheelEvent {
                position: first.center(),
                delta: ScrollDelta::Pixels(point(px(0.), px(-2_000.))),
                ..Default::default()
            });
            cx.update(|window, cx| _ = window.draw(cx));
        }
        assert!(cx.debug_bounds("approval-change-299").is_some());
        assert!(cx.debug_bounds("approval-change-0").is_none());
    }
}
