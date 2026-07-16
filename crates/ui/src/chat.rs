use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use std::path::{Path, PathBuf};

use agent::{FileChange, ItemStatus};
use gpui::{
    Anchor, AnyElement, App, AppContext as _, ClipboardItem, Context, Entity, Focusable as _,
    FollowMode, InteractiveElement as _, IntoElement, ListAlignment, ListState, ParentElement as _,
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window,
    div, list, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _,
    StyledExt as _, WindowExt as _,
    button::{Button, ButtonVariant, ButtonVariants as _},
    dialog::DialogButtonProps,
    h_flex,
    input::{Input, InputEvent, InputState},
    notification::Notification,
    popover::Popover,
    scroll::ScrollableElement as _,
    text::{TextView, TextViewState},
    tooltip::Tooltip,
    v_flex,
};

use tcode_core::git::GitAction;
use tcode_core::session::{
    EntryContent, OrchestrateCallback, SteeringStatus, TimelineEntry, TurnMeta,
    parse_orchestrate_callback,
};
use tcode_runtime::app::{AppState, RightTab};

use crate::commit_dialog::CommitDialog;
use crate::composer::{Composer, ComposerEvent};
use crate::git::{git_action_label_key, git_hint_key};
use crate::terminal_drawer::TerminalDrawer;
use crate::time::now_millis;
use crate::window_drag_area;

/// Content-column max width (T3 centers the timeline at ~760px).
const CONTENT_MAX_WIDTH: f32 = 768.;
/// Minimum horizontal padding around the content column so bubbles/cards never
/// clip when the chat region is narrowed (e.g. the diff panel is open).
const CONTENT_MIN_PADDING: f32 = 24.;
/// How many activity rows to show before the "+N previous log entrys" expander.
const WORKLOG_VISIBLE_ROWS: usize = 2;

/// Localized previous-log toggle. The label remains available while rows are
/// expanded so the same control can collapse them again.
fn previous_logs_toggle_label(hidden: usize, expanded: bool) -> Option<Cow<'static, str>> {
    (hidden > 0).then(|| {
        if expanded {
            tcode_i18n::tr!("chat.hide_previous_logs", count = hidden)
        } else {
            tcode_i18n::tr!("chat.previous_logs", count = hidden)
        }
    })
}
/// Height reserved under every message for its (hover-revealed) action row, so
/// revealing it never shifts the timeline.
const ACTION_ROW_HEIGHT: f32 = 24.;
/// Line height of the preformatted text inside an expanded disclosure card; also
/// the per-line unit used to resolve the card's capped scroll viewport.
const DISCLOSURE_LINE_HEIGHT: f32 = 20.;
/// Vertical padding (top + bottom) added to a disclosure card's viewport estimate.
const DISCLOSURE_CARD_PADDING: f32 = 24.;
/// Cap on an expanded disclosure card's height; taller content scrolls within it.
const DISCLOSURE_CARD_MAX_HEIGHT: f32 = 320.;
/// Child-thread title budget in a callback disclosure row before it is ellipsized.
const CALLBACK_TITLE_MAX_CHARS: usize = 24;
/// Vertical rhythm between turns. Turns are separated by space and typographic
/// hierarchy alone — there is deliberately no rule/divider under the user bubble.
const TURN_GAP: f32 = 44.;
/// Pre-measure this many full-window heights on each side of the chat viewport.
///
/// GPUI's list performs the expensive first layout for items in this band, so a
/// generous buffer keeps ordinary trackpad/wheel scrolling from discovering and
/// laying out a turn on the same frame in which it becomes visible. The chat
/// viewport is shorter than the full window, making this a conservative lower
/// bound in practice while the list itself remains bounded for huge histories.
const TIMELINE_OVERDRAW_VIEWPORTS: f32 = 4.;
const TIMELINE_MIN_OVERDRAW: f32 = 3072.;

fn timeline_overdraw(viewport_height: f32) -> f32 {
    (viewport_height.max(0.) * TIMELINE_OVERDRAW_VIEWPORTS).max(TIMELINE_MIN_OVERDRAW)
}

/// Markdown state that grows with streaming deltas (stream_markdown pattern).
///
/// `TextViewState` keeps two copies of the document: `parsed_content` on the
/// main thread and a private `content` inside its background parse task. Only
/// `push_str` updates reach the background task — the initial text (and
/// `set_text`) are parsed synchronously on the main thread and never sent. The
/// background task therefore only ever knows the text that was *pushed* into
/// it, and because every `push_str` result replaces `parsed_content` wholesale,
/// any text that was seeded (or `set_text`) is dropped the moment the first
/// delta lands. See the `md_state_*` tests.
///
/// So this mirror upholds one invariant: **a state that will ever be grown with
/// `push_str` is seeded empty and only ever grown with `push_str`** (`can_push`).
/// Text that cannot be expressed as an append rebuilds the entity instead of
/// calling `set_text`.
struct MdState {
    state: Entity<TextViewState>,
    /// The text currently mirrored into `state`. Sharing it lets every
    /// re-render install a Copy handler without cloning a long response.
    synced: Arc<str>,
    /// Whether `state`'s whole content arrived through `push_str` (i.e. it was
    /// seeded empty), which is what makes further `push_str` calls sound.
    can_push: bool,
}

/// How to bring a mirrored [`MdState`] in line with the timeline's text.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MdSync {
    /// Already in sync.
    Noop,
    /// The text grew by an append: `push_str` this delta.
    Push(String),
    /// The text changed in a way `push_str` cannot express (or the state is not
    /// safe to push into): rebuild the entity from this text.
    Reset(String),
}

/// The pure delta/reset decision behind [`MdState::sync`].
fn md_sync(synced: &str, text: &str, can_push: bool) -> MdSync {
    if synced == text {
        return MdSync::Noop;
    }
    match text.strip_prefix(synced) {
        Some(delta) if can_push && !delta.is_empty() => MdSync::Push(delta.to_string()),
        _ => MdSync::Reset(text.to_string()),
    }
}

/// A chronological block in a turn. File-change entries stay in activity runs
/// for summary counting, but are rendered by the turn-level CHANGED FILES card.
#[derive(Debug)]
enum Segment<'a> {
    ActivityRun(Vec<&'a TimelineEntry>),
    User(&'a TimelineEntry),
    Assistant(&'a TimelineEntry),
    Error(&'a TimelineEntry),
}

#[derive(Debug)]
struct SegmentedEntries<'a> {
    flow: Vec<Segment<'a>>,
    pending_steers: Vec<&'a TimelineEntry>,
}

fn displayed_error_text(content: &EntryContent) -> Cow<'_, str> {
    match content {
        EntryContent::Error { message } => Cow::Borrowed(message),
        EntryContent::ProviderStartError { error } => {
            tcode_i18n::tr!("errors.provider_start", error = error)
        }
        _ => unreachable!("displayed_error_text requires error timeline content"),
    }
}

/// Coalesce only adjacent activity entries, leaving messages and errors at
/// their exact positions in the timeline.
fn segment_entries<'a>(
    entries: &'a [Arc<TimelineEntry>],
    turn_running: bool,
) -> SegmentedEntries<'a> {
    let mut segments = Vec::new();
    let mut activities = Vec::new();
    let mut pending_steers = Vec::new();
    let live_reasoning_index = turn_running
        .then(|| {
            entries.iter().rposition(|entry| {
                !matches!(
                    entry.content,
                    EntryContent::User {
                        steering: Some(SteeringStatus::Pending),
                        ..
                    }
                )
            })
        })
        .flatten()
        .filter(|index| matches!(entries[*index].content, EntryContent::Reasoning { .. }));

    let flush_activities = |segments: &mut Vec<Segment<'a>>,
                            activities: &mut Vec<&'a TimelineEntry>| {
        if !activities.is_empty() {
            segments.push(Segment::ActivityRun(std::mem::take(activities)));
        }
    };

    for (entry_index, entry) in entries.iter().enumerate() {
        let entry = entry.as_ref();
        if turn_running
            && matches!(
                entry.content,
                EntryContent::User {
                    steering: Some(SteeringStatus::Pending),
                    ..
                }
            )
        {
            pending_steers.push(entry);
            continue;
        }
        match &entry.content {
            EntryContent::Command { .. }
            | EntryContent::Tool { .. }
            | EntryContent::Subagent { .. }
            | EntryContent::ContextCompacted
            | EntryContent::FileChange { .. } => activities.push(entry),
            EntryContent::Reasoning { .. } => {
                if live_reasoning_index == Some(entry_index) {
                    activities.push(entry);
                }
            }
            EntryContent::User { .. } => {
                flush_activities(&mut segments, &mut activities);
                segments.push(Segment::User(entry));
            }
            EntryContent::Assistant { .. } => {
                flush_activities(&mut segments, &mut activities);
                segments.push(Segment::Assistant(entry));
            }
            EntryContent::Error { .. } | EntryContent::ProviderStartError { .. } => {
                flush_activities(&mut segments, &mut activities);
                segments.push(Segment::Error(entry));
            }
        }
    }
    flush_activities(&mut segments, &mut activities);
    SegmentedEntries {
        flow: segments,
        pending_steers,
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct WorkLogCounts {
    commands: usize,
    files: usize,
    tools: usize,
    subagents: usize,
    compactions: usize,
}

fn work_log_counts(entries: &[&TimelineEntry]) -> WorkLogCounts {
    let mut counts = WorkLogCounts::default();
    let mut files = HashSet::new();

    for entry in entries {
        match &entry.content {
            EntryContent::Command { .. } => counts.commands += 1,
            EntryContent::FileChange { changes } => {
                files.extend(changes.iter().map(|change| change.path.as_str()));
            }
            EntryContent::Tool { .. } => counts.tools += 1,
            EntryContent::Subagent { .. } => counts.subagents += 1,
            EntryContent::ContextCompacted => counts.compactions += 1,
            EntryContent::User { .. }
            | EntryContent::Assistant { .. }
            | EntryContent::Reasoning { .. }
            | EntryContent::Error { .. }
            | EntryContent::ProviderStartError { .. } => {}
        }
    }
    counts.files = files.len();
    counts
}

fn localized_count(count: usize, one_key: &str, many_key: &str) -> Option<String> {
    (count > 0).then(|| {
        if count == 1 {
            tcode_i18n::tr!(one_key).into_owned()
        } else {
            tcode_i18n::tr!(many_key, count = count).into_owned()
        }
    })
}

fn work_log_summary_with_command_keys(
    counts: &WorkLogCounts,
    command_one_key: &str,
    commands_key: &str,
) -> Option<String> {
    let mut clauses = Vec::new();
    clauses.extend(localized_count(
        counts.commands,
        command_one_key,
        commands_key,
    ));
    clauses.extend(localized_count(
        counts.files,
        "chat.summary_file_one",
        "chat.summary_files",
    ));
    clauses.extend(localized_count(
        counts.tools,
        "chat.summary_tool_one",
        "chat.summary_tools",
    ));
    clauses.extend(localized_count(
        counts.subagents,
        "chat.summary_subagent_one",
        "chat.summary_subagents",
    ));
    clauses.extend(localized_count(
        counts.compactions,
        "chat.summary_compaction_one",
        "chat.summary_compactions",
    ));
    (!clauses.is_empty()).then(|| clauses.join(" · "))
}

fn work_log_summary(counts: &WorkLogCounts) -> Option<String> {
    work_log_summary_with_command_keys(counts, "chat.summary_command_one", "chat.summary_commands")
}

fn turn_work_log_summary(counts: &WorkLogCounts) -> Option<String> {
    work_log_summary_with_command_keys(counts, "chat.total_command_one", "chat.total_commands")
        .map(|summary| tcode_i18n::tr!("chat.total_summary", summary = summary).into_owned())
}

fn finished_work_log_label(
    is_last_activity: bool,
    segment_counts: &WorkLogCounts,
    turn_counts: &WorkLogCounts,
) -> Option<String> {
    if is_last_activity {
        turn_work_log_summary(turn_counts)
    } else {
        work_log_summary(segment_counts)
    }
}

/// Cached indexing and cheap height-affecting identity for one virtualized turn.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TurnListItem {
    entry_range: Range<usize>,
    entry_count: usize,
    identity: u64,
    content: u64,
}

/// Mutation to apply to the persistent [`ListState`] after a timeline sync.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ListSync {
    None,
    Reset {
        count: usize,
    },
    Incremental {
        append: Option<Range<usize>>,
        remeasure: Vec<usize>,
    },
}

/// Build contiguous entry ranges and fingerprints for turn-level list items.
///
/// Timeline entries are chronological, so all entries for a turn are adjacent.
/// The max entry turn keeps a temporary orphan bucket renderable if a provider
/// ever exposes an entry before its corresponding `TurnMeta`.
fn index_turns(
    turns: &[TurnMeta],
    entries: &[Arc<TimelineEntry>],
    proposed_plan: Option<(usize, &str, &str)>,
    children: &HashMap<String, Vec<Arc<TimelineEntry>>>,
    expanded: &HashSet<String>,
) -> Vec<TurnListItem> {
    debug_assert!(entries.windows(2).all(|pair| pair[0].turn <= pair[1].turn));

    let item_count = turns
        .len()
        .max(entries.last().map_or(0, |entry| entry.turn + 1));
    let mut ranges = vec![entries.len()..entries.len(); item_count];
    for (index, entry) in entries.iter().enumerate() {
        let range = &mut ranges[entry.turn];
        if range.start == entries.len() {
            range.start = index;
        }
        range.end = index + 1;
    }

    ranges
        .into_iter()
        .enumerate()
        .map(|(index, entry_range)| {
            let mut identity = DefaultHasher::new();
            let mut content = DefaultHasher::new();
            for entry in &entries[entry_range.clone()] {
                entry.id.hash(&mut identity);
                std::mem::discriminant(&entry.content).hash(&mut content);
                entry.ts.hash(&mut content);
                hash_entry_shape(&entry.content, &mut content);
                if matches!(&entry.content, EntryContent::Subagent { .. }) {
                    let subagent_expanded = expanded.contains(&format!("subagent-{}", entry.id));
                    subagent_expanded.hash(&mut content);
                    if subagent_expanded {
                        let child_entries = children.get(&entry.id).map_or(&[][..], Vec::as_slice);
                        child_entries.len().hash(&mut content);
                        for child in child_entries {
                            child.id.hash(&mut content);
                            child.ts.hash(&mut content);
                            hash_entry_shape(&child.content, &mut content);
                        }
                    }
                }
                // A disclosure row (orchestrate context / callback) grows a tall
                // scroll card when expanded, so its toggle state must change the
                // turn fingerprint or the list keeps the collapsed measurement.
                if let Some(key) = disclosure_key(&entry.content, &entry.id) {
                    expanded.contains(&key).hash(&mut content);
                }
            }
            if let Some(turn) = turns.get(index) {
                turn.start_ts.hash(&mut content);
                turn.end_ts.hash(&mut content);
                turn.running.hash(&mut content);
                turn.status
                    .as_ref()
                    .map(std::mem::discriminant)
                    .hash(&mut content);
            }
            if let Some((turn, item_id, markdown)) = proposed_plan
                && turn == index
            {
                item_id.hash(&mut identity);
                markdown.len().hash(&mut content);
            }
            TurnListItem {
                entry_count: entry_range.len(),
                entry_range,
                identity: identity.finish(),
                content: content.finish(),
            }
        })
        .collect()
}

