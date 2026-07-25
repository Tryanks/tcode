//! Wire types for the tcode host/client sync protocol.
//!
//! A **host** is a machine that can actually run agents: it spawns provider
//! CLIs, owns the repository, and holds the session log. A **client** is a
//! phone, browser, tablet, or another desktop that cannot — iOS forbids
//! fork/exec and wasm has no process model at all — and therefore drives a
//! host's sessions remotely.
//!
//! See `docs/sync-protocol.md` for the reasoning; this crate is normative.
//!
//! Types and serde only: no I/O, no transport, no async. That keeps the crate
//! buildable for `wasm32-unknown-unknown` and lets the same frames travel over
//! a WebSocket, a unix socket, or a test channel.

use serde::{Deserialize, Serialize};

// `ApprovalDecision` is protocol vocabulary, not an implementation detail:
// `SessionCommand::RespondApproval` carries it, so a client cannot answer an
// approval without naming it.
pub use agent::{AgentEvent, ApprovalDecision, ProviderKind, SessionCommand, WorkspacePath};

/// Oldest protocol version this build can speak.
pub const PROTOCOL_MIN_VERSION: u32 = 1;
/// Newest protocol version this build can speak.
pub const PROTOCOL_MAX_VERSION: u32 = 1;

/// Highest version both peers support, or `None` when the ranges are disjoint.
///
/// Peers exchange *ranges* rather than a single version because host and client
/// ship separately — an App Store release cannot be upgraded in lockstep with
/// the desktop app, so a protocol that demands equality is a protocol that
/// breaks on every release.
pub fn negotiate_version(client_min: u32, client_max: u32) -> Option<u32> {
    let low = client_min.max(PROTOCOL_MIN_VERSION);
    let high = client_max.min(PROTOCOL_MAX_VERSION);
    (low <= high).then_some(high)
}

/// One event as it crosses the wire: the session's canonical event plus the
/// position that makes resumption exact.
///
/// Not `PartialEq`: [`AgentEvent`] deliberately is not either, because it
/// carries `ResumeCursor` — opaque provider-owned JSON for which structural
/// equality would be a claim we cannot honour. Compare encoded forms instead;
/// for a wire type that is the equality that actually matters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeqEvent {
    /// Position in the session's log: 1-based, contiguous, strictly increasing.
    ///
    /// This — never `ts` — is what a cursor tracks. `ts` is a wall clock the
    /// writer tolerates repeating or going backwards, so a `ts` cursor would
    /// skip or duplicate events at exactly the moments that matter.
    pub seq: u64,
    /// Unix milliseconds, for display only.
    pub ts: u64,
    pub event: AgentEvent,
}

/// What a client tells a host about itself. Diagnostics and compatibility
/// warnings; the host must not make authorization decisions on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientInfo {
    /// Stable per-installation id, so a host can recognise a returning client
    /// across reconnects.
    pub client_id: String,
    /// Human-readable, for the host's "connected devices" list.
    pub display_name: String,
    /// e.g. `ios`, `android`, `web`, `macos`.
    pub platform: String,
    /// Client build version, for support and for telling a user which side is
    /// too old.
    pub app_version: String,
}

/// What a host tells a client about itself, once the handshake is accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInfo {
    pub host_id: String,
    pub display_name: String,
    pub platform: String,
    pub app_version: String,
}

/// Enough of a session for a client to render a list and pick one.
///
/// Deliberately its own type rather than the stored `SessionMeta`: the wire
/// format and the on-disk format have different reasons to change, and a client
/// has no use for host-local fields such as absolute working directories.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub title: String,
    pub provider: ProviderKind,
    pub model: Option<String>,
    /// Project display name. Not a path: a host-absolute path means nothing on
    /// a phone (`docs/multiplatform-plan.md` §3.4).
    pub project: Option<String>,
    /// Unix milliseconds of the most recent activity, for sorting.
    pub updated_at: u64,
    /// Highest `seq` the host currently holds, when it happens to know.
    ///
    /// Best-effort and therefore optional: answering it for every session
    /// would mean scanning every log on every list, and a host can hold
    /// thousands. `None` means "not computed", not "empty" — a client uses it
    /// to size a backfill or badge unread counts, and simply omits the badge
    /// when it is absent.
    pub latest_seq: Option<u64>,
    /// True while a turn is in flight.
    pub working: bool,
    /// True when the session is waiting on a human — the state a phone exists
    /// to serve, and the one worth a push notification.
    pub awaiting_approval: bool,
}

