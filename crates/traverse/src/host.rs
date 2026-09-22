//! The machine side: one iroh endpoint that pairs devices over `tcode/pair/1`
//! and serves paired devices over `tcode/1`, bridging each main stream into
//! the [`HostMux`].
use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use iroh::{
    Endpoint, EndpointId, RelayMode, TransportAddr,
    endpoint::{Connection, SendStream, presets},
    protocol::{AcceptError, ProtocolHandler, Router},
};
use tcode_client::pairing::{PairInvite, TRAVERSE_OFF, encode_secret, pair_url};
use tcode_protocol::{HostedDevice, HostingAction, HostingState, PathInfo, ProtocolError};
use url::Url;

use crate::{
    identity::HostIdentity,
    lan,
    manifest::{ManifestLoader, ManifestSource, live},
    mux::HostMux,
    runtime::block_on,
    wire::{self, ClientLine, DeviceClaim, HelloRejection, HostLine, LineReader, PairRejection},
};

pub const INVITATION_LIFETIME: Duration = Duration::from_secs(5 * 60);
/// Wrong secrets an invitation survives; a cheap defence in depth behind
/// 128 bits of entropy.
pub const MAX_PAIRING_FAILURES: u8 = 5;

/// Which Traverse instance this machine publishes to. Official and custom
/// are the same mechanism with a different [`ManifestSource`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraverseMode {
    /// The official service: the bundled manifest, refreshed from the
    /// repository.
    Official,
    /// A self-hosted instance described by its manifest; see
    /// [`crate::manifest`] for how the manifest is obtained.
    Custom(Url),
    /// No relay and no wide-area lookup: the addresses in hand and the LAN
    /// lookup only.
    Off,
}

pub struct HostConfig {
    pub host_name: String,
    pub data_dir: PathBuf,
    pub traverse: TraverseMode,
    /// Whether this host may pair devices at all. The user's persisted
    /// pairing switch applies on top of it.
    pub pairing_enabled: bool,
    /// A fixed UDP port instead of a random one, so invite addresses and
    /// firewall rules survive restarts.
    pub bind_port: Option<u16>,
}

/// A minted invitation: the link is the secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invitation {
    pub invite: PairInvite,
    pub expires_at: Instant,
}

impl Invitation {
    pub fn remaining(&self) -> Duration {
        self.expires_at.saturating_duration_since(Instant::now())
    }

    /// The `tcode://pair?…` link to scan or paste.
    pub fn url(&self) -> String {
        pair_url(&self.invite)
    }
}

/// One paired device, as shown by the hosting UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub platform: Option<String>,
    pub created_unix: u64,
    /// How the device reaches this machine while connected; `None` offline.
    pub live: Option<PathInfo>,
}

/// This machine's addresses at one moment, for invites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointAddrSnapshot {
    pub id: String,
    pub relays: Vec<String>,
    pub addrs: Vec<String>,
}

struct ActiveInvitation {
    invitation: Invitation,
    failures: u8,
}

/// Everything revocation and pairing must see atomically: the allow list,
/// the active invitation and the connections currently admitted.
struct State {
    identity: HostIdentity,
    invitation: Option<ActiveInvitation>,
    live: HashMap<EndpointId, Vec<Connection>>,
}

struct Shared {
    endpoint: Endpoint,
    mux: HostMux,
    state: Mutex<State>,
    /// Told the invitation in effect after every change; see
    /// [`TraverseHost::invitation_events`].
    listeners: Mutex<Vec<async_channel::Sender<Option<Invitation>>>>,
    /// Ends the active invitation when its lifetime runs out.
    expiry: Mutex<Option<tokio::task::AbortHandle>>,
    /// What invitations say about this machine's Traverse instance; see
    /// [`PairInvite::traverse`].
    traverse: Option<String>,
    allow_pairing: bool,
}

pub struct TraverseHost {
    shared: Arc<Shared>,
    router: Router,
    /// The manifest refresh loop, ended with the host.
    refresh: Option<tokio::task::AbortHandle>,
    /// This machine's DNS-SD record, withdrawn with the host.
    advertisement: Option<lan::Advertisement>,
}

