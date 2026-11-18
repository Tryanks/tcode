//! The thread's spine: a column of short marks at the left edge of the
//! conversation, one for each message the user sent, by which the reader
//! finds a turn and goes to it.
//!
//! A mark's length says how long its turn worked, and the marks of the
//! turns on screen stand out. A press on a mark goes to its turn; the
//! pointer resting on the column opens the messages as a list, each with
//! what its turn came to, and a press on one goes there too.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Duration;

use agent::TurnStatus;
use gpui::{
    Anchor, AnyElement, Bounds, Context, Entity, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, Render, Role, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled as _, Task, WeakEntity, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::motion::{Easing, IterationCount, Keyframe, Keyframes, Timing, animate_keyframes};
use gpui_base::{ElementExt as _, h_flex, v_flex};
use tcode_core::turn_marks::{TurnMark, turn_marks};

use super::TimelineView;
use crate::store::WorkspaceStore;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;

/// The width of the column of marks, and the inset of a mark from its left.
pub(super) const SLOT: f32 = 28.;
const INSET: f32 = 6.;

/// The height a mark takes in the column.
const PITCH: f32 = 10.;

/// A mark's length by how long its turn worked, a steer's, and the length
/// of the one the pointer is on.
const LENGTHS: [f32; 3] = [6., 10., 14.];
const STEER_LENGTH: f32 = 4.;
const POINTED_LENGTH: f32 = 18.;

/// How long a turn worked to earn each longer mark, in milliseconds.
const LONGER: [u64; 2] = [30_000, 300_000];

/// The width of the list of messages, the height of a message in it, and
/// the room the column and the list keep above and below themselves.
const LIST: f32 = 380.;
const LINE: f32 = 26.;
const MARGIN: f32 = 56.;

/// One mark: the message that opened a turn, or one put into it.
#[derive(Clone)]
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

impl Row {
    fn key(&self) -> String {
        match &self.steer {
            Some(entry) => format!("chat-spine-steer-{entry}"),
            None => format!("chat-spine-mark-{}", self.turn),
        }
    }
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
    /// Whether the list of messages is open, and where it is scrolled to:
    /// closed, it follows the reader; open, it is the reader's to scroll.
    list_open: bool,
    list_scroll: ScrollHandle,
    /// See [`Self::set_turns_in_view`].
    in_view: Option<(usize, usize)>,
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
            list_open: false,
            list_scroll: ScrollHandle::new(),
            in_view: None,
        }
    }

    pub(super) fn set_session(&mut self, session_id: Option<String>, cx: &mut Context<Self>) {
        self.session_id = session_id;
        self.held.clear();
        self.revision = None;
        self.asked = false;
        self.asking = None;
        self.in_view = None;
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

    /// The turns the timeline painted rows of, among those it holds, as
    /// each frame of it says them.
    pub(super) fn set_turns_in_view(&mut self, turns: (usize, usize), cx: &mut Context<Self>) {
        if self.in_view != Some(turns) {
            self.in_view = Some(turns);
            cx.notify();
        }
    }

    /// Goes to the message `row` marks.
    fn go(&mut self, row: &Row, cx: &mut Context<Self>) {
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
        let turn = row.turn;
        self.workspace_store.update(cx, |store, cx| {
            store.show_thread_at_turn(session_id, turn, cx);
        });
    }
}

/// A mark `length` long.
fn mark(length: f32, heavy: bool, colour: Hsla) -> gpui::Div {
    div()
        .flex_none()
        .w(px(length))
        .h(px(if heavy { 2. } else { 1. }))
        .rounded_full()
        .bg(colour)
}

