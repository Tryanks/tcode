use std::collections::HashSet;
use std::ops::Range;

/// Before GPUI reports its exact visible range, twelve rows is comfortably
/// more than a typical chat viewport. Its list also pre-measures four viewport
/// heights, so the wider eviction band keeps warm rows resident without tying
/// Markdown lifetime to measurement.
const VIEWPORT_ROW_HINT: usize = 12;
const BUILD_MARGIN_ROWS: usize = 24;
/// Three build margins prevent back-and-forth scrolling from rebuilding the
/// same parsed documents at the edge of the warm window.
const EVICT_MARGIN_ROWS: usize = 72;
/// The composer-adjacent tail stays ready even while inspecting old rows.
const TAIL_PIN_ROWS: usize = 4;

#[derive(Clone, Debug)]
pub(super) struct MarkdownEntry {
    pub id: String,
    /// The timeline row that renders this document.
    pub row: usize,
}

pub(super) struct ResidencyInput<'a> {
    pub row_count: usize,
    pub visible_rows: Range<usize>,
    pub one_shot_row_target: Option<usize>,
    pub entries: &'a [MarkdownEntry],
    pub stream_running: bool,
    pub resident_ids: &'a HashSet<String>,
}

/// Row-only superset of entries that can affect [`decide`].
pub(super) struct ResidencyScope {
    build_rows: Range<usize>,
    keep_rows: Range<usize>,
    tail_start: usize,
    last_row: Option<usize>,
    stream_running: bool,
}

impl ResidencyScope {
    pub(super) fn new(
        row_count: usize,
        visible_rows: Range<usize>,
        one_shot_row_target: Option<usize>,
        stream_running: bool,
    ) -> Self {
        let visible_rows = one_shot_row_target
            .filter(|row| *row < row_count)
            .map(|row| row..(row + VIEWPORT_ROW_HINT).min(row_count))
            .unwrap_or(visible_rows);
        Self {
            build_rows: expand_row_window(visible_rows.clone(), BUILD_MARGIN_ROWS, row_count),
            keep_rows: expand_row_window(visible_rows, EVICT_MARGIN_ROWS, row_count),
            tail_start: row_count.saturating_sub(TAIL_PIN_ROWS),
            last_row: row_count.checked_sub(1),
            stream_running,
        }
    }

    pub(super) fn includes(&self, row: usize) -> bool {
        self.keep_rows.contains(&row) || self.pinned(row)
    }

    /// The tail rows stay resident: the composer sits under them, and a
    /// running turn streams into the last of them. The rest of a running
    /// turn is ordinary history, however long it grows.
    fn pinned(&self, row: usize) -> bool {
        self.last_row.is_some() && row >= self.tail_start
            || self.stream_running && self.last_row == Some(row)
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ResidencyDecisions {
    pub build: HashSet<String>,
    pub evict: HashSet<String>,
}

pub(super) fn decide(input: ResidencyInput<'_>) -> ResidencyDecisions {
    let scope = ResidencyScope::new(
        input.row_count,
        input.visible_rows,
        input.one_shot_row_target,
        input.stream_running,
    );

    let build = input
        .entries
        .iter()
        .filter(|entry| scope.build_rows.contains(&entry.row) || scope.pinned(entry.row))
        .map(|entry| entry.id.clone())
        .collect();
    let keep = input
        .entries
        .iter()
        .filter(|entry| scope.includes(entry.row))
        .map(|entry| entry.id.as_str())
        .collect::<HashSet<_>>();
    let evict = input
        .resident_ids
        .iter()
        .filter(|id| !keep.contains(id.as_str()))
        .cloned()
        .collect();

    ResidencyDecisions { build, evict }
}

pub(super) fn tail_row_window(row_count: usize) -> Range<usize> {
    row_count.saturating_sub(VIEWPORT_ROW_HINT)..row_count
}

pub(super) fn viewport_row_window(scroll_top: usize, row_count: usize) -> Range<usize> {
    scroll_top..(scroll_top + VIEWPORT_ROW_HINT).min(row_count)
}

fn expand_row_window(window: Range<usize>, margin: usize, row_count: usize) -> Range<usize> {
    window.start.min(row_count).saturating_sub(margin)
        ..window
            .end
            .min(row_count)
            .saturating_add(margin)
            .min(row_count)
}