impl TraverseHost {
    /// Bind the endpoint and start accepting connections.
    pub fn start(mux: HostMux, config: HostConfig) -> io::Result<TraverseHost> {
        let identity = HostIdentity::load_or_create(&config.data_dir, &config.host_name)?;
        let secret_key = identity.secret_key().clone();
        let traverse = match &config.traverse {
            TraverseMode::Official => None,
            TraverseMode::Custom(url) => Some(url.to_string()),
            TraverseMode::Off => Some(TRAVERSE_OFF.to_owned()),
        };
        block_on(async move {
            let loader = match &config.traverse {
                TraverseMode::Official => Some(ManifestLoader::new(
                    ManifestSource::Official,
                    &config.data_dir,
                )),
                TraverseMode::Custom(base) => Some(ManifestLoader::new(
                    ManifestSource::Custom(base.clone()),
                    &config.data_dir,
                )),
                TraverseMode::Off => None,
            };
            // The copy in hand starts the endpoint; only a self-hosted
            // instance with nothing cached waits for one fetch.
            let manifest = match &loader {
                Some(loader) => Some(loader.startup().await?),
                None => None,
            };
            let build = |ipv6: bool| -> io::Result<iroh::endpoint::Builder> {
                let mut builder = match &manifest {
                    Some(manifest) => Endpoint::builder(presets::Minimal)
                        .relay_mode(RelayMode::Custom(manifest.relay_map())),
                    None => Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Disabled),
                };
                builder = builder
                    .secret_key(secret_key.clone())
                    .transport_config(wire::transport_config());
                if let Some(port) = config.bind_port {
                    let v4 = std::net::SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, port));
                    let v6 = std::net::SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port));
                    builder = builder
                        .clear_ip_transports()
                        .bind_addr(v4)
                        .map_err(io::Error::other)?;
                    if ipv6 {
                        builder = builder.bind_addr(v6).map_err(io::Error::other)?;
                    }
                }
                Ok(builder)
            };
            let endpoint = match build(true)?.bind().await {
                Ok(endpoint) => endpoint,
                // A fixed port binds both families; fall back to IPv4 alone
                // where IPv6 is unavailable.
                Err(error) if config.bind_port.is_some() => {
                    log::debug!("dual-stack bind failed, trying IPv4 only: {error}");
                    build(false)?.bind().await.map_err(io::Error::other)?
                }
                Err(error) => return Err(io::Error::other(error)),
            };
            if let Some(manifest) = &manifest {
                live::install_lookups(&endpoint, [manifest.as_ref()], true, None);
            }
            let advertisement = advertise(&endpoint, &identity.host_name);
            // A refreshed manifest is applied to the running endpoint:
            // relays through `insert_relay`/`remove_relay`, publishers and
            // resolvers rebuilt on the endpoint's lookup services.
            let refresh = loader.zip(manifest.clone()).map(|(loader, applied)| {
                let endpoint = endpoint.clone();
                let applied = Mutex::new(applied);
                loader.spawn_refresh(move |manifest| {
                    let endpoint = endpoint.clone();
                    let previous =
                        std::mem::replace(&mut *applied.lock().unwrap(), manifest.clone());
                    async move {
                        log::info!("applying the refreshed Traverse manifest");
                        live::sync_relays(&endpoint, &previous.relay_map(), &manifest.relay_map())
                            .await;
                        live::install_lookups(&endpoint, [manifest.as_ref()], true, None);
                    }
                })
            });
            let shared = Arc::new(Shared {
                endpoint: endpoint.clone(),
                mux,
                state: Mutex::new(State {
                    identity,
                    invitation: None,
                    live: HashMap::new(),
                }),
                listeners: Mutex::new(Vec::new()),
                expiry: Mutex::new(None),
                traverse,
                allow_pairing: config.pairing_enabled,
            });
            let router = Router::builder(endpoint)
                .accept(wire::ALPN_PAIR, PairHandler(shared.clone()))
                .accept(wire::ALPN_MAIN, MainHandler(shared.clone()))
                .spawn();
            Ok(TraverseHost {
                shared,
                router,
                refresh,
                advertisement,
            })
        })
    }

    pub fn endpoint_id(&self) -> String {
        self.shared.endpoint.id().to_string()
    }

    /// Wait until the endpoint has a home relay, up to `timeout`. Only worth
    /// calling before minting an invite that should work off the LAN; with
    /// no relay configured or no route to one it simply returns at `timeout`.
    pub fn wait_online(&self, timeout: Duration) {
        let endpoint = self.shared.endpoint.clone();
        block_on(async move {
            let _ = tokio::time::timeout(timeout, endpoint.online()).await;
        });
    }

    /// Relay URLs and direct addresses right now. The loopback address of
    /// each bound socket is included so a client on this machine can dial
    /// without any lookup.
    pub fn addr(&self) -> EndpointAddrSnapshot {
        self.shared.snapshot()
    }

    /// Mint an invitation, replacing any active one.
    pub fn new_invitation(&self) -> Invitation {
        self.shared.mint()
    }

    /// The active invitation and its remaining lifetime, carrying where the
    /// machine is reachable right now: the endpoint may have found its
    /// relay since the mint. `None` while pairing is disabled.
    pub fn invitation(&self) -> Option<(Invitation, Duration)> {
        let addr = self.shared.snapshot();
        let state = self.shared.state.lock().unwrap();
        self.shared.current_invitation(&state, &addr)
    }

    /// Every change to the invitation, carrying what is in effect after it:
    /// the invitation minted, or `None` once it is used, expires or pairing
    /// is turned off. Each call gets its own stream of every later change.
    pub fn invitation_events(&self) -> async_channel::Receiver<Option<Invitation>> {
        let (sender, receiver) = async_channel::unbounded();
        self.shared.listeners.lock().unwrap().push(sender);
        receiver
    }

    pub fn pairing_enabled(&self) -> bool {
        self.shared.allow_pairing && self.shared.state.lock().unwrap().identity.pairing_enabled
    }

    pub fn set_pairing_enabled(&self, enabled: bool) {
        self.shared.set_pairing_enabled(enabled);
    }

    /// Paired devices, oldest first, with how each connected one reaches us.
    pub fn devices(&self) -> Vec<DeviceInfo> {
        self.shared.devices()
    }

    /// Remove a device and close every connection it holds. A new
    /// connection from it is refused from this call on. When the allow list
    /// cannot be written the device stays paired and connected, so a
    /// revocation never appears to have happened without lasting.
    pub fn revoke(&self, id: &str) -> io::Result<()> {
        self.shared.revoke(id)
    }

    /// Answer a hosting query from a client of any transport.
    pub fn hosting(&self, action: HostingAction) -> Result<HostingState, ProtocolError> {
        self.shared.hosting(action)
    }

    pub fn shutdown(self) {
        drop(self.advertisement);
        if let Some(refresh) = self.refresh {
            refresh.abort();
        }
        if let Some(expiry) = self.shared.expiry.lock().unwrap().take() {
            expiry.abort();
        }
        let router = self.router;
        block_on(async move {
            let _ = router.shutdown().await;
        });
    }
}

