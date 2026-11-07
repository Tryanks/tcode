use agent::ForkPoint;
use tcode_core::project::ForkOrigin;
use tcode_core::session::EntryContent;
use tcode_protocol::{ForkAt, ForkStatus};
use tcode_services::store::CloneBound;

use super::rewind::stored_prompt;
use super::sessions::fork_refusal;
use super::*;

/// A thread made from a message, and that message to edit or resend.
pub struct ForkedAt {
    pub id: String,
    pub text: String,
    pub attachments: Vec<String>,
}

/// How [`AppState::make_fork`] makes a thread from another.
struct ForkPlan {
    point: ForkPoint,
    bound: Option<CloneBound>,
    /// The branch the new thread takes up, which is the session the restore
    /// points in its log came from. A fork is a new session.
    branch: Option<String>,
}

impl AppState {
    /// What a fork from a message does in this thread and whether one can
    /// start now.
    pub(super) fn fork_status(&self, active: &ActiveSession) -> Option<ForkStatus> {
        let control = active.meta.provider.caps().fork_at?;
        // A branch not started yet holds the source's cursor, not its session's.
        if Self::conversation_read_only(&active.meta)
            || active.meta.resume_cursor.is_none()
            || matches!(active.meta.pending_fork, Some(ForkPoint::Branch { .. }))
        {
            return None;
        }
        Some(ForkStatus {
            first_turn: control.first_turn,
            blocked: self.rewind_requests.contains_key(&active.meta.id),
        })
    }

    /// The harness's restore point for the prompt `at` names, when a fork can
    /// begin before it now.
    pub(super) fn fork_checkpoint(&self, active: &ActiveSession, at: &ForkAt) -> Option<String> {
        let status = self.fork_status(active)?;
        if status.blocked || !status.offers(at.turn) {
            return None;
        }
        active
            .timeline
            .turns
            .get(at.turn)?
            .provider_checkpoint_id
            .clone()
            .filter(|checkpoint| *checkpoint == at.checkpoint_id)
    }

    /// Duplicate a stored transcript and arrange for its next provider start to
    /// fork the source's native session. The fork stays idle until its first
    /// user turn, exactly like a cold-opened stored thread.
    pub fn fork_thread(&mut self, id: &str, cx: &mut HostCx) -> Option<String> {
        let resident = self.resident(id);
        let source = resident
            .map(|session| &session.meta)
            .or_else(|| self.sessions.iter().find(|meta| meta.id == id))?;
        if let Some(error) = fork_refusal(Self::session_fork_availability(source, resident)) {
            self.report_error(RuntimeError::External(error.into()), cx);
            return None;
        }
        let source = source.clone();
        // A fork not started yet still names the point it was made from.
        let point = source.pending_fork.clone().unwrap_or(ForkPoint::Tail);
        Some(self.make_fork(
            &source,
            ForkPlan {
                point,
                bound: None,
                branch: None,
            },
            cx,
        ))
    }

    /// A new thread holding what came before the prompt `at` names, through
    /// the harness's own fork from that message. The source goes on as it is,
    /// a turn running there included: the fork is made by a process of its
    /// own at the new thread's first send.
    pub fn fork_thread_at(&mut self, id: &str, at: &ForkAt, cx: &mut HostCx) -> Option<ForkedAt> {
        let active = self.resident(id)?;
        let checkpoint_id = self.fork_checkpoint(active, at)?;
        let (text, attachments) = stored_prompt(&active.timeline, &checkpoint_id)?;
        let opens_with_divider = active
            .timeline
            .entries
            .iter()
            .find(|entry| entry.turn == at.turn)
            .is_some_and(|entry| matches!(entry.content, EntryContent::Rewound(_)));
        let bound = if opens_with_divider {
            CloneBound::ThroughTurnOpening(at.turn)
        } else {
            CloneBound::BeforeTurn(at.turn)
        };
        let source = active.meta.clone();
        let id = self.make_fork(
            &source,
            ForkPlan {
                point: ForkPoint::Before { checkpoint_id },
                bound: Some(bound),
                branch: None,
            },
            cx,
        );
        Some(ForkedAt {
            id,
            text,
            attachments,
        })
    }