/// The per-entry expansion key for a user message that renders as a disclosure
/// row rather than a bubble: an orchestrate context split (annotated with a
/// `context_len`) or a child-thread callback (whose text parses as one). `None`
/// for an ordinary user message, which stays a plain bubble.
fn disclosure_key(content: &EntryContent, entry_id: &str) -> Option<String> {
    match content {
        EntryContent::User {
            context_len: Some(_),
            ..
        } => Some(format!("orchestrate-context-{entry_id}")),
        EntryContent::User { text, .. } if parse_orchestrate_callback(text).is_some() => {
            Some(format!("orchestrate-callback-{entry_id}"))
        }
        _ => None,
    }
}

/// Hash only data that can alter a turn's layout. Text lengths make streaming
/// updates O(number of entries) without repeatedly hashing growing markdown.
fn hash_entry_shape(content: &EntryContent, hash: &mut DefaultHasher) {
    match content {
        EntryContent::User {
            text,
            steering,
            context_len,
        } => {
            text.len().hash(hash);
            steering.hash(hash);
            context_len.hash(hash);
        }
        EntryContent::Assistant { text } | EntryContent::Reasoning { text } => {
            text.len().hash(hash);
        }
        EntryContent::Command {
            command,
            output,
            exit_code,
            status,
        } => {
            command.len().hash(hash);
            output.len().hash(hash);
            exit_code.hash(hash);
            std::mem::discriminant(status).hash(hash);
        }
        EntryContent::FileChange { changes } => {
            changes.len().hash(hash);
            for change in changes {
                change.path.len().hash(hash);
                change.diff.as_ref().map(String::len).hash(hash);
            }
        }
        EntryContent::Tool {
            name,
            input,
            output,
            status,
        } => {
            name.len().hash(hash);
            input.to_string().len().hash(hash);
            output.as_ref().map(String::len).hash(hash);
            std::mem::discriminant(status).hash(hash);
        }
        EntryContent::Subagent {
            agent_type,
            description,
            status,
            summary,
        } => {
            agent_type.len().hash(hash);
            description.len().hash(hash);
            std::mem::discriminant(status).hash(hash);
            summary.as_ref().map(String::len).hash(hash);
        }
        EntryContent::Error { message } => message.len().hash(hash),
        EntryContent::ProviderStartError { error } => error.len().hash(hash),
        EntryContent::ContextCompacted => {}
    }
}

fn list_sync(old: &[TurnListItem], new: &[TurnListItem], session_changed: bool) -> ListSync {
    let common = old.len().min(new.len());
    let replaced = (0..common).any(|index| {
        let old = &old[index];
        let new = &new[index];
        new.entry_count < old.entry_count
            || (new.entry_count == old.entry_count && new.identity != old.identity)
    });
    if session_changed || new.len() < old.len() || replaced {
        return ListSync::Reset { count: new.len() };
    }

    let append = (new.len() > old.len()).then_some(old.len()..new.len());
    let mut remeasure = (0..common)
        .filter(|&index| {
            old[index].entry_count != new[index].entry_count
                || old[index].content != new[index].content
        })
        .collect::<Vec<_>>();
    // The former last item gains an inter-turn gap when a new turn appears.
    if append.is_some() && !old.is_empty() && !remeasure.contains(&(old.len() - 1)) {
        remeasure.push(old.len() - 1);
    }

    if append.is_none() && remeasure.is_empty() {
        ListSync::None
    } else {
        ListSync::Incremental { append, remeasure }
    }
}

impl MdState {
    /// Mirror `text`. `streaming` marks text that is still being produced (its
    /// turn is running): it is seeded empty and pushed, so the background parse
    /// task stays authoritative. Settled text (replay, finished turns) is
    /// parsed synchronously at construction so the first layout already has the
    /// right height.
    fn new(text: &str, streaming: bool, cx: &mut App) -> Self {
        let state = if streaming {
            let state = cx.new(|cx| TextViewState::markdown("", cx));
            if !text.is_empty() {
                state.update(cx, |state, cx| state.push_str(text, cx));
            }
            state
        } else {
            cx.new(|cx| TextViewState::markdown(text, cx))
        };
        Self {
            state,
            synced: Arc::from(text),
            // An empty seed leaves the background task's document empty too, so
            // it stays safe to push into even outside streaming mode.
            can_push: streaming || text.is_empty(),
        }
    }

    fn sync(&mut self, text: String, streaming: bool, cx: &mut App) {
        match md_sync(&self.synced, &text, self.can_push) {
            MdSync::Noop => {}
            MdSync::Push(delta) => {
                self.state
                    .update(cx, |state, cx| state.push_str(&delta, cx));
                self.synced = Arc::from(text);
            }
            MdSync::Reset(text) => *self = Self::new(&text, streaming, cx),
        }
    }
}

/// An open inline "Edit & resend" editor on a user message.
struct MessageEditor {
    /// The timeline turn the edited message opened (the rewind boundary).
    turn: usize,
    /// The edited message's entry id (a turn can hold more than one).
    entry_id: String,
    input: Entity<InputState>,
    _subscription: Subscription,
}

pub struct ChatView {
    app_state: Entity<AppState>,
    composer: Entity<Composer>,
    terminal_drawer: Entity<TerminalDrawer>,
    list_state: ListState,
    turn_items: Vec<TurnListItem>,
    md_states: HashMap<String, MdState>,
    /// The open inline message editor, if any (at most one at a time).
    editor: Option<MessageEditor>,
    /// Open/closed keys for collapsibles (work logs, activity rows, cards, files).
    expanded: HashSet<String>,
    session_key: Option<String>,
    /// 1s ticker kept alive while a turn is running (drives live "Working for Ns").
    _tick: Option<Task<()>>,
    /// Which copy button is currently showing its "Copied!" confirmation (2s):
    /// the copy target's key (`plan`, `user:<id>`, `assistant:<id>`).
    copied: Option<String>,
    _copied_task: Option<Task<()>>,
    /// The live commit dialog entity while it is open (kept alive across frames).
    commit_dialog: Option<Entity<CommitDialog>>,
    _subscriptions: Vec<Subscription>,
}

impl ChatView {
    pub fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let composer = cx.new(|cx| Composer::new(app_state.clone(), window, cx));
        let overdraw = timeline_overdraw(f32::from(window.bounds().size.height));
        let list_state = ListState::new(0, ListAlignment::Bottom, px(overdraw));
        list_state.set_follow_mode(FollowMode::Tail);

        let subscriptions = vec![
            cx.subscribe(&composer, |this, _, event, cx| {
                let ComposerEvent::Submitted = event;
                // Re-engage tail following even if the user had scrolled up.
                this.list_state.set_follow_mode(FollowMode::Tail);
                this.list_state.scroll_to_end();
                cx.notify();
            }),
            cx.observe(&app_state, |this, _, cx| {
                this.sync_markdown_states(cx);
                cx.notify();
            }),
        ];
        let terminal_drawer = cx.new(|cx| TerminalDrawer::new(app_state.clone(), cx));

