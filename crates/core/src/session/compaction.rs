use std::collections::HashSet;

use agent::AgentEvent;

use super::{StoredEvent, Timeline};

/// One record of a compacted event log, in log order.
#[derive(Debug, Clone, PartialEq)]
pub enum CompactedRecord {
    /// The original record at this index, unchanged.
    Kept(usize),
    /// Adjacent deltas of one item merged into the first of them: its time,
    /// all of their text.
    Merged(Box<StoredEvent>),
}

/// What folding one record did that compaction has to account for.
#[derive(Debug, Clone, Copy, Default)]
struct Note {
    /// It moved the open turn's clock watermark; a record compaction may
    /// leave out only ever raises it.
    raises: bool,
    /// It lifts any lower watermark to the one the whole log had reached
    /// before it, before anything reads the clock: the fold watermarks it, and
    /// its time is no earlier than that watermark.
    repairs: bool,
    /// A turn-changes snapshot that a later snapshot replaces on the same
    /// turn lifetime, and whose fold does nothing else: it lands on an
    /// existing turn without renaming it.
    superseded: bool,
}

/// A record of the compacted log: an original record plus the deltas merged
/// into it.
struct Out {
    index: usize,
    merged: Vec<usize>,
    text_len: usize,
}

impl Out {
    fn new(records: &[StoredEvent], index: usize) -> Self {
        Self {
            index,
            merged: Vec::new(),
            text_len: delta_text(&records[index]).len(),
        }
    }
}

/// Compact a whole event log so that it folds exactly as `records` do,
/// including the state later records continue from:
///
/// * a turn-changes snapshot goes when a later snapshot lands on the same turn
///   lifetime (a rewind ends a lifetime even when the turn's index is reused),
///   since each snapshot replaces the turn's changes wholesale, unless it is
///   the snapshot that opened or renamed that turn;
/// * deltas of one item and kind merge into the first of them, up to
///   `max_merged_text` bytes of text per record, when nothing but dropped
///   snapshots stands between them, so compacting a compacted log changes
///   nothing.
///
/// Every timestamped record of an open turn raises its clock watermark, so a
/// record compaction removes may leave the compacted fold's watermark behind.
/// That is accepted only when the next record kept lifts both folds to the same
/// watermark before the clock is read; otherwise the removed records stay.
///
/// A result as long as `records` means nothing compacts.
pub fn compact_records(records: &[StoredEvent], max_merged_text: usize) -> Vec<CompactedRecord> {
    let notes = notes(records);
    let mut out: Vec<Out> = Vec::with_capacity(records.len());
    // Records left out since the last record kept, and whether both folds'
    // watermarks still agree.
    let mut pending: Vec<usize> = Vec::new();
    let mut in_sync = true;
    for (index, record) in records.iter().enumerate() {
        let note = notes[index];
        if merges(records, &out, index, max_merged_text) {
            let run = out.last_mut().expect("a merge continues a kept record");
            run.merged.push(index);
            run.text_len += delta_text(record).len();
        } else if !note.superseded {
            if !in_sync && !note.repairs {
                restore(records, &mut out, &pending, &notes);
            }
            pending.clear();
            in_sync = true;
            out.push(Out::new(records, index));
            continue;
        }
        pending.push(index);
        in_sync &= !note.raises;
    }
    if !in_sync {
        restore(records, &mut out, &pending, &notes);
    }
    out.into_iter()
        .map(|out| {
            if out.merged.is_empty() {
                return CompactedRecord::Kept(out.index);
            }
            let mut head = records[out.index].clone();
            if let AgentEvent::Delta { text, .. } = &mut head.event {
                for &index in &out.merged {
                    text.push_str(delta_text(&records[index]));
                }
            }
            CompactedRecord::Merged(Box::new(head))
        })
        .collect()
}

