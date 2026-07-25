//! Host side of the tcode sync protocol: the connection state machine, with no
//! I/O in it.
//!
//! [`Connection`] turns one client frame into the host frames it deserves. It
//! never touches a socket, a clock, or a task executor, so the rules that
//! actually matter — you cannot act before a handshake, a bad token gets a
//! refusal and not a session list, a cursor resumes exactly where it left off —
//! are testable without standing up a server.
//!
//! The transport is a thin shell around this: read a text frame, call
//! [`Connection::handle`], write what comes back.
//!
//! Session data arrives through [`SessionSource`], which keeps this crate free
//! of GPUI and of the provider adapters. That is not incidental: phase 1.5 of
//! `docs/multiplatform-plan.md` reuses this crate for a headless `tcode-server`
//! by swapping the binary's `main`, which only works if nothing here reaches
//! for a UI.

pub mod pairing;
pub mod server;
pub mod store_source;

pub use pairing::Pairing;
pub use server::{SyncServer, WakeSource, start, start_on};
pub use store_source::{CommandRequest, CommandSink, LiveFlags, LiveSessions, StoreSource};

use std::collections::HashMap;

use sync_protocol::{
    ClientFrame, ClientInfo, CommandRejection, HostFrame, HostInfo, RefuseReason, SeqEvent,
    SessionCommand, SessionSummary, negotiate_version,
};

/// How many events one `Events` frame carries while replaying backlog.
///
/// Bounded because a session log can hold tens of thousands of events and a
/// phone on a mobile connection should start rendering the first screenful
/// rather than wait for a single enormous frame.
pub const BACKLOG_BATCH: usize = 256;

/// Whatever owns the sessions on this host.
///
/// On the desktop that is the running app; for a headless server it is the
/// session store directly. The state machine cares about neither.
pub trait SessionSource {
    /// Sessions this host is willing to serve, newest first.
    fn list_sessions(&self) -> Vec<SessionSummary>;

    /// Events after `from_seq`, at most `limit` of them, ascending.
    ///
    /// `from_seq` is exclusive: `Some(4)` means "start at 5". `None` means from
    /// the beginning. An unknown session yields an empty vector — the caller
    /// distinguishes the two cases via [`SessionSource::session_exists`].
    fn read_events(&self, session_id: &str, from_seq: Option<u64>, limit: usize) -> Vec<SeqEvent>;

    fn session_exists(&self, session_id: &str) -> bool;

    /// Deliver a command to a live session.
    fn send_command(
        &self,
        session_id: &str,
        command: SessionCommand,
    ) -> Result<(), CommandRejection>;
}

/// Host identity and the credential clients must present.
pub struct HostConfig {
    pub host: HostInfo,
    /// Outstanding pairing code, if the host offers pairing.
    ///
    /// `None` means a client must already hold the token — which is right for a
    /// headless server nobody is standing in front of to read a code off.
    pub pairing: Option<crate::pairing::Pairing>,
    /// Compared in full. Pairing — how a client comes to hold this — is host UX
    /// and deliberately outside the protocol for now.
    pub token: String,
}

/// Per-session streaming position: the highest `seq` this client has been sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cursor(Option<u64>);

/// One client connection.
///
/// Lives from the socket opening to it closing. A reconnect is a new
/// `Connection` — resumption is carried by the client's cursor, not by host
/// memory, which is what makes it survive the host restarting too.
pub struct Connection<S: SessionSource> {
    source: S,
    config: HostConfig,
    state: State,
    cursors: HashMap<String, Cursor>,
}

enum State {
    /// Nothing but `Hello` is accepted.
    AwaitingHello,
    /// Handshake done, carrying the version both peers agreed on. Client
    /// identity is logged at handshake rather than retained: nothing here reads
    /// it back, and storing it "for later" would be speculation.
    Ready { version: u32 },
    /// A refusal was sent. The transport should close; anything further is
    /// ignored rather than answered.
    Refused,
}

