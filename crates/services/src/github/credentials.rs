use super::normalize_host;
use crate::settings::SettingsStore;
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};
use tcode_core::settings::{GitHubCredentialSource, GitHubCredentialStatus, GitHubHostSettings};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialError {
    InvalidHost,
    Disabled,
    CliMissing,
    NotSignedIn,
    CliFailed,
    HostMismatch,
}

impl std::fmt::Display for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GitHub credential unavailable: {self:?}")
    }
}
impl std::error::Error for CredentialError {}

#[derive(Clone)]
pub struct Credential {
    pub(super) host: String,
    pub(super) token: String,
    pub(super) fingerprint: String,
    pub(super) source: GitHubCredentialSource,
}
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("host", &self.host)
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}
impl Credential {
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
    pub fn source(&self) -> GitHubCredentialSource {
        self.source
    }
    fn new(host: String, token: String, source: GitHubCredentialSource) -> Self {
        let fingerprint = format!("{host}:{}", super::digest(&token));
        Self {
            host,
            token,
            fingerprint,
            source,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize, PartialEq, Eq)]
pub struct Identity {
    pub id: u64,
    pub login: String,
}

struct Cached {
    value: Result<Credential, CredentialError>,
    expires: Instant,
}

pub struct Credentials {
    store: SettingsStore,
    environment: BTreeMap<String, String>,
    hosts: RwLock<BTreeMap<String, GitHubHostSettings>>,
    cache: Mutex<HashMap<(String, Option<String>), Cached>>,
}

impl Credentials {
    /// The host's launch environment also supplies PATH for gh (including GUI shell repair).
    pub fn new(
        store: SettingsStore,
        environment: impl IntoIterator<Item = (String, String)>,
    ) -> Arc<Self> {
        let hosts = store.load().github.hosts;
        Arc::new(Self {
            store,
            environment: environment.into_iter().collect(),
            hosts: RwLock::new(hosts),
            cache: Mutex::new(HashMap::new()),
        })
    }

    pub fn configure(&self, hosts: BTreeMap<String, GitHubHostSettings>) {
        *self.hosts.write().unwrap() = hosts;
    }

    pub(super) fn check_enabled(&self, host: &str) -> Result<(), CredentialError> {
        if self
            .hosts
            .read()
            .unwrap()
            .get(host)
            .is_some_and(|choice| !choice.enabled)
        {
            Err(CredentialError::Disabled)
        } else {
            Ok(())
        }
    }

    pub fn get(&self, raw_host: &str) -> Result<Credential, CredentialError> {
        let host = normalize_host(raw_host)?;
        let choice = self
            .hosts
            .read()
            .unwrap()
            .get(&host)
            .cloned()
            .unwrap_or_default();
        if !choice.enabled {
            return Err(CredentialError::Disabled);
        }
        if let Some(token) = self.store.github_token(&host).and_then(nonempty) {
            return Ok(Credential::new(host, token, GitHubCredentialSource::Saved));
        }
        if let Some(token) = self.environment_token(&host) {
            return Ok(Credential::new(host, token, GitHubCredentialSource::Env));
        }
        let key = (host.clone(), choice.account.clone());
        // Holding the lock coalesces keyring lookups; no HTTP runs under this lock.
        let mut cache = self.cache.lock().unwrap();
        cache.retain(|_, value| value.expires > Instant::now());
        if let Some(cached) = cache.get(&key) {
            return cached.value.clone();
        }
        let mut result = self.gh_token(&host, choice.account.as_deref());
        if result == Err(CredentialError::NotSignedIn) && choice.account.is_some() {
            result = self.gh_token(&host, None);
        }
        let value = result.map(|token| Credential::new(host, token, GitHubCredentialSource::Gh));
        let ttl = match &value {
            Ok(_) => 300,
            Err(CredentialError::CliFailed) => 0,
            Err(_) => 10,
        };
        if ttl > 0 {
            if cache.len() >= 32
                && let Some(oldest) = cache
                    .iter()
                    .min_by_key(|(_, value)| value.expires)
                    .map(|(key, _)| key.clone())
            {
                cache.remove(&oldest);
            }
            cache.insert(
                key,
                Cached {
                    value: value.clone(),
                    expires: Instant::now() + Duration::from_secs(ttl),
                },
            );
        }
        value
    }

    pub fn invalidate(&self, host: &str) {
        if let Ok(host) = normalize_host(host) {
            self.cache
                .lock()
                .unwrap()
                .retain(|(cached_host, _), _| cached_host != &host);
        }
    }

