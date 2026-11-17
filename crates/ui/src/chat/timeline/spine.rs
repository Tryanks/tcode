//! The thread's spine: a column of short marks left of the content column,
//! one for each message the user sent, by which the reader finds a turn and
//! goes to it.
//!
//! A mark's length says how long its turn worked. Under the pointer the
//! marks near it unfold into the first line of their messages, the nearest
//! with what its turn came to, and a press goes to that turn.
//!
//! Gap: gpui-base has no layout that opens a list's items by their distance
//! from the pointer, so the spine places its rows itself; how far the whole
//! is open still glides on the view's [`Owned`], and the running turn's mark
//! breathes on gpui-base's keyframes.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Duration;

use agent::TurnStatus;
use gpui::{
    AnyElement, App, Bounds, Context, Entity, InteractiveElement as _, IntoElement, MouseMoveEvent,
    ParentElement as _, Pixels, Render, ScrollEvent, SharedString, StatefulInteractiveElement as _,
    Styled as _, Task, WeakEntity, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::ElementExt as _;
use gpui_base::motion::{Easing, IterationCount, Keyframe, Keyframes, Timing, animate_keyframes};
use tcode_core::turn_marks::{TurnMark, turn_marks};

use super::TimelineView;
use crate::store::WorkspaceStore;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;

/// How long the spine takes to unfold under the pointer and to fold back.
const UNFOLD: Duration = Duration::from_millis(180);

/// How long the mark the pointer comes onto takes to become the one it is on.
const ON: Duration = Duration::from_millis(120);

/// The width the marks are centred in, and its gap to the content column.
pub(super) const SLOT: f32 = 20.;
pub(super) const SLOT_GAP: f32 = 2.;

/// A mark's height at rest and unfolded, and its second line's.
const PITCH: f32 = 10.;
const OPEN: f32 = 24.;
const SECOND: f32 = 17.;

/// How many marks away from the pointer a mark still opens.
const REACH: f32 = 2.6;

/// How far open a mark shows its message from: one less open has no room
/// for a line of text.
const LABELLED: f32 = 0.35;

/// A mark's length by how long its turn worked, a steer's, and the length
/// of the one the pointer is on.
const LENGTHS: [f32; 3] = [6., 10., 14.];
const STEER_LENGTH: f32 = 4.;
const SELECTED_LENGTH: f32 = 18.;

/// How long a turn worked to earn each longer mark, in milliseconds.
const LONGER: [u64; 2] = [30_000, 300_000];

/// The width of the unfolded messages, and the room the unfolded spine
/// keeps above and below itself.
const LABEL: f32 = 300.;
const MARGIN: f32 = 56.;

/// One mark: the message that opened a turn, or one put into it.
struct Row {
    /// The turn among every turn of the thread.
    turn: usize,
    /// The entry of a message put into the running turn.
    steer: Option<String>,
    text: SharedString,
    length: f32,
    status: Option<TurnStatus>,
    running: bool,
    summary: SharedString,
}

pub(in crate::chat) struct SpineView {
    workspace_store: Entity<WorkspaceStore>,
    timeline: WeakEntity<TimelineView>,
    session_id: Option<String>,
    motion: crate::motion::owned::Owned,
    /// The marks known of the thread, by turn among all its turns: those of
    /// the turns the client holds, and what the host said of the ones above.
    held: BTreeMap<usize, TurnMark>,
    /// The revision of the thread's items the held marks were last read at.
    revision: Option<u64>,
    /// Whether the host was asked for the turns above the held window.
    asked: bool,
    asking: Option<Task<()>>,
    bounds: Rc<Cell<Bounds<Pixels>>>,
    /// The pointer's height within the spine while it is over it, and the
    /// last it had, which the spine folds back around.
    pointer: Option<f32>,
    last_pointer: f32,
    /// The first mark shown of a thread with more than fit, and what a
    /// wheel has turned of the next step.
    first: usize,
    wheel: f32,
}

impl SpineView {
    pub(super) fn new(
        workspace_store: Entity<WorkspaceStore>,
        timeline: WeakEntity<TimelineView>,
        session_id: Option<String>,
        scene: &crate::motion::Epoch,
    ) -> Self {
        Self {
            workspace_store,
            timeline,
            session_id,
            motion: crate::motion::owned::Owned::in_scene(scene),
            held: BTreeMap::new(),
            revision: None,
            asked: false,
            asking: None,
            bounds: Rc::default(),
            pointer: None,
            last_pointer: 0.,
            first: 0,
            wheel: 0.,
        }
    }

    pub(super) fn set_session(&mut self, session_id: Option<String>, cx: &mut Context<Self>) {
        self.session_id = session_id;
        self.held.clear();
        self.revision = None;
        self.asked = false;
        self.asking = None;
        self.pointer = None;
        self.first = 0;
        cx.notify();
    }

    /// Reads the marks of the turns the client holds, once its items have
    /// changed, and asks the host for the turns above them.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let store = self.workspace_store.read(cx);
        let session_id = self.session_id.as_deref();
        let revision = store.entries_revision_for(session_id, cx);
        if revision == self.revision {
            return;
        }
        self.revision = revision;
        let offset = store.absolute_turn_for(session_id, 0);
        let above = store.history_available_for(session_id);
        let (local, turns) = store
            .with_entries_for(
                session_id,
                |timeline| (turn_marks(timeline), timeline.turns.len()),
                cx,
            )
            .unwrap_or_default();
        // The first held turn of a window with history above it may lack
        // its opening: what the host said of it stands.
        let whole_from = offset + usize::from(above);
        self.held
            .retain(|turn, _| *turn < whole_from && *turn < offset + turns);
        for mut mark in local {
            mark.turn += offset;
            if mark.turn >= whole_from || !self.held.contains_key(&mark.turn) {
                self.held.insert(mark.turn, mark);
            }
        }
        if above && !self.asked {
            self.asked = true;
            self.ask(cx);
        }
    }

    fn ask(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_id.clone() else {
            return;
        };
        let marks = self
            .workspace_store
            .update(cx, |store, cx| store.thread_turn_marks(&session_id, cx));
        self.asking = Some(cx.spawn(async move |this, cx| {
            let marks = marks.await;
            _ = this.update(cx, |this, cx| {
                this.asking = None;
                if this.session_id.as_deref() != Some(session_id.as_str()) {
                    return;
                }
                let marks = match marks {
                    Ok(marks) => marks,
                    Err(message) => {
                        this.workspace_store.update(cx, |_, cx| {
                            cx.emit(tcode_protocol::RuntimeNotification::Error(
                                tcode_protocol::RuntimeError::External(message),
                            ))
                        });
                        return;
                    }
                };
                let store = this.workspace_store.read(cx);
                let offset = store.absolute_turn_for(Some(&session_id), 0);
                let whole_from =
                    offset + usize::from(store.history_available_for(Some(&session_id)));
                for mark in marks {
                    if mark.turn < whole_from {
                        this.held.insert(mark.turn, mark);
                    }
                }
                cx.notify();
            });
        }));
    }

    fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for mark in self.held.values() {
            let length = match mark.elapsed_ms() {
                Some(elapsed) if elapsed >= LONGER[1] => LENGTHS[2],
                Some(elapsed) if elapsed >= LONGER[0] => LENGTHS[1],
                _ => LENGTHS[0],
            };
            if let Some(message) = &mark.message {
                let mut clauses = Vec::new();
                match mark.work {
                    0 => {}
                    1 => clauses.push(crate::tr!("chat.work_log_tool_one").into_owned()),
                    work => {
                        clauses.push(crate::tr!("chat.work_log_tools", count = work).into_owned())
                    }
                }
                if let Some(elapsed) = mark.elapsed_ms() {
                    clauses.push(crate::chat::model::format_elapsed_seconds(elapsed));
                }
                match mark.status {
                    Some(TurnStatus::Failed) => {
                        clauses.push(crate::tr!("chat.work_log_failed").into_owned())
                    }
                    Some(TurnStatus::Interrupted) => {
                        clauses.push(crate::tr!("chat.work_log_interrupted").into_owned())
                    }
                    Some(TurnStatus::Completed) | None => {}
                }
                rows.push(Row {
                    turn: mark.turn,
                    steer: None,
                    text: message.clone().into(),
                    length,
                    status: mark.status,
                    running: mark.running,
                    summary: clauses.join(" · ").into(),
                });
            }
            for steer in &mark.steers {
                rows.push(Row {
                    turn: mark.turn,
                    steer: Some(steer.entry_id.clone()),
                    text: steer.text.clone().into(),
                    length: STEER_LENGTH,
                    status: None,
                    running: false,
                    summary: SharedString::default(),
                });
            }
        }
        rows
    }

    /// The turns, among all the thread's, the timeline last painted rows of.
    fn turns_in_view(&self, cx: &App) -> Option<(usize, usize)> {
        let timeline = self.timeline.upgrade()?;
        let timeline = timeline.read(cx);
        let (start, end) = timeline.painted_rows.get()?;
        let first = timeline.rows.get(start)?.turn;
        let last = timeline.rows.get(end.saturating_sub(1).max(start))?.turn;
        let offset = self
            .workspace_store
            .read(cx)
            .absolute_turn_for(self.session_id.as_deref(), 0);
        Some((first + offset, last + offset))
    }

    /// Goes to the mark `nearest` of those shown from `self.first`.
    fn go(&mut self, nearest: usize, cx: &mut Context<Self>) {
        let Some(row) = self.rows().into_iter().nth(self.first + nearest) else {
            return;
        };
        let Some(session_id) = self.session_id.clone() else {
            return;
        };
        let offset = self
            .workspace_store
            .read(cx)
            .absolute_turn_for(Some(&session_id), 0);
        // A message put into a turn the client holds has a row to go to;
        // above the held window only its turn can be asked for.
        if let (Some(entry), Some(turn), Some(timeline)) = (
            &row.steer,
            row.turn.checked_sub(offset),
            self.timeline.upgrade(),
        ) {
            timeline.update(cx, |timeline, cx| {
                timeline.scroll_to_activity(turn, entry, cx)
            });
            return;
        }
        self.workspace_store.update(cx, |store, cx| {
            store.show_thread_at_turn(session_id, row.turn, cx);
        });
    }
}

