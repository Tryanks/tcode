use std::collections::HashMap;

use agent::{AgentEvent, ItemContent};

use super::{EntryContent, Timeline};

/// The deltas an item's completion makes redundant: removing `delete` and
/// emptying the text of `empty` leaves the fold of the whole log unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupersededStream<K> {
    pub delete: Vec<K>,
    /// The delta that created the item's entry. It stays, so the entry keeps
    /// the row, time and turn it was first recorded with (an anchor names that
    /// row), with its text emptied.
    pub empty: Option<K>,
}

/// Follows a fold to name the streamed deltas each item completion supersedes.
///
/// Only an item whose first record falls in the current turn is followed, and
/// only up to its first completion: providers reuse placeholder ids across
/// turns and restarts, and a later completion of the same id merges into an
/// entry this tracker never saw whole. For a followed item two folds of its
/// own records are kept, with and without its deltas; the deltas are
/// superseded when the completion's text covers everything they streamed and
/// both folds come out equal. The text rule keeps a delta stream the
/// completion only abbreviates (a truncated command output, a lossy chunk
/// decode), which is the only copy of what was streamed. Since the item's
/// records are the only ones that touch its entry, equal folds mean equal
/// entries in the whole log, provided no other record observed the clock a
/// removed delta raised: each removed delta must be followed by a record
/// stamped no earlier than it that is not a turn completion.
#[derive(Debug, Clone)]
pub struct ItemStreams<K> {
    /// Turn count and current turn after the last record; any change ends
    /// every followed stream.
    turn: (usize, Option<usize>),
    entries: usize,
    items: HashMap<String, Stream<K>>,
    last_delta: Option<(String, u64)>,
}

impl<K> Default for ItemStreams<K> {
    fn default() -> Self {
        Self {
            turn: (0, None),
            entries: 0,
            items: HashMap::new(),
            last_delta: None,
        }
    }
}

#[derive(Debug, Clone)]
enum Stream<K> {
    Following(Box<Following<K>>),
    Settled,
}

#[derive(Debug, Clone)]
struct Following<K> {
    with: Timeline,
    without: Timeline,
    deltas: Vec<K>,
    creator: Option<K>,
    clock_moved: bool,
}

impl<K> ItemStreams<K> {
    /// Follow the record `fold` has just applied, which the caller knows as
    /// `key`. When it completes a followed item, the deltas it supersedes.
    pub fn follow(
        &mut self,
        fold: &mut Timeline,
        ts: Option<u64>,
        event: &AgentEvent,
        key: K,
    ) -> Option<SupersededStream<K>> {
        let created = fold.entries.len() == self.entries + 1;
        self.entries = fold.entries.len();
        let turn = (fold.turns.len(), fold.boundaries.current());
        if turn != self.turn {
            self.turn = turn;
            self.items.clear();
        }
        if let Some((item, delta_ts)) = self.last_delta.take()
            && (ts.is_none_or(|ts| ts < delta_ts)
                || matches!(event, AgentEvent::TurnCompleted { .. }))
            && let Some(Stream::Following(stream)) = self.items.get_mut(&item)
        {
            stream.clock_moved = true;
        }
        let item = match event {
            AgentEvent::ItemStarted(item)
            | AgentEvent::ItemUpdated(item)
            | AgentEvent::ItemCompleted(item)
                if item.parent_item_id.is_none() =>
            {
                &item.id
            }
            AgentEvent::Delta { item_id, .. } | AgentEvent::ItemBackgrounded { item_id, .. } => {
                item_id
            }
            _ => return None,
        };
        if let AgentEvent::Delta { .. } = event
            && let Some(ts) = ts
        {
            self.last_delta = Some((item.clone(), ts));
        }
        let stream = match self.items.get_mut(item) {
            Some(Stream::Settled) => return None,
            Some(Stream::Following(stream)) => stream,
            None => {
                // Only an id's first entry is followed: where a reused id's
                // records land depends on the earlier entry with that id, which
                // the folds of the item's own records never see.
                let creates = created
                    && fold.first_entry(item) == Some(fold.entries.len() - 1)
                    && !matches!(event, AgentEvent::ItemCompleted(_));
                if !creates {
                    if fold.first_entry(item).is_some() {
                        self.items.insert(item.clone(), Stream::Settled);
                    }
                    return None;
                }
                let mut stream = Following {
                    with: Timeline::default(),
                    without: Timeline::default(),
                    deltas: Vec::new(),
                    creator: None,
                    clock_moved: false,
                };
                stream.with.apply_at(ts, event);
                match event {
                    AgentEvent::Delta { item_id, kind, .. } => {
                        stream.without.apply_at(
                            ts,
                            &AgentEvent::Delta {
                                item_id: item_id.clone(),
                                kind: *kind,
                                text: String::new(),
                            },
                        );
                        stream.creator = Some(key);
                    }
                    _ => stream.without.apply_at(ts, event),
                }
                self.items
                    .insert(item.clone(), Stream::Following(Box::new(stream)));
                return None;
            }
        };
        match event {
            AgentEvent::Delta { .. } => {
                stream.with.apply_at(ts, event);
                stream.deltas.push(key);
                None
            }
            AgentEvent::ItemCompleted(completed) => {
                let streamed = stream
                    .with
                    .entries
                    .first()
                    .and_then(|entry| streamed_text(&entry.content))
                    .map(str::to_owned);
                stream.with.apply_at(ts, event);
                stream.without.apply_at(ts, event);
                let covered = matches!(
                    (completed_text(&completed.content), streamed),
                    (Some(text), Some(streamed)) if !text.is_empty() && text.starts_with(&streamed)
                );
                let superseded = covered
                    && !stream.clock_moved
                    && stream.with == stream.without
                    && (!stream.deltas.is_empty() || stream.creator.is_some());
                let Some(Stream::Following(stream)) =
                    self.items.insert(item.clone(), Stream::Settled)
                else {
                    return None;
                };
                superseded.then(|| SupersededStream {
                    delete: stream.deltas,
                    empty: stream.creator,
                })
            }
            _ => {
                stream.with.apply_at(ts, event);
                stream.without.apply_at(ts, event);
                None
            }
        }
    }
}

