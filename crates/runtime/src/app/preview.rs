//! The host-held preview of each thread, and the one client rendering it.
//!
//! A client that can render pages subscribes to its own [`Topic::Preview`],
//! naming itself. Each thread's preview is rendered by at most one of those
//! clients: the one that last showed it here, or, when an agent asks and no
//! client renders the thread, a client in the host's own process, which
//! reaches what the agent serves on that machine and is never suspended in
//! the background, else the client that most recently subscribed or showed
//! a preview. Only that client receives the thread's requests. A
//! client whose subscription ends — it disconnected, or a phone closed its
//! link in the background — renders nothing any more.

use super::*;
use tcode_protocol::{PreviewClient, PreviewRequest, PreviewSession, PreviewTab};

type PreviewReplyTx = async_channel::Sender<Result<preview_mcp::PreviewReply, String>>;

#[derive(Default)]
pub(super) struct Previews {
    /// Every thread whose preview a client has shown or rendered, in memory
    /// only.
    sessions: HashMap<String, PreviewSession>,
    /// The clients subscribed to their [`Topic::Preview`], each with when it
    /// last subscribed or showed a preview.
    clients: HashMap<String, (PreviewClient, u64)>,
    clock: u64,
    pending: HashMap<u64, PendingRequest>,
    next_request: u64,
}

struct PendingRequest {
    reply: PreviewReplyTx,
    session_id: String,
    client_id: String,
    kind: RequestKind,
}

/// What the host adds to, or learns from, a request's reply.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RequestKind {
    Status,
    Snapshot,
    Other,
}

impl Previews {
    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    fn renderer_of(&self, session_id: &str) -> Option<&PreviewClient> {
        self.sessions.get(session_id)?.renderer.as_ref()
    }

    /// The client that renders a thread nobody renders when its agent asks.
    fn agent_client(&self) -> Option<PreviewClient> {
        self.clients
            .values()
            .max_by_key(|(client, seen)| (client.on_host, *seen))
            .map(|(client, _)| client.clone())
    }
}

impl AppState {
    pub(crate) fn pump_preview_requests(
        &mut self,
        requests: Option<async_channel::Receiver<preview_mcp::BrokerRequest>>,
        cx: &mut HostCx,
    ) {
        let Some(requests) = requests else {
            return;
        };
        let host = cx.clone();
        cx.spawn_detached(async move {
            while let Ok(request) = requests.recv().await {
                host.enqueue(move |state, cx| state.route_preview(request, cx));
            }
        });
    }

    pub(crate) fn route_preview(&mut self, broker: preview_mcp::BrokerRequest, cx: &mut HostCx) {
        let session_id = broker.session_id;
        // The agent's thread need not be on anyone's screen. Without a
        // client that can render it, nothing would answer.
        let renderer = match self.previews.renderer_of(&session_id).cloned() {
            Some(renderer) => renderer,
            None => match self.previews.agent_client() {
                Some(client) => {
                    self.render_preview_on(&session_id, client.clone(), cx);
                    client
                }
                None => {
                    let _ = broker.reply.try_send(Err(
                        "preview is unavailable: no connected client has a preview browser".into(),
                    ));
                    return;
                }
            },
        };
        self.previews.next_request += 1;
        let request_id = self.previews.next_request;
        let caller = broker.reply.clone();
        let kind = match broker.op {
            PreviewRequest::Status => RequestKind::Status,
            PreviewRequest::Snapshot => RequestKind::Snapshot,
            _ => RequestKind::Other,
        };
        self.previews.pending.insert(
            request_id,
            PendingRequest {
                reply: broker.reply,
                session_id: session_id.clone(),
                client_id: renderer.id.clone(),
                kind,
            },
        );
        cx.emit(HostEvent::Domain(EventEnvelope {
            request_id: None,
            topic: Topic::Preview { client: renderer },
            event: ServerEvent::PreviewRequest {
                request_id,
                session_id,
                request: broker.op,
            },
        }));
        // The call that asked may end before the client answers, or its turn
        // be interrupted ([`Self::cancel_previews_of`]); its request goes with
        // it, and the client stops waiting on the page for it.
        let host = cx.clone();
        cx.spawn_detached(async move {
            caller.closed().await;
            host.enqueue(move |state, cx| state.forget_preview(request_id, cx));
        });
    }

