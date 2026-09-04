use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AuthStore {
    pub host_id: Uuid,
    pub host_name: String,
    #[serde(default)]
    pub devices: Vec<Device>,
    #[serde(skip)]
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Device {
    pub id: Uuid,
    pub name: String,
    pub token_sha256_hex: String,
    pub created_unix: u64,
}

impl AuthStore {
    pub fn open(data_dir: &Path, host_name: &str) -> io::Result<Self> {
        fs::create_dir_all(data_dir)?;
        let path = data_dir.join("remote.json");
        match fs::read(&path) {
            Ok(bytes) => {
                let mut store: Self = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
                store.path = path;
                if store.host_name != host_name {
                    store.host_name = host_name.to_owned();
                    store.save()?;
                }
                Ok(store)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let store = Self {
                    host_id: Uuid::new_v4(),
                    host_name: host_name.to_owned(),
                    devices: Vec::new(),
                    path,
                };
                store.save()?;
                Ok(store)
            }
            Err(error) => Err(error),
        }
    }

    pub fn issue_token(&mut self, device_name: String) -> io::Result<String> {
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes).map_err(io::Error::other)?;
        let token = URL_SAFE_NO_PAD.encode(bytes);
        self.devices.push(Device {
            id: Uuid::new_v4(),
            name: device_name,
            token_sha256_hex: hex_hash(token.as_bytes()),
            created_unix: unix_now(),
        });
        self.save()?;
        Ok(token)
    }

    /// Drop a paired device by id. Returns whether anything was removed.
    pub fn revoke(&mut self, id: &str) -> io::Result<bool> {
        let before = self.devices.len();
        self.devices.retain(|device| device.id.to_string() != id);
        if self.devices.len() == before {
            return Ok(false);
        }
        self.save()?;
        Ok(true)
    }

    pub fn token_is_valid(&self, token: &str) -> bool {
        let candidate = Sha256::digest(token.as_bytes());
        self.devices.iter().any(|device| {
            let Some(expected) = decode_hex(&device.token_sha256_hex) else {
                return false;
            };
            constant_time_eq(candidate.as_slice(), &expected)
        })
    }

    fn save(&self) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        let temporary = self.path.with_extension("json.tmp");
        let mut options = OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(temporary, &self.path)
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn hex_hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(pair, 16).ok()
        })
        .collect()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}
