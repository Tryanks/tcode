//! Forgejo and Gitea REST, each server reached directly with its own token. Reads of a public
//! repository need none; writes do.

use crate::{
    forge::{ForgeError, ForgeErrorKind, account},
    settings::SettingsStore,
};
use std::{
    collections::{BTreeMap, HashMap},
    io::Read as _,
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant, SystemTime},
};
use tcode_core::{
    pull_request::HostKind,
    settings::{CredentialSource, HostProblem, HostSettings, TokenCheck},
};

const DEADLINE: Duration = Duration::from_secs(60);
/// The environment token tea itself reads, for the server its companion variable names.
pub(super) const ENV_TOKEN: &str = "GITEA_TOKEN";
const ENV_URL: &str = "GITEA_INSTANCE_URL";

pub(super) fn error(kind: ForgeErrorKind, description: impl Into<String>) -> ForgeError {
    ForgeError {
        kind,
        description: description.into(),
    }
}

/// Which of the two a server runs, where their APIs differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Server {
    /// `false` for Forgejo, which reports its own numbering with a `+gitea-` suffix, and for a
    /// server whose version could not be read.
    pub(super) gitea: bool,
}

impl Server {
    pub(super) fn parse(version: &str) -> Self {
        Self {
            gitea: !version.contains("+gitea-"),
        }
    }
}

#[derive(Clone)]
pub(super) struct Credential {
    token: String,
    verification: String,
    pub(super) source: CredentialSource,
}

impl Credential {
    fn new(token: String, source: CredentialSource) -> Self {
        Self {
            verification: crate::settings::credential_fingerprint(&token, None),
            token,
            source,
        }
    }
}

/// Forgejo and Gitea share one token store, so either kind names a server's checks.
const KIND: HostKind = HostKind::Forgejo;

#[derive(Debug)]
pub(super) struct Response {
    pub(super) body: Vec<u8>,
    pub(super) truncated: bool,
}

impl Response {
    pub(super) fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, ForgeError> {
        if self.truncated {
            return Err(error(
                ForgeErrorKind::TooLarge,
                "Forgejo response too large",
            ));
        }
        serde_json::from_slice(&self.body)
            .map_err(|_| error(ForgeErrorKind::Uncertain, "Forgejo response unreadable"))
    }
}

pub(super) struct Request<'a> {
    pub(super) method: &'a str,
    /// Below `/api/v1`, query included.
    pub(super) path: String,
    pub(super) body: Option<serde_json::Value>,
    pub(super) accept: &'a str,
    pub(super) operation: &'static str,
    /// A write goes only with a credential.
    pub(super) write: bool,
    /// The most of a body read; a longer one is cut and marked truncated. `None` reads it
    /// whole.
    pub(super) limit: Option<usize>,
}

impl<'a> Request<'a> {
    pub(super) fn get(path: impl Into<String>, operation: &'static str) -> Self {
        Self {
            method: "GET",
            path: path.into(),
            body: None,
            accept: "application/json",
            operation,
            write: false,
            limit: None,
        }
    }
    pub(super) fn write(
        method: &'a str,
        path: impl Into<String>,
        body: Option<serde_json::Value>,
        operation: &'static str,
    ) -> Self {
        Self {
            method,
            body,
            write: true,
            ..Self::get(path, operation)
        }
    }
}

/// A server's credential, none to read anonymously, or the CLI's failure.
type CredentialRead = Result<Option<Credential>, ForgeError>;

struct Cached<V> {
    value: V,
    expires: Instant,
}

pub(super) struct Api {
    store: SettingsStore,
    environment: BTreeMap<String, String>,
    hosts: RwLock<BTreeMap<String, HostSettings>>,
    agent: ureq::Agent,
    tokens: Mutex<HashMap<String, Cached<CredentialRead>>>,
    servers: Mutex<HashMap<String, Cached<Server>>>,
    rate_limits: Mutex<crate::forge::RateLimits>,
}

