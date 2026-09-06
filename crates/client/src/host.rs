//! Platform services shared by every tcode client.
//!
//! This contract deliberately contains no UI-runtime types. Adapters await the
//! returned local futures and marshal their results onto their own UI thread.

use std::{future::Future, pin::Pin};

use serde::{Deserialize, Serialize};

use crate::{ConnectionState, pairing::PairedHost};

/// A future which may remain on the thread that created it.
pub type HostFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// A live link to a host: NDJSON lines in both directions plus connection state.
pub struct Transport {
    pub to_host: async_channel::Sender<String>,
    pub from_host: async_channel::Receiver<String>,
    pub state: async_channel::Receiver<ConnectionState>,
}

/// What the pairing form submits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairRequest {
    pub addr: String,
    pub port: u16,
    pub code: String,
    pub fingerprint: String,
}

/// A host advertised on the client's local network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredHost {
    pub host_id: String,
    pub name: String,
    pub addr: String,
    pub port: u16,
    pub fp: String,
}

/// Preferences which belong to the client and are never sent to the host.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientPreferences {
    pub appearance: Option<String>,
    pub language: Option<String>,
    pub device_name: Option<String>,
}

/// Parse bounded JSON supplied by platform discovery bridges.
pub fn parse_discovered_hosts(json: &str) -> Vec<DiscoveredHost> {
    if json.len() > 65_536 {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(hosts) = value.as_array() else {
        return Vec::new();
    };
    let mut found: Vec<_> = hosts
        .iter()
        .take(128)
        .filter_map(|value| {
            let field = |name| {
                value
                    .get(name)?
                    .as_str()
                    .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
                    .map(str::to_owned)
            };
            let fp = field("fp")?;
            let port = u16::try_from(value.get("port")?.as_u64()?).ok()?;
            if port == 0 || !crate::pairing::valid_fingerprint(&fp) {
                return None;
            }
            Some(DiscoveredHost {
                host_id: field("host_id")?,
                name: field("name")?,
                addr: field("addr")?,
                fp,
                port,
            })
        })
        .collect();
    // One row per host. Native mDNS already ranks by the receiving interface;
    // JSON platform browsers preserve their first, platform-ranked address.
    found.retain(|host| !host.addr.starts_with("127.") && host.addr != "::1");
    found.sort_by_key(|host| (host.host_id.clone(), host.addr.contains(':')));
    found.dedup_by(|a, b| a.host_id == b.host_id);
    found
}

/// Persistence, pairing, transport, and platform facilities for a tcode client.
pub trait ClientHost: 'static {
    /// Name this device presents to hosts while pairing and connecting.
    fn device_name(&self) -> String;

    fn load_preferences(&self) -> ClientPreferences {
        ClientPreferences::default()
    }

    fn save_preferences(&self, _preferences: &ClientPreferences) {}

    fn load_hosts(&self) -> Vec<PairedHost>;
    fn save_hosts(&self, hosts: &[PairedHost]);

    /// `Some(id)` of the host to reconnect to on launch.
    fn last_host_id(&self) -> Option<String>;
    fn set_last_host_id(&self, host_id: Option<&str>);

    /// Browsers can only pair with the origin that served the application.
    fn fixed_pairing_endpoint(&self) -> Option<(String, u16)> {
        None
    }

    fn pair(&self, request: PairRequest) -> HostFuture<'_, Result<PairedHost, String>>;

    /// Open a reconnecting link. Dropping the returned channels ends it.
    fn connect(&self, host: &PairedHost) -> Transport;

    fn browse_hosts(&self) -> HostFuture<'_, Vec<DiscoveredHost>> {
        Box::pin(async { Vec::new() })
    }

    fn supports_qr(&self) -> bool {
        false
    }

    fn scan_qr(&self) -> HostFuture<'_, Result<String, String>> {
        Box::pin(async { Err("unsupported".into()) })
    }

    fn certificate_changed(&self, _host_id: &str) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovered_hosts_are_bounded_validated_and_deduplicated() {
        let fingerprint = "ab".repeat(32);
        let json = serde_json::json!([
            {"host_id":"b","name":"IPv6","addr":"fd00::2","port":47420,"fp":fingerprint},
            {"host_id":"a","name":"Loopback","addr":"127.0.0.1","port":47420,"fp":fingerprint},
            {"host_id":"b","name":"IPv4","addr":"192.168.1.2","port":47420,"fp":fingerprint},
            {"host_id":"c","name":"Bad pin","addr":"192.168.1.3","port":47420,"fp":"no"},
            {"host_id":"d","name":"Bad port","addr":"192.168.1.4","port":0,"fp":fingerprint}
        ]);

        assert_eq!(
            parse_discovered_hosts(&json.to_string()),
            vec![DiscoveredHost {
                host_id: "b".into(),
                name: "IPv4".into(),
                addr: "192.168.1.2".into(),
                port: 47_420,
                fp: fingerprint,
            }]
        );
        assert!(parse_discovered_hosts("not json").is_empty());
        assert!(parse_discovered_hosts(&" ".repeat(65_537)).is_empty());
    }
}
