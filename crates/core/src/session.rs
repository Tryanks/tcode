//! Pure timeline fold: canonical [`AgentEvent`]s in, renderable timeline out.
//!
//! The same fold is used for live event streams and for JSONL replay, so the
//! UI renders identically in both cases.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use agent::{
    AgentEvent, ApprovalRequest, ChangeCompleteness, DeltaKind, FileChange, ItemContent,
    ItemStatus, PlanStep, ResumeCursor, ThreadItem, TokenUsage, ToolFamily, TurnStatus,
    UserInputDelivery, UserInputQuestion,
};
use serde::{Deserialize, Serialize};

use crate::git::merge_file_changes_by_path;
use crate::relay::{one_line, truncate_chars};

mod boundaries;
mod streams;
mod superseded;

pub use boundaries::{RewindCut, TurnBoundaries, TurnBoundaryChange, TurnStarts};

pub use streams::{ItemStreams, SupersededStream};
pub use superseded::{TurnSnapshots, drop_turn_diffs};

/// Bump when the persisted turn-boundary policy changes.
pub const TURN_INDEX_FOLD_VERSION: u32 = 10;

fn synthetic_entry_id(
    prefix: &str,
    ts: Option<u64>,
    next: &mut u64,
    mut exists: impl FnMut(&str) -> bool,
) -> String {
    let Some(ts) = ts else {
        *next += 1;
        return format!("{prefix}-{next}");
    };
    let base = format!("{prefix}-{ts}");
    let mut id = base.clone();
    let mut duplicate = 1;
    while exists(&id) {
        id = format!("{base}-{duplicate}");
        duplicate += 1;
    }
    id
}

/// Claude Code's own prompt for resuming work after a usage-window reset.
pub const RESUME_PROMPT: &str = "Continue from where you left off.";

/// A local review note attached to a range in the diff panel. These live in
/// the composer draft until the next send; they are never written to session
/// history as separate events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewComment {
    pub file: String,
    pub line_start: u32,
    pub line_end: u32,
    pub side: ReviewSide,
    pub text: String,
    pub code_excerpt: String,
    pub(crate) section_id: String,
    pub(crate) section_title: String,
    pub(crate) start_index: usize,
    pub(crate) end_index: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSide {
    Old,
    New,
}

impl ReviewComment {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        file: String,
        line_start: u32,
        line_end: u32,
        side: ReviewSide,
        text: String,
        code_excerpt: String,
        section_id: String,
        section_title: String,
        start_index: usize,
        end_index: usize,
    ) -> Self {
        Self {
            file,
            line_start: line_start.min(line_end),
            line_end: line_start.max(line_end),
            side,
            text,
            code_excerpt,
            section_id,
            section_title,
            start_index: start_index.min(end_index),
            end_index: start_index.max(end_index),
        }
    }

    pub fn range_label(&self) -> String {
        let marker = match self.side {
            ReviewSide::Old => "-",
            ReviewSide::New => "+",
        };
        if self.line_start == self.line_end {
            format!("{marker}{}", self.line_start)
        } else {
            format!("{marker}{} to {marker}{}", self.line_start, self.line_end)
        }
    }
}

fn escape_review_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn review_fence(contents: &str) -> String {
    let longest = contents
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(3.max(longest + 1));
    format!("{fence}diff\n{}\n{fence}", contents.trim_end())
}

/// Serialize review notes as `<review_comment ...>` blocks in the agent prompt.
pub fn append_review_comments_to_prompt(prompt: &str, comments: &[ReviewComment]) -> String {
    if comments.is_empty() {
        return prompt.to_string();
    }
    let blocks = comments
        .iter()
        .map(|comment| {
            format!(
                "<review_comment sectionId=\"{}\" sectionTitle=\"{}\" filePath=\"{}\" startIndex=\"{}\" endIndex=\"{}\" rangeLabel=\"{}\">\n{}\n{}\n</review_comment>",
                escape_review_attribute(&comment.section_id),
                escape_review_attribute(&comment.section_title),
                escape_review_attribute(&comment.file),
                comment.start_index,
                comment.end_index,
                escape_review_attribute(&comment.range_label()),
                comment.text.trim(),
                review_fence(&comment.code_excerpt),
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let prompt = prompt.trim();
    if prompt.is_empty() {
        blocks
    } else {
        format!("{prompt}\n\n{blocks}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Author {
    pub device_id: String,
    pub name: String,
}

/// One persisted event, optionally tagged with the wall-clock time (unix ms)
/// at which it was recorded. Legacy `.jsonl` lines replay with `ts == None`;
/// envelope lines carry the recorded timestamp.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<MessageOrigin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<Author>,
    pub ts: Option<u64>,
    pub event: AgentEvent,
    /// The byte length of an item output that a host shortened before sending
    /// this record; the full output stays on the host. Never set on a record
    /// read from the log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elided: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageOrigin {
    Human,
    Agent,
    Server,
}

impl MessageOrigin {
    pub fn legacy(has_parent: bool) -> Self {
        if has_parent { Self::Agent } else { Self::Human }
    }
}

impl StoredEvent {
    /// The item an event creates or continues.
    pub fn item_id(&self) -> Option<&str> {
        match &self.event {
            AgentEvent::ItemStarted(item)
            | AgentEvent::ItemUpdated(item)
            | AgentEvent::ItemCompleted(item) => Some(&item.id),
            AgentEvent::Delta { item_id, .. } | AgentEvent::ItemBackgrounded { item_id, .. } => {
                Some(item_id)
            }
            _ => None,
        }
    }

    /// What a connection whose line cannot carry this record, `bytes` long
    /// on the wire and in the log's `row` where the sender knows it, is sent
    /// in its place.
    pub fn withheld(&self, bytes: u64, row: Option<u64>) -> Self {
        StoredEvent {
            origin: self.origin,
            author: self.author.clone(),
            ts: self.ts,
            event: AgentEvent::RecordWithheld { bytes, row },
            elided: None,
        }
    }
}

impl From<AgentEvent> for StoredEvent {
    fn from(event: AgentEvent) -> Self {
        StoredEvent {
            origin: None,
            author: None,
            ts: None,
            event,
            elided: None,
        }
    }
}

/// One renderable row in the chat timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct TimelineEntry {
    /// Provider item id (or a synthetic id for errors).
    pub id: String,
    pub content: EntryContent,
    /// Wall-clock time (unix ms) this entry was first observed, if known.
    pub ts: Option<u64>,
    /// Wall-clock time (unix ms) the item completed, when its entry was
    /// observed before then: an item that arrives already complete took no
    /// time anyone saw.
    pub finished_ts: Option<u64>,
    /// The call returned to the model while the work it started goes on
    /// ([`AgentEvent::ItemBackgrounded`]): until its item settles, that work
    /// runs without the turn waiting on it.
    pub background: bool,
    /// Index into [`Timeline::turns`] of the turn this entry belongs to.
    pub turn: usize,
}

/// Per-turn ("Work Log" section) metadata folded from turn lifecycle events.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnMeta {
    /// Provider-native id for this turn, used to attach replacement turn-diff
    /// snapshots without relying on ambient "current turn" state during replay.
    pub provider_turn_id: Option<String>,
    /// Opaque provider-owned restore point for the user message that opened
    /// this turn. Present only when the provider exposes a native rewind API.
    pub provider_checkpoint_id: Option<String>,
    /// Restore points for the steers in this turn: (steer entry id, checkpoint id).
    pub steer_checkpoints: Vec<(String, String)>,
    /// When the turn began (TurnStarted, or the opening user message).
    pub start_ts: Option<u64>,
    /// When the turn finished (TurnCompleted).
    pub end_ts: Option<u64>,
    pub status: Option<TurnStatus>,
    /// Whether this turn is currently running.
    pub running: bool,
    /// File changes causally attributed to this turn. Provider-native net
    /// snapshots replace this wholesale; structured file-operation items form
    /// a partial fallback without consulting the ambient Git working tree.
    pub changes: Option<TurnChangeSet>,
    /// Wall-clock breakdown of the finished turn. `None` while the turn runs,
    /// whenever the turn lacks a timestamped `TurnStarted`/`TurnCompleted` pair
    /// to measure against, and whenever the recorded clock regressed across the
    /// turn's end — a missing or untrustworthy timestamp yields no breakdown
    /// rather than an invented one.
    pub timing: Option<TurnTiming>,
    /// Model that actually served this turn, when reported by the provider.
    pub served_model: Option<String>,
    /// Provider-reported cost for this turn, in US dollars.
    pub cost_usd: Option<f64>,
    /// Provider-reported duration, distinct from tcode's observed wall clock.
    pub provider_duration_ms: Option<u64>,
}

/// How a finished turn's wall clock divided between waiting on tools and
/// everything else (the model thinking and answering). Millisecond based, and
/// derived purely from the timestamps already recorded on the event stream, so
/// a live session and a replay of its log agree exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct TurnTiming {
    /// The observed `TurnStarted`..`TurnCompleted` span.
    pub total_ms: u64,
    /// The union of the intervals in which at least one tool-like item was in
    /// progress. Tools running in parallel are counted once, never summed.
    pub tool_ms: u64,
}

impl TurnTiming {
    pub fn new(total_ms: u64, tool_ms: u64) -> Self {
        Self { total_ms, tool_ms }
    }
}

/// Running union-of-intervals accounting for the tool-like items of the open
/// turn, intersected with the turn's own `TurnStarted`..`TurnCompleted` bounds.
/// Only the current turn can have items in flight, so one accumulator is
/// enough; it resets whenever a turn opens.
#[derive(Debug, Clone, Default, PartialEq)]
struct ToolClock {
    /// Item ids currently known to be in progress.
    open: HashSet<String>,
    /// Start of the interval that opened when `open` became non-empty.
    union_start: Option<u64>,
    /// Union of the closed tool-active intervals so far, in ms.
    tool_ms: u64,
    /// Latest timestamp observed anywhere inside the turn — tool lifecycle
    /// events, ordinary items, and deltas alike. It clamps a clock that steps
    /// backwards, and it is the watermark a completion must not precede.
    clock: Option<u64>,
    /// The authoritative turn start: the timestamp of the observed
    /// `TurnStarted`. Tool time is measured from here, never earlier, and its
    /// absence means the turn gets no breakdown at all.
    turn_start: Option<u64>,
    /// A tool-like item was observed without a timestamp, so this turn cannot
    /// produce a trustworthy breakdown.
    untimed: bool,
}

impl ToolClock {
    /// Anchor the clock to the authoritative `TurnStarted` time. Anything
    /// accumulated before it belonged to earlier work and is discarded; a tool
    /// still open across the boundary is rebased to begin exactly at the turn
    /// start, so only the in-bounds part of its interval counts.
    fn begin_turn(&mut self, ts: u64) {
        self.advance(ts);
        self.turn_start = Some(ts);
        self.tool_ms = 0;
        if let Some(start) = self.union_start {
            self.union_start = Some(start.max(ts));
        }
    }

    /// Note a timestamp seen inside the turn, whatever event carried it. The
    /// turn's own completion is the one event excluded: it is measured
    /// *against* this watermark rather than folded into it.
    fn observe(&mut self, ts: Option<u64>) {
        if let Some(ts) = ts {
            self.advance(ts);
        }
    }

    /// Record a tool-like lifecycle transition. `active` marks the item as in
    /// progress; otherwise it is finished.
    fn mark(&mut self, ts: Option<u64>, item_id: &str, active: bool) {
        let Some(ts) = ts else {
            self.untimed = true;
            return;
        };
        let ts = self.advance(ts);
        if active {
            // Repeated updates for an already-open item change nothing; the
            // first sighting opens it.
            if self.open.insert(item_id.to_owned()) && self.open.len() == 1 {
                self.union_start = Some(ts);
            }
        } else if self.open.remove(item_id) && self.open.is_empty() {
            self.close(ts);
        }
    }

    /// Accept a timestamp, never letting it move the clock backwards: a
    /// backward stamp contributes a zero-length step instead of underflowing.
    fn advance(&mut self, ts: u64) -> u64 {
        let clamped = self.clock.map_or(ts, |clock| ts.max(clock));
        self.clock = Some(clamped);
        clamped
    }

    /// Charge the open interval up to `ts`, intersected with the turn start.
    fn close(&mut self, ts: u64) {
        if let Some(start) = self.union_start.take() {
            let start = self.turn_start.map_or(start, |bound| start.max(bound));
            self.tool_ms = self.tool_ms.saturating_add(ts.saturating_sub(start));
        }
    }

    /// Close the turn at `end`, charging any still-open tools up to it. `end`
    /// is the turn's own upper bound, so the interval never extends past it.
    fn finish(mut self, end: u64) -> u64 {
        self.close(end);
        self.tool_ms
    }
}

/// Derive the finished turn's breakdown, or `None` when the events cannot
/// support one: no timestamped `TurnStarted`/`TurnCompleted` pair (legacy logs,
/// or a turn opened only by a user message), a tool-like item seen without a
/// timestamp, or a completion that precedes work already recorded inside the
/// turn.
///
/// That last case is a wall clock that regressed across the turn boundary: some
/// event inside the turn is stamped later than the turn's own end, so the true
/// bounds are unknowable. Clamping the aggregate would keep the total honest
/// while silently misattributing the split between the buckets — a tool that
/// "ran" past the end would eat AI time that may never have been tool time at
/// all. There is no defensible attribution to guess at, so the breakdown is
/// withheld and the UI falls back to the bare completion clock.
fn finish_timing(clock: ToolClock, end: Option<u64>) -> Option<TurnTiming> {
    let (start, end) = (clock.turn_start?, end?);
    if clock.untimed || end < start || clock.clock.is_some_and(|latest| end < latest) {
        return None;
    }
    Some(TurnTiming::new(end - start, clock.finish(end)))
}

/// Which lifecycle event carried a tool-like item. The variant is authoritative
/// over the item's own `status` field, which several providers drop entirely
/// (Codex maps `webSearch` and unmodeled items to statusless content).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolLifecycle {
    Started,
    Updated,
    Completed,
}

