//! One thread answering what another asked or owes: a question put to the user, a
//! subagent's approval, and a subagent's report to the thread that started it.

use super::*;
use crate::app::orchestrate::resolve_approval_decision;
use control_mcp::{ReportResultParams, RespondApprovalParams, RespondInputParams};
use tcode_core::control::action;
use tcode_core::settings::ChildApprovalMode;
use tcode_protocol::Command;

impl AppState {
    pub(super) fn op_respond_input(
        &mut self,
        caller: &Caller,
        params: RespondInputParams,
        cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        let target = params.thread_id;
        let pending = self
            .resident(&target)
            .and_then(|session| session.timeline.pending_user_input.as_ref());
        let (request_id, questions) = match (params.question_id, pending) {
            (Some(asked), Some(pending)) if asked == pending.request_id => {
                (asked, &pending.questions)
            }
            (None, Some(pending)) => (pending.request_id.clone(), &pending.questions),
            (Some(asked), _) => {
                return Err(format!(
                    "refused: request {asked} is not pending in thread {target}"
                ));
            }
            (None, None) => {
                return Err(format!(
                    "refused: thread {target} has no pending user-input request"
                ));
            }
        };
        let ids: Vec<&str> = questions
            .iter()
            .map(|question| question.id.as_str())
            .collect();
        if let Some(unknown) = params
            .answers
            .keys()
            .find(|key| !ids.contains(&key.as_str()))
        {
            return Err(format!(
                "unknown question id {unknown:?}; the question ids are {ids:?}"
            ));
        }
        let missing: Vec<&str> = ids
            .iter()
            .filter(|id| !params.answers.contains_key(**id))
            .copied()
            .collect();
        if !missing.is_empty() {
            return Err(format!("missing answers for question ids {missing:?}"));
        }
        let command = Command::RespondUserInput {
            session_id: target.clone(),
            request_id: request_id.clone(),
            answers: params.answers.clone(),
        };
        self.validate_command_target(&command).map_err(refusal)?;
        self.respond_user_input(&target, request_id.clone(), params.answers.clone(), cx)
            .map_err(refusal)?;
        self.audit(
            caller,
            &target,
            action::RESPOND_INPUT,
            Some(request_id.clone()),
            cx,
        );
        Ok(serde_json::json!({ "thread_id": target, "request_id": request_id, "answered": true }))
    }

    /// The lead answers its own subagent's approval, where Settings → Control child
    /// approval makes it the approver; every other approval is the user's.
    pub(super) fn op_respond_approval(
        &mut self,
        caller: &Caller,
        params: RespondApprovalParams,
        cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        let target = params.thread_id;
        if !self
            .find_meta(&target)
            .is_some_and(|meta| self.own_subagent(caller, &meta))
        {
            return Err(format!(
                "refused: thread {target} is not your subagent; its approvals are the user's"
            ));
        }
        if self.settings.subagents.child_approval == ChildApprovalMode::Manual {
            return Err(
                "refused: Settings → Control subagent approvals is manual, so the user answers \
                 your subagents' approvals"
                    .into(),
            );
        }
        let pending = self.approval_requests(&target);
        let request = match params.approval_id {
            Some(id) => pending
                .iter()
                .find(|request| request.id == id)
                .cloned()
                .ok_or_else(|| {
                    format!("refused: approval {id} is not pending in thread {target}")
                })?,
            None => match pending {
                [request] => request.clone(),
                [] => return Err(format!("refused: thread {target} has no pending approval")),
                _ => {
                    return Err(format!(
                        "thread {target} has several pending approvals; approval_id is required"
                    ));
                }
            },
        };
        let decision =
            resolve_approval_decision(params.option.as_deref(), params.cancel, &request)?;
        self.respond_session_approval(&target, request.id.clone(), decision)?;
        self.audit(
            caller,
            &target,
            action::RESPOND_APPROVAL,
            Some(request.id.clone()),
            cx,
        );
        Ok(serde_json::json!({ "thread_id": target, "approval_id": request.id, "answered": true }))
    }

    /// Nothing is stored: the report is read back from this call's own tool item when the
    /// turn ends (`Timeline::last_report`), so the transcript is the one copy.
    pub(super) fn op_report_result(
        &mut self,
        _caller: &Caller,
        params: ReportResultParams,
    ) -> Result<serde_json::Value, String> {
        Ok(serde_json::json!({
            "ok": true,
            "chars": params.text.chars().count(),
            "delivered_when": "turn_ends",
        }))
    }
}
