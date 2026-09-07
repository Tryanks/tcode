use rustls::pki_types::ServerName;
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_channel::{Receiver, Sender};
use async_tungstenite::WebSocketStream;
use futures_util::{FutureExt as _, StreamExt as _};
use serde::Deserialize;
use smol::Async;
use tungstenite::Message;

pub use tcode_client::pairing::{
    PairInvite, PairedHost, is_pairing_code, lan_origin, pair_url, parse_pair_url,
};
pub use tcode_client::{ConnectionFailure, ConnectionState};

use tcode_client::{
    outgoing::{Outgoing, OutgoingReceiver, subscription_key},
    recovery::{Backoff, Wake},
};

pub struct RemoteClient {
    pub to_host: Outgoing,
    pub from_host: Receiver<String>,
    pub state: Receiver<ConnectionState>,
}

#[derive(Deserialize)]
struct PairResponse {
    host_id: String,
    host_name: String,
    token: String,
}

pub fn pair(origin: &str, code: &str, device_name: &str) -> Result<PairedHost, String> {
    let origin = tcode_client::pairing::parse_origin(origin)?;
    if !is_pairing_code(code) || device_name.is_empty() || device_name.len() > 256 {
        return Err("invalid pairing request".into());
    }
    let body = serde_json::json!({ "code": code, "device_name": device_name }).to_string();
    let bytes = http(&origin, "POST", "/pair", &body)?;
    let response: PairResponse =
        serde_json::from_slice(&bytes).map_err(|_| "invalid pairing response")?;
    Ok(PairedHost {
        host_id: response.host_id,
        name: response.host_name,
        origin,
        token: response.token,
        last_connected_unix: None,
    })
}

/// Bounded HTTP/1.1; HTTPS origins use the standard WebPKI trust roots.
pub fn http(origin: &str, method: &str, path: &str, body: &str) -> Result<Vec<u8>, String> {
    use std::io::{Read as _, Write as _};
    if !matches!(
        (method, path),
        ("POST", "/pair") | ("GET", "/admin/pair") | ("GET", "/") | ("HEAD", "/")
    ) || body.len() > 4096
    {
        return Err("invalid HTTP request".into());
    }
    let url = url::Url::parse(origin).map_err(|e| e.to_string())?;
    let addr = url
        .host_str()
        .ok_or("missing host")?
        .trim_matches(['[', ']']);
    let port = url.port_or_known_default().ok_or("missing port")?;
    let socket = TcpStream::connect_timeout(&socket_addr(addr, port)?, Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    socket
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    trait HttpStream: std::io::Read + std::io::Write {}
    impl<T: std::io::Read + std::io::Write> HttpStream for T {}
    let mut stream: Box<dyn HttpStream> = if url.scheme() == "https" {
        let session = rustls::ClientConnection::new(
            tls_client_config()?,
            ServerName::try_from(addr.to_owned()).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        Box::new(rustls::StreamOwned::new(session, socket))
    } else if url.scheme() == "http" {
        Box::new(socket)
    } else {
        return Err("unsupported scheme".into());
    };
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        authority(addr, port),
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    // A tunnel may close TLS without close_notify. Require the complete
    // Content-Length-delimited response before accepting such a close.
    let read = (&mut stream).take(65537).read_to_end(&mut bytes);
    if bytes.len() > 65536 {
        return Err("HTTP response too large".into());
    }
    let split = bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| {
            read.err()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "invalid HTTP response".into())
        })?;
    let head = std::str::from_utf8(&bytes[..split]).map_err(|_| "invalid HTTP response")?;
    if !head.starts_with("HTTP/1.1 200 ") {
        return Err(head.lines().next().unwrap_or("HTTP failure").to_owned());
    }
    let length: usize = head
        .lines()
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse().ok())
        .ok_or("missing Content-Length")?;
    let body = &bytes[split + 4..];
    if body.len() != length {
        return Err("incomplete HTTP response".into());
    }
    Ok(body.to_vec())
}