fn smooth(t: f32) -> f32 {
    let t = t.clamp(0., 1.);
    t * t * (3. - 2. * t)
}

/// Where among `count` marks at rest in a spine `height` tall a pointer at
/// `pointer` is, in marks from the first, and the mark it is on.
fn focus(count: usize, height: f32, pointer: f32) -> (f32, usize) {
    let rest_top = ((height - count as f32 * PITCH) / 2.).max(0.);
    let focus = ((pointer - rest_top) / PITCH).clamp(0., count as f32);
    (focus, (focus as usize).min(count.saturating_sub(1)))
}

/// Where the marks stand: each one's height and how far it is open, and
/// the top of the first.
struct Placed {
    top: f32,
    heights: Vec<f32>,
    open: Vec<f32>,
}

/// The marks of a spine `height` tall, `presence` of the way unfolded
/// around a pointer `focus` marks from the first; `second` is how far each
/// mark's second line is in. The point under the pointer stays under it as
/// the marks open, so the mark the pointer is on is the one it was on at
/// rest.
fn place(height: f32, focus: f32, presence: f32, second: &[f32]) -> Placed {
    let count = second.len();
    let rest_top = ((height - count as f32 * PITCH) / 2.).max(0.);
    let nearest = (focus as usize).min(count.saturating_sub(1));
    let open: Vec<f32> = (0..count)
        .map(|index| {
            let away = (index as f32 + 0.5 - focus) / REACH;
            presence * (-away * away).exp()
        })
        .collect();
    let heights: Vec<f32> = (0..count)
        .map(|index| PITCH + open[index] * (OPEN - PITCH) + second[index] * SECOND)
        .collect();
    let before: f32 = heights[..nearest].iter().sum();
    let within = (focus - nearest as f32).clamp(0., 1.);
    let grown = before + within * heights.get(nearest).copied().unwrap_or(0.);
    Placed {
        top: rest_top + focus * PITCH - grown,
        heights,
        open,
    }
}

