//! Projects moving from this host to another machine of its group, as the
//! host's [`Topic::ProjectMoves`] lists them, and what became of each one
//! that left the list.

use std::collections::{HashMap, HashSet};

use gpui::{App, Context, Task};
use tcode_protocol::{
    Command, MovePhase, ProjectMovePreview, ProjectMoveStatus, ProtocolError, Query, QueryResponse,
    Topic,
};

use super::WorkspaceStore;

#[derive(Default)]
pub(super) struct ProjectMoves {
    list: Vec<ProjectMoveStatus>,
    /// Each listed project's name and threads, kept while it is listed: the
    /// index may drop them before the list does.
    listed: HashMap<String, (String, Vec<String>)>,
    /// Moves that left the list whose outcome the index has yet to show.
    departing: Vec<Departing>,
    /// Projects whose move this client asked to stop.
    cancelled_here: HashSet<String>,
    /// Threads that went with a move that ended here, and the machine each
    /// went to.
    moved_threads: HashMap<String, String>,
}

struct Departing {
    status: ProjectMoveStatus,
    project_name: String,
    thread_ids: Vec<String>,
}

/// A move the host listed has ended: the project is on `machine` now, or the
/// move was cancelled by someone other than this client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectMoveEnded {
    pub project_name: String,
    pub machine: String,
    /// The threads that went with it, which keep their ids there.
    pub thread_ids: Vec<String>,
    pub moved: bool,
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

    /// The machine `session_id` went to with its project, as far as the
    /// moves this store followed say; a thread that left while this client
    /// was away is the host's to answer ([`Self::moved_thread`]).
    pub(crate) fn moved_thread_machine(&self, session_id: &str) -> Option<&str> {
        let moves = &self.project_moves;
        if let Some(machine) = moves.moved_threads.get(session_id) {
            return Some(machine);
        }
        let holds = |ids: &[String]| ids.iter().any(|id| id == session_id);
        moves
            .departing
            .iter()
            .filter(|departing| departing.status.phase != MovePhase::Cancelling)
            .find(|departing| holds(&departing.thread_ids))
            .map(|departing| departing.status.machine.as_str())
            .or_else(|| {
                moves
                    .list
                    .iter()
                    .filter(|status| status.phase != MovePhase::Cancelling)
                    .find(|status| {
                        moves
                            .listed
                            .get(&status.project_id)
                            .is_some_and(|(_, ids)| holds(ids))
                    })
                    .map(|status| status.machine.as_str())
            })
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
        self.project_moves.cancelled_here.insert(project_id.clone());
        self.command(Command::CancelProjectMove { project_id, force }, cx)
    }

    /// The host's whole list of moves: a move gone from it either ended in
    /// the project leaving this host's index, or was cancelled.
    pub(super) fn apply_project_moves(
        &mut self,
        list: &[ProjectMoveStatus],
        cx: &mut Context<Self>,
    ) {
        let moves = &mut self.project_moves;
        for left in moves
            .list
            .iter()
            .filter(|old| !list.iter().any(|new| new.move_id == old.move_id))
        {
            let (project_name, thread_ids) =
                moves.listed.remove(&left.project_id).unwrap_or_default();
            moves.departing.push(Departing {
                status: left.clone(),
                project_name,
                thread_ids,
            });
        }
        moves.departing.retain(|departing| {
            !list
                .iter()
                .any(|new| new.move_id == departing.status.move_id)
        });
        moves.list = list.to_vec();
        let (sessions, projects) = &self.index_replica;
        for status in &moves.list {
            let Some(project) = projects.iter().find(|p| p.id == status.project_id) else {
                continue;
            };
            let threads = sessions
                .iter()
                .filter(|meta| meta.project_id.as_deref() == Some(project.id.as_str()))
                .map(|meta| meta.id.clone());
            let entry = moves
                .listed
                .entry(status.project_id.clone())
                .or_insert_with(|| (project.name.clone(), Vec::new()));
            entry.0.clone_from(&project.name);
            for id in threads {
                if !entry.1.contains(&id) {
                    entry.1.push(id);
                }
            }
        }
        self.resolve_departed_moves(cx);
    }

    /// Settle what the departed moves came to as far as the index shows it:
    /// a cancelled move ends at once; one that went through ends once its
    /// project has left the index.
    pub(super) fn resolve_departed_moves(&mut self, cx: &mut Context<Self>) {
        if self.project_moves.departing.is_empty() {
            return;
        }
        let hydrated = self.index_hydrated;
        let projects = &self.index_replica.1;
        let mut ended = Vec::new();
        self.project_moves.departing.retain(|departing| {
            let cancelled = departing.status.phase == MovePhase::Cancelling;
            let moved = !cancelled
                && hydrated
                && !projects
                    .iter()
                    .any(|project| project.id == departing.status.project_id);
            if cancelled || moved {
                ended.push((
                    departing.status.project_id.clone(),
                    ProjectMoveEnded {
                        project_name: departing.project_name.clone(),
                        machine: departing.status.machine.clone(),
                        thread_ids: departing.thread_ids.clone(),
                        moved,
                    },
                ));
            }
            !(cancelled || moved)
        });
        for (project_id, event) in ended {
            if event.moved {
                for id in &event.thread_ids {
                    self.project_moves
                        .moved_threads
                        .insert(id.clone(), event.machine.clone());
                }
            }
            let asked_here = self.project_moves.cancelled_here.remove(&project_id);
            if event.moved || !asked_here {
                cx.emit(event);
            }
        }
    }

    /// The move the store had followed is no longer the host's to say: a
    /// new scope or a new baseline lists it again.
    pub(super) fn forget_project_moves(&mut self) {
        self.project_moves = ProjectMoves::default();
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