/// Advertise the endpoint's UDP port on the LAN; see [`lan`]. Without a
/// port shared by both address families only IPv4 is advertised, since one
/// SRV record carries one port. Unavailable mDNS is logged, not fatal.
fn advertise(endpoint: &Endpoint, host_name: &str) -> Option<lan::Advertisement> {
    let bound = endpoint.bound_sockets();
    let v4 = bound
        .iter()
        .find(|addr| addr.is_ipv4())
        .map(|addr| addr.port());
    let v6 = bound
        .iter()
        .find(|addr| addr.is_ipv6())
        .map(|addr| addr.port());
    let port = v4.or(v6)?;
    let ipv6 = v6 == Some(port);
    match lan::Advertisement::start(&endpoint.id(), host_name, port, ipv6) {
        Ok(advertisement) => Some(advertisement),
        Err(error) => {
            log::warn!("not advertising this machine on the LAN: {error}");
            None
        }
    }
}

impl Shared {
    fn mint(self: &Arc<Self>) -> Invitation {
        let mut random = [0_u8; tcode_client::pairing::SECRET_BYTES];
        getrandom::fill(&mut random).expect("the OS random source is available");
        let addr = self.snapshot();
        let mut state = self.state.lock().unwrap();
        let invitation = Invitation {
            invite: PairInvite {
                host_id: addr.id,
                name: state.identity.host_name.clone(),
                secret: encode_secret(&random),
                traverse: self.traverse.clone(),
                relay: addr.relays.first().cloned(),
                addrs: addr.addrs,
            },
            expires_at: Instant::now() + INVITATION_LIFETIME,
        };
        state.invitation = Some(ActiveInvitation {
            invitation: invitation.clone(),
            failures: 0,
        });
        // The timer holds no reference that would keep a stopped host alive,
        // and checks it is still ending the invitation it was set for.
        let shared = Arc::downgrade(self);
        let secret = invitation.invite.secret.clone();
        let expires_at = invitation.expires_at + Duration::from_millis(10);
        let expiry = crate::runtime::runtime().spawn(async move {
            tokio::time::sleep_until(expires_at.into()).await;
            if let Some(shared) = shared.upgrade() {
                shared.expire(&secret);
            }
        });
        if let Some(previous) = self.expiry.lock().unwrap().replace(expiry.abort_handle()) {
            previous.abort();
        }
        self.notify(Some(invitation.clone()));
        invitation
    }

