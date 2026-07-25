//! Headless, read-only tcode sync host.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::any;
use futures_util::{SinkExt as _, StreamExt as _};
use sync_host::{CommandRequest, Connection, HostConfig, LiveSessions, StoreSource};
use sync_protocol::{ClientFrame, HostFrame, HostInfo, SessionCommand};
use tcode_services::settings::SettingsStore;
use tcode_services::store::SessionStore;

const HELP: &str = "\
tcode-server — serve tcode session history over WebSocket

Usage: tcode-server [OPTIONS]

Options:
  --bind <IP[:PORT]>  Address to listen on [default: 127.0.0.1:0]
                      Port 0 selects an available port. Widening the IP exposes
                      unencrypted traffic; the sync token is the only protection.
  --print-token       Print the sync token to stdout and exit
  -h, --help          Print help
";

const LIVE_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, PartialEq, Eq)]
struct Options {
    bind: SocketAddr,
    print_token: bool,
    help: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            print_token: false,
            help: false,
        }
    }
}

impl Options {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut options = Self::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => options.help = true,
                "--print-token" => options.print_token = true,
                "--bind" => {
                    let value = args
                        .next()
                        .ok_or_else(|| "--bind requires an IP address".to_owned())?;
                    options.bind = parse_bind(&value)?;
                }
                _ => {
                    if let Some(value) = arg.strip_prefix("--bind=") {
                        options.bind = parse_bind(value)?;
                    } else {
                        return Err(format!("unknown option: {arg}"));
                    }
                }
            }
        }
        Ok(options)
    }
}

fn parse_bind(value: &str) -> Result<SocketAddr, String> {
    if let Ok(address) = value.parse::<SocketAddr>() {
        return Ok(address);
    }
    value
        .parse::<IpAddr>()
        .map(|ip| SocketAddr::new(ip, 0))
        .map_err(|_| format!("invalid bind address {value:?}; expected IP[:PORT]"))
}

#[derive(Clone)]
struct ServerState {
    store: SessionStore,
    host: HostInfo,
    token: String,
    live: LiveSessions,
    commands: async_channel::Sender<CommandRequest>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
}

fn event_log_stamp(root: &Path, session_id: &str) -> Option<FileStamp> {
    let metadata = std::fs::metadata(root.join(format!("{session_id}.jsonl"))).ok()?;
    Some(FileStamp {
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

#[tokio::main]
async fn main() {
    env_logger::init();

    let options = match Options::parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(err) => {
            eprintln!("error: {err}\n\n{HELP}");
            std::process::exit(2);
        }
    };
    if options.help {
        print!("{HELP}");
        return;
    }
    if let Err(err) = run(options).await {
        log::error!("tcode-server: {err}");
        std::process::exit(1);
    }
}

async fn run(options: Options) -> io::Result<()> {
    let store = SessionStore::open_default()?;
    let settings = SettingsStore::new(store.root().clone());
    let token = settings.load_or_create_sync_token()?;

    if options.print_token {
        println!("{token}");
        return Ok(());
    }

    let display_name = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "tcode".into());
    let host = HostInfo {
        host_id: format!("{display_name}:{}", store.root().display()),
        display_name,
        platform: std::env::consts::OS.into(),
        app_version: env!("CARGO_PKG_VERSION").into(),
    };

    let listener = tokio::net::TcpListener::bind(options.bind).await?;
    let local_addr = listener.local_addr()?;
    let url = format!("ws://{local_addr}/sync");
    let (command_tx, command_rx) = async_channel::bounded(256);
    let live = LiveSessions::new();
    let state = Arc::new(ServerState {
        store,
        host,
        token,
        live,
        commands: command_tx,
    });

    log::info!("tcode-server: serving at {url}");
    log::info!(
        "tcode-server: data directory {}",
        state.store.root().display()
    );
    log::warn!("tcode-server: read-only host; serves history and live events but cannot run turns");

    let command_drain = tokio::spawn(drain_commands(command_rx));
    let app = Router::new().route("/sync", any(upgrade)).with_state(state);
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await;
    command_drain.abort();
    result
}

async fn shutdown_signal() {
    if let Err(err) = tokio::signal::ctrl_c().await {
        log::error!("tcode-server: failed to listen for Ctrl-C: {err}");
    }
    log::info!("tcode-server: stopping");
}

async fn drain_commands(commands: async_channel::Receiver<CommandRequest>) {
    while let Ok(request) = commands.recv().await {
        log::warn!(
            "tcode-server: dropped {} command for session {}: no provider process",
            command_kind(&request.command),
            request.session_id
        );
    }
}

fn command_kind(command: &SessionCommand) -> &'static str {
    match command {
        SessionCommand::SendTurn { .. } => "send-turn",
        SessionCommand::Interrupt => "interrupt",
        SessionCommand::RespondApproval { .. } => "approval-response",
        SessionCommand::RespondUserInput { .. } => "user-input-response",
        SessionCommand::SetApprovalMode(_) => "set-approval-mode",
        SessionCommand::Steer { .. } => "steer",
        SessionCommand::SetInteractionMode(_) => "set-interaction-mode",
        SessionCommand::SetOption { .. } => "set-option",
        SessionCommand::Rewind { .. } => "rewind",
        SessionCommand::Shutdown => "shutdown",
    }
}

