//! What every host reached over its own HTTP API does alike: how a sent write's answer reads,
//! a write made of several requests, media read with the host's credential, and a host CLI
//! asked for a stored login.

use super::{ForgeError, ForgeErrorKind};
use std::{
    collections::BTreeMap,
    io::Read as _,
    path::PathBuf,
    time::{Duration, Instant, SystemTime},
};
use tcode_protocol::{
    PullRequestActionResult as Outcome, PullRequestMedia, max_pull_request_media_bytes,
};

const DEADLINE: Duration = Duration::from_secs(60);

/// The answer to a write that was sent. Without an answer, or with a server failure, the host
/// may have applied it; a refusal or a rate limit says it did not.
pub(crate) fn answered<T>(result: Result<T, ForgeError>) -> Outcome {
    match result {
        Ok(_) => Outcome::Applied,
        Err(ForgeError {
            kind: ForgeErrorKind::Uncertain | ForgeErrorKind::Deadline | ForgeErrorKind::TooLarge,
            ..
        }) => Outcome::Uncertain,
        Err(error) => Outcome::Rejected(error.rejection()),
    }
}

/// One request of a write made of several, and the names it covers.
pub(crate) type Step<'a> = (Vec<String>, Box<dyn FnOnce() -> Outcome + 'a>);

/// Sends the steps in order until one is not applied; when some went through before it, the
/// answer is [`Outcome::Partial`] by the names each covers.
pub(crate) fn in_order(steps: Vec<Step<'_>>) -> Outcome {
    let mut applied = Vec::new();
    let mut steps = steps.into_iter();
    while let Some((names, send)) = steps.next() {
        let outcome = send();
        if outcome == Outcome::Applied {
            applied.extend(names);
            continue;
        }
        if applied.is_empty() {
            return outcome;
        }
        return Outcome::Partial {
            applied,
            unapplied: names
                .into_iter()
                .chain(steps.flat_map(|(names, _)| names))
                .collect(),
            failure: Box::new(outcome),
        };
    }
    Outcome::Applied
}

/// Whether two URLs are one origin: scheme, host and port. A token goes nowhere else, so a
/// redirect to another port of the same host never carries it.
pub(crate) fn same_origin(left: &url::Url, right: &url::Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

/// Media on a host's own server, read with `authorization` as its Authorization header. A
/// redirect may lead to object storage that authorizes with its own signature, which never
/// sees the credential. `host` names the host in error descriptions.
pub(crate) fn media(
    agent: &ureq::Agent,
    url: &url::Url,
    authorization: Option<&str>,
    validator: Option<&str>,
    host: &str,
) -> Result<PullRequestMedia, ForgeError> {
    use crate::github::media;
    let error = |kind, what: &str| ForgeError {
        kind,
        description: format!("{host} media {what}"),
    };
    let failed = || error(ForgeErrorKind::Uncertain, "unreadable");
    let started = Instant::now();
    let mut target = url.clone();
    let mut response = None;
    for _ in 0..=3 {
        let remaining = DEADLINE.saturating_sub(started.elapsed());
        let mut request = agent
            .get(target.as_str())
            .timeout(remaining)
            .set("User-Agent", "tcode")
            .set("Accept-Encoding", "identity");
        if let Some(authorization) = authorization.filter(|_| same_origin(&target, url)) {
            request = request.set("Authorization", authorization);
        }
        if let Some(validator) = validator {
            let header = if validator.starts_with('"') || validator.starts_with("W/") {
                "If-None-Match"
            } else {
                "If-Modified-Since"
            };
            request = request.set(header, validator);
        }
        let answer = match request.call() {
            Ok(answer) => answer,
            Err(ureq::Error::Status(404, _)) => {
                return Err(error(ForgeErrorKind::NotFound, "not found"));
            }
            Err(_) => return Err(failed()),
        };
        if (300..400).contains(&answer.status()) && answer.status() != 304 {
            target = answer
                .header("location")
                .and_then(|location| target.join(location).ok())
                .filter(|next| next.scheme() == "https")
                .ok_or_else(failed)?;
            continue;
        }
        response = Some(answer);
        break;
    }
    let response = response.ok_or_else(failed)?;
    let expires_at = (SystemTime::now() + Duration::from_secs(3600))
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if response.status() == 304 {
        return Ok(PullRequestMedia::NotModified { expires_at });
    }
    let mime = response
        .header("content-type")
        .and_then(media::media_type)
        .or_else(|| media::type_from_name(&target).map(str::to_owned))
        .or_else(|| media::type_from_name(url).map(str::to_owned))
        .ok_or_else(|| error(ForgeErrorKind::UnsupportedMedia, "is not media"))?;
    if !mime.starts_with("image/") {
        return Ok(PullRequestMedia::External { mime });
    }
    let validator = response
        .header("etag")
        .or_else(|| response.header("last-modified"))
        .map(str::to_owned);
    let limit = max_pull_request_media_bytes(&mime, validator.as_deref());
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| failed())?;
    if bytes.len() > limit {
        return Err(error(ForgeErrorKind::TooLarge, "too large"));
    }
    media::bounded(&mime, &bytes)?;
    Ok(PullRequestMedia::Image {
        bytes,
        mime,
        validator,
        expires_at,
    })
}