impl<S: SessionSource> Connection<S> {
    pub fn new(source: S, config: HostConfig) -> Self {
        Self {
            source,
            config,
            state: State::AwaitingHello,
            cursors: HashMap::new(),
        }
    }

    /// True once the handshake succeeded.
    pub fn is_ready(&self) -> bool {
        matches!(self.state, State::Ready { .. })
    }

    /// The protocol version both peers agreed on, once the handshake is done.
    ///
    /// The transport needs this to shape frames when more than one version
    /// exists; today it is also what proves the negotiated value is retained
    /// rather than computed and dropped.
    pub fn negotiated_version(&self) -> Option<u32> {
        match self.state {
            State::Ready { version } => Some(version),
            _ => None,
        }
    }

    /// True when the transport should close the socket.
    pub fn is_refused(&self) -> bool {
        matches!(self.state, State::Refused)
    }

    /// Handle one client frame, returning the frames to send back.
    pub fn handle(&mut self, frame: ClientFrame) -> Vec<HostFrame> {
        match &self.state {
            State::Refused => Vec::new(),
            State::AwaitingHello => self.handle_unauthenticated(frame),
            State::Ready { .. } => self.handle_ready(frame),
        }
    }

    fn handle_unauthenticated(&mut self, frame: ClientFrame) -> Vec<HostFrame> {
        if let ClientFrame::Pair {
            min_version,
            max_version,
            code,
            client,
        } = frame
        {
            return self.handle_pairing(min_version, max_version, &code, client);
        }
        let ClientFrame::Hello {
            min_version,
            max_version,
            client,
            token,
        } = frame
        else {
            // Anything before a handshake is treated as unauthorized rather
            // than as a protocol error: an unauthenticated peer must not be
            // able to tell "wrong credential" from "wrong order", or the
            // refusal becomes an oracle.
            return self.refuse(RefuseReason::Unauthorized);
        };

        let Some(version) = negotiate_version(min_version, max_version) else {
            // Version is checked before the token on purpose. A client too old
            // to speak our protocol cannot be expected to have a valid token
            // format either, and "upgrade" is the more useful thing to say.
            return self.refuse(RefuseReason::UnsupportedVersion {
                host_min: sync_protocol::PROTOCOL_MIN_VERSION,
                host_max: sync_protocol::PROTOCOL_MAX_VERSION,
            });
        };

        if !constant_time_eq(token.as_bytes(), self.config.token.as_bytes()) {
            return self.refuse(RefuseReason::Unauthorized);
        }

        log::info!(
            "sync client connected: {} ({}) protocol v{version}",
            client.display_name,
            client.platform
        );
        self.state = State::Ready { version };
        vec![HostFrame::Welcome {
            version,
            host: self.config.host.clone(),
        }]
    }

    /// Trade a pairing code for the durable token.
    ///
    /// Deliberately does *not* authenticate the connection: the client sends a
    /// normal `Hello` afterwards. Pairing then has exactly one job, and a
    /// `Paired` frame is worth no more than the token inside it.
    fn handle_pairing(
        &mut self,
        min_version: u32,
        max_version: u32,
        code: &str,
        client: ClientInfo,
    ) -> Vec<HostFrame> {
        let Some(version) = negotiate_version(min_version, max_version) else {
            return self.refuse(RefuseReason::UnsupportedVersion {
                host_min: sync_protocol::PROTOCOL_MIN_VERSION,
                host_max: sync_protocol::PROTOCOL_MAX_VERSION,
            });
        };
        let Some(pairing) = &self.config.pairing else {
            // A host that offers no pairing says so as a rejection rather than
            // as a distinct error: whether pairing is disabled or the code was
            // wrong is not something an unauthenticated peer needs to learn.
            return self.refuse(RefuseReason::PairingRejected);
        };
        if !pairing.redeem(code, std::time::Instant::now()) {
            return self.refuse(RefuseReason::PairingRejected);
        }
        log::info!(
            "sync client paired: {} ({})",
            client.display_name,
            client.platform
        );
        // The connection stays unauthenticated; the client reconnects, or sends
        // Hello on this one, with the token it just received.
        vec![HostFrame::Paired {
            version,
            token: self.config.token.clone(),
        }]
    }

