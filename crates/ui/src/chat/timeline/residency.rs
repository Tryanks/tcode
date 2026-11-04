use super::*;
use std::collections::HashSet;
use std::ops::Range;

/// Before GPUI reports its exact visible range, twelve rows is comfortably
/// more than a typical chat viewport. Its list also pre-measures four viewport
/// heights, so the wider eviction band keeps warm rows resident without tying
/// Markdown lifetime to measurement.
const VIEWPORT_ROW_HINT: usize = 12;
const BUILD_MARGIN_ROWS: usize = 24;
/// Three build margins prevent back-and-forth scrolling from rebuilding the
/// same parsed documents at the edge of the warm window.
const EVICT_MARGIN_ROWS: usize = 72;
/// The composer-adjacent tail stays ready even while inspecting old rows.
const TAIL_PIN_ROWS: usize = 4;

#[derive(Clone, Debug)]
pub(super) struct MarkdownEntry {
    pub id: String,
    /// The timeline row that renders this document.
    pub row: usize,
}

pub(super) struct ResidencyInput<'a> {
    pub row_count: usize,
    pub visible_rows: Range<usize>,
    pub one_shot_row_target: Option<usize>,
    pub entries: &'a [MarkdownEntry],
    pub stream_running: bool,
    pub resident_ids: &'a HashSet<String>,
}

/// Row-only superset of entries that can affect [`decide`].
pub(super) struct ResidencyScope {
    build_rows: Range<usize>,
    keep_rows: Range<usize>,
    tail_start: usize,
    last_row: Option<usize>,
    stream_running: bool,
}

impl ResidencyScope {
    pub(super) fn new(
        row_count: usize,
        visible_rows: Range<usize>,
        one_shot_row_target: Option<usize>,
        stream_running: bool,
    ) -> Self {
        let visible_rows = one_shot_row_target
            .filter(|row| *row < row_count)
            .map(|row| row..(row + VIEWPORT_ROW_HINT).min(row_count))
            .unwrap_or(visible_rows);
        Self {
            build_rows: expand_row_window(visible_rows.clone(), BUILD_MARGIN_ROWS, row_count),
            keep_rows: expand_row_window(visible_rows, EVICT_MARGIN_ROWS, row_count),
            tail_start: row_count.saturating_sub(TAIL_PIN_ROWS),
            last_row: row_count.checked_sub(1),
            stream_running,
        }
    }

    pub(super) fn includes(&self, row: usize) -> bool {
        self.keep_rows.contains(&row) || self.pinned(row)
    }

