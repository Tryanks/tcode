//! Mergeability as a watch may act on it, for hosts whose one-read answer cannot tell a
//! conflict from a check still running.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};
use tcode_core::pull_request::{Mergeability, PullRequestKey};

/// How long a conflict must hold at one head before it is one: two consumers reading back to
/// back while the host checks a push would otherwise both see the check.
const CONFLICT_CONFIRMATION: Duration = Duration::from_secs(30);
/// Pull requests whose last read is older than this are forgotten.
const VERDICT_AGE: Duration = Duration::from_secs(24 * 3600);
const VERDICTS: usize = 512;

/// A pull request's head at its last read, when its conflict at that head was first read, and
/// when it was last read.
struct Verdict {
    head: String,
    conflict_since: Option<Instant>,
    read_at: Instant,
}

/// What each pull request's host said of conflicts, by head. A host still checking a new head
/// may answer as a conflict does, so a conflict is one only once it has held at the same head
/// for [`CONFLICT_CONFIRMATION`].
#[derive(Default)]
pub(crate) struct Verdicts(Mutex<HashMap<PullRequestKey, Verdict>>);

impl Verdicts {
    /// `read` is what the host answered now at `head`.
    pub(crate) fn read(
        &self,
        key: &PullRequestKey,
        head: Option<&str>,
        read: Mergeability,
        now: Instant,
    ) -> Mergeability {
        let Some(head) = head else {
            return read;
        };
        let mut verdicts = self.0.lock().unwrap();
        verdicts.retain(|_, verdict| now.saturating_duration_since(verdict.read_at) < VERDICT_AGE);
        if verdicts.len() >= VERDICTS
            && !verdicts.contains_key(key)
            && let Some(oldest) = verdicts
                .iter()
                .min_by_key(|(_, verdict)| verdict.read_at)
                .map(|(key, _)| key.clone())
        {
            verdicts.remove(&oldest);
        }
        let earlier = verdicts
            .get(key)
            .filter(|verdict| verdict.head == head)
            .and_then(|verdict| verdict.conflict_since);
        let (conflict_since, answer) = match read {
            Mergeability::Conflicting => {
                let since = earlier.unwrap_or(now);
                let held = now.saturating_duration_since(since) >= CONFLICT_CONFIRMATION;
                (Some(since), if held { read } else { Mergeability::Unknown })
            }
            Mergeability::Clean => (None, read),
            Mergeability::Unknown => (earlier, read),
        };
        verdicts.insert(
            key.clone(),
            Verdict {
                head: head.to_owned(),
                conflict_since,
                read_at: now,
            },
        );
        answer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A conflict read just after a push is the host still checking: it is one only once it has
    /// held at the same head for 30 seconds, so two back-to-back reads never confirm one; a new
    /// head or a clean read starts over.
    #[test]
    fn a_conflict_is_one_that_holds_at_one_head() {
        let key = PullRequestKey::new("gitea.test", "a/b", 1);
        let start = Instant::now();
        let at = |seconds: u64| start + Duration::from_secs(seconds);
        let verdicts = Verdicts::default();
        let read = |head: &str, read: Mergeability, seconds: u64| {
            verdicts.read(&key, Some(head), read, at(seconds))
        };
        assert_eq!(
            read("h1", Mergeability::Conflicting, 0),
            Mergeability::Unknown
        );
        assert_eq!(
            read("h1", Mergeability::Conflicting, 1),
            Mergeability::Unknown
        );
        assert_eq!(
            read("h1", Mergeability::Conflicting, 30),
            Mergeability::Conflicting
        );
        assert_eq!(
            read("h2", Mergeability::Conflicting, 31),
            Mergeability::Unknown
        );
        assert_eq!(read("h2", Mergeability::Clean, 40), Mergeability::Clean);
        assert_eq!(
            read("h2", Mergeability::Conflicting, 80),
            Mergeability::Unknown
        );
    }
}
