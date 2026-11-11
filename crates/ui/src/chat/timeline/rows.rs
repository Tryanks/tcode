use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::ops::Range;
use std::sync::{Arc, Weak};

use agent::{ItemContent, TurnStatus};
use tcode_core::session::{EntryContent, SteeringStatus, TimelineEntry, TurnMeta};

use crate::chat::model::{
    Segment, disclosure_keys, live_activity_segment, partition_activity_run, segment_entries,
    work_log_key,
};

/// The list virtualizes segments rather than turns, and an expanded Work Log's
/// activities rather than its run, so a turn with hundreds of tool calls and
/// interim messages costs the rows on screen, not the whole turn, every
/// frame. A turn without segments still owns one empty row for its trailer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TimelineRow {
    pub(crate) turn: usize,
    /// The entries the row renders, in timeline order across rows.
    pub(crate) entry_range: Range<usize>,
    pub(crate) entry_count: usize,
    pub(crate) part: RowPart,
    pub(crate) first_in_turn: bool,
    pub(crate) last_in_turn: bool,
    /// The turn's live work log: the run that opens on its own while the
    /// turn runs ([`live_activity_segment`]).
    pub(crate) live_activity: bool,
    /// The turn's final assistant text, the one that carries an action row.
    pub(crate) last_assistant: bool,
    /// Identity of the row's first entry: a run grows at its end, so the
    /// row stays itself while it streams.
    pub(crate) identity: u64,
    /// Identity of the newest entry alone. A history page can only add
    /// entries before a partially loaded turn, so this survives completion
    /// while `identity` does not.
    pub(crate) tail_identity: u64,
    pub(crate) content: u64,
}

/// Which part of its segment a row renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RowPart {
    Segment,
    /// The header of an expanded Work Log. It renders no entry of its own
    /// but reads the whole run, `run`, for its summary.
    WorkLogHeader {
        run: Range<usize>,
    },
    /// One folded activity under an expanded Work Log header.
    WorkLogActivity,
    /// The live window below an expanded Work Log's folded activities.
    WorkLogLive,
}

/// The rows of `turn` within `rows`, which are sorted by turn.
pub(crate) fn rows_of_turn(rows: &[TimelineRow], turn: usize) -> Range<usize> {
    let start = rows.partition_point(|row| row.turn < turn);
    let end = start + rows[start..].partition_point(|row| row.turn == turn);
    start..end
}

/// The row that renders the entry at `entry_index`. An entry no segment
/// covers (a pending steer between two runs) renders in its turn's trailer,
/// the turn's last row.
pub(crate) fn row_of_entry(rows: &[TimelineRow], entry_index: usize, turn: usize) -> Option<usize> {
    let candidate = rows.partition_point(|row| row.entry_range.end <= entry_index);
    if rows
        .get(candidate)
        .is_some_and(|row| row.entry_range.contains(&entry_index))
    {
        return Some(candidate);
    }
    rows_of_turn(rows, turn).last()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TurnIndexMeta {
    start_ts: Option<u64>,
    end_ts: Option<u64>,
    running: bool,
    timing: Option<tcode_core::session::TurnTiming>,
    served_model: Option<String>,
    cost_usd: Option<u64>,
    status: Option<TurnStatus>,
}

impl From<&TurnMeta> for TurnIndexMeta {
    fn from(turn: &TurnMeta) -> Self {
        Self {
            start_ts: turn.start_ts,
            end_ts: turn.end_ts,
            running: turn.running,
            timing: turn.timing,
            served_model: turn.served_model.clone(),
            cost_usd: turn.cost_usd.map(f64::to_bits),
            status: turn.status,
        }
    }
}

impl TurnIndexMeta {
    fn matches(&self, turn: &TurnMeta) -> bool {
        self.start_ts == turn.start_ts
            && self.end_ts == turn.end_ts
            && self.running == turn.running
            && self.timing == turn.timing
            && self.served_model.as_deref() == turn.served_model.as_deref()
            && self.cost_usd == turn.cost_usd.map(f64::to_bits)
            && self.status == turn.status
    }
}

/// How the timeline being synced relates to the previously indexed one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimelineContinuity {
    /// The same session with its whole history loaded: turns only grow at
    /// the end or change in place.
    Complete,
    /// The same session with earlier history still unloaded: a page can
    /// complete or merge the first turn's entries without replacing its row.
    PartialFirstTurn,
    /// A different session: every row is new.
    NewSession,
}