/// A tool-like item's own view of its state. `Unknown` is a tool-like item that
/// reports no status of its own (`WebSearch`, `Other`); `None` from
/// [`tool_item_state`] means the item is model output and is never timed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolState {
    Active,
    Finished,
    Unknown,
}

/// Classify an item as tool-like — a command, file edit, tool call, subagent,
/// web search, or an unmodeled provider item — and read the status it carries.
fn tool_item_state(content: &ItemContent) -> Option<ToolState> {
    let status = match content {
        ItemContent::CommandExecution { status, .. }
        | ItemContent::FileChange { status, .. }
        | ItemContent::ToolCall { status, .. }
        | ItemContent::Subagent { status, .. } => *status,
        ItemContent::ImageRead { .. }
        | ItemContent::WebSearch { .. }
        | ItemContent::Other { .. } => {
            return Some(ToolState::Unknown);
        }
        ItemContent::UserMessage { .. }
        | ItemContent::AssistantMessage { .. }
        | ItemContent::Plan { .. }
        | ItemContent::Reasoning { .. } => return None,
    };
    Some(match status {
        ItemStatus::InProgress => ToolState::Active,
        ItemStatus::Completed
        | ItemStatus::Failed
        | ItemStatus::Interrupted
        | ItemStatus::Declined => ToolState::Finished,
    })
}

/// Whether a tool-like item is in progress after this transition. The lifecycle
/// variant wins: a start always opens the interval and a completion always
/// closes it, whatever status the snapshot carries (or fails to carry). Only an
/// update defers to an explicit status, and a statusless update keeps the item
/// active.
fn tool_is_active(lifecycle: ToolLifecycle, state: ToolState) -> bool {
    match lifecycle {
        ToolLifecycle::Started => true,
        ToolLifecycle::Completed => false,
        ToolLifecycle::Updated => state != ToolState::Finished,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TurnChangeSet {
    pub changes: Vec<FileChange>,
    pub completeness: ChangeCompleteness,
}

/// [`EntryContent::Worktree`] in plain words, for text that is not the timeline.
pub fn worktree_summary(
    request: &agent::WorktreeRequest,
    answer: &agent::WorktreeAnswer,
) -> String {
    use agent::{WorktreeAnswer, WorktreeRequest};
    match (request, answer) {
        (WorktreeRequest::Create { name, .. }, WorktreeAnswer::Ready { path, reused }) => format!(
            "{} worktree `{name}` at `{}`",
            if *reused { "reused" } else { "created" },
            path.display()
        ),
        (WorktreeRequest::Create { name, .. }, WorktreeAnswer::Refused { error }) => {
            format!("could not create worktree `{name}`: {error}")
        }
        (WorktreeRequest::Remove { path }, WorktreeAnswer::Refused { error }) => {
            format!(
                "could not remove the worktree at `{}`: {error}",
                path.display()
            )
        }
        (WorktreeRequest::Create { name, .. }, WorktreeAnswer::Removed) => {
            format!("removed worktree `{name}`")
        }
        (WorktreeRequest::Remove { path }, _) => {
            format!("removed the worktree at `{}`", path.display())
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum EntryContent {
    Item(ItemContent),
    /// A user message injected into an already-open turn. Provider-originated
    /// user messages live in [`EntryContent::Item`]; this tcode-only variant
    /// carries the local delivery state that [`ItemContent`] does not model.
    Steer {
        text: String,
        /// Delivery state for a message injected into an already-open turn.
        status: SteeringStatus,
        /// Local paths of the image attachments sent with this message (empty
        /// for text-only messages and for logs predating the field).
        attachments: Vec<String>,
    },
    /// The user's own words declining a request the agent waits on
    /// ([`ApprovalDecision::Feedback`](agent::ApprovalDecision::Feedback)), which
    /// the agent continues from inside its open turn.
    Feedback {
        text: String,
    },
    Error {
        message: String,
        /// Unix seconds when the turn failed because the usage window is exhausted.
        limit_resets_at: Option<u64>,
    },
    #[rustfmt::skip]
    ProviderStartError { error: String },
    /// What the harness warned of, where in the conversation it did.
    Warning {
        message: String,
    },
    /// The harness's latest attempt at a request it is trying again.
    RequestRetry {
        message: String,
    },
    /// A tcode-level conversation handoff, rendered as a divider before the
    /// first user message sent to the new provider, with the handoff `prompt`
    /// as sent disclosed under it.
    ProviderRelay {
        from_provider: agent::ProviderKind,
        from_model: Option<String>,
        to_provider: agent::ProviderKind,
        to_model: Option<String>,
        prompt: Option<String>,
        /// The directory the thread moved from, when the handoff follows a move.
        moved_from: Option<std::path::PathBuf>,
        /// The native session the handoff left, where the thread can go on
        /// in it again ([`Timeline::left_session`]).
        left: Option<agent::LeftSession>,
    },
    /// The provider changed the model actually serving this session.
    ModelChanged {
        from: Option<String>,
        to: String,
        reason: Option<String>,
    },
    /// The provider compacted its context window (a "Context compacted" work-log row).
    ContextCompacted(agent::Compaction),
    /// The user changed the context window for the next provider turn.
    ContextWindowChanged {
        window: u64,
    },
    /// The message before this ran on the live process's old settings.
    SettingsKept,
    /// The native session goes on in another directory: the thread moved and its
    /// session went along, or the session changed directory on its own.
    SessionMoved {
        from: std::path::PathBuf,
        to: std::path::PathBuf,
    },
    /// What Tcode did with a git worktree the harness asked it for.
    Worktree {
        request: agent::WorktreeRequest,
        answer: agent::WorktreeAnswer,
    },
    /// Another thread's model acted on this thread through `tcode_control`
    /// (a system row naming the actor and the action).
    ControlAction(agent::ControlAction),
    /// A rewind the harness made: a divider where it cut the conversation, or
    /// a note where it restored files alone.
    Rewound(Box<Rewound>),
    /// A rewind, or the undo of one, the harness did not make.
    RewindFailed {
        mode: Option<agent::RewindMode>,
        failure: agent::RewindFailure,
    },
    /// A row of the log this build cannot read, as it is stored
    /// ([`AgentEvent::UnreadableRecord`]).
    Unreadable {
        line: String,
    },
    /// A record too large to send to this client, kept on the host
    /// ([`AgentEvent::RecordWithheld`]).
    Withheld {
        bytes: u64,
        row: Option<u64>,
    },
}

/// What a rewind did, as its divider shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Rewound {
    pub checkpoint_id: String,
    /// What the harness restored.
    pub mode: agent::RewindMode,
    pub later_history: agent::LaterHistory,
    /// What the cut took out of the conversation, kept to be read: the entries,
    /// and how many turns they began. Empty for a files-only restore.
    pub removed: Vec<Arc<TimelineEntry>>,
    pub removed_turns: usize,
    pub files: Option<agent::RewindFiles>,
    pub incomplete: Option<agent::RewindIncomplete>,
    /// Present until a message is sent after the rewind, while the harness
    /// can undo it.
    pub undo_token: Option<String>,
    /// The branch the rewind left, which outlives the undo: the harness keeps
    /// it whatever is sent after ([`agent::ForkPoint::Branch`]). A fork of the
    /// thread does not carry it, each branch being one thread's to go on in.
    pub branch: Option<String>,
    /// The turns' metadata the cut removed, restored with the entries by an undo.
    removed_meta: Vec<TurnMeta>,
    /// A steer cut's turn and the steer restore points it had, which an
    /// undo gives back.
    cut_steers: Option<(usize, Vec<(String, String)>)>,
}

/// Whether a handoff left a native session behind, which holds the turns
/// before it and which the thread's harness cannot reach. One Tcode made
/// inside a harness whose history it relays left none: there the thread's log
/// is the conversation, rewound and forked whatever session ran a turn
/// ([`agent::Caps::relayed_history`]). A move to another directory still
/// leaves the files of the turns before it behind.
fn relay_left_session(
    from: agent::ProviderKind,
    to: agent::ProviderKind,
    moved_from: &Option<std::path::PathBuf>,
    left: &Option<agent::LeftSession>,
) -> bool {
    left.is_some() || moved_from.is_some() || from != to || !from.caps().relayed_history
}

/// Where a fork through a finished turn stops ([`Timeline::fork_edge`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForkEdge {
    /// Before the next prompt, the one that opened `turn`.
    BeforePrompt { turn: usize, checkpoint_id: String },
    /// Nothing comes after it: all of the session.
    End,
    /// All of a session a handoff left, which `turn` opened
    /// ([`Timeline::left_session`]).
    LeftEnd { turn: usize },
}

/// The native session a turn ran in, once a handoff has left it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LeftTurn<'a> {
    /// The turn the handoff opened: the first one not in that session.
    pub relay_turn: usize,
    pub provider: agent::ProviderKind,
    pub model: Option<&'a str>,
    /// `None` where the session cannot be gone on in.
    pub session: Option<&'a agent::LeftSession>,
}

/// The session a rewind returns the thread to, when it is not the one the
/// thread is in ([`Timeline::rewind_return`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RewindReturn<'a> {
    pub provider: agent::ProviderKind,
    pub model: Option<&'a str>,
    pub session: &'a agent::LeftSession,
    /// The session's harness rewinds it to the message. `false` for the
    /// message a handoff carried: the session it left is returned to as it
    /// stands, no harness is asked for anything
    /// ([`AgentEvent::RelayUndone`]), and the session's name stands for the
    /// message's restore point.
    pub harness_rewinds: bool,
}

impl RewindReturn<'_> {
    /// What such a rewind restores: the conversation alone. The turns after
    /// the message ran in other sessions, whose files this one's harness
    /// never tracked.
    pub const MODE: agent::RewindMode = agent::RewindMode::Conversation;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SteeringStatus {
    Pending,
    Accepted,
}

/// A structured question set the agent is waiting on, or working past, from
/// [`AgentEvent::UserInputRequested`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingUserInput {
    pub request_id: String,
    pub questions: Vec<UserInputQuestion>,
    pub delivery: UserInputDelivery,
}

