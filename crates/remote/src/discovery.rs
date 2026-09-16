//! Address hints for reaching a machine: DNS-SD, this device's own
//! interfaces and the gateways they imply. A hint is never authorization to
//! connect; the transport verifies the machine identity at hello.
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

/// One address of this device with its IPv4 prefix length, when known.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LocalNetwork {
    pub ip: IpAddr,
    pub prefix: Option<u8>,
}

/// This device's usable addresses: no loopback, no link-local, sorted and
/// deduplicated so two snapshots compare as sets.
pub fn local_networks() -> Vec<LocalNetwork> {
    let mut networks: Vec<_> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|interface| !interface.is_loopback())
        .filter_map(|interface| match interface.addr {
            if_addrs::IfAddr::V4(addr) if !addr.ip.is_link_local() => Some(LocalNetwork {
                ip: addr.ip.into(),
                prefix: Some(addr.prefixlen),
            }),
            if_addrs::IfAddr::V6(addr) if !addr.ip.is_unicast_link_local() => Some(LocalNetwork {
                ip: addr.ip.into(),
                prefix: None,
            }),
            _ => None,
        })
        .collect();
    networks.sort();
    networks.dedup();
    networks
}

/// Addresses other devices can reach this machine at, as strings for pairing
/// codes and hello.
pub fn local_addrs() -> Vec<String> {
    let mut addrs: Vec<_> = local_networks()
        .into_iter()
        .map(|network| network.ip.to_string())
        .collect();
    addrs.sort();
    addrs.dedup();
    addrs
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

/// Private subnets that only exist while one device shares its connection.
/// The device hosting the hotspot is the gateway, so its `.1` is the machine
/// when the laptop shares to the phone, and worth a probe either way.
#[cfg(feature = "client")]
const HOTSPOT_NETWORKS: [(Ipv4Addr, u8); 4] = [
    (Ipv4Addr::new(172, 20, 10, 0), 28),   // iOS Personal Hotspot
    (Ipv4Addr::new(192, 168, 43, 0), 24),  // Android hotspot
    (Ipv4Addr::new(192, 168, 2, 0), 24),   // macOS Internet Sharing
    (Ipv4Addr::new(192, 168, 137, 0), 24), // Windows Mobile Hotspot
];

/// Origins worth probing for a machine listening on `port`, derived from
/// this device's interfaces alone: the gateway (`.1`) of every hotspot subnet
/// this device sits in and of every private IPv4 network no larger than a
/// /24, plus every other host of a tiny private network (a /28 or smaller,
/// such as the hotspot this device shares to the machine). A wrong guess
/// costs one refused connection; the transport verifies the machine identity
/// before trusting any answer.
#[cfg(feature = "client")]
pub fn interface_probe_origins(networks: &[LocalNetwork], port: u16) -> Vec<String> {
    let mut hosts = Vec::new();
    for network in networks {
        let IpAddr::V4(ip) = network.ip else {
            continue;
        };
        let hotspot = HOTSPOT_NETWORKS
            .iter()
            .find(|(subnet, prefix)| ipv4_network(ip, *prefix) == *subnet)
            .map(|(subnet, _)| *subnet);
        let private = network
            .prefix
            .filter(|prefix| (24..=30).contains(prefix) && ip.is_private())
            .map(|prefix| ipv4_network(ip, prefix));
        for subnet in hotspot.into_iter().chain(private) {
            hosts.push(Ipv4Addr::from(u32::from(subnet) + 1));
        }
        if let Some(prefix) = network.prefix.filter(|prefix| (28..=30).contains(prefix))
            && ip.is_private()
        {
            let subnet = u32::from(ipv4_network(ip, prefix));
            let hosts_in_subnet = (1_u32 << (32 - u32::from(prefix))) - 2;
            hosts.extend((1..=hosts_in_subnet).map(|offset| Ipv4Addr::from(subnet + offset)));
        }
    }
    let mut origins = Vec::new();
    for host in hosts {
        if networks.iter().any(|network| network.ip == host) {
            continue;
        }
        let origin = tcode_client::pairing::lan_origin(&host.to_string(), port);
        if !origins.contains(&origin) {
            origins.push(origin);
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
#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalInterface {
    pub name: String,
    pub addr: IpAddr,
}

#[cfg(feature = "client")]
/// Lower values are better. A private IPv4 address on the receiving LAN wins
/// over addresses advertised for virtual bridges on the execution host.
fn address_preference(address: &str, local_interfaces: &[LocalInterface]) -> u8 {
    let Ok(address) = address.parse::<IpAddr>() else {
        return 5;
    };
    if address.is_loopback() || is_link_local(address) {
        return 4;
    }
    match address {
        IpAddr::V4(address) => {
            let same_lan = local_interfaces.iter().any(|interface| {
                !is_virtual_bridge(&interface.name)
                    && !interface.addr.is_loopback()
                    && !is_link_local(interface.addr)
                    && matches!(interface.addr, IpAddr::V4(local) if local.octets()[..3] == address.octets()[..3])
            });
            if same_lan {
                0
            } else if address.is_private() {
                1
            } else {
                2
            }
        }
        IpAddr::V6(_) => 3,
    }
}

#[cfg(feature = "client")]
fn is_link_local(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => address.is_link_local(),
        IpAddr::V6(address) => address.is_unicast_link_local(),
    }
}

#[cfg(feature = "client")]
fn is_virtual_bridge(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.contains("bridge")
        || name.contains("vmnet")
        || name.contains("vboxnet")
        || name.contains("docker")
        || name.starts_with("virbr")
        || name.starts_with("br-")
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
    let mut found = std::collections::BTreeMap::<String, Beacon>::new();
    if let Ok(events) = daemon.browse(SERVICE_TYPE) {
        let deadline = Instant::now() + timeout.min(Duration::from_secs(30));
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            let Ok(event) = events.recv_timeout(left) else {
                break;
            };
            if let ServiceEvent::ServiceResolved(info) = event {
                let local_interfaces = receiving_interfaces(info.get_addresses());
                let preferred = info
                    .get_addresses()
                    .iter()
                    .filter(|address| !address.is_loopback())
                    .min_by_key(|address| {
                        address_preference(&address.to_string(), &local_interfaces)
                    });
                if let Some(address) = preferred
                    && let Some(beacon) =
                        parse_txt(info.get_properties(), info.get_port(), address.to_string())
                    && (found.len() < 128 || found.contains_key(&beacon.host_id))
                {
                    let prefer = found.get(&beacon.host_id).is_none_or(|old| {
                        address_preference(&beacon.addr, &local_interfaces)
                            < address_preference(&old.addr, &local_interfaces)
                    });
                    if prefer {
                        found.insert(beacon.host_id.clone(), beacon);
                    }
                }
            }
        }
        let _ = daemon.stop_browse(SERVICE_TYPE);
    }
    let _ = daemon.shutdown();
    found.into_values().collect()
}

#[cfg(feature = "client")]
fn receiving_interfaces(addresses: &std::collections::HashSet<ScopedIp>) -> Vec<LocalInterface> {
    let mut interfaces = Vec::new();
    for address in addresses {
        let ids: Vec<_> = match address {
            ScopedIp::V4(address) => address.interface_ids().iter().collect(),
            ScopedIp::V6(address) => vec![address.scope_id()],
            _ => Vec::new(),
        };
        for id in ids {
            interfaces.extend(id.get_addrs().into_iter().map(|addr| LocalInterface {
                name: id.name.clone(),
                addr,
            }));
        }
    }
    interfaces.sort_by(|a, b| (&a.name, a.addr).cmp(&(&b.name, b.addr)));
    interfaces.dedup();
    interfaces
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
    #[ignore = "requires local multicast; run explicitly during native acceptance"]
    fn mdns_loopback_round_trip() {
        let advertise = ServiceDaemon::new().unwrap();
        let browser = ServiceDaemon::new().unwrap();
        for daemon in [&advertise, &browser] {
            daemon.disable_interface(mdns_sd::IfKind::All).unwrap();
            daemon
                .enable_interface(mdns_sd::IfKind::LoopbackV4)
                .unwrap();
            daemon.set_multicast_loop_v4(true).unwrap();
        }
        let name = format!("p4c-{}", std::process::id());
        let props = [
            ("host_id", name.as_str()),
            ("name", "Loopback"),
            ("port", "47420"),
        ];
        let mut info = ServiceInfo::new(
            SERVICE_TYPE,
            &name,
            &format!("{name}.local."),
            "127.0.0.1",
            47420,
            &props[..],
        )
        .unwrap();
        info.set_requires_probe(false);
        let fullname = info.get_fullname().to_owned();
        let events = browser.browse(SERVICE_TYPE).unwrap();
        advertise.register(info).unwrap();
        let deadline = Instant::now() + Duration::from_secs(6);
        let mut found = false;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            let Ok(event) = events.recv_timeout(left) else {
                break;
            };
            if let ServiceEvent::ServiceResolved(info) = event
                && info.get_fullname() == fullname
            {
                found =
                    parse_txt(info.get_properties(), info.get_port(), "127.0.0.1".into()).is_some();
                break;
            }
        }
        let _ = advertise.unregister(&fullname);
        let _ = browser.stop_browse(SERVICE_TYPE);
        let _ = advertise.shutdown();
        let _ = browser.shutdown();
        assert!(
            found,
            "loopback multicast unavailable; TXT validation remains the portable test"
        );
    }

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
    fn interface_probes_cover_hotspot_gateways_small_private_networks_and_tiny_subnets() {
        let network = |ip: &str, prefix: Option<u8>| LocalNetwork {
            ip: ip.parse().unwrap(),
            prefix,
        };
        let networks = [
            // Phone on an iPhone hotspot hosted by another device.
            network("172.20.10.3", Some(28)),
            // Laptop on an Android hotspot that handed out a wide mask.
            network("192.168.43.17", Some(16)),
            // Ordinary home LAN.
            network("192.168.1.22", Some(24)),
            // Same LAN seen again on a second interface.
            network("192.168.1.23", Some(24)),
            // This device hosts a Windows Mobile Hotspot: it is the gateway.
            network("192.168.137.1", Some(24)),
            // Corporate /16 is too large to guess a gateway for.
            network("10.20.30.40", Some(16)),
            // Public and overlay (CGNAT) addresses are never probed.
            network("203.0.113.5", Some(24)),
            network("100.64.0.10", Some(10)),
            // IPv6 has no `.1` convention.
            network("fd00::1234", None),
        ];
        let mut expected = vec!["http://172.20.10.1:47420".to_owned()];
        // The /28 hotspot is small enough to try every other host on it.
        expected.extend(
            (2..=14)
                .filter(|host| *host != 3)
                .map(|host| format!("http://172.20.10.{host}:47420")),
        );
        expected.extend([
            "http://192.168.43.1:47420".to_owned(),
            "http://192.168.1.1:47420".to_owned(),
        ]);
        assert_eq!(interface_probe_origins(&networks, 47420), expected);
        // This phone shares its own hotspot: the machine is one of its guests.
        assert_eq!(
            interface_probe_origins(&[network("172.20.10.1", Some(28))], 47420),
            (2..=14)
                .map(|host| format!("http://172.20.10.{host}:47420"))
                .collect::<Vec<_>>()
        );
        assert!(interface_probe_origins(&[], 47420).is_empty());
    }

    #[test]
    fn interface_watch_ignores_order_and_reports_added_or_lost_addresses() {
        let network = |ip: &str| LocalNetwork {
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
    fn address_ranking_ignores_virtual_bridges_and_prefers_the_receiving_lan() {
        let local_interfaces = [
            LocalInterface {
                name: "bridge100".into(),
                addr: "192.168.139.1".parse().unwrap(),
            },
            LocalInterface {
                name: "vmnet8".into(),
                addr: "192.168.215.1".parse().unwrap(),
            },
            LocalInterface {
                name: "en0".into(),
                addr: "192.168.1.22".parse().unwrap(),
            },
        ];
        let mut addresses = ["192.168.139.3", "192.168.215.0", "192.168.1.6"];
        addresses.sort_by_key(|address| address_preference(address, &local_interfaces));
        assert_eq!(addresses[0], "192.168.1.6");
    }
}
