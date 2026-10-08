//! Media a pull request's conversation points at on GitHub's own hosts. In a private repository
//! GitHub answers an unauthenticated request for one with 404, so the host reads it with the
//! github.com credential and hands back bounded bytes.

use super::{GitHubApi, GitHubError};
use std::{
    io::Read as _,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tcode_protocol::{MAX_PULL_REQUEST_MEDIA_BYTES, PullRequestMedia};
use url::Url;

/// The hosts the credential is for. A redirect leads to a signed object URL that authorizes
/// with its own signature, and the store behind it has no business seeing a token.
const CREDENTIALED_HOSTS: [&str; 4] = [
    "github.com",
    "www.github.com",
    "raw.githubusercontent.com",
    "media.githubusercontent.com",
];
const MAX_REDIRECTS: usize = 3;
const TIMEOUT: Duration = Duration::from_secs(30);
/// How long a client may reuse the bytes before asking again.
const EXPIRY: Duration = Duration::from_secs(3600);
const MAX_SIDE: u32 = 8192;
const MAX_PIXELS: u64 = 24_000_000;

/// The URL to read for `source`, or `None` when it is not media GitHub hosts behind a
/// credential: an upload, a legacy upload, or a repository file.
pub fn asset_url(source: &str) -> Option<Url> {
    let url = Url::parse(source).ok()?;
    if url.scheme() != "https" {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let path = url.path();
    if host == "raw.githubusercontent.com" || host == "media.githubusercontent.com" {
        let mut canonical = Url::parse(&format!("https://{host}{path}")).ok()?;
        canonical.set_query(url.query());
        return Some(canonical);
    }
    if host != "github.com" && host != "www.github.com" {
        return None;
    }
    let segments: Vec<_> = path.trim_start_matches('/').split('/').collect();
    let token = |value: &str| {
        !value.is_empty()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    };
    let upload = matches!(segments.as_slice(), ["user-attachments", "assets", id] if token(id));
    let legacy = matches!(
        segments.as_slice(),
        [owner, repository, "assets", number, id]
            if !owner.is_empty() && !repository.is_empty()
                && !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) && token(id)
    );
    if upload || legacy {
        return Url::parse(&format!("https://github.com{path}")).ok();
    }
    // `blob` and `raw` both address file bytes; the raw host is the one that honours the token.
    match segments.as_slice() {
        [owner, repository, "raw" | "blob", rest @ ..]
            if !owner.is_empty()
                && !repository.is_empty()
                && rest.iter().any(|s| !s.is_empty()) =>
        {
            Url::parse(&format!(
                "https://raw.githubusercontent.com/{owner}/{repository}/{}",
                rest.join("/")
            ))
            .ok()
        }
        _ => None,
    }
}

fn media_type(value: &str) -> Option<String> {
    let value = value.split(';').next()?.trim().to_ascii_lowercase();
    let (kind, subtype) = value.split_once('/')?;
    (matches!(kind, "image" | "video" | "audio")
        && !subtype.is_empty()
        && subtype
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$&^.+-_".contains(&byte)))
    .then_some(value)
}

/// The raw host labels every committed binary `application/octet-stream`, so the name decides
/// those.
fn type_from_name(url: &Url) -> Option<&'static str> {
    let name = url.path_segments()?.next_back()?.to_ascii_lowercase();
    Some(match name.rsplit_once('.')?.1 {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "tif" | "tiff" => "image/tiff",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        _ => return None,
    })
}

/// An author's avatar, which GitHub serves publicly and never with the credential.
fn avatar_url(source: &str) -> Option<Url> {
    let url = Url::parse(source).ok()?;
    (url.scheme() == "https"
        && url
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case("avatars.githubusercontent.com")))
    .then_some(url)
}

