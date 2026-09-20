//! Address hints for reaching a machine: DNS-SD and attached private networks.
//! A hint is never authorization; transport authenticates the machine before
//! sending credentials on the candidate connection.
use mdns_sd::ServiceDaemon;
#[cfg(any(feature = "server", test))]
use mdns_sd::ServiceInfo;
#[cfg(feature = "client")]
use mdns_sd::{ScopedIp, ServiceEvent};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
#[cfg(feature = "client")]
use std::net::Ipv4Addr;
use std::time::Duration;
#[cfg(feature = "client")]
use std::time::Instant;

pub const SERVICE_TYPE: &str = "_tcode._tcp.local.";

/// One usable interface address, retaining its name and actual network prefix.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LocalNetwork {
    pub name: String,
    pub ip: IpAddr,
    pub prefix: Option<u8>,
}

/// This device's usable addresses: no loopback, no link-local, sorted and
/// deduplicated so two snapshots compare as sets.
pub fn local_networks() -> Vec<LocalNetwork> {
    // Enumeration runs on every recovery poll. Report one warning per outage
    // and reset it after success instead of flooding logs while access is denied.
    static ENUMERATION_FAILED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    let interfaces = match if_addrs::get_if_addrs() {
        Ok(interfaces) => {
            if ENUMERATION_FAILED.swap(false, std::sync::atomic::Ordering::Relaxed) {
                log::info!("local network enumeration recovered");
            }
            interfaces
        }
        Err(error) => {
            if !ENUMERATION_FAILED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                log::warn!("local network enumeration failed; recovery will retry: {error}");
            }
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
                prefix: Some(addr.prefixlen),
            },
            if_addrs::IfAddr::V6(addr) => LocalNetwork {
                name: interface.name,
                ip: addr.ip.into(),
                prefix: Some(addr.prefixlen),
            },
        })
        .collect();
    networks.sort();
    networks.dedup();
    networks
}

/// Addresses other devices can reach this machine at, as strings for pairing
/// codes and hello.
pub fn local_addrs() -> Vec<String> {
    pairing_addrs(&local_networks())
}

fn pairing_addrs(networks: &[LocalNetwork]) -> Vec<String> {
    let mut networks: Vec<_> = networks
        .iter()
        .filter(|network| usable_address(network.ip))
        .collect();
    networks.sort_by_key(|network| {
        (
            interface_preference(&network.name),
            address_family_preference(network.ip),
            network.ip,
        )
    });
    let mut seen = std::collections::HashSet::new();
    networks
        .into_iter()
        .filter(|network| seen.insert(network.ip))
        .map(|network| network.ip.to_string())
        .collect()
}

// Interface names are a preference, never an exclusion: Internet Sharing uses
// a bridge, and VPNs can be the only route to a machine. Invites retain them all.
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

fn address_family_preference(ip: IpAddr) -> u8 {
    match ip {
        IpAddr::V4(ip) if ip.is_private() => 0,
        IpAddr::V4(_) => 1,
        IpAddr::V6(_) => 2,
    }
}

fn usable_address(address: IpAddr) -> bool {
    !address.is_loopback()
        && !address.is_unspecified()
        && !address.is_multicast()
        && match address {
            IpAddr::V4(ip) => !ip.is_link_local() && !ip.is_broadcast(),
            IpAddr::V6(ip) => !ip.is_unicast_link_local(),
        }
}

/// Detects when this device's address set changes, e.g. joining another
/// Wi-Fi network or a hotspot, from periodic [`local_networks`] snapshots.
#[cfg(feature = "client")]
pub struct InterfaceWatch {
    networks: Vec<LocalNetwork>,
}

#[cfg(feature = "client")]
impl InterfaceWatch {
    pub fn new(networks: Vec<LocalNetwork>) -> Self {
        let mut watch = Self {
            networks: Vec::new(),
        };
        watch.observe(networks);
        watch
    }

    pub fn networks(&self) -> &[LocalNetwork] {
        &self.networks
    }

    /// Record a snapshot; true when the address set differs from the last one.
    pub fn observe(&mut self, mut networks: Vec<LocalNetwork>) -> bool {
        networks.sort();
        networks.dedup();
        let changed = networks != self.networks;
        self.networks = networks;
        changed
    }
}

