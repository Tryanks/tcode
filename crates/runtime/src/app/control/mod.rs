//! The host side of `tcode_control`: dispatch of a thread's tool calls, the few things a
//! call is closed to whoever makes it, and the record every write leaves.

use super::*;
use control_mcp::{ControlOp, ThreadAccess};
use tcode_core::control::{ControlAction, ControlScope};
use tcode_core::settings::SettingsPatch;
use tcode_protocol::{
    ControlActivityEntry, ControlActivityFilter, ControlActivityPage, ControlActivityThread,
    ControlStatus,
};

mod content;
mod observe;
mod operate;
mod organize;
mod respond;
#[cfg(test)]
mod tests;
mod watches;

/// How many writes Settings → Control lists.
const CONTROL_ACTIVITY_LIMIT: usize = 200;

/// What one answer may hold, whatever the caller asks for, in bytes of JSON. Claude Code
/// refuses an MCP tool result over 25 000 tokens by default (`MAX_MCP_OUTPUT_TOKENS`), and
/// an answer made of thread ids, anchors and field names runs near two bytes a token: a
/// 64 KiB search answer of them was refused.
const RESPONSE_CAP: usize = 32 * 1024;

fn json_len<T: serde::Serialize>(value: &T) -> usize {
    serde_json::to_vec(value).map_or(0, |json| json.len())
}

pub(super) struct ControlState {
    activity: VecDeque<ControlActivityEntry>,
    activity_unreadable: Option<UnreadableFile>,
    /// Answers to recent calls that carried a `request_id`, so a retry does not act twice.
    answered: VecDeque<(String, serde_json::Value)>,
}

impl ControlState {
    pub(super) fn new(
        mut activity: Vec<ControlActivityEntry>,
        activity_unreadable: Option<UnreadableFile>,
    ) -> Self {
        activity.truncate(CONTROL_ACTIVITY_LIMIT);
        Self {
            activity: activity.into(),
            activity_unreadable,
            answered: VecDeque::new(),
        }
    }
}

/// A call's answer: ready on the host thread, or computed off it because it runs git, reads
/// the store or waits for work the call started, which would stall every client if awaited
/// on the host thread. A `Later` task is dropped once its caller goes.
pub(in crate::app) enum Answer {
    Now(Result<serde_json::Value, String>),
    Later(HostTask<Result<serde_json::Value, String>>),
}

/// The part of a call that needs the host state, run on the host thread once the call's
/// off-thread work has finished; its result is the call's answer. It runs in `call_cx`, the
/// context the call was given: the host's own carries no author, origin or space.
pub(in crate::app) async fn finish_on_host(
    call_cx: HostCx,
    finish: impl FnOnce(&mut AppState, &mut HostCx) -> Result<serde_json::Value, String>
    + Send
    + 'static,
) -> Result<serde_json::Value, String> {
    let (answered, answer) = smol::channel::bounded(1);
    let mut cx = call_cx.clone();
    call_cx.enqueue(move |state, _| {
        let _ = answered.try_send(finish(state, &mut cx));
    });
    answer
        .recv()
        .await
        .unwrap_or_else(|_| Err("the host stopped before the call finished".into()))
}

fn refusal(error: tcode_protocol::ProtocolError) -> String {
    format!("refused: {}", error.message)
}

/// The thread behind a `tcode_control` call.
#[derive(Clone)]
pub(in crate::app) struct Caller {
    /// The calling thread: this machine's, or `machine`'s when that is set, which then
    /// names no thread here.
    pub(in crate::app) thread_id: String,
    pub(in crate::app) project_id: Option<String>,
    /// The space whose member's turn makes the call, which confines it to that space's
    /// projects and policy.
    pub(in crate::app) space: Option<CallerSpace>,
    pub(in crate::app) machine: Option<RemoteCaller>,
}

/// The machine of the group a calling thread is on, when it is not this one.
#[derive(Clone)]
pub(in crate::app) struct RemoteCaller {
    pub(in crate::app) peer: super::machines::MachinePeer,
    pub(in crate::app) title: String,
    pub(in crate::app) approval: Option<agent::ApprovalClass>,
}

