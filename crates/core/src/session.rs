//! Pure timeline fold: canonical [`AgentEvent`]s in, renderable timeline out.
//!
//! The same fold is used for live event streams and for JSONL replay, so the
//! UI renders identically in both cases.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use agent::{
    AgentEvent, ApprovalRequest, DeltaKind, FileChange, ItemContent, ItemStatus, PlanStep,
    ResumeCursor, ThreadItem, TokenUsage, TurnStatus, UserInputQuestion,
};

/// A local review note attached to a range in the diff panel. These live in
/// the composer draft until the next send; they are never written to session
/// history as separate events.
#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

    /// The final rendered-row index covered by this comment.
    pub fn end_index(&self) -> usize {
        self.end_index
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

/// Serialize review notes using T3's exact `<review_comment ...>` wire format.
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

/// One persisted event, optionally tagged with the wall-clock time (unix ms)
/// at which it was recorded. Legacy `.jsonl` lines replay with `ts == None`;
/// envelope lines carry the recorded timestamp.
#[derive(Debug, Clone)]
pub struct StoredEvent {
    pub ts: Option<u64>,
    pub event: AgentEvent,
}

impl From<AgentEvent> for StoredEvent {
    fn from(event: AgentEvent) -> Self {
        StoredEvent { ts: None, event }
    }
}

/// One renderable row in the chat timeline.
#[derive(Debug, Clone)]
pub struct TimelineEntry {
    /// Provider item id (or a synthetic id for errors).
    pub id: String,
    pub content: EntryContent,
    /// Wall-clock time (unix ms) this entry was first observed, if known.
    pub ts: Option<u64>,
    /// Index into [`Timeline::turns`] of the turn this entry belongs to.
    pub turn: usize,
}

/// Per-turn ("Work Log" section) metadata folded from turn lifecycle events.
#[derive(Debug, Clone, Default)]
pub struct TurnMeta {
    /// When the turn began (TurnStarted, or the opening user message).
    pub start_ts: Option<u64>,
    /// When the turn finished (TurnCompleted).
    pub end_ts: Option<u64>,
    pub status: Option<TurnStatus>,
    /// Whether this turn is currently running.
    pub running: bool,
}

impl TurnMeta {
    /// Wall-clock duration of the turn in whole seconds, when both ends known.
    pub fn duration_secs(&self) -> Option<u64> {
        match (self.start_ts, self.end_ts) {
            (Some(start), Some(end)) if end >= start => Some((end - start) / 1000),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum EntryContent {
    User {
        text: String,
        /// Delivery state for a message injected into an already-open turn.
        steering: Option<SteeringStatus>,
        /// Byte length of an injected context prefix folded into `text` (the
        /// orchestrate guidance + configuration composed ahead of the user's own
        /// words). When present, the UI renders `text[..context_len]` as a
        /// collapsed disclosure row and keeps the bubble to `text[context_len..]`.
        /// `None` for ordinary messages and for logs predating the annotation.
        context_len: Option<usize>,
    },
    Assistant {
        text: String,
    },
    Reasoning {
        text: String,
    },
    Command {
        command: String,
        output: String,
        exit_code: Option<i32>,
        status: ItemStatus,
    },
    FileChange {
        changes: Vec<FileChange>,
    },
    Tool {
        name: String,
        input: serde_json::Value,
        output: Option<String>,
        status: ItemStatus,
    },
    Subagent {
        agent_type: String,
        description: String,
        status: ItemStatus,
        summary: Option<String>,
    },
    Error {
        message: String,
    },
    #[rustfmt::skip]
    ProviderStartError { error: String },
    /// The provider compacted its context window (a "Context compacted" work-log row).
    ContextCompacted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SteeringStatus {
    Pending,
    Accepted,
}

/// A proposed plan captured this session (Codex plan item / Claude
/// `ExitPlanMode`). Streaming deltas accumulate into `markdown`; a `ProposedPlan`
/// event replaces it with the final text.
#[derive(Debug, Clone, Default)]
pub struct ProposedPlan {
    pub item_id: String,
    pub markdown: String,
    /// Index into [`Timeline::turns`] of the turn that produced it.
    pub turn: usize,
}

/// Folded view of a session's event history.
#[derive(Debug, Clone, Default)]
pub struct Timeline {
    /// Top-level entries are shared so virtualized UI snapshots can retain a
    /// turn without cloning its potentially large message, command-output, and
    /// diff payloads. Updates use [`Arc::make_mut`], preserving the value
    /// semantics of a cloned [`Timeline`] while keeping read snapshots cheap.
    pub entries: Vec<Arc<TimelineEntry>>,
    /// Child activity grouped by the top-level subagent spawn item id.
    pub children: HashMap<String, Vec<Arc<TimelineEntry>>>,
    /// Parent ids whose child activity exceeded the in-memory progress cap.
    truncated_children: HashSet<String>,
    /// One entry per turn ("Work Log" section), in order.
    pub turns: Vec<TurnMeta>,
    pub turn_running: bool,
    pub pending_approvals: Vec<ApprovalRequest>,
    /// The latest proposed plan captured this session, if any. Survives replay
    /// (it is the accept/refine anchor) until a newer plan supersedes it.
    pub proposed_plan: Option<ProposedPlan>,
    /// The latest structured plan/task list (`PlanUpdated`), if any.
    pub plan_steps: Vec<PlanStep>,
    /// The explanation string from the latest `PlanUpdated`, if any.
    pub plan_explanation: Option<String>,
    /// The active user-input request (Claude `AskUserQuestion` / Codex
    /// `requestUserInput`), if the agent is currently blocked on one. Cleared
    /// when it resolves or the turn ends.
    pub pending_user_input: Option<(String, Vec<UserInputQuestion>)>,
    pub usage: Option<TokenUsage>,
    pub resume: Option<ResumeCursor>,
    pub provider_session_id: Option<String>,
    pub model: Option<String>,
    pub last_turn_status: Option<TurnStatus>,
    /// The turn currently accumulating entries, if any.
    current_turn: Option<usize>,
    /// Monotonic counter for synthetic entry ids.
    next_synthetic_id: u64,
}

impl Timeline {
    #[allow(dead_code)]
    pub fn children(&self, parent_id: &str) -> &[Arc<TimelineEntry>] {
        self.children.get(parent_id).map_or(&[], Vec::as_slice)
    }

    pub fn children_truncated(&self, parent_id: &str) -> bool {
        self.truncated_children.contains(parent_id)
    }

    /// Fold a whole event sequence (replay path). Accepts either bare
    /// [`AgentEvent`]s (ts unknown) or timestamped [`StoredEvent`]s.
    pub fn fold_events(events: impl IntoIterator<Item = impl Into<StoredEvent>>) -> Self {
        let mut timeline = Self::default();
        for event in events {
            let stored = event.into();
            timeline.apply_at(stored.ts, &stored.event);
        }
        timeline
    }

    /// Clear any lingering "running" state (used after replaying a stored
    /// session whose provider process is no longer live).
    pub fn mark_idle(&mut self) {
        self.turn_running = false;
        self.pending_approvals.clear();
        self.pending_user_input = None;
        for turn in &mut self.turns {
            turn.running = false;
        }
    }

    /// First user message in the timeline, if any (used for session titles).
    pub fn first_user_message(&self) -> Option<&str> {
        self.entries.iter().find_map(|entry| match &entry.content {
            EntryContent::User { text, .. } => Some(text.as_str()),
            _ => None,
        })
    }

    /// Apply one event recorded at `ts` (unix ms). Mutates in place.
    pub fn apply_at(&mut self, ts: Option<u64>, event: &AgentEvent) {
        match event {
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
            }
            AgentEvent::TurnStarted { .. } => {
                // Reuse the open turn (typically opened by the user message);
                // otherwise begin a fresh one.
                let turn = match self.current_turn {
                    Some(t) if self.turns[t].end_ts.is_none() => t,
                    _ => self.push_turn(ts),
                };
                // TurnStarted is the authoritative turn start; prefer it over
                // the opening user message's time when known.
                if ts.is_some() {
                    self.turns[turn].start_ts = ts;
                }
                self.turns[turn].running = true;
                self.turn_running = true;
                self.last_turn_status = None;
            }
            AgentEvent::TurnCompleted { status, usage, .. } => {
                self.turn_running = false;
                self.last_turn_status = Some(*status);
                if let Some(turn) = self.current_turn {
                    if ts.is_some() {
                        self.turns[turn].end_ts = ts;
                    }
                    self.turns[turn].status = Some(*status);
                    self.turns[turn].running = false;
                }
                if usage.is_some() {
                    self.usage = *usage;
                }
                // A finished turn can no longer be waiting on approvals or input.
                self.pending_approvals.clear();
                self.pending_user_input = None;
            }
            AgentEvent::ItemStarted(item)
            | AgentEvent::ItemUpdated(item)
            | AgentEvent::ItemCompleted(item) => self.upsert_item(ts, item),
            AgentEvent::SteerRequested { request_id, text } => {
                self.request_steer(ts, request_id, text)
            }
            AgentEvent::SteerAccepted { request_id } => self.accept_steer(request_id),
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
            AgentEvent::ApprovalResolved { request_id, .. } => {
                self.pending_approvals.retain(|r| r.id != *request_id);
            }
            AgentEvent::UserInputRequested {
                request_id,
                questions,
            } => {
                self.pending_user_input = Some((request_id.clone(), questions.clone()));
            }
            AgentEvent::UserInputResolved { request_id, .. } => {
                if self
                    .pending_user_input
                    .as_ref()
                    .is_some_and(|(id, _)| id == request_id)
                {
                    self.pending_user_input = None;
                }
            }
            AgentEvent::TokenUsage(usage) => self.usage = Some(*usage),
            AgentEvent::Warning(message) => log::warn!("provider warning: {message}"),
            AgentEvent::ProviderStartFailed { error } => {
                let turn = self.ensure_turn(ts);
                let id = self.synthetic_id("error");
                self.entries.push(Arc::new(TimelineEntry {
                    id,
                    content: EntryContent::ProviderStartError {
                        error: error.clone(),
                    },
                    ts,
                    turn,
                }));
            }
            AgentEvent::Error { message, .. } => {
                let turn = self.ensure_turn(ts);
                let id = self.synthetic_id("error");
                self.entries.push(Arc::new(TimelineEntry {
                    id,
                    content: EntryContent::Error {
                        message: message.clone(),
                    },
                    ts,
                    turn,
                }));
            }
            AgentEvent::SessionClosed { reason } => {
                // An abnormal close carries the provider's dying words (exit
                // status, stderr tail). Fold them into the transcript so the
                // cause survives past the one-shot toast — a reopened session
                // must still show why the work stopped.
                if let Some(reason) = reason {
                    let turn = self.ensure_turn(ts);
                    let id = self.synthetic_id("error");
                    self.entries.push(Arc::new(TimelineEntry {
                        id,
                        content: EntryContent::Error {
                            message: reason.clone(),
                        },
                        ts,
                        turn,
                    }));
                }
                self.turn_running = false;
                self.pending_approvals.clear();
                self.pending_user_input = None;
                if let Some(turn) = self.current_turn {
                    self.turns[turn].running = false;
                }
            }
            AgentEvent::PlanUpdated {
                steps, explanation, ..
            } => {
                self.plan_steps = steps.clone();
                self.plan_explanation = explanation.clone();
            }
            AgentEvent::ProposedPlanDelta { item_id, text } => {
                let turn = self.ensure_turn(ts);
                match &mut self.proposed_plan {
                    Some(plan) if plan.item_id == *item_id => plan.markdown.push_str(text),
                    _ => {
                        self.proposed_plan = Some(ProposedPlan {
                            item_id: item_id.clone(),
                            markdown: text.clone(),
                            turn,
                        });
                    }
                }
            }
            AgentEvent::ProposedPlan { item_id, markdown } => {
                let turn = self.ensure_turn(ts);
                self.proposed_plan = Some(ProposedPlan {
                    item_id: item_id.clone(),
                    markdown: markdown.clone(),
                    turn,
                });
            }
            AgentEvent::ContextCompacted => {
                let turn = self.ensure_turn(ts);
                let id = self.synthetic_id("compacted");
                self.entries.push(Arc::new(TimelineEntry {
                    id,
                    content: EntryContent::ContextCompacted,
                    ts,
                    turn,
                }));
            }
            // Session metadata (composer menus) — not folded into the timeline.
            // Session metadata (composer menus / traits picker) — held on the
            // active session, not folded into the timeline.
            AgentEvent::ProviderCommands { .. } | AgentEvent::ProviderOptions { .. } => {}
        }
    }

    /// Push a new (open) turn and make it current. `start_ts` seeds the turn's
    /// start time (refined later by a TurnStarted event if one arrives).
    fn push_turn(&mut self, start_ts: Option<u64>) -> usize {
        self.turns.push(TurnMeta {
            start_ts,
            end_ts: None,
            status: None,
            running: false,
        });
        let idx = self.turns.len() - 1;
        self.current_turn = Some(idx);
        idx
    }

    /// The current open turn, creating one if none exists.
    fn ensure_turn(&mut self, ts: Option<u64>) -> usize {
        match self.current_turn {
            Some(turn) => turn,
            None => self.push_turn(ts),
        }
    }

    /// Turn a user message belongs to: a fresh turn when the previous one has
    /// already completed (a new exchange), otherwise the current open turn.
    fn begin_user_turn(&mut self, ts: Option<u64>) -> usize {
        let need_new = match self.current_turn {
            None => true,
            Some(turn) => self.turns[turn].end_ts.is_some() || self.turns[turn].status.is_some(),
        };
        if need_new {
            self.push_turn(ts)
        } else {
            self.current_turn.unwrap()
        }
    }

    fn synthetic_id(&mut self, prefix: &str) -> String {
        self.next_synthetic_id += 1;
        format!("{prefix}-{}", self.next_synthetic_id)
    }

    fn upsert_item(&mut self, ts: Option<u64>, item: &ThreadItem) {
        let mut incoming = Self::content_from_item(&item.content);
        if let Some(parent_id) = &item.parent_item_id {
            let turn = self.ensure_turn(ts);
            let children = self.children.entry(parent_id.clone()).or_default();
            if let Some(entry) = children.iter_mut().find(|entry| entry.id == item.id) {
                let entry = Arc::make_mut(entry);
                entry.content = merge_content(
                    std::mem::replace(&mut entry.content, incoming.clone()),
                    incoming,
                );
            } else {
                children.push(Arc::new(TimelineEntry {
                    id: item.id.clone(),
                    content: incoming,
                    ts,
                    turn,
                }));
                if children.len() > 200 {
                    children.remove(0);
                    self.truncated_children.insert(parent_id.clone());
                }
            }
            return;
        }
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == item.id) {
            let entry = Arc::make_mut(entry);
            entry.content = merge_content(
                std::mem::replace(&mut entry.content, incoming.clone()),
                incoming,
            );
        } else {
            let turn = if matches!(incoming, EntryContent::User { .. }) {
                let turn = self.begin_user_turn(ts);
                if let EntryContent::User { steering, .. } = &mut incoming
                    && self.entries.iter().any(|entry| {
                        entry.turn == turn && matches!(entry.content, EntryContent::User { .. })
                    })
                {
                    // Legacy logs represented a steer as a second UserMessage
                    // item. Preserve their historical accepted rendering.
                    *steering = Some(SteeringStatus::Accepted);
                }
                turn
            } else {
                self.ensure_turn(ts)
            };
            self.entries.push(Arc::new(TimelineEntry {
                id: item.id.clone(),
                content: incoming,
                ts,
                turn,
            }));
        }
    }

    fn request_steer(&mut self, ts: Option<u64>, request_id: &str, text: &str) {
        if self.entries.iter().any(|entry| entry.id == request_id) {
            return;
        }
        let turn = self.ensure_turn(ts);
        self.entries.push(Arc::new(TimelineEntry {
            id: request_id.to_owned(),
            content: EntryContent::User {
                text: text.to_owned(),
                steering: Some(SteeringStatus::Pending),
                context_len: None,
            },
            ts,
            turn,
        }));
    }

    fn accept_steer(&mut self, request_id: &str) {
        let Some(position) = self.entries.iter().position(|entry| entry.id == request_id) else {
            return;
        };
        if !matches!(
            self.entries[position].content,
            EntryContent::User {
                steering: Some(SteeringStatus::Pending),
                ..
            }
        ) {
            return;
        }

        let mut entry = self.entries.remove(position);
        let current_turn = self.turns.iter().rposition(|turn| turn.running);
        if let EntryContent::User {
            steering: steering @ Some(SteeringStatus::Pending),
            ..
        } = &mut Arc::make_mut(&mut entry).content
        {
            *steering = Some(SteeringStatus::Accepted);
        }
        if let Some(turn) = current_turn {
            Arc::make_mut(&mut entry).turn = turn;
        }
        self.entries.push(entry);
    }

    fn content_from_item(content: &ItemContent) -> EntryContent {
        match content {
            ItemContent::UserMessage { text, context_len } => EntryContent::User {
                text: text.clone(),
                steering: None,
                context_len: *context_len,
            },
            ItemContent::AssistantMessage { text } => {
                EntryContent::Assistant { text: text.clone() }
            }
            ItemContent::Reasoning { text } => EntryContent::Reasoning { text: text.clone() },
            ItemContent::CommandExecution {
                command,
                output,
                exit_code,
                status,
            } => EntryContent::Command {
                command: command.clone(),
                output: output.clone(),
                exit_code: *exit_code,
                status: *status,
            },
            ItemContent::FileChange { changes, .. } => EntryContent::FileChange {
                changes: changes.clone(),
            },
            ItemContent::ToolCall {
                name,
                input,
                output,
                status,
            } => EntryContent::Tool {
                name: name.clone(),
                input: input.clone(),
                output: output.clone(),
                status: *status,
            },
            ItemContent::Subagent {
                agent_type,
                description,
                status,
                summary,
            } => EntryContent::Subagent {
                agent_type: agent_type.clone(),
                description: description.clone(),
                status: *status,
                summary: summary.clone(),
            },
            ItemContent::WebSearch { query } => EntryContent::Tool {
                name: "web_search".into(),
                input: serde_json::json!({ "query": query }),
                output: None,
                status: ItemStatus::Completed,
            },
            ItemContent::Other {
                provider_kind,
                summary,
            } => EntryContent::Tool {
                name: provider_kind.clone(),
                input: serde_json::json!({ "summary": summary }),
                output: None,
                status: ItemStatus::Completed,
            },
        }
    }

    fn apply_delta(&mut self, ts: Option<u64>, item_id: &str, kind: DeltaKind, text: &str) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == item_id) {
            let entry = Arc::make_mut(entry);
            match (&mut entry.content, kind) {
                (EntryContent::Assistant { text: existing }, DeltaKind::AssistantText)
                | (EntryContent::Reasoning { text: existing }, DeltaKind::ReasoningText) => {
                    existing.push_str(text);
                }
                (EntryContent::Command { output, .. }, DeltaKind::CommandOutput) => {
                    output.push_str(text);
                }
                _ => log::warn!("delta kind {kind:?} does not match item {item_id}"),
            }
            return;
        }
        // Providers may stream deltas before announcing the item: create lazily.
        let content = match kind {
            DeltaKind::AssistantText => EntryContent::Assistant { text: text.into() },
            DeltaKind::ReasoningText => EntryContent::Reasoning { text: text.into() },
            DeltaKind::CommandOutput => EntryContent::Command {
                command: String::new(),
                output: text.into(),
                exit_code: None,
                status: ItemStatus::InProgress,
            },
        };
        let turn = self.ensure_turn(ts);
        self.entries.push(Arc::new(TimelineEntry {
            id: item_id.to_string(),
            content,
            ts,
            turn,
        }));
    }
}

/// The index in a session's stored event log of the event that introduces the
/// **first user message of `turn`** — i.e. the JSONL truncation boundary for
/// rewinding the thread to just before that message (revert / edit & resend).
///
/// A checkpoint records this offset when it is captured (`Checkpoint::event_offset`);
/// this recomputes it by replaying the log, which is what makes rewinding work in
/// a cwd that has no git checkpoints at all. The two agree — see the tests.
pub fn turn_user_event_offset(events: &[StoredEvent], turn: usize) -> Option<usize> {
    let mut timeline = Timeline::default();
    for (index, stored) in events.iter().enumerate() {
        let before = timeline.entries.len();
        timeline.apply_at(stored.ts, &stored.event);
        let opened_turn = timeline.entries[before..]
            .iter()
            .any(|entry| matches!(entry.content, EntryContent::User { .. }) && entry.turn == turn);
        if opened_turn {
            return Some(index);
        }
    }
    None
}

/// Extract a plan's title from its markdown: the text of the first ATX heading
/// (`#`…`######`), else `None` (callers fall back to a localized "Proposed
/// plan"). Leading `#`s and surrounding whitespace are stripped.
pub fn plan_title(markdown: &str) -> Option<String> {
    for line in markdown.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix('#') {
            let heading = rest.trim_start_matches('#').trim();
            if !heading.is_empty() {
                return Some(heading.to_string());
            }
        }
    }
    None
}

