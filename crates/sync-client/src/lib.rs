//! Client side of the tcode sync protocol: the connection and session state
//! machines, with no I/O in them.
//!
//! [`Client`] turns host frames into client frames and observable replicated
//! state. A transport opens a socket, sends [`Client::connect`], passes each
//! received frame to [`Client::handle`], and writes the returned frames. The
//! client itself never touches a socket, a clock, or a task executor, so the
//! same logic runs behind a native WebSocket and the browser's WebSocket API.

use std::collections::{BTreeMap, HashMap};

use sync_protocol::{
    AgentEvent, ClientFrame, ClientInfo, CommandRejection, HostFrame, HostInfo, RefuseReason,
    SessionCommand, SessionEndReason, SessionSummary,
};
use tcode_core::session::Timeline;

/// Identity and credential sent at the start of every connection.
pub struct ClientConfig {
    pub client: ClientInfo,
    pub token: String,
}

enum ConnectionState {
    Disconnected,
    AwaitingWelcome,
    Ready { version: u32, host: HostInfo },
    Refused(RefuseReason),
}

/// A host frame that could not be reconciled with known-good client state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolViolation {
    /// The host selected a version outside the range this build advertised.
    UnsupportedWelcomeVersion { version: u32 },
    /// No part of an invalid batch is applied. The returned resubscribe starts
    /// after the cursor named here, so recovery cannot bless partial data.
    EventGap {
        session_id: String,
        expected_seq: u64,
        received_seq: u64,
        cursor: Option<u64>,
    },
}

/// One command rejection retained until the application has presented it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedCommand {
    pub session_id: String,
    pub command: Option<sync_protocol::RejectedCommand>,
    pub reason: CommandRejection,
}

/// One terminal session notification retained for the application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndedSession {
    pub session_id: String,
    pub reason: SessionEndReason,
}

/// The replicated state of one session.
#[derive(Debug, Clone, Default)]
pub struct SessionState {
    cursor: Option<u64>,
    timeline: Timeline,
    subscribed: bool,
    caught_up: bool,
}

impl SessionState {
    /// Highest contiguous sequence folded into this replica.
    pub fn cursor(&self) -> Option<u64> {
        self.cursor
    }

    pub fn timeline(&self) -> &Timeline {
        &self.timeline
    }

    /// False during backlog replay and after requesting a resubscribe.
    pub fn is_caught_up(&self) -> bool {
        self.caught_up
    }

    pub fn is_subscribed(&self) -> bool {
        self.subscribed
    }
}

/// Client state that survives transport connections.
///
/// In particular, session cursors and unacknowledged turns are intentionally
/// not connection state. A mobile transport can discard and recreate its
/// socket without forcing either a full replay or a blind resend.
pub struct Client {
    config: ClientConfig,
    connection: ConnectionState,
    sessions: Vec<SessionSummary>,
    replicas: HashMap<String, SessionState>,
    pending_turns: HashMap<String, BTreeMap<u64, SessionCommand>>,
    violations: Vec<ProtocolViolation>,
    rejections: Vec<RejectedCommand>,
    ended_sessions: Vec<EndedSession>,
    pongs: Vec<u64>,
    /// Token obtained by pairing, for the shell to persist.
    paired_token: Option<String>,
}

impl Client {
    pub fn new(config: ClientConfig) -> Self {
        Self {
            config,
            connection: ConnectionState::Disconnected,
            sessions: Vec::new(),
            replicas: HashMap::new(),
            pending_turns: HashMap::new(),
            violations: Vec::new(),
            rejections: Vec::new(),
            ended_sessions: Vec::new(),
            pongs: Vec::new(),
            paired_token: None,
        }
    }

    /// Start a transport connection and produce its mandatory first frame.
    ///
    /// Replicas and delivery bookkeeping are left intact. After `Welcome`,
    /// [`Self::handle`] automatically restores every requested subscription
    /// from its known-good cursor.
    pub fn connect(&mut self) -> ClientFrame {
        self.connection = ConnectionState::AwaitingWelcome;
        ClientFrame::Hello {
            min_version: sync_protocol::PROTOCOL_MIN_VERSION,
            max_version: sync_protocol::PROTOCOL_MAX_VERSION,
            client: self.config.client.clone(),
            token: self.config.token.clone(),
        }
    }