/// What a thread is doing, as told by the item its running turn started last.
/// It says what kind of work that is and what the item names, never the
/// words the UI puts around it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum LatestActivity {
    Thinking,
    Writing,
    Command {
        text: String,
    },
    Edit {
        path: Option<String>,
    },
    Read {
        path: String,
    },
    WebSearch {
        query: String,
    },
    Tool {
        /// As the provider spells it; [`agent::ToolName::parse`] reads it.
        name: String,
        /// The argument that identifies the call.
        target: Option<String>,
    },
    Subagent {
        description: String,
    },
    /// An item canonicalization does not model: its kind, and what its
    /// summary names.
    Other {
        kind: String,
        summary: Option<String>,
    },
}

impl LatestActivity {
    /// The most a text field carries, in chars: this reaches every client
    /// with every thread's index, and a row shows no more.
    const MAX_CHARS: usize = 160;

    /// What `item` says it is doing. `None` for an item that is not the
    /// agent's work.
    fn of(item: &ItemContent) -> Option<Self> {
        let clip = |text: &str| truncate_chars(&one_line(text), Self::MAX_CHARS);
        Some(match item {
            ItemContent::UserMessage { .. } => return None,
            ItemContent::Reasoning { .. } => Self::Thinking,
            ItemContent::AssistantMessage { .. } | ItemContent::Plan { .. } => Self::Writing,
            ItemContent::CommandExecution { command, .. } => Self::Command {
                text: clip(command),
            },
            ItemContent::FileChange { changes, .. } => Self::Edit {
                path: changes.first().map(|change| clip(&change.path)),
            },
            ItemContent::ImageRead { path, .. } => Self::Read { path: clip(path) },
            ItemContent::WebSearch { query } => Self::WebSearch { query: clip(query) },
            ItemContent::ToolCall {
                name,
                input,
                family,
                ..
            } => match family.as_deref() {
                Some(ToolFamily::Read { path, .. }) => Self::Read { path: clip(path) },
                Some(ToolFamily::WebSearch { query, .. }) => Self::WebSearch { query: clip(query) },
                family => Self::Tool {
                    name: clip(name),
                    target: family
                        .and_then(ToolFamily::subject)
                        .map(str::to_owned)
                        .or_else(|| identifying_argument(input))
                        .map(|target| clip(&target)),
                },
            },
            ItemContent::Subagent { description, .. } => Self::Subagent {
                description: clip(description),
            },
            ItemContent::Other {
                provider_kind,
                summary,
            } => Self::Other {
                kind: clip(provider_kind),
                summary: other_argument(summary).map(|summary| clip(&summary)),
            },
        })
    }
}

/// What an unmodelled item's summary names: the identifying value of a JSON
/// object, whose own kind and id say nothing, or else the summary itself.
pub fn other_argument(summary: &str) -> Option<String> {
    match serde_json::from_str::<serde_json::Value>(summary) {
        Ok(serde_json::Value::Object(mut map)) => {
            map.remove("type");
            map.remove("id");
            identifying_argument(&serde_json::Value::Object(map))
        }
        _ => Some(one_line(summary)).filter(|summary| !summary.is_empty()),
    }
}

/// The plain value that names what a tool call no family claims is about:
/// the first the harness gave, a word before a number. Which of a tool's
/// arguments matters most is the tool's to say, and an unknown tool does
/// not say it.
pub fn identifying_argument(input: &serde_json::Value) -> Option<String> {
    use serde_json::Value;
    fn primitive(value: &Value) -> Option<String> {
        let text = match value {
            Value::String(text) => one_line(text),
            Value::Number(number) => number.to_string(),
            Value::Bool(_) | Value::Null | Value::Array(_) | Value::Object(_) => return None,
        };
        (!text.is_empty()).then_some(text)
    }
    match input {
        Value::Object(map) => map
            .values()
            .filter(|value| value.is_string())
            .find_map(primitive)
            .or_else(|| map.values().find_map(primitive)),
        Value::String(text) => Some(one_line(text)).filter(|text| !text.is_empty()),
        _ => None,
    }
}

/// The turn a live provider is running, located in the session's whole log so
/// a fold of any window of it can find the turn among its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningTurn {
    /// Index of the turn among every turn the whole log folds to.
    pub turn: u64,
    /// When the turn began, if the record that opened it carried a time.
    pub started_at: Option<u64>,
}

/// Folded view of a session's event history. Two timelines are equal only
/// when every later event folds the same onto both, so equality covers the
/// private state the fold continues from, not just what renders.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Timeline {
    /// Top-level entries are shared so virtualized UI snapshots can retain a
    /// turn without cloning its potentially large message, command-output, and
    /// diff payloads. Updates use [`Arc::make_mut`], preserving the value
    /// semantics of a cloned [`Timeline`] while keeping read snapshots cheap.
    pub entries: Vec<Arc<TimelineEntry>>,
    /// One entry per turn ("Work Log" section), in order.
    pub turns: Vec<TurnMeta>,
    pub turn_running: bool,
    pub turn_retry: Option<agent::TurnRetry>,
    pub pending_approvals: Vec<ApprovalRequest>,
    /// The latest structured plan/task list (`PlanUpdated`), if any.
    pub plan_steps: Vec<PlanStep>,
    /// The explanation string from the latest `PlanUpdated`, if any.
    pub plan_explanation: Option<String>,
    /// The harness's standing goal from the latest `GoalUpdated`, if any.
    pub goal: Option<agent::Goal>,
    /// The open user-input request (Claude `AskUserQuestion`, Codex
    /// `requestUserInput` / `request_user_input_async`), if any. Cleared when
    /// the provider reports it resolved or the session closes.
    pub pending_user_input: Option<PendingUserInput>,
    pub usage: Option<TokenUsage>,
    pub resume: Option<ResumeCursor>,
    pub provider_session_id: Option<String>,
    pub model: Option<String>,
    /// Latest model reported by the serving provider. Seeded from `model`.
    last_served_model: Option<String>,
    pub last_turn_status: Option<TurnStatus>,
    /// Why this thread's harness session cannot be rewound, when it cannot.
    pub rewind_unavailable: Option<agent::RewindFailureReason>,
    /// A divider in `entries` still carries an undo token.
    undo_open: bool,
    /// How many handoffs `entries` holds.
    relays: usize,
    boundaries: TurnBoundaries,
    /// Monotonic counter for synthetic entry ids.
    next_synthetic_id: u64,
    /// FileChange items known to have completed successfully. The ids rebuild
    /// deterministically from persisted ItemCompleted events during replay.
    committed_file_change_items: HashSet<String>,
    /// Tool-time accounting for the open turn (see [`TurnMeta::timing`]).
    tool_clock: ToolClock,
    /// Full output lengths of the items whose records arrived shortened
    /// ([`StoredEvent::elided`]), keyed by item id.
    pub elided_outputs: HashMap<String, u64>,
    entry_index: EntryIndex,
}