        let mut this = Self {
            app_state,
            composer,
            terminal_drawer,
            list_state,
            turn_items: Vec::new(),
            md_states: HashMap::new(),
            editor: None,
            expanded: HashSet::new(),
            session_key: None,
            _tick: None,
            copied: None,
            _copied_task: None,
            commit_dialog: None,
            _subscriptions: subscriptions,
        };
        this.sync_markdown_states(cx);
        this
    }

    /// Mirror timeline markdown text into `TextViewState` entities, growing
    /// them with `push_str` when possible so streaming reparses incrementally.
    fn sync_markdown_states(&mut self, cx: &mut Context<Self>) {
        // (id, text, streaming): `streaming` marks text whose turn is still
        // running, i.e. text that further deltas will grow.
        let (session_key, texts, running, turn_items) = {
            let state = self.app_state.read(cx);
            let session_key = state.active_session_id().map(str::to_string);
            let mut texts: Vec<(String, String, bool)> = Vec::new();
            let mut running = false;
            let mut turn_items = Vec::new();
            if let Some(active) = &state.active {
                let timeline = &active.timeline;
                running = timeline.turn_running;
                turn_items = index_turns(
                    &timeline.turns,
                    &timeline.entries,
                    timeline
                        .proposed_plan
                        .as_ref()
                        .map(|plan| (plan.turn, plan.item_id.as_str(), plan.markdown.as_str())),
                    &timeline.children,
                    &self.expanded,
                );
                let turn_running =
                    |turn: usize| timeline.turns.get(turn).is_some_and(|t| t.running);
                for entry in &timeline.entries {
                    match &entry.content {
                        EntryContent::Assistant { text } | EntryContent::Reasoning { text } => {
                            texts.push((entry.id.clone(), text.clone(), turn_running(entry.turn)));
                        }
                        _ => {}
                    }
                }
                // The proposed-plan card renders its markdown too.
                if let Some(plan) = &timeline.proposed_plan {
                    texts.push((
                        format!("plan:{}", plan.item_id),
                        plan.markdown.clone(),
                        turn_running(plan.turn),
                    ));
                }
            }
            (session_key, texts, running, turn_items)
        };

        let session_changed = session_key != self.session_key;
        let list_sync = list_sync(&self.turn_items, &turn_items, session_changed);
        if session_changed {
            self.md_states.clear();
            self.expanded.clear();
            self.editor = None;
            self.session_key = session_key;
        }
        self.turn_items = turn_items;

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

        for (id, text, streaming) in texts {
            match self.md_states.get_mut(&id) {
                Some(md) => md.sync(text, streaming, cx),
                None => {
                    self.md_states
                        .insert(id, MdState::new(&text, streaming, cx));
                }
            }
        }

        // Keep a 1s ticker alive while a turn runs so the live "Working for Ns"
        // counter advances; drop it (cancelling) when nothing is running.
        if running && self._tick.is_none() {
            self._tick = Some(cx.spawn(async move |this, cx| {
                loop {
                    smol::Timer::after(Duration::from_secs(1)).await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }));
        } else if !running {
            self._tick = None;
        }
    }

    fn toggle_expanded(&mut self, turn: usize, key: &str, cx: &mut Context<Self>) {
        if !self.expanded.remove(key) {
            self.expanded.insert(key.to_string());
        }
        // Refresh the cached turn fingerprint immediately. Subagent keys feed
        // `index_turns`, while the direct remeasure below still covers every
        // other collapsible whose state is intentionally not fingerprinted.
        self.sync_markdown_states(cx);
        self.list_state.remeasure_items(turn..turn + 1);
        cx.notify();
    }

    // -- turn rendering -----------------------------------------------------

    /// Render one turn as chronological messages, errors, and Work Log runs.
    ///
    /// `pinned` carries the ids of the last user / last assistant message in the
    /// whole timeline: their action rows stay visible instead of waiting for a
    /// hover, so Copy (and Edit / Revert) are never invisible-and-hover-only.
    fn render_turn(
        &self,
        index: usize,
        turn: &TurnMeta,
        cwd: &Path,
        entries: &[Arc<TimelineEntry>],
        pinned: (Option<&str>, Option<&str>),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut column = v_flex().w_full().gap_4();

        let segmented = segment_entries(entries, turn.running);
        let segments = &segmented.flow;
        let turn_entries: Vec<&TimelineEntry> = entries.iter().map(AsRef::as_ref).collect();
        let turn_counts = work_log_counts(&turn_entries);
        let last_assistant_id = entries.iter().rev().find_map(|entry| {
            matches!(entry.content, EntryContent::Assistant { .. }).then_some(entry.id.as_str())
        });
        let last_segment_is_activity = matches!(segments.last(), Some(Segment::ActivityRun(_)));
        let append_tail_work_log = turn.running && !last_segment_is_activity;
        let last_activity_segment = (!append_tail_work_log)
            .then(|| {
                segments
                    .iter()
                    .rposition(|segment| matches!(segment, Segment::ActivityRun(_)))
            })
            .flatten();

        // Only the turn's FIRST user message opens it — a steered/queued message
        // can join a turn already in progress — so it alone carries the rewind
        // actions (Edit & resend / Revert), whose boundary is the turn.
        let mut head_seen = false;
        for (segment_index, segment) in segments.iter().enumerate() {
            match segment {
                Segment::ActivityRun(activities) => {
                    let segment_id = activities[0].id.as_str();
                    column = column.child(self.render_work_log(
                        index,
                        segment_id,
                        turn,
                        activities,
                        &turn_counts,
                        last_activity_segment == Some(segment_index),
                        cx,
                    ));
                }
                Segment::User(entry) => {
                    let EntryContent::User {
                        text,
                        steering,
                        context_len,
                    } = &entry.content
                    else {
                        unreachable!();
                    };
                    // A child-thread callback (never annotated with a split) is a
                    // centered disclosure row, not a bubble, and carries no action
                    // row — so it does not consume the turn's `head` either.
                    if let Some(callback) = context_len
                        .is_none()
                        .then(|| parse_orchestrate_callback(text))
                        .flatten()
                    {
                        column =
                            column.child(self.render_callback_row(index, &entry.id, &callback, cx));
                    } else {
                        let is_head = !head_seen;
                        head_seen = true;
                        column = column.child(self.render_user(
                            index,
                            &entry.id,
                            text,
                            *context_len,
                            *steering,
                            is_head,
                            pinned.0 == Some(entry.id.as_str()),
                            cx,
                        ));
                    }
                }
                Segment::Assistant(entry) => {
                    let EntryContent::Assistant { text } = &entry.content else {
                        unreachable!();
                    };
                    let streaming =
                        turn.running && last_assistant_id.is_some_and(|id| id == entry.id.as_str());
                    column = column.child(self.render_assistant(
                        &entry.id,
                        text,
                        pinned.1 == Some(entry.id.as_str()),
                        !streaming,
                        cx,
                    ));
                }
                Segment::Error(entry) => {
                    let message = displayed_error_text(&entry.content);
                    column = column.child(self.render_error_card(&entry.id, &message, cx));
                }
            }
        }

        if append_tail_work_log {
            let segment_id = entries
                .last()
                .map(|entry| format!("tail-{}", entry.id))
                .unwrap_or_else(|| "tail".to_string());
            column = column.child(self.render_work_log(
                index,
                &segment_id,
                turn,
                &[],
                &turn_counts,
                true,
                cx,
            ));
        }

        // Proposed-plan card (the captured plan for this turn).
        if let Some((item_id, markdown)) = {
            let state = self.app_state.read(cx);
            state
                .active
                .as_ref()
                .and_then(|a| a.timeline.proposed_plan.as_ref())
                .filter(|plan| plan.turn == index)
                .map(|plan| (plan.item_id.clone(), plan.markdown.clone()))
        } {
            column = column.child(self.render_proposed_plan_card(index, &item_id, &markdown, cx));
        }

        // 4. CHANGED FILES card (aggregated across the turn's file changes).
        // The card only appears once the turn has finished: while a turn runs,
        // file edits are visible as Work Log activity rows and accumulate
        // silently. Replay marks turns idle (mark_idle), so finished turns from
        // stored sessions still render the card.
        if !turn.running {
            let mut changes: Vec<&FileChange> = Vec::new();
            for entry in entries {
                if let EntryContent::FileChange {
                    changes: file_changes,
                    ..
                } = &entry.content
                {
                    changes.extend(file_changes.iter());
                }
            }
            if !changes.is_empty() {
                column = column.child(self.render_changed_files(index, cwd, &changes, cx));
            }
        }

        // 5. Turn timestamp row (finished turns with a known end time).
        if !turn.running
            && let Some(ts) = turn.end_ts.or(entries.last().and_then(|e| e.ts))
        {
            column = column.child(self.render_timestamp(ts, cx));
        }

        // Pending steers float below every live transcript/work-log element.
        // Keeping them separate from `segments` preserves FIFO order without
        // making their request-time position look model-visible.
        for entry in segmented.pending_steers {
            let EntryContent::User {
                text,
                steering,
                context_len,
            } = &entry.content
            else {
                unreachable!();
            };
            column = column.child(self.render_user(
                index,
                &entry.id,
                text,
                *context_len,
                *steering,
                false,
                pinned.0 == Some(entry.id.as_str()),
                cx,
            ));
        }

        column.into_any_element()
    }

    /// A user message: the right-aligned bubble plus its action row (Copy ·
    /// Edit & resend · Revert). The row's height is always reserved, so revealing
    /// it on hover never shifts the timeline; it is revealed for `pinned` (the
    /// last user message) so the actions are reachable without hovering.
    ///
    /// While this message is being edited the bubble is replaced by the inline
    /// editor.
    #[allow(clippy::too_many_arguments)]
    fn render_user(
        &self,
        turn: usize,
        entry_id: &str,
        text: &str,
        context_len: Option<usize>,
        steering: Option<SteeringStatus>,
        is_head: bool,
        pinned: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self
            .editor
            .as_ref()
            .is_some_and(|editor| editor.entry_id == entry_id)
        {
            return self.render_message_editor(turn, cx);
        }

        // An `/orchestrate` turn folds an injected context prefix (guidance +
        // configuration) ahead of the user's own words. Split it off so the
        // bubble — and its Copy / Edit — show only `visible`, while the prefix
        // rides above in a collapsed disclosure row. The provider already
        // received the whole `text`; edit & resend re-routes through
        // `orchestrate_turn`, which re-composes the prefix (see `confirm_edit`).
        let context = context_len
            .filter(|len| *len <= text.len() && text.is_char_boundary(*len))
            .map(|len| &text[..len]);
        let visible = context.map_or(text, |prefix| &text[prefix.len()..]);

        let group_key = SharedString::from(format!("user-{entry_id}"));
        let turn_running = self
            .app_state
            .read(cx)
            .active
            .as_ref()
            .is_some_and(|a| a.timeline.turn_running);
        // Only the message that opened the turn can rewind it, and only a turn
        // with a checkpoint can restore the worktree (Group B).
        let has_checkpoint = is_head && self.app_state.read(cx).turn_has_checkpoint(turn);

        let mut actions = h_flex()
            .gap_1()
            .items_center()
            .justify_end()
            .child(self.render_copy_button(&format!("user:{entry_id}"), Arc::from(visible), cx));

        if is_head {
            let edit_text = visible.to_string();
            let edit_id = entry_id.to_string();
            actions = actions.child(
                Button::new(SharedString::from(format!("edit-{entry_id}")))
                    .ghost()
                    .xsmall()
                    .icon(Icon::empty().path("icons/pencil.svg"))
                    .label(tcode_i18n::tr!("chat.edit_message"))
                    .disabled(turn_running)
                    .tooltip(if turn_running {
                        tcode_i18n::tr!("chat.edit_blocked")
                    } else {
                        tcode_i18n::tr!("chat.edit_message_tooltip")
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.start_edit(turn, edit_id.clone(), edit_text.clone(), window, cx);
                    })),
            );
        }

        if has_checkpoint {
            let app_state = self.app_state.clone();
            actions = actions.child(
                Button::new(SharedString::from(format!("revert-{entry_id}")))
                    .ghost()
                    .xsmall()
                    .icon(Icon::empty().path("icons/rotate-ccw.svg"))
                    .label(tcode_i18n::tr!("checkpoint.revert_action"))
                    .disabled(turn_running)
                    .tooltip(if turn_running {
                        tcode_i18n::tr!("checkpoint.revert_blocked")
                    } else {
                        tcode_i18n::tr!("checkpoint.revert_tooltip")
                    })
                    .on_click(move |_, window, cx| {
                        let app_state = app_state.clone();
                        window.open_alert_dialog(cx, move |alert, _, _| {
                            let app_state = app_state.clone();
                            alert
                                .title(tcode_i18n::tr!("checkpoint.revert_title", turn = turn))
                                .description(tcode_i18n::tr!("checkpoint.revert_description"))
                                .button_props(
                                    DialogButtonProps::default()
                                        .ok_variant(ButtonVariant::Danger)
                                        .ok_text(tcode_i18n::tr!("checkpoint.revert_action"))
                                        .cancel_text(tcode_i18n::tr!("settings.cancel"))
                                        .show_cancel(true),
                                )
                                .on_ok(move |_, _, cx| {
                                    app_state.update(cx, |state, cx| {
                                        state.revert_to_turn(turn, cx);
                                    });
                                    true
                                })
                        });
                    }),
            );
        }

        let bubble = v_flex()
            .group(group_key.clone())
            .w_full()
            .items_end()
            .gap(px(2.))
            .when_some(steering, |column, steering| {
                column.child(
                    div()
                        .px_2()
                        .py(px(1.))
                        .rounded_full()
                        .bg(cx.theme().muted)
                        .text_size(px(11.))
                        .text_color(cx.theme().muted_foreground)
                        .child(match steering {
                            SteeringStatus::Pending => tcode_i18n::tr!("chat.steering"),
                            SteeringStatus::Accepted => tcode_i18n::tr!("chat.steered"),
                        }),
                )
            })
            .child({
                let pending = steering == Some(SteeringStatus::Pending);
                div()
                    .max_w_3_4()
                    .px_4()
                    .py_3()
                    .rounded_xl()
                    .when(pending, |bubble| {
                        bubble
                            .border_1()
                            .border_dashed()
                            .border_color(cx.theme().border)
                    })
                    .when(!pending, |bubble| bubble.bg(cx.theme().muted))
                    .text_color(cx.theme().foreground)
                    .text_size(px(15.))
                    .child(visible.to_string())
            })
            .child(self.reserve_action_row(actions, group_key, pinned));

        let Some(context) = context else {
            return bubble.into_any_element();
        };
        // The injected context sits above the bubble as a centered disclosure row
        // — collapsed by default, expandable to the verbatim prompt source.
        v_flex()
            .w_full()
            .gap_2()
            .child(
                self.render_disclosure(
                    turn,
                    format!("orchestrate-context-{entry_id}"),
                    tcode_i18n::tr!("chat.orchestrate_skill")
                        .into_owned()
                        .into(),
                    context,
                    cx,
                ),
            )
            .child(bubble)
            .into_any_element()
    }

    /// A child-thread orchestrate callback: rendered as a centered disclosure
    /// row (title + localized state) rather than a bubble, with no action row.
    /// Expanding it reveals the callback body (the digest the orchestrator saw).
    fn render_callback_row(
        &self,
        turn: usize,
        entry_id: &str,
        callback: &OrchestrateCallback,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let title = truncate_chars(&callback.title, CALLBACK_TITLE_MAX_CHARS);
        let label = SharedString::from(format!(
            "{title} {}",
            localized_callback_state(&callback.state)
        ));
        let body = if callback.body.trim().is_empty() {
            tcode_i18n::tr!("chat.orchestrate_callback_empty").into_owned()
        } else {
            callback.body.clone()
        };
        self.render_disclosure(
            turn,
            format!("orchestrate-callback-{entry_id}"),
            label,
            &body,
            cx,
        )
    }

    /// The reusable disclosure element: a centered, collapsed-by-default row of
    /// 12px muted `label` + rotating chevron (hover shows an accent background);
    /// clicking toggles the per-entry expansion keyed by `key`. When open it
    /// reveals `full_text` verbatim inside a bordered, height-capped scroll card.
    /// Shared by the orchestrate context split and child-thread callbacks so any
    /// future injected context can adopt the same affordance.
    fn render_disclosure(
        &self,
        turn: usize,
        key: String,
        label: SharedString,
        full_text: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let expanded = self.expanded.contains(&key);
        let muted = cx.theme().muted_foreground;
        let toggle_key = key.clone();
        let row = h_flex()
            .id(SharedString::from(format!("disclosure-{key}")))
            .gap_1()
            .items_center()
            .px_2()
            .py_0p5()
            .rounded(px(8.))
            .text_size(px(12.))
            .text_color(muted)
            .cursor_pointer()
            .hover(|row| row.bg(cx.theme().accent))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_expanded(turn, &toggle_key, cx);
            }))
            .child(label)
            .child(Icon::new(chevron(expanded)).xsmall().text_color(muted));

        let mut block = v_flex().w_full().items_center().gap_1().child(row);
        if expanded {
            block = block.child(self.render_disclosure_body(&key, full_text, cx));
        }
        block.into_any_element()
    }

    /// The expanded body of a disclosure: the injected text rendered verbatim
    /// (line by line, so newlines survive regardless of wrapping) as 13px muted
    /// preformatted source inside a bordered muted card. The guidance can be long,
    /// so the card gets its own resolved-height, capped scroll viewport rather
    /// than growing the turn without bound (DESIGN.md scrolling contract).
    fn render_disclosure_body(
        &self,
        key: &str,
        full_text: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let lines: Vec<AnyElement> = full_text
            .split('\n')
            .map(|line| {
                div()
                    .w_full()
                    .line_height(px(DISCLOSURE_LINE_HEIGHT))
                    .child(if line.is_empty() {
                        " ".to_string()
                    } else {
                        line.to_string()
                    })
                    .into_any_element()
            })
            .collect();
        let line_count = lines.len() as f32;
        let viewport = (line_count * DISCLOSURE_LINE_HEIGHT + DISCLOSURE_CARD_PADDING)
            .min(DISCLOSURE_CARD_MAX_HEIGHT);
        div()
            .id(SharedString::from(format!("disclosure-body-{key}")))
            .w_full()
            .h(px(viewport))
            .overflow_y_scrollbar()
            .p_3()
            .rounded(px(10.))
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().muted)
            .child(
                v_flex()
                    .w_full()
                    .text_size(px(13.))
                    .text_color(muted)
                    .children(lines),
            )
    }

    /// An assistant message: the rendered markdown plus a hover-revealed Copy
    /// action (raw text, not the rendered markdown). Same reserved-height row as
    /// the user bubble, so nothing jumps.
    fn render_assistant(
        &self,
        id: &str,
        text: &str,
        pinned: bool,
        show_actions: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (content, copy_text): (AnyElement, Arc<str>) = if let Some(md) = self.md_states.get(id)
        {
            (
                TextView::new(&md.state).selectable(true).into_any_element(),
                md.synced.clone(),
            )
        } else {
            (
                div().child(text.to_string()).into_any_element(),
                Arc::from(text),
            )
        };
        let message = v_flex().w_full().items_start().gap(px(2.)).child(
            div()
                .w_full()
                .text_size(px(15.))
                .line_height(px(26.))
                .child(content),
        );

        if !show_actions {
            return message.into_any_element();
        }

        let group_key = SharedString::from(format!("assistant-{id}"));
        let actions = h_flex()
            .gap_1()
            .items_center()
            .child(self.render_copy_button(&format!("assistant:{id}"), copy_text, cx));
        message
            .group(group_key.clone())
            .child(self.reserve_action_row(actions, group_key, pinned))
            .into_any_element()
    }

    /// A provider/app error as a first-class timeline block: a danger-tinted
    /// card carrying the FULL message, wrapped across as many lines as it needs,
    /// with a Copy action. Never a one-line ellipsis, never folded into the
    /// collapsing Work Log — a truncated or hidden error is how T3 Code leaves
    /// its users staring at "Request was abo…".
    fn render_error_card(&self, id: &str, message: &str, cx: &mut Context<Self>) -> AnyElement {
        let danger = cx.theme().danger;
        v_flex()
            .w_full()
            .gap_2()
            .p_3()
            .rounded(px(10.))
            .border_1()
            .border_color(danger.opacity(0.35))
            .bg(danger.opacity(0.06))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Icon::new(IconName::TriangleAlert)
                            .xsmall()
                            .text_color(danger),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .font_medium()
                            .text_color(danger)
                            .child(tcode_i18n::tr!("chat.error_label").to_uppercase()),
                    )
                    .child(div().flex_1())
                    .child(self.render_copy_button(&format!("error:{id}"), Arc::from(message), cx)),
            )
            .child(
                div()
                    .w_full()
                    .text_size(px(13.))
                    .line_height(px(20.))
                    .text_color(cx.theme().foreground)
                    .whitespace_normal()
                    .child(message.to_string()),
            )
            .into_any_element()
    }

    /// Wrap a message's action row so it occupies its height whether or not it is
    /// showing (no layout shift on hover) and is revealed by hovering the message
    /// — or unconditionally when `pinned` (the newest message of its kind).
    fn reserve_action_row(
        &self,
        actions: gpui::Div,
        group_key: SharedString,
        pinned: bool,
    ) -> impl IntoElement {
        div()
            .h(px(ACTION_ROW_HEIGHT))
            .flex()
            .items_center()
            .when(!pinned, |this| {
                this.invisible()
                    .group_hover(group_key, |style| style.visible())
            })
            .child(actions)
    }

    /// A Copy button for one message: puts the RAW text on the clipboard and
    /// flips to "Copied!" for 2s (the plan card's confirmation, shared).
    fn render_copy_button(
        &self,
        key: &str,
        text: Arc<str>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let copied = self.copied.as_deref() == Some(key);
        let mark = key.to_string();
        Button::new(SharedString::from(format!("copy-{key}")))
            .ghost()
            .xsmall()
            .icon(if copied {
                IconName::Check
            } else {
                IconName::Copy
            })
            .label(if copied {
                tcode_i18n::tr!("chat.copied")
            } else {
                tcode_i18n::tr!("chat.copy")
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.write_to_clipboard(copy_payload(text.as_ref()));
                this.mark_copied(mark.clone(), cx);
            }))
    }

    // -- edit & resend ------------------------------------------------------

    /// Open the inline editor on a user message, seeded with its original text.
    fn start_edit(
        &mut self,
        turn: usize,
        entry_id: String,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .auto_grow(1, 12)
                .submit_on_enter(true)
                .default_value(text)
        });
        // Enter (without shift) resends; Esc is handled on the editor's wrapper.
        let subscription = cx.subscribe_in(&input, window, move |this, _, event, window, cx| {
            if matches!(event, InputEvent::Change) {
                this.list_state.remeasure_items(turn..turn + 1);
            }
            if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                this.confirm_edit(window, cx);
            }
        });
        input.update(cx, |input, cx| input.focus(window, cx));
        self.list_state
            .splice_focusable(turn..turn + 1, [Some(input.focus_handle(cx))]);
        self.editor = Some(MessageEditor {
            turn,
            entry_id,
            input,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn cancel_edit(&mut self, cx: &mut Context<Self>) {
        if let Some(editor) = self.editor.take() {
            // Replace the item to clear the editor's registered focus handle.
            self.list_state.splice(editor.turn..editor.turn + 1, 1);
        }
        cx.notify();
    }

    /// Resend the edited text: the thread rewinds to the state just before the
    /// edited message (worktree + transcript + provider session) and the new text
    /// is sent as a fresh turn.
    fn confirm_edit(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.take() else {
            return;
        };
        self.list_state.splice(editor.turn..editor.turn + 1, 1);
        let text = editor.input.read(cx).value().trim().to_string();
        if text.is_empty() {
            cx.notify();
            return;
        }
        let turn = editor.turn;
        self.app_state
            .update(cx, |state, cx| state.edit_and_resend_turn(turn, text, cx));
        self.list_state.set_follow_mode(FollowMode::Tail);
        self.list_state.scroll_to_end();
        cx.notify();
    }

    /// The inline editor that replaces a user bubble while it is being edited:
    /// a multi-line input (Enter resends, Esc cancels) + explicit Cancel / Resend.
    fn render_message_editor(&self, turn: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(editor) = self.editor.as_ref().filter(|e| e.turn == turn) else {
            return div().into_any_element();
        };
        let running = self
            .app_state
            .read(cx)
            .active
            .as_ref()
            .is_some_and(|a| a.timeline.turn_running);

        v_flex()
            .id(("message-editor", turn))
            .w_full()
            .gap_2()
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                if ev.keystroke.key.as_str() == "escape" {
                    this.cancel_edit(cx);
                }
            }))
            .child(
                div()
                    .w_full()
                    .px_3()
                    .py_2()
                    .rounded_xl()
                    .border_1()
                    .border_color(cx.theme().primary)
                    .bg(cx.theme().background)
                    .text_size(px(15.))
                    .child(Input::new(&editor.input).appearance(false)),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .justify_end()
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child(tcode_i18n::tr!("chat.edit_hint")),
                    )
                    .child(
                        Button::new("edit-cancel")
                            .ghost()
                            .xsmall()
                            .label(tcode_i18n::tr!("settings.cancel"))
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_edit(cx))),
                    )
                    .child(
                        Button::new("edit-resend")
                            .primary()
                            .xsmall()
                            .label(tcode_i18n::tr!("chat.resend"))
                            .disabled(running)
                            .tooltip(if running {
                                tcode_i18n::tr!("chat.edit_blocked")
                            } else {
                                tcode_i18n::tr!("chat.resend_tooltip")
                            })
                            .on_click(
                                cx.listener(|this, _, window, cx| this.confirm_edit(window, cx)),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// The Work Log section: activity rows (collapsible) and an event-count
    /// summary footer (or a live "Working for Ns" indicator).
    ///
    /// It used to hang off a hairline that ran right under the user bubble; the
    /// rule is gone. Turns read as separate because of the space around them
    /// (`TURN_GAP`) and the uppercase 11px label that opens the section — rhythm
    /// and hierarchy, not another line.
    #[allow(clippy::too_many_arguments)]
    fn render_work_log(
        &self,
        index: usize,
        segment_id: &str,
        turn: &TurnMeta,
        activities: &[&TimelineEntry],
        turn_counts: &WorkLogCounts,
        is_last: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let section_key = format!("worklog-{index}-{segment_id}");
        let rows_key = format!("worklog-rows-{index}-{segment_id}");
        let running = is_last && turn.running;
        // Only the final segment can be live; finished segments collapse by default.
        let expanded = running || self.expanded.contains(&section_key);
        let muted = cx.theme().muted_foreground;
        let subagent_count = activities
            .iter()
            .filter(|entry| matches!(entry.content, EntryContent::Subagent { .. }))
            .count();
        let segment_counts = work_log_counts(activities);

        let mut section = v_flex().w_full().gap_2();

        if expanded {
            if !running || subagent_count > 0 {
                section = section.child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .text_size(px(11.))
                        .font_medium()
                        .text_color(muted)
                        .child(tcode_i18n::tr!("chat.work_log").to_uppercase())
                        .when(subagent_count > 0, |row| {
                            row.child(
                                div()
                                    .px_2()
                                    .py(px(1.))
                                    .rounded_full()
                                    .bg(cx.theme().muted)
                                    .child(tcode_i18n::tr!(
                                        "chat.subagent_count",
                                        count = subagent_count
                                    )),
                            )
                        }),
                );
            }

            let display_activities: Vec<&TimelineEntry> = activities
                .iter()
                .copied()
                .filter(|entry| !matches!(entry.content, EntryContent::FileChange { .. }))
                .collect();
            let total = display_activities.len();
            let rows_expanded = self.expanded.contains(&rows_key);
            let hidden = total.saturating_sub(WORKLOG_VISIBLE_ROWS);
            let visible: Vec<&TimelineEntry> = if rows_expanded || hidden == 0 {
                display_activities
            } else {
                display_activities[total - WORKLOG_VISIBLE_ROWS..].to_vec()
            };

            for entry in &visible {
                section = section.child(self.render_activity_row(entry, false, cx));
            }

            if let Some(toggle_label) = previous_logs_toggle_label(hidden, rows_expanded) {
                section = section.child(
                    h_flex()
                        .id(SharedString::from(format!(
                            "worklog-more-{index}-{segment_id}"
                        )))
                        .gap_1()
                        .items_center()
                        .py_0p5()
                        .text_size(px(13.))
                        .text_color(muted)
                        .cursor_pointer()
                        .hover(|s| s.text_color(cx.theme().foreground))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_expanded(index, &rows_key, cx);
                        }))
                        .child(
                            Icon::new(if rows_expanded {
                                IconName::ChevronUp
                            } else {
                                IconName::ChevronDown
                            })
                            .xsmall(),
                        )
                        .child(toggle_label),
                );
            }
        }

        // Footer: live "Working" indicator, or a toggleable nonzero event summary.
        if running {
            let secs = turn
                .start_ts
                .map(|start| now_millis().saturating_sub(start) / 1000)
                .unwrap_or(0);
            section = section.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .text_size(px(13.))
                    .text_color(muted)
                    .child(div().text_color(cx.theme().primary).child("•••"))
                    .child(tcode_i18n::tr!(
                        "chat.working_for",
                        duration = format_duration(secs)
                    )),
            );
        } else if let Some(label) = finished_work_log_label(is_last, &segment_counts, turn_counts) {
            section = section.child(
                h_flex()
                    .id(SharedString::from(format!(
                        "worklog-footer-{index}-{segment_id}"
                    )))
                    .gap_1()
                    .items_center()
                    .text_size(px(13.))
                    .text_color(muted)
                    .cursor_pointer()
                    .hover(|s| s.text_color(cx.theme().foreground))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_expanded(index, &section_key, cx);
                    }))
                    .child(label)
                    .when(subagent_count > 0 && !expanded && !is_last, |row| {
                        row.child(
                            div()
                                .px_2()
                                .py(px(1.))
                                .rounded_full()
                                .bg(cx.theme().muted)
                                .text_size(px(11.))
                                .child(tcode_i18n::tr!(
                                    "chat.subagent_count",
                                    count = subagent_count
                                )),
                        )
                    })
                    .child(Icon::new(chevron(expanded)).xsmall()),
            );
        }

        section.into_any_element()
    }

    /// One Work Log activity row: a muted status icon + a one-line summary.
    fn render_activity_row(
        &self,
        entry: &TimelineEntry,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if matches!(&entry.content, EntryContent::Subagent { .. }) {
            return self.render_subagent_row(entry, cx);
        }
        let muted = cx.theme().muted_foreground;
        let (icon, summary): (IconName, AnyElement) = match &entry.content {
            EntryContent::Command {
                command, status, ..
            } => {
                let icon = if matches!(status, ItemStatus::InProgress) {
                    IconName::SquareTerminal
                } else {
                    IconName::Check
                };
                let summary = h_flex()
                    .min_w_0()
                    .flex_1()
                    .gap_1()
                    .overflow_hidden()
                    .child(div().flex_none().child(tcode_i18n::tr!("chat.command_run")))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_color(muted)
                            .font_family(cx.theme().mono_font_family.clone())
                            .child(one_line(command)),
                    )
                    .into_any_element();
                (icon, summary)
            }
            EntryContent::Tool {
                name,
                input,
                output,
                status,
            } => {
                // Prefer an input summary; fall back to a snippet of the output.
                let mut brief = tool_brief(input);
                if brief.is_empty()
                    && let Some(output) = output
                {
                    brief = one_line(output);
                }
                let summary = h_flex()
                    .min_w_0()
                    .flex_1()
                    .gap_1()
                    .overflow_hidden()
                    .child(div().flex_none().child(name.clone()))
                    .when(!brief.is_empty(), |this| {
                        this.child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .text_color(muted)
                                .child(brief),
                        )
                    })
                    .into_any_element();
                (activity_icon(*status), summary)
            }
            EntryContent::Reasoning { text } => {
                let summary = h_flex()
                    .min_w_0()
                    .flex_1()
                    .gap_1()
                    .overflow_hidden()
                    .child(div().flex_none().child(tcode_i18n::tr!("chat.thinking")))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_color(muted)
                            .child(one_line(text)),
                    )
                    .into_any_element();
                (IconName::Check, summary)
            }
            EntryContent::ContextCompacted => {
                let summary = div()
                    .min_w_0()
                    .flex_1()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_color(muted)
                    .child(tcode_i18n::tr!("chat.context_compacted"))
                    .into_any_element();
                (IconName::Minimize, summary)
            }
            // Non-activity content never reaches this row.
            _ => (IconName::Check, div().into_any_element()),
        };

        h_flex()
            .w_full()
            .gap_2()
            .items_center()
            .when(!compact, |row| row.py_0p5())
            .text_size(px(if compact { 12. } else { 13. }))
            .child(Icon::new(icon).xsmall().text_color(muted))
            .child(summary)
            .into_any_element()
    }

    fn render_subagent_row(&self, entry: &TimelineEntry, cx: &mut Context<Self>) -> AnyElement {
        let EntryContent::Subagent {
            agent_type,
            description,
            status,
            summary,
        } = &entry.content
        else {
            unreachable!();
        };
        let key = format!("subagent-{}", entry.id);
        let expanded = self.expanded.contains(&key);
        let parent_id = entry.id.clone();
        let (children, truncated) = {
            let state = self.app_state.read(cx);
            state
                .active
                .as_ref()
                .map(|active| {
                    (
                        active.timeline.children(&parent_id).to_vec(),
                        active.timeline.children_truncated(&parent_id),
                    )
                })
                .unwrap_or_default()
        };
        let muted = cx.theme().muted_foreground;
        let finished = !matches!(status, ItemStatus::InProgress);
        let turn = entry.turn;
        let click_key = key.clone();
        let mut row = h_flex()
            .id(SharedString::from(format!("subagent-row-{}", entry.id)))
            .w_full()
            .min_w_0()
            .gap_2()
            .items_center()
            .py_0p5()
            .text_size(px(13.))
            .cursor_pointer()
            .hover(|row| row.text_color(cx.theme().foreground))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_expanded(turn, &click_key, cx);
            }))
            .child(Icon::new(activity_icon(*status)).xsmall().text_color(muted))
            .child(div().flex_none().font_medium().child(agent_type.clone()))
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_color(muted)
                    .child(one_line(description)),
            );
        if finished && let Some(summary) = summary.as_deref().filter(|summary| !summary.is_empty())
        {
            row = row.child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_color(muted)
                    .child(one_line(summary)),
            );
        }
        row = row.child(Icon::new(chevron(expanded)).xsmall().text_color(muted));

        let mut block = v_flex().w_full().gap_1().child(row);
        if expanded {
            let mut nested = v_flex()
                .w_full()
                .gap_1()
                .ml_2()
                .pl_3()
                .py_1()
                .border_l_1()
                .border_color(cx.theme().border);
            if truncated {
                nested = nested.child(
                    div()
                        .text_size(px(11.))
                        .text_color(muted)
                        .child(tcode_i18n::tr!("chat.earlier_steps_truncated")),
                );
            }
            for child in &children {
                nested = nested.child(self.render_subagent_child(child, cx));
            }
            block = block.child(nested);
        }
        block.into_any_element()
    }

    fn render_subagent_child(&self, entry: &TimelineEntry, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        match &entry.content {
            EntryContent::User { text, .. } => h_flex()
                .w_full()
                .justify_end()
                .child(
                    div()
                        .max_w_3_4()
                        .px_2()
                        .py_1()
                        .rounded_lg()
                        .bg(cx.theme().muted)
                        .text_size(px(12.))
                        .text_color(cx.theme().foreground)
                        .child(text.clone()),
                )
                .into_any_element(),
            EntryContent::Assistant { text } => div()
                .w_full()
                .text_size(px(12.))
                .line_height(px(19.))
                .text_color(cx.theme().foreground)
                .child(text.clone())
                .into_any_element(),
            EntryContent::Error { .. } | EntryContent::ProviderStartError { .. } => div()
                .w_full()
                .text_size(px(12.))
                .text_color(cx.theme().danger)
                .child(displayed_error_text(&entry.content).into_owned())
                .into_any_element(),
            EntryContent::FileChange { changes } => div()
                .text_size(px(12.))
                .text_color(muted)
                .child(tcode_i18n::tr!("chat.changed_files", count = changes.len()))
                .into_any_element(),
            _ => self.render_activity_row(entry, true, cx),
        }
    }

    /// The CHANGED FILES card: header with totals + a directory-grouped tree.
    fn render_changed_files(
        &self,
        index: usize,
        cwd: &Path,
        changes: &[&FileChange],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let card_key = format!("card-{index}");
        let collapsed = self.expanded.contains(&card_key);

        let (total_add, total_del): (u32, u32) = changes
            .iter()
            .map(|c| diff_stats(c.diff.as_deref()))
            .fold((0, 0), |(a, d), (ca, cd)| (a + ca, d + cd));

        let header = h_flex()
            .w_full()
            .px_4()
            .py_2()
            .gap_2()
            .items_center()
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1p5()
                    .items_center()
                    .text_size(px(11.))
                    .font_medium()
                    .text_color(muted)
                    .child(tcode_i18n::tr!("chat.changed_files", count = changes.len()))
                    .child("·")
                    .child(
                        div()
                            .text_color(cx.theme().success)
                            .child(format!("+{total_add}")),
                    )
                    .child(
                        div()
                            .text_color(cx.theme().danger)
                            .child(format!("-{total_del}")),
                    ),
            )
            .child(
                Button::new(("collapse-all", index))
                    .ghost()
                    .xsmall()
                    .label(if collapsed {
                        tcode_i18n::tr!("chat.expand_all")
                    } else {
                        tcode_i18n::tr!("chat.collapse_all")
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_expanded(index, &card_key, cx);
                    })),
            )
            .child(
                Button::new(("view-diff", index))
                    .outline()
                    .xsmall()
                    .label(tcode_i18n::tr!("chat.view_diff"))
                    .tooltip(tcode_i18n::tr!("chat.view_diff_tooltip"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.app_state
                            .update(cx, |state, cx| state.open_diff_for_turn(index, cx));
                    })),
            );

        let mut card = v_flex()
            .w_full()
            .rounded(px(10.))
            .border_1()
            .border_color(cx.theme().border)
            .overflow_hidden()
            .child(header);

        if !collapsed {
            let mut body = v_flex().w_full().px_2().pb_2().gap(px(1.));
            for (dir, files) in group_by_dir(changes, cwd) {
                let dir_add: u32 = files.iter().map(|f| f.added).sum();
                let dir_del: u32 = files.iter().map(|f| f.deleted).sum();
                if !dir.is_empty() {
                    body = body.child(
                        h_flex()
                            .w_full()
                            .px_2()
                            .py_1()
                            .gap_1p5()
                            .items_center()
                            .text_size(px(13.))
                            .child(Icon::new(IconName::ChevronDown).xsmall().text_color(muted))
                            .child(Icon::new(IconName::Folder).xsmall().text_color(muted))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .child(dir.clone()),
                            )
                            .child(diff_counts(dir_add, dir_del, cx)),
                    );
                }
                for file in files {
                    body = body.child(
                        h_flex()
                            .w_full()
                            .pl(px(if dir.is_empty() { 8. } else { 28. }))
                            .pr_2()
                            .py_1()
                            .gap_1p5()
                            .items_center()
                            .text_size(px(13.))
                            .child(Icon::new(IconName::File).xsmall().text_color(muted))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .child(file.name),
                            )
                            .child(diff_counts(file.added, file.deleted, cx)),
                    );
                }
            }
            card = card.child(body);
        }

        card.into_any_element()
    }

    /// The inline proposed-plan timeline card (S1 §5): a "Plan" badge, title,
    /// markdown body (collapsible when long), and Copy / Download / Save actions.
    fn render_proposed_plan_card(
        &self,
        turn: usize,
        item_id: &str,
        markdown: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let title = tcode_core::session::plan_title(markdown)
            .unwrap_or_else(|| tcode_i18n::tr!("plan.proposed_plan").into_owned());
        let long = markdown.chars().count() > 900 || markdown.lines().count() > 20;
        let collapse_key = format!("plan-card-{turn}");
        let collapsed = long && self.expanded.contains(&collapse_key);

        let body: AnyElement = if collapsed {
            div().into_any_element()
        } else if let Some(md) = self.md_states.get(&format!("plan:{item_id}")) {
            div()
                .w_full()
                .text_size(px(14.))
                .line_height(px(22.))
                .child(TextView::new(&md.state).selectable(true))
                .into_any_element()
        } else {
            div()
                .w_full()
                .child(markdown.to_string())
                .into_any_element()
        };

        let md_copy = markdown.to_string();
        let md_download = markdown.to_string();
        let md_save = markdown.to_string();
        let copied = self.copied.as_deref() == Some("plan");

        v_flex()
            .w_full()
            .gap_2()
            .p_4()
            .rounded(px(12.))
            .border_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .px_2()
                            .py(px(1.))
                            .rounded(px(4.))
                            .bg(cx.theme().primary)
                            .text_color(cx.theme().primary_foreground)
                            .text_size(px(11.))
                            .font_medium()
                            .child(tcode_i18n::tr!("plan.badge")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(15.))
                            .font_semibold()
                            .child(title),
                    )
                    .when(long, |this| {
                        let key = collapse_key.clone();
                        this.child(
                            Button::new(("plan-collapse", turn))
                                .ghost()
                                .xsmall()
                                .label(if collapsed {
                                    tcode_i18n::tr!("plan.expand")
                                } else {
                                    tcode_i18n::tr!("plan.collapse")
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.toggle_expanded(turn, &key, cx);
                                })),
                        )
                    }),
            )
            .child(body)
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .flex_wrap()
                    .child(
                        Button::new(("plan-copy", turn))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Copy)
                            .label(if copied {
                                tcode_i18n::tr!("plan.copied")
                            } else {
                                tcode_i18n::tr!("plan.copy")
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let md = md_copy.clone();
                                this.app_state.update(cx, |s, cx| s.copy_plan(md, cx));
                                this.mark_copied("plan".into(), cx);
                            })),
                    )
                    .child(
                        Button::new(("plan-download", turn))
                            .ghost()
                            .xsmall()
                            .icon(Icon::empty().path("icons/download.svg"))
                            .label(tcode_i18n::tr!("plan.download"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let md = md_download.clone();
                                let fallback = tcode_i18n::tr!("plan.proposed_plan").into_owned();
                                this.app_state
                                    .update(cx, |s, cx| s.download_plan(md, fallback, cx));
                            })),
                    )
                    .child(
                        Button::new(("plan-save", turn))
                            .ghost()
                            .xsmall()
                            .icon(IconName::HardDrive)
                            .label(tcode_i18n::tr!("plan.save_workspace"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let md = md_save.clone();
                                this.app_state
                                    .update(cx, |s, cx| s.save_plan_to_workspace(md, cx));
                            })),
                    ),
            )
            .into_any_element()
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

    fn render_timestamp(&self, ts: u64, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .w_full()
            .gap_1p5()
            .items_center()
            .text_size(px(12.))
            .text_color(cx.theme().muted_foreground)
            .child(Icon::new(IconName::Info).xsmall())
            .child(format_local_time(ts))
            .into_any_element()
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
        // With the sidebar collapsed to its 48px strip, the native traffic
        // lights (which sit at the window's top-left) overhang into the chat
        // header — inset the header content so the title clears them.
        let collapsed = self.app_state.read(cx).sidebar_collapsed;
        let base = h_flex()
            .flex_shrink_0()
            .h(px(52.))
            .px_4()
            .when(collapsed, |this| this.pl(px(36.)))
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().border);

        // A draft shows a muted "New thread" label; an open thread its title;
        // nothing active shows "No active thread".
        let title_el = if is_draft {
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(16.))
                .font_medium()
                .text_color(cx.theme().muted_foreground)
                .child(tcode_i18n::tr!("chat.new_thread"))
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
                    .text_size(px(16.))
                    .font_medium()
                    .child(title.clone()),
                None => div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(16.))
                    .font_medium()
                    .text_color(cx.theme().muted_foreground)
                    .child(tcode_i18n::tr!("chat.no_active_thread")),
            }
        };

        // The right-side cluster (Open split-button + panel toggles) shows for
        // any active thread, including a draft.
        let show_actions = is_draft || title.is_some();
        let diff_showing = {
            let state = self.app_state.read(cx);
            state.diff_panel_open() && state.right_tab() == RightTab::Diff
        };
        let plan_showing = self.app_state.read(cx).plan_panel_showing();
        let preview_showing = self.app_state.read(cx).preview_panel_showing();
        let terminal_open = self.app_state.read(cx).terminal_panel_open();
        window_drag_area("chat-header-drag", base, window, cx)
            .child(title_el)
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
                                    .tooltip(tcode_i18n::tr!("chat.toggle_terminal"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.app_state.update(cx, |state, cx| {
                                            state.toggle_terminal_panel(cx)
                                        });
                                    })),
                            )
                            .child(
                                Button::new("plan-panel")
                                    .ghost()
                                    .small()
                                    .compact()
                                    .icon(IconName::Map)
                                    .selected(plan_showing)
                                    .tooltip(tcode_i18n::tr!("chat.toggle_plan"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.app_state
                                            .update(cx, |state, cx| state.toggle_plan_panel(cx));
                                    })),
                            )
                            .child(
                                Button::new("preview-panel")
                                    .ghost()
                                    .small()
                                    .compact()
                                    .icon(IconName::Globe)
                                    .selected(preview_showing)
                                    .tooltip(tcode_i18n::tr!("chat.toggle_preview"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.app_state
                                            .update(cx, |state, cx| state.toggle_preview_panel(cx));
                                    })),
                            )
                            .child(
                                Button::new("diff-panel")
                                    .ghost()
                                    .small()
                                    .compact()
                                    .icon(IconName::PanelRight)
                                    .selected(diff_showing)
                                    .tooltip(tcode_i18n::tr!("chat.toggle_diff"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.app_state
                                            .update(cx, |state, cx| state.toggle_diff_panel(cx));
                                    })),
                            ),
                    )
            })
            .into_any_element()
    }

    /// The adaptive Git quick-action split-button (left of Open): the primary
    /// action follows the background git status (Commit / Commit & push / Push /
    /// Pull / Publish branch / Initialize Git, or a disabled status hint); the
    /// chevron lists the applicable subset. Ported from T3's `GitActionsControl`.
    fn render_git_button(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let quick = self.app_state.read(cx).git_quick_action()?;
        let border = cx.theme().border;
        let items = self.app_state.read(cx).git_menu_items();

        // Main action segment.
        let label: SharedString = tcode_i18n::tr!(git_action_label_key(quick.label))
            .into_owned()
            .into();
        let main_icon = quick
            .action
            .map(git_action_icon)
            .unwrap_or_else(|| Icon::empty().path("icons/git-branch.svg"));
        let mut main = h_flex()
            .id("git-main")
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
                let text: SharedString = tcode_i18n::tr!(git_hint_key(hint)).into_owned().into();
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
        let chevron = Popover::new("git-menu")
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
                    let label: SharedString = tcode_i18n::tr!(git_action_label_key(item.action))
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
                                tcode_i18n::tr!(git_hint_key(hint)).into_owned().into();
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
            self.app_state.update(cx, |state, cx| {
                state.run_git_action(action, None, None, None, cx)
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
        let dialog = cx.new(|cx| CommitDialog::new(self.app_state.clone(), action, window, cx));
        self.commit_dialog = Some(dialog.clone());
        window.open_dialog(cx, move |dlg, window, cx| {
            let content = dialog.clone();
            let footer_dialog = dialog.clone();
            dlg.title(tcode_i18n::tr!("git.commit.title").into_owned())
                .w(px(600.))
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

        let chevron = Popover::new("open-menu")
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
                            tcode_i18n::tr!("chat.open_zed").into_owned().into(),
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
                            tcode_i18n::tr!("chat.reveal_in_file_manager")
                                .into_owned()
                                .into(),
                        )
                        .on_click(move |_, window, cx| {
                            reveal_in_file_manager(&reveal_cwd, cx);
                            p2.update(cx, |st, cx| st.dismiss(window, cx));
                        }),
                    )
                    .child(
                        menu_item(
                            "copy-path",
                            IconName::Copy,
                            tcode_i18n::tr!("chat.copy_path").into_owned().into(),
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
                h_flex()
                    .id("open-main")
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
                    .child(tcode_i18n::tr!("chat.open"))
                    .on_click(cx.listener(move |_, _, window, cx| {
                        open_in_zed(&main_cwd, window, cx);
                    })),
            )
            .child(div().w_px().h(px(16.)).bg(border))
            .child(chevron)
            .into_any_element()
    }

    fn render_empty_state(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .gap_1()
            .child(
                div()
                    .text_size(px(20.))
                    .font_semibold()
                    .child(tcode_i18n::tr!("chat.empty_title")),
            )
            .child(
                div()
                    .text_size(px(14.))
                    .text_color(cx.theme().muted_foreground)
                    .child(tcode_i18n::tr!("chat.empty_description")),
            )
            .into_any_element()
    }

    fn render_scroll_pill(&self, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .flex_shrink_0()
            .w_full()
            .justify_center()
            .pb_1()
            .child(
                Button::new("scroll-to-end")
                    .outline()
                    .small()
                    .icon(IconName::ChevronDown)
                    .label(tcode_i18n::tr!("chat.scroll_end"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.list_state.set_follow_mode(FollowMode::Tail);
                        this.list_state.scroll_to_end();
                        cx.notify();
                    })),
            )
            .into_any_element()
    }
}

impl Render for ChatView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Screenshot-only: `--debug-git-dialog` opens the commit dialog once the
        // background git status has landed (a header click is not drivable
        // headlessly). Consumed once.
        let open_commit_dialog = self.app_state.update(cx, |state, _| {
            let armed = state.debug_open_commit_dialog && state.git_status.is_some();
            if armed {
                state.debug_open_commit_dialog = false;
            }
            armed
        });
        if open_commit_dialog {
            self.open_commit_dialog(GitAction::Commit, window, cx);
        }

        // Screenshot-only: `--debug-edit-open` opens the inline message editor on
        // the last user message (a hover + click is not drivable headlessly).
        // Consumed once.
        let open_editor = self.app_state.update(cx, |state, _| {
            let armed = state.debug_edit_open && state.active.is_some();
            if armed {
                state.debug_edit_open = false;
            }
            armed
        });
        if open_editor
            && let Some((turn, id, text)) = self.app_state.read(cx).active.as_ref().and_then(|a| {
                a.timeline
                    .entries
                    .iter()
                    .rev()
                    .find_map(|e| match &e.content {
                        EntryContent::User { text, .. } => {
                            Some((e.turn, e.id.clone(), text.clone()))
                        }
                        _ => None,
                    })
            })
        {
            self.start_edit(turn, id, text, window, cx);
        }

        let active = {
            let state = self.app_state.read(cx);
            state.active.as_ref().map(|active| {
                (
                    active.meta.title.clone(),
                    active.meta.cwd.clone(),
                    active.draft,
                )
            })
        };

        let root = v_flex().size_full().min_w_0().bg(cx.theme().background);

        let Some((title, cwd, is_draft)) = active else {
            return root
                .child(self.render_header(None, false, None, window, cx))
                .child(self.render_empty_state(cx));
        };

        let title = if is_draft { None } else { Some(title) };
        let header = self.render_header(title, is_draft, Some(cwd.clone()), window, cx);
        let terminal_open = self.app_state.read(cx).terminal_panel_open();
        let terminal_height = self
            .app_state
            .read(cx)
            .active
            .as_ref()
            .map(|a| a.terminal_workspace.height)
            .unwrap_or(240.);

        // Group entries by turn and render each turn section into the centered
        // content column. The column fills the available width up to
        // `CONTENT_MAX_WIDTH`; horizontal padding lives on the centering wrapper
        // (below) so the column shrinks gracefully — never clipping — when the
        // diff panel narrows the chat region.
        // The newest user / assistant message: their action rows stay visible
        // (hover is not the only way to reach Copy / Edit / Revert).
        let (last_user_id, last_assistant_id) = {
            let state = self.app_state.read(cx);
            let entries = &state
                .active
                .as_ref()
                .expect("active session")
                .timeline
                .entries;
            (
                entries
                    .iter()
                    .rev()
                    .find(|entry| matches!(entry.content, EntryContent::User { .. }))
                    .map(|entry| entry.id.clone()),
                entries
                    .iter()
                    .rev()
                    .find(|entry| matches!(entry.content, EntryContent::Assistant { .. }))
                    .map(|entry| entry.id.clone()),
            )
        };

        let item_count = self.turn_items.len();
        let item_cwd = cwd.clone();
        let timeline = list(
            self.list_state.clone(),
            cx.processor(move |this, index: usize, _window, cx| {
                let Some(item) = this.turn_items.get(index) else {
                    return div().into_any_element();
                };
                // Clone only the handful of entries in this visible/overdrawn
                // turn. The full history remains in AppState and is never
                // cloned by the render path.
                let Some((turn, entries)) = this.app_state.read(cx).active.as_ref().map(|active| {
                    (
                        active
                            .timeline
                            .turns
                            .get(index)
                            .cloned()
                            .unwrap_or_default(),
                        active.timeline.entries[item.entry_range.clone()].to_vec(),
                    )
                }) else {
                    return div().into_any_element();
                };
                let rendered = this.render_turn(
                    index,
                    &turn,
                    &item_cwd,
                    &entries,
                    (last_user_id.as_deref(), last_assistant_id.as_deref()),
                    cx,
                );
                h_flex()
                    .w_full()
                    .justify_center()
                    .px(px(CONTENT_MIN_PADDING))
                    .when(index + 1 < item_count, |item| item.pb(px(TURN_GAP)))
                    .child(div().w_full().max_w(px(CONTENT_MAX_WIDTH)).child(rendered))
                    .into_any_element()
            }),
        )
        .with_sizing_behavior(gpui::ListSizingBehavior::Auto)
        .flex_1()
        .min_h_0()
        .py_6();

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
                    .child(timeline),
            )
            .when(
                self.list_state.is_scrolled_to_end() == Some(false),
                |this| this.child(self.render_scroll_pill(cx)),
            )
            .child(self.composer.clone());

        let body: AnyElement = if terminal_open {
            let drawer = self.terminal_drawer.clone();
            let drawer_resize = self.terminal_drawer.clone();
            let width = f32::from(window.bounds().size.width);
            drawer.update(cx, |drawer, cx| drawer.resize(width, terminal_height, cx));
            gpui_component::resizable::v_resizable("chat-terminal-panels")
                .on_resize(move |state, _, cx| {
                    let height = state.read(cx).sizes().get(1).copied();
                    if let Some(height) = height {
                        drawer_resize
                            .update(cx, |drawer, cx| drawer.resize(width, f32::from(height), cx));
                    }
                })
                .child(gpui_component::resizable::resizable_panel().child(main))
                .child(
                    gpui_component::resizable::resizable_panel()
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

/// The clipboard payload of a message's Copy action: the message's **raw text**
/// (the markdown source a message was written in / streamed as), never the
/// rendered document the timeline draws. See `copy_puts_raw_text_on_the_clipboard`.
fn copy_payload(text: &str) -> ClipboardItem {
    ClipboardItem::new_string(text.to_string())
}

/// The localized state word for a callback disclosure row (`completed` /
/// `failed`), falling back to the raw provider word for anything unexpected.
fn localized_callback_state(state: &str) -> Cow<'static, str> {
    match state {
        "completed" => tcode_i18n::tr!("chat.orchestrate_state_completed"),
        "failed" => tcode_i18n::tr!("chat.orchestrate_state_failed"),
        other => Cow::Owned(other.to_string()),
    }
}

/// Truncate `text` to at most `max` characters (collapsing any newlines first),
/// appending an ellipsis when it was shortened.
fn truncate_chars(text: &str, max: usize) -> String {
    let text = one_line(text);
    if text.chars().count() <= max {
        return text;
    }
    let head: String = text.chars().take(max).collect();
    format!("{head}…")
}

fn chevron(open: bool) -> IconName {
    if open {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    }
}

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
                .label(tcode_i18n::tr!("git.commit.cancel"))
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
    if tcode_runtime::ui_facade::open_in_zed(cwd).is_err() {
        window.push_notification(
            Notification::error(tcode_i18n::tr!("errors.zed_cli_missing")),
            cx,
        );
    }
}