/// Why a host CLI gave no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CliFailure {
    /// It ran and exited unsuccessfully, rather than failing to run or to answer.
    pub(crate) exited: bool,
    /// What it wrote to stderr, trimmed, or why it could not be run.
    pub(crate) said: String,
}

impl CliFailure {
    fn run(said: impl std::fmt::Display) -> Self {
        Self {
            exited: false,
            said: said.to_string(),
        }
    }

    /// The failure as `tool`'s, in its own words.
    pub(crate) fn error(self, tool: &str) -> ForgeError {
        ForgeError {
            kind: ForgeErrorKind::CliFailed { tool: tool.into() },
            description: if self.said.is_empty() {
                format!("{tool} exited unsuccessfully without saying why")
            } else {
                self.said
            },
        }
    }
}

/// A host CLI's output, given `input` on stdin, in the launch environment without `remove`,
/// which the CLI would prefer over its stored login. A failure when it does not answer within
/// ten seconds, or, with `success`, when it exits unsuccessfully.
pub(crate) fn run_cli(
    program: PathBuf,
    args: &[&str],
    input: Option<&str>,
    environment: &BTreeMap<String, String>,
    remove: &[&str],
    success: bool,
) -> Result<String, CliFailure> {
    use smol::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let name = program.file_name().map_or_else(
        || program.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    let mut command = crate::process::async_command(program);
    command.args(args).env_clear().envs(environment);
    for name in remove {
        command.env_remove(name);
    }
    command
        .stdin(if input.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| CliFailure::run(format!("{name}: {error}")))?;
    let stdin = child.stdin.take();
    let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(CliFailure::run(format!("{name}: no output pipe")));
    };
    futures_lite::future::block_on(smol::future::race(
        async {
            if let (Some(mut stdin), Some(input)) = (stdin, input) {
                stdin
                    .write_all(input.as_bytes())
                    .await
                    .map_err(|error| CliFailure::run(format!("{name}: {error}")))?;
                drop(stdin);
            }
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let (read_out, read_err) = futures_lite::future::zip(
                stdout.read_to_end(&mut out),
                stderr.read_to_end(&mut err),
            )
            .await;
            read_out
                .and(read_err)
                .map_err(|error| CliFailure::run(format!("{name}: {error}")))?;
            let status = child
                .status()
                .await
                .map_err(|error| CliFailure::run(format!("{name}: {error}")))?;
            if success && !status.success() {
                return Err(CliFailure {
                    exited: true,
                    said: String::from_utf8_lossy(&err).trim().to_owned(),
                });
            }
            String::from_utf8(out).map_err(|_| {
                CliFailure::run(format!("{name} answered with text that is not UTF-8"))
            })
        },
        async {
            smol::Timer::after(Duration::from_secs(10)).await;
            Err(CliFailure::run(format!(
                "{name} did not answer within ten seconds"
            )))
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::same_origin;

    /// A CLI that fails is its exit and its own words on stderr, not an absent answer.
    #[cfg(unix)]
    #[test]
    fn a_failing_cli_is_what_it_said() {
        let environment = std::env::vars().collect();
        let failure = super::run_cli(
            "/bin/sh".into(),
            &["-c", "echo partial; echo 'config is torn' >&2; exit 3"],
            None,
            &environment,
            &[],
            true,
        )
        .unwrap_err();
        assert_eq!(
            failure,
            super::CliFailure {
                exited: true,
                said: "config is torn".into(),
            }
        );
    }

    /// An upload's redirect keeps the token only on the server's own origin.
    #[test]
    fn a_redirect_off_the_servers_origin_drops_the_token() {
        let url = |value: &str| url::Url::parse(value).unwrap();
        let upload = url("https://git.acme.test/attachments/1");
        assert!(same_origin(
            &upload,
            &url("https://git.acme.test:443/attachments/2")
        ));
        assert!(!same_origin(
            &upload,
            &url("https://git.acme.test:8443/attachments/1")
        ));
        assert!(!same_origin(
            &upload,
            &url("http://git.acme.test/attachments/1")
        ));
        assert!(!same_origin(
            &upload,
            &url("https://storage.acme.test/attachments/1")
        ));
    }
}
