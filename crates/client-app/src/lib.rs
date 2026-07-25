//! The tcode remote client, shared by every shell that hosts it.
//!
//! Web, Android and iOS mount this same view. They differ only in how bytes
//! reach a host — a browser `WebSocket`, or a native one — which is what
//! [`TransportFactory`] abstracts, and in where a token is kept between loads,
//! which is what [`ConnectionStore`] abstracts. Everything above those two
//! seams is identical, so a fix to pairing, the session list, or the approval
//! panel lands on all three at once.
//!
//! A device joins a host once, by typing a short pairing code the host shows on
//! startup; the token that buys is stored and the code is never needed again.
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
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    input::{Input, InputEvent, InputState},
};
use sync_client::{Client, ClientConfig};
use sync_protocol::{
    AgentEvent, ApprovalDecision, ClientFrame, ClientInfo, HostFrame, RejectedCommand,
    SessionCommand, SessionSummary,
};
use tcode_ui::{ChatReadModel, ChatSessionReadModel, ChatView};

pub mod pairing;
pub mod storage;

#[cfg(not(target_family = "wasm"))]
pub use storage::FileStore;
pub use storage::{ConnectionStore, MemoryStore, StoredConnection};

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

fn turn_command(delivery_id: u64, text: String) -> SessionCommand {
    SessionCommand::SendTurn {
        delivery_id,
        text,
        options: None,
        attachments: Vec::new(),
    }
}

fn approval_command(request_id: String, decision: ApprovalDecision) -> SessionCommand {
    SessionCommand::RespondApproval {
        request_id,
        decision,
    }
}