/// Reveal `cwd` in the platform's file manager (Finder / Explorer / the XDG
/// file manager). gpui does the platform dispatch, so no shell-out is needed.
fn reveal_in_file_manager(cwd: &Path, cx: &mut App) {
    cx.reveal_path(cwd);
}

/// Leading icon for a Work Log activity row, keyed on the item's status.
fn activity_icon(status: ItemStatus) -> IconName {
    match status {
        ItemStatus::InProgress => IconName::LoaderCircle,
        ItemStatus::Completed => IconName::Check,
        ItemStatus::Failed | ItemStatus::Declined => IconName::CircleX,
    }
}

/// First non-empty line of `text`, collapsed to a single spaced line.
fn one_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

/// A short one-line summary of a tool call's input for the Work Log.
fn tool_brief(input: &serde_json::Value) -> String {
    match input {
        serde_json::Value::Object(map) => map
            .get("query")
            .or_else(|| map.get("path"))
            .or_else(|| map.get("command"))
            .or_else(|| map.get("summary"))
            .and_then(|v| v.as_str())
            .map(one_line)
            .unwrap_or_default(),
        serde_json::Value::String(s) => one_line(s),
        _ => String::new(),
    }
}

/// Wall-clock duration formatted as "XmYYs" / "YYs".
fn format_duration(secs: u64) -> String {
    if secs >= 60 {
        tcode_i18n::tr!(
            "time.duration_minutes",
            minutes = secs / 60,
            seconds = format!("{:02}", secs % 60)
        )
        .into_owned()
    } else {
        tcode_i18n::tr!("time.duration_seconds", seconds = secs).into_owned()
    }
}

