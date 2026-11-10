//! Tcode's task tools (`tcode_tasks`) for threads whose harness has no
//! background commands of its own ([`agent::Caps::native_background`]): the
//! host authorizes and starts each command, lists the ones running in the
//! background with the harness's own background work, and wakes the thread
//! when one ends.
//!
//! The commands run with Tcode's privileges, outside whatever sandbox or
//! approval rules the harness applies to its own shell. So the thread's
//! permission mode decides here: a thread whose mode already runs every
//! command unasked runs these unasked too; any other mode asks the user
//! through Tcode's approval UI, as the harness would, and a member's turn in
//! a space that answers no approvals is declined at once.

use super::send::provider_command_error;
use super::*;
use agent::{ApprovalClass, ApprovalOption, ApprovalOptionKind, is_task_run};
use tasks_mcp::{Mover, Phase, Spawn, TaskEvent, TaskInfo, TaskManager, Window};
use tcode_core::notice::CONTROL_CALLBACK_PREFIX;
use tcode_protocol::SpaceApproval;

/// How long a wake waits for other tasks of its thread ending close behind,
/// to report them in one message.
const WAKE_COALESCE: Duration = Duration::from_millis(1500);
/// How much of a task's output a wake quotes.
const WAKE_TAIL_LINES: usize = 15;
const WAKE_TAIL_BYTES: usize = 2000;
/// The request ids of the approvals Tcode asks for its own task tools.
const APPROVAL_PREFIX: &str = "tcode-task-";
const ALLOW_ONCE: &str = "run";
const ALLOW_THREAD: &str = "allow-thread";
const DECLINE: &str = "decline";

/// A command waiting on the user's approval.
pub(super) struct PendingTaskApproval {
    session_id: String,
    spawn: Spawn,
    space: Option<String>,
    reply: async_channel::Sender<Result<TaskInfo, String>>,
}

/// The task tools' wiring, and what the host keeps of their tasks.
#[derive(Default)]
pub(super) struct TaskWiring {
    url: Option<String>,
    tokens: Option<tasks_mcp::TokenRegistry>,
    registrations: HashMap<String, agent::McpRegistration>,
    manager: Option<TaskManager>,
    approvals: HashMap<String, PendingTaskApproval>,
    /// Threads whose user allowed every task command for the rest of the
    /// host's run.
    allowed: HashSet<String>,
    /// Ended tasks of each thread waiting for their wake, in order.
    wakes: HashMap<String, Vec<String>>,
    /// The space each task's turn ran for, which its wake runs under.
    spaces: HashMap<(String, String), String>,
}

/// What the thread's permission mode makes of a task command.
#[derive(Debug, PartialEq, Eq)]
enum Admission {
    Run,
    Ask,
    Decline(String),
}

fn task_ended(task: &TaskInfo) -> String {
    match &task.phase {
        Phase::Exited { .. } if task.timed_out => "timed out and was stopped".to_owned(),
        Phase::Exited { .. } if task.stopped => "was stopped".to_owned(),
        Phase::Exited {
            exit_code: Some(code),
            ..
        } => format!("exited with code {code}"),
        Phase::Exited {
            signal: Some(signal),
            ..
        } => format!("was ended by signal {signal}"),
        Phase::Exited { .. } => "exited".to_owned(),
        Phase::Lost => "was lost when Tcode restarted".to_owned(),
        Phase::Running => "is running".to_owned(),
    }
}

fn duration_text(ms: u64) -> String {
    let secs = ms / 1000;
    match secs {
        0..=59 => format!("{}.{}s", secs, (ms % 1000) / 100),
        60..=3599 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3600, (secs % 3600) / 60),
    }
}

impl AppState {
    /// Take the task tools' server: its tasks are kept under the data dir.
    pub(crate) fn attach_tasks_mcp(
        &mut self,
        server: Option<tasks_mcp::TasksMcpServer>,
        cx: &mut HostCx,
    ) {
        let Some(server) = server else { return };
        let (manager, events) = TaskManager::open(self.store.root().join("tasks"));
        let _ = server.manager.set(manager.clone());
        self.mcp.tasks.url = Some(server.url);
        self.mcp.tasks.tokens = Some(server.tokens);
        self.mcp.tasks.manager = Some(manager);
        let host = cx.clone();
        let requests = server.requests;
        cx.spawn_detached(async move {
            while let Ok(request) = requests.recv().await {
                host.enqueue(move |state, cx| state.handle_task_request(request, cx));
            }
        });
        let host = cx.clone();
        cx.spawn_detached(async move {
            while let Ok(event) = events.recv().await {
                host.enqueue(move |state, cx| state.on_task_event(event, cx));
            }
        });
    }

