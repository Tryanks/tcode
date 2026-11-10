use agent::{RewindFailure, RewindFailureReason, RewindPreviewResult};
use tcode_core::session::EntryContent;
use tcode_protocol::{PendingRewind, RewindBlocked, RewindStatus};

use super::*;

/// A rewind, or the undo of one, the host asked a harness for and has no
/// answer to yet. Kept in memory: the harness's answer is the only authority
/// that can settle it.
pub(super) struct RewindRequest {
    pub(super) checkpoint_id: String,
    pub(super) mode: RewindMode,
    /// The undo token of the rewind being undone; `None` for a rewind.
    pub(super) undo_token: Option<String>,
    /// The client whose composer takes the restored prompt.
    pub(super) client_id: Option<String>,
    /// Told the harness's answer (a `tcode_control` call waiting on it).
    pub(super) waiters: Vec<smol::channel::Sender<AgentEvent>>,
}

impl RewindRequest {
    fn command(&self) -> SessionCommand {
        match &self.undo_token {
            Some(undo_token) => SessionCommand::UndoRewind {
                checkpoint_id: self.checkpoint_id.clone(),
                undo_token: undo_token.clone(),
            },
            None => SessionCommand::Rewind {
                checkpoint_id: self.checkpoint_id.clone(),
                mode: self.mode,
            },
        }
    }

    fn failure(&self, failure: RewindFailure) -> AgentEvent {
        if self.undo_token.is_some() {
            AgentEvent::RewindUndoFailed {
                checkpoint_id: self.checkpoint_id.clone(),
                failure,
            }
        } else {
            AgentEvent::RewindFailed {
                checkpoint_id: self.checkpoint_id.clone(),
                mode: self.mode,
                failure,
            }
        }
    }
}

/// The message a rewind goes to before: the prompt that opened absolute
/// turn `turn`, or the steer `steer` in it, with the restore point the asker
/// saw there when it says.
pub struct RewindAt<'a> {
    pub turn: usize,
    pub steer: Option<&'a str>,
    pub checkpoint: Option<&'a str>,
}

/// A client waiting for what a rewind would restore.
pub(super) struct PreviewRequest {
    checkpoint_id: String,
    mode: RewindMode,
    reply: smol::channel::Sender<RewindPreviewResult>,
}

impl AppState {
    /// What a rewind does in this thread and whether one can start now.
    pub(super) fn rewind_status(&self, active: &ActiveSession) -> Option<RewindStatus> {
        let control = active.meta.provider.caps().rewind?;
        if Self::conversation_read_only(&active.meta)
            || active.timeline.rewind_unavailable.is_some()
        {
            return None;
        }
        let pending = self.rewind_requests.get(&active.meta.id);
        let blocked = if pending.is_some() {
            Some(RewindBlocked::InProgress)
        } else if active.delivery_in_flight.is_some() {
            Some(RewindBlocked::Delivering)
        } else {
            None
        };
        Some(RewindStatus {
            modes: control.modes.to_vec(),
            first_turn_conversation: control.first_turn_conversation,
            steer_targets: control.steer_targets,
            later_history: control.later_history,
            preview: control.preview,
            restores_command_edits: control.restores_command_edits,
            blocked,
            pending: pending.map(|request| PendingRewind {
                checkpoint_id: request.checkpoint_id.clone(),
                mode: request.mode,
                undo: request.undo_token.is_some(),
            }),
        })
    }

    /// The harness's restore point for the prompt that opened `turn`, or for
    /// the steer `steer` in it, when `mode` can restore it now.
    pub(super) fn rewind_checkpoint(
        &self,
        active: &ActiveSession,
        at: &RewindAt<'_>,
        mode: RewindMode,
    ) -> Option<String> {
        let status = self.rewind_status(active)?;
        if status.blocked.is_some() || !status.offers(mode, at.turn, at.steer.is_some()) {
            return None;
        }
        let turn = active.timeline.turns.get(at.turn)?;
        match at.steer {
            None => turn.provider_checkpoint_id.clone(),
            Some(steer) => turn
                .steer_checkpoints
                .iter()
                .find(|(id, _)| id == steer)
                .map(|(_, checkpoint)| checkpoint.clone()),
        }
        .filter(|checkpoint| at.checkpoint.is_none_or(|seen| seen == checkpoint))
    }

