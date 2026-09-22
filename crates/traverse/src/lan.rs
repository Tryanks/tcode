//! Finding a paired machine on the local network without any service: the
//! machine advertises standard DNS-SD, and the device resolves a paired id
//! through iroh's [`AddressLookup`] extension point from three sources in
//! turn — the addresses it saved from the last connection, a DNS-SD browse,
//! and, for networks that block multicast, unicast probes of its attached
//! private IPv4 networks. A candidate address is never authorization: the
//! QUIC handshake verifies the machine's key, so a wrong guess costs one
//! datagram. The device publishes nothing.
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};

use futures_lite::{Stream, stream::Boxed};
use iroh::{
    EndpointId,
    address_lookup::{AddressLookup, EndpointInfo, Error, Item},
};
use mdns_sd::{IfKind, RecvTimeoutError, ServiceDaemon, ServiceEvent, ServiceInfo, TxtProperties};

use crate::runtime::runtime;

pub const SERVICE_TYPE: &str = "_tcode._udp.local.";
/// The UDP port a machine binds unless told otherwise; probes fall back to it.
pub const DEFAULT_PORT: u16 = 47_420;
/// The longest machine name a TXT record carries.
pub(crate) const MAX_NAME_BYTES: usize = 64;
const TXT_VERSION: &str = "1";
/// How long one resolve browses DNS-SD before it starts probing.
pub const BROWSE_TIME: Duration = Duration::from_millis(2_500);
/// Addresses per probe page and the pause between pages. iroh sends every
/// handshake packet to every address it knows for the machine, so pages
/// stay small and arrive spaced out.
pub(crate) const PROBE_PAGE: usize = 32;
pub(crate) const PROBE_PAGE_INTERVAL: Duration = Duration::from_millis(250);
/// Pages beyond the device's own /24 in one resolve; the next resolve
/// continues outward from where this one stopped.
pub(crate) const PROBE_NEIGHBOUR_PAGES: usize = 2;
/// Attached networks probed per resolve, so a machine with many virtual
/// interfaces still probes the ones people actually plug into.
const MAX_PROBE_NETWORKS: usize = 4;
const MAX_PROBE_PORTS: usize = 3;
/// How often the device compares its interface addresses with the last
/// snapshot; see [`InterfaceWatch`].
pub(crate) const INTERFACE_POLL: Duration = Duration::from_secs(5);

/// `TCODE_DISABLE_MDNS=1` turns DNS-SD off on both sides of a debug build,
/// to exercise the probes.
fn mdns_disabled() -> bool {
    cfg!(debug_assertions) && std::env::var_os("TCODE_DISABLE_MDNS").is_some_and(|v| v == "1")
}

/// One usable interface address of this device with its real prefix.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LocalNetwork {
    pub name: String,
    pub ip: IpAddr,
    pub prefix: u8,
}

/// This device's usable addresses: no loopback, no link-local, sorted and
/// deduplicated so two snapshots compare as sets.
pub(crate) fn local_networks() -> Vec<LocalNetwork> {
    let interfaces = match if_addrs::get_if_addrs() {
        Ok(interfaces) => interfaces,
        Err(error) => {
            log::debug!("local network enumeration failed: {error}");
            return Vec::new();
        }
    };
    let mut networks: Vec<_> = interfaces
        .into_iter()
        .filter(|interface| interface.is_oper_up() && usable_address(interface.ip()))
        .map(|interface| match interface.addr {
            if_addrs::IfAddr::V4(addr) => LocalNetwork {
                name: interface.name,
                ip: addr.ip.into(),
                prefix: addr.prefixlen,
            },
            if_addrs::IfAddr::V6(addr) => LocalNetwork {
                name: interface.name,
                ip: addr.ip.into(),
                prefix: addr.prefixlen,
            },
        })
        .collect();
    networks.sort();
    networks.dedup();
    networks
}

/// An address a machine can be dialled at from another host.
fn usable_address(address: IpAddr) -> bool {
    !address.is_loopback()
        && !address.is_unspecified()
        && !address.is_multicast()
        && match address {
            IpAddr::V4(ip) => !ip.is_link_local() && !ip.is_broadcast(),
            IpAddr::V6(ip) => !ip.is_unicast_link_local(),
        }
}

