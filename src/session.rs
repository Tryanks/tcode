//! Pure timeline fold: canonical [`AgentEvent`]s in, renderable timeline out.
//!
//! The same fold is used for live event streams and for JSONL replay, so the
//! UI renders identically in both cases.

use agent::{
    AgentEvent, ApprovalRequest, DeltaKind, FileChange, ItemContent, ItemStatus, PlanStep,
    ResumeCursor, ThreadItem, TokenUsage, TurnStatus, UserInputQuestion,
};

pub use crate::store::StoredEvent;

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
    Error {
        message: String,
    },
    /// The provider compacted its context window (a "Context compacted" work-log row).
    ContextCompacted,
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
    pub entries: Vec<TimelineEntry>,
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
            EntryContent::User { text } => Some(text.as_str()),
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
            AgentEvent::Error { message, .. } => {
                let turn = self.ensure_turn(ts);
                let id = self.synthetic_id("error");
                self.entries.push(TimelineEntry {
                    id,
                    content: EntryContent::Error {
                        message: message.clone(),
                    },
                    ts,
                    turn,
                });
            }
            AgentEvent::SessionClosed { .. } => {
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
                self.entries.push(TimelineEntry {
                    id,
                    content: EntryContent::ContextCompacted,
                    ts,
                    turn,
                });
            }
            // Session metadata (composer menus) — not folded into the timeline.
            AgentEvent::ProviderCommands { .. } => {}
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
            Some(turn) => self.turns[turn].end_ts.is_some(),
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
        let incoming = Self::content_from_item(&item.content);
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == item.id) {
            entry.content = merge_content(
                std::mem::replace(&mut entry.content, incoming.clone()),
                incoming,
            );
        } else {
            let turn = if matches!(incoming, EntryContent::User { .. }) {
                self.begin_user_turn(ts)
            } else {
                self.ensure_turn(ts)
            };
            self.entries.push(TimelineEntry {
                id: item.id.clone(),
                content: incoming,
                ts,
                turn,
            });
        }
    }

    fn content_from_item(content: &ItemContent) -> EntryContent {
        match content {
            ItemContent::UserMessage { text } => EntryContent::User { text: text.clone() },
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
        self.entries.push(TimelineEntry {
            id: item_id.to_string(),
            content,
            ts,
            turn,
        });
    }
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

/// Merge an authoritative item snapshot over an existing entry, keeping
/// delta-accumulated text when the snapshot's text field is empty.
fn merge_content(existing: EntryContent, incoming: EntryContent) -> EntryContent {
    match (existing, incoming) {
        (EntryContent::Assistant { text: old }, EntryContent::Assistant { text: new }) => {
            EntryContent::Assistant {
                text: if new.is_empty() { old } else { new },
            }
        }
        (EntryContent::Reasoning { text: old }, EntryContent::Reasoning { text: new }) => {
            EntryContent::Reasoning {
                text: if new.is_empty() { old } else { new },
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
            output: if output.is_empty() {
                old_output
            } else {
                output
            },
            exit_code,
            status,
        },
        (_, incoming) => incoming,
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
            content: ItemContent::UserMessage { text: text.into() },
        })
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
            EntryContent::User { text } if text == "hi"
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
            .find(|e| matches!(&e.content, EntryContent::User { text } if text == "second"))
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
        assert_eq!(plan_title("#\n# Real title"), Some("Real title".to_string()));
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
    }
}
