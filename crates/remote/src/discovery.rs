//! DNS-SD discovery is an address hint, never authorization to connect.
use mdns_sd::ServiceDaemon;
#[cfg(feature = "client")]
use mdns_sd::ServiceEvent;
#[cfg(any(feature = "server", test))]
use mdns_sd::ServiceInfo;
use serde::{Deserialize, Serialize};
use std::time::Duration;
#[cfg(feature = "client")]
use std::time::Instant;

pub const SERVICE_TYPE: &str = "_tcode._tcp.local.";

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
                for address in info.get_addresses() {
                    if address.to_string().starts_with("127.") || address.to_string() == "::1" {
                        continue;
                    }
                    if let Some(beacon) =
                        parse_txt(info.get_properties(), info.get_port(), address.to_string())
                        && found.len() < 128
                    {
                        let prefer = found
                            .get(&beacon.host_id)
                            .is_none_or(|old| old.addr.contains(':') && !beacon.addr.contains(':'));
                        if prefer {
                            found.insert(beacon.host_id.clone(), beacon);
                        }
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
}
