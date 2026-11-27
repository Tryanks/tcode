//! Projects moving from this host to another machine of its group, and how
//! the moves that ended did, as the host's [`Topic::ProjectMoves`] says.

use std::collections::HashMap;

use gpui::{App, Context, Task};
use tcode_protocol::{
    Command, EndedProjectMove, MoveOutcome, ProjectMovePreview, ProjectMoveStatus,
    ProjectMovesState, ProtocolError, Query, QueryResponse, Topic,
};

use super::WorkspaceStore;

#[derive(Default)]
pub(super) struct ProjectMoves {
    list: Vec<ProjectMoveStatus>,
    /// How the host says each move that ended in its run ended.
    ended: Vec<EndedProjectMove>,
    /// The host's state has come at least once: what it holds then is learned,
    /// not announced.
    synced: bool,
    /// Projects whose move a progress view of this window shows, and how many
    /// do: the view says how the move ended, so no notice does.
    watching: HashMap<String, usize>,
    /// Open drafts of a project that moved away, and the machine it went to:
    /// a new thread there was never written, so none went with the project.
    drafts: HashMap<String, String>,
}

impl WorkspaceStore {
    /// `project_id`'s move away from this host, while one runs.
    pub(crate) fn project_move(&self, project_id: &str) -> Option<&ProjectMoveStatus> {
        self.project_moves
            .list
            .iter()
            .find(|status| status.project_id == project_id)
    }

    /// The move of the project `session_id` is in, while one runs: the host
    /// takes no work in the thread until it ends.
    pub(crate) fn thread_project_move(
        &self,
        session_id: &str,
        cx: &App,
    ) -> Option<&ProjectMoveStatus> {
        if self.project_moves.list.is_empty() {
            return None;
        }
        // A draft the index does not list yet has its project in its status.
        let project_id = self
            .session_status_for(Some(session_id), cx)
            .and_then(|status| status.project_id.as_deref())
            .or_else(|| self.thread_meta(session_id)?.project_id.as_deref())?;
        self.project_move(project_id)
    }

    /// How the host says move `move_id` ended, once it has.
    pub(crate) fn ended_project_move(&self, move_id: &str) -> Option<&EndedProjectMove> {
        self.project_moves
            .ended
            .iter()
            .find(|ended| ended.move_id == move_id)
    }

    /// The machine `session_id` went to with its project, as far as the
    /// host's record of the moves that ended in its run says; a thread that
    /// left before that is the host's to answer ([`Self::moved_thread`]).
    pub(crate) fn moved_thread_machine(&self, session_id: &str) -> Option<&str> {
        let moves = &self.project_moves;
        moves
            .ended
            .iter()
            .find(|ended| {
                ended.outcome == MoveOutcome::Moved
                    && ended.thread_ids.iter().any(|id| id == session_id)
            })
            .map(|ended| ended.machine.as_str())
            .or_else(|| moves.drafts.get(session_id).map(String::as_str))
    }

    /// A progress view shows `project_id`'s move from now on.
    pub(crate) fn watch_project_move(&mut self, project_id: &str) {
        *self
            .project_moves
            .watching
            .entry(project_id.to_owned())
            .or_default() += 1;
    }

    /// A progress view that showed `project_id`'s move is gone.
    pub(crate) fn unwatch_project_move(&mut self, project_id: &str) {
        let watching = &mut self.project_moves.watching;
        if let Some(count) = watching.get_mut(project_id) {
            *count -= 1;
            if *count == 0 {
                watching.remove(project_id);
            }
        }
    }

    /// Whether a progress view of this window shows `project_id`'s move.
    pub(crate) fn project_move_watched(&self, project_id: &str) -> bool {
        self.project_moves.watching.contains_key(project_id)
    }

    /// Whether `session_id` was a draft of a project that moved away.
    pub(crate) fn moved_draft(&self, session_id: &str) -> bool {
        self.project_moves.drafts.contains_key(session_id)
    }

    /// What moving `project_id` to `machine` would take.
    pub(crate) fn project_move_preview(
        &self,
        project_id: String,
        machine: String,
        cx: &mut App,
    ) -> Task<Result<ProjectMovePreview, ProtocolError>> {
        let host = self.host.clone();
        cx.spawn(async move |_| {
            match host
                .query(Query::ProjectMovePreview {
                    project_id,
                    machine,
                })
                .await?
            {
                QueryResponse::ProjectMovePreview(preview) => Ok(preview),
                other => Err(unexpected(&other)),
            }
        })
    }

    /// The machine of the group `thread_id` went to with its project, when
    /// it moved away from this host.
    pub(crate) fn moved_thread(
        &self,
        thread_id: String,
        cx: &mut App,
    ) -> Task<Result<Option<String>, ProtocolError>> {
        let host = self.host.clone();
        cx.spawn(
            async move |_| match host.query(Query::MovedThread { thread_id }).await? {
                QueryResponse::MovedThread(machine) => Ok(machine),
                other => Err(unexpected(&other)),
            },
        )
    }

    /// Ask the host to stop moving `project_id`; with `force`, whatever the
    /// other machine did with it.
    pub(crate) fn cancel_project_move(
        &mut self,
        project_id: String,
        force: bool,
        cx: &mut App,
    ) -> Task<Result<tcode_protocol::CommandResponse, ProtocolError>> {
        self.command(Command::CancelProjectMove { project_id, force }, cx)
    }

    /// The host's whole state of the moves. A move it now says ended, that
    /// this store had not heard of, is announced, except what the first state
    /// holds.
    pub(super) fn apply_project_moves(
        &mut self,
        state: &ProjectMovesState,
        cx: &mut Context<Self>,
    ) {
        let moves = &mut self.project_moves;
        let fresh: Vec<EndedProjectMove> = if moves.synced {
            state
                .ended
                .iter()
                .filter(|ended| {
                    !moves
                        .ended
                        .iter()
                        .any(|known| known.move_id == ended.move_id)
                })
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        moves.list.clone_from(&state.moving);
        moves.ended.clone_from(&state.ended);
        moves.synced = true;
        let drafts: Vec<(String, String)> = self
            .open_threads()
            .into_iter()
            .filter_map(|id| {
                let status = self.session_status_for(Some(&id), cx)?;
                let project = status.project_id.as_deref().filter(|_| status.draft)?;
                let moved = fresh.iter().find(|ended| {
                    ended.outcome == MoveOutcome::Moved && ended.project_id == project
                })?;
                Some((id, moved.machine.clone()))
            })
            .collect();
        self.project_moves.drafts.extend(drafts);
        for ended in fresh {
            cx.emit(ended);
        }
    }

    /// What the store had learned of the moves is no longer the host's to say:
    /// a new scope or a new baseline says it again.
    pub(super) fn forget_project_moves(&mut self) {
        self.project_moves = ProjectMoves {
            watching: std::mem::take(&mut self.project_moves.watching),
            ..ProjectMoves::default()
        };
    }

    /// Follow the moves of the host's projects; only a full workspace may.
    pub(super) fn follow_project_moves(&self) {
        let _ = self.host.subscribe(tcode_protocol::Subscription {
            known_index: None,
            topic: Topic::ProjectMoves,
            after: None,
        });
    }
}

fn unexpected(answer: &QueryResponse) -> ProtocolError {
    ProtocolError {
        code: "unexpected_response".into(),
        message: format!("unexpected response: {answer:?}"),
    }
}