impl Api {
    pub(super) fn new(
        store: SettingsStore,
        environment: impl IntoIterator<Item = (String, String)>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            environment: environment.into_iter().collect(),
            hosts: RwLock::default(),
            // No pooling: ureq retries a stale pooled socket, and a write must never be retried.
            agent: ureq::AgentBuilder::new()
                .redirects(0)
                .max_idle_connections(0)
                .build(),
            tokens: Mutex::default(),
            servers: Mutex::default(),
            rate_limits: Mutex::default(),
        })
    }

    pub(super) fn configure(&self, hosts: BTreeMap<String, HostSettings>) {
        *self.hosts.write().unwrap() = hosts;
    }

    pub(super) fn configured(&self) -> BTreeMap<String, HostSettings> {
        self.hosts.read().unwrap().clone()
    }

    pub(super) fn enabled(&self, authority: &str) -> bool {
        self.hosts
            .read()
            .unwrap()
            .get(authority)
            .is_none_or(|choice| choice.enabled)
    }

    /// The token saved for the server, then `GITEA_TOKEN` when `GITEA_INSTANCE_URL` names it,
    /// then the one tea stored for it. `None` reads anonymously.
    pub(super) fn credential(&self, authority: &str) -> Result<Option<Credential>, ForgeError> {
        if !self.enabled(authority) {
            return Err(error(
                ForgeErrorKind::HostDisabled,
                "Forgejo host turned off",
            ));
        }
        let kind = self
            .hosts
            .read()
            .unwrap()
            .get(authority)
            .map(|choice| choice.kind)
            .unwrap_or(HostKind::Forgejo);
        if let Some(token) = self
            .store
            .token(kind, authority)
            .map_err(ForgeError::credentials_unreadable)?
            .and_then(nonempty)
        {
            return Ok(Some(Credential::new(token, CredentialSource::Saved)));
        }
        if let Some(token) = self.environment_token(authority) {
            return Ok(Some(Credential::new(
                token,
                CredentialSource::Env {
                    name: ENV_TOKEN.into(),
                },
            )));
        }
        let mut tokens = self.tokens.lock().unwrap();
        tokens.retain(|_, cached| cached.expires > Instant::now());
        if let Some(cached) = tokens.get(authority) {
            return cached.value.clone();
        }
        let value = self.tea_token(authority).map(|found| {
            found.map(|token| Credential::new(token, CredentialSource::Cli { tool: "tea".into() }))
        });
        let ttl = if matches!(value, Ok(Some(_))) {
            300
        } else {
            10
        };
        tokens.insert(
            authority.to_owned(),
            Cached {
                value: value.clone(),
                expires: Instant::now() + Duration::from_secs(ttl),
            },
        );
        value
    }

    pub(super) fn forget(&self, authority: &str) {
        self.tokens.lock().unwrap().remove(authority);
    }

    pub(super) fn token_saved(&self, kind: HostKind, authority: &str) -> bool {
        self.store
            .token(kind, authority)
            .is_ok_and(|token| token.and_then(nonempty).is_some())
    }

    /// What the server's last answer to `credential` says: its account, and any problem.
    pub(super) fn assessed(
        &self,
        authority: &str,
        credential: &Credential,
    ) -> (Option<HostProblem>, Option<String>) {
        account::assessed(
            &self.store,
            KIND,
            authority,
            &credential.source,
            &credential.verification,
        )
    }

    /// Whose `token` is on the server. Gitea refuses a token without `read:user` the account
    /// with a 403 naming the scopes it lacks.
    pub(super) fn whose(&self, authority: &str, token: &str) -> account::Whose {
        let answer = account::ask(
            &self.agent,
            &format!("https://{authority}/api/v1/user"),
            &format!("token {token}"),
            &[],
        )?;
        if answer.status == 403
            && let Some(scopes) = answer
                .body()
                .and_then(|body| body["message"].as_str())
                .filter(|message| message.contains("scope"))
                .and_then(|message| message.split_once('[')?.1.split_once(']'))
                .map(|(scopes, _)| {
                    scopes
                        .split([',', ' '])
                        .filter(|scope| !scope.is_empty())
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .filter(|scopes| !scopes.is_empty())
        {
            return Err(TokenCheck::MissingScopes { scopes });
        }
        let body = answer.accepted()?;
        Ok((account::account(&body, "login", "full_name"), Vec::new()))
    }

    /// Learns whose the token in use is, when that is not yet known.
    pub(super) fn identify(&self, authority: &str) {
        if let Ok(Some(credential)) = self.credential(authority) {
            account::identify(
                &self.store,
                KIND,
                authority,
                &credential.source,
                &credential.verification,
                || self.whose(authority, &credential.token),
            );
        }
    }

    fn environment_token(&self, authority: &str) -> Option<String> {
        let named = self.environment.get(ENV_URL)?;
        (HostKind::Gitea.authority(named).ok()? == authority)
            .then(|| self.environment.get(ENV_TOKEN).cloned().and_then(nonempty))?
    }

    /// The server `GITEA_INSTANCE_URL` names, when it is one.
    pub(super) fn environment_host(&self) -> Option<String> {
        HostKind::Gitea
            .authority(self.environment.get(ENV_URL)?)
            .ok()
    }

    pub(super) fn tea_program(&self) -> Option<PathBuf> {
        let path = self.environment.get("PATH")?;
        std::env::split_paths(path)
            .map(|dir| dir.join(if cfg!(windows) { "tea.exe" } else { "tea" }))
            .find(|program| program.is_file())
    }

    /// tea's git credential helper answers with the stored token of the login whose URL is the
    /// server's; `tea login list` never prints one. Without a login it fails like any other
    /// error, saying "no login found for host" (tea 0.16, `cmd/login/helper.go`), which is
    /// none; any other failure is tea's.
    fn tea_token(&self, authority: &str) -> Result<Option<String>, ForgeError> {
        let Some(program) = self.tea_program() else {
            return Ok(None);
        };
        let (host, path) = authority.split_once('/').unwrap_or((authority, ""));
        let mut input = format!("protocol=https\nhost={host}\n");
        if !path.is_empty() {
            input.push_str(&format!("path={path}\n"));
        }
        input.push('\n');
        let output = match self.run_tea(program, &["login", "helper", "get"], Some(&input)) {
            Ok(output) => output,
            Err(failure) if failure.exited && failure.said.contains("no login found for host") => {
                return Ok(None);
            }
            Err(failure) => return Err(failure.error("tea")),
        };
        Ok(output
            .lines()
            .find_map(|line| line.strip_prefix("password="))
            .map(str::to_owned)
            .and_then(nonempty))
    }

    /// The servers tea has logins for; none without tea.
    pub(super) fn tea_logins(&self) -> Result<Vec<String>, ForgeError> {
        let Some(program) = self.tea_program() else {
            return Ok(Vec::new());
        };
        let output = self
            .run_tea(program, &["logins", "list", "--output", "json"], None)
            .map_err(|failure| failure.error("tea"))?;
        let logins: Vec<serde_json::Value> = serde_json::from_str(&output).map_err(|error| {
            crate::forge::CliFailure {
                exited: false,
                said: format!("tea logins list answered with what is not its JSON: {error}"),
            }
            .error("tea")
        })?;
        Ok(logins
            .iter()
            .filter_map(|login| login["url"].as_str())
            .filter_map(|url| HostKind::Gitea.authority(url).ok())
            .collect())
    }

    fn run_tea(
        &self,
        program: PathBuf,
        args: &[&str],
        input: Option<&str>,
    ) -> Result<String, crate::forge::CliFailure> {
        // tea would otherwise prefer these over its stored login.
        crate::forge::run_cli(
            program,
            args,
            input,
            &self.environment,
            &[ENV_TOKEN, ENV_URL],
            true,
        )
    }

    /// What the server runs, read once an hour.
    pub(super) fn server(&self, authority: &str) -> Server {
        if let Some(cached) = self.servers.lock().unwrap().get(authority)
            && cached.expires > Instant::now()
        {
            return cached.value;
        }
        let read = self
            .send(
                authority,
                Request {
                    operation: "Version",
                    ..Request::get("/version", "Version")
                },
            )
            .and_then(|response| response.json::<serde_json::Value>());
        let (value, ttl) = match read {
            Ok(version) => (
                Server::parse(version["version"].as_str().unwrap_or_default()),
                3600,
            ),
            Err(_) => (Server { gitea: false }, 60),
        };
        self.servers.lock().unwrap().insert(
            authority.to_owned(),
            Cached {
                value,
                expires: Instant::now() + Duration::from_secs(ttl),
            },
        );
        value
    }

    pub(super) fn send(
        &self,
        authority: &str,
        request: Request<'_>,
    ) -> Result<Response, ForgeError> {
        let credential = self.credential(authority)?;
        let fingerprint = credential.as_ref().map_or_else(
            || "anonymous".to_owned(),
            |credential| credential.verification.clone(),
        );
        let lease = self
            .rate_limits
            .lock()
            .unwrap()
            .admit(authority, &fingerprint, false)
            .map_err(|retry_at| {
                error(
                    ForgeErrorKind::Paused { retry_at },
                    "Forgejo request paused",
                )
            })?;
        if request.write && credential.is_none() {
            return Err(error(
                ForgeErrorKind::NoCredential,
                "Forgejo credential unavailable",
            ));
        }
        let url = format!("https://{authority}/api/v1{}", request.path);
        let started = Instant::now();
        let mut call = self
            .agent
            .request(request.method, &url)
            .timeout(DEADLINE)
            .set("Accept", request.accept)
            .set("User-Agent", "tcode");
        if let Some(credential) = &credential {
            call = call.set("Authorization", &format!("token {}", credential.token));
        }
        let answer = match &request.body {
            Some(body) => call
                .set("Content-Type", "application/json")
                .send_bytes(&serde_json::to_vec(body).unwrap_or_default()),
            None => call.call(),
        };
        let response = match answer {
            Ok(response) | Err(ureq::Error::Status(_, response)) => response,
            Err(ureq::Error::Transport(_)) => {
                return Err(if started.elapsed() >= DEADLINE {
                    error(ForgeErrorKind::Deadline, "Forgejo request deadline")
                } else {
                    error(
                        ForgeErrorKind::Uncertain,
                        "Forgejo request failed (network)",
                    )
                });
            }
        };
        let status = response.status();
        log::debug!(
            "forgejo host={} path={} operation={} status={}",
            authority,
            request.path.split('?').next().unwrap_or("/"),
            request.operation,
            status
        );
        let headers: BTreeMap<_, _> = response
            .headers_names()
            .into_iter()
            .filter_map(|name| {
                response
                    .header(&name)
                    .map(|value| (name.to_ascii_lowercase(), value.to_owned()))
            })
            .collect();
        let mut body = Vec::new();
        response
            .into_reader()
            .take(request.limit.map_or(u64::MAX, |limit| limit as u64 + 1))
            .read_to_end(&mut body)
            .map_err(|_| error(ForgeErrorKind::Uncertain, "Forgejo response cut off"))?;
        let truncated = request.limit.is_some_and(|limit| body.len() > limit);
        if let Some(limit) = request.limit {
            body.truncate(limit);
        }
        if status == 429 {
            let named = headers
                .get("retry-after")
                .and_then(|value| value.trim().parse().ok())
                .map(|seconds| SystemTime::now() + Duration::from_secs(seconds));
            let retry_at =
                self.rate_limits
                    .lock()
                    .unwrap()
                    .refused(authority, &fingerprint, lease, named);
            return Err(error(
                ForgeErrorKind::RateLimited { retry_at },
                "Forgejo request failed (rate limited)",
            ));
        }
        if (200..300).contains(&status) {
            self.rate_limits
                .lock()
                .unwrap()
                .succeeded(authority, &fingerprint, lease);
            if let Some(credential) = &credential {
                account::record(
                    &self.store,
                    KIND,
                    authority,
                    &credential.source,
                    &credential.verification,
                    true,
                );
            }
            return Ok(Response { body, truncated });
        }
        let messages = match serde_json::from_slice::<serde_json::Value>(&body) {
            Ok(value) => value["message"]
                .as_str()
                .map(str::to_owned)
                .into_iter()
                .collect(),
            Err(_) => crate::forge::unparsed(&body),
        };
        Err(match status {
            401 => {
                if let Some(credential) = &credential {
                    account::record(
                        &self.store,
                        KIND,
                        authority,
                        &credential.source,
                        &credential.verification,
                        false,
                    );
                }
                self.forget(authority);
                error(
                    ForgeErrorKind::Unauthorized,
                    "Forgejo request failed (unauthorized)",
                )
            }
            404 => error(
                ForgeErrorKind::NotFound,
                "Forgejo request failed (not found)",
            ),
            500.. => error(
                ForgeErrorKind::Uncertain,
                format!("Forgejo response failed ({status})"),
            ),
            _ => error(
                ForgeErrorKind::Refused { messages },
                format!("Forgejo response failed ({status})"),
            ),
        })
    }

    /// A list read page by page while each page is full, up to `pages`; `false` when more
    /// remained. A server may cap a page below the size asked for, so a page is full at the
    /// size the first one came back with.
    pub(super) fn list(
        &self,
        authority: &str,
        path: &str,
        operation: &'static str,
        pages: usize,
    ) -> Result<(Vec<serde_json::Value>, bool), ForgeError> {
        let separator = if path.contains('?') { '&' } else { '?' };
        let mut rows = Vec::new();
        let mut size = None;
        for page in 1..=pages {
            let batch: Vec<serde_json::Value> = self
                .send(
                    authority,
                    Request::get(
                        format!("{path}{separator}limit={PAGE_LIMIT}&page={page}"),
                        operation,
                    ),
                )?
                .json()?;
            let full = page_full(batch.len(), size.get_or_insert(batch.len()));
            rows.extend(batch);
            if !full {
                return Ok((rows, true));
            }
        }
        Ok((rows, false))
    }
}

impl Api {
    /// An upload or avatar on the server, read with its token. `authority` is the server the
    /// pull request is on, mount path included, whose credential the URL's host is sent.
    pub(super) fn media(
        &self,
        authority: &str,
        url: &url::Url,
        validator: Option<&str>,
    ) -> Result<tcode_protocol::PullRequestMedia, ForgeError> {
        let authorization = self
            .credential(authority)
            .ok()
            .flatten()
            .map(|credential| format!("token {}", credential.token));
        crate::forge::media(
            &self.agent,
            url,
            authorization.as_deref(),
            validator,
            "Forgejo",
        )
    }
}

/// The page size asked for; a server may answer fewer.
pub(super) const PAGE_LIMIT: usize = 50;

/// Whether a page of `rows` was full, so another may follow: a server may cap a page below
/// [`PAGE_LIMIT`], so a page is full at the size the first page came back with.
pub(super) fn page_full(rows: usize, first: &usize) -> bool {
    rows > 0 && rows >= *first
}

fn nonempty(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    /// `GITEA_TOKEN` goes only to the server `GITEA_INSTANCE_URL` names.
    #[test]
    fn the_environment_token_goes_only_to_the_server_it_is_for() {
        use super::Api;
        use tcode_core::settings::CredentialSource;
        let root =
            std::env::temp_dir().join(format!("tcode-forgejo-token-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let api = Api::new(
            crate::settings::SettingsStore::new(root.clone()),
            [
                (
                    "GITEA_INSTANCE_URL".to_owned(),
                    "https://env.test/".to_owned(),
                ),
                ("GITEA_TOKEN".to_owned(), "env-secret".to_owned()),
            ],
        );
        let source = |authority: &str| {
            api.credential(authority)
                .unwrap()
                .map(|credential| (credential.source, credential.token))
        };
        assert_eq!(
            source("env.test"),
            Some((
                CredentialSource::Env {
                    name: "GITEA_TOKEN".into()
                },
                "env-secret".into()
            ))
        );
        assert_eq!(source("other.test"), None);
        assert_eq!(source("env.test.evil"), None);
        let _ = std::fs::remove_dir_all(root);
    }
}
