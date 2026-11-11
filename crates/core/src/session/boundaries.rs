use std::collections::HashMap;

use agent::{AgentEvent, ContextFreshness, ItemContent};
use serde::{Deserialize, Serialize};

/// The part of a timeline needed to assign records to turns. Both the rendered
/// fold and the append transaction advance this state through `apply`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnBoundaries {
    turns: Vec<Boundary>,
    users: HashMap<String, User>,
    compacting: bool,
    /// Steer restore points, by checkpoint id: the steer's request id.
    steer_checkpoints: HashMap<String, String>,
    /// The next message's [`User::seq`].
    next_seq: u64,
    /// What the latest rewind removed while the harness can still undo it.
    undo: Option<Box<UndoState>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct UndoState {
    checkpoint: String,
    turns: Vec<Boundary>,
    users: HashMap<String, User>,
    steer_checkpoints: HashMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Boundary {
    provider: Option<String>,
    checkpoint: Option<String>,
    closed: bool,
    running: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct User {
    turn: usize,
    pending: bool,
    /// When the message joined the fold, which orders a turn's messages:
    /// a cut at a steer removes it and the messages after it.
    seq: u64,
}

/// Decisions the timeline consumes while applying the same event's payload.
#[derive(Debug, Clone, Default)]
pub struct TurnBoundaryChange {
    pub opened: bool,
    pub changed: bool,
    pub target: Option<usize>,
    /// A conversation rewind cut here; the row opened the turn the cut
    /// leaves for what comes next.
    pub rewind: Option<RewindCut>,
    /// The latest rewind was undone: the turns it removed are back.
    pub undone: bool,
}

/// Where a conversation rewind cut: before turn `turn`, or before the steer
/// `steer` inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindCut {
    pub turn: usize,
    pub steer: Option<String>,
}

impl TurnBoundaries {
    pub fn len(&self) -> usize {
        self.turns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }

    pub fn current(&self) -> Option<usize> {
        self.turns.len().checked_sub(1)
    }

    pub fn is_open(&self) -> bool {
        self.turns.last().is_some_and(|turn| !turn.closed)
    }

    /// The turn the provider names `id`: the latest so named, since a
    /// provider process numbers its turns from scratch (Claude Code's and an
    /// ACP agent's `turn-1`, …) and a thread outlives its processes.
    pub fn provider_turn(&self, id: &str) -> Option<usize> {
        self.turns
            .iter()
            .rposition(|turn| turn.provider.as_deref() == Some(id))
            .or(self.current())
    }

    pub(super) fn settle_running(&mut self, running: Option<usize>) {
        for (index, turn) in self.turns.iter_mut().enumerate() {
            turn.running = running == Some(index);
        }
    }

    pub fn apply(&mut self, event: &AgentEvent) -> TurnBoundaryChange {
        let opened = self.opens_turn(event);
        if opened {
            self.turns.push(Boundary::default());
        }
        let mut change = TurnBoundaryChange {
            opened,
            changed: opened,
            target: self.current(),
            rewind: None,
            undone: false,
        };
        change.changed |= matches!(
            event,
            AgentEvent::TurnStarted { .. }
                | AgentEvent::TurnCompleted { .. }
                | AgentEvent::SessionClosed { .. }
                | AgentEvent::TurnChangesUpdated { .. }
                | AgentEvent::TurnCheckpoint { .. }
                | AgentEvent::RewindCompleted { .. }
                | AgentEvent::RewindUndone { .. }
                | AgentEvent::SteerCheckpoint { .. }
                | AgentEvent::CheckpointsCleared
                | AgentEvent::RewindUndoClosed
                | AgentEvent::SteerRequested { .. }
                | AgentEvent::SteerAccepted { .. }
                | AgentEvent::SteerRejected { .. }
                | AgentEvent::TokenUsage(_)
                | AgentEvent::ContextCompacted(_)
        );
        match event {
            AgentEvent::TurnStarted { turn_id } => {
                if let Some(turn) = self.turns.last_mut() {
                    turn.provider = Some(turn_id.clone());
                    turn.running = true;
                }
            }
            AgentEvent::TurnChangesUpdated { turn_id, .. }
            | AgentEvent::TurnCheckpoint { turn_id, .. } => {
                change.target = self.provider_turn(turn_id);
                if let Some(turn) = change.target.map(|index| &mut self.turns[index]) {
                    if !turn_id.is_empty() || matches!(event, AgentEvent::TurnCheckpoint { .. }) {
                        turn.provider = Some(turn_id.clone());
                    }
                    if let AgentEvent::TurnCheckpoint { checkpoint_id, .. } = event {
                        turn.checkpoint = Some(checkpoint_id.clone());
                    }
                }
            }
            AgentEvent::TurnCompleted { usage, .. } => {
                if let Some(usage) = usage {
                    self.compacting = usage.freshness == ContextFreshness::Compacting;
                }
                if let Some(turn) = self.turns.last_mut() {
                    turn.closed = true;
                    turn.running = false;
                }
            }
            AgentEvent::SessionClosed { .. } => {
                if let Some(turn) = self.turns.last_mut() {
                    turn.running = false;
                }
            }
            AgentEvent::SteerCheckpoint {
                request_id,
                checkpoint_id,
            } => {
                change.target = self.users.get(request_id).map(|user| user.turn);
                self.steer_checkpoints
                    .insert(checkpoint_id.clone(), request_id.clone());
            }
            AgentEvent::CheckpointsCleared => {
                for turn in &mut self.turns {
                    turn.checkpoint = None;
                }
                self.steer_checkpoints.clear();
                self.undo = None;
            }
            AgentEvent::RewindUndoClosed => self.undo = None,
            AgentEvent::RewindCompleted {
                checkpoint_id,
                mode,
                undo_token,
                ..
            } => {
                if mode.includes_conversation()
                    && let Some(cut) = self.cut(checkpoint_id)
                {
                    let before = UndoState {
                        checkpoint: checkpoint_id.clone(),
                        turns: self.turns.clone(),
                        users: self.users.clone(),
                        steer_checkpoints: self.steer_checkpoints.clone(),
                    };
                    let kept = match &cut.steer {
                        Some(steer) => {
                            if let Some(cut_seq) = self.users.get(steer).map(|user| user.seq) {
                                self.users
                                    .retain(|_, user| user.turn != cut.turn || user.seq < cut_seq);
                            }
                            if let Some(turn) = self.turns.get_mut(cut.turn) {
                                turn.closed = true;
                                turn.running = false;
                            }
                            cut.turn + 1
                        }
                        None => cut.turn,
                    };
                    self.turns.truncate(kept);
                    self.users.retain(|_, user| user.turn < kept);
                    let users = &self.users;
                    self.steer_checkpoints
                        .retain(|_, steer| users.contains_key(steer));
                    self.compacting = false;
                    self.undo = undo_token.is_some().then(|| Box::new(before));
                    // What follows the cut joins a turn of its own, which the
                    // rewind's divider opens.
                    self.turns.push(Boundary::default());
                    change.opened = true;
                    change.rewind = Some(cut);
                    change.target = self.current();
                }
            }
            AgentEvent::RewindUndone { checkpoint_id } => {
                if let Some(undo) = self.undo.take_if(|undo| undo.checkpoint == *checkpoint_id) {
                    self.turns = undo.turns;
                    self.users = undo.users;
                    self.steer_checkpoints = undo.steer_checkpoints;
                    change.undone = true;
                    change.target = self.current();
                }
            }
            AgentEvent::ItemStarted(item)
            | AgentEvent::ItemUpdated(item)
            | AgentEvent::ItemCompleted(item)
                if item.parent_item_id.is_none() =>
            {
                if matches!(item.content, ItemContent::UserMessage { .. }) {
                    change.changed = true;
                    // A message sent after a rewind makes it final.
                    self.undo = None;
                    if let Some(turn) = self.current() {
                        let seq = self.next_seq;
                        self.users.entry(item.id.clone()).or_insert_with(|| User {
                            turn,
                            pending: false,
                            seq,
                        });
                        self.next_seq += 1;
                    }
                } else if let Some(user) = self.users.get_mut(&item.id) {
                    user.pending = false;
                    change.changed = true;
                }
            }
            AgentEvent::SteerRequested { request_id, .. } => {
                self.undo = None;
                if let Some(turn) = self.current() {
                    let seq = self.next_seq;
                    self.users
                        .entry(request_id.clone())
                        .or_insert_with(|| User {
                            turn,
                            pending: true,
                            seq,
                        });
                    self.next_seq += 1;
                }
            }
            AgentEvent::SteerAccepted { request_id } => {
                let running = self.turns.iter().rposition(|turn| turn.running);
                if let Some(user) = self.users.get_mut(request_id)
                    && user.pending
                {
                    user.pending = false;
                    if let Some(turn) = running {
                        user.turn = turn;
                    }
                    change.target = Some(user.turn);
                } else {
                    change.target = None;
                }
            }
            AgentEvent::SteerRejected { request_id } => {
                if self.users.get(request_id).is_some_and(|user| user.pending) {
                    self.users.remove(request_id);
                }
            }
            AgentEvent::TokenUsage(usage) => {
                self.compacting = usage.freshness == ContextFreshness::Compacting;
            }
            AgentEvent::ContextCompacted(compaction) => self.compacting = compaction.in_progress,
            _ => {}
        }
        change
    }

    /// Where the restore point `checkpoint` cuts the conversation.
    fn cut(&self, checkpoint: &str) -> Option<RewindCut> {
        if let Some(turn) = self
            .turns
            .iter()
            .position(|turn| turn.checkpoint.as_deref() == Some(checkpoint))
        {
            return Some(RewindCut { turn, steer: None });
        }
        let steer = self.steer_checkpoints.get(checkpoint)?;
        let user = self.users.get(steer)?;
        (user.turn < self.turns.len()).then(|| RewindCut {
            turn: user.turn,
            steer: Some(steer.clone()),
        })
    }

    fn opens_turn(&self, event: &AgentEvent) -> bool {
        match event {
            AgentEvent::TurnStarted { .. } | AgentEvent::ProviderRelay { .. } => !self.is_open(),
            AgentEvent::ItemStarted(item)
            | AgentEvent::ItemUpdated(item)
            | AgentEvent::ItemCompleted(item) => {
                item.parent_item_id.is_none()
                    && if matches!(item.content, ItemContent::UserMessage { .. }) {
                        !self.users.contains_key(&item.id) && !self.is_open()
                    } else {
                        self.is_empty()
                    }
            }
            AgentEvent::SteerRequested { .. }
            | AgentEvent::ServedModel { .. }
            | AgentEvent::TurnChangesUpdated { .. }
            | AgentEvent::TurnCheckpoint { .. }
            | AgentEvent::ProviderStartFailed { .. }
            | AgentEvent::Error { .. }
            | AgentEvent::SessionClosed { reason: Some(_) }
            | AgentEvent::Delta { .. }
            | AgentEvent::ContextWindowChanged { .. }
            | AgentEvent::SettingsKept
            | AgentEvent::SessionMoved { .. }
            | AgentEvent::WorktreeResolved { .. }
            | AgentEvent::ControlAction(_)
            | AgentEvent::UnreadableRecord { .. }
            | AgentEvent::RecordWithheld { .. } => self.is_empty(),
            AgentEvent::ContextCompacted(compaction) => {
                !(compaction.in_progress && self.compacting) && self.is_empty()
            }
            AgentEvent::SessionStarted { .. }
            | AgentEvent::TokenUsage(_)
            | AgentEvent::TurnCompleted { .. }
            | AgentEvent::SessionClosed { reason: None }
            | AgentEvent::SteerAccepted { .. }
            | AgentEvent::SteerRejected { .. }
            | AgentEvent::TurnRetry { .. }
            | AgentEvent::TurnAccepted { .. }
            | AgentEvent::McpServersNotAttached
            | AgentEvent::BackgroundTasksChanged { .. }
            | AgentEvent::SettingsApplied { .. }
            | AgentEvent::BackgroundTaskList { .. }
            | AgentEvent::ItemBackgrounded { .. }
            | AgentEvent::RewindCompleted { .. }
            | AgentEvent::RewindFailed { .. }
            | AgentEvent::RewindUndone { .. }
            | AgentEvent::RewindUndoFailed { .. }
            | AgentEvent::RewindPreviewed { .. }
            | AgentEvent::RewindAvailability { .. }
            | AgentEvent::SteerCheckpoint { .. }
            | AgentEvent::CheckpointsCleared
            | AgentEvent::RewindUndoClosed
            | AgentEvent::ApprovalRequested(_)
            | AgentEvent::ApprovalResolved { .. }
            | AgentEvent::WorktreeRequested { .. }
            | AgentEvent::UserInputRequested { .. }
            | AgentEvent::UserInputResolved { .. }
            | AgentEvent::Warning { .. }
            | AgentEvent::PlanUpdated { .. }
            | AgentEvent::GoalUpdated { .. }
            | AgentEvent::UsageLimitReached { .. }
            | AgentEvent::ProviderCommands { .. }
            | AgentEvent::ProviderOptions { .. } => false,
        }
    }
}

/// The log rows that opened the turns a fold holds now, in order. A reader
/// of the log is sent windows that begin at one of them: a window that began
/// inside turns a later rewind took out would hold the rewind but not its
/// target, and the reader's fold could not cut where the whole log's does.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnStarts {
    rows: Vec<u64>,
    /// The rows before the latest rewind, which its undo puts back.
    before_rewind: Option<Vec<u64>>,
}

impl TurnStarts {
    pub fn rows(&self) -> &[u64] {
        &self.rows
    }

    /// Follow what the record at `row` did to the turns.
    pub fn apply(&mut self, change: &TurnBoundaryChange, row: u64) {
        if change.undone
            && let Some(rows) = self.before_rewind.take()
        {
            self.rows = rows;
        }
        if let Some(cut) = &change.rewind {
            self.before_rewind = Some(self.rows.clone());
            self.rows
                .truncate(cut.turn + usize::from(cut.steer.is_some()));
        }
        if change.opened {
            self.rows.push(row);
        }
    }
}
