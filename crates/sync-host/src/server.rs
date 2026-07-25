//! WebSocket transport for the host.
//!
//! Deliberately thin: read a text frame, hand it to [`Connection`], write back
//! whatever it returns. All the rules live in the state machine, where they are
//! testable without a socket — the only judgement here is about sockets.
//!
//! Bind happens synchronously so the caller learns the port before [`start`]
//! returns, matching the in-process MCP servers this app already runs.

use std::collections::HashMap;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::any;
use futures_util::{SinkExt as _, StreamExt as _};
use sync_protocol::{ClientFrame, HostFrame, HostInfo};
use tcode_services::store::SessionStore;

use crate::store_source::{CommandRequest, LiveSessions, StoreSource};
use crate::{Connection, HostConfig};

/// How many session-advance notifications may queue before a slow connection
/// starts missing them.
///
/// Missing one is survivable by design: a notification carries no data, only
/// "this session moved". A connection that lags simply drains from its cursor
/// on the next notification it does see, and catches up on everything at once.
const ADVANCE_BUFFER: usize = 256;

/// How connections learn that a subscribed session may have advanced.
#[derive(Debug, Clone, Copy)]
pub enum WakeSource {
    /// The embedding process calls [`SyncServer::notify_advanced`] after an
    /// append, so clients wake immediately without touching the filesystem.
    Broadcast,
    /// Check event-log metadata periodically for changes.
    ///
    /// The headless host uses this because its process never appends to the log
    /// and therefore has no in-process notification to subscribe to.
    Polling { interval: Duration },
}

/// A running sync server.
pub struct SyncServer {
    /// WebSocket endpoint, e.g. `ws://127.0.0.1:53213/sync`.
    pub url: String,
    /// The credential clients must present, supplied by the embedding app.
    pub token: String,
    /// Commands from remote clients, for the app to apply. Single consumer.
    pub commands: async_channel::Receiver<CommandRequest>,
    /// Per-session liveness for the app to publish into.
    pub live: LiveSessions,
    advance: Option<tokio::sync::broadcast::Sender<String>>,
}

impl SyncServer {
    /// Tell connected clients that a session's log has grown.
    ///
    /// Carries no payload: subscribers re-read from their own cursor, so a
    /// notification cannot deliver an event twice or out of order however many
    /// times it fires. Cheap enough to call on every appended event.
    pub fn notify_advanced(&self, session_id: &str) {
        // An error means nobody is listening, which is the normal case.
        if let Some(advance) = &self.advance {
            let _ = advance.send(session_id.to_owned());
        }
    }
}

#[derive(Clone)]
struct ServerState {
    store: SessionStore,
    host: HostInfo,
    token: String,
    live: LiveSessions,
    commands: async_channel::Sender<CommandRequest>,
    wake: WakeFactory,
}

#[derive(Clone)]
enum WakeFactory {
    Broadcast(tokio::sync::broadcast::Sender<String>),
    Polling { root: PathBuf, interval: Duration },
}

enum ConnectionWake {
    Broadcast(tokio::sync::broadcast::Receiver<String>),
    Polling(Box<PollWake>),
}

struct PollWake {
    root: PathBuf,
    stamps: HashMap<String, Option<FileStamp>>,
    interval: tokio::time::Interval,
}

enum WakeEvent {
    Session(String),
    AllSubscriptions,
    Poll,
    Closed,
}

