use super::{
    Credential, CredentialError, Credentials, Identity,
    graphql::Document,
    normalize_host,
    quota::{Ledger, number},
};
use std::{
    collections::{BTreeMap, HashMap},
    io::Read as _,
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant, SystemTime},
};

pub type Headers = BTreeMap<String, String>;
const BODY_LIMIT: usize = 8 * 1024 * 1024;
const MAX_BODY_LIMIT: usize = 16_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitHubError {
    Credential(CredentialError),
    Paused { retry_at: SystemTime },
    RateLimited { status: u16, retry_at: SystemTime },
    Unauthorized,
    NotFound,
    Response { status: u16, messages: Vec<String> },
    Request,
    Deadline,
    BodyTooLarge,
    InvalidResponse,
    InvalidPath,
}
impl std::fmt::Display for GitHubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Server messages may echo request values; keep the display safe for generic logging.
        match self {
            Self::Response { status, .. } => write!(f, "GitHub response failed ({status})"),
            Self::Credential(error) => error.fmt(f),
            _ => write!(
                f,
                "GitHub request failed ({})",
                match self {
                    Self::Paused { .. } => "paused",
                    Self::RateLimited { .. } => "rate limited",
                    Self::Unauthorized => "unauthorized",
                    Self::NotFound => "not found",
                    Self::Request => "network",
                    Self::Deadline => "deadline",
                    Self::BodyTooLarge => "body too large",
                    Self::InvalidResponse => "invalid response",
                    Self::InvalidPath => "invalid path",
                    _ => unreachable!(),
                }
            ),
        }
    }
}
impl std::error::Error for GitHubError {}
impl From<CredentialError> for GitHubError {
    fn from(value: CredentialError) -> Self {
        Self::Credential(value)
    }
}

#[derive(Debug, Clone, Default)]
pub enum Authentication {
    #[default]
    Automatic,
    Pinned(Credential),
    Anonymous {
        namespace: String,
    },
}

#[derive(Debug, Clone)]
pub struct RequestOptions {
    pub authentication: Authentication,
    pub interactive: bool,
    pub operation: &'static str,
    pub timeout: Duration,
    pub body_limit: usize,
}
impl Default for RequestOptions {
    fn default() -> Self {
        Self {
            authentication: Authentication::Automatic,
            interactive: false,
            operation: "GitHub",
            timeout: Duration::from_secs(30),
            body_limit: BODY_LIMIT,
        }
    }
}

pub struct RestRequest<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub body: Option<&'a serde_json::Value>,
    pub if_none_match: Option<&'a str>,
}
impl<'a> RestRequest<'a> {
    pub fn get(path: &'a str) -> Self {
        Self {
            method: "GET",
            path,
            body: None,
            if_none_match: None,
        }
    }
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: Headers,
    pub body: Vec<u8>,
    /// REST exposes bounded bytes; structured consumers must reject a cut response.
    pub truncated: bool,
}
impl Response {
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, GitHubError> {
        if self.truncated {
            return Err(GitHubError::BodyTooLarge);
        }
        serde_json::from_slice(&self.body).map_err(|_| GitHubError::InvalidResponse)
    }
}

#[derive(Default)]
struct Gate {
    active: Mutex<usize>,
    ready: Condvar,
}
struct Permit(Arc<Gate>);
impl Gate {
    fn acquire(self: &Arc<Self>) -> Permit {
        let mut active = self.active.lock().unwrap();
        while *active >= 8 {
            active = self.ready.wait(active).unwrap();
        }
        *active += 1;
        Permit(self.clone())
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        *self.0.active.lock().unwrap() -= 1;
        self.0.ready.notify_one();
    }
}

struct CachedIdentity {
    identity: Identity,
    expires: Instant,
}

pub struct GitHubApi {
    credentials: Arc<Credentials>,
    agent: ureq::Agent,
    gate: Arc<Gate>,
    ledger: Mutex<Ledger>,
    identities: Mutex<HashMap<String, CachedIdentity>>,
    identity_gates: Mutex<HashMap<String, std::sync::Weak<Mutex<()>>>>,
}

