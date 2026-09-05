use std::io;
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use async_tungstenite::WebSocketStream;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use futures_lite::io::AsyncWriteExt as _;
use futures_util::{FutureExt as _, StreamExt as _};
use serde::{Deserialize, Serialize};
use sha1::{Digest as _, Sha1};
use smol::Async;
use tungstenite::Message;
use tungstenite::protocol::Role;

use crate::auth::AuthStore;
use crate::mux::HostMux;
use crate::wire::{Request, content_type, read_request, response, response_with_body_mode};

const PAIRING_LIFETIME: Duration = Duration::from_secs(5 * 60);
const MAX_PAIRING_FAILURES: u8 = 5;
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

pub type StaticBundle = &'static [(&'static str, &'static [u8])];

pub struct RemoteConfig {
    pub listen: SocketAddr,
    pub host_name: String,
    pub data_dir: PathBuf,
    pub static_bundle: Option<StaticBundle>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingCode {
    pub code: String,
    pub fp: String,
    pub expires_in_secs: u64,
    pub host_id: String,
    pub host_name: String,
    pub port: u16,
    pub addrs: Vec<String>,
}

/// One paired device, as shown by the hosting UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub created_unix: u64,
}

struct ActiveCode {
    code: String,
    expires: Instant,
    failures: u8,
}

struct Shared {
    fingerprint: String,
    tls: futures_rustls::TlsAcceptor,
    mux: HostMux,
    auth: Mutex<AuthStore>,
    pairing: Mutex<Option<ActiveCode>>,
    static_bundle: Option<StaticBundle>,
    local_addr: SocketAddr,
    shutdown: async_channel::Receiver<()>,
}