/// Where each entry id first and last appears in [`Timeline::entries`], so
/// the fold finds an item's entry without a linear search per event, which
/// made folding a long log quadratic. It is derived from `entries`: it covers the
/// prefix it has seen, takes no part in equality, is reset by the fold's own
/// edits other than appends, and every hit is checked against `entries`,
/// which code outside the fold may also edit.
#[derive(Debug, Clone, Default)]
struct EntryIndex {
    first: HashMap<String, usize>,
    last: HashMap<String, usize>,
    covered: usize,
}

impl PartialEq for EntryIndex {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Timeline {
    /// The position of the first entry with `id`.
    fn first_entry(&mut self, id: &str) -> Option<usize> {
        self.indexed_entry(id, |index| &index.first)
    }

    fn last_entry(&mut self, id: &str) -> Option<usize> {
        self.indexed_entry(id, |index| &index.last)
    }

    fn indexed_entry(
        &mut self,
        id: &str,
        map: fn(&EntryIndex) -> &HashMap<String, usize>,
    ) -> Option<usize> {
        let index = &mut self.entry_index;
        if index.covered > self.entries.len() {
            *index = EntryIndex::default();
        }
        for (position, entry) in self.entries.iter().enumerate().skip(index.covered) {
            index.first.entry(entry.id.clone()).or_insert(position);
            index.last.insert(entry.id.clone(), position);
        }
        index.covered = self.entries.len();
        let position = *map(index).get(id)?;
        if self.entries[position].id == id {
            return Some(position);
        }
        self.entry_index = EntryIndex::default();
        self.indexed_entry(id, map)
    }

    /// The entry an item's lifecycle event belongs to: the latest with its id,
    /// unless that one finished in an earlier turn than the open one and was
    /// not backgrounded. Providers reuse ids — the ACP session numbers its
    /// blocks from zero in every process — so such an event starts a new item.
    fn item_entry(&mut self, id: &str) -> Option<usize> {
        let position = self.last_entry(id)?;
        let entry = &self.entries[position];
        let ended_earlier = entry.finished_ts.is_some()
            && !entry.background
            && self
                .boundaries
                .current()
                .is_some_and(|turn| entry.turn < turn);
        (!ended_earlier).then_some(position)
    }

    /// Edits to `entries` other than appends move the positions the index
    /// holds.
    fn reindex_entries(&mut self) {
        self.entry_index = EntryIndex::default();
        self.relays = self
            .entries
            .iter()
            .filter(|entry| match &entry.content {
                EntryContent::ProviderRelay {
                    from_provider,
                    to_provider,
                    moved_from,
                    left,
                    ..
                } => relay_left_session(*from_provider, *to_provider, moved_from, left),
                _ => false,
            })
            .count();
    }

    /// Fold a whole event sequence (replay path). Accepts either bare
    /// [`AgentEvent`]s (ts unknown) or timestamped [`StoredEvent`]s.
    pub fn fold_events(events: impl IntoIterator<Item = impl Into<StoredEvent>>) -> Self {
        let mut timeline = Self::default();
        for event in events {
            timeline.apply_stored(&event.into());
        }
        timeline
    }

    /// Fold records a client already holds, without cloning their payloads:
    /// a history page refolds the whole held log, whose tool outputs and
    /// diffs are the bulk of it.
    pub fn fold_stored<'a>(records: impl IntoIterator<Item = &'a StoredEvent>) -> Self {
        let mut timeline = Self::default();
        for record in records {
            timeline.apply_stored(record);
        }
        timeline
    }

    /// Fold one record, remembering whether it carried a shortened output.
    pub fn apply_stored(&mut self, stored: &StoredEvent) -> TurnBoundaryChange {
        if let Some(bytes) = stored.elided
            && let Some(item_id) = stored.item_id()
        {
            self.elided_outputs.insert(item_id.to_owned(), bytes);
        }
        self.apply_at(stored.ts, &stored.event)
    }

    /// What the running turn last started, or `None` between turns. Items
    /// keep the place they first appeared in, so this moves when a new one
    /// starts and not as the current one streams.
    pub fn latest_activity(&self) -> Option<LatestActivity> {
        if !self.turn_running {
            return None;
        }
        let turn = self.boundaries.current()?;
        self.entries
            .iter()
            .rev()
            .take_while(|entry| entry.turn == turn)
            .find_map(|entry| match &entry.content {
                EntryContent::Item(item) => LatestActivity::of(item),
                _ => None,
            })
    }