/// Count added / removed lines in a unified diff (ignoring the `+++`/`---`
/// file headers).
fn diff_stats(diff: Option<&str>) -> (u32, u32) {
    let Some(diff) = diff else {
        return (0, 0);
    };
    let mut added = 0;
    let mut removed = 0;
    for line in diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        match line.as_bytes().first() {
            Some(b'+') => added += 1,
            Some(b'-') => removed += 1,
            _ => {}
        }
    }
    (added, removed)
}

struct FileRow {
    name: String,
    added: u32,
    deleted: u32,
}

/// Group file changes by their parent directory (preserving first-seen order),
/// so the CHANGED FILES card can render a folder → files tree. Paths are shown
/// relative to the session `cwd` when they live under it.
fn group_by_dir(changes: &[&FileChange], cwd: &Path) -> Vec<(String, Vec<FileRow>)> {
    let mut groups: Vec<(String, Vec<FileRow>)> = Vec::new();
    for change in changes {
        let display = tcode_runtime::ui_facade::relativize_to_workspace(&change.path, cwd);
        let (dir, name) = match display.rsplit_once('/') {
            Some((dir, name)) => (dir.to_string(), name.to_string()),
            None => (String::new(), display.clone()),
        };
        let (added, deleted) = diff_stats(change.diff.as_deref());
        let row = FileRow {
            name,
            added,
            deleted,
        };
        if let Some(group) = groups.iter_mut().find(|(d, _)| *d == dir) {
            group.1.push(row);
        } else {
            groups.push((dir, vec![row]));
        }
    }
    groups
}

