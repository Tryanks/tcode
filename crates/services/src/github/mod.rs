//! Host-owned GitHub HTTP, credentials and admission policy. Call blocking entries via HostCx::unblock.

pub mod api;
pub mod credentials;
pub mod graphql;
pub mod media;
mod merge_message;
pub mod pull_request_actions;
pub mod pull_request_reads;
pub mod pull_request_watch;
pub mod pull_requests;
mod quota;
mod read_cache;
pub mod repository;

pub use api::{GitHubApi, GitHubError, RequestOptions, Response, RestRequest};
pub use credentials::{Credential, CredentialError, Credentials, Identity};
pub use read_cache::Fresh;

pub fn normalize_host(host: &str) -> Result<String, CredentialError> {
    let host = host.trim().to_ascii_lowercase();
    if host.is_empty()
        || host.len() > 253
        || host.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
    {
        return Err(CredentialError::InvalidHost);
    }
    Ok(host)
}

fn digest(value: &str) -> String {
    use sha2::{Digest as _, Sha256};
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
