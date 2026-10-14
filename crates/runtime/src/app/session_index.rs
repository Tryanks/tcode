use std::{
    cell::OnceCell,
    collections::HashMap,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicU64, Ordering},
};
use tcode_core::project::SessionMeta;

/// Ordered metadata with a parent lookup invalidated by every mutable access.
/// Positions are cached only while the underlying order and metadata are immutable.
pub struct SessionIndex {
    records: Vec<SessionMeta>,
    revision: u64,
    children: OnceCell<HashMap<String, Vec<usize>>>,
}

impl SessionIndex {
    pub(super) fn revision(&self) -> u64 {
        self.revision
    }

    pub(super) fn children(&self) -> &HashMap<String, Vec<usize>> {
        self.children.get_or_init(|| {
            let mut children: HashMap<String, Vec<usize>> = HashMap::new();
            for (index, meta) in self.records.iter().enumerate() {
                if meta.archived_at.is_none()
                    && let Some(parent) = &meta.parent_session_id
                {
                    children.entry(parent.clone()).or_default().push(index);
                }
            }
            children
        })
    }
}

impl From<Vec<SessionMeta>> for SessionIndex {
    fn from(records: Vec<SessionMeta>) -> Self {
        Self {
            records,
            revision: next_revision(),
            children: OnceCell::new(),
        }
    }
}

impl Deref for SessionIndex {
    type Target = Vec<SessionMeta>;
    fn deref(&self) -> &Self::Target {
        &self.records
    }
}

impl DerefMut for SessionIndex {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.children.take();
        self.revision = next_revision();
        &mut self.records
    }
}

impl<'a> IntoIterator for &'a SessionIndex {
    type Item = &'a SessionMeta;
    type IntoIter = std::slice::Iter<'a, SessionMeta>;
    fn into_iter(self) -> Self::IntoIter {
        self.records.iter()
    }
}

// Whole-index replacement must not reuse the previous instance's revision.
fn next_revision() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}