    pub fn is_ready(&self) -> bool {
        matches!(self.connection, ConnectionState::Ready { .. })
    }

    pub fn negotiated_version(&self) -> Option<u32> {
        match self.connection {
            ConnectionState::Ready { version, .. } => Some(version),
            _ => None,
        }
    }

    pub fn host(&self) -> Option<&HostInfo> {
        match &self.connection {
            ConnectionState::Ready { host, .. } => Some(host),
            _ => None,
        }
    }

    pub fn refusal_reason(&self) -> Option<&RefuseReason> {
        match &self.connection {
            ConnectionState::Refused(reason) => Some(reason),
            _ => None,
        }
    }

    pub fn sessions(&self) -> &[SessionSummary] {
        &self.sessions
    }

    pub fn session(&self, session_id: &str) -> Option<&SessionState> {
        self.replicas.get(session_id)
    }

    pub fn violations(&self) -> &[ProtocolViolation] {
        &self.violations
    }

    pub fn command_rejections(&self) -> &[RejectedCommand] {
        &self.rejections
    }

    pub fn ended_sessions(&self) -> &[EndedSession] {
        &self.ended_sessions
    }

    pub fn pongs(&self) -> &[u64] {
        &self.pongs
    }

    /// Offer a pairing code in exchange for the durable token.
    ///
    /// Used instead of `connect` when the client has no token yet. The reply is
    /// a `Paired` frame carrying one; the client then handshakes normally,
    /// which is why this does not change the connection's state.
    pub fn pair(&self, code: impl Into<String>) -> ClientFrame {
        ClientFrame::Pair {
            min_version: sync_protocol::PROTOCOL_MIN_VERSION,
            max_version: sync_protocol::PROTOCOL_MAX_VERSION,
            code: code.into(),
            client: self.config.client.clone(),
        }
    }

    /// The token a completed pairing produced, if one has arrived.
    ///
    /// A shell reads this to persist the token so the code is never needed
    /// again — which is the entire point of pairing rather than passing a code
    /// on every connection.
    pub fn paired_token(&self) -> Option<&str> {
        self.paired_token.as_deref()
    }

    pub fn request_sessions(&self) -> ClientFrame {
        ClientFrame::ListSessions
    }

    /// Subscribe from the replica's highest contiguous event, if any.
    pub fn subscribe(&mut self, session_id: impl Into<String>) -> ClientFrame {
        let session_id = session_id.into();
        let state = self.replicas.entry(session_id.clone()).or_default();
        state.subscribed = true;
        state.caught_up = false;
        ClientFrame::Subscribe {
            session_id,
            from_seq: state.cursor,
        }
    }

    /// Stop live delivery without discarding the replica or its resume cursor.
    pub fn unsubscribe(&mut self, session_id: impl Into<String>) -> ClientFrame {
        let session_id = session_id.into();
        if let Some(state) = self.replicas.get_mut(&session_id) {
            state.subscribed = false;
            state.caught_up = false;
        }
        ClientFrame::Unsubscribe { session_id }
    }

    /// Produce a command frame, retaining `SendTurn` until its delivery id is
    /// observed in a contiguous event batch.
    pub fn command(
        &mut self,
        session_id: impl Into<String>,
        command: SessionCommand,
    ) -> ClientFrame {
        let session_id = session_id.into();
        if let SessionCommand::SendTurn { delivery_id, .. } = &command {
            self.pending_turns
                .entry(session_id.clone())
                .or_default()
                .insert(*delivery_id, command.clone());
        }
        ClientFrame::Command {
            session_id,
            command,
        }
    }

    /// Turns that have been sent but have no observed `TurnAccepted`.
    pub fn unacknowledged_turns(&self, session_id: &str) -> impl Iterator<Item = &SessionCommand> {
        self.pending_turns
            .get(session_id)
            .into_iter()
            .flat_map(|turns| turns.values())
    }