pub struct RemoteServer {
    local_addr: SocketAddr,
    shared: Arc<Shared>,
    shutdown: async_channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl RemoteServer {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn new_pairing_code(&self) -> PairingCode {
        mint_pairing_code(&self.shared)
    }

    /// Devices that hold a valid token, oldest pairing first.
    pub fn devices(&self) -> Vec<DeviceInfo> {
        self.shared
            .auth
            .lock()
            .unwrap()
            .devices
            .iter()
            .map(|device| DeviceInfo {
                id: device.id.to_string(),
                name: device.name.clone(),
                created_unix: device.created_unix,
            })
            .collect()
    }

    /// Revoke a device's token. A connection already using it survives at most
    /// one keepalive interval: the websocket loop rechecks the token on every
    /// ping and closes when it no longer validates.
    pub fn revoke_device(&self, id: &str) -> io::Result<bool> {
        self.shared.auth.lock().unwrap().revoke(id)
    }

    pub fn shutdown(mut self) {
        self.shutdown.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for RemoteServer {
    fn drop(&mut self) {
        self.shutdown.close();
    }
}

pub fn serve(mux: HostMux, config: RemoteConfig) -> io::Result<RemoteServer> {
    let auth = AuthStore::open(&config.data_dir, &config.host_name)?;
    let (tls, fingerprint) = auth.tls_config()?;
    let listener = TcpListener::bind(config.listen)?;
    listener.set_nonblocking(true)?;
    let local_addr = listener.local_addr()?;
    let (shutdown, shutdown_rx) = async_channel::bounded::<()>(1);
    let shared = Arc::new(Shared {
        fingerprint,
        tls: futures_rustls::TlsAcceptor::from(Arc::new(tls)),
        mux,
        auth: Mutex::new(auth),
        pairing: Mutex::new(None),
        static_bundle: config.static_bundle,
        local_addr,
        shutdown: shutdown_rx.clone(),
    });
    let thread_shared = shared.clone();
    let thread = std::thread::Builder::new()
        .name("tcode-remote-server".into())
        .spawn(move || {
            smol::block_on(async move {
                let listener = match Async::new(listener) {
                    Ok(listener) => listener,
                    Err(error) => {
                        log::error!("remote listener initialization failed: {error}");
                        return;
                    }
                };
                loop {
                    enum Next {
                        Accepted(io::Result<(Async<std::net::TcpStream>, SocketAddr)>),
                        Shutdown,
                    }
                    let next = futures_lite::future::race(
                        async { Next::Accepted(listener.accept().await) },
                        async {
                            let _ = shutdown_rx.recv().await;
                            Next::Shutdown
                        },
                    )
                    .await;
                    match next {
                        Next::Accepted(Ok((stream, peer))) => {
                            let shared = thread_shared.clone();
                            smol::spawn(async move {
                                if let Err(error) = handle_connection(stream, peer, shared).await {
                                    log::debug!("remote connection ended: {error}");
                                }
                            })
                            .detach();
                        }
                        Next::Accepted(Err(error)) => {
                            log::warn!("remote accept failed: {error}");
                        }
                        Next::Shutdown => break,
                    }
                }
            });
        })?;
    Ok(RemoteServer {
        local_addr,
        shared,
        shutdown,
        thread: Some(thread),
    })
}

async fn handle_connection(
    stream: Async<std::net::TcpStream>,
    peer: SocketAddr,
    shared: Arc<Shared>,
) -> io::Result<()> {
    let mut stream = futures_lite::future::race(shared.tls.accept(stream), async {
        smol::Timer::after(Duration::from_secs(5)).await;
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "TLS handshake timed out",
        ))
    })
    .await?;
    let request = match futures_lite::future::race(read_request(&mut stream), async {
        smol::Timer::after(Duration::from_secs(5)).await;
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "HTTP request timed out",
        ))
    })
    .await
    {
        Ok(request) => request,
        Err(error) => {
            let _ = response(
                &mut stream,
                "400 Bad Request",
                "text/plain; charset=utf-8",
                b"bad request",
            )
            .await;
            return Err(error);
        }
    };
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/ws") if is_websocket_upgrade(&request) => {
            websocket(stream, request, shared).await
        }
        ("POST", "/pair") => pair(&mut stream, request, &shared).await,
        ("GET", "/admin/pair") if peer.ip().is_loopback() => {
            json_response(&mut stream, "200 OK", &mint_pairing_code(&shared)).await
        }
        ("GET", "/admin/pair") => {
            response(
                &mut stream,
                "403 Forbidden",
                "application/json",
                br#"{"error":"loopback only"}"#,
            )
            .await
        }
        ("GET" | "HEAD", path) => {
            serve_static(&mut stream, path, &shared, request.method == "HEAD").await
        }
        _ => {
            response(
                &mut stream,
                "404 Not Found",
                "text/plain; charset=utf-8",
                b"not found",
            )
            .await
        }
    }
}

#[derive(Deserialize)]
struct PairRequest {
    code: String,
    device_name: String,
}

#[derive(Serialize)]
struct PairResponse {
    host_id: String,
    host_name: String,
    token: String,
    fp: String,
}

async fn pair<S>(stream: &mut S, request: Request, shared: &Shared) -> io::Result<()>
where
    S: futures_lite::io::AsyncWrite + Unpin,
{
    let request: PairRequest = match serde_json::from_slice::<PairRequest>(&request.body) {
        Ok(request)
            if !request.device_name.trim().is_empty()
                && request.device_name.len() <= 256
                && request.code.len() <= 64 =>
        {
            request
        }
        _ => {
            return response(
                stream,
                "400 Bad Request",
                "application/json",
                br#"{"error":"malformed pairing request"}"#,
            )
            .await;
        }
    };
    if !consume_pairing_code(shared, &request.code) {
        return response(
            stream,
            "403 Forbidden",
            "application/json",
            br#"{"error":"invalid or expired pairing code"}"#,
        )
        .await;
    }
    let result = {
        let mut auth = shared.auth.lock().unwrap();
        let token = auth.issue_token(request.device_name)?;
        PairResponse {
            fp: shared.fingerprint.clone(),
            host_id: auth.host_id.to_string(),
            host_name: auth.host_name.clone(),
            token,
        }
    };
    json_response(stream, "200 OK", &result).await
}

