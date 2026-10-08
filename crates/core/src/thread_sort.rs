//! Thread sections and static lifecycle ordering, independent of client layout.
use crate::{project::SessionMeta, settlement::ThreadActivity};
use std::{cmp::Ordering, collections::HashMap};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ThreadSection {
    Pinned,
    Active,
    Settled,
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

pub fn settled_timestamp_ms(meta: &SessionMeta, activity: Option<&ThreadActivity>) -> u64 {
    meta.settled_at
        .map(|at| at.saturating_mul(1000))
        .or_else(|| activity.and_then(ThreadActivity::last_activity_at))
        .unwrap_or_else(|| meta.updated_at.saturating_mul(1000))
}

pub fn compare_threads(
    a: &SessionMeta,
    b: &SessionMeta,
    clocks: &HashMap<String, ThreadActivity>,
) -> Ordering {
    let section = thread_section(a);
    section
        .cmp(&thread_section(b))
        .then_with(|| match section {
            ThreadSection::Settled => settled_timestamp_ms(b, clocks.get(&b.id))
                .cmp(&settled_timestamp_ms(a, clocks.get(&a.id))),
            ThreadSection::Active => match (&a.active_order, &b.active_order) {
                (None, Some(_)) => Ordering::Less,
                (Some(_), None) => Ordering::Greater,
                (Some(a), Some(b)) => a.cmp(b),
                (None, None) => b
                    .created_at
                    .max(b.unsettled_at.unwrap_or(0))
                    .cmp(&a.created_at.max(a.unsettled_at.unwrap_or(0))),
            },
            ThreadSection::Pinned => match (&a.pin_order, &b.pin_order) {
                (Some(a), Some(b)) => a.cmp(b),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => b.created_at.cmp(&a.created_at),
            },
        })
        .then_with(|| a.id.cmp(&b.id))
}

pub fn sort_threads(sessions: &mut [SessionMeta], clocks: &HashMap<String, ThreadActivity>) {
    sessions.sort_by(|a, b| compare_threads(a, b, clocks));
}

pub fn partition_threads(
    sessions: &[SessionMeta],
) -> (Vec<SessionMeta>, Vec<SessionMeta>, Vec<SessionMeta>) {
    let (mut pinned, mut active, mut settled) = (vec![], vec![], vec![]);
    for meta in sessions {
        match thread_section(meta) {
            ThreadSection::Pinned => pinned.push(meta.clone()),
            ThreadSection::Active => active.push(meta.clone()),
            ThreadSection::Settled => settled.push(meta.clone()),
        }
    }
    (pinned, active, settled)
}

/// Fractional base-26 key strictly between the neighbors; corrupt bounds refuse the reorder.
pub fn order_key_between(before: Option<&str>, after: Option<&str>) -> Option<String> {
    let a = before.unwrap_or("");
    let b = after.unwrap_or("");
    let valid = |key: &str| key.bytes().all(|ch| ch.is_ascii_lowercase()) && !key.ends_with('a');
    if (!a.is_empty() && !valid(a)) || (!b.is_empty() && !valid(b)) || (!b.is_empty() && a >= b) {
        return None;
    }
    fn midpoint(a: &[u8], b: &[u8]) -> String {
        let mut n = 0;
        while n < b.len() && a.get(n).copied().unwrap_or(b'a') == b[n] {
            n += 1;
        }
        if n > 0 {
            return format!(
                "{}{}",
                String::from_utf8_lossy(&b[..n]),
                midpoint(a.get(n..).unwrap_or(&[]), &b[n..])
            );
        }
        let low = a.first().copied().unwrap_or(b'a') - b'a';
        let high = b.first().map_or(26, |ch| ch - b'a');
        if high - low > 1 {
            return char::from(b'a' + (low + high).div_ceil(2)).to_string();
        }
        if b.len() > 1 {
            return char::from(b[0]).to_string();
        }
        format!(
            "{}{}",
            char::from(b'a' + low),
            midpoint(a.get(1..).unwrap_or(&[]), &[])
        )
    }
    Some(midpoint(a.as_bytes(), b.as_bytes()))
}
