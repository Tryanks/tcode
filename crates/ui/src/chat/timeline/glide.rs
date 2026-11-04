//! Which timeline rows rise from below: the timeline's stamp policy over
//! [`crate::motion::list_rows`].
//!
//! While the reader follows the tail the list keeps its end on the
//! viewport's bottom edge, so a row whose layout height grows carries the
//! content above it upward: the row rises from below.

use gpui::App;
use tcode_core::session::EntryContent;

use crate::motion::list_rows::{Arrival, Attach};

use super::super::ChatView;
use super::LeftRow;
use super::rows::{RowPart, TimelineRow};

/// The rows a regroup takes out, each with the survivor it folds against.
/// The reader's anchor is never among them: the header they collapse is on
/// screen above them.
#[derive(Default)]
pub(super) struct Leaving(Vec<(u64, LeftRow, Attach)>);

impl ChatView {
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
            .with_timeline_for(self.session_id.as_deref(), |timeline| {
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
            })
            .unwrap_or_default();
        for (key, arrival, brings_turn) in stamps {
            self.rows_motion.arrive(key, arrival, brings_turn, at);
        }
    }
}

impl ChatView {
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
            .with_timeline_for(self.session_id.as_deref(), |timeline| {
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
            })
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
    use crate::test_frames::{draw, draw_settled};
    use crate::theme::ActiveTheme as _;

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

    /// The last row's height, where row `above` starts, and how far the last
    /// row ends from the timeline's bottom edge.
    #[derive(Debug, Clone, Copy, PartialEq)]
    struct Tail {
        height: gpui::Pixels,
        above_top: gpui::Pixels,
        bottom_gap: gpui::Pixels,
    }

    fn tail(chat: &Entity<ChatView>, above: usize, cx: &mut VisualTestContext) -> Tail {
        let list = chat.read_with(cx, |chat, _| chat.list_state.clone());
        let last = list.item_count() - 1;
        let row = cx
            .debug_bounds(format!("timeline-row-{last}").leak())
            .expect("the last row is on screen");
        let above = cx
            .debug_bounds(format!("timeline-row-{above}").leak())
            .expect("the row above is on screen");
        Tail {
            height: row.size.height,
            above_top: above.top(),
            bottom_gap: list.viewport_bounds().bottom() - row.bottom(),
        }
    }

    /// While the reader follows the tail, a row that arrives live grows from
    /// nothing to its height over the glide, and the list keeps its end on
    /// the timeline's bottom edge, so the content above is carried upward
    /// continuously: from where it was, never back down. Reduced motion puts
    /// the row in at once. Once in, the window comes to rest.
    #[gpui::test]
    fn a_row_that_arrives_live_rises_from_below_on_the_followed_tail(cx: &mut TestAppContext) {
        for reduce in [false, true] {
            let mut thread = ScriptedThread::new(cx, earlier_turns_and_a_running_one(4), 0, true);
            cx.update(|cx| cx.set_reduce_motion(reduce));
            let (chat, cx) = thread.open(cx);
            thread.deliver(cx);
            // The running turn's working status repeats: frames, not rest.
            for _ in 0..20 {
                frame(cx);
            }
            let list = chat.read_with(cx, |chat, _| chat.list_state.clone());
            assert!(list.is_following_tail());
            assert!(
                list.max_offset_for_scrollbar().y > list.viewport_bounds().size.height,
                "the thread scrolls"
            );
            assert!(
                cx.debug_bounds("working-status-4").is_some(),
                "the turn runs"
            );
            let rows = list.item_count();
            let before = tail(&chat, rows - 1, cx);

            thread.live(answer_event(
                "answer-4",
                &"The build passes; here is what changed.\n\n".repeat(5),
            ));
            thread.deliver(cx);
            let mut seen = Vec::new();
            for index in 0..16 {
                if index == 0 {
                    draw(cx);
                } else {
                    frame(cx);
                }
                assert_eq!(
                    list.item_count(),
                    rows + 1,
                    "the answer is a row of its own"
                );
                assert!(list.is_following_tail(), "the arrival paused the follow");
                let now = tail(&chat, rows - 1, cx);
                assert!(
                    now.bottom_gap.abs() <= px(1.),
                    "reduce {reduce}: the tail left the bottom edge: {now:?}"
                );
                seen.push(now);
            }
            let settled = *seen.last().unwrap();
            let risen = before.above_top - settled.above_top;
            assert!(
                risen > px(150.),
                "the answer has its height: {before:?} -> {settled:?}"
            );
            if reduce {
                assert_eq!(seen[0], settled, "reduced motion puts the row in at once");
            } else {
                assert!(
                    (seen[0].above_top - before.above_top).abs() <= px(0.5),
                    "the content above jumped as the row arrived: {before:?} -> {:?}",
                    seen[0]
                );
                assert!(
                    seen.windows(2)
                        .all(|pair| pair[1].above_top <= pair[0].above_top + px(0.5)),
                    "the content above moved back down: {seen:?}"
                );
                let rising = seen
                    .windows(2)
                    .filter(|pair| pair[1].above_top < pair[0].above_top - px(0.5))
                    .count();
                assert!(rising >= 8, "the rise took {rising} frames: {seen:?}");
                assert_eq!(seen[seen.len() - 3], settled, "{seen:?}");
            }

            // The turn ends; nothing is left moving.
            thread.live(agent::AgentEvent::TurnCompleted {
                turn_id: "4".into(),
                status: agent::TurnStatus::Completed,
                usage: None,
            });
            thread.set_running(false);
            thread.deliver(cx);
            draw_settled(cx);
            cx.update(|_, cx| cx.set_reduce_motion(false));
        }
    }

