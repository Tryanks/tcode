use std::collections::HashSet;
use std::ops::Range;

/// Before GPUI reports its exact visible range, eight turns is comfortably
/// more than a typical chat viewport. Its list also pre-measures four viewport
/// heights, so the wider eviction band keeps warm rows resident without tying
/// Markdown lifetime to measurement.
const VIEWPORT_TURN_HINT: usize = 8;
const BUILD_MARGIN_TURNS: usize = 8;
/// Three build margins prevent back-and-forth scrolling from rebuilding the
/// same parsed documents at the edge of the warm window.
const EVICT_MARGIN_TURNS: usize = 24;
/// The composer-adjacent tail stays ready even while inspecting old turns.
const TAIL_PIN_TURNS: usize = 2;

#[derive(Clone, Debug)]
pub(super) struct MarkdownEntry {
    pub id: String,
    pub turn: usize,
    pub turn_running: bool,
}

pub(super) struct ResidencyInput<'a> {
    pub turn_count: usize,
    pub visible_turns: Range<usize>,
    pub one_shot_turn_target: Option<usize>,
    pub entries: &'a [MarkdownEntry],
    pub stream_running: bool,
    pub resident_ids: &'a HashSet<String>,
    pub selection_participants: &'a HashSet<String>,
    pub selection_drag_active: bool,
}

/// Turn-only superset of entries that can affect [`decide`].
pub(super) struct ResidencyScope {
    build_turns: Range<usize>,
    keep_turns: Range<usize>,
    tail_start: usize,
    last_turn: Option<usize>,
    stream_running: bool,
}

impl ResidencyScope {
    pub(super) fn new(
        turn_count: usize,
        visible_turns: Range<usize>,
        one_shot_turn_target: Option<usize>,
        stream_running: bool,
    ) -> Self {
        let visible_turns = one_shot_turn_target
            .filter(|turn| *turn < turn_count)
            .map(|turn| turn..(turn + VIEWPORT_TURN_HINT).min(turn_count))
            .unwrap_or(visible_turns);
        Self {
            build_turns: expand_turn_window(visible_turns.clone(), BUILD_MARGIN_TURNS, turn_count),
            keep_turns: expand_turn_window(visible_turns, EVICT_MARGIN_TURNS, turn_count),
            tail_start: turn_count.saturating_sub(TAIL_PIN_TURNS),
            last_turn: turn_count.checked_sub(1),
            stream_running,
        }
    }

    pub(super) fn includes(&self, turn: usize, turn_running: bool) -> bool {
        self.keep_turns.contains(&turn) || self.pinned(turn, turn_running)
    }

