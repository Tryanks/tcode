//! The thread's spine: a column of short marks at the left edge of the
//! conversation, one for each turn the user opened, by which the reader
//! finds a turn and goes to it.
//!
//! A mark's length says how long its turn worked, and the marks from the
//! turn the reader is at down to the last stand out. Under the pointer the messages unfold beside
//! the column, each with what its turn came to, the one the pointer's mark
//! stands for level with that mark and after a stroke like it; they slide
//! against the pointer as it goes from mark to mark, and a press goes to the
//! message it is on.
//!
//! Gap: gpui-base has no list that keeps the item a pointer stands for level
//! with it, so the spine places its messages itself; how far they are out
//! still glides on the view's [`Owned`], and the running turn's mark
//! breathes on gpui-base's keyframes.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Duration;

use agent::TurnStatus;
use gpui::{
    AnyElement, Bounds, ClickEvent, Context, Entity, InteractiveElement as _, IntoElement,
    MouseMoveEvent, ParentElement as _, Pixels, Render, ScrollEvent, SharedString,
    StatefulInteractiveElement as _, Styled as _, Task, Window, div, prelude::FluentBuilder as _,
    px,
};
use gpui_base::motion::{Easing, IterationCount, Keyframe, Keyframes, Timing, animate_keyframes};
use gpui_base::{ElementExt as _, h_flex};
use tcode_core::turn_marks::{TurnMark, turn_marks};

use crate::store::WorkspaceStore;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;

/// How long the spine takes to unfold under the pointer and to fold back.
const UNFOLD: Duration = Duration::from_millis(180);

/// How long the messages take to slide to the mark the pointer comes onto.
const SLIDE: Duration = Duration::from_millis(140);

/// How long the pointer rests on the column before the spine unfolds: one
/// crossing it on its way elsewhere opens nothing.
const REST: Duration = Duration::from_millis(120);

/// The width of the column of marks, and the inset of a mark from its left.
pub(super) const SLOT: f32 = 28.;
const INSET: f32 = 6.;

/// The height a mark takes in the column, and a message beside it.
const PITCH: f32 = 10.;
const LINE: f32 = 26.;

/// How many messages show above and below the one level with the pointer.
const AROUND: f32 = 4.;

/// A mark's length by how long its turn worked, and the length of the one
/// the pointer is on.
const LENGTHS: [f32; 3] = [6., 10., 14.];
const POINTED_LENGTH: f32 = 18.;

/// How long a turn worked to earn each longer mark, in milliseconds.
const LONGER: [u64; 2] = [30_000, 300_000];

/// The width of the messages beside the column and the inset of their
/// edges, and the room the column keeps above and below itself.
const LABEL: f32 = 380.;
const EDGE: f32 = 4.;
const MARGIN: f32 = 56.;

/// One mark: the message that opened a turn.
struct Row {
    /// The turn among every turn of the thread.
    turn: usize,
    text: SharedString,
    length: f32,
    status: Option<TurnStatus>,
    running: bool,
    summary: SharedString,
}

pub(in crate::chat) struct SpineView {
    workspace_store: Entity<WorkspaceStore>,
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
    /// See [`Self::set_reading`].
    reading: Option<usize>,
    /// The pointer's height within the spine while it is over it, and the
    /// last it had, which the spine folds back around.
    pointer: Option<f32>,
    last_pointer: f32,
    /// Whether the pointer has rested on the column, and the wait for it.
    rested: bool,
    resting: Option<Task<()>>,
    /// The first mark shown of a thread with more than fit, and what a
    /// wheel has turned of the next step.
    first: usize,
    wheel: f32,
}