/// One bounded page of directly attached private IPv4 networks. Start with
/// each interface's own /24 (or its smaller real subnet), then alternate between
/// higher and lower neighboring pages across retries. This assumes no gateway or
/// fixed hotspot range. Credentials may only be sent after host authentication.
#[cfg(feature = "client")]
pub fn interface_probe_origins(networks: &[LocalNetwork], port: u16, round: u32) -> Vec<String> {
    let mut eligible: Vec<_> = networks
        .iter()
        .filter_map(|network| {
            let IpAddr::V4(ip) = network.ip else {
                return None;
            };
            let prefix = network.prefix.filter(|prefix| (8..=30).contains(prefix))?;
            if !ip.is_private() {
                return None;
            }
            // A broad interface mask must never authorize probing public space.
            let private_prefix = match ip.octets()[0] {
                10 => 8,
                172 => 12,
                _ => 16,
            };
            Some((network, ip, prefix, prefix.max(private_prefix)))
        })
        .collect();
    eligible.sort_by_key(|(network, ip, _, _)| (interface_preference(&network.name), *ip));
    let mut subnets = std::collections::HashSet::new();
    let mut origins = Vec::new();
    // Bound work even on a host with hundreds of virtual interfaces. Rotate
    // groups as well as pages, so a lower-ranked attached network is not lost.
    eligible.retain(|(_, ip, prefix, _)| subnets.insert((ipv4_network(*ip, *prefix), *prefix)));
    let groups = eligible.len().div_ceil(4).max(1);
    let group = round as usize % groups;
    let page_round = round / groups as u32;
    for (_, ip, prefix, scan_prefix) in eligible.into_iter().skip(group * 4).take(4) {
        let base = u32::from(ipv4_network(ip, scan_prefix));
        let page_prefix = scan_prefix.max(24);
        let pages = 1_u32 << (24_u8.saturating_sub(scan_prefix));
        let local_page = (u32::from(ip) - base) >> 8;
        // Visit 0, +1, -1, +2, -2, ... relative to the client's page. A host
        // in the preceding /24 must not wait for the entire subnet to wrap.
        let step = page_round % pages;
        let offset = if step & 1 == 1 {
            step.div_ceil(2)
        } else {
            pages - step / 2
        };
        let page = (local_page + offset) % pages;
        let first = base + (page << 8);
        let count = 1_u32 << (32 - u32::from(page_prefix));
        let network = u32::from(ipv4_network(ip, prefix));
        let broadcast = network + (1_u32 << (32 - u32::from(prefix))) - 1;
        for address in first..first + count {
            // .0/.255 are valid hosts in larger subnets; exclude only the
            // actual network and broadcast addresses, and this device itself.
            // Clipping a broad mask to RFC1918 does not create a new subnet.
            let host = Ipv4Addr::from(address);
            if address == network
                || address == broadcast
                || networks.iter().any(|network| network.ip == host)
            {
                continue;
            }
            origins.push(tcode_client::pairing::lan_origin(&host.to_string(), port));
        }
    }
    origins
}

#[cfg(feature = "client")]
fn ipv4_network(ip: Ipv4Addr, prefix: u8) -> Ipv4Addr {
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix.min(32)))
    };
    Ipv4Addr::from(u32::from(ip) & mask)
}

#[cfg(feature = "client")]
/// Prefer a connected physical LAN using the actual prefix; preserve bridges
/// and tunnels as lower-priority alternatives for Internet Sharing and VPNs.
fn address_preference(address: &str, networks: &[LocalNetwork]) -> u8 {
    let Ok(address) = address.parse::<IpAddr>() else {
        return u8::MAX;
    };
    if !usable_address(address) {
        return u8::MAX;
    }
    let interface_rank = networks
        .iter()
        .filter(|network| same_network(address, network))
        .map(|network| interface_preference(&network.name))
        .min()
        .unwrap_or(3);
    interface_rank * 3 + address_family_preference(address)
}

