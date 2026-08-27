use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use std::path::{Path, PathBuf};

pub(crate) mod components;
mod model;
mod residency;

use crate::overlay::{Notification, OverlayExt as _};
use crate::theme::ActiveTheme as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::tooltip::Tooltip;
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use agent::{ItemContent, RewindMode};
use gpui::{
    Anchor, AnyElement, App, AppContext as _, ClickEvent, ClipboardItem, Context, Entity,
    FollowMode, InteractiveElement as _, IntoElement, ListAlignment, ListOffset, ListState,
    ParentElement as _, Render, Role, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Task, Window, div, list, prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};

use tcode_core::git::GitAction;
use tcode_core::session::{
    EntryContent, OrchestrateCallback, Timeline, TimelineEntry, parse_orchestrate_callback,
};
use tcode_core::ui::RightTab;

use crate::commit_dialog::CommitDialog;
use crate::composer::{Composer, ComposerEvent};
use crate::git::{git_action_label_key, git_hint_key};
use crate::shortcut::format_secondary_shortcut;
use crate::store::WorkspaceStore;
use crate::terminal_drawer::TerminalDrawer;
use crate::time::now_secs;
use crate::window_caption;
use crate::window_drag_area;
use crate::window_state::WindowState;

use self::components::assistant::MdState;
use self::components::command_panel::CommandPanelCache;
use self::model::{
    ListSync, Segment, TurnIndexCache, TurnListItem, TurnRenderArgs, activity_run_duration_ms,
    displayed_error_text, divergent_served_model, format_elapsed_deciseconds, latest_message_ids,
    live_activity_segment, live_edit_rows, partition_activity_run, plain_text_as_markdown,
    segment_entries, start_hub_projects, timeline_overdraw, user_content, user_visible_text,
    work_log_capsule_label, work_log_counts, work_log_outcome,
};
use self::residency::{
    MarkdownEntry, ResidencyInput, ResidencyScope, decide, tail_turn_window, viewport_turn_window,
};
pub(crate) use crate::material::{
    CHAT_CONTENT_MAX_WIDTH as CONTENT_MAX_WIDTH, CHAT_CONTENT_MIN_PADDING as CONTENT_MIN_PADDING,
};
/// Left padding on the chat header while the sidebar is collapsed, so its
/// leading control clears the native macOS traffic lights (which end near x=72
/// on macOS 26). Only applied on macOS: see `render_header`.
const TRAFFIC_LIGHT_INSET: f32 = 80.;
/// Vertical rhythm between turns. Turns are separated by space and typographic
/// hierarchy alone — there is deliberately no rule/divider under the user bubble.
const TURN_GAP: f32 = 32.;
/// Large documents are parsed away from the UI executor before becoming resident.
const ASYNC_MARKDOWN_THRESHOLD_BYTES: usize = 4 * 1024;
/// Minimum time an automatically expanded activity stays visible. The latest
/// activity remains open beyond this until another activity supersedes it.
const AUTO_ACTIVITY_MIN_VISIBILITY: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoActivityExpansion {
    Expanded {
        visible_since: Instant,
    },
    CollapsePending {
        generation: u64,
        visible_since: Instant,
    },
    Collapsed,
}

#[derive(Debug, Default)]
struct AutoActivityExpansions {
    entries_by_session: HashMap<String, HashMap<String, AutoActivityExpansion>>,
    next_generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AutoActivityObservation {
    expanded: bool,
    collapse: Option<(u64, Duration)>,
}

impl AutoActivityExpansions {
    fn observe(
        &mut self,
        session_key: &str,
        key: &str,
        enabled: bool,
        latest: bool,
        now: Instant,
    ) -> AutoActivityObservation {
        if !enabled {
            if let Some(entries) = self.entries_by_session.get_mut(session_key) {
                entries.remove(key);
            }
            return AutoActivityObservation {
                expanded: false,
                collapse: None,
            };
        }

        let current = self
            .entries_by_session
            .get(session_key)
            .and_then(|entries| entries.get(key))
            .copied();
        if latest {
            let visible_since = match current {
                Some(AutoActivityExpansion::Expanded { visible_since })
                | Some(AutoActivityExpansion::CollapsePending { visible_since, .. }) => {
                    visible_since
                }
                Some(AutoActivityExpansion::Collapsed) | None => now,
            };
            self.insert(
                session_key,
                key,
                AutoActivityExpansion::Expanded { visible_since },
            );
            return AutoActivityObservation {
                expanded: true,
                collapse: None,
            };
        }

        let visible_since = match current {
            Some(AutoActivityExpansion::Expanded { visible_since }) => visible_since,
            None => now,
            Some(AutoActivityExpansion::CollapsePending { .. }) => {
                return AutoActivityObservation {
                    expanded: true,
                    collapse: None,
                };
            }
            Some(AutoActivityExpansion::Collapsed) => {
                return AutoActivityObservation {
                    expanded: false,
                    collapse: None,
                };
            }
        };
        let remaining = AUTO_ACTIVITY_MIN_VISIBILITY
            .saturating_sub(now.saturating_duration_since(visible_since));
        if remaining.is_zero() {
            self.insert(session_key, key, AutoActivityExpansion::Collapsed);
            AutoActivityObservation {
                expanded: false,
                collapse: None,
            }
        } else {
            self.next_generation = self.next_generation.wrapping_add(1);
            let generation = self.next_generation;
            self.insert(
                session_key,
                key,
                AutoActivityExpansion::CollapsePending {
                    generation,
                    visible_since,
                },
            );
            AutoActivityObservation {
                expanded: true,
                collapse: Some((generation, remaining)),
            }
        }
    }

    fn finish_collapse(&mut self, session_key: &str, key: &str, generation: u64) -> bool {
        if matches!(
            self.entries_by_session
                .get(session_key)
                .and_then(|entries| entries.get(key)),
            Some(AutoActivityExpansion::CollapsePending {
                generation: pending,
                ..
            }) if *pending == generation
        ) {
            self.insert(session_key, key, AutoActivityExpansion::Collapsed);
            true
        } else {
            false
        }
    }

    fn insert(&mut self, session_key: &str, key: &str, state: AutoActivityExpansion) {
        self.entries_by_session
            .entry(session_key.to_string())
            .or_default()
            .insert(key.to_string(), state);
    }
}

struct PendingMarkdownBuild {
    generation: u64,
    session_key: Option<String>,
    desired_text: String,
    turn: usize,
}

pub struct ChatView {
    workspace_store: Entity<WorkspaceStore>,
    window_state: Entity<WindowState>,
    composer: Entity<Composer>,
    terminal_drawer: Entity<TerminalDrawer>,
    list_state: ListState,
    turn_items: Vec<TurnListItem>,
    turn_index_cache: TurnIndexCache,
    md_states: HashMap<String, MdState>,
    pending_md_builds: HashMap<String, PendingMarkdownBuild>,
    next_md_build_generation: u64,
    markdown_visible_turns: Range<usize>,
    markdown_scroll_top: Option<usize>,
    /// Open/closed keys for collapsibles (work logs, activity rows, cards, files).
    expanded: HashSet<String>,
    auto_activity_expansions: AutoActivityExpansions,
    command_panels: RefCell<CommandPanelCache>,
    session_key: Option<String>,
    /// Turn selected from a command-palette content hit.
    highlighted_turn: Option<usize>,
    /// 1s ticker kept alive while a turn is running (drives live "Working for Ns").
    _tick: Option<Task<()>>,
    /// Which copy button is currently showing its "Copied!" confirmation (2s):
    /// the copy target's key (`plan`, `user:<id>`, `assistant:<id>`).
    copied: Option<String>,
    _copied_task: Option<Task<()>>,
    /// The live commit dialog entity while it is open (kept alive across frames).
    commit_dialog: Option<Entity<CommitDialog>>,
    _subscriptions: Vec<Subscription>,
    #[cfg(test)]
    markdown_remeasured_turns: Vec<usize>,
}

impl ChatView {
    pub fn new(
        workspace_store: Entity<WorkspaceStore>,
        window_state: Entity<WindowState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer = cx.new(|cx| Composer::new(workspace_store.clone(), window, cx));
        let overdraw = timeline_overdraw(f32::from(window.bounds().size.height));
        let list_state = ListState::new(0, ListAlignment::Bottom, px(overdraw));
        list_state.set_follow_mode(FollowMode::Tail);
        let chat = cx.entity().downgrade();
        list_state.set_scroll_handler(move |event, window, cx| {
            let visible_turns = event.visible_range.clone();
            let chat = chat.clone();
            window.defer(cx, move |_, cx| {
                let _ = chat.update(cx, |chat, cx| {
                    chat.set_markdown_visible_turns(visible_turns, cx);
                });
            });
        });

        let subscriptions = vec![
            cx.subscribe(&composer, |this, _, event, cx| {
                let ComposerEvent::Submitted = event;
                // Re-engage tail following even if the user had scrolled up.
                this.list_state.set_follow_mode(FollowMode::Tail);
                this.list_state.scroll_to_end();
                cx.notify();
            }),
            cx.observe(&workspace_store, |this, _, cx| {
                this.sync_markdown_states(cx);
                cx.notify();
            }),
        ];
        let terminal_drawer = cx.new(|cx| TerminalDrawer::new(workspace_store.clone(), window, cx));

        let mut this = Self {
            workspace_store,
            window_state,
            composer,
            terminal_drawer,
            list_state,
            turn_items: Vec::new(),
            turn_index_cache: TurnIndexCache::default(),
            md_states: HashMap::new(),
            pending_md_builds: HashMap::new(),
            next_md_build_generation: 0,
            markdown_visible_turns: 0..0,
            markdown_scroll_top: None,
            expanded: HashSet::new(),
            auto_activity_expansions: AutoActivityExpansions::default(),
            command_panels: RefCell::new(CommandPanelCache::new()),
            session_key: None,
            highlighted_turn: None,
            _tick: None,
            copied: None,
            _copied_task: None,
            commit_dialog: None,
            _subscriptions: subscriptions,
            #[cfg(test)]
            markdown_remeasured_turns: Vec::new(),
        };
        this.sync_markdown_states(cx);
        this
    }

