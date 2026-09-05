//! DNS-SD discovery is an address hint, never authorization to connect.
use mdns_sd::ServiceDaemon;
#[cfg(any(feature = "server", test))]
use mdns_sd::ServiceInfo;
#[cfg(feature = "client")]
use mdns_sd::{ScopedIp, ServiceEvent};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::time::Duration;
#[cfg(feature = "client")]
use std::time::Instant;

pub const SERVICE_TYPE: &str = "_tcode._tcp.local.";

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalInterface {
    pub name: String,
    pub addr: IpAddr,
}

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

fn is_link_local(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => address.is_link_local(),
        IpAddr::V6(address) => address.is_unicast_link_local(),
    }
}

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
    pub fp: String,
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
    fp: impl Into<String>,
) -> BeaconHandle {
    let host_id = host_id.into();
    let name = name.into();
    let fp = fp.into();
    let mut handle = BeaconHandle {
        daemon: None,
        fullname: String::new(),
    };
    let start = || -> Result<(ServiceDaemon, String), mdns_sd::Error> {
        let props = [
            ("host_id", host_id.as_str()),
            ("name", name.as_str()),
            ("port", &port.to_string()),
            ("fp", fp.as_str()),
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
    let fp = field("fp")?;
    if port == 0
        || field("port")?.parse::<u16>().ok()? != port
        || !tcode_client::pairing::valid_fingerprint(&fp)
    {
        return None;
    }
    Some(Beacon {
        host_id,
        name,
        fp,
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
            ("fp", &"ab".repeat(32)),
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
            ("fp", &"ab".repeat(32)),
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
        let malformed = [
            ("host_id", "test"),
            ("name", "Test"),
            ("port", "47420"),
            ("fp", "invalid"),
        ];
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