    fn forget_preview(&mut self, request_id: u64, cx: &mut HostCx) {
        let Some(pending) = self.previews.pending.remove(&request_id) else {
            return;
        };
        let Some((client, _)) = self.previews.clients.get(&pending.client_id) else {
            return;
        };
        cx.emit(HostEvent::Domain(EventEnvelope {
            request_id: None,
            topic: Topic::Preview {
                client: client.clone(),
            },
            event: ServerEvent::PreviewRequestCancelled { request_id },
        }));
    }

    /// The thread's turn was interrupted: the preview calls it made are
    /// answered no more, which closes them for the harness and the client.
    pub(crate) fn cancel_previews_of(&mut self, session_id: &str) {
        for pending in self.previews.pending.values() {
            if pending.session_id == session_id {
                pending.reply.close();
            }
        }
    }

    pub(crate) fn resolve_preview(
        &mut self,
        request_id: u64,
        response: Result<tcode_protocol::PreviewResponse, String>,
        cx: &mut HostCx,
    ) {
        let Some(pending) = self.previews.pending.remove(&request_id) else {
            return;
        };
        let response = match (pending.kind, response) {
            (RequestKind::Status, Ok(tcode_protocol::PreviewResponse::Json(mut value))) => {
                if let (Some(object), Some(session)) = (
                    value.as_object_mut(),
                    self.previews.sessions.get(&pending.session_id),
                ) {
                    object.insert(
                        "userActedSinceSnapshot".into(),
                        session.user_acted_since_snapshot.into(),
                    );
                    if let Some(renderer) = &session.renderer {
                        object.insert("renderedOn".into(), renderer.name.clone().into());
                    }
                }
                Ok(tcode_protocol::PreviewResponse::Json(value))
            }
            (RequestKind::Snapshot, Ok(response)) => {
                if let Some(session) = self.previews.sessions.get_mut(&pending.session_id)
                    && std::mem::take(&mut session.user_acted_since_snapshot)
                {
                    self.publish_preview_session(&pending.session_id, cx);
                }
                Ok(response)
            }
            (_, response) => response,
        };
        let _ = pending.reply.try_send(response);
    }

    /// A client began to offer rendering, or renamed itself.
    pub(super) fn preview_client_subscribed(&mut self, client: &PreviewClient, cx: &mut HostCx) {
        let seen = self.previews.tick();
        let renamed = self
            .previews
            .clients
            .insert(client.id.clone(), (client.clone(), seen))
            .is_some_and(|(previous, _)| previous != *client);
        if !renamed {
            return;
        }
        let rendered: Vec<String> = self
            .previews
            .sessions
            .iter_mut()
            .filter(|(_, session)| {
                session
                    .renderer
                    .as_ref()
                    .is_some_and(|renderer| renderer.id == client.id)
            })
            .map(|(id, session)| {
                session.renderer = Some(client.clone());
                id.clone()
            })
            .collect();
        for session_id in rendered {
            self.publish_preview_session(&session_id, cx);
        }
    }

    /// A client's preview channel closed: it renders nothing any more, and
    /// requests waiting on it fail now.
    pub(super) fn preview_client_unsubscribed(&mut self, client: &PreviewClient, cx: &mut HostCx) {
        // A renamed client subscribed under its new name first.
        if self
            .previews
            .clients
            .get(&client.id)
            .is_none_or(|(current, _)| current != client)
        {
            return;
        }
        self.previews.clients.remove(&client.id);
        let released: Vec<String> = self
            .previews
            .sessions
            .iter_mut()
            .filter(|(_, session)| {
                session
                    .renderer
                    .as_ref()
                    .is_some_and(|renderer| renderer.id == client.id)
            })
            .map(|(id, session)| {
                session.renderer = None;
                id.clone()
            })
            .collect();
        for session_id in released {
            self.publish_preview_session(&session_id, cx);
        }
        let orphaned: Vec<u64> = self
            .previews
            .pending
            .iter()
            .filter(|(_, pending)| pending.client_id == client.id)
            .map(|(id, _)| *id)
            .collect();
        for request_id in orphaned {
            self.resolve_preview(
                request_id,
                Err(format!(
                    "the preview was rendered on {}, which went away; the next preview \
                     call opens the page on another client",
                    client.name
                )),
                cx,
            );
        }
    }