/// Stateful input snapshot for reusing indexed turns across store notifications.
#[derive(Debug, Default)]
pub(crate) struct TurnIndexCache {
    /// Each entry indexed, by identity, with its turn. Weak: a strong
    /// reference would make the fold's `Arc::make_mut` on a streaming entry
    /// copy the whole message or output on every delta instead of writing it
    /// in place. A weak one keeps the allocation, so its address is never
    /// reused while it is held here, and an entry changed in place moves to
    /// a new one.
    entries: Vec<(Weak<TimelineEntry>, usize)>,
    turns: Vec<TurnIndexMeta>,
    expanded: HashSet<String>,
}

impl TurnIndexCache {
    pub(crate) fn sync(
        &mut self,
        items: &mut Vec<TimelineRow>,
        turns: &[TurnMeta],
        entries: &[Arc<TimelineEntry>],
        expanded: &HashSet<String>,
        continuity: TimelineContinuity,
    ) -> ListSync {
        let reset = continuity == TimelineContinuity::NewSession;
        let turn_count = turns
            .len()
            .max(entries.last().map_or(0, |entry| entry.turn + 1));
        let entry_divergence = self
            .entries
            .iter()
            .zip(entries)
            .position(|((old, _), new)| !std::ptr::eq(old.as_ptr(), Arc::as_ptr(new)))
            .unwrap_or(self.entries.len().min(entries.len()));
        let tail_replace = entries.len() == self.entries.len()
            && entry_divergence.checked_add(1) == Some(entries.len());
        let append = entry_divergence == self.entries.len() && entries.len() >= self.entries.len();
        let settings_changed = self.expanded != *expanded;
        let must_reset = reset
            || settings_changed
            || entries.len() < self.entries.len()
            || (!append && !tail_replace)
            || turns.len() < self.turns.len();

        let turn_divergence = self
            .turns
            .iter()
            .zip(turns)
            .position(|(old, new)| !old.matches(new))
            .unwrap_or(self.turns.len().min(turns.len()));
        let mut reindex_from = if must_reset { 0 } else { turn_count };
        if !must_reset {
            if entry_divergence < entries.len() {
                reindex_from = reindex_from.min(entries[entry_divergence].turn);
            }
            if entry_divergence < self.entries.len() {
                reindex_from = reindex_from.min(self.entries[entry_divergence].1);
            }
            if turn_divergence < turns.len() {
                reindex_from = reindex_from.min(turn_divergence);
            }
        }

        let suffix = if reindex_from == 0 {
            index_rows(turns, entries, expanded)
        } else {
            if reindex_from < turn_count {
                index_rows_from(turns, entries, expanded, reindex_from)
            } else {
                Vec::new()
            }
        };
        // Rows of the turns before `reindex_from` are kept as they are.
        let kept = rows_of_turn(items, reindex_from).start;
        let item_count = kept + suffix.len();
        let sync = list_sync_with(items, item_count, continuity, |index| {
            if index < kept {
                &items[index]
            } else {
                &suffix[index - kept]
            }
        });
        items.truncate(kept);
        items.extend(suffix);

        self.entries.truncate(entry_divergence);
        self.entries.extend(
            entries[entry_divergence..]
                .iter()
                .map(|entry| (Arc::downgrade(entry), entry.turn)),
        );
        self.turns.truncate(turn_divergence);
        self.turns
            .extend(turns[turn_divergence..].iter().map(TurnIndexMeta::from));
        if self.expanded != *expanded {
            self.expanded.clone_from(expanded);
        }
        sync
    }
}

/// Build the segment rows and their fingerprints for every turn.
///
/// Timeline entries are chronological, so all entries for a turn are adjacent.
/// The max entry turn keeps a temporary orphan bucket renderable if a provider
/// ever exposes an entry before its corresponding `TurnMeta`.
pub(crate) fn index_rows(
    turns: &[TurnMeta],
    entries: &[Arc<TimelineEntry>],
    expanded: &HashSet<String>,
) -> Vec<TimelineRow> {
    index_rows_from(turns, entries, expanded, 0)
}