/// Interface names are a preference, never an exclusion: Internet Sharing
/// uses a bridge, and a VPN can be the only route to a machine.
fn interface_preference(name: &str) -> u8 {
    let name = name.to_ascii_lowercase();
    if [
        "utun",
        "tun",
        "tap",
        "ppp",
        "ipsec",
        "wg",
        "tailscale",
        "zerotier",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
    {
        2
    } else if [
        "bridge",
        "vmnet",
        "vbox",
        "vmenet",
        "docker",
        "vethernet",
        "vmware",
        "parallels",
    ]
    .iter()
    .any(|marker| name.contains(marker))
        || name.starts_with("virbr")
        || name.starts_with("br-")
    {
        1
    } else {
        0
    }
}

/// Detects when this device's address set changes — another Wi-Fi network,
/// a hotspot, a new DHCP lease — from periodic [`local_networks`] snapshots.
pub(crate) struct InterfaceWatch {
    networks: Vec<LocalNetwork>,
}

impl InterfaceWatch {
    pub(crate) fn new(networks: Vec<LocalNetwork>) -> Self {
        let mut watch = Self {
            networks: Vec::new(),
        };
        watch.observe(networks);
        watch
    }

    /// Record a snapshot; true when the address set differs from the last one.
    pub(crate) fn observe(&mut self, mut networks: Vec<LocalNetwork>) -> bool {
        networks.sort();
        networks.dedup();
        let changed = networks != self.networks;
        self.networks = networks;
        changed
    }
}

/// The first 32 hex characters of the id: a DNS label holds at most 63
/// bytes, so the full id travels in the TXT record and the label only keeps
/// instances apart.
fn instance_label(id: &EndpointId) -> String {
    id.to_string()[..32].to_owned()
}

/// `name` as the TXT `name` value: trimmed, control characters dropped, cut
/// to [`MAX_NAME_BYTES`] on a character boundary.
fn advertised_name(name: &str) -> String {
    let mut name: String = name.trim().chars().filter(|c| !c.is_control()).collect();
    while name.len() > MAX_NAME_BYTES {
        name.pop();
    }
    name
}

fn txt_records(id: &EndpointId, name: &str) -> [(&'static str, String); 3] {
    [
        ("v", TXT_VERSION.into()),
        ("id", id.to_string()),
        ("name", advertised_name(name)),
    ]
}

/// The id a browsed instance claims; `None` for a record of another version
/// or with no valid id.
fn advertised_id(txt: &TxtProperties) -> Option<EndpointId> {
    if txt.get_property_val_str("v") != Some(TXT_VERSION) {
        return None;
    }
    txt.get_property_val_str("id")?.parse().ok()
}

/// This machine's DNS-SD record, withdrawn on drop.
pub(crate) struct Advertisement {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Advertisement {
    /// Advertise `id` at UDP `port` on every non-loopback interface. One SRV
    /// record carries one port, so when the IPv6 socket is on another port
    /// only A records are published.
    pub(crate) fn start(
        id: &EndpointId,
        name: &str,
        port: u16,
        ipv6: bool,
    ) -> Result<Option<Self>, mdns_sd::Error> {
        if mdns_disabled() {
            return Ok(None);
        }
        let label = instance_label(id);
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            &label,
            &format!("{label}.local."),
            (),
            port,
            &txt_records(id, name)[..],
        )?
        .enable_addr_auto();
        let fullname = info.get_fullname().to_owned();
        let daemon = ServiceDaemon::new()?;
        let configure = || -> Result<(), mdns_sd::Error> {
            daemon.disable_interface(IfKind::LoopbackV4)?;
            daemon.disable_interface(IfKind::LoopbackV6)?;
            if !ipv6 {
                daemon.disable_interface(IfKind::IPv6)?;
            }
            daemon.register(info)
        };
        if let Err(error) = configure() {
            let _ = daemon.shutdown();
            return Err(error);
        }
        Ok(Some(Self { daemon, fullname }))
    }
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        // The goodbye packet needs the daemon alive for a moment.
        if let Ok(done) = self.daemon.unregister(&self.fullname) {
            let _ = done.recv_timeout(Duration::from_secs(1));
        }
        let _ = self.daemon.shutdown();
    }
}

/// Where the device's DNS-SD browse runs.
#[derive(Clone)]
pub enum Browse {
    /// mdns-sd in this process.
    DnsSd,
    /// The platform's own browser, for iOS where a raw multicast socket is
    /// not available to the app.
    System(Arc<SystemBrowser>),
    Off,
}

/// How a device's [`LanLookup`] observes the network.
#[derive(Clone)]
pub struct LanOptions {
    pub browse: Browse,
    /// The device's attached networks, sampled at each resolve.
    pub networks: Arc<dyn Fn() -> Vec<LocalNetwork> + Send + Sync>,
    /// Held around each browse; Android drops multicast without it.
    pub multicast_lock: Option<Arc<dyn Fn(bool) + Send + Sync>>,
}

impl Default for LanOptions {
    fn default() -> Self {
        Self {
            browse: if mdns_disabled() {
                Browse::Off
            } else {
                Browse::DnsSd
            },
            networks: Arc::new(local_networks),
            multicast_lock: None,
        }
    }
}

impl std::fmt::Debug for LanOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let browse = match self.browse {
            Browse::DnsSd => "dns-sd",
            Browse::System(_) => "system",
            Browse::Off => "off",
        };
        f.debug_struct("LanOptions")
            .field("browse", &browse)
            .finish_non_exhaustive()
    }
}

