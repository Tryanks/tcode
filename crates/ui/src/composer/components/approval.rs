use super::super::*;
use crate::scroll::ScrollableElement as _;

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

/// How tall a request's detail grows before it scrolls inside the card, so the
/// options stay on screen; no owner sizes a card's scrolling body.
const DETAIL_MAX_HEIGHT: f32 = 240.;

impl Composer {
    /// The card for the first pending approval; when it is answered and the
    /// next shows, the next cross-fades over it.
    pub(in super::super) fn render_approval_panel(
        &self,
        request: &ApprovalRequest,
        count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.motion.cross_fade(
            self.motion_key("approval-request"),
            request.clone(),
            CHANGE,
            window,
            cx,
            |request, window, cx| self.render_approval(request, count, window, cx),
        )
    }

    fn render_approval(
        &self,
        request: &ApprovalRequest,
        count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // In a space that leaves approvals to the agent's harness, a member
        // answers none: theirs are declined at once, and the owner's are the
        // owner's.
        let answerable = matches!(request.kind, ApprovalKind::Plan { .. })
            || self.workspace_store.read(cx).space_allows(|policy| {
                policy.approval != tcode_protocol::SpaceApproval::ReviewedOnly
            });
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
            ApprovalKind::Plan { .. } => crate::tr!("approval.plan_requested"),
        };
        let muted = cx.theme().muted_foreground;