#[derive(Clone)]
pub(in crate::app) struct CallerSpace {
    pub(super) projects: BTreeSet<String>,
    pub(super) policy: tcode_protocol::SpacePolicy,
}

impl CallerSpace {
    pub(super) fn holds(&self, project_id: Option<&str>) -> bool {
        project_id.is_some_and(|id| self.projects.contains(id))
    }
}

impl Caller {
    /// Whether a call may name `project_id`: any project, or one of the caller's space when
    /// a member's turn makes the call.
    pub(super) fn holds_project(&self, project_id: &str) -> bool {
        self.space
            .as_ref()
            .is_none_or(|space| space.holds(Some(project_id)))
    }

    /// Whether a listing or search covering `scope` includes `project_id`.
    pub(super) fn covers_project(&self, scope: &ControlScope, project_id: &str) -> bool {
        scope.reaches(self.project_id.as_deref(), Some(project_id))
            && self.holds_project(project_id)
    }
}

impl AppState {
    pub fn attach_control_mcp(&mut self, server: control_mcp::ControlMcpServer) {
        self.mcp.control_url = Some(server.url);
        self.mcp.control_tokens = Some(server.tokens);
        self.mcp.control_requests = Some(server.requests);
        let access = self.mcp.control_thread_access.clone();
        self.mcp.control_source = Some(server.access.clone());
        server.access.set(Arc::new(move |thread_id| {
            access.lock().unwrap().get(thread_id).copied()
        }));
    }