impl GitHubApi {
    pub fn host(credentials: Arc<Credentials>) -> Arc<Self> {
        Self::new(credentials, ureq::AgentBuilder::new())
    }

    /// The builder allows host-specific TLS trust and proxy/DNS configuration. Pooling is
    /// disabled because ureq retries stale pooled sockets; GitHub writes must never retry here.
    pub fn new(credentials: Arc<Credentials>, builder: ureq::AgentBuilder) -> Arc<Self> {
        Arc::new(Self {
            credentials,
            agent: builder.redirects(0).max_idle_connections(0).build(),
            gate: Arc::new(Gate::default()),
            ledger: Mutex::new(Ledger::default()),
            identities: Mutex::new(HashMap::new()),
            identity_gates: Mutex::new(HashMap::new()),
        })
    }

    pub fn credentials(&self) -> &Arc<Credentials> {
        &self.credentials
    }

    pub fn rest(
        self: &Arc<Self>,
        host: &str,
        request: RestRequest<'_>,
        options: &RequestOptions,
    ) -> Result<Response, GitHubError> {
        self.execute(host, request, None, options)
    }

    pub fn graphql(
        self: &Arc<Self>,
        host: &str,
        document: &Document,
        options: &RequestOptions,
    ) -> Result<Response, GitHubError> {
        let body = serde_json::json!({"query": document.query, "variables": document.variables});
        self.execute(
            host,
            RestRequest {
                method: "POST",
                path: "/graphql",
                body: Some(&body),
                if_none_match: None,
            },
            Some(&document.query),
            options,
        )
    }