    /// The tail rows stay resident: the composer sits under them, and a
    /// running turn streams into the last of them. The rest of a running
    /// turn is ordinary history, however long it grows.
    fn pinned(&self, row: usize) -> bool {
        self.last_row.is_some() && row >= self.tail_start
            || self.stream_running && self.last_row == Some(row)
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ResidencyDecisions {
    pub build: HashSet<String>,
    pub evict: HashSet<String>,
}

pub(super) fn decide(input: ResidencyInput<'_>) -> ResidencyDecisions {
    let scope = ResidencyScope::new(
        input.row_count,
        input.visible_rows,
        input.one_shot_row_target,
        input.stream_running,
    );

    let build = input
        .entries
        .iter()
        .filter(|entry| scope.build_rows.contains(&entry.row) || scope.pinned(entry.row))
        .map(|entry| entry.id.clone())
        .collect();
    let keep = input
        .entries
        .iter()
        .filter(|entry| scope.includes(entry.row))
        .map(|entry| entry.id.as_str())
        .collect::<HashSet<_>>();
    let evict = input
        .resident_ids
        .iter()
        .filter(|id| !keep.contains(id.as_str()))
        .cloned()
        .collect();

    ResidencyDecisions { build, evict }
}

pub(in crate::chat) fn tail_row_window(row_count: usize) -> Range<usize> {
    row_count.saturating_sub(VIEWPORT_ROW_HINT)..row_count
}

pub(in crate::chat) fn viewport_row_window(scroll_top: usize, row_count: usize) -> Range<usize> {
    scroll_top..(scroll_top + VIEWPORT_ROW_HINT).min(row_count)
}

fn expand_row_window(window: Range<usize>, margin: usize, row_count: usize) -> Range<usize> {
    window.start.min(row_count).saturating_sub(margin)
        ..window
            .end
            .min(row_count)
            .saturating_add(margin)
            .min(row_count)
}

/// Large documents are parsed away from the UI executor before becoming resident.
pub(in crate::chat) const ASYNC_MARKDOWN_THRESHOLD_BYTES: usize = 4 * 1024;

pub(in crate::chat) struct PendingMarkdownBuild {
    generation: u64,
    session_key: Option<String>,
    desired_text: String,
}

impl ChatView {
    pub(in crate::chat) fn sync_markdown_residency(
        &mut self,
        one_shot_row_target: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let running = self
            .workspace_store
            .read(cx)
            .composer_state_for(self.session_id.as_deref())
            .turn_running;
        let row_count = self.rows.len();
        let resident_ids = self.md_states.keys().cloned().collect();
        let (texts, decisions) = self
            .workspace_store
            .read(cx)
            .with_timeline_for(self.session_id.as_deref(), |timeline| {
                let scope = ResidencyScope::new(
                    row_count,
                    self.markdown_visible_rows.clone(),
                    one_shot_row_target,
                    running,
                );
                let entries = markdown_entries_for_residency(timeline, &self.rows, &scope);
                let decisions = decide(ResidencyInput {
                    row_count,
                    visible_rows: self.markdown_visible_rows.clone(),
                    one_shot_row_target,
                    entries: &entries,
                    stream_running: running,
                    resident_ids: &resident_ids,
                });
                let mut texts = Vec::new();
                let last = timeline.entries.len().checked_sub(1);
                for (index, entry) in timeline.entries.iter().enumerate() {
                    if !decisions.build.contains(&entry.id) {
                        continue;
                    }
                    let row = row_of_entry(&self.rows, index, entry.turn);
                    // A running turn's text arrives into its last entry.
                    let streaming = running && Some(index) == last;
                    // A resident message whose entry is unchanged keeps its
                    // text: it is neither copied out again nor compared.
                    let text = if self
                        .md_states
                        .get(&entry.id)
                        .is_some_and(|md| md.synced_from(entry))
                    {
                        None
                    } else {
                        let Some(text) = entry_markdown(entry) else {
                            continue;
                        };
                        Some(text)
                    };
                    texts.push((row, entry.clone(), text, streaming));
                }
                (texts, decisions)
            })
            .unwrap_or_default();
        self.md_states.retain(|id, _| !decisions.evict.contains(id));
        self.pending_md_builds
            .retain(|id, _| decisions.build.contains(id) && !decisions.evict.contains(id));

        let mut rebuilt_rows = HashSet::new();
        for (row, entry, text, streaming) in texts {
            let id = entry.id.clone();
            if let Some(md) = self.md_states.get_mut(&id) {
                if let Some(text) = text {
                    md.sync_from(&entry, text, cx);
                }
                md.set_streaming(streaming, cx);
                continue;
            }
            let Some(text) = text else {
                continue;
            };
            if let Some(pending) = self.pending_md_builds.get_mut(&id) {
                pending.desired_text = text;
            } else if text.len() > ASYNC_MARKDOWN_THRESHOLD_BYTES {
                self.spawn_markdown_build(id, text, cx);
            } else {
                let mut md = MdState::new(&text, cx);
                md.sync_from(&entry, text, cx);
                md.set_streaming(streaming, cx);
                self.md_states.insert(id, md);
                rebuilt_rows.extend(row);
            }
        }
        self.place_messages_in_selection(cx);
        let mut rebuilt_rows = rebuilt_rows
            .into_iter()
            .filter(|row| *row < self.rows.len())
            .collect::<Vec<_>>();
        rebuilt_rows.sort_unstable();
        let Some((&first, rest)) = rebuilt_rows.split_first() else {
            return;
        };
        let mut range = first..first + 1;
        for &row in rest {
            if row == range.end {
                range.end += 1;
            } else {
                // Eviction leaves this cache untouched. A lazy rebuild only
                // invalidates rebuilt rows; ListState preserves the absolute
                // scroll-top offset while it measures the parsed Markdown.
                self.list_state.remeasure_items(range);
                range = row..row + 1;
            }
        }
        self.list_state.remeasure_items(range);
    }

    /// Make each resident message the selection entry at its index in the
    /// timeline.
    fn place_messages_in_selection(&self, cx: &mut Context<Self>) {
        let places = self
            .workspace_store
            .read(cx)
            .with_timeline_for(self.session_id.as_deref(), |timeline| {
                timeline
                    .entries
                    .iter()
                    .enumerate()
                    .filter_map(|(index, entry)| {
                        let md = self.md_states.get(&entry.id)?;
                        Some((md.state.clone(), index as u64))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for (state, entry) in places {
            state.update(cx, |state, cx| {
                state.set_selection_entry(&self.selection, entry, cx)
            });
        }
    }

    pub(in crate::chat) fn markdown_row(&self, id: &str, cx: &App) -> Option<usize> {
        self.workspace_store
            .read(cx)
            .with_timeline_for(self.session_id.as_deref(), |timeline| {
                timeline
                    .entries
                    .iter()
                    .position(|entry| entry.id == id)
                    .and_then(|index| row_of_entry(&self.rows, index, timeline.entries[index].turn))
            })
            .flatten()
    }

    fn spawn_markdown_build(&mut self, id: String, text: String, cx: &mut Context<Self>) {
        self.next_md_build_generation = self.next_md_build_generation.wrapping_add(1);
        let generation = self.next_md_build_generation;
        self.pending_md_builds.insert(
            id.clone(),
            PendingMarkdownBuild {
                generation,
                session_key: self.session_key.clone(),
                desired_text: text.clone(),
            },
        );
        let parse_text = text;
        cx.spawn(async move |this, cx| {
            let parsed_text = parse_text.clone();
            let parsed = cx
                .background_executor()
                .spawn(async move { crate::markdown::parse::parse_document(&parse_text) })
                .await;
            let _ = this.update(cx, |chat, cx| {
                chat.finish_markdown_build(id, generation, parsed_text, parsed, cx);
            });
        })
        .detach();
    }

    fn finish_markdown_build(
        &mut self,
        id: String,
        generation: u64,
        parsed_text: String,
        parsed: crate::markdown::parse::ParsedDocument,
        cx: &mut Context<Self>,
    ) {
        let Some(pending) = self.pending_md_builds.get(&id) else {
            return;
        };
        if pending.generation != generation || pending.session_key != self.session_key {
            return;
        }
        let desired_text = pending.desired_text.clone();
        self.pending_md_builds.remove(&id);

        if desired_text != parsed_text && !desired_text.starts_with(&parsed_text) {
            // An edit invalidated the parse. Re-evaluate residency and kick a
            // replacement job for the latest text if it is still wanted.
            self.sync_markdown_residency(None, cx);
            return;
        }

        let mut state = MdState::from_parsed(&parsed_text, parsed, cx);
        if desired_text != parsed_text {
            state.sync(desired_text, cx);
        }
        let row = self.markdown_row(&id, cx);
        self.md_states.insert(id, state);
        self.place_messages_in_selection(cx);
        if let Some(row) = row.filter(|row| *row < self.rows.len()) {
            self.list_state.remeasure_items(row..row + 1);
        }
        cx.notify();
    }

    pub(in crate::chat) fn set_markdown_visible_rows(
        &mut self,
        visible_rows: Range<usize>,
        cx: &mut Context<Self>,
    ) {
        self.painted_tail_start = None;
        let row_count = self.rows.len();
        let visible_rows = visible_rows.start.min(row_count)..visible_rows.end.min(row_count);
        self.markdown_scroll_top = Some(visible_rows.start);
        if visible_rows == self.markdown_visible_rows {
            return;
        }
        self.markdown_visible_rows = visible_rows;
        self.sync_markdown_residency(None, cx);
        cx.notify();
    }

    pub(in crate::chat) fn sync_markdown_scroll_position(&mut self, cx: &mut Context<Self>) {
        let row_count = self.rows.len();
        let scroll_top = self.list_state.logical_scroll_top().item_ix.min(row_count);
        if self.markdown_scroll_top == Some(scroll_top) {
            return;
        }
        self.markdown_scroll_top = Some(scroll_top);
        self.markdown_visible_rows = if scroll_top == row_count {
            self.markdown_tail_window()
        } else {
            self.painted_tail_start = None;
            viewport_row_window(scroll_top, row_count)
        };
        self.sync_markdown_residency(None, cx);
    }

    /// The tail's rows: the row hint, widened to the rows last painted there.
    /// Rows only join the tail while it is followed, so those stay in place.
    pub(in crate::chat) fn markdown_tail_window(&self) -> Range<usize> {
        let tail = tail_row_window(self.rows.len());
        let start = self
            .painted_tail_start
            .map_or(tail.start, |start| start.min(tail.start));
        start..tail.end
    }

    /// Row hints only approximate the rows on screen, which can be far
    /// shorter than a hint assumes (an expanded Work Log's activities). Take
    /// the rows the list painted when the hint missed some of them. `None`
    /// when they change nothing.
    pub(in crate::chat) fn painted_rows_change(
        &self,
        painted: Range<usize>,
    ) -> Option<PaintedRowsChange> {
        let row_count = self.rows.len();
        let painted = painted.start.min(row_count)..painted.end.min(row_count);
        let tail_start = (self.list_state.is_following_tail() && painted.end == row_count)
            .then_some(painted.start);
        let window = &self.markdown_visible_rows;
        let missed = painted.start < window.start || window.end < painted.end;
        (missed || tail_start != self.painted_tail_start).then(|| PaintedRowsChange {
            tail_start,
            visible: missed.then_some(painted),
        })
    }

    pub(in crate::chat) fn adopt_painted_rows(
        &mut self,
        change: PaintedRowsChange,
        cx: &mut Context<Self>,
    ) {
        self.painted_tail_start = change.tail_start;
        if let Some(visible) = change.visible {
            self.markdown_visible_rows = visible;
            self.sync_markdown_residency(None, cx);
            cx.notify();
        }
    }
}

/// The Markdown a message renders: an assistant message's text, or a user
/// message's visible text.
fn entry_markdown(entry: &TimelineEntry) -> Option<String> {
    match &entry.content {
        EntryContent::Item(
            ItemContent::AssistantMessage { text, .. } | ItemContent::Plan { markdown: text },
        ) => Some(text.clone()),
        content => {
            let (text, _, context_len, _) = user_content(content)?;
            Some(plain_text_as_markdown(user_visible_text(text, context_len)))
        }
    }
}

/// The transcript's selection document: its entries are the messages at
/// their timeline indices, read from their resident Markdown when they have
/// one and parsed from the timeline when they are not resident.
pub(in crate::chat) fn transcript_selection(
    chat: gpui::WeakEntity<ChatView>,
    cx: &mut App,
) -> Entity<SelectionDocument> {
    cx.new(|_| {
        SelectionDocument::new(Rc::new(move |range, cx| {
            let Some(chat) = chat.upgrade() else {
                return Vec::new();
            };
            let chat = chat.read(cx);
            chat.workspace_store
                .read(cx)
                .with_timeline_for(chat.session_id.as_deref(), |timeline| {
                    let (start, end) = (*range.start() as usize, *range.end() as usize);
                    timeline
                        .entries
                        .iter()
                        .enumerate()
                        .skip(start)
                        .take(end.saturating_sub(start) + 1)
                        .filter_map(|(index, entry)| {
                            let root = match chat.md_states.get(&entry.id) {
                                Some(md) => md.state.read(cx).parsed_root(),
                                None => Rc::new(
                                    crate::markdown::parse::parse_document(&entry_markdown(entry)?)
                                        .root,
                                ),
                            };
                            Some((index as u64, root))
                        })
                        .collect()
                })
                .unwrap_or_default()
        }))
    })
}

fn markdown_entries_for_residency(
    timeline: &Timeline,
    rows: &[TimelineRow],
    scope: &ResidencyScope,
) -> Vec<MarkdownEntry> {
    let mut entries = Vec::new();
    for (index, entry) in timeline.entries.iter().enumerate() {
        let Some(row) = row_of_entry(rows, index, entry.turn) else {
            continue;
        };
        if !scope.includes(row) {
            continue;
        }
        let markdown_bearing = matches!(
            entry.content,
            EntryContent::Item(ItemContent::AssistantMessage { .. } | ItemContent::Plan { .. })
        ) || user_content(&entry.content).is_some();
        if markdown_bearing {
            entries.push(MarkdownEntry {
                id: entry.id.clone(),
                row,
            });
        }
    }
    entries
}

/// What the rows a frame painted change; see [`ChatView::painted_rows_change`].
pub(in crate::chat) struct PaintedRowsChange {
    tail_start: Option<usize>,
    visible: Option<Range<usize>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::tests::*;
    use crate::test_frames::draw_settled;
    use gpui::TestAppContext;
    use tcode_core::session::TurnMeta;

    /// A message on screen dozens of short rows above the end shows as
    /// Markdown, not as its source, when the reader comes back to the
    /// latest from the start of the thread.
    #[gpui::test]
    fn markdown_on_screen_above_an_expanded_work_log_is_built(cx: &mut TestAppContext) {
        use gpui::{FollowMode, ListOffset, Modifiers, VisualTestContext, px, size};

        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta::default()];
        timeline.entries = vec![entry("user", user_item("go"))];
        for index in 0..100 {
            timeline.entries.push(entry(
                &format!("note-{index}"),
                assistant(&format!("Note {index}.")),
            ));
        }
        // The only code block in the thread: its lines exist only once the
        // message is rendered as Markdown.
        timeline
            .entries
            .push(entry("early", assistant("```\nLooking around first.\n```")));
        for index in 0..40 {
            timeline.entries.push(command(&format!("command-{index}")));
        }
        timeline.entries.push(entry("late", assistant("Done.")));
        let (store, window_state, session_id) = seed_chat(cx, timeline);
        let (view, cx) = cx.add_window_view(|window, cx| {
            ChatView::new(store, window_state, Some(session_id), window, cx)
        });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(1_900.)));
        draw_settled(cx);
        let header = cx
            .debug_bounds("worklog-header-0-command-0")
            .expect("collapsed work log");
        cx.simulate_click(header.center(), Modifiers::default());
        draw_settled(cx);

        let list = view.read_with(cx, |chat, _| chat.list_state.clone());
        list.scroll_to(ListOffset::default());
        draw_settled(cx);
        list.set_follow_mode(FollowMode::Tail);
        draw_settled(cx);

        let line = cx
            .debug_bounds("markdown-code-line-0")
            .expect("the message on screen is rendered as Markdown");
        assert!(
            line.intersects(&list.viewport_bounds()),
            "{line:?} is not on screen"
        );
    }

    /// Scrolling to a turn far above the tail renders its messages there
    /// without moving the reader.
    #[gpui::test]
    fn a_distant_turn_renders_its_messages_under_the_reader_without_moving_them(
        cx: &mut TestAppContext,
    ) {
        use gpui::{FollowMode, ListOffset, VisualTestContext, px, size};

        // The user bubble of turn 40: three rows per synthetic turn.
        const TARGET: usize = 120;
        let mut timeline = synthetic_markdown_timeline(240);
        // The thread's only code block, in the answer below the reader: its
        // lines exist only once the message is rendered as Markdown.
        Arc::make_mut(&mut timeline.entries[TARGET + 2]).content =
            assistant("```\nAssistant 40\n```");
        let (workspace_store, window_state, session_id) = seed_chat(cx, timeline);
        let (view, cx) = cx.add_window_view(|window, cx| {
            ChatView::new(workspace_store, window_state, Some(session_id), window, cx)
        });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(700.)));
        draw_settled(cx);
        let list_state = view.read_with(cx, |chat, _| chat.list_state.clone());

        list_state.set_follow_mode(FollowMode::Normal);
        list_state.scroll_to(ListOffset {
            item_ix: TARGET,
            offset_in_item: px(7.),
        });
        draw_settled(cx);

        assert!(
            cx.debug_bounds("markdown-code-line-0")
                .is_some_and(|line| line.intersects(&list_state.viewport_bounds())),
            "the turn under the reader is not rendered as Markdown"
        );
        let scroll_top = list_state.logical_scroll_top();
        assert_eq!(scroll_top.item_ix, TARGET);
        assert_eq!(
            scroll_top.offset_in_item,
            px(7.),
            "rendering the scroll-top turn changed the list anchor"
        );
    }

    /// A message over the background-parse threshold shows its text at once
    /// and its Markdown from the frame after the parse lands, inside its row;
    /// a smaller one is Markdown from its first frame.
    #[gpui::test]
    fn large_markdown_renders_once_its_background_parse_lands(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px, size};
        for (id, text, asynchronous) in [
            ("small", "small **streaming** reply".to_string(), false),
            ("large", large_markdown("async content"), true),
        ] {
            assert_eq!(text.len() >= ASYNC_MARKDOWN_THRESHOLD_BYTES, asynchronous);
            let timeline = single_assistant_timeline(id, &text);
            let (workspace_store, window_state, session_id) = seed_chat(cx, timeline);
            // Opening the window draws a frame before any background work runs.
            let window = cx.add_window(|window, cx| {
                ChatView::new(workspace_store, window_state, Some(session_id), window, cx)
            });
            let cx = &mut VisualTestContext::from_window(window.into(), cx);
            cx.simulate_resize(size(px(1_024.), px(700.)));
            assert_eq!(
                painted_markdown_block(cx).is_some(),
                !asynchronous,
                "{id}: the first frame"
            );
            draw_settled(cx);
            let block = painted_markdown_block(cx).expect("the parsed message");
            let row = cx.debug_bounds("timeline-row-0").expect("the message row");
            assert!(
                block.top() >= row.top() && block.bottom() <= row.bottom(),
                "{id}: {block:?} spills out of its row {row:?}"
            );
        }
    }

