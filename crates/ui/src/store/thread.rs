//! What the client holds of one thread, whether or not a surface shows it.

use std::collections::HashMap;

use gpui::Task;
use tcode_core::{session::Timeline, ui::ConversationDestination};
use tcode_protocol::{GitStatusStatus, SessionPlan, SessionStatus};

use super::{command_output::CommandOutputReplica, history::HeldHistory};

/// One thread's replicas and the client-side paging state over them.
///
/// A thread is `open` while a surface shows it: its session topics are
/// subscribed and events apply to it. A closed thread keeps its replicas for
/// a while ([`super::KEPT_THREADS`]) so reopening it fetches only what it
/// missed.
#[derive(Default)]
pub(super) struct ThreadReplica {
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
    /// Turns of the log before the first held one.
    pub(super) turn_offset: usize,
    /// The window's records are still arriving.
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
        self.timeline = Some(timeline);
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
