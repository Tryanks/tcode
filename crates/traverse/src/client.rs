//! The device side: one iroh endpoint per [`DeviceIdentity`], pairing over
//! `tcode/pair/1` and a reconnecting main-stream [`Transport`] over `tcode/1`.
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_channel::Sender;
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayUrl, TransportAddr,
    address_lookup::{AddressLookupBuilder as _, PkarrResolver},
    endpoint::{Connection, ConnectionError, SendStream, presets},
};
use tcode_client::{
    ConnectionFailure, ConnectionState,
    heartbeat::{Heartbeat, LIVENESS_REPLY_MS, NATIVE_IDLE_MS, Tick},
    host::{LiveHost, Transport},
    outgoing::{Outgoing, OutgoingReceiver, subscription_key},
    pairing::{MAX_ADDRS, PairInvite, PairedHost},
    recovery::{Backoff, Wake},
};
use url::Url;

use crate::{
    identity::DeviceIdentity,
    runtime::{block_on, runtime},
    wire::{self, ClientLine, HelloRejection, HostLine, LineReader, PairRejection},
};

/// Budget for dialing and completing the QUIC handshake.
const CONNECT_BUDGET: Duration = Duration::from_secs(20);
/// Close codes the machine uses; see [`crate::host`].
const CLOSE_UNPAIRED: u32 = 1;
const CLOSE_REVOKED: u32 = 3;

/// The endpoint a device dials from, built on first use.
pub(crate) struct ClientEndpoint {
    endpoint: Endpoint,
    /// Transports to probe when the network changes.
    transports: Mutex<Vec<Outgoing>>,
    /// Traverse instances whose resolvers are already installed.
    resolvers: Mutex<HashSet<Url>>,
}

impl DeviceIdentity {
    /// The endpoint, bound on first use. Must run on the runtime.
    pub(crate) async fn client(&self) -> io::Result<&ClientEndpoint> {
        let inner = self.inner();
        inner
            .endpoint
            .get_or_try_init(|| async {
                let options = *inner.options.lock().unwrap();
                let secret_key = inner.secret_key.clone();
                // Resolve machines through the official services without
                // publishing this device anywhere; the machine is what gets
                // looked up. A relay of our own still helps hole punching.
                let mut builder = if options.official {
                    Endpoint::builder(presets::Minimal)
                        .relay_mode(iroh::endpoint::default_relay_mode())
                        .address_lookup(PkarrResolver::n0_dns())
                        .address_lookup(iroh::address_lookup::DnsAddressLookup::n0_dns())
                } else {
                    Endpoint::builder(presets::Minimal).relay_mode(iroh::RelayMode::Disabled)
                };
                builder = builder
                    .secret_key(secret_key.clone())
                    .transport_config(wire::transport_config());
                let endpoint = builder.bind().await.map_err(io::Error::other)?;
                Ok(ClientEndpoint {
                    endpoint,
                    transports: Mutex::new(Vec::new()),
                    resolvers: Mutex::new(HashSet::new()),
                })
            })
            .await
    }

    /// The device's network changed: rebind paths and probe every live
    /// transport at once instead of waiting for the idle timer.
    pub fn network_changed(&self) {
        let Some(client) = self.inner().endpoint.get() else {
            return;
        };
        let endpoint = client.endpoint.clone();
        runtime().spawn(async move { endpoint.network_change().await });
        let mut transports = client.transports.lock().unwrap();
        transports.retain(|transport| !transport.is_closed());
        for transport in transports.iter() {
            transport.wake(Wake::Probe);
        }
    }
}

