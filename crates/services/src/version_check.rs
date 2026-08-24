//! Update assessment modules and their host-facing network adapter.

use std::io::Read as _;
use std::time::Duration;

pub mod app_releases;
pub mod provider_updates;

const TCODE_LATEST_RELEASE_URL: &str = "https://api.github.com/repos/Tryanks/tcode/releases/latest";
const MAX_RELEASE_RESPONSE_BYTES: u64 = 1024 * 1024;

/// Fetch the GitHub release payload consumed by [`app_releases::check`].
///
/// This is an adapter, separate from the assessment module, so callers and
/// tests can supply already-fetched bytes to the same check interface.
pub fn fetch_latest_tcode_release_json() -> Result<Vec<u8>, app_releases::FetchError> {
    let response = ureq::get(TCODE_LATEST_RELEASE_URL)
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .set("User-Agent", "tcode-update-check")
        .timeout(Duration::from_secs(10))
        .call()
        .map_err(|error| match error {
            ureq::Error::Status(status, _) if matches!(status, 403 | 429) => {
                app_releases::FetchError::RateLimited { status }
            }
            ureq::Error::Status(status, _) => app_releases::FetchError::Http { status },
            ureq::Error::Transport(_) => app_releases::FetchError::Network,
        })?;
    let mut body = Vec::new();
    response
        .into_reader()
        .take(MAX_RELEASE_RESPONSE_BYTES + 1)
        .read_to_end(&mut body)
        .map_err(|_| app_releases::FetchError::Read)?;
    if body.len() as u64 > MAX_RELEASE_RESPONSE_BYTES {
        return Err(app_releases::FetchError::ResponseTooLarge);
    }
    Ok(body)
}

/// Internal compatibility seam used by provider probing. Update assessments
/// and their tests use [`provider_updates::check`] directly.
pub(crate) fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    provider_updates::parse_version(text)
}
