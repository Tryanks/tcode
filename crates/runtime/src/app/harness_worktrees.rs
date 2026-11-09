use super::*;
use agent::{WorktreeAnswer, WorktreeRequest};
use tcode_core::project::OwnedWorktree;

/// What provisioning for a harness's request found.
enum Provisioned {
    Created(OwnedWorktree),
    /// The thread already owns the worktree the name maps to.
    Reused(PathBuf),
}

impl AppState {
    /// Answer a harness asking for a git worktree ([`AgentEvent::WorktreeRequested`]):
    /// every worktree it works in comes from Tcode's worktree module and belongs to the
    /// thread whose provider asked.
    pub(super) fn on_worktree_request(
        &mut self,
        session_id: &str,
        request_id: String,
        request: WorktreeRequest,
        cx: &mut HostCx,
    ) {
        let Some(meta) = self
            .resident(session_id)
            .map(|session| session.meta.clone())
        else {
            return;
        };
        let owned: Vec<(PathBuf, WorktreeInfo)> = meta
            .worktree
            .iter()
            .map(|info| (meta.cwd.clone(), info.clone()))
            .chain(
                meta.other_worktrees
                    .iter()
                    .map(|owned| (owned.path.clone(), owned.info.clone())),
            )
            .collect();
        let session_id = session_id.to_owned();
        let host_cx = cx.clone();
        match request {
            WorktreeRequest::Create { cwd, name } => {
                let data_dir = self.store.root().clone();
                HostCx::spawn_detached(cx, async move {
                    let provisioned = host_cx
                        .unblock(move || provision_for_harness(&cwd, &name, &data_dir, &owned))
                        .await;
                    host_cx.enqueue(move |state, cx| {
                        let answer = match provisioned {
                            Ok(Provisioned::Created(created)) => {
                                let path = created.path.clone();
                                if let Some(meta) = state.meta_mut(&session_id) {
                                    meta.other_worktrees.push(created);
                                    let meta = meta.clone();
                                    state.persist_meta(&meta, cx);
                                }
                                WorktreeAnswer::Ready {
                                    path,
                                    reused: false,
                                }
                            }
                            Ok(Provisioned::Reused(path)) => {
                                WorktreeAnswer::Ready { path, reused: true }
                            }
                            Err(error) => WorktreeAnswer::Refused { error },
                        };
                        state.answer_worktree_request(&session_id, request_id, answer);
                    });
                });
            }
            WorktreeRequest::Remove { path } => {
                let Some((path, info)) = owned.into_iter().find(|(owned, _)| *owned == path) else {
                    self.answer_worktree_request(
                        &session_id,
                        request_id,
                        WorktreeAnswer::Refused {
                            error: format!(
                                "{} is not a worktree Tcode made for this thread",
                                path.display()
                            ),
                        },
                    );
                    return;
                };
                HostCx::spawn_detached(cx, async move {
                    let removed = {
                        let path = path.clone();
                        host_cx
                            .unblock(move || remove_git_worktree(&info.root_project_path, &path))
                            .await
                    };
                    host_cx.enqueue(move |state, cx| {
                        let answer = match removed {
                            Ok(()) => {
                                if let Some(meta) = state.meta_mut(&session_id) {
                                    if meta.cwd == path {
                                        meta.worktree = None;
                                    }
                                    meta.other_worktrees.retain(|owned| owned.path != path);
                                    let meta = meta.clone();
                                    state.persist_meta(&meta, cx);
                                }
                                WorktreeAnswer::Removed
                            }
                            Err(error) => WorktreeAnswer::Refused {
                                error: error.to_string(),
                            },
                        };
                        state.answer_worktree_request(&session_id, request_id, answer);
                    });
                });
            }
        }
    }

    fn answer_worktree_request(
        &mut self,
        session_id: &str,
        request_id: String,
        answer: WorktreeAnswer,
    ) {
        let Some(Runtime::Live(commands)) = self.resident(session_id).map(|s| &s.runtime) else {
            log::warn!("worktree answer for {session_id} found no live provider: {answer:?}");
            return;
        };
        let _ = commands.try_send(SessionCommand::RespondWorktree { request_id, answer });
    }
}

/// Provision `name` for a harness asking from `cwd`: in the checkout `cwd` is in, from
/// what it has checked out now. A name the thread was given a worktree for before is
/// answered with that worktree while it is there, as Claude Code resumes an existing
/// worktree of the name its `EnterWorktree` is given.
fn provision_for_harness(
    cwd: &Path,
    name: &str,
    data_dir: &Path,
    owned: &[(PathBuf, WorktreeInfo)],
) -> Result<Provisioned, String> {
    let checkout = tcode_services::git::run_git(cwd, &["rev-parse", "--show-toplevel"])
        .map(|top| PathBuf::from(top.trim()))?;
    let path = tcode_services::worktree::worktree_path(&checkout, name, data_dir)
        .map_err(|error| error.to_string())?;
    if let Some((path, _)) = owned
        .iter()
        .find(|(owned, _)| owned.is_dir() && same_directory(owned, &path))
    {
        return Ok(Provisioned::Reused(path.clone()));
    }
    let created = provision(&checkout, name, None, data_dir).map_err(|error| error.to_string())?;
    // The harness checks the path it is given for links, and reports where it works
    // resolved; the thread records it the same way.
    let path = created
        .path
        .canonicalize()
        .map_err(|error| error.to_string())?;
    Ok(Provisioned::Created(OwnedWorktree {
        path,
        info: WorktreeInfo {
            root_project_path: checkout,
            base: created.base,
            branch: created.branch,
        },
    }))
}

/// Whether `left` and `right` name one directory, however each is spelled.
fn same_directory(left: &Path, right: &Path) -> bool {
    left == right
        || matches!(
            (left.canonicalize(), right.canonicalize()),
            (Ok(left), Ok(right)) if left == right
        )
}