    /// Mirror timeline markdown text into synchronous [`MarkdownState`] entities.
    fn sync_markdown_states(&mut self, cx: &mut Context<Self>) {
        let session_key = self.workspace_store.read(cx).active_session_id();
        let session_changed = session_key != self.session_key;
        if session_changed {
            self.expanded.clear();
        }
        let (running, list_sync) = self
            .workspace_store
            .read(cx)
            .with_active_timeline(|timeline| {
                let list_sync = self.turn_index_cache.sync(
                    &mut self.turn_items,
                    &timeline.turns,
                    &timeline.entries,
                    timeline
                        .proposed_plan
                        .as_ref()
                        .map(|plan| (plan.turn, plan.item_id.as_str(), plan.markdown.as_str())),
                    &self.expanded,
                    session_changed,
                );
                (timeline.turn_running, list_sync)
            })
            .unwrap_or_else(|| {
                let list_sync = self.turn_index_cache.sync(
                    &mut self.turn_items,
                    &[],
                    &[],
                    None,
                    &self.expanded,
                    session_changed,
                );
                (false, list_sync)
            });

        let requested_turn = session_key
            .as_deref()
            .and_then(|session_id| self.workspace_store.read(cx).pending_chat_turn(session_id));
        if session_changed {
            self.md_states.clear();
            self.pending_md_builds.clear();
            self.command_panels.borrow_mut().clear();
            self.highlighted_turn = None;
            self.session_key = session_key;
            self.markdown_visible_turns = tail_turn_window(self.turn_items.len());
            self.markdown_scroll_top = Some(self.turn_items.len());
        }

        match list_sync {
            ListSync::None => {}
            ListSync::Reset { count } => {
                self.list_state.reset(count);
                if session_changed {
                    // Reset also clears stale item focus handles. A newly opened
                    // session always starts actively following its tail.
                    self.list_state.set_follow_mode(FollowMode::Tail);
                }
            }
            ListSync::Incremental { append, remeasure } => {
                if let Some(range) = append {
                    let count = range.len();
                    self.list_state.splice(range.start..range.start, count);
                }
                for index in remeasure {
                    self.list_state.remeasure_items(index..index + 1);
                }
            }
        }

        if self.list_state.is_following_tail() {
            self.markdown_visible_turns = tail_turn_window(self.turn_items.len());
            self.markdown_scroll_top = Some(self.turn_items.len());
        }

        if let Some(turn) = requested_turn.filter(|turn| *turn < self.turn_items.len()) {
            self.list_state.set_follow_mode(FollowMode::Normal);
            self.list_state.scroll_to(ListOffset {
                item_ix: turn,
                offset_in_item: px(0.),
            });
            self.highlighted_turn = Some(turn);
            self.markdown_visible_turns = viewport_turn_window(turn, self.turn_items.len());
            self.markdown_scroll_top = Some(turn);
            if let Some(session_id) = self.session_key.as_deref() {
                self.workspace_store.update(cx, |store, _cx| {
                    store.take_pending_chat_turn(session_id, turn);
                });
            }
        }

        self.sync_markdown_residency(requested_turn, cx);

        // Keep a 100ms ticker alive while a turn runs so the live elapsed timer
        // advances at decisecond precision; dropping it cancels the task.
        if running && self._tick.is_none() {
            self._tick = Some(cx.spawn(async move |this, cx| {
                loop {
                    smol::Timer::after(Duration::from_millis(100)).await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }));
        } else if !running {
            self._tick = None;
        }
    }

    fn sync_markdown_residency(
        &mut self,
        one_shot_turn_target: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let turn_count = self.turn_items.len();
        // Auto-scroll can move many rows during a drag. Do not retire any
        // participant until mouse-up; completed-selection participants remain
        // pinned individually below so copy keeps its full projection.
        let mut selection_drag_active = false;
        let mut selection_participants = HashSet::new();
        for (id, md) in &self.md_states {
            let state = md.state.read(cx);
            let selection = state.selection_handle();
            let snapshot = selection.snapshot(cx);
            selection_drag_active |= snapshot
                .as_ref()
                .is_some_and(|selection| selection.is_selecting());
            if snapshot.is_some() || selection.has_local_selection(cx) {
                selection_participants.insert(id.clone());
            }
        }
        let resident_ids = self.md_states.keys().cloned().collect();
        let (texts, decisions) = self
            .workspace_store
            .read(cx)
            .with_active_timeline(|timeline| {
                let scope = ResidencyScope::new(
                    turn_count,
                    self.markdown_visible_turns.clone(),
                    one_shot_turn_target,
                    timeline.turn_running,
                );
                let entries = markdown_entries_for_residency(timeline, &scope).entries;
                let decisions = decide(ResidencyInput {
                    turn_count,
                    visible_turns: self.markdown_visible_turns.clone(),
                    one_shot_turn_target,
                    entries: &entries,
                    stream_running: timeline.turn_running,
                    resident_ids: &resident_ids,
                    selection_participants: &selection_participants,
                    selection_drag_active,
                });
                let mut texts = Vec::new();
                for entry in &timeline.entries {
                    if !decisions.build.contains(&entry.id) {
                        continue;
                    }
                    match &entry.content {
                        EntryContent::Item(ItemContent::AssistantMessage { text })
                        | EntryContent::Item(ItemContent::Reasoning { text }) => {
                            texts.push((entry.turn, entry.id.clone(), text.clone()));
                        }
                        content => {
                            let Some((text, _, context_len, _)) = user_content(content) else {
                                continue;
                            };
                            texts.push((
                                entry.turn,
                                entry.id.clone(),
                                plain_text_as_markdown(user_visible_text(text, context_len)),
                            ));
                        }
                    }
                }
                if let Some(plan) = &timeline.proposed_plan {
                    let id = format!("plan:{}", plan.item_id);
                    if decisions.build.contains(&id) {
                        texts.push((plan.turn, id, plan.markdown.clone()));
                    }
                }
                (texts, decisions)
            })
            .unwrap_or_default();
        self.md_states.retain(|id, _| !decisions.evict.contains(id));
        self.pending_md_builds
            .retain(|id, _| decisions.build.contains(id) && !decisions.evict.contains(id));

        let mut rebuilt_turns = HashSet::new();
        for (turn, id, text) in texts {
            match self.md_states.get_mut(&id) {
                Some(md) => md.sync(text, cx),
                None if self.pending_md_builds.contains_key(&id) => {
                    let pending = self
                        .pending_md_builds
                        .get_mut(&id)
                        .expect("pending Markdown build disappeared");
                    pending.desired_text = text;
                    pending.turn = turn;
                }
                None if text.len() > ASYNC_MARKDOWN_THRESHOLD_BYTES => {
                    self.spawn_markdown_build(turn, id, text, cx);
                }
                None => {
                    self.md_states.insert(id, MdState::new(&text, cx));
                    rebuilt_turns.insert(turn);
                }
            }
        }
        let mut rebuilt_turns = rebuilt_turns
            .into_iter()
            .filter(|turn| *turn < self.turn_items.len())
            .collect::<Vec<_>>();
        rebuilt_turns.sort_unstable();
        let Some((&first, rest)) = rebuilt_turns.split_first() else {
            return;
        };
        let mut range = first..first + 1;
        for &turn in rest {
            if turn == range.end {
                range.end += 1;
            } else {
                // Eviction leaves this cache untouched. A lazy rebuild only
                // invalidates rebuilt rows; ListState preserves the absolute
                // scroll-top offset while it measures the parsed Markdown.
                self.remeasure_markdown_turns(range);
                range = turn..turn + 1;
            }
        }
        self.remeasure_markdown_turns(range);
    }

    fn spawn_markdown_build(
        &mut self,
        turn: usize,
        id: String,
        text: String,
        cx: &mut Context<Self>,
    ) {
        self.next_md_build_generation = self.next_md_build_generation.wrapping_add(1);
        let generation = self.next_md_build_generation;
        self.pending_md_builds.insert(
            id.clone(),
            PendingMarkdownBuild {
                generation,
                session_key: self.session_key.clone(),
                desired_text: text.clone(),
                turn,
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
        let turn = pending.turn;
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
        self.md_states.insert(id, state);
        if turn < self.turn_items.len() {
            self.remeasure_markdown_turns(turn..turn + 1);
        }
        cx.notify();
    }

    fn remeasure_markdown_turns(&mut self, range: Range<usize>) {
        #[cfg(test)]
        self.markdown_remeasured_turns.extend(range.clone());
        self.list_state.remeasure_items(range);
    }

    fn set_markdown_visible_turns(&mut self, visible_turns: Range<usize>, cx: &mut Context<Self>) {
        let turn_count = self.turn_items.len();
        let visible_turns = visible_turns.start.min(turn_count)..visible_turns.end.min(turn_count);
        self.markdown_scroll_top = Some(visible_turns.start);
        if visible_turns == self.markdown_visible_turns {
            return;
        }
        self.markdown_visible_turns = visible_turns;
        self.sync_markdown_residency(None, cx);
        cx.notify();
    }

    fn sync_markdown_scroll_position(&mut self, cx: &mut Context<Self>) {
        let turn_count = self.turn_items.len();
        let scroll_top = self.list_state.logical_scroll_top().item_ix.min(turn_count);
        if self.markdown_scroll_top == Some(scroll_top) {
            return;
        }
        self.markdown_scroll_top = Some(scroll_top);
        self.markdown_visible_turns = if scroll_top == turn_count {
            tail_turn_window(turn_count)
        } else {
            viewport_turn_window(scroll_top, turn_count)
        };
        self.sync_markdown_residency(None, cx);
    }

    #[cfg(test)]
    fn resident_markdown_state_count(&self) -> usize {
        self.md_states.len()
    }

    #[cfg(test)]
    fn has_resident_markdown_state(&self, id: &str) -> bool {
        self.md_states.contains_key(id)
    }

    #[cfg(test)]
    fn resident_markdown_source(&self, id: &str) -> Option<&str> {
        self.md_states.get(id).map(|md| md.synced.as_ref())
    }

    fn toggle_expanded(&mut self, turn: usize, key: &str, cx: &mut Context<Self>) {
        if !self.expanded.remove(key) {
            self.expanded.insert(key.to_string());
        }
        // Refresh the cached turn fingerprint immediately; the direct remeasure
        // below covers collapsibles whose state is intentionally not fingerprinted.
        self.sync_markdown_states(cx);
        self.list_state.remeasure_items(turn..turn + 1);
        cx.notify();
    }

    fn auto_activity_expanded(
        &mut self,
        turn: usize,
        key: &str,
        enabled: bool,
        latest: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(session_key) = self.session_key.clone() else {
            return false;
        };
        let auto = self.auto_activity_expansions.observe(
            &session_key,
            key,
            enabled,
            latest,
            Instant::now(),
        );
        if let Some((generation, delay)) = auto.collapse {
            let collapse_key = key.to_string();
            let collapse_session_key = session_key.clone();
            let timer = cx.background_executor().timer(delay);
            cx.spawn(async move |this, cx| {
                timer.await;
                let _ = this.update(cx, |this, cx| {
                    if this.auto_activity_expansions.finish_collapse(
                        &collapse_session_key,
                        &collapse_key,
                        generation,
                    ) && this.session_key.as_deref() == Some(collapse_session_key.as_str())
                    {
                        this.list_state.remeasure_items(turn..turn + 1);
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        auto.expanded
    }

    // -- turn rendering -----------------------------------------------------

    /// Render one turn as chronological messages, errors, and Work Log runs.
    ///
    /// `pinned` carries the ids of the last user / last assistant message in the
    /// whole timeline: their action rows stay visible instead of waiting for a
    /// hover, so Copy is never invisible-and-hover-only.
    fn render_turn(
        &mut self,
        args: TurnRenderArgs<'_>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (index, turn, cwd, entries, pinned) = args;
        let mut column = v_flex().w_full().gap(px(10.));

        let segmented = segment_entries(entries, turn.running);
        let segments = &segmented.flow;
        let last_activity_segment = live_activity_segment(segments, turn.running);
        // Only the turn's final assistant text is the deliverable; interim
        // notes between tool runs carry no action row.
        let last_assistant_segment = segments
            .iter()
            .rposition(|segment| matches!(segment, Segment::Assistant(_)));

        for (segment_index, segment) in segments.iter().enumerate() {
            match segment {
                Segment::Relay(entry) => {
                    let EntryContent::ProviderRelay {
                        from_provider,
                        to_provider,
                        ..
                    } = entry.content
                    else {
                        unreachable!();
                    };
                    column = column.child(components::dividers::relay_divider(
                        &entry.id,
                        from_provider,
                        to_provider,
                        cx,
                    ));
                }
                Segment::ModelChange(entry) => {
                    let EntryContent::ModelChanged { from, to, reason } = &entry.content else {
                        unreachable!();
                    };
                    column = column.child(components::dividers::model_change_divider(
                        &entry.id,
                        from.as_deref(),
                        to,
                        reason.as_deref(),
                        cx,
                    ));
                }
                Segment::ContextCompacted(entry) => {
                    column = column.child(components::dividers::context_compacted_divider(
                        &entry.id, cx,
                    ));
                }
                Segment::ActivityRun(activities) => {
                    let segment_id = activities[0].id.as_str();
                    column = column.child(self.compose_work_log(
                        (
                            index,
                            segment_id,
                            turn,
                            cwd,
                            activities,
                            last_activity_segment == Some(segment_index),
                        ),
                        cx,
                    ));
                }
                Segment::User(entry) => {
                    let (text, steering, context_len, attachments) =
                        user_content(&entry.content).expect("user segment");
                    // A child-thread callback (never annotated with a split) is a
                    // centered disclosure row, not a bubble, and carries no
                    // action row.
                    if let Some(callback) = context_len
                        .is_none()
                        .then(|| parse_orchestrate_callback(text))
                        .flatten()
                    {
                        column = column
                            .child(self.compose_callback_row(index, &entry.id, &callback, cx));
                    } else {
                        column = column.child(self.compose_user(
                            (
                                index,
                                &entry.id,
                                text,
                                cwd,
                                context_len,
                                attachments,
                                steering,
                                pinned.0 == Some(entry.id.as_str()),
                            ),
                            window,
                            cx,
                        ));
                    }
                }
                Segment::Assistant(entry) => {
                    let EntryContent::Item(ItemContent::AssistantMessage { text }) = &entry.content
                    else {
                        unreachable!();
                    };
                    let (markdown, copy_text) = self.md_states.get(&entry.id).map_or_else(
                        || (None, Arc::from(text.as_str())),
                        |md| (Some(md.state.clone()), md.synced.clone()),
                    );
                    let copy_key = format!("assistant:{}", entry.id);
                    let copied = self.copied.as_deref() == Some(copy_key.as_str());
                    let mark = copy_key;
                    column = column.child(components::assistant::assistant(
                        components::assistant::AssistantData {
                            id: &entry.id,
                            text,
                            cwd,
                            markdown,
                            pinned: pinned.1 == Some(entry.id.as_str()),
                            show_actions: !turn.running
                                && last_assistant_segment == Some(segment_index),
                            copied,
                        },
                        cx.listener(move |this, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy_text.to_string()));
                            this.mark_copied(mark.clone(), cx);
                        }),
                        cx,
                    ));
                }
                Segment::Error(entry) => {
                    let message = displayed_error_text(&entry.content);
                    let copy_key = format!("error:{}", entry.id);
                    let copied = self.copied.as_deref() == Some(copy_key.as_str());
                    let mark = copy_key;
                    let copy_text = message.to_string();
                    column = column.child(components::error_card::error_card(
                        &entry.id,
                        &message,
                        copied,
                        cx.listener(move |this, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
                            this.mark_copied(mark.clone(), cx);
                        }),
                        cx,
                    ));
                }
            }
        }

        // Proposed-plan card (the captured plan for this turn).
        if let Some((item_id, markdown)) = self
            .workspace_store
            .read(cx)
            .with_active_timeline(|timeline| {
                timeline
                    .proposed_plan
                    .as_ref()
                    .filter(|plan| plan.turn == index)
                    .map(|plan| (plan.item_id.clone(), plan.markdown.clone()))
            })
            .flatten()
        {
            column =
                column.child(self.compose_proposed_plan_card(index, &item_id, &markdown, cwd, cx));
        }

        // Changed-file evidence is the turn's settled summary: while the turn
        // still runs, the live per-file rows inside the work log carry this
        // information, so the section appears only once the turn finishes.
        let (changes, completeness) = self.workspace_store.read(cx).chat_turn_changes(index);
        if !turn.running && !changes.is_empty() {
            let more_key = format!("changed-files-more-{index}");
            let show_all = self.expanded.contains(&more_key);
            let toggle_more_key = more_key;
            let handlers = components::changed_files::ChangedFilesHandlers {
                view_diff: Box::new(cx.listener(move |this, _, _, cx| {
                    this.workspace_store
                        .update(cx, |store, cx| store.open_diff_for_turn(index, cx));
                })),
                toggle_more: Box::new(cx.listener(move |this, _, _, cx| {
                    this.toggle_expanded(index, &toggle_more_key, cx);
                })),
                open_files: changes
                    .iter()
                    .map(|change| {
                        let path = change.path.clone();
                        Box::new(cx.listener(move |this, _, _, cx| {
                            this.workspace_store.update(cx, |store, cx| {
                                store.open_diff_for_file(index, path.clone(), cx)
                            });
                        })) as components::changed_files::ClickHandler
                    })
                    .collect(),
            };
            column = column.child(components::changed_files::changed_files(
                index,
                cwd,
                &changes,
                completeness,
                show_all,
                handlers,
                cx,
            ));
        }

        // The turn's liveness, bare after everything it produced: no fold can
        // reach it, so collapsing the last disclosure never hides that we work.
        if turn.running {
            let requested_model = self.workspace_store.read(cx).chat_requested_model();
            let served_model =
                divergent_served_model(turn.served_model.as_deref(), requested_model.as_deref())
                    .map(str::to_owned);
            column = column.child(components::indicator::turn_working_indicator(
                index,
                turn.start_ts,
                served_model,
                cx,
            ));
        } else if let Some(ts) = turn.end_ts.or(entries.last().and_then(|e| e.ts)) {
            // 5. Turn timestamp row (finished turns with a known end time).
            let requested_model = self.workspace_store.read(cx).chat_requested_model();
            column = column.child(components::indicator::finished_turn_time(
                ts,
                turn,
                requested_model.as_deref(),
                cx,
            ));
        }

        // Pending steers float below every live transcript/work-log element.
        // Keeping them separate from `segments` preserves FIFO order without
        // making their request-time position look model-visible.
        for entry in segmented.pending_steers {
            let EntryContent::Steer {
                text,
                status,
                context_len,
                attachments,
            } = &entry.content
            else {
                unreachable!();
            };
            column = column.child(self.compose_user(
                (
                    index,
                    &entry.id,
                    text,
                    cwd,
                    *context_len,
                    attachments,
                    Some(*status),
                    pinned.0 == Some(entry.id.as_str()),
                ),
                window,
                cx,
            ));
        }

        column.into_any_element()
    }

    fn compose_user(
        &self,
        args: components::bubble::UserMessageArgs<'_>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (turn, entry_id, text, cwd, context_len, attachments, steering, pinned) = args;
        let context = context_len
            .filter(|len| *len <= text.len() && text.is_char_boundary(*len))
            .map(|len| &text[..len]);
        let visible = user_visible_text(text, context_len);
        let rewind = steering
            .is_none()
            .then(|| {
                let rewind_handler = |mode| {
                    let workspace_store = self.workspace_store.clone();
                    Arc::new(cx.listener(move |_, _, _, cx| {
                        workspace_store.update(cx, |store, _cx| store.rewind_turn(turn, mode));
                    })) as components::bubble::SharedClickHandler
                };
                components::bubble::native_rewind_button(
                    turn,
                    self.workspace_store.read(cx).chat_native_rewind_state(turn),
                    components::bubble::RewindHandlers {
                        files_and_conversation: rewind_handler(RewindMode::FilesAndConversation),
                        conversation: rewind_handler(RewindMode::Conversation),
                        files: rewind_handler(RewindMode::Files),
                    },
                    cx,
                )
            })
            .flatten();
        let markdown = self.md_states.get(entry_id).map(|md| md.state.clone());
        let copy_key = format!("user:{entry_id}");
        let copied = self.copied.as_deref() == Some(copy_key.as_str());
        let mark = copy_key;
        let copy_text: Arc<str> = Arc::from(visible);
        let images = attachments
            .iter()
            .map(|path| {
                let path = PathBuf::from(path);
                let title = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "image".to_string());
                Box::new(cx.listener(move |_, _, window, cx| {
                    crate::attachments::open_image_lightbox(
                        path.clone(),
                        title.clone(),
                        window,
                        cx,
                    );
                })) as components::bubble::ClickHandler
            })
            .collect();
        let bubble = components::bubble::user_bubble(
            components::bubble::BubbleData {
                entry_id,
                visible,
                cwd,
                attachments,
                steering,
                pinned,
                copied,
                markdown,
                rewind,
            },
            components::bubble::BubbleHandlers {
                copy: Box::new(cx.listener(move |this, _, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy_text.to_string()));
                    this.mark_copied(mark.clone(), cx);
                })),
                images,
            },
            window,
            cx,
        );

        let Some(context) = context else {
            return bubble;
        };
        v_flex()
            .w_full()
            .gap_2()
            .child(self.compose_disclosure(
                turn,
                format!("orchestrate-context-{entry_id}"),
                crate::tr!("chat.orchestrate_skill").into_owned().into(),
                context,
                cx,
            ))
            .child(bubble)
            .into_any_element()
    }

    fn compose_callback_row(
        &self,
        turn: usize,
        entry_id: &str,
        callback: &OrchestrateCallback,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = format!("orchestrate-callback-{entry_id}");
        let expanded = self.expanded.contains(&key);
        let toggle_key = key;
        components::disclosure::callback_row(
            entry_id,
            callback,
            expanded,
            cx.listener(move |this, _, _, cx| {
                this.toggle_expanded(turn, &toggle_key, cx);
            }),
            cx,
        )
    }

    fn compose_disclosure(
        &self,
        turn: usize,
        key: String,
        label: SharedString,
        full_text: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let expanded = self.expanded.contains(&key);
        let toggle_key = key.clone();
        components::disclosure::disclosure(
            &key,
            label,
            full_text,
            expanded,
            cx.listener(move |this, _, _, cx| {
                this.toggle_expanded(turn, &toggle_key, cx);
            }),
            cx,
        )
    }

    /// Prepare one stateless Work Log capsule.
    fn compose_work_log(
        &mut self,
        args: components::work_log::WorkLogArgs<'_>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (index, segment_id, turn, cwd, activities, is_last) = args;
        let section_key = format!("worklog-{index}-{segment_id}");
        let running = is_last && turn.running;
        let (folded, visible) = partition_activity_run(activities, running);
        let expanded = self.expanded.contains(&section_key);
        let live_reasoning_id = running
            .then(|| activities.last().copied())
            .flatten()
            .filter(|entry| {
                matches!(
                    entry.content,
                    EntryContent::Item(ItemContent::Reasoning { .. })
                )
            })
            .map(|entry| entry.id.as_str());

        let mut flow = v_flex().w_full().gap_1();
        if !folded.is_empty() {
            let segment_counts = work_log_counts(folded);
            let mut capsule_label = work_log_capsule_label(&segment_counts, folded.len());
            if capsule_label.is_empty() {
                capsule_label = crate::tr!("chat.work_log").into_owned();
            }
            let duration =
                format_elapsed_deciseconds(activity_run_duration_ms(folded, turn, is_last));
            let outcome = work_log_outcome(turn, folded, is_last);
            let rows = if expanded {
                self.compose_work_log_rows(folded, cwd, live_reasoning_id, false, cx)
            } else {
                Vec::new()
            };

            let toggle_section_key = section_key;
            flow = flow.child(components::work_log::work_log(
                components::work_log::WorkLogData {
                    index,
                    segment_id: segment_id.to_string(),
                    capsule_label,
                    duration,
                    outcome,
                    expanded,
                    running,
                    rows,
                },
                cx.listener(move |this, _, _, cx| {
                    this.toggle_expanded(index, &toggle_section_key, cx);
                }),
                cx,
            ));
        }

        if !visible.is_empty() {
            flow = flow.child(
                v_flex()
                    .w_full()
                    .gap_1()
                    .children(self.compose_work_log_rows(
                        visible,
                        cwd,
                        live_reasoning_id,
                        running,
                        cx,
                    )),
            );
        }

        flow.into_any_element()
    }

    fn compose_work_log_rows(
        &mut self,
        activities: &[&TimelineEntry],
        cwd: &Path,
        live_reasoning_id: Option<&str>,
        auto_expand: bool,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut rows = Vec::new();
        for (activity_index, entry) in activities.iter().enumerate() {
            let latest = activity_index + 1 == activities.len();
            if let EntryContent::Item(ItemContent::FileChange { changes, .. }) = &entry.content {
                let turn = entry.turn;
                for (file_index, row) in live_edit_rows(changes, cwd).iter().enumerate() {
                    let key = format!("activity-{}-file-{file_index}", entry.id);
                    let enabled = auto_expand && row.counts.is_some();
                    let expanded = self.auto_activity_expanded(turn, &key, enabled, latest, cx)
                        || self.expanded.contains(&key);
                    let toggle_key = key.clone();
                    rows.push(components::changed_files::file_edit_row(
                        &key,
                        row,
                        expanded,
                        cx.listener(move |this, _, _, cx| {
                            this.toggle_expanded(turn, &toggle_key, cx);
                        }),
                        cx,
                    ));
                }
            } else {
                rows.push(self.compose_activity_row(
                    entry,
                    false,
                    live_reasoning_id == Some(entry.id.as_str()),
                    auto_expand,
                    latest,
                    cx,
                ));
            }
        }
        rows
    }

    /// Prepare one stateless Work Log activity component.
    fn compose_activity_row(
        &mut self,
        entry: &TimelineEntry,
        compact: bool,
        live_reasoning: bool,
        auto_expand: bool,
        latest: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if matches!(
            &entry.content,
            EntryContent::Item(ItemContent::Subagent { .. })
        ) {
            return self.compose_subagent_row(entry, cx);
        }
        let key = format!("activity-{}", entry.id);
        let turn = entry.turn;
        let is_command = matches!(
            &entry.content,
            EntryContent::Item(ItemContent::CommandExecution { .. })
        );
        let auto_enabled =
            auto_expand && is_command && self.workspace_store.read(cx).live_command_panel();
        let auto_expanded = self.auto_activity_expanded(turn, &key, auto_enabled, latest, cx);
        let expanded = auto_expanded || self.expanded.contains(&key);
        let command_detail = if expanded {
            match &entry.content {
                EntryContent::Item(ItemContent::CommandExecution {
                    command, output, ..
                }) => {
                    let panel_id = entry.id.clone();
                    let on_cols_change = cx.listener(move |_this, cols: &usize, window, cx| {
                        // Fires from `on_prepaint`, i.e. while `List` holds its state
                        // borrowed; remeasuring inline would panic. Defer past the frame.
                        let cols = *cols;
                        let panel_id = panel_id.clone();
                        cx.defer_in(window, move |this, _window, cx| {
                            if this.command_panels.borrow_mut().resize(&panel_id, cols) {
                                this.list_state.remeasure_items(turn..turn + 1);
                                cx.notify();
                            }
                        });
                    });
                    Some(self.command_panels.borrow_mut().render(
                        &entry.id,
                        command,
                        output,
                        Some(Box::new(on_cols_change)),
                        cx,
                    ))
                }
                _ => None,
            }
        } else {
            None
        };
        let click_key = key;
        components::activity::activity_row_with_command_detail(
            entry,
            compact,
            live_reasoning,
            expanded,
            command_detail,
            cx.listener(move |this, _, _, cx| {
                this.toggle_expanded(turn, &click_key, cx);
            }),
            cx,
        )
    }

    fn compose_subagent_row(&self, entry: &TimelineEntry, cx: &mut Context<Self>) -> AnyElement {
        let active_id = self.workspace_store.read(cx).active_session_id();
        let mirror_id = active_id.as_deref().and_then(|active_id| {
            self.workspace_store
                .read(cx)
                .sidebar_sessions()
                .into_iter()
                .find(|meta| {
                    meta.parent_session_id.as_deref() == Some(active_id)
                        && meta.native_subagent.as_deref() == Some(entry.id.as_str())
                })
                .map(|meta| meta.id)
        });
        let on_open = mirror_id.map(|mirror_id| {
            let store = self.workspace_store.clone();
            Box::new(move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                store.update(cx, |store, _| store.select_session(mirror_id.clone()));
            }) as components::subagent::ClickHandler
        });
        components::subagent::subagent_row(entry, on_open, cx)
    }