    /// Taking the receiver makes repeated calls harmless: exactly one pump owns the stream.
    pub fn pump_control_requests(&mut self, cx: &mut HostCx) {
        let Some(requests) = self.mcp.control_requests.take() else {
            return;
        };
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            while let Ok(request) = requests.recv().await {
                let control_mcp::BrokerRequest {
                    thread_id,
                    op,
                    machine,
                    reply,
                } = request;
                host_cx.enqueue(move |state, cx| {
                    state.handle_control_call(thread_id, op, machine, reply, cx)
                });
            }
        });
    }

    /// What the control server offers `meta`'s thread: nothing with the feature off,
    /// otherwise every tool and, for a subagent, `report_result`, for the thread the
    /// user opened to research a model's guide, `set_model_guide`, and the tools about the
    /// group's machines where this machine reaches a group and lets its threads do so.
    pub(super) fn control_access(&self, meta: &SessionMeta) -> ThreadAccess {
        ThreadAccess {
            enabled: self.settings.control.enabled,
            subagent: meta.has_lead(),
            model_guide: meta.guide_for.is_some(),
            machines: self.settings.control.machines && self.machine_link().is_ok(),
        }
    }

    /// Whether `target` is a subagent `caller` started: here, or, for a caller on another
    /// machine, on this machine for that thread.
    pub(in crate::app) fn own_subagent(&self, caller: &Caller, target: &SessionMeta) -> bool {
        match &caller.machine {
            None => {
                target.is_dispatched()
                    && target.parent_session_id.as_deref() == Some(&caller.thread_id)
            }
            Some(remote) => target.remote_parent.as_ref().is_some_and(|parent| {
                parent.machine == remote.peer.id && parent.thread_id == caller.thread_id
            }),
        }
    }

    /// Whether the thread's provider is given the `tcode_control` server when it starts.
    pub(super) fn control_offered(&self, meta: &SessionMeta) -> bool {
        self.settings.control.enabled
            && self.mcp.control_url.is_some()
            && meta.provider.caps().mcp_servers
    }

    pub(super) fn control_registration_for(
        &mut self,
        meta: &SessionMeta,
    ) -> Option<agent::McpRegistration> {
        if !self.control_offered(meta) {
            return None;
        }
        if let Some(registration) = self.mcp.control_registrations.get(&meta.id) {
            return Some(registration.clone());
        }
        let token = self.mcp.control_tokens.as_ref()?.register(&meta.id);
        let registration = agent::McpRegistration {
            name: agent::McpRegistration::SERVER_NAME_CONTROL.into(),
            url: self.mcp.control_url.clone()?,
            bearer_token: token,
        };
        self.mcp
            .control_registrations
            .insert(meta.id.clone(), registration.clone());
        self.mcp
            .control_thread_access
            .lock()
            .unwrap()
            .insert(meta.id.clone(), self.control_access(meta));
        Some(registration)
    }

    pub(super) fn revoke_control_registration(&mut self, session_id: &str) {
        if let Some(registration) = self.mcp.control_registrations.remove(session_id)
            && let Some(tokens) = &self.mcp.control_tokens
        {
            tokens.revoke(&registration.bearer_token);
        }
        self.mcp
            .control_thread_access
            .lock()
            .unwrap()
            .remove(session_id);
        if let Some(source) = &self.mcp.control_source {
            source.forget(session_id);
        }
    }

    /// Re-read every registered thread's access after the setting changed, so the next
    /// connection lists the right tools; with the feature off a registered thread is offered
    /// none.
    pub(in crate::app) fn refresh_control_access(&mut self) {
        let registered: Vec<String> = self.mcp.control_registrations.keys().cloned().collect();
        let access: Vec<(String, ThreadAccess)> = registered
            .into_iter()
            .filter_map(|id| {
                let meta = self.find_meta(&id)?;
                Some((id, self.control_access(&meta)))
            })
            .collect();
        let mut table = self.mcp.control_thread_access.lock().unwrap();
        for (id, access) in access {
            table.insert(id, access);
        }
    }

    pub(super) fn control_status(&self, meta: &SessionMeta) -> Option<ControlStatus> {
        if !meta.provider.caps().mcp_servers {
            return None;
        }
        Some(ControlStatus {
            self_settle: meta.self_settle.clone(),
            tools_offered: self.control_offered(meta),
        })
    }

    /// `meta`'s thread as a `tcode_control` caller.
    pub(super) fn caller_of(&self, meta: &SessionMeta) -> Caller {
        let space = self.turn_space_of(&meta.id).map(|id| CallerSpace {
            projects: self.space_scopes.get(&id).cloned().unwrap_or_default(),
            policy: self
                .turn_policy(&meta.id)
                .cloned()
                .unwrap_or_else(tcode_protocol::SpacePolicy::new_space),
        });
        Caller {
            thread_id: meta.id.clone(),
            project_id: meta.project_id.clone(),
            space,
            machine: None,
        }
    }

    /// Whether a call may name `target`: any thread on the host, or, in a member's turn,
    /// one inside the space.
    pub(super) fn open_to(&self, caller: &Caller, target: &SessionMeta) -> bool {
        caller
            .space
            .as_ref()
            .is_none_or(|space| space.holds(target.project_id.as_deref()))
    }

    /// Whether a listing or search covering `scope` includes `target`.
    pub(super) fn covered(
        &self,
        caller: &Caller,
        scope: &ControlScope,
        target: &SessionMeta,
    ) -> bool {
        scope.reaches(caller.project_id.as_deref(), target.project_id.as_deref())
            && self.open_to(caller, target)
    }

    /// Resolve one `tcode_control` call on the host thread, for `machine` of the group when
    /// it names one.
    pub(crate) fn handle_control_call(
        &mut self,
        thread_id: String,
        op: ControlOp,
        machine: Option<String>,
        reply: smol::channel::Sender<Result<serde_json::Value, String>>,
        cx: &mut HostCx,
    ) {
        let mut authored_cx = cx.clone();
        authored_cx.origin = Some(MessageOrigin::Agent);
        authored_cx.author = None;
        authored_cx.space = self.turn_space_of(&thread_id);
        let cx = &mut authored_cx;
        let answer = match self.find_meta(&thread_id) {
            _ if !self.settings.control.enabled => Answer::Now(Err(
                "tcode_control is turned off in Settings → Control".into(),
            )),
            Some(meta) if meta.archived_at.is_some() => {
                Answer::Now(Err("this thread is archived".into()))
            }
            Some(meta) => {
                let caller = self.caller_of(&meta);
                match machine {
                    // The ids it names are the other machine's, which this one does not
                    // check.
                    Some(machine) => self.forward_control_op(&caller, machine, op, cx),
                    None => match self.authorize_control(&caller, &op) {
                        Ok(()) => self.dispatch_control_op(&caller, op, cx),
                        Err(refusal) => Answer::Now(Err(refusal)),
                    },
                }
            }
            None => Answer::Now(Err("unknown thread".into())),
        };
        self.answer_control_call(answer, reply, cx);
    }

    pub(in crate::app) fn answer_control_call(
        &mut self,
        answer: Answer,
        reply: smol::channel::Sender<Result<serde_json::Value, String>>,
        cx: &mut HostCx,
    ) {
        match answer {
            Answer::Now(result) => {
                let _ = reply.try_send(result);
            }
            // A caller that has gone, its call cancelled or its harness
            // stopped, takes the work on its answer with it.
            Answer::Later(task) => HostCx::spawn_detached(cx, async move {
                let answered = smol::future::or(async { Some(task.await) }, async {
                    reply.closed().await;
                    None
                })
                .await;
                if let Some(answer) = answered {
                    let _ = reply.send(answer).await;
                }
            }),
        }
    }

    /// The answer an earlier call with this `key` got, if it is still remembered.
    pub(super) fn answered(&self, key: &str) -> Option<serde_json::Value> {
        self.control
            .answered
            .iter()
            .find(|(answered, _)| answered == key)
            .map(|(_, value)| value.clone())
    }

    pub(super) fn remember_answer(&mut self, key: String, value: serde_json::Value) {
        self.control.answered.push_front((key, value));
        self.control.answered.truncate(CONTROL_ACTIVITY_LIMIT);
    }

    /// What every call passes host-side. A call is closed only where no thread's model may
    /// go: what a space withholds from its members' turns, the settings no agent changes, and
    /// a target that is unknown, the caller's own Orchestrate ancestor or outside its space.
    /// Whether a call that is open should be made is the harness's permission mode to settle.
    pub(in crate::app) fn authorize_control(
        &self,
        caller: &Caller,
        op: &ControlOp,
    ) -> Result<(), String> {
        if let Some(space) = &caller.space {
            self.authorize_space_control(space, op)?;
        }
        if let ControlOp::PatchSettings(params) = op {
            let patch: SettingsPatch = serde_json::from_value(params.patch.clone())
                .map_err(|error| format!("invalid settings patch: {error}"))?;
            if operate::settings_patch_never_exposed(&patch) {
                let key = params
                    .patch
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                return Err(format!(
                    "refused: `patch_settings` never changes {key}: an agent may change only \
                     display preferences (language, theme, diff word wrap, panels, sidebar \
                     provider marks, frame throttling, scrolling to the latest on send, \
                     shortening long commands)"
                ));
            }
        }
        if op.answers_per_target() {
            return Ok(());
        }
        for target in op.target_thread_ids() {
            self.check_target(caller, target)
                .map_err(|refusal| refusal.whole_call(target))?;
        }
        Ok(())
    }

    /// Why `target` is closed to `caller`, or its meta when it is open.
    fn check_target(&self, caller: &Caller, target: &str) -> Result<SessionMeta, TargetRefusal> {
        let Some(meta) = self.find_meta(target) else {
            return Err(match self.moved_thread_machine(target) {
                Some(machine) => TargetRefusal::Moved(machine.to_owned()),
                None => TargetRefusal::Unknown,
            });
        };
        if !self.open_to(caller, &meta) {
            return Err(TargetRefusal::OutsideSpace);
        }
        Ok(meta)
    }

    /// One target of a per-item batch ([`ControlOp::answers_per_target`]): its meta, or the
    /// reason that item is skipped.
    pub(super) fn batch_target(
        &self,
        caller: &Caller,
        target: &str,
    ) -> Result<SessionMeta, String> {
        self.check_target(caller, target)
            .map_err(|refusal| refusal.per_item())
    }

    pub(in crate::app) fn dispatch_control_op(
        &mut self,
        caller: &Caller,
        op: ControlOp,
        cx: &mut HostCx,
    ) -> Answer {
        let now = match op {
            ControlOp::ListThreads(p) => return self.op_list_threads(caller, p, cx),
            ControlOp::Search(p) => return self.op_search(caller, p, cx),
            ControlOp::GetGit(p) => return self.op_get_git(caller, p, cx),
            ControlOp::ReviewCandidates(p) => return self.op_review_candidates(caller, p, cx),
            ControlOp::UpdateThreads(p) => return self.op_update_threads(caller, p, cx),
            ControlOp::ListProjects(p) => self.op_list_projects(caller, p, cx),
            ControlOp::GetThread(p) => self.op_get_thread(caller, p, cx),
            ControlOp::Outline(p) => return self.op_outline(caller, p, cx),
            ControlOp::Read(p) => return self.op_read(caller, p, cx),
            ControlOp::SearchInItem(p) => return self.op_search_in_item(caller, p, cx),
            ControlOp::FileHistory(p) => return self.op_file_history(caller, p, cx),
            ControlOp::GetPullRequest(p) => self.op_get_pull_request(caller, p, cx),
            ControlOp::ListProviders(p) => self.op_list_providers(caller, p, cx),
            ControlOp::GetSettings(p) => self.op_get_settings(caller, p, cx),
            ControlOp::ReadTerminal(p) => self.op_read_terminal(caller, p, cx),
            ControlOp::SettleSelf(p) => self.op_settle_self(caller, p, cx),
            ControlOp::StartThread(p) => return self.op_start_thread(caller, p, cx),
            ControlOp::SendMessage(p) => self.op_send_message(caller, p, cx),
            ControlOp::ControlThread(p) => self.op_control_thread(caller, p, cx),
            ControlOp::PullRequest(p) => return self.op_pull_request(caller, p, cx),
            ControlOp::Git(p) => return self.op_git(caller, p, cx),
            ControlOp::CreateProject(p) => self.op_create_project(caller, p, cx),
            ControlOp::MoveThread(p) => self.op_move_thread(caller, p, cx),
            ControlOp::NotifyWhen(p) => self.op_notify_when(caller, p, cx),
            ControlOp::CancelNotify(p) => self.op_cancel_notify(caller, p, cx),
            ControlOp::RespondInput(p) => self.op_respond_input(caller, p, cx),
            ControlOp::RespondApproval(p) => self.op_respond_approval(caller, p, cx),
            ControlOp::ReportResult(p) => self.op_report_result(caller, p),
            ControlOp::Terminal(p) => self.op_terminal(caller, p, cx),
            ControlOp::DeleteThreads(p) => return self.op_delete_threads(caller, p, cx),
            ControlOp::Rewind(p) => return self.op_rewind(caller, p, cx),
            ControlOp::PatchSettings(p) => self.op_patch_settings(caller, p, cx),
            ControlOp::SetModelGuide(p) => self.op_set_model_guide(caller, p, cx),
            ControlOp::ListMachines(p) => return self.op_list_machines(caller, p, cx),
            ControlOp::StartThreadOnMachine(p) => {
                return self.op_start_thread_on_machine(caller, p, cx);
            }
        };
        Answer::Now(now)
    }

    /// Record a write in the target thread, as the actor's thread, and in the activity list.
    /// A caller on another machine is named with its machine here, and its own log is that
    /// machine's to write.
    pub(in crate::app) fn audit(
        &mut self,
        caller: &Caller,
        target: &str,
        action: &str,
        detail: Option<String>,
        cx: &mut HostCx,
    ) {
        let actor_title = match &caller.machine {
            Some(remote) => remote.title.clone(),
            None => self
                .find_meta(&caller.thread_id)
                .map(|meta| meta.title)
                .unwrap_or_default(),
        };
        let target_title = self
            .find_meta(target)
            .map(|meta| meta.title)
            .unwrap_or_default();
        let action = ControlAction {
            actor_thread_id: caller.thread_id.clone(),
            actor_title,
            actor_machine: caller.machine.as_ref().map(|remote| remote.peer.id.clone()),
            action: action.to_owned(),
            detail,
        };
        let at = cx.clock().unix_millis();
        self.record_event(target, &AgentEvent::ControlAction(action.clone()), cx);
        if caller.machine.is_none() && target != caller.thread_id {
            let in_caller = ControlAction {
                detail: Some(match &action.detail {
                    Some(detail) => format!("{target_title} — {detail}"),
                    None => target_title.clone(),
                }),
                ..action.clone()
            };
            self.record_event(&caller.thread_id, &AgentEvent::ControlAction(in_caller), cx);
        }
        self.control.activity.push_front(ControlActivityEntry {
            target_thread_id: target.to_owned(),
            target_title,
            at,
            action,
        });
        self.control.activity.truncate(CONTROL_ACTIVITY_LIMIT);
        if let Some(unreadable) = &mut self.control.activity_unreadable {
            if let Some(error) = unreadable.first_refusal() {
                self.report_error(RuntimeError::FileUnreadable { error }, cx);
            }
            return;
        }
        let entries = self.control.activity.iter().cloned().collect();
        self.enqueue_store_write(StoreWrite::WriteControlActivity(entries), cx);
    }

    pub(crate) fn control_activity(
        &self,
        filter: &ControlActivityFilter,
        limit: usize,
    ) -> ControlActivityPage {
        let log = &self.control.activity;
        let matching = || log.iter().filter(|entry| filter.matches(entry));
        let mut threads = Vec::<ControlActivityThread>::new();
        let mut actions = Vec::<String>::new();
        for entry in log {
            for (id, title) in [
                (&entry.target_thread_id, &entry.target_title),
                (&entry.action.actor_thread_id, &entry.action.actor_title),
            ] {
                if !threads.iter().any(|thread| thread.id == *id) {
                    threads.push(ControlActivityThread {
                        id: id.clone(),
                        title: title.clone(),
                    });
                }
            }
            if !actions.contains(&entry.action.action) {
                actions.push(entry.action.action.clone());
            }
        }
        ControlActivityPage {
            entries: matching().take(limit).cloned().collect(),
            total: matching().count() as u32,
            threads,
            actions,
        }
    }

    /// Write a changed meta through to the resident copy, the index and the store.
    pub(in crate::app) fn store_meta(&mut self, meta: SessionMeta, cx: &mut HostCx) {
        if let Some(live) = self.meta_mut(&meta.id) {
            *live = meta.clone();
        }
        self.persist_meta(&meta, cx);
    }
}

