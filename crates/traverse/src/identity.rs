//! Machine and device identities. Each is an iroh secret key; the public
//! `EndpointId` is what pairing records and what the transport authenticates.
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use iroh::{EndpointId, SecretKey};
use serde::{Deserialize, Serialize};

/// One device allowed to open `tcode/1` connections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRecord {
    /// The device's `EndpointId`.
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    pub created_unix: u64,
}

/// `traverse.json`: this machine's key, name, allow list and pairing switch.
#[derive(Debug, Clone)]
pub struct HostIdentity {
    pub host_name: String,
    secret_key: SecretKey,
    pub devices: Vec<DeviceRecord>,
    pub pairing_enabled: bool,
    path: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct HostFile {
    v: u32,
    host_name: String,
    secret_key: String,
    #[serde(default)]
    devices: Vec<DeviceRecord>,
    #[serde(default = "enabled")]
    pairing_enabled: bool,
}

fn enabled() -> bool {
    true
}

pub const HOST_FILE: &str = "traverse.json";

impl HostIdentity {
    /// Load the machine identity, or create one. A `remote.json` written by
    /// the retired transport donates its identity seed so the machine keeps
    /// the key it already had; devices must pair again either way.
    pub fn load_or_create(data_dir: &Path, host_name: &str) -> io::Result<Self> {
        fs::create_dir_all(data_dir)?;
        let path = data_dir.join(HOST_FILE);
        match fs::read(&path) {
            Ok(bytes) => {
                let file: HostFile = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
                if file.v != 2 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{HOST_FILE} has unsupported version {}", file.v),
                    ));
                }
                let mut identity = Self {
                    host_name: file.host_name,
                    secret_key: parse_secret_key(&file.secret_key)?,
                    devices: file.devices,
                    pairing_enabled: file.pairing_enabled,
                    path,
                };
                if identity.host_name != host_name {
                    identity.host_name = host_name.to_owned();
                    identity.save()?;
                }
                Ok(identity)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let secret_key = legacy_seed(data_dir)
                    .map(|seed| SecretKey::from_bytes(&seed))
                    .unwrap_or_else(SecretKey::generate);
                let identity = Self {
                    host_name: host_name.to_owned(),
                    secret_key,
                    devices: Vec::new(),
                    pairing_enabled: true,
                    path,
                };
                identity.save()?;
                Ok(identity)
            }
            Err(error) => Err(error),
        }
    }

    pub fn secret_key(&self) -> &SecretKey {
        &self.secret_key
    }

    pub fn endpoint_id(&self) -> EndpointId {
        self.secret_key.public()
    }

    pub fn save(&self) -> io::Result<()> {
        let file = HostFile {
            v: 2,
            host_name: self.host_name.clone(),
            secret_key: encode_hex(&self.secret_key.to_bytes()),
            devices: self.devices.clone(),
            pairing_enabled: self.pairing_enabled,
        };
        write_private(&self.path, &serde_json::to_vec_pretty(&file)?)
    }

    pub fn is_paired(&self, id: &EndpointId) -> bool {
        let id = id.to_string();
        self.devices.iter().any(|device| device.id == id)
    }

    /// Admit a device, keeping the original pairing time when it pairs again.
    pub fn admit(&mut self, id: &EndpointId, name: String, platform: Option<String>) {
        let id = id.to_string();
        match self.devices.iter_mut().find(|device| device.id == id) {
            Some(device) => {
                device.name = name;
                device.platform = platform;
            }
            None => self.devices.push(DeviceRecord {
                id,
                name,
                platform,
                created_unix: now_unix(),
            }),
        }
    }

    /// Remove a device from the allow list. Returns whether it was listed.
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.devices.len();
        self.devices.retain(|device| device.id != id);
        self.devices.len() != before
    }
}

fn legacy_seed(data_dir: &Path) -> Option<[u8; 32]> {
    let bytes = fs::read(data_dir.join("remote.json")).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    serde_json::from_value(value.get("identity_seed")?.clone()).ok()
}

/// `device.json`: this device's key and how it introduces itself. Cloning
/// shares one identity; the client endpoint built on it is shared too.
#[derive(Clone)]
pub struct DeviceIdentity {
    inner: Arc<DeviceInner>,
}