/// A DNS-SD browse run by the platform: `start` asks it to browse under a
/// request id, results come back through [`found`](Self::found), and `stop`
/// ends the request.
pub struct SystemBrowser {
    start: Box<dyn Fn(u64) + Send + Sync>,
    stop: Box<dyn Fn(u64) + Send + Sync>,
    pending: Mutex<HashMap<u64, async_channel::Sender<Found>>>,
    next: AtomicU64,
}

/// One instance a platform browse resolved: the id it claims, its addresses.
type Found = (EndpointId, Vec<SocketAddr>);

impl SystemBrowser {
    pub fn new(
        start: impl Fn(u64) + Send + Sync + 'static,
        stop: impl Fn(u64) + Send + Sync + 'static,
    ) -> Self {
        Self {
            start: Box::new(start),
            stop: Box::new(stop),
            pending: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        }
    }

    /// One resolved instance of request `request`: the id its TXT record
    /// claims and where its SRV and address records point. Unknown requests
    /// and malformed ids are dropped.
    pub fn found(&self, request: u64, id: &str, addrs: impl IntoIterator<Item = SocketAddr>) {
        let Ok(id) = id.parse::<EndpointId>() else {
            return;
        };
        let addrs: Vec<_> = addrs
            .into_iter()
            .filter(|addr| usable_address(addr.ip()))
            .collect();
        let sender = self.pending.lock().unwrap().get(&request).cloned();
        if let Some(sender) = sender
            && !addrs.is_empty()
        {
            let _ = sender.try_send((id, addrs));
        }
    }

    fn begin(&self) -> (u64, async_channel::Receiver<Found>) {
        let request = self.next.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = async_channel::bounded(64);
        self.pending.lock().unwrap().insert(request, sender);
        (self.start)(request);
        (request, receiver)
    }

    fn end(&self, request: u64) {
        self.pending.lock().unwrap().remove(&request);
        (self.stop)(request);
    }
}

/// The device side: resolves paired ids only, publishes nothing.
#[derive(Clone)]
pub struct LanLookup {
    inner: Arc<Inner>,
}

type SavedAddrs = dyn Fn(EndpointId) -> Vec<SocketAddr> + Send + Sync;

struct Inner {
    saved: Box<SavedAddrs>,
    options: LanOptions,
    /// How many resolves have probed beyond each id's own /24.
    rounds: Mutex<HashMap<EndpointId, u32>>,
    /// Resolves in flight, ended early by [`LanLookup::connected`].
    active: Mutex<HashMap<EndpointId, Vec<Arc<AtomicBool>>>>,
}

impl std::fmt::Debug for LanLookup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LanLookup")
            .field("options", &self.inner.options)
            .finish()
    }
}