    /// The invitation `secret` ran out of time.
    fn expire(&self, secret: &str) {
        let mut state = self.state.lock().unwrap();
        let expired = state.invitation.as_ref().is_some_and(|active| {
            active.invitation.invite.secret == secret && active.invitation.remaining().is_zero()
        });
        if expired {
            state.invitation = None;
            self.notify(None);
        }
    }

    /// Under the state lock, so listeners hear changes in the order they
    /// happened.
    fn notify(&self, invitation: Option<Invitation>) {
        self.listeners
            .lock()
            .unwrap()
            .retain(|listener| listener.try_send(invitation.clone()).is_ok());
    }

    /// The unexpired invitation with `addr` as its routing hints. The
    /// secret never changes; only where the link says to dial does.
    fn current_invitation(
        &self,
        state: &State,
        addr: &EndpointAddrSnapshot,
    ) -> Option<(Invitation, Duration)> {
        if !self.allow_pairing || !state.identity.pairing_enabled {
            return None;
        }
        let active = state.invitation.as_ref()?;
        let remaining = active.invitation.remaining();
        if remaining.is_zero() {
            return None;
        }
        let invitation = Invitation {
            invite: PairInvite {
                relay: addr.relays.first().cloned(),
                addrs: addr.addrs.clone(),
                ..active.invitation.invite.clone()
            },
            expires_at: active.invitation.expires_at,
        };
        Some((invitation, remaining))
    }

    fn snapshot(&self) -> EndpointAddrSnapshot {
        let endpoint = &self.endpoint;
        let addr = endpoint.addr();
        let mut addrs: Vec<String> = addr.ip_addrs().map(ToString::to_string).collect();
        for bound in endpoint.bound_sockets() {
            let loopback = match bound.ip() {
                std::net::IpAddr::V4(ip) if ip.is_unspecified() => {
                    std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
                }
                std::net::IpAddr::V6(ip) if ip.is_unspecified() => {
                    std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
                }
                ip => ip,
            };
            let loopback = std::net::SocketAddr::new(loopback, bound.port()).to_string();
            if !addrs.contains(&loopback) {
                addrs.push(loopback);
            }
        }
        addrs.truncate(tcode_client::pairing::MAX_ADDRS);
        EndpointAddrSnapshot {
            id: endpoint.id().to_string(),
            relays: addr.relay_urls().map(ToString::to_string).collect(),
            addrs,
        }
    }

    fn set_pairing_enabled(&self, enabled: bool) {
        let mut state = self.state.lock().unwrap();
        state.identity.pairing_enabled = enabled;
        if let Err(error) = state.identity.save() {
            log::error!("could not persist the pairing switch: {error}");
        }
        if !enabled && state.invitation.take().is_some() {
            self.notify(None);
        }
    }

