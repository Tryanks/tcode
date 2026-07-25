//! The tcode remote client, shared by every shell that hosts it.
//!
//! Web, Android and iOS mount this same view. They differ only in how bytes
//! reach a host — a browser `WebSocket`, or a native one — which is what
//! [`TransportFactory`] abstracts. Everything above that seam is identical, so
//! a fix to the session list or the approval panel lands on all three at once.
//!
//! Portable by construction: `sync-client` folds the protocol, `tcode-ui`'s
//! `portable` feature supplies the timeline rendering, and nothing here touches
//! a filesystem or a process — which is the whole reason a phone can run it.

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use gpui::{
    AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    input::{Input, InputState},
};
use sync_client::{Client, ClientConfig};
use sync_protocol::{
    ApprovalDecision, ClientFrame, ClientInfo, HostFrame, SessionCommand, SessionSummary,
};
use tcode_ui::{ChatReadModel, ChatSessionReadModel, ChatView};

// Native shells (Android, iOS) share one WebSocket implementation; the browser
// supplies its own, since it has no sockets of this kind.
#[cfg(not(target_family = "wasm"))]
pub mod native_transport;

/// Monotonic within this process. `SendTurn` carries the id and `TurnAccepted`
/// echoes it back, so two turns sharing one would be indistinguishable to the
/// acknowledgement tracking — which is what makes a reconnect able to tell what
/// actually landed.
fn next_delivery_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// A live connection to a host, owned by the shell.
pub trait Transport {
    /// Send one encoded frame. The error is surfaced to the user, so it should
    /// say what went wrong rather than just that something did.
    fn send(&self, text: &str) -> Result<(), String>;
}

/// How a shell opens connections.
///
/// Taken as a factory rather than a single connection because the client
/// reconnects on its own: a dropped socket is normal on a phone, and the
/// reconnect logic belongs here — with the cursors — rather than in three
/// separate shells.
pub trait TransportFactory: Send + Sync {
    /// Open a connection, send `hello` once it is ready, and push every
    /// incoming frame onto `inbox`.
    fn connect(&self, url: &str, hello: &str, inbox: Inbox) -> Result<Box<dyn Transport>, String>;
}

/// Where a transport delivers what it receives.
///
/// A queue rather than a callback: transports deliver on whatever thread or
/// event loop they live on, while GPUI state may only be touched from its own
/// context, so the client drains this on a timer instead.
///
/// `Arc<Mutex<_>>` rather than `Rc<RefCell<_>>` because a native transport
/// pushes from its socket thread. The browser pays nothing for that — it is
/// single-threaded, so the lock is never contended — and it is the difference
/// between one shared client and two.
pub type Inbox = Arc<Mutex<VecDeque<TransportEvent>>>;

/// What a shell reports about itself, and how to reach the host.
pub struct ClientIdentity {
    pub client_id: String,
    pub display_name: String,
    pub platform: String,
    pub app_version: String,
}

/// True when nothing is waiting. Split out so the poison handling lives in one
/// place: a panicked producer must not stop the client draining what it already
/// received.
fn inbox_is_empty(inbox: &Inbox) -> bool {
    lock_inbox(inbox).is_empty()
}

fn lock_inbox(inbox: &Inbox) -> std::sync::MutexGuard<'_, VecDeque<TransportEvent>> {
    inbox
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub enum TransportEvent {
    Open,
    Text(String),
    Error,
    Closed(String),
}

pub struct ClientConnection {
    pub url: Option<String>,
    pub token: Option<String>,
    pub identity: ClientIdentity,
}

pub struct ClientApp {
    client: Client,
    transport: Arc<dyn TransportFactory>,
    socket: Option<Box<dyn Transport>>,
    incoming: Inbox,
    sessions: Vec<SessionSummary>,
    selected: Option<String>,
    chat: Entity<ChatView>,
    composer: Entity<InputState>,
    status: SharedString,
    /// Retained so a dropped socket can be re-established. Mobile and laptop
    /// networks drop connections constantly; a client that cannot reconnect is
    /// a client that works once.
    url: Option<String>,
    hello: String,
    /// Consecutive failed attempts, for backoff. Reset on a successful open.
    reconnect_attempts: u32,
    /// How many rejections had been reported the last time we looked, so a new
    /// one can be surfaced. The host only sends these when a command could not
    /// be applied, and staying silent would leave a turn rendered that never ran.
    rejections_seen: usize,
    _poll: gpui::Task<()>,
    _reconnect: Option<gpui::Task<()>>,
}

