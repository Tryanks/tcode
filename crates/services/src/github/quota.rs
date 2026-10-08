use super::api::{GitHubError, Headers};
use std::{
    collections::HashMap,
    time::{Duration, SystemTime},
};

#[derive(Clone)]
struct Snapshot {
    limit: u64,
    remaining: u64,
    reset: SystemTime,
}
#[derive(Default)]
struct Pause {
    attempt: u32,
    generation: u64,
    retry_at: Option<SystemTime>,
}
#[derive(Default)]
pub(super) struct Ledger {
    quotas: HashMap<(String, String, String), Snapshot>,
    pauses: HashMap<(String, String), Pause>,
}

impl Ledger {
    pub fn admit(
        &self,
        host: &str,
        scope: &str,
        resource: &str,
        interactive: bool,
    ) -> Result<u64, GitHubError> {
        let now = SystemTime::now();
        let pause = self.pauses.get(&(host.to_owned(), scope.to_owned()));
        if !interactive
            && let Some(retry_at) = pause
                .and_then(|pause| pause.retry_at)
                .filter(|retry_at| *retry_at > now)
        {
            return Err(GitHubError::Paused { retry_at });
        }
        if let Some(snapshot) =
            self.quotas
                .get(&(host.to_owned(), resource.to_owned(), scope.to_owned()))
            && snapshot.reset > now
            && (snapshot.remaining == 0
                || (!interactive && (snapshot.remaining as f64) < (snapshot.limit as f64) * 0.1))
        {
            return Err(GitHubError::Paused {
                retry_at: snapshot.reset,
            });
        }
        Ok(pause.map_or(0, |pause| pause.generation))
    }

    pub fn observe(&mut self, host: &str, scope: &str, headers: &Headers) {
        let Some(resource) = headers
            .get("x-ratelimit-resource")
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
        else {
            return;
        };
        let (Some(limit), Some(remaining), Some(reset)) = (
            number(headers, "x-ratelimit-limit"),
            number(headers, "x-ratelimit-remaining"),
            reset_time(headers),
        ) else {
            return;
        };
        if limit == 0 {
            return;
        }
        let key = (host.to_owned(), resource.to_owned(), scope.to_owned());
        if self
            .quotas
            .get(&key)
            .is_some_and(|snapshot| snapshot.reset > reset)
        {
            return;
        }
        self.quotas.insert(
            key,
            Snapshot {
                limit,
                remaining,
                reset,
            },
        );
    }

    pub fn refused(
        &mut self,
        host: &str,
        scope: &str,
        lease: u64,
        headers: &Headers,
    ) -> SystemTime {
        let now = SystemTime::now();
        let explicit = headers
            .get("retry-after")
            .and_then(|value| {
                if let Ok(seconds) = value.trim().parse::<u64>() {
                    now.checked_add(Duration::from_secs(seconds))
                } else {
                    chrono::DateTime::parse_from_rfc2822(value)
                        .ok()
                        .map(SystemTime::from)
                        .filter(|time| *time > now)
                }
            })
            .or_else(|| reset_time(headers).filter(|time| *time > now));
        let pause = self
            .pauses
            .entry((host.to_owned(), scope.to_owned()))
            .or_default();
        if pause.generation > lease {
            if let Some(explicit) = explicit
                && pause.retry_at.is_none_or(|old| explicit > old)
            {
                pause.retry_at = Some(explicit);
            }
            return pause.retry_at.unwrap_or(now);
        }
        pause.attempt = pause.attempt.saturating_add(1);
        let cooldown = 30_u64
            .saturating_mul(2_u64.saturating_pow(pause.attempt - 1))
            .min(900);
        let proposed = explicit
            .filter(|time| *time > now)
            .unwrap_or(now + Duration::from_secs(cooldown));
        pause.retry_at = Some(
            pause
                .retry_at
                .filter(|time| *time > now)
                .map_or(proposed, |old| old.max(proposed)),
        );
        pause.generation = pause.generation.max(lease) + 1;
        pause.retry_at.unwrap()
    }

    pub fn succeeded(&mut self, host: &str, scope: &str, lease: u64) {
        if let Some(pause) = self.pauses.get_mut(&(host.to_owned(), scope.to_owned()))
            && pause.generation == lease
            && pause.retry_at.is_none_or(|time| time <= SystemTime::now())
        {
            pause.attempt = 0;
            pause.retry_at = None;
        }
    }
}

pub(super) fn number(headers: &Headers, name: &str) -> Option<u64> {
    headers.get(name)?.trim().parse().ok()
}
fn reset_time(headers: &Headers) -> Option<SystemTime> {
    SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(number(headers, "x-ratelimit-reset")?))
}