/// The text an item's deltas stream into.
fn streamed_text(content: &EntryContent) -> Option<&str> {
    match content {
        EntryContent::Item(item) => completed_text(item),
        _ => None,
    }
}

fn completed_text(content: &ItemContent) -> Option<&str> {
    match content {
        ItemContent::AssistantMessage { text, .. } | ItemContent::Reasoning { text } => Some(text),
        ItemContent::Plan { markdown } => Some(markdown),
        ItemContent::CommandExecution { output, .. } => Some(output),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use agent::{DeltaKind, ItemStatus, ThreadItem, TurnStatus};

    use super::*;
    use crate::session::StoredEvent;

    fn at(ts: u64, event: AgentEvent) -> StoredEvent {
        StoredEvent {
            origin: None,
            author: None,
            ts: Some(ts),
            event,
            elided: None,
        }
    }

    fn turn(id: &str) -> AgentEvent {
        AgentEvent::TurnStarted { turn_id: id.into() }
    }

    fn turn_done(id: &str) -> AgentEvent {
        AgentEvent::TurnCompleted {
            turn_id: id.into(),
            status: TurnStatus::Completed,
            usage: None,
        }
    }

    fn delta(id: &str, kind: DeltaKind, text: &str) -> AgentEvent {
        AgentEvent::Delta {
            item_id: id.into(),
            kind,
            text: text.into(),
        }
    }

    fn item(id: &str, content: ItemContent) -> ThreadItem {
        ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content,
        }
    }

    fn message(text: &str) -> ItemContent {
        ItemContent::AssistantMessage {
            text: text.into(),
            summarized: false,
        }
    }

    fn command(output: &str, status: ItemStatus) -> ItemContent {
        ItemContent::CommandExecution {
            command: "cargo test".into(),
            output: output.into(),
            exit_code: (status == ItemStatus::Completed).then_some(0),
            status,
        }
    }

    /// The log with every superseded delta removed and every creating delta
    /// emptied, as a store holding it would replay it.
    fn superseded(records: &[StoredEvent]) -> (Vec<StoredEvent>, usize) {
        let mut fold = Timeline::default();
        let mut streams = ItemStreams::default();
        let (mut delete, mut empty) = (Vec::new(), Vec::new());
        for (index, record) in records.iter().enumerate() {
            fold.apply_stored(record);
            if let Some(found) = streams.follow(&mut fold, record.ts, &record.event, index) {
                delete.extend(found.delete);
                empty.extend(found.empty);
            }
        }
        let removed = delete.len();
        let kept = records
            .iter()
            .enumerate()
            .filter(|(index, _)| !delete.contains(index))
            .map(|(index, record)| {
                let mut record = record.clone();
                if empty.contains(&index)
                    && let AgentEvent::Delta { text, .. } = &mut record.event
                {
                    text.clear();
                }
                record
            })
            .collect();
        (kept, removed)
    }

    /// Streams a completion repeats in full lose their deltas; every other
    /// stream keeps them. Either way the fold of what is left is the fold of
    /// the whole log.
    #[test]
    fn only_deltas_a_completion_covers_are_superseded_and_the_fold_never_changes() {
        let completed = |id: &str, content| AgentEvent::ItemCompleted(item(id, content));
        let started = |id: &str, content| AgentEvent::ItemStarted(item(id, content));
        let cases: Vec<(&str, Vec<StoredEvent>, usize)> = vec![
            (
                "Claude text streamed before its item, completed verbatim",
                vec![
                    at(1, turn("t1")),
                    at(2, delta("a", DeltaKind::AssistantText, "Hel")),
                    at(3, delta("a", DeltaKind::AssistantText, "lo")),
                    at(4, delta("a", DeltaKind::AssistantText, " world")),
                    at(5, completed("a", message("Hello world"))),
                    at(6, turn_done("t1")),
                ],
                2,
            ),
            (
                "command started, output streamed, completion extends it",
                vec![
                    at(1, turn("t1")),
                    at(2, started("c", command("", ItemStatus::InProgress))),
                    at(3, delta("c", DeltaKind::CommandOutput, "line 1\n")),
                    at(4, delta("c", DeltaKind::CommandOutput, "line 2\n")),
                    at(
                        5,
                        completed("c", command("line 1\nline 2\nok\n", ItemStatus::Completed)),
                    ),
                ],
                2,
            ),
            (
                "Codex truncated output: the completion abbreviates the stream",
                vec![
                    at(1, turn("t1")),
                    at(2, started("c", command("", ItemStatus::InProgress))),
                    at(
                        3,
                        delta("c", DeltaKind::CommandOutput, "a".repeat(64).as_str()),
                    ),
                    at(
                        4,
                        delta("c", DeltaKind::CommandOutput, "b".repeat(64).as_str()),
                    ),
                    at(
                        5,
                        completed(
                            "c",
                            command("aaaa… 120 bytes omitted …bbbb", ItemStatus::Completed),
                        ),
                    ),
                ],
                0,
            ),
            (
                "lossy chunk decode: the completion has the right character",
                vec![
                    at(1, turn("t1")),
                    at(2, delta("c", DeltaKind::CommandOutput, "caf\u{FFFD}")),
                    at(3, delta("c", DeltaKind::CommandOutput, "\u{FFFD}\n")),
                    at(4, completed("c", command("café\n", ItemStatus::Completed))),
                ],
                0,
            ),
            (
                "Codex reasoning: summary and content joined with a newline",
                vec![
                    at(1, turn("t1")),
                    at(2, delta("r", DeltaKind::ReasoningText, "Plan")),
                    at(3, delta("r", DeltaKind::ReasoningText, "Do it")),
                    at(
                        4,
                        completed(
                            "r",
                            ItemContent::Reasoning {
                                text: "Plan\nDo it".into(),
                            },
                        ),
                    ),
                ],
                0,
            ),
            (
                "an empty completion keeps the streamed text",
                vec![
                    at(1, turn("t1")),
                    at(2, delta("a", DeltaKind::AssistantText, "kept")),
                    at(3, delta("a", DeltaKind::AssistantText, " too")),
                    at(4, completed("a", message(""))),
                ],
                0,
            ),
            (
                "redacted thinking never completes",
                vec![
                    at(1, turn("t1")),
                    at(2, delta("r", DeltaKind::ReasoningText, "secret")),
                    at(3, delta("r", DeltaKind::ReasoningText, " more")),
                    at(4, turn_done("t1")),
                ],
                0,
            ),
            (
                "an id reused from an earlier turn merges into that turn's entry",
                vec![
                    at(1, turn("t1")),
                    at(2, completed("msg-1", message("first turn"))),
                    at(3, turn_done("t1")),
                    at(4, turn("t2")),
                    at(5, delta("msg-1", DeltaKind::AssistantText, "second")),
                    at(6, delta("msg-1", DeltaKind::AssistantText, " turn")),
                    at(7, completed("msg-1", message("second turn"))),
                ],
                0,
            ),
            (
                "a delta stamped after the record that follows it",
                vec![
                    at(1, turn("t1")),
                    at(2, started("c", command("", ItemStatus::InProgress))),
                    at(9, delta("c", DeltaKind::CommandOutput, "out")),
                    at(5, completed("c", command("out", ItemStatus::Completed))),
                    at(6, turn_done("t1")),
                ],
                0,
            ),
            (
                "a stream completed after its turn ended",
                vec![
                    at(1, turn("t1")),
                    at(2, delta("a", DeltaKind::AssistantText, "late")),
                    at(3, delta("a", DeltaKind::AssistantText, " text")),
                    at(4, turn_done("t1")),
                    at(5, completed("a", message("late text"))),
                ],
                0,
            ),
            (
                "two streams interleaved, each covered by its completion",
                vec![
                    at(1, turn("t1")),
                    at(2, delta("r", DeltaKind::ReasoningText, "think")),
                    at(3, delta("a", DeltaKind::AssistantText, "an")),
                    at(4, delta("r", DeltaKind::ReasoningText, "ing")),
                    at(5, delta("a", DeltaKind::AssistantText, "swer")),
                    at(
                        6,
                        completed(
                            "r",
                            ItemContent::Reasoning {
                                text: "thinking".into(),
                            },
                        ),
                    ),
                    at(7, completed("a", message("answer"))),
                ],
                2,
            ),
        ];
        for (name, records, removed) in cases {
            let (kept, actual) = superseded(&records);
            assert_eq!(actual, removed, "{name}");
            assert_eq!(
                Timeline::fold_stored(&kept),
                Timeline::fold_stored(&records),
                "{name}"
            );
        }
    }
}
