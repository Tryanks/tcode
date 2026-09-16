use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use async_channel::{Receiver, Sender};
use async_tungstenite::WebSocketStream;
use futures_util::{FutureExt as _, StreamExt as _};
use serde::Deserialize;
use tungstenite::Message;

pub use tcode_client::pairing::{
    PairInvite, PairedHost, is_pairing_code, lan_origin, pair_url, parse_pair_url,
};
pub use tcode_client::{ConnectionFailure, ConnectionState};

use tcode_client::{
    host::DeviceIdentity,
    outgoing::{Outgoing, OutgoingReceiver, subscription_key},
    recovery::{Backoff, Wake},
};

use crate::discovery::{InterfaceWatch, local_networks};

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

pub fn pair(origin: &str, code: &str, device: &DeviceIdentity) -> Result<PairedHost, String> {
    smol::block_on(pair_async(origin, code, device))
}

pub(crate) async fn pair_async(
    origin: &str,
    code: &str,
    device: &DeviceIdentity,
) -> Result<PairedHost, String> {
    let origin = tcode_client::pairing::parse_origin(origin)?;
    if !is_pairing_code(code) || device.name.is_empty() || device.name.len() > 256 {
        return Err("invalid pairing request".into());
    }
    let bytes = http_request(&origin, "POST", "/pair", &device.pair_body(code)).await?;
    let response: PairResponse =
        serde_json::from_slice(&bytes).map_err(|_| "invalid pairing response")?;
    Ok(PairedHost {
        host_id: response.host_id,
        name: response.host_name,
        origin,
        candidates: Vec::new(),
        token: response.token,
        last_connected_unix: None,
    })
}

/// Bounded HTTP/1.1; HTTPS origins use the standard WebPKI trust roots.
pub fn http(origin: &str, method: &str, path: &str, body: &str) -> Result<Vec<u8>, String> {
    smol::block_on(http_request(origin, method, path, body))
}

async fn http_request(
    origin: &str,
    method: &str,
    path: &str,
    body: &str,
) -> Result<Vec<u8>, String> {
    if !matches!(
        (method, path),
        ("POST", "/pair" | "/auth/setup" | "/auth/login")
            | ("GET", "/auth/state" | "/admin/pair" | "/")
            | ("HEAD", "/")
    ) || body.len() > 4096
    {
        return Err("invalid HTTP request".into());
    }
    let endpoint = crate::endpoint::Endpoint::new(origin)?;
    futures_lite::future::race(http_async(&endpoint, method, path, body), async {
        smol::Timer::after(Duration::from_secs(
            if matches!(path, "/auth/setup" | "/auth/login") {
                60
            } else {
                5
            },
        ))
        .await;
        Err("HTTP request timed out".into())
    })
    .await
}