    fn compose_proposed_plan_card(
        &self,
        turn: usize,
        item_id: &str,
        markdown: &str,
        cwd: &Path,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let long = markdown.chars().count() > 900 || markdown.lines().count() > 20;
        let collapse_key = format!("plan-card-{turn}");
        let collapsed = long && self.expanded.contains(&collapse_key);
        let markdown_state = self
            .md_states
            .get(&format!("plan:{item_id}"))
            .map(|md| md.state.clone());
        let md_copy = markdown.to_string();
        let md_download = markdown.to_string();
        let md_save = markdown.to_string();
        let copied = self.copied.as_deref() == Some("plan");
        let toggle_key = collapse_key;
        components::disclosure::proposed_plan_card(
            components::disclosure::PlanCardData {
                turn,
                markdown,
                cwd,
                markdown_state,
                collapsed,
                copied,
            },
            components::disclosure::PlanCardHandlers {
                toggle: Box::new(cx.listener(move |this, _, _, cx| {
                    this.toggle_expanded(turn, &toggle_key, cx);
                })),
                copy: Box::new(cx.listener(move |this, _, _, cx| {
                    let markdown = md_copy.clone();
                    this.workspace_store
                        .update(cx, |store, _cx| store.copy_plan(markdown));
                    this.mark_copied("plan".into(), cx);
                })),
                download: Box::new(cx.listener(move |this, _, _, cx| {
                    let markdown = md_download.clone();
                    let fallback_title = crate::tr!("plan.proposed_plan").into_owned();
                    this.workspace_store.update(cx, |store, _cx| {
                        store.download_plan(markdown, fallback_title)
                    });
                })),
                save: Box::new(cx.listener(move |this, _, _, cx| {
                    let markdown = md_save.clone();
                    this.workspace_store
                        .update(cx, |store, _cx| store.save_plan_to_workspace(markdown));
                })),
            },
            cx,
        )
    }