impl SpineView {
    pub(super) fn new(
        workspace_store: Entity<WorkspaceStore>,
        session_id: Option<String>,
        scene: &crate::motion::Epoch,
    ) -> Self {
        Self {
            workspace_store,
            session_id,
            motion: crate::motion::owned::Owned::in_scene(scene),
            held: BTreeMap::new(),
            revision: None,
            asked: false,
            asking: None,
            bounds: Rc::default(),
            reading: None,
            pointer: None,
            last_pointer: 0.,
            rested: false,
            resting: None,
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
        self.reading = None;
        self.leave();
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
        self.held
            .values()
            .map(|mark| {
                let elapsed = mark.elapsed_ms();
                let mut clauses = Vec::new();
                match mark.work {
                    0 => {}
                    1 => clauses.push(crate::tr!("chat.work_log_tool_one").into_owned()),
                    work => {
                        clauses.push(crate::tr!("chat.work_log_tools", count = work).into_owned())
                    }
                }
                if let Some(elapsed) = elapsed {
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
                Row {
                    turn: mark.turn,
                    text: mark.message.clone().into(),
                    length: match elapsed {
                        Some(elapsed) if elapsed >= LONGER[1] => LENGTHS[2],
                        Some(elapsed) if elapsed >= LONGER[0] => LENGTHS[1],
                        _ => LENGTHS[0],
                    },
                    status: mark.status,
                    running: mark.running,
                    summary: clauses.join(" · ").into(),
                }
            })
            .collect()
    }

    /// The turn the reader is at, among those the client holds, as each
    /// frame of the timeline says it.
    pub(super) fn set_reading(&mut self, turn: usize, cx: &mut Context<Self>) {
        if self.reading != Some(turn) {
            self.reading = Some(turn);
            cx.notify();
        }
    }

    /// Goes to the turn `turn`, among all the thread's.
    fn go(&mut self, turn: usize, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_id.clone() else {
            return;
        };
        self.workspace_store.update(cx, |store, cx| {
            store.show_thread_at_turn(session_id, turn, cx);
        });
    }

    /// The pointer is over the spine at `y` within it.
    fn point(&mut self, y: f32, cx: &mut Context<Self>) {
        if self.pointer == Some(y) {
            return;
        }
        self.pointer = Some(y);
        self.last_pointer = y;
        if !self.rested && self.resting.is_none() {
            self.resting = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(REST).await;
                _ = this.update(cx, |this, cx| {
                    this.resting = None;
                    this.rested = this.pointer.is_some();
                    cx.notify();
                });
            }));
        }
        cx.notify();
    }

    /// The pointer has left the spine.
    fn leave(&mut self) {
        self.pointer = None;
        self.rested = false;
        self.resting = None;
    }
}

fn smooth(t: f32) -> f32 {
    let t = t.clamp(0., 1.);
    t * t * (3. - 2. * t)
}

/// The top of `count` marks in a spine `height` tall.
fn column_top(count: usize, height: f32) -> f32 {
    ((height - count as f32 * PITCH) / 2.).max(0.)
}

