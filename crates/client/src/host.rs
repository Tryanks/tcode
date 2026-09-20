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
    pub to_host: crate::outgoing::Outgoing,
    pub from_host: async_channel::Receiver<String>,
    pub state: async_channel::Receiver<ConnectionState>,
    /// Native routes follow this attachment's authenticated endpoint even when
    /// persisting it fails. Local and fixed-origin browser transports use None.
    pub current_host: Option<LiveHost>,
}

/// The transport publishes its current pairing before emitting Syncing.
/// Consumers take an atomic snapshot; saved hosts are only restart storage.
#[derive(Clone)]
pub struct LiveHost(std::sync::Arc<std::sync::Mutex<PairedHost>>);

impl LiveHost {
    pub fn new(host: PairedHost) -> Self {
        Self(std::sync::Arc::new(std::sync::Mutex::new(host)))
    }

    pub fn snapshot(&self) -> PairedHost {
        self.0.lock().unwrap().clone()
    }

    /// Called by the owning transport only after authenticating the endpoint.
    pub fn authenticated(&self, host: &PairedHost) {
        *self.0.lock().unwrap() = host.clone();
    }
}

/// What the pairing form submits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairRequest {
    pub origin: String,
    pub code: String,
    pub host_id: Option<String>,
    pub identity_key: Option<String>,
    pub candidates: Vec<String>,
}

/// A host advertised on the client's local network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredHost {
    pub host_id: String,
    pub name: String,
    pub origin: String,
}

/// What a client says about itself when pairing and connecting. Serializes to
/// the `device_id`, `device_name` and `platform` fields shared by `/pair`,
/// `/auth/login` and the websocket hello; the host keeps one device record per
/// `device_id` across repeated pairings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceIdentity {
    #[serde(rename = "device_id")]
    pub id: String,
    #[serde(rename = "device_name")]
    pub name: String,
    /// Operating system name and version, such as `Android 15` or `macOS 26.0`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
}

impl DeviceIdentity {
    /// The `/pair` request body for `code`.
    pub fn pair_body(&self, code: &str) -> String {
        #[derive(Serialize)]
        struct PairBody<'a> {
            code: &'a str,
            #[serde(flatten)]
            device: &'a DeviceIdentity,
        }
        serde_json::to_string(&PairBody { code, device: self }).expect("string fields serialize")
    }

    /// The first websocket line: the version-3 hello as the baseline with
    /// version 4 advertised, the device token, and this identity.
    pub fn hello_line(&self, token: &str) -> String {
        #[derive(Serialize)]
        struct Hello<'a> {
            #[serde(rename = "type")]
            kind: &'static str,
            protocol_version: u32,
            supported_versions: [u32; 2],
            token: &'a str,
            #[serde(flatten)]
            device: &'a DeviceIdentity,
        }
        serde_json::to_string(&Hello {
            kind: "hello",
            protocol_version: 3,
            supported_versions: [3, tcode_protocol::PROTOCOL_VERSION],
            token,
            device: self,
        })
        .expect("string fields serialize")
    }
}

/// Hosts accept a device id of at most this many bytes; longer or
/// control-bearing values are treated as absent.
pub const MAX_DEVICE_ID_LEN: usize = 64;

/// Whether a stored or received device id is one a host will accept.
pub fn valid_device_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_DEVICE_ID_LEN && !id.chars().any(char::is_control)
}

/// The client's persistent device id: the stored value when it is usable,
/// otherwise a freshly minted UUID that `store` persists for the next read.
pub fn persistent_device_id(stored: Option<String>, store: impl FnOnce(&str)) -> String {
    if let Some(id) = stored.filter(|id| valid_device_id(id)) {
        return id;
    }
    let id = uuid::Uuid::new_v4().to_string();
    store(&id);
    id
}

/// Preferences which belong to the client and are never sent to the host.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientPreferences {
    pub appearance: Option<String>,
    pub language: Option<String>,
    pub device_name: Option<String>,
    /// Opaque, client-local UI restoration state. The shell owns its schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub navigation: Option<serde_json::Value>,
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
            let port = u16::try_from(value.get("port")?.as_u64()?).ok()?;
            if port == 0 {
                return None;
            }
            Some(DiscoveredHost {
                host_id: field("host_id")?,
                name: field("name")?,
                origin: crate::pairing::parse_origin(&crate::pairing::lan_origin(
                    &field("addr")?,
                    port,
                ))
                .ok()?,
            })
        })
        .collect();
    // A machine may advertise Wi-Fi, virtual bridge and IPv6 addresses. Keep
    // every distinct origin so an unreachable first choice cannot hide it.
    found.retain(|host| {
        !host.origin.starts_with("http://127.") && !host.origin.starts_with("http://[::1]")
    });
    found.sort_by_key(|host| (host.host_id.clone(), host.origin.contains('[')));
    let mut seen = std::collections::HashSet::new();
    found.retain(|host| seen.insert((host.host_id.clone(), host.origin.clone())));
    found
}