fn diff_counts(added: u32, deleted: u32, cx: &Context<ChatView>) -> AnyElement {
    h_flex()
        .flex_none()
        .gap_2()
        .text_size(px(12.))
        .child(
            div()
                .text_color(cx.theme().success)
                .child(format!("+{added}")),
        )
        .child(
            div()
                .text_color(cx.theme().danger)
                .child(format!("-{deleted}")),
        )
        .into_any_element()
}

/// Format a unix-ms timestamp as a local 12-hour clock, e.g. "2:39 AM".
///
/// `chrono::Local` reads the platform's timezone (Unix: the tz database /
/// `localtime_r`; Windows: the OS timezone API), so this is correct on all three
/// targets — unlike the hand-rolled `localtime_r` FFI it replaces, whose `tm`
/// layout was UB on 32-bit and which fell back to a UTC clock on Windows.
fn format_local_time(unix_ms: u64) -> String {
    use chrono::{Local, TimeZone as _, Timelike as _};

    let Some(local) = Local.timestamp_millis_opt(unix_ms as i64).single() else {
        return String::new();
    };
    twelve_hour(local.hour() as i32, local.minute() as i32)
}

fn twelve_hour(hour24: i32, minute: i32) -> String {
    let (hour12, meridiem) = match hour24 {
        0 => (12, "AM"),
        1..=11 => (hour24, "AM"),
        12 => (12, "PM"),
        _ => (hour24 - 12, "PM"),
    };
    format!("{hour12}:{minute:02} {meridiem}")
}

#[cfg(test)]
mod tests {
    use super::{
        ListSync, MdState, MdSync, Segment, WorkLogCounts, copy_payload, displayed_error_text,
        finished_work_log_label, index_turns, list_sync, md_sync, previous_logs_toggle_label,
        segment_entries, timeline_overdraw, turn_work_log_summary, work_log_counts,
        work_log_summary,
    };
    use agent::{FileChange, FileChangeKind, ItemStatus};
    use gpui::{AppContext as _, Entity, TestAppContext};
    use gpui_component::text::TextViewState;
    use std::collections::{HashMap, HashSet};
    use std::path::Path;
    use std::sync::Arc;
    use tcode_core::session::{EntryContent, SteeringStatus, TimelineEntry, TurnMeta};

