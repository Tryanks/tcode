//! Thread sections and static lifecycle ordering, independent of client layout.
use crate::project::SessionMeta;
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ThreadSection {
    Pinned,
    Active,
    Settled,
}

/// Whether a thread is listed among the threads: archived threads, dispatched
/// orchestrate children, provider-native subagents and a new thread whose
/// composer holds nothing are not; forks and drafts holding text are.
pub fn in_roster(meta: &SessionMeta) -> bool {
    meta.archived_at.is_none() && !meta.is_subagent() && meta.draft_text.as_deref() != Some("")
}

/// The thread nobody started that `meta` descends from, or the last one
/// `by_id` knows on the way there.
pub fn lead<'a>(
    meta: &'a SessionMeta,
    by_id: impl Fn(&str) -> Option<&'a SessionMeta>,
) -> &'a SessionMeta {
    let mut lead = meta;
    // A stored parent cycle ends at its depth rather than never.
    for _ in 0..64 {
        match lead.parent_session_id.as_deref().and_then(&by_id) {
            Some(parent) => lead = parent,
            None => break,
        }
    }
    lead
}

/// Whether a host's index carries the thread: it descends from no settled
/// thread and is none itself. A settled thread and what it started are read
/// a page or a family at a time instead.
pub fn in_index<'a>(
    meta: &'a SessionMeta,
    by_id: impl Fn(&str) -> Option<&'a SessionMeta>,
) -> bool {
    !lead(meta, by_id).is_settled()
}

/// Where a settled thread stands among settled threads, the newest settled
/// first: what a page of them is cut by.
pub fn settled_rank(meta: &SessionMeta) -> (std::cmp::Reverse<u64>, &str) {
    (std::cmp::Reverse(settled_timestamp(meta)), &meta.id)
}

pub fn thread_section(meta: &SessionMeta) -> ThreadSection {
    if meta.is_settled() {
        ThreadSection::Settled
    } else if meta.pinned_at.is_some() {
        ThreadSection::Pinned
    } else {
        ThreadSection::Active
    }
}

/// How much an active thread wants its user, the most first: what the
/// Active section orders by before time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Attention {
    /// It waits on an approval or an answer.
    Waiting,
    /// It changed since it was last looked at.
    Unread,
    /// A turn of it runs.
    Working,
    #[default]
    Idle,
}

/// What orders and dates a thread besides its meta, as its host reports it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Standing {
    pub attention: Attention,
    /// Its last message or run, in Unix milliseconds
    /// ([`crate::settlement::ThreadActivity::last_activity_at`]).
    pub last_activity_at: Option<u64>,
}

/// When a settled thread settled, in Unix seconds; the time it shows and sorts by.
fn settled_timestamp(meta: &SessionMeta) -> u64 {
    meta.settled_at.unwrap_or(meta.updated_at)
}

/// When a thread was last worked on, in Unix milliseconds: its last message
/// or run, or a later reopen. A thread whose host reports no activity dates
/// by its `updated_at`.
fn last_activity(meta: &SessionMeta, standing: Standing) -> u64 {
    standing
        .last_activity_at
        .unwrap_or(meta.updated_at.saturating_mul(1000))
        .max(meta.unsettled_at.unwrap_or(0).saturating_mul(1000))
}

/// The time a threads-view row shows, in Unix seconds: when a settled thread
/// settled, else its last activity, which is what an active row sorts by
/// within its [`Attention`].
pub fn shown_timestamp(meta: &SessionMeta, standing: Standing) -> u64 {
    if meta.is_settled() {
        settled_timestamp(meta)
    } else {
        last_activity(meta, standing) / 1000
    }
}

/// A thread's place among one host's threads, the least first. Pinned
/// threads by the key the user gave them, then keyless ones newest created
/// first. Active threads by [`Attention`], then by their last activity,
/// newest first: nothing arranges them by hand. Settled threads newest
/// settled first.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ThreadRank {
    Pinned {
        keyless: bool,
        key: Option<String>,
        created: std::cmp::Reverse<u64>,
    },
    Active(Attention, std::cmp::Reverse<u64>),
    Settled(std::cmp::Reverse<u64>),
}

/// The rank `meta` has in Active, or would have once it is there.
pub fn active_rank(meta: &SessionMeta, standing: Standing) -> ThreadRank {
    ThreadRank::Active(
        standing.attention,
        std::cmp::Reverse(last_activity(meta, standing)),
    )
}

pub fn thread_rank(meta: &SessionMeta, standing: Standing) -> ThreadRank {
    match thread_section(meta) {
        ThreadSection::Settled => ThreadRank::Settled(std::cmp::Reverse(settled_timestamp(meta))),
        ThreadSection::Pinned => ThreadRank::Pinned {
            keyless: meta.pin_order.is_none(),
            key: meta.pin_order.clone(),
            created: std::cmp::Reverse(meta.created_at),
        },
        ThreadSection::Active => active_rank(meta, standing),
    }
}