    fn handle_ready(&mut self, frame: ClientFrame) -> Vec<HostFrame> {
        match frame {
            // A second handshake is a confused client, not an attack. Refusing
            // is still right: silently ignoring it would leave the two sides
            // disagreeing about the negotiated version.
            // Both are "you already did this": a second handshake, or pairing
            // on a connection that is already authenticated. Silently ignoring
            // either would leave the two sides disagreeing about state.
            ClientFrame::Hello { .. } | ClientFrame::Pair { .. } => {
                self.refuse(RefuseReason::Unauthorized)
            }
            ClientFrame::Ping { nonce } => vec![HostFrame::Pong { nonce }],
            ClientFrame::ListSessions => vec![HostFrame::SessionList {
                sessions: self.source.list_sessions(),
            }],
            ClientFrame::Subscribe {
                session_id,
                from_seq,
            } => self.subscribe(session_id, from_seq),
            ClientFrame::Unsubscribe { session_id } => {
                self.cursors.remove(&session_id);
                Vec::new()
            }
            ClientFrame::Command {
                session_id,
                command,
            } => match self.source.send_command(&session_id, command) {
                Ok(()) => Vec::new(),
                Err(reason) => vec![HostFrame::CommandRejected { session_id, reason }],
            },
        }
    }

    fn subscribe(&mut self, session_id: String, from_seq: Option<u64>) -> Vec<HostFrame> {
        if !self.source.session_exists(&session_id) {
            return vec![HostFrame::CommandRejected {
                session_id,
                reason: CommandRejection::UnknownSession,
            }];
        }
        self.cursors.insert(session_id.clone(), Cursor(from_seq));
        self.drain(&session_id)
    }

    /// Send everything the host holds past this session's cursor.
    ///
    /// Also the live path: when new events land, the transport calls this and
    /// the client receives whatever it has not seen. One code path for backlog
    /// and live traffic means the catch-up boundary cannot drift between them.
    pub fn drain(&mut self, session_id: &str) -> Vec<HostFrame> {
        let Some(Cursor(mut cursor)) = self.cursors.get(session_id).copied() else {
            return Vec::new();
        };
        let mut frames = Vec::new();
        loop {
            let events = self.source.read_events(session_id, cursor, BACKLOG_BATCH);
            if events.is_empty() {
                break;
            }
            let caught_up = events.len() < BACKLOG_BATCH;
            cursor = Some(events.last().expect("non-empty").seq);
            frames.push(HostFrame::Events {
                session_id: session_id.to_owned(),
                events,
                caught_up,
            });
            if caught_up {
                break;
            }
        }
        self.cursors.insert(session_id.to_owned(), Cursor(cursor));

        // A subscribe that had nothing to send still owes the client a frame:
        // otherwise it cannot tell "you are up to date" from "the host has not
        // answered yet", and would sit showing a spinner forever.
        if frames.is_empty() {
            frames.push(HostFrame::Events {
                session_id: session_id.to_owned(),
                events: Vec::new(),
                caught_up: true,
            });
        }
        frames
    }

    /// Sessions this connection is streaming, for the transport to consult when
    /// deciding whether a newly recorded event concerns this client.
    pub fn subscribed_sessions(&self) -> impl Iterator<Item = &str> {
        self.cursors.keys().map(String::as_str)
    }

    fn refuse(&mut self, reason: RefuseReason) -> Vec<HostFrame> {
        self.state = State::Refused;
        vec![HostFrame::Refused { reason }]
    }
}

