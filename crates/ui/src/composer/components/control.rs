use super::super::*;
use tcode_protocol::ControlStatus;

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

impl Composer {
    fn control_status(&self, cx: &App) -> Option<(String, ControlStatus)> {
        let store = self.workspace_store.read(cx);
        let status = store.session_status_for(self.session_id.as_deref(), cx)?;
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
        let (projects, own_project, current) = {
            let store = self.workspace_store.read(cx);
            let projects = store.projects();
            let meta = store.thread_meta(&session_id).cloned();
            let own_project = meta
                .as_ref()
                .and_then(|meta| meta.project_id.as_deref())
                .and_then(|id| projects.iter().find(|project| project.id == id))
                .map(|project| project.name.clone());
            let current = self.control_status(cx).map(|(_, status)| status.grant);
            (projects, own_project, current)
        };
        let wants = crate::tr!(
            "control.request.wants",
            what = crate::control::grant_phrase(&request.grant, own_project.as_deref(), &projects)
                .unwrap_or_default()
        )
        .into_owned();
        let mut context = current
            .map(|grant| {
                crate::control::current_grant_sentence(&grant, own_project.as_deref(), &projects)
            })
            .unwrap_or_default();
        if !context.is_empty() {
            context.push_str(&crate::tr!("list.sentence_separator"));
        }
        context.push_str(&crate::tr!("control.request.applies"));
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
                    .child(div().text_caption(cx).line_height(title_line).child(
                        crate::widgets::fade_text(crate::tr!("control.request.caption")),
                    ))
                    .child(div().flex_1().min_w_0().text_title(cx).child(wants))
                    .when(count > 1, |row| {
                        row.child(
                            div()
                                .text_secondary(self.compact, cx)
                                .line_height(title_line)
                                .child(format!("1/{count}")),
                        )
                    }),
            )
            .child(div().text_secondary(self.compact, cx).child(context))
            .when(!request.reason.trim().is_empty(), |card| {
                card.child(div().text_body(self.compact, cx).italic().child(crate::tr!(
                    "control.request.reason",
                    reason = request.reason.trim()
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
}

/// The grant request the composer asks about, kept while its card folds
/// away so it leaves as it was.
#[derive(Clone, PartialEq)]
pub(in super::super) struct ControlRequest {
    session_id: String,
    request: tcode_core::control::ControlGrantRequest,
    count: usize,
}
