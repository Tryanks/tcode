//! A project and its threads moving to this machine from another of the
//! group: staged as the parts arrive, invisible until `MoveCommit` makes the
//! project and every thread at once. The transport has already checked that
//! the caller is a member on this protocol and that this machine takes
//! projects.
use std::path::{Path, PathBuf};

use tcode_protocol::{MachineRequest, MachineResponse, MoveState};
use tcode_services::project_move as disk;

use super::MachinePeer;
use crate::app::*;

fn refusal(code: &str, message: impl Into<String>) -> ProtocolError {
    ProtocolError {
        code: code.into(),
        message: message.into(),
    }
}

fn io_refusal(error: std::io::Error) -> ProtocolError {
    refusal("move_failed", error.to_string())
}

impl AppState {
    /// One step of a move from `peer` to this machine.
    pub(super) fn machine_move(
        &mut self,
        peer: &MachinePeer,
        request: MachineRequest,
        cx: &mut HostCx,
    ) -> HostTask<Result<MachineResponse, ProtocolError>> {
        let data_dir = self.store.root().clone();
        let now = |result| cx.spawn_background(async move { result });
        match request {
            MachineRequest::MovePreflight {
                project_name,
                root,
                thread_ids,
            } => {
                if let Err(error) = self.arriving_threads(&thread_ids) {
                    return now(Err(error));
                }
                if let Some(Err(error)) = root.map(|root| self.arrival_root(&root)) {
                    return now(Err(error));
                }
                let parent = self
                    .user_directories
                    .resolve_host_path(Path::new(self.settings.new_project_dir()));
                let taken: Vec<PathBuf> = self.projects.iter().map(|p| p.root.clone()).collect();
                cx.unblock(move || {
                    let default_root = match parent {
                        Ok(parent) => unique_root(&parent, &project_name, &taken),
                        Err(error) => return Err(io_refusal(error)),
                    };
                    Ok(MachineResponse::MovePreflight {
                        default_root: default_root.display().to_string(),
                    })
                })
            }
            MachineRequest::MoveBegin {
                move_id,
                project_name,
                root,
                git,
                thread_ids,
                parts,
            } => {
                if !disk::safe_name(&move_id) {
                    return now(Err(refusal("invalid_move", "That move id names no file.")));
                }
                if self.moves.arrived.contains_key(&move_id) {
                    return now(Ok(MachineResponse::MoveBegun {
                        received: parts
                            .iter()
                            .map(|part| (part.name.clone(), part.size))
                            .collect(),
                    }));
                }
                if let Err(error) = self
                    .arriving_threads(&thread_ids)
                    .and_then(|()| self.arrival_root(&root).map(drop))
                {
                    return now(Err(error));
                }
                let staged = disk::Staged {
                    project_name,
                    root,
                    git,
                    thread_ids,
                    parts,
                };
                cx.unblock(move || {
                    disk::stage_begin(&data_dir, &move_id, &staged)
                        .map(|received| MachineResponse::MoveBegun { received })
                        .map_err(io_refusal)
                })
            }
            MachineRequest::MoveChunk {
                move_id,
                part,
                offset,
                bytes,
            } => cx.unblock(move || {
                disk::stage_chunk(&data_dir, &move_id, &part, offset, &bytes)
                    .map(|received| MachineResponse::MoveProgress { received })
                    .map_err(io_refusal)
            }),
            MachineRequest::MoveAbort { move_id } => {
                if self.moves.committing.contains(&move_id) {
                    return now(Err(refusal(
                        "move_busy",
                        "This move is being committed here.",
                    )));
                }
                if self.moves.arrived.contains_key(&move_id) {
                    return now(Ok(MachineResponse::Done));
                }
                cx.unblock(move || {
                    disk::discard_staging(&data_dir, &move_id)
                        .map(|()| MachineResponse::Done)
                        .map_err(io_refusal)
                })
            }
            MachineRequest::MoveStatus { move_id } => {
                if let Some(project_id) = self.moves.arrived.get(&move_id) {
                    return now(Ok(MachineResponse::MoveState(MoveState::Committed {
                        project_id: project_id.clone(),
                    })));
                }
                if self.moves.committing.contains(&move_id) {
                    return now(Ok(MachineResponse::MoveState(MoveState::Staging)));
                }
                cx.unblock(move || {
                    disk::staged(&data_dir, &move_id)
                        .map(|staged| {
                            MachineResponse::MoveState(match staged {
                                Some(_) => MoveState::Staging,
                                None => MoveState::Unknown,
                            })
                        })
                        .map_err(io_refusal)
                })
            }
            MachineRequest::MoveCommit { move_id } => self.commit_arrival(peer, move_id, cx),
            other => now(Err(refusal(
                "unexpected_request",
                format!("{other:?} is no step of a move"),
            ))),
        }
    }

    /// None of `ids` is a thread here.
    fn arriving_threads(&self, ids: &[String]) -> Result<(), ProtocolError> {
        if let Some(id) = ids.iter().find(|id| !disk::safe_name(id)) {
            return Err(refusal(
                "invalid_move",
                format!("thread id {id:?} names no file"),
            ));
        }
        match ids.iter().find(|id| self.current_meta(id).is_some()) {
            Some(id) => Err(refusal(
                "thread_exists",
                format!("This machine already has thread {id}."),
            )),
            None => Ok(()),
        }
    }