enum ConnectionEvent {
    Socket(Option<Result<Message, axum::Error>>),
    Wake(WakeEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
}

/// Bind the desktop's deliberate ephemeral-loopback default.
///
/// Kept as the compact compatibility entry point used by socket-level tests.
/// Hosts that choose an address or wake policy use [`start_on`].
pub fn start(store: SessionStore, host: HostInfo, token: String) -> std::io::Result<SyncServer> {
    start_on(
        store,
        host,
        token,
        SocketAddr::from(([127, 0, 0, 1], 0)),
        WakeSource::Broadcast,
    )
}

/// Bind `address` and serve using the selected connection wake source.
pub fn start_on(
    store: SessionStore,
    host: HostInfo,
    token: String,
    address: SocketAddr,
    wake_source: WakeSource,
) -> std::io::Result<SyncServer> {
    let listener = TcpListener::bind(address)?;
    listener.set_nonblocking(true)?;
    let local_addr = listener.local_addr()?;
    let url = format!("ws://{local_addr}/sync");
    // Bounded: an unbounded queue would let a wedged app absorb commands
    // forever while every client believes they were delivered.
    let (command_tx, command_rx) = async_channel::bounded(256);
    let live = LiveSessions::new();
    let (wake, advance) = match wake_source {
        WakeSource::Broadcast => {
            let (advance, _) = tokio::sync::broadcast::channel(ADVANCE_BUFFER);
            (WakeFactory::Broadcast(advance.clone()), Some(advance))
        }
        WakeSource::Polling { interval } => (
            WakeFactory::Polling {
                root: store.root().clone(),
                interval,
            },
            None,
        ),
    };

    let state = ServerState {
        store,
        host,
        token: token.clone(),
        live: live.clone(),
        commands: command_tx,
        wake,
    };

    std::thread::Builder::new()
        .name("sync-host".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    log::error!("sync-host: failed to build tokio runtime: {err}");
                    return;
                }
            };
            runtime.block_on(async move {
                let listener = match tokio::net::TcpListener::from_std(listener) {
                    Ok(listener) => listener,
                    Err(err) => {
                        log::error!("sync-host: failed to adopt listener: {err}");
                        return;
                    }
                };
                let app = Router::new()
                    .route("/sync", any(upgrade))
                    .with_state(Arc::new(state));
                if let Err(err) = axum::serve(listener, app).await {
                    log::error!("sync-host: server exited with error: {err}");
                }
            });
        })?;

    log::info!("sync-host: serving at {url}");
    Ok(SyncServer {
        url,
        token,
        commands: command_rx,
        live,
        advance,
    })
}

async fn upgrade(upgrade: WebSocketUpgrade, State(state): State<Arc<ServerState>>) -> Response {
    upgrade.on_upgrade(move |socket| serve_connection(socket, state))
}

async fn serve_connection(socket: WebSocket, state: Arc<ServerState>) {
    let (mut sink, mut stream) = socket.split();
    let mut wake = state.wake.for_connection();
    let mut connection = Connection::new(
        StoreSource::new(
            state.store.clone(),
            state.live.clone(),
            state.commands.clone(),
        ),
        HostConfig {
            host: state.host.clone(),
            token: state.token.clone(),
        },
    );

    loop {
        let event = tokio::select! {
            incoming = stream.next() => ConnectionEvent::Socket(incoming),
            wake = wake.next() => ConnectionEvent::Wake(wake),
        };
        let outgoing = match event {
            ConnectionEvent::Socket(incoming) => {
                match incoming {
                    Some(Ok(Message::Text(text))) => match ClientFrame::decode(&text) {
                        Ok(frame) => connection.handle(frame),
                        Err(err) => {
                            // Undecodable input is a broken or hostile peer.
                            // There is no frame that means "you sent gibberish"
                            // and inventing one would only help an attacker
                            // probe the parser.
                            log::warn!("sync-host: dropping undecodable frame: {err}");
                            break;
                        }
                    },
                    Some(Ok(Message::Close(_))) | None => break,
                    // Ping/Pong are handled by axum; binary frames are not part
                    // of this protocol.
                    Some(Ok(_)) => Vec::new(),
                    Some(Err(err)) => {
                        log::debug!("sync-host: connection closed: {err}");
                        break;
                    }
                }
            }
            ConnectionEvent::Wake(WakeEvent::Session(session_id)) => connection.drain(&session_id),
            // A lagged broadcast identifies no particular missed session, so
            // cursors are the authority and every subscription is re-drained.
            ConnectionEvent::Wake(WakeEvent::AllSubscriptions) => {
                drain_all_subscriptions(&mut connection)
            }
            ConnectionEvent::Wake(WakeEvent::Poll) => wake.drain_changed(&mut connection),
            ConnectionEvent::Wake(WakeEvent::Closed) => break,
        };

        for frame in outgoing {
            let Ok(text) = frame.encode() else {
                log::error!("sync-host: failed to encode a host frame");
                continue;
            };
            if sink.send(Message::Text(text.into())).await.is_err() {
                return;
            }
        }
        wake.sync_subscriptions(&connection);

        // A refusal is the last thing this connection ever says.
        if connection.is_refused() {
            let _ = sink.send(Message::Close(None)).await;
            return;
        }
    }
}

impl WakeFactory {
    fn for_connection(&self) -> ConnectionWake {
        match self {
            Self::Broadcast(advance) => ConnectionWake::Broadcast(advance.subscribe()),
            Self::Polling { root, interval } => ConnectionWake::Polling(Box::new(PollWake {
                root: root.clone(),
                stamps: HashMap::new(),
                interval: tokio::time::interval(*interval),
            })),
        }
    }
}