/// Why a handshake was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RefuseReason {
    /// No overlap between the peers' supported ranges. Carries the host's range
    /// so the client can say which side needs upgrading instead of "failed".
    UnsupportedVersion { host_min: u32, host_max: u32 },
    /// Missing, malformed, expired, or revoked token.
    Unauthorized,
    /// The host is shutting down or already serving its connection limit.
    Unavailable { detail: String },
    /// The pairing code was wrong, already used, or has expired.
    ///
    /// Deliberately one reason for all three: distinguishing them would let an
    /// attacker learn that a guessed code was *once* valid, which is most of
    /// what they need to know.
    PairingRejected,
}

/// Why the host could not apply a command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommandRejection {
    UnknownSession,
    /// The session exists but has no running provider, so there is nothing to
    /// send the command to.
    SessionNotLive,
    /// The provider does not support this command (steering, rewind, and live
    /// mode switches are not universal).
    Unsupported {
        detail: String,
    },
    /// The token does not grant this session.
    Forbidden,
}

/// The command identity needed to reconcile a rejection with optimistic state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RejectedCommand {
    Turn { delivery_id: u64 },
    Approval { request_id: String },
}

/// Why a session stopped producing events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEndReason {
    /// The provider process exited. The session's log remains readable.
    ProviderExited {
        detail: String,
    },
    Archived,
    Deleted,
}

/// Client to host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ClientFrame {
    /// Exchange a short-lived pairing code for a durable token.
    ///
    /// The one frame a client may send before `Hello`, because it is how a
    /// client that has no token gets one. A phone cannot be handed a
    /// credential the way a query string hands one to a browser, and asking
    /// someone to type a UUID off another screen is not a pairing flow.
    Pair {
        min_version: u32,
        max_version: u32,
        code: String,
        client: ClientInfo,
    },
    /// Must be the first frame; a host rejects anything else before it.
    Hello {
        min_version: u32,
        max_version: u32,
        client: ClientInfo,
        token: String,
    },
    ListSessions,
    /// Stream a session from `from_seq` exclusive — `Some(n)` means "I hold
    /// through n, send n+1 onward". `None` means from the beginning.
    Subscribe {
        session_id: String,
        from_seq: Option<u64>,
    },
    /// Stop streaming. The session keeps running on the host.
    Unsubscribe {
        session_id: String,
    },
    Command {
        session_id: String,
        command: SessionCommand,
    },
    /// Liveness. Mobile networks drop idle connections without closing them.
    Ping {
        nonce: u64,
    },
}

/// Host to client.
///
/// Not `PartialEq` — see [`SeqEvent`], which it carries.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum HostFrame {
    /// Handshake accepted. `version` is the agreed one, which both peers use
    /// from here on.
    Welcome {
        version: u32,
        host: HostInfo,
    },
    Refused {
        reason: RefuseReason,
    },
    /// A pairing code was accepted. The token is durable: a client stores it and
    /// never needs the code again.
    ///
    /// The connection is *not* authenticated by this — the client still sends
    /// `Hello` with the new token. Keeping the two steps separate means pairing
    /// has exactly one job, and a stolen `Paired` frame is worth no more than
    /// the token it carries.
    Paired {
        version: u32,
        token: String,
    },
    SessionList {
        sessions: Vec<SessionSummary>,
    },
    /// A batch of events, ascending and contiguous by `seq`.
    ///
    /// `caught_up` is false while the host is replaying backlog and true once
    /// the client is live. Clients use it to hold off rendering "waiting for
    /// the agent" states during a long replay.
    Events {
        session_id: String,
        events: Vec<SeqEvent>,
        caught_up: bool,
    },
    /// A command could not be applied. Sent even though the client cannot
    /// usually recover, because silence would leave it rendering a turn that
    /// never ran.
    CommandRejected {
        session_id: String,
        /// Older hosts omit this, so clients must retain a send-order fallback.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<RejectedCommand>,
        reason: CommandRejection,
    },
    SessionEnded {
        session_id: String,
        reason: SessionEndReason,
    },
    Pong {
        nonce: u64,
    },
}