impl Render for SpineView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync(cx);
        let rows: Rc<[Row]> = self.rows().into();
        let bounds = self.bounds.clone();
        let root = div().h_full().flex().flex_col().justify_center().child(
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
        let in_view = self
            .in_view
            .map(|(first, last)| (first + offset, last + offset));
        let viewed = move |row: &Row| {
            in_view.is_some_and(|(first, last)| (first..=last).contains(&row.turn))
        };
        // The last message of the turns on screen: where the reader is.
        let read = in_view
            .map_or(rows.len(), |(_, last)| {
                rows.partition_point(|row| row.turn <= last)
            })
            .saturating_sub(1);
        if !self.list_open {
            self.list_scroll.scroll_to_item(read);
        }
        // A thread with more marks than fit shows those around the reader.
        let room = (f32::from(self.bounds.get().size.height) - 2. * MARGIN).max(PITCH);
        let fit = ((room / PITCH) as usize).min(rows.len());
        let first = (read + 1)
            .saturating_sub(fit.div_ceil(2))
            .min(rows.len() - fit);

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
        let (border, muted, foreground, danger, warning) = (
            theme.border,
            theme.muted_foreground,
            theme.foreground,
            theme.danger,
            theme.warning,
        );
        let quiet = crate::motion::blend(border, muted, 0.6);
        let colour = move |row: &Row, pointed: f32| match row.status {
            Some(TurnStatus::Failed) => danger,
            Some(TurnStatus::Interrupted) => warning,
            _ => crate::motion::blend(
                if viewed(row) { foreground } else { quiet },
                foreground,
                pointed,
            ),
        };

        let mut marks: Vec<AnyElement> = Vec::with_capacity(fit);
        for row in &rows[first..first + fit] {
            let key = SharedString::from(row.key());
            let (hovered, on_hover) = crate::motion::hovered(key.clone(), window, cx);
            let pointed = self.motion.transition(
                key.clone(),
                if hovered { 1f32 } else { 0. },
                crate::material::HOVER,
                window,
                cx,
            );
            let line = mark(
                row.length + pointed * (POINTED_LENGTH - row.length),
                viewed(row) || hovered,
                colour(row, pointed),
            )
            .when_some(breath.filter(|_| row.running), |line, breath| {
                line.opacity(breath)
            });
            let (selector, target) = (row.key(), row.clone());
            marks.push(
                div()
                    .id(key)
                    .debug_selector(move || selector.clone())
                    .role(Role::Button)
                    .aria_label(row.text.clone())
                    .w(px(SLOT))
                    .h(px(PITCH))
                    .pl(px(INSET))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .on_hover(crate::motion::pointer_hover(on_hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.go(&target, cx)))
                    .child(line)
                    .into_any_element(),
            );
        }
        let column = v_flex().children(marks);

        let spine = cx.entity().downgrade();
        let scroll = self.list_scroll.clone();
        let list = crate::widgets::Popover::new("chat-spine-list")
            .open_on_hover()
            .anchor(Anchor::LeftCenter)
            .trigger_with(move |_, _, _, _| column.into_any_element())
            .p(cx.theme().tokens.spacing.xs)
            .on_open_change(cx.listener(|this, open: &bool, _, cx| {
                this.list_open = *open;
                cx.notify();
            }))
            .content(move |_, window, cx| {
                let list_hover = cx.theme().list_hover;
                let spacing = cx.theme().tokens.spacing;
                let radius = crate::material::radius_chip(cx);
                let mut list = v_flex()
                    .id("chat-spine-messages")
                    .w(px(LIST))
                    .max_h(px(room))
                    .overflow_y_scroll()
                    .track_scroll(&scroll)
                    .text_token(&cx.theme().tokens.typography.sm);
                for (index, row) in rows.iter().enumerate() {
                    // The pointer over a message fills it gradually.
                    let (fill, on_hover) = crate::motion::hover_fill(
                        crate::motion::Clock::Element,
                        SharedString::from(format!("{}-row", row.key())),
                        list_hover.opacity(0.),
                        list_hover,
                        &crate::material::HOVER.into(),
                        window,
                        cx,
                    );
                    let (spine, target) = (spine.clone(), row.clone());
                    list = list.child(
                        h_flex()
                            .id(index)
                            .flex_none()
                            .h(px(LINE))
                            .px(spacing.sm)
                            .gap(spacing.sm)
                            .items_center()
                            .rounded(radius)
                            .cursor_pointer()
                            .bg(fill)
                            .on_hover(on_hover)
                            .on_click(move |_, _, cx| {
                                _ = spine.update(cx, |spine, cx| spine.go(&target, cx));
                            })
                            .child(div().flex_none().w(px(LENGTHS[2])).child(mark(
                                row.length,
                                viewed(row),
                                colour(row, 0.),
                            )))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(foreground)
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
                list
            });
        root.child(list)
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