fn consume_pairing_code(shared: &Shared, candidate: &str) -> bool {
    let mut active = shared.pairing.lock().unwrap();
    let Some(code) = active.as_mut() else {
        return false;
    };
    if Instant::now() >= code.expires {
        *active = None;
        return false;
    }
    if crate::auth::constant_time_eq(code.code.as_bytes(), candidate.as_bytes()) {
        *active = None;
        return true;
    }
    code.failures += 1;
    if code.failures >= MAX_PAIRING_FAILURES {
        *active = None;
    }
    false
}

fn mint_pairing_code(shared: &Shared) -> PairingCode {
    let mut random = [0_u8; 4];
    if let Err(error) = getrandom::fill(&mut random) {
        log::error!("unable to generate pairing code: {error}");
    }
    let code = format!("{:06}", u32::from_le_bytes(random) % 1_000_000);
    *shared.pairing.lock().unwrap() = Some(ActiveCode {
        code: code.clone(),
        expires: Instant::now() + PAIRING_LIFETIME,
        failures: 0,
    });
    let auth = shared.auth.lock().unwrap();
    PairingCode {
        fp: shared.fingerprint.clone(),
        code,
        expires_in_secs: PAIRING_LIFETIME.as_secs(),
        host_id: auth.host_id.to_string(),
        host_name: auth.host_name.clone(),
        port: shared.local_addr.port(),
        addrs: local_addrs(),
    }
}

fn local_addrs() -> Vec<String> {
    let mut result: Vec<_> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .map(|interface| interface.ip())
        .filter(|address| {
            !address.is_loopback()
                && match address {
                    std::net::IpAddr::V4(v4) => !v4.is_link_local(),
                    std::net::IpAddr::V6(v6) => !v6.is_unicast_link_local(),
                }
        })
        .map(|address| address.to_string())
        .collect();
    result.sort();
    result.dedup();
    result
}

async fn json_response<S, T>(stream: &mut S, status: &str, value: &T) -> io::Result<()>
where
    S: futures_lite::io::AsyncWrite + Unpin,
    T: Serialize,
{
    let body = serde_json::to_vec(value).map_err(io::Error::other)?;
    response(stream, status, "application/json", &body).await
}

async fn serve_static<S>(
    stream: &mut S,
    request_path: &str,
    shared: &Shared,
    head_only: bool,
) -> io::Result<()>
where
    S: futures_lite::io::AsyncWrite + Unpin,
{
    let lookup = if request_path == "/" {
        "/index.html"
    } else {
        request_path
    };
    let found = shared.static_bundle.and_then(|bundle| {
        bundle
            .iter()
            .find(|(path, _)| *path == lookup)
            .map(|(_, bytes)| *bytes)
    });
    match found {
        Some(bytes) => {
            response_with_body_mode(stream, "200 OK", content_type(lookup), bytes, head_only).await
        }
        None => {
            response_with_body_mode(
                stream,
                "404 Not Found",
                "text/plain; charset=utf-8",
                b"not found",
                head_only,
            )
            .await
        }
    }
}

fn is_websocket_upgrade(request: &Request) -> bool {
    request
        .headers
        .get("upgrade")
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
        && request.headers.contains_key("sec-websocket-key")
}

#[derive(Deserialize)]
struct Hello {
    #[serde(rename = "type")]
    kind: String,
    protocol_version: u32,
    token: String,
}