impl ClientEndpoint {
    /// Make a machine's Traverse instance resolvable. Missing manifests are
    /// logged: the stored relay and addresses may still reach the machine.
    fn install_resolvers(&self, device: &DeviceIdentity, traverse: Option<&str>) {
        let Some(base) = traverse.and_then(|base| Url::parse(base).ok()) else {
            return;
        };
        if !self.resolvers.lock().unwrap().insert(base.clone()) {
            return;
        }
        let manifest = match crate::manifest::load(device.data_dir(), &base) {
            Ok(manifest) => manifest,
            Err(error) => {
                log::warn!("Traverse manifest for {base} unavailable: {error}");
                return;
            }
        };
        let Ok(services) = self.endpoint.address_lookup() else {
            return;
        };
        for pkarr in manifest.pkarr_urls() {
            match PkarrResolver::builder(pkarr.clone()).into_address_lookup(&self.endpoint) {
                Ok(resolver) => services.add(resolver),
                Err(error) => log::warn!("could not add pkarr resolver {pkarr}: {error}"),
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairError {
    /// Wrong, expired or already used code.
    Code,
    Disabled,
    Busy,
    Unreachable(String),
    /// The machine answered with something other than a pairing reply.
    Protocol(String),
}

impl std::fmt::Display for PairError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Code => f.write_str("invalid or expired pairing code"),
            Self::Disabled => f.write_str("pairing_disabled"),
            Self::Busy => f.write_str("the machine could not record the pairing; try again"),
            Self::Unreachable(error) => write!(f, "could not connect to the machine: {error}"),
            Self::Protocol(error) => write!(f, "invalid pairing response: {error}"),
        }
    }
}

impl std::error::Error for PairError {}

/// Where to dial a machine from what a device knows about it. Discovery
/// services add addresses for the same id; nothing can change the id.
fn dial_addr(host_id: &str, relay: Option<&str>, addrs: &[String]) -> Option<EndpointAddr> {
    let id: EndpointId = host_id.parse().ok()?;
    let mut transport_addrs = Vec::new();
    if let Some(relay) = relay.and_then(|relay| relay.parse::<RelayUrl>().ok()) {
        transport_addrs.push(TransportAddr::Relay(relay));
    }
    for addr in addrs {
        if let Ok(addr) = addr.parse::<std::net::SocketAddr>() {
            transport_addrs.push(TransportAddr::Ip(addr));
        }
    }
    Some(EndpointAddr::from_parts(id, transport_addrs))
}

/// Exchange `code` for a pairing with exactly `invite.host_id`.
pub async fn pair(
    invite: &PairInvite,
    code: &str,
    device: &DeviceIdentity,
) -> Result<PairedHost, PairError> {
    let client = device
        .client()
        .await
        .map_err(|error| PairError::Unreachable(error.to_string()))?;
    let addr = dial_addr(&invite.host_id, invite.relay.as_deref(), &invite.addrs)
        .ok_or_else(|| PairError::Protocol("invalid machine id".into()))?;
    client.install_resolvers(device, invite.traverse.as_deref());
    let connection = tokio::time::timeout(
        CONNECT_BUDGET,
        client.endpoint.connect(addr, wire::ALPN_PAIR),
    )
    .await
    .map_err(|_| PairError::Unreachable("connection timed out".into()))?
    .map_err(|error| PairError::Unreachable(error.to_string()))?;
    let exchange = async {
        let (mut send, recv) = connection
            .open_bi()
            .await
            .map_err(|error| PairError::Unreachable(error.to_string()))?;
        wire::write_line(
            &mut send,
            &ClientLine::Pair {
                code: code.to_owned(),
                device: device.claim(),
            },
        )
        .await
        .map_err(|error| PairError::Unreachable(error.to_string()))?;
        send.finish()
            .map_err(|error| PairError::Unreachable(error.to_string()))?;
        let mut reader = wire::reader(recv);
        // Never resubmit a possibly consumed code after losing its response.
        match wire::read_control::<HostLine>(&mut reader)
            .await
            .map_err(|error| PairError::Protocol(error.to_string()))?
        {
            HostLine::Paired { host_name } => Ok(invite.paired(host_name)),
            HostLine::PairRejected { reason } => Err(match reason {
                PairRejection::Code => PairError::Code,
                PairRejection::Disabled => PairError::Disabled,
                PairRejection::Busy => PairError::Busy,
            }),
            HostLine::Refused { reason } => Err(PairError::Protocol(reason)),
            other => Err(PairError::Protocol(format!("unexpected reply {other:?}"))),
        }
    };
    let result = exchange.await;
    connection.close(0_u32.into(), b"done");
    result
}

/// [`pair`] from a thread outside the runtime.
pub fn pair_blocking(
    invite: &PairInvite,
    code: &str,
    device: &DeviceIdentity,
) -> Result<PairedHost, PairError> {
    block_on(pair(invite, code, device))
}

/// Open a reconnecting link to `host`. Dropping the returned channels ends
/// it. The stored relay and addresses are refreshed in `hosts.json` after
/// each authenticated connection.
pub fn connect(host: &PairedHost, device: &DeviceIdentity) -> Transport {
    let (to_host, outgoing) = tcode_client::outgoing::channel();
    let (incoming, from_host) = async_channel::unbounded();
    let (state_tx, state) = async_channel::unbounded();
    let current_host = LiveHost::new(host.clone());
    let live = current_host.clone();
    let device = device.clone();
    let registered = to_host.clone();
    runtime().spawn(async move {
        if let Ok(client) = device.client().await {
            client.transports.lock().unwrap().push(registered);
        }
        connection_loop(device, live, Arc::new(outgoing), incoming, state_tx).await;
    });
    Transport {
        to_host,
        from_host,
        state,
        current_host: Some(current_host),
    }
}

struct Established {
    connection: Connection,
    send: SendStream,
    reader: LineReader,
}

async fn connection_loop(
    device: DeviceIdentity,
    live: LiveHost,
    outgoing: Arc<OutgoingReceiver>,
    incoming: Sender<String>,
    state: Sender<ConnectionState>,
) {
    let mut host = live.snapshot();
    let mut buffered = VecDeque::<String>::new();
    let mut subscriptions = HashMap::<String, String>::new();
    let mut backoff = Backoff::default();
    let mut reason = None;
    while !outgoing.is_closed() && !incoming.is_closed() {
        outgoing.discard_retained_writes(&mut buffered);
        let _ = state
            .send(ConnectionState::Reconnecting {
                attempt: backoff.attempt(),
                reason,
            })
            .await;
        let mut stable_ms = 0;
        let established = {
            let establishing = std::pin::pin!(establish(&device, &host));
            let mut establishing = Some(establishing);
            loop {
                let Some(attempt) = establishing.as_mut() else {
                    break None;
                };
                tokio::select! {
                    result = attempt => break Some(result),
                    wake = outgoing.wake.recv() => match wake {
                        // A probe cannot hurry a dial; a reconnect restarts it.
                        Ok(Wake::Probe) => continue,
                        Ok(Wake::Reconnect) => break None,
                        Err(_) => { establishing = None; }
                    },
                }
            }
        };
        let Some(established) = established else {
            if outgoing.is_closed() {
                break;
            }
            continue;
        };
        let failure = match established {
            Ok(established) => {
                learn_addresses(&mut host, &established.connection);
                live.authenticated(&host);
                persist_addresses(&device, &host);
                let _ = state.send(ConnectionState::Syncing).await;
                let started = Instant::now();
                let lost = relay_connected(
                    established,
                    &outgoing,
                    &incoming,
                    &state,
                    &mut subscriptions,
                    &mut buffered,
                )
                .await;
                if lost.healthy {
                    stable_ms = started.elapsed().as_millis() as u64;
                }
                if lost.wake == Some(Wake::Reconnect) {
                    continue;
                }
                lost.failure
            }
            Err(failure) => failure,
        };
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
        // Publish loss before sleeping, so the UI never claims the link is alive.
        let _ = state
            .send(ConnectionState::Reconnecting {
                attempt: backoff.attempt(),
                reason,
            })
            .await;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(delay);
        buffer_during_backoff(&outgoing, &mut buffered, &mut subscriptions, deadline).await;
    }
    let _ = state
        .send(ConnectionState::Offline {
            reason: ConnectionFailure::HostClosed,
        })
        .await;
    incoming.close();
}

async fn establish(
    device: &DeviceIdentity,
    host: &PairedHost,
) -> Result<Established, ConnectionFailure> {
    let client = device.client().await.map_err(|error| {
        log::error!("device endpoint unavailable: {error}");
        ConnectionFailure::Unreachable
    })?;
    let addr = dial_addr(&host.host_id, host.relay.as_deref(), &host.addrs)
        .ok_or(ConnectionFailure::Unreachable)?;
    client.install_resolvers(device, host.traverse.as_deref());
    let connection = match tokio::time::timeout(
        CONNECT_BUDGET,
        client.endpoint.connect(addr, wire::ALPN_MAIN),
    )
    .await
    {
        Ok(Ok(connection)) => connection,
        Ok(Err(error)) => {
            log::debug!("connection to {} failed: {error}", host.host_id);
            return Err(ConnectionFailure::Unreachable);
        }
        Err(_) => return Err(ConnectionFailure::Timeout),
    };
    let handshake = async {
        let (mut send, recv) = connection
            .open_bi()
            .await
            .map_err(|_| ConnectionFailure::Unreachable)?;
        wire::write_line(
            &mut send,
            &ClientLine::Hello {
                protocol_version: tcode_protocol::PROTOCOL_VERSION,
                device: device.claim(),
            },
        )
        .await
        .map_err(|_| ConnectionFailure::Unreachable)?;
        let mut reader = wire::reader(recv);
        let reply = match wire::read_control::<HostLine>(&mut reader).await {
            Ok(reply) => reply,
            Err(error) => {
                return Err(connection
                    .close_reason()
                    .map(|reason| close_failure(&reason))
                    .unwrap_or_else(|| {
                        if error.kind() == io::ErrorKind::TimedOut {
                            ConnectionFailure::Timeout
                        } else {
                            ConnectionFailure::Unreachable
                        }
                    }));
            }
        };
        match reply {
            HostLine::HelloOk {
                protocol_version, ..
            } if protocol_version == tcode_protocol::PROTOCOL_VERSION => Ok((send, reader)),
            HostLine::HelloOk { .. }
            | HostLine::HelloRejected {
                reason: HelloRejection::Protocol,
            } => Err(ConnectionFailure::ProtocolMismatch),
            HostLine::HelloRejected {
                reason: HelloRejection::Unpaired,
            } => Err(ConnectionFailure::AuthenticationRejected),
            _ => Err(ConnectionFailure::Unreachable),
        }
    };
    match handshake.await {
        Ok((send, reader)) => Ok(Established {
            connection,
            send,
            reader,
        }),
        Err(failure) => {
            connection.close(0_u32.into(), b"hello failed");
            Err(failure)
        }
    }
}

/// Why the machine closed the connection, in the client's terms.
fn close_failure(reason: &ConnectionError) -> ConnectionFailure {
    match reason {
        ConnectionError::ApplicationClosed(close)
            if close.error_code == CLOSE_UNPAIRED.into()
                || close.error_code == CLOSE_REVOKED.into() =>
        {
            ConnectionFailure::AuthenticationRejected
        }
        ConnectionError::ApplicationClosed(_) | ConnectionError::ConnectionClosed(_) => {
            ConnectionFailure::HostClosed
        }
        ConnectionError::TimedOut => ConnectionFailure::Timeout,
        _ => ConnectionFailure::Unreachable,
    }
}

/// Remember how this connection actually reached the machine, ahead of the
/// hints the invite carried.
fn learn_addresses(host: &mut PairedHost, connection: &Connection) {
    let paths = connection.paths();
    for path in paths.iter() {
        match path.remote_addr() {
            TransportAddr::Ip(addr) => {
                let addr = addr.to_string();
                host.addrs.retain(|known| *known != addr);
                host.addrs.insert(0, addr);
            }
            TransportAddr::Relay(url) => host.relay = Some(url.to_string()),
            _ => {}
        }
    }
    host.addrs.truncate(MAX_ADDRS);
}

fn persist_addresses(device: &DeviceIdentity, host: &PairedHost) {
    let result = crate::hosts::update_hosts(device.data_dir(), |hosts| {
        if let Some(saved) = hosts.iter_mut().find(|saved| saved.host_id == host.host_id) {
            saved.relay = host.relay.clone();
            saved.addrs = host.addrs.clone();
        }
    });
    if let Err(error) = result {
        log::error!("could not persist the machine's addresses: {error}");
    }
}

/// Why a healthy link ended and how the loop should continue.
struct Lost {
    failure: ConnectionFailure,
    /// The link carried at least one host message.
    healthy: bool,
    /// Retry at once rather than after backoff.
    wake: Option<Wake>,
}

/// ID zero is reserved for transport probes; `HostLink` starts at one.
fn ping_line() -> String {
    tcode_protocol::encode_line(&tcode_protocol::ClientMessage {
        key: None,
        id: 0,
        payload: tcode_protocol::ClientPayload::Query(tcode_protocol::Query::Ping),
    })
    .expect("heartbeat serialization")
}

fn now_ms() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

async fn relay_connected(
    established: Established,
    outgoing: &Arc<OutgoingReceiver>,
    incoming: &Sender<String>,
    state: &Sender<ConnectionState>,
    subscriptions: &mut HashMap<String, String>,
    buffered: &mut VecDeque<String>,
) -> Lost {
    let Established {
        connection,
        mut send,
        mut reader,
    } = established;
    // Reads and writes run in their own tasks: a line read or written
    // halfway must never be abandoned by a select branch.
    let (lines_tx, lines_rx) = async_channel::unbounded::<String>();
    let reader_task = tokio::spawn(async move {
        loop {
            match wire::read_line(&mut reader, wire::MAX_LINE).await {
                Ok(Some(line)) => {
                    if lines_tx.send(line).await.is_err() {
                        return Ok(());
                    }
                }
                Ok(None) => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    });
    let (wire_tx, wire_rx) = async_channel::unbounded::<String>();
    let writer_outgoing = outgoing.clone();
    let writer_task = tokio::spawn(async move {
        let mut unsent = Vec::new();
        while let Ok(line) = wire_rx.recv().await {
            if wire::write_raw_line(&mut send, &line).await.is_err() {
                unsent.push(line);
                break;
            }
            if subscription_key(&line).is_none() {
                writer_outgoing.sent(&line);
            }
        }
        while let Ok(line) = wire_rx.try_recv() {
            unsent.push(line);
        }
        unsent
    });
    for line in subscriptions.values() {
        let _ = wire_tx.send(line.clone()).await;
    }
    while let Some(line) = buffered.pop_front() {
        if subscription_key(&line).is_none() {
            let _ = wire_tx.send(line).await;
        }
    }
    let mut heartbeat = Heartbeat::with_idle(now_ms(), NATIVE_IDLE_MS);
    let mut healthy = false;
    let mut connected = false;
    let lost = loop {
        let wait = match heartbeat.tick(now_ms()) {
            Tick::Wait(ms) => ms,
            Tick::Ping => {
                if wire_tx.send(ping_line()).await.is_err() {
                    break Lost {
                        failure: ConnectionFailure::Unreachable,
                        healthy,
                        wake: None,
                    };
                }
                LIVENESS_REPLY_MS
            }
            Tick::Lost => {
                break Lost {
                    failure: ConnectionFailure::Timeout,
                    healthy,
                    wake: None,
                };
            }
        };
        tokio::select! {
            line = outgoing.recv() => match line {
                Ok(line) => {
                    if let Some(key) = subscription_key(&line) {
                        subscriptions.insert(key, line.clone());
                    }
                    if wire_tx.send(line).await.is_err() {
                        break Lost { failure: ConnectionFailure::Unreachable, healthy, wake: None };
                    }
                }
                Err(_) => break Lost { failure: ConnectionFailure::HostClosed, healthy, wake: None },
            },
            line = lines_rx.recv() => match line {
                Ok(line) => {
                    heartbeat.received(now_ms());
                    healthy = true;
                    if !connected {
                        connected = true;
                        let _ = state.send(ConnectionState::Connected).await;
                    }
                    if incoming.send(format!("{line}\n")).await.is_err() {
                        break Lost { failure: ConnectionFailure::HostClosed, healthy, wake: None };
                    }
                }
                Err(_) => {
                    let failure = match connection.close_reason() {
                        Some(reason) => close_failure(&reason),
                        None => ConnectionFailure::HostClosed,
                    };
                    break Lost { failure, healthy, wake: None };
                }
            },
            reason = connection.closed() => {
                break Lost { failure: close_failure(&reason), healthy, wake: None };
            }
            _ = tokio::time::sleep(Duration::from_millis(wait)) => {}
            wake = outgoing.wake.recv() => match wake {
                Ok(Wake::Reconnect) => {
                    break Lost { failure: ConnectionFailure::Unreachable, healthy, wake: Some(Wake::Reconnect) };
                }
                // Probe now: rewind the idle window so the next tick pings.
                Ok(Wake::Probe) => {
                    heartbeat = Heartbeat::with_idle(now_ms().saturating_sub(NATIVE_IDLE_MS), NATIVE_IDLE_MS);
                }
                Err(_) => break Lost { failure: ConnectionFailure::HostClosed, healthy, wake: None },
            },
        }
    };
    connection.close(0_u32.into(), b"reconnecting");
    reader_task.abort();
    drop(wire_tx);
    if let Ok(unsent) = writer_task.await {
        for line in unsent.into_iter().rev() {
            if subscription_key(&line).is_none() {
                buffered.push_front(line);
            }
        }
    }
    lost
}

/// Queue writes while waiting for `deadline`. Ends early on a wake.
async fn buffer_during_backoff(
    outgoing: &OutgoingReceiver,
    buffered: &mut VecDeque<String>,
    subscriptions: &mut HashMap<String, String>,
    deadline: tokio::time::Instant,
) {
    let done = tokio::time::sleep_until(deadline);
    tokio::pin!(done);
    loop {
        tokio::select! {
            line = outgoing.recv() => match line {
                Ok(line) => {
                    if let Some(key) = subscription_key(&line) {
                        subscriptions.insert(key, line);
                    } else {
                        buffered.push_back(line);
                    }
                }
                Err(_) => return,
            },
            _ = &mut done => return,
            _ = outgoing.wake.recv() => return,
        }
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