    /// The branch the rewind whose divider opened `turn` left, once it can no
    /// longer be undone in the thread: while it can, the thread may still go
    /// back to that branch, which two threads cannot both go on in.
    pub(super) fn rewound_branch(
        active: &ActiveSession,
        turn: usize,
        checkpoint_id: &str,
    ) -> Option<String> {
        active
            .timeline
            .entries
            .iter()
            .filter(|entry| entry.turn == turn)
            .find_map(|entry| match &entry.content {
                EntryContent::Rewound(rewound)
                    if rewound.checkpoint_id == checkpoint_id && rewound.undo_token.is_none() =>
                {
                    rewound.branch.clone()
                }
                _ => None,
            })
    }

    /// Go on in the branch a rewind left, in a thread of its own: the one
    /// already on it, or a new one holding that branch's turns, which are the
    /// log before the rewind. The thread goes on in the harness's kept
    /// session, not a copy of it.
    pub fn open_rewound_branch(
        &mut self,
        id: &str,
        turn: usize,
        checkpoint_id: &str,
        cx: &mut HostCx,
    ) -> Option<String> {
        let active = self.resident(id)?;
        let token = Self::rewound_branch(active, turn, checkpoint_id)?;
        let source = active.meta.clone();
        let taken = self
            .sessions
            .iter()
            .find(|meta| {
                meta.fork_origin
                    .as_ref()
                    .is_some_and(|origin| origin.branch.as_ref() == Some(&token))
            })
            .map(|meta| meta.id.clone());
        if let Some(thread) = taken {
            return Some(thread);
        }
        Some(self.make_fork(
            &source,
            ForkPlan {
                point: ForkPoint::Branch {
                    token: token.clone(),
                },
                bound: Some(CloneBound::BeforeTurn(turn)),
                branch: Some(token),
            },
            cx,
        ))
    }

    fn make_fork(&mut self, source: &SessionMeta, plan: ForkPlan, cx: &mut HostCx) -> String {
        let mut fork = SessionMeta::new(source.provider, source.cwd.clone(), source.model.clone());
        fork.title = format!("{} (fork)", source.title);
        fork.option_selections = source.option_selections.clone();
        fork.project_id = source.project_id.clone();
        fork.acp_agent_id = source.acp_agent_id.clone();
        fork.profile_id = source.profile_id.clone();
        fork.resume_cursor = source.resume_cursor.clone();
        fork.session_transfer_from = source.session_transfer_from.clone();
        // The fork carries the source's context, so it may read what the source could.
        fork.references = source.references.clone();
        fork.pending_fork = Some(plan.point);
        let new_session = plan.branch.is_none();
        fork.fork_origin = Some(ForkOrigin {
            thread_id: source.id.clone(),
            title: source.title.clone(),
            branch: plan.branch,
        });
        // `worktree` deliberately stays absent: it is an ownership/cleanup
        // marker. The cwd may be shared, but the fork must not own the source's
        // generated worktree or offer to delete it.

        let fork_id = fork.id.clone();
        self.upsert_session_in_memory(fork.clone());
        let (completion, completed) = smol::channel::bounded(1);
        self.enqueue_store_write(
            StoreWrite::Fork {
                src: source.id.clone(),
                bound: plan.bound,
                meta: Box::new(fork.clone()),
                completion,
            },
            cx,
        );
        self.queue_activity_load(&fork_id, cx);
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let result = completed
                .recv()
                .await
                .unwrap_or_else(|_| Err("session store writer stopped".into()));
            host_cx.enqueue(move |state, cx| match result {
                Ok(()) => {
                    // The parent's restore points do not address a new session.
                    // A branch keeps them, but not the return from a rewind in
                    // the rows it was copied with: that left another session.
                    let closing = if new_session {
                        AgentEvent::CheckpointsCleared
                    } else {
                        AgentEvent::RewindUndoClosed
                    };
                    state.record_event(&fork.id, &closing, cx);
                    state.select_session(&fork.id, cx);
                    state.reply_to_subscription(
                        None,
                        tcode_protocol::Subscription {
                            known_index: None,
                            topic: Topic::SessionEvents {
                                session_id: fork.id.clone(),
                            },
                            after: None,
                        },
                        cx,
                    );
                }
                Err(error) => {
                    state.report_error(RuntimeError::PersistSession { error }, cx);
                }
            });
        });
        fork_id
    }
}

#[cfg(test)]
#[path = "fork_tests.rs"]
mod tests;