    /// Show the "Copied!" confirmation on the copy button identified by `key` for
    /// 2s (T3's confirmation). One at a time: a second copy re-arms the timer.
    fn mark_copied(&mut self, key: String, cx: &mut Context<Self>) {
        self.copied = Some(key.clone());
        self._copied_task = Some(cx.spawn(async move |this, cx| {
            smol::Timer::after(Duration::from_secs(2)).await;
            let _ = this.update(cx, |this, cx| {
                if this.copied.as_deref() == Some(key.as_str()) {
                    this.copied = None;
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    // -- top-level surfaces -------------------------------------------------

    fn render_header(
        &self,
        title: Option<String>,
        is_draft: bool,
        cwd: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Collapsed, the sidebar has zero width and this header starts at the
        // window's left edge. On macOS the native traffic lights sit there, so
        // the row's leading content (the sidebar toggle) is inset past them —
        // but only when the platform actually draws them: they are hidden in
        // fullscreen, and other platforms never had them.
        let collapsed = self.window_state.read(cx).sidebar_collapsed;
        let clears_traffic_lights =
            cfg!(target_os = "macos") && collapsed && !window.is_fullscreen();
        // Windows: with no right panel open this header is the window's
        // top-right corner, so it hosts the caption buttons — flush to the
        // right edge, past the header's usual inset.
        let (right_panel_open, right_tab) = self.workspace_store.read(cx).window_caption_state();
        let hosts_caption = window_caption::hosts_caption_for_state(
            window_caption::CaptionSurface::Chat,
            self.window_state.read(cx).route,
            right_panel_open,
            right_tab,
        );
        let base = h_flex()
            .flex_shrink_0()
            .h(px(52.))
            .px_4()
            .when(clears_traffic_lights, |this| {
                this.pl(px(TRAFFIC_LIGHT_INSET))
            })
            .when(hosts_caption, |this| this.pr_0())
            .gap_2()
            .items_center();

        // The sidebar toggle: the header's first control, immediately left of
        // the title. It lives here rather than in the sidebar because a
        // collapsed sidebar occupies no width at all (`crate::shell`), so a
        // control mounted inside it would have nowhere to be.
        let sidebar_toggle = Button::new("toggle-sidebar")
            .ghost()
            .small()
            .compact()
            .icon(if collapsed {
                IconName::PanelLeftOpen
            } else {
                IconName::PanelLeft
            })
            .tooltip(if collapsed {
                crate::tr!("sidebar.expand")
            } else {
                crate::tr!("sidebar.collapse")
            })
            .on_click(cx.listener(|this, _, _, cx| {
                let workspace_store = this.workspace_store.clone();
                this.window_state.update(cx, |state, cx| {
                    state.toggle_sidebar_collapsed(&workspace_store, cx)
                });
            }));

        // A draft shows a muted "New thread" label; an open thread its title;
        // nothing active shows "No active thread". The title stretch carries no
        // controls, so it doubles as the window's native drag handle where the
        // platform needs one.
        let title_el = if is_draft {
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(15.))
                .font_medium()
                .text_color(cx.theme().muted_foreground)
                .child(crate::tr!("chat.new_thread"))
        } else {
            match &title {
                Some(title) => div()
                    .flex_1()
                    // Keep a few words of the title even when the diff panel and
                    // the git/Open buttons squeeze the header; without a floor it
                    // collapses to a lone "I…".
                    .min_w(px(120.))
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_size(px(15.))
                    .font_medium()
                    .child(title.clone()),
                None => div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(15.))
                    .font_medium()
                    .text_color(cx.theme().muted_foreground)
                    .child(crate::tr!("chat.no_active_thread")),
            }
        };

        // The right-side cluster (Open split-button + panel toggles) shows for
        // any active thread, including a draft.
        let show_actions = is_draft || title.is_some();
        let panel = self.workspace_store.read(cx).chat_panel_state();
        let right_panel_open = panel.right_panel_open;
        let right_tab = panel.right_tab;
        let plan_showing = panel.plan_showing;
        let preview_showing = panel.preview_showing;
        let terminal_open = panel.terminal_open;
        let diff_showing = right_panel_open && right_tab == RightTab::Diff;
        window_drag_area("chat-header-drag", base, window, cx)
            .child(sidebar_toggle)
            .child(window_caption::drag_region(title_el))
            .when(show_actions, |this| {
                this.children(self.render_git_button(cx))
                    .children(cwd.clone().map(|cwd| self.render_open_button(cwd, cx)))
                    .child(
                        h_flex()
                            .flex_none()
                            .gap_1()
                            .child(
                                Button::new("panel-layout")
                                    .ghost()
                                    .small()
                                    .compact()
                                    .icon(IconName::PanelBottom)
                                    .selected(terminal_open)
                                    .tooltip(crate::tr!("chat.toggle_terminal"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.workspace_store
                                            .update(cx, |store, cx| store.toggle_terminal_panel(cx))
                                    })),
                            )
                            .child(
                                Button::new("plan-panel")
                                    .ghost()
                                    .small()
                                    .compact()
                                    .icon(IconName::Map)
                                    .selected(plan_showing)
                                    .tooltip(crate::tr!("chat.toggle_plan"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.workspace_store
                                            .update(cx, |store, cx| store.toggle_plan_panel(cx));
                                    })),
                            )
                            .child(
                                Button::new("preview-panel")
                                    .ghost()
                                    .small()
                                    .compact()
                                    .icon(IconName::Globe)
                                    .selected(preview_showing)
                                    .tooltip(crate::tr!("chat.toggle_preview"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.workspace_store
                                            .update(cx, |store, cx| store.toggle_preview_panel(cx));
                                    })),
                            )
                            .child(
                                Button::new("diff-panel")
                                    .ghost()
                                    .small()
                                    .compact()
                                    .icon(IconName::PanelRight)
                                    .selected(diff_showing)
                                    .tooltip(crate::tr!("chat.toggle_diff"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.workspace_store
                                            .update(cx, |store, cx| store.toggle_diff_panel(cx));
                                    })),
                            ),
                    )
            })
            // Last child, so the header's own actions keep their places to the
            // left of it.
            .children(hosts_caption.then(|| window_caption::caption_controls(window, cx)))
            .into_any_element()
    }

    /// The adaptive Git quick-action split-button (left of Open): the primary
    /// action follows the background git status (Commit / Commit & push / Push /
    /// Pull / Publish branch / Initialize Git, or a disabled status hint); the
    /// chevron lists the applicable subset. Ported from T3's `GitActionsControl`.
    fn render_git_button(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (quick, items) = self.workspace_store.read(cx).chat_git_controls()?;
        let border = cx.theme().border;

        // Main action segment.
        let label: SharedString = crate::tr!(git_action_label_key(quick.label))
            .into_owned()
            .into();
        let main_icon = quick
            .action
            .map(git_action_icon)
            .unwrap_or_else(|| Icon::empty().path("icons/git-branch.svg"));
        let main_base = if quick.disabled {
            h_flex()
                .id("git-main")
                .role(Role::Button)
                .aria_label(label.clone())
        } else {
            crate::material::accessible_clickable(
                h_flex(),
                "git-main",
                Role::Button,
                label.clone(),
                cx,
            )
        };
        let mut main = main_base
            .h_full()
            .px_2()
            .gap_1p5()
            .items_center()
            .text_size(px(13.))
            .child(main_icon.xsmall().text_color(if quick.disabled {
                cx.theme().muted_foreground
            } else {
                cx.theme().foreground
            }))
            .child(label);
        if quick.disabled {
            main = main.text_color(cx.theme().muted_foreground);
            if let Some(hint) = quick.hint {
                let text: SharedString = crate::tr!(git_hint_key(hint)).into_owned().into();
                main = main.tooltip(move |window, cx| Tooltip::new(text.clone()).build(window, cx));
            }
        } else if let Some(action) = quick.action {
            main = main
                .cursor_pointer()
                .hover(|s| s.bg(cx.theme().accent))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.trigger_git_action(action, window, cx);
                }));
        }