    /// Whether a provider of this kind receives the task tools when it starts.
    pub(super) fn task_tools_offered(&self, provider: ProviderKind) -> bool {
        !provider.caps().native_background && self.mcp.tasks.url.is_some()
    }

    pub(super) fn task_tools_registered(&self, session_id: &str) -> bool {
        self.mcp.tasks.registrations.contains_key(session_id)
    }

    pub(super) fn tasks_registration_for(
        &mut self,
        meta: &SessionMeta,
    ) -> Option<agent::McpRegistration> {
        if let Some(registration) = self.mcp.tasks.registrations.get(&meta.id) {
            return Some(registration.clone());
        }
        let registration = agent::McpRegistration {
            name: agent::McpRegistration::SERVER_NAME_TASKS.into(),
            url: self.mcp.tasks.url.clone()?,
            bearer_token: self.mcp.tasks.tokens.as_ref()?.register(&meta.id),
        };
        self.mcp
            .tasks
            .registrations
            .insert(meta.id.clone(), registration.clone());
        Some(registration)
    }

    /// The thread's provider session is gone: its token goes, and any command
    /// a call of that session still waited on is stopped, as a cancelled call
    /// stops it. Its background tasks go on.
    pub(super) fn revoke_tasks_registration(&mut self, session_id: &str) {
        if let Some(registration) = self.mcp.tasks.registrations.remove(session_id)
            && let Some(tokens) = &self.mcp.tasks.tokens
        {
            tokens.revoke(&registration.bearer_token);
        }
        self.stop_foreground_tasks(session_id);
    }

    /// Stop the commands a `task_run` call of the thread still waits on.
    pub(super) fn stop_foreground_tasks(&mut self, session_id: &str) {
        if let Some(manager) = &self.mcp.tasks.manager {
            manager.stop_all(session_id, true);
        }
        self.drop_task_approvals(session_id);
    }

    /// The thread is archived: its tasks are stopped.
    pub(super) fn stop_thread_tasks(&mut self, session_id: &str) {
        if let Some(manager) = &self.mcp.tasks.manager {
            manager.stop_all(session_id, false);
        }
        self.drop_task_approvals(session_id);
    }

    /// The thread is deleted: its tasks are killed and their output removed.
    pub(super) fn forget_thread_tasks(&mut self, session_id: &str) {
        if let Some(manager) = &self.mcp.tasks.manager {
            manager.forget(session_id);
        }
        self.drop_task_approvals(session_id);
        self.mcp.tasks.allowed.remove(session_id);
        self.mcp.tasks.wakes.remove(session_id);
        self.mcp
            .tasks
            .spaces
            .retain(|(session, _), _| session != session_id);
    }

    /// The thread's tasks running in the background, as its background work
    /// lists them.
    pub(super) fn host_background_tasks(&self, session_id: &str) -> Vec<agent::BackgroundTask> {
        let Some(manager) = &self.mcp.tasks.manager else {
            return Vec::new();
        };
        manager
            .background(session_id)
            .into_iter()
            .map(|task| agent::BackgroundTask {
                id: task.task_id,
                name: task.command,
                kind: agent::BackgroundKind::Command,
                item_id: task.item_id,
                started_at: task.started_at,
                started_reported: true,
            })
            .collect()
    }

    pub(super) fn host_tasks_running(&self, session_id: &str) -> bool {
        self.mcp
            .tasks
            .manager
            .as_ref()
            .is_some_and(|manager| !manager.background(session_id).is_empty())
    }

    /// The user moves the command the call `item_id` waits on to the
    /// background.
    pub(super) fn move_host_task(
        &mut self,
        session_id: &str,
        item_id: &str,
    ) -> Result<(), ProtocolError> {
        let manager = self
            .mcp
            .tasks
            .manager
            .as_ref()
            .ok_or_else(|| provider_command_error("Tcode's task tools are not running."))?;
        let task = manager
            .foreground_for_item(session_id, item_id)
            .ok_or_else(|| provider_command_error("That call runs no command Tcode can move."))?;
        manager
            .move_to_background(session_id, &task, Mover::User)
            .map(|_| ())
            .map_err(provider_command_error)
    }