/// Where among `count` marks in a spine `height` tall a pointer at
/// `pointer` is, in marks from the first, and the mark it is on.
fn pointed(count: usize, height: f32, pointer: f32) -> (f32, usize) {
    let at = ((pointer - column_top(count, height)) / PITCH).clamp(0., count as f32);
    (at, (at as usize).min(count.saturating_sub(1)))
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
        let offset = self
            .workspace_store
            .read(cx)
            .absolute_turn_for(self.session_id.as_deref(), 0);
        // The turn the reader is at, among all the thread's: its marks
        // and those of every turn after it stand out.
        let reading = self.reading.map(|turn| turn + offset);
        let height = f32::from(self.bounds.get().size.height);
        let fit = (((height - 2. * MARGIN) / PITCH).max(1.) as usize).min(rows.len());
        let last_first = rows.len() - fit;
        if self.pointer.is_none() {
            // A thread with more marks than fit shows those around the
            // reader; under the pointer they hold still.
            let read = reading.map_or(rows.len(), |turn| {
                rows.partition_point(|row| row.turn <= turn)
            });
            self.first = read.saturating_sub(fit.div_ceil(2));
        }
        self.first = self.first.min(last_first);
        let rows = &rows[self.first..self.first + fit];
        let count = rows.len();

        let unfolded = self.rested && self.pointer.is_some();
        let presence = self.motion.transition(
            "unfold",
            if unfolded { 1f32 } else { 0. },
            UNFOLD,
            window,
            cx,
        );
        let pointer = self.pointer.unwrap_or(self.last_pointer);
        let (_, mark) = pointed(count, height, pointer);
        // The middle of the mark the pointer is on, in marks from the
        // first, gliding from the one it was on: the messages go by it, so
        // the one the mark stands for comes to rest level with its mark.
        let at = self
            .motion
            .transition("at", mark as f32 + 0.5, SLIDE, window, cx);
        // How far each mark is from that one, in marks, and how far it is
        // the one the pointer is on.
        let away = |index: usize| index as f32 + 0.5 - at;
        let on = |index: usize| presence * smooth(1. - away(index).abs());
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
        let (border, muted, foreground, danger, warning, popover, list_hover) = (
            theme.border,
            theme.muted_foreground,
            theme.foreground,
            theme.danger,
            theme.warning,
            theme.popover,
            theme.list_hover,
        );
        let quiet = crate::motion::blend(border, muted, 0.6);
        let column_top = column_top(count, height);

        // A mark's stroke, in the column and before its message alike.
        let stroke = |row: &Row, on: f32| {
            let viewed = reading.is_some_and(|turn| row.turn >= turn);
            div()
                .flex_none()
                .w(px(row.length + on * (POINTED_LENGTH - row.length)))
                .h(px(if viewed { 2. } else { 1. + on }))
                .rounded_full()
                .bg(match row.status {
                    Some(TurnStatus::Failed) => danger,
                    Some(TurnStatus::Interrupted) => warning,
                    _ => crate::motion::blend(
                        if viewed { foreground } else { quiet },
                        foreground,
                        on,
                    ),
                })
                .when_some(breath.filter(|_| row.running), |stroke, breath| {
                    stroke.opacity(breath)
                })
        };

        // The marks keep their places in the column.
        let mut marks: Vec<AnyElement> = Vec::with_capacity(count);
        for (index, row) in rows.iter().enumerate() {
            let selector = format!("chat-spine-mark-{}", row.turn);
            marks.push(
                div()
                    .debug_selector(move || selector.clone())
                    .absolute()
                    .left_0()
                    .top(px(column_top + index as f32 * PITCH))
                    .w(px(SLOT))
                    .h(px(PITCH))
                    .pl(px(INSET))
                    .flex()
                    .items_center()
                    .child(stroke(row, on(index)))
                    .into_any_element(),
            );
        }

        // The messages unfold beside the column, each after its mark's
        // stroke, the one the pointer's mark stands for level with that
        // mark, so the two strokes read as one. A message is taller than a
        // mark, so they slide against the pointer as it goes from mark to
        // mark, and hold still while it stays on one.
        let level = column_top + at * PITCH;
        let (above, below) = (at.min(AROUND), (count as f32 - at).min(AROUND));
        let (top, bottom) = (level - above * LINE, level + below * LINE);
        let panel = (presence > 0.).then(|| {
            let mut panel = div()
                .absolute()
                .left(px(SLOT - (1. - presence) * 6.))
                .top(px(top - EDGE))
                .h(px(bottom - top + 2. * EDGE))
                .w(px(LABEL))
                .opacity(presence)
                .overflow_hidden()
                .rounded(crate::material::radius_chip(cx))
                .bg(popover)
                .border_1()
                .border_color(border)
                .shadow_md();
            for (index, row) in rows.iter().enumerate() {
                let away = away(index);
                // A message fades as it nears the edge it slides out by.
                let shown = smooth(AROUND + 0.5 - away.abs());
                if shown <= 0. {
                    continue;
                }
                let on = smooth(1. - away.abs());
                panel = panel.child(
                    h_flex()
                        .absolute()
                        .left(px(EDGE))
                        .right(px(EDGE))
                        .top(px(EDGE + (above + away - 0.5) * LINE))
                        .h(px(LINE))
                        .pr_2()
                        .gap_2()
                        .opacity(shown)
                        .rounded(crate::material::radius_chip(cx))
                        .bg(list_hover.opacity(list_hover.a * on))
                        .child(
                            div()
                                .flex_none()
                                .w(px(POINTED_LENGTH))
                                .child(stroke(row, on)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_secondary(false, cx)
                                .text_color(crate::motion::blend(muted, foreground, on))
                                .child(row.text.clone()),
                        )
                        .when(!row.summary.is_empty(), |line| {
                            line.child(
                                div()
                                    .flex_none()
                                    .text_caption(cx)
                                    .text_color(muted)
                                    .child(row.summary.clone()),
                            )
                        }),
                );
            }
            panel
        });

        // The strip takes the pointer over the column, and over the
        // messages once they are out, so going onto one does not fold them.
        let column_bottom = column_top + count as f32 * PITCH;
        let (strip_top, strip_bottom) = if unfolded {
            (top.min(column_top), bottom.max(column_bottom))
        } else {
            (column_top, column_bottom)
        };
        let strip = div()
            .id("chat-spine")
            .absolute()
            .left_0()
            .top(px(strip_top - 12.))
            .h(px(strip_bottom - strip_top + 24.))
            .w(px(if unfolded { SLOT + LABEL } else { SLOT }))
            .cursor_pointer()
            .block_mouse_except_scroll()
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                // A touch has no pointer to rest over the spine.
                if !window.last_input_was_touch() {
                    let y = f32::from(event.position.y - this.bounds.get().origin.y);
                    this.point(y, cx);
                }
            }))
            .on_hover(crate::motion::pointer_hover(cx.listener(
                |this, hovered: &bool, _, cx| {
                    if !hovered {
                        this.leave();
                        cx.notify();
                    }
                },
            )))
            .when(last_first > 0, |strip| {
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
            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                // The message level with the press.
                let spine = this.bounds.get();
                let y = f32::from(event.position().y - spine.origin.y);
                let (_, mark) = pointed(count, f32::from(spine.size.height), y);
                if let Some(row) = this.rows().get(this.first + mark) {
                    this.go(row.turn, cx);
                }
            }));

        root.children(marks).children(panel).child(strip)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gpui::{Modifiers, TestAppContext, VisualTestContext};

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

        let first = cx
            .debug_bounds("chat-spine-mark-0")
            .expect("the first turn, above the held window, has its mark");
        cx.simulate_click(first.center(), Modifiers::default());
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