    fn environment_token(&self, host: &str) -> Option<String> {
        let keys = if host == "github.com" || host.ends_with(".ghe.com") {
            ["GH_TOKEN", "GITHUB_TOKEN"]
        } else if self
            .environment
            .get("GH_HOST")
            .and_then(|value| normalize_host(value).ok())
            .as_deref()
            == Some(host)
        {
            ["GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN"]
        } else {
            return None;
        };
        keys.iter()
            .find_map(|key| self.environment.get(*key).cloned().and_then(nonempty))
    }

    fn gh_program(&self) -> Result<PathBuf, CredentialError> {
        if let Some(path) = self.environment.get("PATH") {
            for dir in std::env::split_paths(path) {
                let program = dir.join(if cfg!(windows) { "gh.exe" } else { "gh" });
                if program.is_file() {
                    return Ok(program);
                }
            }
        }
        Err(CredentialError::CliMissing)
    }

    fn gh_token(&self, host: &str, account: Option<&str>) -> Result<String, CredentialError> {
        let mut args = vec!["auth", "token", "--hostname", host];
        if let Some(account) = account {
            args.extend(["--user", account]);
        }
        self.run_gh(&args)
            .and_then(|output| nonempty(output).ok_or(CredentialError::NotSignedIn))
    }

    fn run_gh(&self, args: &[&str]) -> Result<String, CredentialError> {
        use smol::io::AsyncReadExt as _;
        let mut command = crate::process::async_command(self.gh_program()?);
        command
            .args(args)
            .env_clear()
            .envs(&self.environment)
            .env("GH_DEBUG", "")
            .env("GH_PROMPT_DISABLED", "1")
            // Ambient tokens must not let gh bypass the explicit GH_HOST binding.
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .env_remove("GH_ENTERPRISE_TOKEN")
            .env_remove("GITHUB_ENTERPRISE_TOKEN")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                CredentialError::CliMissing
            } else {
                CredentialError::CliFailed
            }
        })?;
        let stdout = child.stdout.take().ok_or(CredentialError::CliFailed)?;
        futures_lite::future::block_on(smol::future::race(
            async {
                let mut bytes = Vec::new();
                stdout
                    .take(1024 * 1024 + 1)
                    .read_to_end(&mut bytes)
                    .await
                    .map_err(|_| CredentialError::CliFailed)?;
                if bytes.len() > 1024 * 1024 {
                    return Err(CredentialError::CliFailed);
                }
                if !child
                    .status()
                    .await
                    .map_err(|_| CredentialError::CliFailed)?
                    .success()
                {
                    return Err(CredentialError::NotSignedIn);
                }
                String::from_utf8(bytes).map_err(|_| CredentialError::CliFailed)
            },
            async {
                smol::Timer::after(Duration::from_secs(10)).await;
                Err(CredentialError::CliFailed)
            },
        ))
    }

    /// Discovery is safe to replicate: login names and source only, never a token or fingerprint.
    pub fn discover(&self) -> BTreeMap<String, GitHubCredentialStatus> {
        let accounts: serde_json::Value = self
            .run_gh(&["auth", "status", "--json", "hosts"])
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        let mut hosts = self.hosts.read().unwrap().clone();
        hosts.entry("github.com".into()).or_default();
        if let Some(logins) = accounts.get("hosts").and_then(|hosts| hosts.as_object()) {
            for host in logins.keys() {
                if let Ok(host) = normalize_host(host) {
                    hosts.entry(host).or_default();
                }
            }
        }
        hosts
            .into_iter()
            .map(|(host, choice)| {
                let token_set = self.store.github_token(&host).and_then(nonempty).is_some();
                let logins = accounts
                    .get("hosts")
                    .and_then(|hosts| hosts.get(&host))
                    .and_then(|logins| logins.as_array());
                let accounts = logins
                    .into_iter()
                    .flatten()
                    .filter_map(|login| {
                        login
                            .get("login")
                            .and_then(|value| value.as_str())
                            .map(str::to_owned)
                    })
                    .collect();
                let source = self.get(&host).ok().map(|credential| credential.source);
                let env_overrides_account =
                    source == Some(GitHubCredentialSource::Env) && choice.account.is_some();
                (
                    host,
                    GitHubCredentialStatus {
                        token_set,
                        source,
                        accounts,
                        env_overrides_account,
                    },
                )
            })
            .collect()
    }
}

fn nonempty(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}
