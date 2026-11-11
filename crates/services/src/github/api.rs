use super::{
    Credential, CredentialError, Credentials,
    graphql::Document,
    normalize_host,
    quota::{Ledger, number},
};
use std::{
    collections::BTreeMap,
    io::Read as _,
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant, SystemTime},
};

pub type Headers = BTreeMap<String, String>;
const BODY_LIMIT: usize = 8 * 1024 * 1024;
/// Every request ends by this deadline; only diff and file reads ask for more than the default.
pub const DEADLINE_CAP: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitHubError {
    Credential(CredentialError),
    Paused {
        retry_at: SystemTime,
    },
    RateLimited {
        status: u16,
        retry_at: SystemTime,
    },
    Unauthorized,
    NotFound,
    Response {
        status: u16,
        messages: Vec<String>,
    },
    Request,
    Deadline,
    BodyTooLarge,
    InvalidResponse,
    /// A read Tcode refuses before any request: an invalid revision, path or media URL.
    InvalidInput,
    /// Media whose type is not an image, video or audio, or an image that does not decode.
    UnsupportedMedia,
}
impl std::fmt::Display for GitHubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Server messages may echo request values; keep the display safe for generic logging.
        let reason = match self {
            Self::Credential(error) => return error.fmt(f),
            Self::Response { status, .. } => return write!(f, "GitHub response failed ({status})"),
            Self::Paused { .. } => "paused",
            Self::RateLimited { .. } => "rate limited",
            Self::Unauthorized => "unauthorized",
            Self::NotFound => "not found",
            Self::Request => "network",
            Self::Deadline => "deadline",
            Self::BodyTooLarge => "body too large",
            Self::InvalidResponse => "invalid response",
            Self::InvalidInput => "invalid input",
            Self::UnsupportedMedia => "unsupported media",
        };
        write!(f, "GitHub request failed ({reason})")
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
    /// A separate ledger scope, so unauthenticated quota never mixes with a credential's.
    Anonymous,
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
    /// Replaces the JSON media type, for representations such as a raw diff or file.
    pub accept: Option<&'a str>,
    /// Error statuses whose body the caller reads as an answer, such as a conflict naming what it
    /// conflicts with.
    pub answers: &'a [u16],
}
impl<'a> RestRequest<'a> {
    pub fn get(path: &'a str) -> Self {
        Self {
            method: "GET",
            path,
            body: None,
            if_none_match: None,
            accept: None,
            answers: &[],
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
pub(super) struct Gate {
    active: Mutex<usize>,
    ready: Condvar,
}
pub(super) struct Permit<'a>(&'a Gate);
impl Gate {
    pub(super) fn acquire(&self) -> Permit<'_> {
        let mut active = self.active.lock().unwrap();
        while *active >= 8 {
            active = self.ready.wait(active).unwrap();
        }
        *active += 1;
        Permit(self)
    }
}
impl Drop for Permit<'_> {
    fn drop(&mut self) {
        *self.0.active.lock().unwrap() -= 1;
        self.0.ready.notify_one();
    }
}

pub struct GitHubApi {
    credentials: Arc<Credentials>,
    pub(super) agent: ureq::Agent,
    pub(super) gate: Gate,
    ledger: Mutex<Ledger>,
}

impl GitHubApi {
    /// Pooling is disabled because ureq retries an idempotent request without a body on a
    /// stale pooled socket, and a label's DELETE is one.
    pub fn host(credentials: Arc<Credentials>) -> Arc<Self> {
        Arc::new(Self {
            credentials,
            agent: ureq::AgentBuilder::new()
                .redirects(0)
                .max_idle_connections(0)
                .build(),
            gate: Gate::default(),
            ledger: Mutex::new(Ledger::default()),
        })
    }

    pub fn credentials(&self) -> &Arc<Credentials> {
        &self.credentials
    }

    pub fn rest(
        &self,
        host: &str,
        request: RestRequest<'_>,
        options: &RequestOptions,
    ) -> Result<Response, GitHubError> {
        self.execute(host, request, None, false, options)
    }

    pub fn graphql(
        &self,
        host: &str,
        document: &Document,
        options: &RequestOptions,
    ) -> Result<Response, GitHubError> {
        self.graphql_answer(host, document, false, options)
    }

    /// A GraphQL answer whose `data` stands beside its `errors`, for the caller to read field by
    /// field: the GraphQL specification ("Response", "Errors") lets a field fail on its own,
    /// with an error naming its `path` and the rest of `data` still answered. An answer
    /// without `data` fails as [`Self::graphql`] does.
    pub fn graphql_partial(
        &self,
        host: &str,
        document: &Document,
        options: &RequestOptions,
    ) -> Result<Response, GitHubError> {
        self.graphql_answer(host, document, true, options)
    }

