//! End-to-end over a real WebSocket.
//!
//! The unit tests cover the state machine without a socket, which is where the
//! protocol rules belong. This file covers the part they cannot: that a client
//! can actually reach the host, that the token is enforced on the wire, and
//! that an event appended after a client subscribed reaches it live.
//!
//! That last property is the one worth a real socket. It spans the store, the
//! sequence numbers, the advance broadcast, and the cursor — every piece has to
//! agree, and a fake transport would let them disagree quietly.

use std::path::PathBuf;
use std::time::Duration;

use agent::{AgentEvent, ProviderKind};
use futures_util::{SinkExt as _, StreamExt as _};
use sync_host::{SyncServer, start};
use sync_protocol::{ClientFrame, ClientInfo, HostFrame, HostInfo, RefuseReason};
use tcode_core::project::SessionMeta;
use tcode_services::store::SessionStore;
use tokio_tungstenite::tungstenite::Message;

/// Long enough that a loaded machine does not fail the test, short enough that
/// a genuine hang is still a fast failure rather than a stuck suite.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn temp_store() -> SessionStore {
    let mut root = std::env::temp_dir();
    root.push(format!("tcode-sync-e2e-{}", uuid::Uuid::new_v4()));
    SessionStore::open_at(root).expect("temp store")
}

fn host_info() -> HostInfo {
    HostInfo {
        host_id: "host-1".into(),
        display_name: "workstation".into(),
        platform: "test".into(),
        app_version: "0.1.0".into(),
    }
}

fn client_info() -> ClientInfo {
    ClientInfo {
        client_id: "client-1".into(),
        display_name: "Pixel".into(),
        platform: "android".into(),
        app_version: "0.1.0".into(),
    }
}

fn event(n: u64) -> AgentEvent {
    AgentEvent::TurnAccepted { delivery_id: n }
}

fn start_server(store: SessionStore) -> SyncServer {
    start(store, host_info(), "correct-horse".into()).expect("server starts")
}

/// A session with `len` events already logged.
fn seed(store: &SessionStore, len: u64) -> String {
    let meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/work/alpha"), None);
    store.upsert_meta(&meta).expect("meta");
    for n in 1..=len {
        store
            .append_event(&meta.id, 1_000 + n, &event(n))
            .expect("append");
    }
    meta.id
}

async fn connect(server: &SyncServer) -> Socket {
    let (socket, _) = tokio_tungstenite::connect_async(&server.url)
        .await
        .expect("client connects");
    socket
}

async fn send(socket: &mut Socket, frame: ClientFrame) {
    socket
        .send(Message::Text(frame.encode().expect("encodes").into()))
        .await
        .expect("client sends");
}

/// Next host frame, or a failure naming what we were waiting for.
async fn recv(socket: &mut Socket, waiting_for: &str) -> HostFrame {
    let message = tokio::time::timeout(REPLY_TIMEOUT, socket.next())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {waiting_for}"))
        .unwrap_or_else(|| panic!("socket closed while waiting for {waiting_for}"))
        .expect("socket error");
    match message {
        Message::Text(text) => HostFrame::decode(&text).expect("host frame decodes"),
        other => panic!("expected a text frame while waiting for {waiting_for}, got {other:?}"),
    }
}

fn hello(token: &str) -> ClientFrame {
    ClientFrame::Hello {
        min_version: sync_protocol::PROTOCOL_MIN_VERSION,
        max_version: sync_protocol::PROTOCOL_MAX_VERSION,
        client: client_info(),
        token: token.into(),
    }
}

fn seqs(frame: &HostFrame) -> Vec<u64> {
    match frame {
        HostFrame::Events { events, .. } => events.iter().map(|event| event.seq).collect(),
        other => panic!("expected events, got {other:?}"),
    }
}