    /// The latest `report_result` call this thread made: the tool-call item id
    /// and the report text. Read from the call itself, so a reload or a cold log
    /// sees exactly the report the live stream did. A call the broker failed
    /// still counts: the child chose that text as its report.
    pub fn last_report(&self) -> Option<(&str, &str)> {
        self.entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.content {
                EntryContent::Item(ItemContent::ToolCall {
                    name,
                    input,
                    status,
                    ..
                }) if is_report_result_tool(name) && *status != ItemStatus::InProgress => input
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .map(|text| (entry.id.as_str(), text)),
                _ => None,
            })
    }

    /// Clear any lingering "running" state (used after replaying a stored
    /// session whose provider process is no longer live).
    pub fn mark_idle(&mut self) {
        self.turn_retry = None;
        self.boundaries.settle_running(None);
        self.turn_running = false;
        self.pending_approvals.clear();
        self.pending_user_input = None;
        for turn in &mut self.turns {
            turn.running = false;
        }
    }

    /// The calls whose work goes on in the background: backgrounded, and
    /// not ended.
    pub fn running_background_items(&self) -> impl Iterator<Item = (&TimelineEntry, &ItemContent)> {
        self.entries.iter().filter_map(|entry| {
            let EntryContent::Item(content) = &entry.content else {
                return None;
            };
            let (ItemContent::CommandExecution { status, .. }
            | ItemContent::FileChange { status, .. }
            | ItemContent::ToolCall { status, .. }
            | ItemContent::Subagent { status, .. }) = content
            else {
                return None;
            };
            (entry.background && *status == ItemStatus::InProgress).then_some((&**entry, content))
        })
    }

    /// The completions that end the background work still open here, for a
    /// log whose process cannot finish it.
    pub fn interrupted_background_items(&self) -> Vec<ThreadItem> {
        self.running_background_items()
            .map(|(entry, content)| {
                let mut content = content.clone();
                if let ItemContent::CommandExecution { status, .. }
                | ItemContent::FileChange { status, .. }
                | ItemContent::ToolCall { status, .. }
                | ItemContent::Subagent { status, .. } = &mut content
                {
                    *status = ItemStatus::Interrupted;
                }
                ThreadItem {
                    id: entry.id.clone(),
                    parent_item_id: None,
                    content,
                }
            })
            .collect()
    }

    /// Whether a handoff has left a native session behind in this fold.
    pub fn has_relays(&self) -> bool {
        self.relays > 0
    }

    fn relay(&self, found: impl Fn(usize) -> bool) -> Option<LeftTurn<'_>> {
        if self.relays == 0 {
            return None;
        }
        self.entries.iter().find_map(|entry| match &entry.content {
            EntryContent::ProviderRelay {
                from_provider,
                from_model,
                to_provider,
                moved_from,
                left,
                ..
            } if found(entry.turn)
                && relay_left_session(*from_provider, *to_provider, moved_from, left) =>
            {
                Some(LeftTurn {
                    relay_turn: entry.turn,
                    provider: *from_provider,
                    model: from_model.as_deref(),
                    session: left.as_ref(),
                })
            }
            _ => None,
        })
    }

    /// The native session `turn` ran in, when a handoff has since left it:
    /// the one the first handoff after the turn names. The turns of the
    /// session the thread is in now answer `None`.
    pub fn left_session(&self, turn: usize) -> Option<LeftTurn<'_>> {
        self.relay(|relay_turn| relay_turn > turn)
    }

    /// Where a conversation rewind to the prompt that opened `turn` (absolute
    /// turn `absolute`) takes the thread, when that is out of the session it
    /// is in: `Some(None)` when that session cannot be gone on in. A rewind
    /// to the message a handoff carried returns to the session it left; one
    /// to an earlier message is that session's harness's to make.
    pub fn rewind_return(&self, turn: usize, absolute: usize) -> Option<Option<RewindReturn<'_>>> {
        let (left, harness_rewinds) = match self.relay(|relay_turn| relay_turn == turn) {
            Some(left) => (left, false),
            None => (self.left_session(turn)?, true),
        };
        Some(left.session.and_then(|session| {
            let offered = !harness_rewinds
                || left
                    .provider
                    .caps()
                    .rewind
                    .is_some_and(|control| control.offers(RewindReturn::MODE, absolute, false));
            offered.then_some(RewindReturn {
                provider: left.provider,
                model: left.model,
                session,
                harness_rewinds,
            })
        }))
    }

    /// Where a fork through finished turn `turn` ends in the harness's
    /// session, if it can be named now: a later prompt not yet taken up or
    /// held by a rewind leaves that turn with nothing to anchor on.
    pub fn fork_edge(&self, turn: usize) -> Option<ForkEdge> {
        self.turns
            .get(turn)
            .filter(|meta| !meta.running && meta.status.is_some())?;
        let next = turn + 1;
        if let Some(left) = self.left_session(turn) {
            left.session?;
            left.provider.caps().fork_at.then_some(())?;
            if left.relay_turn == next {
                return Some(ForkEdge::LeftEnd { turn: next });
            }
            return Some(ForkEdge::BeforePrompt {
                turn: next,
                checkpoint_id: self.turns.get(next)?.provider_checkpoint_id.clone()?,
            });
        }
        let Some(meta) = self.turns.get(next) else {
            return Some(ForkEdge::End);
        };
        if let Some(checkpoint_id) = &meta.provider_checkpoint_id {
            return Some(ForkEdge::BeforePrompt {
                turn: next,
                checkpoint_id: checkpoint_id.clone(),
            });
        }
        let holds_more = self.turns.len() > next + 1
            || self
                .entries
                .iter()
                .filter(|entry| entry.turn == next)
                .any(|entry| match &entry.content {
                    EntryContent::Rewound(rewound) => {
                        rewound.later_history == agent::LaterHistory::HiddenUntilNextSend
                    }
                    EntryContent::Item(ItemContent::UserMessage { .. })
                    | EntryContent::Steer { .. } => true,
                    _ => false,
                });
        (!holds_more).then_some(ForkEdge::End)
    }

    /// The running turn of a fold that holds the whole log.
    pub fn running_turn(&self) -> Option<RunningTurn> {
        let turn = self.turns.len().checked_sub(1)?;
        (self.turn_running && self.turns[turn].running).then(|| RunningTurn {
            turn: turn as u64,
            started_at: self.turns[turn].start_ts,
        })
    }

    /// Take liveness from the host instead of the records: only the turn at
    /// `running` runs, timed from `started_at` when given. A fold of a window
    /// cannot tell a live turn from one whose provider stopped without a
    /// record, and a window cut inside the turn misses its start. Nothing the
    /// records built is discarded, so a later settle can revive the turn.
    pub fn settle_running_turn(
        &mut self,
        turn_running: bool,
        running: Option<usize>,
        started_at: Option<u64>,
    ) {
        let running = running.filter(|turn| turn_running && *turn < self.turns.len());
        self.turn_running = turn_running;
        self.boundaries.settle_running(running);
        for (index, turn) in self.turns.iter_mut().enumerate() {
            turn.running = running == Some(index);
        }
        if let (Some(turn), Some(started_at)) = (running, started_at) {
            self.turns[turn].start_ts = Some(started_at);
        }
    }

    /// First user message in the timeline, if any (used for session titles).
    pub fn first_user_message(&self) -> Option<&str> {
        self.entries.iter().find_map(|entry| match &entry.content {
            EntryContent::Item(ItemContent::UserMessage { text, .. })
            | EntryContent::Steer { text, .. } => Some(text.as_str()),
            _ => None,
        })
    }

    /// Apply one event recorded at `ts` (unix ms). Mutates in place, and
    /// returns what the event did to the turns.
    pub fn apply_at(&mut self, ts: Option<u64>, event: &AgentEvent) -> TurnBoundaryChange {
        // Every timestamped event inside the open turn raises the turn's
        // watermark, not just tool lifecycle ones: a completion stamped before
        // any of them means the wall clock regressed, and the breakdown is
        // withheld rather than guessed at. The completion itself is excluded —
        // it is what gets compared against the watermark.
        if self.turn_is_open() && !matches!(event, AgentEvent::TurnCompleted { .. }) {
            self.tool_clock.observe(ts);
        }
        let boundary = self.boundaries.apply(event);
        if boundary.opened && boundary.rewind.is_none() {
            self.push_turn(ts);
        }
        self.apply_payload(ts, event, &boundary);
        boundary
    }

    fn apply_payload(
        &mut self,
        ts: Option<u64>,
        event: &AgentEvent,
        boundary: &TurnBoundaryChange,
    ) {
        match event {
            AgentEvent::TurnRetry { retry } => {
                self.turn_retry = retry.clone();
            }
            AgentEvent::ProviderRelay {
                from_provider,
                from_model,
                to_provider,
                to_model,
                prompt,
                moved_from,
                left,
            } => {
                let turn = self.begin_user_turn();
                let id = self.synthetic_id("relay", ts);
                if relay_left_session(*from_provider, *to_provider, moved_from, left) {
                    self.relays += 1;
                }
                self.entries.push(Arc::new(TimelineEntry {
                    id,
                    content: EntryContent::ProviderRelay {
                        from_provider: *from_provider,
                        from_model: from_model.clone(),
                        to_provider: *to_provider,
                        to_model: to_model.clone(),
                        prompt: prompt.clone(),
                        moved_from: moved_from.clone(),
                        left: left.clone(),
                    },
                    ts,
                    finished_ts: None,
                    background: false,
                    turn,
                }));
            }
            AgentEvent::SessionStarted {
                provider_session_id,
                resume,
                model,
            } => {
                self.provider_session_id = Some(provider_session_id.clone());
                self.resume = Some(resume.clone());
                if model.is_some() {
                    self.model = model.clone();
                }
                self.last_served_model = self.model.clone();
            }
            AgentEvent::ServedModel {
                model,
                from,
                reason,
            } => {
                let turn = self.ensure_turn();
                self.turns[turn].served_model = Some(model.clone());
                // A `[1m]` launch id and the bare id the API reports back
                // name the same model, so that pair draws no divider.
                let same_model = self
                    .last_served_model
                    .as_deref()
                    .map(agent::claude::strip_context_window_suffix)
                    == Some(agent::claude::strip_context_window_suffix(model));
                if !same_model {
                    let id = self.synthetic_id("model", ts);
                    self.entries.push(Arc::new(TimelineEntry {
                        id,
                        content: EntryContent::ModelChanged {
                            // The record's own, not the fold's: a reader
                            // folding a window of the log that starts after
                            // the previous model was named draws it the same.
                            from: from.clone(),
                            to: model.clone(),
                            reason: reason.clone(),
                        },
                        ts,
                        finished_ts: None,
                        background: false,
                        turn,
                    }));
                }
                self.last_served_model = Some(model.clone());
            }
            AgentEvent::TurnStarted { turn_id } => {
                self.turn_retry = None;
                if let Some(usage) = self.usage.as_mut()
                    && usage.freshness == agent::ContextFreshness::Current
                {
                    usage.freshness = agent::ContextFreshness::LastKnown;
                }
                let turn = self.ensure_turn();
                // TurnStarted is the authoritative turn start; prefer it over
                // the opening user message's time when known. It is also the
                // only start the timing breakdown will measure from.
                if let Some(ts) = ts {
                    self.turns[turn].start_ts = Some(ts);
                    self.tool_clock.begin_turn(ts);
                }
                self.turns[turn].provider_turn_id = Some(turn_id.clone());
                self.turns[turn].running = true;
                self.turn_running = true;
                self.last_turn_status = None;
            }
            AgentEvent::TurnAccepted { .. }
            | AgentEvent::WorktreeRequested { .. }
            | AgentEvent::McpServersNotAttached
            | AgentEvent::BackgroundTasksChanged { .. }
            | AgentEvent::SettingsApplied { .. }
            | AgentEvent::BackgroundTaskList { .. } => {}
            AgentEvent::TurnChangesUpdated {
                turn_id,
                changes,
                completeness,
            } => {
                let turn = boundary
                    .target
                    .expect("snapshot or checkpoint opens a turn");
                if !turn_id.is_empty() {
                    self.turns[turn].provider_turn_id = Some(turn_id.clone());
                }
                self.turns[turn].changes = Some(TurnChangeSet {
                    changes: changes.clone(),
                    completeness: *completeness,
                });
            }
            AgentEvent::TurnCheckpoint {
                turn_id,
                checkpoint_id,
            } => {
                let turn = boundary
                    .target
                    .expect("snapshot or checkpoint opens a turn");
                self.turns[turn].provider_turn_id = Some(turn_id.clone());
                self.turns[turn].provider_checkpoint_id = Some(checkpoint_id.clone());
            }
            AgentEvent::SteerCheckpoint {
                request_id,
                checkpoint_id,
            } => {
                if let Some(turn) = boundary.target.and_then(|turn| self.turns.get_mut(turn)) {
                    turn.steer_checkpoints
                        .push((request_id.clone(), checkpoint_id.clone()));
                }
            }
            AgentEvent::CheckpointsCleared => {
                for turn in &mut self.turns {
                    turn.provider_checkpoint_id = None;
                    turn.steer_checkpoints.clear();
                }
                self.close_undo();
                for entry in &mut self.entries {
                    if matches!(&entry.content, EntryContent::Rewound(rewound) if rewound.branch.is_some())
                        && let EntryContent::Rewound(rewound) = &mut Arc::make_mut(entry).content
                    {
                        rewound.branch = None;
                    }
                }
            }
            AgentEvent::RewindUndoClosed => self.close_undo(),
            AgentEvent::RewindAvailability { unavailable } => {
                self.rewind_unavailable = *unavailable;
            }
            AgentEvent::RewindCompleted {
                checkpoint_id,
                mode,
                later_history,
                files,
                incomplete,
                undo_token,
                ..
            } => {
                let mut rewound = Rewound {
                    checkpoint_id: checkpoint_id.clone(),
                    mode: *mode,
                    later_history: *later_history,
                    removed: Vec::new(),
                    removed_turns: 0,
                    files: files.clone(),
                    incomplete: incomplete.clone(),
                    undo_token: None,
                    branch: undo_token
                        .clone()
                        .filter(|_| *later_history == agent::LaterHistory::Branch),
                    removed_meta: Vec::new(),
                    cut_steers: None,
                };
                if let Some(cut) = &boundary.rewind {
                    self.close_undo();
                    self.rewind_conversation(cut, &mut rewound);
                    rewound.undo_token = undo_token.clone();
                    self.undo_open = undo_token.is_some();
                    self.push_turn(ts);
                } else if mode.includes_conversation() {
                    log::warn!(
                        "provider rewind target is absent from the local timeline: {checkpoint_id}"
                    );
                }
                self.push_rewound(ts, rewound);
            }
            AgentEvent::RelayUndone { relay } => {
                let mut rewound = Rewound {
                    checkpoint_id: relay.clone(),
                    mode: RewindReturn::MODE,
                    later_history: agent::LaterHistory::Truncated,
                    removed: Vec::new(),
                    removed_turns: 0,
                    files: None,
                    incomplete: None,
                    undo_token: None,
                    branch: None,
                    removed_meta: Vec::new(),
                    cut_steers: None,
                };
                if let Some(cut) = &boundary.rewind {
                    self.close_undo();
                    self.rewind_conversation(cut, &mut rewound);
                    self.push_turn(ts);
                }
                self.push_rewound(ts, rewound);
            }
            AgentEvent::RewindUndone { checkpoint_id } => {
                if boundary.undone {
                    self.undo_rewind(checkpoint_id);
                }
            }
            AgentEvent::RewindFailed { mode, failure, .. } => {
                self.push_rewind_failure(ts, Some(*mode), failure)
            }
            AgentEvent::RewindUndoFailed { failure, .. } => {
                self.push_rewind_failure(ts, None, failure)
            }
            AgentEvent::RewindPreviewed { .. } => {}
            AgentEvent::TurnCompleted { status, usage, .. } => {
                let newly_completed = self
                    .boundaries
                    .current()
                    .is_none_or(|turn| self.turns[turn].status.is_none());
                let usage = usage.map(|mut usage| {
                    usage.context_window = usage
                        .context_window
                        .or(self.usage.and_then(|u| u.context_window));
                    if let Some(processed) = usage.turn_processed_tokens {
                        let previous = self
                            .usage
                            .and_then(|u| u.total_processed_tokens)
                            .unwrap_or(0);
                        usage.total_processed_tokens =
                            Some(previous.saturating_add(if newly_completed {
                                processed
                            } else {
                                0
                            }));
                    }
                    usage
                });
                self.turn_running = false;
                self.last_turn_status = Some(*status);
                if let Some(turn) = self.boundaries.current() {
                    if ts.is_some() {
                        self.turns[turn].end_ts = ts;
                    }
                    self.turns[turn].status = Some(*status);
                    self.turns[turn].running = false;
                    if let Some(usage) = usage {
                        self.turns[turn].cost_usd = usage.cost_usd;
                        self.turns[turn].provider_duration_ms = usage.duration_ms;
                    }
                    let clock = std::mem::take(&mut self.tool_clock);
                    // A repeated completion finds an already-spent clock; it
                    // must not erase the breakdown the first one derived.
                    if let Some(timing) = finish_timing(clock, self.turns[turn].end_ts) {
                        self.turns[turn].timing = Some(timing);
                    }
                }
                if usage.is_some() {
                    self.usage = usage;
                }
            }
            AgentEvent::ItemStarted(item) => {
                self.upsert_item(ts, item);
                self.track_tool_item(ts, item, ToolLifecycle::Started);
            }
            AgentEvent::ItemUpdated(item) => {
                self.upsert_item(ts, item);
                self.track_tool_item(ts, item, ToolLifecycle::Updated);
            }
            AgentEvent::ItemBackgrounded {
                item_id,
                parent_item_id,
            } => {
                // A child's item is its mirror's, as in `upsert_item`.
                if parent_item_id.is_none()
                    && let Some(position) = self.item_entry(item_id)
                {
                    Arc::make_mut(&mut self.entries[position]).background = true;
                    // The turn no longer waits on it.
                    if self.turn_is_open() {
                        self.tool_clock.mark(ts, item_id, false);
                    }
                }
            }
            AgentEvent::ItemCompleted(item) => {
                let observed = self.item_entry(&item.id).is_some();
                self.upsert_item(ts, item);
                if observed
                    && item.parent_item_id.is_none()
                    && let Some(position) = self.item_entry(&item.id)
                {
                    Arc::make_mut(&mut self.entries[position]).finished_ts = ts;
                }
                self.track_tool_item(ts, item, ToolLifecycle::Completed);
                if matches!(
                    &item.content,
                    ItemContent::FileChange {
                        status: ItemStatus::Completed,
                        ..
                    }
                ) {
                    self.committed_file_change_items.insert(item.id.clone());
                    if let Some(turn) = self.item_turn(&item.id) {
                        self.refresh_partial_turn_changes(turn);
                    }
                }
            }
            AgentEvent::SteerRequested {
                request_id,
                text,
                attachments,
            } => self.request_steer(ts, request_id, text, attachments),
            AgentEvent::SteerAccepted { request_id } => {
                self.accept_steer(request_id, boundary.target)
            }
            AgentEvent::SteerRejected { request_id } => self.reject_steer(request_id),
            AgentEvent::Delta {
                item_id,
                kind,
                text,
            } => self.apply_delta(ts, item_id, *kind, text),
            AgentEvent::ApprovalRequested(request) => {
                if !self.pending_approvals.iter().any(|r| r.id == request.id) {
                    self.pending_approvals.push(request.clone());
                }
            }
            AgentEvent::ApprovalResolved {
                request_id,
                decision,
            } => {
                self.pending_approvals.retain(|r| r.id != *request_id);
                if let agent::ApprovalDecision::Feedback(text) = decision {
                    let turn = self.ensure_turn();
                    let id = self.synthetic_id("feedback", ts);
                    self.entries.push(Arc::new(TimelineEntry {
                        id,
                        content: EntryContent::Feedback { text: text.clone() },
                        ts,
                        finished_ts: None,
                        background: false,
                        turn,
                    }));
                }
            }
            AgentEvent::UserInputRequested {
                request_id,
                questions,
                delivery,
            } => {
                self.pending_user_input = Some(PendingUserInput {
                    request_id: request_id.clone(),
                    questions: questions.clone(),
                    delivery: *delivery,
                });
            }
            AgentEvent::UserInputResolved { request_id, .. } => {
                if self
                    .pending_user_input
                    .as_ref()
                    .is_some_and(|pending| pending.request_id == *request_id)
                {
                    self.pending_user_input = None;
                }
            }
            AgentEvent::TokenUsage(usage) => {
                let mut usage = *usage;
                usage.context_window = usage
                    .context_window
                    .or(self.usage.and_then(|u| u.context_window));
                if usage.turn_processed_tokens.is_some() || usage.total_processed_tokens.is_none() {
                    usage.total_processed_tokens = self
                        .usage
                        .and_then(|u| u.total_processed_tokens)
                        .or(usage.total_processed_tokens);
                }
                self.usage = Some(usage);
            }
            AgentEvent::RequestRetry { message } => {
                let turn = self.ensure_turn();
                let content = EntryContent::RequestRetry {
                    message: message.clone(),
                };
                match self.entries.last_mut() {
                    Some(last) if matches!(last.content, EntryContent::RequestRetry { .. }) => {
                        *last = Arc::new(TimelineEntry {
                            content,
                            ..(**last).clone()
                        });
                    }
                    _ => {
                        let id = self.synthetic_id("retry", ts);
                        self.entries.push(Arc::new(TimelineEntry {
                            id,
                            content,
                            ts,
                            finished_ts: None,
                            background: false,
                            turn,
                        }));
                    }
                }
            }
            AgentEvent::Warning { message } => {
                let turn = self.ensure_turn();
                let id = self.synthetic_id("warning", ts);
                self.entries.push(Arc::new(TimelineEntry {
                    id,
                    content: EntryContent::Warning {
                        message: message.clone(),
                    },
                    ts,
                    finished_ts: None,
                    background: false,
                    turn,
                }));
            }
            AgentEvent::ProviderStartFailed { error } => {
                let turn = self.ensure_turn();
                let id = self.synthetic_id("error", ts);
                self.entries.push(Arc::new(TimelineEntry {
                    id,
                    content: EntryContent::ProviderStartError {
                        error: error.clone(),
                    },
                    ts,
                    finished_ts: None,
                    background: false,
                    turn,
                }));
            }
            AgentEvent::Error { message, .. } => {
                let turn = self.ensure_turn();
                let id = self.synthetic_id("error", ts);
                self.entries.push(Arc::new(TimelineEntry {
                    id,
                    content: EntryContent::Error {
                        message: message.clone(),
                        limit_resets_at: None,
                    },
                    ts,
                    finished_ts: None,
                    background: false,
                    turn,
                }));
            }
            AgentEvent::UsageLimitReached { resets_at } => {
                let Some((index, entry)) = self
                    .entries
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(_, entry)| matches!(entry.content, EntryContent::Error { .. }))
                else {
                    return;
                };
                let EntryContent::Error { message, .. } = &entry.content else {
                    unreachable!();
                };
                self.entries[index] = Arc::new(TimelineEntry {
                    id: entry.id.clone(),
                    content: EntryContent::Error {
                        message: message.clone(),
                        limit_resets_at: Some(*resets_at),
                    },
                    ts: entry.ts,
                    finished_ts: None,
                    background: false,
                    turn: entry.turn,
                });
            }
            AgentEvent::SessionClosed { reason } => {
                self.turn_retry = None;
                // An abnormal close carries the provider's dying words (exit
                // status, stderr tail). Fold them into the transcript so the
                // cause survives past the one-shot toast — a reopened session
                // must still show why the work stopped.
                if let Some(reason) = reason {
                    let turn = self.ensure_turn();
                    let id = self.synthetic_id("error", ts);
                    self.entries.push(Arc::new(TimelineEntry {
                        id,
                        content: EntryContent::Error {
                            message: reason.clone(),
                            limit_resets_at: None,
                        },
                        ts,
                        finished_ts: None,
                        background: false,
                        turn,
                    }));
                }
                self.turn_running = false;
                self.pending_approvals.clear();
                self.pending_user_input = None;
                if let Some(turn) = self.boundaries.current() {
                    self.turns[turn].running = false;
                }
            }
            AgentEvent::PlanUpdated {
                steps, explanation, ..
            } => {
                self.plan_steps = steps.clone();
                self.plan_explanation = explanation.clone();
            }
            AgentEvent::GoalUpdated { goal } => self.goal = goal.clone(),
            AgentEvent::ControlAction(action) => {
                let turn = self.ensure_turn();
                let id = self.synthetic_id("control", ts);
                self.entries.push(Arc::new(TimelineEntry {
                    id,
                    content: EntryContent::ControlAction(action.clone()),
                    ts,
                    finished_ts: None,
                    background: false,
                    turn,
                }));
            }
            AgentEvent::ContextCompacted(compaction) => {
                let in_progress = compaction.in_progress;
                let usage = self.usage.get_or_insert_with(Default::default);
                if in_progress && usage.freshness == agent::ContextFreshness::Compacting {
                    return;
                }
                usage.freshness = if in_progress {
                    agent::ContextFreshness::Compacting
                } else {
                    agent::ContextFreshness::AwaitingObservation
                };
                if !in_progress {
                    usage.used_tokens = None;
                    usage.input_tokens = None;
                    usage.cached_input_tokens = None;
                    usage.output_tokens = None;
                }
                let turn = self.ensure_turn();
                let id = self.synthetic_id("compacted", ts);
                self.entries.push(Arc::new(TimelineEntry {
                    id,
                    content: EntryContent::ContextCompacted(compaction.clone()),
                    ts,
                    finished_ts: None,
                    background: false,
                    turn,
                }));
            }
            AgentEvent::ContextWindowChanged { window } => {
                let turn = self.ensure_turn();
                let id = self.synthetic_id("context-window", ts);
                self.entries.push(Arc::new(TimelineEntry {
                    id,
                    content: EntryContent::ContextWindowChanged { window: *window },
                    ts,
                    finished_ts: None,
                    background: false,
                    turn,
                }));
            }
            AgentEvent::SettingsKept => {
                let turn = self.ensure_turn();
                let id = self.synthetic_id("settings-kept", ts);
                self.entries.push(Arc::new(TimelineEntry {
                    id,
                    content: EntryContent::SettingsKept,
                    ts,
                    finished_ts: None,
                    background: false,
                    turn,
                }));
            }
            AgentEvent::SessionMoved { from, to, .. } => {
                let turn = self.ensure_turn();
                let id = self.synthetic_id("moved", ts);
                self.entries.push(Arc::new(TimelineEntry {
                    id,
                    content: EntryContent::SessionMoved {
                        from: from.clone(),
                        to: to.clone(),
                    },
                    ts,
                    finished_ts: None,
                    background: false,
                    turn,
                }));
            }
            AgentEvent::WorktreeResolved {
                request, answer, ..
            } => {
                let turn = self.ensure_turn();
                let id = self.synthetic_id("worktree", ts);
                self.entries.push(Arc::new(TimelineEntry {
                    id,
                    content: EntryContent::Worktree {
                        request: request.clone(),
                        answer: answer.clone(),
                    },
                    ts,
                    finished_ts: None,
                    background: false,
                    turn,
                }));
            }
            AgentEvent::UnreadableRecord { line } => {
                let turn = self.ensure_turn();
                let id = self.synthetic_id("unreadable", ts);
                self.entries.push(Arc::new(TimelineEntry {
                    id,
                    content: EntryContent::Unreadable { line: line.clone() },
                    ts,
                    finished_ts: None,
                    background: false,
                    turn,
                }));
            }
            AgentEvent::RecordWithheld { bytes, row } => {
                let turn = self.ensure_turn();
                let id = self.synthetic_id("withheld", ts);
                self.entries.push(Arc::new(TimelineEntry {
                    id,
                    content: EntryContent::Withheld {
                        bytes: *bytes,
                        row: *row,
                    },
                    ts,
                    finished_ts: None,
                    background: false,
                    turn,
                }));
            }
            // Composer metadata belongs to the runtime, not the timeline.
            AgentEvent::ProviderCommands { .. }
            | AgentEvent::ProviderOptions { .. }
            | AgentEvent::SessionModel { .. } => {}
        }
    }

    /// The turn a provider-addressed record lands on: the first turn carrying
    /// `turn_id`, else the current one. `None` means the record opens a turn.
    fn provider_turn(&self, turn_id: &str) -> Option<usize> {
        self.boundaries.provider_turn(turn_id)
    }

    /// Whether the current turn is still accumulating. A turn is finished once
    /// a `TurnCompleted` has been folded, which records a status even when the
    /// event carried no timestamp to store as `end_ts`; both must be checked or
    /// a stray later transition would leak into the next turn's accounting
    fn turn_is_open(&self) -> bool {
        self.boundaries.is_open()
    }

    /// Feed one item lifecycle transition to the open turn's clock, ignoring
    /// items that are not tool-like. Transitions arriving after the turn has
    /// already been finalized are ignored too, so a settled breakdown cannot be
    /// reopened.
    fn track_tool_item(&mut self, ts: Option<u64>, item: &ThreadItem, lifecycle: ToolLifecycle) {
        let Some(state) = tool_item_state(&item.content) else {
            return;
        };
        if self.turn_is_open() {
            let waited_on = !self
                .item_entry(&item.id)
                .is_some_and(|position| self.entries[position].background);
            self.tool_clock
                .mark(ts, &item.id, waited_on && tool_is_active(lifecycle, state));
        }
    }

    /// Push a new (open) turn and make it current. `start_ts` seeds the turn's
    /// start time (refined later by a TurnStarted event if one arrives).
    fn push_turn(&mut self, start_ts: Option<u64>) {
        self.tool_clock = ToolClock::default();
        self.turns.push(TurnMeta {
            provider_turn_id: None,
            provider_checkpoint_id: None,
            steer_checkpoints: Vec::new(),
            start_ts,
            end_ts: None,
            status: None,
            running: false,
            changes: None,
            timing: None,
            served_model: None,
            cost_usd: None,
            provider_duration_ms: None,
        });
    }

    /// The divider of a cut, in the turn it opened.
    fn push_rewound(&mut self, ts: Option<u64>, rewound: Rewound) {
        // A window of the log may begin after every turn the cut touched.
        let Some(turn) = self.boundaries.current() else {
            return;
        };
        let id = self.synthetic_id("rewound", ts);
        self.entries.push(Arc::new(TimelineEntry {
            id,
            content: EntryContent::Rewound(Box::new(rewound)),
            ts,
            finished_ts: None,
            background: false,
            turn,
        }));
    }

    /// Apply a provider-confirmed conversation rewind. The event log remains
    /// append-only; replaying this marker produces the provider's authoritative
    /// active history without tcode rewriting either transcript file. What the
    /// cut takes out moves into `rewound`, to be read and, by an undo, put back.
    fn rewind_conversation(&mut self, cut: &RewindCut, rewound: &mut Rewound) {
        let from = match &cut.steer {
            Some(steer) => self
                .entries
                .iter()
                .position(|entry| entry.id == *steer && entry.turn == cut.turn),
            None => None,
        }
        .unwrap_or_else(|| {
            let turn = cut.turn + usize::from(cut.steer.is_some());
            self.entries
                .iter()
                .position(|entry| entry.turn >= turn)
                .unwrap_or(self.entries.len())
        });
        rewound.removed = self.entries.split_off(from);
        self.reindex_entries();
        let kept = cut.turn + usize::from(cut.steer.is_some());
        rewound.removed_meta = self.turns.split_off(kept.min(self.turns.len()));
        rewound.removed_turns = rewound.removed_meta.len();
        if cut.steer.is_some()
            && let Some(turn) = self.turns.get_mut(cut.turn)
        {
            rewound.cut_steers = Some((cut.turn, turn.steer_checkpoints.clone()));
            let removed = &rewound.removed;
            turn.steer_checkpoints
                .retain(|(id, _)| !removed.iter().any(|entry| entry.id == *id));
            turn.running = false;
        }
        self.tool_clock = ToolClock::default();
        self.turn_running = false;
        self.pending_approvals.clear();
        self.pending_user_input = None;
        self.plan_steps.clear();
        self.plan_explanation = None;
        self.usage = None;
        self.last_turn_status = self.turns.last().and_then(|turn| turn.status);
        self.committed_file_change_items
            .retain(|item_id| self.entries.iter().any(|entry| entry.id == *item_id));
    }

    /// Put back what the latest rewind removed. The divider's turn holds
    /// nothing of the user's: no message was sent after it, or the harness
    /// would no longer undo it.
    fn undo_rewind(&mut self, checkpoint_id: &str) {
        let Some(position) = self.entries.iter().rposition(|entry| {
            matches!(&entry.content, EntryContent::Rewound(rewound)
                if rewound.checkpoint_id == checkpoint_id && rewound.undo_token.is_some())
        }) else {
            return;
        };
        let divider = self.entries.remove(position);
        let EntryContent::Rewound(rewound) = &divider.content else {
            unreachable!("the position holds a rewind divider");
        };
        let rewound = (**rewound).clone();
        self.undo_open = false;
        let divider_turn = divider.turn;
        let notes: Vec<_> = self
            .entries
            .extract_if(.., |entry| entry.turn >= divider_turn)
            .collect();
        self.turns.truncate(divider_turn);
        if let Some((turn, steers)) = rewound.cut_steers
            && let Some(turn) = self.turns.get_mut(turn)
        {
            turn.steer_checkpoints = steers;
        }
        self.turns.extend(rewound.removed_meta);
        self.entries.extend(rewound.removed);
        let last = self.turns.len().saturating_sub(1);
        self.entries.extend(notes.into_iter().map(|mut entry| {
            Arc::make_mut(&mut entry).turn = last;
            entry
        }));
        self.reindex_entries();
        self.tool_clock = ToolClock::default();
        self.last_turn_status = self.turns.last().and_then(|turn| turn.status);
    }

    /// A message sent after a rewind makes it final.
    fn close_undo(&mut self) {
        if !std::mem::take(&mut self.undo_open) {
            return;
        }
        if let Some(entry) = self.entries.iter_mut().rev().find(|entry| {
            matches!(&entry.content, EntryContent::Rewound(rewound) if rewound.undo_token.is_some())
        }) && let EntryContent::Rewound(rewound) = &mut Arc::make_mut(entry).content
        {
            rewound.undo_token = None;
        }
    }

    fn push_rewind_failure(
        &mut self,
        ts: Option<u64>,
        mode: Option<agent::RewindMode>,
        failure: &agent::RewindFailure,
    ) {
        let Some(turn) = self.boundaries.current() else {
            return;
        };
        let id = self.synthetic_id("rewind-failed", ts);
        self.entries.push(Arc::new(TimelineEntry {
            id,
            content: EntryContent::RewindFailed {
                mode,
                failure: failure.clone(),
            },
            ts,
            finished_ts: None,
            background: false,
            turn,
        }));
    }

    fn ensure_turn(&self) -> usize {
        self.boundaries
            .current()
            .expect("the turn-opening policy supplied a turn")
    }

    fn begin_user_turn(&mut self) -> usize {
        self.turn_retry = None;
        self.ensure_turn()
    }

    /// Ids for entries that have no provider item. The UI pages history in
    /// from the tail, so an id must not depend on how many records precede it:
    /// a timestamped record names itself, and only untimestamped legacy logs
    /// fall back to the fold-relative counter.
    fn synthetic_id(&mut self, prefix: &str, ts: Option<u64>) -> String {
        let mut next = self.next_synthetic_id;
        let id = synthetic_entry_id(prefix, ts, &mut next, |id| self.first_entry(id).is_some());
        self.next_synthetic_id = next;
        id
    }

    fn refresh_partial_turn_changes(&mut self, turn: usize) {
        if self.turns[turn]
            .changes
            .as_ref()
            .is_some_and(|changes| changes.completeness == ChangeCompleteness::Exact)
        {
            return;
        }
        let fragments = self.entries.iter().filter_map(|entry| {
            if entry.turn != turn || !self.committed_file_change_items.contains(&entry.id) {
                return None;
            }
            match &entry.content {
                EntryContent::Item(ItemContent::FileChange { changes, .. }) => {
                    Some(changes.as_slice())
                }
                _ => None,
            }
        });
        let changes = merge_file_changes_by_path(fragments.flatten());
        self.turns[turn].changes = Some(TurnChangeSet {
            changes,
            completeness: ChangeCompleteness::Partial,
        });
    }

    fn item_turn(&mut self, item_id: &str) -> Option<usize> {
        self.last_entry(item_id)
            .map(|position| self.entries[position].turn)
    }

    fn upsert_item(&mut self, ts: Option<u64>, item: &ThreadItem) {
        // A subagent's items are its mirror thread's: the runtime reroutes
        // them, and a row an older build wrote into the parent's log is read
        // as it is stored (the store's `read_row`). Only a fold of decoded
        // rows, as compaction makes, still meets one here.
        if item.parent_item_id.is_some() {
            return;
        }
        let incoming = EntryContent::Item(item.content.clone());
        if let Some(position) = self.item_entry(&item.id) {
            let entry = Arc::make_mut(&mut self.entries[position]);
            entry.content = merge_content(
                std::mem::replace(&mut entry.content, incoming.clone()),
                incoming,
            );
        } else {
            let turn = if matches!(
                incoming,
                EntryContent::Item(ItemContent::UserMessage { .. })
            ) {
                self.close_undo();
                self.begin_user_turn()
            } else {
                self.ensure_turn()
            };
            self.entries.push(Arc::new(TimelineEntry {
                id: item.id.clone(),
                content: incoming,
                ts,
                finished_ts: None,
                background: false,
                turn,
            }));
        }
    }

    fn request_steer(
        &mut self,
        ts: Option<u64>,
        request_id: &str,
        text: &str,
        attachments: &[String],
    ) {
        if self.first_entry(request_id).is_some() {
            return;
        }
        self.close_undo();
        let turn = self.ensure_turn();
        self.entries.push(Arc::new(TimelineEntry {
            id: request_id.to_owned(),
            content: EntryContent::Steer {
                text: text.to_owned(),
                status: SteeringStatus::Pending,
                attachments: attachments.to_vec(),
            },
            ts,
            finished_ts: None,
            background: false,
            turn,
        }));
    }

    fn accept_steer(&mut self, request_id: &str, target: Option<usize>) {
        let Some(position) = self.first_entry(request_id) else {
            return;
        };
        if !matches!(
            self.entries[position].content,
            EntryContent::Steer {
                status: SteeringStatus::Pending,
                ..
            }
        ) {
            return;
        }

        let mut entry = self.entries.remove(position);
        self.reindex_entries();
        if let EntryContent::Steer {
            status: status @ SteeringStatus::Pending,
            ..
        } = &mut Arc::make_mut(&mut entry).content
        {
            *status = SteeringStatus::Accepted;
        }
        if let Some(turn) = target {
            Arc::make_mut(&mut entry).turn = turn;
        }
        self.entries.push(entry);
    }

    /// The provider never consumed the steer; the runtime re-sends its text as
    /// an ordinary message, which records its own user entry.
    fn reject_steer(&mut self, request_id: &str) {
        self.entries.retain(|entry| {
            entry.id != request_id
                || !matches!(
                    entry.content,
                    EntryContent::Steer {
                        status: SteeringStatus::Pending,
                        ..
                    }
                )
        });
        self.reindex_entries();
    }

    fn apply_delta(&mut self, ts: Option<u64>, item_id: &str, kind: DeltaKind, text: &str) {
        // Background output belongs to its original call across turns.
        // Other streams can reuse a placeholder id in each new turn.
        let current_turn = self.boundaries.current();
        if let Some(position) = self.last_entry(item_id).filter(|&position| {
            let entry = &self.entries[position];
            entry.background || Some(entry.turn) == current_turn
        }) {
            let entry = Arc::make_mut(&mut self.entries[position]);
            match (&mut entry.content, kind) {
                (
                    EntryContent::Item(ItemContent::AssistantMessage { text: existing, .. }),
                    DeltaKind::AssistantText,
                )
                | (
                    EntryContent::Item(ItemContent::Plan { markdown: existing }),
                    DeltaKind::AssistantText,
                )
                | (
                    EntryContent::Item(ItemContent::Reasoning { text: existing }),
                    DeltaKind::ReasoningText,
                ) => {
                    existing.push_str(text);
                }
                (
                    EntryContent::Item(ItemContent::CommandExecution { output, .. }),
                    DeltaKind::CommandOutput,
                ) => {
                    output.push_str(text);
                }
                _ => log::warn!("delta kind {kind:?} does not match item {item_id}"),
            }
            return;
        }
        // Providers may stream deltas before announcing the item: create lazily.
        let content = match kind {
            DeltaKind::AssistantText => EntryContent::Item(ItemContent::AssistantMessage {
                text: text.into(),
                summarized: false,
            }),
            DeltaKind::ReasoningText => {
                EntryContent::Item(ItemContent::Reasoning { text: text.into() })
            }
            DeltaKind::CommandOutput => EntryContent::Item(ItemContent::CommandExecution {
                command: String::new(),
                input: Vec::new(),
                output: text.into(),
                exit_code: None,
                status: ItemStatus::InProgress,
            }),
        };
        let turn = self.ensure_turn();
        self.entries.push(Arc::new(TimelineEntry {
            id: item_id.to_string(),
            content,
            ts,
            finished_ts: None,
            background: false,
            turn,
        }));
    }
}