fn provider_approval_decision(option_id: String) -> ApprovalDecision {
    ApprovalDecision::Option(option_id)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LocalTurnStatus {
    AwaitingAcceptance,
    Rejected(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalTurn {
    session_id: String,
    delivery_id: u64,
    text: String,
    status: LocalTurnStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CommandAttemptKind {
    Turn(u64),
    Approval(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandAttempt {
    session_id: String,
    kind: CommandAttemptKind,
}

#[derive(Default)]
struct CommandUiState {
    attempts: VecDeque<CommandAttempt>,
    turns: Vec<LocalTurn>,
}

impl CommandUiState {
    fn sent_turn(&mut self, session_id: String, delivery_id: u64, text: String) {
        self.attempts.push_back(CommandAttempt {
            session_id: session_id.clone(),
            kind: CommandAttemptKind::Turn(delivery_id),
        });
        self.turns.push(LocalTurn {
            session_id,
            delivery_id,
            text,
            status: LocalTurnStatus::AwaitingAcceptance,
        });
    }

    fn sent_approval(&mut self, session_id: String, request_id: String) {
        self.attempts.push_back(CommandAttempt {
            session_id,
            kind: CommandAttemptKind::Approval(request_id),
        });
    }

    fn accepted_turn(&mut self, delivery_id: u64) {
        self.attempts.retain(
            |attempt| !matches!(attempt.kind, CommandAttemptKind::Turn(id) if id == delivery_id),
        );
        // The host records the canonical user-message event before acknowledging
        // delivery, so keeping the optimistic copy would render the turn twice.
        self.turns.retain(|turn| turn.delivery_id != delivery_id);
    }

    fn resolved_approvals<'a>(
        &mut self,
        session_id: &str,
        pending_request_ids: impl Iterator<Item = &'a str>,
    ) {
        let pending: Vec<&str> = pending_request_ids.collect();
        self.attempts.retain(|attempt| {
            attempt.session_id != session_id
                || !matches!(
                    &attempt.kind,
                    CommandAttemptKind::Approval(request_id)
                        if !pending.contains(&request_id.as_str())
                )
        });
    }

    fn rejected(&mut self, session_id: &str, command: Option<&RejectedCommand>, reason: String) {
        let index = match command {
            Some(RejectedCommand::Turn { delivery_id }) => {
                self.attempts.iter().position(|attempt| {
                    attempt.session_id == session_id
                        && matches!(
                            attempt.kind,
                            CommandAttemptKind::Turn(id) if id == *delivery_id
                        )
                })
            }
            Some(RejectedCommand::Approval { request_id }) => {
                self.attempts.iter().position(|attempt| {
                    attempt.session_id == session_id
                        && matches!(
                            &attempt.kind,
                            CommandAttemptKind::Approval(id) if id == request_id
                        )
                })
            }
            // Hosts predating correlation leave no safer choice than send order;
            // consuming one attempt prevents optimistic state from hanging forever.
            None => self
                .attempts
                .iter()
                .position(|attempt| attempt.session_id == session_id),
        };
        let Some(index) = index else {
            return;
        };
        let Some(attempt) = self.attempts.remove(index) else {
            return;
        };
        if let CommandAttemptKind::Turn(delivery_id) = attempt.kind
            && let Some(turn) = self
                .turns
                .iter_mut()
                .find(|turn| turn.delivery_id == delivery_id)
        {
            turn.status = LocalTurnStatus::Rejected(reason);
        }
    }
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

/// What a shell reports about itself.
pub struct ClientIdentity {
    pub client_id: String,
    pub display_name: String,
    pub platform: String,
    pub app_version: String,
}

impl ClientIdentity {
    fn info(&self) -> ClientInfo {
        ClientInfo {
            client_id: self.client_id.clone(),
            display_name: self.display_name.clone(),
            platform: self.platform.clone(),
            app_version: self.app_version.clone(),
        }
    }
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

/// What a shell supplies to start the client.
pub struct ClientConnection {
    pub identity: ClientIdentity,
    /// Optional prefill for the host-address field. A URL is not a secret, so a
    /// shell may pass one taken from its page URL; the token is never taken
    /// that way.
    pub url: Option<String>,
    /// Where the token earned by pairing is kept between loads.
    pub store: Arc<dyn ConnectionStore>,
}

/// Which screen the client is on.
///
/// A device with no stored token starts on `Connect`; one that has paired goes
/// straight to `Session`. There is no third state — a running session is the
/// only thing worth showing once a token exists.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Collecting a host address and a pairing code. `pairing` is true from the
    /// moment Connect is pressed until a `Paired` or a refusal comes back, so
    /// the button can disable itself while a code is in flight.
    Connect {
        pairing: bool,
    },
    Session,
}

pub struct ClientApp {
    identity: ClientIdentity,
    store: Arc<dyn ConnectionStore>,
    client: Client,
    transport: Arc<dyn TransportFactory>,
    socket: Option<Box<dyn Transport>>,
    incoming: Inbox,
    phase: Phase,
    // Connect screen.
    url_input: Entity<InputState>,
    code_input: Entity<InputState>,
    /// In-progress or refusal text on the connect screen. Held apart from
    /// `status` so returning to pairing does not inherit a session's last line.
    connect_status: SharedString,
    // Session screen.
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
    command_ui: CommandUiState,
    _subscriptions: Vec<Subscription>,
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
        let ClientConnection {
            identity,
            url: url_prefill,
            store,
        } = config;
        let incoming: Inbox = Arc::new(Mutex::new(VecDeque::new()));
        let chat = cx.new(|cx| ChatView::from_read_model(ChatReadModel::default(), window, cx));
        let composer =
            cx.new(|cx| InputState::new(window, cx).placeholder("Send a message to this session…"));
        let url_input = cx.new(|cx| InputState::new(window, cx).placeholder("ws://host:port/sync"));
        let code_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("6-character code from the host"));

        let subscriptions = vec![
            // Upper-case the code as it is typed. A code is read off one screen
            // and typed into another; failing that on caps reads as "pairing is
            // broken" rather than as a typo. `set_value` suppresses its own
            // Change event, so this does not recurse.
            cx.subscribe_in(
                &code_input,
                window,
                |this, input, event, window, cx| match event {
                    InputEvent::Change => {
                        let raw = input.read(cx).value().to_string();
                        let upper = raw.to_ascii_uppercase();
                        if upper != raw {
                            input.update(cx, |state, cx| state.set_value(upper, window, cx));
                        }
                    }
                    InputEvent::PressEnter { .. } => this.start_pairing(cx),
                    _ => {}
                },
            ),
            cx.subscribe_in(&url_input, window, |this, _input, event, _window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.start_pairing(cx);
                }
            }),
        ];

        let stored = storage::usable_connection(store.load());
        if let Some(prefill) = stored
            .as_ref()
            .map(|connection| connection.url.clone())
            .or(url_prefill)
        {
            url_input.update(cx, |state, cx| state.set_value(prefill, window, cx));
        }

        // A stored token skips the connect screen entirely and opens the
        // session; its absence leaves the client on Connect awaiting a code.
        let mut client = Client::new(ClientConfig {
            client: identity.info(),
            token: String::new(),
        });
        let mut phase = Phase::Connect { pairing: false };
        let mut retained_hello = String::new();
        let mut socket = None;
        let mut retained_url = None;
        let mut status = SharedString::default();
        if let Some(connection) = stored {
            client = Client::new(ClientConfig {
                client: identity.info(),
                token: connection.token,
            });
            retained_hello = client
                .connect()
                .encode()
                .expect("the fixed hello frame must encode");
            retained_url = Some(connection.url.clone());
            phase = Phase::Session;
            status = "Connecting…".into();
            match transport.connect(&connection.url, &retained_hello, incoming.clone()) {
                Ok(open) => socket = Some(open),
                Err(error) => {
                    status = format!("Could not reach {}: {error}", connection.url).into()
                }
            }
        }

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
            identity,
            store,
            client,
            transport,
            socket,
            incoming,
            phase,
            url_input,
            code_input,
            connect_status: SharedString::default(),
            sessions: Vec::new(),
            selected: None,
            chat,
            composer,
            status,
            url: retained_url,
            hello: retained_hello,
            reconnect_attempts: 0,
            rejections_seen: 0,
            command_ui: CommandUiState::default(),
            _subscriptions: subscriptions,
            _poll: poll,
            _reconnect: None,
        }
    }

    /// Trade the typed code for a token: open a connection whose first frame is
    /// `Pair` rather than `Hello`.
    fn start_pairing(&mut self, cx: &mut Context<Self>) {
        if matches!(self.phase, Phase::Connect { pairing: true }) {
            return;
        }
        let url = self.url_input.read(cx).value().trim().to_string();
        let code = pairing::normalize_code(&self.code_input.read(cx).value());
        if url.is_empty() {
            self.connect_status = "Enter the host address.".into();
            cx.notify();
            return;
        }
        if !pairing::is_complete_code(&code) {
            self.connect_status =
                format!("A pairing code is {} characters.", pairing::CODE_LEN).into();
            cx.notify();
            return;
        }

        // A pairing client carries no token — the code stands in for one — so it
        // must not fall back to any stored secret. The `Pair` frame is the
        // connection's first, before any `Hello`.
        self.client = Client::new(ClientConfig {
            client: self.identity.info(),
            token: String::new(),
        });
        let pair = self
            .client
            .pair(code)
            .encode()
            .expect("the pair frame must encode");
        self.url = Some(url.clone());
        self.socket = None;
        lock_inbox(&self.incoming).clear();
        self.phase = Phase::Connect { pairing: true };
        self.connect_status = "Pairing…".into();
        match self.transport.connect(&url, &pair, self.incoming.clone()) {
            Ok(socket) => self.socket = Some(socket),
            Err(error) => {
                self.connect_status = format!("Could not reach {url}: {error}").into();
                self.phase = Phase::Connect { pairing: false };
            }
        }
        cx.notify();
    }

    /// A code was accepted: persist the token so the code is never needed again,
    /// then open the session with an ordinary `Hello`.
    fn complete_pairing(&mut self, token: String, cx: &mut Context<Self>) {
        let url = self.url.clone().unwrap_or_default();
        self.store.save(&StoredConnection {
            url: url.clone(),
            token: token.clone(),
        });
        self.open_session(url, token, cx);
    }

    /// Open (or reopen) the session connection with a token in hand.
    ///
    /// Shared by first pairing and by a stored-token start: both recreate the
    /// client with the token, cache the `Hello` for reconnects, and connect.
    fn open_session(&mut self, url: String, token: String, cx: &mut Context<Self>) {
        self.client = Client::new(ClientConfig {
            client: self.identity.info(),
            token,
        });
        self.hello = self
            .client
            .connect()
            .encode()
            .expect("the fixed hello frame must encode");
        self.url = Some(url.clone());
        self.phase = Phase::Session;
        self.reconnect_attempts = 0;
        self.rejections_seen = 0;
        self.sessions.clear();
        self.selected = None;
        self.command_ui = CommandUiState::default();
        // Drop the pairing socket and discard anything it left queued before the
        // session socket opens, so a pairing-socket close cannot be mistaken for
        // a session drop.
        self.socket = None;
        lock_inbox(&self.incoming).clear();
        self.status = "Connecting…".into();
        match self
            .transport
            .connect(&url, &self.hello, self.incoming.clone())
        {
            Ok(socket) => self.socket = Some(socket),
            Err(error) => self.status = format!("Could not reach {url}: {error}").into(),
        }
        cx.notify();
    }

    /// Forget the stored token and return to the connect screen.
    fn sign_out(&mut self, cx: &mut Context<Self>) {
        self.store.clear();
        self.socket = None;
        self._reconnect = None;
        lock_inbox(&self.incoming).clear();
        self.client = Client::new(ClientConfig {
            client: self.identity.info(),
            token: String::new(),
        });
        self.sessions.clear();
        self.selected = None;
        self.command_ui = CommandUiState::default();
        self.phase = Phase::Connect { pairing: false };
        self.status = SharedString::default();
        // The address field keeps its value: signing out is not a reason to
        // make the user retype where the host is.
        self.connect_status = "Signed out.".into();
        cx.notify();
    }

    fn drain_transport(&mut self, cx: &mut Context<Self>) {
        let events: Vec<_> = lock_inbox(&self.incoming).drain(..).collect();
        for event in events {
            match event {
                TransportEvent::Open => match self.phase {
                    Phase::Session => {
                        self.status = "Authenticating…".into();
                        self.reconnect_attempts = 0;
                    }
                    Phase::Connect { .. } => self.connect_status = "Pairing…".into(),
                },
                TransportEvent::Text(text) => self.handle_text(&text, cx),
                TransportEvent::Error => match self.phase {
                    Phase::Session => self.status = "WebSocket transport error".into(),
                    Phase::Connect { pairing } if pairing => {
                        self.connect_status =
                            "Could not reach the host — check the address.".into();
                        self.phase = Phase::Connect { pairing: false };
                        self.socket = None;
                    }
                    Phase::Connect { .. } => {}
                },
                TransportEvent::Closed(reason) => match self.phase {
                    Phase::Session => {
                        self.status = if reason.is_empty() {
                            "Disconnected — reconnecting…".into()
                        } else {
                            format!("Disconnected: {reason} — reconnecting…").into()
                        };
                        self.schedule_reconnect(cx);
                    }
                    Phase::Connect { pairing } if pairing => {
                        self.connect_status =
                            "The host closed the connection before pairing — check the address."
                                .into();
                        self.phase = Phase::Connect { pairing: false };
                        self.socket = None;
                    }
                    Phase::Connect { .. } => {}
                },
            }
        }
        cx.notify();
    }

    fn handle_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let frame = match HostFrame::decode(text) {
            Ok(frame) => frame,
            Err(error) => {
                let message: SharedString = format!("Protocol decode error: {error}").into();
                match self.phase {
                    Phase::Session => self.status = message,
                    Phase::Connect { .. } => self.connect_status = message,
                }
                return;
            }
        };

        // Before a token exists the shared `Client` cannot fold these frames —
        // its connection is not yet in a handshake — so pairing is handled here,
        // exactly as `sync-probe`'s `pair` command does.
        if matches!(self.phase, Phase::Connect { .. }) {
            match frame {
                HostFrame::Paired { token, .. } => self.complete_pairing(token, cx),
                HostFrame::Refused { reason } => {
                    self.connect_status = pairing::refusal_message(&reason).into();
                    self.phase = Phase::Connect { pairing: false };
                    self.socket = None;
                }
                _ => {}
            }
            return;
        }

        let was_ready = self.client.is_ready();
        let is_session_list = matches!(frame, HostFrame::SessionList { .. });
        let affects_selected = matches!(
            &frame,
            HostFrame::Events { session_id, .. } | HostFrame::SessionEnded { session_id, .. }
                if self.selected.as_deref() == Some(session_id)
        );

        let accepted_delivery_ids: Vec<u64> = match &frame {
            HostFrame::Events { events, .. } => events
                .iter()
                .filter_map(|event| match event.event {
                    AgentEvent::TurnAccepted { delivery_id } => Some(delivery_id),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        let rejected = match &frame {
            HostFrame::CommandRejected {
                session_id,
                command,
                reason,
            } => Some((session_id.clone(), command.clone(), format!("{reason:?}"))),
            _ => None,
        };

        let outgoing = self.client.handle(frame);
        self.send_all(outgoing);
        for delivery_id in accepted_delivery_ids {
            self.command_ui.accepted_turn(delivery_id);
            self.status = "Turn accepted by host".into();
        }
        if let Some((session_id, command, reason)) = rejected {
            self.command_ui
                .rejected(&session_id, command.as_ref(), reason);
        }

        if !was_ready && self.client.is_ready() {
            let host = self
                .client
                .host()
                .map(|host| host.display_name.clone())
                .unwrap_or_else(|| "host".into());
            self.status = format!("Connected to {host}").into();
            self.send(self.client.request_sessions());
        } else if let Some(reason) = self.client.refusal_reason().cloned() {
            // A stored token the host no longer honours cannot be salvaged by
            // reconnecting, so clear it and send the user back to pairing rather
            // than into a loop. Other refusals land there too — the connect
            // screen is where a fresh code is entered.
            self.store.clear();
            self.socket = None;
            self._reconnect = None;
            self.phase = Phase::Connect { pairing: false };
            self.connect_status = pairing::refusal_message(&reason).into();
            return;
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
            if let Some(session_id) = self.selected.as_deref()
                && let Some(state) = self.client.session(session_id)
            {
                self.command_ui.resolved_approvals(
                    session_id,
                    state
                        .timeline()
                        .pending_approvals
                        .iter()
                        .map(|request| request.id.as_str()),
                );
            }
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

    fn send(&mut self, frame: ClientFrame) -> bool {
        let Some(socket) = &self.socket else {
            return false;
        };
        let result = frame
            .encode()
            .map_err(|error| error.to_string())
            .and_then(|text| socket.send(&text));
        if let Err(error) = result {
            self.status = format!("WebSocket send error: {error}").into();
            false
        } else {
            true
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
    /// `Client::command` retains the allocated `delivery_id`; the turn is not
    /// accepted until `AgentEvent::TurnAccepted` echoes it back, which is what
    /// the status line reports.
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
        let command = turn_command(delivery_id, text.clone());
        let frame = self.client.command(session_id.clone(), command);
        self.command_ui
            .sent_turn(session_id.clone(), delivery_id, text);
        let sent = self.send(frame);
        self.composer
            .update(cx, |state, cx| state.set_value("", window, cx));
        if sent {
            self.status = "Turn sent — awaiting acceptance…".into();
        } else {
            self.command_ui.rejected(
                &session_id,
                Some(&RejectedCommand::Turn { delivery_id }),
                "transport could not send the command".into(),
            );
        }
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
        let command = approval_command(request_id.clone(), decision);
        let frame = self.client.command(session_id.clone(), command);
        self.command_ui
            .sent_approval(session_id.clone(), request_id.clone());
        if !self.send(frame) {
            self.command_ui.rejected(
                &session_id,
                Some(&RejectedCommand::Approval { request_id }),
                "transport could not send the command".into(),
            );
        }
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
        match self.phase {
            Phase::Connect { pairing } => self.render_connect(pairing, cx).into_any_element(),
            Phase::Session => self.render_session(window, cx).into_any_element(),
        }
    }
}

impl ClientApp {
    /// The connect screen: a host address, a pairing code, and one action.
    fn render_connect(&self, pairing: bool, cx: &mut Context<Self>) -> gpui::AnyElement {
        let muted = cx.theme().muted_foreground;
        let field = |label: &str, input: &Entity<InputState>| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(SharedString::from(label.to_string())),
                )
                .child(Input::new(input))
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .items_center()
            .justify_center()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .w(px(320.))
                    .child(div().text_xl().font_semibold().child("Connect to a host"))
                    .child(div().text_sm().text_color(muted).child(
                        "On the host computer, tcode shows a short pairing code when it starts. \
                         Enter the host's address and that code to pair this device once.",
                    ))
                    .child(field("Host address", &self.url_input))
                    .child(field("Pairing code", &self.code_input))
                    .child(
                        Button::new("connect")
                            .primary()
                            .label(if pairing { "Pairing…" } else { "Connect" })
                            .disabled(pairing)
                            .on_click(cx.listener(|this, _, _, cx| this.start_pairing(cx))),
                    )
                    .when(!self.connect_status.is_empty(), |column| {
                        column.child(
                            div()
                                .text_sm()
                                .text_color(muted)
                                .child(self.connect_status.clone()),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_session(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
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
            .child(
                // The host name and the one control that leaves it: sign-out
                // clears the stored token and returns to pairing.
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_lg().font_semibold().child("tcode"))
                    .child(
                        Button::new("sign-out")
                            .ghost()
                            .label("Sign out")
                            .on_click(cx.listener(|this, _, _, cx| this.sign_out(cx))),
                    ),
            )
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
            .children(self.render_local_turns(cx))
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
            .into_any_element()
    }

    fn render_local_turns(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let Some(session_id) = self.selected.as_deref() else {
            return Vec::new();
        };
        self.command_ui
            .turns
            .iter()
            .filter(|turn| turn.session_id == session_id)
            .map(|turn| {
                let (label, color) = match &turn.status {
                    LocalTurnStatus::AwaitingAcceptance => (
                        "Sending — awaiting host acceptance".to_owned(),
                        cx.theme().warning,
                    ),
                    LocalTurnStatus::Rejected(reason) => (
                        format!("Not sent — host rejected the command: {reason}"),
                        cx.theme().danger,
                    ),
                };
                div()
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap_1()
                    .px_3()
                    .py_2()
                    .child(
                        div()
                            .max_w(px(680.))
                            .rounded_lg()
                            .px_3()
                            .py_2()
                            .bg(cx.theme().accent.opacity(0.16))
                            .child(turn.text.clone()),
                    )
                    .child(div().text_xs().text_color(color).child(label))
                    .into_any_element()
            })
            .collect()
    }

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
                let request_id = request.id.clone();
                let options = request.options.clone();
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
                    .child(if options.is_empty() {
                        self.render_fixed_approval_choices(id, cx)
                    } else {
                        options
                            .into_iter()
                            .fold(div().flex().gap_2(), |row, option| {
                                let request_id = request_id.clone();
                                let option_id = option.id.clone();
                                row.child(
                                    Button::new(SharedString::from(format!(
                                        "approval-option-{}",
                                        option.id
                                    )))
                                    .label(option.label)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.respond_approval(
                                            request_id.clone(),
                                            provider_approval_decision(option_id.clone()),
                                            cx,
                                        );
                                    })),
                                )
                            })
                    })
                    .into_any_element()
            })
            .collect()
    }

    fn render_fixed_approval_choices(
        &self,
        request_id: String,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        [
            ("Cancel turn", ApprovalDecision::Cancel),
            ("Deny", ApprovalDecision::Deny),
            ("Always allow", ApprovalDecision::ApproveForSession),
            ("Approve", ApprovalDecision::Approve),
        ]
        .into_iter()
        .fold(div().flex().gap_2(), |row, (label, decision)| {
            let request_id = request_id.clone();
            row.child(
                Button::new(SharedString::from(format!("approval-{label}-{request_id}")))
                    .label(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.respond_approval(request_id.clone(), decision.clone(), cx);
                    })),
            )
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructs_text_turn_command() {
        assert!(matches!(
            turn_command(42, "ship it".into()),
            SessionCommand::SendTurn {
                delivery_id: 42,
                text,
                options: None,
                attachments,
            } if text == "ship it" && attachments.is_empty()
        ));
    }

    #[test]
    fn maps_provider_choice_to_opaque_option_id() {
        assert_eq!(
            provider_approval_decision("allow-worktree".into()),
            ApprovalDecision::Option("allow-worktree".into())
        );
    }

    #[test]
    fn constructs_approval_response_command() {
        assert_eq!(
            approval_command("request-7".into(), ApprovalDecision::ApproveForSession),
            SessionCommand::RespondApproval {
                request_id: "request-7".into(),
                decision: ApprovalDecision::ApproveForSession,
            }
        );
    }

    #[test]
    fn rejection_marks_the_matching_optimistic_turn() {
        let mut state = CommandUiState::default();
        state.sent_turn("s1".into(), 7, "run tests".into());
        state.rejected("s1", None, "session ended".into());

        assert_eq!(
            state.turns[0].status,
            LocalTurnStatus::Rejected("session ended".into())
        );
    }

    #[test]
    fn acceptance_replaces_the_optimistic_turn_with_canonical_timeline() {
        let mut state = CommandUiState::default();
        state.sent_turn("s1".into(), 7, "run tests".into());
        state.accepted_turn(7);

        assert!(state.turns.is_empty());
        assert!(state.attempts.is_empty());
    }

    #[test]
    fn approval_rejection_does_not_mark_a_turn_rejected() {
        let mut state = CommandUiState::default();
        state.sent_approval("s1".into(), "approval-1".into());
        state.sent_turn("s1".into(), 8, "continue".into());
        state.rejected("s1", None, "approval expired".into());

        assert_eq!(state.turns[0].status, LocalTurnStatus::AwaitingAcceptance);
    }

    #[test]
    fn correlated_approval_rejection_does_not_reject_an_earlier_turn() {
        let mut state = CommandUiState::default();
        state.sent_turn("s1".into(), 8, "continue".into());
        state.sent_approval("s1".into(), "approval-1".into());
        state.rejected(
            "s1",
            Some(&RejectedCommand::Approval {
                request_id: "approval-1".into(),
            }),
            "approval expired".into(),
        );

        assert_eq!(state.turns[0].status, LocalTurnStatus::AwaitingAcceptance);
    }

    #[test]
    fn uncorrelated_rejection_still_consumes_the_oldest_attempt() {
        let mut state = CommandUiState::default();
        state.sent_turn("s1".into(), 8, "continue".into());
        state.sent_approval("s1".into(), "approval-1".into());
        state.rejected("s1", None, "older host refused command".into());

        assert_eq!(
            state.turns[0].status,
            LocalTurnStatus::Rejected("older host refused command".into())
        );
        assert_eq!(state.attempts.len(), 1);
    }
}