#[tokio::test]
async fn a_client_handshakes_backfills_and_receives_a_live_event() {
    let store = temp_store();
    let session_id = seed(&store, 3);
    let server = start_server(store.clone());

    let mut socket = connect(&server).await;

    send(&mut socket, hello(&server.token)).await;
    match recv(&mut socket, "welcome").await {
        HostFrame::Welcome { version, host } => {
            assert_eq!(version, sync_protocol::PROTOCOL_MAX_VERSION);
            assert_eq!(host.host_id, "host-1");
        }
        other => panic!("expected a welcome, got {other:?}"),
    }

    send(&mut socket, ClientFrame::ListSessions).await;
    match recv(&mut socket, "session list").await {
        HostFrame::SessionList { sessions } => {
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].session_id, session_id);
            // The host's working directory must not travel.
            assert_eq!(sessions[0].project.as_deref(), Some("alpha"));
        }
        other => panic!("expected a session list, got {other:?}"),
    }

    // Backfill from scratch.
    send(
        &mut socket,
        ClientFrame::Subscribe {
            session_id: session_id.clone(),
            from_seq: None,
        },
    )
    .await;
    let backfill = recv(&mut socket, "backfill").await;
    assert_eq!(seqs(&backfill), vec![1, 2, 3]);
    assert!(matches!(
        backfill,
        HostFrame::Events {
            caught_up: true,
            ..
        }
    ));

    // The live path: append, notify, and the subscriber gets only what is new.
    let seq = store
        .append_event(&session_id, 2_000, &event(4))
        .expect("append");
    assert_eq!(seq, 4, "the store continues the sequence");
    server.notify_advanced(&session_id);

    let live = recv(&mut socket, "the live event").await;
    assert_eq!(
        seqs(&live),
        vec![4],
        "a live event must not repeat backfill"
    );

    let _ = std::fs::remove_dir_all(store.root());
}

/// The cursor is what makes a dropped mobile connection cheap: reconnecting
/// costs exactly the events that were missed, not the whole log.
#[tokio::test]
async fn reconnecting_with_a_cursor_replays_only_the_gap() {
    let store = temp_store();
    let session_id = seed(&store, 5);
    let server = start_server(store.clone());

    let mut socket = connect(&server).await;
    send(&mut socket, hello(&server.token)).await;
    recv(&mut socket, "welcome").await;
    drop(socket);

    let mut socket = connect(&server).await;
    send(&mut socket, hello(&server.token)).await;
    recv(&mut socket, "welcome").await;
    send(
        &mut socket,
        ClientFrame::Subscribe {
            session_id: session_id.clone(),
            from_seq: Some(3),
        },
    )
    .await;
    assert_eq!(seqs(&recv(&mut socket, "the gap").await), vec![4, 5]);

    let _ = std::fs::remove_dir_all(store.root());
}

/// The token must be enforced by the server that actually runs, not only by the
/// state machine in isolation.
#[tokio::test]
async fn a_wrong_token_is_refused_and_the_socket_closes() {
    let store = temp_store();
    seed(&store, 1);
    let server = start_server(store.clone());

    let mut socket = connect(&server).await;
    send(&mut socket, hello("not-the-token")).await;

    match recv(&mut socket, "refusal").await {
        HostFrame::Refused {
            reason: RefuseReason::Unauthorized,
        } => {}
        other => panic!("expected an unauthorized refusal, got {other:?}"),
    }

    // The host hangs up rather than leaving a rejected peer holding a socket.
    let closed = tokio::time::timeout(REPLY_TIMEOUT, socket.next())
        .await
        .expect("host must close the socket");
    assert!(
        matches!(closed, None | Some(Ok(Message::Close(_))) | Some(Err(_))),
        "expected a close, got {closed:?}"
    );

    let _ = std::fs::remove_dir_all(store.root());
}

/// A client that never authenticates must not be able to read anything.
#[tokio::test]
async fn subscribing_without_a_handshake_is_refused() {
    let store = temp_store();
    let session_id = seed(&store, 3);
    let server = start_server(store.clone());

    let mut socket = connect(&server).await;
    send(
        &mut socket,
        ClientFrame::Subscribe {
            session_id,
            from_seq: None,
        },
    )
    .await;

    match recv(&mut socket, "refusal").await {
        HostFrame::Refused {
            reason: RefuseReason::Unauthorized,
        } => {}
        other => panic!("expected a refusal, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(store.root());
}

/// A command from a client must arrive on the channel the app drains, with its
/// session id intact.
#[tokio::test]
async fn a_command_reaches_the_apps_channel() {
    let store = temp_store();
    let session_id = seed(&store, 1);
    let server = start_server(store.clone());

    let mut socket = connect(&server).await;
    send(&mut socket, hello(&server.token)).await;
    recv(&mut socket, "welcome").await;
    send(
        &mut socket,
        ClientFrame::Command {
            session_id: session_id.clone(),
            command: sync_protocol::SessionCommand::Interrupt,
        },
    )
    .await;

    let request = tokio::time::timeout(REPLY_TIMEOUT, server.commands.recv())
        .await
        .expect("the app must receive the command")
        .expect("channel open");
    assert_eq!(request.session_id, session_id);
    assert_eq!(request.command, sync_protocol::SessionCommand::Interrupt);

    let _ = std::fs::remove_dir_all(store.root());
}
