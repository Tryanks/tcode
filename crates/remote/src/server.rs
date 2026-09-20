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
use tungstenite::Message;
use tungstenite::protocol::{Role, WebSocketConfig};

use crate::auth::{AuthStore, DeviceDetails};
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
    /// Password login for the served browser; native clients still use codes.
    pub browser_password: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingCode {
    pub code: String,
    #[serde(default)]
    pub browser_url: String,
    pub expires_in_secs: u64,
    pub host_id: String,
    pub identity_key: String,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
}

/// The device fields a client sends with `/pair`, `/auth/login` and hello.
/// `device_id` and `platform` are optional so older clients keep working.
#[derive(Deserialize)]
struct DeviceClaim {
    #[serde(default)]
    device_name: String,
    #[serde(default)]
    device_id: Option<String>,
    #[serde(default)]
    platform: Option<String>,
}

impl DeviceClaim {
    fn name_is_valid(&self) -> bool {
        !self.device_name.trim().is_empty() && self.device_name.len() <= 256
    }

    fn optional_fields_are_valid(&self) -> bool {
        self.device_id
            .as_deref()
            .is_none_or(tcode_client::host::valid_device_id)
            && self.platform.as_deref().is_none_or(|platform| {
                platform.len() <= 256 && !platform.chars().any(char::is_control)
            })
    }

    fn details(&self) -> DeviceDetails {
        DeviceDetails {
            name: self.device_name.clone(),
            platform: self
                .platform
                .as_deref()
                .map(str::trim)
                .filter(|platform| !platform.is_empty())
                .map(str::to_owned),
        }
    }
}

struct ActiveCode {
    code: String,
    expires: Instant,
    failures: u8,
}

pub(crate) struct Shared {
    mux: HostMux,
    pub(crate) auth: Mutex<AuthStore>,
    pairing: Mutex<Option<ActiveCode>>,
    browser_password: bool,
    static_bundle: Option<StaticBundle>,
    local_addr: SocketAddr,
    pub(crate) shutdown: async_channel::Receiver<()>,
    connections: std::sync::atomic::AtomicUsize,
}

