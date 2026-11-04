use std::{
    collections::{HashMap, VecDeque},
    hash::Hash,
};

/// The most recent results of a render step keyed by its inputs, so a repaint
/// reuses the work and a long conversation does not keep every result.
pub(crate) struct RecentCache<K, V> {
    capacity: usize,
    entries: HashMap<K, V>,
    order: VecDeque<K>,
    weight: usize,
    weight_limit: usize,
    weigh: fn(&K, &V) -> usize,
}

impl<K: Clone + Eq + Hash, V: Clone> RecentCache<K, V> {
    pub(crate) fn new(capacity: usize) -> Self {
        Self::with_weight_limit(capacity, capacity, |_, _| 1)
    }

    pub(crate) fn with_weight_limit(
        capacity: usize,
        weight_limit: usize,
        weigh: fn(&K, &V) -> usize,
    ) -> Self {
        Self {
            capacity,
            entries: HashMap::new(),
            order: VecDeque::new(),
            weight: 0,
            weight_limit,
            weigh,
        }
    }

    pub(crate) fn remove(&mut self, key: &K) {
        if let Some(value) = self.entries.remove(key) {
            self.weight -= (self.weigh)(key, &value);
            self.order.retain(|entry| entry != key);
        }
    }

    pub(crate) fn get_or_insert_with(&mut self, key: K, compute: impl FnOnce() -> V) -> V {
        if let Some(value) = self.entries.get(&key) {
            return value.clone();
        }
        let value = compute();
        let weight = (self.weigh)(&key, &value);
        // Keep the newest result even when it alone exceeds the budget: a single
        // large block's visible slices must share one highlighting pass.
        while !self.entries.is_empty()
            && (self.entries.len() >= self.capacity
                || self.weight.saturating_add(weight) > self.weight_limit)
        {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(value) = self.entries.remove(&oldest) {
                self.weight -= (self.weigh)(&oldest, &value);
            }
        }
        self.weight = self.weight.saturating_add(weight);
        self.order.push_back(key.clone());
        self.entries.insert(key, value.clone());
        value
    }
}
