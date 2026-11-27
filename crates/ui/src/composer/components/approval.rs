use super::super::*;
use crate::material::event_card::{self, CardTone};
use crate::scroll::ScrollableElement as _;
use gpui_base::StyledExt as _;

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
        asker: Option<tcode_protocol::SubagentAsking>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.motion.cross_fade(
            self.motion_key("approval-request"),
            (request.clone(), asker),
            CHANGE,
            window,
            cx,
            |(request, asker), window, cx| {
                self.render_approval(request, count, asker.as_ref(), window, cx)
            },
        )
    }

    fn render_approval(
        &self,
        request: &ApprovalRequest,
        count: usize,
        asker: Option<&tcode_protocol::SubagentAsking>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let answerable = approval_answerable(request, self.workspace_store.read(cx));
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
        let icon = match &request.kind {
            ApprovalKind::ExecCommand { .. } => IconName::SquareTerminal,
            ApprovalKind::FileRead { .. } => IconName::FileText,
            ApprovalKind::FileChange { .. } => IconName::Pencil,
            ApprovalKind::ToolUse { .. } => IconName::Puzzle,
            ApprovalKind::Plan { .. } => IconName::Map,
        };
        let muted = cx.theme().muted_foreground;

        let detail_area = |content: AnyElement| {
            div()
                .id("approval-detail-scroll")
                .w_full()
                .max_h(px(DETAIL_MAX_HEIGHT))
                .overflow_y_scroll_area()
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
                        .max_h(px(DETAIL_MAX_HEIGHT)),
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

        // The delivery line and the offline notice fold in and away; the
        // gap under each is carried inside it, so it folds with them.
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
                shown.fold(
                    div()
                        .px(px(crate::material::CARD_INSET))
                        .pb(gap)
                        .text_secondary(self.compact, cx)
                        .child(line),
                )
            });
        let offline = self
            .motion
            .shown(
                self.motion_key(format_args!("approval-offline-{}", request.id)),
                (!self.interactive(cx)).then_some(()),
                SHOW,
                window,
                cx,
            )
            .map(|shown| {
                shown.fold(
                    div()
                        .px(px(crate::material::CARD_INSET))
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
            event_card::section(cx)
                .gap_1()
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

        event_card::raised_event_card(CardTone::Waiting, cx)
            .debug_selector(|| "approval-card".into())
            .child(
                crate::material::accessible_clickable(
                    event_card::header(true, cx),
                    "approval-header",
                    Role::Button,
                    summary.clone(),
                    cx,
                )
                .when(self.compact, |header| {
                    header.min_h(px(crate::material::TOUCH_TARGET))
                })
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.approval_expanded = !this.approval_expanded;
                    cx.notify();
                }))
                .child(event_card::icon_tile(icon, cx))
                .child(event_card::header_text(summary, None, cx))
                .child(event_card::status(
                    None,
                    if self.compact {
                        crate::tr!("mobile.approval")
                    } else {
                        crate::tr!("approval.pending")
                    }
                    .into_owned(),
                    cx.theme().primary,
                    cx,
                ))
                .when(count > 1, |this| {
                    this.child(
                        div()
                            .flex_none()
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
            .when_some(asker, |card, asker| {
                card.child(self.render_asker(asker, cx))
            })
            .children(delivery)
            .children(offline)
            .child(detail)
            .when(!answerable, |card| {
                card.child(
                    event_card::section(cx)
                        .text_secondary(self.compact, cx)
                        .child(not_answerable(&self.workspace_store, cx)),
                )
            })
            .when(answerable, |card| {
                let respond = cx.entity().downgrade();
                card.child(
                    event_card::footer(cx)
                        .items_start()
                        .child(approval_decisions(
                            "approval",
                            request,
                            self.interactive(cx) && !pending,
                            self.compact,
                            &self.button_motion(),
                            Rc::new(move |request_id, decision, _, cx| {
                                respond
                                    .update(cx, |this, cx| this.respond(request_id, decision, cx))
                                    .ok();
                            }),
                        )),
                )
            })
            .into_any_element()
    }

    /// The line naming the harness's own subagent a request on the approval
    /// or question card comes from: it is answered in its lead's composer,
    /// where it would otherwise pass for the lead's own.
    pub(in super::super) fn render_asker(
        &self,
        asker: &tcode_protocol::SubagentAsking,
        cx: &App,
    ) -> AnyElement {
        let description = asker
            .description
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        h_flex()
            .debug_selector(|| "request-asker".into())
            .w_full()
            .px(px(crate::material::CARD_INSET))
            .pb(cx.theme().tokens.spacing.sm)
            .min_w_0()
            .gap_1p5()
            .items_center()
            .text_secondary(self.compact, cx)
            .child(
                h_flex()
                    .flex_none()
                    .gap_1()
                    .px_1p5()
                    .items_center()
                    .rounded(crate::material::radius_chip(cx))
                    .bg(cx.theme().muted)
                    .text_caption(cx)
                    .child(Icon::new(IconName::Bot).xsmall())
                    .child(crate::tr!("composer.subagent")),
            )
            .child(
                div()
                    .flex_none()
                    .font_medium()
                    .text_color(cx.theme().foreground)
                    .child(asker.agent_type.clone()),
            )
            .when(!description.is_empty(), |line| {
                line.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .child(crate::widgets::fade_text(format!("· {description}"))),
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
        self.workspace_store.update(cx, |store, _cx| {
            store.respond_approval_for(
                self.session_id.clone().unwrap_or_default(),
                request_id,
                decision,
            )
        });
    }
}

/// Whether this client's user may answer `request`. In a space that leaves
/// approvals to the agent's harness, a member answers none but a plan: theirs
/// are declined at once, and the owner's are the owner's. A machine that
/// lets this device only view it takes no answer from it.
pub(crate) fn approval_answerable(request: &ApprovalRequest, store: &WorkspaceStore) -> bool {
    !store.view_only()
        && (matches!(request.kind, ApprovalKind::Plan { .. })
            || store.space_allows(|policy| {
                policy.approval != tcode_protocol::SpaceApproval::ReviewedOnly
            }))
}

/// Why a request [`approval_answerable`] refuses cannot be answered here.
pub(crate) fn not_answerable(store: &Entity<WorkspaceStore>, cx: &App) -> gpui::SharedString {
    let store = store.read(cx);
    if store.view_only() {
        crate::tr!("machines.access.composer", machine = store.host_title())
    } else {
        crate::tr!("approval.space_reviewed_only")
    }
    .into_owned()
    .into()
}

/// Sends a decision on the approval with the given id.
pub(crate) type Decide = Rc<dyn Fn(String, ApprovalDecision, &mut Window, &mut App)>;

/// What a pending approval offers whoever answers it: the harness's own
/// options in its order and words, a rejecting one marked as such, then
/// cancelling the turn.
/// `id` prefixes the ids and debug selectors of its buttons.
pub(crate) fn approval_decisions(
    id: &str,
    request: &ApprovalRequest,
    enabled: bool,
    compact: bool,
    motion: &crate::motion::owned::Owned,
    decide: Decide,
) -> gpui::Div {
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
        let id = gpui::SharedString::from(format!("{id}-option-{}", option.id));
        let decide = decide.clone();
        let button = Button::new(id.clone())
            .motion(motion)
            .chip()
            .touch(compact)
            .disabled(!enabled)
            .label(option.label.clone())
            .when(rejects, |button| button.danger().outline())
            .debug_selector(move || id.to_string())
            .on_click(move |_, window, cx| {
                decide(
                    request_id.clone(),
                    ApprovalDecision::Option(option_id.clone()),
                    window,
                    cx,
                );
            });
        options = options.child(button);
    }
    let request_id = request.id.clone();
    let cancel = gpui::SharedString::from(format!("{id}-cancel"));
    h_flex()
        .w_full()
        .gap_2()
        .items_start()
        .child(options)
        .child(
            Button::new(cancel.clone())
                .motion(motion)
                .ghost()
                .chip()
                .touch(compact)
                .disabled(!enabled)
                .label(crate::tr!("approval.cancel_turn"))
                .debug_selector(move || cancel.to_string())
                .on_click(move |_, window, cx| {
                    decide(request_id.clone(), ApprovalDecision::Cancel, window, cx);
                }),
        )
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
        let request = ApprovalRequest {
            id: "patch".into(),
            parent_item_id: None,
            turn_id: None,
            kind,
            reason,
            blocked_path: None,
            options,
        };
        let crate::chat::tests::RunningThread {
            store,
            session_id,
            provider,
            ..
        } = crate::chat::tests::running_thread(
            cx,
            vec![
                agent::AgentEvent::TurnStarted {
                    turn_id: "turn".into(),
                },
                agent::AgentEvent::ApprovalRequested(request),
            ],
            |store, cx| {
                store
                    .sole_status(cx)
                    .is_some_and(|status| !status.pending_approvals.is_empty())
            },
        );
        // The provider waits on the answer for as long as the test runs.
        cx.on_quit(move || drop(provider));
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