    pub(crate) fn show_preview_here(
        &mut self,
        session_id: &str,
        client_id: &str,
        take_over: bool,
        cx: &mut HostCx,
    ) -> Result<(), ProtocolError> {
        let seen = self.previews.tick();
        let Some((client, last_seen)) = self.previews.clients.get_mut(client_id) else {
            return Err(ProtocolError {
                code: "preview_unavailable".into(),
                message: "this client does not offer a preview browser".into(),
            });
        };
        *last_seen = seen;
        let client = client.clone();
        if take_over || self.previews.renderer_of(session_id).is_none() {
            self.render_preview_on(session_id, client, cx);
        }
        Ok(())
    }

    pub(crate) fn report_preview_tabs(
        &mut self,
        session_id: &str,
        client_id: &str,
        tabs: Vec<PreviewTab>,
        cx: &mut HostCx,
    ) {
        if let Some(session) = self.rendered_by(session_id, client_id)
            && session.tabs != tabs
        {
            session.tabs = tabs;
            self.publish_preview_session(session_id, cx);
        }
    }

    pub(crate) fn report_preview_input(
        &mut self,
        session_id: &str,
        client_id: &str,
        cx: &mut HostCx,
    ) {
        if let Some(session) = self.rendered_by(session_id, client_id)
            && !std::mem::replace(&mut session.user_acted_since_snapshot, true)
        {
            self.publish_preview_session(session_id, cx);
        }
    }

    fn rendered_by(&mut self, session_id: &str, client_id: &str) -> Option<&mut PreviewSession> {
        self.previews
            .sessions
            .get_mut(session_id)
            .filter(|session| {
                session
                    .renderer
                    .as_ref()
                    .is_some_and(|renderer| renderer.id == client_id)
            })
    }

    /// Hand `session_id`'s rendering to `client`; the client rendering it
    /// before lets its page go.
    fn render_preview_on(&mut self, session_id: &str, client: PreviewClient, cx: &mut HostCx) {
        let session = self
            .previews
            .sessions
            .entry(session_id.to_owned())
            .or_insert_with(|| PreviewSession {
                session_id: session_id.to_owned(),
                ..PreviewSession::default()
            });
        let previous = session.renderer.replace(client.clone());
        if previous.as_ref() == Some(&client) {
            return;
        }
        if let Some((previous, _)) =
            previous.and_then(|previous| self.previews.clients.get(&previous.id))
        {
            self.publish_preview_rendering(&previous.clone(), cx);
        }
        self.publish_preview_rendering(&client, cx);
        self.publish_preview_session(session_id, cx);
    }

    pub(super) fn preview_rendering_snapshot(&self, client: &PreviewClient) -> Vec<PreviewSession> {
        self.previews
            .sessions
            .values()
            .filter(|session| {
                session
                    .renderer
                    .as_ref()
                    .is_some_and(|renderer| renderer.id == client.id)
            })
            .cloned()
            .collect()
    }

    pub(super) fn preview_session_snapshot(&self, session_id: &str) -> PreviewSession {
        self.previews
            .sessions
            .get(session_id)
            .cloned()
            .unwrap_or_else(|| PreviewSession {
                session_id: session_id.to_owned(),
                ..PreviewSession::default()
            })
    }

    fn publish_preview_rendering(&self, client: &PreviewClient, cx: &mut HostCx) {
        cx.emit(HostEvent::Domain(EventEnvelope {
            request_id: None,
            topic: Topic::Preview {
                client: client.clone(),
            },
            event: ServerEvent::PreviewRenderingReplaced(self.preview_rendering_snapshot(client)),
        }));
    }

    fn publish_preview_session(&self, session_id: &str, cx: &mut HostCx) {
        let topic = Topic::PreviewSession {
            session_id: session_id.to_owned(),
        };
        if self.subscriptions.contains(&topic) {
            cx.emit(HostEvent::Domain(EventEnvelope {
                request_id: None,
                topic,
                event: ServerEvent::PreviewSessionReplaced(
                    self.preview_session_snapshot(session_id),
                ),
            }));
        }
    }
}
