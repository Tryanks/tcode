use super::super::*;
use crate::scroll::ScrollableElement as _;
use tcode_core::control::{ControlGrantSource, ReferenceOrigin};
use tcode_protocol::{ControlStatus, ThreadReferenceStatus};

/// How tall the control pane grows before it scrolls; no owner sizes a
/// popover's scrolling body.
const PANE_MAX_HEIGHT: f32 = 480.;

impl Composer {
    fn control_status(&self, cx: &App) -> Option<(String, ControlStatus)> {
        let store = self.workspace_store.read(cx);
        let status = store.session_status()?;
        if status.draft {
            return None;
        }
        Some((store.active_session_id()?, status.control.clone()?))
    }

    /// The thread's `tcode_control` chip beside the permission picker: its
    /// effective grant and where it came from, with a badge while the model
    /// waits on a grant request.
    pub(in super::super) fn render_control_chip(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (_, status) = self.control_status(cx)?;
        if !status.tools_offered && status.source != ControlGrantSource::Thread {
            return None;
        }
        let muted = cx.theme().muted_foreground;
        let label = crate::tr!(
            "control.chip.label",
            summary = crate::control::grant_summary(&status.grant)
        )
        .into_owned();
        let pending = status.pending_requests.len();
        let trigger = Button::new("control-chip")
            .debug_selector(|| "control-chip".into())
            .aria_label(label.clone())
            // The grant summary truncates so the send button keeps its place in the row.
            .max_w(px(200.))
            .min_w_0()
            .overflow_hidden()
            .ghost()
            .chip()
            .touch(self.compact)
            .disabled(!self.interactive(cx))
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .overflow_hidden()
                    .gap_1p5()
                    .items_center()
                    .text_body(self.compact, cx)
                    .text_color(muted)
                    .child(Icon::new(IconName::Layers))
                    .child(div().min_w_0().truncate().child(label))
                    .when(pending > 0, |row| {
                        row.child(
                            crate::material::semantic_chip(
                                if pending == 1 {
                                    crate::tr!("control.chip.pending_one")
                                } else {
                                    crate::tr!("control.chip.pending", count = pending)
                                },
                                cx.theme().warning.opacity(0.15),
                                cx.theme().warning,
                                cx,
                            )
                            .flex_none(),
                        )
                    })
                    .child(Icon::new(IconName::ChevronDown)),
            );
        let store = self.workspace_store.clone();
        let compact = self.compact;
        Some(
            crate::material::overlay_popover("control-popover", cx)
                .anchor(Anchor::BottomLeft)
                .when(compact, |popover| {
                    popover.bottom_sheet(crate::tr!("control.chip.title"))
                })
                .trigger(trigger)
                .content(move |_, _, cx| render_control_pane(&store, compact, cx))
                .into_any_element(),
        )
    }

    /// The grant requests the thread's model made with `request_grant`. Only
    /// these buttons answer one; nothing said in the chat does.
    pub(in super::super) fn render_control_request_panel(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (session_id, status) = self.control_status(cx)?;
        let count = status.pending_requests.len();
        let request = status.pending_requests.into_iter().next()?;
        let respond = |approve: bool| {
            let session_id = session_id.clone();
            let request_id = request.id.clone();
            cx.listener(move |this: &mut Self, _, _, cx| {
                let session_id = session_id.clone();
                let request_id = request_id.clone();
                this.workspace_store.update(cx, |store, _| {
                    store.respond_control_grant(session_id, request_id, approve)
                });
            })
        };
        let summary = crate::tr!(
            "control.request.summary",
            summary = crate::control::grant_summary(&request.grant)
        )
        .into_owned();
        Some(
            crate::material::card_raised(cx)
                .w_full()
                .debug_selector(|| "control-request".into())
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .text_caption(cx)
                                .child(crate::tr!("control.request.caption")),
                        )
                        .child(div().flex_1().text_title(cx).child(summary))
                        .when(count > 1, |row| {
                            row.child(
                                div()
                                    .text_secondary(self.compact, cx)
                                    .child(format!("1/{count}")),
                            )
                        }),
                )
                .when(!request.reason.trim().is_empty(), |card| {
                    card.child(div().text_body(self.compact, cx).child(crate::tr!(
                        "control.request.reason",
                        reason = request.reason.as_str()
                    )))
                })
                .child(
                    div()
                        .text_secondary(self.compact, cx)
                        .child(crate::tr!("control.request.note")),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .flex_wrap()
                        .child(
                            Button::new("control-request-approve")
                                .primary()
                                .chip()
                                .touch(self.compact)
                                .disabled(!self.interactive(cx))
                                .label(crate::tr!("control.request.approve"))
                                .on_click(respond(true)),
                        )
                        .child(
                            Button::new("control-request-decline")
                                .danger()
                                .outline()
                                .chip()
                                .touch(self.compact)
                                .disabled(!self.interactive(cx))
                                .label(crate::tr!("control.request.decline"))
                                .on_click(respond(false)),
                        ),
                )
                .into_any_element(),
        )
    }

    /// One chip per thread this one may read, in the composer's chip row.
    pub(in super::super) fn render_reference_chips(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (session_id, status) = self.control_status(cx)?;
        if status.references.is_empty() {
            return None;
        }
        Some(
            h_flex()
                .w_full()
                .flex_wrap()
                .gap_1()
                .children(
                    status
                        .references
                        .into_iter()
                        .enumerate()
                        .map(|(index, reference)| {
                            let session_id = session_id.clone();
                            let thread_id = reference.thread_id.clone();
                            Button::new(("thread-reference-chip", index))
                                .debug_selector(move || format!("thread-reference-chip-{index}"))
                                .ghost()
                                .small()
                                .chip()
                                .text_meta_mono(self.compact, cx)
                                .label(format!("{}  ×", reference_chip_label(&reference)))
                                .tooltip(reference_chip_tooltip(&reference))
                                .aria_label(
                                    crate::tr!("control.reference.remove").into_owned()
                                        + ": "
                                        + &reference_title(&reference),
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let session_id = session_id.clone();
                                    let thread_id = thread_id.clone();
                                    this.workspace_store.update(cx, |store, _| {
                                        store.remove_thread_reference(session_id, thread_id)
                                    });
                                }))
                        }),
                )
                .into_any_element(),
        )
    }

    /// Reference `thread_id` from the active thread, telling the user first
    /// when its content would reach another project's or provider's vendor.
    pub(in super::super) fn reference_thread(
        &mut self,
        thread_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.workspace_store.read(cx);
        let Some(session_id) = store.active_session_id() else {
            return;
        };
        let (Some(from), Some(to)) = (
            store.thread_meta(&session_id),
            store.thread_meta(&thread_id),
        ) else {
            return;
        };
        let crosses = crate::control::crosses_boundary(from, to);
        let title = to.title.clone();
        let store = self.workspace_store.clone();
        if !crosses {
            store.update(cx, |store, _| {
                store.add_thread_reference(session_id, thread_id)
            });
            return;
        }
        window.open_alert_dialog(cx, move |alert, _, _| {
            let store = store.clone();
            let session_id = session_id.clone();
            let thread_id = thread_id.clone();
            alert
                .title(crate::tr!("control.reference.notice_title"))
                .description(crate::tr!(
                    "control.reference.notice_description",
                    title = title.as_str()
                ))
                .button_props(
                    DialogButtons::default()
                        .ok_text(crate::tr!("control.reference.notice_ok"))
                        .cancel_text(crate::tr!("control.reference.notice_cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, _, cx| {
                    let session_id = session_id.clone();
                    let thread_id = thread_id.clone();
                    store.update(cx, |store, _| {
                        store.add_thread_reference(session_id, thread_id)
                    });
                    true
                })
        });
    }
}