    fn devices(&self) -> Vec<DeviceInfo> {
        let state = self.state.lock().unwrap();
        state
            .identity
            .devices
            .iter()
            .map(|device| {
                let live = device
                    .id
                    .parse::<EndpointId>()
                    .ok()
                    .and_then(|id| state.live.get(&id))
                    .and_then(|connections| connections.first())
                    .map(path_info);
                DeviceInfo {
                    id: device.id.clone(),
                    name: device.name.clone(),
                    platform: device.platform.clone(),
                    created_unix: device.created_unix,
                    live,
                }
            })
            .collect()
    }

    fn revoke(&self, id: &str) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        let previous = state.identity.clone();
        if !state.identity.remove(id) {
            return Ok(());
        }
        if let Err(error) = state.identity.save() {
            log::error!("could not persist the revocation: {error}");
            state.identity = previous;
            return Err(error);
        }
        let closed = id
            .parse::<EndpointId>()
            .ok()
            .and_then(|id| state.live.remove(&id))
            .unwrap_or_default();
        for connection in closed {
            connection.close(3_u32.into(), b"revoked");
        }
        Ok(())
    }

    fn hosting(self: &Arc<Self>, action: HostingAction) -> Result<HostingState, ProtocolError> {
        match action {
            HostingAction::State => {}
            HostingAction::SetEnabled(enabled) => {
                self.set_pairing_enabled(enabled);
                if enabled && self.allow_pairing {
                    self.mint();
                }
            }
            HostingAction::NewInvitation => {
                if self.allow_pairing && self.state.lock().unwrap().identity.pairing_enabled {
                    self.mint();
                }
            }
            HostingAction::RevokeDevice(id) => self.revoke(&id).map_err(revoke_error)?,
        }
        let addr = self.snapshot();
        let state = self.state.lock().unwrap();
        let enabled = self.allow_pairing && state.identity.pairing_enabled;
        let (invite, expires_in_secs) = self
            .current_invitation(&state, &addr)
            .map(|(invitation, remaining)| (Some(invitation.url()), remaining.as_secs()))
            .unwrap_or((None, 0));
        Ok(HostingState {
            enabled,
            expires_in_secs,
            host_id: addr.id,
            host_name: state.identity.host_name.clone(),
            invite,
            devices: state
                .identity
                .devices
                .iter()
                .map(|device| HostedDevice {
                    id: device.id.clone(),
                    name: device.name.clone(),
                    created_unix: device.created_unix,
                    platform: device.platform.clone(),
                    path: device
                        .id
                        .parse::<EndpointId>()
                        .ok()
                        .and_then(|id| state.live.get(&id))
                        .and_then(|connections| connections.first())
                        .map(path_info),
                })
                .collect(),
        })
    }

    /// Exchange an invitation's secret for a place on the allow list.
    /// Expiry, single use and the failure budget are judged under the one
    /// lock every requester shares, and the device is on disk before it is
    /// told so.
    fn pair(&self, remote: EndpointId, secret: &str, device: &DeviceClaim) -> HostLine {
        let mut state = self.state.lock().unwrap();
        if !self.allow_pairing || !state.identity.pairing_enabled {
            return HostLine::PairRejected {
                reason: PairRejection::Disabled,
            };
        }
        let accepted = match state.invitation.as_mut() {
            None => false,
            Some(active) if active.invitation.remaining().is_zero() => {
                state.invitation = None;
                self.notify(None);
                false
            }
            Some(active) => {
                if constant_time_eq(
                    active.invitation.invite.secret.as_bytes(),
                    secret.as_bytes(),
                ) {
                    state.invitation = None;
                    self.notify(None);
                    true
                } else {
                    active.failures += 1;
                    if active.failures >= MAX_PAIRING_FAILURES {
                        state.invitation = None;
                        self.notify(None);
                    }
                    false
                }
            }
        };
        if !accepted {
            return HostLine::PairRejected {
                reason: PairRejection::Invalid,
            };
        }
        let (name, platform) = device.normalized();
        let previous = state.identity.clone();
        state.identity.admit(&remote, name, platform);
        if let Err(error) = state.identity.save() {
            log::error!("could not record the paired device: {error}");
            state.identity = previous;
            return HostLine::PairRejected {
                reason: PairRejection::Busy,
            };
        }
        HostLine::Paired {
            host_name: state.identity.host_name.clone(),
        }
    }

    /// Admit a `tcode/1` connection if its peer is paired, registering it
    /// under the same lock so a concurrent revocation either sees it or
    /// refuses it.
    fn admit(&self, connection: &Connection) -> bool {
        let remote = connection.remote_id();
        let mut state = self.state.lock().unwrap();
        if !state.identity.is_paired(&remote) {
            return false;
        }
        state
            .live
            .entry(remote)
            .or_default()
            .push(connection.clone());
        true
    }

    fn release(&self, connection: &Connection) {
        let mut state = self.state.lock().unwrap();
        if let Some(connections) = state.live.get_mut(&connection.remote_id()) {
            connections.retain(|live| live.stable_id() != connection.stable_id());
            if connections.is_empty() {
                state.live.remove(&connection.remote_id());
            }
        }
    }

    /// The hosting list shows what the device calls itself now. It is
    /// already authenticated, so a failed write is not a reason to refuse.
    fn refresh_device(&self, remote: EndpointId, device: &DeviceClaim) {
        if !device.is_valid() {
            return;
        }
        let mut state = self.state.lock().unwrap();
        if !state.identity.is_paired(&remote) {
            return;
        }
        let (name, platform) = device.normalized();
        let changed = state.identity.devices.iter().any(|record| {
            record.id == remote.to_string() && (record.name != name || record.platform != platform)
        });
        if !changed {
            return;
        }
        state.identity.admit(&remote, name, platform);
        if let Err(error) = state.identity.save() {
            log::warn!("could not record the connecting device's details: {error}");
        }
    }

    fn host_name(&self) -> String {
        self.state.lock().unwrap().identity.host_name.clone()
    }
}