    /// Resend only turns for which no acceptance event has been observed.
    ///
    /// A caller should wait until the resumed subscription is caught up before
    /// using this: its backfill may contain the acknowledgement that was in
    /// flight when the old socket disappeared.
    pub fn resend_unacknowledged_turns(&self, session_id: &str) -> Vec<ClientFrame> {
        self.unacknowledged_turns(session_id)
            .cloned()
            .map(|command| ClientFrame::Command {
                session_id: session_id.to_owned(),
                command,
            })
            .collect()
    }

    pub fn ping(&self, nonce: u64) -> ClientFrame {
        ClientFrame::Ping { nonce }
    }

    /// Handle one host frame, returning any frames the transport should send.
    pub fn handle(&mut self, frame: HostFrame) -> Vec<ClientFrame> {
        match &self.connection {
            ConnectionState::AwaitingWelcome => self.handle_handshake(frame),
            ConnectionState::Ready { .. } => self.handle_ready(frame),
            ConnectionState::Disconnected | ConnectionState::Refused(_) => Vec::new(),
        }
    }

    fn handle_handshake(&mut self, frame: HostFrame) -> Vec<ClientFrame> {
        match frame {
            HostFrame::Welcome { version, host }
                if (sync_protocol::PROTOCOL_MIN_VERSION..=sync_protocol::PROTOCOL_MAX_VERSION)
                    .contains(&version) =>
            {
                self.connection = ConnectionState::Ready { version, host };
                self.replicas
                    .iter()
                    .filter(|(_, state)| state.subscribed)
                    .map(|(session_id, state)| ClientFrame::Subscribe {
                        session_id: session_id.clone(),
                        from_seq: state.cursor,
                    })
                    .collect()
            }
            HostFrame::Welcome { version, .. } => {
                self.violations
                    .push(ProtocolViolation::UnsupportedWelcomeVersion { version });
                Vec::new()
            }
            HostFrame::Refused { reason } => {
                self.connection = ConnectionState::Refused(reason);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn handle_ready(&mut self, frame: HostFrame) -> Vec<ClientFrame> {
        match frame {
            HostFrame::Welcome { .. } | HostFrame::Refused { .. } => Vec::new(),
            HostFrame::SessionList { sessions } => {
                self.sessions = sessions;
                Vec::new()
            }
            HostFrame::Events {
                session_id,
                events,
                caught_up,
            } => self.apply_events(session_id, events, caught_up),
            HostFrame::CommandRejected {
                session_id,
                command,
                reason,
            } => {
                if let Some(sync_protocol::RejectedCommand::Turn { delivery_id }) = &command
                    && let Some(turns) = self.pending_turns.get_mut(&session_id)
                {
                    turns.remove(delivery_id);
                }
                self.rejections.push(RejectedCommand {
                    session_id,
                    command,
                    reason,
                });
                Vec::new()
            }
            HostFrame::SessionEnded { session_id, reason } => {
                if let Some(state) = self.replicas.get_mut(&session_id) {
                    state.caught_up = true;
                }
                self.ended_sessions
                    .push(EndedSession { session_id, reason });
                Vec::new()
            }
            HostFrame::Paired { token, .. } => {
                // Retained, not applied: the connection is still
                // unauthenticated, and the shell decides whether to persist the
                // token before handshaking with it.
                self.paired_token = Some(token);
                Vec::new()
            }
            HostFrame::Pong { nonce } => {
                self.pongs.push(nonce);
                Vec::new()
            }
        }
    }

    fn apply_events(
        &mut self,
        session_id: String,
        events: Vec<sync_protocol::SeqEvent>,
        caught_up: bool,
    ) -> Vec<ClientFrame> {
        let state = self.replicas.entry(session_id.clone()).or_default();
        let mut expected = state.cursor.unwrap_or(0).saturating_add(1);
        for event in &events {
            if event.seq != expected {
                let violation = ProtocolViolation::EventGap {
                    session_id: session_id.clone(),
                    expected_seq: expected,
                    received_seq: event.seq,
                    cursor: state.cursor,
                };
                log::warn!("sync event sequence violation: {violation:?}");
                self.violations.push(violation);
                state.caught_up = false;
                return vec![ClientFrame::Subscribe {
                    session_id,
                    from_seq: state.cursor,
                }];
            }
            expected = expected.saturating_add(1);
        }

        for event in &events {
            if let AgentEvent::TurnAccepted { delivery_id } = &event.event
                && let Some(turns) = self.pending_turns.get_mut(&session_id)
            {
                turns.remove(delivery_id);
            }
        }

        // The complete contiguity check above happens before any mutation, so
        // applying only this batch is equivalent to replaying the whole log:
        // no duplicate or out-of-order event can reach the reducer.
        for event in &events {
            state.timeline.apply_at(Some(event.ts), &event.event);
        }
        state.cursor = events.last().map(|event| event.seq).or(state.cursor);
        state.caught_up = caught_up;
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::TurnStatus;
    use sync_protocol::{PROTOCOL_MAX_VERSION, PROTOCOL_MIN_VERSION, SeqEvent};
    use tcode_core::session::StoredEvent;

    fn config() -> ClientConfig {
        ClientConfig {
            client: ClientInfo {
                client_id: "client-1".into(),
                display_name: "Pixel".into(),
                platform: "android".into(),
                app_version: "0.1.0".into(),
            },
            token: "correct-horse".into(),
        }
    }

    fn host() -> HostInfo {
        HostInfo {
            host_id: "host-1".into(),
            display_name: "workstation".into(),
            platform: "macos".into(),
            app_version: "0.1.0".into(),
        }
    }

    fn event(seq: u64, event: AgentEvent) -> SeqEvent {
        SeqEvent {
            seq,
            ts: 1_000 + seq,
            event,
        }
    }

    fn connect(client: &mut Client) {
        assert!(matches!(
            client.connect(),
            ClientFrame::Hello {
                min_version: PROTOCOL_MIN_VERSION,
                max_version: PROTOCOL_MAX_VERSION,
                ..
            }
        ));
        assert!(
            client
                .handle(HostFrame::Welcome {
                    version: PROTOCOL_MAX_VERSION,
                    host: host(),
                })
                .is_empty()
        );
    }

    fn turn(delivery_id: u64) -> SessionCommand {
        SessionCommand::SendTurn {
            delivery_id,
            text: format!("turn {delivery_id}"),
            options: None,
            attachments: Vec::new(),
        }
    }

    #[test]
    fn a_successful_handshake_exposes_the_host_and_version() {
        let mut client = Client::new(config());
        connect(&mut client);

        assert!(client.is_ready());
        assert_eq!(client.negotiated_version(), Some(PROTOCOL_MAX_VERSION));
        assert_eq!(
            client.host().map(|host| host.host_id.as_str()),
            Some("host-1")
        );
        assert_eq!(client.refusal_reason(), None);
    }

    #[test]
    fn authentication_and_version_refusals_are_distinguishable() {
        let mut unauthorized = Client::new(config());
        unauthorized.connect();
        unauthorized.handle(HostFrame::Refused {
            reason: RefuseReason::Unauthorized,
        });
        assert_eq!(
            unauthorized.refusal_reason(),
            Some(&RefuseReason::Unauthorized)
        );

        let mut old_app = Client::new(config());
        old_app.connect();
        old_app.handle(HostFrame::Refused {
            reason: RefuseReason::UnsupportedVersion {
                host_min: PROTOCOL_MAX_VERSION + 1,
                host_max: PROTOCOL_MAX_VERSION + 2,
            },
        });
        assert!(matches!(
            old_app.refusal_reason(),
            Some(RefuseReason::UnsupportedVersion { host_min, host_max })
                if *host_min > PROTOCOL_MAX_VERSION && host_max >= host_min
        ));
    }

    #[test]
    fn a_backfill_is_folded_by_the_shared_timeline_reducer() {
        let mut client = Client::new(config());
        connect(&mut client);
        client.subscribe("s1");

        client.handle(HostFrame::Events {
            session_id: "s1".into(),
            events: vec![event(
                1,
                AgentEvent::TurnStarted {
                    turn_id: "turn-1".into(),
                },
            )],
            caught_up: true,
        });

        let replica = client.session("s1").expect("replica");
        let local = Timeline::fold_events([StoredEvent {
            ts: Some(1_001),
            seq: Some(1),
            event: AgentEvent::TurnStarted {
                turn_id: "turn-1".into(),
            },
        }]);
        assert_eq!(replica.cursor(), Some(1));
        assert!(replica.is_caught_up());
        assert_eq!(replica.timeline().turns.len(), local.turns.len());
        assert_eq!(
            replica.timeline().turns[0].provider_turn_id,
            local.turns[0].provider_turn_id
        );
        assert_eq!(replica.timeline().turn_running, local.turn_running);
    }

    #[test]
    fn incremental_batches_equal_a_single_whole_history_fold() {
        let mut events = Vec::new();
        for turn in 0..384 {
            let turn_id = format!("turn-{turn}");
            let seq = events.len() as u64 + 1;
            events.push(event(
                seq,
                AgentEvent::TurnStarted {
                    turn_id: turn_id.clone(),
                },
            ));
            events.push(event(
                seq + 1,
                AgentEvent::TurnCompleted {
                    turn_id,
                    status: TurnStatus::Completed,
                    usage: None,
                },
            ));
        }
        let expected = Timeline::fold_events(events.iter().cloned().map(|event| StoredEvent {
            ts: Some(event.ts),
            seq: Some(event.seq),
            event: event.event,
        }));

        let mut client = Client::new(config());
        connect(&mut client);
        client.subscribe("s1");
        for (index, batch) in events.chunks(256).enumerate() {
            assert!(
                client
                    .handle(HostFrame::Events {
                        session_id: "s1".into(),
                        events: batch.to_vec(),
                        caught_up: index == 2,
                    })
                    .is_empty()
            );
        }

        let actual = client.session("s1").expect("replica");
        assert_eq!(actual.cursor(), Some(768));
        assert!(actual.is_caught_up());
        // Timeline has no PartialEq implementation. Its complete Debug form
        // includes private reducer bookkeeping, so this compares more than the
        // public render fields while keeping that API decision in core.
        assert_eq!(
            format!("{:#?}", actual.timeline()),
            format!("{expected:#?}")
        );
    }

    #[test]
    fn contiguous_batches_advance_the_cursor() {
        let mut client = Client::new(config());
        connect(&mut client);
        client.subscribe("s1");

        for seq in 1..=3 {
            assert!(
                client
                    .handle(HostFrame::Events {
                        session_id: "s1".into(),
                        events: vec![event(seq, AgentEvent::TurnAccepted { delivery_id: seq },)],
                        caught_up: seq == 3,
                    })
                    .is_empty()
            );
        }

        assert_eq!(client.session("s1").expect("replica").cursor(), Some(3));
    }

    #[test]
    fn the_latest_session_list_replaces_the_previous_one() {
        let mut client = Client::new(config());
        connect(&mut client);
        let summary = SessionSummary {
            session_id: "s1".into(),
            title: "Fix the parser".into(),
            provider: sync_protocol::ProviderKind::Codex,
            model: None,
            project: Some("tcode".into()),
            updated_at: 1_000,
            latest_seq: Some(3),
            working: true,
            awaiting_approval: false,
        };

        client.handle(HostFrame::SessionList {
            sessions: vec![summary.clone()],
        });
        assert_eq!(client.sessions(), [summary]);

        client.handle(HostFrame::SessionList {
            sessions: Vec::new(),
        });
        assert!(client.sessions().is_empty());
    }

    #[test]
    fn a_gap_is_rejected_and_resubscribed_from_the_known_good_cursor() {
        let mut client = Client::new(config());
        connect(&mut client);
        client.subscribe("s1");
        client.handle(HostFrame::Events {
            session_id: "s1".into(),
            events: vec![event(
                1,
                AgentEvent::TurnStarted {
                    turn_id: "good".into(),
                },
            )],
            caught_up: false,
        });

        let response = client.handle(HostFrame::Events {
            session_id: "s1".into(),
            events: vec![
                event(
                    3,
                    AgentEvent::TurnStarted {
                        turn_id: "not-folded".into(),
                    },
                ),
                event(4, AgentEvent::TurnAccepted { delivery_id: 4 }),
            ],
            caught_up: true,
        });

        assert_eq!(
            response,
            vec![ClientFrame::Subscribe {
                session_id: "s1".into(),
                from_seq: Some(1),
            }]
        );
        assert_eq!(client.session("s1").expect("replica").cursor(), Some(1));
        assert_eq!(
            client
                .session("s1")
                .expect("replica")
                .timeline()
                .turns
                .len(),
            1
        );
        assert!(matches!(
            client.violations(),
            [ProtocolViolation::EventGap {
                expected_seq: 2,
                received_seq: 3,
                cursor: Some(1),
                ..
            }]
        ));
    }

    #[test]
    fn reconnecting_restores_a_subscription_from_its_cursor() {
        let mut client = Client::new(config());
        connect(&mut client);
        client.subscribe("s1");
        client.handle(HostFrame::Events {
            session_id: "s1".into(),
            events: vec![event(1, AgentEvent::TurnAccepted { delivery_id: 1 })],
            caught_up: true,
        });

        client.connect();
        let restore = client.handle(HostFrame::Welcome {
            version: PROTOCOL_MAX_VERSION,
            host: host(),
        });

        assert_eq!(
            restore,
            vec![ClientFrame::Subscribe {
                session_id: "s1".into(),
                from_seq: Some(1),
            }]
        );
    }

    #[test]
    fn reconnect_retains_only_turns_without_an_observed_acknowledgement() {
        let mut client = Client::new(config());
        connect(&mut client);
        client.subscribe("s1");
        client.command("s1", turn(41));
        client.command("s1", turn(42));
        client.handle(HostFrame::Events {
            session_id: "s1".into(),
            events: vec![event(1, AgentEvent::TurnAccepted { delivery_id: 41 })],
            caught_up: true,
        });

        client.connect();
        client.handle(HostFrame::Welcome {
            version: PROTOCOL_MAX_VERSION,
            host: host(),
        });

        let pending: Vec<&SessionCommand> = client.unacknowledged_turns("s1").collect();
        assert_eq!(pending, vec![&turn(42)]);
        assert_eq!(
            client.resend_unacknowledged_turns("s1"),
            vec![ClientFrame::Command {
                session_id: "s1".into(),
                command: turn(42),
            }]
        );
    }

    #[test]
    fn command_rejections_session_endings_and_pongs_are_observable() {
        let mut client = Client::new(config());
        connect(&mut client);

        client.handle(HostFrame::CommandRejected {
            session_id: "s1".into(),
            command: Some(sync_protocol::RejectedCommand::Approval {
                request_id: "approval-1".into(),
            }),
            reason: CommandRejection::SessionNotLive,
        });
        client.handle(HostFrame::SessionEnded {
            session_id: "s1".into(),
            reason: SessionEndReason::Archived,
        });
        client.handle(HostFrame::Pong { nonce: 99 });

        assert_eq!(
            client.command_rejections(),
            [RejectedCommand {
                session_id: "s1".into(),
                command: Some(sync_protocol::RejectedCommand::Approval {
                    request_id: "approval-1".into(),
                }),
                reason: CommandRejection::SessionNotLive,
            }]
        );
        assert_eq!(
            client.ended_sessions(),
            [EndedSession {
                session_id: "s1".into(),
                reason: SessionEndReason::Archived,
            }]
        );
        assert_eq!(client.pongs(), [99]);
    }

    #[test]
    fn correlated_turn_rejection_removes_only_that_pending_turn() {
        let mut client = Client::new(config());
        connect(&mut client);
        client.command("s1", turn(41));
        client.command("s1", turn(42));

        client.handle(HostFrame::CommandRejected {
            session_id: "s1".into(),
            command: Some(sync_protocol::RejectedCommand::Turn { delivery_id: 41 }),
            reason: CommandRejection::SessionNotLive,
        });

        assert_eq!(
            client
                .unacknowledged_turns("s1")
                .collect::<Vec<&SessionCommand>>(),
            vec![&turn(42)]
        );
    }
}