async fn websocket(
    mut stream: futures_rustls::server::TlsStream<Async<std::net::TcpStream>>,
    request: Request,
    shared: Arc<Shared>,
) -> io::Result<()> {
    let key = request
        .headers
        .get("sec-websocket-key")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing websocket key"))?;
    let accept = STANDARD.encode(Sha1::digest(format!("{key}{WS_GUID}").as_bytes()));
    let handshake = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    stream.write_all(handshake.as_bytes()).await?;
    stream.flush().await?;
    let mut websocket = WebSocketStream::from_raw_socket(stream, Role::Server, None).await;
    let first = futures_lite::future::race(websocket.next(), async {
        smol::Timer::after(Duration::from_secs(5)).await;
        None
    })
    .await;
    let hello = match first {
        Some(Ok(Message::Text(text))) => serde_json::from_str::<Hello>(&text).ok(),
        _ => None,
    };
    if let Some(hello) = &hello
        && hello.protocol_version != tcode_protocol::PROTOCOL_VERSION
    {
        let rejected = serde_json::json!({
            "type": "hello_rejected",
            "reason": "protocol version mismatch",
            "expected": tcode_protocol::PROTOCOL_VERSION,
            "received": hello.protocol_version
        });
        let _ = websocket
            .send(Message::Text(rejected.to_string().into()))
            .await;
        let _ = websocket.close(None).await;
        return Ok(());
    }
    let token = hello.filter(|hello| {
        hello.kind == "hello"
            && hello.protocol_version == tcode_protocol::PROTOCOL_VERSION
            && shared.auth.lock().unwrap().token_is_valid(&hello.token)
    });
    let Some(token) = token.map(|hello| hello.token) else {
        let rejected = serde_json::json!({
            "type": "hello_rejected",
            "reason": "invalid hello or token"
        });
        let _ = websocket
            .send(Message::Text(rejected.to_string().into()))
            .await;
        let _ = websocket.close(None).await;
        return Ok(());
    };
    let hello_ok = {
        let auth = shared.auth.lock().unwrap();
        serde_json::json!({
            "type": "hello_ok",
            "host_id": auth.host_id,
            "host_name": auth.host_name,
            "protocol_version": tcode_protocol::PROTOCOL_VERSION
        })
    };
    websocket
        .send(Message::Text(hello_ok.to_string().into()))
        .await
        .map_err(io::Error::other)?;
    let connection = shared.mux.attach();
    let mut unanswered_pings = 0_u8;
    loop {
        enum Input {
            WebSocket(Option<Result<Message, tungstenite::Error>>),
            Host(Result<String, async_channel::RecvError>),
            Ping,
            Shutdown,
        }
        let input = {
            let websocket_input = websocket.next().fuse();
            let host_input = connection.from_host.recv().fuse();
            let timer = futures_util::FutureExt::fuse(smol::Timer::after(Duration::from_secs(10)));
            let shutdown = shared.shutdown.recv().fuse();
            futures_util::pin_mut!(websocket_input, host_input, timer, shutdown);
            futures_util::select! {
                message = websocket_input => Input::WebSocket(message),
                line = host_input => Input::Host(line),
                _ = timer => Input::Ping,
                _ = shutdown => Input::Shutdown,
            }
        };
        match input {
            Input::WebSocket(Some(Ok(Message::Text(line)))) => {
                if line.len() > crate::wire::MAX_BODY_BYTES
                    || serde_json::from_str::<serde_json::Value>(&line).is_err()
                {
                    let _ = websocket.close(None).await;
                    break;
                }
                let mut line = line.to_string();
                if !line.ends_with('\n') {
                    line.push('\n');
                }
                if connection.to_host.send(line).await.is_err() {
                    break;
                }
            }
            Input::WebSocket(Some(Ok(Message::Ping(payload)))) => {
                websocket
                    .send(Message::Pong(payload))
                    .await
                    .map_err(io::Error::other)?;
            }
            Input::WebSocket(Some(Ok(Message::Pong(_)))) => unanswered_pings = 0,
            Input::WebSocket(Some(Ok(Message::Close(_))) | Some(Err(_)) | None) => break,
            Input::WebSocket(Some(Ok(_))) => {}
            Input::Host(Ok(line)) => websocket
                .send(Message::Text(line.trim_end().to_owned().into()))
                .await
                .map_err(io::Error::other)?,
            Input::Host(Err(_)) => break,
            Input::Ping => {
                // Revoking a device only rewrites remote.json; this is what
                // actually evicts a connection that already presented the token.
                if !shared.auth.lock().unwrap().token_is_valid(&token) {
                    let _ = websocket.close(None).await;
                    break;
                }
                if unanswered_pings >= 2 {
                    let _ = websocket.close(None).await;
                    break;
                }
                unanswered_pings += 1;
                websocket
                    .send(Message::Ping(Vec::new().into()))
                    .await
                    .map_err(io::Error::other)?;
            }
            Input::Shutdown => {
                let _ = websocket.close(None).await;
                break;
            }
        }
    }
    connection.to_host.close();
    Ok(())
}