/// The tool a subagent reports through: tcode_control's `report_result`.
pub fn is_report_result_tool(name: &str) -> bool {
    let name = agent::ToolName::parse(name);
    name.tool == "report_result" && name.server == Some(agent::McpRegistration::SERVER_NAME_CONTROL)
}

/// Merge an authoritative item snapshot over an existing entry, keeping
/// incremental text or file diffs when the snapshot omits them.
fn merge_content(existing: EntryContent, incoming: EntryContent) -> EntryContent {
    match (existing, incoming) {
        (
            EntryContent::Steer { status, .. },
            EntryContent::Item(ItemContent::UserMessage { text, attachments }),
        ) => EntryContent::Steer {
            text,
            status,
            attachments,
        },
        (
            EntryContent::Item(ItemContent::AssistantMessage { text: old, .. }),
            EntryContent::Item(ItemContent::AssistantMessage {
                text: new,
                summarized,
            }),
        ) => EntryContent::Item(ItemContent::AssistantMessage {
            text: merge_text(old, new),
            summarized,
        }),
        (
            EntryContent::Item(ItemContent::Reasoning { text: old }),
            EntryContent::Item(ItemContent::Reasoning { text: new }),
        ) => EntryContent::Item(ItemContent::Reasoning {
            text: merge_text(old, new),
        }),
        (
            EntryContent::Item(
                ItemContent::Plan { markdown: old }
                | ItemContent::AssistantMessage { text: old, .. },
            ),
            EntryContent::Item(ItemContent::Plan { markdown: new }),
        ) => EntryContent::Item(ItemContent::Plan {
            markdown: merge_text(old, new),
        }),
        (
            EntryContent::Item(ItemContent::CommandExecution {
                output: old_output, ..
            }),
            EntryContent::Item(ItemContent::CommandExecution {
                command,
                input,
                output,
                exit_code,
                status,
            }),
        ) => EntryContent::Item(ItemContent::CommandExecution {
            command,
            input,
            output: merge_text(old_output, output),
            exit_code,
            status,
        }),
        (
            EntryContent::Item(ItemContent::FileChange {
                changes: existing_changes,
                ..
            }),
            EntryContent::Item(ItemContent::FileChange {
                mut changes,
                status,
                output,
                command,
            }),
        ) => {
            for change in &mut changes {
                if change.diff.is_none()
                    && let Some(existing_diff) = existing_changes
                        .iter()
                        .find(|existing| existing.path == change.path)
                        .and_then(|existing| existing.diff.as_ref())
                {
                    change.diff = Some(existing_diff.clone());
                }
            }
            EntryContent::Item(ItemContent::FileChange {
                changes,
                status,
                output,
                command,
            })
        }
        (
            EntryContent::Item(ItemContent::Subagent {
                summary: old_summary,
                model: old_model,
                effort: old_effort,
                ..
            }),
            EntryContent::Item(ItemContent::Subagent {
                agent_type,
                description,
                status,
                summary,
                model,
                effort,
            }),
        ) => EntryContent::Item(ItemContent::Subagent {
            agent_type,
            description,
            status,
            summary: summary.or(old_summary),
            model: model.or(old_model),
            effort: effort.or(old_effort),
        }),
        (_, incoming) => incoming,
    }
}

/// Merge an item snapshot's text (`new`) over text already accumulated from
/// deltas (`old`).
///
/// Snapshots (`ItemStarted` / `ItemUpdated` / `ItemCompleted`) are authoritative
/// when they carry text, but they can *lag* the delta stream: providers emit an
/// item snapshot holding the text so far while more deltas are still arriving.
/// Three rules:
///
/// * an empty snapshot never clobbers accumulated text;
/// * a snapshot that is only a prefix of what the deltas already produced (a
///   lagging/partial snapshot) never shortens it — shortening would make the
///   next delta look like a fresh append and duplicate the overlapping text;
/// * a snapshot with different text replaces (never concatenates onto) the
///   accumulated text.
fn merge_text(old: String, new: String) -> String {
    if new.is_empty() || old.starts_with(new.as_str()) {
        old
    } else {
        new
    }
}

#[cfg(test)]
mod tests;