/// Persistence, pairing, transport, and platform facilities for a tcode client.
pub trait ClientHost: 'static {
    /// Name this device presents to hosts while pairing and connecting.
    fn device_name(&self) -> String;

    /// Stable, client-generated id (see [`persistent_device_id`]) so a host
    /// keeps one record for this device however often it pairs again.
    fn device_id(&self) -> String;

    /// Operating system name and version shown next to the device name on hosts.
    fn device_platform(&self) -> Option<String>;

    fn device_identity(&self) -> DeviceIdentity {
        DeviceIdentity {
            id: self.device_id(),
            name: self.device_name(),
            platform: self.device_platform(),
        }
    }

    fn load_preferences(&self) -> ClientPreferences {
        ClientPreferences::default()
    }

    fn save_preferences(&self, _preferences: &ClientPreferences) {}

    fn outbox_storage(&self, _host_id: &str) -> Option<std::sync::Arc<dyn crate::outbox::Storage>> {
        None
    }

    fn load_hosts(&self) -> Vec<PairedHost>;
    fn save_hosts(&self, hosts: &[PairedHost]);

    /// Replace one pairing while preserving unrelated saved machines. Native
    /// adapters make these mutations atomic with transport address updates.
    fn remember_host(&self, host: PairedHost) {
        let mut hosts = self.load_hosts();
        crate::pairing::remember_host(&mut hosts, host);
        self.save_hosts(&hosts);
    }

    fn remove_host(&self, host_id: &str) {
        let mut hosts = self.load_hosts();
        hosts.retain(|host| host.host_id != host_id);
        self.save_hosts(&hosts);
    }

    fn stamp_connected(&self, host_id: &str, timestamp: u64) {
        let mut hosts = self.load_hosts();
        if let Some(host) = hosts.iter_mut().find(|host| host.host_id == host_id) {
            host.last_connected_unix = Some(timestamp);
            self.save_hosts(&hosts);
        }
    }

    /// `Some(id)` of the host to reconnect to on launch.
    fn last_host_id(&self) -> Option<String>;
    fn set_last_host_id(&self, host_id: Option<&str>);

    /// Browsers can only pair with the origin that served the application.
    fn fixed_pairing_endpoint(&self) -> Option<String> {
        None
    }

    fn pair(&self, request: PairRequest) -> HostFuture<'_, Result<PairedHost, String>>;

    /// Open a reconnecting link. Dropping the returned channels ends it.
    fn connect(&self, host: &PairedHost) -> Transport;

    fn browse_hosts(&self) -> HostFuture<'_, Vec<DiscoveredHost>> {
        Box::pin(async { Vec::new() })
    }

    /// LAN origins where the machine `host_id` currently advertises itself,
    /// for the transport to verify and race after an unreachable reconnect
    /// cycle. Browser adapters have a fixed page origin and report none.
    fn discover_origins(&self, _host_id: &str) -> HostFuture<'_, Vec<String>> {
        Box::pin(async { Vec::new() })
    }

    fn supports_qr(&self) -> bool {
        false
    }

    fn scan_qr(&self) -> HostFuture<'_, Result<String, String>> {
        Box::pin(async { Err("unsupported".into()) })
    }

    /// Whether [`ClientHost::deliver_artifact`] can actually hand a produced
    /// file to the user here (a browser download, a share sheet). Views ask
    /// before offering the action, so a client without one shows Copy instead of
    /// a button that silently does nothing.
    fn supports_artifact_delivery(&self) -> bool {
        false
    }

    /// Hand finished bytes to the platform's own delivery path. `Err` is a real
    /// failure worth reporting; callers must check
    /// [`ClientHost::supports_artifact_delivery`] first.
    fn deliver_artifact(&self, _name: &str, _mime: &str, _bytes: &[u8]) -> Result<(), String> {
        Err("this client cannot save files".into())
    }

    /// Open a path in the user's external editor. `None` means this client has
    /// no editor integration; the path is always one this client can reach.
    fn open_in_editor(&self, _path: &std::path::Path) -> Option<Result<(), String>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_and_hello_carry_the_device_fields_hosts_read() {
        let device = DeviceIdentity {
            id: "3f2b8c6e-1d4a-4b9e-8c7d-2a1f0e9d8c7b".into(),
            name: "Xiaomi 15".into(),
            platform: Some("Android 15".into()),
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&device.pair_body("123456")).unwrap(),
            serde_json::json!({
                "code": "123456",
                "device_id": "3f2b8c6e-1d4a-4b9e-8c7d-2a1f0e9d8c7b",
                "device_name": "Xiaomi 15",
                "platform": "Android 15",
            })
        );
        let unknown_platform = DeviceIdentity {
            platform: None,
            ..device
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&unknown_platform.hello_line("token"))
                .unwrap(),
            serde_json::json!({
                "type": "hello",
                "protocol_version": 3,
                "supported_versions": [3, 4],
                "token": "token",
                "device_id": "3f2b8c6e-1d4a-4b9e-8c7d-2a1f0e9d8c7b",
                "device_name": "Xiaomi 15",
            })
        );
    }

    #[test]
    fn device_id_is_reused_when_valid_and_minted_and_stored_otherwise() {
        let stored = std::cell::Cell::new(None);
        let keep = "3f2b8c6e-1d4a-4b9e-8c7d-2a1f0e9d8c7b".to_owned();
        assert_eq!(
            persistent_device_id(Some(keep.clone()), |id| stored.set(Some(id.to_owned()))),
            keep
        );
        assert_eq!(stored.take(), None, "a usable id must not be rewritten");
        for damaged in [
            None,
            Some(String::new()),
            Some("a\u{0}b".into()),
            Some("x".repeat(65)),
        ] {
            let minted = persistent_device_id(damaged, |id| stored.set(Some(id.to_owned())));
            assert!(valid_device_id(&minted));
            assert_eq!(stored.take().as_deref(), Some(minted.as_str()));
        }
    }

    #[test]
    fn discovered_hosts_are_bounded_validated_and_deduplicated() {
        let json = serde_json::json!([
            {"host_id":"b","name":"IPv6","addr":"fd00::2","port":47420},
            {"host_id":"a","name":"Loopback","addr":"127.0.0.1","port":47420},
            {"host_id":"b","name":"IPv4","addr":"192.168.1.2","port":47420},
            {"host_id":"b","name":"IPv4 duplicate","addr":"192.168.1.2","port":47420},
            {"host_id":"b","name":"Virtual bridge","addr":"192.168.139.3","port":47420},
            {"host_id":"d","name":"Bad port","addr":"192.168.1.4","port":0}
        ]);

        assert_eq!(
            parse_discovered_hosts(&json.to_string()),
            vec![
                DiscoveredHost {
                    host_id: "b".into(),
                    name: "IPv4".into(),
                    origin: "http://192.168.1.2:47420".into(),
                },
                DiscoveredHost {
                    host_id: "b".into(),
                    name: "Virtual bridge".into(),
                    origin: "http://192.168.139.3:47420".into(),
                },
                DiscoveredHost {
                    host_id: "b".into(),
                    name: "IPv6".into(),
                    origin: "http://[fd00::2]:47420".into(),
                },
            ]
        );
        assert!(parse_discovered_hosts("not json").is_empty());
        assert!(parse_discovered_hosts("{}").is_empty());
        assert!(parse_discovered_hosts(&" ".repeat(65_537)).is_empty());
        let valid =
            serde_json::json!({"host_id":"h","name":"Desk","addr":"192.168.1.2","port":47420});
        for (field, invalid) in [
            ("host_id", serde_json::json!("")),
            ("name", serde_json::json!("line\nbreak")),
            ("name", serde_json::json!("x".repeat(257))),
            ("addr", serde_json::json!("::1")),
            ("addr", serde_json::json!("user@desk")),
            ("port", serde_json::json!(65536)),
            ("port", serde_json::json!(-1)),
        ] {
            let mut invalid_host = valid.clone();
            invalid_host[field] = invalid;
            assert!(
                parse_discovered_hosts(&serde_json::json!([invalid_host]).to_string()).is_empty(),
                "{field}"
            );
        }
        let many: Vec<_> = (0..129)
            .map(|index| {
                let mut host = valid.clone();
                host["host_id"] = serde_json::json!(format!("host-{index:03}"));
                host
            })
            .collect();
        let bounded = parse_discovered_hosts(&serde_json::to_string(&many).unwrap());
        assert_eq!(bounded.len(), 128);
        assert_eq!(bounded.last().unwrap().host_id, "host-127");
    }
}
