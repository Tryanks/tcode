//! Timeline rows that rise from below.
//!
//! gpui's `list` has no insert or item-height motion and gpui-base has no
//! animated list scroll. gpui-base's `MotionReveal` scales its previous
//! frame's measurement by a progress, and its measurement, like every keyed
//! state, ends when a row leaves the list's rendered window. So a row's
//! height glide is [`RowGlide`], driven by arrival stamps the timeline owns
//! (one per row identity, written only for rows that arrive live), until gpui
//! or gpui-base moves list rows themselves.
//!
//! While the reader follows the tail the list keeps its end on the
//! viewport's bottom edge, so a row whose layout height grows carries the
//! content above it upward: the row rises from below.

use std::ops::Range;

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, ContentMask, Element, ElementId, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, ParentElement as _, Pixels, Style, Styled as _,
    Window, div, point, prelude::FluentBuilder as _, px, relative, size,
};
use gpui_base::motion::{Easing, Timing};
use tcode_core::session::EntryContent;

use super::super::{ChatView, Instant};

/// How a row that arrived while the reader was looking came in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::chat) enum Arrival {
    /// After every row the list had: the row above gives up the end's
    /// padding, which the glide hands over.
    Appended,
    /// Between rows the list keeps.
    Inserted,
    /// In place of a row that went, such as an empty turn's.
    Replaced,
}

/// A row's arrival, stamped when the list learns of it.
#[derive(Clone, Copy, Debug)]
pub(in crate::chat) struct RowArrival {
    pub(in crate::chat) at: Instant,
    pub(in crate::chat) arrival: Arrival,
    /// The row is the first on screen of its turn, so the turn's trailer
    /// (its working status) arrives with it rather than moving down to it.
    pub(in crate::chat) brings_turn: bool,
}

impl RowArrival {
    /// How far in the row is, from absent to at rest, `now`.
    pub(in crate::chat) fn progress(&self, now: Instant) -> f32 {
        Timing::new(crate::edge_back::SETTLE)
            .ease(Easing::Custom(std::rc::Rc::new(
                gpui_base::animation::ease_out_cubic,
            )))
            .sample(now.saturating_duration_since(self.at))
            .directed_progress
    }

    pub(in crate::chat) fn settled(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.at) >= crate::edge_back::SETTLE
    }
}

/// What a [`RowGlide`] does this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::chat) enum Glide {
    /// Laid out as a plain child: no measurement, no clip, no frame.
    Rest,
    /// Arriving: its height is its measurement scaled by the progress, and
    /// it fades and rises in.
    Enter(f32),
    /// The live end of a running turn the reader follows: each time its
    /// `content` changes, its height glides to the new measurement. Height
    /// that changes on its own (a fold inside it, an image that loaded)
    /// passes through, so one change never moves twice.
    Tail { content: u64 },
}

/// A timeline row's content, gliding its layout height.
///
/// The same element in every [`Glide`], so what it draws keeps its element
/// ids, and with them its state, when a glide starts or ends.
pub(in crate::chat) struct RowGlide {
    id: ElementId,
    glide: Glide,
    child: AnyElement,
}