fn index_rows_from(
    turns: &[TurnMeta],
    entries: &[Arc<TimelineEntry>],
    expanded: &HashSet<String>,
    first_turn: usize,
) -> Vec<TimelineRow> {
    debug_assert!(entries.windows(2).all(|pair| pair[0].turn <= pair[1].turn));

    let turn_count = turns
        .len()
        .max(entries.last().map_or(0, |entry| entry.turn + 1));
    let first_entry = entries.partition_point(|entry| entry.turn < first_turn);
    let mut turn_ranges = vec![entries.len()..entries.len(); turn_count.saturating_sub(first_turn)];
    for (index, entry) in entries.iter().enumerate().skip(first_entry) {
        let range = &mut turn_ranges[entry.turn - first_turn];
        if range.start == entries.len() {
            range.start = index;
        }
        range.end = index + 1;
    }

    let mut rows = Vec::new();
    for (offset, turn_range) in turn_ranges.into_iter().enumerate() {
        let index = first_turn + offset;
        let turn = turns.get(index);
        let running = turn.is_some_and(|turn| turn.running);
        let segmented = segment_entries(&entries[turn_range.clone()], running);
        let live_activity = live_activity_segment(&segmented.flow, running);
        let last_assistant = segmented
            .flow
            .iter()
            .rposition(|segment| matches!(segment, Segment::Assistant(_)));
        let mut segment_ranges = segmented
            .ranges
            .iter()
            .map(|range| turn_range.start + range.start..turn_range.start + range.end)
            .collect::<Vec<_>>();
        if segment_ranges.is_empty() {
            segment_ranges.push(turn_range.end..turn_range.end);
        }
        let last_segment = segment_ranges.len() - 1;
        for (segment, segment_range) in segment_ranges.into_iter().enumerate() {
            let live = live_activity == Some(segment);
            let final_assistant = last_assistant == Some(segment);
            let parts = segment_parts(
                entries,
                segmented.flow.get(segment),
                segment_range.clone(),
                expanded,
                live && running,
            );
            let last_part = parts.len() - 1;
            for (part_index, (part, entry_range)) in parts.into_iter().enumerate() {
                let first_in_turn = segment == 0 && part_index == 0;
                let last_in_turn = segment == last_segment && part_index == last_part;
                // A header renders its whole run, and a run is known by its
                // first entry whether or not its Work Log is expanded.
                let shape_range = match &part {
                    RowPart::WorkLogHeader { run } => run.clone(),
                    _ => entry_range.clone(),
                };
                let mut identity = DefaultHasher::new();
                let mut tail_identity = DefaultHasher::new();
                let mut content = DefaultHasher::new();
                if let Some(entry) = entries[shape_range.clone()].last() {
                    entry.id.hash(&mut tail_identity);
                }
                match &part {
                    RowPart::WorkLogActivity => {
                        ("worklog-activity", &entries[entry_range.end - 1].id).hash(&mut identity);
                    }
                    RowPart::WorkLogLive => {
                        ("worklog-live", &entries[segment_range.start].id).hash(&mut identity);
                        // The window slides over activities of the same shape.
                        entries[entry_range.start].id.hash(&mut content);
                    }
                    RowPart::Segment | RowPart::WorkLogHeader { .. } => {
                        if let Some(entry) = entries[shape_range.clone()].first() {
                            entry.id.hash(&mut identity);
                        }
                    }
                }
                std::mem::discriminant(&part).hash(&mut content);
                for entry in &entries[shape_range.clone()] {
                    std::mem::discriminant(&entry.content).hash(&mut content);
                    entry.ts.hash(&mut content);
                    hash_entry_shape(&entry.content, &mut content);
                    // A disclosure row (orchestrate context / callback) grows a tall
                    // scroll card when expanded, so its toggle state must change the
                    // row fingerprint or the list keeps the collapsed measurement.
                    for key in disclosure_keys(&entry.content, &entry.id) {
                        expanded.contains(&key).hash(&mut content);
                    }
                }
                if shape_range.is_empty() {
                    // An empty turn's only row has no entry to be known by.
                    ("empty-turn", index).hash(&mut identity);
                }
                (first_in_turn, last_in_turn, live, final_assistant).hash(&mut content);
                running.hash(&mut content);
                if last_in_turn {
                    // The trailer: pending steers float here while the turn
                    // runs, and the finished turn renders its breakdown.
                    for entry in &entries[turn_range.clone()] {
                        if let EntryContent::Steer { text, status, .. } = &entry.content {
                            entry.id.hash(&mut content);
                            text.len().hash(&mut content);
                            status.hash(&mut content);
                            for key in disclosure_keys(&entry.content, &entry.id) {
                                expanded.contains(&key).hash(&mut content);
                            }
                        }
                    }
                    entries[turn_range.clone()]
                        .last()
                        .and_then(|entry| entry.ts)
                        .hash(&mut content);
                    if let Some(turn) = turn {
                        turn.start_ts.hash(&mut content);
                        turn.end_ts.hash(&mut content);
                        turn.timing.hash(&mut content);
                        turn.served_model.hash(&mut content);
                        turn.cost_usd.map(f64::to_bits).hash(&mut content);
                        turn.status
                            .as_ref()
                            .map(std::mem::discriminant)
                            .hash(&mut content);
                    }
                }
                rows.push(TimelineRow {
                    turn: index,
                    entry_count: entry_range.len(),
                    entry_range,
                    part,
                    first_in_turn,
                    last_in_turn,
                    live_activity: live,
                    last_assistant: final_assistant,
                    identity: identity.finish(),
                    tail_identity: tail_identity.finish(),
                    content: content.finish(),
                });
            }
        }
    }
    rows
}

