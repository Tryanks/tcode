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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gpui::{Entity, TestAppContext, VisualTestContext, px};

    use crate::chat::ChatView;
    use crate::chat::tests::{ScriptedThread, answer_event, user_event};
    use crate::test_frames::draw;

    fn turn(turn: usize) -> [agent::AgentEvent; 4] {
        [
            agent::AgentEvent::TurnStarted {
                turn_id: turn.to_string(),
            },
            user_event(&format!("user-{turn}"), &format!("Question {turn}")),
            answer_event(
                &format!("answer-{turn}"),
                &"An earlier answer.\n\n".repeat(6),
            ),
            agent::AgentEvent::TurnCompleted {
                turn_id: turn.to_string(),
                status: agent::TurnStatus::Completed,
                usage: None,
            },
        ]
    }

    /// `count` finished turns that fill more than a screen, then a turn the
    /// agent is still working on, holding only its question.
    fn earlier_turns_and_a_running_one(count: usize) -> Vec<agent::AgentEvent> {
        let mut events: Vec<_> = (0..count).flat_map(turn).collect();
        events.extend([
            agent::AgentEvent::TurnStarted {
                turn_id: count.to_string(),
            },
            user_event(&format!("user-{count}"), "Look at the build."),
        ]);
        events
    }

    /// One display frame: time passes, the frames the window asked for are
    /// delivered, and it draws.
    fn frame(cx: &mut VisualTestContext) {
        cx.executor().advance_clock(Duration::from_millis(16));
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
        });
        draw(cx);
    }

    fn streamed(id: &str, text: &str) -> agent::AgentEvent {
        agent::AgentEvent::Delta {
            item_id: id.into(),
            kind: agent::DeltaKind::AssistantText,
            text: text.into(),
        }
    }

    const SEED: &str = "`seed` opens the reply.\n\n";

    /// A running turn answering in a row the reader follows to its end.
    fn following_a_reply(
        cx: &mut TestAppContext,
    ) -> (ScriptedThread, Entity<ChatView>, &mut VisualTestContext) {
        let mut events = earlier_turns_and_a_running_one(4);
        events.push(agent::AgentEvent::ItemStarted(agent::ThreadItem {
            id: "streaming".into(),
            parent_item_id: None,
            content: agent::ItemContent::AssistantMessage {
                text: SEED.into(),
                summarized: false,
            },
        }));
        let thread = ScriptedThread::new(cx, events, 0, true);
        let (chat, cx) = thread.open(cx);
        thread.deliver(cx);
        for _ in 0..20 {
            frame(cx);
        }
        (thread, chat, cx)
    }

    /// `count` one-line paragraphs of six words, each opening on a chip.
    fn paragraphs(count: usize) -> String {
        (0..count)
            .map(|index| format!("`{index}` is one short line of it.\n\n"))
            .collect()
    }

    fn scrolled(list: &gpui::ListState) -> gpui::Pixels {
        -list.scroll_px_offset_for_scrollbar().y
    }

    /// The turn ending with a block still revealing, or with the block
    /// itself, leaves the reveal to finish: the row opens with it rather than
    /// taking its height at once.
    #[gpui::test]
    fn a_reveal_outlives_the_message_that_brought_it(cx: &mut TestAppContext) {
        for frames_before_the_end in [0, 2] {
            let (mut thread, chat, cx) = following_a_reply(cx);
            let list = chat.read_with(cx, |chat, _| chat.list_state.clone());
            let start = scrolled(&list);
            thread.live(streamed("streaming", &paragraphs(40)));
            if frames_before_the_end > 0 {
                thread.deliver(cx);
            }
            for _ in 0..frames_before_the_end {
                frame(cx);
                assert!(
                    scrolled(&list) < start + px(1_000.),
                    "the block came at once"
                );
            }
            thread.live(agent::AgentEvent::TurnCompleted {
                turn_id: "4".into(),
                status: agent::TurnStatus::Completed,
                usage: None,
            });
            thread.set_running(false);
            thread.deliver(cx);
            let mut position = start;
            let mut steps = vec![scrolled(&list) - start];
            position += steps[0];
            for _ in 0..90 {
                frame(cx);
                let now = scrolled(&list);
                assert!(now >= position - px(0.5), "the list moved back");
                steps.push(now - position);
                position = now;
            }
            let travel = position - start;
            assert!(travel > px(1_000.), "the block has height: {travel:?}");
            let largest = steps.iter().copied().fold(px(0.), gpui::Pixels::max);
            assert!(
                largest < travel / 4.,
                "the row took its height at once as the turn ended: {steps:?}"
            );
            assert_eq!(steps[steps.len() - 1], px(0.), "the row came to rest");
        }
    }

    /// A reader who scrolled up is not moved as a block arrives below them,
    /// and the list holds it at its real height: coming back to the tail
    /// nothing folds up, and the next words do not take the height back.
    #[gpui::test]
    fn a_reader_who_paused_finds_the_block_at_its_height_on_return(cx: &mut TestAppContext) {
        let (mut thread, chat, cx) = following_a_reply(cx);
        let list = chat.read_with(cx, |chat, _| chat.list_state.clone());
        let wheel = |pixels: f32, cx: &mut VisualTestContext| {
            cx.simulate_event(gpui::ScrollEvent {
                position: list.viewport_bounds().center(),
                delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(pixels))),
                touch_phase: gpui::TouchPhase::Moved,
                ..Default::default()
            });
        };
        // Not so far that the reply leaves the screen: a row drawn all along.
        wheel(30., cx);
        for _ in 0..4 {
            frame(cx);
        }
        assert!(!list.is_following_tail(), "the wheel paused the follow");
        let reading = scrolled(&list);

        thread.live(streamed("streaming", &paragraphs(120)));
        thread.deliver(cx);
        for step in 0..6 {
            frame(cx);
            assert_eq!(
                scrolled(&list),
                reading,
                "step {step}: the reader was moved"
            );
        }
        let whole = list.max_offset_for_scrollbar().y;
        assert!(whole > reading + px(1_000.), "the block has its height");

        wheel(-100_000., cx);
        frame(cx);
        assert!(list.is_following_tail(), "the reader is back at the tail");
        let mut height = list.max_offset_for_scrollbar().y;
        assert!(height >= whole - px(0.5), "the block folded up on return");
        for step in 0..40 {
            if step == 4 {
                thread.live(streamed("streaming", "One more line.\n\n"));
                thread.deliver(cx);
            }
            frame(cx);
            let now = list.max_offset_for_scrollbar().y;
            assert!(now >= height - px(0.5), "step {step}: the list shrank");
            height = now;
        }
    }

    fn command(id: &str) -> agent::AgentEvent {
        agent::AgentEvent::ItemCompleted(agent::ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content: agent::ItemContent::CommandExecution {
                command: format!("run {id}"),
                output: format!("output of {id}\n").repeat(4),
                exit_code: Some(0),
                status: agent::ItemStatus::Completed,
            },
        })
    }

    /// When prose ends a live run of tool calls, the run settles into its
    /// Work Log header in the same record that brings the reply's row. The
    /// run shrinks as the reply rises, so the content above moves one way,
    /// up, from where it was: never down, then up.
    #[gpui::test]
    fn a_run_settling_as_the_reply_arrives_moves_the_content_above_one_way(
        cx: &mut TestAppContext,
    ) {
        let mut events = earlier_turns_and_a_running_one(4);
        events.push(command("first"));
        let mut thread = ScriptedThread::new(cx, events, 0, true);
        let (chat, cx) = thread.open(cx);
        thread.deliver(cx);
        for _ in 0..40 {
            frame(cx);
        }
        let list = chat.read_with(cx, |chat, _| chat.list_state.clone());
        assert!(crate::chat::tests::command_open(cx, "first").is_some());
        let rows = list.item_count();
        // The question's row sits above the run.
        let question = rows - 2;
        let top = |cx: &mut VisualTestContext| {
            cx.debug_bounds(format!("timeline-row-{question}").leak())
                .expect("the question is on screen")
                .top()
        };
        let before = top(cx);
        thread.live(answer_event(
            "answer-4",
            &"The command printed what was expected; the build is fine.\n\n".repeat(4),
        ));
        thread.deliver(cx);
        let mut tops = Vec::new();
        for index in 0..16 {
            if index == 0 {
                draw(cx);
            } else {
                frame(cx);
            }
            assert_eq!(list.item_count(), rows + 1);
            tops.push(top(cx));
        }
        assert!(
            crate::chat::tests::command_open(cx, "first").is_none(),
            "the run settled into its header"
        );
        assert!(
            (tops[0] - before).abs() <= px(0.5),
            "the content above jumped as the reply arrived: {before:?} -> {tops:?}"
        );
        assert!(
            tops.windows(2).all(|pair| pair[1] <= pair[0] + px(0.5)),
            "the content above moved back down: {before:?} -> {tops:?}"
        );
    }
}