async fn upgrade(upgrade: WebSocketUpgrade, State(state): State<Arc<ServerState>>) -> Response {
    upgrade.on_upgrade(move |socket| serve_connection(socket, state))
}

async fn serve_connection(socket: WebSocket, state: Arc<ServerState>) {
    let (mut sink, mut stream) = socket.split();
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
    let mut stamps = HashMap::<String, Option<FileStamp>>::new();
    let mut poll = tokio::time::interval(LIVE_POLL_INTERVAL);

    loop {
        let outgoing = tokio::select! {
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => match ClientFrame::decode(&text) {
                        Ok(frame) => connection.handle(frame),
                        Err(err) => {
                            log::warn!("tcode-server: dropping undecodable frame: {err}");
                            break;
                        }
                    },
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => Vec::new(),
                    Some(Err(err)) => {
                        log::debug!("tcode-server: connection closed: {err}");
                        break;
                    }
                }
            }
            _ = poll.tick() => {
                drain_changed_sessions(&mut connection, state.store.root(), &mut stamps)
            }
        };

        for frame in outgoing {
            if send_frame(&mut sink, frame).await.is_err() {
                return;
            }
        }

        stamps.retain(|session_id, _| {
            connection
                .subscribed_sessions()
                .any(|subscribed| subscribed == session_id)
        });
        for session_id in connection.subscribed_sessions() {
            stamps
                .entry(session_id.to_owned())
                .or_insert_with(|| event_log_stamp(state.store.root(), session_id));
        }

        if connection.is_refused() {
            let _ = sink.send(Message::Close(None)).await;
            return;
        }
    }
}

fn drain_changed_sessions(
    connection: &mut Connection<StoreSource>,
    root: &Path,
    stamps: &mut HashMap<String, Option<FileStamp>>,
) -> Vec<HostFrame> {
    let session_ids = connection
        .subscribed_sessions()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut outgoing = Vec::new();
    for session_id in session_ids {
        let current = event_log_stamp(root, &session_id);
        match stamps.insert(session_id.clone(), current) {
            Some(previous) if previous != current => outgoing.extend(connection.drain(&session_id)),
            _ => {}
        }
    }
    outgoing
}

async fn send_frame(
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    frame: HostFrame,
) -> Result<(), ()> {
    let text = frame.encode().map_err(|err| {
        log::error!("tcode-server: failed to encode a host frame: {err}");
    })?;
    sink.send(Message::Text(text.into())).await.map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_an_ephemeral_loopback_port() {
        assert_eq!(
            Options::parse(Vec::<String>::new()).unwrap(),
            Options::default()
        );
    }

    #[test]
    fn bind_accepts_an_ip_with_or_without_a_port() {
        assert_eq!(
            Options::parse(["--bind".into(), "0.0.0.0".into()])
                .unwrap()
                .bind,
            "0.0.0.0:0".parse().unwrap()
        );
        assert_eq!(
            Options::parse(["--bind=[::1]:9876".into()]).unwrap().bind,
            "[::1]:9876".parse().unwrap()
        );
    }

    #[test]
    fn unknown_options_are_rejected() {
        assert_eq!(
            Options::parse(["--listen".into()]).unwrap_err(),
            "unknown option: --listen"
        );
    }
}