        // Dropdown listing the applicable subset. Menu rows dispatch through the
        // ChatView entity (the popover content runs at App level, not in a view
        // context, so `cx.listener` is unavailable here).
        let chat = cx.entity();
        let chevron = crate::material::overlay_popover("git-menu")
            .anchor(Anchor::TopRight)
            .trigger(
                Button::new("git-menu-trigger")
                    .ghost()
                    .compact()
                    .icon(IconName::ChevronDown),
            )
            .content(move |_state, _window, cx| {
                let muted = cx.theme().muted_foreground;
                let accent = cx.theme().accent;
                let popover = cx.entity();
                let mut menu = v_flex().w(px(210.)).p_1().gap_0p5();
                for (index, item) in items.clone().into_iter().enumerate() {
                    let label: SharedString = crate::tr!(git_action_label_key(item.action))
                        .into_owned()
                        .into();
                    let action = item.action;
                    let disabled = item.disabled;
                    let popover = popover.clone();
                    let chat = chat.clone();
                    let mut row = h_flex()
                        .id(("git-menu-item", index))
                        .w_full()
                        .px_2()
                        .py_1p5()
                        .gap_2()
                        .items_center()
                        .rounded(px(6.))
                        .text_size(px(13.))
                        .child(git_action_icon(action).xsmall().text_color(muted))
                        .child(div().flex_1().child(label));
                    if disabled {
                        row = row.text_color(muted);
                        if let Some(hint) = item.hint {
                            let text: SharedString =
                                crate::tr!(git_hint_key(hint)).into_owned().into();
                            row = row.tooltip(move |window, cx| {
                                Tooltip::new(text.clone()).build(window, cx)
                            });
                        }
                    } else {
                        row = row.cursor_pointer().hover(move |s| s.bg(accent)).on_click(
                            move |_, window, cx| {
                                popover.update(cx, |st, cx| st.dismiss(window, cx));
                                chat.update(cx, |this, cx| {
                                    this.trigger_git_action(action, window, cx)
                                });
                            },
                        );
                    }
                    menu = menu.child(row);
                }
                menu.into_any_element()
            });