    /// Capture and verify once before a multi-request operation to prevent account changes mid-page.
    pub fn verified_credential(
        self: &Arc<Self>,
        host: &str,
    ) -> Result<(Credential, Identity), GitHubError> {
        let credential = self.credentials.get(host)?;
        {
            let mut cache = self.identities.lock().unwrap();
            cache.retain(|_, value| value.expires > Instant::now());
            if let Some(cached) = cache.get(&credential.fingerprint) {
                return Ok((credential, cached.identity.clone()));
            }
        }
        let gate = {
            let mut gates = self.identity_gates.lock().unwrap();
            gates.retain(|_, gate| gate.strong_count() > 0);
            let weak = gates.entry(credential.fingerprint.clone()).or_default();
            match weak.upgrade() {
                Some(gate) => gate,
                None => {
                    let gate = Arc::new(Mutex::new(()));
                    *weak = Arc::downgrade(&gate);
                    gate
                }
            }
        };
        let _verification = gate.lock().unwrap();
        if let Some(cached) = self.identities.lock().unwrap().get(&credential.fingerprint) {
            return Ok((credential, cached.identity.clone()));
        }
        let response = self.rest(
            host,
            RestRequest::get("/user"),
            &RequestOptions {
                authentication: Authentication::Pinned(credential.clone()),
                interactive: true,
                operation: "Identity",
                ..Default::default()
            },
        )?;
        let identity: Identity = response.json()?;
        let mut cache = self.identities.lock().unwrap();
        if cache.len() >= 128
            && let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, value)| value.expires)
                .map(|(key, _)| key.clone())
        {
            cache.remove(&oldest);
        }
        cache.insert(
            credential.fingerprint.clone(),
            CachedIdentity {
                identity: identity.clone(),
                expires: Instant::now() + Duration::from_secs(600),
            },
        );
        Ok((credential, identity))
    }

    fn execute(
        self: &Arc<Self>,
        raw_host: &str,
        input: RestRequest<'_>,
        graphql: Option<&str>,
        options: &RequestOptions,
    ) -> Result<Response, GitHubError> {
        let host = normalize_host(raw_host)?;
        if !input.path.starts_with('/')
            || input.path.starts_with("//")
            || input.path.contains(['\r', '\n', '#', '\\'])
        {
            return Err(GitHubError::InvalidPath);
        }
        let credential = match &options.authentication {
            Authentication::Automatic => Some(self.credentials.get(&host)?),
            Authentication::Pinned(credential) => {
                if credential.host != host {
                    return Err(CredentialError::HostMismatch.into());
                }
                self.credentials.check_enabled(&host)?;
                Some(credential.clone())
            }
            Authentication::Anonymous { .. } => None,
        };
        let scope = match (&credential, &options.authentication) {
            (Some(credential), _) => credential.fingerprint.clone(),
            (_, Authentication::Anonymous { namespace }) => format!("anonymous:{namespace}"),
            _ => unreachable!(),
        };
        let permit = self.gate.acquire();
        let lease = self.ledger.lock().unwrap().admit(
            &host,
            &scope,
            if graphql.is_some() { "graphql" } else { "core" },
            options.interactive,
        )?;
        let timeout = options.timeout.min(Duration::from_secs(30));
        let api = self.clone();
        let method = input.method.to_owned();
        let path = input.path.to_owned();
        let body = input.body.cloned();
        let validator = input.if_none_match.map(str::to_owned);
        let graphql = graphql.map(str::to_owned);
        let options = options.clone();
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        // ureq 2 cannot apply its deadline to DNS or every TLS handshake phase. The
        // result deadline includes them; this I/O job retains the permit until it exits,
        // even after timeout or when the HostCx::unblock waiter has been dropped.
        std::thread::Builder::new()
            .name("github-io".into())
            .spawn(move || {
                let _permit = permit;
                let answer = api.execute_io(
                    &host,
                    RestRequest {
                        method: &method,
                        path: &path,
                        body: body.as_ref(),
                        if_none_match: validator.as_deref(),
                    },
                    graphql.as_deref(),
                    &options,
                    credential,
                    &scope,
                    lease,
                );
                let _ = send.send(answer);
            })
            .map_err(|_| GitHubError::Request)?;
        receive.recv_timeout(timeout).map_err(|error| match error {
            std::sync::mpsc::RecvTimeoutError::Timeout => GitHubError::Deadline,
            _ => GitHubError::Request,
        })?
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_io(
        &self,
        host: &str,
        input: RestRequest<'_>,
        graphql: Option<&str>,
        options: &RequestOptions,
        credential: Option<Credential>,
        scope: &str,
        lease: u64,
    ) -> Result<Response, GitHubError> {
        let public = host == "github.com";
        let residency = host.ends_with(".ghe.com");
        let root = if public {
            "https://api.github.com".to_owned()
        } else if residency {
            format!("https://api.{host}")
        } else {
            format!("https://{host}/api/v3")
        };
        let url = if graphql.is_some() && !public && !residency {
            format!("https://{host}/api/graphql")
        } else {
            format!("{root}{}", input.path)
        };
        let timeout = options.timeout.min(Duration::from_secs(30));
        let started = Instant::now();
        let mut request = self
            .agent
            .request(input.method, &url)
            .timeout(timeout)
            .set(
                "Accept",
                if graphql.is_some() {
                    "application/json"
                } else {
                    "application/vnd.github+json"
                },
            )
            .set("X-GitHub-Api-Version", "2022-11-28")
            .set("User-Agent", "tcode");
        if let Some(credential) = &credential {
            request = request.set("Authorization", &format!("Bearer {}", credential.token));
        }
        if let Some(etag) = input.if_none_match {
            request = request.set("If-None-Match", etag);
        }
        let answer = if let Some(body) = input.body {
            request
                .set("Content-Type", "application/json")
                .send_bytes(&serde_json::to_vec(body).map_err(|_| GitHubError::InvalidResponse)?)
        } else {
            request.call()
        };
        let response = match answer {
            Ok(response) | Err(ureq::Error::Status(_, response)) => response,
            Err(ureq::Error::Transport(error)) => {
                use std::error::Error as _;
                let timed_out = error
                    .source()
                    .and_then(|error| error.downcast_ref::<std::io::Error>())
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::TimedOut);
                return Err(if timed_out || started.elapsed() >= timeout {
                    GitHubError::Deadline
                } else {
                    GitHubError::Request
                });
            }
        };
        let status = response.status();
        let headers: Headers = response
            .headers_names()
            .into_iter()
            .filter_map(|name| {
                response
                    .header(&name)
                    .map(|value| (name.to_ascii_lowercase(), value.to_owned()))
            })
            .collect();
        self.ledger.lock().unwrap().observe(host, scope, &headers);
        log::debug!(
            "github host={} path={} operation={} status={} limit={:?} remaining={:?} reset={:?} document={}",
            host,
            input.path.split('?').next().unwrap_or("/"),
            options.operation,
            status,
            number(&headers, "x-ratelimit-limit"),
            number(&headers, "x-ratelimit-remaining"),
            number(&headers, "x-ratelimit-reset"),
            graphql.map(super::digest).unwrap_or_default()
        );
        let cap = options.body_limit.min(MAX_BODY_LIMIT);
        let mut body = Vec::new();
        response
            .into_reader()
            .take(cap as u64 + 1)
            .read_to_end(&mut body)
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::TimedOut || started.elapsed() >= timeout {
                    GitHubError::Deadline
                } else {
                    GitHubError::Request
                }
            })?;
        if started.elapsed() >= timeout {
            return Err(GitHubError::Deadline);
        }
        let truncated = body.len() > cap;
        body.truncate(cap);
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        let errors = graphql
            .and_then(|_| parsed.get("errors").and_then(|errors| errors.as_array()))
            .filter(|errors| !errors.is_empty());
        let types: Vec<_> = errors
            .into_iter()
            .flatten()
            .filter_map(|error| error.get("type").and_then(|kind| kind.as_str()))
            .collect();
        let messages: Vec<String> = errors
            .into_iter()
            .flatten()
            .filter_map(|error| {
                error
                    .get("message")
                    .and_then(|value| value.as_str())
                    .map(str::to_owned)
            })
            .collect();
        let limited = status == 429
            || types.contains(&"RATE_LIMITED")
            || (errors.is_some()
                && headers
                    .get("x-ratelimit-remaining")
                    .is_some_and(|value| value == "0"))
            || messages.iter().any(|message| {
                let message = message.to_ascii_lowercase();
                message.contains("rate limit exceeded")
                    || message.contains("rate limit already exceeded")
            })
            || (status == 403
                && (headers
                    .get("x-ratelimit-remaining")
                    .is_some_and(|value| value == "0")
                    || headers.contains_key("retry-after")
                    || String::from_utf8_lossy(&body)
                        .to_ascii_lowercase()
                        .contains("rate limit")));
        if limited {
            let retry_at = self
                .ledger
                .lock()
                .unwrap()
                .refused(host, scope, lease, &headers);
            return Err(GitHubError::RateLimited { status, retry_at });
        }
        if status == 401 {
            self.credentials.invalidate(host);
            return Err(GitHubError::Unauthorized);
        }
        if status == 404
            || errors.is_some_and(|errors| {
                errors.iter().all(|error| {
                    error.get("type").and_then(|value| value.as_str()) == Some("NOT_FOUND")
                })
            })
        {
            return Err(GitHubError::NotFound);
        }
        if errors.is_some() {
            return Err(GitHubError::Response { status, messages });
        }
        if (200..300).contains(&status) || (status == 304 && input.if_none_match.is_some()) {
            self.ledger.lock().unwrap().succeeded(host, scope, lease);
            if graphql.is_some() && truncated {
                return Err(GitHubError::BodyTooLarge);
            }
            if graphql.is_some() && !parsed.is_object() {
                return Err(GitHubError::InvalidResponse);
            }
            return Ok(Response {
                status,
                headers,
                body,
                truncated,
            });
        }
        let mut messages = parsed
            .get("message")
            .and_then(|value| value.as_str())
            .map(|message| vec![message.to_owned()])
            .unwrap_or_default();
        for error in parsed
            .get("errors")
            .and_then(|errors| errors.as_array())
            .into_iter()
            .flatten()
        {
            if let Some(message) = error
                .as_str()
                .or_else(|| error.get("message").and_then(|value| value.as_str()))
            {
                messages.push(message.to_owned());
            } else if let (Some(field), Some(code)) = (
                error.get("field").and_then(|value| value.as_str()),
                error.get("code").and_then(|value| value.as_str()),
            ) {
                messages.push(format!("{field} {code}"));
            }
        }
        Err(GitHubError::Response { status, messages })
    }
}