/// A target closed to the caller; worded for a whole-call refusal or a per-item skip.
enum TargetRefusal {
    Unknown,
    OutsideSpace,
    /// The thread moved with its project to this machine of the group.
    Moved(String),
}

impl TargetRefusal {
    fn whole_call(&self, target: &str) -> String {
        match self {
            Self::Unknown => format!("unknown thread: {target}"),
            Self::Moved(machine) => {
                format!("thread {target} moved to machine {machine} of the group")
            }
            Self::OutsideSpace => {
                format!("refused: thread {target} is outside the space this turn runs in")
            }
        }
    }

    fn per_item(&self) -> String {
        match self {
            Self::Unknown => "unknown_thread".to_owned(),
            Self::Moved(machine) => format!("thread_moved: to machine {machine}"),
            Self::OutsideSpace => "outside_space: this turn runs in a space".to_owned(),
        }
    }
}

impl AppState {
    /// What a call made during a member's turn may not do: what a member could not do from
    /// their own device, and what their space's policy withholds. Which threads it reaches
    /// is confined in [`Self::open_to`].
    fn authorize_space_control(&self, space: &CallerSpace, op: &ControlOp) -> Result<(), String> {
        let policy = &space.policy;
        let name = op.tool_name();
        let refused = |why: &str| Err(format!("refused: `{name}` {why} in this space"));
        match op {
            ControlOp::GetSettings(_)
            | ControlOp::PatchSettings(_)
            | ControlOp::SetModelGuide(_)
            | ControlOp::CreateProject(_)
            | ControlOp::DeleteThreads(_) => refused("is not available"),
            // Spaces are this machine's: a member's turn reaches no other machine.
            ControlOp::ListMachines(_) | ControlOp::StartThreadOnMachine(_) => {
                refused("reaches no other machine")
            }
            ControlOp::Terminal(_) | ControlOp::ReadTerminal(_) if !policy.terminals => {
                refused("needs terminals, which the space does not allow")
            }
            ControlOp::PullRequest(_) | ControlOp::GetPullRequest(_) if !policy.git_remote => {
                refused("needs pull requests, which the space does not allow")
            }
            ControlOp::Git(params)
                if git_action(&params.op).is_some_and(GitAction::remote) && !policy.git_remote =>
            {
                refused("needs pushing and pulling, which the space does not allow")
            }
            ControlOp::MoveThread(params) if !space.holds(Some(&params.project_id)) => {
                refused("cannot move a thread out of the space's projects")
            }
            ControlOp::StartThread(params)
                if params
                    .project_id
                    .as_deref()
                    .is_some_and(|project| !space.holds(Some(project))) =>
            {
                refused("cannot start a thread outside the space's projects")
            }
            ControlOp::StartThread(params) => {
                let provider = super::orchestrate::provider_named(&params.provider)
                    .ok_or_else(|| format!("unknown provider: {}", params.provider.trim()))?;
                let profile = params
                    .profile
                    .as_deref()
                    .map(str::trim)
                    .filter(|id| !id.is_empty());
                self.model_allowed(policy, provider, profile, params.model.as_deref())
                    .map_err(refusal)
            }
            ControlOp::RespondApproval(params)
                if policy.approval == tcode_protocol::SpaceApproval::ReviewedOnly =>
            {
                let plan = params.approval_id.as_deref().is_some_and(|id| {
                    self.approval_requests(&params.thread_id)
                        .iter()
                        .any(|request| {
                            request.id == id
                                && matches!(request.kind, agent::ApprovalKind::Plan { .. })
                        })
                });
                if plan {
                    Ok(())
                } else {
                    refused("cannot answer an approval: the agent's harness settles them itself")
                }
            }
            ControlOp::ControlThread(params) => {
                use control_mcp::params::RunOp;
                for change in &params.changes {
                    match &change.op {
                        RunOp::SetModel { model } => {
                            if let Some(meta) = self.current_meta(&change.thread_id) {
                                self.model_allowed(
                                    policy,
                                    meta.provider,
                                    meta.profile_id.as_deref(),
                                    Some(model),
                                )
                                .map_err(refusal)?;
                            }
                        }
                        // Each runs the thread's model, as a message would.
                        RunOp::Retry { .. } | RunOp::Compact | RunOp::SteerQueued { .. } => self
                            .session_setup_allowed(policy, &change.thread_id)
                            .map_err(refusal)?,
                        _ => {}
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

/// The git quick action a control `git` op runs, for the ops that are one.
fn git_action(op: &control_mcp::GitOp) -> Option<GitAction> {
    use control_mcp::GitOp;
    match op {
        GitOp::Commit { .. } => Some(GitAction::Commit),
        GitOp::CommitPush { .. } => Some(GitAction::CommitPush),
        GitOp::Push => Some(GitAction::Push),
        GitOp::Pull => Some(GitAction::Pull),
        GitOp::PublishBranch => Some(GitAction::PublishBranch),
        GitOp::CheckoutBranch { .. } | GitOp::MergeWorktree => None,
    }
}
