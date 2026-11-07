use super::Revisioned;
use std::{
    cell::OnceCell,
    collections::HashMap,
    ops::{Deref, DerefMut},
};
use tcode_core::project::SessionMeta;

/// Ordered metadata with id and parent lookups invalidated by every mutable
/// access. Positions are cached only while the underlying order and metadata
/// are immutable.
pub struct SessionIndex {
    records: Revisioned<Vec<SessionMeta>>,
    children: OnceCell<HashMap<String, Vec<usize>>>,
    positions: OnceCell<HashMap<String, usize>>,
}

impl SessionIndex {
    pub(super) fn revision(&self) -> u64 {
        self.records.revision()
    }

    pub(super) fn by_id(&self, id: &str) -> Option<&SessionMeta> {
        let positions = self.positions.get_or_init(|| {
            let mut positions = HashMap::with_capacity(self.records.len());
            for (index, meta) in self.records.iter().enumerate() {
                positions.entry(meta.id.clone()).or_insert(index);
            }
            positions
        });
        positions.get(id).map(|index| &self.records[*index])
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
            records: records.into(),
            children: OnceCell::new(),
            positions: OnceCell::new(),
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
        self.positions.take();
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