/// A revocation the machine could not record, as the hosting query reports
/// it: the device is still paired.
pub(crate) fn revoke_error(error: io::Error) -> ProtocolError {
    ProtocolError {
        code: "revoke_failed".into(),
        message: format!(
            "the machine could not record the revocation; the device is still paired: {error}"
        ),
    }
}

/// How `connection` is carried right now, judged by its selected path. A
/// relay selected while a direct path is already open is a relay still
/// waiting for that path to prove itself.
pub(crate) fn path_info(connection: &Connection) -> PathInfo {
    let paths = connection.paths();
    let selected = paths
        .iter()
        .find(|path| path.is_selected())
        .or_else(|| paths.iter().next());
    let direct_open = paths
        .iter()
        .any(|path| matches!(path.remote_addr(), TransportAddr::Ip(_)));
    match selected.map(|path| path.remote_addr().clone()) {
        Some(TransportAddr::Ip(addr)) => PathInfo {
            direct: true,
            relay: None,
            lan: on_own_network(addr.ip()),
            probing_direct: false,
        },
        Some(TransportAddr::Relay(url)) => PathInfo {
            direct: false,
            relay: Some(url.to_string()),
            lan: false,
            probing_direct: direct_open,
        },
        _ => PathInfo {
            direct: false,
            relay: None,
            lan: false,
            probing_direct: direct_open,
        },
    }
}

/// Whether a peer at `ip` was reached without crossing the internet:
/// private, link-local or loopback addresses.
fn on_own_network(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => ip.is_private() || ip.is_link_local() || ip.is_loopback(),
        std::net::IpAddr::V6(ip) => {
            ip.is_unique_local() || ip.is_unicast_link_local() || ip.is_loopback()
        }
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0_u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Clone)]
struct PairHandler(Arc<Shared>);

impl std::fmt::Debug for PairHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PairHandler")
    }
}

impl ProtocolHandler for PairHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let remote = connection.remote_id();
        let (mut send, recv) = tokio::time::timeout(wire::CONTROL_TIMEOUT, connection.accept_bi())
            .await
            .map_err(|_| AcceptError::from_err(timed_out("pairing stream")))??;
        let mut reader = wire::reader(recv);
        let reply = match wire::read_control::<ClientLine>(&mut reader).await {
            Ok(ClientLine::Pair { secret, device })
                if device.is_valid() && secret.len() <= wire::MAX_CONTROL_LINE =>
            {
                self.0.pair(remote, &secret, &device)
            }
            Ok(_) => HostLine::Refused {
                reason: "expected a pair line".into(),
            },
            Err(error) => {
                connection.close(2_u32.into(), b"protocol");
                return Err(AcceptError::from_err(error));
            }
        };
        wire::write_line(&mut send, &reply).await?;
        send.finish()?;
        // Let the reply drain before closing; the device closes once it
        // has read it.
        let _ = tokio::time::timeout(wire::CONTROL_TIMEOUT, connection.closed()).await;
        connection.close(0_u32.into(), b"done");
        Ok(())
    }
}