macro_rules! json_codec {
    ($ty:ty) => {
        impl $ty {
            /// Encode for a WebSocket text frame.
            pub fn encode(&self) -> Result<String, serde_json::Error> {
                serde_json::to_string(self)
            }

            /// Decode one received frame.
            pub fn decode(text: &str) -> Result<Self, serde_json::Error> {
                serde_json::from_str(text)
            }
        }
    };
}

json_codec!(ClientFrame);
json_codec!(HostFrame);

#[cfg(test)]
mod tests {
    use super::*;

    fn client_info() -> ClientInfo {
        ClientInfo {
            client_id: "client-1".into(),
            display_name: "Pixel".into(),
            platform: "android".into(),
            app_version: "0.1.0".into(),
        }
    }

    fn every_client_frame() -> Vec<ClientFrame> {
        vec![
            ClientFrame::Hello {
                min_version: PROTOCOL_MIN_VERSION,
                max_version: PROTOCOL_MAX_VERSION,
                client: client_info(),
                token: "secret".into(),
            },
            ClientFrame::Pair {
                min_version: PROTOCOL_MIN_VERSION,
                max_version: PROTOCOL_MAX_VERSION,
                code: "K7M2QX".into(),
                client: client_info(),
            },
            ClientFrame::ListSessions,
            ClientFrame::Subscribe {
                session_id: "s1".into(),
                from_seq: Some(42),
            },
            ClientFrame::Subscribe {
                session_id: "s1".into(),
                from_seq: None,
            },
            ClientFrame::Unsubscribe {
                session_id: "s1".into(),
            },
            ClientFrame::Command {
                session_id: "s1".into(),
                command: SessionCommand::Interrupt,
            },
            ClientFrame::Ping { nonce: 7 },
        ]
    }

    fn every_host_frame() -> Vec<HostFrame> {
        vec![
            HostFrame::Welcome {
                version: PROTOCOL_MAX_VERSION,
                host: HostInfo {
                    host_id: "host-1".into(),
                    display_name: "workstation".into(),
                    platform: "macos".into(),
                    app_version: "0.1.0".into(),
                },
            },
            HostFrame::Refused {
                reason: RefuseReason::UnsupportedVersion {
                    host_min: 2,
                    host_max: 3,
                },
            },
            HostFrame::Refused {
                reason: RefuseReason::Unauthorized,
            },
            HostFrame::Refused {
                reason: RefuseReason::PairingRejected,
            },
            HostFrame::Paired {
                version: PROTOCOL_MAX_VERSION,
                token: "durable-token".into(),
            },
            HostFrame::SessionList {
                sessions: vec![SessionSummary {
                    session_id: "s1".into(),
                    title: "Fix the parser".into(),
                    provider: ProviderKind::Codex,
                    model: Some("gpt-5".into()),
                    project: Some("tcode".into()),
                    updated_at: 1_700_000_000_000,
                    latest_seq: Some(128),
                    working: true,
                    awaiting_approval: false,
                }],
            },
            HostFrame::Events {
                session_id: "s1".into(),
                events: vec![SeqEvent {
                    seq: 1,
                    ts: 1_700_000_000_000,
                    event: AgentEvent::TurnAccepted { delivery_id: 9 },
                }],
                caught_up: true,
            },
            HostFrame::CommandRejected {
                session_id: "s1".into(),
                command: Some(RejectedCommand::Approval {
                    request_id: "approval-1".into(),
                }),
                reason: CommandRejection::SessionNotLive,
            },
            HostFrame::SessionEnded {
                session_id: "s1".into(),
                reason: SessionEndReason::Archived,
            },
            HostFrame::Pong { nonce: 7 },
        ]
    }

    #[test]
    fn client_frames_round_trip() {
        for frame in every_client_frame() {
            let text = frame.encode().expect("frame must encode");
            let back = ClientFrame::decode(&text)
                .unwrap_or_else(|err| panic!("{frame:?} must decode from {text}: {err}"));
            assert_eq!(back, frame);
        }
    }

