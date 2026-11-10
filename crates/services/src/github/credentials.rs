use super::normalize_host;
use crate::settings::SettingsStore;
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};
use tcode_core::{
    pull_request::HostKind,
    settings::{CredentialSource, HostProblem, HostSettings, HostStatus},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialError {
    InvalidHost,
    Disabled,
    CliMissing,
    NotSignedIn,
    /// gh failed when asked for its login, in its own words.
    CliFailed(String),
    HostMismatch,
    /// The token Tcode saved could not be read.
    SavedUnreadable,
}

impl std::fmt::Display for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CliFailed(said) => f.write_str(said),
            _ => write!(f, "GitHub credential unavailable: {self:?}"),
        }
    }
}
impl std::error::Error for CredentialError {}

#[derive(Clone)]
pub struct Credential {
    pub(super) host: String,
    pub(super) token: String,
    pub(super) fingerprint: String,
    /// The fingerprint the host's checks are recorded under.
    verification: String,
    pub(super) source: CredentialSource,
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
    fn new(host: String, token: String, source: CredentialSource) -> Self {
        let fingerprint = format!("{host}:{}", super::digest(&token));
        Self {
            verification: crate::settings::credential_fingerprint(&token, None),
            host,
            token,
            fingerprint,
            source,
        }
    }
}

/// Why gh gave no answer.
enum GhFailure {
    Missing,
    Ran(crate::forge::CliFailure),
}

impl From<GhFailure> for CredentialError {
    fn from(failure: GhFailure) -> Self {
        match failure {
            GhFailure::Missing => Self::CliMissing,
            GhFailure::Ran(failure) if failure.said.is_empty() => {
                Self::CliFailed("gh exited unsuccessfully without saying why".into())
            }
            GhFailure::Ran(failure) => Self::CliFailed(failure.said),
        }
    }
}

struct Cached {
    value: Result<Credential, CredentialError>,
    expires: Instant,
}

pub struct Credentials {
    store: SettingsStore,
    environment: BTreeMap<String, String>,
    hosts: RwLock<BTreeMap<String, HostSettings>>,
    cache: Mutex<HashMap<(String, Option<String>), Cached>>,
}