/// The rows one segment spanning `range` renders as. An expanded Work Log
/// with folded activities becomes its header, one row per folded activity and
/// the live window; every other segment is one row. Each folded activity
/// ends its part's entry range.
fn segment_parts(
    entries: &[Arc<TimelineEntry>],
    segment: Option<&Segment<'_>>,
    range: Range<usize>,
    expanded: &HashSet<String>,
    live_window: bool,
) -> Vec<(RowPart, Range<usize>)> {
    let whole = || vec![(RowPart::Segment, range.clone())];
    let Some(Segment::ActivityRun(activities)) = segment else {
        return whole();
    };
    if !expanded.contains(&work_log_key(&activities[0].id)) {
        return whole();
    }
    let (folded, visible) = partition_activity_run(activities, live_window);
    if folded.is_empty() {
        return whole();
    }
    let mut parts = vec![(
        RowPart::WorkLogHeader { run: range.clone() },
        range.start..range.start,
    )];
    let mut start = range.start;
    // A computer run is one window, so one part.
    for window in crate::chat::tools::run::windows(folded) {
        let activity = &folded[window.end - 1];
        let end = start
            + entries[start..range.end]
                .iter()
                .position(|entry| std::ptr::eq(entry.as_ref(), *activity))
                .expect("a folded activity lies in its run")
            + 1;
        parts.push((RowPart::WorkLogActivity, start..end));
        start = end;
    }
    if !visible.is_empty() {
        parts.push((RowPart::WorkLogLive, start..range.end));
    }
    parts
}

