use std::{
    borrow::Cow, cell::RefCell, collections::VecDeque, path::PathBuf, rc::Rc, time::Duration,
};

use gpui::{
    App, AppContext as _, ApplicationHandle, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    WindowOptions, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Root, StyledExt as _,
    button::{Button, ButtonVariants as _},
    input::{Input, InputState},
};
use sync_client::{Client, ClientConfig};
use sync_protocol::{
    ApprovalDecision, ClientFrame, ClientInfo, HostFrame, SessionCommand, SessionSummary,
};
use tcode_ui::{ChatReadModel, ChatSessionReadModel, ChatView};
use wasm_bindgen::{JsCast as _, closure::Closure, prelude::*};
use web_sys::{CloseEvent, Event, MessageEvent, UrlSearchParams, WebSocket};

thread_local! {
    // WebPlatform::run returns after installing browser callbacks. This handle
    // deliberately owns GPUI for the rest of the page lifetime.
    static APPLICATION: RefCell<Option<ApplicationHandle>> = const { RefCell::new(None) };
}

enum TransportEvent {
    Open,
    Text(String),
    Error,
    Closed(String),
}

struct PageConfig {
    url: Option<String>,
    token: Option<String>,
}

struct WebApp {
    client: Client,
    socket: Option<WebSocket>,
    incoming: Rc<RefCell<VecDeque<TransportEvent>>>,
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

impl WebApp {
    fn new(config: PageConfig, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let incoming = Rc::new(RefCell::new(VecDeque::new()));
        let client_id = format!("tcode-web-{}", js_sys::Date::now() as u64);
        let mut client = Client::new(ClientConfig {
            client: ClientInfo {
                client_id,
                display_name: "tcode web".into(),
                platform: "web".into(),
                app_version: env!("CARGO_PKG_VERSION").into(),
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
            (Some(url), Some(_)) => match connect(&url, &hello, incoming.clone()) {
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
                if incoming.borrow().is_empty() {
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
        let events: Vec<_> = self.incoming.borrow_mut().drain(..).collect();
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
            .and_then(|text| socket.send_with_str(&text).map_err(js_error));
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
                match connect(&url, &hello, incoming) {
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

impl Render for WebApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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

        div()
            .flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w(px(280.))
                    .h_full()
                    .flex_shrink_0()
                    .border_r_1()
                    .border_color(cx.theme().border)
                    .p_3()
                    .gap_3()
                    .child(div().text_lg().font_semibold().child("tcode"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(self.status.clone()),
                    )
                    .child(sidebar),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(div().flex_1().min_h_0().child(self.chat.clone()))
                    .children(self.render_approvals(cx))
                    .child(self.render_composer(cx)),
            )
    }
}

impl WebApp {
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

fn connect(
    url: &str,
    hello: &str,
    incoming: Rc<RefCell<VecDeque<TransportEvent>>>,
) -> Result<WebSocket, JsValue> {
    let socket = WebSocket::new(url)?;

    let on_open = Closure::<dyn FnMut(Event)>::new({
        let socket = socket.clone();
        let hello = hello.to_owned();
        let incoming = incoming.clone();
        move |_| {
            if socket.send_with_str(&hello).is_err() {
                incoming.borrow_mut().push_back(TransportEvent::Error);
            } else {
                incoming.borrow_mut().push_back(TransportEvent::Open);
            }
        }
    });
    socket.set_onopen(Some(on_open.as_ref().unchecked_ref()));
    on_open.forget();

    let on_message = Closure::<dyn FnMut(MessageEvent)>::new({
        let incoming = incoming.clone();
        move |event: MessageEvent| {
            if let Some(text) = event.data().as_string() {
                incoming.borrow_mut().push_back(TransportEvent::Text(text));
            }
        }
    });
    socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    on_message.forget();

    let on_error = Closure::<dyn FnMut(Event)>::new({
        let incoming = incoming.clone();
        move |_| incoming.borrow_mut().push_back(TransportEvent::Error)
    });
    socket.set_onerror(Some(on_error.as_ref().unchecked_ref()));
    on_error.forget();

    let on_close = Closure::<dyn FnMut(CloseEvent)>::new(move |event: CloseEvent| {
        incoming
            .borrow_mut()
            .push_back(TransportEvent::Closed(event.reason()));
    });
    socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));
    on_close.forget();

    Ok(socket)
}

fn page_config() -> Result<PageConfig, JsValue> {
    let window = web_sys::window().ok_or_else(|| JsValue::from_str("window is unavailable"))?;
    let search = window.location().search()?;
    let params = UrlSearchParams::new_with_str(&search)?;
    Ok(PageConfig {
        url: params.get("url").filter(|value| !value.is_empty()),
        token: params.get("token").filter(|value| !value.is_empty()),
    })
}

/// Delivery ids only need to be unique within this page's lifetime, since the
/// host correlates them per session and the client forgets them once accepted.
fn next_delivery_id() -> u64 {
    thread_local! {
        static NEXT: RefCell<u64> = RefCell::new(js_sys::Date::now() as u64);
    }
    NEXT.with(|next| {
        let mut next = next.borrow_mut();
        *next = next.wrapping_add(1);
        *next
    })
}

fn js_error(value: JsValue) -> String {
    value.as_string().unwrap_or_else(|| format!("{value:?}"))
}

#[wasm_bindgen(start)]
pub fn start() -> Result<(), JsValue> {
    gpui_platform::web_init();
    let config = page_config()?;
    let app = gpui_platform::application().with_assets(gpui_component_assets::Assets::new(
        "https://longbridge.github.io/gpui-component/gallery/",
    ));
    let handle = app.run_embedded(move |cx: &mut App| {
        gpui_component::init(cx);
        tcode_ui::markdown::init(cx);
        let fonts: Vec<Cow<'static, [u8]>> = vec![
            Cow::Borrowed(tcode_ui::assets::DM_SANS),
            Cow::Borrowed(tcode_ui::assets::LILEX_REGULAR),
            Cow::Borrowed(tcode_ui::assets::LILEX_BOLD),
            Cow::Borrowed(tcode_ui::assets::LILEX_ITALIC),
            Cow::Borrowed(tcode_ui::assets::LILEX_BOLD_ITALIC),
        ];
        cx.text_system()
            .add_fonts(fonts)
            .expect("bundled web fonts must load");
        cx.open_window(WindowOptions::default(), |window, cx| {
            let app = cx.new(|cx| WebApp::new(config, window, cx));
            cx.new(|cx| Root::new(app, window, cx).bordered(false))
        })
        .expect("the GPUI web window must open");
        cx.activate(true);
    });
    APPLICATION.with(|slot| {
        *slot.borrow_mut() = Some(handle);
    });
    Ok(())
}