impl ClientApp {
    pub fn new(
        config: ClientConnection,
        transport: Arc<dyn TransportFactory>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let incoming: Inbox = Arc::new(Mutex::new(VecDeque::new()));
        let client_id = config.identity.client_id.clone();
        let mut client = Client::new(ClientConfig {
            client: ClientInfo {
                client_id,
                display_name: config.identity.display_name.clone(),
                platform: config.identity.platform.clone(),
                app_version: config.identity.app_version.clone(),
            },
            token: config.token.clone().unwrap_or_default(),
        });
        let hello = client
            .connect()
            .encode()
            .expect("the fixed hello frame must encode");
        let retained_hello = hello.clone();
        let chat = cx.new(|cx| ChatView::from_read_model(ChatReadModel::default(), window, cx));
        let composer =
            cx.new(|cx| InputState::new(window, cx).placeholder("Send a message to this session…"));

        let retained_url = config.url.clone();
        let (socket, status) = match (config.url, config.token) {
            (Some(url), Some(_)) => match transport.connect(&url, &hello, incoming.clone()) {
                Ok(socket) => (Some(socket), "Connecting…".into()),
                Err(error) => (None, format!("WebSocket error: {error:?}").into()),
            },
            _ => (
                None,
                "Add ?url=ws%3A%2F%2FHOST%3APORT%2Fsync&token=TOKEN to this page URL.".into(),
            ),
        };

        let poll = cx.spawn({
            let incoming = incoming.clone();
            async move |this, cx| loop {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                if inbox_is_empty(&incoming) {
                    continue;
                }
                if this
                    .update(cx, |this, cx| this.drain_transport(cx))
                    .is_err()
                {
                    break;
                }
            }
        });

        Self {
            client,
            transport,
            socket,
            incoming,
            sessions: Vec::new(),
            selected: None,
            chat,
            composer,
            status,
            url: retained_url,
            hello: retained_hello,
            reconnect_attempts: 0,
            rejections_seen: 0,
            _poll: poll,
            _reconnect: None,
        }
    }

    fn drain_transport(&mut self, cx: &mut Context<Self>) {
        let events: Vec<_> = lock_inbox(&self.incoming).drain(..).collect();
        for event in events {
            match event {
                TransportEvent::Open => {
                    self.status = "Authenticating…".into();
                    self.reconnect_attempts = 0;
                }
                TransportEvent::Text(text) => self.handle_text(&text, cx),
                TransportEvent::Error => {
                    self.status = "WebSocket transport error".into();
                }
                TransportEvent::Closed(reason) => {
                    self.status = if reason.is_empty() {
                        "Disconnected — reconnecting…".into()
                    } else {
                        format!("Disconnected: {reason} — reconnecting…").into()
                    };
                    self.schedule_reconnect(cx);
                }
            }
        }
        cx.notify();
    }

    fn handle_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let frame = match HostFrame::decode(text) {
            Ok(frame) => frame,
            Err(error) => {
                self.status = format!("Protocol decode error: {error}").into();
                return;
            }
        };
        let was_ready = self.client.is_ready();
        let is_session_list = matches!(frame, HostFrame::SessionList { .. });
        let affects_selected = matches!(
            &frame,
            HostFrame::Events { session_id, .. } | HostFrame::SessionEnded { session_id, .. }
                if self.selected.as_deref() == Some(session_id)
        );

        let outgoing = self.client.handle(frame);
        self.send_all(outgoing);

        if !was_ready && self.client.is_ready() {
            let host = self
                .client
                .host()
                .map(|host| host.display_name.clone())
                .unwrap_or_else(|| "host".into());
            self.status = format!("Connected to {host}").into();
            self.send(self.client.request_sessions());
        } else if let Some(reason) = self.client.refusal_reason() {
            self.status = format!("Connection refused: {reason:?}").into();
        }

        if is_session_list {
            self.sessions = self.client.sessions().to_vec();
            if self.selected.is_none()
                && let Some(session_id) = self
                    .sessions
                    .first()
                    .map(|session| session.session_id.clone())
            {
                self.select(session_id, cx);
            }
        }
        if affects_selected {
            self.refresh_chat(cx);
        }