    /// The user stops one of the thread's tasks, or all of them.
    pub(super) fn stop_host_tasks(
        &mut self,
        session_id: &str,
        task_id: Option<&str>,
    ) -> Result<(), ProtocolError> {
        let manager = self
            .mcp
            .tasks
            .manager
            .as_ref()
            .ok_or_else(|| provider_command_error("Tcode's task tools are not running."))?;
        match task_id {
            Some(task_id) => manager
                .stop(session_id, task_id)
                .map(|_| ())
                .map_err(provider_command_error),
            None => {
                manager.stop_all(session_id, false);
                Ok(())
            }
        }
    }

    /// What the thread's permission mode makes of a task command now.
    fn task_admission(&self, meta: &SessionMeta) -> Admission {
        if self.mcp.tasks.allowed.contains(&meta.id) {
            return Admission::Run;
        }
        // An ACP agent's modes are its own: what one lets run unasked cannot
        // be known here, so each command asks. A mode a harness reviews
        // itself has no reviewer here, so it asks too.
        let class = if meta.provider == ProviderKind::Acp {
            ApprovalClass::Manual
        } else {
            agent::session_approval_class(meta.provider, &meta.option_selections)
        };
        if class == ApprovalClass::Unrestricted {
            return Admission::Run;
        }
        if self
            .turn_policy(&meta.id)
            .is_some_and(|policy| policy.approval == SpaceApproval::ReviewedOnly)
        {
            return Admission::Decline(
                "This space answers no approvals in its members' turns, and this thread's \
                 permission mode asks before Tcode runs a command."
                    .into(),
            );
        }
        Admission::Ask
    }