    /// A rewrite of a message whose parse is still in flight shows the
    /// rewritten text once the parse lands, not the text it started from.
    #[gpui::test]
    fn in_flight_markdown_update_finishes_with_latest_text(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px, size};
        let original = large_markdown("original");
        // The thread's only code block: its lines show only the rewrite.
        let latest = format!("{}\n```\nedited\n```\n", large_markdown("edited"));
        let timeline = single_assistant_timeline("large", &original);
        let (workspace_store, window_state, session_id) = seed_chat(cx, timeline);
        let window = cx.add_window(|window, cx| {
            ChatView::new(
                workspace_store.clone(),
                window_state,
                Some(session_id.clone()),
                window,
                cx,
            )
        });
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        cx.simulate_resize(size(px(1_024.), px(700.)));
        assert!(
            painted_markdown_block(cx).is_none(),
            "the original's parse is in flight"
        );

        // The rewrite lands and is drawn before the parse finishes.
        workspace_store.update(cx, |store, cx| {
            store.set_session_replica_for_test(
                session_id,
                single_assistant_timeline("large", &latest),
                cx,
            );
            cx.notify();
        });
        assert!(painted_markdown_block(cx).is_none());
        draw_settled(cx);
        assert!(
            cx.debug_bounds("markdown-code-line-0").is_some(),
            "the message settled on the text its parse started from"
        );
    }

    /// A thread switched away from while a message's parse is in flight
    /// does not get that message back in the thread it switched to, even
    /// under the same item id.
    #[gpui::test]
    fn session_switch_does_not_resurrect_in_flight_markdown(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px, size};
        // The thread's only code block: its lines show only the stale text.
        let text = format!("{}\n```\nstale\n```\n", large_markdown("stale session"));
        let mut threads = seed_two_threads(cx, single_assistant_timeline("large", &text));
        let window_state = cx.new(|_| WindowState::new(false));
        let store = threads.store.clone();
        let window = cx.add_window(|window, cx| {
            ChatView::new(store, window_state, Some(threads.first.clone()), window, cx)
        });
        let view = window.root(cx).expect("the chat is the window's root");
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        cx.simulate_resize(size(px(1_024.), px(700.)));
        assert!(
            painted_markdown_block(cx).is_none(),
            "the stale message's parse is in flight"
        );

        let second = threads.second.clone();
        threads.bind(
            &view,
            &second,
            single_assistant_timeline("large", "The replacement."),
            cx,
        );
        draw_settled(cx);

        assert!(
            painted_markdown_block(cx).is_some(),
            "the replacement message"
        );
        assert!(
            cx.debug_bounds("markdown-code-line-0").is_none(),
            "the switched-away message came back"
        );
        threads.shutdown(cx);
    }

    /// A Markdown block of any message painted in the last frame.
    fn painted_markdown_block(
        cx: &mut gpui::VisualTestContext,
    ) -> Option<gpui::Bounds<gpui::Pixels>> {
        static SELECTORS: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
        SELECTORS
            .get_or_init(|| {
                (0..256)
                    .map(|ix| &*format!("markdown-block-{ix}").leak())
                    .collect()
            })
            .iter()
            .find_map(|selector| cx.debug_bounds(selector))
    }

    fn large_markdown(marker: &str) -> String {
        let mut text = format!("# {marker}\n\n");
        while text.len() <= ASYNC_MARKDOWN_THRESHOLD_BYTES {
            text.push_str("- a sufficiently substantial Markdown list item\n");
        }
        text
    }
}
