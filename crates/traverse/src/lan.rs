//! Finding a paired machine on the local network without any service: the
//! machine advertises standard DNS-SD, and the device resolves a paired id
//! through iroh's [`AddressLookup`] extension point from exactly two
//! sources — the addresses it saved for that machine, handed over at once
//! with the one that carried the last connection first, and a DNS-SD browse
//! for its id that runs meanwhile and adds what it finds. Nothing else is
//! tried. A candidate address is never authorization: the QUIC handshake
//! verifies the machine's key. The device publishes nothing.
use std::{
    collections::{BTreeSet, HashMap},
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use futures_lite::{Stream, stream::Boxed};
use iroh::{
    EndpointAddr, EndpointId, Watcher,
    address_lookup::{AddressLookup, EndpointInfo, Error, Item},
};
use mdns_sd::{IfKind, ServiceDaemon, ServiceEvent, ServiceInfo, TxtProperties};

use crate::runtime::runtime;

pub const SERVICE_TYPE: &str = "_tcode._udp.local.";
/// The UDP port a machine binds unless told otherwise, so firewall rules
/// and invitation addresses survive restarts.
pub const DEFAULT_PORT: u16 = 47_420;
/// The longest machine name a TXT record carries.
pub(crate) const MAX_NAME_BYTES: usize = 64;
const TXT_VERSION: &str = "1";
/// How long one resolve browses DNS-SD.
pub const BROWSE_TIME: Duration = Duration::from_millis(2_500);
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

/// Wait until the endpoint's own IP addresses differ from `known`, which it
/// then holds; a change of relay alone does not count. iroh's network
/// monitor updates them when an interface or its addresses change — another
/// Wi-Fi network, a hotspot, a new lease. `None` once the endpoint is gone.
pub(crate) async fn address_change(
    addresses: &mut impl Watcher<Value = EndpointAddr>,
    known: &mut BTreeSet<SocketAddr>,
) -> Option<()> {
    loop {
        let current: BTreeSet<SocketAddr> = addresses
            .updated()
            .await
            .ok()?
            .ip_addrs()
            .copied()
            .collect();
        if current != *known {
            *known = current;
            return Some(());
        }
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
    recheck: tokio::task::AbortHandle,
}

impl Advertisement {
    /// Advertise `id` at UDP `port` on every non-loopback interface. One SRV
    /// record carries one port, so when the IPv6 socket is on another port
    /// only A records are published.
    ///
    /// mdns-sd reads the interfaces again only on its IP check, every 5 s by
    /// default. The check runs instead when the endpoint's `addresses`
    /// change: the interval is set to 1 s for one check, then off again.
    pub(crate) fn start(
        id: &EndpointId,
        name: &str,
        port: u16,
        ipv6: bool,
        mut addresses: impl Watcher<Value = EndpointAddr> + Send + 'static,
    ) -> Result<Self, mdns_sd::Error> {
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
            daemon.set_ip_check_interval(0)?;
            daemon.register(info)
        };
        if let Err(error) = configure() {
            let _ = daemon.shutdown();
            return Err(error);
        }
        let checked = daemon.clone();
        let recheck = runtime()
            .spawn(async move {
                let mut known = addresses.get().ip_addrs().copied().collect();
                while address_change(&mut addresses, &mut known).await.is_some() {
                    if checked.set_ip_check_interval(1).is_err() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(1_500)).await;
                    if checked.set_ip_check_interval(0).is_err() {
                        return;
                    }
                }
            })
            .abort_handle();
        Ok(Self {
            daemon,
            fullname,
            recheck,
        })
    }
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        self.recheck.abort();
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
    /// Held around each browse; Android drops multicast without it.
    pub multicast_lock: Option<Arc<dyn Fn(bool) + Send + Sync>>,
}

