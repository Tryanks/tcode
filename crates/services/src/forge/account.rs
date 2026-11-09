//! Asking a host whose a token is: one read of its current-user endpoint, before the token is
//! saved, or for the token in use when its account is not yet known.

use crate::settings::{SettingsStore, Verdict};
use std::{
    collections::BTreeMap,
    io::Read as _,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tcode_core::{
    pull_request::HostKind,
    settings::{CredentialSource, HostAccount, HostProblem, TokenCheck},
};

/// Shorter than a read's deadline: the user waits on it, and a server that does not answer in
/// this long is one to save for later.
const DEADLINE: Duration = Duration::from_secs(15);
const BODY_LIMIT: u64 = 256 * 1024;

/// What the host said about a token it answered: whose it is, and the scopes Tcode needs that
/// it lacks, where the host names them.
pub(crate) type Whose = Result<(Option<HostAccount>, Vec<String>), TokenCheck>;

pub(crate) struct Answer {
    pub(crate) status: u16,
    headers: BTreeMap<String, String>,
    pub(crate) body: serde_json::Value,
}

impl Answer {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    /// The body of a success; a refusal of the token, or another answer, as the check it is.
    pub(crate) fn accepted(self) -> Result<serde_json::Value, TokenCheck> {
        match self.status {
            200..=299 => Ok(self.body),
            401 | 403 => Err(TokenCheck::Rejected {
                status: self.status,
            }),
            status => Err(TokenCheck::Unreachable {
                status: Some(status),
            }),
        }
    }
}

/// One GET of `url` with `authorization`; no answer at all is [`TokenCheck::Unreachable`].
pub(crate) fn ask(
    agent: &ureq::Agent,
    url: &str,
    authorization: &str,
    headers: &[(&str, &str)],
) -> Result<Answer, TokenCheck> {
    let mut request = agent
        .get(url)
        .timeout(DEADLINE)
        .set("Accept", "application/json")
        .set("User-Agent", "tcode")
        .set("Authorization", authorization);
    for (name, value) in headers {
        request = request.set(name, value);
    }
    let response = match request.call() {
        Ok(response) | Err(ureq::Error::Status(_, response)) => response,
        Err(ureq::Error::Transport(_)) => return Err(TokenCheck::Unreachable { status: None }),
    };
    let status = response.status();
    let headers = response
        .headers_names()
        .into_iter()
        .filter_map(|name| {
            response
                .header(&name)
                .map(|value| (name.to_ascii_lowercase(), value.to_owned()))
        })
        .collect();
    let mut body = Vec::new();
    // An unreadable body is no answer about the token; the status still is.
    let _ = response
        .into_reader()
        .take(BODY_LIMIT)
        .read_to_end(&mut body);
    Ok(Answer {
        status,
        headers,
        body: serde_json::from_slice(&body).unwrap_or_default(),
    })
}

/// The account a body names, by the fields the host keeps them in.
pub(crate) fn account(body: &serde_json::Value, login: &str, name: &str) -> Option<HostAccount> {
    nonempty(&body[login]).map(|login| HostAccount {
        login,
        name: nonempty(&body[name]),
    })
}

fn nonempty(value: &serde_json::Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// The check a token gets before it is saved.
pub(crate) fn check(whose: Whose) -> TokenCheck {
    match whose {
        Ok((account, missing)) if missing.is_empty() => TokenCheck::Accepted { account },
        Ok((_, scopes)) => TokenCheck::MissingScopes { scopes },
        Err(check) => check,
    }
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// A token, saved or from the environment, that a host answered (`Some(true)`) or refused
/// (`Some(false)`) a request with. A CLI's login is its own to keep; nothing is recorded for it.
pub(crate) fn record(
    store: &SettingsStore,
    kind: HostKind,
    host: &str,
    source: &CredentialSource,
    fingerprint: &str,
    accepted: bool,
) {
    if matches!(source, CredentialSource::Cli { .. }) {
        return;
    }
    let verdict = if accepted {
        Verdict::Accepted { login: None }
    } else {
        Verdict::Rejected {
            status: 401,
            at: now(),
        }
    };
    if let Err(error) = store.record_check(kind, host, fingerprint, verdict) {
        log::warn!("failed to save credential check: {error}");
    }
}

/// Learns whose the token in use is when that is not yet known, and records the answer: the
/// account, or a refusal. A CLI's login and a token already refused are left alone.
pub(crate) fn identify(
    store: &SettingsStore,
    kind: HostKind,
    host: &str,
    source: &CredentialSource,
    fingerprint: &str,
    whose: impl FnOnce() -> Whose,
) {
    if matches!(source, CredentialSource::Cli { .. }) {
        return;
    }
    if matches!(
        store.credential_check(kind, host, fingerprint),
        Some(Verdict::Accepted { login: Some(_) } | Verdict::Rejected { .. })
    ) {
        return;
    }
    let verdict = match whose() {
        Ok((account, _)) => Verdict::Accepted {
            login: account.map(|account| account.login),
        },
        Err(TokenCheck::Rejected { status }) => Verdict::Rejected { status, at: now() },
        Err(_) => return,
    };
    if let Err(error) = store.record_check(kind, host, fingerprint, verdict) {
        log::warn!("failed to save credential check: {error}");
    }
}

/// What the host's last answer to the token in use says: the account it signs in as, and a
/// problem when it was refused, or when a saved one was never answered.
pub(crate) fn assessed(
    store: &SettingsStore,
    kind: HostKind,
    host: &str,
    source: &CredentialSource,
    fingerprint: &str,
) -> (Option<HostProblem>, Option<String>) {
    if matches!(source, CredentialSource::Cli { .. }) {
        return (None, None);
    }
    match store.credential_check(kind, host, fingerprint) {
        Some(Verdict::Accepted { login }) => (None, login),
        Some(Verdict::Rejected { status, at }) => {
            (Some(HostProblem::Rejected { status, at }), None)
        }
        None if matches!(source, CredentialSource::Env { .. }) => (None, None),
        None => (Some(HostProblem::Unverified), None),
    }
}