    fn streamed(id: &str, text: &str) -> agent::AgentEvent {
        agent::AgentEvent::Delta {
            item_id: id.into(),
            kind: agent::DeltaKind::AssistantText,
            text: text.into(),
        }
    }

    /// The followed end of a running turn glides to each new measurement of
    /// what it shows: a streamed paragraph rises in over several frames
    /// instead of popping its height, and the end stays on the bottom edge.
    #[gpui::test]
    fn a_streamed_paragraph_rises_into_the_followed_tail(cx: &mut TestAppContext) {
        let mut events = earlier_turns_and_a_running_one(4);
        events.push(agent::AgentEvent::ItemStarted(agent::ThreadItem {
            id: "streaming".into(),
            parent_item_id: None,
            content: agent::ItemContent::AssistantMessage {
                text: "The first paragraph of the reply.\n\n".into(),
                summarized: false,
            },
        }));
        let mut thread = ScriptedThread::new(cx, events, 0, true);
        let (chat, cx) = thread.open(cx);
        thread.deliver(cx);
        for _ in 0..20 {
            frame(cx);
        }
        let list = chat.read_with(cx, |chat, _| chat.list_state.clone());
        let rows = list.item_count();
        let above = rows - 2;
        for paragraph in 0..3 {
            let before = tail(&chat, above, cx);
            thread.live(streamed(
                "streaming",
                &format!("Paragraph {paragraph} arrives in one delta, long enough to wrap.\n\n")
                    .repeat(3),
            ));
            thread.deliver(cx);
            let mut seen = Vec::new();
            for _ in 0..16 {
                frame(cx);
                assert_eq!(list.item_count(), rows, "the reply grows in place");
                let now = tail(&chat, above, cx);
                assert!(
                    now.bottom_gap.abs() <= px(1.),
                    "paragraph {paragraph}: the tail left the bottom edge: {now:?}"
                );
                seen.push(now);
            }
            let settled = *seen.last().unwrap();
            assert!(
                settled.height > before.height + px(40.),
                "paragraph {paragraph} has height: {before:?} -> {settled:?}"
            );
            let heights: Vec<_> = seen.iter().map(|tail| tail.height).collect();
            assert!(
                heights.windows(2).all(|pair| pair[1] >= pair[0]),
                "paragraph {paragraph}: the reply shrank as it grew: {heights:?}"
            );
            let growing = heights.windows(2).filter(|pair| pair[1] > pair[0]).count();
            assert!(
                growing >= 6,
                "paragraph {paragraph} popped in over {growing} frames: {heights:?}"
            );
        }
    }