        let detail_bg = cx.theme().muted;
        let detail_area = |content: AnyElement| {
            div()
                .id("approval-detail-scroll")
                .w_full()
                .max_h(px(DETAIL_MAX_HEIGHT))
                .overflow_y_scroll_area()
                .p_2()
                .rounded(crate::material::radius_card(cx))
                .bg(detail_bg)
                .child(content)
                .into_any_element()
        };
        let detail: AnyElement = match &request.kind {
            ApprovalKind::ExecCommand {
                command,
                cwd,
                description,
            } => detail_area(
                v_flex()
                    .gap_1()
                    .child(div().text_code(cx).child(command.clone()))
                    .when_some(description.clone(), |this, description| {
                        this.child(div().text_secondary(self.compact, cx).child(description))
                    })
                    .when_some(cwd.clone(), |this, cwd| {
                        this.child(
                            div()
                                .text_secondary(self.compact, cx)
                                .child(crate::tr!("approval.in_directory", cwd = cwd)),
                        )
                    })
                    .into_any_element(),
            ),
            ApprovalKind::FileChange { changes, .. } => {
                let changes = Rc::new(changes.clone());
                let last = changes.len().saturating_sub(1);
                div()
                    .debug_selector(|| "approval-detail".into())
                    .w_full()
                    .rounded(crate::material::radius_card(cx))
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
                                    .text_code(cx)
                                    .child(format!(
                                        "{} {}",
                                        file_change_kind_label(change.kind),
                                        change.path
                                    ))
                            },
                        )
                        .w_full()
                        .max_h(px(DETAIL_MAX_HEIGHT))
                        .p_2(),
                    )
                    .into_any_element()
            }
            ApprovalKind::FileRead { detail } => {
                detail_area(div().text_code(cx).child(detail.clone()).into_any_element())
            }
            ApprovalKind::Plan { markdown, .. } => detail_area(
                div()
                    .text_body(self.compact, cx)
                    .child(markdown.clone())
                    .into_any_element(),
            ),
            ApprovalKind::ToolUse {
                name,
                input,
                detail,
            } => {
                let input = input.to_string();
                detail_area(
                    v_flex()
                        .gap_1()
                        .when(
                            !detail.is_empty() && *detail != *name && *detail != input,
                            |this| {
                                this.child(div().text_body(self.compact, cx).child(detail.clone()))
                            },
                        )
                        .child(div().text_code(cx).child(format!("{name} {input}")))
                        .into_any_element(),
                )
            }
        };

        let pending = self
            .workspace_store
            .read(cx)
            .approval_delivery_pending_for(self.session_id.as_deref(), &request.id);
        let expanded = self.approval_expanded;
        let cancel_id = request.id.clone();

        // The delivery line and the offline notice fold in and away; the
        // card's own gap is carried under each, so it folds with them.
        let gap = cx.theme().tokens.spacing.sm;
        let connected = self
            .workspace_store
            .read(cx)
            .connection_state()
            .is_connected();
        let delivery = self
            .motion
            .shown(
                self.motion_key(format_args!("approval-delivery-{}", request.id)),
                pending.then_some(connected),
                SHOW,
                window,
                cx,
            )
            .map(|shown| {
                let line = self.motion.cross_fade_part(
                    self.motion_key(format_args!("approval-delivery-text-{}", request.id)),
                    shown.value,
                    CHANGE,
                    window,
                    cx,
                    |connected, _, _| {
                        div()
                            .child(crate::tr!(if *connected {
                                "chat.sending"
                            } else {
                                "chat.waiting_connection"
                            }))
                            .into_any_element()
                    },
                );
                shown.fold(div().pb(gap).text_secondary(self.compact, cx).child(line))
            });
        let offline = self
            .motion
            .shown(
                self.motion_key(format_args!("approval-offline-{}", request.id)),
                (self.compact && !self.interactive(cx)).then_some(()),
                SHOW,
                window,
                cx,
            )
            .map(|shown| {
                shown.fold(
                    div()
                        .pb(gap)
                        .text_body(self.compact, cx)
                        .text_color(muted)
                        .child(crate::tr!("mobile.offline_action")),
                )
            });
        let detail = crate::widgets::Collapsible::new(
            self.motion_id(format_args!("approval-detail-{}", request.id)),
            expanded,
        )
        .motion(&self.motion)
        .w_full()
        .content(
            v_flex()
                .w_full()
                .gap_1()
                .pb(gap)
                .child(detail)
                .when_some(request.reason.clone(), |this, reason| {
                    this.child(
                        div()
                            .debug_selector(|| "approval-reason".into())
                            .text_body(self.compact, cx)
                            .child(reason),
                    )
                })
                .when_some(request.blocked_path.clone(), |this, path| {
                    this.child(
                        div()
                            .text_secondary(self.compact, cx)
                            .child(crate::tr!("approval.blocked_path", path = path)),
                    )
                }),
        );

        crate::material::card_raised(cx)
            .w_full()
            .gap_0()
            .children(delivery)
            .children(offline)
            .child(
                crate::material::accessible_clickable(
                    h_flex(),
                    "approval-header",
                    Role::Button,
                    summary.clone(),
                    cx,
                )
                .when(self.compact, |header| {
                    header.min_h(px(crate::material::TOUCH_TARGET))
                })
                .w_full()
                .gap_2()
                .items_center()
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.approval_expanded = !this.approval_expanded;
                    cx.notify();
                }))
                .child(div().text_caption(cx).child(if self.compact {
                    crate::tr!("mobile.approval")
                } else {
                    crate::tr!("approval.pending")
                }))
                .child(div().flex_1().text_title(cx).child(summary))
                .when(count > 1, |this| {
                    this.child(
                        div()
                            .text_secondary(self.compact, cx)
                            .child(format!("1/{count}")),
                    )
                })
                .child(
                    crate::widgets::Chevron::new(
                        self.motion_id(format_args!("approval-chevron-{}", request.id)),
                        expanded,
                    )
                    .owned(&self.motion)
                    .text_color(muted),
                ),
            )
            .child(div().h(gap))
            .child(detail)
            .when(!answerable, |card| {
                card.child(
                    div()
                        .text_secondary(self.compact, cx)
                        .child(crate::tr!("approval.space_reviewed_only")),
                )
            })
            .when(answerable, |card| {
                card.child({
                    let mut options = h_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_2()
                        .items_center()
                        .flex_wrap();
                    for option in &request.options {
                        let request_id = request.id.clone();
                        let option_id = option.id.clone();
                        let rejects = matches!(
                            option.kind,
                            ApprovalOptionKind::RejectOnce | ApprovalOptionKind::RejectAlways
                        );
                        let id = gpui::SharedString::from(format!("approval-option-{}", option.id));
                        let button = Button::new(id)
                            .motion(&self.button_motion())
                            .chip()
                            .touch(self.compact)
                            .disabled(!self.interactive(cx) || pending)
                            .label(option.label.clone())
                            .when(rejects, |button| button.danger().outline())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.respond(
                                    request_id.clone(),
                                    ApprovalDecision::Option(option_id.clone()),
                                    cx,
                                );
                            }));
                        options = options.child(button);
                    }
                    h_flex()
                        .w_full()
                        .gap_2()
                        .items_start()
                        .child(options)
                        .child(
                            Button::new("approval-cancel")
                                .motion(&self.button_motion())
                                .ghost()
                                .chip()
                                .touch(self.compact)
                                .disabled(!self.interactive(cx) || pending)
                                .label(crate::tr!("approval.cancel_turn"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.respond(cancel_id.clone(), ApprovalDecision::Cancel, cx);
                                })),
                        )
                })
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
            store.respond_approval_for(
                self.session_id.clone().unwrap_or_default(),
                request_id,
                decision,
            )
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext, size};

    /// A composer, phone or desktop, whose session awaits approval of a patch
    /// to `paths`.
    fn composer_with_patch(
        cx: &mut TestAppContext,
        compact: bool,
        paths: impl Iterator<Item = String>,
    ) -> &mut VisualTestContext {
        let changes = paths
            .map(|path| agent::FileChange {
                path,
                kind: agent::FileChangeKind::Modify,
                diff: None,
            })
            .collect();
        composer_with_request(
            cx,
            compact,
            ApprovalKind::FileChange { changes },
            None,
            Vec::new(),
        )
    }

    /// A composer, phone or desktop, whose session awaits approval of `kind`,
    /// asked for `reason`, with `options`.
    fn composer_with_request(
        cx: &mut TestAppContext,
        compact: bool,
        kind: ApprovalKind,
        reason: Option<String>,
        options: Vec<agent::ApprovalOption>,
    ) -> &mut VisualTestContext {
        cx.update(crate::theme::init);
        let host = tcode_runtime::pipe::spawn_host(
            tcode_services::store::SessionStore::open_at(crate::store::tests::scratch_root(
                "approval-list-test",
            ))
            .unwrap(),
            tcode_runtime::pipe::HostServices::default(),
        )
        .unwrap();
        let session_id = smol::block_on(host.update_state_for_test(|state, cx| {
            let id = state.start_draft("approval-list".into(), std::env::temp_dir(), cx);
            let request = ApprovalRequest {
                id: "patch".into(),
                parent_item_id: None,
                turn_id: None,
                kind,
                reason,
                blocked_path: None,
                options,
            };
            state.provider_event_for_test(&id, agent::AgentEvent::ApprovalRequested(request), cx);
            id
        }))
        .unwrap();
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                cx,
            )
        });
        driver.wait_until(cx, "workspace baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });
        store.update(cx, |store, cx| store.show_alone(session_id.clone(), cx));
        driver.wait_until(cx, "selected conversation", |cx| {
            store.read_with(cx, |store, cx| {
                store.baseline_ready()
                    && !store.chat_loading_for(store.sole_thread().as_deref(), cx)
            })
        });
        let (_composer, cx) = cx.add_window_view(|window, cx| {
            Composer::new_with_layout(store.clone(), Some(session_id), compact, window, cx)
        });
        cx.simulate_resize(if compact {
            size(px(393.), px(852.))
        } else {
            size(px(1280.), px(800.))
        });
        cx.update(|window, cx| _ = window.draw(cx));
        // The first layout finds the list's width; the next frame lays out
        // rows at it.
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
            _ = window.draw(cx);
        });
        cx
    }

    #[gpui::test]
    fn a_short_request_shows_every_wrapped_path(cx: &mut TestAppContext) {
        let cx = composer_with_patch(
            cx,
            true,
            (0..2).map(|index| format!("src/{}file-{index}.rs", "deeply/nested/".repeat(6))),
        );
        let detail = cx.debug_bounds("approval-detail").expect("detail box");
        let first = cx.debug_bounds("approval-change-0").expect("first path");
        let second = cx.debug_bounds("approval-change-1").expect("second path");
        assert!(first.size.height > px(30.), "a long path wraps on a phone");
        assert!(second.top() >= first.bottom());
        assert!(
            second.bottom() <= detail.bottom(),
            "a short list grows to show every path"
        );
    }

    #[gpui::test]
    fn a_desktop_request_opens_with_what_it_asks_to_do(cx: &mut TestAppContext) {
        let cx = composer_with_patch(cx, false, std::iter::once("src/main.rs".to_owned()));
        assert!(
            cx.debug_bounds("approval-change-0").is_some(),
            "the request's detail is shown before anyone clicks its header"
        );
    }

    /// Why the harness asks is on the card, as the native dialog's consent
    /// line is: under bypassPermissions it is the only clue to a prompt.
    #[gpui::test]
    fn a_request_says_why_the_harness_asks(cx: &mut TestAppContext) {
        let cx = composer_with_request(
            cx,
            false,
            ApprovalKind::ExecCommand {
                command: "rm -rf $F/*".into(),
                cwd: None,
                description: Some("Clear the scratch directory".into()),
            },
            Some("Dangerous rm operation detected.".into()),
            Vec::new(),
        );
        assert!(cx.debug_bounds("approval-reason").is_some());
    }
}