    /// The undo token of the thread's latest rewind to `checkpoint_id`, while
    /// it can be undone now.
    pub(super) fn rewind_undo_token(
        &self,
        active: &ActiveSession,
        checkpoint_id: &str,
    ) -> Option<String> {
        if self.rewind_status(active)?.blocked.is_some() {
            return None;
        }
        active
            .timeline
            .entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.content {
                EntryContent::Rewound(rewound) => Some(rewound),
                _ => None,
            })
            .filter(|rewound| rewound.checkpoint_id == checkpoint_id)
            .and_then(|rewound| rewound.undo_token.clone())
    }

    /// Ask the harness to restore one of its own restore points. Nothing local
    /// changes until it answers: the timeline cuts only on its confirmation.
    /// Queued messages wait from now until the user next sends one.
    pub fn rewind_turn(
        &mut self,
        target_id: &str,
        at: RewindAt<'_>,
        mode: RewindMode,
        client_id: Option<String>,
        cx: &mut HostCx,
    ) -> Option<smol::channel::Receiver<AgentEvent>> {
        let active = self.resident(target_id)?;
        let checkpoint_id = self.rewind_checkpoint(active, &at, mode)?;
        self.begin_rewind_request(
            target_id,
            RewindRequest {
                checkpoint_id,
                mode,
                undo_token: None,
                client_id,
                waiters: Vec::new(),
            },
            cx,
        )
    }

    /// Ask the harness to undo the thread's latest rewind.
    pub fn undo_rewind(
        &mut self,
        target_id: &str,
        checkpoint_id: &str,
        cx: &mut HostCx,
    ) -> Option<smol::channel::Receiver<AgentEvent>> {
        let active = self.resident(target_id)?;
        let undo_token = self.rewind_undo_token(active, checkpoint_id)?;
        let mode = active
            .timeline
            .entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.content {
                EntryContent::Rewound(rewound) => Some(rewound.mode),
                _ => None,
            })?;
        self.begin_rewind_request(
            target_id,
            RewindRequest {
                checkpoint_id: checkpoint_id.to_owned(),
                mode,
                undo_token: Some(undo_token),
                client_id: None,
                waiters: Vec::new(),
            },
            cx,
        )
    }

    fn begin_rewind_request(
        &mut self,
        target_id: &str,
        mut request: RewindRequest,
        cx: &mut HostCx,
    ) -> Option<smol::channel::Receiver<AgentEvent>> {
        let active = self.resident_mut(target_id)?;
        let session_id = active.meta.id.clone();
        // What was queued when the rewind was asked for waits for the
        // user's next message; what is queued later only waits for the
        // rewind itself.
        if request.undo_token.is_none() && !active.queue.is_empty() {
            active.queue_held = true;
        }
        let live = match &active.runtime {
            Runtime::Live(commands) => Some(commands.clone()),
            Runtime::Idle | Runtime::Starting { .. } => None,
        };
        active.idle_since = None;
        let (waiter, answer) = smol::channel::bounded(1);
        request.waiters.push(waiter);
        let command = request.command();
        self.rewind_requests.insert(session_id.clone(), request);
        match live {
            Some(commands) => {
                if commands.try_send(command).is_err() {
                    self.settle_rewind_unanswered(
                        &session_id,
                        RewindFailureReason::ProcessGone,
                        cx,
                    );
                }
            }
            None => self.ensure_started(target_id, cx),
        }
        Some(answer)
    }

    /// What a rewind would restore, from the harness, without restoring
    /// anything. Starts an idle provider to ask it.
    pub fn preview_rewind(
        &mut self,
        target_id: &str,
        at: RewindAt<'_>,
        mode: RewindMode,
        cx: &mut HostCx,
    ) -> smol::channel::Receiver<RewindPreviewResult> {
        let (reply, answer) = smol::channel::bounded(1);
        let refused = |reason| RewindPreviewResult::Refused(RewindFailure::new(reason, None));
        let Some(active) = self.resident(target_id) else {
            let _ = reply.try_send(refused(RewindFailureReason::ProcessGone));
            return answer;
        };
        let previews = active
            .meta
            .provider
            .caps()
            .rewind
            .is_some_and(|control| control.preview != agent::RewindPreviewKind::None);
        let Some(checkpoint_id) = self
            .rewind_checkpoint(active, &at, mode)
            .filter(|_| previews)
        else {
            let _ = reply.try_send(refused(RewindFailureReason::Unsupported));
            return answer;
        };
        let session_id = active.meta.id.clone();
        let live = match &active.runtime {
            Runtime::Live(commands) => Some(commands.clone()),
            Runtime::Idle | Runtime::Starting { .. } => None,
        };
        let command = SessionCommand::PreviewRewind {
            checkpoint_id: checkpoint_id.clone(),
            mode,
        };
        self.rewind_previews
            .entry(session_id.clone())
            .or_default()
            .push(PreviewRequest {
                checkpoint_id,
                mode,
                reply,
            });
        match live {
            Some(commands) => {
                if commands.try_send(command).is_err() {
                    self.settle_previews(&session_id, RewindFailureReason::ProcessGone);
                }
            }
            None => self.ensure_started(target_id, cx),
        }
        answer
    }

    /// Send what waited for the provider to start. Returns whether a rewind
    /// went, which takes the place of the queue's next message.
    pub(super) fn send_waiting_rewinds(
        &mut self,
        session_id: &str,
        commands: &smol::channel::Sender<SessionCommand>,
        cx: &mut HostCx,
    ) -> bool {
        for preview in self.rewind_previews.get(session_id).into_iter().flatten() {
            let _ = commands.try_send(SessionCommand::PreviewRewind {
                checkpoint_id: preview.checkpoint_id.clone(),
                mode: preview.mode,
            });
        }
        let Some(command) = self
            .rewind_requests
            .get(session_id)
            .map(RewindRequest::command)
        else {
            return false;
        };
        if commands.try_send(command).is_err() {
            self.settle_rewind_unanswered(session_id, RewindFailureReason::ProcessGone, cx);
        }
        true
    }

    /// The provider is gone or never came: whatever waited on it fails. A
    /// rewind it had been asked for is recorded as failed in the thread.
    pub(super) fn settle_rewind_unanswered(
        &mut self,
        session_id: &str,
        reason: RewindFailureReason,
        cx: &mut HostCx,
    ) {
        self.settle_previews(session_id, reason);
        if let Some(request) = self.rewind_requests.get(session_id) {
            let event = request.failure(RewindFailure::new(reason, None));
            self.on_rewind_outcome(session_id, event, cx);
        }
    }

    fn settle_previews(&mut self, session_id: &str, reason: RewindFailureReason) {
        for preview in self
            .rewind_previews
            .remove(session_id)
            .into_iter()
            .flatten()
        {
            let _ = preview
                .reply
                .try_send(RewindPreviewResult::Refused(RewindFailure::new(
                    reason, None,
                )));
        }
    }

    /// Forget what waits on a provider shut down on purpose.
    pub(super) fn drop_rewind_requests(&mut self, session_id: &str) {
        self.rewind_requests.remove(session_id);
        self.settle_previews(session_id, RewindFailureReason::ProcessGone);
    }

    /// The harness answered a preview, a rewind or an undo.
    pub(super) fn on_rewind_outcome(
        &mut self,
        session_id: &str,
        event: AgentEvent,
        cx: &mut HostCx,
    ) {
        if let AgentEvent::RewindPreviewed {
            checkpoint_id,
            mode,
            result,
        } = event
        {
            let previews = self
                .rewind_previews
                .entry(session_id.to_owned())
                .or_default();
            previews.retain(|preview| {
                let answered = preview.checkpoint_id == checkpoint_id && preview.mode == mode;
                if answered {
                    let _ = preview.reply.try_send(result.clone());
                }
                !answered
            });
            if previews.is_empty() {
                self.rewind_previews.remove(session_id);
            }
            return;
        }
        let request = self.rewind_requests.remove(session_id);
        let mut event = event;
        if let AgentEvent::RewindCompleted {
            checkpoint_id,
            mode,
            prefill,
            prefill_attachments,
            ..
        } = &mut event
            && mode.includes_conversation()
            && let Some((text, attachments)) = self
                .resident(session_id)
                .and_then(|active| stored_prompt(&active.timeline, checkpoint_id))
        {
            // A Harness that answers without the prompt (Codex) has its own
            // client refill it from the transcript it keeps; Tcode refills it
            // from the one it stored. The images Tcode sent with it come back
            // whatever the Harness kept of them.
            prefill.get_or_insert(text);
            if prefill_attachments.is_empty() {
                *prefill_attachments = attachments;
            }
        }
        if let AgentEvent::RewindFailed { failure, .. }
        | AgentEvent::RewindUndoFailed { failure, .. } = &event
        {
            log::warn!(
                "rewind in {session_id} failed: {:?} {}",
                failure.reason,
                failure.detail.as_deref().unwrap_or_default()
            );
        }
        self.record_event(session_id, &event, cx);
        if let AgentEvent::RewindCompleted {
            prefill,
            prefill_attachments,
            mode,
            ..
        } = &event
            && mode.includes_conversation()
            && let Some(client_id) = request
                .as_ref()
                .and_then(|request| request.client_id.clone())
            && let Some(text) = prefill
        {
            self.emit_domain(
                Topic::SessionStatus {
                    session_id: session_id.to_owned(),
                },
                ServerEvent::RewindPrefill {
                    session_id: session_id.to_owned(),
                    client_id,
                    text: text.clone(),
                    attachments: prefill_attachments.clone(),
                },
                cx,
            );
        }
        if let Some(meta) = self.meta_mut(session_id) {
            meta.updated_at = now_secs();
            let meta = meta.clone();
            self.persist_meta(&meta, cx);
        }
        for waiter in request.into_iter().flat_map(|request| request.waiters) {
            let _ = waiter.try_send(event.clone());
        }
        // A rewind the harness did not make leaves the queue as it was.
        if matches!(event, AgentEvent::RewindFailed { .. })
            && let Some(resident) = self.resident_mut(session_id)
        {
            resident.queue_held = false;
        }
        if !self.residents.live.contains_key(session_id) {
            self.on_background_turn_completed(session_id, cx);
            return;
        }
        // Sends made while the rewind was made waited for it.
        if self.dispatch_next_queued(session_id, cx).is_err() {
            self.report_error(RuntimeError::ProcessGone, cx);
        }
        // A provider started only to rewind goes back to idle like one that
        // finished a turn, so its process is reaped after the grace.
        self.mark_resident_idle(session_id, cx);
    }
}

/// The words and images of the message the restore point `checkpoint_id`
/// stands before, as Tcode stored them: the prompt that opened its turn, or
/// the steer.
fn stored_prompt(
    timeline: &tcode_core::session::Timeline,
    checkpoint_id: &str,
) -> Option<(String, Vec<String>)> {
    let (turn, steer) = timeline
        .turns
        .iter()
        .enumerate()
        .find_map(|(index, turn)| {
            if turn.provider_checkpoint_id.as_deref() == Some(checkpoint_id) {
                return Some((index, None));
            }
            turn.steer_checkpoints
                .iter()
                .find(|(_, checkpoint)| checkpoint == checkpoint_id)
                .map(|(steer, _)| (index, Some(steer.as_str())))
        })?;
    timeline.entries.iter().find_map(|entry| {
        if entry.turn != turn || steer.is_some_and(|steer| entry.id != steer) {
            return None;
        }
        match &entry.content {
            EntryContent::Item(agent::ItemContent::UserMessage { text, attachments })
            | EntryContent::Steer {
                text, attachments, ..
            } => Some((text.clone(), attachments.clone())),
            _ => None,
        }
    })
}

#[cfg(test)]
#[path = "rewind_tests.rs"]
mod tests;