/// Hash only data that can alter a turn's layout. Text lengths make streaming
/// updates O(number of entries) without repeatedly hashing growing markdown.
fn hash_entry_shape(content: &EntryContent, hash: &mut DefaultHasher) {
    if let EntryContent::Item(item) = content {
        std::mem::discriminant(item).hash(hash);
    }
    match content {
        EntryContent::Item(ItemContent::UserMessage { text, attachments }) => {
            attachments.len().hash(hash);
            text.len().hash(hash);
            Option::<SteeringStatus>::None.hash(hash);
        }
        EntryContent::Steer {
            text,
            status,
            attachments,
        } => {
            attachments.len().hash(hash);
            text.len().hash(hash);
            status.hash(hash);
        }
        EntryContent::Item(ItemContent::AssistantMessage { text, .. })
        | EntryContent::Item(ItemContent::Plan { markdown: text })
        | EntryContent::Item(ItemContent::Reasoning { text })
        | EntryContent::Feedback { text } => {
            text.len().hash(hash);
        }
        EntryContent::Item(ItemContent::CommandExecution {
            command,
            output,
            exit_code,
            status,
        }) => {
            command.len().hash(hash);
            output.len().hash(hash);
            exit_code.hash(hash);
            std::mem::discriminant(status).hash(hash);
        }
        EntryContent::Item(ItemContent::FileChange {
            changes,
            status,
            output,
        }) => {
            changes.len().hash(hash);
            for change in changes {
                change.path.len().hash(hash);
                change.diff.as_ref().map(String::len).hash(hash);
            }
            std::mem::discriminant(status).hash(hash);
            output.as_ref().map(String::len).hash(hash);
        }
        EntryContent::Item(ItemContent::ToolCall {
            name,
            input,
            output,
            status,
            image_reads,
            ..
        }) => {
            image_reads.len().hash(hash);
            name.len().hash(hash);
            input.to_string().len().hash(hash);
            output.as_ref().map(String::len).hash(hash);
            std::mem::discriminant(status).hash(hash);
        }
        EntryContent::Item(ItemContent::Subagent {
            agent_type,
            description,
            status,
            summary,
            model,
            effort,
        }) => {
            agent_type.len().hash(hash);
            description.len().hash(hash);
            std::mem::discriminant(status).hash(hash);
            summary.as_ref().map(String::len).hash(hash);
            model.as_ref().map(String::len).hash(hash);
            effort.as_ref().map(String::len).hash(hash);
        }

        EntryContent::Error {
            message,
            limit_resets_at,
        } => {
            message.len().hash(hash);
            limit_resets_at.hash(hash);
        }
        EntryContent::ProviderStartError { error } => error.len().hash(hash),
        EntryContent::ProviderRelay {
            from_provider,
            to_provider,
            prompt,
            ..
        } => {
            from_provider.hash(hash);
            to_provider.hash(hash);
            prompt.as_ref().map(String::len).hash(hash);
        }
        EntryContent::ModelChanged { from, to, reason } => {
            from.hash(hash);
            to.hash(hash);
            reason.hash(hash);
        }
        EntryContent::ContextCompacted(_) => {}
        EntryContent::ContextWindowChanged { window } => window.hash(hash),
        EntryContent::SettingsKept => {}
        EntryContent::SessionMoved { from, to } => {
            from.hash(hash);
            to.hash(hash);
        }
        EntryContent::Worktree { request, answer } => {
            request.hash(hash);
            answer.hash(hash);
        }
        EntryContent::ControlAction(action) => {
            action.actor_thread_id.hash(hash);
            action.action.hash(hash);
            action.detail.hash(hash);
        }
        EntryContent::Rewound(rewound) => {
            rewound.removed.len().hash(hash);
            rewound.undo_token.is_some().hash(hash);
            rewound.branch.is_some().hash(hash);
            rewound.incomplete.is_some().hash(hash);
        }
        EntryContent::RewindFailed { failure, .. } => failure.reason.hash(hash),
        EntryContent::Unreadable { line } => line.len().hash(hash),
        EntryContent::Withheld { bytes } => bytes.hash(hash),
        EntryContent::Item(ItemContent::ImageRead { path, .. }) => path.hash(hash),
        EntryContent::Item(ItemContent::WebSearch { query }) => query.len().hash(hash),
        EntryContent::Item(ItemContent::Other {
            provider_kind,
            summary,
        }) => {
            provider_kind.len().hash(hash);
            summary.len().hash(hash);
        }
    }
}

/// Mutation to apply to the persistent [`ListState`] after a timeline sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ListSync {
    None,
    Reset {
        count: usize,
    },
    /// Rows inserted, removed or replaced in place. `splices` are ranges of
    /// the old rows and the number of new rows standing in for each,
    /// ascending, so they apply back to front; `remeasure` indexes the new
    /// rows.
    /// `carried` are the spliced old ranges whose content continues, in
    /// order, in the rows standing in for them: an expanded Work Log's live
    /// window folding activities out into rows of their own. A reader resting
    /// in one keeps its pixel offset into the new rows.
    Incremental {
        splices: Vec<(Range<usize>, usize)>,
        remeasure: Vec<usize>,
        carried: Vec<Range<usize>>,
    },
}

