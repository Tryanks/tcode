//! The device side: one iroh endpoint per [`DeviceIdentity`], pairing over
//! `tcode/pair/1` and a reconnecting main-stream [`Transport`] over `tcode/1`.
use std::{
    collections::{HashMap, VecDeque},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::{
    identity::DeviceIdentity,
    manifest::{Manifest, ManifestLoader, ManifestSource, live},
    runtime::{block_on, runtime},
    wire::{self, ClientLine, HelloRejection, HostLine, LineReader, PairRejection},
};
use async_channel::Sender;
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayMap, RelayUrl, TransportAddr,
    endpoint::{Connection, ConnectionError, SendStream, presets},
};
use tcode_client::{
    ConnectionFailure, ConnectionState,
    heartbeat::{Heartbeat, LIVENESS_REPLY_MS, NATIVE_IDLE_MS, Tick},
    host::{LiveHost, Transport, Tunnel, TunnelFuture, TunnelOpener},
    outgoing::{Outgoing, OutgoingReceiver, subscription_key},
    pairing::{MAX_ADDRS, PairInvite, PairedHost},
    recovery::{Backoff, Wake},
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
    lookups: Arc<Lookups>,
}

/// The Traverse instances the endpoint resolves machines through and relays
/// from: one per instance a saved machine publishes to, plus the instance of
/// an invitation being paired right now. The relay map is the union of their
/// manifests, so a device whose machines are all Off carries no relay and a
/// device with no machine on the official service never loads its manifest.
/// The device only resolves; it never publishes.
struct Lookups {
    endpoint: Endpoint,
    data_dir: PathBuf,
    sources: Mutex<HashMap<ManifestSource, Source>>,
    /// The relay map the endpoint was bound with. iroh shares it with the
    /// endpoint, so it always reads as what the endpoint dials from; the
    /// lock serializes syncs so two cannot undo each other's changes.
    relays: tokio::sync::Mutex<RelayMap>,
}

struct Source {
    loader: ManifestLoader,
    /// `None` while the first fetch is in flight.
    manifest: Option<Arc<Manifest>>,
    /// Pairings in flight through this instance; they keep a source no
    /// saved machine names yet.
    pinned: usize,
    refresher: Option<tokio::task::AbortHandle>,
}

impl Drop for Source {
    fn drop(&mut self) {
        if let Some(refresher) = &self.refresher {
            refresher.abort();
        }
    }
}

/// The instances the saved machines publish to.
fn saved_sources(data_dir: &Path) -> Vec<ManifestSource> {
    let hosts = crate::hosts::load_hosts(data_dir).unwrap_or_else(|error| {
        log::error!("could not read hosts.json: {error}");
        Vec::new()
    });
    let mut sources = Vec::new();
    for host in &hosts {
        if let Some(source) = ManifestSource::from_traverse(host.traverse.as_deref())
            && !sources.contains(&source)
        {
            sources.push(source);
        }
    }
    sources
}

impl Lookups {
    fn manifests(&self) -> Vec<Arc<Manifest>> {
        self.sources
            .lock()
            .unwrap()
            .values()
            .filter_map(|source| source.manifest.clone())
            .collect()
    }

    /// Bring the endpoint in line with the manifests in hand: the relay map
    /// becomes their union and the lookup services their resolvers.
    async fn apply(&self) {
        let relays = self.relays.lock().await;
        let manifests = self.manifests();
        let wanted = RelayMap::empty();
        for manifest in &manifests {
            wanted.extend(&manifest.relay_map());
        }
        live::install_lookups(&self.endpoint, manifests.iter().map(Arc::as_ref), false);
        live::sync_relays(&self.endpoint, &relays, &wanted).await;
    }