pub(super) fn fetch(
    api: &GitHubApi,
    source: &str,
    validator: Option<&str>,
) -> Result<PullRequestMedia, GitHubError> {
    let mut target = asset_url(source)
        .or_else(|| avatar_url(source))
        .ok_or(GitHubError::InvalidInput)?;
    // Without a github.com credential a public asset still loads, and a private one fails as
    // it does in a browser that is not signed in.
    let token = api
        .credentials()
        .get("github.com")
        .ok()
        .map(|credential| credential.token);
    let started = Instant::now();
    let _permit = api.gate.acquire();
    let response = 'hops: {
        for hop in 0..=MAX_REDIRECTS {
            let remaining = TIMEOUT.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(GitHubError::Deadline);
            }
            let mut request = api
                .agent
                .get(target.as_str())
                .timeout(remaining)
                .set("User-Agent", "tcode")
                .set("Accept-Encoding", "identity");
            let credentialed = target.host_str().is_some_and(|host| {
                CREDENTIALED_HOSTS.contains(&host.to_ascii_lowercase().as_str())
            });
            if let (true, Some(token)) = (credentialed, &token) {
                request = request.set("Authorization", &format!("Bearer {token}"));
            }
            if let Some(validator) = validator {
                request = request.set("If-None-Match", validator);
            }
            let response = match request.call() {
                Ok(response) => response,
                Err(ureq::Error::Status(status, _)) => {
                    return Err(match status {
                        404 => GitHubError::NotFound,
                        status => GitHubError::Response {
                            status,
                            messages: Vec::new(),
                        },
                    });
                }
                Err(ureq::Error::Transport(_)) => {
                    return Err(if started.elapsed() >= TIMEOUT {
                        GitHubError::Deadline
                    } else {
                        GitHubError::Request
                    });
                }
            };
            if !(300..400).contains(&response.status()) || response.status() == 304 {
                break 'hops response;
            }
            // A chain this long is not GitHub answering with bytes.
            let next = response
                .header("location")
                .and_then(|location| target.join(location).ok())
                .filter(|next| next.scheme() == "https" && hop < MAX_REDIRECTS)
                .ok_or(GitHubError::InvalidResponse)?;
            target = next;
        }
        return Err(GitHubError::InvalidResponse);
    };
    let expires_at = (SystemTime::now() + EXPIRY)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if response.status() == 304 {
        return Ok(PullRequestMedia::NotModified { expires_at });
    }
    let mime = response
        .header("content-type")
        .and_then(media_type)
        .or_else(|| type_from_name(&target).map(str::to_owned))
        .or_else(|| type_from_name(&asset_url(source)?).map(str::to_owned))
        .ok_or(GitHubError::UnsupportedMedia)?;
    if !mime.starts_with("image/") {
        return Ok(PullRequestMedia::External { mime });
    }
    let declared = response
        .header("content-length")
        .and_then(|value| value.parse::<u64>().ok());
    if declared.is_some_and(|length| length > MAX_PULL_REQUEST_MEDIA_BYTES as u64) {
        return Err(GitHubError::BodyTooLarge);
    }
    let validator = response
        .header("etag")
        .or_else(|| response.header("last-modified"))
        .map(str::to_owned);
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_PULL_REQUEST_MEDIA_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            if started.elapsed() >= TIMEOUT {
                GitHubError::Deadline
            } else {
                GitHubError::Request
            }
        })?;
    if bytes.len() > MAX_PULL_REQUEST_MEDIA_BYTES {
        return Err(GitHubError::BodyTooLarge);
    }
    // An SVG is a document; the client draws it as an image only, so its dimensions are the
    // layout's. Every other image must decode to bounded dimensions.
    if mime != "image/svg+xml" {
        let (width, height) = image::ImageReader::new(std::io::Cursor::new(&bytes))
            .with_guessed_format()
            .ok()
            .and_then(|reader| reader.into_dimensions().ok())
            .ok_or(GitHubError::UnsupportedMedia)?;
        if width > MAX_SIDE || height > MAX_SIDE || width as u64 * height as u64 > MAX_PIXELS {
            return Err(GitHubError::BodyTooLarge);
        }
    }
    Ok(PullRequestMedia::Image {
        bytes,
        mime,
        validator,
        expires_at,
    })
}