/// The exact implementation prompt sent when a proposed plan is accepted
/// (`Implement` / `Implement in a new thread`): the T3 verbatim prefix plus the
/// trimmed plan markdown.
pub fn implement_prompt(markdown: &str) -> String {
    format!("PLEASE IMPLEMENT THIS PLAN:\n{}", markdown.trim())
}

/// The prefix every orchestrate child-thread callback user message opens with.
/// Callbacks are injected verbatim (see the runtime's `assemble_callback_text`);
/// the UI reparses that shape to render a disclosure row instead of a bubble.
pub const ORCHESTRATE_CALLBACK_PREFIX: &str = "[orchestrate] thread ";

/// The parts of an orchestrate child-thread callback user message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchestrateCallback {
    /// The child session id the callback reports on.
    pub child_id: String,
    /// The child thread's title (may itself contain quotes).
    pub title: String,
    /// The reported state word (`completed` / `failed` / …).
    pub state: String,
    /// Everything after the first line — the digest body (empty when absent).
    pub body: String,
}

/// Parse a user-message text that a child-thread callback injected, mirroring
/// the runtime's `[orchestrate] thread {id} ("{title}") {state}.{tokens}\n{body}`
/// wire format. Returns `None` for any text that is not a callback, so callers
/// fall back to the plain bubble. Works on historical logs too: it reads the
/// stored text and never depends on any stored annotation.
pub fn parse_orchestrate_callback(text: &str) -> Option<OrchestrateCallback> {
    let (first_line, body) = match text.split_once('\n') {
        Some((line, body)) => (line, body),
        None => (text, ""),
    };
    let rest = first_line.strip_prefix(ORCHESTRATE_CALLBACK_PREFIX)?;
    // `{child_id} ("{title}") {state}.…` — the id has no spaces, so the first
    // ` ("` opens the title and the last `") ` closes it (titles may contain
    // quotes, but the trailing state word never does).
    let open = rest.find(" (\"")?;
    let child_id = rest[..open].to_string();
    let after = &rest[open + 3..];
    let close = after.rfind("\") ")?;
    let title = after[..close].to_string();
    let tail = &after[close + 3..];
    let state = tail.split('.').next().unwrap_or("").trim().to_string();
    if child_id.is_empty() || state.is_empty() {
        return None;
    }
    Some(OrchestrateCallback {
        child_id,
        title,
        state,
        body: body.to_string(),
    })
}