impl ConnectionWake {
    async fn next(&mut self) -> WakeEvent {
        match self {
            Self::Broadcast(advance) => match advance.recv().await {
                Ok(session_id) => WakeEvent::Session(session_id),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                    log::debug!("sync-host: connection lagged {missed} notifications");
                    WakeEvent::AllSubscriptions
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => WakeEvent::Closed,
            },
            Self::Polling(poll) => {
                poll.interval.tick().await;
                WakeEvent::Poll
            }
        }
    }

    fn drain_changed(&mut self, connection: &mut Connection<StoreSource>) -> Vec<HostFrame> {
        let Self::Polling(poll) = self else {
            return Vec::new();
        };
        let session_ids = connection
            .subscribed_sessions()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let mut outgoing = Vec::new();
        for session_id in session_ids {
            let current = event_log_stamp(&poll.root, &session_id);
            match poll.stamps.insert(session_id.clone(), current) {
                Some(previous) if previous != current => {
                    outgoing.extend(connection.drain(&session_id));
                }
                _ => {}
            }
        }
        outgoing
    }

    fn sync_subscriptions(&mut self, connection: &Connection<StoreSource>) {
        let Self::Polling(poll) = self else {
            return;
        };
        poll.stamps.retain(|session_id, _| {
            connection
                .subscribed_sessions()
                .any(|subscribed| subscribed == session_id)
        });
        for session_id in connection.subscribed_sessions() {
            poll.stamps
                .entry(session_id.to_owned())
                .or_insert_with(|| event_log_stamp(&poll.root, session_id));
        }
    }
}

fn drain_all_subscriptions(connection: &mut Connection<StoreSource>) -> Vec<HostFrame> {
    let sessions = connection
        .subscribed_sessions()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    sessions
        .iter()
        .flat_map(|session_id| connection.drain(session_id))
        .collect()
}

fn event_log_stamp(root: &Path, session_id: &str) -> Option<FileStamp> {
    let metadata = std::fs::metadata(root.join(format!("{session_id}.jsonl"))).ok()?;
    Some(FileStamp {
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host_info() -> HostInfo {
        HostInfo {
            host_id: "host-1".into(),
            display_name: "workstation".into(),
            platform: "test".into(),
            app_version: "0.1.0".into(),
        }
    }

    fn temp_store() -> SessionStore {
        let mut root = std::env::temp_dir();
        root.push(format!("tcode-sync-server-test-{}", uuid::Uuid::new_v4()));
        SessionStore::open_at(root).expect("temp store")
    }

    #[test]
    fn start_binds_a_loopback_port_and_uses_the_supplied_token() {
        let store = temp_store();
        let server =
            start(store.clone(), host_info(), "stable-token".into()).expect("server starts");

        assert!(
            server.url.starts_with("ws://127.0.0.1:"),
            "must not bind beyond loopback: {}",
            server.url
        );
        assert!(server.url.ends_with("/sync"));
        assert_eq!(server.token, "stable-token");
        let _ = std::fs::remove_dir_all(store.root());
    }

    #[test]
    fn separate_servers_can_share_a_persisted_token() {
        let store = temp_store();
        let a = start(store.clone(), host_info(), "stable-token".into()).expect("first");
        let b = start(store.clone(), host_info(), "stable-token".into()).expect("second");
        assert_eq!(a.token, b.token);
        assert_ne!(a.url, b.url);
        let _ = std::fs::remove_dir_all(store.root());
    }

    #[test]
    fn start_on_uses_the_supplied_bind_address() {
        let store = temp_store();
        let server = start_on(
            store.clone(),
            host_info(),
            "stable-token".into(),
            SocketAddr::from(([127, 0, 0, 1], 0)),
            WakeSource::Polling {
                interval: Duration::from_millis(250),
            },
        )
        .expect("server starts");

        assert!(server.url.starts_with("ws://127.0.0.1:"));
        assert!(server.url.ends_with("/sync"));
        let _ = std::fs::remove_dir_all(store.root());
    }

    /// Notifying with nobody connected must not fail or block: the app calls
    /// this on every appended event, whether or not a client exists.
    #[test]
    fn notifying_with_no_subscribers_is_harmless() {
        let store = temp_store();
        let server = start(store.clone(), host_info(), "test-token".into()).expect("server starts");
        server.notify_advanced("s1");
        server.notify_advanced("s1");
        let _ = std::fs::remove_dir_all(store.root());
    }
}
