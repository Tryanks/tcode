//! Which timeline rows rise from below: the timeline's stamp policy over
//! [`crate::motion::list_rows`].
//!
//! While the reader follows the tail the list keeps its end on the
//! viewport's bottom edge, so a row whose layout height grows carries the
//! content above it upward: the row rises from below.

use gpui::App;
use tcode_core::session::EntryContent;

use crate::motion::list_rows::{Arrival, Attach};

use super::LeftRow;
use super::TimelineView;
use super::rows::{RowPart, TimelineRow};

/// The rows a regroup takes out, each with the survivor it folds against.
/// The reader's anchor is never among them: the header they collapse is on
/// screen above them.
#[derive(Default)]
pub(super) struct Leaving(Vec<(u64, LeftRow, Attach)>);

impl TimelineView {
    /// Stamp the rows that just arrived live, so they rise in; each notes
    /// whether it is the first on screen of its turn, so the turn's trailer
    /// (its working status) arrives with it rather than moving down to it.
    /// A reader who
    /// paused above the tail is not moved: only rows below the one at the top
    /// of the viewport arrive, and they only grow downward.
    pub(super) fn stamp_arrivals(
        &mut self,
        arrived: Vec<(usize, Arrival)>,
        old_last_turn: Option<usize>,
        cx: &App,
    ) {
        if arrived.is_empty() {
            return;
        }
        let following = self.list_state.is_following_tail();
        let top = self.list_state.logical_scroll_top().item_ix;
        let at = cx.background_executor().now();
        let (rows, shown) = (&self.rows, &self.shown_deliveries);
        let stamps = self
            .workspace_store
            .read(cx)
            .with_timeline_for(
                self.session_id.as_deref(),
                |timeline| {
                    arrived
                        .into_iter()
                        .filter(|(index, _)| following || *index > top)
                        .filter_map(|(index, arrival)| {
                            let row = rows.get(index)?;
                            // A message this window sent was on screen as its
                            // delivery bubble, and a steer taken after everything
                            // in its turn as its pending steer in the trailer just
                            // above: the row is that message moving into place. A
                            // steer taken before what followed it opens its place
                            // as its pending one folds away and it travels there.
                            let on_screen = (!row.entry_range.is_empty())
                                .then(|| timeline.entries.get(row.entry_range.start))
                                .flatten()
                                .is_some_and(|entry| {
                                    matches!(entry.content, EntryContent::Steer { .. })
                                        && arrival == Arrival::Appended
                                        || shown
                                            .iter()
                                            .any(|key| entry.id == format!("local-user-{key}"))
                                });
                            (!on_screen).then_some((
                                row.identity,
                                arrival,
                                old_last_turn.is_none_or(|turn| row.turn > turn),
                            ))
                        })
                        .collect::<Vec<_>>()
                },
                cx,
            )
            .unwrap_or_default();
        for (key, arrival, brings_turn) in stamps {
            self.rows_motion.arrive(key, arrival, brings_turn, at);
        }
    }
}

impl TimelineView {
    /// The rows of `old` a regroup took out, as they were drawn: a collapsed
    /// Work Log's activities. An expanded live window is not among them: its
    /// activities stay on screen in the run's own row.
    pub(super) fn rows_leaving(&self, old: &[TimelineRow], cx: &App) -> Leaving {
        let keys = |rows: &[TimelineRow]| rows.iter().map(|row| row.identity).collect::<Vec<_>>();
        let left = crate::motion::list_rows::changes(&keys(old), &keys(&self.rows)).left;
        if left.is_empty() {
            return Leaving::default();
        }
        let rows = self
            .workspace_store
            .read(cx)
            .with_timeline_for(
                self.session_id.as_deref(),
                |timeline| {
                    left.into_iter()
                        .filter(|(index, _)| old[*index].part != RowPart::WorkLogLive)
                        .filter_map(|(index, attach)| {
                            let row = &old[index];
                            let own = timeline.entries.get(row.entry_range.clone())?.to_vec();
                            Some((
                                row.identity,
                                LeftRow {
                                    row: TimelineRow {
                                        last_in_turn: false,
                                        ..row.clone()
                                    },
                                    turn: timeline.turns.get(row.turn).cloned().unwrap_or_default(),
                                    own,
                                },
                                attach,
                            ))
                        })
                        .collect()
                },
                cx,
            )
            .unwrap_or_default();
        Leaving(rows)
    }

    /// Fold the rows a regroup took out away against their survivors.
    pub(super) fn fold_away(&mut self, leaving: Leaving, cx: &App) {
        let at = cx.background_executor().now();
        for (key, row, attach) in leaving.0 {
            self.rows_motion.leave(key, row, attach, at);
        }
    }
}
