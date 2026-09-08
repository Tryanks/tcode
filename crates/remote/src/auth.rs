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
    #[serde(default)]
    password: Option<PasswordHash>,
    #[serde(default = "enabled_by_default")]
    pub pairing_enabled: bool,
    #[serde(skip)]
    failures: u8,
    #[serde(skip)]
    locked_until: Option<std::time::Instant>,
    #[serde(skip)]
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PasswordHash {
    salt: [u8; 16],
    hash: [u8; 32],
    iterations: u32,
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
                    password: None,
                    pairing_enabled: true,
                    failures: 0,
                    locked_until: None,
                    path,
                };
                store.save()?;
                Ok(store)
            }
            Err(error) => Err(error),
        }
    }

    pub fn password_configured(&self) -> bool {
        self.password.is_some()
    }

    pub fn set_password(&mut self, password: &str, revoke_tokens: bool) -> io::Result<()> {
        if password.chars().count() < 8 || password.len() > 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "password must contain at least 8 characters and at most 1024 bytes",
            ));
        }
        let mut salt = [0; 16];
        getrandom::fill(&mut salt).map_err(io::Error::other)?;
        let mut hash = [0; 32];
        pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), &salt, 600_000, &mut hash);
        // Persist before committing the in-memory change.
        let mut updated = self.clone();
        updated.password = Some(PasswordHash {
            salt,
            hash,
            iterations: 600_000,
        });
        updated.failures = 0;
        updated.locked_until = None;
        if revoke_tokens {
            updated.devices.clear();
        }
        updated.save()?;
        *self = updated;
        Ok(())
    }

    pub fn verify_password(&mut self, password: &str) -> bool {
        if let Some(until) = self.locked_until {
            if std::time::Instant::now() < until {
                return false;
            }
            self.locked_until = None;
            self.failures = 0;
        }
        let Some(expected) = &self.password else {
            return false;
        };
        let mut hash = [0; 32];
        if password.len() <= 1024 {
            pbkdf2::pbkdf2_hmac::<Sha256>(
                password.as_bytes(),
                &expected.salt,
                expected.iterations,
                &mut hash,
            );
        }
        if password.len() <= 1024 && constant_time_eq(&hash, &expected.hash) {
            self.failures = 0;
            return true;
        }
        self.failures += 1;
        if self.failures >= 5 {
            self.locked_until =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(300));
        }
        false
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
        if token.len() != 43 {
            return false;
        }
        let candidate = Sha256::digest(token.as_bytes());
        self.devices.iter().any(|device| {
            let Some(expected) = decode_hex(&device.token_sha256_hex) else {
                return false;
            };
            constant_time_eq(candidate.as_slice(), &expected)
        })
    }

    pub(crate) fn save(&self) -> io::Result<()> {
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

pub(crate) fn hex_hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    let (pairs, _) = value.as_bytes().as_chunks::<2>();
    pairs
        .iter()
        .map(|pair| {
            let pair = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(pair, 16).ok()
        })
        .collect()
}

pub(crate) fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
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

fn enabled_by_default() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn password_hash_persists_and_password_changes_keep_or_revoke_tokens() {
        let root = std::env::temp_dir().join(format!("tcode-password-{}", Uuid::new_v4()));
        let mut auth = AuthStore::open(&root, "host").unwrap();
        let token = auth.issue_token("existing phone".into()).unwrap();
        assert!(!auth.verify_password("password"));
        assert!(auth.set_password("short", false).is_err());
        auth.set_password("test password", false).unwrap();
        let first_salt = auth.password.as_ref().unwrap().salt;
        let bytes = fs::read_to_string(root.join("remote.json")).unwrap();
        assert!(!bytes.contains("test password"));
        assert!(!bytes.contains(&token));
        assert!(bytes.contains("600000"));
        let mut auth = AuthStore::open(&root, "host").unwrap();
        assert!(auth.verify_password("test password"));
        assert!(auth.token_is_valid(&token));
        auth.set_password("new password", false).unwrap();
        assert_ne!(first_salt, auth.password.as_ref().unwrap().salt);
        assert!(!auth.verify_password("test password"));
        assert!(auth.verify_password("new password"));
        assert!(auth.token_is_valid(&token));
        auth.set_password("last password", true).unwrap();
        assert!(!auth.token_is_valid(&token));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn five_wrong_passwords_lock_out_even_the_right_password_then_recover() {
        let root = std::env::temp_dir().join(format!("tcode-lockout-{}", Uuid::new_v4()));
        let mut auth = AuthStore::open(&root, "host").unwrap();
        auth.set_password("correct password", false).unwrap();
        for _ in 0..5 {
            assert!(!auth.verify_password("wrong password"));
        }
        assert!(!auth.verify_password("correct password"));
        auth.locked_until = Some(std::time::Instant::now());
        assert!(auth.verify_password("correct password"));
        assert!(!auth.verify_password("wrong password"));
        assert!(auth.verify_password("correct password"));
        fs::remove_dir_all(root).unwrap();
    }
}