        // A rejected command is the one failure the timeline cannot show: the
        // host applied nothing, so no event will ever arrive to explain it.
        let rejections = self.client.command_rejections();
        if rejections.len() > self.rejections_seen {
            if let Some(latest) = rejections.last() {
                self.status = format!("Command rejected: {:?}", latest.reason).into();
            }
            self.rejections_seen = rejections.len();
        }
    }

    fn send_all(&mut self, frames: Vec<ClientFrame>) {
        for frame in frames {
            self.send(frame);
        }
    }

    fn send(&mut self, frame: ClientFrame) {
        let Some(socket) = &self.socket else {
            return;
        };
        let result = frame
            .encode()
            .map_err(|error| error.to_string())
            .and_then(|text| socket.send(&text));
        if let Err(error) = result {
            self.status = format!("WebSocket send error: {error}").into();
        }
    }

    fn select(&mut self, session_id: String, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(&session_id) {
            return;
        }
        if let Some(previous) = self.selected.replace(session_id.clone()) {
            let unsubscribe = self.client.unsubscribe(previous);
            self.send(unsubscribe);
        }
        let subscribe = self.client.subscribe(session_id);
        self.send(subscribe);
        self.refresh_chat(cx);
    }

    /// Reconnect after a backoff, then resume every subscription from the
    /// cursor this client already holds — so a reconnect costs the events that
    /// were missed, not the whole log.
    fn schedule_reconnect(&mut self, cx: &mut Context<Self>) {
        let Some(url) = self.url.clone() else {
            return;
        };
        self.socket = None;
        self.reconnect_attempts = self.reconnect_attempts.saturating_add(1);
        // Capped exponential backoff: a host that is down should not be
        // hammered, but a transient drop should recover in well under a second.
        let delay = Duration::from_millis(250u64 << self.reconnect_attempts.min(6));
        let hello = self.hello.clone();
        let incoming = self.incoming.clone();
        self._reconnect = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |this, cx| {
                match this.transport.connect(&url, &hello, incoming) {
                    Ok(socket) => {
                        this.socket = Some(socket);
                        // The client keeps its cursors across the drop, so the
                        // resubscribe below asks only for the gap.
                        let resume: Vec<ClientFrame> = this
                            .selected
                            .clone()
                            .map(|session_id| vec![this.client.subscribe(session_id)])
                            .unwrap_or_default();
                        this.send_all(resume);
                    }
                    Err(error) => {
                        this.status = format!("Reconnect failed: {error:?}").into();
                        this.schedule_reconnect(cx);
                    }
                }
                cx.notify();
            });
        }));
    }

    /// Send whatever is in the composer as a turn.
    ///
    /// `Client::command` allocates and retains the `delivery_id`; the turn is
    /// not accepted until `AgentEvent::TurnAccepted` echoes it back, which is
    /// what the status line reports. That mechanism predates this client and is
    /// reused rather than duplicated.
    fn send_turn(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected.clone() else {
            return;
        };
        let text = self.composer.read(cx).value().to_string();
        if text.trim().is_empty() {
            return;
        }
        // Monotonic within the page: `Date::now` alone can repeat inside one
        // millisecond, and a repeated delivery id would make two turns
        // indistinguishable to the acknowledgement tracking.
        let delivery_id = next_delivery_id();
        let frame = self.client.command(
            session_id.clone(),
            SessionCommand::SendTurn {
                delivery_id,
                text,
                options: None,
                attachments: Vec::new(),
            },
        );
        self.send(frame);
        self.composer
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.status = "Turn sent — awaiting acceptance…".into();
        cx.notify();
    }

    fn respond_approval(
        &mut self,
        request_id: String,
        decision: ApprovalDecision,
        cx: &mut Context<Self>,
    ) {
        let Some(session_id) = self.selected.clone() else {
            return;
        };
        let frame = self.client.command(
            session_id,
            SessionCommand::RespondApproval {
                request_id,
                decision,
            },
        );
        self.send(frame);
        cx.notify();
    }

    /// Return to the session list on a narrow layout.
    ///
    /// The subscription is left in place: the host keeps streaming, so coming
    /// back is instant and no backfill is re-fetched. Only the selection —
    /// which is presentation state — is cleared.
    fn deselect(&mut self, cx: &mut Context<Self>) {
        self.selected = None;
        cx.notify();
    }

    fn refresh_chat(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected.as_deref() else {
            return;
        };
        let Some(summary) = self
            .sessions
            .iter()
            .find(|summary| summary.session_id == session_id)
        else {
            return;
        };
        let timeline = self
            .client
            .session(session_id)
            .map(|state| state.timeline().clone())
            .unwrap_or_default();
        // SessionSummary intentionally carries a display name rather than a
        // meaningless host-absolute path. Relative file labels still render.
        let cwd = PathBuf::from(summary.project.as_deref().unwrap_or("."));
        let model = ChatReadModel::new(ChatSessionReadModel::new(
            &summary.session_id,
            &summary.title,
            cwd,
            summary.provider,
            timeline,
        ));
        self.chat
            .update(cx, |chat, cx| chat.set_read_model(model, cx));
    }
}