impl Render for SpineView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync(cx);
        let rows = self.rows();
        let bounds = self.bounds.clone();
        let root = div().size_full().relative().child(
            div()
                .absolute()
                .size_full()
                .on_prepaint(move |measured, _, _| bounds.set(measured)),
        );
        // One message is where the thread starts: there is nowhere to go.
        if rows.len() < 2 {
            return root;
        }
        let in_view = self.turns_in_view(cx);
        let height = f32::from(self.bounds.get().size.height);
        let room = (((height - 2. * MARGIN) / PITCH).max(1.) as usize).min(rows.len());
        let last_first = rows.len() - room;
        if self.pointer.is_none() {
            // A thread with more marks than fit shows those around the
            // reader; under the pointer they hold still.
            let read = in_view.map_or(rows.len(), |(_, last)| {
                rows.partition_point(|row| row.turn <= last)
            });
            self.first = read.saturating_sub(room.div_ceil(2)).min(last_first);
        }
        self.first = self.first.min(last_first);
        let rows = &rows[self.first..self.first + room];
        let count = rows.len();

        let presence = self.motion.transition(
            "unfold",
            if self.pointer.is_some() { 1f32 } else { 0. },
            UNFOLD,
            window,
            cx,
        );
        let pointer = self.pointer.unwrap_or(self.last_pointer);
        let (focus, nearest) = focus(count, height, pointer);
        // One mark is the one the pointer is on, and the next takes over
        // from it in time, not by how far the pointer has come: between two
        // marks neither would have room for its second line.
        let on: Vec<f32> = rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                presence
                    * self.motion.transition(
                        format!("on-{}-{}", row.turn, row.steer.as_deref().unwrap_or("")),
                        if index == nearest { 1f32 } else { 0. },
                        ON,
                        window,
                        cx,
                    )
            })
            .collect();
        let second: Vec<f32> = rows
            .iter()
            .zip(&on)
            .map(|(row, on)| if row.summary.is_empty() { 0. } else { *on })
            .collect();
        let placed = place(height, focus, presence, &second);
        let breath = rows.iter().any(|row| row.running).then(|| {
            animate_keyframes(
                "spine-breath",
                &Keyframes::try_new([
                    Keyframe::new(0., 1.),
                    Keyframe::new(0.5, 0.35).ease(Easing::EaseInOut),
                    Keyframe::new(1., 1.).ease(Easing::EaseInOut),
                ])
                .expect("ordered keyframes from rest to rest"),
                Timing::new(Duration::from_millis(1800)).iterations(IterationCount::Infinite),
                window,
                cx,
            )
            .value
        });

        let theme = cx.theme();
        let (border, muted, foreground, danger, warning, popover) = (
            theme.border,
            theme.muted_foreground,
            theme.foreground,
            theme.danger,
            theme.warning,
            theme.popover,
        );
        let quiet = crate::motion::blend(border, muted, 0.6);
        let mut marks: Vec<AnyElement> = Vec::with_capacity(count);
        let mut span: Option<(f32, f32)> = None;
        let mut y = placed.top;
        for (index, row) in rows.iter().enumerate() {
            let (open, on, row_height) = (placed.open[index], on[index], placed.heights[index]);
            let viewed = in_view.is_some_and(|(first, last)| (first..=last).contains(&row.turn));
            let colour = match row.status {
                Some(TurnStatus::Failed) => danger,
                Some(TurnStatus::Interrupted) => warning,
                _ => crate::motion::blend(if viewed { foreground } else { quiet }, foreground, on),
            };
            let first_line = PITCH + open * (OPEN - PITCH);
            if open > LABELLED {
                span = Some(span.map_or((y, y + row_height), |(start, _)| (start, y + row_height)));
            }
            let line = div()
                .flex_none()
                .w(px(row.length + on * (SELECTED_LENGTH - row.length)))
                .h(px(if viewed || on > 0.5 { 2. } else { 1. }))
                .rounded_full()
                .bg(colour)
                .when_some(breath.filter(|_| row.running), |line, breath| {
                    line.opacity(breath)
                });
            let label = (open > LABELLED).then(|| {
                div()
                    .flex_none()
                    .ml_2()
                    .w(px(LABEL))
                    .opacity(smooth((open - LABELLED) / (0.8 - LABELLED)))
                    .child(
                        div().h(px(first_line)).flex().items_center().child(
                            div()
                                .w_full()
                                .truncate()
                                .text_secondary(false, cx)
                                .text_color(crate::motion::blend(muted, foreground, on))
                                .child(row.text.clone()),
                        ),
                    )
                    .child(
                        div()
                            .h(px(row_height - first_line))
                            .overflow_hidden()
                            .truncate()
                            .text_caption(cx)
                            .text_color(muted)
                            .child(row.summary.clone()),
                    )
            });
            marks.push(
                div()
                    .absolute()
                    .left_0()
                    .top(px(y))
                    .h(px(row_height))
                    .flex()
                    .items_start()
                    .child(
                        div()
                            .flex_none()
                            .w(px(SLOT))
                            .h(px(first_line))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(line),
                    )
                    .children(label)
                    .into_any_element(),
            );
            y += row_height;
        }

        let panel = px(SLOT + 8. + LABEL + 12.);
        let backing = span.filter(|_| presence > 0.).map(|(start, end)| {
            div()
                .absolute()
                .left(px(-4.))
                .top(px(start - 6.))
                .h(px(end - start + 12.))
                .w(panel)
                .opacity(presence)
                .rounded(crate::material::radius_chip(cx))
                .bg(popover)
                .border_1()
                .border_color(border)
                .shadow_md()
        });

        let rest_top = ((height - count as f32 * PITCH) / 2.).max(0.);
        let more = last_first > 0;
        let strip = div()
            .id("chat-spine")
            .debug_selector(|| "chat-spine".into())
            .absolute()
            .left(px(-4.))
            .top(px(rest_top - 12.))
            .h(px(count as f32 * PITCH + 24.))
            .w(if self.pointer.is_some() {
                panel
            } else {
                px(SLOT + 4.)
            })
            .cursor_pointer()
            .block_mouse_except_scroll()
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                // A touch has no pointer to rest over the spine.
                if window.last_input_was_touch() {
                    return;
                }
                let y = f32::from(event.position.y - this.bounds.get().origin.y);
                if this.pointer != Some(y) {
                    this.pointer = Some(y);
                    this.last_pointer = y;
                    cx.notify();
                }
            }))
            .on_hover(crate::motion::pointer_hover(cx.listener(
                |this, hovered: &bool, _, cx| {
                    if !hovered && this.pointer.take().is_some() {
                        cx.notify();
                    }
                },
            )))
            .when(more, |strip| {
                strip.on_scroll(cx.listener(move |this, event: &ScrollEvent, window, cx| {
                    let turned = f32::from(event.delta.pixel_delta(window.line_height()).y);
                    this.wheel -= turned / PITCH;
                    let steps = this.wheel.trunc();
                    this.wheel -= steps;
                    let first = (this.first as f32 + steps).clamp(0., last_first as f32);
                    this.first = first as usize;
                    cx.stop_propagation();
                    cx.notify();
                }))
            })
            .on_click(cx.listener(move |this, _, _, cx| this.go(nearest, cx)));

        root.children(backing).children(marks).child(strip)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gpui::{Modifiers, TestAppContext, VisualTestContext, point, px};

    use super::PITCH;
    use crate::chat::tests::{ScriptedThread, THREAD, answer_event, user_event};
    use crate::test_frames::{draw, draw_settled};

    /// A press on the mark of a turn above the window the client holds,
    /// which only the host could name, reads that turn in and lands on it.
    #[gpui::test]
    fn a_press_on_the_mark_of_a_turn_above_the_held_window_goes_to_it(cx: &mut TestAppContext) {
        let events = (0..20)
            .flat_map(|turn| {
                [
                    agent::AgentEvent::TurnStarted {
                        turn_id: turn.to_string(),
                    },
                    user_event(&format!("user-{turn}"), &format!("Question {turn}")),
                    answer_event(
                        &format!("answer-{turn}"),
                        &format!("Answer {turn}, one paragraph of several.\n\n").repeat(8),
                    ),
                    agent::AgentEvent::TurnCompleted {
                        turn_id: turn.to_string(),
                        status: agent::TurnStatus::Completed,
                        usage: None,
                    },
                ]
            })
            .collect();
        let mut thread = ScriptedThread::new(cx, events, 4 * 16, false);
        let store = thread.store.clone();
        let (view, cx) = thread.open(cx);
        let settle = |thread: &mut ScriptedThread, cx: &mut VisualTestContext| {
            let pages = std::iter::from_fn(|| {
                cx.run_until_parked();
                draw(cx);
                cx.update(|window, cx| window.simulate_next_frame(cx));
                cx.run_until_parked();
                let before = thread.answer_page(8)?;
                cx.run_until_parked();
                cx.executor().advance_clock(Duration::from_millis(250));
                Some(before)
            })
            .take(100)
            .count();
            assert!(pages < 100, "the prefetch settles");
        };
        settle(&mut thread, cx);
        draw_settled(cx);
        assert!(
            store.read_with(cx, |store, _| store.absolute_turn_for(Some(THREAD), 0)) > 0,
            "the window holds the whole thread"
        );

        let spine = cx
            .debug_bounds("chat-spine")
            .expect("a thread of several messages shows its spine");
        // The strip reaches 12px above its first mark.
        let first = point(spine.center().x, spine.top() + px(12. + PITCH / 2.));
        cx.simulate_mouse_move(first, None, Modifiers::default());
        draw_settled(cx);
        cx.simulate_click(first, Modifiers::default());
        settle(&mut thread, cx);
        draw_settled(cx);

        let marked = view
            .read_with(cx, |chat, _| chat.timeline())
            .read_with(cx, |timeline, _| timeline.highlighted_turn);
        assert_eq!(marked, Some(0), "the first turn was not the one gone to");
        assert_eq!(
            store.read_with(cx, |store, _| store.absolute_turn_for(Some(THREAD), 0)),
            0,
            "the first turn was not read in"
        );
    }
}
