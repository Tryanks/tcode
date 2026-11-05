//! What the client holds of one thread, whether or not a surface shows it.

use std::collections::HashMap;

use gpui::{App, AppContext as _, Entity, Task};
use tcode_core::{session::Timeline, ui::ConversationDestination};
use tcode_protocol::{GitStatusStatus, SessionPlan, SessionStatus};

use super::{command_output::CommandOutputReplica, history::HeldHistory};

/// A part of the store a view can depend on alone. The store bumps it (an
/// update that notifies) whenever the part changes, and the accessors of the
/// part read it: in gpui-fast a view is drawn again when an entity it read as
/// it rendered changed, so a view drawn from the part is drawn again for it,
/// not for everything else the store holds.
pub(crate) struct Revision(u64);

impl Revision {
    pub(super) fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|_| Self(0))
    }

    pub(super) fn bump(this: &Entity<Self>, cx: &mut App) {
        this.update(cx, |revision, cx| {
            revision.0 = revision.0.wrapping_add(1);
            cx.notify();
        });
    }

    /// Where the part stands, read as a view depending on it.
    pub(super) fn watch(this: &Entity<Self>, cx: &App) -> u64 {
        this.read(cx).0
    }
}

/// The parts of a thread's replica that change on their own, each with the
/// revision its readers depend on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Part {
    /// The held log and its fold: every live record.
    Timeline,
    /// The fold's entries and turns as items: every live record but text
    /// streamed into an item already there, which changes no item's kind,
    /// status or place.
    Entries,
    /// Status, plan and Git, and what the thread's status topic carries
    /// besides (a rewind's prefill, a classifier stop).
    Status,
    /// The screens of its commands' outputs.
    Outputs,
}

pub(super) struct Revisions {
    timeline: Entity<Revision>,
    entries: Entity<Revision>,
    status: Entity<Revision>,
    outputs: Entity<Revision>,
}

impl Revisions {
    fn new(cx: &mut App) -> Self {
        Self {
            timeline: Revision::new(cx),
            entries: Revision::new(cx),
            status: Revision::new(cx),
            outputs: Revision::new(cx),
        }
    }

    pub(super) fn of(&self, part: Part) -> &Entity<Revision> {
        match part {
            Part::Timeline => &self.timeline,
            Part::Entries => &self.entries,
            Part::Status => &self.status,
            Part::Outputs => &self.outputs,
        }
    }
}

/// One thread's replicas and the client-side paging state over them.
///
/// A thread is `open` while a surface shows it: its session topics are
/// subscribed and events apply to it. A closed thread keeps its replicas for
/// a while ([`super::KEPT_THREADS`]) so reopening it fetches only what it
/// missed.
pub(super) struct ThreadReplica {
    pub(super) revisions: Revisions,
    pub(super) open: bool,
    /// Bumped whenever the thread opens or closes; a task spawned for one
    /// opening finds a different number after the thread was left.
    pub(super) generation: u64,
    /// The `selection_generation` the user left the thread at.
    pub(super) left_at: u64,
    /// Absent until the first window of the thread's log arrives.
    pub(super) history: Option<HeldHistory>,
    /// Total turns at the last complete window, absent during catch-up.
    pub(super) total_turns: Option<usize>,
    pub(super) status: Option<SessionStatus>,
    pub(super) plan: Option<SessionPlan>,
    pub(super) git: Option<GitStatusStatus>,
    /// The held records folded, settled against `status`.
    pub(super) timeline: Option<Timeline>,
    /// Counts the times `timeline` was folded whole from the held records,
    /// rather than extended by a record streamed in live.
    pub(super) folds: u64,
    /// Turns of the log before the first held one.
    pub(super) turn_offset: usize,
    /// Subscribed, and the window that brings the replica to the host's
    /// position has not arrived: what it holds is the thread as it was left.
    pub(super) catching_up: bool,
    pub(super) history_task: Option<Task<()>>,
    /// Drops the pages fetched above the tail once the reader has stayed
    /// there ([`super::history::HISTORY_TRIM_DELAY`]).
    pub(super) history_trim: Option<Task<()>>,
    pub(super) history_error: Option<String>,
    pub(super) history_pages_fetched: usize,
    pub(super) history_logged_records: Option<usize>,
    /// Command output screens, keyed by item id.
    pub(super) command_outputs: HashMap<String, CommandOutputReplica>,
    /// The `updated_at` this thread was last reported read through.
    pub(super) read_acknowledged: Option<u64>,
    /// Where the thread's client-side conversation state lives: its own id,
    /// or its project while it is still a draft.
    pub(super) destination: Option<ConversationDestination>,
}