/// Fold `records` once, noting for each what compaction needs from the fold's
/// own decisions: which turn a snapshot lands on and how the clock moves.
fn notes(records: &[StoredEvent]) -> Vec<Note> {
    let mut fold = Timeline::default();
    let mut notes = vec![Note::default(); records.len()];
    // A serial per turn the fold holds, so a turn index reused after a rewind
    // is a different lifetime.
    let mut lifetimes: Vec<u64> = Vec::new();
    let mut next_lifetime = 0;
    // (record, lifetime, removable) for every turn-changes snapshot.
    let mut snapshots: Vec<(usize, u64, bool)> = Vec::new();
    for (index, record) in records.iter().enumerate() {
        let watermark = fold.tool_clock.clock;
        let target = match &record.event {
            AgentEvent::TurnChangesUpdated { turn_id, .. } => {
                let turn = fold.provider_turn(turn_id);
                let removable = turn.is_some_and(|turn| {
                    turn_id.is_empty()
                        || fold.turns[turn].provider_turn_id.as_deref() == Some(turn_id)
                });
                Some((turn, removable))
            }
            _ => None,
        };
        notes[index].repairs = fold.watermarks(&record.event)
            && record
                .ts
                .is_some_and(|ts| watermark.is_none_or(|watermark| ts >= watermark));
        fold.apply_stored(record);
        notes[index].raises = fold.tool_clock.clock != watermark;
        lifetimes.truncate(fold.turns.len());
        while lifetimes.len() < fold.turns.len() {
            lifetimes.push(next_lifetime);
            next_lifetime += 1;
        }
        if let Some((turn, removable)) = target {
            let turn = turn.unwrap_or(fold.turns.len() - 1);
            snapshots.push((index, lifetimes[turn], removable));
        }
    }
    let mut replaced = HashSet::new();
    for &(index, lifetime, removable) in snapshots.iter().rev() {
        notes[index].superseded = !replaced.insert(lifetime) && removable;
    }
    notes
}

/// Whether record `index` is a delta continuing the run the last kept record
/// opened: that record is a delta of the same item and kind, and the merged
/// text stays within `max_merged_text`. Every record since the last kept one
/// was merged into it or dropped, so nothing the fold reads stands between.
fn merges(records: &[StoredEvent], out: &[Out], index: usize, max_merged_text: usize) -> bool {
    let Some(run) = out.last() else {
        return false;
    };
    let (
        AgentEvent::Delta {
            item_id,
            kind,
            text,
            ..
        },
        AgentEvent::Delta {
            item_id: run_item,
            kind: run_kind,
            ..
        },
    ) = (&records[index].event, &records[run.index].event)
    else {
        return false;
    };
    item_id == run_item && kind == run_kind && run.text_len + text.len() <= max_merged_text
}

/// Keep records left out since the last kept one, because the record after
/// them cannot lift the compacted fold's watermark: the last of them, when it
/// can lift the watermark itself, or else all of them. The deltas among them
/// are the whole tail of the last kept record's run.
fn restore(records: &[StoredEvent], out: &mut Vec<Out>, pending: &[usize], notes: &[Note]) {
    let Some(&last) = pending.last() else {
        return;
    };
    let run = out
        .last_mut()
        .expect("records are left out after a kept one");
    let restored = if notes[last].repairs {
        if run.merged.pop_if(|merged| *merged == last).is_some() {
            run.text_len -= delta_text(&records[last]).len();
        }
        &pending[pending.len() - 1..]
    } else {
        run.merged.clear();
        run.text_len = delta_text(&records[run.index]).len();
        pending
    };
    out.extend(restored.iter().map(|&index| Out::new(records, index)));
}