    /// The running `task_run` call of the thread no task is bound to yet,
    /// latest first: the one this command was started for.
    fn unbound_task_run_call(&self, session_id: &str) -> Option<String> {
        let session = self.resident(session_id)?;
        let bound: HashSet<String> = self
            .mcp
            .tasks
            .manager
            .as_ref()
            .map(|manager| {
                manager
                    .list(session_id)
                    .into_iter()
                    .filter(|task| task.foreground && task.phase.running())
                    .filter_map(|task| task.item_id)
                    .collect()
            })
            .unwrap_or_default();
        session
            .timeline
            .entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.content {
                EntryContent::Item(ItemContent::ToolCall { name, status, .. })
                    if *status == agent::ItemStatus::InProgress
                        && is_task_run(name)
                        && !bound.contains(&entry.id) =>
                {
                    Some(entry.id.clone())
                }
                _ => None,
            })
    }

    pub(super) fn handle_task_request(
        &mut self,
        request: tasks_mcp::BrokerRequest,
        _cx: &mut HostCx,
    ) {
        let tasks_mcp::BrokerRequest {
            session_id,
            command,
            cwd,
            foreground,
            notify_on_exit,
            env,
            timeout,
            call_id,
            reply,
        } = request;
        let refuse = |message: &str| {
            let _ = reply.try_send(Err(message.to_owned()));
        };
        let Some(meta) = self.current_meta(&session_id).cloned() else {
            return refuse("Unknown thread.");
        };
        if meta.archived_at.is_some() {
            return refuse("This thread is archived.");
        }
        if !self.task_tools_registered(&meta.id) {
            return refuse("Tcode's task tools are unavailable for this thread.");
        }
        let cwd = match cwd.as_deref().map(str::trim).filter(|cwd| !cwd.is_empty()) {
            Some(cwd) => meta.cwd.join(cwd),
            None => meta.cwd.clone(),
        };
        if !cwd.is_dir() {
            return refuse(&format!("{} is not a directory.", cwd.display()));
        }
        let spawn = Spawn {
            command,
            cwd,
            foreground,
            notify_on_exit,
            // The call that waits on it: the one the client names, else the
            // running `task_run` call no other task claims.
            item_id: call_id.or_else(|| {
                foreground
                    .then(|| self.unbound_task_run_call(&meta.id))
                    .flatten()
            }),
            env,
            timeout,
        };
        let space = self
            .resident(&meta.id)
            .and_then(|session| session.turn_space.clone());
        match self.task_admission(&meta) {
            Admission::Run => self.start_task(&meta.id, spawn, space, reply),
            Admission::Decline(reason) => refuse(&reason),
            Admission::Ask => self.ask_task_approval(meta.id, spawn, space, reply),
        }
    }

    fn start_task(
        &mut self,
        session_id: &str,
        spawn: Spawn,
        space: Option<String>,
        reply: async_channel::Sender<Result<TaskInfo, String>>,
    ) {
        let Some(manager) = &self.mcp.tasks.manager else {
            let _ = reply.try_send(Err("Tcode's task tools are not running.".into()));
            return;
        };
        let started = manager.spawn(session_id, spawn);
        if let (Ok(task), Some(space)) = (&started, space) {
            self.mcp
                .tasks
                .spaces
                .insert((session_id.to_owned(), task.task_id.clone()), space);
        }
        let _ = reply.try_send(started);
    }

    fn ask_task_approval(
        &mut self,
        session_id: String,
        spawn: Spawn,
        space: Option<String>,
        reply: async_channel::Sender<Result<TaskInfo, String>>,
    ) {
        let id = format!("{APPROVAL_PREFIX}{}", uuid::Uuid::new_v4());
        let option = |id: &str, label: &str, kind| ApprovalOption {
            id: id.into(),
            label: label.into(),
            kind,
        };
        let request = agent::ApprovalRequest {
            id: id.clone(),
            turn_id: None,
            parent_item_id: None,
            kind: agent::ApprovalKind::ExecCommand {
                command: spawn.command.clone(),
                cwd: Some(spawn.cwd.display().to_string()),
                description: None,
            },
            reason: Some(
                "The agent asks Tcode to run this command through its task tools, outside the \
                 agent's own sandbox and approval rules."
                    .into(),
            ),
            blocked_path: None,
            options: vec![
                option(ALLOW_ONCE, "Run", ApprovalOptionKind::AllowOnce),
                option(
                    ALLOW_THREAD,
                    "Run every task command in this thread",
                    ApprovalOptionKind::AllowAlways,
                ),
                option(DECLINE, "Decline", ApprovalOptionKind::RejectOnce),
            ],
        };
        self.approvals
            .entry(session_id.clone())
            .or_default()
            .push(request);
        self.mcp.tasks.approvals.insert(
            id,
            PendingTaskApproval {
                session_id,
                spawn,
                space,
                reply,
            },
        );
    }

    /// The user's answer to a task command, when `request_id` asks for one;
    /// `None` for a request the harness asked.
    pub(super) fn respond_task_approval(
        &mut self,
        session_id: &str,
        request_id: &str,
        decision: &ApprovalDecision,
    ) -> Option<Result<(), String>> {
        if !request_id.starts_with(APPROVAL_PREFIX) {
            return None;
        }
        let Some(pending) = self
            .mcp
            .tasks
            .approvals
            .remove(request_id)
            .filter(|pending| pending.session_id == session_id)
        else {
            return Some(Err("This request was already answered.".into()));
        };
        if let Some(requests) = self.approvals.get_mut(session_id) {
            requests.retain(|request| request.id != request_id);
            if requests.is_empty() {
                self.approvals.remove(session_id);
            }
        }
        match decision {
            ApprovalDecision::Option(option) if option == ALLOW_ONCE || option == ALLOW_THREAD => {
                if option == ALLOW_THREAD {
                    self.mcp.tasks.allowed.insert(session_id.to_owned());
                }
                self.start_task(session_id, pending.spawn, pending.space, pending.reply);
            }
            ApprovalDecision::Feedback(words) => {
                let _ = pending.reply.try_send(Err(format!(
                    "The user declined to run this command: {words}"
                )));
            }
            _ => {
                let _ = pending
                    .reply
                    .try_send(Err("The user declined to run this command.".into()));
            }
        }
        Some(Ok(()))
    }

    /// Withdraw the thread's unanswered task approvals: the turn whose call
    /// waited on them is over.
    pub(super) fn drop_task_approvals(&mut self, session_id: &str) {
        let ids: Vec<String> = self
            .mcp
            .tasks
            .approvals
            .iter()
            .filter(|(_, pending)| pending.session_id == session_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            if let Some(pending) = self.mcp.tasks.approvals.remove(&id) {
                let _ = pending.reply.try_send(Err(
                    "The request to run this command was withdrawn: the turn ended first.".into(),
                ));
            }
            if let Some(requests) = self.approvals.get_mut(session_id) {
                requests.retain(|request| request.id != id);
                if requests.is_empty() {
                    self.approvals.remove(session_id);
                }
            }
        }
    }

    pub(super) fn on_task_event(&mut self, event: TaskEvent, cx: &mut HostCx) {
        // A change in the list reaches clients with the next snapshot, which
        // this turn of the host loop takes.
        let TaskEvent::Exited {
            session_id,
            task_id,
        } = event
        else {
            return;
        };
        let pending = self.mcp.tasks.wakes.entry(session_id.clone()).or_default();
        pending.push(task_id);
        if pending.len() > 1 {
            return;
        }
        let host = cx.clone();
        cx.spawn_detached(async move {
            smol::Timer::after(WAKE_COALESCE).await;
            host.enqueue(move |state, cx| state.deliver_task_wakes(&session_id, cx));
        });
    }

    /// Wake the thread with the end of every task it waits to hear of, in
    /// one message, after the turn running now, as an agent's message: the
    /// user did nothing.
    pub(super) fn deliver_task_wakes(&mut self, session_id: &str, cx: &mut HostCx) {
        let ended = self.mcp.tasks.wakes.remove(session_id).unwrap_or_default();
        let Some(manager) = self.mcp.tasks.manager.clone() else {
            return;
        };
        let tasks: Vec<TaskInfo> = ended
            .iter()
            .filter_map(|task_id| manager.take_wake(session_id, task_id))
            .collect();
        let space = ended.iter().find_map(|task_id| {
            self.mcp
                .tasks
                .spaces
                .remove(&(session_id.to_owned(), task_id.clone()))
        });
        if tasks.is_empty()
            || !self
                .find_meta(session_id)
                .is_some_and(|meta| meta.archived_at.is_none())
        {
            return;
        }
        let now = now_millis();
        let text = tasks
            .iter()
            .map(|task| self.task_wake_text(&manager, session_id, task, now))
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut wake_cx = cx.clone();
        wake_cx.origin = Some(MessageOrigin::Agent);
        wake_cx.author = None;
        wake_cx.space = space;
        let cx = &mut wake_cx;
        self.reactivate_session(session_id, cx);
        self.queue_automatic_turn(
            session_id,
            |session| {
                session.push_or_merge_orchestrate_callback(text);
            },
            cx,
        );
    }

    /// One `[control]` line for the end of `task`, with the end of its
    /// output on the lines after it. No line is blank: a blank line parts
    /// one control message from the next.
    fn task_wake_text(
        &self,
        manager: &TaskManager,
        session_id: &str,
        task: &TaskInfo,
        now: u64,
    ) -> String {
        let mut text = format!(
            "{CONTROL_CALLBACK_PREFIX}task {} {} after {}: {} (in {}).",
            task.task_id,
            task_ended(task),
            duration_text(task.duration_ms(now)),
            serde_json::to_string(&task.command).unwrap_or_default(),
            task.cwd.display(),
        );
        let tail = manager
            .read(
                session_id,
                &task.task_id,
                Window::Tail {
                    lines: WAKE_TAIL_LINES,
                    max_bytes: WAKE_TAIL_BYTES,
                },
            )
            .ok();
        let lines: Vec<&str> = tail
            .as_ref()
            .map(|tail| {
                tail.text
                    .lines()
                    .map(|line| line.trim_end_matches('\r'))
                    .filter(|line| !line.trim().is_empty())
                    .collect()
            })
            .unwrap_or_default();
        if lines.is_empty() {
            text.push_str(" It printed nothing.");
        } else {
            text.push_str(&format!(
                " The end of its output ({} bytes in all; task_read reads more):",
                tail.as_ref().map_or(0, |tail| tail.total_bytes)
            ));
            for line in lines {
                text.push('\n');
                text.push_str(line);
            }
        }
        text
    }
}

#[cfg(test)]
#[path = "tasks_tests.rs"]
mod tests;