/// Merge an authoritative item snapshot over an existing entry, keeping
/// delta-accumulated text when the snapshot's text field is empty.
fn merge_content(existing: EntryContent, incoming: EntryContent) -> EntryContent {
    match (existing, incoming) {
        (
            EntryContent::User { steering, .. },
            EntryContent::User {
                text, context_len, ..
            },
        ) => EntryContent::User {
            text,
            steering,
            context_len,
        },
        (EntryContent::Assistant { text: old }, EntryContent::Assistant { text: new }) => {
            EntryContent::Assistant {
                text: merge_text(old, new),
            }
        }
        (EntryContent::Reasoning { text: old }, EntryContent::Reasoning { text: new }) => {
            EntryContent::Reasoning {
                text: merge_text(old, new),
            }
        }
        (
            EntryContent::Command {
                output: old_output, ..
            },
            EntryContent::Command {
                command,
                output,
                exit_code,
                status,
            },
        ) => EntryContent::Command {
            command,
            output: merge_text(old_output, output),
            exit_code,
            status,
        },
        (
            EntryContent::Subagent {
                summary: old_summary,
                ..
            },
            EntryContent::Subagent {
                agent_type,
                description,
                status,
                summary,
            },
        ) => EntryContent::Subagent {
            agent_type,
            description,
            status,
            summary: summary.or(old_summary),
        },
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
mod tests {
    use super::*;
    use agent::{ApprovalDecision, ApprovalKind, FileChangeKind};
    use serde_json::json;

    fn user_msg(id: &str, text: &str) -> AgentEvent {
        AgentEvent::ItemCompleted(ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content: ItemContent::UserMessage {
                text: text.into(),
                context_len: None,
            },
        })
    }

    #[test]
    fn cloned_timeline_entries_are_shared_until_updated() {
        let mut timeline = Timeline::fold_events([user_msg("user-1", "before")]);
        let snapshot = timeline.clone();

        assert!(Arc::ptr_eq(&timeline.entries[0], &snapshot.entries[0]));

        timeline.apply_at(None, &user_msg("user-1", "after"));

        assert!(!Arc::ptr_eq(&timeline.entries[0], &snapshot.entries[0]));
        assert!(matches!(
            &snapshot.entries[0].content,
            EntryContent::User { text, .. } if text == "before"
        ));
        assert!(matches!(
            &timeline.entries[0].content,
            EntryContent::User { text, .. } if text == "after"
        ));
    }

    /// Modeled on crates/agent/tests/fixtures/claude/simple_trace.jsonl:
    /// session init → streamed text deltas → full assistant message → result.
    #[test]
    fn fold_simple_claude_style_trace() {
        let events = vec![
            AgentEvent::SessionStarted {
                provider_session_id: "78b7774c".into(),
                resume: ResumeCursor(json!({ "session_id": "78b7774c" })),
                model: Some("claude-opus-4-8".into()),
            },
            user_msg("user-1", "hi"),
            AgentEvent::TurnStarted {
                turn_id: "t1".into(),
            },
            AgentEvent::Delta {
                item_id: "msg_011".into(),
                kind: DeltaKind::AssistantText,
                text: "Hi! ".into(),
            },
            AgentEvent::Delta {
                item_id: "msg_011".into(),
                kind: DeltaKind::AssistantText,
                text: "How can I help you today?".into(),
            },
            AgentEvent::ItemCompleted(ThreadItem {
                id: "msg_011".into(),
                parent_item_id: None,
                content: ItemContent::AssistantMessage {
                    text: "Hi! How can I help you today?".into(),
                },
            }),
            AgentEvent::TurnCompleted {
                turn_id: "t1".into(),
                status: TurnStatus::Completed,
                usage: Some(TokenUsage {
                    input_tokens: Some(3355),
                    output_tokens: Some(17),
                    ..Default::default()
                }),
            },
        ];
        let timeline = Timeline::fold_events(events);

        assert_eq!(timeline.entries.len(), 2);
        assert!(matches!(
            &timeline.entries[0].content,
            EntryContent::User { text, .. } if text == "hi"
        ));
        assert!(matches!(
            &timeline.entries[1].content,
            EntryContent::Assistant { text } if text == "Hi! How can I help you today?"
        ));
        assert!(!timeline.turn_running);
        assert_eq!(timeline.last_turn_status, Some(TurnStatus::Completed));
        assert_eq!(timeline.usage.unwrap().output_tokens, Some(17));
        assert_eq!(timeline.model.as_deref(), Some("claude-opus-4-8"));
        assert!(timeline.resume.is_some());
        assert_eq!(timeline.first_user_message(), Some("hi"));
    }

    #[test]
    fn fold_marks_only_mid_turn_user_messages_as_steered() {
        let events = vec![
            user_msg("user-a", "A"),
            AgentEvent::TurnStarted {
                turn_id: "t1".into(),
            },
            AgentEvent::ItemCompleted(ThreadItem {
                id: "assistant-a".into(),
                parent_item_id: None,
                content: ItemContent::AssistantMessage {
                    text: "working".into(),
                },
            }),
            user_msg("user-b", "B"),
            AgentEvent::TurnCompleted {
                turn_id: "t1".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
            user_msg("user-c", "C"),
            AgentEvent::TurnStarted {
                turn_id: "t2".into(),
            },
        ];
        let timeline = Timeline::fold_events(events);
        let users: Vec<(&str, Option<SteeringStatus>)> = timeline
            .entries
            .iter()
            .filter_map(|entry| match &entry.content {
                EntryContent::User { text, steering, .. } => Some((text.as_str(), *steering)),
                _ => None,
            })
            .collect();

        assert_eq!(
            users,
            vec![
                ("A", None),
                ("B", Some(SteeringStatus::Accepted)),
                ("C", None),
            ]
        );
    }

    #[test]
    fn correlated_steering_replays_pending_then_only_matching_acceptance() {
        let request = AgentEvent::SteerRequested {
            request_id: "steer-a".into(),
            text: "change direction".into(),
        };
        let encoded = serde_json::to_string(&request).unwrap();
        let decoded: AgentEvent = serde_json::from_str(&encoded).unwrap();
        let mut timeline = Timeline::fold_events([
            user_msg("user-a", "start"),
            AgentEvent::TurnStarted {
                turn_id: "turn-a".into(),
            },
            decoded,
        ]);

        assert!(matches!(
            &timeline.entries[1].content,
            EntryContent::User {
                text,
                steering: Some(SteeringStatus::Pending),
                ..
            } if text == "change direction"
        ));

        timeline.apply_at(
            None,
            &AgentEvent::SteerAccepted {
                request_id: "steer-b".into(),
            },
        );
        assert!(matches!(
            timeline.entries[1].content,
            EntryContent::User {
                steering: Some(SteeringStatus::Pending),
                ..
            }
        ));

        let accepted = AgentEvent::SteerAccepted {
            request_id: "steer-a".into(),
        };
        let accepted: AgentEvent =
            serde_json::from_str(&serde_json::to_string(&accepted).unwrap()).unwrap();
        timeline.apply_at(None, &accepted);
        assert!(matches!(
            timeline.entries[1].content,
            EntryContent::User {
                steering: Some(SteeringStatus::Accepted),
                ..
            }
        ));

        // A restart folds the persisted request and acceptance to the same
        // accepted state; confirmation cannot regress to pending on replay.
        let replayed = Timeline::fold_events([
            user_msg("user-a", "start"),
            AgentEvent::TurnStarted {
                turn_id: "turn-a".into(),
            },
            request,
            accepted,
        ]);
        assert!(matches!(
            replayed.entries[1].content,
            EntryContent::User {
                steering: Some(SteeringStatus::Accepted),
                ..
            }
        ));
    }

    #[test]
    fn accepted_steer_moves_to_its_consumption_position_live_and_on_replay() {
        let assistant_item = |id: &str| {
            AgentEvent::ItemCompleted(ThreadItem {
                id: id.into(),
                parent_item_id: None,
                content: ItemContent::AssistantMessage { text: id.into() },
            })
        };
        let events = vec![
            user_msg("user-a", "start"),
            AgentEvent::TurnStarted {
                turn_id: "turn-a".into(),
            },
            AgentEvent::SteerRequested {
                request_id: "S".into(),
                text: "change direction".into(),
            },
            assistant_item("A"),
            assistant_item("B"),
            AgentEvent::SteerAccepted {
                request_id: "S".into(),
            },
            assistant_item("C"),
            assistant_item("D"),
        ];

        let mut live = Timeline::default();
        for event in &events {
            live.apply_at(None, event);
        }
        let replayed = Timeline::fold_events(events);

        let live_ids: Vec<&str> = live.entries.iter().map(|entry| entry.id.as_str()).collect();
        let replayed_ids: Vec<&str> = replayed
            .entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect();
        assert_eq!(live_ids, ["user-a", "A", "B", "S", "C", "D"]);
        assert_eq!(replayed_ids, live_ids);
        assert!(matches!(
            live.entries[3].content,
            EntryContent::User {
                steering: Some(SteeringStatus::Accepted),
                ..
            }
        ));
        assert!(matches!(
            replayed.entries[3].content,
            EntryContent::User {
                steering: Some(SteeringStatus::Accepted),
                ..
            }
        ));
        assert_eq!(replayed.entries[3].turn, live.entries[3].turn);
    }

    fn assistant_delta(id: &str, text: &str) -> AgentEvent {
        AgentEvent::Delta {
            item_id: id.into(),
            kind: DeltaKind::AssistantText,
            text: text.into(),
        }
    }

    fn assistant_snapshot(id: &str, text: &str) -> AgentEvent {
        AgentEvent::ItemUpdated(ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content: ItemContent::AssistantMessage { text: text.into() },
        })
    }

    fn assistant_text(timeline: &Timeline, id: &str) -> String {
        timeline
            .entries
            .iter()
            .find(|e| e.id == id)
            .map(|e| match &e.content {
                EntryContent::Assistant { text } => text.clone(),
                other => panic!("entry {id} is not assistant text: {other:?}"),
            })
            .unwrap_or_else(|| panic!("no entry {id}"))
    }

    /// An item snapshot must never be concatenated onto delta-accumulated text,
    /// and a snapshot that *lags* the deltas (it carries only the text so far,
    /// or nothing at all) must not shorten what is already there: shortening
    /// makes the next delta look like a fresh append and duplicates the
    /// overlapping paragraph.
    #[test]
    fn fold_snapshot_never_duplicates_or_shortens_delta_text() {
        let timeline = Timeline::fold_events(vec![
            assistant_delta("msg", "Para one.\n\n"),
            assistant_delta("msg", "Para two."),
            // Snapshot lagging one delta behind: must not shorten.
            assistant_snapshot("msg", "Para one.\n\n"),
            assistant_delta("msg", " Tail."),
            // Empty snapshot: must not clobber.
            assistant_snapshot("msg", ""),
            // Authoritative final snapshot: replaces, never concatenates.
            AgentEvent::ItemCompleted(ThreadItem {
                id: "msg".into(),
                parent_item_id: None,
                content: ItemContent::AssistantMessage {
                    text: "Para one.\n\nPara two. Tail.".into(),
                },
            }),
        ]);

        assert_eq!(
            assistant_text(&timeline, "msg"),
            "Para one.\n\nPara two. Tail."
        );
    }

    /// A snapshot whose text genuinely differs (the provider rewrote the
    /// message) still wins outright.
    #[test]
    fn fold_snapshot_with_different_text_replaces_deltas() {
        let timeline = Timeline::fold_events(vec![
            assistant_delta("msg", "draft"),
            assistant_snapshot("msg", "final answer"),
        ]);

        assert_eq!(assistant_text(&timeline, "msg"), "final answer");
    }

    /// Modeled on crates/agent/tests/fixtures/codex/v2_messages.jsonl:
    /// file-change item + approval + deltas for message/reasoning/command output.
    #[test]
    fn fold_codex_style_trace_with_approval() {
        let changes = vec![FileChange {
            path: "/tmp/probe-codex/hello.txt".into(),
            kind: FileChangeKind::Create,
            diff: Some("hi\n".into()),
        }];
        let mut timeline = Timeline::default();
        timeline.apply_at(
            None,
            &AgentEvent::TurnStarted {
                turn_id: "turn-1".into(),
            },
        );
        timeline.apply_at(
            None,
            &AgentEvent::ItemStarted(ThreadItem {
                id: "patch-1".into(),
                parent_item_id: None,
                content: ItemContent::FileChange {
                    changes: changes.clone(),
                    status: ItemStatus::InProgress,
                },
            }),
        );
        timeline.apply_at(
            None,
            &AgentEvent::ApprovalRequested(ApprovalRequest {
                id: "41".into(),
                turn_id: Some("turn-1".into()),
                kind: ApprovalKind::FileChange {
                    changes: changes.clone(),
                    reason: None,
                },
                options: Vec::new(),
            }),
        );

        assert!(timeline.turn_running);
        assert_eq!(timeline.pending_approvals.len(), 1);

        timeline.apply_at(
            None,
            &AgentEvent::ApprovalResolved {
                request_id: "41".into(),
                decision: ApprovalDecision::Approve,
            },
        );
        assert!(timeline.pending_approvals.is_empty());

        // Deltas create items lazily.
        timeline.apply_at(
            None,
            &AgentEvent::Delta {
                item_id: "message-1".into(),
                kind: DeltaKind::AssistantText,
                text: "PONG".into(),
            },
        );
        timeline.apply_at(
            None,
            &AgentEvent::Delta {
                item_id: "reasoning-1".into(),
                kind: DeltaKind::ReasoningText,
                text: "Checking".into(),
            },
        );
        timeline.apply_at(
            None,
            &AgentEvent::Delta {
                item_id: "command-1".into(),
                kind: DeltaKind::CommandOutput,
                text: "ok\n".into(),
            },
        );
        timeline.apply_at(
            None,
            &AgentEvent::TokenUsage(TokenUsage {
                used_tokens: Some(123),
                context_window: Some(200000),
                ..Default::default()
            }),
        );
        timeline.apply_at(
            None,
            &AgentEvent::ItemCompleted(ThreadItem {
                id: "patch-1".into(),
                parent_item_id: None,
                content: ItemContent::FileChange {
                    changes: changes.clone(),
                    status: ItemStatus::Completed,
                },
            }),
        );
        timeline.apply_at(
            None,
            &AgentEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
        );

        assert!(!timeline.turn_running);
        assert_eq!(timeline.entries.len(), 4);
        assert!(matches!(
            &timeline.entries[0].content,
            EntryContent::FileChange { changes }
                if changes.len() == 1 && changes[0].path.ends_with("hello.txt")
        ));
        assert!(matches!(
            &timeline.entries[1].content,
            EntryContent::Assistant { text } if text == "PONG"
        ));
        assert!(matches!(
            &timeline.entries[2].content,
            EntryContent::Reasoning { text } if text == "Checking"
        ));
        assert!(matches!(
            &timeline.entries[3].content,
            EntryContent::Command { output, .. } if output == "ok\n"
        ));
        assert_eq!(timeline.usage.unwrap().used_tokens, Some(123));
    }

    #[test]
    fn command_snapshot_keeps_streamed_output_when_snapshot_output_empty() {
        let mut timeline = Timeline::default();
        timeline.apply_at(
            None,
            &AgentEvent::ItemStarted(ThreadItem {
                id: "cmd-1".into(),
                parent_item_id: None,
                content: ItemContent::CommandExecution {
                    command: "echo hi".into(),
                    output: String::new(),
                    exit_code: None,
                    status: ItemStatus::InProgress,
                },
            }),
        );
        timeline.apply_at(
            None,
            &AgentEvent::Delta {
                item_id: "cmd-1".into(),
                kind: DeltaKind::CommandOutput,
                text: "hi\n".into(),
            },
        );
        timeline.apply_at(
            None,
            &AgentEvent::ItemCompleted(ThreadItem {
                id: "cmd-1".into(),
                parent_item_id: None,
                content: ItemContent::CommandExecution {
                    command: "echo hi".into(),
                    output: String::new(),
                    exit_code: Some(0),
                    status: ItemStatus::Completed,
                },
            }),
        );

        assert!(matches!(
            &timeline.entries[0].content,
            EntryContent::Command { command, output, exit_code: Some(0), status: ItemStatus::Completed }
                if command == "echo hi" && output == "hi\n"
        ));
    }

    #[test]
    fn timestamps_and_turn_grouping_fold_across_two_exchanges() {
        // Two user→assistant exchanges; timestamps thread through as envelopes.
        let stored = vec![
            StoredEvent {
                ts: Some(1_000_000),
                event: user_msg("u1", "first"),
            },
            StoredEvent {
                ts: Some(1_000_500),
                event: AgentEvent::TurnStarted {
                    turn_id: "t1".into(),
                },
            },
            StoredEvent {
                ts: Some(1_002_000),
                event: AgentEvent::ItemCompleted(ThreadItem {
                    id: "a1".into(),
                    parent_item_id: None,
                    content: ItemContent::AssistantMessage { text: "hi".into() },
                }),
            },
            StoredEvent {
                ts: Some(1_005_500),
                event: AgentEvent::TurnCompleted {
                    turn_id: "t1".into(),
                    status: TurnStatus::Completed,
                    usage: None,
                },
            },
            StoredEvent {
                ts: Some(2_000_000),
                event: user_msg("u2", "second"),
            },
            StoredEvent {
                ts: Some(2_000_400),
                event: AgentEvent::TurnStarted {
                    turn_id: "t2".into(),
                },
            },
        ];
        let timeline = Timeline::fold_events(stored);

        // Two turns; the first is finished with a 5s wall-clock duration.
        assert_eq!(timeline.turns.len(), 2);
        assert_eq!(timeline.turns[0].start_ts, Some(1_000_500));
        assert_eq!(timeline.turns[0].end_ts, Some(1_005_500));
        assert_eq!(timeline.turns[0].duration_secs(), Some(5));
        assert_eq!(timeline.turns[0].status, Some(TurnStatus::Completed));
        assert!(!timeline.turns[0].running);
        // Second turn is still running (no TurnCompleted yet).
        assert!(timeline.turns[1].running);
        assert!(timeline.turn_running);

        // Entries carry their timestamp and map to the right turn.
        let u1 = &timeline.entries[0];
        assert_eq!(u1.ts, Some(1_000_000));
        assert_eq!(u1.turn, 0);
        let a1 = &timeline.entries[1];
        assert_eq!(a1.turn, 0);
        let u2 = timeline
            .entries
            .iter()
            .find(|e| matches!(&e.content, EntryContent::User { text, .. } if text == "second"))
            .unwrap();
        assert_eq!(u2.turn, 1);

        // mark_idle clears running state after a replayed session goes cold.
        let mut cold = timeline.clone();
        cold.mark_idle();
        assert!(!cold.turn_running);
        assert!(cold.turns.iter().all(|t| !t.running));
    }

    #[test]
    fn plan_title_extracts_first_heading() {
        assert_eq!(
            plan_title("# Refactor the parser\n\nBody text"),
            Some("Refactor the parser".to_string())
        );
        assert_eq!(
            plan_title("intro line\n### Step one\nmore"),
            Some("Step one".to_string())
        );
        // No heading -> None (caller supplies the localized fallback).
        assert_eq!(plan_title("just a paragraph\nsecond line"), None);
        // Empty heading is skipped.
        assert_eq!(
            plan_title("#\n# Real title"),
            Some("Real title".to_string())
        );
    }

    #[test]
    fn implement_prompt_uses_verbatim_prefix() {
        assert_eq!(
            implement_prompt("  # Plan\nDo the thing\n  "),
            "PLEASE IMPLEMENT THIS PLAN:\n# Plan\nDo the thing"
        );
    }

    #[test]
    fn proposed_plan_deltas_accumulate_then_finalize() {
        let mut timeline = Timeline::default();
        timeline.apply_at(
            None,
            &AgentEvent::TurnStarted {
                turn_id: "t1".into(),
            },
        );
        timeline.apply_at(
            None,
            &AgentEvent::ProposedPlanDelta {
                item_id: "plan-1".into(),
                text: "# Plan\n".into(),
            },
        );
        timeline.apply_at(
            None,
            &AgentEvent::ProposedPlanDelta {
                item_id: "plan-1".into(),
                text: "step one".into(),
            },
        );
        let plan = timeline.proposed_plan.as_ref().unwrap();
        assert_eq!(plan.markdown, "# Plan\nstep one");
        assert_eq!(plan.turn, 0);

        // The final ProposedPlan replaces the accumulated text.
        timeline.apply_at(
            None,
            &AgentEvent::ProposedPlan {
                item_id: "plan-1".into(),
                markdown: "# Plan\nstep one\nstep two".into(),
            },
        );
        assert_eq!(
            timeline.proposed_plan.as_ref().unwrap().markdown,
            "# Plan\nstep one\nstep two"
        );
        // The proposed plan survives replay's mark_idle (it is the accept anchor).
        timeline.mark_idle();
        assert!(timeline.proposed_plan.is_some());
    }

    #[test]
    fn plan_updated_tracks_latest_steps() {
        use agent::PlanStepStatus;
        let mut timeline = Timeline::default();
        timeline.apply_at(
            None,
            &AgentEvent::PlanUpdated {
                turn_id: Some("t".into()),
                explanation: Some("Working".into()),
                steps: vec![
                    PlanStep {
                        step: "a".into(),
                        status: PlanStepStatus::Completed,
                    },
                    PlanStep {
                        step: "b".into(),
                        status: PlanStepStatus::InProgress,
                    },
                ],
            },
        );
        assert_eq!(timeline.plan_steps.len(), 2);
        assert_eq!(timeline.plan_explanation.as_deref(), Some("Working"));
        assert_eq!(timeline.plan_steps[1].status, PlanStepStatus::InProgress);
    }

    #[test]
    fn provider_start_failure_folds_semantically() {
        let mut timeline = Timeline::default();
        timeline.apply_at(
            Some(1_234),
            &AgentEvent::ProviderStartFailed {
                error: "spawn failed".into(),
            },
        );

        let provider_error = &timeline.entries[0];
        assert_eq!(provider_error.ts, Some(1_234));
        assert!(timeline.turns.get(provider_error.turn).is_some());
        assert!(matches!(
            &provider_error.content,
            EntryContent::ProviderStartError { error } if error == "spawn failed"
        ));

        timeline.apply_at(
            Some(1_235),
            &AgentEvent::Error {
                message: "boom".into(),
                fatal: true,
            },
        );
        assert!(matches!(
            &timeline.entries[1].content,
            EntryContent::Error { message } if message == "boom"
        ));
    }

    #[test]
    fn errors_and_session_close_fold_into_timeline() {
        let mut timeline = Timeline::default();
        timeline.apply_at(
            None,
            &AgentEvent::TurnStarted {
                turn_id: "t".into(),
            },
        );
        timeline.apply_at(
            None,
            &AgentEvent::ApprovalRequested(ApprovalRequest {
                id: "req".into(),
                turn_id: None,
                kind: ApprovalKind::ExecCommand {
                    command: "rm -rf /".into(),
                    cwd: None,
                    reason: None,
                },
                options: Vec::new(),
            }),
        );
        timeline.apply_at(
            None,
            &AgentEvent::Error {
                message: "boom".into(),
                fatal: true,
            },
        );
        timeline.apply_at(None, &AgentEvent::SessionClosed { reason: None });

        assert!(!timeline.turn_running);
        assert!(timeline.pending_approvals.is_empty());
        assert!(matches!(
            &timeline.entries[0].content,
            EntryContent::Error { message } if message == "boom"
        ));
        // A silent close (reason: None) leaves no entry…
        let entries_after_silent_close = timeline.entries.len();
        // …but an abnormal close records the provider's dying words.
        timeline.apply_at(
            None,
            &AgentEvent::SessionClosed {
                reason: Some("codex app-server exited with exit status: 1\nstderr:\nboom".into()),
            },
        );
        assert_eq!(timeline.entries.len(), entries_after_silent_close + 1);
        assert!(matches!(
            &timeline.entries.last().unwrap().content,
            EntryContent::Error { message } if message.contains("stderr:\nboom")
        ));
    }

    #[test]
    fn subagent_children_fold_below_spawn_only() {
        let spawn = ThreadItem {
            id: "spawn".into(),
            parent_item_id: None,
            content: ItemContent::Subagent {
                agent_type: "general-purpose".into(),
                description: "Ping test".into(),
                status: ItemStatus::InProgress,
                summary: None,
            },
        };
        let child = ThreadItem {
            id: "spawn:user-1".into(),
            parent_item_id: Some("spawn".into()),
            content: ItemContent::UserMessage {
                text: "ping".into(),
                context_len: None,
            },
        };
        let completed = ThreadItem {
            content: ItemContent::Subagent {
                agent_type: "general-purpose".into(),
                description: "Ping test".into(),
                status: ItemStatus::Completed,
                summary: Some("pong".into()),
            },
            ..spawn.clone()
        };
        let timeline = Timeline::fold_events([
            AgentEvent::ItemStarted(spawn),
            AgentEvent::ItemCompleted(child),
            AgentEvent::ItemCompleted(completed),
        ]);
        assert_eq!(timeline.entries.len(), 1);
        assert_eq!(timeline.entries[0].id, "spawn");
        assert!(matches!(
            &timeline.entries[0].content,
            EntryContent::Subagent { status: ItemStatus::Completed, summary: Some(summary), .. }
                if summary == "pong"
        ));
        assert_eq!(timeline.children("spawn").len(), 1);
        assert!(matches!(
            &timeline.children("spawn")[0].content,
            EntryContent::User { text, .. } if text == "ping"
        ));
    }

    #[test]
    fn subagent_child_cap_records_actual_truncation() {
        let mut timeline = Timeline::default();
        for index in 0..=200 {
            timeline.apply_at(
                None,
                &AgentEvent::ItemCompleted(ThreadItem {
                    id: format!("spawn:child-{index}"),
                    parent_item_id: Some("spawn".into()),
                    content: ItemContent::AssistantMessage {
                        text: index.to_string(),
                    },
                }),
            );
        }
        assert_eq!(timeline.children("spawn").len(), 200);
        assert!(timeline.children_truncated("spawn"));
        assert_eq!(timeline.children("spawn")[0].id, "spawn:child-1");
    }

    #[test]
    fn review_comment_serialization_matches_t3_format() {
        let comment = ReviewComment::new(
            "src/lib.rs".into(),
            7,
            8,
            ReviewSide::New,
            "  Please avoid the unwrap.  ".into(),
            "@@ -7,1 +7,2 @@\n old\n+new".into(),
            "turn:3".into(),
            "Turn 4".into(),
            12,
            13,
        );
        assert_eq!(
            append_review_comments_to_prompt("Fix this", &[comment]),
            "Fix this\n\n<review_comment sectionId=\"turn:3\" sectionTitle=\"Turn 4\" filePath=\"src/lib.rs\" startIndex=\"12\" endIndex=\"13\" rangeLabel=\"+7 to +8\">\nPlease avoid the unwrap.\n```diff\n@@ -7,1 +7,2 @@\n old\n+new\n```\n</review_comment>"
        );
    }

    #[test]
    fn parse_orchestrate_callback_reads_the_wire_format() {
        // Normal callback: id, quoted title, state word, and a multi-line body.
        let normal = parse_orchestrate_callback(
            "[orchestrate] thread child-7 (\"Investigate zed terminal\") completed. tokens: input 5, output 3, total 8.\nHere is the report.\nSecond line.",
        )
        .expect("normal callback parses");
        assert_eq!(normal.child_id, "child-7");
        assert_eq!(normal.title, "Investigate zed terminal");
        assert_eq!(normal.state, "completed");
        assert_eq!(normal.body, "Here is the report.\nSecond line.");

        // A title that itself contains quotes survives (the last `") ` closes it).
        let quoted = parse_orchestrate_callback(
            "[orchestrate] thread abc (\"He said \"hi\" twice\") failed.\nbody",
        )
        .expect("quoted-title callback parses");
        assert_eq!(quoted.title, "He said \"hi\" twice");
        assert_eq!(quoted.state, "failed");
        assert_eq!(quoted.body, "body");

        // Missing body (no newline) → empty body, still parses.
        let no_body = parse_orchestrate_callback("[orchestrate] thread c (\"Title\") completed.")
            .expect("bodyless callback parses");
        assert_eq!(no_body.body, "");
        assert_eq!(no_body.state, "completed");

        // Non-matching text (an ordinary user message) is not a callback.
        assert!(parse_orchestrate_callback("Please run the tests").is_none());
        assert!(parse_orchestrate_callback("[orchestrate] thread only-a-header").is_none());
    }

    #[test]
    fn user_message_context_len_survives_a_serde_roundtrip() {
        let event = user_msg_with_context("u1", "PREFIX\n\nvisible", Some(8));
        let encoded = serde_json::to_string(&event).unwrap();
        // The annotation is present in the wire form when set…
        assert!(encoded.contains("\"context_len\":8"));
        let decoded: AgentEvent = serde_json::from_str(&encoded).unwrap();
        let timeline = Timeline::fold_events([decoded]);
        assert!(matches!(
            &timeline.entries[0].content,
            EntryContent::User { text, context_len: Some(8), .. } if text == "PREFIX\n\nvisible"
        ));

        // …and omitted entirely when absent (skip_serializing_if).
        let plain = user_msg_with_context("u2", "hello", None);
        let plain_encoded = serde_json::to_string(&plain).unwrap();
        assert!(!plain_encoded.contains("context_len"));
    }

    #[test]
    fn old_format_user_message_without_the_field_folds_to_a_plain_bubble() {
        // A JSONL line written before the annotation existed carries no field.
        let legacy = r#"{"type":"item_completed","id":"u1","content":{"kind":"user_message","text":"just words"}}"#;
        let event: AgentEvent = serde_json::from_str(legacy).unwrap();
        let timeline = Timeline::fold_events([event]);
        assert!(matches!(
            &timeline.entries[0].content,
            EntryContent::User { text, context_len: None, .. } if text == "just words"
        ));
    }

    fn user_msg_with_context(id: &str, text: &str, context_len: Option<usize>) -> AgentEvent {
        AgentEvent::ItemCompleted(ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content: ItemContent::UserMessage {
                text: text.into(),
                context_len,
            },
        })
    }
}