impl Default for LanOptions {
    fn default() -> Self {
        Self {
            browse: Browse::DnsSd,
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

    /// Start a browse; the request ends when the returned guard drops.
    fn begin(self: &Arc<Self>) -> (BrowseRequest, async_channel::Receiver<Found>) {
        let request = self.next.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = async_channel::bounded(64);
        self.pending.lock().unwrap().insert(request, sender);
        (self.start)(request);
        (
            BrowseRequest {
                browser: self.clone(),
                request,
            },
            receiver,
        )
    }

    fn end(&self, request: u64) {
        self.pending.lock().unwrap().remove(&request);
        (self.stop)(request);
    }
}

/// One platform browse in flight. The resolve task that holds it is aborted
/// when its stream drops, so the platform is told to stop from here, not
/// from the task's own tail.
struct BrowseRequest {
    browser: Arc<SystemBrowser>,
    request: u64,
}

impl Drop for BrowseRequest {
    fn drop(&mut self) {
        self.browser.end(self.request);
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
    /// The tasks of the resolves in flight, aborted early by
    /// [`LanLookup::connected`].
    active: Mutex<HashMap<EndpointId, Vec<tokio::task::AbortHandle>>>,
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
                active: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// A connection to `id` is up: whatever its resolves have not tried yet
    /// is not needed. iroh keeps polling a lookup after it has a path, and
    /// every address it learns stays on the machine's dial list.
    pub fn connected(&self, id: EndpointId) {
        if let Some(resolves) = self.inner.active.lock().unwrap().remove(&id) {
            for resolve in resolves {
                resolve.abort();
            }
        }
    }
}

impl AddressLookup for LanLookup {
    /// The resolve runs on the runtime of the endpoint asking, so its
    /// browse is timed by that runtime's clock.
    fn resolve(&self, id: EndpointId) -> Option<Boxed<Result<Item, Error>>> {
        let (sender, receiver) = async_channel::bounded(16);
        let resolve = tokio::spawn(self.inner.clone().run(id, sender)).abort_handle();
        self.inner
            .active
            .lock()
            .unwrap()
            .entry(id)
            .or_default()
            .push(resolve.clone());
        Some(Box::pin(Resolving {
            receiver: Box::pin(receiver),
            _stop: Stop {
                resolve,
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
    resolve: tokio::task::AbortHandle,
    inner: Arc<Inner>,
    id: EndpointId,
}

impl Drop for Stop {
    fn drop(&mut self) {
        self.resolve.abort();
        let mut active = self.inner.active.lock().unwrap();
        if let Some(resolves) = active.get_mut(&self.id) {
            resolves.retain(|resolve| resolve.id() != self.resolve.id());
            if resolves.is_empty() {
                active.remove(&self.id);
            }
        }
    }
}

fn item(id: EndpointId, provenance: &'static str, addrs: Vec<SocketAddr>) -> Item {
    Item::new(EndpointInfo::new(id).with_ip_addrs(addrs), provenance, None)
}

impl Inner {
    async fn run(self: Arc<Self>, id: EndpointId, sender: async_channel::Sender<Item>) {
        let saved = (self.saved)(id);
        if !saved.is_empty() && sender.send(item(id, "lan-saved", saved)).await.is_err() {
            return;
        }
        self.browse(id, &sender).await;
    }

    /// Browse for [`BROWSE_TIME`], yielding every instance that claims `id`
    /// as it resolves.
    async fn browse(&self, id: EndpointId, sender: &async_channel::Sender<Item>) {
        match &self.options.browse {
            Browse::Off => {}
            Browse::DnsSd => {
                let _lock = self
                    .options
                    .multicast_lock
                    .clone()
                    .map(MulticastLock::acquire);
                browse_dns_sd(id, sender).await;
            }
            Browse::System(browser) => {
                let _lock = self
                    .options
                    .multicast_lock
                    .clone()
                    .map(MulticastLock::acquire);
                let (_request, results) = browser.begin();
                let deadline = tokio::time::Instant::now() + BROWSE_TIME;
                let mut seen: Vec<SocketAddr> = Vec::new();
                loop {
                    let result = tokio::select! {
                        result = results.recv() => result,
                        _ = tokio::time::sleep_until(deadline) => break,
                    };
                    let Ok((claimed, addrs)) = result else {
                        break;
                    };
                    if claimed != id {
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
                    if sender.send(item(id, "lan-dns-sd", fresh)).await.is_err() {
                        break;
                    }
                }
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

/// One mdns-sd browse of [`BROWSE_TIME`]. Dropped with an aborted resolve,
/// it stops the browse and its daemon all the same.
async fn browse_dns_sd(id: EndpointId, sender: &async_channel::Sender<Item>) {
    let daemon = match ServiceDaemon::new() {
        Ok(daemon) => Browsing(daemon),
        Err(error) => {
            log::debug!("DNS-SD unavailable: {error}");
            return;
        }
    };
    let events = match daemon.0.browse(SERVICE_TYPE) {
        Ok(events) => events,
        Err(error) => {
            log::debug!("DNS-SD browse failed: {error}");
            return;
        }
    };
    let deadline = tokio::time::Instant::now() + BROWSE_TIME;
    let mut seen: Vec<SocketAddr> = Vec::new();
    loop {
        let event = tokio::select! {
            event = events.recv_async() => event,
            _ = tokio::time::sleep_until(deadline) => break,
        };
        let Ok(event) = event else {
            break;
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
        log::debug!("DNS-SD found {} at {fresh:?}", id.fmt_short());
        if sender.send(item(id, "lan-dns-sd", fresh)).await.is_err() {
            break;
        }
    }
}

/// The daemon behind one browse, shut down however the browse ends.
struct Browsing(ServiceDaemon);

impl Drop for Browsing {
    fn drop(&mut self) {
        let _ = self.0.stop_browse(SERVICE_TYPE);
        let _ = self.0.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