impl ThreadReplica {
    pub(super) fn new(cx: &mut App) -> Self {
        Self {
            revisions: Revisions::new(cx),
            open: false,
            generation: 0,
            left_at: 0,
            history: None,
            total_turns: None,
            status: None,
            plan: None,
            git: None,
            timeline: None,
            folds: 0,
            turn_offset: 0,
            catching_up: false,
            history_task: None,
            history_trim: None,
            history_error: None,
            history_pages_fetched: 0,
            history_logged_records: None,
            command_outputs: HashMap::new(),
            read_acknowledged: None,
            destination: None,
        }
    }

    /// The part's revision, read as a view depending on it.
    pub(super) fn watch(&self, part: Part, cx: &App) -> u64 {
        Revision::watch(self.revisions.of(part), cx)
    }

    pub(super) fn bump(&self, part: Part, cx: &mut App) {
        Revision::bump(self.revisions.of(part), cx);
    }
    pub(super) fn fold_held_records(&self) -> Timeline {
        Timeline::fold_stored(
            self.history
                .as_ref()
                .into_iter()
                .flat_map(|held| &held.records),
        )
    }

    pub(super) fn turn_running(&self) -> bool {
        self.status
            .as_ref()
            .is_some_and(|status| status.activity.turn_running)
    }

    /// Records folded after their provider stopped still end running, and a
    /// window cut inside the running turn misses its start: the host's status
    /// places the running turn among the held ones. Status and events are
    /// separate topics, so each settles the replica whenever it changes.
    pub(super) fn settle_running_turn(&self, timeline: &mut Timeline) {
        timeline.turn_retry = self
            .status
            .as_ref()
            .and_then(|status| status.turn_retry.clone());
        let running = self.status.as_ref().and_then(|status| status.running_turn);
        timeline.settle_running_turn(
            self.turn_running(),
            running.and_then(|running| {
                usize::try_from(running.turn)
                    .ok()?
                    .checked_sub(self.turn_offset)
            }),
            running.and_then(|running| running.started_at),
        );
    }

    /// Fold the held records into the timeline, placing `total_turns` turns
    /// of log before them.
    pub(super) fn refold(&mut self, total_turns: usize) {
        let mut timeline = self.fold_held_records();
        self.turn_offset = total_turns.saturating_sub(timeline.turns.len());
        self.settle_running_turn(&mut timeline);
        self.adopt_fold(timeline);
    }

    /// Show `timeline`, folded from the held records.
    pub(super) fn adopt_fold(&mut self, timeline: Timeline) {
        self.timeline = Some(timeline);
        self.folds = self.folds.wrapping_add(1);
    }

    /// Settle the timeline against a status that just changed.
    pub(super) fn resettle(&mut self) {
        if let Some(mut timeline) = self.timeline.take() {
            self.settle_running_turn(&mut timeline);
            self.timeline = Some(timeline);
        }
    }

    /// Forget the paging state of a visit; the replicas stay.
    pub(super) fn reset_paging(&mut self) {
        self.history_task = None;
        self.history_trim = None;
        self.history_error = None;
        self.history_pages_fetched = 0;
        self.history_logged_records = None;
        self.turn_offset = 0;
        self.catching_up = false;
    }
}