/// `sessions` by [`ThreadRank`], then by id.
pub fn sort_threads(sessions: &mut [SessionMeta], standing: impl Fn(&SessionMeta) -> Standing) {
    sessions.sort_by_cached_key(|meta| (thread_rank(meta, standing(meta)), meta.id.clone()));
}

/// Threads split by section, each keeping the order it was given in.
#[derive(Debug, Default)]
pub struct ThreadSections<T> {
    pub pinned: Vec<T>,
    pub active: Vec<T>,
    pub settled: Vec<T>,
}

impl<T> ThreadSections<T> {
    pub fn section(&self, section: ThreadSection) -> &Vec<T> {
        match section {
            ThreadSection::Pinned => &self.pinned,
            ThreadSection::Active => &self.active,
            ThreadSection::Settled => &self.settled,
        }
    }

    pub fn section_mut(&mut self, section: ThreadSection) -> &mut Vec<T> {
        match section {
            ThreadSection::Pinned => &mut self.pinned,
            ThreadSection::Active => &mut self.active,
            ThreadSection::Settled => &mut self.settled,
        }
    }
}

pub fn partition_threads<'a>(
    sessions: impl IntoIterator<Item = &'a SessionMeta>,
) -> ThreadSections<&'a SessionMeta> {
    let mut sections = ThreadSections {
        pinned: vec![],
        active: vec![],
        settled: vec![],
    };
    for meta in sessions {
        match thread_section(meta) {
            ThreadSection::Pinned => sections.pinned.push(meta),
            ThreadSection::Active => sections.active.push(meta),
            ThreadSection::Settled => sections.settled.push(meta),
        }
    }
    sections
}

/// Several hosts' rows of one section as one list, each sequence already in
/// its host's order: the head with the least `key` goes next, earlier
/// sequences first on a tie. Hosts are never sorted against each other, so
/// the order each host's user arranged by hand survives the merge.
pub fn merge_hosts<T, K: Ord>(
    sequences: impl IntoIterator<Item = Vec<T>>,
    key: impl Fn(&T) -> K,
) -> Vec<T> {
    let mut heads: Vec<_> = sequences
        .into_iter()
        .map(|rows| rows.into_iter().peekable())
        .collect();
    let mut merged = Vec::new();
    loop {
        let mut next: Option<(usize, K)> = None;
        for (index, rows) in heads.iter_mut().enumerate() {
            if let Some(row) = rows.peek() {
                let key = key(row);
                if next.as_ref().is_none_or(|(_, least)| key < *least) {
                    next = Some((index, key));
                }
            }
        }
        let Some((index, _)) = next else {
            return merged;
        };
        merged.extend(heads[index].next());
    }
}

// Order keys are base-26 fractions in (0, 1) written with the digits a–z and
// compared as plain strings, so a move writes one key to one thread and every
// client converges on the same order. A trailing `a` would leave no key
// immediately before it; generators never produce one.
const DIGITS: &[u8; 26] = b"abcdefghijklmnopqrstuvwxyz";

fn valid_order_key(key: &str) -> bool {
    !key.is_empty() && key.bytes().all(|byte| byte.is_ascii_lowercase()) && !key.ends_with('a')
}

/// The midpoint of two digit strings; an empty string is the open bound on
/// either side. Requires `a < b`.
fn midpoint(a: &[u8], b: &[u8]) -> Vec<u8> {
    if !b.is_empty() {
        // Past the longest common prefix; the shorter side pads with `a`.
        let mut n = 0;
        while b.get(n) == Some(&a.get(n).copied().unwrap_or(b'a')) {
            n += 1;
        }
        if n > 0 {
            let mut key = b[..n].to_vec();
            key.extend(midpoint(a.get(n..).unwrap_or_default(), &b[n..]));
            return key;
        }
    }
    let digit_a = a.first().map_or(0, |digit| usize::from(digit - b'a'));
    let digit_b = b
        .first()
        .map_or(DIGITS.len(), |digit| usize::from(digit - b'a'));
    if digit_b - digit_a > 1 {
        return vec![DIGITS[(digit_a + digit_b).div_ceil(2)]];
    }
    // Consecutive leading digits: shorten into `b` when it has more digits,
    // or else extend `a`.
    if b.len() > 1 {
        return vec![b[0]];
    }
    let mut key = vec![DIGITS[digit_a]];
    key.extend(midpoint(a.get(1..).unwrap_or_default(), &[]));
    key
}

