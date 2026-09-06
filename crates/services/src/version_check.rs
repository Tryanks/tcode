//! Update assessments and their host-facing adapters.

use std::io::Read as _;
use std::time::Duration;

use serde::Deserialize;

pub mod provider_updates;

const TCODE_LATEST_RELEASE_URL: &str = "https://api.github.com/repos/Tryanks/tcode/releases/latest";
const MAX_RELEASE_RESPONSE_BYTES: u64 = 1024 * 1024;

/// Fetch the GitHub release payload consumed by [`check`].
///
/// This is an adapter, separate from the assessment function, so callers and
/// tests can supply already-fetched bytes to the same check interface.
pub fn fetch_latest_tcode_release_json() -> Result<Vec<u8>, FetchError> {
    let response = ureq::get(TCODE_LATEST_RELEASE_URL)
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .set("User-Agent", "tcode-update-check")
        .timeout(Duration::from_secs(10))
        .call()
        .map_err(|error| match error {
            ureq::Error::Status(status, _) if matches!(status, 403 | 429) => {
                FetchError::RateLimited { status }
            }
            ureq::Error::Status(status, _) => FetchError::Http { status },
            ureq::Error::Transport(_) => FetchError::Network,
        })?;
    let mut body = Vec::new();
    response
        .into_reader()
        .take(MAX_RELEASE_RESPONSE_BYTES + 1)
        .read_to_end(&mut body)
        .map_err(|_| FetchError::Read)?;
    if body.len() as u64 > MAX_RELEASE_RESPONSE_BYTES {
        return Err(FetchError::ResponseTooLarge);
    }
    Ok(body)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchError {
    Network,
    RateLimited { status: u16 },
    Http { status: u16 },
    Read,
    ResponseTooLarge,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Assessment {
    pub latest: Option<String>,
    pub release_url: Option<String>,
    /// True only when a parsed release is newer and permitted by prerelease policy.
    pub update_available: bool,
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    prerelease: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Version {
    core: (u32, u32, u32),
    prerelease: bool,
}

/// Assess the running version using a fetched GitHub release payload.
///
/// App release tags deliberately require exactly three numeric parts because
/// they identify a precise published build; prerelease state also participates
/// in the app's stable/prerelease update policy. Provider CLI output is looser
/// and is parsed independently in `provider_updates`.
pub fn check(current: &str, fetched_release_json: Result<&[u8], FetchError>) -> Assessment {
    let Some(release) = fetched_release_json
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Release>(bytes).ok())
    else {
        return Assessment::default();
    };
    let mut assessment = Assessment {
        latest: Some(release.tag_name.trim_start_matches('v').to_string()),
        release_url: Some(release.html_url),
        update_available: false,
    };
    let Some(current_version) = parse_app_version(current) else {
        return assessment;
    };
    let Some(latest_version) = parse_app_version(&release.tag_name) else {
        return assessment;
    };

    // Preserve the release-metadata policy as well as the tag-based policy:
    // stable builds do not opt into releases GitHub marks as prereleases.
    assessment.update_available = (!release.prerelease || current.contains('-'))
        && if latest_version.prerelease && !current_version.prerelease {
            false
        } else {
            match latest_version.core.cmp(&current_version.core) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Less => false,
                std::cmp::Ordering::Equal => {
                    current_version.prerelease && !latest_version.prerelease
                }
            }
        };

    assessment
}

fn parse_app_version(raw: &str) -> Option<Version> {
    let raw = raw.trim().strip_prefix('v').unwrap_or(raw.trim());
    let without_build = raw.split_once('+').map_or(raw, |(core, _)| core);
    let (core, prerelease) = match without_build.split_once('-') {
        Some((core, suffix)) if !suffix.is_empty() => (core, true),
        Some(_) => return None,
        None => (without_build, false),
    };
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(Version {
        core: (major, minor, patch),
        prerelease,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELEASE_URL: &str = "https://github.com/Tryanks/tcode/releases/tag/v0.4.1";

    fn release(tag: &str, prerelease: bool) -> Vec<u8> {
        format!(
            r#"{{"tag_name":"{tag}","html_url":"{RELEASE_URL}","prerelease":{prerelease},"assets":[{{"name":"SHA256SUMS.txt"}}]}}"#
        )
        .into_bytes()
    }

    #[test]
    fn assesses_release_comparisons_and_prerelease_policy() {
        for (current, latest, prerelease, available) in [
            ("0.4.0", "v0.4.0", false, false),
            ("0.4.0", "v0.4.1", false, true),
            ("0.4.1", "v0.4.0", false, false),
            ("0.4.0", "v0.5.0-beta.1", true, false),
            ("0.5.0-beta.1", "v0.5.0", false, true),
            ("0.5.0-beta.1", "v0.6.0-beta.1", true, true),
        ] {
            let assessment = check(current, Ok(&release(latest, prerelease)));
            assert_eq!(
                assessment.update_available, available,
                "{current} -> {latest}"
            );
            assert_eq!(
                assessment.latest.as_deref(),
                Some(latest.trim_start_matches('v'))
            );
            assert_eq!(assessment.release_url.as_deref(), Some(RELEASE_URL));
        }
    }

    #[test]
    fn malformed_versions_preserve_release_details_without_announcing_updates() {
        for (current, latest) in [("0.4", "v0.4.1"), ("0.4.0", "latest")] {
            let assessment = check(current, Ok(&release(latest, false)));
            assert!(!assessment.update_available);
            assert_eq!(
                assessment.latest.as_deref(),
                Some(latest.trim_start_matches('v'))
            );
            assert_eq!(assessment.release_url.as_deref(), Some(RELEASE_URL));
        }
    }

    #[test]
    fn fetch_and_json_failures_leave_no_release_details() {
        assert_eq!(
            check("0.4.0", Err(FetchError::Network)),
            Assessment::default()
        );
        assert_eq!(check("0.4.0", Ok(b"not json")), Assessment::default());
    }
}