/// Align the old rows with the new ones by identity, in order.
///
/// Rows keep their measured height wherever their identity survives in
/// order; a row whose content changed is remeasured in place. Rows that
/// appear are spliced in where they stand and rows that vanish are spliced
/// out, which is how the list keeps the reader's anchor across a steer
/// landing mid-turn, a page arriving above or the pages read above the tail
/// being dropped. A row that survives out of order with the rows around it
/// moved: it leaves where it stood and arrives where it stands, as a page
/// that brings the start of an item a later turn reported on moves the
/// item's row back to that start. The list resets only for another session,
/// for rows it cannot tell apart, and when none of its rows survives and not
/// all of them can be replaced.
fn list_sync_with<'a>(
    old: &[TimelineRow],
    new_len: usize,
    continuity: TimelineContinuity,
    new_at: impl Fn(usize) -> &'a TimelineRow,
) -> ListSync {
    if continuity == TimelineContinuity::NewSession {
        return ListSync::Reset { count: new_len };
    }
    let partial_first_turn = continuity == TimelineContinuity::PartialFirstTurn;
    let mut old_pos = HashMap::with_capacity(old.len());
    for (index, row) in old.iter().enumerate() {
        if old_pos.insert(row.identity, index).is_some() {
            return ListSync::Reset { count: new_len };
        }
    }
    let mut new_pos = HashMap::with_capacity(new_len);
    for index in 0..new_len {
        if new_pos.insert(new_at(index).identity, index).is_some() {
            return ListSync::Reset { count: new_len };
        }
    }
    // An empty turn's row goes when the turn gains entries; the partial
    // first turn's rows may merge as their records arrive; an expanded Work
    // Log's rows go when it collapses or its live window settles.
    let replaceable = |row: &TimelineRow| {
        row.entry_count == 0 || partial_first_turn && row.turn == 0 || row.part != RowPart::Segment
    };
    let survivors: Vec<(u64, usize)> = old
        .iter()
        .filter_map(|row| Some((row.identity, *new_pos.get(&row.identity)?)))
        .collect();
    // Splicing out every row would leave no row to hold a reader's place;
    // a reset at least lays the bottom-aligned list out at its end.
    if survivors.is_empty() && !old.iter().all(replaceable) {
        return ListSync::Reset { count: new_len };
    }
    let moved = moved_rows(&survivors);
    let survives =
        |row: &TimelineRow| new_pos.contains_key(&row.identity) && !moved.contains(&row.identity);
    let existed =
        |row: &TimelineRow| old_pos.contains_key(&row.identity) && !moved.contains(&row.identity);
    // The partial first turn's row keeps its place when a page completes
    // it: its newest entry is still its newest entry.
    let completes = |old_row: &TimelineRow, new_row: &TimelineRow| {
        partial_first_turn
            && old_row.turn == 0
            && old_row.entry_count > 0
            && old_row.tail_identity == new_row.tail_identity
            && new_row.entry_count >= old_row.entry_count
    };

    let mut splices: Vec<(Range<usize>, usize)> = Vec::new();
    let mut remeasure = Vec::new();
    let mut carried = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < old.len() || j < new_len {
        if i < old.len() && j < new_len {
            let (old_row, new_row) = (&old[i], new_at(j));
            if old_row.identity == new_row.identity {
                if old_row.content != new_row.content || old_row.entry_count != new_row.entry_count
                {
                    remeasure.push(j);
                }
                i += 1;
                j += 1;
                continue;
            }
        }
        let old_survives = i < old.len() && survives(&old[i]);
        let new_existed = j < new_len && existed(new_at(j));
        if i < old.len() && !old_survives {
            // The completed first-turn row may sit below rows the page
            // added above it.
            if let Some(k) = (j..new_len).find(|&k| completes(&old[i], new_at(k))) {
                if k > j {
                    splices.push((i..i, k - j));
                }
                remeasure.push(k);
                i += 1;
                j = k + 1;
                continue;
            }
            if !replaceable(&old[i]) {
                let start = i;
                while i < old.len() && !survives(&old[i]) {
                    i += 1;
                }
                match splices.last_mut() {
                    Some((dropped, 0)) if dropped.end == start => dropped.end = i,
                    _ => splices.push((start..i, 0)),
                }
                continue;
            }
            let start = i;
            while i < old.len() && !survives(&old[i]) && replaceable(&old[i]) {
                i += 1;
            }
            let inserted = j;
            while j < new_len && !existed(new_at(j)) {
                j += 1;
            }
            splices.push((start..i, j - inserted));
            if old[start].part == RowPart::WorkLogLive {
                carried.push(start..i);
            } else {
                remeasure.extend(inserted..j);
            }
        } else if j < new_len && !new_existed {
            let inserted = j;
            while j < new_len && !existed(new_at(j)) {
                j += 1;
            }
            // Activities folded out of a live window arrive right above it.
            if i < old.len()
                && old[i].part == RowPart::WorkLogLive
                && j < new_len
                && new_at(j).identity == old[i].identity
            {
                splices.push((i..i + 1, j - inserted + 1));
                carried.push(i..i + 1);
                i += 1;
                j += 1;
            } else {
                splices.push((i..i, j - inserted));
            }
        } else {
            // Rows that survive in order pair up one to one; the splices
            // cannot state anything else.
            return ListSync::Reset { count: new_len };
        }
    }

    match splices.as_slice() {
        [] if remeasure.is_empty() => ListSync::None,
        _ => ListSync::Incremental {
            splices,
            remeasure,
            carried,
        },
    }
}

