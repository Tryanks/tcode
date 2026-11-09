use std::{
    collections::HashMap,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicU64, Ordering},
};

/// Invalidates derived projections on mutable access, including whole-value replacement.
/// Values used here must not mutate projection inputs through shared references.
pub struct Revisioned<T> {
    value: T,
    revision: u64,
}

impl<T> Revisioned<T> {
    pub(super) fn revision(&self) -> u64 {
        self.revision
    }
}

impl<T> From<T> for Revisioned<T> {
    fn from(value: T) -> Self {
        Self {
            value,
            revision: next_revision(),
        }
    }
}

impl<T: Default> Default for Revisioned<T> {
    fn default() -> Self {
        T::default().into()
    }
}

impl<T> Deref for Revisioned<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> DerefMut for Revisioned<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.revision = next_revision();
        &mut self.value
    }
}

static NEXT: AtomicU64 = AtomicU64::new(1);

fn next_revision() -> u64 {
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// A [`Revisioned`] map whose entries borrowed one at a time through
/// [`RevisionedMap::get_mut`] are recorded by key instead of invalidating the
/// whole map, so a projection over every entry can redo just the ones
/// touched. Any other mutable access invalidates the whole map.
pub struct RevisionedMap<V> {
    map: Revisioned<HashMap<String, V>>,
    touched: HashMap<String, u64>,
}

impl<V> RevisionedMap<V> {
    /// Moved only by mutable access to the map as a whole.
    pub(super) fn revision(&self) -> u64 {
        self.map.revision()
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut V> {
        let value = self.map.value.get_mut(key)?;
        self.touched.insert(key.to_owned(), next_revision());
        Some(value)
    }

    /// Keys borrowed through [`RevisionedMap::get_mut`] at or after
    /// `revision`, since the map as a whole last moved.
    pub(super) fn touched_since(&self, revision: u64) -> impl Iterator<Item = &str> {
        self.touched
            .iter()
            .filter(move |(_, touched)| **touched >= revision)
            .map(|(key, _)| key.as_str())
    }
}

impl<V> Default for RevisionedMap<V> {
    fn default() -> Self {
        Self {
            map: Revisioned::default(),
            touched: HashMap::new(),
        }
    }
}

impl<V> Deref for RevisionedMap<V> {
    type Target = HashMap<String, V>;
    fn deref(&self) -> &Self::Target {
        &self.map
    }
}

impl<V> DerefMut for RevisionedMap<V> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.touched.clear();
        &mut self.map
    }
}

/// The revision the next change will carry: anything stamped at or after it
/// changed after this call.
pub(super) fn upcoming_revision() -> u64 {
    NEXT.load(Ordering::Relaxed)
}