impl LanLookup {
    /// `saved` reads the direct addresses last known for an id, at every
    /// resolve.
    pub fn new(
        saved: impl Fn(EndpointId) -> Vec<SocketAddr> + Send + Sync + 'static,
        options: LanOptions,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                saved: Box::new(saved),
                options,
                rounds: Mutex::new(HashMap::new()),
                active: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// A connection to `id` is up: whatever its resolves have not tried yet
    /// is not needed. iroh keeps polling a lookup after it has a path, and
    /// every address it learns stays on the machine's dial list.
    pub fn connected(&self, id: EndpointId) {
        if let Some(flags) = self.inner.active.lock().unwrap().remove(&id) {
            for flag in flags {
                flag.store(true, Ordering::Relaxed);
            }
        }
    }
}

impl AddressLookup for LanLookup {
    fn resolve(&self, id: EndpointId) -> Option<Boxed<Result<Item, Error>>> {
        let (sender, receiver) = async_channel::bounded(16);
        let cancel = Arc::new(AtomicBool::new(false));
        self.inner
            .active
            .lock()
            .unwrap()
            .entry(id)
            .or_default()
            .push(cancel.clone());
        let task = runtime().spawn(self.inner.clone().run(id, sender, cancel.clone()));
        Some(Box::pin(Resolving {
            receiver: Box::pin(receiver),
            _stop: Stop {
                task: task.abort_handle(),
                cancel,
                inner: self.inner.clone(),
                id,
            },
        }))
    }
}

/// The items of one resolve; dropping it stops the work behind them.
struct Resolving {
    receiver: Pin<Box<async_channel::Receiver<Item>>>,
    _stop: Stop,
}

impl Stream for Resolving {
    type Item = Result<Item, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver
            .as_mut()
            .poll_next(cx)
            .map(|item| item.map(Ok))
    }
}

struct Stop {
    task: tokio::task::AbortHandle,
    cancel: Arc<AtomicBool>,
    inner: Arc<Inner>,
    id: EndpointId,
}

impl Drop for Stop {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.task.abort();
        let mut active = self.inner.active.lock().unwrap();
        if let Some(flags) = active.get_mut(&self.id) {
            flags.retain(|flag| !Arc::ptr_eq(flag, &self.cancel));
            if flags.is_empty() {
                active.remove(&self.id);
            }
        }
    }
}

fn item(id: EndpointId, provenance: &'static str, addrs: Vec<SocketAddr>) -> Item {
    Item::new(EndpointInfo::new(id).with_ip_addrs(addrs), provenance, None)
}

impl Inner {
    async fn run(
        self: Arc<Self>,
        id: EndpointId,
        sender: async_channel::Sender<Item>,
        cancel: Arc<AtomicBool>,
    ) {
        let saved = (self.saved)(id);
        if !saved.is_empty()
            && sender
                .send(item(id, "lan-saved", saved.clone()))
                .await
                .is_err()
        {
            return;
        }
        let found = self.browse(id, &sender, &cancel).await;
        if found || cancel.load(Ordering::Relaxed) {
            return;
        }
        let round = {
            let mut rounds = self.rounds.lock().unwrap();
            let round = rounds.entry(id).or_default();
            let this = *round;
            *round = round.wrapping_add(1);
            this
        };
        let pages = probe_pages(&(self.options.networks)(), &probe_ports(&saved), round);
        log::debug!(
            "probing {} pages for {} (round {round})",
            pages.len(),
            id.fmt_short()
        );
        for (index, page) in pages.into_iter().enumerate() {
            if index > 0 {
                tokio::time::sleep(PROBE_PAGE_INTERVAL).await;
            }
            if cancel.load(Ordering::Relaxed)
                || sender.send(item(id, "lan-probe", page)).await.is_err()
            {
                return;
            }
        }
    }

    /// Browse for [`BROWSE_TIME`], yielding every instance that claims `id`
    /// as it resolves. True if any did.
    async fn browse(
        &self,
        id: EndpointId,
        sender: &async_channel::Sender<Item>,
        cancel: &Arc<AtomicBool>,
    ) -> bool {
        match &self.options.browse {
            Browse::Off => false,
            Browse::DnsSd => {
                let _lock = self
                    .options
                    .multicast_lock
                    .clone()
                    .map(MulticastLock::acquire);
                let sender = sender.clone();
                let cancel = cancel.clone();
                tokio::task::spawn_blocking(move || browse_dns_sd(id, &sender, &cancel))
                    .await
                    .unwrap_or(false)
            }
            Browse::System(browser) => {
                let _lock = self
                    .options
                    .multicast_lock
                    .clone()
                    .map(MulticastLock::acquire);
                let (request, results) = browser.begin();
                let deadline = tokio::time::Instant::now() + BROWSE_TIME;
                let mut found = false;
                let mut seen: Vec<SocketAddr> = Vec::new();
                loop {
                    let result = tokio::select! {
                        result = results.recv() => result,
                        _ = tokio::time::sleep_until(deadline) => break,
                    };
                    let Ok((claimed, addrs)) = result else {
                        break;
                    };
                    if claimed != id || cancel.load(Ordering::Relaxed) {
                        continue;
                    }
                    let fresh: Vec<_> = addrs
                        .into_iter()
                        .filter(|addr| !seen.contains(addr))
                        .collect();
                    if fresh.is_empty() {
                        continue;
                    }
                    seen.extend(fresh.iter().copied());
                    found = true;
                    if sender.send(item(id, "lan-dns-sd", fresh)).await.is_err() {
                        break;
                    }
                }
                browser.end(request);
                found
            }
        }
    }
}