pub fn load_hosts(data_dir: &Path) -> io::Result<Vec<PairedHost>> {
    match fs::read(data_dir.join("hosts.json")) {
        Ok(bytes) => {
            let hosts: Vec<PairedHost> =
                serde_json::from_slice(&bytes).map_err(io::Error::other)?;
            Ok(hosts)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

pub fn save_hosts(data_dir: &Path, hosts: &[PairedHost]) -> io::Result<()> {
    fs::create_dir_all(data_dir)?;
    let bytes = serde_json::to_vec_pretty(hosts).map_err(io::Error::other)?;
    let temporary = data_dir.join("hosts.json.tmp");
    use std::io::Write as _;
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(temporary, data_dir.join("hosts.json"))
}

pub fn connect(host: PairedHost, device_name: String) -> RemoteClient {
    let (to_host, outgoing) = tcode_client::outgoing::channel();
    let (incoming, from_host) = async_channel::unbounded();
    let (state_tx, state) = async_channel::unbounded();
    std::thread::Builder::new()
        .name("tcode-remote-client".into())
        .spawn(move || {
            smol::block_on(connection_loop(
                host,
                device_name,
                outgoing,
                incoming,
                state_tx,
            ));
        })
        .expect("failed to spawn remote client thread");
    RemoteClient {
        to_host,
        from_host,
        state,
    }
}

async fn connection_loop(
    mut host: PairedHost,
    device_name: String,
    outgoing: OutgoingReceiver,
    incoming: Sender<String>,
    state: Sender<ConnectionState>,
) {
    let mut buffered = VecDeque::<String>::new();
    let mut subscriptions = HashMap::<String, String>::new();
    let mut backoff = Backoff::default();
    let mut reason = None;
    while !outgoing.is_closed() && !incoming.is_closed() {
        let _ = state
            .send(ConnectionState::Reconnecting {
                attempt: backoff.attempt(),
                reason,
            })
            .await;
        let mut stable_ms = 0;
        let mut interrupted = None;
        let opened = futures_lite::future::race(
            async { Ok(race_addresses(&host, &device_name).await) },
            async { Err(outgoing.wake.recv().await) },
        )
        .await;
        let opened = match opened {
            Ok(result) => result,
            Err(wake) => {
                if let Ok(Wake::Origin(origin)) = wake {
                    host.origin = origin;
                }
                continue;
            }
        };
        let failure = match opened {
            Ok(mut websocket) => {
                let _ = state.send(ConnectionState::Syncing).await;
                let mut failure = None;
                for line in subscriptions.values() {
                    if let Err(error) = send_interruptible(
                        &mut websocket,
                        Message::Text(line.trim_end().to_owned().into()),
                        Instant::now() + Duration::from_secs(10),
                        &outgoing,
                        &mut interrupted,
                    )
                    .await
                    {
                        failure = Some(error);
                        break;
                    }
                }
                if failure.is_none() {
                    while let Some(line) = buffered.pop_front() {
                        if subscription_key(&line).is_some() {
                            continue;
                        }
                        if let Err(error) = send_interruptible(
                            &mut websocket,
                            Message::Text(line.trim_end().to_owned().into()),
                            Instant::now() + Duration::from_secs(10),
                            &outgoing,
                            &mut interrupted,
                        )
                        .await
                        {
                            buffered.push_front(line);
                            failure = Some(error);
                            break;
                        }
                        outgoing.sent(&line);
                    }
                }
                match failure {
                    Some(error) => error,
                    None => {
                        let started = Instant::now();
                        let (error, healthy, wake) = relay_connected(
                            &mut websocket,
                            &outgoing,
                            &incoming,
                            &mut subscriptions,
                            &mut buffered,
                            &state,
                        )
                        .await;
                        if healthy {
                            stable_ms = started.elapsed().as_millis() as u64;
                        }
                        if let Some(wake) = wake {
                            if let Wake::Origin(origin) = wake {
                                host.origin = origin;
                            }
                            continue;
                        }
                        error
                    }
                }
            }
            Err(error) => error,
        };
        if let Some(wake) = interrupted {
            if let Wake::Origin(origin) = wake {
                host.origin = origin;
            }
            continue;
        }
        if failure.is_terminal() {
            let _ = state
                .send(ConnectionState::Offline { reason: failure })
                .await;
            incoming.close();
            return;
        }
        if outgoing.is_closed() || incoming.is_closed() {
            break;
        }
        reason = Some(failure);
        let delay = backoff.failed(stable_ms, jitter_sample());
        // Publish loss before sleeping, so the UI never claims this socket is alive.
        let _ = state
            .send(ConnectionState::Reconnecting {
                attempt: backoff.attempt(),
                reason,
            })
            .await;
        if let Some(Wake::Origin(origin)) = buffer_during_backoff(
            &outgoing,
            &mut buffered,
            &mut subscriptions,
            Duration::from_millis(delay),
        )
        .await
        {
            host.origin = origin;
        }
    }
    let _ = state
        .send(ConnectionState::Offline {
            reason: ConnectionFailure::HostClosed,
        })
        .await;
    incoming.close();
}

trait SocketStream: futures_lite::io::AsyncRead + futures_lite::io::AsyncWrite + Unpin + Send {}
impl<T: futures_lite::io::AsyncRead + futures_lite::io::AsyncWrite + Unpin + Send> SocketStream
    for T
{
}
type WebSocket = WebSocketStream<Box<dyn SocketStream>>;

fn connection_failure(error: String) -> ConnectionFailure {
    log::debug!("remote connection failed: {error}");
    ConnectionFailure::Unreachable
}

async fn race_addresses(
    host: &PairedHost,
    device_name: &str,
) -> Result<WebSocket, ConnectionFailure> {
    let url = url::Url::parse(&host.origin).map_err(|e| connection_failure(e.to_string()))?;
    let name = url
        .host_str()
        .ok_or(ConnectionFailure::Unreachable)?
        .trim_matches(['[', ']'])
        .to_owned();
    let port = url
        .port_or_known_default()
        .ok_or(ConnectionFailure::Unreachable)?;
    let addresses = futures_lite::future::race(
        smol::unblock(move || {
            (name.as_str(), port)
                .to_socket_addrs()
                .map(|a| a.collect::<Vec<_>>())
                .map_err(|e| connection_failure(e.to_string()))
        }),
        async {
            smol::Timer::after(Duration::from_secs(5)).await;
            Err(ConnectionFailure::Timeout)
        },
    )
    .await?;
    let mut attempts = futures_util::stream::FuturesUnordered::new();
    for (index, address) in addresses.into_iter().enumerate() {
        let url = &url;
        attempts.push(async move {
            smol::Timer::after(Duration::from_millis(250 * index as u64)).await;
            futures_lite::future::race(open_websocket(address, url, host, device_name), async {
                smol::Timer::after(Duration::from_secs(15)).await;
                Err(ConnectionFailure::Timeout)
            })
            .await
        });
    }
    let mut failure = ConnectionFailure::Unreachable;
    while let Some(result) = attempts.next().await {
        match result {
            Ok(socket) => return Ok(socket),
            Err(error) if error.is_terminal() => return Err(error),
            Err(error) => failure = error,
        }
    }
    Err(failure)
}

async fn send_interruptible(
    socket: &mut WebSocket,
    message: Message,
    deadline: Instant,
    outgoing: &OutgoingReceiver,
    interrupted: &mut Option<Wake>,
) -> Result<(), ConnectionFailure> {
    futures_lite::future::race(send_before(socket, message, deadline), async {
        *interrupted = outgoing.wake.recv().await.ok();
        Err(ConnectionFailure::Unreachable)
    })
    .await
}

async fn send_before(
    socket: &mut WebSocket,
    message: Message,
    deadline: Instant,
) -> Result<(), ConnectionFailure> {
    let deadline = deadline.min(Instant::now() + Duration::from_secs(10));
    futures_lite::future::race(
        async {
            socket
                .send(message)
                .await
                .map_err(|_| ConnectionFailure::Unreachable)
        },
        async {
            smol::Timer::at(deadline).await;
            Err(ConnectionFailure::Timeout)
        },
    )
    .await
}

async fn open_websocket(
    address: SocketAddr,
    origin: &url::Url,
    host: &PairedHost,
    device_name: &str,
) -> Result<WebSocket, ConnectionFailure> {
    let tcp = Async::<TcpStream>::connect(address)
        .await
        .map_err(|e| connection_failure(e.to_string()))?;
    let stream: Box<dyn SocketStream> = if origin.scheme() == "https" {
        let name = origin
            .host_str()
            .ok_or(ConnectionFailure::Unreachable)?
            .trim_matches(['[', ']'])
            .to_owned();
        Box::new(
            futures_rustls::TlsConnector::from(tls_client_config().map_err(connection_failure)?)
                .connect(
                    ServerName::try_from(name).map_err(|e| connection_failure(e.to_string()))?,
                    tcp,
                )
                .await
                .map_err(|e| connection_failure(e.to_string()))?,
        )
    } else if origin.scheme() == "http" {
        Box::new(tcp)
    } else {
        return Err(ConnectionFailure::Unreachable);
    };
    let mut url = origin.clone();
    url.set_scheme(if origin.scheme() == "https" {
        "wss"
    } else {
        "ws"
    })
    .map_err(|_| ConnectionFailure::Unreachable)?;
    url.set_path("/ws");
    let (mut websocket, _) = async_tungstenite::client_async(url.as_str(), stream)
        .await
        .map_err(|e| connection_failure(e.to_string()))?;
    let hello = serde_json::json!({
        "type": "hello",
        "protocol_version": tcode_protocol::PROTOCOL_VERSION,
        "token": host.token,
        "device_name": device_name,
    });
    websocket
        .send(Message::Text(hello.to_string().into()))
        .await
        .map_err(|error| connection_failure(error.to_string()))?;
    match websocket.next().await {
        Some(Ok(Message::Text(text))) => {
            let value: serde_json::Value = serde_json::from_str(&text)
                .map_err(|error| connection_failure(error.to_string()))?;
            if value.get("type").and_then(serde_json::Value::as_str) == Some("hello_ok")
                && value
                    .get("protocol_version")
                    .and_then(serde_json::Value::as_u64)
                    == Some(u64::from(tcode_protocol::PROTOCOL_VERSION))
            {
                Ok(websocket)
            } else {
                Err(if value["type"].as_str() == Some("hello_rejected") {
                    ConnectionFailure::hello_rejected(value["reason"].as_str())
                } else if value["type"].as_str() == Some("hello_ok") {
                    ConnectionFailure::ProtocolMismatch
                } else {
                    ConnectionFailure::Unreachable
                })
            }
        }
        Some(Ok(Message::Close(_))) => Err(ConnectionFailure::HostClosed),
        Some(Ok(_)) => Err(ConnectionFailure::Unreachable),
        Some(Err(error)) => Err(connection_failure(error.to_string())),
        None => Err(ConnectionFailure::HostClosed),
    }
}

async fn relay_connected(
    websocket: &mut WebSocket,
    outgoing: &OutgoingReceiver,
    incoming: &Sender<String>,
    subscriptions: &mut HashMap<String, String>,
    buffered: &mut VecDeque<String>,
    state: &Sender<ConnectionState>,
) -> (ConnectionFailure, bool, Option<Wake>) {
    let mut healthy = false;
    let mut interrupted = None;
    let mut connected = false;
    let mut deadline = Instant::now() + Duration::from_secs(10);
    let mut probing = false;
    let mut foreground_probe = false;
    loop {
        enum Input {
            Outgoing(Result<String, async_channel::RecvError>),
            WebSocket(Option<Result<Message, tungstenite::Error>>),
            Timer,
            Wake(Result<Wake, async_channel::RecvError>),
        }
        let input = {
            let wake = outgoing.wake.recv().fuse();
            let outbound = outgoing.recv().fuse();
            let websocket_input = websocket.next().fuse();
            let timer = futures_util::FutureExt::fuse(smol::Timer::at(deadline));
            futures_util::pin_mut!(outbound, websocket_input, timer, wake);
            futures_util::select! {
                line = outbound => Input::Outgoing(line),
                message = websocket_input => Input::WebSocket(message),
                _ = timer => Input::Timer,
                wake = wake => Input::Wake(wake),
            }
        };
        if matches!(&input, Input::WebSocket(Some(Ok(message))) if !matches!(message, Message::Close(_)))
        {
            healthy = true;
            probing = false;
            foreground_probe = false;
            deadline = Instant::now() + Duration::from_secs(10);
        }
        let failure = match input {
            Input::Wake(Ok(Wake::Probe)) => {
                foreground_probe = true;
                probing = true;
                deadline = Instant::now() + Duration::from_secs(3);
                send_interruptible(
                    websocket,
                    Message::Ping(Vec::new().into()),
                    deadline,
                    outgoing,
                    &mut interrupted,
                )
                .await
                .err()
            }
            Input::Wake(Ok(wake)) => return (ConnectionFailure::Unreachable, healthy, Some(wake)),
            Input::Wake(Err(_)) => Some(ConnectionFailure::HostClosed),
            Input::Timer if probing => Some(ConnectionFailure::Timeout),
            Input::Timer => {
                probing = true;
                // Keep the silence window absolute so timer scheduling does not
                // accumulate beyond the 10 + 20 second liveness budget.
                deadline += Duration::from_secs(20);
                send_interruptible(
                    websocket,
                    Message::Ping(Vec::new().into()),
                    deadline,
                    outgoing,
                    &mut interrupted,
                )
                .await
                .err()
            }
            Input::Outgoing(Ok(line)) => {
                remember_subscription(&line, subscriptions);
                let send_deadline = if probing {
                    deadline
                } else {
                    Instant::now() + Duration::from_secs(10)
                };
                let failure = send_interruptible(
                    websocket,
                    Message::Text(line.trim_end().to_owned().into()),
                    send_deadline,
                    outgoing,
                    &mut interrupted,
                )
                .await
                .err();
                if failure.is_some() {
                    if subscription_key(&line).is_none() {
                        buffered.push_back(line);
                    }
                } else {
                    outgoing.sent(&line);
                }
                failure
            }
            Input::Outgoing(Err(_)) => Some(ConnectionFailure::HostClosed),
            Input::WebSocket(Some(Ok(Message::Text(line)))) => {
                if !connected {
                    connected = true;
                    let _ = state.send(ConnectionState::Connected).await;
                }
                incoming
                    .send(format!("{}\n", line.trim_end()))
                    .await
                    .err()
                    .map(|_| ConnectionFailure::HostClosed)
            }
            Input::WebSocket(Some(Ok(Message::Ping(payload)))) => send_interruptible(
                websocket,
                Message::Pong(payload),
                deadline,
                outgoing,
                &mut interrupted,
            )
            .await
            .err(),
            Input::WebSocket(Some(Ok(Message::Close(_))) | None) => {
                Some(ConnectionFailure::HostClosed)
            }
            Input::WebSocket(Some(Err(_))) => Some(ConnectionFailure::Unreachable),
            Input::WebSocket(Some(Ok(_))) => None,
        };
        if let Some(failure) = failure {
            return (
                failure,
                healthy,
                interrupted.or_else(|| foreground_probe.then_some(Wake::Reconnect)),
            );
        }
    }
}

async fn buffer_during_backoff(
    outgoing: &OutgoingReceiver,
    buffered: &mut VecDeque<String>,
    subscriptions: &mut HashMap<String, String>,
    duration: Duration,
) -> Option<Wake> {
    let deadline = futures_util::FutureExt::fuse(smol::Timer::after(duration));
    futures_util::pin_mut!(deadline);
    loop {
        enum Input {
            Line(Result<String, async_channel::RecvError>),
            Done,
            Wake(Result<Wake, async_channel::RecvError>),
        }
        let input = {
            let wake = outgoing.wake.recv().fuse();
            let line = outgoing.recv().fuse();
            futures_util::pin_mut!(line, wake);
            futures_util::select! {
                line = line => Input::Line(line),
                _ = deadline => Input::Done,
                wake = wake => Input::Wake(wake),
            }
        };
        match input {
            Input::Line(Ok(line)) => {
                remember_subscription(&line, subscriptions);
                if subscription_key(&line).is_none() {
                    buffered.push_back(line);
                }
            }
            Input::Wake(wake) => return wake.ok(),
            Input::Line(Err(_)) | Input::Done => return None,
        }
    }
}

fn remember_subscription(line: &str, subscriptions: &mut HashMap<String, String>) {
    if let Some(key) = subscription_key(line) {
        subscriptions.insert(key, line.to_owned());
    }
}

fn authority(address: &str, port: u16) -> String {
    if address.contains(':') && !address.starts_with('[') {
        format!("[{address}]:{port}")
    } else {
        format!("{address}:{port}")
    }
}

fn socket_addr(address: &str, port: u16) -> Result<SocketAddr, String> {
    if address.len() > 253 || address.contains(['\r', '\n', '/', ' ']) {
        return Err("invalid host address".into());
    }
    (address.trim_matches(['[', ']']), port)
        .to_socket_addrs()
        .map_err(|e| format!("DNS: {e}"))?
        .next()
        .ok_or_else(|| "DNS returned no addresses".into())
}

fn tls_client_config() -> Result<Arc<rustls::ClientConfig>, String> {
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| e.to_string())?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(config))
}

fn jitter_sample() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    // Timing entropy decorrelates clients without adding a cryptographic RNG.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    f64::from(nanos % 1_000_001) / 1_000_000.
}