/// A key that sorts strictly between two neighbours; `None` bounds are the
/// ends of the section. `None` when a bound is corrupt or the bounds are
/// reversed: the caller spreads fresh keys instead ([`plan_reorder`]).
pub fn order_key_between(before: Option<&str>, after: Option<&str>) -> Option<String> {
    let a = before.unwrap_or_default();
    let b = after.unwrap_or_default();
    if (!a.is_empty() && !valid_order_key(a))
        || (!b.is_empty() && !valid_order_key(b))
        || (!b.is_empty() && a >= b)
    {
        return None;
    }
    String::from_utf8(midpoint(a.as_bytes(), b.as_bytes())).ok()
}

/// `count` evenly spaced keys in ascending order, wide enough that a key
/// still fits between any two of them.
pub fn spread_order_keys(count: usize) -> Vec<String> {
    let base = DIGITS.len() as u64;
    let mut width = 2;
    let mut space = base * base;
    while space <= (count as u64 + 1) * 2 {
        width += 1;
        space *= base;
    }
    let step = space as f64 / (count + 1) as f64;
    (1..=count)
        .map(|index| {
            let mut value = (step * index as f64).round() as u64;
            if value.is_multiple_of(base) {
                value += 1;
            }
            let mut key = vec![b'a'; width];
            for slot in key.iter_mut().rev() {
                *slot = DIGITS[(value % base) as usize];
                value /= base;
            }
            String::from_utf8(key).expect("ASCII digits")
        })
        .collect()
}