impl RowGlide {
    pub(in crate::chat) fn new(
        id: impl Into<ElementId>,
        glide: Glide,
        child: impl IntoElement,
    ) -> Self {
        let child = div()
            .w_full()
            .flex()
            .flex_col()
            .when_some(
                match glide {
                    Glide::Enter(progress) if progress < 1. => Some(progress),
                    _ => None,
                },
                |child, progress| child.opacity(progress),
            )
            .child(child)
            .into_any_element();
        Self {
            id: id.into(),
            glide,
            child,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Measured {
    /// The child's height as last laid out.
    height: Option<Pixels>,
    /// The tail's content when the glide last took aim.
    content: Option<u64>,
    /// The measurement the tail glides to: the one taken after its content
    /// last changed.
    target: Option<Pixels>,
    /// The content changed; the next measurement is the new target.
    retarget: bool,
}

pub(in crate::chat) enum GlideLayout {
    /// The child is this element's layout.
    Plain,
    /// This element is laid out at the glided height; the child is laid out
    /// on its own and clipped to it, `lift` below its top.
    Clipped { lift: Pixels },
}

impl IntoElement for RowGlide {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for RowGlide {
    type RequestLayoutState = GlideLayout;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, GlideLayout) {
        let global_id = global_id.expect("RowGlide has an id");
        let height = match self.glide {
            Glide::Rest => None,
            Glide::Enter(progress) => {
                let measured = window.with_element_state(global_id, |state, _| {
                    let state: Measured = state.unwrap_or_default();
                    (state.height, state)
                });
                Some((
                    measured.unwrap_or_default() * progress,
                    crate::motion::lift(progress),
                ))
            }
            Glide::Tail { content } => {
                let state = window.with_element_state(global_id, |state, _| {
                    let mut state: Measured = state.unwrap_or_default();
                    if state.content != Some(content) {
                        state.content = Some(content);
                        state.retarget = true;
                    }
                    (state, state)
                });
                match (state.height, state.target) {
                    (Some(measured), Some(target)) => {
                        let glided = gpui_base::motion::transition(
                            "tail",
                            target,
                            crate::motion::settle(),
                            window,
                            cx,
                        );
                        Some(((glided + measured - target).max(px(0.)), px(0.)))
                    }
                    // Nothing measured yet: the first frame lays the child
                    // out as it is and takes its measurement as the target.
                    _ => None,
                }
            }
        };
        match height {
            None => (self.child.request_layout(window, cx), GlideLayout::Plain),
            Some((height, lift)) => {
                let mut style = Style::default();
                style.size.width = relative(1.).into();
                style.size.height = height.into();
                (
                    window.request_layout(style, None, cx),
                    GlideLayout::Clipped { lift },
                )
            }
        }
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut GlideLayout,
        window: &mut Window,
        cx: &mut App,
    ) {
        let lift = match layout {
            GlideLayout::Plain => {
                self.child.prepaint(window, cx);
                if matches!(self.glide, Glide::Tail { .. }) {
                    let height = bounds.size.height;
                    window.with_element_state(
                        global_id.expect("RowGlide has an id"),
                        |state, _| {
                            let mut state: Measured = state.unwrap_or_default();
                            state.height = Some(height);
                            state.target = Some(height);
                            state.retarget = false;
                            ((), state)
                        },
                    );
                }
                return;
            }
            GlideLayout::Clipped { lift } => *lift,
        };
        let measured = self
            .child
            .layout_as_root(
                size(
                    AvailableSpace::Definite(bounds.size.width),
                    AvailableSpace::MinContent,
                ),
                window,
                cx,
            )
            .height;
        let moved =
            window.with_element_state(global_id.expect("RowGlide has an id"), |state, _| {
                let mut state: Measured = state.unwrap_or_default();
                let mut moved = state.height != Some(measured);
                state.height = Some(measured);
                if state.retarget {
                    moved |= state.target != Some(measured);
                    state.target = Some(measured);
                    state.retarget = false;
                }
                (moved, state)
            });
        if moved {
            window.request_animation_frame();
        }
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            self.child
                .prepaint_at(bounds.origin + point(px(0.), lift), window, cx);
        });
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut GlideLayout,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        match layout {
            GlideLayout::Plain => self.child.paint(window, cx),
            GlideLayout::Clipped { .. } => {
                window.with_content_mask(Some(ContentMask { bounds }), |window| {
                    self.child.paint(window, cx);
                });
            }
        }
    }
}

/// The rows a sync's splices bring in, by their index among the new rows,
/// and how each came in. The splices in `carried` hand rows already on
/// screen over to new ones, so nothing in them arrives.
pub(super) fn arrived_rows(
    splices: &[(Range<usize>, usize)],
    carried: &[Range<usize>],
    old_len: usize,
) -> Vec<(usize, Arrival)> {
    let mut arrived = Vec::new();
    let mut shift = 0isize;
    for (range, count) in splices {
        let start = range.start.saturating_add_signed(shift);
        shift += *count as isize - range.len() as isize;
        if carried.contains(range) {
            continue;
        }
        arrived.extend((0..*count).map(|offset| {
            let arrival = if offset < range.len() {
                Arrival::Replaced
            } else if range.end == old_len {
                Arrival::Appended
            } else {
                Arrival::Inserted
            };
            (start + offset, arrival)
        }));
    }
    arrived
}

impl ChatView {
    /// Stamp the rows that just arrived live, so they rise in. A reader who
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
                        // delivery bubble, and a steer as its turn's pending
                        // steer: the row is that message moving into place.
                        let on_screen = (!row.entry_range.is_empty())
                            .then(|| timeline.entries.get(row.entry_range.start))
                            .flatten()
                            .is_some_and(|entry| {
                                matches!(entry.content, EntryContent::Steer { .. })
                                    || shown
                                        .iter()
                                        .any(|key| entry.id == format!("local-user-{key}"))
                            });
                        (!on_screen).then_some((
                            row.identity,
                            RowArrival {
                                at,
                                arrival,
                                brings_turn: old_last_turn.is_none_or(|turn| row.turn > turn),
                            },
                        ))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        self.arrivals.extend(stamps);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext, px, size};
    use tcode_core::session::StoredEvent;
    use tcode_protocol::{EventEnvelope, HostMessage, ServerEvent, Topic};