fn reference_title(reference: &ThreadReferenceStatus) -> String {
    if reference.deleted || reference.title.is_empty() {
        crate::tr!("control.settings.deleted_thread").into_owned()
    } else {
        reference.title.clone()
    }
}

/// "@title · model · shared": who started the reference, and whether its
/// content crosses a project or provider boundary or is gone.
fn reference_chip_label(reference: &ThreadReferenceStatus) -> String {
    let mut label = format!("@{}", reference_title(reference));
    if reference.by == ReferenceOrigin::Model {
        label.push_str(" · ");
        label.push_str(&crate::tr!("control.reference.model"));
    }
    if reference.deleted {
        label.push_str(" · ");
        label.push_str(&crate::tr!("control.reference.deleted"));
    } else if reference.boundary_notice {
        label.push_str(" · ");
        label.push_str(&crate::tr!("control.reference.boundary"));
    }
    label
}

fn reference_chip_tooltip(reference: &ThreadReferenceStatus) -> String {
    let mut tooltip = crate::tr!(
        "control.reference.chip_tooltip",
        title = reference_title(reference)
    )
    .into_owned();
    if reference.boundary_notice && !reference.deleted {
        tooltip.push(' ');
        tooltip.push_str(&crate::tr!("control.reference.boundary_tooltip"));
    }
    tooltip
}