/// Compare two secrets without leaking their common prefix length through
/// timing. Short and dependency-free; the alternative is pulling a crypto crate
/// into a type-level crate for sixteen lines.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b)
        .fold(0_u8, |acc, (x, y)| acc | (x ^ y))
        .eq(&0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::AgentEvent;
    use std::cell::RefCell;
    use sync_protocol::ClientInfo;

    /// A source with a fixed set of sessions and logs.
    struct FakeSource {
        sessions: Vec<SessionSummary>,
        logs: HashMap<String, Vec<SeqEvent>>,
        sent: RefCell<Vec<(String, SessionCommand)>>,
        reject: Option<CommandRejection>,
    }

    impl FakeSource {
        fn with_log(session_id: &str, len: u64) -> Self {
            let events = (1..=len)
                .map(|seq| SeqEvent {
                    seq,
                    ts: 1_000 + seq,
                    event: AgentEvent::TurnAccepted { delivery_id: seq },
                })
                .collect();
            Self {
                sessions: vec![summary(session_id)],
                logs: HashMap::from([(session_id.to_owned(), events)]),
                sent: RefCell::new(Vec::new()),
                reject: None,
            }
        }
    }

    impl SessionSource for FakeSource {
        fn list_sessions(&self) -> Vec<SessionSummary> {
            self.sessions.clone()
        }

        fn read_events(
            &self,
            session_id: &str,
            from_seq: Option<u64>,
            limit: usize,
        ) -> Vec<SeqEvent> {
            let after = from_seq.unwrap_or(0);
            self.logs
                .get(session_id)
                .map(|events| {
                    events
                        .iter()
                        .filter(|event| event.seq > after)
                        .take(limit)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default()
        }

        fn session_exists(&self, session_id: &str) -> bool {
            self.logs.contains_key(session_id)
        }

        fn send_command(
            &self,
            session_id: &str,
            command: SessionCommand,
        ) -> Result<(), CommandRejection> {
            if let Some(reason) = self.reject.clone() {
                return Err(reason);
            }
            self.sent
                .borrow_mut()
                .push((session_id.to_owned(), command));
            Ok(())
        }
    }

    fn summary(session_id: &str) -> SessionSummary {
        SessionSummary {
            session_id: session_id.to_owned(),
            title: "Fix the parser".into(),
            provider: sync_protocol::ProviderKind::Codex,
            model: None,
            project: None,
            updated_at: 0,
            latest_seq: None,
            working: false,
            awaiting_approval: false,
        }
    }

    const TOKEN: &str = "correct-horse";

    fn config() -> HostConfig {
        HostConfig {
            host: HostInfo {
                host_id: "host-1".into(),
                display_name: "workstation".into(),
                platform: "macos".into(),
                app_version: "0.1.0".into(),
            },
            token: TOKEN.into(),
            // Most tests exercise token auth; pairing gets its own fixtures.
            pairing: None,
        }
    }

    fn hello(token: &str) -> ClientFrame {
        ClientFrame::Hello {
            min_version: sync_protocol::PROTOCOL_MIN_VERSION,
            max_version: sync_protocol::PROTOCOL_MAX_VERSION,
            client: ClientInfo {
                client_id: "client-1".into(),
                display_name: "Pixel".into(),
                platform: "android".into(),
                app_version: "0.1.0".into(),
            },
            token: token.into(),
        }
    }

    fn connected(source: FakeSource) -> Connection<FakeSource> {
        let mut connection = Connection::new(source, config());
        let welcome = connection.handle(hello(TOKEN));
        assert!(matches!(welcome.as_slice(), [HostFrame::Welcome { .. }]));
        connection
    }

    fn events_of(frames: &[HostFrame]) -> Vec<u64> {
        frames
            .iter()
            .flat_map(|frame| match frame {
                HostFrame::Events { events, .. } => events.iter().map(|e| e.seq).collect(),
                _ => Vec::new(),
            })
            .collect()
    }

    #[test]
    fn a_valid_handshake_is_welcomed() {
        let mut connection = Connection::new(FakeSource::with_log("s1", 0), config());
        let frames = connection.handle(hello(TOKEN));
        match frames.as_slice() {
            [HostFrame::Welcome { version, host }] => {
                assert_eq!(*version, sync_protocol::PROTOCOL_MAX_VERSION);
                assert_eq!(host.host_id, "host-1");
            }
            other => panic!("expected a welcome, got {other:?}"),
        }
        assert!(connection.is_ready());
        assert_eq!(
            connection.negotiated_version(),
            Some(sync_protocol::PROTOCOL_MAX_VERSION)
        );
    }

    #[test]
    fn a_wrong_token_is_refused_and_the_connection_is_done() {
        let mut connection = Connection::new(FakeSource::with_log("s1", 3), config());
        let frames = connection.handle(hello("wrong"));
        assert!(matches!(
            frames.as_slice(),
            [HostFrame::Refused {
                reason: RefuseReason::Unauthorized
            }]
        ));
        assert!(connection.is_refused());
        assert!(!connection.is_ready());
        assert_eq!(connection.negotiated_version(), None);
    }

    /// The property that matters after a refusal: the connection goes mute.
    /// Answering anything at all would let an unauthenticated peer keep probing
    /// on a socket the host has already rejected.
    #[test]
    fn a_refused_connection_answers_nothing_further() {
        let mut connection = Connection::new(FakeSource::with_log("s1", 3), config());
        connection.handle(hello("wrong"));

        assert!(connection.handle(ClientFrame::ListSessions).is_empty());
        assert!(connection.handle(ClientFrame::Ping { nonce: 1 }).is_empty());
        assert!(
            connection
                .handle(ClientFrame::Subscribe {
                    session_id: "s1".into(),
                    from_seq: None,
                })
                .is_empty()
        );
        assert!(connection.handle(hello(TOKEN)).is_empty());
    }

    /// Acting before the handshake must not be distinguishable from presenting
    /// a bad credential, or the refusal tells an unauthenticated peer which of
    /// the two it got wrong.
    #[test]
    fn frames_before_the_handshake_are_refused_as_unauthorized() {
        for frame in [
            ClientFrame::ListSessions,
            ClientFrame::Ping { nonce: 1 },
            ClientFrame::Subscribe {
                session_id: "s1".into(),
                from_seq: None,
            },
            ClientFrame::Command {
                session_id: "s1".into(),
                command: SessionCommand::Interrupt,
            },
        ] {
            let mut connection = Connection::new(FakeSource::with_log("s1", 3), config());
            let frames = connection.handle(frame);
            assert!(
                matches!(
                    frames.as_slice(),
                    [HostFrame::Refused {
                        reason: RefuseReason::Unauthorized
                    }]
                ),
                "got {frames:?}"
            );
        }
    }

    #[test]
    fn an_unsupported_version_is_refused_with_the_hosts_range() {
        let mut connection = Connection::new(FakeSource::with_log("s1", 0), config());
        let frames = connection.handle(ClientFrame::Hello {
            min_version: sync_protocol::PROTOCOL_MAX_VERSION + 1,
            max_version: sync_protocol::PROTOCOL_MAX_VERSION + 3,
            client: ClientInfo {
                client_id: "c".into(),
                display_name: "c".into(),
                platform: "web".into(),
                app_version: "9.9.9".into(),
            },
            token: TOKEN.into(),
        });
        match frames.as_slice() {
            [
                HostFrame::Refused {
                    reason: RefuseReason::UnsupportedVersion { host_min, host_max },
                },
            ] => {
                assert_eq!(*host_min, sync_protocol::PROTOCOL_MIN_VERSION);
                assert_eq!(*host_max, sync_protocol::PROTOCOL_MAX_VERSION);
            }
            other => panic!("expected a version refusal, got {other:?}"),
        }
    }

    /// A version mismatch must win over a bad token: a client too old to speak
    /// the protocol cannot be expected to hold a well-formed credential, and
    /// "you need to upgrade" is the actionable message.
    #[test]
    fn version_is_checked_before_the_token() {
        let mut connection = Connection::new(FakeSource::with_log("s1", 0), config());
        let frames = connection.handle(ClientFrame::Hello {
            min_version: sync_protocol::PROTOCOL_MAX_VERSION + 1,
            max_version: sync_protocol::PROTOCOL_MAX_VERSION + 1,
            client: ClientInfo {
                client_id: "c".into(),
                display_name: "c".into(),
                platform: "web".into(),
                app_version: "9.9.9".into(),
            },
            token: "definitely wrong".into(),
        });
        assert!(
            matches!(
                frames.as_slice(),
                [HostFrame::Refused {
                    reason: RefuseReason::UnsupportedVersion { .. }
                }]
            ),
            "got {frames:?}"
        );
    }

    #[test]
    fn subscribing_from_scratch_replays_the_whole_log() {
        let mut connection = connected(FakeSource::with_log("s1", 5));
        let frames = connection.handle(ClientFrame::Subscribe {
            session_id: "s1".into(),
            from_seq: None,
        });
        assert_eq!(events_of(&frames), vec![1, 2, 3, 4, 5]);
        assert!(matches!(
            frames.last(),
            Some(HostFrame::Events {
                caught_up: true,
                ..
            })
        ));
    }

    /// The whole point of the cursor: a reconnecting client gets exactly what
    /// it missed — no gap, no duplicate.
    #[test]
    fn subscribing_from_a_cursor_resumes_exactly_after_it() {
        let mut connection = connected(FakeSource::with_log("s1", 5));
        let frames = connection.handle(ClientFrame::Subscribe {
            session_id: "s1".into(),
            from_seq: Some(3),
        });
        assert_eq!(events_of(&frames), vec![4, 5]);
    }

    /// An up-to-date client still gets an answer. Silence is indistinguishable
    /// from a host that has not replied, and the client would spin forever.
    #[test]
    fn subscribing_at_the_end_of_the_log_still_answers() {
        let mut connection = connected(FakeSource::with_log("s1", 5));
        let frames = connection.handle(ClientFrame::Subscribe {
            session_id: "s1".into(),
            from_seq: Some(5),
        });
        match frames.as_slice() {
            [
                HostFrame::Events {
                    events, caught_up, ..
                },
            ] => {
                assert!(events.is_empty());
                assert!(*caught_up);
            }
            other => panic!("expected an empty caught-up frame, got {other:?}"),
        }
    }

    /// Long logs arrive in bounded batches, with `caught_up` false until the
    /// last one, so a client can render progressively instead of waiting.
    #[test]
    fn a_long_backlog_is_batched_and_only_the_last_frame_is_caught_up() {
        let total = (BACKLOG_BATCH * 2 + 7) as u64;
        let mut connection = connected(FakeSource::with_log("s1", total));
        let frames = connection.handle(ClientFrame::Subscribe {
            session_id: "s1".into(),
            from_seq: None,
        });

        assert_eq!(frames.len(), 3, "expected three batches");
        let flags: Vec<bool> = frames
            .iter()
            .map(|frame| match frame {
                HostFrame::Events { caught_up, .. } => *caught_up,
                other => panic!("expected events, got {other:?}"),
            })
            .collect();
        assert_eq!(flags, vec![false, false, true]);

        // Contiguous, ascending, complete — the guarantee a cursor rests on.
        let seqs = events_of(&frames);
        assert_eq!(seqs.len(), total as usize);
        assert_eq!(seqs.first(), Some(&1));
        assert_eq!(seqs.last(), Some(&total));
        assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1));
    }

    /// After a subscribe drains the log, new events reach the client through
    /// the same path with no repeats — backlog and live traffic share one code
    /// path precisely so the boundary between them cannot drift.
    #[test]
    fn draining_again_sends_only_what_arrived_since() {
        let mut source = FakeSource::with_log("s1", 2);
        source.logs.get_mut("s1").expect("seeded").push(SeqEvent {
            seq: 3,
            ts: 1_003,
            event: AgentEvent::TurnAccepted { delivery_id: 3 },
        });
        let mut connection = connected(source);

        let first = connection.handle(ClientFrame::Subscribe {
            session_id: "s1".into(),
            from_seq: Some(2),
        });
        assert_eq!(events_of(&first), vec![3]);

        let again = connection.drain("s1");
        assert_eq!(
            events_of(&again),
            Vec::<u64>::new(),
            "an event must not be delivered twice"
        );
    }

    #[test]
    fn draining_a_session_that_was_never_subscribed_sends_nothing() {
        let mut connection = connected(FakeSource::with_log("s1", 3));
        assert!(connection.drain("s1").is_empty());
    }

    #[test]
    fn unsubscribing_stops_the_stream() {
        let mut connection = connected(FakeSource::with_log("s1", 3));
        connection.handle(ClientFrame::Subscribe {
            session_id: "s1".into(),
            from_seq: None,
        });
        assert_eq!(connection.subscribed_sessions().count(), 1);

        connection.handle(ClientFrame::Unsubscribe {
            session_id: "s1".into(),
        });
        assert_eq!(connection.subscribed_sessions().count(), 0);
        assert!(connection.drain("s1").is_empty());
    }

    #[test]
    fn subscribing_to_an_unknown_session_is_rejected() {
        let mut connection = connected(FakeSource::with_log("s1", 3));
        let frames = connection.handle(ClientFrame::Subscribe {
            session_id: "nope".into(),
            from_seq: None,
        });
        assert!(
            matches!(
                frames.as_slice(),
                [HostFrame::CommandRejected {
                    reason: CommandRejection::UnknownSession,
                    ..
                }]
            ),
            "got {frames:?}"
        );
    }

    #[test]
    fn an_accepted_command_reaches_the_source_and_is_answered_with_silence() {
        let mut connection = connected(FakeSource::with_log("s1", 0));
        let frames = connection.handle(ClientFrame::Command {
            session_id: "s1".into(),
            command: SessionCommand::Interrupt,
        });
        assert!(frames.is_empty(), "got {frames:?}");
        let sent = connection.source.sent.borrow();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "s1");
        assert_eq!(sent[0].1, SessionCommand::Interrupt);
    }

    /// A rejected command must be reported. Silence would leave the client
    /// rendering a turn that never ran.
    #[test]
    fn a_rejected_command_is_reported_back() {
        let mut source = FakeSource::with_log("s1", 0);
        source.reject = Some(CommandRejection::SessionNotLive);
        let mut connection = connected(source);

        let frames = connection.handle(ClientFrame::Command {
            session_id: "s1".into(),
            command: SessionCommand::Interrupt,
        });
        match frames.as_slice() {
            [HostFrame::CommandRejected { session_id, reason }] => {
                assert_eq!(session_id, "s1");
                assert_eq!(*reason, CommandRejection::SessionNotLive);
            }
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    #[test]
    fn ping_is_answered_with_the_same_nonce() {
        let mut connection = connected(FakeSource::with_log("s1", 0));
        let frames = connection.handle(ClientFrame::Ping { nonce: 99 });
        assert!(matches!(frames.as_slice(), [HostFrame::Pong { nonce: 99 }]));
    }

    #[test]
    fn a_second_handshake_is_refused() {
        let mut connection = connected(FakeSource::with_log("s1", 0));
        let frames = connection.handle(hello(TOKEN));
        assert!(
            matches!(frames.as_slice(), [HostFrame::Refused { .. }]),
            "got {frames:?}"
        );
        assert!(connection.is_refused());
    }

    #[test]
    fn listing_sessions_returns_what_the_source_holds() {
        let mut connection = connected(FakeSource::with_log("s1", 0));
        let frames = connection.handle(ClientFrame::ListSessions);
        match frames.as_slice() {
            [HostFrame::SessionList { sessions }] => {
                assert_eq!(sessions.len(), 1);
                assert_eq!(sessions[0].session_id, "s1");
            }
            other => panic!("expected a session list, got {other:?}"),
        }
    }

    fn pair(code: &str) -> ClientFrame {
        ClientFrame::Pair {
            min_version: sync_protocol::PROTOCOL_MIN_VERSION,
            max_version: sync_protocol::PROTOCOL_MAX_VERSION,
            code: code.into(),
            client: ClientInfo {
                client_id: "client-1".into(),
                display_name: "Pixel".into(),
                platform: "android".into(),
                app_version: "0.1.0".into(),
            },
        }
    }

    fn paired_config(pairing: crate::pairing::Pairing) -> HostConfig {
        HostConfig {
            pairing: Some(pairing),
            ..config()
        }
    }

    /// The point of pairing: a client with no token can obtain one.
    #[test]
    fn a_valid_code_yields_the_token() {
        let pairing = crate::pairing::Pairing::new();
        let code = pairing.issue(std::time::Instant::now(), || [1, 2, 3, 4, 5, 6]);
        let mut connection = Connection::new(FakeSource::with_log("s1", 0), paired_config(pairing));

        match connection.handle(pair(&code)).as_slice() {
            [HostFrame::Paired { token, .. }] => assert_eq!(token, TOKEN),
            other => panic!("expected a paired frame, got {other:?}"),
        }
    }

    /// Pairing must not authenticate the connection. Keeping the two steps
    /// apart means a `Paired` frame is worth no more than the token in it, and
    /// the client still proves possession by sending `Hello`.
    #[test]
    fn pairing_does_not_authenticate_the_connection() {
        let pairing = crate::pairing::Pairing::new();
        let code = pairing.issue(std::time::Instant::now(), || [1, 2, 3, 4, 5, 6]);
        let mut connection = Connection::new(FakeSource::with_log("s1", 0), paired_config(pairing));

        connection.handle(pair(&code));
        assert!(!connection.is_ready(), "pairing is not a handshake");

        // The token it just received is what actually authenticates.
        assert!(matches!(
            connection.handle(hello(TOKEN)).as_slice(),
            [HostFrame::Welcome { .. }]
        ));
    }

    #[test]
    fn a_wrong_code_is_rejected_and_the_connection_is_done() {
        let pairing = crate::pairing::Pairing::new();
        pairing.issue(std::time::Instant::now(), || [1, 2, 3, 4, 5, 6]);
        let mut connection = Connection::new(FakeSource::with_log("s1", 0), paired_config(pairing));

        assert!(matches!(
            connection.handle(pair("ZZZZZZ")).as_slice(),
            [HostFrame::Refused {
                reason: RefuseReason::PairingRejected
            }]
        ));
        assert!(connection.is_refused());
    }

    /// A host with pairing disabled — a headless server nobody is standing in
    /// front of — must answer the same way as a wrong code. Whether pairing is
    /// off or the guess was bad is not something an unauthenticated peer needs.
    #[test]
    fn a_host_without_pairing_rejects_identically() {
        let mut connection = Connection::new(FakeSource::with_log("s1", 0), config());
        assert!(matches!(
            connection.handle(pair("ACDEFG")).as_slice(),
            [HostFrame::Refused {
                reason: RefuseReason::PairingRejected
            }]
        ));
    }

    #[test]
    fn pairing_after_a_handshake_is_refused() {
        let pairing = crate::pairing::Pairing::new();
        let code = pairing.issue(std::time::Instant::now(), || [1, 2, 3, 4, 5, 6]);
        let mut connection = Connection::new(FakeSource::with_log("s1", 0), paired_config(pairing));
        connection.handle(hello(TOKEN));

        assert!(matches!(
            connection.handle(pair(&code)).as_slice(),
            [HostFrame::Refused { .. }]
        ));
    }

    #[test]
    fn token_comparison_rejects_prefixes_and_accepts_the_exact_value() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"ab", b"abc"));
        assert!(constant_time_eq(b"", b""));
    }
}