#[derive(Clone)]
struct MainHandler(Arc<Shared>);

impl std::fmt::Debug for MainHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MainHandler")
    }
}

impl ProtocolHandler for MainHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let shared = self.0.clone();
        if !shared.admit(&connection) {
            // Say why before closing, so the device stops retrying and asks
            // to pair again instead of treating this as a network fault.
            if let Ok(Ok((mut send, recv))) =
                tokio::time::timeout(wire::CONTROL_TIMEOUT, connection.accept_bi()).await
            {
                let mut reader = wire::reader(recv);
                let _ = wire::read_control::<ClientLine>(&mut reader).await;
                let _ = wire::write_line(
                    &mut send,
                    &HostLine::HelloRejected {
                        reason: HelloRejection::Unpaired,
                    },
                )
                .await;
                let _ = send.finish();
                let _ = tokio::time::timeout(wire::CONTROL_TIMEOUT, connection.closed()).await;
            }
            connection.close(1_u32.into(), b"unpaired");
            return Ok(());
        }
        let _released = Release {
            shared: shared.clone(),
            connection: connection.clone(),
        };
        let hello_done = Arc::new(AtomicBool::new(false));
        let open_streams = Arc::new(AtomicUsize::new(0));
        let tunnels = Arc::new(AtomicUsize::new(0));
        loop {
            let accepted = tokio::select! {
                accepted = connection.accept_bi() => accepted,
                _ = connection.closed() => break,
            };
            let Ok((send, recv)) = accepted else {
                break;
            };
            if open_streams.fetch_add(1, Ordering::AcqRel) >= wire::MAX_STREAMS as usize {
                open_streams.fetch_sub(1, Ordering::AcqRel);
                drop((send, recv));
                continue;
            }
            let stream = StreamTask {
                shared: shared.clone(),
                connection: connection.clone(),
                hello_done: hello_done.clone(),
                open_streams: open_streams.clone(),
                tunnels: tunnels.clone(),
            };
            tokio::spawn(async move {
                let result = stream.run(send, wire::reader(recv)).await;
                stream.open_streams.fetch_sub(1, Ordering::AcqRel);
                if let Err(error) = result {
                    log::debug!("main stream ended: {error}");
                }
            });
        }
        Ok(())
    }
}

/// Unregisters a connection when its accept task ends, however it ends.
struct Release {
    shared: Arc<Shared>,
    connection: Connection,
}

impl Drop for Release {
    fn drop(&mut self) {
        self.shared.release(&self.connection);
    }
}

struct StreamTask {
    shared: Arc<Shared>,
    connection: Connection,
    hello_done: Arc<AtomicBool>,
    open_streams: Arc<AtomicUsize>,
    tunnels: Arc<AtomicUsize>,
}

/// One of the connection's [`wire::MAX_TUNNELS`] tunnel slots, held while a
/// tunnel is served.
struct TunnelSlot(Arc<AtomicUsize>);