    /// Make `source` resolvable: the manifest in hand is installed at once
    /// and refreshed in the background; with nothing in hand yet, one fetch
    /// is awaited. A manifest that stays unavailable is logged and the
    /// source dropped, so the next dial fetches again: the stored relay and
    /// addresses may still reach the machine. `pin` marks a pairing in
    /// flight, released with [`Self::unpin`].
    async fn ensure(self: &Arc<Self>, source: &ManifestSource, pin: bool) {
        let loader = {
            let mut sources = self.sources.lock().unwrap();
            if let Some(known) = sources.get_mut(source) {
                known.pinned += usize::from(pin);
                // Installed, or claimed by a fetch already under way.
                return;
            }
            // Claim the source before awaiting, so a concurrent dial to
            // the same instance does not fetch twice.
            let loader = ManifestLoader::new(source.clone(), &self.data_dir);
            sources.insert(
                source.clone(),
                Source {
                    loader: loader.clone(),
                    manifest: None,
                    pinned: usize::from(pin),
                    refresher: None,
                },
            );
            loader
        };
        match loader.startup().await {
            Ok(manifest) => self.adopt(source, manifest).await,
            Err(error) => {
                log::warn!("Traverse manifest unavailable: {error}");
                self.sources.lock().unwrap().remove(source);
            }
        }
    }

    fn unpin(&self, source: &ManifestSource) {
        if let Some(known) = self.sources.lock().unwrap().get_mut(source) {
            known.pinned = known.pinned.saturating_sub(1);
        }
    }

    /// Install `manifest` for a claimed `source` now and each time a
    /// refresh changes it.
    async fn adopt(self: &Arc<Self>, source: &ManifestSource, manifest: Arc<Manifest>) {
        {
            let mut sources = self.sources.lock().unwrap();
            let Some(known) = sources.get_mut(source) else {
                return;
            };
            known.manifest = Some(manifest);
            let lookups = self.clone();
            let refreshed = source.clone();
            known.refresher = Some(known.loader.spawn_refresh(move |manifest| {
                let lookups = lookups.clone();
                let source = refreshed.clone();
                async move {
                    log::info!("applying the refreshed Traverse manifest for {source:?}");
                    let installed = match lookups.sources.lock().unwrap().get_mut(&source) {
                        Some(known) => {
                            known.manifest = Some(manifest);
                            true
                        }
                        None => false,
                    };
                    if installed {
                        lookups.apply().await;
                    }
                }
            }));
        }
        self.apply().await;
    }

    /// The saved machines changed: drop the instances none of them and no
    /// pairing in flight names, and load the ones that are new.
    async fn reconcile(self: &Arc<Self>) {
        let wanted = saved_sources(&self.data_dir);
        let dropped = {
            let mut sources = self.sources.lock().unwrap();
            let before = sources.len();
            sources.retain(|source, known| known.pinned > 0 || wanted.contains(source));
            sources.len() != before
        };
        if dropped {
            self.apply().await;
        }
        for source in &wanted {
            self.ensure(source, false).await;
        }
    }
}