pub(crate) async fn http_async(
    endpoint: &crate::endpoint::Endpoint,
    method: &str,
    path: &str,
    body: &str,
) -> Result<Vec<u8>, String> {
    use futures_lite::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut stream = endpoint.connect().await.map_err(|e| e.to_string())?;
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        endpoint.authority(),
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    stream.flush().await.map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    // A tunnel may close TLS without close_notify. Require the complete
    // Content-Length-delimited response before accepting such a close.
    let read = (&mut stream).take(65537).read_to_end(&mut bytes).await;
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

/// Open a reconnecting link to `host`. With `data_dir`, an origin that answers
/// hello is promoted in hosts.json together with the addresses the machine
/// reports, so the next launch starts from what worked last.
pub fn connect(
    host: PairedHost,
    device: DeviceIdentity,
    data_dir: Option<PathBuf>,
) -> RemoteClient {
    let (to_host, outgoing) = tcode_client::outgoing::channel();
    let (incoming, from_host) = async_channel::unbounded();
    let (state_tx, state) = async_channel::unbounded();
    std::thread::Builder::new()
        .name("tcode-remote-client".into())
        .spawn(move || {
            smol::block_on(connection_loop(
                host, device, data_dir, outgoing, incoming, state_tx,
            ));
        })
        .expect("failed to spawn remote client thread");
    RemoteClient {
        to_host,
        from_host,
        state,
    }
}

/// How often this device's address set is re-read while a link is being
/// re-established, and while it is healthy.
const RECONNECTING_INTERFACE_POLL: Duration = Duration::from_millis(2_500);
const CONNECTED_INTERFACE_POLL: Duration = Duration::from_secs(10);
/// Handshake budget per origin when several are raced, and the delay between
/// starting one origin and the next.
const RACE_BUDGET: Duration = Duration::from_secs(5);
const RACE_STAGGER: Duration = Duration::from_millis(250);
const MAX_RACED_ORIGINS: usize = 32;

/// What interrupted an attempt or a backoff sleep.
enum Interrupt {
    Wake(Result<Wake, async_channel::RecvError>),
    NetworkChanged,
}

async fn connection_loop(
    mut host: PairedHost,
    device: DeviceIdentity,
    data_dir: Option<PathBuf>,
    outgoing: OutgoingReceiver,
    incoming: Sender<String>,
    state: Sender<ConnectionState>,
) {
    let mut buffered = VecDeque::<String>::new();
    let mut subscriptions = HashMap::<String, String>::new();
    let mut backoff = Backoff::default();
    let mut reason = None;
    let mut interfaces = InterfaceWatch::new(local_networks());
    // The saved origin is tried alone first; candidates are raced once it
    // failed to answer, or as soon as this device's addresses changed.
    let mut race = false;
    while !outgoing.is_closed() && !incoming.is_closed() {
        outgoing.discard_retained_writes(&mut buffered);
        if interfaces.observe(local_networks()) {
            backoff.network_changed();
            race = true;
        }
        let _ = state
            .send(ConnectionState::Reconnecting {
                attempt: backoff.attempt(),
                reason,
            })
            .await;
        let mut stable_ms = 0;
        let mut interrupted = None;
        let opened = {
            let attempt = host.clone();
            let establishing = establish_websocket(&attempt, &device, race).fuse();
            let wakes = async {
                loop {
                    let wake = outgoing.wake.recv().await;
                    // Hints that add nothing must not restart a live attempt.
                    if let Ok(Wake::Candidates(origins)) = &wake
                        && !new_candidates(&host, origins)
                    {
                        continue;
                    }
                    return Interrupt::Wake(wake);
                }
            }
            .fuse();
            let network = watch_interfaces(&mut interfaces, RECONNECTING_INTERFACE_POLL).fuse();
            futures_util::pin_mut!(establishing, wakes, network);
            futures_util::select! {
                result = establishing => Ok(result),
                interrupt = wakes => Err(interrupt),
                _ = network => Err(Interrupt::NetworkChanged),
            }
        };
        let opened = match opened {
            Ok(result) => result,
            Err(Interrupt::Wake(wake)) => {
                if let Ok(Wake::Candidates(origins)) = wake {
                    remember_candidates(&mut host, data_dir.as_deref(), &origins);
                    race = true;
                }
                continue;
            }
            Err(Interrupt::NetworkChanged) => {
                backoff.network_changed();
                race = true;
                continue;
            }
        };
        let failure = match opened {
            Ok(established) => {
                let Established {
                    mut websocket,
                    origin,
                    reported,
                } = established;
                let promoted = host.promote_origin(&origin);
                let learned = host.add_candidates(reported.iter().map(String::as_str));
                if promoted || learned {
                    save_origins(data_dir.as_deref(), &host);
                }
                race = false;
                let _ = state.send(ConnectionState::Syncing).await;
                let mut failure = None;
                for line in subscriptions.values() {
                    if let Err(error) = send_interruptible(
                        &mut websocket,
                        Message::Text(line.trim_end().to_owned().into()),
                        Instant::now()
                            + Duration::from_millis(tcode_client::heartbeat::NATIVE_IDLE_MS),
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
                            Instant::now()
                                + Duration::from_millis(tcode_client::heartbeat::NATIVE_IDLE_MS),
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
                        let lost = relay_connected(
                            &mut websocket,
                            &outgoing,
                            &incoming,
                            &mut subscriptions,
                            &mut buffered,
                            &state,
                            &mut host,
                            data_dir.as_deref(),
                            &mut interfaces,
                        )
                        .await;
                        if lost.healthy {
                            stable_ms = started.elapsed().as_millis() as u64;
                        }
                        if lost.network_changed {
                            backoff.network_changed();
                            race = true;
                        }
                        if lost.wake.is_some() {
                            continue;
                        }
                        lost.failure
                    }
                }
            }
            Err(error) => error,
        };
        if let Some(wake) = interrupted {
            if let Wake::Candidates(origins) = wake {
                remember_candidates(&mut host, data_dir.as_deref(), &origins);
                race = true;
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
        if matches!(
            failure,
            ConnectionFailure::Unreachable | ConnectionFailure::Timeout
        ) {
            race = true;
        }
        let delay = backoff.failed(stable_ms, jitter_sample());
        // Publish loss before sleeping, so the UI never claims this socket is alive.
        let _ = state
            .send(ConnectionState::Reconnecting {
                attempt: backoff.attempt(),
                reason,
            })
            .await;
        let deadline = Instant::now() + Duration::from_millis(delay);
        loop {
            match buffer_during_backoff(
                &outgoing,
                &mut buffered,
                &mut subscriptions,
                deadline,
                &mut interfaces,
            )
            .await
            {
                Some(Interrupt::Wake(Ok(Wake::Candidates(origins)))) => {
                    if remember_candidates(&mut host, data_dir.as_deref(), &origins) {
                        race = true;
                        break;
                    }
                }
                Some(Interrupt::NetworkChanged) => {
                    backoff.network_changed();
                    race = true;
                    break;
                }
                Some(Interrupt::Wake(_)) | None => break,
            }
        }
    }
    let _ = state
        .send(ConnectionState::Offline {
            reason: ConnectionFailure::HostClosed,
        })
        .await;
    incoming.close();
}

/// Resolves once this device's address set differs from the last snapshot.
async fn watch_interfaces(interfaces: &mut InterfaceWatch, every: Duration) {
    loop {
        smol::Timer::after(every).await;
        if interfaces.observe(local_networks()) {
            return;
        }
    }
}

fn new_candidates(host: &PairedHost, origins: &[String]) -> bool {
    remember_candidates(&mut host.clone(), None, origins)
}

/// Merge discovery hints into the saved record; true when any were new.
fn remember_candidates(host: &mut PairedHost, data_dir: Option<&Path>, origins: &[String]) -> bool {
    let added = host.add_candidates(
        origins
            .iter()
            .filter(|origin| plain_http_origin(origin))
            .map(String::as_str),
    );
    if added {
        save_origins(data_dir, host);
    }
    added
}

/// Persist the origin and candidates of `host` on its hosts.json record. The
/// record's token, name and connection stamp belong to the UI, which may have
/// changed them meanwhile, so only the address fields are replaced.
fn save_origins(data_dir: Option<&Path>, host: &PairedHost) {
    let Some(data_dir) = data_dir else {
        return;
    };
    let result = load_hosts(data_dir).and_then(|mut hosts| {
        let Some(saved) = hosts.iter_mut().find(|saved| saved.host_id == host.host_id) else {
            return Ok(());
        };
        if saved.origin == host.origin && saved.candidates == host.candidates {
            return Ok(());
        }
        saved.origin = host.origin.clone();
        saved.candidates = host.candidates.clone();
        save_hosts(data_dir, &hosts)
    });
    if let Err(error) = result {
        log::error!("could not persist the machine's addresses: {error}");
    }
}

/// Whether alternates may be raced for this origin: the token already
/// travels in clear to it, so probing other plain addresses adds no
/// exposure. An HTTPS pairing is never downgraded.
pub(crate) fn plain_http_origin(origin: &str) -> bool {
    url::Url::parse(origin).is_ok_and(|url| url.scheme() == "http")
}

/// A machine reached over loopback is this device; the gateways of its
/// networks are other devices entirely.
fn loopback_origin(origin: &str) -> bool {
    url::Url::parse(origin).is_ok_and(|url| match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        None => true,
    })
}

type WebSocket = WebSocketStream<crate::endpoint::Stream>;

fn connection_failure(error: String) -> ConnectionFailure {
    log::debug!("remote connection failed: {error}");
    ConnectionFailure::Unreachable
}

struct Established {
    websocket: WebSocket,
    /// The origin that completed hello.
    origin: String,
    /// Origins the machine reports for itself.
    reported: Vec<String>,
}

/// Try the saved origin, and when `race` is set every other origin the
/// machine may answer at: saved candidates and the gateways of this device's
/// current networks. The first identity-verified hello wins; an origin where
/// a different machine answers counts as unreachable. A rejection is only
/// terminal when it comes from the paired machine itself.
async fn establish_websocket(
    host: &PairedHost,
    device: &DeviceIdentity,
    race: bool,
) -> Result<Established, ConnectionFailure> {
    let mut origins = vec![host.origin.clone()];
    if race && plain_http_origin(&host.origin) {
        origins.extend(host.candidates.iter().cloned());
        if !loopback_origin(&host.origin) {
            let port = url::Url::parse(&host.origin)
                .ok()
                .and_then(|url| url.port_or_known_default())
                .unwrap_or(tcode_client::pairing::DEFAULT_REMOTE_PORT);
            origins.extend(crate::discovery::interface_probe_origins(
                &local_networks(),
                port,
            ));
        }
        let mut seen = std::collections::HashSet::new();
        origins.retain(|origin| seen.insert(origin.clone()));
        origins.truncate(MAX_RACED_ORIGINS);
    }
    let racing = origins.len() > 1;
    let mut attempts = futures_util::stream::FuturesUnordered::new();
    for (index, origin) in origins.into_iter().enumerate() {
        attempts.push(async move {
            smol::Timer::after(RACE_STAGGER * index as u32).await;
            let attempt = attempt_origin(&origin, host, device, index == 0);
            let result = if racing {
                futures_lite::future::race(attempt, async {
                    smol::Timer::after(RACE_BUDGET).await;
                    Err(ConnectionFailure::Timeout)
                })
                .await
            } else {
                attempt.await
            };
            (index == 0, origin, result)
        });
    }
    let mut failure = ConnectionFailure::Unreachable;
    let mut primary_failure = None;
    while let Some((primary, origin, result)) = attempts.next().await {
        match result {
            Ok((websocket, reported)) => {
                return Ok(Established {
                    websocket,
                    origin,
                    reported,
                });
            }
            Err(error) if error.is_terminal() => return Err(error),
            Err(error) if primary => primary_failure = Some(error),
            Err(error) => failure = error,
        }
    }
    // The banner explains the saved origin, not whichever probe failed last.
    Err(primary_failure.unwrap_or(failure))
}

async fn attempt_origin(
    origin: &str,
    host: &PairedHost,
    device: &DeviceIdentity,
    primary: bool,
) -> Result<(WebSocket, Vec<String>), ConnectionFailure> {
    let url = url::Url::parse(origin).map_err(|e| connection_failure(e.to_string()))?;
    let endpoint = crate::endpoint::Endpoint::new(origin).map_err(connection_failure)?;
    endpoint
        .establish(|stream| open_websocket(stream, &url, host, device, primary))
        .await
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
    let deadline = deadline
        .min(Instant::now() + Duration::from_millis(tcode_client::heartbeat::NATIVE_IDLE_MS));
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

/// Complete the WebSocket upgrade and hello on `stream`. `primary` marks the
/// saved origin: an older machine there that rejects without naming itself is
/// still taken to be the paired one; anywhere else it is a stranger.
pub(crate) async fn open_websocket(
    stream: crate::endpoint::Stream,
    origin: &url::Url,
    host: &PairedHost,
    device: &DeviceIdentity,
    primary: bool,
) -> Result<(WebSocket, Vec<String>), ConnectionFailure> {
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
    websocket
        .send(Message::Text(device.hello_line(&host.token).into()))
        .await
        .map_err(|error| connection_failure(error.to_string()))?;
    match websocket.next().await {
        Some(Ok(Message::Text(text))) => {
            let value: serde_json::Value = serde_json::from_str(&text)
                .map_err(|error| connection_failure(error.to_string()))?;
            hello_verdict(&value, &host.host_id, primary).map(|reported| (websocket, reported))
        }
        Some(Ok(Message::Close(_))) => Err(ConnectionFailure::HostClosed),
        Some(Ok(_)) => Err(ConnectionFailure::Unreachable),
        Some(Err(error)) => Err(connection_failure(error.to_string())),
        None => Err(ConnectionFailure::HostClosed),
    }
}

/// Judge a hello reply against the paired machine identity. Accepting a
/// reply from another machine would bind the session to a stranger; treating
/// its rejection as terminal would send the user to pair again for nothing.
/// Success carries the origins the machine reports for itself.
fn hello_verdict(
    reply: &serde_json::Value,
    host_id: &str,
    primary: bool,
) -> Result<Vec<String>, ConnectionFailure> {
    let identity = reply["host_id"].as_str();
    match reply["type"].as_str() {
        Some("hello_ok") => {
            if identity != Some(host_id) {
                log::debug!("another machine answered: {identity:?}");
                return Err(ConnectionFailure::Unreachable);
            }
            match reply["protocol_version"].as_u64() {
                Some(3 | 4) => Ok(reported_origins(reply)),
                _ => Err(ConnectionFailure::ProtocolMismatch),
            }
        }
        Some("hello_rejected") => {
            let ours = identity.map_or(primary, |identity| identity == host_id);
            if ours {
                Err(ConnectionFailure::hello_rejected(reply["reason"].as_str()))
            } else {
                log::debug!("another machine rejected hello: {identity:?}");
                Err(ConnectionFailure::Unreachable)
            }
        }
        _ => Err(ConnectionFailure::Unreachable),
    }
}

/// `addrs` and `port` from hello_ok as origins; absent on older machines.
fn reported_origins(reply: &serde_json::Value) -> Vec<String> {
    let Some(port) = reply["port"]
        .as_u64()
        .and_then(|port| u16::try_from(port).ok())
    else {
        return Vec::new();
    };
    if port == 0 {
        return Vec::new();
    }
    reply["addrs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .filter_map(|addr| addr.parse::<std::net::IpAddr>().ok())
        .filter(|ip| match ip {
            std::net::IpAddr::V4(ip) => {
                !ip.is_loopback() && !ip.is_link_local() && !ip.is_unspecified()
            }
            std::net::IpAddr::V6(ip) => {
                !ip.is_loopback() && !ip.is_unicast_link_local() && !ip.is_unspecified()
            }
        })
        .map(|ip| lan_origin(&ip.to_string(), port))
        .take(tcode_client::pairing::MAX_CANDIDATE_ORIGINS)
        .collect()
}

/// Why a healthy link ended and how the loop should continue.
struct Lost {
    failure: ConnectionFailure,
    /// The link carried at least one host message.
    healthy: bool,
    /// Retry at once rather than after backoff.
    wake: Option<Wake>,
    /// This device's addresses changed while the link was up.
    network_changed: bool,
}

#[allow(clippy::too_many_arguments)]
async fn relay_connected(
    websocket: &mut WebSocket,
    outgoing: &OutgoingReceiver,
    incoming: &Sender<String>,
    subscriptions: &mut HashMap<String, String>,
    buffered: &mut VecDeque<String>,
    state: &Sender<ConnectionState>,
    host: &mut PairedHost,
    data_dir: Option<&Path>,
    interfaces: &mut InterfaceWatch,
) -> Lost {
    let mut healthy = false;
    let mut interrupted = None;
    let mut connected = false;
    let mut deadline =
        Instant::now() + Duration::from_millis(tcode_client::heartbeat::NATIVE_IDLE_MS);
    let mut probing = false;
    let mut foreground_probe = false;
    let mut network_changed = false;
    let mut network_check = Instant::now() + CONNECTED_INTERFACE_POLL;
    loop {
        enum Input {
            Outgoing(Result<String, async_channel::RecvError>),
            WebSocket(Option<Result<Message, tungstenite::Error>>),
            Timer,
            Network,
            Wake(Result<Wake, async_channel::RecvError>),
        }
        let input = {
            let wake = outgoing.wake.recv().fuse();
            let outbound = outgoing.recv().fuse();
            let websocket_input = websocket.next().fuse();
            let timer = futures_util::FutureExt::fuse(smol::Timer::at(deadline));
            let network = futures_util::FutureExt::fuse(smol::Timer::at(network_check));
            futures_util::pin_mut!(outbound, websocket_input, timer, network, wake);
            futures_util::select! {
                line = outbound => Input::Outgoing(line),
                message = websocket_input => Input::WebSocket(message),
                _ = timer => Input::Timer,
                _ = network => Input::Network,
                wake = wake => Input::Wake(wake),
            }
        };
        if matches!(&input, Input::WebSocket(Some(Ok(message))) if !matches!(message, Message::Close(_)))
        {
            healthy = true;
            probing = false;
            foreground_probe = false;
            // The link outlived the address change; no need to race later.
            network_changed = false;
            deadline =
                Instant::now() + Duration::from_millis(tcode_client::heartbeat::NATIVE_IDLE_MS);
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
            Input::Wake(Ok(Wake::Candidates(origins))) => {
                // A late discovery result must not cost a healthy link.
                remember_candidates(host, data_dir, &origins);
                None
            }
            Input::Wake(Ok(wake)) => {
                return Lost {
                    failure: ConnectionFailure::Unreachable,
                    healthy,
                    wake: Some(wake),
                    network_changed,
                };
            }
            Input::Wake(Err(_)) => Some(ConnectionFailure::HostClosed),
            Input::Network => {
                network_check = Instant::now() + CONNECTED_INTERFACE_POLL;
                if !interfaces.observe(local_networks()) {
                    None
                } else {
                    // The socket may be dead on a vanished interface; find
                    // out within seconds instead of the idle window.
                    network_changed = true;
                    if probing {
                        None
                    } else {
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
                }
            }
            Input::Timer if probing => Some(ConnectionFailure::Timeout),
            Input::Timer => {
                probing = true;
                // Keep the silence window absolute so timer scheduling does not
                // accumulate beyond the 10 + 20 second liveness budget.
                deadline += Duration::from_millis(tcode_client::heartbeat::LIVENESS_REPLY_MS);
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
                    Instant::now() + Duration::from_millis(tcode_client::heartbeat::NATIVE_IDLE_MS)
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
            return Lost {
                failure,
                healthy,
                wake: interrupted.or_else(|| foreground_probe.then_some(Wake::Reconnect)),
                network_changed,
            };
        }
    }
}

/// Queue writes while waiting for `deadline`. Ends early on a wake or when
/// this device's addresses change.
async fn buffer_during_backoff(
    outgoing: &OutgoingReceiver,
    buffered: &mut VecDeque<String>,
    subscriptions: &mut HashMap<String, String>,
    deadline: Instant,
    interfaces: &mut InterfaceWatch,
) -> Option<Interrupt> {
    let done = futures_util::FutureExt::fuse(smol::Timer::at(deadline));
    let network = watch_interfaces(interfaces, RECONNECTING_INTERFACE_POLL).fuse();
    futures_util::pin_mut!(done, network);
    loop {
        enum Input {
            Line(Result<String, async_channel::RecvError>),
            Done,
            Network,
            Wake(Result<Wake, async_channel::RecvError>),
        }
        let input = {
            let wake = outgoing.wake.recv().fuse();
            let line = outgoing.recv().fuse();
            futures_util::pin_mut!(line, wake);
            futures_util::select! {
                line = line => Input::Line(line),
                _ = done => Input::Done,
                _ = network => Input::Network,
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
            Input::Wake(wake) => return Some(Interrupt::Wake(wake)),
            Input::Network => return Some(Interrupt::NetworkChanged),
            Input::Line(Err(_)) | Input::Done => return None,
        }
    }
}

fn remember_subscription(line: &str, subscriptions: &mut HashMap<String, String>) {
    if let Some(key) = subscription_key(line) {
        subscriptions.insert(key, line.to_owned());
    }
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

#[cfg(test)]
mod establishment_tests {
    use super::*;
    use futures_lite::io::{AsyncReadExt as _, AsyncWriteExt as _};

    #[test]
    fn hello_replies_are_judged_by_machine_identity() {
        let ok = serde_json::json!({
            "type": "hello_ok",
            "host_id": "mine",
            "host_name": "Desk",
            "protocol_version": 4,
            "addrs": ["192.168.1.10", "fd00::10", "127.0.0.1", "169.254.7.7", "fe80::1", "not an ip"],
            "port": 47420
        });
        assert_eq!(
            hello_verdict(&ok, "mine", true).unwrap(),
            ["http://192.168.1.10:47420", "http://[fd00::10]:47420"]
        );
        assert_eq!(
            hello_verdict(&ok, "other", false),
            Err(ConnectionFailure::Unreachable)
        );
        assert_eq!(
            hello_verdict(&ok, "other", true),
            Err(ConnectionFailure::Unreachable)
        );
        // A machine from before address reporting still connects.
        let older = serde_json::json!({"type": "hello_ok", "host_id": "mine", "host_name": "Desk", "protocol_version": 3});
        assert_eq!(
            hello_verdict(&older, "mine", false).unwrap(),
            Vec::<String>::new()
        );
        let mismatch =
            serde_json::json!({"type": "hello_ok", "host_id": "mine", "protocol_version": 9});
        assert_eq!(
            hello_verdict(&mismatch, "mine", true),
            Err(ConnectionFailure::ProtocolMismatch)
        );

        let refused = |host_id: Option<&str>| {
            let mut reply = serde_json::json!({"type": "hello_rejected", "reason": "token"});
            if let Some(host_id) = host_id {
                reply["host_id"] = host_id.into();
            }
            reply
        };
        assert_eq!(
            hello_verdict(&refused(Some("mine")), "mine", false),
            Err(ConnectionFailure::AuthenticationRejected)
        );
        assert_eq!(
            hello_verdict(&refused(Some("stranger")), "mine", true),
            Err(ConnectionFailure::Unreachable)
        );
        // Only the saved origin may assume an older, silent machine is ours.
        assert_eq!(
            hello_verdict(&refused(None), "mine", true),
            Err(ConnectionFailure::AuthenticationRejected)
        );
        assert_eq!(
            hello_verdict(&refused(None), "mine", false),
            Err(ConnectionFailure::Unreachable)
        );
        let protocol =
            serde_json::json!({"type": "hello_rejected", "reason": "protocol", "host_id": "mine"});
        assert_eq!(
            hello_verdict(&protocol, "mine", false),
            Err(ConnectionFailure::ProtocolMismatch)
        );
        assert_eq!(
            hello_verdict(&serde_json::json!({"type": "event"}), "mine", true),
            Err(ConnectionFailure::Unreachable)
        );
    }

    #[test]
    fn stalled_http_times_out_and_cancelled_pairing_closes_its_stream() {
        smol::block_on(async {
            for cancel in [false, true] {
                let listener = smol::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let origin = format!("http://{}", listener.local_addr().unwrap());
                let (received, request_received) = async_channel::bounded(1);
                let peer = smol::spawn(async move {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut head = vec![];
                    while !head.ends_with(b"\r\n\r\n") {
                        let mut byte = [0];
                        stream.read_exact(&mut byte).await.unwrap();
                        head.push(byte[0]);
                    }
                    received.send(()).await.unwrap();
                    if !cancel {
                        stream
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\nx")
                            .await
                            .unwrap();
                    }
                    let mut remaining = vec![];
                    stream.read_to_end(&mut remaining).await.unwrap();
                });
                let started = Instant::now();
                if cancel {
                    let device = DeviceIdentity {
                        id: "cancelled".into(),
                        name: "cancelled device".into(),
                        platform: None,
                    };
                    let task =
                        smol::spawn(async move { pair_async(&origin, "123456", &device).await });
                    request_received.recv().await.unwrap();
                    task.cancel().await;
                } else {
                    assert_eq!(
                        http_request(&origin, "GET", "/auth/state", "")
                            .await
                            .unwrap_err(),
                        "HTTP request timed out"
                    );
                    assert!(started.elapsed() >= Duration::from_secs(5));
                    assert!(started.elapsed() < Duration::from_secs(8));
                }
                futures_lite::future::race(
                    async {
                        peer.await;
                    },
                    async {
                        smol::Timer::after(Duration::from_secs(2)).await;
                        panic!("cancelled HTTP stream remained open");
                    },
                )
                .await;
            }
        });
    }
}