    fn graphql_answer(
        &self,
        host: &str,
        document: &Document,
        partial: bool,
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
                accept: None,
                answers: &[],
            },
            Some(&document.query),
            partial,
            options,
        )
    }

    /// Whose `token` is on the host, and which of the scopes Tcode needs a classic token
    /// lacks. A fine-grained token names no scopes, so none are missing.
    pub fn whose(&self, host: &str, token: &str) -> crate::forge::account::Whose {
        use tcode_core::settings::TokenCheck;
        let host = normalize_host(host).map_err(|_| TokenCheck::Unreachable { status: None })?;
        let answer = crate::forge::account::ask(
            &self.agent,
            &format!("{}/user", api_root(&host)),
            &format!("Bearer {}", token.trim()),
            &[("X-GitHub-Api-Version", "2022-11-28")],
        )?;
        let scopes = answer.header("x-oauth-scopes").map(|scopes| {
            scopes
                .split(',')
                .map(str::trim)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        });
        let body = answer.accepted()?;
        let missing = scopes
            .map(|scopes| {
                let has = |scope: &str| scopes.iter().any(|held| held == scope);
                let mut missing = Vec::new();
                if !has("repo") {
                    missing.push("repo".to_owned());
                }
                if !has("read:org") && !has("write:org") && !has("admin:org") {
                    missing.push("read:org".to_owned());
                }
                missing
            })
            .unwrap_or_default();
        Ok((
            crate::forge::account::account(&body, "login", "name"),
            missing,
        ))
    }

    /// Learns whose the saved or environment token in use is, when that is not yet known.
    pub fn identify(&self, host: &str) {
        if let Ok(credential) = self.credentials.get(host) {
            self.credentials
                .identify(&credential, || self.whose(host, &credential.token));
        }
    }

    fn execute(
        &self,
        raw_host: &str,
        input: RestRequest<'_>,
        graphql: Option<&str>,
        partial: bool,
        options: &RequestOptions,
    ) -> Result<Response, GitHubError> {
        let host = normalize_host(raw_host)?;
        let credential = match &options.authentication {
            Authentication::Automatic => Some(self.credentials.get(&host)?),
            Authentication::Pinned(credential) => {
                if credential.host != host {
                    return Err(CredentialError::HostMismatch.into());
                }
                self.credentials.check_enabled(&host)?;
                Some(credential.clone())
            }
            Authentication::Anonymous => None,
        };
        let scope = credential
            .as_ref()
            .map_or("anonymous", |credential| &credential.fingerprint);
        // Callers run this inside HostCx::unblock, so a dropped waiter leaves the job, and
        // its permit, alive until the socket closes. ureq 2 does not apply its deadline to
        // DNS resolution; a stalled lookup is bounded by the system resolver instead.
        let _permit = self.gate.acquire();
        let lease = self.ledger.lock().unwrap().admit(
            &host,
            scope,
            if graphql.is_some() { "graphql" } else { "core" },
            options.interactive,
        )?;
        let host = host.as_str();
        let public = host == "github.com";
        let residency = host.ends_with(".ghe.com");
        let root = api_root(host);
        let url = if graphql.is_some() && !public && !residency {
            format!("https://{host}/api/graphql")
        } else {
            format!("{root}{}", input.path)
        };
        let timeout = options.timeout.min(DEADLINE_CAP);
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
                    input.accept.unwrap_or("application/vnd.github+json")
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
        let cap = options.body_limit.min(BODY_LIMIT);
        let mut body = Vec::new();
        // A 304 has no message body, even when GitHub includes the cached
        // representation's Content-Encoding. Reading it asks ureq 2 to decode an empty gzip stream.
        if status != 304 {
            response
                .into_reader()
                .take(cap as u64 + 1)
                .read_to_end(&mut body)
                .map_err(|error| {
                    if error.kind() == std::io::ErrorKind::TimedOut || started.elapsed() >= timeout
                    {
                        GitHubError::Deadline
                    } else {
                        GitHubError::Request
                    }
                })?;
        }
        let truncated = body.len() > cap;
        body.truncate(cap);
        let parsed: Option<serde_json::Value> = serde_json::from_slice(&body).ok();
        let unparsed = parsed.is_none();
        let parsed = parsed.unwrap_or_default();
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
            if let Some(credential) = &credential {
                self.credentials.record(credential, false);
            }
            self.credentials.invalidate(host);
            return Err(GitHubError::Unauthorized);
        }
        let partial = partial && parsed.get("data").is_some_and(|data| data.is_object());
        if status == 404
            || !partial
                && errors.is_some_and(|errors| {
                    errors.iter().all(|error| {
                        error.get("type").and_then(|value| value.as_str()) == Some("NOT_FOUND")
                    })
                })
        {
            return Err(GitHubError::NotFound);
        }
        if errors.is_some() && !partial {
            return Err(GitHubError::Response { status, messages });
        }
        if (200..300).contains(&status)
            || (status == 304 && input.if_none_match.is_some())
            || input.answers.contains(&status)
        {
            self.ledger.lock().unwrap().succeeded(host, scope, lease);
            if let Some(credential) = &credential {
                self.credentials.record(credential, true);
            }
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
        if unparsed {
            messages = crate::forge::unparsed(&body);
        }
        Err(GitHubError::Response { status, messages })
    }
}

/// Where the host's REST API is: api.github.com for github.com, `api.` before a data-residency
/// host, and `/api/v3` on an Enterprise Server.
fn api_root(host: &str) -> String {
    if host == "github.com" {
        "https://api.github.com".to_owned()
    } else if host.ends_with(".ghe.com") {
        format!("https://api.{host}")
    } else {
        format!("https://{host}/api/v3")
    }
}