impl DeviceIdentity {
    /// The endpoint, bound on first use with the relays of the saved
    /// machines' Traverse instances. Must run on the runtime.
    pub(crate) async fn client(&self) -> io::Result<&ClientEndpoint> {
        let inner = self.inner();
        inner
            .endpoint
            .get_or_try_init(|| async {
                let secret_key = inner.secret_key.clone();
                // Manifests in hand shape the endpoint at bind; the rest are
                // fetched once it is up.
                let mut in_hand = Vec::new();
                let mut to_fetch = Vec::new();
                let relays = RelayMap::empty();
                for source in saved_sources(&inner.data_dir) {
                    let loader = ManifestLoader::new(source.clone(), &inner.data_dir);
                    match loader.current() {
                        Ok(manifest) => {
                            relays.extend(&manifest.relay_map());
                            in_hand.push((source, loader, manifest));
                        }
                        Err(_) => to_fetch.push(source),
                    }
                }
                let endpoint = Endpoint::builder(presets::Minimal)
                    .relay_mode(iroh::RelayMode::Custom(relays.clone()))
                    .secret_key(secret_key)
                    .transport_config(wire::transport_config())
                    .bind()
                    .await
                    .map_err(io::Error::other)?;
                let lookups = Arc::new(Lookups {
                    endpoint: endpoint.clone(),
                    data_dir: inner.data_dir.clone(),
                    sources: Mutex::new(HashMap::new()),
                    relays: tokio::sync::Mutex::new(relays),
                });
                for (source, loader, manifest) in in_hand {
                    lookups.sources.lock().unwrap().insert(
                        source.clone(),
                        Source {
                            loader,
                            manifest: None,
                            pinned: 0,
                            refresher: None,
                        },
                    );
                    lookups.adopt(&source, manifest).await;
                }
                for source in to_fetch {
                    let lookups = lookups.clone();
                    runtime().spawn(async move { lookups.ensure(&source, false).await });
                }
                Ok(ClientEndpoint {
                    endpoint,
                    transports: Mutex::new(Vec::new()),
                    lookups,
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

    /// `hosts.json` changed: the endpoint's relays and lookups follow the
    /// machines saved now. Before the endpoint is bound there is nothing to
    /// update; it reads the file when it binds.
    pub fn hosts_changed(&self) {
        let Some(client) = self.inner().endpoint.get() else {
            return;
        };
        let lookups = client.lookups.clone();
        runtime().spawn(async move { lookups.reconcile().await });
    }

    /// The relay URLs the endpoint dials from right now; empty before the
    /// endpoint is bound.
    pub fn relays(&self) -> Vec<String> {
        let Some(client) = self.inner().endpoint.get() else {
            return Vec::new();
        };
        let relays = block_on(async { client.lookups.relays.lock().await.clone() });
        let mut urls: Vec<String> = relays
            .urls::<Vec<_>>()
            .iter()
            .map(ToString::to_string)
            .collect();
        urls.sort();
        urls
    }
}

impl ClientEndpoint {
    /// Make the machine's Traverse instance resolvable before dialing it.
    async fn ensure_lookups(&self, traverse: Option<&str>) {
        if let Some(source) = ManifestSource::from_traverse(traverse) {
            self.lookups.ensure(&source, false).await;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairError {
    /// Wrong, expired or already used invitation.
    Invalid,
    Disabled,
    Busy,
    Unreachable(String),
    /// The machine answered with something other than a pairing reply.
    Protocol(String),
}

impl std::fmt::Display for PairError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid => f.write_str("invalid or expired invitation"),
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

/// Exchange the invitation's secret for a pairing with exactly
/// `invite.host_id`.
pub async fn pair(invite: &PairInvite, device: &DeviceIdentity) -> Result<PairedHost, PairError> {
    let client = device
        .client()
        .await
        .map_err(|error| PairError::Unreachable(error.to_string()))?;
    let addr = dial_addr(&invite.host_id, invite.relay.as_deref(), &invite.addrs)
        .ok_or_else(|| PairError::Protocol("invalid machine id".into()))?;
    // The machine's instance is held for the exchange; the caller saves the
    // machine on success, which keeps it, and a failure lets it go again.
    let source = ManifestSource::from_traverse(invite.traverse.as_deref());
    if let Some(source) = &source {
        client.lookups.ensure(source, true).await;
    }
    let result = pair_exchange(client, addr, invite, device).await;
    if let Some(source) = &source {
        client.lookups.unpin(source);
        if result.is_err() {
            client.lookups.reconcile().await;
        }
    }
    result
}

async fn pair_exchange(
    client: &ClientEndpoint,
    addr: EndpointAddr,
    invite: &PairInvite,
    device: &DeviceIdentity,
) -> Result<PairedHost, PairError> {
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
                secret: invite.secret.clone(),
                device: device.claim(),
            },
        )
        .await
        .map_err(|error| PairError::Unreachable(error.to_string()))?;
        send.finish()
            .map_err(|error| PairError::Unreachable(error.to_string()))?;
        let mut reader = wire::reader(recv);
        // Never resubmit a possibly consumed invitation after losing its
        // response.
        match wire::read_control::<HostLine>(&mut reader)
            .await
            .map_err(|error| PairError::Protocol(error.to_string()))?
        {
            HostLine::Paired { host_name } => Ok(invite.paired(host_name)),
            HostLine::PairRejected { reason } => Err(match reason {
                PairRejection::Invalid => PairError::Invalid,
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
    device: &DeviceIdentity,
) -> Result<PairedHost, PairError> {
    block_on(pair(invite, device))
}

/// Preview tunnels of one attachment: opened on whichever connection the
/// main stream currently runs on, and none while it is reconnecting. The
/// transport publishes it through `LiveHost::tunnels` on its `current_host`.
#[derive(Clone, Default)]
pub struct AttachmentTunnels {
    current: Arc<Mutex<Option<Connection>>>,
}

impl AttachmentTunnels {
    fn set(&self, connection: Option<Connection>) {
        *self.current.lock().unwrap() = connection;
    }
}

impl TunnelOpener for AttachmentTunnels {
    fn open(&self, host: &str, port: u16) -> TunnelFuture {
        let current = self.current.lock().unwrap().clone();
        let host = host.to_owned();
        Box::pin(async move {
            let connection = current.ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotConnected, "not connected to the machine")
            })?;
            // The handshake needs the runtime's timers; the caller may be on
            // any executor.
            let (done, result) = async_channel::bounded::<io::Result<Tunnel>>(1);
            runtime().spawn(async move {
                let _ = done
                    .send(crate::tunnel::open(&connection, &host, port).await)
                    .await;
            });
            result
                .recv()
                .await
                .unwrap_or_else(|_| Err(io::Error::other("tunnel opener stopped")))
        })
    }
}

/// Open a reconnecting link to `host`. Dropping the returned channels ends
/// it. The stored relay and addresses are refreshed in `hosts.json` after
/// each authenticated connection. The transport's `current_host` carries
/// the attachment's [`AttachmentTunnels`].
pub fn connect(host: &PairedHost, device: &DeviceIdentity) -> Transport {
    let (to_host, outgoing) = tcode_client::outgoing::channel();
    let (incoming, from_host) = async_channel::unbounded();
    let (state_tx, state) = async_channel::unbounded();
    let tunnels = AttachmentTunnels::default();
    let current_host = LiveHost::with_tunnels(host.clone(), Arc::new(tunnels.clone()));
    let live = current_host.clone();
    let device = device.clone();
    let registered = to_host.clone();
    runtime().spawn(async move {
        if let Ok(client) = device.client().await {
            client.transports.lock().unwrap().push(registered);
        }
        connection_loop(
            device,
            live,
            tunnels,
            Arc::new(outgoing),
            incoming,
            StateSender::new(state_tx),
        )
        .await;
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

/// The state channel, remembering whether the last state it carried was
/// `Connected`. A path change is published by repeating `Connected`, which
/// is the only wake the UI has and is idempotent for it; repeating any other
/// state would restart a sync, so before the first host line it stays quiet.
struct StateSender {
    tx: Sender<ConnectionState>,
    connected: std::sync::atomic::AtomicBool,
}

impl StateSender {
    fn new(tx: Sender<ConnectionState>) -> Self {
        Self {
            tx,
            connected: std::sync::atomic::AtomicBool::new(false),
        }
    }

    async fn send(
        &self,
        state: ConnectionState,
    ) -> Result<(), async_channel::SendError<ConnectionState>> {
        self.connected.store(
            state == ConnectionState::Connected,
            std::sync::atomic::Ordering::Relaxed,
        );
        self.tx.send(state).await
    }

    async fn republish_connected(&self) {
        if self.connected.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = self.tx.send(ConnectionState::Connected).await;
        }
    }
}

/// Keep `live` telling the truth about how the connection is carried: the
/// selected path at connect time, then every change until the connection
/// closes.
async fn watch_paths(connection: Connection, live: LiveHost, state: Arc<StateSender>) {
    use futures_lite::StreamExt as _;
    let mut events = connection.path_events();
    live.set_path(Some(crate::host::path_info(&connection)));
    while events.next().await.is_some() {
        if live.set_path(Some(crate::host::path_info(&connection))) {
            state.republish_connected().await;
        }
    }
}

async fn connection_loop(
    device: DeviceIdentity,
    live: LiveHost,
    tunnels: AttachmentTunnels,
    outgoing: Arc<OutgoingReceiver>,
    incoming: Sender<String>,
    state: StateSender,
) {
    let state = Arc::new(state);
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
                // Tunnels are available by the time Syncing is observable.
                tunnels.set(Some(established.connection.clone()));
                let _ = state.send(ConnectionState::Syncing).await;
                let paths = tokio::spawn(watch_paths(
                    established.connection.clone(),
                    live.clone(),
                    state.clone(),
                ));
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
                paths.abort();
                live.set_path(None);
                tunnels.set(None);
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
    client.ensure_lookups(host.traverse.as_deref()).await;
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
    state: &StateSender,
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
        let ping = ping_line();
        while let Ok(line) = wire_rx.recv().await {
            if wire::write_raw_line(&mut send, &line).await.is_err() {
                unsent.push(line);
                break;
            }
            // The heartbeat is the transport's own line: the outbox never
            // charged for it, so it must not be credited either.
            if line != ping && subscription_key(&line).is_none() {
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