    /// Compares canonical encodings because `HostFrame` is not `PartialEq`
    /// (see its doc comment). Re-encoding what we decoded proves the decode
    /// lost nothing, which is the property a wire type owes its peer.
    #[test]
    fn host_frames_round_trip() {
        for frame in every_host_frame() {
            let text = frame.encode().expect("frame must encode");
            let reencoded = HostFrame::decode(&text)
                .unwrap_or_else(|err| panic!("{frame:?} must decode from {text}: {err}"))
                .encode()
                .expect("a decoded frame must re-encode");
            assert_eq!(reencoded, text, "round trip changed {frame:?}");
        }
    }

    #[test]
    fn command_rejection_correlation_is_backward_compatible() {
        for command in [
            Some(RejectedCommand::Turn { delivery_id: 9 }),
            Some(RejectedCommand::Approval {
                request_id: "approval-1".into(),
            }),
            None,
        ] {
            let frame = HostFrame::CommandRejected {
                session_id: "s1".into(),
                command,
                reason: CommandRejection::SessionNotLive,
            };
            let text = frame.encode().expect("rejection must encode");
            let reencoded = HostFrame::decode(&text)
                .expect("rejection must decode")
                .encode()
                .expect("decoded rejection must encode");
            assert_eq!(reencoded, text);
        }

        let older_host = r#"{"type":"command_rejected","data":{"session_id":"s1","reason":{"kind":"session_not_live"}}}"#;
        match HostFrame::decode(older_host).expect("older-host rejection must decode") {
            HostFrame::CommandRejected { command, .. } => assert_eq!(command, None),
            other => panic!("decoded as {other:?}"),
        }
    }

    /// `from_seq: None` (start from the beginning) and `from_seq: Some(0)` are
    /// different requests, and JSON null must not collapse them — a client
    /// resuming from nothing would otherwise be indistinguishable from one
    /// asking for a full backfill.
    #[test]
    fn absent_cursor_survives_encoding() {
        let text = ClientFrame::Subscribe {
            session_id: "s1".into(),
            from_seq: None,
        }
        .encode()
        .unwrap();
        match ClientFrame::decode(&text).unwrap() {
            ClientFrame::Subscribe { from_seq, .. } => assert_eq!(from_seq, None),
            other => panic!("decoded as {other:?}"),
        }
    }

    #[test]
    fn version_negotiation_picks_the_highest_shared_version() {
        // Exactly our range.
        assert_eq!(
            negotiate_version(PROTOCOL_MIN_VERSION, PROTOCOL_MAX_VERSION),
            Some(PROTOCOL_MAX_VERSION)
        );
        // A newer client that still speaks our version: meet at ours.
        assert_eq!(
            negotiate_version(PROTOCOL_MIN_VERSION, PROTOCOL_MAX_VERSION + 5),
            Some(PROTOCOL_MAX_VERSION)
        );
        // A future-only client.
        assert_eq!(
            negotiate_version(PROTOCOL_MAX_VERSION + 1, PROTOCOL_MAX_VERSION + 5),
            None
        );
        // A client too old to speak anything we still support.
        assert_eq!(negotiate_version(0, PROTOCOL_MIN_VERSION - 1), None);
    }

    /// A refusal must be actionable: the host's range travels with it so the
    /// client can tell the user which side is out of date.
    #[test]
    fn version_refusal_carries_the_hosts_range() {
        let text = HostFrame::Refused {
            reason: RefuseReason::UnsupportedVersion {
                host_min: 4,
                host_max: 7,
            },
        }
        .encode()
        .unwrap();
        assert!(text.contains("\"host_min\":4"), "{text}");
        assert!(text.contains("\"host_max\":7"), "{text}");
    }

    /// The frame carrying a `SessionCommand` must survive the enum-in-enum
    /// nesting. `SessionCommand` is adjacently tagged for its own reasons
    /// (newtype variants wrapping string-serializing enums), and nesting two
    /// tagged enums is exactly where serde representations tend to break.
    #[test]
    fn commands_survive_nesting_inside_a_frame() {
        let frame = ClientFrame::Command {
            session_id: "s1".into(),
            command: SessionCommand::SetInteractionMode(agent::InteractionMode::Plan),
        };
        let text = frame.encode().unwrap();
        assert_eq!(ClientFrame::decode(&text).unwrap(), frame);
    }
}