fn delta_text(record: &StoredEvent) -> &str {
    match &record.event {
        AgentEvent::Delta { text, .. } => text,
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use agent::{
        ChangeCompleteness, DeltaKind, FileChange, FileChangeKind, ItemContent, RewindMode,
        ThreadItem, TurnStatus,
    };

    use super::*;
    use crate::session::TurnTiming;

    fn at(ts: u64, event: AgentEvent) -> StoredEvent {
        StoredEvent {
            ts: Some(ts),
            event,
            elided: None,
        }
    }

    fn started(turn: &str) -> AgentEvent {
        AgentEvent::TurnStarted {
            turn_id: turn.into(),
        }
    }

    fn completed(turn: &str) -> AgentEvent {
        AgentEvent::TurnCompleted {
            turn_id: turn.into(),
            status: TurnStatus::Completed,
            usage: None,
        }
    }

    fn checkpoint(turn: &str, checkpoint: &str) -> AgentEvent {
        AgentEvent::TurnCheckpoint {
            turn_id: turn.into(),
            checkpoint_id: checkpoint.into(),
        }
    }

    fn rewind(checkpoint: &str) -> AgentEvent {
        AgentEvent::RewindCompleted {
            checkpoint_id: checkpoint.into(),
            mode: RewindMode::Conversation,
            prefill: None,
        }
    }

    /// A snapshot of the turn's net changes: one file whose diff is `diff`.
    fn changes(turn: &str, diff: &str) -> AgentEvent {
        AgentEvent::TurnChangesUpdated {
            turn_id: turn.into(),
            changes: vec![FileChange {
                path: "f".into(),
                kind: FileChangeKind::Modify,
                diff: Some(diff.into()),
            }],
            completeness: ChangeCompleteness::Exact,
        }
    }

    /// Streamed text of the assistant message `m`.
    fn delta(text: &str) -> AgentEvent {
        AgentEvent::Delta {
            item_id: "m".into(),
            kind: DeltaKind::AssistantText,
            text: text.into(),
        }
    }

    fn message(id: &str, text: &str) -> AgentEvent {
        AgentEvent::ItemCompleted(ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content: ItemContent::AssistantMessage { text: text.into() },
        })
    }

    fn user(id: &str) -> AgentEvent {
        AgentEvent::ItemCompleted(ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content: ItemContent::UserMessage {
                text: id.into(),
                context_len: None,
                attachments: Vec::new(),
            },
        })
    }

    fn keep(log: &[StoredEvent], indices: &[usize]) -> Vec<StoredEvent> {
        indices.iter().map(|index| log[*index].clone()).collect()
    }

    #[test]
    fn compaction_drops_and_merges_only_what_the_fold_lets_go() {
        let reused_id = vec![
            at(1, started("x")),
            at(2, changes("x", "a")),
            at(3, message("m1", "one")),
            at(4, completed("x")),
            at(5, started("x")),
            // Lands on the first turn named x, replacing record 1 there.
            at(6, changes("x", "b")),
            // Names no turn, so lands on the current one; replaced by record 8.
            at(7, changes("", "c")),
            at(8, message("m2", "two")),
            at(9, changes("", "d")),
            at(10, message("m3", "three")),
            at(11, completed("x")),
        ];
        let opens_turn = vec![
            at(1, changes("x", "a")),
            at(2, message("m1", "one")),
            at(3, changes("x", "b")),
            at(4, message("m2", "two")),
        ];
        let names_turn = vec![
            at(1, started("t")),
            // No turn is named x yet: lands on the current turn and names it.
            at(2, changes("x", "a")),
            at(3, message("m1", "one")),
            at(4, completed("t")),
            at(5, started("u")),
            // Finds the first turn by the name record 1 gave it.
            at(6, changes("x", "b")),
            at(7, message("m2", "two")),
            at(8, completed("u")),
        ];
        let rewound = vec![
            at(1, user("u1")),
            at(2, started("t1")),
            at(3, checkpoint("t1", "c1")),
            at(4, completed("t1")),
            at(5, user("u2")),
            at(6, started("t2")),
            at(7, checkpoint("t2", "c2")),
            at(8, changes("t2", "a")),
            at(9, message("m1", "one")),
            at(10, completed("t2")),
            // Drops the second turn; the next user message reopens index 1.
            at(11, rewind("c2")),
            at(12, user("u3")),
            at(13, started("t3")),
            at(14, changes("t3", "b")),
            at(15, message("m2", "two")),
            at(16, changes("t3", "c")),
            at(17, message("m3", "three")),
            at(18, completed("t3")),
        ];
        let empty_completion = vec![
            at(1, started("t")),
            at(2, delta("Hel")),
            at(3, delta("lo")),
            at(4, message("m", "")),
            at(5, completed("t")),
        ];
        let separated = vec![
            at(1, started("t")),
            at(2, delta("a")),
            at(3, changes("t", "x")),
            at(4, delta("b")),
            at(5, changes("t", "y")),
            at(6, message("m", "ab")),
            at(7, completed("t")),
        ];
        let late_changes = vec![
            at(1, started("t")),
            at(2, changes("t", "a")),
            at(3, message("m", "done")),
            at(4, completed("t")),
            at(5, changes("t", "b")),
        ];
        let open_tail = vec![at(10, started("t")), at(20, delta("a")), at(40, delta("b"))];
        let longer_open_tail = vec![
            at(10, started("t")),
            at(20, delta("a")),
            at(30, delta("b")),
            at(40, delta("c")),
        ];
        let over_bound = vec![
            at(1, started("t")),
            at(2, delta("aaaa")),
            at(3, delta("bbbb")),
            at(4, delta("cccc")),
            at(5, delta("dd")),
            at(6, message("m", "aaaabbbbccccdd")),
        ];
        let cases = [
            (
                "a reused turn id lands on its first turn",
                keep(&reused_id, &[0, 2, 3, 4, 5, 7, 8, 9, 10]),
                reused_id,
            ),
            (
                "a snapshot that opens its turn stays",
                opens_turn.clone(),
                opens_turn,
            ),
            (
                "a snapshot that names its turn stays",
                names_turn.clone(),
                names_turn,
            ),
            (
                "a rewind ends a turn's lifetime though its index is reused",
                keep(
                    &rewound,
                    &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 14, 15, 16, 17],
                ),
                rewound,
            ),
            (
                "deltas a completion without text relies on are merged, not dropped",
                vec![
                    empty_completion[0].clone(),
                    at(2, delta("Hello")),
                    empty_completion[3].clone(),
                    empty_completion[4].clone(),
                ],
                empty_completion,
            ),
            (
                "deltas a dropped snapshot separated merge",
                vec![
                    separated[0].clone(),
                    at(2, delta("ab")),
                    separated[4].clone(),
                    separated[5].clone(),
                    separated[6].clone(),
                ],
                separated,
            ),
            (
                "a snapshot after its turn completed replaces the earlier one",
                keep(&late_changes, &[0, 2, 3, 4]),
                late_changes,
            ),
            (
                "the last delta of an open turn alone holds its clock",
                open_tail.clone(),
                open_tail,
            ),
            (
                "an open turn's earlier deltas still merge",
                vec![
                    longer_open_tail[0].clone(),
                    at(20, delta("ab")),
                    longer_open_tail[3].clone(),
                ],
                longer_open_tail,
            ),
            (
                "a run longer than the bound splits",
                vec![
                    over_bound[0].clone(),
                    at(2, delta("aaaabbbb")),
                    at(4, delta("ccccdd")),
                    over_bound[5].clone(),
                ],
                over_bound,
            ),
        ];
        for (label, expected, log) in cases {
            let compacted: Vec<StoredEvent> = compact_records(&log, 8)
                .into_iter()
                .map(|record| match record {
                    CompactedRecord::Kept(index) => log[index].clone(),
                    CompactedRecord::Merged(record) => *record,
                })
                .collect();
            assert_eq!(compacted, expected, "{label}");
            assert_eq!(
                Timeline::fold_stored(&compacted),
                Timeline::fold_stored(&log),
                "{label}"
            );
            assert_eq!(
                compact_records(&compacted, 8).len(),
                compacted.len(),
                "{label}: a compacted log compacts no further"
            );
        }
    }

    /// Folds are equal only when every later record folds the same onto
    /// both, so the open turn's clock counts though nothing renders it.
    #[test]
    fn equal_folds_include_the_open_turns_clock() {
        let log = [at(10, started("t")), at(20, delta("a")), at(40, delta("b"))];
        let merged = [at(10, started("t")), at(20, delta("ab"))];
        let (log, merged) = (Timeline::fold_stored(&log), Timeline::fold_stored(&merged));
        assert_eq!(log.entries, merged.entries);
        assert_eq!(log.turns, merged.turns);
        assert_ne!(log, merged);

        let finish = |mut fold: Timeline| {
            fold.apply_at(Some(30), &completed("t"));
            fold.turns[0].timing
        };
        // A completion stamped before work the turn already recorded has no
        // trustworthy breakdown.
        assert_eq!(finish(log), None);
        assert_eq!(finish(merged), Some(TurnTiming::new(20, 0)));
    }
}