/// Holds the platform's multicast lock for a browse.
struct MulticastLock(Arc<dyn Fn(bool) + Send + Sync>);

impl MulticastLock {
    fn acquire(lock: Arc<dyn Fn(bool) + Send + Sync>) -> Self {
        lock(true);
        Self(lock)
    }
}

impl Drop for MulticastLock {
    fn drop(&mut self) {
        (self.0)(false);
    }
}

/// One mdns-sd browse of [`BROWSE_TIME`] on a blocking thread; mdns-sd
/// answers through a synchronous channel.
fn browse_dns_sd(
    id: EndpointId,
    sender: &async_channel::Sender<Item>,
    cancel: &AtomicBool,
) -> bool {
    let daemon = match ServiceDaemon::new() {
        Ok(daemon) => daemon,
        Err(error) => {
            log::debug!("DNS-SD unavailable: {error}");
            return false;
        }
    };
    let events = match daemon.browse(SERVICE_TYPE) {
        Ok(events) => events,
        Err(error) => {
            log::debug!("DNS-SD browse failed: {error}");
            let _ = daemon.shutdown();
            return false;
        }
    };
    let deadline = Instant::now() + BROWSE_TIME;
    let mut found = false;
    let mut seen: Vec<SocketAddr> = Vec::new();
    while !cancel.load(Ordering::Relaxed) {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        let event = match events.recv_timeout(left.min(Duration::from_millis(100))) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let ServiceEvent::ServiceResolved(info) = event else {
            continue;
        };
        if advertised_id(info.get_properties()) != Some(id) || info.get_port() == 0 {
            continue;
        }
        let fresh: Vec<_> = info
            .get_addresses()
            .iter()
            .map(|scoped| SocketAddr::new(scoped.to_ip_addr(), info.get_port()))
            .filter(|addr| usable_address(addr.ip()) && !seen.contains(addr))
            .collect();
        if fresh.is_empty() {
            continue;
        }
        seen.extend(fresh.iter().copied());
        found = true;
        log::debug!("DNS-SD found {} at {fresh:?}", id.fmt_short());
        if sender.send_blocking(item(id, "lan-dns-sd", fresh)).is_err() {
            break;
        }
    }
    let _ = daemon.stop_browse(SERVICE_TYPE);
    let _ = daemon.shutdown();
    found
}

/// The ports probes try: those of the saved addresses, newest first, and
/// [`DEFAULT_PORT`].
fn probe_ports(saved: &[SocketAddr]) -> Vec<u16> {
    let mut ports = Vec::new();
    for port in saved.iter().map(SocketAddr::port).chain([DEFAULT_PORT]) {
        if port != 0 && !ports.contains(&port) && ports.len() < MAX_PROBE_PORTS {
            ports.push(port);
        }
    }
    ports
}