/// The identities of the `survivors` (each old row's identity and new index,
/// in old order) that moved: the longest run of them whose new indexes
/// ascend keeps its place, and every other one left it.
fn moved_rows(survivors: &[(u64, usize)]) -> HashSet<u64> {
    if survivors.is_sorted_by_key(|&(_, at)| at) {
        return HashSet::new();
    }
    // `tails[n]` ends the ascending run of length `n + 1` found so far whose
    // last new index is least; `previous` links each survivor to the one
    // before it in its run.
    let mut tails: Vec<usize> = Vec::new();
    let mut previous = vec![None; survivors.len()];
    for (index, &(_, at)) in survivors.iter().enumerate() {
        let length = tails.partition_point(|&tail| survivors[tail].1 < at);
        previous[index] = length.checked_sub(1).map(|before| tails[before]);
        if length == tails.len() {
            tails.push(index);
        } else {
            tails[length] = index;
        }
    }
    let mut kept = vec![false; survivors.len()];
    let mut next = tails.last().copied();
    while let Some(index) = next {
        kept[index] = true;
        next = previous[index];
    }
    survivors
        .iter()
        .zip(kept)
        .filter(|(_, kept)| !kept)
        .map(|(&(identity, _), _)| identity)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::model::tests::{command, file_change};

    /// An expanded Work Log of a long turn gives each folded activity its own
    /// row while the live window keeps the newest five.
    #[test]
    fn an_expanded_work_log_gives_each_folded_activity_a_row() {
        let activities = [
            command("cargo check"),
            file_change("edit", &["src/foo.rs"]),
            command("cargo test"),
            command("cargo clippy"),
            command("cargo fmt"),
            command("cargo nextest"),
        ];
        for (count, running, folded, live) in [
            (
                5,
                true,
                vec![],
                vec![
                    "cargo check",
                    "edit",
                    "cargo test",
                    "cargo clippy",
                    "cargo fmt",
                ],
            ),
            (
                6,
                true,
                vec!["cargo check"],
                vec![
                    "edit",
                    "cargo test",
                    "cargo clippy",
                    "cargo fmt",
                    "cargo nextest",
                ],
            ),
            (
                6,
                false,
                vec![
                    "cargo check",
                    "edit",
                    "cargo test",
                    "cargo clippy",
                    "cargo fmt",
                    "cargo nextest",
                ],
                vec![],
            ),
        ] {
            let entries = &activities[..count];
            let mut rows = Vec::new();
            TurnIndexCache::default().sync(
                &mut rows,
                &[TurnMeta {
                    running,
                    ..Default::default()
                }],
                entries,
                &HashSet::from([work_log_key("cargo check")]),
                TimelineContinuity::Complete,
            );
            let ids = |part: fn(&RowPart) -> bool| {
                rows.iter()
                    .filter(|row| part(&row.part))
                    .flat_map(|row| &entries[row.entry_range.clone()])
                    .map(|entry| entry.id.as_str())
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                ids(|part| matches!(part, RowPart::WorkLogActivity)),
                folded,
                "count {count}, running {running}"
            );
            assert_eq!(
                ids(|part| matches!(part, RowPart::Segment | RowPart::WorkLogLive)),
                live,
                "count {count}, running {running}"
            );
        }
    }
}
