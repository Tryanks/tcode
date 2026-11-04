use super::super::*;
use tcode_core::control::ReferenceOrigin;
use tcode_protocol::{ControlStatus, ThreadReferenceStatus};

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

impl Composer {
    fn control_status(&self, cx: &App) -> Option<(String, ControlStatus)> {
        let store = self.workspace_store.read(cx);
        let status = store.session_status_for(self.session_id.as_deref())?;
        if status.draft {
            return None;
        }
        Some((self.session_id.clone()?, status.control.clone()?))
    }

    /// The grant requests the thread's model made with `request_grant`. Only
    /// these buttons answer one; nothing said in the chat does.
    pub(in super::super) fn control_request(&self, cx: &App) -> Option<ControlRequest> {
        let (session_id, status) = self.control_status(cx)?;
        let count = status.pending_requests.len();
        let request = status.pending_requests.into_iter().next()?;
        Some(ControlRequest {
            session_id,
            request,
            count,
        })
    }

    /// The card for the first grant request; when it is answered and the next
    /// shows, the next cross-fades over it.
    pub(in super::super) fn render_control_request_panel(
        &self,
        pending: &ControlRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let count = pending.count;
        let session_id = pending.session_id.clone();
        self.motion.cross_fade(
            self.motion_key("control-request"),
            pending.request.clone(),
            CHANGE,
            window,
            cx,
            move |request, _, cx| self.render_control_request(&session_id, request, count, cx),
        )
    }

    fn render_control_request(
        &self,
        session_id: &str,
        request: &tcode_core::control::ControlGrantRequest,
        count: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let session_id = session_id.to_owned();
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
        // The caption and the count sit on the title's first line however
        // many lines the title wraps to.
        let title_line = cx.theme().tokens.typography.md.line_height;
        crate::material::card_raised(cx)
            .w_full()
            .debug_selector(|| "control-request".into())
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_start()
                    .child(
                        div()
                            .text_caption(cx)
                            .line_height(title_line)
                            .child(crate::tr!("control.request.caption")),
                    )
                    .child(div().flex_1().min_w_0().text_title(cx).child(summary))
                    .when(count > 1, |row| {
                        row.child(
                            div()
                                .text_secondary(self.compact, cx)
                                .line_height(title_line)
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
                            .motion(&self.button_motion())
                            .primary()
                            .chip()
                            .touch(self.compact)
                            .disabled(!self.interactive(cx))
                            .label(crate::tr!("control.request.approve"))
                            .on_click(respond(true)),
                    )
                    .child(
                        Button::new("control-request-decline")
                            .motion(&self.button_motion())
                            .danger()
                            .outline()
                            .chip()
                            .touch(self.compact)
                            .disabled(!self.interactive(cx))
                            .label(crate::tr!("control.request.decline"))
                            .on_click(respond(false)),
                    ),
            )
            .into_any_element()
    }

    /// The threads this one may read, for the composer's chip rows.
    pub(in super::super) fn thread_references(
        &self,
        cx: &App,
    ) -> Option<(String, Vec<ThreadReferenceStatus>)> {
        let (session_id, status) = self.control_status(cx)?;
        (!status.references.is_empty()).then_some((session_id, status.references))
    }

    /// The chip for one thread this one may read; pressing it takes the
    /// reference back.
    pub(in super::super) fn reference_chip(
        &self,
        session_id: &str,
        reference: &ThreadReferenceStatus,
        cx: &mut Context<Self>,
    ) -> Button {
        let session_id = session_id.to_owned();
        let thread_id = reference.thread_id.clone();
        let selector = format!("thread-reference-chip-{thread_id}");
        Button::new(gpui::SharedString::from(format!(
            "thread-reference-chip-{thread_id}"
        )))
        .debug_selector(move || selector.clone())
        .ghost()
        .small()
        .chip()
        .text_meta_mono(self.compact, cx)
        .label(format!("{}  ×", reference_chip_label(reference)))
        .tooltip(reference_chip_tooltip(reference))
        .aria_label(
            crate::tr!("control.reference.remove").into_owned()
                + ": "
                + &reference_title(reference),
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            let session_id = session_id.clone();
            let thread_id = thread_id.clone();
            this.workspace_store.update(cx, |store, _| {
                store.remove_thread_reference(session_id, thread_id)
            });
        }))
    }

    /// Reference `thread_id` from the active thread. When the host answers that the new
    /// reference crosses a project or provider boundary, the user is told once and may take
    /// it back.
    pub(in super::super) fn reference_thread(
        &mut self,
        thread_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.workspace_store.read(cx);
        let Some(session_id) = self.session_id.clone() else {
            return;
        };
        let title = store
            .thread_meta(&thread_id)
            .map(|meta| meta.title.clone())
            .unwrap_or_default();
        let added = self.workspace_store.update(cx, |store, cx| {
            store.command(
                tcode_protocol::Command::AddThreadReference {
                    session_id: session_id.clone(),
                    thread_id: thread_id.clone(),
                },
                cx,
            )
        });
        let store = self.workspace_store.clone();
        cx.spawn_in(window, async move |this, cx| {
            let Ok(tcode_protocol::CommandResponse::ThreadReferenceAdded {
                boundary_notice: true,
            }) = added.await
            else {
                return;
            };
            let _ = this.update_in(cx, |_, window, cx| {
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
                        .on_cancel(move |_, _, cx| {
                            let session_id = session_id.clone();
                            let thread_id = thread_id.clone();
                            store.update(cx, |store, _| {
                                store.remove_thread_reference(session_id, thread_id)
                            });
                            true
                        })
                })
            });
        })
        .detach();
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

/// The grant request the composer asks about, kept while its card folds
/// away so it leaves as it was.
#[derive(Clone, PartialEq)]
pub(in super::super) struct ControlRequest {
    session_id: String,
    request: tcode_core::control::ControlGrantRequest,
    count: usize,
}
