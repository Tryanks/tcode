//! WebSocket transport for the host.
//!
//! Deliberately thin: read a text frame, hand it to [`Connection`], write back
//! whatever it returns. All the rules live in the state machine, where they are
//! testable without a socket — the only judgement here is about sockets.
//!
//! Bind happens synchronously so the caller learns the port before [`start`]
//! returns, matching the in-process MCP servers this app already runs.

use std::net::TcpListener;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::any;
use futures_util::{SinkExt as _, StreamExt as _};
use sync_protocol::{ClientFrame, HostInfo};
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
    advance: tokio::sync::broadcast::Sender<String>,
}

impl SyncServer {
    /// Tell connected clients that a session's log has grown.
    ///
    /// Carries no payload: subscribers re-read from their own cursor, so a
    /// notification cannot deliver an event twice or out of order however many
    /// times it fires. Cheap enough to call on every appended event.
    pub fn notify_advanced(&self, session_id: &str) {
        // An error means nobody is listening, which is the normal case.
        let _ = self.advance.send(session_id.to_owned());
    }
}

#[derive(Clone)]
struct ServerState {
    store: SessionStore,
    host: HostInfo,
    token: String,
    live: LiveSessions,
    commands: async_channel::Sender<CommandRequest>,
    advance: tokio::sync::broadcast::Sender<String>,
}

/// Bind a loopback port and serve on a dedicated tokio runtime thread.
///
/// Loopback only for now: exposing this to a LAN is a decision about
/// trust boundaries and pairing UX, not about transport, and it should be made
/// deliberately rather than inherited from a default.
pub fn start(store: SessionStore, host: HostInfo, token: String) -> std::io::Result<SyncServer> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let url = format!("ws://127.0.0.1:{port}/sync");
    // Bounded: an unbounded queue would let a wedged app absorb commands
    // forever while every client believes they were delivered.
    let (command_tx, command_rx) = async_channel::bounded(256);
    let (advance_tx, _) = tokio::sync::broadcast::channel(ADVANCE_BUFFER);
    let live = LiveSessions::new();

    let state = ServerState {
        store,
        host,
        token: token.clone(),
        live: live.clone(),
        commands: command_tx,
        advance: advance_tx.clone(),
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
        advance: advance_tx,
    })
}

async fn upgrade(upgrade: WebSocketUpgrade, State(state): State<Arc<ServerState>>) -> Response {
    upgrade.on_upgrade(move |socket| serve_connection(socket, state))
}

async fn serve_connection(socket: WebSocket, state: Arc<ServerState>) {
    let (mut sink, mut stream) = socket.split();
    let mut advance = state.advance.subscribe();
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
        let outgoing = tokio::select! {
            incoming = stream.next() => {
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
            advanced = advance.recv() => {
                match advanced {
                    Ok(session_id) => connection.drain(&session_id),
                    // Lagged: this connection missed notifications while busy.
                    // Every subscription is re-drained, since a missed
                    // notification is only ever "some session moved" and the
                    // cursors still say precisely what was not sent.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        log::debug!("sync-host: connection lagged {missed} notifications");
                        let sessions: Vec<String> = connection
                            .subscribed_sessions()
                            .map(str::to_owned)
                            .collect();
                        sessions
                            .iter()
                            .flat_map(|session_id| connection.drain(session_id))
                            .collect()
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
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

        // A refusal is the last thing this connection ever says.
        if connection.is_refused() {
            let _ = sink.send(Message::Close(None)).await;
            return;
        }
    }
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
