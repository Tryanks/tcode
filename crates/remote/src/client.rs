use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use sha2::{Digest as _, Sha256};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use async_channel::{Receiver, Sender};
use async_tungstenite::WebSocketStream;
use futures_util::{FutureExt as _, StreamExt as _};
use serde::Deserialize;
use smol::Async;
use tungstenite::Message;

pub use tcode_client::ConnectionState;
pub use tcode_client::pairing::{
    PairInvite, PairedHost, is_pairing_code, pair_url, parse_pair_url,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfflineReason {
    CertificateChanged,
}

pub struct RemoteClient {
    pub to_host: Sender<String>,
    pub from_host: Receiver<String>,
    pub state: Receiver<ConnectionState>,
    pub reason: Arc<Mutex<Option<OfflineReason>>>,
}

#[derive(Deserialize)]
struct PairResponse {
    host_id: String,
    host_name: String,
    token: String,
    fp: String,
}

pub fn pair(addr: &str, port: u16, code: &str, device_name: &str) -> Result<PairedHost, String> {
    pair_pinned(addr, port, code, device_name, "")
}

pub fn pair_pinned(
    addr: &str,
    port: u16,
    code: &str,
    device_name: &str,
    fingerprint: &str,
) -> Result<PairedHost, String> {
    if !is_pairing_code(code) || device_name.is_empty() || device_name.len() > 256 {
        return Err("invalid pairing request".into());
    }
    let body = serde_json::json!({ "code": code, "device_name": device_name }).to_string();
    let (bytes, seen) = tls_http(addr, port, "POST", "/pair", &body, fingerprint)?;
    let response: PairResponse =
        serde_json::from_slice(&bytes).map_err(|_| "invalid pairing response")?;
    if !same_fingerprint(&seen, &response.fp) {
        return Err(CERT_CHANGED.into());
    }
    Ok(PairedHost {
        host_id: response.host_id,
        name: response.host_name,
        addrs: vec![addr.to_owned()],
        port,
        token: response.token,
        fingerprint: seen,
        last_connected_unix: None,
    })
}

/// Bounded HTTP/1.1 over TLS, also used by the loopback headless admin client.
/// Empty pin is allowed only for explicit pairing / legacy migration.
pub fn tls_http(
    addr: &str,
    port: u16,
    method: &str,
    path: &str,
    body: &str,
    pin: &str,
) -> Result<(Vec<u8>, String), String> {
    use std::io::{Read as _, Write as _};
    if !matches!(
        (method, path),
        ("POST", "/pair") | ("GET", "/admin/pair") | ("GET", "/") | ("HEAD", "/")
    ) || body.len() > 4096
    {
        return Err("invalid HTTP request".into());
    }
    let (config, seen) = tls_client_config(pin)?;
    let socket = TcpStream::connect_timeout(&socket_addr(addr, port)?, Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    socket
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let session =
        rustls::ClientConnection::new(config, ServerName::try_from("tcode.local").unwrap())
            .map_err(|e| e.to_string())?;
    let mut stream = rustls::StreamOwned::new(session, socket);
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        authority(addr, port),
        body.len()
    );
    stream.write_all(request.as_bytes()).map_err(tls_error)?;
    stream.flush().map_err(tls_error)?;
    let mut bytes = Vec::new();
    // The server closes its HTTP stream without TLS close_notify; accept that
    // only when the complete Content-Length-delimited response was received.
    let read = (&mut stream).take(65537).read_to_end(&mut bytes);
    if bytes.len() > 65536 {
        return Err("HTTP response too large".into());
    }
    let split = bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| {
            read.err()
                .map(tls_error)
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
    let fingerprint = seen.lock().unwrap().clone();
    Ok((body.to_vec(), fingerprint))
}

pub fn load_hosts(data_dir: &Path) -> io::Result<Vec<PairedHost>> {
    match fs::read(data_dir.join("hosts.json")) {
        Ok(bytes) => {
            let hosts: Vec<PairedHost> =
                serde_json::from_slice(&bytes).map_err(io::Error::other)?;
            for host in &hosts {
                HOST_PATHS
                    .lock()
                    .unwrap()
                    .insert(host.host_id.clone(), data_dir.to_owned());
            }
            Ok(hosts)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

pub fn save_hosts(data_dir: &Path, hosts: &[PairedHost]) -> io::Result<()> {
    fs::create_dir_all(data_dir)?;
    // A UI may still hold its pre-migration record while updating last-used.
    // Never erase a persisted TOFU pin when saving such a legacy record.
    let existing = load_hosts(data_dir)?;
    let mut merged = hosts.to_vec();
    for host in &mut merged {
        if host.fingerprint.is_empty()
            && let Some(old) = existing.iter().find(|old| old.host_id == host.host_id)
        {
            host.fingerprint = old.fingerprint.clone();
        }
    }
    let bytes = serde_json::to_vec_pretty(&merged).map_err(io::Error::other)?;
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
    for host in hosts {
        HOST_PATHS
            .lock()
            .unwrap()
            .insert(host.host_id.clone(), data_dir.to_owned());
    }
    fs::rename(temporary, data_dir.join("hosts.json"))
}

pub fn connect(host: PairedHost, device_name: String) -> RemoteClient {
    let (to_host, outgoing) = async_channel::unbounded();
    let (incoming, from_host) = async_channel::unbounded();
    let (state_tx, state) = async_channel::unbounded();
    let reason = Arc::new(Mutex::new(None));
    let thread_reason = reason.clone();
    std::thread::Builder::new()
        .name("tcode-remote-client".into())
        .spawn(move || {
            smol::block_on(connection_loop(
                host,
                device_name,
                outgoing,
                incoming,
                state_tx,
                thread_reason,
            ));
        })
        .expect("failed to spawn remote client thread");
    RemoteClient {
        to_host,
        from_host,
        state,
        reason,
    }
}

async fn connection_loop(
    mut host: PairedHost,
    device_name: String,
    outgoing: Receiver<String>,
    incoming: Sender<String>,
    state: Sender<ConnectionState>,
    reason: Arc<Mutex<Option<OfflineReason>>>,
) {
    CERT_ERRORS.lock().unwrap().remove(&host.host_id);
    let mut buffered = VecDeque::<String>::new();
    let mut subscriptions = HashMap::<String, String>::new();
    let mut attempt = 0_u32;
    while !outgoing.is_closed() && !incoming.is_closed() {
        attempt = attempt.saturating_add(1);
        let _ = state.send(ConnectionState::Reconnecting { attempt }).await;
        let mut connected = None;
        for address in host.addrs.clone() {
            match open_websocket(&address, &mut host, &device_name).await {
                Ok(websocket) => {
                    connected = Some(websocket);
                    break;
                }
                Err(error) if error.contains(CERT_CHANGED) => {
                    *reason.lock().unwrap() = Some(OfflineReason::CertificateChanged);
                    CERT_ERRORS
                        .lock()
                        .unwrap()
                        .insert(host.host_id.clone(), true);
                    let _ = state.send(ConnectionState::Offline).await;
                    incoming.close();
                    return;
                }
                Err(_) => log::debug!("remote connection attempt failed"),
            }
        }
        if let Some(mut websocket) = connected {
            let mut ready = true;
            for line in subscriptions.values() {
                if websocket
                    .send(Message::Text(line.trim_end().to_owned().into()))
                    .await
                    .is_err()
                {
                    ready = false;
                    break;
                }
            }
            if ready {
                while let Some(line) = buffered.pop_front() {
                    if subscription_key(&line).is_some() {
                        continue;
                    }
                    if websocket
                        .send(Message::Text(line.trim_end().to_owned().into()))
                        .await
                        .is_err()
                    {
                        buffered.push_front(line);
                        ready = false;
                        break;
                    }
                }
            }
            if ready {
                attempt = 0;
                let _ = state.send(ConnectionState::Connected).await;
                relay_connected(
                    &mut websocket,
                    &outgoing,
                    &incoming,
                    &mut subscriptions,
                    &mut buffered,
                )
                .await;
            }
        }
        if outgoing.is_closed() || incoming.is_closed() {
            break;
        }
        let backoff_attempt = attempt.clamp(1, 6);
        let seconds = (1_u64 << (backoff_attempt - 1)).min(30);
        buffer_during_backoff(
            &outgoing,
            &mut buffered,
            &mut subscriptions,
            Duration::from_secs(seconds),
        )
        .await;
    }
    let _ = state.send(ConnectionState::Offline).await;
    incoming.close();
}

async fn open_websocket(
    address: &str,
    host: &mut PairedHost,
    device_name: &str,
) -> Result<WebSocketStream<futures_rustls::client::TlsStream<Async<TcpStream>>>, String> {
    // A foreground reconnect can still carry the UI's original legacy record.
    // Consult the durable pin before allowing TOFU again.
    if host.fingerprint.is_empty() {
        let path = HOST_PATHS.lock().unwrap().get(&host.host_id).cloned();
        if let Some(path) = path
            && let Some(saved) = load_hosts(&path)
                .map_err(|e| e.to_string())?
                .into_iter()
                .find(|saved| saved.host_id == host.host_id)
        {
            host.fingerprint = saved.fingerprint;
        }
    }
    let socket = socket_addr(address, host.port)?;
    let (config, seen) = tls_client_config(&host.fingerprint)?;
    let stream = futures_lite::future::race(
        async {
            let tcp = Async::<TcpStream>::connect(socket)
                .await
                .map_err(tls_error)?;
            futures_rustls::TlsConnector::from(config)
                .connect(ServerName::try_from("tcode.local").unwrap(), tcp)
                .await
                .map_err(tls_error)
        },
        async {
            smol::Timer::after(Duration::from_secs(5)).await;
            Err("connection timed out".into())
        },
    )
    .await?;
    if host.fingerprint.is_empty() {
        persist_tofu_pin(host, &seen.lock().unwrap())?;
    }
    let url = format!("wss://{}/ws", authority(address, host.port));
    let (mut websocket, _) = async_tungstenite::client_async(url, stream)
        .await
        .map_err(|e| e.to_string())?;
    let hello = serde_json::json!({
        "type": "hello",
        "protocol_version": tcode_protocol::PROTOCOL_VERSION,
        "token": host.token,
        "device_name": device_name,
    });
    websocket
        .send(Message::Text(hello.to_string().into()))
        .await
        .map_err(|error| error.to_string())?;
    match websocket.next().await {
        Some(Ok(Message::Text(text))) => {
            let value: serde_json::Value =
                serde_json::from_str(&text).map_err(|error| error.to_string())?;
            if value.get("type").and_then(serde_json::Value::as_str) == Some("hello_ok")
                && value
                    .get("protocol_version")
                    .and_then(serde_json::Value::as_u64)
                    == Some(u64::from(tcode_protocol::PROTOCOL_VERSION))
            {
                Ok(websocket)
            } else {
                Err("host rejected remote hello".into())
            }
        }
        Some(Ok(_)) => Err("host sent a non-text hello response".into()),
        Some(Err(error)) => Err(error.to_string()),
        None => Err("host closed during hello".into()),
    }
}

fn persist_tofu_pin(host: &mut PairedHost, observed: &str) -> Result<(), String> {
    // Two initial connections must not overwrite each other's first pin.
    let _migration = PIN_MIGRATION.lock().unwrap();
    let path = HOST_PATHS.lock().unwrap().get(&host.host_id).cloned();
    if let Some(path) = path {
        let mut hosts = load_hosts(&path).map_err(|e| e.to_string())?;
        if let Some(saved) = hosts.iter_mut().find(|saved| saved.host_id == host.host_id) {
            if !saved.fingerprint.is_empty() && !same_fingerprint(&saved.fingerprint, observed) {
                return Err(CERT_CHANGED.into());
            }
            saved.fingerprint = observed.to_owned();
        }
        save_hosts(&path, &hosts).map_err(|e| e.to_string())?;
    }
    log::warn!("legacy paired host has no certificate pin; trusting first TLS connection");
    host.fingerprint = observed.to_owned();
    Ok(())
}

async fn relay_connected(
    websocket: &mut WebSocketStream<futures_rustls::client::TlsStream<Async<TcpStream>>>,
    outgoing: &Receiver<String>,
    incoming: &Sender<String>,
    subscriptions: &mut HashMap<String, String>,
    buffered: &mut VecDeque<String>,
) {
    loop {
        enum Input {
            Outgoing(Result<String, async_channel::RecvError>),
            WebSocket(Option<Result<Message, tungstenite::Error>>),
        }
        let input = {
            let outbound = outgoing.recv().fuse();
            let websocket_input = websocket.next().fuse();
            futures_util::pin_mut!(outbound, websocket_input);
            futures_util::select! {
                line = outbound => Input::Outgoing(line),
                message = websocket_input => Input::WebSocket(message),
            }
        };
        match input {
            Input::Outgoing(Ok(line)) => {
                remember_subscription(&line, subscriptions);
                if websocket
                    .send(Message::Text(line.trim_end().to_owned().into()))
                    .await
                    .is_err()
                {
                    buffered.push_back(line);
                    return;
                }
            }
            Input::Outgoing(Err(_)) => return,
            Input::WebSocket(Some(Ok(Message::Text(line)))) => {
                let mut line = line.to_string();
                if !line.ends_with('\n') {
                    line.push('\n');
                }
                if incoming.send(line).await.is_err() {
                    return;
                }
            }
            Input::WebSocket(Some(Ok(Message::Ping(payload)))) => {
                if websocket.send(Message::Pong(payload)).await.is_err() {
                    return;
                }
            }
            Input::WebSocket(Some(Ok(Message::Close(_))) | Some(Err(_)) | None) => return,
            Input::WebSocket(Some(Ok(_))) => {}
        }
    }
}

async fn buffer_during_backoff(
    outgoing: &Receiver<String>,
    buffered: &mut VecDeque<String>,
    subscriptions: &mut HashMap<String, String>,
    duration: Duration,
) {
    let deadline = futures_util::FutureExt::fuse(smol::Timer::after(duration));
    futures_util::pin_mut!(deadline);
    loop {
        enum Input {
            Line(Result<String, async_channel::RecvError>),
            Done,
        }
        let input = {
            let line = outgoing.recv().fuse();
            futures_util::pin_mut!(line);
            futures_util::select! {
                line = line => Input::Line(line),
                _ = deadline => Input::Done,
            }
        };
        match input {
            Input::Line(Ok(line)) => {
                remember_subscription(&line, subscriptions);
                buffered.push_back(line);
            }
            Input::Line(Err(_)) | Input::Done => return,
        }
    }
}

fn remember_subscription(line: &str, subscriptions: &mut HashMap<String, String>) {
    if let Some(key) = subscription_key(line) {
        subscriptions.insert(key, line.to_owned());
    }
}

fn subscription_key(line: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(line.trim_end()).ok()?;
    let payload = value.get("payload")?;
    if !matches!(payload.get("type")?.as_str()?, "subscribe" | "unsubscribe") {
        return None;
    }
    serde_json::to_string(payload.get("content")?.get("topic")?).ok()
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

pub const CERT_CHANGED: &str = "host certificate changed; pair again";
static PIN_MIGRATION: Mutex<()> = Mutex::new(());
static HOST_PATHS: LazyLock<Mutex<HashMap<String, PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static CERT_ERRORS: LazyLock<Mutex<HashMap<String, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// The shared ConnectionState stays transport-neutral; Offline carries this
/// distinguishable reason through the native host's status accessor.
pub fn certificate_changed(host_id: &str) -> bool {
    CERT_ERRORS.lock().unwrap().contains_key(host_id)
}

fn tls_error(error: io::Error) -> String {
    if error.to_string().contains(CERT_CHANGED) {
        CERT_CHANGED.into()
    } else {
        "TLS connection failed".into()
    }
}

fn same_fingerprint(left: &str, right: &str) -> bool {
    tcode_client::pairing::valid_fingerprint(left)
        && tcode_client::pairing::valid_fingerprint(right)
        && left
            .bytes()
            .zip(right.bytes())
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0
}

struct PinVerifier {
    pin: String,
    seen: Arc<Mutex<String>>,
}
impl std::fmt::Debug for PinVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PinVerifier")
    }
}
impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        cert: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fp: String = Sha256::digest(cert.as_ref())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if !self.pin.is_empty() && !same_fingerprint(&fp, &self.pin) {
            return Err(rustls::Error::General(CERT_CHANGED.into()));
        }
        *self.seen.lock().unwrap() = fp;
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

type ObservedPin = Arc<Mutex<String>>;
fn tls_client_config(pin: &str) -> Result<(Arc<rustls::ClientConfig>, ObservedPin), String> {
    if !pin.is_empty() && !tcode_client::pairing::valid_fingerprint(pin) {
        return Err("invalid certificate fingerprint".into());
    }
    let seen = Arc::new(Mutex::new(String::new()));
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| e.to_string())?
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(PinVerifier {
        pin: pin.to_owned(),
        seen: seen.clone(),
    }))
    .with_no_client_auth();
    Ok((Arc::new(config), seen))
}