    /// `root` as a project root here: absolute once `~` is this machine's
    /// home, absent or an empty directory, and inside no project.
    fn arrival_root(&self, root: &str) -> Result<PathBuf, ProtocolError> {
        let invalid = |message: String| refusal("invalid_move_root", message);
        let path = self
            .user_directories
            .resolve_host_path(Path::new(root.trim()))
            .map_err(|_| invalid(format!("{root} is not an absolute path on this machine")))?;
        if let Some(project) = self
            .projects
            .iter()
            .find(|project| path.starts_with(&project.root))
        {
            return Err(invalid(format!(
                "{} is inside the project {}",
                path.display(),
                project.name
            )));
        }
        match std::fs::read_dir(&path).map(|mut entries| entries.next().is_none()) {
            Ok(false) => Err(invalid(format!("{} is not empty", path.display()))),
            Ok(true) => Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path),
            Err(error) => Err(invalid(format!("{}: {error}", path.display()))),
        }
    }

    /// Make the project staged by `move_id` and every thread it brings, once.
    fn commit_arrival(
        &mut self,
        peer: &MachinePeer,
        move_id: String,
        cx: &mut HostCx,
    ) -> HostTask<Result<MachineResponse, ProtocolError>> {
        let now = |result| cx.spawn_background(async move { result });
        if let Some(project_id) = self.moves.arrived.get(&move_id) {
            return now(Ok(MachineResponse::MoveCommitted {
                project_id: project_id.clone(),
            }));
        }
        if self.moves.committing.contains(&move_id) {
            return now(Err(refusal(
                "move_busy",
                "This move is being committed here.",
            )));
        }
        let data_dir = self.store.root().clone();
        let staged = match disk::staged(&data_dir, &move_id) {
            Ok(Some(staged)) => staged,
            Ok(None) => {
                return now(Err(refusal(
                    "move_not_staged",
                    "Nothing of this move is staged here.",
                )));
            }
            Err(error) => return now(Err(io_refusal(error))),
        };
        let root = match self
            .arriving_threads(&staged.thread_ids)
            .and_then(|()| self.arrival_root(&staged.root))
        {
            Ok(root) => root,
            Err(error) => return now(Err(error)),
        };
        if let Err(error) = self.machine_link() {
            return now(Err(error));
        }
        self.moves.committing.insert(move_id.clone());
        let project = Project {
            id: uuid::Uuid::new_v4().to_string(),
            name: staged.project_name.clone(),
            root: root.clone(),
            quick_chat: false,
            icon_path: None,
            permission_defaults: Default::default(),
            created_at: cx.clock().unix_secs(),
        };
        let settings = self.settings.clone();
        let store = self.store.clone();
        let peer = peer.clone();
        let now_ms = cx.clock().unix_millis();
        let host = cx.clone();
        cx.spawn_background(async move {
            let arrived = {
                let (project, move_id) = (project.clone(), move_id.clone());
                host.unblock(move || {
                    // A thread keeps its profile where this machine has it for
                    // the same harness, and else takes the harness's own.
                    let profile = |meta: &SessionMeta| {
                        meta.profile_id.clone().filter(|id| {
                            settings
                                .resolved_profile(id)
                                .is_some_and(|profile| profile.kind == meta.provider)
                        })
                    };
                    disk::arrive(
                        &store,
                        &disk::Arrival {
                            move_id: &move_id,
                            root: &root,
                            project,
                            from_machine_name: &peer.name,
                            profile: &profile,
                            now_ms,
                        },
                    )
                })
                .await
            };
            host.enqueue_and_wait(move |app, cx| {
                app.moves.committing.remove(&move_id);
                let metas = arrived.map_err(|message| refusal("move_failed", message))?;
                app.moves.arrived.insert(move_id, project.id.clone());
                let project_id = project.id.clone();
                app.projects.push(project);
                for meta in &metas {
                    app.moves.moved_threads.remove(&meta.id);
                }
                let archived = app.archived_sessions();
                for meta in metas {
                    let id = meta.id.clone();
                    app.upsert_session_in_memory(meta);
                    app.queue_activity_load(&id, cx);
                }
                if app.archived_sessions() != archived {
                    app.archived_revision += 1;
                }
                Ok(MachineResponse::MoveCommitted { project_id })
            })
            .await
            .unwrap_or_else(|()| {
                Err(refusal(
                    "transport_closed",
                    "Host stopped before the move was committed.",
                ))
            })
        })
    }
}

/// `parent/name`, or `parent/name-2` and on when that is taken by a file or
/// a project.
fn unique_root(parent: &Path, name: &str, taken: &[PathBuf]) -> PathBuf {
    let name = Path::new(name)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty() && name != "..")
        .unwrap_or_else(|| "project".into());
    (1..)
        .map(|n| match n {
            1 => parent.join(&name),
            n => parent.join(format!("{name}-{n}")),
        })
        .find(|candidate| {
            !taken.iter().any(|root| candidate.starts_with(root))
                && std::fs::read_dir(candidate).map_or_else(
                    |error| error.kind() == std::io::ErrorKind::NotFound,
                    |mut entries| entries.next().is_none(),
                )
        })
        .expect("an unused name is found")
}