pub struct RemoteServer {
    local_addr: SocketAddr,
    shared: Arc<Shared>,
    shutdown: async_channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

/// Ingress provenance is established by the listener, never by request headers.
#[derive(Clone, Copy, Debug)]
enum Ingress {
    Direct(SocketAddr),
    Imported,
}

impl Ingress {
    fn local_admin(self) -> bool {
        matches!(self, Self::Direct(peer) if peer.ip().is_loopback())
    }
}

impl RemoteServer {
    /// Admit a remotely supplied duplex stream into the existing host dispatch.
    /// Dropping this future cancels the connection. Imported streams never gain
    /// local administration rights, including streams forwarded over loopback.
    pub fn admit<S>(
        &self,
        stream: S,
    ) -> impl std::future::Future<Output = io::Result<()>> + Send + use<S>
    where
        S: futures_lite::io::AsyncRead + futures_lite::io::AsyncWrite + Unpin + Send,
    {
        admit(stream, Ingress::Imported, self.shared.clone())
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn pairing_enabled(&self) -> bool {
        !self.shared.browser_password || self.shared.auth.lock().unwrap().pairing_enabled
    }

    pub fn password_configured(&self) -> bool {
        self.shared.auth.lock().unwrap().password_configured()
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
                platform: device.platform.clone(),
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
    let listener = TcpListener::bind(config.listen)?;
    let local_addr = listener.local_addr()?;
    let (shutdown, shutdown_rx) = async_channel::bounded::<()>(1);
    let shared = Arc::new(Shared {
        mux,
        auth: Mutex::new(auth),
        pairing: Mutex::new(None),
        static_bundle: config.static_bundle,
        browser_password: config.browser_password,
        local_addr,
        shutdown: shutdown_rx.clone(),
        connections: std::sync::atomic::AtomicUsize::new(0),
    });
    let thread_shared = shared.clone();
    let thread = std::thread::Builder::new()
        .name("tcode-remote-server".into())
        .spawn(move || {
            smol::block_on(async move {
                let listener = match smol::net::TcpListener::try_from(listener) {
                    Ok(listener) => listener,
                    Err(error) => {
                        log::error!("remote listener initialization failed: {error}");
                        return;
                    }
                };
                loop {
                    enum Next {
                        Accepted(io::Result<(smol::net::TcpStream, SocketAddr)>),
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
                                let result = admit(stream, Ingress::Direct(peer), shared).await;
                                if let Err(error) = result {
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

async fn admit<S>(stream: S, ingress: Ingress, shared: Arc<Shared>) -> io::Result<()>
where
    S: futures_lite::io::AsyncRead + futures_lite::io::AsyncWrite + Unpin + Send,
{
    use std::sync::atomic::Ordering;
    struct Permit(Arc<Shared>);
    impl Drop for Permit {
        fn drop(&mut self) {
            self.0.connections.fetch_sub(1, Ordering::Relaxed);
        }
    }
    let previous = shared.connections.fetch_add(1, Ordering::Relaxed);
    let _permit = Permit(shared.clone());
    if previous >= 256 || shared.shutdown.is_closed() {
        return Err(io::Error::other("server unavailable"));
    }
    handle_connection(stream, ingress, shared).await
}

async fn handle_connection<S>(stream: S, ingress: Ingress, shared: Arc<Shared>) -> io::Result<()>
where
    S: futures_lite::io::AsyncRead + futures_lite::io::AsyncWrite + Unpin + Send,
{
    let mut stream = stream;
    let mut request = match futures_lite::future::race(read_request(&mut stream), async {
        futures_lite::future::race(
            async {
                smol::Timer::after(Duration::from_secs(5)).await;
            },
            async {
                let _ = shared.shutdown.recv().await;
            },
        )
        .await;
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "HTTP request timed out",
        ))
    })
    .await
    {
        Ok(request) => request,
        Err(error) => {
            if shared.shutdown.is_closed() {
                return Ok(());
            }
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
    if request.method == "POST" && request.path == "/identity" {
        // Main connections and Preview verify this stream before upgrading
        // to WebSocket or sending a credential-bearing proxy request.
        request =
            match futures_lite::future::race(http_identity(&mut stream, request, &shared), async {
                futures_lite::future::race(
                    async {
                        smol::Timer::after(Duration::from_secs(5)).await;
                    },
                    async {
                        let _ = shared.shutdown.recv().await;
                    },
                )
                .await;
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "identity exchange timed out",
                ))
            })
            .await?
            {
                Some(request) => request,
                None => return Ok(()),
            };
    }
    if request.method == "GET" && request.path == "/ws" && is_websocket_upgrade(&request) {
        // The WS loop sends its existing close frame. Bound teardown as well
        // when an admitted stream is blocked writing to an unresponsive peer.
        return futures_lite::future::race(websocket(stream, request, shared.clone()), async {
            let _ = shared.shutdown.recv().await;
            smol::Timer::after(Duration::from_secs(1)).await;
            Ok(())
        })
        .await;
    }
    futures_lite::future::race(dispatch(stream, request, ingress, &shared), async {
        let _ = shared.shutdown.recv().await;
        Ok(())
    })
    .await
}

async fn http_identity<S>(
    stream: &mut S,
    request: Request,
    shared: &Shared,
) -> io::Result<Option<Request>>
where
    S: futures_lite::io::AsyncRead + futures_lite::io::AsyncWrite + Unpin,
{
    let challenge = (request.body.len() <= crate::identity::MAX_IDENTITY_MESSAGE_BYTES)
        .then(|| serde_json::from_slice::<crate::identity::IdentityChallenge>(&request.body).ok())
        .flatten();
    let proof = challenge.and_then(|challenge| shared.auth.lock().unwrap().identify(&challenge));
    let Some(proof) = proof else {
        json_response(
            stream,
            "403 Forbidden",
            &serde_json::json!({"error": "invalid host identity challenge"}),
        )
        .await?;
        return Ok(None);
    };
    let body = serde_json::to_vec(&proof).map_err(io::Error::other)?;
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&body).await?;
    stream.flush().await?;
    // Exactly one follow-up request. Another /identity enters normal dispatch
    // and is rejected, rather than resetting the unauthenticated deadline.
    read_request(stream).await.map(Some)
}

async fn dispatch<S>(
    mut stream: S,
    request: Request,
    ingress: Ingress,
    shared: &Arc<Shared>,
) -> io::Result<()>
where
    S: futures_lite::io::AsyncRead + futures_lite::io::AsyncWrite + Unpin + Send,
{
    if request.method == "CONNECT" || request.path.starts_with("http://") {
        return crate::proxy::handle(stream, request, shared).await;
    }
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/auth/state") => {
            let state = if shared.browser_password {
                serde_json::json!({"mode": "password", "configured": shared.auth.lock().unwrap().password_configured()})
            } else {
                serde_json::json!({"mode": "code"})
            };
            json_response(&mut stream, "200 OK", &state).await
        }
        ("POST", "/auth/setup" | "/auth/login") if shared.browser_password => {
            password_auth(&mut stream, request, shared.clone()).await
        }
        ("POST", "/pair") => pair(&mut stream, request, shared).await,
        ("GET", "/admin/pair")
            if ingress.local_admin()
                && shared.browser_password
                && !shared.auth.lock().unwrap().pairing_enabled =>
        {
            json_response(
                &mut stream,
                "403 Forbidden",
                &serde_json::json!({"error":"pairing_disabled"}),
            )
            .await
        }
        ("GET", "/admin/pair") if ingress.local_admin() => {
            json_response(&mut stream, "200 OK", &mint_pairing_code(shared)).await
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
            serve_static(&mut stream, path, shared, request.method == "HEAD").await
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
    #[serde(flatten)]
    device: DeviceClaim,
}

#[derive(Serialize)]
struct PairResponse {
    host_id: String,
    identity_key: String,
    host_name: String,
    token: String,
}

async fn pair<S>(stream: &mut S, request: Request, shared: &Shared) -> io::Result<()>
where
    S: futures_lite::io::AsyncWrite + Unpin,
{
    match pairing_result(&request.body, shared)? {
        Ok(paired) => json_response(stream, "200 OK", &paired).await,
        Err(rejected) => {
            json_response(
                stream,
                rejected.status,
                &serde_json::json!({"error": rejected.reason}),
            )
            .await
        }
    }
}

struct PairingRejection {
    status: &'static str,
    reason: &'static str,
}

/// HTTP code entry and a QR-authenticated socket consume the same single-use
/// code and apply the same device validation and token rotation policy.
fn pairing_result(
    body: &[u8],
    shared: &Shared,
) -> io::Result<Result<PairResponse, PairingRejection>> {
    if shared.browser_password && !shared.auth.lock().unwrap().pairing_enabled {
        return Ok(Err(PairingRejection {
            status: "403 Forbidden",
            reason: "pairing_disabled",
        }));
    }
    let request: PairRequest = match serde_json::from_slice::<PairRequest>(body) {
        Ok(request)
            if request.device.name_is_valid()
                && request.device.optional_fields_are_valid()
                && request.code.len() <= 64 =>
        {
            request
        }
        _ => {
            return Ok(Err(PairingRejection {
                status: "400 Bad Request",
                reason: "malformed pairing request",
            }));
        }
    };
    if !consume_pairing_code(shared, &request.code) {
        return Ok(Err(PairingRejection {
            status: "403 Forbidden",
            reason: "invalid or expired pairing code",
        }));
    }
    let result = {
        let mut auth = shared.auth.lock().unwrap();
        let details = request.device.details();
        let token = auth.issue_token(request.device.device_id, details)?;
        PairResponse {
            host_id: auth.host_id.to_string(),
            identity_key: auth.identity_public_key(),
            host_name: auth.host_name.clone(),
            token,
        }
    };
    Ok(Ok(result))
}

// Password hashing runs off the executor; the auth lock serializes setup and login.
async fn password_auth<S>(stream: &mut S, request: Request, shared: Arc<Shared>) -> io::Result<()>
where
    S: futures_lite::io::AsyncWrite + Unpin,
{
    #[derive(Deserialize)]
    struct PasswordRequest {
        password: String,
        #[serde(flatten)]
        device: DeviceClaim,
    }
    // Requiring JSON prevents a cross-origin HTML form from claiming first setup.
    if !request.headers.get("content-type").is_some_and(|value| {
        value
            .split(';')
            .next()
            .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
    }) {
        return json_response(
            stream,
            "415 Unsupported Media Type",
            &serde_json::json!({"error":"application/json required"}),
        )
        .await;
    }
    let setup = request.path == "/auth/setup";
    let Ok(request) = serde_json::from_slice::<PasswordRequest>(&request.body) else {
        return json_response(
            stream,
            "400 Bad Request",
            &serde_json::json!({"error":"malformed request"}),
        )
        .await;
    };
    if request.password.len() > 1024
        || !request.device.optional_fields_are_valid()
        || (!setup && !request.device.name_is_valid())
    {
        return json_response(
            stream,
            "400 Bad Request",
            &serde_json::json!({"error":"malformed request"}),
        )
        .await;
    }
    // PBKDF2 takes a few hundred milliseconds; a dedicated thread keeps the
    // executor free without the runtime's unblock helper, which this crate
    // cannot depend on.
    let (done, wait) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = done.send_blocking((move || -> io::Result<_> {
        let mut auth = shared.auth.lock().unwrap();
        if setup {
            if auth.password_configured() {
                return Ok(("409 Conflict", serde_json::json!({"error":"already configured"})));
            }
            if request.password.chars().count() < 8 {
                return Ok(("400 Bad Request", serde_json::json!({"error":"password requires at least 8 characters"})));
            }
            auth.set_password(&request.password, false)?;
            Ok(("200 OK", serde_json::json!({"configured":true})))
        } else if auth.verify_password(&request.password) {
            let details = request.device.details();
            let token = auth.issue_token(request.device.device_id, details)?;
            Ok(("200 OK", serde_json::json!({"host_id":auth.host_id,"host_name":auth.host_name,"token":token,"identity_key":auth.identity_public_key()})))
        } else {
            Ok(("403 Forbidden", serde_json::json!({"error":"invalid password or temporarily locked"})))
        }
        })());
    });
    let (status, value) = wait
        .recv()
        .await
        .map_err(|_| io::Error::other("password worker exited"))??;
    json_response(stream, status, &value).await
}

/// Change a stopped headless host's password without changing its identity.
pub fn set_password(
    data_dir: &std::path::Path,
    password: &str,
    revoke_tokens: bool,
) -> io::Result<()> {
    let name = std::fs::read(data_dir.join("remote.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| value["host_name"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "Tcode".into());
    AuthStore::open(data_dir, &name)?.set_password(password, revoke_tokens)
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
    let addrs = crate::discovery::local_addrs();
    let browser_ip = if shared.local_addr.ip().is_unspecified() {
        addrs
            .iter()
            .filter_map(|addr| addr.parse::<std::net::IpAddr>().ok())
            .find(|ip| ip.is_ipv4() == shared.local_addr.is_ipv4())
            .unwrap_or_else(|| {
                if shared.local_addr.is_ipv6() {
                    std::net::Ipv6Addr::LOCALHOST.into()
                } else {
                    std::net::Ipv4Addr::LOCALHOST.into()
                }
            })
    } else {
        shared.local_addr.ip()
    };
    let browser_url = format!(
        "http://{}/#code={code}",
        SocketAddr::new(browser_ip, shared.local_addr.port())
    );
    let auth = shared.auth.lock().unwrap();
    PairingCode {
        code,
        browser_url,
        expires_in_secs: PAIRING_LIFETIME.as_secs(),
        host_id: auth.host_id.to_string(),
        identity_key: auth.identity_public_key(),
        host_name: auth.host_name.clone(),
        port: shared.local_addr.port(),
        addrs,
    }
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
    #[serde(default)]
    supported_versions: Vec<u32>,
    token: String,
    #[serde(flatten)]
    device: DeviceClaim,
}

enum Handshake {
    Hello(Hello),
    PairingComplete,
    Invalid,
}

async fn websocket<S>(mut stream: S, request: Request, shared: Arc<Shared>) -> io::Result<()>
where
    S: futures_lite::io::AsyncRead + futures_lite::io::AsyncWrite + Unpin + Send,
{
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
    // Apply the existing 64 KiB input contract while assembling frames too,
    // before an unauthenticated peer can consume tungstenite's 64 MiB default.
    let config = WebSocketConfig::default()
        .read_buffer_size(crate::identity::MAX_IDENTITY_MESSAGE_BYTES)
        .max_message_size(Some(crate::wire::MAX_BODY_BYTES))
        .max_frame_size(Some(crate::wire::MAX_BODY_BYTES));
    let mut websocket = WebSocketStream::from_raw_socket(stream, Role::Server, Some(config)).await;
    let hello = futures_lite::future::race(receive_handshake(&mut websocket, &shared), async {
        smol::Timer::after(Duration::from_secs(5)).await;
        Ok(Handshake::Invalid)
    })
    .await?;
    let hello = match hello {
        Handshake::Hello(hello) => Some(hello),
        Handshake::PairingComplete => return Ok(()),
        Handshake::Invalid => None,
    };
    if let Some(hello) = &hello
        && !matches!(hello.protocol_version, 3 | 4)
    {
        let rejected = serde_json::json!({
            "type": "hello_rejected",
            "reason": "protocol",
            "host_id": shared.auth.lock().unwrap().host_id,
            "expected": tcode_protocol::PROTOCOL_VERSION,
            "received": hello.protocol_version
        });
        let _ = websocket
            .send(Message::Text(rejected.to_string().into()))
            .await;
        let _ = websocket.close(None).await;
        return Ok(());
    }
    let version = hello.as_ref().map_or(3, |hello| {
        if hello.protocol_version == 4 || hello.supported_versions.contains(&4) {
            4
        } else {
            3
        }
    });
    let hello = hello.filter(|hello| {
        hello.kind == "hello"
            && matches!(hello.protocol_version, 3 | 4)
            && shared.auth.lock().unwrap().token_is_valid(&hello.token)
    });
    let Some(hello) = hello else {
        // Native recovery trusts this rejection only after an identity proof;
        // the public host id also remains available to older hello clients.
        let rejected = serde_json::json!({
            "type": "hello_rejected",
            "reason": "token",
            "host_id": shared.auth.lock().unwrap().host_id
        });
        let _ = websocket
            .send(Message::Text(rejected.to_string().into()))
            .await;
        let _ = websocket.close(None).await;
        return Ok(());
    };
    let token = hello.token;
    // The hosting list shows what the device calls itself now, not what it
    // said when it paired. The token already validated, so a failed write is
    // not a reason to refuse the connection.
    if hello.device.name_is_valid()
        && hello.device.optional_fields_are_valid()
        && let Err(error) = shared
            .auth
            .lock()
            .unwrap()
            .refresh_device(&token, hello.device.details())
    {
        log::warn!("could not record the connecting device's details: {error}");
    }
    // `addrs` and `port` let the client remember every address this machine
    // can be reached at, so it can find the machine again after a move.
    let hello_ok = {
        let auth = shared.auth.lock().unwrap();
        serde_json::json!({
            "type": "hello_ok",
            "host_id": auth.host_id,
            "identity_key": auth.identity_public_key(),
            "host_name": auth.host_name,
            "protocol_version": version,
            "addrs": crate::discovery::local_addrs(),
            "port": shared.local_addr.port()
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
        // Active streams may never reach the idle keepalive tick. Recheck
        // before forwarding in either direction, including hosting polls.
        if !shared.auth.lock().unwrap().token_is_valid(&token) {
            let _ = websocket.close(None).await;
            break;
        }
        match input {
            Input::WebSocket(Some(Ok(Message::Text(line)))) => {
                if line.len() > crate::wire::MAX_BODY_BYTES
                    || serde_json::from_str::<serde_json::Value>(&line).is_err()
                {
                    let _ = websocket.close(None).await;
                    break;
                }
                if let Ok(tcode_protocol::ClientMessage {
                    id,
                    key: _,
                    payload:
                        tcode_protocol::ClientPayload::Query(tcode_protocol::Query::Hosting { action }),
                }) = serde_json::from_str(&line)
                {
                    let result = if shared.browser_password {
                        hosting_action(&shared, action)
                            .map(tcode_protocol::QueryResponse::Hosting)
                            .map_err(|error| tcode_protocol::ProtocolError {
                                code: "hosting_error".into(),
                                message: error.to_string(),
                            })
                    } else {
                        Err(tcode_protocol::ProtocolError {
                            code: "unsupported".into(),
                            message: "manage desktop hosting on that machine".into(),
                        })
                    };
                    let reply = tcode_protocol::HostMessage::QueryResult { id, result };
                    websocket
                        .send(Message::Text(
                            serde_json::to_string(&reply)
                                .map_err(io::Error::other)?
                                .into(),
                        ))
                        .await
                        .map_err(io::Error::other)?;
                    continue;
                }
                // The authenticated token scopes keys across socket lifetimes.
                // Ignore client-supplied prefixes so devices cannot share a cache.
                let mut value: serde_json::Value = serde_json::from_str(&line).unwrap();
                if let Some(key) = value.get("key").and_then(serde_json::Value::as_str) {
                    if uuid::Uuid::parse_str(key).is_err() {
                        let _ = websocket.close(None).await;
                        break;
                    }
                    let device = crate::identity::encode_hex(&Sha1::digest(token.as_bytes()));
                    value["key"] = format!("{device}:{key}").into();
                }
                let mut line = value.to_string();
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

/// Optional identity proof followed by pairing or hello shares one deadline.
/// A second identify is rejected: this socket only signs one challenge.
async fn receive_handshake<S>(
    websocket: &mut WebSocketStream<S>,
    shared: &Shared,
) -> io::Result<Handshake>
where
    S: futures_lite::io::AsyncRead + futures_lite::io::AsyncWrite + Unpin,
{
    let Some(Ok(Message::Text(mut first))) = websocket.next().await else {
        return Ok(Handshake::Invalid);
    };
    if first.len() > crate::identity::MAX_IDENTITY_MESSAGE_BYTES {
        return Ok(Handshake::Invalid);
    }
    if let Ok(challenge) = serde_json::from_str::<crate::identity::IdentityChallenge>(&first) {
        let proof = shared.auth.lock().unwrap().identify(&challenge);
        let Some(proof) = proof else {
            return Ok(Handshake::Invalid);
        };
        websocket
            .send(Message::Text(
                serde_json::to_string(&proof)
                    .map_err(io::Error::other)?
                    .into(),
            ))
            .await
            .map_err(io::Error::other)?;
        let Some(Ok(Message::Text(next))) = websocket.next().await else {
            return Ok(Handshake::Invalid);
        };
        first = next;
        if first.len() > crate::identity::MAX_IDENTITY_MESSAGE_BYTES {
            return Ok(Handshake::Invalid);
        }
        if serde_json::from_str::<serde_json::Value>(&first)
            .ok()
            .is_some_and(|value| value["type"] == "pair")
        {
            let response = match pairing_result(first.as_bytes(), shared)? {
                Ok(paired) => {
                    let mut value = serde_json::to_value(paired).map_err(io::Error::other)?;
                    value["type"] = "pair_ok".into();
                    value
                }
                Err(rejected) => serde_json::json!({
                    "type": "pair_rejected", "error": rejected.reason,
                }),
            };
            websocket
                .send(Message::Text(response.to_string().into()))
                .await
                .map_err(io::Error::other)?;
            let _ = websocket.close(None).await;
            return Ok(Handshake::PairingComplete);
        }
    }
    if first.len() > crate::identity::MAX_IDENTITY_MESSAGE_BYTES {
        return Ok(Handshake::Invalid);
    }
    Ok(serde_json::from_str(&first).map_or(Handshake::Invalid, Handshake::Hello))
}

fn hosting_action(
    shared: &Shared,
    action: tcode_protocol::HostingAction,
) -> io::Result<tcode_protocol::HostingState> {
    use tcode_protocol::HostingAction;
    match action {
        HostingAction::State => {}
        HostingAction::SetEnabled(enabled) => {
            let mut auth = shared.auth.lock().unwrap();
            let mut updated = auth.clone();
            updated.pairing_enabled = enabled;
            updated.save()?;
            *auth = updated;
            drop(auth);
            if !enabled {
                *shared.pairing.lock().unwrap() = None;
            } else {
                mint_pairing_code(shared);
            }
        }
        HostingAction::NewCode => {
            if shared.auth.lock().unwrap().pairing_enabled {
                mint_pairing_code(shared);
            }
        }
        HostingAction::RevokeDevice(id) => {
            shared.auth.lock().unwrap().revoke(&id)?;
        }
    }
    let (code, expires_in_secs) = shared
        .pairing
        .lock()
        .unwrap()
        .as_ref()
        .filter(|code| code.expires > Instant::now())
        .map(|code| {
            (
                Some(code.code.clone()),
                code.expires
                    .saturating_duration_since(Instant::now())
                    .as_secs(),
            )
        })
        .unwrap_or((None, 0));
    let auth = shared.auth.lock().unwrap();
    Ok(tcode_protocol::HostingState {
        enabled: auth.pairing_enabled,
        code: if auth.pairing_enabled { code } else { None },
        expires_in_secs,
        host_id: auth.host_id.to_string(),
        host_name: auth.host_name.clone(),
        devices: auth
            .devices
            .iter()
            .map(|device| tcode_protocol::HostedDevice {
                id: device.id.to_string(),
                name: device.name.clone(),
                created_unix: device.created_unix,
                platform: device.platform.clone(),
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_lite::io::AsyncReadExt as _;
    use sha2::Sha256;

    struct Fixture {
        server: RemoteServer,
        root: PathBuf,
        _host_rx: async_channel::Receiver<String>,
        _host_tx: async_channel::Sender<String>,
    }

    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("tcode-identify-{}", uuid::Uuid::new_v4()));
            let (to_host, host_rx) = async_channel::unbounded();
            let (host_tx, from_host) = async_channel::unbounded();
            let server = serve(
                HostMux::new(to_host, from_host),
                RemoteConfig {
                    listen: "127.0.0.1:0".parse().unwrap(),
                    host_name: "Desk".into(),
                    data_dir: root.clone(),
                    static_bundle: None,
                    browser_password: false,
                },
            )
            .unwrap();
            Self {
                server,
                root,
                _host_rx: host_rx,
                _host_tx: host_tx,
            }
        }

        async fn socket(&self) -> WebSocketStream<smol::net::TcpStream> {
            let stream = smol::net::TcpStream::connect(self.server.local_addr())
                .await
                .unwrap();
            async_tungstenite::client_async(format!("ws://{}/ws", self.server.local_addr()), stream)
                .await
                .unwrap()
                .0
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.server.shutdown.close();
            if let Some(thread) = self.server.thread.take() {
                thread.join().unwrap();
            }
            std::fs::remove_dir_all(&self.root).unwrap();
        }
    }

    async fn reply(ws: &mut WebSocketStream<smol::net::TcpStream>) -> serde_json::Value {
        let message =
            futures_lite::future::race(async { ws.next().await.unwrap().unwrap() }, async {
                smol::Timer::after(Duration::from_secs(2)).await;
                panic!("server did not answer the handshake");
            })
            .await;
        serde_json::from_str(message.to_text().unwrap()).unwrap()
    }

    async fn http_reply(stream: &mut smol::net::TcpStream) -> (String, Vec<u8>) {
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).await.unwrap();
            head.extend_from_slice(&byte);
            assert!(head.len() < 4096);
        }
        let head = String::from_utf8(head).unwrap();
        let length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .unwrap()
            .parse()
            .unwrap();
        let mut body = vec![0; length];
        stream.read_exact(&mut body).await.unwrap();
        (head, body)
    }

    #[test]
    fn identity_proof_precedes_token_on_the_same_websocket() {
        let fixture = Fixture::new();
        let server = &fixture.server;
        let (host_id, token) = {
            let mut auth = server.shared.auth.lock().unwrap();
            let token = auth
                .issue_token(
                    None,
                    DeviceDetails {
                        name: "Phone".into(),
                        platform: None,
                    },
                )
                .unwrap();
            (auth.host_id.to_string(), token)
        };
        smol::block_on(async {
            let mut ws = fixture.socket().await;
            let nonce = "12".repeat(32);
            let challenge = serde_json::json!({
                "type": "identify",
                "host_id": host_id,
                "token_id": crate::auth::hex_hash(&Sha256::digest(token.as_bytes())),
                "nonce": nonce,
            });
            ws.send(Message::Text(challenge.to_string().into()))
                .await
                .unwrap();
            let response = reply(&mut ws).await;
            assert_eq!(response["type"], "identity");
            assert_eq!(response["host_id"], host_id);
            assert_eq!(response["nonce"], nonce);
            ws.send(Message::Text(
                serde_json::json!({
                    "type": "hello", "protocol_version": 4, "token": token,
                    "device_name": "Phone"
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
            let response = reply(&mut ws).await;
            assert_eq!(response["type"], "hello_ok");
        });
    }

    #[test]
    fn http_identity_keeps_one_verified_stream_for_the_request_and_never_repeats() {
        let fixture = Fixture::new();
        let invite = fixture.server.new_pairing_code();
        smol::block_on(futures_lite::future::race(
            async {
                for repeat_identity in [false, true] {
                    let mut stream = smol::net::TcpStream::connect(fixture.server.local_addr())
                        .await
                        .unwrap();
                    let challenge =
                        crate::identity::IdentityChallenge::for_pairing(&invite.host_id).unwrap();
                    let body = serde_json::to_string(&challenge).unwrap();
                    let identify = format!(
                        "POST /identity HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(identify.as_bytes()).await.unwrap();
                    let (head, body) = http_reply(&mut stream).await;
                    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"));
                    assert!(head.contains("Connection: keep-alive\r\n"));
                    let proof = serde_json::from_slice(&body).unwrap();
                    assert_eq!(
                        challenge.verify("", Some(&invite.identity_key), &proof),
                        Some(invite.identity_key.clone())
                    );
                    if repeat_identity {
                        stream.write_all(identify.as_bytes()).await.unwrap();
                        let (head, _) = http_reply(&mut stream).await;
                        assert!(head.starts_with("HTTP/1.1 404 Not Found\r\n"));
                    } else {
                        let body = serde_json::json!({"code": invite.code, "device_name": "Phone"})
                            .to_string();
                        stream.write_all(format!("POST /pair HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                        let (head, body) = http_reply(&mut stream).await;
                        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"));
                        let paired: serde_json::Value = serde_json::from_slice(&body).unwrap();
                        assert_eq!(paired["host_id"], invite.host_id);
                        assert!(
                            fixture
                                .server
                                .shared
                                .auth
                                .lock()
                                .unwrap()
                                .token_is_valid(paired["token"].as_str().unwrap())
                        );
                    }
                    let mut byte = [0];
                    assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
                }
            },
            async {
                smol::Timer::after(Duration::from_secs(3)).await;
                panic!("HTTP identity exchange stalled");
            },
        ));
    }

    #[test]
    fn qr_pairing_follows_host_proof_and_uses_the_existing_single_use_code() {
        let fixture = Fixture::new();
        let invite = fixture.server.new_pairing_code();
        let request = serde_json::json!({
            "type": "pair", "code": invite.code, "device_name": "Phone",
            "device_id": "phone-id", "platform": "Android 15"
        });
        smol::block_on(async {
            // Pair frames are only admitted after the optional identity exchange.
            let mut direct = fixture.socket().await;
            direct
                .send(Message::Text(request.to_string().into()))
                .await
                .unwrap();
            assert_eq!(reply(&mut direct).await["type"], "hello_rejected");
            assert!(fixture.server.devices().is_empty());

            for expected in ["pair_ok", "pair_rejected"] {
                let mut ws = fixture.socket().await;
                let challenge =
                    crate::identity::IdentityChallenge::for_pairing(&invite.host_id).unwrap();
                ws.send(Message::Text(
                    serde_json::to_string(&challenge).unwrap().into(),
                ))
                .await
                .unwrap();
                let proof = reply(&mut ws).await;
                assert_eq!(
                    challenge.verify("", Some(&invite.identity_key), &proof),
                    Some(invite.identity_key.clone())
                );
                ws.send(Message::Text(request.to_string().into()))
                    .await
                    .unwrap();
                let paired = reply(&mut ws).await;
                assert_eq!(paired["type"], expected);
                if expected == "pair_ok" {
                    assert_eq!(paired["host_id"], invite.host_id);
                    assert_eq!(paired["identity_key"], invite.identity_key);
                    assert!(
                        fixture
                            .server
                            .shared
                            .auth
                            .lock()
                            .unwrap()
                            .token_is_valid(paired["token"].as_str().unwrap())
                    );
                } else {
                    assert_eq!(paired["error"], "invalid or expired pairing code");
                }
                assert!(matches!(
                    ws.next().await,
                    Some(Ok(Message::Close(_))) | None
                ));
            }
        });
        assert_eq!(fixture.server.devices().len(), 1);
    }

    #[test]
    fn unauthenticated_handshake_rejects_malformed_identity_and_oversized_frames() {
        let fixture = Fixture::new();
        let invite = fixture.server.new_pairing_code();
        smol::block_on(async {
            let challenge =
                crate::identity::IdentityChallenge::for_pairing(&invite.host_id).unwrap();
            let challenge = serde_json::to_value(challenge).unwrap();
            for (field, bad) in [
                ("host_id", "another-host".to_owned()),
                ("nonce", "12".repeat(33)),
                ("token_id", "garbage".to_owned()),
                (
                    "padding",
                    "x".repeat(crate::identity::MAX_IDENTITY_MESSAGE_BYTES),
                ),
                ("padding", "x".repeat(crate::wire::MAX_BODY_BYTES)),
            ] {
                let mut ws = fixture.socket().await;
                let mut request = challenge.clone();
                request[field] = bad.into();
                ws.send(Message::Text(request.to_string().into()))
                    .await
                    .unwrap();
                let response = futures_lite::future::race(ws.next(), async {
                    smol::Timer::after(Duration::from_secs(2)).await;
                    panic!("server did not reject invalid handshake");
                })
                .await;
                if let Some(Ok(Message::Text(response))) = response {
                    let response: serde_json::Value = serde_json::from_str(&response).unwrap();
                    assert_eq!(response["type"], "hello_rejected");
                }
            }
        });
        assert!(fixture.server.devices().is_empty());
    }
}