/// The probe pages of one resolve, in order: each attached private IPv4
/// network's own /24 in pages of [`PROBE_PAGE`] addresses, then
/// [`PROBE_NEIGHBOUR_PAGES`] pages of the neighbouring /24s, alternating
/// above and below and continuing across `round`s. A broad interface mask
/// is clipped to its RFC 1918 block, so a wrong mask never probes public
/// space; the network, broadcast and this device's own addresses are left
/// out. Every address is tried at every port.
fn probe_pages(networks: &[LocalNetwork], ports: &[u16], round: u32) -> Vec<Vec<SocketAddr>> {
    let mut eligible: Vec<(&LocalNetwork, Ipv4Addr, u8, u8)> = networks
        .iter()
        .filter_map(|network| {
            let IpAddr::V4(ip) = network.ip else {
                return None;
            };
            // Loopback never comes from `local_networks`; it lets a test
            // probe without touching a real network.
            if !(ip.is_private() || ip.is_loopback()) || !(8..=30).contains(&network.prefix) {
                return None;
            }
            let block = match ip.octets()[0] {
                10 | 127 => 8,
                172 => 12,
                _ => 16,
            };
            Some((network, ip, network.prefix, network.prefix.max(block)))
        })
        .collect();
    eligible.sort_by_key(|(network, ip, _, _)| (interface_preference(&network.name), *ip));
    let mut subnets = Vec::new();
    eligible.retain(|(_, ip, prefix, _)| {
        let subnet = (ipv4_network(*ip, *prefix), *prefix);
        if subnets.contains(&subnet) {
            false
        } else {
            subnets.push(subnet);
            true
        }
    });
    eligible.truncate(MAX_PROBE_NETWORKS);
    let own: Vec<IpAddr> = networks.iter().map(|network| network.ip).collect();
    let mut pages = Vec::new();
    // Every network's own /24 — or its smaller real subnet — comes before
    // any neighbour.
    for (_, ip, prefix, scan) in &eligible {
        let (first, count) = if *scan >= 24 {
            (
                u32::from(ipv4_network(*ip, *scan)),
                1 << (32 - u32::from(*scan)),
            )
        } else {
            (u32::from(*ip) & !0xff, 256)
        };
        pages.extend(slab_pages(first, count, *ip, *prefix, &own, ports));
    }
    for (_, ip, prefix, scan) in &eligible {
        let slabs = 1_u32 << (24_u8.saturating_sub(*scan));
        if slabs < 2 {
            continue;
        }
        let base = u32::from(ipv4_network(*ip, *scan));
        let local = (u32::from(*ip) - base) >> 8;
        // Neighbour slabs in the order +1, -1, +2, -2, …; each yields
        // several pages, and the walk resumes where the last resolve ended.
        let per_slab = 256_usize.div_ceil(PROBE_PAGE);
        let total = (slabs as usize - 1) * per_slab;
        let start = (round as usize * PROBE_NEIGHBOUR_PAGES) % total;
        let mut taken = Vec::new();
        let mut cursor = start;
        while taken.len() < PROBE_NEIGHBOUR_PAGES && taken.len() < total {
            let step = (cursor / per_slab) as u32 + 1;
            let offset = if step % 2 == 1 {
                step.div_ceil(2)
            } else {
                slabs - step / 2
            };
            let slab = (local + offset) % slabs;
            let slab_pages = slab_pages(base + (slab << 8), 256, *ip, *prefix, &own, ports);
            if let Some(page) = slab_pages.into_iter().nth(cursor % per_slab) {
                taken.push(page);
            }
            cursor = (cursor + 1) % total;
        }
        pages.extend(taken);
    }
    pages
}

/// The pages of the `count` addresses from `first`, for the interface
/// `ip`/`prefix`.
fn slab_pages(
    first: u32,
    count: u32,
    ip: Ipv4Addr,
    prefix: u8,
    own: &[IpAddr],
    ports: &[u16],
) -> Vec<Vec<SocketAddr>> {
    let network = u32::from(ipv4_network(ip, prefix));
    let broadcast = network + (1_u32 << (32 - u32::from(prefix))) - 1;
    let hosts = (first..first + count).filter(|address| {
        let host = Ipv4Addr::from(*address);
        *address != network && *address != broadcast && !own.contains(&IpAddr::V4(host))
    });
    let mut pages: Vec<Vec<SocketAddr>> = Vec::new();
    for (index, address) in hosts.enumerate() {
        if index % PROBE_PAGE == 0 {
            pages.push(Vec::with_capacity(PROBE_PAGE * ports.len()));
        }
        let page = pages.last_mut().expect("a page was opened");
        for port in ports {
            page.push(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(address)), *port));
        }
    }
    pages
}