fn render_control_pane(
    store_entity: &Entity<WorkspaceStore>,
    compact: bool,
    cx: &mut Context<PopoverState>,
) -> AnyElement {
    let store = store_entity.read(cx);
    let Some((session_id, status)) = store.active_session_id().zip(
        store
            .session_status()
            .and_then(|status| status.control.clone()),
    ) else {
        return div().into_any_element();
    };
    let projects = store.projects();
    let caption = |text: Cow<'static, str>, cx: &App| div().pt_1().text_caption(cx).child(text);
    let mut pane = v_flex()
        .id("control-pane")
        .debug_selector(|| "control-pane".into())
        .w_full()
        .when(!compact, |pane| pane.w(px(360.)).max_h(px(PANE_MAX_HEIGHT)))
        .overflow_y_scroll_area()
        .p_2()
        .gap_2()
        .child(crate::control::grant_heading(
            &status.grant,
            status.source,
            compact,
            cx,
        ));
    if !status.tools_offered {
        pane = pane.child(
            div()
                .text_secondary(compact, cx)
                .child(crate::tr!("control.chip.not_offered")),
        );
    }
    if status.source == ControlGrantSource::Thread {
        let store = store_entity.clone();
        let session_id = session_id.clone();
        pane = pane.child(
            Button::new("control-use-default")
                .ghost()
                .outline()
                .small()
                .label(crate::tr!("control.chip.use_default"))
                .on_click(move |_, _, cx| {
                    let session_id = session_id.clone();
                    store.update(cx, |store, _| store.set_control_grant(session_id, None));
                }),
        );
    } else {
        pane = pane.child(caption(crate::tr!("control.chip.own_grant"), cx));
    }
    let store = store_entity.clone();
    let editor_session = session_id.clone();
    pane = pane.child(crate::control::grant_editor(
        "control-thread-grant",
        &status.grant,
        &projects,
        compact,
        Rc::new(move |grant, _, cx| {
            let session_id = editor_session.clone();
            store.update(cx, |store, _| {
                store.set_control_grant(session_id, Some(grant))
            });
        }),
        cx,
    ));
    if let Some(grant) = &status.self_settle {
        let store = store_entity.clone();
        let session_id = session_id.clone();
        pane = pane
            .child(caption(crate::tr!("control.self_settle.title"), cx))
            .child(
                crate::material::group_row(cx)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_body(compact, cx)
                                    .child(crate::control::when_label(&grant.when)),
                            )
                            .child(div().text_secondary(compact, cx).child(crate::tr!(
                                "control.self_settle.consent",
                                quote = grant.consent_quote.as_str()
                            ))),
                    )
                    .child(
                        Button::new("control-revoke-self-settle")
                            .ghost()
                            .xsmall()
                            .label(crate::tr!("control.self_settle.revoke"))
                            .on_click(move |_, _, cx| {
                                let session_id = session_id.clone();
                                store.update(cx, |store, _| store.revoke_self_settle(session_id));
                            }),
                    ),
            );
    }
    pane = pane.child(caption(crate::tr!("control.chip.references"), cx));
    if status.references.is_empty() {
        pane = pane.child(
            div()
                .text_secondary(compact, cx)
                .child(crate::tr!("control.chip.references_empty")),
        );
    } else {
        let rows = status
            .references
            .iter()
            .enumerate()
            .map(|(index, reference)| {
                let store = store_entity.clone();
                let session_id = session_id.clone();
                let thread_id = reference.thread_id.clone();
                crate::material::group_row(cx)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_body(compact, cx)
                            .child(reference_chip_label(reference)),
                    )
                    .child(
                        Button::new(("control-remove-reference", index))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip(crate::tr!("control.reference.remove"))
                            .on_click(move |_, _, cx| {
                                let session_id = session_id.clone();
                                let thread_id = thread_id.clone();
                                store.update(cx, |store, _| {
                                    store.remove_thread_reference(session_id, thread_id)
                                });
                            }),
                    )
                    .into_any_element()
            })
            .collect();
        pane = pane.child(crate::material::grouped(rows, cx));
    }
    pane.into_any_element()
}