#[cfg(feature = "client")]
fn same_network(address: IpAddr, network: &LocalNetwork) -> bool {
    match (address, network.ip, network.prefix) {
        (IpAddr::V4(address), IpAddr::V4(local), Some(prefix @ 1..=32)) => {
            ipv4_network(address, prefix) == ipv4_network(local, prefix)
        }
        (IpAddr::V6(address), IpAddr::V6(local), Some(prefix @ 1..=128)) => {
            let mask = u128::MAX << (128 - u32::from(prefix));
            u128::from(address) & mask == u128::from(local) & mask
        }
        _ => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Beacon {
    pub host_id: String,
    pub name: String,
    pub port: u16,
    pub addr: String,
}

#[cfg(feature = "server")]
pub struct BeaconHandle {
    daemon: Option<ServiceDaemon>,
    fullname: String,
}
#[cfg(feature = "server")]
impl BeaconHandle {
    pub fn shutdown(self) {
        drop(self);
    }
}
#[cfg(feature = "server")]
impl Drop for BeaconHandle {
    fn drop(&mut self) {
        if let Some(daemon) = &self.daemon {
            if let Ok(done) = daemon.unregister(&self.fullname) {
                let _ = done.recv_timeout(Duration::from_secs(1));
            }
            let _ = daemon.shutdown();
        }
    }
}

#[cfg(feature = "server")]
pub fn start_beacon(
    host_id: impl Into<String>,
    name: impl Into<String>,
    port: u16,
) -> BeaconHandle {
    let host_id = host_id.into();
    let name = name.into();
    let mut handle = BeaconHandle {
        daemon: None,
        fullname: String::new(),
    };
    let start = || -> Result<(ServiceDaemon, String), mdns_sd::Error> {
        let props = [
            ("host_id", host_id.as_str()),
            ("name", name.as_str()),
            ("port", &port.to_string()),
        ];
        let service = ServiceInfo::new(
            SERVICE_TYPE,
            &host_id,
            &format!("{host_id}.local."),
            "",
            port,
            &props[..],
        )?
        .enable_addr_auto();
        let fullname = service.get_fullname().to_owned();
        let daemon = ServiceDaemon::new()?;
        daemon.disable_interface(mdns_sd::IfKind::LoopbackV4)?;
        daemon.disable_interface(mdns_sd::IfKind::LoopbackV6)?;
        if let Err(error) = daemon.register(service) {
            let _ = daemon.shutdown();
            return Err(error);
        }
        Ok((daemon, fullname))
    };
    match start() {
        Ok((daemon, fullname)) => {
            handle.daemon = Some(daemon);
            handle.fullname = fullname;
        }
        Err(_) => log::warn!("mDNS advertising unavailable"),
    }
    handle
}

#[cfg(feature = "client")]
pub fn browse(timeout: Duration) -> Vec<Beacon> {
    let Ok(daemon) = ServiceDaemon::new() else {
        return Vec::new();
    };
    let networks = local_networks();
    let mut found = Vec::new();
    if let Ok(events) = daemon.browse(SERVICE_TYPE) {
        let deadline = Instant::now() + timeout.min(Duration::from_secs(30));
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            let Ok(event) = events.recv_timeout(left) else {
                break;
            };
            if let ServiceEvent::ServiceResolved(info) = event {
                collect_resolved_beacons(
                    &mut found,
                    info.get_properties(),
                    info.get_port(),
                    info.get_addresses().iter().map(ScopedIp::to_ip_addr),
                    &networks,
                );
            }
        }
        let _ = daemon.stop_browse(SERVICE_TYPE);
    }
    let _ = daemon.shutdown();
    found
}

#[cfg(feature = "client")]
fn collect_resolved_beacons(
    found: &mut Vec<Beacon>,
    txt: &mdns_sd::TxtProperties,
    port: u16,
    addresses: impl Iterator<Item = IpAddr>,
    networks: &[LocalNetwork],
) {
    let mut addresses: Vec<_> = addresses
        .filter(|address| usable_address(*address))
        .collect();
    addresses.sort_by_key(|address| (address_preference(&address.to_string(), networks), *address));
    addresses.dedup();
    for beacon in addresses
        .into_iter()
        .filter_map(|address| parse_txt(txt, port, address.to_string()))
    {
        if let Some(previous) = found.iter_mut().find(|previous| {
            previous.host_id == beacon.host_id
                && previous.addr == beacon.addr
                && previous.port == beacon.port
        }) {
            *previous = beacon;
        } else if found.len() < 128 {
            found.push(beacon);
        }
    }
    found.sort_by_key(|beacon| {
        (
            beacon.host_id.clone(),
            address_preference(&beacon.addr, networks),
            beacon.addr.clone(),
            beacon.port,
        )
    });
}

#[cfg(feature = "client")]
fn parse_txt(txt: &mdns_sd::TxtProperties, port: u16, addr: String) -> Option<Beacon> {
    let field = |key| {
        txt.get_property_val_str(key)
            .filter(|v| !v.is_empty() && v.len() <= 256 && !v.chars().any(char::is_control))
            .map(str::to_owned)
    };
    let host_id = field("host_id")?;
    let name = field("name")?;
    if port == 0 || field("port")?.parse::<u16>().ok()? != port {
        return None;
    }
    Some(Beacon {
        host_id,
        name,
        addr,
        port,
    })
}

#[cfg(all(test, feature = "client"))]
mod tests {
    use super::*;
    #[test]
    fn txt_records_are_bounded_and_consistent() {
        let props = [
            ("host_id", "test"),
            ("name", "Test host"),
            ("port", "47420"),
        ];
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            "test",
            "test.local.",
            "127.0.0.1",
            47420,
            &props[..],
        )
        .unwrap();
        assert!(parse_txt(info.get_properties(), 47420, "127.0.0.1".into()).is_some());
        assert!(parse_txt(info.get_properties(), 1, "127.0.0.1".into()).is_none());
        let malformed = [("host_id", "test"), ("name", ""), ("port", "47420")];
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            "test",
            "test.local.",
            "127.0.0.1",
            47420,
            &malformed[..],
        )
        .unwrap();
        assert!(parse_txt(info.get_properties(), 47420, "127.0.0.1".into()).is_none());
    }

    #[test]
    fn unicast_discovery_covers_unknown_lan_and_hotspot_guests_without_gateway_assumptions() {
        let network = |ip: &str, prefix| LocalNetwork {
            name: "wlan0".into(),
            ip: ip.parse().unwrap(),
            prefix: Some(prefix),
        };
        for (phone, prefix, computer) in [
            ("192.168.1.22", 24, "192.168.1.161"),
            ("192.168.43.1", 24, "192.168.43.77"),
            ("192.168.87.201", 24, "192.168.87.254"),
            ("172.20.10.1", 28, "172.20.10.14"),
        ] {
            let origins = interface_probe_origins(&[network(phone, prefix)], 47420, 0);
            assert!(
                origins.contains(&format!("http://{computer}:47420")),
                "{phone}/{prefix}"
            );
            assert!(!origins.contains(&format!("http://{phone}:47420")));
            assert!(origins.len() <= 254);
        }
        let corporate = [network("10.20.30.40", 16)];
        let first = interface_probe_origins(&corporate, 47420, 0);
        assert!(first.contains(&"http://10.20.30.50:47420".into()));
        assert!(!first.contains(&"http://10.20.31.50:47420".into()));
        let second = interface_probe_origins(&corporate, 47420, 1);
        assert!(second.contains(&"http://10.20.31.50:47420".into()));
        assert!(second.contains(&"http://10.20.31.0:47420".into()));
        for ip in ["203.0.113.5", "100.64.0.10"] {
            assert!(interface_probe_origins(&[network(ip, 24)], 47420, 0).is_empty());
        }
        assert!(interface_probe_origins(&[], 47420, 0).is_empty());
    }

    #[test]
    fn broad_masks_stay_private_without_dropping_real_hosts_at_private_block_edges() {
        for (ip, prefix, last_host, wrapped_host, actual_network, actual_broadcast) in [
            (
                "172.31.255.42",
                8,
                "172.31.255.255",
                "172.16.0.0",
                "172.0.0.0",
                "172.255.255.255",
            ),
            (
                "192.168.255.42",
                8,
                "192.168.255.255",
                "192.168.0.0",
                "192.0.0.0",
                "192.255.255.255",
            ),
            (
                "192.168.255.42",
                15,
                "192.168.255.255",
                "192.168.0.1",
                "192.168.0.0",
                "192.169.255.255",
            ),
            (
                "10.255.255.42",
                8,
                "10.255.255.254",
                "10.0.0.1",
                "10.0.0.0",
                "10.255.255.255",
            ),
        ] {
            let networks = [LocalNetwork {
                name: "en0".into(),
                ip: ip.parse().unwrap(),
                prefix: Some(prefix),
            }];
            let initial = interface_probe_origins(&networks, 47420, 0);
            let wrapped = interface_probe_origins(&networks, 47420, 1);
            assert!(
                initial.contains(&format!("http://{last_host}:47420")),
                "lost valid host {last_host}/{prefix}"
            );
            assert!(
                wrapped.contains(&format!("http://{wrapped_host}:47420")),
                "lost valid host {wrapped_host}/{prefix}"
            );
            for round in [0, 1, 255, 256, 4095, 4096, u32::MAX] {
                let origins = interface_probe_origins(&networks, 47420, round);
                assert!(!origins.is_empty());
                assert!(origins.len() <= 256);
                for origin in origins {
                    let candidate: Ipv4Addr = url::Url::parse(&origin)
                        .unwrap()
                        .host_str()
                        .unwrap()
                        .parse()
                        .unwrap();
                    assert!(
                        candidate.is_private(),
                        "broad mask leaked a public probe: {origin}"
                    );
                    assert_ne!(candidate.to_string(), actual_network);
                    assert_ne!(candidate.to_string(), actual_broadcast);
                    assert_ne!(candidate.to_string(), ip);
                }
            }
        }
    }

    #[test]
    fn maximum_probe_round_wraps_pages_without_overflow() {
        for (ip, prefix, expected) in [
            ("10.20.30.40", 8, "10.148.30.1"),
            ("172.20.30.40", 12, "172.28.30.1"),
            ("192.168.30.40", 16, "192.168.158.1"),
            ("192.168.30.40", 24, "192.168.30.1"),
            ("192.168.30.42", 30, "192.168.30.41"),
        ] {
            let networks = [LocalNetwork {
                name: "en0".into(),
                ip: ip.parse().unwrap(),
                prefix: Some(prefix),
            }];
            let origins = interface_probe_origins(&networks, 47420, u32::MAX);
            assert!(
                origins.contains(&format!("http://{expected}:47420")),
                "wrong last-round page for {ip}/{prefix}"
            );
            assert!(!origins.contains(&format!("http://{ip}:47420")));
        }
    }

    #[test]
    fn probe_groups_visit_every_interface_and_advance_every_network_page() {
        let networks: Vec<_> = (1..=9)
            .map(|id| LocalNetwork {
                name: format!("en{id}"),
                ip: format!("10.{id}.7.42").parse().unwrap(),
                prefix: Some(16),
            })
            .collect();
        let mut observed = std::collections::HashMap::<u8, Vec<u8>>::new();
        for round in 0..9 {
            let origins = interface_probe_origins(&networks, 47420, round);
            assert!(origins.len() <= 1024);
            let mut pages = std::collections::HashSet::new();
            for origin in origins {
                let ip: Ipv4Addr = url::Url::parse(&origin)
                    .unwrap()
                    .host_str()
                    .unwrap()
                    .parse()
                    .unwrap();
                assert!(!networks.iter().any(|network| network.ip == ip));
                pages.insert((ip.octets()[1], ip.octets()[2]));
            }
            assert!(pages.len() <= 4);
            for (network, page) in pages {
                observed.entry(network).or_default().push(page);
            }
        }
        assert_eq!(observed.len(), 9, "low-priority interfaces must not starve");
        for pages in observed.values() {
            assert_eq!(
                pages,
                &[7, 8, 6],
                "each group must advance after a complete rotation"
            );
        }
    }

    #[test]
    fn subnet_pages_visit_both_neighbors_early_and_cover_a_full_cycle_without_repeats() {
        let page = |origins: &[String]| {
            let ip: Ipv4Addr = url::Url::parse(&origins[0])
                .unwrap()
                .host_str()
                .unwrap()
                .parse()
                .unwrap();
            ip.octets()[2]
        };
        for (prefix, first_page, last_page) in [(16, 0, 255), (21, 24, 31), (23, 30, 31)] {
            let networks = [LocalNetwork {
                name: "en0".into(),
                ip: "10.20.30.40".parse().unwrap(),
                prefix: Some(prefix),
            }];
            let pages = u32::from(last_page - first_page) + 1;
            assert_eq!(page(&interface_probe_origins(&networks, 47420, 0)), 30);
            assert_eq!(page(&interface_probe_origins(&networks, 47420, 1)), 31);
            if pages > 2 {
                assert_eq!(page(&interface_probe_origins(&networks, 47420, 2)), 29);
            }
            let mut visited = std::collections::HashSet::new();
            for round in 0..pages {
                let origins = interface_probe_origins(&networks, 47420, round);
                assert!(origins.len() <= 256);
                let page = page(&origins);
                assert!((first_page..=last_page).contains(&page));
                assert!(
                    visited.insert(page),
                    "repeated page {page} before completing /{prefix}"
                );
            }
            assert_eq!(visited.len(), pages as usize);
            assert_eq!(page(&interface_probe_origins(&networks, 47420, pages)), 30);
        }
    }

    #[test]
    fn interface_watch_ignores_order_and_reports_added_or_lost_addresses() {
        let network = |ip: &str| LocalNetwork {
            name: "en0".into(),
            ip: ip.parse().unwrap(),
            prefix: Some(24),
        };
        let mut watch = InterfaceWatch::new(vec![network("192.168.1.22"), network("10.0.0.5")]);
        assert!(!watch.observe(vec![network("10.0.0.5"), network("192.168.1.22")]));
        assert!(watch.observe(vec![network("10.0.0.5")]));
        assert!(!watch.observe(vec![network("10.0.0.5"), network("10.0.0.5")]));
        assert!(watch.observe(vec![network("10.0.0.5"), network("172.20.10.3")]));
        assert_eq!(
            watch.networks(),
            [network("10.0.0.5"), network("172.20.10.3")]
        );
    }

    #[test]
    fn pairing_addresses_prefer_wifi_at_home_and_work_without_losing_shared_networks() {
        for wifi in ["192.168.31.42", "192.168.1.161"] {
            let networks = [
                LocalNetwork {
                    name: "bridge100".into(),
                    ip: "192.168.139.3".parse().unwrap(),
                    prefix: Some(23),
                },
                LocalNetwork {
                    name: "en0".into(),
                    ip: wifi.parse().unwrap(),
                    prefix: Some(24),
                },
                LocalNetwork {
                    name: "utun4".into(),
                    ip: "10.0.0.1".parse().unwrap(),
                    prefix: Some(32),
                },
            ];
            let addrs = pairing_addrs(&networks);
            assert_eq!(addrs, [wifi, "192.168.139.3", "10.0.0.1"]);
            // Internet Sharing can be the only route to the phone.
            assert_eq!(pairing_addrs(&networks[..1]), ["192.168.139.3"]);
        }
    }

    #[test]
    fn resolved_hosts_keep_deduplicated_alternatives_ranked_by_the_receiving_subnet() {
        for (local, prefix, addresses, expected) in [
            (
                "192.168.1.22",
                24,
                "192.168.139.3,192.168.1.161,fd00::2",
                vec!["192.168.1.161", "192.168.139.3", "fd00::2"],
            ),
            (
                "10.20.30.40",
                16,
                "192.168.139.3,10.20.90.2",
                vec!["10.20.90.2", "192.168.139.3"],
            ),
            (
                "192.168.1.200",
                25,
                "192.168.1.2,192.168.1.161",
                vec!["192.168.1.161", "192.168.1.2"],
            ),
        ] {
            let info = ServiceInfo::new(
                SERVICE_TYPE,
                "workstation",
                "workstation.local.",
                addresses,
                47420,
                &[
                    ("host_id", "workstation"),
                    ("name", "Workstation"),
                    ("port", "47420"),
                ][..],
            )
            .unwrap();
            let interfaces = [
                LocalNetwork {
                    name: "bridge100".into(),
                    ip: "192.168.139.1".parse().unwrap(),
                    prefix: Some(24),
                },
                LocalNetwork {
                    name: "en0".into(),
                    ip: local.parse().unwrap(),
                    prefix: Some(prefix),
                },
            ];
            let mut beacons = Vec::new();
            for _ in 0..2 {
                collect_resolved_beacons(
                    &mut beacons,
                    info.get_properties(),
                    info.get_port(),
                    info.get_addresses().iter().copied(),
                    &interfaces,
                );
            }
            assert_eq!(
                beacons
                    .iter()
                    .map(|beacon| beacon.addr.as_str())
                    .collect::<Vec<_>>(),
                expected,
                "{local}/{prefix}"
            );
        }
    }
}