pub(crate) struct DeviceInner {
    pub(crate) secret_key: SecretKey,
    pub(crate) data_dir: PathBuf,
    details: Mutex<Details>,
    pub(crate) endpoint: tokio::sync::OnceCell<crate::client::ClientEndpoint>,
    pub(crate) options: Mutex<EndpointOptions>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Details {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    platform: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct DeviceFile {
    v: u32,
    secret_key: String,
    #[serde(flatten)]
    details: Details,
}

/// How a device endpoint reaches the wider network. Tests turn it off to
/// stay on loopback; phones keep it on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndpointOptions {
    /// Use the official relay and lookup services for machines that publish
    /// to them.
    pub official: bool,
}

impl Default for EndpointOptions {
    fn default() -> Self {
        Self { official: true }
    }
}

pub const DEVICE_FILE: &str = "device.json";

impl DeviceIdentity {
    pub fn load_or_create(data_dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(data_dir)?;
        let path = data_dir.join(DEVICE_FILE);
        let (secret_key, details) = match fs::read(&path) {
            Ok(bytes) => {
                let file: DeviceFile = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
                if file.v != 1 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{DEVICE_FILE} has unsupported version {}", file.v),
                    ));
                }
                (parse_secret_key(&file.secret_key)?, file.details)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let secret_key = SecretKey::generate();
                let file = DeviceFile {
                    v: 1,
                    secret_key: encode_hex(&secret_key.to_bytes()),
                    details: Details::default(),
                };
                write_private(&path, &serde_json::to_vec_pretty(&file)?)?;
                (secret_key, Details::default())
            }
            Err(error) => return Err(error),
        };
        Ok(Self {
            inner: Arc::new(DeviceInner {
                secret_key,
                data_dir: data_dir.to_owned(),
                details: Mutex::new(details),
                endpoint: tokio::sync::OnceCell::new(),
                options: Mutex::new(EndpointOptions::default()),
            }),
        })
    }

    /// Must be set before the first connection; later calls are ignored.
    pub fn with_options(self, options: EndpointOptions) -> Self {
        *self.inner.options.lock().unwrap() = options;
        self
    }

    pub fn endpoint_id(&self) -> EndpointId {
        self.inner.secret_key.public()
    }

    pub fn data_dir(&self) -> &Path {
        &self.inner.data_dir
    }

    /// What this device tells machines about itself.
    pub fn claim(&self) -> crate::wire::DeviceClaim {
        let details = self.inner.details.lock().unwrap();
        crate::wire::DeviceClaim {
            name: details.name.clone().unwrap_or_else(|| "Tcode".into()),
            platform: details.platform.clone(),
        }
    }

    /// Update the name and platform sent with pairing and hello.
    pub fn set_details(&self, name: String, platform: Option<String>) {
        let mut details = self.inner.details.lock().unwrap();
        let next = Details {
            name: Some(name),
            platform,
        };
        if details.name == next.name && details.platform == next.platform {
            return;
        }
        *details = next.clone();
        let file = DeviceFile {
            v: 1,
            secret_key: encode_hex(&self.inner.secret_key.to_bytes()),
            details: next,
        };
        let result = serde_json::to_vec_pretty(&file)
            .map_err(io::Error::other)
            .and_then(|bytes| write_private(&self.inner.data_dir.join(DEVICE_FILE), &bytes));
        if let Err(error) = result {
            log::error!("could not write {DEVICE_FILE}: {error}");
        }
    }

    pub(crate) fn inner(&self) -> &DeviceInner {
        &self.inner
    }
}

pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn parse_secret_key(hex: &str) -> io::Result<SecretKey> {
    let bytes = decode_hex(hex)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid secret key"))?;
    Ok(SecretKey::from_bytes(&bytes))
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_hex(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut bytes = [0; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(bytes)
}

/// Write `bytes` to `path` atomically, readable by the owner only.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temporary = path.with_extension("tmp");
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    use io::Write as _;
    let mut file = options.open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "tcode-traverse-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn host_identity_survives_restart_and_adopts_the_legacy_seed() {
        let dir = temp_dir("host");
        let seed = [7_u8; 32];
        fs::write(
            dir.join("remote.json"),
            serde_json::json!({"host_id":"x","host_name":"Old","identity_seed":seed}).to_string(),
        )
        .unwrap();
        let mut identity = HostIdentity::load_or_create(&dir, "Desk").unwrap();
        assert_eq!(
            identity.endpoint_id(),
            SecretKey::from_bytes(&seed).public(),
            "the machine keeps the key it already had"
        );
        let phone = SecretKey::generate().public();
        identity.admit(&phone, "Phone".into(), Some("Android 15".into()));
        identity.pairing_enabled = false;
        identity.save().unwrap();
        let reopened = HostIdentity::load_or_create(&dir, "Renamed").unwrap();
        assert_eq!(reopened.endpoint_id(), identity.endpoint_id());
        assert_eq!(reopened.host_name, "Renamed");
        assert!(reopened.is_paired(&phone));
        assert!(!reopened.pairing_enabled);
        let stored: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.join(HOST_FILE)).unwrap()).unwrap();
        assert_eq!(stored["v"], 2);
        assert_eq!(stored["devices"][0]["id"], phone.to_string());
        assert_eq!(stored["devices"][0]["platform"], "Android 15");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(dir.join(HOST_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn device_identity_is_stable_and_records_its_details() {
        let dir = temp_dir("device");
        let device = DeviceIdentity::load_or_create(&dir).unwrap();
        device.set_details("Xiaomi 15".into(), Some("Android 15".into()));
        let reopened = DeviceIdentity::load_or_create(&dir).unwrap();
        assert_eq!(reopened.endpoint_id(), device.endpoint_id());
        assert_eq!(reopened.claim().name, "Xiaomi 15");
        assert_eq!(reopened.claim().platform.as_deref(), Some("Android 15"));
        fs::remove_dir_all(dir).unwrap();
    }
}