    /// A reader who scrolled up while the agent works is not moved: rows
    /// arriving and a reply streaming below leave every pixel on screen in
    /// place, frame by frame.
    #[gpui::test]
    fn a_reader_scrolled_up_stays_put_while_rows_arrive(cx: &mut TestAppContext) {
        let mut thread = ScriptedThread::new(cx, earlier_turns_and_a_running_one(6), 0, true);
        let (chat, cx) = thread.open(cx);
        thread.deliver(cx);
        for _ in 0..20 {
            frame(cx);
        }
        let list = chat.read_with(cx, |chat, _| chat.list_state.clone());
        cx.simulate_event(gpui::ScrollEvent {
            position: list.viewport_bounds().center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(240.))),
            touch_phase: gpui::TouchPhase::Moved,
            ..Default::default()
        });
        for _ in 0..4 {
            frame(cx);
        }
        assert!(!list.is_following_tail(), "the wheel paused the follow");
        let rows = list.item_count();
        let on_screen = |cx: &mut VisualTestContext| {
            (0..rows)
                .filter_map(|row| cx.debug_bounds(format!("timeline-row-{row}").leak()))
                .filter(|bounds| bounds.intersects(&list.viewport_bounds()))
                .collect::<Vec<_>>()
        };
        let reading = on_screen(cx);
        assert!(!reading.is_empty());

        thread.live(answer_event(
            "answer-6",
            &"Done; here is the summary.\n\n".repeat(4),
        ));
        thread.live(agent::AgentEvent::ItemStarted(agent::ThreadItem {
            id: "more".into(),
            parent_item_id: None,
            content: agent::ItemContent::AssistantMessage {
                text: String::new(),
                summarized: false,
            },
        }));
        thread.deliver(cx);
        for step in 0..16 {
            if step % 4 == 0 {
                thread.live(streamed("more", "Another streamed paragraph.\n\n"));
                thread.deliver(cx);
            }
            frame(cx);
            assert!(
                !list.is_following_tail(),
                "step {step}: the reader was pulled down"
            );
            assert_eq!(on_screen(cx), reading, "step {step}: the reader was moved");
        }
        assert!(
            list.item_count() > rows,
            "the rows arrived below the reader"
        );
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

    /// The bottom edge and alpha of each inline code chip painted last frame,
    /// as far down as the list shows text, top to bottom. A chip is several
    /// quads on one line.
    fn chips(limit: gpui::Pixels, cx: &mut VisualTestContext) -> Vec<(gpui::Pixels, f32)> {
        cx.update(|window, cx| {
            let muted = cx.theme().tokens.colors.muted;
            let scale = window.scale_factor();
            let mut quads: Vec<_> = crate::test_frames::painted_quads(window)
                .into_iter()
                .filter_map(|quad| {
                    let color = quad.background.as_solid()?;
                    let (top, bottom) = (quad.bounds.top().0, quad.bounds.bottom().0);
                    ((color.h, color.s, color.l) == (muted.h, muted.s, muted.l)
                        && (bottom - top) / scale < 40.)
                        .then(|| (px(bottom / scale), color.a))
                })
                .filter(|(bottom, _)| *bottom <= limit)
                .collect();
            quads.sort_by(|(a, _), (b, _)| a.as_f32().total_cmp(&b.as_f32()));
            let mut chips: Vec<(gpui::Pixels, f32)> = Vec::new();
            for (bottom, alpha) in quads {
                match chips.last_mut() {
                    Some(chip) if (bottom - chip.0).abs() < px(8.) => {
                        chip.0 = chip.0.max(bottom);
                        chip.1 = chip.1.max(alpha);
                    }
                    _ => chips.push((bottom, alpha)),
                }
            }
            chips
        })
    }

    /// Where the followed text ends at rest, above the end's padding and the
    /// turn's status, and a chip's resting alpha.
    fn text_edge(chat: &Entity<ChatView>, cx: &mut VisualTestContext) -> (gpui::Pixels, f32) {
        let list = chat.read_with(cx, |chat, _| chat.list_state.clone());
        let chips = chips(list.viewport_bounds().bottom(), cx);
        let &(bottom, rest) = chips.last().expect("the reply's last chip is on screen");
        (bottom, rest)
    }

    /// How far above `edge` the lowest chip the reveal has reached ends: the
    /// blank under the text. A chip past the edge is still under the row's clip.
    fn blank(edge: gpui::Pixels, rest: f32, cx: &mut VisualTestContext) -> gpui::Pixels {
        chips(edge + px(1.), cx)
            .iter()
            .rev()
            .find(|(_, alpha)| *alpha >= rest * 0.2)
            .map_or(px(1_000.), |(bottom, _)| edge - *bottom)
    }

    fn scrolled(list: &gpui::ListState) -> gpui::Pixels {
        -list.scroll_px_offset_for_scrollbar().y
    }

    /// The height the row is laid out at, measured afresh.
    fn measured_again(list: &gpui::ListState, cx: &mut VisualTestContext) -> gpui::Pixels {
        let row = format!("timeline-row-{}", list.item_count() - 1);
        list.remeasure();
        draw(cx);
        cx.debug_bounds(row.leak())
            .expect("the last row")
            .size
            .height
    }

    /// A large block delivered in one piece reveals as the list scrolls to
    /// it, as one motion: on every frame the text reaches the bottom edge of
    /// the followed list, the list only moves on, and the block is whole
    /// within a second; the row lands on its measurement, at the tail. Code,
    /// a table and a list among the prose come as they are reached.
    #[gpui::test]
    fn a_large_block_reveals_as_the_followed_tail_scrolls_to_it(cx: &mut TestAppContext) {
        let code = format!("```rust\n{}```\n\n", "let line = 1;\n".repeat(8));
        let table = "| a | b |\n| - | - |\n| 1 | 2 |\n| 3 | 4 |\n\n";
        let mixed = format!("{}{code}{table}- one\n- two\n\n", paragraphs(1)).repeat(16);
        for (block, bound) in [(paragraphs(120), px(48.)), (mixed, px(260.))] {
            let (mut thread, chat, cx) = following_a_reply(cx);
            let list = chat.read_with(cx, |chat, _| chat.list_state.clone());
            let (edge, rest) = text_edge(&chat, cx);
            let last = format!("timeline-row-{}", list.item_count() - 1).leak() as &'static str;
            let start = scrolled(&list);

            thread.live(streamed("streaming", &block));
            thread.deliver(cx);
            let mut positions = Vec::new();
            let mut heights = Vec::new();
            for index in 0..96 {
                frame(cx);
                assert!(list.is_following_tail(), "frame {index}: the follow paused");
                positions.push(scrolled(&list));
                heights.push(cx.debug_bounds(last).expect("the last row").size.height);
                let gap = blank(edge, rest, cx);
                assert!(
                    gap <= bound,
                    "frame {index}: {gap:?} blank under the revealed text"
                );
            }
            assert!(
                positions
                    .windows(2)
                    .all(|pair| pair[1] >= pair[0] - px(0.5)),
                "the list moved back: {positions:?}"
            );
            assert!(
                heights.windows(2).all(|pair| pair[1] >= pair[0] - px(0.5)),
                "the row shrank: {heights:?}"
            );
            let end = *positions.last().unwrap();
            assert!(
                end > start + px(1_000.),
                "the block has height: {positions:?}"
            );
            let arrived = positions
                .iter()
                .position(|at| *at >= end - px(0.5))
                .unwrap();
            assert!(
                (30..=75).contains(&arrived),
                "the list reached the block's end after {arrived} frames: {positions:?}"
            );
            let row = cx.debug_bounds(last).expect("the last row");
            assert!(
                (list.viewport_bounds().bottom() - row.bottom()).abs() <= px(1.),
                "the list came to rest away from the tail"
            );
            assert_eq!(
                measured_again(&list, cx),
                *heights.last().unwrap(),
                "the row landed off its measurement"
            );
        }
    }

    /// A block too small to need hurrying comes at the word-by-word pace,
    /// and the row opens with it, not ahead of it.
    #[gpui::test]
    fn a_small_block_keeps_the_word_by_word_pace(cx: &mut TestAppContext) {
        let (mut thread, chat, cx) = following_a_reply(cx);
        let (edge, rest) = text_edge(&chat, cx);
        thread.live(streamed("streaming", "`a`\n\n`b`\n\n`c`\n\n`d`\n\n`e`"));
        thread.deliver(cx);
        let mut revealed = Vec::new();
        for index in 0..30 {
            frame(cx);
            let gap = blank(edge, rest, cx);
            assert!(
                gap <= px(40.),
                "frame {index}: {gap:?} blank under the text"
            );
            let chips = chips(edge + px(400.), cx);
            revealed.push(chips.iter().filter(|(_, a)| *a >= rest * 0.2).count());
        }
        let list = chat.read_with(cx, |chat, _| chat.list_state.clone());
        assert!(list.is_following_tail());
        assert_eq!(revealed[29], revealed[0] + 5, "{revealed:?}");
        // Display frames come faster than words: none brings two, and the
        // five are in within their steps.
        assert!(
            revealed.windows(2).all(|pair| pair[1] <= pair[0] + 1),
            "words came together: {revealed:?}"
        );
        let last = revealed.iter().position(|count| *count == revealed[29]);
        assert!(
            last.is_some_and(|last| last <= 12),
            "five words took too long: {revealed:?}"
        );
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
            thread.live(streamed("streaming", &paragraphs(120)));
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

    /// Rows a fold brings were already in the thread; they are in place at
    /// once, on the first frame that shows them: the window a thread opens
    /// with, a history page that lands above it, and the records a client
    /// catches up on.
    #[gpui::test]
    fn rows_a_fold_brings_are_in_place_at_once(cx: &mut TestAppContext) {
        // Of six finished turns the window holds the last; the rest are a
        // page above it.
        let events: Vec<_> = (0..6).flat_map(turn).collect();
        let held_from = events.len() - 4;
        let mut thread = ScriptedThread::new(cx, events, held_from, false);
        let (chat, cx) = thread.open(cx);
        thread.deliver(cx);
        let list = chat.read_with(cx, |chat, _| chat.list_state.clone());
        // What a frame shows, as the rows' heights: a row still rising would
        // differ from the frames after it.
        let heights = |cx: &mut VisualTestContext| {
            (0..list.item_count())
                .map(|row| {
                    cx.debug_bounds(format!("timeline-row-{row}").leak())
                        .map(|bounds| bounds.size.height)
                })
                .collect::<Vec<_>>()
        };
        // A page in flight shows its spinner, which never rests: frames
        // spanning a glide stand in for coming to rest.
        let glide = |cx: &mut VisualTestContext| {
            for _ in 0..16 {
                frame(cx);
            }
        };
        draw(cx);
        let opened = heights(cx);
        assert!(
            opened.iter().all(Option::is_some),
            "the window fits the screen"
        );
        glide(cx);
        assert_eq!(
            heights(cx),
            opened,
            "the window the thread opened with rose in"
        );

        // The window does not fill the screen, so the timeline asks for the
        // page above it.
        let rows = list.item_count();
        let mut asked = false;
        for _ in 0..20 {
            frame(cx);
            if thread.answer_page(usize::MAX).is_some() {
                asked = true;
                break;
            }
        }
        assert!(asked, "the timeline asks for the page above its window");
        thread.deliver(cx);
        draw(cx);
        assert!(list.item_count() > rows, "the page landed");
        let paged = heights(cx);
        glide(cx);
        assert_eq!(heights(cx), paged, "the history page rose in");

        // The host sends its window again with a turn the client missed.
        let rows = list.item_count();
        thread.send_window(turn(6).into());
        thread.deliver(cx);
        draw(cx);
        assert!(list.item_count() > rows, "the missed turn is shown");
        let caught_up = heights(cx);
        glide(cx);
        assert_eq!(heights(cx), caught_up, "the records caught up on rose in");
        assert!(list.is_following_tail());
    }

    /// A scrolling screenshot shows every row whole: rows still rising when
    /// it begins are drawn at rest by the next frame.
    #[gpui::test]
    fn a_capture_shows_rising_rows_whole(cx: &mut TestAppContext) {
        let mut thread = ScriptedThread::new(cx, earlier_turns_and_a_running_one(4), 0, true);
        let (chat, cx) = thread.open(cx);
        thread.deliver(cx);
        for _ in 0..20 {
            frame(cx);
        }
        let list = chat.read_with(cx, |chat, _| chat.list_state.clone());
        let rows = list.item_count();
        let reply = &"The build passes; here is what changed.\n\n".repeat(5);
        thread.live(answer_event("answer-4", reply));
        thread.deliver(cx);
        draw(cx);
        frame(cx);
        let rising = tail(&chat, rows - 1, cx);
        assert!(chat.update(cx, |chat, cx| chat.capture_begin(cx)));
        draw(cx);
        let captured = tail(&chat, rows - 1, cx);
        assert!(chat.read_with(cx, |chat, _| chat.capture_settled()));
        assert!(
            captured.height > rising.height + px(40.),
            "the capture's frame shows the row still rising: {rising:?} -> {captured:?}"
        );
        chat.update(cx, |chat, cx| chat.capture_end(cx));
        for _ in 0..16 {
            frame(cx);
        }
        assert_eq!(
            tail(&chat, rows - 1, cx).height,
            captured.height,
            "the row the capture showed is the row at rest"
        );
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

    /// A tool call that found what it looked for: a one-line window.
    fn lookup(id: &str) -> agent::AgentEvent {
        agent::AgentEvent::ItemCompleted(agent::ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content: agent::ItemContent::ToolCall {
                name: "lookup".into(),
                input: serde_json::json!({ "query": id }),
                output: Some("found".into()),
                status: agent::ItemStatus::Completed,
                image_reads: Vec::new(),
            },
        })
    }

    /// A steer the agent takes leaves the turn's trailer for its place in the
    /// turn: the message travels there from where it was drawn, frame by
    /// frame, and the turn's working status, which it now stands above,
    /// travels below it; neither jumps. With motion reduced both are there at
    /// once.
    #[gpui::test]
    fn a_taken_steer_travels_to_its_place_in_the_turn(cx: &mut TestAppContext) {
        for reduce in [false, true] {
            let mut events = earlier_turns_and_a_running_one(4);
            events.push(command("first"));
            let mut thread = ScriptedThread::new(cx, events, 0, true);
            cx.update(|cx| cx.set_reduce_motion(reduce));
            let (_chat, cx) = thread.open(cx);
            thread.deliver(cx);
            thread.live(agent::AgentEvent::SteerRequested {
                request_id: "steer".into(),
                text: "Also check the docs.".into(),
                attachments: Vec::new(),
            });
            thread.deliver(cx);
            for _ in 0..40 {
                frame(cx);
            }
            let places = |cx: &mut VisualTestContext| {
                let bubble = cx
                    .debug_bounds("user-bubble-steer")
                    .expect("the steer is on screen");
                let status = cx
                    .debug_bounds("working-status-4")
                    .expect("the turn's working status");
                (bubble.top(), status.top())
            };
            let pending = places(cx);
            assert!(pending.0 > pending.1, "a pending steer is below the status");

            thread.live(agent::AgentEvent::SteerAccepted {
                request_id: "steer".into(),
            });
            thread.deliver(cx);
            draw(cx);
            let mut seen = vec![places(cx)];
            for _ in 0..20 {
                frame(cx);
                seen.push(places(cx));
            }
            let taken = *seen.last().unwrap();
            assert!(
                taken.0 < taken.1,
                "a taken steer is above the status: {seen:?}"
            );
            if reduce {
                assert_eq!(seen[0], taken, "{seen:?}");
            } else {
                assert!(
                    (seen[0].0 - pending.0).abs() <= px(1.),
                    "the steer jumped from where it was: {pending:?} -> {seen:?}"
                );
                for (part, name) in [(0, "steer"), (1, "status")] {
                    let at = |place: &(gpui::Pixels, gpui::Pixels)| {
                        if part == 0 { place.0 } else { place.1 }
                    };
                    let total = (at(&taken) - at(&seen[0])).abs();
                    assert!(total > px(10.), "the {name} moved: {seen:?}");
                    assert!(
                        seen.windows(2)
                            .all(|pair| (at(&pair[1]) - at(&pair[0])).abs() < total / 2.),
                        "the {name} jumped: {seen:?}"
                    );
                    assert!(
                        seen.windows(2)
                            .filter(|pair| (at(&pair[1]) - at(&pair[0])).abs() > px(0.5))
                            .count()
                            >= 6,
                        "the {name} travelled in too few frames: {seen:?}"
                    );
                }
            }
            cx.update(|_, cx| cx.set_reduce_motion(false));
        }
    }

    /// A tool call that completes with the pictures it read leaves its run
    /// for the turn's images, below their tiles: its window travels there
    /// from where it stood in the run, frame by frame, never jumping and never
    /// fading with the tiles. While the reader follows the tail, where it
    /// stood is where it ends, and it holds still as the tiles open above it.
    #[gpui::test]
    fn an_image_read_travels_from_its_run_to_the_images(cx: &mut TestAppContext) {
        let call = |status, image_reads| {
            agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                id: "shot".into(),
                parent_item_id: None,
                content: agent::ItemContent::ToolCall {
                    name: "screenshot".into(),
                    input: serde_json::json!({}),
                    output: Some("captured".into()),
                    status,
                    image_reads,
                },
            })
        };
        for following in [true, false] {
            let mut events = earlier_turns_and_a_running_one(6);
            events.extend([
                command("first"),
                call(agent::ItemStatus::InProgress, Vec::new()),
            ]);
            let mut thread = ScriptedThread::new(cx, events, 0, true);
            let (chat, cx) = thread.open_at(gpui::size(px(1_024.), px(1_000.)), cx);
            thread.deliver(cx);
            for _ in 0..40 {
                frame(cx);
            }
            let list = chat.read_with(cx, |chat, _| chat.list_state.clone());
            if !following {
                // Just up from the end: the window is still on screen, the
                // turn's status below it.
                cx.simulate_event(gpui::ScrollEvent {
                    position: list.viewport_bounds().center(),
                    delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(30.))),
                    touch_phase: gpui::TouchPhase::Moved,
                    ..Default::default()
                });
                for _ in 0..20 {
                    frame(cx);
                }
            }
            assert_eq!(list.is_following_tail(), following);
            let top = |cx: &mut VisualTestContext| {
                cx.debug_bounds("activity-row-shot")
                    .expect("the call's window is on screen")
                    .top()
            };
            let before = top(cx);

            thread.live(call(
                agent::ItemStatus::Completed,
                vec![agent::Attachment {
                    media_type: "image/png".into(),
                    data_base64: String::new(),
                    source_path: Some("/tmp/tcode-missing-shot.png".into()),
                }],
            ));
            thread.deliver(cx);
            draw(cx);
            let mut tops = vec![top(cx)];
            for _ in 0..20 {
                frame(cx);
                tops.push(top(cx));
            }
            let tile = cx
                .debug_bounds("image-read-shot-0")
                .expect("the picture is a tile");
            assert!(
                tile.bottom() <= *tops.last().unwrap(),
                "the window is below the tiles"
            );
            assert!(
                (tops[0] - before).abs() <= px(1.),
                "the window jumped from its run: {before:?} -> {tops:?}"
            );
            if following {
                assert!(
                    tops.iter().all(|top| (*top - before).abs() <= px(1.)),
                    "the window moved: {tops:?}"
                );
                continue;
            }
            let total = (*tops.last().unwrap() - tops[0]).abs();
            assert!(
                total > px(20.),
                "the window moved below the tiles: {tops:?}"
            );
            assert!(
                tops.windows(2)
                    .all(|pair| (pair[1] - pair[0]).abs() < total / 2.),
                "the window jumped: {tops:?}"
            );
            assert!(
                tops.windows(2)
                    .filter(|pair| (pair[1] - pair[0]).abs() > px(0.5))
                    .count()
                    >= 6,
                "the window travelled in too few frames: {tops:?}"
            );
        }
    }

    /// While a turn runs, its Work Log shows its newest activities; when
    /// another comes in, the oldest of them leaves into the header by
    /// folding away, so the activities after it move up over several frames
    /// instead of jumping, and the new one folds in at the end. The tail
    /// stays on the bottom edge throughout.
    #[gpui::test]
    fn the_oldest_live_activity_folds_away_into_the_header(cx: &mut TestAppContext) {
        let mut events = earlier_turns_and_a_running_one(4);
        events.extend((0..6).map(|index| lookup(&format!("live-{index}"))));
        let mut thread = ScriptedThread::new(cx, events, 0, true);
        let (chat, cx) = thread.open_at(gpui::size(px(1_024.), px(1_000.)), cx);
        thread.deliver(cx);
        for _ in 0..60 {
            frame(cx);
        }
        let list = chat.read_with(cx, |chat, _| chat.list_state.clone());
        assert!(cx.debug_bounds("worklog-header-4-live-0").is_some());
        assert!(cx.debug_bounds("activity-row-live-1").is_some());
        let next = |cx: &mut VisualTestContext| {
            cx.debug_bounds("activity-row-live-2")
                .expect("the activity after the oldest")
                .top()
        };
        let before = next(cx);

        thread.live(lookup("live-6"));
        thread.deliver(cx);
        draw(cx);
        let mut tops = vec![before, next(cx)];
        let mut leaving = 0;
        for _ in 0..20 {
            frame(cx);
            tops.push(next(cx));
            leaving += usize::from(cx.debug_bounds("activity-row-live-1").is_some());
            let status = cx
                .debug_bounds("working-status-4")
                .expect("the turn's working status");
            assert!(
                (list.viewport_bounds().bottom() - status.bottom()).abs() <= px(30.),
                "the tail left the bottom edge"
            );
        }
        assert!(list.is_following_tail());
        assert!(
            cx.debug_bounds("activity-row-live-1").is_none(),
            "the oldest activity went into the header"
        );
        assert!(leaving >= 6, "it went in {leaving} frames");
        let total = (*tops.last().unwrap() - before).abs();
        assert!(
            total > px(30.),
            "the next activity took its place: {tops:?}"
        );
        assert!(
            tops.windows(2).all(|pair| pair[1] <= pair[0] + px(0.5)),
            "the next activity moved back: {tops:?}"
        );
        assert!(
            tops.windows(2)
                .all(|pair| (pair[1] - pair[0]).abs() < total / 2.),
            "the next activity jumped: {tops:?}"
        );
    }

    /// A fold inside the followed tail is the only thing that moves it: the
    /// tail is laid out as it is between its glides, so the turn's working
    /// status stays on the bottom edge and the content above follows the
    /// fold down, frame by frame, without the glide chasing it. Here the
    /// reader shuts the newest command's output, which had opened on its own.
    #[gpui::test]
    fn a_fold_inside_the_followed_tail_moves_it_once(cx: &mut TestAppContext) {
        let mut events = earlier_turns_and_a_running_one(4);
        events.push(command("first"));
        let mut thread = ScriptedThread::new(cx, events, 0, true);
        let (chat, cx) = thread.open(cx);
        thread.deliver(cx);
        for _ in 0..20 {
            frame(cx);
        }
        thread.live(command("second"));
        thread.deliver(cx);
        // The second command's window rises in, and the first's output
        // folds shut once it has been open its while.
        for _ in 0..60 {
            frame(cx);
        }
        let list = chat.read_with(cx, |chat, _| chat.list_state.clone());
        assert!(crate::chat::tests::command_open(cx, "first").is_none());
        assert!(crate::chat::tests::command_open(cx, "second").is_some());
        let place = |cx: &mut VisualTestContext| {
            let header = cx
                .debug_bounds("activity-row-second")
                .expect("the newest command's window");
            let status = cx
                .debug_bounds("working-status-4")
                .expect("the turn's working status");
            (
                header.top(),
                list.viewport_bounds().bottom() - status.bottom(),
            )
        };
        let (open_top, edge) = place(cx);
        let header = cx.debug_bounds("activity-row-second").unwrap();
        cx.simulate_click(header.center(), gpui::Modifiers::none());
        let mut tops = Vec::new();
        for _ in 0..20 {
            frame(cx);
            let (top, gap) = place(cx);
            assert_eq!(gap, edge, "the fold moved the tail off the bottom edge");
            tops.push(top);
        }
        assert!(crate::chat::tests::command_open(cx, "second").is_none());
        let shut_top = *tops.last().unwrap();
        assert!(shut_top > open_top + px(40.), "the output folded: {tops:?}");
        assert!(
            tops.windows(2).all(|pair| pair[1] >= pair[0]),
            "the content above went back up: {tops:?}"
        );
        assert!(
            tops.windows(2).filter(|pair| pair[1] > pair[0]).count() >= 6,
            "the fold moved the content above in one step: {tops:?}"
        );
        assert_eq!(
            tops[15], shut_top,
            "the tail kept moving after its fold: {tops:?}"
        );
        assert!(list.is_following_tail());
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

    /// A computer run that gains a call is the same window growing, not a
    /// row arriving: the list keeps its rows, and the tail stays on the
    /// bottom edge without anything above moving down.
    #[gpui::test]
    fn a_computer_run_gaining_a_call_is_its_row_growing(cx: &mut TestAppContext) {
        let call = |id: &str| {
            agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                id: id.into(),
                parent_item_id: None,
                content: agent::ItemContent::ToolCall {
                    name: "mcp__tcode_computer_use__find_roots".into(),
                    input: serde_json::json!({}),
                    output: Some("@r1 window app=\"Notes\"".into()),
                    status: agent::ItemStatus::Completed,
                    image_reads: Vec::new(),
                },
            })
        };
        let mut events = earlier_turns_and_a_running_one(4);
        events.push(call("look-0"));
        let mut thread = ScriptedThread::new(cx, events, 0, true);
        let (chat, cx) = thread.open(cx);
        thread.deliver(cx);
        for _ in 0..20 {
            frame(cx);
        }
        let list = chat.read_with(cx, |chat, _| chat.list_state.clone());
        let rows = list.item_count();
        assert!(
            cx.debug_bounds("activity-row-run-look-0").is_some(),
            "a run window"
        );
        let question = rows - 2;
        let top = |cx: &mut VisualTestContext| {
            cx.debug_bounds(format!("timeline-row-{question}").leak())
                .expect("the question is on screen")
                .top()
        };
        for step in 1..4 {
            let before = top(cx);
            thread.live(call(&format!("look-{step}")));
            thread.deliver(cx);
            for _ in 0..16 {
                frame(cx);
                assert_eq!(list.item_count(), rows, "call {step} arrived as a row");
                assert!(
                    top(cx) <= before + px(0.5),
                    "call {step} moved the content above down"
                );
                let last = cx
                    .debug_bounds(format!("timeline-row-{}", rows - 1).leak())
                    .expect("the run is on screen");
                assert!(
                    (list.viewport_bounds().bottom() - last.bottom()).abs() <= px(1.),
                    "call {step}: the tail left the bottom edge"
                );
            }
        }
    }

    /// Four finished turns, then one that ran six commands and answered.
    fn turns_ending_with_a_work_log() -> Vec<agent::AgentEvent> {
        let mut events: Vec<_> = (0..4).flat_map(turn).collect();
        events.extend([
            agent::AgentEvent::TurnStarted {
                turn_id: "4".into(),
            },
            user_event("user-4", "Run the checks."),
        ]);
        events.extend((0..6).map(|index| command(&format!("check-{index}"))));
        events.extend([
            answer_event("answer-4", &"Every check passed.\n\n".repeat(6)),
            agent::AgentEvent::TurnCompleted {
                turn_id: "4".into(),
                status: agent::TurnStatus::Completed,
                usage: None,
            },
        ]);
        events
    }

    /// Where the Work Log's header and the answer's row below it start.
    fn work_log_tops(
        chat: &Entity<ChatView>,
        cx: &mut VisualTestContext,
    ) -> (gpui::Pixels, gpui::Pixels) {
        let last = chat.read_with(cx, |chat, _| chat.rows.len() - 1);
        let header = cx
            .debug_bounds("worklog-header-4-check-0")
            .expect("the Work Log's header is on screen");
        let answer = cx
            .debug_bounds(format!("timeline-row-{last}").leak())
            .expect("the answer is on screen");
        (header.top(), answer.top())
    }

    /// How a Work Log's fold moved what is below it, frame by frame: the
    /// header holds still; with motion the answer goes one way, a little
    /// each frame, never in one step; without, it is there at once.
    fn assert_folds(tops: &[(gpui::Pixels, gpui::Pixels)], down: bool, reduce: bool) {
        assert!(
            tops.iter().all(|top| (top.0 - tops[0].0).abs() <= px(0.5)),
            "reduce {reduce}: the header moved: {tops:?}"
        );
        if reduce {
            assert_eq!(tops[0], *tops.last().unwrap(), "{tops:?}");
            return;
        }
        assert!(
            tops.windows(2).all(|pair| if down {
                pair[1].1 >= pair[0].1 - px(0.5)
            } else {
                pair[1].1 <= pair[0].1 + px(0.5)
            }),
            "the answer moved back: {tops:?}"
        );
        let total = (tops.last().unwrap().1 - tops[0].1).abs();
        assert!(
            tops.windows(2)
                .all(|pair| (pair[1].1 - pair[0].1).abs() < total / 2.),
            "the answer jumped: {tops:?}"
        );
        assert!(
            tops.windows(2)
                .filter(|pair| (pair[1].1 - pair[0].1).abs() > px(0.5))
                .count()
                >= 6,
            "the fold took too few frames: {tops:?}"
        );
    }

    /// Expanding a Work Log folds its activities in under its header, and
    /// collapsing it folds them out: the header stays where the reader
    /// clicked it, the reader stays where they read, and the answer below
    /// moves with the fold. A row folding out takes no press. With motion
    /// reduced both happen at once.
    #[gpui::test]
    fn a_work_log_folds_its_rows_in_and_out_under_its_header(cx: &mut TestAppContext) {
        for reduce in [false, true] {
            let thread = ScriptedThread::new(cx, turns_ending_with_a_work_log(), 0, false);
            cx.update(|cx| cx.set_reduce_motion(reduce));
            let (chat, cx) = thread.open_at(gpui::size(px(1_024.), px(1_000.)), cx);
            thread.deliver(cx);
            draw_settled(cx);
            // The reader scrolls up a little, so the follow pauses with the
            // header well inside the viewport.
            let list = chat.read_with(cx, |chat, _| chat.list_state.clone());
            cx.simulate_event(gpui::ScrollEvent {
                position: list.viewport_bounds().center(),
                delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(40.))),
                touch_phase: gpui::TouchPhase::Moved,
                ..Default::default()
            });
            draw_settled(cx);
            assert!(!list.is_following_tail());
            let header = cx
                .debug_bounds("worklog-header-4-check-0")
                .unwrap()
                .center();
            let reading = list.logical_scroll_top();

            let closed = work_log_tops(&chat, cx);
            cx.simulate_click(header, gpui::Modifiers::none());
            draw(cx);
            let mut opening = vec![closed, work_log_tops(&chat, cx)];
            for _ in 0..16 {
                frame(cx);
                opening.push(work_log_tops(&chat, cx));
            }
            assert!(cx.debug_bounds("activity-row-check-5").is_some());
            let open = *opening.last().unwrap();
            assert!(
                open.1 > closed.1 + px(100.),
                "the activities opened: {opening:?}"
            );
            assert_folds(&opening[1..], true, reduce);
            assert!(!list.is_following_tail());
            assert_eq!(list.logical_scroll_top().item_ix, reading.item_ix);
            draw_settled(cx);
            let output_open = crate::chat::tests::command_open(cx, "check-2").is_some();

            cx.simulate_click(header, gpui::Modifiers::none());
            draw(cx);
            let mut closing = vec![work_log_tops(&chat, cx)];
            for step in 0..16 {
                if step == 3 && !reduce {
                    // A press on a row still folding away reaches nothing.
                    let leaving = cx
                        .debug_bounds("activity-row-check-2")
                        .expect("the activity is still folding away");
                    let press = gpui::point(leaving.center().x, leaving.top() + px(4.));
                    cx.simulate_click(press, gpui::Modifiers::none());
                }
                frame(cx);
                closing.push(work_log_tops(&chat, cx));
            }
            assert_folds(&closing, false, reduce);
            assert!(
                (closing.last().unwrap().1 - closed.1).abs() <= px(0.5),
                "the answer came back to its place: {closing:?}"
            );
            draw_settled(cx);
            assert!(cx.debug_bounds("activity-row-check-2").is_none());
            assert_eq!(list.logical_scroll_top().item_ix, reading.item_ix);

            cx.simulate_click(header, gpui::Modifiers::none());
            draw_settled(cx);
            assert_eq!(
                crate::chat::tests::command_open(cx, "check-2").is_some(),
                output_open,
                "the press on the folding row reached it"
            );
            cx.update(|_, cx| cx.set_reduce_motion(false));
        }
    }
}