    fn pinned(&self, turn: usize, turn_running: bool) -> bool {
        self.last_turn.is_some() && turn >= self.tail_start
            || turn_running
            || self.stream_running && self.last_turn == Some(turn)
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ResidencyDecisions {
    pub build: HashSet<String>,
    pub evict: HashSet<String>,
}

pub(super) fn decide(input: ResidencyInput<'_>) -> ResidencyDecisions {
    let scope = ResidencyScope::new(
        input.turn_count,
        input.visible_turns,
        input.one_shot_turn_target,
        input.stream_running,
    );

    let build = input
        .entries
        .iter()
        .filter(|entry| {
            scope.build_turns.contains(&entry.turn) || scope.pinned(entry.turn, entry.turn_running)
        })
        .map(|entry| entry.id.clone())
        .collect();
    let evict = if input.selection_drag_active {
        HashSet::new()
    } else {
        let keep = input
            .entries
            .iter()
            .filter(|entry| scope.includes(entry.turn, entry.turn_running))
            .map(|entry| entry.id.as_str())
            .collect::<HashSet<_>>();
        input
            .resident_ids
            .iter()
            .filter(|id| !keep.contains(id.as_str()) && !input.selection_participants.contains(*id))
            .cloned()
            .collect()
    };

    ResidencyDecisions { build, evict }
}

pub(super) fn tail_turn_window(turn_count: usize) -> Range<usize> {
    turn_count.saturating_sub(VIEWPORT_TURN_HINT)..turn_count
}

pub(super) fn viewport_turn_window(scroll_top: usize, turn_count: usize) -> Range<usize> {
    scroll_top..(scroll_top + VIEWPORT_TURN_HINT).min(turn_count)
}

fn expand_turn_window(window: Range<usize>, margin: usize, turn_count: usize) -> Range<usize> {
    window.start.min(turn_count).saturating_sub(margin)
        ..window
            .end
            .min(turn_count)
            .saturating_add(margin)
            .min(turn_count)
}

#[cfg(test)]
mod tests {
    use super::{
        MarkdownEntry, ResidencyDecisions, ResidencyInput, ResidencyScope, decide, tail_turn_window,
    };
    use std::collections::HashSet;

    #[test]
    fn tail_residency_is_bounded() {
        let entries = entries(240);
        let decisions = decisions(&entries, tail_turn_window(240), None, &HashSet::new());

        assert_eq!(decisions.build.len(), 48);
        assert!(decisions.evict.is_empty());
    }

    #[test]
    fn small_scrolls_keep_the_hysteresis_band_warm() {
        let entries = entries(240);
        let mut residents = decisions(&entries, tail_turn_window(240), None, &HashSet::new()).build;
        let shifted = decisions(&entries, 230..238, None, &residents);
        apply(&mut residents, shifted);
        let back_to_tail = decisions(&entries, tail_turn_window(240), None, &residents);

        assert!(back_to_tail.build.is_subset(&residents));
        assert!(back_to_tail.evict.is_empty());
    }

    #[test]
    fn jump_rebuilds_an_evicted_region() {
        let entries = entries(240);
        let mut residents = decisions(&entries, tail_turn_window(240), None, &HashSet::new()).build;
        let jump = decisions(&entries, tail_turn_window(240), Some(40), &residents);
        apply(&mut residents, jump);

        assert_eq!(residents.len(), 78);
        assert!(residents.contains("assistant-40"));
        assert!(!residents.contains("assistant-230"));
        assert!(residents.contains("assistant-238"));
        assert!(residents.contains("assistant-239"));
    }

    #[test]
    fn running_tail_and_selection_pins_are_honored() {
        let mut entries = entries(240);
        for entry in &mut entries {
            entry.turn_running = entry.turn == 5;
        }
        let residents = ["assistant-7".to_string(), "assistant-100".to_string()]
            .into_iter()
            .collect();
        let selection_participants = ["assistant-7".to_string()].into_iter().collect();
        let decisions = decide(ResidencyInput {
            turn_count: 240,
            visible_turns: 40..48,
            one_shot_turn_target: None,
            entries: &entries,
            stream_running: false,
            resident_ids: &residents,
            selection_participants: &selection_participants,
            selection_drag_active: false,
        });

        assert!(decisions.build.contains("assistant-5"));
        assert!(decisions.build.contains("assistant-238"));
        assert!(decisions.build.contains("assistant-239"));
        assert!(!decisions.evict.contains("assistant-7"));
        assert!(decisions.evict.contains("assistant-100"));
    }

    #[test]
    fn candidate_filter_preserves_decisions_with_distant_running_and_selection() {
        let mut all_entries = entries(200);
        for entry in &mut all_entries {
            entry.turn_running = entry.turn == 5;
        }
        let residents = [
            "assistant-5".to_string(),
            "assistant-40".to_string(),
            "assistant-100".to_string(),
            "assistant-199".to_string(),
        ]
        .into_iter()
        .collect();
        let selection_participants = ["assistant-100".to_string()].into_iter().collect();
        let scope = ResidencyScope::new(200, 40..48, None, true);
        let filtered_entries = all_entries
            .iter()
            .filter(|entry| scope.includes(entry.turn, entry.turn_running))
            .cloned()
            .collect::<Vec<_>>();
        let run = |entries: &[MarkdownEntry]| {
            decide(ResidencyInput {
                turn_count: 200,
                visible_turns: 40..48,
                one_shot_turn_target: None,
                entries,
                stream_running: true,
                resident_ids: &residents,
                selection_participants: &selection_participants,
                selection_drag_active: false,
            })
        };

        assert_eq!(run(&filtered_entries), run(&all_entries));
        assert!(run(&filtered_entries).build.contains("assistant-5"));
        assert!(!run(&filtered_entries).evict.contains("assistant-100"));
    }

    fn entries(turn_count: usize) -> Vec<MarkdownEntry> {
        (0..turn_count)
            .flat_map(|turn| {
                ["user", "reasoning", "assistant"].map(move |kind| MarkdownEntry {
                    id: format!("{kind}-{turn}"),
                    turn,
                    turn_running: false,
                })
            })
            .collect()
    }

    fn decisions(
        entries: &[MarkdownEntry],
        visible_turns: std::ops::Range<usize>,
        one_shot_turn_target: Option<usize>,
        resident_ids: &HashSet<String>,
    ) -> ResidencyDecisions {
        decide(ResidencyInput {
            turn_count: 240,
            visible_turns,
            one_shot_turn_target,
            entries,
            stream_running: false,
            resident_ids,
            selection_participants: &HashSet::new(),
            selection_drag_active: false,
        })
    }

    fn apply(residents: &mut HashSet<String>, decisions: ResidencyDecisions) {
        residents.retain(|id| !decisions.evict.contains(id));
        residents.extend(decisions.build);
    }
}