    use crate::chat::ChatView;
    use crate::store::WorkspaceStore;
    use crate::test_frames::{draw, draw_settled};
    use crate::window_state::WindowState;

    /// A thread whose host is scripted: its window arrives as a snapshot,
    /// then records stream in one at a time, as a host sends them.
    struct ScriptedThread {
        store: Entity<WorkspaceStore>,
        incoming: async_channel::Sender<String>,
        records: Vec<StoredEvent>,
        /// The first record the window holds; the ones before it are history
        /// pages.
        held_from: usize,
        status: tcode_protocol::SessionStatus,
        outgoing: async_channel::Receiver<String>,
        _pump: gpui::Task<()>,
    }

    const THREAD: &str = "scripted";

    fn events_topic() -> Topic {
        Topic::SessionEvents {
            session_id: THREAD.into(),
        }
    }

    fn user(id: &str, text: &str) -> agent::AgentEvent {
        agent::AgentEvent::ItemCompleted(agent::ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content: agent::ItemContent::UserMessage {
                text: text.into(),
                context_len: None,
                attachments: Vec::new(),
            },
        })
    }

    fn answer(id: &str, text: &str) -> agent::AgentEvent {
        agent::AgentEvent::ItemCompleted(agent::ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content: agent::ItemContent::AssistantMessage {
                text: text.into(),
                summarized: false,
            },
        })
    }

    fn turn(turn: usize) -> [agent::AgentEvent; 4] {
        [
            agent::AgentEvent::TurnStarted {
                turn_id: turn.to_string(),
            },
            user(&format!("user-{turn}"), &format!("Question {turn}")),
            answer(
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
            user(&format!("user-{count}"), "Look at the build."),
        ]);
        events
    }

    impl ScriptedThread {
        /// The thread holding `events`, its window starting at record
        /// `held_from`.
        fn new(
            cx: &mut TestAppContext,
            events: Vec<agent::AgentEvent>,
            held_from: usize,
            running: bool,
        ) -> Self {
            use tcode_runtime::pipe::{HostServices, spawn_host};
            use tcode_services::store::SessionStore;

            cx.update(crate::theme::init);
            cx.update(crate::markdown::init);
            let data_root = crate::store::tests::scratch_root("chat-glide-test");
            let host = spawn_host(
                SessionStore::open_at(data_root.clone()).expect("test session store"),
                HostServices::default(),
            )
            .expect("spawn test host");
            let mut status = smol::block_on(host.update_state_for_test(|state, cx| {
                let id = state.start_draft("glide".into(), std::env::temp_dir(), cx);
                state.session_status_snapshot(&id).unwrap()
            }))
            .unwrap();
            host.shutdown_blocking().unwrap();
            let _ = std::fs::remove_dir_all(data_root);
            status.session_id = THREAD.into();
            status.draft = false;
            let mut meta = tcode_core::project::SessionMeta::new(
                agent::ProviderKind::Codex,
                std::env::temp_dir(),
                None,
            );
            meta.id = THREAD.into();

            let (to_host, outgoing) = async_channel::unbounded::<String>();
            let (incoming, from_host) = async_channel::unbounded::<String>();
            let link = tcode_client::HostLink::new(to_host, from_host);
            let executor = cx.background_executor.clone();
            let pump = cx.background_executor.spawn({
                let link = link.clone();
                async move {
                    link.pump_with_timer(|| executor.timer(Duration::from_millis(25)))
                        .await;
                }
            });
            let store = cx.new(|cx| {
                WorkspaceStore::new_attached(
                    link,
                    crate::store::WorkspaceAttachment::Local,
                    None,
                    None,
                    false,
                    cx,
                )
            });
            crate::store::tests::seed_full_scope(&store, &incoming, Vec::new(), cx);
            store.update(cx, |store, cx| store.select_session(THREAD.into(), cx));
            let mut thread = Self {
                store,
                incoming,
                records: Vec::new(),
                held_from,
                status,
                outgoing,
                _pump: pump,
            };
            thread.event(
                Topic::Settings,
                ServerEvent::SettingsSnapshot(Default::default()),
            );
            thread.event(
                Topic::Index,
                ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                    summary: Default::default(),
                    sessions: vec![meta],
                    projects: vec![],
                }),
            );
            thread.send_window(events);
            thread.set_running(running);
            thread
        }

        fn event(&self, topic: Topic, event: ServerEvent) {
            self.incoming
                .try_send(
                    tcode_protocol::encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic,
                        event,
                    }))
                    .unwrap(),
                )
                .unwrap();
        }

        fn turns(&self) -> u64 {
            self.records
                .iter()
                .filter(|record| matches!(record.event, agent::AgentEvent::TurnStarted { .. }))
                .count() as u64
        }

        /// The host's status for the thread: whether its last turn runs.
        fn set_running(&mut self, running: bool) {
            self.status.activity.turn_running = running;
            self.status.running_turn = running.then(|| tcode_core::session::RunningTurn {
                turn: self.turns() - 1,
                started_at: None,
            });
            self.event(
                Topic::SessionStatus {
                    session_id: THREAD.into(),
                },
                ServerEvent::SessionStatusReplaced(Box::new(self.status.clone())),
            );
        }

        /// The host sends its whole window, holding `events` after what it
        /// sent before: what a client folds when it opens the thread, or
        /// when it catches up on the records it missed.
        fn send_window(&mut self, events: Vec<agent::AgentEvent>) {
            self.records
                .extend(events.into_iter().map(StoredEvent::from));
            let end = self.records.len() as u64;
            let total_turns = self.turns();
            self.event(
                events_topic(),
                ServerEvent::SessionSnapshot {
                    from: self.held_from as u64,
                    end,
                    records: self.records[self.held_from..].to_vec(),
                    total: end,
                    total_turns,
                    truncated: false,
                },
            );
        }

        /// Answer the history page the client asked for with every record
        /// before the window. False when it asked for none.
        fn answer_page(&mut self) -> bool {
            use tcode_protocol::{ClientPayload, Query, QueryResponse};
            let asked = std::iter::from_fn(|| self.outgoing.try_recv().ok())
                .map(|line| tcode_protocol::decode_client_line(&line).unwrap())
                .find_map(|message| match message.payload {
                    ClientPayload::Query(Query::SessionHistoryPage { before, .. }) => {
                        Some((message.id, before))
                    }
                    _ => None,
                });
            let Some((id, before)) = asked else {
                return false;
            };
            self.held_from = 0;
            self.incoming
                .try_send(
                    tcode_protocol::encode_line(&HostMessage::QueryResult {
                        id,
                        result: Ok(QueryResponse::SessionHistoryPage {
                            records: self.records[..before as usize].to_vec(),
                            from: 0,
                            end: before,
                            truncated: false,
                        }),
                    })
                    .unwrap(),
                )
                .unwrap();
            true
        }

        /// The host records `event` and streams it, as it does while the
        /// agent works.
        fn live(&mut self, event: agent::AgentEvent) {
            let record = StoredEvent::from(event);
            self.records.push(record.clone());
            self.event(events_topic(), ServerEvent::SessionEvent(record));
        }

        /// Deliver what the host sent.
        fn deliver(&self, cx: &mut VisualTestContext) {
            cx.run_until_parked();
            self.store
                .update(cx, |store, cx| store.drain_host_events_for_test(cx));
        }

        fn open<'a>(
            &self,
            cx: &'a mut TestAppContext,
        ) -> (Entity<ChatView>, &'a mut VisualTestContext) {
            let store = self.store.clone();
            let window_state = cx.new(|_| WindowState::new(false));
            let (view, cx) =
                cx.add_window_view(|window, cx| ChatView::new(store, window_state, window, cx));
            cx.simulate_resize(size(px(1_024.), px(700.)));
            (view, cx)
        }
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

            thread.live(answer(
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

        thread.live(answer(
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
            if thread.answer_page() {
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
        thread.live(answer("answer-4", reply));
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
}
