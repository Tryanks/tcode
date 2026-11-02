//! Rows that come into a sidebar list and go out of it: a thread started,
//! archived or settled away, a caption a section brings, the rows "Show
//! more" adds, the hint an emptied list shows.
//!
//! A sidebar list's disclosures fold their own rows (`fold_row`), so a row
//! one brings or takes — one below full height, or one a moving disclosure
//! leaves out of the list — is the disclosure's, and arrives or leaves here
//! only when no disclosure moves it.

use std::{
    collections::{HashMap, HashSet},
    hash::{DefaultHasher, Hash, Hasher},
};

use gpui::{App, ListState, px};

use crate::motion::list_rows::{Around, ListRows};

/// A row's key in [`RowPresence`], from what names it in its list.
pub(super) fn row_key(name: impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    name.hash(&mut hasher);
    hasher.finish()
}

/// One list's rows coming and going, by row key, kept beside its
/// `ListState`. `V` is what a leaving row is drawn from.
pub(super) struct RowPresence<V> {
    rows: ListRows<(), V>,
    /// Each row the list had last frame, those a moving disclosure left out
    /// included, with its fold factor.
    factors: HashMap<u64, f32>,
    /// The keys of the rows the list holds.
    keys: Vec<u64>,
}

impl<V> Default for RowPresence<V> {
    fn default() -> Self {
        Self {
            rows: ListRows::default(),
            factors: HashMap::new(),
            keys: Vec::new(),
        }
    }
}

impl<V: Clone> RowPresence<V> {
    /// The list's rows are now `rows`, each a key with its fold factor (a
    /// negative one is a row a moving disclosure leaves out, which the list
    /// does not hold). Called before `list` is spliced, while it still lays
    /// out the rows it drew: a row that came in where the view was arrives,
    /// and one that went while in view folds away, drawn from what `left`
    /// gives for its old index. A list replaced whole (another project
    /// filter, another layout) is the same change of many rows at once. The
    /// rows a list opens with are in place at once, as are rows that came in
    /// out of view. While a disclosure moves (`folding`), what comes and goes
    /// is its rows, which it folds.
    pub(super) fn update(
        &mut self,
        rows: &[(u64, f32)],
        folding: bool,
        list: &ListState,
        mut left: impl FnMut(usize) -> V,
        cx: &App,
    ) {
        let keys: Vec<u64> = rows
            .iter()
            .filter(|(_, factor)| *factor >= 0.)
            .map(|(key, _)| *key)
            .collect();
        if !self.factors.is_empty() && keys != self.keys && !folding {
            let top = list.logical_scroll_top();
            let at_top = top.item_ix == 0 && top.offset_in_item <= px(0.);
            let viewport = list.viewport_bounds();
            let in_view = |index: usize| {
                list.bounds_for_item(index).is_some_and(|bounds| {
                    bounds.top() < viewport.bottom() && bounds.bottom() >= viewport.top()
                })
            };
            // How many rows the view showed: as many arrivals as that, from
            // the top of the view, can be what the reader sees come in.
            let shown = (top.item_ix..self.keys.len())
                .take_while(|index| in_view(*index))
                .count();
            let reach = match shown {
                // Not on screen: nothing came in where the reader looks.
                0 => 0..0,
                _ if at_top => 0..shown + 1,
                _ => top.item_ix + 1..top.item_ix + shown + 1,
            };
            let now: HashMap<u64, f32> = rows.iter().copied().collect();
            let present: HashSet<u64> = now.keys().copied().collect();
            let (was, old) = (&self.factors, std::mem::take(&mut self.keys));
            self.rows.replace(
                &old,
                &keys,
                |index, _| {
                    let key = keys[index];
                    let whole = now.get(&key).is_some_and(|factor| *factor >= 1.);
                    (whole && !was.contains_key(&key) && reach.contains(&index)).then_some(())
                },
                |index| {
                    let key = old[index];
                    let whole = was.get(&key).is_some_and(|factor| *factor >= 1.);
                    (whole && in_view(index) && !present.contains(&key)).then(|| left(index))
                },
                cx,
            );
        }
        self.factors = rows.iter().copied().collect();
        self.keys = keys;
    }

    /// Row `key`'s arrival and the rows folding away against it, this frame.
    pub(super) fn around(&self, key: u64, cx: &App) -> Around<V> {
        self.rows.around(key, cx)
    }

    /// Lets go of what has come in and gone; the rows the gone ones folded
    /// against are measured again.
    pub(super) fn settle(&mut self, list: &ListState, cx: &App) {
        self.rows.settle_list(list, &self.keys, cx);
    }
}