        Some(
            h_flex()
                .flex_none()
                .h(px(28.))
                .items_center()
                .rounded(px(8.))
                .border_1()
                .border_color(border)
                .overflow_hidden()
                .child(main)
                .child(div().w_px().h(px(16.)).bg(border))
                .child(chevron)
                .into_any_element(),
        )
    }

    /// Dispatch a git quick-action: commit-style actions open the commit dialog;
    /// everything else runs in the background with a progress toast.
    fn trigger_git_action(
        &mut self,
        action: GitAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if action.opens_commit_dialog() {
            self.open_commit_dialog(action, window, cx);
        } else {
            self.workspace_store.update(cx, |store, _cx| {
                store.run_git_action(action, None, None, None)
            });
        }
    }

    /// Open the commit dialog for `action` (Commit or Commit & push).
    fn open_commit_dialog(
        &mut self,
        action: GitAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dialog =
            cx.new(|cx| CommitDialog::new(self.workspace_store.clone(), action, window, cx));
        self.commit_dialog = Some(dialog.clone());
        window.open_dialog(cx, move |dlg, window, cx| {
            let content = dialog.clone();
            let footer_dialog = dialog.clone();
            dlg.title(crate::tr!("git.commit.title").into_owned())
                .w(px(600.))
                // Opaque T3 panel over the library's translucent default.
                .bg(cx.theme().popover)
                .shadow_xl()
                .content(move |content_el, _window, _cx| content_el.child(content.clone()))
                .footer(render_commit_footer(&footer_dialog, window, cx))
        });
    }

    /// The bordered "Open" split-button: main click opens the session cwd in
    /// Zed; the chevron opens a menu (Zed / Finder / Copy path). Matches T3's
    /// header control.
    fn render_open_button(&self, cwd: PathBuf, cx: &mut Context<Self>) -> AnyElement {
        let border = cx.theme().border;
        let main_cwd = cwd.clone();
        let menu_cwd = cwd;

        let chevron = crate::material::overlay_popover("open-menu")
            .anchor(Anchor::TopRight)
            .trigger(
                Button::new("open-menu-trigger")
                    .ghost()
                    .compact()
                    .icon(IconName::ChevronDown),
            )
            .content(move |_state, _window, cx| {
                let zed_cwd = menu_cwd.clone();
                let reveal_cwd = menu_cwd.clone();
                let copy_cwd = menu_cwd.clone();
                let popover = cx.entity();
                let p1 = popover.clone();
                let p2 = popover.clone();
                let p3 = popover.clone();
                let muted = cx.theme().muted_foreground;
                let accent = cx.theme().accent;
                let menu_item = move |id: &'static str, icon: IconName, label: SharedString| {
                    h_flex()
                        .id(id)
                        .w_full()
                        .px_2()
                        .py_1p5()
                        .gap_2()
                        .items_center()
                        .rounded(px(6.))
                        .cursor_pointer()
                        .text_size(px(13.))
                        .hover(move |s| s.bg(accent))
                        .child(Icon::new(icon).xsmall().text_color(muted))
                        .child(label)
                };
                v_flex()
                    .w(px(180.))
                    .p_1()
                    .gap_0p5()
                    .child(
                        menu_item(
                            "open-zed",
                            IconName::ExternalLink,
                            crate::tr!("chat.open_zed").into_owned().into(),
                        )
                        .on_click(move |_, window, cx| {
                            open_in_zed(&zed_cwd, window, cx);
                            p1.update(cx, |st, cx| st.dismiss(window, cx));
                        }),
                    )
                    .child(
                        menu_item(
                            "reveal-in-file-manager",
                            IconName::FolderOpen,
                            crate::tr!("chat.reveal_in_file_manager")
                                .into_owned()
                                .into(),
                        )
                        .on_click(move |_, window, cx| {
                            cx.reveal_path(&reveal_cwd);
                            p2.update(cx, |st, cx| st.dismiss(window, cx));
                        }),
                    )
                    .child(
                        menu_item(
                            "copy-path",
                            IconName::Copy,
                            crate::tr!("chat.copy_path").into_owned().into(),
                        )
                        .on_click(move |_, window, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(
                                copy_cwd.display().to_string(),
                            ));
                            p3.update(cx, |st, cx| st.dismiss(window, cx));
                        }),
                    )
                    .into_any_element()
            });

        h_flex()
            .flex_none()
            .h(px(28.))
            .items_center()
            .rounded(px(8.))
            .border_1()
            .border_color(border)
            .overflow_hidden()
            .child(
                crate::material::accessible_clickable(
                    h_flex(),
                    "open-main",
                    Role::Button,
                    crate::tr!("chat.open"),
                    cx,
                )
                .h_full()
                .px_2()
                .gap_1p5()
                .items_center()
                .cursor_pointer()
                .text_size(px(13.))
                .hover(|s| s.bg(cx.theme().accent))
                .child(
                    Icon::new(IconName::ExternalLink)
                        .xsmall()
                        .text_color(cx.theme().muted_foreground),
                )
                .child(crate::tr!("chat.open"))
                .on_click(cx.listener(move |_, _, window, cx| {
                    open_in_zed(&main_cwd, window, cx);
                })),
            )
            .child(div().w_px().h(px(16.)).bg(border))
            .child(chevron)
            .into_any_element()
    }

    fn render_empty_state(&self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let projects = self.workspace_store.read(cx).projects();
        let sessions = self.workspace_store.read(cx).sidebar_sessions();
        let hub_projects = start_hub_projects(&projects, &sessions);
        let add_project = Button::new("add-project-empty")
            .ghost()
            .small()
            .icon(
                Icon::empty()
                    .path("icons/folder-plus.svg")
                    .text_color(cx.theme().muted_foreground),
            )
            .label(crate::tr!("sidebar.add_project"))
            .on_click(cx.listener(|this, _, window, cx| {
                crate::add_project_dialog::open(this.workspace_store.clone(), window, cx);
            }));

        let mut content = v_flex()
            .w_full()
            .max_w(px(420.))
            .px_4()
            .items_center()
            .gap_3()
            .child(
                div()
                    .text_size(px(15.))
                    .font_semibold()
                    .child(crate::tr!("chat.empty_title")),
            );
        if projects.is_empty() {
            content = content
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(cx.theme().muted_foreground)
                        .child(crate::tr!("chat.empty_description")),
                )
                .child(add_project);
        } else {
            let mut launcher = v_flex().w_full().gap_1().child(
                div()
                    .px_3()
                    .text_size(px(10.5))
                    .font_medium()
                    .text_color(cx.theme().muted_foreground)
                    .child(crate::material::tracked_uppercase(
                        crate::tr!("chat.start_hub_title").as_ref(),
                    )),
            );
            for (project, last_activity) in hub_projects {
                let project_id = project.id.clone();
                let cwd = project.root.clone();
                let row_label =
                    crate::tr!("sidebar.project", name = project.name.clone()).into_owned();
                launcher = launcher.child(
                    crate::material::accessible_clickable(
                        h_flex(),
                        SharedString::from(format!("start-hub-project-{}", project.id)),
                        Role::Button,
                        row_label,
                        cx,
                    )
                    .h(px(40.))
                    .items_center()
                    .gap_2()
                    .px_3()
                    .rounded(cx.theme().radius)
                    .cursor_pointer()
                    .hover(|row| row.bg(cx.theme().accent))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.workspace_store.update(cx, |store, _cx| {
                            store.start_draft(project_id.clone(), cwd.clone());
                        });
                    }))
                    .child(
                        Icon::new(IconName::Folder)
                            .size_4()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(13.))
                            .text_color(cx.theme().foreground)
                            .child(project.name),
                    )
                    .when_some(last_activity, |row, last_activity| {
                        row.child(
                            div()
                                .flex_none()
                                .text_size(px(11.))
                                .text_color(cx.theme().muted_foreground)
                                .child(crate::time::humanize_ago(
                                    now_secs().saturating_sub(last_activity),
                                )),
                        )
                    }),
                );
            }
            content = content.child(launcher).child(
                h_flex()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(add_project)
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(cx.theme().muted_foreground)
                            .child(crate::tr!(
                                "chat.palette_hint",
                                shortcut = format_secondary_shortcut("k")
                            )),
                    ),
            );
        }

        v_flex()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .child(content)
            .into_any_element()
    }

    fn render_scroll_pill(&self, cx: &mut Context<Self>) -> AnyElement {
        // Absolute positioning ignores mx_auto without pinned horizontal
        // insets; span the region and center with flex instead.
        div()
            .absolute()
            .bottom(px(12.))
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(
                // The outline button's bg is ~transparent; an opaque popover
                // backing keeps the pill readable over the chat text below.
                div()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().popover)
                    .shadow_md()
                    .child(
                        Button::new("scroll-to-end")
                            .outline()
                            .small()
                            .icon(IconName::ChevronDown)
                            .label(crate::tr!("chat.scroll_end"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.list_state.set_follow_mode(FollowMode::Tail);
                                this.list_state.scroll_to_end();
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }
}

struct ResidencyMarkdownEntries {
    entries: Vec<MarkdownEntry>,
    #[cfg(test)]
    constructions: usize,
}

fn markdown_entries_for_residency(
    timeline: &Timeline,
    scope: &ResidencyScope,
) -> ResidencyMarkdownEntries {
    let mut entries = Vec::new();
    for entry in &timeline.entries {
        let turn_running = timeline
            .turns
            .get(entry.turn)
            .is_some_and(|turn| turn.running);
        if !scope.includes(entry.turn, turn_running) {
            continue;
        }
        let markdown_bearing = matches!(
            entry.content,
            EntryContent::Item(ItemContent::AssistantMessage { .. })
                | EntryContent::Item(ItemContent::Reasoning { .. })
        ) || user_content(&entry.content).is_some();
        if markdown_bearing {
            entries.push(MarkdownEntry {
                id: entry.id.clone(),
                turn: entry.turn,
                turn_running,
            });
        }
    }
    if let Some(plan) = &timeline.proposed_plan {
        let turn_running = timeline
            .turns
            .get(plan.turn)
            .is_some_and(|turn| turn.running);
        if scope.includes(plan.turn, turn_running) {
            entries.push(MarkdownEntry {
                id: format!("plan:{}", plan.item_id),
                turn: plan.turn,
                turn_running,
            });
        }
    }
    ResidencyMarkdownEntries {
        #[cfg(test)]
        constructions: entries.len(),
        entries,
    }
}

impl Render for ChatView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_markdown_scroll_position(cx);
        let active = self.workspace_store.read(cx).chat_active_session();

        let root = v_flex().size_full().min_w_0().bg(cx.theme().background);

        let Some((title, cwd, is_draft)) = active else {
            return root
                .child(self.render_header(None, false, None, window, cx))
                .child(self.render_empty_state(window, cx));
        };

        let active_session_id = self.workspace_store.read(cx).active_session_id();
        let native_subagent_readonly = active_session_id.as_deref().is_some_and(|active_id| {
            self.workspace_store
                .read(cx)
                .sidebar_sessions()
                .iter()
                .any(|meta| meta.id == active_id && meta.native_subagent.is_some())
        });

        let title = if is_draft { None } else { Some(title) };
        let header = self.render_header(title, is_draft, Some(cwd.clone()), window, cx);
        let panel = self.workspace_store.read(cx).chat_panel_state();
        let terminal_open = panel.terminal_open;
        let terminal_height = panel.terminal_height;

        // Group entries by turn and render each turn section into the centered
        // content column. The column fills the available width up to
        // `CONTENT_MAX_WIDTH`; horizontal padding lives on the centering wrapper
        // (below) so the column shrinks gracefully — never clipping — when the
        // diff panel narrows the chat region.
        // The newest user / assistant message: their action rows stay visible
        // (hover is not the only way to reach Copy / native rewind).
        let (last_user_id, last_assistant_id) = self
            .workspace_store
            .read(cx)
            .with_active_timeline(|timeline| latest_message_ids(&timeline.entries))
            .unwrap_or_default();

        let item_count = self.turn_items.len();
        let item_cwd = cwd.clone();
        let timeline = list(
            self.list_state.clone(),
            cx.processor(move |this, index: usize, window, cx| {
                let Some(item) = this.turn_items.get(index) else {
                    return div().into_any_element();
                };
                // Clone only the handful of entries in this visible/overdrawn
                // turn. The full history remains behind the store and is never
                // cloned by the render path.
                let Some((turn, entries)) =
                    this.workspace_store
                        .read(cx)
                        .with_active_timeline(|timeline| {
                            (
                                timeline.turns.get(index).cloned().unwrap_or_default(),
                                // `entry_range` comes from `turn_items`, a snapshot
                                // that can trail the live timeline by a frame (e.g.
                                // adopting a running background thread whose timeline
                                // is being re-folded), so it must not index blindly.
                                timeline
                                    .entries
                                    .get(item.entry_range.clone())
                                    .map(<[_]>::to_vec)
                                    .unwrap_or_default(),
                            )
                        })
                else {
                    return div().into_any_element();
                };
                let rendered = this.render_turn(
                    (
                        index,
                        &turn,
                        &item_cwd,
                        &entries,
                        (last_user_id.as_deref(), last_assistant_id.as_deref()),
                    ),
                    window,
                    cx,
                );
                h_flex()
                    .w_full()
                    .justify_center()
                    .px(px(CONTENT_MIN_PADDING))
                    .when(this.highlighted_turn == Some(index), |item| {
                        item.rounded(crate::material::radius_card())
                            .bg(cx.theme().list_active)
                    })
                    .when(index + 1 < item_count, |item| item.pb(px(TURN_GAP)))
                    .child(div().w_full().max_w(px(CONTENT_MAX_WIDTH)).child(rendered))
                    .into_any_element()
            }),
        )
        .with_sizing_behavior(gpui::ListSizingBehavior::Auto)
        .flex_1()
        .min_h_0()
        .py_4();

        let composer: AnyElement = if native_subagent_readonly {
            div()
                .w_full()
                .py_3()
                .text_center()
                .text_size(px(12.))
                .text_color(cx.theme().muted_foreground)
                .child(crate::tr!("chat.subagent_readonly"))
                .into_any_element()
        } else {
            self.composer.clone().into_any_element()
        };
        let main = v_flex()
            .size_full()
            .min_h_0()
            .child(
                div()
                    .id("timeline")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(timeline)
                    .when(
                        self.list_state.is_scrolled_to_end() == Some(false),
                        |this| this.child(self.render_scroll_pill(cx)),
                    ),
            )
            .child(composer);

        let body: AnyElement = if terminal_open {
            let drawer = self.terminal_drawer.clone();
            let drawer_resize = self.terminal_drawer.clone();
            let width = f32::from(window.bounds().size.width);
            if !drawer.read(cx).is_size(width, terminal_height) {
                drawer.update(cx, |drawer, cx| drawer.resize(width, terminal_height, cx));
            }
            gpui_base::v_resizable("chat-terminal-panels")
                .on_resize(move |state, _, cx| {
                    let height = state.read(cx).sizes().get(1).copied();
                    if let Some(height) = height {
                        drawer_resize
                            .update(cx, |drawer, cx| drawer.resize(width, f32::from(height), cx));
                    }
                })
                .child(gpui_base::resizable_panel().child(main))
                .child(
                    gpui_base::resizable_panel()
                        .flex_none()
                        .size(px(terminal_height))
                        .size_range(px(120.)..px(600.))
                        .child(self.terminal_drawer.clone()),
                )
                .into_any_element()
        } else {
            main.into_any_element()
        };
        root.child(header).child(body)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Launch `zed <cwd>` detached; surface a notification if the CLI is missing.
/// The leading icon for a git quick-action.
fn git_action_icon(action: GitAction) -> Icon {
    match action {
        GitAction::Push => Icon::new(IconName::ArrowUp),
        GitAction::Pull => Icon::empty().path("icons/download.svg"),
        _ => Icon::empty().path("icons/git-branch.svg"),
    }
}

/// The commit dialog's footer action row (Cancel / Commit[& push]). Built inside
/// the `open_dialog` builder so the buttons can close the dialog on click.
fn render_commit_footer(
    dialog: &Entity<CommitDialog>,
    _window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let confirm_label = dialog.update(cx, |d, cx| d.confirm_label(cx));
    let cancel_dialog = dialog.clone();
    let confirm_dialog = dialog.clone();
    h_flex()
        .w_full()
        .gap_2()
        .justify_end()
        .child(
            Button::new("commit-cancel")
                .ghost()
                .label(crate::tr!("git.commit.cancel"))
                .on_click(move |_, window, cx| {
                    let _ = &cancel_dialog;
                    window.close_dialog(cx);
                }),
        )
        .child(
            Button::new("commit-confirm")
                .primary()
                .label(confirm_label)
                .on_click(move |_, window, cx| {
                    let should_close = confirm_dialog.update(cx, |d, cx| d.confirm(window, cx));
                    if should_close {
                        window.close_dialog(cx);
                    }
                }),
        )
        .into_any_element()
}

fn open_in_zed(cwd: &Path, window: &mut Window, cx: &mut App) {
    if tcode_services::desktop::open_in_zed(cwd).is_err() {
        window.push_notification(
            Notification::error(crate::tr!("errors.zed_cli_missing")),
            cx,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ASYNC_MARKDOWN_THRESHOLD_BYTES, AUTO_ACTIVITY_MIN_VISIBILITY, AutoActivityExpansions,
        ChatView, ResidencyScope, markdown_entries_for_residency,
    };
    use crate::store::WorkspaceStore;
    use crate::window_state::WindowState;
    use agent::ItemContent;
    use gpui::{AppContext as _, Entity, TestAppContext};
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };
    use std::time::{Duration, Instant};
    use tcode_core::session::{EntryContent, Timeline, TimelineEntry, TurnMeta};

    const AUTO_ACTIVITY_TEST_SESSION: &str = "session-a";
    static NEXT_RESIDENCY_TEST_ID: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn latest_activity_stays_expanded_until_a_successor_appears() {
        let mut expansions = AutoActivityExpansions::default();
        let first_seen = Instant::now();

        let running = expansions.observe(
            AUTO_ACTIVITY_TEST_SESSION,
            "activity-command",
            true,
            true,
            first_seen,
        );
        assert!(running.expanded);
        assert_eq!(running.collapse, None);

        let completed = expansions.observe(
            AUTO_ACTIVITY_TEST_SESSION,
            "activity-command",
            true,
            true,
            first_seen + Duration::from_secs(1),
        );
        assert!(completed.expanded);
        assert_eq!(completed.collapse, None);

        let superseded = expansions.observe(
            AUTO_ACTIVITY_TEST_SESSION,
            "activity-command",
            true,
            false,
            first_seen + Duration::from_secs(1),
        );
        assert!(!superseded.expanded);
        assert_eq!(superseded.collapse, None);
        assert_eq!(AUTO_ACTIVITY_MIN_VISIBILITY, Duration::from_millis(500));
    }

    #[test]
    fn early_successor_only_waits_for_remaining_minimum_visibility() {
        let mut expansions = AutoActivityExpansions::default();
        let first_seen = Instant::now();
        expansions.observe(
            AUTO_ACTIVITY_TEST_SESSION,
            "activity-command",
            true,
            true,
            first_seen,
        );
        let superseded = expansions.observe(
            AUTO_ACTIVITY_TEST_SESSION,
            "activity-command",
            true,
            false,
            first_seen + Duration::from_millis(200),
        );

        assert!(superseded.expanded);
        let (generation, delay) = superseded
            .collapse
            .expect("early successor should schedule the remaining delay");
        assert_eq!(delay, Duration::from_millis(300));
        assert!(expansions.finish_collapse(
            AUTO_ACTIVITY_TEST_SESSION,
            "activity-command",
            generation
        ));
        assert!(
            !expansions
                .observe(
                    AUTO_ACTIVITY_TEST_SESSION,
                    "activity-command",
                    true,
                    false,
                    first_seen + AUTO_ACTIVITY_MIN_VISIBILITY,
                )
                .expanded
        );
    }

    #[test]
    fn activity_first_seen_non_latest_still_gets_full_visibility_window() {
        let mut expansions = AutoActivityExpansions::default();
        let first_seen = Instant::now();
        let observed = expansions.observe(
            AUTO_ACTIVITY_TEST_SESSION,
            "activity-command",
            true,
            false,
            first_seen,
        );

        assert!(observed.expanded);
        let (generation, delay) = observed
            .collapse
            .expect("first observation should schedule a collapse");
        assert_eq!(delay, AUTO_ACTIVITY_MIN_VISIBILITY);
        assert!(expansions.finish_collapse(
            AUTO_ACTIVITY_TEST_SESSION,
            "activity-command",
            generation
        ));
    }

    #[test]
    fn collapsed_activity_stays_collapsed_after_visiting_another_session() {
        let mut expansions = AutoActivityExpansions::default();
        let first_seen = Instant::now();
        expansions.observe("session-a", "activity-command", true, true, first_seen);
        let superseded = expansions.observe(
            "session-a",
            "activity-command",
            true,
            false,
            first_seen + AUTO_ACTIVITY_MIN_VISIBILITY,
        );
        assert!(!superseded.expanded);

        let same_id_in_other_session = expansions.observe(
            "session-b",
            "activity-command",
            true,
            true,
            first_seen + Duration::from_millis(600),
        );
        assert!(same_id_in_other_session.expanded);

        let revisited = expansions.observe(
            "session-a",
            "activity-command",
            true,
            false,
            first_seen + Duration::from_millis(700),
        );
        assert!(!revisited.expanded);
        assert_eq!(revisited.collapse, None);
    }

    #[test]
    fn pending_collapse_is_scoped_to_its_session() {
        let mut expansions = AutoActivityExpansions::default();
        let first_seen = Instant::now();
        expansions.observe("session-a", "activity-command", true, true, first_seen);
        let (generation, _) = expansions
            .observe(
                "session-a",
                "activity-command",
                true,
                false,
                first_seen + Duration::from_millis(100),
            )
            .collapse
            .expect("supersession should schedule a collapse");
        expansions.observe(
            "session-b",
            "activity-command",
            true,
            true,
            first_seen + Duration::from_millis(200),
        );

        assert!(expansions.finish_collapse("session-a", "activity-command", generation));
        let other_session = expansions.observe(
            "session-b",
            "activity-command",
            true,
            true,
            first_seen + Duration::from_millis(300),
        );
        assert!(other_session.expanded);
        assert_eq!(other_session.collapse, None);
    }

    #[test]
    fn reactivated_activity_cancels_its_stale_delayed_collapse() {
        let mut expansions = AutoActivityExpansions::default();
        let first_seen = Instant::now();
        expansions.observe(
            AUTO_ACTIVITY_TEST_SESSION,
            "activity-command",
            true,
            true,
            first_seen,
        );
        let (generation, _) = expansions
            .observe(
                AUTO_ACTIVITY_TEST_SESSION,
                "activity-command",
                true,
                false,
                first_seen + Duration::from_millis(100),
            )
            .collapse
            .expect("supersession should schedule a collapse");

        assert!(
            expansions
                .observe(
                    AUTO_ACTIVITY_TEST_SESSION,
                    "activity-command",
                    true,
                    true,
                    first_seen + Duration::from_millis(200),
                )
                .expanded
        );
        assert!(!expansions.finish_collapse(
            AUTO_ACTIVITY_TEST_SESSION,
            "activity-command",
            generation
        ));
        assert!(
            !expansions
                .observe(
                    AUTO_ACTIVITY_TEST_SESSION,
                    "activity-command",
                    true,
                    false,
                    first_seen + AUTO_ACTIVITY_MIN_VISIBILITY,
                )
                .expanded
        );

        assert!(
            !expansions
                .observe(
                    AUTO_ACTIVITY_TEST_SESSION,
                    "activity-command",
                    false,
                    true,
                    first_seen + AUTO_ACTIVITY_MIN_VISIBILITY,
                )
                .expanded
        );
    }

    #[test]
    fn residency_markdown_entry_allocations_are_bounded_by_candidate_windows() {
        let mut timeline = synthetic_markdown_timeline(200);
        timeline.turns[5].running = true;
        timeline.turn_running = true;
        let scope = ResidencyScope::new(200, 40..48, None, true);

        let candidates = markdown_entries_for_residency(&timeline, &scope);

        assert_eq!(candidates.constructions, 177);
        assert!(candidates.constructions < timeline.entries.len());
        assert!(candidates.entries.iter().any(|entry| entry.turn == 5));
        assert!(candidates.entries.iter().any(|entry| entry.turn == 199));
    }

    #[gpui::test]
    fn chat_view_applies_markdown_residency_decisions(cx: &mut TestAppContext) {
        use gpui::{FollowMode, ListOffset, VisualTestContext, px, size};

        const TARGET: usize = 40;
        let timeline = synthetic_markdown_timeline(240);
        let (workspace_store, window_state, _) = seed_chat(cx, timeline);
        let (view, cx) = cx
            .add_window_view(|window, cx| ChatView::new(workspace_store, window_state, window, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(700.)));
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert_eq!(
            view.read_with(cx, |chat, _| chat.resident_markdown_state_count()),
            48
        );
        let list_state = view.read_with(cx, |chat, _| chat.list_state.clone());
        assert!(!view.read_with(cx, |chat, _| {
            chat.has_resident_markdown_state("assistant-40")
        }));

        list_state.set_follow_mode(FollowMode::Normal);
        list_state.scroll_to(ListOffset {
            item_ix: TARGET,
            offset_in_item: px(7.),
        });
        view.update(cx, |_, cx| cx.notify());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let (resident, old_rebuilt, distant_tail_evicted, composer_tail_resident) =
            view.read_with(cx, |chat, _| {
                (
                    chat.resident_markdown_state_count(),
                    ["user-40", "reasoning-40", "assistant-40"]
                        .iter()
                        .all(|id| chat.has_resident_markdown_state(id)),
                    !chat.has_resident_markdown_state("assistant-230"),
                    ["assistant-238", "assistant-239"]
                        .iter()
                        .all(|id| chat.has_resident_markdown_state(id)),
                )
            });
        assert!(old_rebuilt, "the old visible turn was not lazily rebuilt");
        assert!(
            distant_tail_evicted,
            "a tail state outside the hysteresis band remained resident"
        );
        assert!(
            composer_tail_resident,
            "the composer-adjacent tail was evicted while viewing old turns"
        );
        assert!(
            resident <= 96,
            "old-turn window retained {resident} MarkdownStates; expected at most 96"
        );
        assert_eq!(resident, 78);
        let scroll_top = list_state.logical_scroll_top();
        assert_eq!(scroll_top.item_ix, TARGET);
        assert_eq!(
            scroll_top.offset_in_item,
            px(7.),
            "rebuilding the scroll-top turn changed the list anchor"
        );
    }

    #[gpui::test]
    fn large_markdown_becomes_resident_asynchronously_and_remeasures_turn(cx: &mut TestAppContext) {
        let text = large_markdown("async content");
        let timeline = single_assistant_timeline("large", &text);
        let (workspace_store, window_state, _) = seed_chat(cx, timeline);
        let view =
            cx.add_window(|window, cx| ChatView::new(workspace_store, window_state, window, cx));
        let view = view.root(cx).expect("chat window should have a root");

        view.read_with(cx, |chat, _| {
            assert!(!chat.has_resident_markdown_state("large"));
            assert!(!chat.markdown_remeasured_turns.contains(&0));
        });
        cx.run_until_parked();
        view.read_with(cx, |chat, cx| {
            assert!(chat.has_resident_markdown_state("large"));
            assert_eq!(chat.resident_markdown_source("large"), Some(text.as_str()));
            let rendered = chat
                .md_states
                .get("large")
                .expect("large Markdown state should be resident")
                .state
                .read(cx)
                .rendered_text();
            assert!(rendered.contains("async content"));
            assert!(chat.markdown_remeasured_turns.contains(&0));
        });
    }

    #[gpui::test]
    fn in_flight_markdown_update_finishes_with_latest_text(cx: &mut TestAppContext) {
        let original = large_markdown("original");
        let latest = large_markdown("edited");
        let timeline = single_assistant_timeline("large", &original);
        let (workspace_store, window_state, session_id) = seed_chat(cx, timeline);
        let view = cx.add_window(|window, cx| {
            ChatView::new(workspace_store.clone(), window_state, window, cx)
        });
        let view = view.root(cx).expect("chat window should have a root");
        let generation = view.read_with(cx, |chat, _| {
            chat.pending_md_builds
                .get("large")
                .expect("large build should be pending")
                .generation
        });

        workspace_store.update(cx, |store, _| {
            store.set_session_replica_for_test(
                session_id,
                single_assistant_timeline("large", &latest),
            );
        });
        view.update(cx, |chat, cx| chat.sync_markdown_states(cx));
        view.read_with(cx, |chat, _| {
            assert_eq!(chat.pending_md_builds.len(), 1);
            assert_eq!(
                chat.pending_md_builds["large"].generation, generation,
                "a text update spawned a duplicate in-flight job"
            );
        });

        cx.run_until_parked();
        view.read_with(cx, |chat, _| {
            assert_eq!(
                chat.resident_markdown_source("large"),
                Some(latest.as_str())
            );
            assert!(chat.pending_md_builds.is_empty());
        });
    }

    #[gpui::test]
    fn session_switch_does_not_resurrect_in_flight_markdown(cx: &mut TestAppContext) {
        let text = large_markdown("stale session");
        let timeline = single_assistant_timeline("large", &text);
        let (workspace_store, window_state, _) = seed_chat(cx, timeline);
        let view = cx.add_window(|window, cx| {
            ChatView::new(workspace_store.clone(), window_state, window, cx)
        });
        let view = view.root(cx).expect("chat window should have a root");
        assert!(view.read_with(cx, |chat, _| chat.pending_md_builds.contains_key("large")));

        workspace_store.update(cx, |store, _| {
            store.set_session_replica_for_test("replacement-session".into(), Timeline::default());
        });
        view.update(cx, |chat, cx| chat.sync_markdown_states(cx));
        cx.run_until_parked();

        view.read_with(cx, |chat, _| {
            assert!(!chat.has_resident_markdown_state("large"));
            assert!(!chat.pending_md_builds.contains_key("large"));
        });
    }

    #[gpui::test]
    fn small_markdown_is_resident_synchronously(cx: &mut TestAppContext) {
        let text = "small **streaming** reply";
        assert!(text.len() < ASYNC_MARKDOWN_THRESHOLD_BYTES);
        let timeline = single_assistant_timeline("small", text);
        let (workspace_store, window_state, _) = seed_chat(cx, timeline);
        let view =
            cx.add_window(|window, cx| ChatView::new(workspace_store, window_state, window, cx));
        let view = view.root(cx).expect("chat window should have a root");

        view.read_with(cx, |chat, _| {
            assert_eq!(chat.resident_markdown_source("small"), Some(text));
            assert!(!chat.pending_md_builds.contains_key("small"));
        });
    }

    #[gpui::test]
    fn long_markdown_paints_middle_blocks_when_scrolled_in_chat_outer_list(
        cx: &mut TestAppContext,
    ) {
        use gpui::{VisualTestContext, point, px, size};
        use tcode_runtime::pipe::{HostServices, spawn_host};
        use tcode_services::store::SessionStore;

        const DEMO_MARKDOWN: &str = r#"# H1

## H2

This paragraph has **bold**, *italic*, ~~strikethrough~~, and `inline code`.

```rust
fn main() {
    let language = "Rust";
    let message = format!("Hello from {language}!");
    println!("{message}");
}
```

```typescript
const count: number = 3;
interface Demo {
  title: string;
  enabled: boolean;
}
const demo: Demo = { title: "TypeScript", enabled: true };
```

```python
def greet(name: str) -> str:
    message = f"Hello, {name}!"
    return message

print(greet("Python"))
```

```go
package main
func main() {
    message := "Hello from Go"
    println(message)
}
```

```toml
[demo]
title = "TOML sample"
enabled = true
count = 3
```

```kotlin
fun main() {
    val language: String = "Kotlin"
    println("Hello from $language")
}
```

```text
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
```

| Left | Center | Right |
| :--- | :----: | ----: |
| alpha | beta | 100 |
| gamma | delta | 200 |

| Name | Status | Owner | Description | Count | Notes |
| :--- | :----: | :--- | :---------- | ----: | :---- |
| Short | Ready | UI | This deliberately long table cell contains about sixty characters total. | 12 | wraps |
| Longer component name | Pending | Visual QA | Brief | 3 | compact |

- [x] Render headings
- [ ] Inspect every pixel

1. Ordered parent
   - Unordered child
     1. Nested ordered child
2. Second ordered item

- Unordered parent
  - Nested bullet
    1. Ordered grandchild

> First line of the blockquote.
> Second line of the blockquote.

Visit [Example](https://example.com) or the bare URL https://tcode.dev for more.

---

This paragraph has a soft
line break, followed by a hard break.\
This begins after the hard break."#;

        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let data_root = std::env::temp_dir().join(format!(
            "tcode-markdown-outer-list-test-{}",
            tcode_services::store::now_millis()
        ));
        let store = SessionStore::open_at(data_root).expect("test session store");
        let host = spawn_host(store, HostServices::default()).expect("spawn test host");
        let (session_id, timeline) = smol::block_on(host.update_state_for_test(|state, cx| {
            state.start_draft("markdown-test".into(), std::env::temp_dir(), cx);
            let active = state.residents.active.as_mut().expect("active draft");
            active.timeline = Timeline::default();
            active.timeline.turns = vec![TurnMeta::default()];
            active.timeline.entries = vec![
                entry("user", user_item("render a long document")),
                entry("assistant", assistant(DEMO_MARKDOWN)),
            ];
            active.draft = false;
            (active.meta.id.clone(), active.timeline.clone())
        }))
        .expect("seed markdown host");
        let workspace_store = cx.new(|cx| crate::store::WorkspaceStore::new(host.clone(), cx));
        workspace_store.update(cx, |store, _| {
            store.set_session_replica_for_test(session_id, timeline);
        });
        let window_state = cx.new(|_| WindowState::new(false));

        let (view, cx) = cx.add_window_view(|window, cx| {
            ChatView::new(workspace_store.clone(), window_state, window, cx)
        });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(700.)));
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let outer_list = view.read_with(cx, |chat, _| chat.list_state.clone());
        let max_scroll = outer_list.max_offset_for_scrollbar().y;
        assert!(
            max_scroll > px(800.),
            "long markdown did not contribute its full height to the chat list: {max_scroll:?}"
        );
        let scroll_step = px(40.);
        let steps = (f32::from(max_scroll / 2.) / f32::from(scroll_step)).ceil() as usize;
        for step in 0..steps {
            let distance = (scroll_step * (step + 1) as f32).min(max_scroll / 2.);
            outer_list.set_offset_from_scrollbar(point(px(0.), -(max_scroll - distance)));
            view.update(cx, |_, cx| cx.notify());
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
        }

        let viewport = outer_list.viewport_bounds();
        let middle = cx.debug_bounds("markdown-block-6");
        assert!(
            middle.is_some_and(|bounds| bounds.intersects(&viewport)),
            "middle markdown block did not paint at the chat list's middle offset; bounds={middle:?}, max_scroll={max_scroll:?}, viewport={viewport:?}"
        );
        assert!(
            cx.debug_bounds("markdown-block-18").is_none(),
            "offscreen tail block was painted; block virtualization did not cull it"
        );
    }

    fn entry(id: &str, content: EntryContent) -> Arc<TimelineEntry> {
        Arc::new(TimelineEntry {
            id: id.to_string(),
            content,
            ts: None,
            turn: 0,
        })
    }

    fn user_item(text: &str) -> EntryContent {
        EntryContent::Item(ItemContent::UserMessage {
            text: text.into(),
            context_len: None,
            attachments: Vec::new(),
        })
    }

    fn assistant(text: &str) -> EntryContent {
        EntryContent::Item(ItemContent::AssistantMessage { text: text.into() })
    }

    fn reasoning(text: &str) -> EntryContent {
        EntryContent::Item(ItemContent::Reasoning { text: text.into() })
    }

    fn at_turn(mut entry: Arc<TimelineEntry>, turn: usize) -> Arc<TimelineEntry> {
        Arc::make_mut(&mut entry).turn = turn;
        entry
    }

    fn synthetic_markdown_timeline(turn_count: usize) -> Timeline {
        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta::default(); turn_count];
        for turn in 0..turn_count {
            timeline.entries.extend([
                at_turn(
                    entry(&format!("user-{turn}"), user_item(&format!("User {turn}"))),
                    turn,
                ),
                at_turn(
                    entry(
                        &format!("reasoning-{turn}"),
                        reasoning(&format!("Reasoning {turn}")),
                    ),
                    turn,
                ),
                at_turn(
                    entry(
                        &format!("assistant-{turn}"),
                        assistant(&format!("Assistant {turn}")),
                    ),
                    turn,
                ),
            ]);
        }
        timeline
    }

    fn single_assistant_timeline(id: &str, text: &str) -> Timeline {
        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta::default()];
        timeline.entries.push(entry(id, assistant(text)));
        timeline
    }

    fn large_markdown(marker: &str) -> String {
        let mut text = format!("# {marker}\n\n");
        while text.len() <= ASYNC_MARKDOWN_THRESHOLD_BYTES {
            text.push_str("- a sufficiently substantial Markdown list item\n");
        }
        text
    }

    fn seed_chat(
        cx: &mut TestAppContext,
        timeline: Timeline,
    ) -> (Entity<WorkspaceStore>, Entity<WindowState>, String) {
        use tcode_runtime::pipe::{HostServices, spawn_host};
        use tcode_services::store::SessionStore;

        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let data_root = std::env::temp_dir().join(format!(
            "tcode-chat-residency-test-{}-{}-{}",
            std::process::id(),
            tcode_services::store::now_millis(),
            NEXT_RESIDENCY_TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let store = SessionStore::open_at(data_root).expect("test session store");
        let host = spawn_host(store, HostServices::default()).expect("spawn test host");
        let (session_id, timeline) = smol::block_on(host.update_state_for_test(|state, cx| {
            state.start_draft("markdown-residency-test".into(), std::env::temp_dir(), cx);
            let active = state.residents.active.as_mut().expect("active draft");
            active.timeline = timeline;
            active.draft = false;
            (active.meta.id.clone(), active.timeline.clone())
        }))
        .expect("seed markdown host");
        let workspace_store = cx.new(|cx| WorkspaceStore::new(host, cx));
        workspace_store.update(cx, |store, _| {
            store.set_session_replica_for_test(session_id.clone(), timeline);
        });
        let window_state = cx.new(|_| WindowState::new(false));
        (workspace_store, window_state, session_id)
    }
}