fn ipv4_network(ip: Ipv4Addr, prefix: u8) -> Ipv4Addr {
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix.min(32)))
    };
    Ipv4Addr::from(u32::from(ip) & mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn network(name: &str, ip: &str, prefix: u8) -> LocalNetwork {
        LocalNetwork {
            name: name.into(),
            ip: ip.parse().unwrap(),
            prefix,
        }
    }

    fn ips(page: &[SocketAddr]) -> Vec<Ipv4Addr> {
        page.iter()
            .filter_map(|addr| match addr.ip() {
                IpAddr::V4(ip) => Some(ip),
                IpAddr::V6(_) => None,
            })
            .collect()
    }

    #[test]
    fn txt_carries_the_full_id_and_a_bounded_name_and_parses_only_its_own_version() {
        let id = iroh::SecretKey::from_bytes(&[7; 32]).public();
        let long = "Désk ".repeat(20);
        let records = txt_records(&id, &format!("  {long}\x07\n "));
        assert_eq!(records[0], ("v", "1".into()));
        assert_eq!(records[1], ("id", id.to_string()));
        let name = &records[2].1;
        assert!(name.len() <= MAX_NAME_BYTES && name.is_char_boundary(name.len()));
        assert!(name.starts_with("Désk Désk") && !name.chars().any(char::is_control));
        assert_eq!(advertised_name("Desk"), "Desk");
        assert!(instance_label(&id).len() <= 63);

        let txt = |records: &[(&str, &str)]| {
            ServiceInfo::new(SERVICE_TYPE, "x", "x.local.", (), 1, records)
                .unwrap()
                .get_properties()
                .clone()
        };
        let hex = id.to_string();
        assert_eq!(
            advertised_id(&txt(&[("v", "1"), ("id", &hex), ("name", "Desk")])),
            Some(id)
        );
        assert_eq!(advertised_id(&txt(&[("v", "1"), ("id", &hex)])), Some(id));
        assert_eq!(advertised_id(&txt(&[("v", "2"), ("id", &hex)])), None);
        assert_eq!(advertised_id(&txt(&[("id", &hex)])), None);
        assert_eq!(advertised_id(&txt(&[("v", "1"), ("id", &hex[1..])])), None);
        assert_eq!(advertised_id(&txt(&[("v", "1"), ("id", "not hex")])), None);
    }

    #[test]
    fn probe_ports_follow_the_saved_addresses_and_always_include_the_default() {
        assert_eq!(probe_ports(&[]), [DEFAULT_PORT]);
        let saved = ["10.0.0.2:47421", "10.0.0.2:47420", "10.0.0.3:47421"]
            .map(|addr| addr.parse().unwrap());
        assert_eq!(probe_ports(&saved), [47421, 47420]);
        let many = ["10.0.0.2:1", "10.0.0.2:2", "10.0.0.2:3", "10.0.0.2:4"]
            .map(|addr| addr.parse().unwrap());
        assert_eq!(probe_ports(&many), [1, 2, 3]);
    }

    #[test]
    fn the_own_slash_24_comes_first_in_pages_of_32_without_self_network_or_broadcast() {
        let pages = probe_pages(&[network("wlan0", "192.168.1.22", 24)], &[47420], 0);
        assert_eq!(pages.len(), 8, "a /24 has no neighbour to walk into");
        assert!(pages.iter().all(|page| page.len() <= PROBE_PAGE));
        let all: Vec<Ipv4Addr> = pages.iter().flat_map(|page| ips(page)).collect();
        assert_eq!(all.len(), 253);
        assert_eq!(all[0], Ipv4Addr::new(192, 168, 1, 1));
        assert_eq!(all[all.len() - 1], Ipv4Addr::new(192, 168, 1, 254));
        assert!(!all.contains(&Ipv4Addr::new(192, 168, 1, 22)));
        assert!(!all.contains(&Ipv4Addr::new(192, 168, 1, 0)));
        assert!(!all.contains(&Ipv4Addr::new(192, 168, 1, 255)));
        assert!(pages[0].iter().all(|addr| addr.port() == 47420));

        // Each address is tried at every port, in the page it belongs to.
        let pages = probe_pages(&[network("wlan0", "172.20.10.1", 28)], &[47421, 47420], 0);
        let first = &pages[0];
        assert_eq!(
            first.len(),
            26,
            "a /28 has 14 hosts minus this device, at two ports"
        );
        assert!(first.contains(&"172.20.10.14:47421".parse().unwrap()));
        assert!(first.contains(&"172.20.10.14:47420".parse().unwrap()));
        assert!(
            !first
                .iter()
                .any(|addr| addr.ip() == "172.20.10.15".parse::<IpAddr>().unwrap())
        );
        assert_eq!(pages.len(), 1);
    }

    #[test]
    fn neighbours_alternate_above_and_below_and_advance_across_resolves() {
        let corporate = [network("en0", "10.20.30.40", 16)];
        let pages = probe_pages(&corporate, &[47420], 0);
        assert_eq!(pages.len(), 8 + PROBE_NEIGHBOUR_PAGES);
        assert_eq!(ips(&pages[8])[0], Ipv4Addr::new(10, 20, 31, 0));
        assert_eq!(ips(&pages[9])[0], Ipv4Addr::new(10, 20, 31, 32));
        let pages = probe_pages(&corporate, &[47420], 4);
        assert_eq!(ips(&pages[8])[0], Ipv4Addr::new(10, 20, 29, 0));
        let pages = probe_pages(&corporate, &[47420], 8);
        assert_eq!(ips(&pages[8])[0], Ipv4Addr::new(10, 20, 32, 0));
        // The walk wraps within the /16 instead of leaving it.
        let edge = [network("en0", "10.20.255.40", 16)];
        let pages = probe_pages(&edge, &[47420], 0);
        assert_eq!(
            ips(&pages[8])[0],
            Ipv4Addr::new(10, 20, 0, 1),
            "10.20.0.0 is the network"
        );
        assert_eq!(ips(&pages[8]).last(), Some(&Ipv4Addr::new(10, 20, 0, 32)));
    }

    #[test]
    fn broad_masks_stay_private_and_keep_hosts_at_block_edges() {
        for (ip, prefix, kept, actual_network, actual_broadcast) in [
            (
                "172.31.255.42",
                8,
                "172.31.255.255",
                "172.0.0.0",
                "172.255.255.255",
            ),
            (
                "192.168.255.42",
                8,
                "192.168.255.255",
                "192.0.0.0",
                "192.255.255.255",
            ),
            (
                "10.255.255.42",
                8,
                "10.255.255.254",
                "10.0.0.0",
                "10.255.255.255",
            ),
        ] {
            let networks = [network("en0", ip, prefix)];
            let all: Vec<Ipv4Addr> = (0..3)
                .flat_map(|round| probe_pages(&networks, &[47420], round))
                .flat_map(|page| ips(&page))
                .collect();
            assert!(all.contains(&kept.parse().unwrap()), "lost {kept}/{prefix}");
            for candidate in &all {
                assert!(
                    candidate.is_private(),
                    "public probe {candidate} from {ip}/{prefix}"
                );
                assert_ne!(candidate.to_string(), actual_network);
                assert_ne!(candidate.to_string(), actual_broadcast);
                assert_ne!(candidate.to_string(), ip);
            }
        }
        for ip in ["203.0.113.5", "100.64.0.10", "169.254.3.4"] {
            assert!(probe_pages(&[network("en0", ip, 24)], &[47420], 0).is_empty());
        }
        assert!(probe_pages(&[network("en0", "fd00::2", 64)], &[47420], 0).is_empty());
        assert!(probe_pages(&[], &[47420], 0).is_empty());
    }

    #[test]
    fn every_attached_network_is_probed_before_any_neighbour_with_the_lan_first() {
        let networks = [
            network("bridge100", "192.168.139.3", 24),
            network("en0", "192.168.1.22", 24),
            network("utun4", "10.0.0.1", 32),
            network("en0", "192.168.1.22", 24),
        ];
        let pages = probe_pages(&networks, &[47420], 0);
        assert_eq!(pages.len(), 16, "two /24s, the /32 has no hosts");
        assert_eq!(ips(&pages[0])[0], Ipv4Addr::new(192, 168, 1, 1));
        assert_eq!(ips(&pages[8])[0], Ipv4Addr::new(192, 168, 139, 1));
        let many: Vec<_> = (1..=9)
            .map(|n| network(&format!("en{n}"), &format!("10.{n}.7.42"), 16))
            .collect();
        let pages = probe_pages(&many, &[47420], 0);
        assert_eq!(
            pages.len(),
            MAX_PROBE_NETWORKS * (8 + PROBE_NEIGHBOUR_PAGES)
        );
    }

    #[test]
    fn interface_watch_ignores_order_and_reports_added_or_lost_addresses() {
        let mut watch = InterfaceWatch::new(vec![
            network("en0", "192.168.1.22", 24),
            network("en0", "10.0.0.5", 24),
        ]);
        assert!(!watch.observe(vec![
            network("en0", "10.0.0.5", 24),
            network("en0", "192.168.1.22", 24)
        ]));
        assert!(watch.observe(vec![network("en0", "10.0.0.5", 24)]));
        assert!(!watch.observe(vec![
            network("en0", "10.0.0.5", 24),
            network("en0", "10.0.0.5", 24)
        ]));
        assert!(watch.observe(vec![
            network("en0", "10.0.0.5", 24),
            network("en0", "172.20.10.3", 28)
        ]));
    }
}