impl TunnelSlot {
    fn take(tunnels: &Arc<AtomicUsize>) -> Option<Self> {
        if tunnels.fetch_add(1, Ordering::AcqRel) >= wire::MAX_TUNNELS {
            tunnels.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(Self(tunnels.clone()))
    }
}

impl Drop for TunnelSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl StreamTask {
    async fn run(&self, mut send: SendStream, mut reader: LineReader) -> io::Result<()> {
        let first = match wire::read_control::<ClientLine>(&mut reader).await {
            Ok(first) => first,
            Err(error) => {
                self.connection.close(2_u32.into(), b"protocol");
                return Err(error);
            }
        };
        match first {
            ClientLine::Hello {
                protocol_version,
                device,
            } => {
                if protocol_version != tcode_protocol::PROTOCOL_VERSION {
                    wire::write_line(
                        &mut send,
                        &HostLine::HelloRejected {
                            reason: HelloRejection::Protocol,
                        },
                    )
                    .await?;
                    send.finish()?;
                    return Ok(());
                }
                if self.hello_done.swap(true, Ordering::AcqRel) {
                    return refuse(send, "one main stream per connection").await;
                }
                self.shared
                    .refresh_device(self.connection.remote_id(), &device);
                wire::write_line(
                    &mut send,
                    &HostLine::HelloOk {
                        host_name: self.shared.host_name(),
                        protocol_version: tcode_protocol::PROTOCOL_VERSION,
                    },
                )
                .await?;
                self.bridge(send, reader).await
            }
            ClientLine::Connect { host, port } => {
                if !self.hello_done.load(Ordering::Acquire) {
                    return refuse(send, "hello required").await;
                }
                if !wire::valid_tunnel_target(&host, port) {
                    return refuse(send, "invalid tunnel target").await;
                }
                let Some(_slot) = TunnelSlot::take(&self.tunnels) else {
                    return refuse(send, "too many tunnels").await;
                };
                crate::tunnel::serve(send, reader, &host, port).await
            }
            ClientLine::Pair { .. } => refuse(send, "pairing uses tcode/pair/1").await,
        }
    }

    /// Pump NDJSON between the stream and one mux attachment until either
    /// side ends. Retained-command keys are scoped to the device so its
    /// dedup cache cannot be shared with or spoofed by another device.
    async fn bridge(&self, mut send: SendStream, mut reader: LineReader) -> io::Result<()> {
        let attachment = self.shared.mux.attach();
        let scope = self.connection.remote_id().to_string();
        let (outbound, outbound_rx) = async_channel::unbounded::<String>();
        let from_host = attachment.from_host.clone();
        let forward = outbound.clone();
        let forwarder = tokio::spawn(async move {
            while let Ok(line) = from_host.recv().await {
                if forward.send(line).await.is_err() {
                    break;
                }
            }
        });
        let writer = tokio::spawn(async move {
            while let Ok(line) = outbound_rx.recv().await {
                if wire::write_raw_line(&mut send, &line).await.is_err() {
                    break;
                }
            }
            let _ = send.finish();
        });
        let result =
            async {
                while let Some(line) = wire::read_line(&mut reader, wire::MAX_LINE).await? {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&line) else {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "client line is not JSON",
                        ));
                    };
                    if let Ok(tcode_protocol::ClientMessage {
                        id,
                        payload:
                            tcode_protocol::ClientPayload::Query(tcode_protocol::Query::Hosting {
                                action,
                            }),
                        ..
                    }) = serde_json::from_value(value.clone())
                    {
                        let reply = tcode_protocol::HostMessage::QueryResult {
                            id,
                            result: self
                                .shared
                                .hosting(action)
                                .map(tcode_protocol::QueryResponse::Hosting),
                        };
                        let reply = tcode_protocol::encode_line(&reply)
                            .map_err(|error| io::Error::other(error.message))?;
                        let _ = outbound.send(reply).await;
                        continue;
                    }
                    if let Some(key) = value.get("key").and_then(serde_json::Value::as_str) {
                        if !valid_key(key) {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "client line carries an invalid key",
                            ));
                        }
                        value["key"] = format!("{scope}:{key}").into();
                    }
                    let mut line = value.to_string();
                    line.push('\n');
                    if attachment.to_host.send(line).await.is_err() {
                        break;
                    }
                }
                Ok(())
            }
            .await;
        attachment.to_host.close();
        outbound.close();
        forwarder.abort();
        let _ = writer.await;
        if result.is_err() {
            self.connection.close(2_u32.into(), b"protocol");
        }
        result
    }
}

async fn refuse(mut send: SendStream, reason: &str) -> io::Result<()> {
    wire::write_line(
        &mut send,
        &HostLine::Refused {
            reason: reason.into(),
        },
    )
    .await?;
    send.finish()?;
    Ok(())
}

/// A client-minted key is one UUID; the host adds the device scope itself.
pub(crate) fn valid_key(key: &str) -> bool {
    key.len() == 36
        && key.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn timed_out(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, format!("{what} timed out"))
}