    fn entry(id: &str, content: EntryContent) -> Arc<TimelineEntry> {
        Arc::new(TimelineEntry {
            id: id.to_string(),
            content,
            ts: None,
            turn: 0,
        })
    }

    #[test]
    fn provider_start_error_is_localized_only_at_render_boundary() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let generic = EntryContent::Error {
            message: "generic\0原样".into(),
        };
        let provider_start = EntryContent::ProviderStartError {
            error: "spawn failed".into(),
        };

        tcode_i18n::set_locale(tcode_i18n::LANGUAGE_ENGLISH);
        assert_eq!(
            displayed_error_text(&generic).as_bytes(),
            b"generic\0\xe5\x8e\x9f\xe6\xa0\xb7"
        );
        assert_eq!(
            displayed_error_text(&provider_start),
            "Failed to start provider: spawn failed"
        );

        tcode_i18n::set_locale(tcode_i18n::LANGUAGE_SIMPLIFIED_CHINESE);
        assert_eq!(
            displayed_error_text(&generic).as_bytes(),
            b"generic\0\xe5\x8e\x9f\xe6\xa0\xb7"
        );
        assert_eq!(
            displayed_error_text(&provider_start),
            "启动提供商失败：spawn failed"
        );
        tcode_i18n::set_locale(tcode_i18n::LANGUAGE_ENGLISH);
    }

    #[test]
    fn steering_status_strings_are_exact_in_both_locales() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        tcode_i18n::set_locale(tcode_i18n::LANGUAGE_ENGLISH);
        assert_eq!(tcode_i18n::tr!("chat.steering"), "Steering…");
        assert_eq!(tcode_i18n::tr!("chat.steered"), "Steered");

        tcode_i18n::set_locale(tcode_i18n::LANGUAGE_SIMPLIFIED_CHINESE);
        assert_eq!(tcode_i18n::tr!("chat.steering"), "引导中…");
        assert_eq!(tcode_i18n::tr!("chat.steered"), "已引导");
        tcode_i18n::set_locale(tcode_i18n::LANGUAGE_ENGLISH);
    }

    #[test]
    fn timeline_overdraw_keeps_multiple_viewports_warm() {
        // Headless/early construction still gets a useful buffer.
        assert_eq!(timeline_overdraw(0.), 3072.);
        // Normal windows retain four full window heights on both sides.
        assert_eq!(timeline_overdraw(900.), 3600.);
        assert_eq!(timeline_overdraw(1440.), 5760.);
    }

    #[test]
    fn previous_log_rows_keep_their_toggle_after_expanding() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        tcode_i18n::set_locale(tcode_i18n::LANGUAGE_SIMPLIFIED_CHINESE);

        assert_eq!(previous_logs_toggle_label(0, false), None);
        assert_eq!(
            previous_logs_toggle_label(3, false).as_deref(),
            Some("前面还有 3 条日志")
        );
        assert_eq!(
            previous_logs_toggle_label(3, true).as_deref(),
            Some("收起前面的 3 条日志")
        );
    }

    fn command(id: &str) -> Arc<TimelineEntry> {
        entry(
            id,
            EntryContent::Command {
                command: id.to_string(),
                output: String::new(),
                exit_code: Some(0),
                status: ItemStatus::Completed,
            },
        )
    }

    fn at_turn(mut entry: Arc<TimelineEntry>, turn: usize) -> Arc<TimelineEntry> {
        Arc::make_mut(&mut entry).turn = turn;
        entry
    }

    #[test]
    fn turn_list_index_and_sync_cover_stream_append_truncate_and_session_switch() {
        let turns = vec![TurnMeta::default()];
        let children = HashMap::new();
        let expanded = HashSet::new();
        let mut entries = vec![
            entry(
                "user-0",
                EntryContent::User {
                    text: "go".into(),
                    steering: None,
                    context_len: None,
                },
            ),
            entry(
                "assistant-0",
                EntryContent::Assistant {
                    text: "working".into(),
                },
            ),
        ];
        let initial = index_turns(&turns, &entries, None, &children, &expanded);
        assert_eq!(initial.len(), 1);
        assert_eq!(initial[0].entry_range, 0..2);

        // Another entry joins the current turn: identity stays at item index 0,
        // but its variable height must be measured again.
        entries.push(command("command-0"));
        let current_turn_append = index_turns(&turns, &entries, None, &children, &expanded);
        assert_eq!(current_turn_append[0].entry_range, 0..3);
        assert_eq!(
            list_sync(&initial, &current_turn_append, false),
            ListSync::Incremental {
                append: None,
                remeasure: vec![0],
            }
        );

        // A new turn adds exactly one list item. The former tail is also
        // remeasured because it gains the visual inter-turn gap.
        let turns = vec![TurnMeta::default(), TurnMeta::default()];
        entries.push(at_turn(
            entry(
                "user-1",
                EntryContent::User {
                    text: "next".into(),
                    steering: None,
                    context_len: None,
                },
            ),
            1,
        ));
        let new_turn = index_turns(&turns, &entries, None, &children, &expanded);
        assert_eq!(new_turn[0].entry_range, 0..3);
        assert_eq!(new_turn[1].entry_range, 3..4);
        assert_eq!(
            list_sync(&current_turn_append, &new_turn, false),
            ListSync::Incremental {
                append: Some(1..2),
                remeasure: vec![0],
            }
        );

        // Revert/truncate cannot leave ListState with stale item indices.
        assert_eq!(
            list_sync(&new_turn, &initial, false),
            ListSync::Reset { count: 1 }
        );
        // Even an equal-shaped replacement must reset when the session changes.
        assert_eq!(
            list_sync(&initial, &initial, true),
            ListSync::Reset { count: 1 }
        );
    }

    #[test]
    fn subagent_expansion_and_live_children_remeasure_the_turn() {
        let turns = vec![TurnMeta::default()];
        let entries = vec![entry(
            "spawn",
            EntryContent::Subagent {
                agent_type: "researcher".into(),
                description: "Inspect the protocol".into(),
                status: ItemStatus::InProgress,
                summary: None,
            },
        )];
        let mut children = HashMap::new();
        let collapsed = index_turns(&turns, &entries, None, &children, &HashSet::new());

        let expanded_keys = HashSet::from(["subagent-spawn".to_string()]);
        let expanded = index_turns(&turns, &entries, None, &children, &expanded_keys);
        assert_eq!(
            list_sync(&collapsed, &expanded, false),
            ListSync::Incremental {
                append: None,
                remeasure: vec![0],
            }
        );

        children.insert(
            "spawn".to_string(),
            vec![entry(
                "spawn:child",
                EntryContent::Assistant {
                    text: "Found the event envelope".into(),
                },
            )],
        );
        let with_child = index_turns(&turns, &entries, None, &children, &expanded_keys);
        assert_eq!(
            list_sync(&expanded, &with_child, false),
            ListSync::Incremental {
                append: None,
                remeasure: vec![0],
            }
        );
    }

    #[test]
    fn segment_entries_preserves_interleaved_timeline_order() {
        let entries = [
            entry(
                "user",
                EntryContent::User {
                    text: "go".into(),
                    steering: None,
                    context_len: None,
                },
            ),
            command("cmd-1"),
            command("cmd-2"),
            entry(
                "assistant-1",
                EntryContent::Assistant {
                    text: "first".into(),
                },
            ),
            command("cmd-3"),
            entry(
                "assistant-2",
                EntryContent::Assistant {
                    text: "second".into(),
                },
            ),
            entry(
                "error",
                EntryContent::Error {
                    message: "boom".into(),
                },
            ),
        ];
        let segments = segment_entries(&entries, false).flow;

        assert_eq!(segments.len(), 6);
        assert!(matches!(segments[0], Segment::User(entry) if entry.id == "user"));
        assert!(matches!(
            &segments[1],
            Segment::ActivityRun(entries)
                if entries.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>()
                    == ["cmd-1", "cmd-2"]
        ));
        assert!(matches!(segments[2], Segment::Assistant(entry) if entry.id == "assistant-1"));
        assert!(matches!(
            &segments[3],
            Segment::ActivityRun(entries)
                if entries.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>() == ["cmd-3"]
        ));
        assert!(matches!(segments[4], Segment::Assistant(entry) if entry.id == "assistant-2"));
        assert!(matches!(segments[5], Segment::Error(entry) if entry.id == "error"));
    }

    #[test]
    fn segment_entries_coalesces_an_all_activity_turn() {
        let entries = [command("cmd-1"), command("cmd-2")];
        let segments = segment_entries(&entries, false).flow;

        assert!(matches!(
            segments.as_slice(),
            [Segment::ActivityRun(entries)] if entries.len() == 2
        ));
    }

    #[test]
    fn segment_entries_handles_an_empty_turn() {
        let segmented = segment_entries(&[], false);
        assert!(segmented.flow.is_empty());
        assert!(segmented.pending_steers.is_empty());
    }

    #[test]
    fn pending_steers_float_after_live_flow_in_fifo_order_only_while_running() {
        let pending = |id: &str| {
            entry(
                id,
                EntryContent::User {
                    text: id.into(),
                    steering: Some(SteeringStatus::Pending),
                    context_len: None,
                },
            )
        };
        let entries = [
            entry("assistant-a", EntryContent::Assistant { text: "a".into() }),
            pending("steer-a"),
            command("command"),
            pending("steer-b"),
            entry("assistant-b", EntryContent::Assistant { text: "b".into() }),
        ];

        let live = segment_entries(&entries, true);
        assert_eq!(live.flow.len(), 3);
        assert!(matches!(live.flow[0], Segment::Assistant(entry) if entry.id == "assistant-a"));
        assert!(matches!(
            &live.flow[1],
            Segment::ActivityRun(run) if run.len() == 1 && run[0].id == "command"
        ));
        assert!(matches!(live.flow[2], Segment::Assistant(entry) if entry.id == "assistant-b"));
        assert_eq!(
            live.pending_steers
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["steer-a", "steer-b"]
        );

        let idle = segment_entries(&entries, false);
        assert!(idle.pending_steers.is_empty());
        assert_eq!(idle.flow.len(), 5);
        assert!(matches!(idle.flow[1], Segment::User(entry) if entry.id == "steer-a"));
        assert!(matches!(idle.flow[3], Segment::User(entry) if entry.id == "steer-b"));
    }

    #[test]
    fn steer_status_and_reordering_invalidate_the_virtualized_turn_row() {
        let turns = vec![TurnMeta {
            running: true,
            ..Default::default()
        }];
        let children = HashMap::new();
        let expanded = HashSet::new();
        let pending = entry(
            "steer",
            EntryContent::User {
                text: "redirect".into(),
                steering: Some(SteeringStatus::Pending),
                context_len: None,
            },
        );
        let assistant = entry(
            "assistant",
            EntryContent::Assistant {
                text: "working".into(),
            },
        );
        let before = index_turns(
            &turns,
            &[pending.clone(), assistant.clone()],
            None,
            &children,
            &expanded,
        );

        let mut accepted = pending;
        if let EntryContent::User { steering, .. } = &mut Arc::make_mut(&mut accepted).content {
            *steering = Some(SteeringStatus::Accepted);
        }
        let status_changed = index_turns(
            &turns,
            &[accepted.clone(), assistant.clone()],
            None,
            &children,
            &expanded,
        );
        assert_eq!(
            list_sync(&before, &status_changed, false),
            ListSync::Incremental {
                append: None,
                remeasure: vec![0],
            }
        );

        let reordered = index_turns(&turns, &[assistant, accepted], None, &children, &expanded);
        assert_eq!(
            list_sync(&status_changed, &reordered, false),
            ListSync::Reset { count: 1 }
        );
    }

    /// A file edit between two commands is one continuous work log. FileChange
    /// entries count toward its summary but render in the CHANGED FILES card.
    #[test]
    fn segment_entries_keeps_activity_runs_continuous_across_file_changes() {
        let entries = [
            command("cmd-1"),
            entry("edit", EntryContent::FileChange { changes: vec![] }),
            command("cmd-2"),
        ];
        let segments = segment_entries(&entries, false).flow;

        assert!(matches!(
            segments.as_slice(),
            [Segment::ActivityRun(run)]
                if run.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>()
                    == ["cmd-1", "edit", "cmd-2"]
        ));
    }

    #[test]
    fn only_latest_live_reasoning_is_visible() {
        let entries = [
            entry(
                "reason-1",
                EntryContent::Reasoning {
                    text: "first".into(),
                },
            ),
            entry(
                "reason-2",
                EntryContent::Reasoning {
                    text: "latest".into(),
                },
            ),
        ];

        let segments = segment_entries(&entries, true).flow;
        assert!(matches!(
            segments.as_slice(),
            [Segment::ActivityRun(run)] if run.len() == 1 && run[0].id == "reason-2"
        ));
    }

    #[test]
    fn later_activity_removes_live_reasoning() {
        let entries = [
            entry(
                "reason",
                EntryContent::Reasoning {
                    text: "thinking".into(),
                },
            ),
            command("later-command"),
        ];

        let segments = segment_entries(&entries, true).flow;
        assert!(matches!(
            segments.as_slice(),
            [Segment::ActivityRun(run)] if run.len() == 1 && run[0].id == "later-command"
        ));

        let entries = [
            entry(
                "reason",
                EntryContent::Reasoning {
                    text: "thinking".into(),
                },
            ),
            entry(
                "assistant",
                EntryContent::Assistant {
                    text: "answer".into(),
                },
            ),
        ];
        let segments = segment_entries(&entries, true).flow;
        assert!(matches!(
            segments.as_slice(),
            [Segment::Assistant(entry)] if entry.id == "assistant"
        ));
    }

    #[test]
    fn completion_removes_reasoning_from_history() {
        let entries = [entry(
            "reason",
            EntryContent::Reasoning {
                text: "finished thinking".into(),
            },
        )];

        assert!(segment_entries(&entries, false).flow.is_empty());
    }

    fn file_change(id: &str, paths: &[&str]) -> Arc<TimelineEntry> {
        entry(
            id,
            EntryContent::FileChange {
                changes: paths
                    .iter()
                    .map(|path| FileChange {
                        path: (*path).to_string(),
                        kind: FileChangeKind::Modify,
                        diff: None,
                    })
                    .collect(),
            },
        )
    }

    fn refs(entries: &[Arc<TimelineEntry>]) -> Vec<&TimelineEntry> {
        entries.iter().map(AsRef::as_ref).collect()
    }

    #[test]
    fn work_log_summary_localizes_mixed_nonzero_counts_exactly() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let counts = WorkLogCounts {
            commands: 2,
            files: 3,
            tools: 1,
            subagents: 2,
            compactions: 1,
        };

        tcode_i18n::set_locale(tcode_i18n::LANGUAGE_ENGLISH);
        assert_eq!(
            work_log_summary(&counts).as_deref(),
            Some(
                "Ran 2 commands · Edited 3 files · Made 1 tool call · Started 2 subagents · Compacted context 1 time"
            )
        );
        tcode_i18n::set_locale(tcode_i18n::LANGUAGE_SIMPLIFIED_CHINESE);
        assert_eq!(
            work_log_summary(&counts).as_deref(),
            Some(
                "已执行 2 条命令 · 编辑 3 个文件 · 调用 1 次工具 · 启动 2 个子代理 · 压缩 1 次上下文"
            )
        );
    }

    #[test]
    fn chinese_turn_summary_prefixes_the_whole_sentence_once() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let counts = WorkLogCounts {
            commands: 5,
            files: 3,
            tools: 2,
            ..WorkLogCounts::default()
        };

        tcode_i18n::set_locale(tcode_i18n::LANGUAGE_SIMPLIFIED_CHINESE);
        assert_eq!(
            turn_work_log_summary(&counts).as_deref(),
            Some("共执行 5 条命令 · 编辑 3 个文件 · 调用 2 次工具")
        );
    }

    #[test]
    fn work_log_summary_omits_zero_counts_and_empty_rows() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let tools_only = WorkLogCounts {
            tools: 2,
            ..WorkLogCounts::default()
        };

        tcode_i18n::set_locale(tcode_i18n::LANGUAGE_ENGLISH);
        assert_eq!(
            work_log_summary(&tools_only).as_deref(),
            Some("Made 2 tool calls")
        );
        assert_eq!(work_log_summary(&WorkLogCounts::default()), None);
        assert_eq!(
            finished_work_log_label(true, &WorkLogCounts::default(), &WorkLogCounts::default()),
            None
        );
        assert_eq!(
            finished_work_log_label(
                false,
                &WorkLogCounts::default(),
                &WorkLogCounts {
                    commands: 1,
                    ..WorkLogCounts::default()
                }
            ),
            None
        );
        tcode_i18n::set_locale(tcode_i18n::LANGUAGE_SIMPLIFIED_CHINESE);
        assert_eq!(
            work_log_summary(&tools_only).as_deref(),
            Some("调用 2 次工具")
        );
        assert_eq!(work_log_summary(&WorkLogCounts::default()), None);
    }

    #[test]
    fn work_log_counts_unique_file_paths_across_snapshots() {
        let entries = [
            file_change("files-1", &["src/a.rs", "src/b.rs"]),
            file_change("files-2", &["src/a.rs", "src/a.rs"]),
        ];

        assert_eq!(work_log_counts(&refs(&entries)).files, 2);
    }

    #[test]
    fn finished_activity_runs_show_real_counts_and_end_with_turn_wide_summary() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let entries = [
            command("command-1"),
            file_change("files-1", &["src/shared.rs"]),
            entry(
                "assistant",
                EntryContent::Assistant {
                    text: "intermediate output".into(),
                },
            ),
            command("command-2"),
            file_change("files-2", &["src/shared.rs"]),
        ];
        let segments = segment_entries(&entries, false).flow;
        let activity_indexes: Vec<usize> = segments
            .iter()
            .enumerate()
            .filter_map(|(index, segment)| {
                matches!(segment, Segment::ActivityRun(_)).then_some(index)
            })
            .collect();
        assert_eq!(activity_indexes.len(), 2);

        let counts = work_log_counts(&refs(&entries));
        assert_eq!(counts.commands, 2);
        assert_eq!(counts.files, 1);

        let labels = |last_activity, counts: &WorkLogCounts| {
            activity_indexes
                .iter()
                .map(|index| {
                    let Segment::ActivityRun(activities) = &segments[*index] else {
                        unreachable!();
                    };
                    let segment_counts = work_log_counts(activities);
                    finished_work_log_label(*index == last_activity, &segment_counts, counts)
                        .expect("each real activity run has a footer affordance")
                })
                .collect::<Vec<_>>()
        };
        let last_activity = *activity_indexes.last().unwrap();

        tcode_i18n::set_locale(tcode_i18n::LANGUAGE_ENGLISH);
        assert_eq!(
            labels(last_activity, &counts),
            [
                "Ran 1 command · Edited 1 file",
                "Ran 2 commands · Edited 1 file"
            ]
        );
        tcode_i18n::set_locale(tcode_i18n::LANGUAGE_SIMPLIFIED_CHINESE);
        assert_eq!(
            labels(last_activity, &counts),
            [
                "已执行 1 条命令 · 编辑 1 个文件",
                "共执行 2 条命令 · 编辑 1 个文件"
            ]
        );
    }

    /// A message's Copy action puts the RAW text on the clipboard — the markdown
    /// source, not the document the timeline renders from it (which drops the
    /// syntax: `**bold**` renders as "bold").
    #[gpui::test]
    fn copy_puts_raw_text_on_the_clipboard(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let raw = "Done — **bold**, `code` and:\n\n- one\n- two\n";

        let payload = copy_payload(raw);
        assert_eq!(payload.text().as_deref(), Some(raw));

        // The rendered document is a different (lossy) string; copying it would
        // be the bug this test exists to prevent.
        let md = cx.update(|cx| MdState::new(raw, false, cx));
        let rendered = rendered(&md.state, cx);
        assert_ne!(rendered, raw);
        assert!(!rendered.contains("**"));
    }

    #[test]
    fn relativize_strips_cwd_prefix() {
        let cwd = Path::new("/tmp/proj");
        assert_eq!(
            tcode_runtime::ui_facade::relativize_to_workspace("/tmp/proj/src/a.rs", cwd),
            "src/a.rs"
        );
        assert_eq!(
            tcode_runtime::ui_facade::relativize_to_workspace("/tmp/proj/a.rs", cwd),
            "a.rs"
        );
        // Outside the cwd stays absolute.
        assert_eq!(
            tcode_runtime::ui_facade::relativize_to_workspace("/other/x.rs", cwd),
            "/other/x.rs"
        );
        // Already-relative paths are left as-is.
        assert_eq!(
            tcode_runtime::ui_facade::relativize_to_workspace("src/b.rs", cwd),
            "src/b.rs"
        );
    }

    // -- the pure delta/reset decision ---------------------------------------

    #[test]
    fn md_sync_decides_push_reset_and_noop() {
        // Unchanged text does nothing (the streaming hot path: most notifies
        // carry no new text for a given entry).
        assert_eq!(md_sync("abc", "abc", true), MdSync::Noop);
        assert_eq!(md_sync("", "", true), MdSync::Noop);
        // An append is a push of just the delta.
        assert_eq!(md_sync("", "I", true), MdSync::Push("I".into()));
        assert_eq!(md_sync("I", "I'll go", true), MdSync::Push("'ll go".into()));
        // Anything that is not an append rebuilds: a rewrite, a shrink, or a
        // snapshot that replaces the accumulated text.
        assert_eq!(md_sync("abc", "xbc", true), MdSync::Reset("xbc".into()));
        assert_eq!(md_sync("abcd", "abc", true), MdSync::Reset("abc".into()));
        assert_eq!(md_sync("abc", "", true), MdSync::Reset(String::new()));
        // A state that cannot be pushed into (its content was parsed at
        // construction, so the background parse task never saw it) always
        // rebuilds, even for a pure append.
        assert_eq!(
            md_sync("I", "I'll go", false),
            MdSync::Reset("I'll go".into())
        );
        assert_eq!(md_sync("abc", "abc", false), MdSync::Noop);
    }

    // -- headless TextViewState mirroring ------------------------------------

    /// The document the widget would actually render.
    fn rendered(state: &Entity<TextViewState>, cx: &mut TestAppContext) -> String {
        state.update(cx, |state, cx| {
            state.select_all(cx);
            state.selected_text()
        })
    }

    /// Pins the upstream constraint this mirror is built around: a
    /// `TextViewState` seeded with text and then grown with `push_str` renders
    /// only the pushed text — the seed is dropped, because the initial parse
    /// happens on the main thread and never reaches the background parse task
    /// whose document each `push_str` result replaces `parsed_content` with.
    ///
    /// This is what made a streamed "I'll run that command" render as "'ll run
    /// that command". If this test ever fails, gpui-component fixed it and
    /// `MdState::can_push` can go away.
    #[gpui::test]
    fn text_view_state_drops_a_non_empty_seed_on_push(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let state = cx.update(|cx| cx.new(|cx| TextViewState::markdown("I", cx)));
        cx.run_until_parked();
        state.update(cx, |state, cx| state.push_str("'ll run it.", cx));
        cx.run_until_parked();

        assert_eq!(rendered(&state, cx), "'ll run it.\n");
    }

    /// ...and the same for `set_text`: the background parse task keeps its own
    /// (delta-only) document, so a later `push_str` resurrects it and throws the
    /// `set_text` away. Hence [`MdState::sync`] rebuilds instead of calling it.
    #[gpui::test]
    fn text_view_state_drops_set_text_on_push(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let state = cx.update(|cx| cx.new(|cx| TextViewState::markdown("", cx)));
        cx.run_until_parked();
        state.update(cx, |state, cx| state.push_str("Hello", cx));
        state.update(cx, |state, cx| state.set_text("Snapshot.", cx));
        state.update(cx, |state, cx| state.push_str(" More.", cx));
        cx.run_until_parked();

        assert_eq!(rendered(&state, cx), "Hello More.\n");
    }

    /// Incremental `push_str` from an empty seed is faithful: streaming a
    /// document in 1/3/7-char chunks parses to exactly what a one-shot parse of
    /// the same markdown produces (paragraphs, list, fenced code, inline marks).
    /// This is why the streaming path keeps `push_str` instead of re-`set_text`ing
    /// the whole message on every delta.
    #[gpui::test]
    fn push_str_streaming_matches_a_one_shot_parse(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let full = "I'll run that now.\n\nThe plan:\n\n- one\n- two\n\n```rust\nfn main() {}\n```\n\nDone — **bold** and `code`.\n";
        let one_shot = cx.update(|cx| cx.new(|cx| TextViewState::markdown(full, cx)));
        cx.run_until_parked();
        let expected = rendered(&one_shot, cx);

        for size in [1usize, 3, 7] {
            let chars: Vec<char> = full.chars().collect();
            let mut md = cx.update(|cx| MdState::new("", true, cx));
            let mut text = String::new();
            for chunk in chars.chunks(size) {
                text.extend(chunk);
                let next = text.clone();
                cx.update(|cx| md.sync(next, true, cx));
                cx.run_until_parked();
            }
            assert_eq!(rendered(&md.state, cx), expected, "chunk size {size}");
        }
    }

    /// The live streaming path: an assistant entry first appears with its first
    /// delta and grows. Every character must survive — this is the regression
    /// for the dropped leading "I" / "Q".
    #[gpui::test]
    fn md_state_streams_without_dropping_the_first_chunk(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let mut md = cx.update(|cx| MdState::new("Q", true, cx));
        cx.run_until_parked();

        let mut text = String::from("Q");
        for delta in ["UEUED", "_ONE"] {
            text.push_str(delta);
            let next = text.clone();
            cx.update(|cx| md.sync(next, true, cx));
            cx.run_until_parked();
        }

        assert_eq!(md.synced.as_ref(), "QUEUED_ONE");
        assert_eq!(rendered(&md.state, cx), "QUEUED_ONE\n");
    }

    /// A non-append change (a snapshot that rewrites the accumulated text)
    /// rebuilds the state instead of `set_text`ing it, and further deltas keep
    /// streaming onto the rebuilt state — no stale text resurfaces, nothing is
    /// duplicated.
    #[gpui::test]
    fn md_state_reset_rebuilds_without_duplicating(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let mut md = cx.update(|cx| MdState::new("First para.", true, cx));
        cx.run_until_parked();

        // Snapshot rewrites the text (not an append) -> rebuild.
        cx.update(|cx| md.sync("Rewritten para.".into(), true, cx));
        cx.run_until_parked();
        assert!(md.can_push);
        assert_eq!(rendered(&md.state, cx), "Rewritten para.\n");

        // Streaming resumes on the rebuilt state.
        cx.update(|cx| md.sync("Rewritten para. Tail.".into(), true, cx));
        cx.run_until_parked();
        assert_eq!(rendered(&md.state, cx), "Rewritten para. Tail.\n");
    }

    /// Settled text (replay / finished turns) is parsed at construction, so it
    /// is already rendered on the first layout — no empty first frame, which is
    /// what keeps opening a stored session pinned to the bottom.
    #[gpui::test]
    fn md_state_settled_text_parses_synchronously(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let md = cx.update(|cx| MdState::new("Stored reply.", false, cx));
        // Deliberately no `run_until_parked`: the parse must have happened
        // during construction.
        assert!(!md.can_push);
        assert_eq!(rendered(&md.state, cx), "Stored reply.\n");
    }

    /// A settled state that unexpectedly grows still renders correctly: it
    /// cannot be pushed into (the background task never saw its seed), so it
    /// rebuilds.
    #[gpui::test]
    fn md_state_settled_growth_rebuilds(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let mut md = cx.update(|cx| MdState::new("Stored reply.", false, cx));
        cx.run_until_parked();

        cx.update(|cx| md.sync("Stored reply. And more.".into(), false, cx));
        cx.run_until_parked();
        assert_eq!(rendered(&md.state, cx), "Stored reply. And more.\n");
    }
}