impl Credentials {
    /// The host's launch environment also supplies PATH for gh (including GUI shell repair).
    pub fn new(
        store: SettingsStore,
        environment: impl IntoIterator<Item = (String, String)>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            environment: environment.into_iter().collect(),
            hosts: RwLock::default(),
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// The GitHub hosts among `hosts` are the ones read.
    pub fn configure(&self, hosts: BTreeMap<String, HostSettings>) {
        *self.hosts.write().unwrap() = hosts
            .into_iter()
            .filter(|(_, choice)| choice.kind == HostKind::Github)
            .collect();
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
            .unwrap_or_else(|| HostSettings::new(HostKind::Github));
        if !choice.enabled {
            return Err(CredentialError::Disabled);
        }
        let saved = self.store.token(HostKind::Github, &host).map_err(|error| {
            log::error!("cannot read the saved GitHub token for {host}: {error}");
            CredentialError::SavedUnreadable
        })?;
        if let Some(token) = saved.and_then(nonempty) {
            return Ok(Credential::new(host, token, CredentialSource::Saved));
        }
        if let Some((name, token)) = self.environment_token(&host) {
            return Ok(Credential::new(
                host,
                token,
                CredentialSource::Env { name: name.into() },
            ));
        }
        let key = (host.clone(), choice.account.clone());
        // Holding the lock coalesces keyring lookups; no HTTP runs under this lock.
        let mut cache = self.cache.lock().unwrap();
        cache.retain(|_, value| value.expires > Instant::now());
        if let Some(cached) = cache.get(&key) {
            return cached.value.clone();
        }
        let value = self
            .gh_token(&host, choice.account.as_deref())
            .map(|token| Credential::new(host, token, CredentialSource::Cli { tool: "gh".into() }));
        let ttl = match &value {
            Ok(_) => 300,
            Err(CredentialError::CliFailed(_)) => 0,
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

    /// Records that the host answered (`true`) or refused a request sent with `credential`.
    pub(super) fn record(&self, credential: &Credential, accepted: bool) {
        crate::forge::account::record(
            &self.store,
            HostKind::Github,
            &credential.host,
            &credential.source,
            &credential.verification,
            accepted,
        );
    }

    pub(super) fn identify(
        &self,
        credential: &Credential,
        whose: impl FnOnce() -> crate::forge::account::Whose,
    ) {
        crate::forge::account::identify(
            &self.store,
            HostKind::Github,
            &credential.host,
            &credential.source,
            &credential.verification,
            whose,
        );
    }

    pub fn invalidate(&self, host: &str) {
        if let Ok(host) = normalize_host(host) {
            self.cache
                .lock()
                .unwrap()
                .retain(|(cached_host, _), _| cached_host != &host);
        }
    }

    /// The variables the host's environment token is read from, in order.
    fn environment_keys(&self, host: &str) -> Option<[&'static str; 2]> {
        if host == "github.com" || host.ends_with(".ghe.com") {
            Some(["GH_TOKEN", "GITHUB_TOKEN"])
        } else if self
            .environment
            .get("GH_HOST")
            .and_then(|value| normalize_host(value).ok())
            .as_deref()
            == Some(host)
        {
            Some(["GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN"])
        } else {
            None
        }
    }

    fn environment_token(&self, host: &str) -> Option<(&'static str, String)> {
        self.environment_keys(host)?.into_iter().find_map(|key| {
            self.environment
                .get(key)
                .cloned()
                .and_then(nonempty)
                .map(|token| (key, token))
        })
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
        // Without a token for the host gh fails like any other error, saying "no oauth token
        // found for <host>" (gh 2.103, `pkg/cmd/auth/token/token.go`); that is no login.
        match self.run_gh(&args) {
            Ok(output) => nonempty(output).ok_or(CredentialError::NotSignedIn),
            Err(GhFailure::Ran(failure))
                if failure.exited && failure.said.contains("no oauth token found for") =>
            {
                Err(CredentialError::NotSignedIn)
            }
            Err(failure) => Err(failure.into()),
        }
    }

    fn run_gh(&self, args: &[&str]) -> Result<String, GhFailure> {
        let program = self.gh_program().map_err(|_| GhFailure::Missing)?;
        let mut environment = self.environment.clone();
        environment.insert("GH_DEBUG".into(), String::new());
        environment.insert("GH_PROMPT_DISABLED".into(), "1".into());
        crate::forge::run_cli(
            program,
            args,
            None,
            &environment,
            // Ambient tokens must not let gh bypass the explicit GH_HOST binding.
            &[
                "GH_TOKEN",
                "GITHUB_TOKEN",
                "GH_ENTERPRISE_TOKEN",
                "GITHUB_ENTERPRISE_TOKEN",
            ],
            true,
        )
        .map_err(GhFailure::Ran)
    }

    /// Discovery is safe to replicate: login names and source only, never a token or fingerprint.
    pub fn discover(&self) -> BTreeMap<String, HostStatus> {
        // Signed out, gh still answers `{"hosts":{}}` (gh 2.103).
        let gh = self
            .run_gh(&["auth", "status", "--json", "hosts"])
            .map_err(CredentialError::from)
            .and_then(|text| {
                serde_json::from_str::<serde_json::Value>(&text).map_err(|error| {
                    CredentialError::CliFailed(format!(
                        "gh auth status answered with what is not its JSON: {error}"
                    ))
                })
            });
        let tools_missing = if gh == Err(CredentialError::CliMissing) {
            vec!["gh".to_owned()]
        } else {
            Vec::new()
        };
        let listing = match &gh {
            Err(CredentialError::CliFailed(said)) => Some(HostProblem::CliFailed {
                tool: "gh".into(),
                error: said.clone(),
            }),
            _ => None,
        };
        let accounts = gh.unwrap_or_default();
        let configured = self.hosts.read().unwrap().clone();
        // Whether only settings name the host.
        let mut hosts: BTreeMap<String, bool> =
            configured.keys().map(|host| (host.clone(), true)).collect();
        hosts.insert("github.com".into(), false);
        if let Some(logins) = accounts.get("hosts").and_then(|hosts| hosts.as_object()) {
            for host in logins.keys() {
                if let Ok(host) = normalize_host(host) {
                    hosts.insert(host, false);
                }
            }
        }
        hosts
            .into_iter()
            .map(|(host, added)| {
                let choice = configured
                    .get(&host)
                    .cloned()
                    .unwrap_or_else(|| HostSettings::new(HostKind::Github));
                let token_set = self
                    .store
                    .token(HostKind::Github, &host)
                    .is_ok_and(|token| token.and_then(nonempty).is_some());
                let logins = accounts
                    .get("hosts")
                    .and_then(|hosts| hosts.get(&host))
                    .and_then(|logins| logins.as_array());
                let accounts: Vec<String> = logins
                    .into_iter()
                    .flatten()
                    .filter_map(|login| {
                        login
                            .get("login")
                            .and_then(|value| value.as_str())
                            .map(str::to_owned)
                    })
                    .collect();
                let read = self.get(&host);
                // Without a login of its own, gh failing to list them leaves it unknown.
                let failed = match &read {
                    Err(CredentialError::CliFailed(said)) => Some(HostProblem::CliFailed {
                        tool: "gh".into(),
                        error: said.clone(),
                    }),
                    _ => listing.clone(),
                };
                let credential = read.ok();
                let (assessed, login) = credential
                    .as_ref()
                    .map(|credential| {
                        crate::forge::account::assessed(
                            &self.store,
                            HostKind::Github,
                            &host,
                            &credential.source,
                            &credential.verification,
                        )
                    })
                    .unwrap_or_default();
                let source = credential.map(|credential| credential.source);
                // gh's login is the account chosen, else the one gh has active.
                let login = match &source {
                    Some(CredentialSource::Cli { .. }) => choice.account.clone().or_else(|| {
                        logins.into_iter().flatten().find_map(|login| {
                            (login.get("active").and_then(|active| active.as_bool())
                                == Some(true))
                            .then(|| login.get("login")?.as_str().map(str::to_owned))
                            .flatten()
                        })
                    }),
                    _ => login,
                };
                let env_overrides_account = matches!(source, Some(CredentialSource::Env { .. }))
                    && choice.account.is_some();
                let mut order = vec![CredentialSource::Saved];
                order.extend(
                    self.environment_keys(&host)
                        .into_iter()
                        .flatten()
                        .map(|name| CredentialSource::Env { name: name.into() }),
                );
                order.push(CredentialSource::Cli { tool: "gh".into() });
                let problem = match &source {
                    _ if !choice.enabled => None,
                    None if failed.is_some() => failed,
                    None if tools_missing.is_empty() => Some(HostProblem::NotSignedIn {
                        tool: "gh".into(),
                        command: Some(format!("gh auth login --hostname {host}")),
                    }),
                    None => Some(HostProblem::NoCredential {
                        tools_missing: tools_missing.clone(),
                    }),
                    Some(_) => assessed,
                };
                (
                    host.clone(),
                    HostStatus {
                        kind: HostKind::Github,
                        added,
                        token_set,
                        source,
                        accounts,
                        env_overrides_account,
                        order,
                        problem,
                        token_url: Some(format!(
                            "https://{host}/settings/tokens/new?scopes=repo,read:org&description=Tcode"
                        )),
                        access_token_url: None,
                        token_scopes: vec!["repo".into(), "read:org".into()],
                        login,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// gh's variables name the token for each host: `GH_TOKEN` (then `GITHUB_TOKEN`) for
    /// github.com and its data-residency hosts, `GH_ENTERPRISE_TOKEN` (then
    /// `GITHUB_ENTERPRISE_TOKEN`) for the Enterprise Server `GH_HOST` names. A token saved in
    /// Tcode goes before them, and a host turned off gets none.
    #[test]
    fn each_host_gets_the_token_that_is_for_it() {
        let root =
            std::env::temp_dir().join(format!("tcode-github-token-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let store = SettingsStore::new(root.clone());
        let credentials = Credentials::new(
            store.clone(),
            [
                ("GH_TOKEN", "public-first"),
                ("GITHUB_TOKEN", "public-second"),
                ("GH_ENTERPRISE_TOKEN", "enterprise-first"),
                ("GITHUB_ENTERPRISE_TOKEN", "enterprise-second"),
                ("GH_HOST", " GIT.EXAMPLE.COM "),
            ]
            .map(|(key, value)| (key.to_owned(), value.to_owned())),
        );
        let token = |host: &str| credentials.get(host).map(|credential| credential.token);
        assert_eq!(token(" GITHUB.COM ").as_deref(), Ok("public-first"));
        assert_eq!(token("tenant.ghe.com").as_deref(), Ok("public-first"));
        assert_eq!(token("git.example.com").as_deref(), Ok("enterprise-first"));
        store
            .set_token(HostKind::Github, "GITHUB.COM", Some("saved-first"))
            .unwrap();
        assert_eq!(token("github.com").as_deref(), Ok("saved-first"));
        store
            .set_token(HostKind::Github, "github.com", None)
            .unwrap();
        assert_eq!(token("github.com").as_deref(), Ok("public-first"));
        credentials.configure(BTreeMap::from([(
            "github.com".into(),
            HostSettings {
                kind: HostKind::Github,
                enabled: false,
                account: None,
            },
        )]));
        assert_eq!(token("github.com").unwrap_err(), CredentialError::Disabled);
        let _ = std::fs::remove_dir_all(root);
    }
}
