//! Thread sections and static lifecycle ordering, independent of client layout.
use crate::project::SessionMeta;
use std::cmp::Ordering;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ThreadSection {
    Active,
    Settled,
}

/// Whether a thread is listed among the threads: archived threads, dispatched
/// orchestrate children and provider-native subagents are not; forks are.
pub fn in_roster(meta: &SessionMeta) -> bool {
    meta.archived_at.is_none() && !meta.is_subagent()
}

pub fn thread_section(meta: &SessionMeta) -> ThreadSection {
    if meta.is_settled() {
        ThreadSection::Settled
    } else {
        ThreadSection::Active
    }
}

/// When a settled thread settled, in Unix seconds; the time it shows and sorts by.
pub fn settled_timestamp(meta: &SessionMeta) -> u64 {
    meta.settled_at.unwrap_or(meta.updated_at)
}

/// Active threads newest created or reopened first; settled threads newest
/// settled first. Activity never reorders a row.
pub fn compare_threads(a: &SessionMeta, b: &SessionMeta) -> Ordering {
    let section = thread_section(a);
    section
        .cmp(&thread_section(b))
        .then_with(|| match section {
            ThreadSection::Settled => settled_timestamp(b).cmp(&settled_timestamp(a)),
            ThreadSection::Active => b
                .created_at
                .max(b.unsettled_at.unwrap_or(0))
                .cmp(&a.created_at.max(a.unsettled_at.unwrap_or(0))),
        })
        .then_with(|| a.id.cmp(&b.id))
}

pub fn sort_threads(sessions: &mut [SessionMeta]) {
    sessions.sort_by(compare_threads);
}

pub fn partition_threads(sessions: &[SessionMeta]) -> (Vec<SessionMeta>, Vec<SessionMeta>) {
    sessions
        .iter()
        .cloned()
        .partition(|meta| thread_section(meta) == ThreadSection::Active)
}
