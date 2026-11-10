use std::{
    collections::HashMap,
    time::{Duration, SystemTime},
};

/// How long a host is left alone after its rate limit refuses a request, per host and credential:
/// until the time the host names, else `min(30 s·2^(n−1), 15 min)` for the n-th refusal in a row.
/// A success after the pause starts the count again.
///
/// A request runs under the lease [`Self::admit`] gave it, so the refusals of requests already in
/// flight when a pause began count once, not once each.
#[derive(Default)]
pub(crate) struct RateLimits {
    pauses: HashMap<(String, String), Pause>,
}

#[derive(Default)]
struct Pause {
    attempt: u32,
    generation: u64,
    retry_at: Option<SystemTime>,
}

impl RateLimits {
    /// The lease for a request, or the time the pause in force ends. An interactive request goes
    /// through a pause.
    pub(crate) fn admit(
        &self,
        host: &str,
        credential: &str,
        interactive: bool,
    ) -> Result<u64, SystemTime> {
        let pause = self.pauses.get(&(host.to_owned(), credential.to_owned()));
        if !interactive
            && let Some(retry_at) = pause
                .and_then(|pause| pause.retry_at)
                .filter(|retry_at| *retry_at > SystemTime::now())
        {
            return Err(retry_at);
        }
        Ok(pause.map_or(0, |pause| pause.generation))
    }

    /// The host refused a request admitted under `lease`; `explicit` is the time it named.
    /// Returns when the pause ends.
    pub(crate) fn refused(
        &mut self,
        host: &str,
        credential: &str,
        lease: u64,
        explicit: Option<SystemTime>,
    ) -> SystemTime {
        let now = SystemTime::now();
        let explicit = explicit.filter(|time| *time > now);
        let pause = self
            .pauses
            .entry((host.to_owned(), credential.to_owned()))
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
        let proposed = explicit.unwrap_or(now + Duration::from_secs(cooldown));
        let retry_at = pause
            .retry_at
            .filter(|time| *time > now)
            .map_or(proposed, |old| old.max(proposed));
        pause.retry_at = Some(retry_at);
        pause.generation = pause.generation.max(lease) + 1;
        retry_at
    }

    /// A request admitted under `lease` succeeded.
    pub(crate) fn succeeded(&mut self, host: &str, credential: &str, lease: u64) {
        if let Some(pause) = self
            .pauses
            .get_mut(&(host.to_owned(), credential.to_owned()))
            && pause.generation == lease
            && pause.retry_at.is_none_or(|time| time <= SystemTime::now())
        {
            pause.attempt = 0;
            pause.retry_at = None;
        }
    }
}