/// The key writes that put a section in `ordered` (thread id and current
/// key, after the move) once `moved` has moved. Between keyed neighbours this
/// is one write to `moved`; next to a keyless neighbour, or between corrupt
/// keys, every listed thread gets a fresh spread key. `hidden` holds the keys
/// of the section's threads that are not listed: they are never written and
/// never reused, so those threads keep their places.
pub fn plan_reorder(
    ordered: &[(&str, Option<&str>)],
    hidden: &[&str],
    moved: &str,
) -> Vec<(String, String)> {
    let reserved: HashSet<&str> = hidden.iter().copied().collect();
    let Some(index) = ordered.iter().position(|(id, _)| *id == moved) else {
        return vec![];
    };
    let before = index.checked_sub(1).map(|index| ordered[index].1);
    let after = ordered.get(index + 1).map(|(_, key)| *key);
    if before.is_none_or(|key| key.is_some()) && after.is_none_or(|key| key.is_some()) {
        let after = after.flatten();
        let mut key = order_key_between(before.flatten(), after);
        while let Some(found) = key.as_deref().filter(|key| reserved.contains(key)) {
            key = order_key_between(Some(found), after);
        }
        if let Some(key) = key {
            return vec![(moved.to_owned(), key)];
        }
    }
    let keys = spread_order_keys(ordered.len() + reserved.len())
        .into_iter()
        .filter(|key| !reserved.contains(key.as_str()));
    ordered
        .iter()
        .zip(keys)
        .filter(|((_, current), key)| *current != Some(key.as_str()))
        .map(|((id, _), key)| ((*id).to_owned(), key))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(id: &str, created: u64, updated: u64) -> SessionMeta {
        let mut meta = SessionMeta::new(agent::ProviderKind::Codex, "/sample".into(), None);
        meta.id = id.into();
        meta.created_at = created;
        meta.updated_at = updated;
        meta
    }

    fn ids<'a>(sessions: impl IntoIterator<Item = &'a SessionMeta>) -> Vec<&'a str> {
        sessions.into_iter().map(|meta| meta.id.as_str()).collect()
    }

    /// Active threads order by what they want of the user and then by their
    /// last message or run, which a later change to the meta alone does not
    /// move; a reopen counts as activity, and a pin given a key before the
    /// first pinned one leads Pinned.
    #[test]
    fn active_threads_order_by_attention_then_activity() {
        let standing = |meta: &SessionMeta| {
            let (attention, last_activity_at) = match meta.id.as_str() {
                "waiting" => (Attention::Waiting, 1_000),
                "unread" => (Attention::Unread, 90_000),
                "working" => (Attention::Working, 2_000),
                "renamed" => (Attention::Idle, 30_000),
                "recent" => (Attention::Idle, 50_000),
                _ => return Standing::default(),
            };
            Standing {
                attention,
                last_activity_at: Some(last_activity_at),
            }
        };
        let mut reopened = thread("reopened", 10, 10);
        reopened.unsettled_at = Some(60);
        let mut pinned = thread("pinned", 1, 1);
        pinned.pinned_at = Some(1);
        pinned.pin_order = Some("h".into());
        let mut sessions = vec![
            thread("recent", 50, 50),
            thread("renamed", 30, 500),
            thread("working", 2, 2),
            thread("unread", 90, 90),
            thread("waiting", 1, 1),
            reopened,
            pinned,
        ];
        sort_threads(&mut sessions, standing);
        assert_eq!(
            ids(&sessions),
            [
                "pinned", "waiting", "unread", "working", "reopened", "recent", "renamed"
            ]
        );
        let reopened = sessions.iter().find(|meta| meta.id == "reopened").unwrap();
        assert_eq!(
            shown_timestamp(reopened, standing(reopened)),
            60,
            "a row shows the time it sorts by"
        );

        let first = sessions[0].pin_order.clone();
        let fresh = sessions
            .iter_mut()
            .find(|meta| meta.id == "recent")
            .unwrap();
        fresh.pinned_at = Some(300);
        fresh.pin_order = order_key_between(None, first.as_deref());
        sort_threads(&mut sessions, standing);
        assert_eq!(ids(&sessions)[..2], ["recent", "pinned"]);
    }

    /// Two hosts' Active threads, each in its host's order, merge by the
    /// same rank; on a tie the first host's row leads.
    #[test]
    fn several_hosts_active_threads_merge_by_rank() {
        let standing = |meta: &SessionMeta| Standing {
            attention: if meta.id == "b-waiting" {
                Attention::Waiting
            } else {
                Attention::Idle
            },
            last_activity_at: None,
        };
        let mut a = vec![thread("a-old", 1, 10), thread("a-new", 1, 50)];
        let mut b = vec![
            thread("b-tie", 1, 10),
            thread("b-newer", 1, 40),
            thread("b-waiting", 1, 5),
        ];
        sort_threads(&mut a, standing);
        sort_threads(&mut b, standing);
        let merged = merge_hosts([a.iter().collect(), b.iter().collect()], |meta| {
            thread_rank(meta, standing(meta))
        });
        assert_eq!(
            ids(merged),
            ["b-waiting", "a-new", "b-newer", "a-old", "b-tie"]
        );
    }

    #[test]
    fn keys_fit_between_any_neighbours_and_refuse_corrupt_bounds() {
        let between = |a: Option<&str>, b: Option<&str>| order_key_between(a, b).unwrap();
        for (a, b) in [
            (None, None),
            (None, Some("b")),
            (Some("z"), None),
            (Some("b"), Some("c")),
            (Some("b"), Some("bb")),
            (Some("bz"), Some("c")),
            (Some("mzzz"), Some("n")),
        ] {
            let key = between(a, b);
            assert!(valid_order_key(&key), "{a:?} {b:?} gave {key}");
            assert!(a.is_none_or(|a| a < key.as_str()), "{a:?} < {key}");
            assert!(b.is_none_or(|b| key.as_str() < b), "{key} < {b:?}");
        }
        for (a, b) in [
            (Some("c"), Some("b")),
            (Some("b"), Some("b")),
            (Some("ba"), None),
            (None, Some("B")),
        ] {
            assert_eq!(order_key_between(a, b), None, "{a:?} {b:?}");
        }
    }

    #[test]
    fn spread_keys_stay_ordered_and_insertable() {
        for count in [0, 1, 650, 676, 2_000] {
            let keys = spread_order_keys(count);
            assert_eq!(keys.len(), count);
            assert!(keys.windows(2).all(|pair| pair[0] < pair[1]), "{count}");
            for (index, key) in keys.iter().enumerate() {
                assert!(valid_order_key(key), "{key}");
                let before = index.checked_sub(1).map(|index| keys[index].as_str());
                assert!(order_key_between(before, Some(key)).is_some(), "{key}");
            }
        }
    }

    #[test]
    fn a_move_writes_one_key_unless_a_neighbour_has_none() {
        let hidden = order_key_between(Some("f"), Some("t")).unwrap();
        let writes = plan_reorder(
            &[("a", Some("f")), ("moved", Some("z")), ("b", Some("t"))],
            &[&hidden],
            "moved",
        );
        assert_eq!(writes.len(), 1);
        let (id, key) = &writes[0];
        assert_eq!(id, "moved");
        assert!("f" < key.as_str() && key.as_str() < "t" && *key != hidden);

        let reserved = spread_order_keys(6);
        let reserved: Vec<_> = reserved.iter().map(String::as_str).collect();
        let writes = plan_reorder(
            &[("c", None), ("a", None), ("b", Some("m"))],
            &reserved,
            "c",
        );
        let ids: Vec<_> = writes.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["c", "a", "b"]);
        let keys: Vec<_> = writes.iter().map(|(_, key)| key.as_str()).collect();
        assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(keys.iter().all(|key| !reserved.contains(key)));

        let corrupt = plan_reorder(
            &[("a", Some("t")), ("moved", None), ("b", Some("f"))],
            &[],
            "moved",
        );
        let keys: Vec<_> = corrupt.iter().map(|(_, key)| key.as_str()).collect();
        assert_eq!(corrupt.len(), 3, "reversed neighbours respread the section");
        assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
    }
}