impl Render for ClientApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // One breakpoint, not a scale factor. Below it a phone cannot show two
        // panes at once — a 280px sidebar eats 72% of a 390pt screen — so the
        // shell switches from side-by-side to one-at-a-time rather than
        // shrinking both into uselessness.
        //
        // 640 logical pixels is where a sidebar plus a readable chat column
        // stops fitting, not a device class: a narrow desktop window gets the
        // same treatment, which is the honest generalisation and also makes the
        // layout testable without a phone.
        let narrow = window.viewport_size().width < px(640.0);
        let showing_chat = self.selected.is_some();
        let sidebar =
            self.sessions
                .iter()
                .fold(div().flex().flex_col().gap_1(), |column, session| {
                    let session_id = session.session_id.clone();
                    let selected = self.selected.as_deref() == Some(session_id.as_str());
                    let meta: SharedString = format!(
                        "{:?}{}{}",
                        session.provider,
                        if session.working { " · working" } else { "" },
                        if session.awaiting_approval {
                            " · awaiting approval"
                        } else {
                            ""
                        }
                    )
                    .into();
                    column.child(
                        div()
                            .id(session_id.clone())
                            .flex()
                            .flex_col()
                            .gap_1()
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .cursor_pointer()
                            .when(selected, |item| item.bg(cx.theme().accent.opacity(0.18)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.select(session_id.clone(), cx);
                            }))
                            .child(
                                div()
                                    .text_sm()
                                    .font_semibold()
                                    .text_color(cx.theme().foreground)
                                    .child(session.title.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(meta),
                            ),
                    )
                });

        let session_list = div()
            .flex()
            .flex_col()
            .h_full()
            .when(!narrow, |column| {
                column
                    .w(px(280.))
                    .flex_shrink_0()
                    .border_r_1()
                    .border_color(cx.theme().border)
            })
            .when(narrow, |column| column.size_full())
            .p_3()
            .gap_3()
            .child(div().text_lg().font_semibold().child("tcode"))
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(self.status.clone()),
            )
            .child(sidebar);

        let conversation = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            // On a phone the only way back to the list is an explicit control:
            // there is no second pane to click, and hiding it behind a gesture
            // would make the app's one navigation action undiscoverable.
            .when(narrow, |column| {
                column.child(
                    div()
                        .flex()
                        .p_2()
                        .border_b_1()
                        .border_color(cx.theme().border)
                        .child(
                            Button::new("back-to-sessions")
                                .ghost()
                                .label("‹ Sessions")
                                .on_click(cx.listener(|this, _, _, cx| this.deselect(cx))),
                        ),
                )
            })
            .child(div().flex_1().min_h_0().child(self.chat.clone()))
            .children(self.render_approvals(cx))
            .child(self.render_composer(cx));

        div()
            .flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            // Narrow shows exactly one pane, chosen by whether a session is
            // open; wide shows both. Collected first because each pane can only
            // be placed once.
            .children(if narrow {
                if showing_chat {
                    vec![conversation.into_any_element()]
                } else {
                    vec![session_list.into_any_element()]
                }
            } else {
                vec![
                    session_list.into_any_element(),
                    conversation.into_any_element(),
                ]
            })
    }
}

impl ClientApp {
    /// Pending approvals, rendered above the composer rather than inline in the
    /// timeline.
    ///
    /// Approving a command while away from the desk is the reason a remote
    /// client exists, so this must be impossible to scroll past. The requests
    /// come from the synced `Timeline`, which the shared reducer already
    /// maintains — the browser does not track them itself.
    fn render_approvals(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let Some(session_id) = self.selected.as_deref() else {
            return Vec::new();
        };
        let Some(state) = self.client.session(session_id) else {
            return Vec::new();
        };
        state
            .timeline()
            .pending_approvals
            .iter()
            .map(|request| {
                let id = request.id.clone();
                let approve = id.clone();
                let deny = id.clone();
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .m_3()
                    .p_3()
                    .rounded_lg()
                    .border_1()
                    .border_color(cx.theme().warning)
                    .bg(cx.theme().warning.opacity(0.08))
                    .child(div().text_sm().font_semibold().child("Approval needed"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(SharedString::from(format!("{:?}", request.kind))),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                Button::new(SharedString::from(format!("approve-{id}")))
                                    .primary()
                                    .label("Approve")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.respond_approval(
                                            approve.clone(),
                                            ApprovalDecision::Approve,
                                            cx,
                                        );
                                    })),
                            )
                            .child(
                                Button::new(SharedString::from(format!("deny-{id}")))
                                    .danger()
                                    .label("Deny")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.respond_approval(
                                            deny.clone(),
                                            ApprovalDecision::Deny,
                                            cx,
                                        );
                                    })),
                            ),
                    )
                    .into_any_element()
            })
            .collect()
    }

    fn render_composer(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let live = self.selected.is_some() && self.socket.is_some();
        div()
            .flex()
            .gap_2()
            .p_3()
            .border_t_1()
            .border_color(cx.theme().border)
            .child(div().flex_1().min_w_0().child(Input::new(&self.composer)))
            .child(
                Button::new("send")
                    .primary()
                    .label("Send")
                    .disabled(!live)
                    .on_click(cx.listener(|this, _, window, cx| this.send_turn(window, cx))),
            )
            .into_any_element()
    }
}
