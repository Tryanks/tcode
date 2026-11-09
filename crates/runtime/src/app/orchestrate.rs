use super::*;
use tcode_core::notice::{
    CHILD_REPORT_REMINDER, ORCHESTRATE_APPROVAL_PHRASE, ORCHESTRATE_FALLBACK_LABEL,
    ORCHESTRATE_OUTPUT_TAIL_LABEL, ORCHESTRATE_QUESTION_PHRASE, ORCHESTRATE_REPORT_LABEL,
    callback_header,
};
use tcode_core::project::ChildExpectation;
use tcode_core::settlement::{AgentDelivery, AgentReport};

/// Appended to every subagent's first message so the report contract reaches it
/// regardless of what the thread that started it wrote.
pub(super) const CHILD_REPORT_FOOTER: &str = "\n\n---\nThe tcode_control report_result tool is the only channel through which your work reaches the thread that started you as its subagent; it cannot see your transcript, and your own turns, waits, background tasks and wake-ups never reach it. When your work is complete, send your complete final report through it once, then end your turn. Make the report self-contained: for a discussion, give your reasoning, recommendations, disagreements, and open questions; for execution, include files changed and commands actually run with their outcomes. Include the evidence behind your conclusions. Only if the tool call fails, write that same complete report as your final message instead — it is sent back as the fallback, truncated when long.";

/// A thread's pull request tool registration.
pub(super) struct PullRequestRegistration {
    pub(super) registration: agent::McpRegistration,
    /// The host names the tools were described with.
    pub(super) hosts: String,
    /// The linking instructions the provider is given with the tools.
    pub(super) instructions: String,
}

#[derive(Default)]
pub(super) struct McpWiring {
    pub(super) pull_request_url: Option<String>,
    pub(super) pull_request_tokens: Option<pull_request_mcp::TokenRegistry>,
    /// The hosts the pull request tools' descriptions name.
    pub(super) pull_request_hosts: Option<pull_request_mcp::HostNames>,
    pub(super) pull_request_registrations: HashMap<String, PullRequestRegistration>,
    pub(super) preview_url: Option<String>,
    pub(super) preview_tokens: Option<preview_mcp::TokenRegistry>,
    pub(super) preview_registrations: HashMap<String, agent::McpRegistration>,
    pub(super) computer_use_url: Option<String>,
    pub(super) computer_use_tokens: Option<computer_use_mcp::TokenRegistry>,
    pub(super) computer_use_registrations: HashMap<String, agent::McpRegistration>,
    pub(super) control_url: Option<String>,
    pub(super) control_tokens: Option<control_mcp::TokenRegistry>,
    pub(super) control_registrations: HashMap<String, agent::McpRegistration>,
    pub(super) control_requests: Option<smol::channel::Receiver<control_mcp::BrokerRequest>>,
    /// Each registered thread's effective access, read by the control server when a
    /// connection opens to list the tools that access allows.
    pub(super) control_grants: Arc<std::sync::Mutex<HashMap<String, control_mcp::ThreadAccess>>>,
    /// The control server's record of the tools each thread's connection received.
    pub(super) control_source: Option<control_mcp::GrantSource>,
}

impl AppState {
    /// Attach the serializable registration half of the preview MCP server.
    /// Its broker receiver is drained by the host reverse-RPC pump.
    pub fn attach_preview_mcp(&mut self, url: String, tokens: preview_mcp::TokenRegistry) {
        self.mcp.preview_url = Some(url);
        self.mcp.preview_tokens = Some(tokens);
    }

    pub fn attach_computer_use_mcp(
        &mut self,
        url: String,
        tokens: computer_use_mcp::TokenRegistry,
    ) {
        self.mcp.computer_use_url = Some(url);
        self.mcp.computer_use_tokens = Some(tokens);
    }

    /// Every MCP tool offered to this thread's provider, and every text Tcode gives its model
    /// outside the conversation, as delivered; `None` for a provider that takes no MCP server.
    pub(super) fn injected_tools(
        &self,
        session: &ActiveSession,
    ) -> Option<tcode_protocol::InjectedTools> {
        use tcode_protocol::{InjectedChannel, InjectedInstructions};
        let meta = &session.meta;
        let provider = meta.provider;
        if !provider.caps().mcp_servers {
            return None;
        }
        let pull_requests = self.pull_request_tools_offered(provider);
        let preview = self.mcp.preview_url.is_some();
        let computer_use =
            computer_use_attaches(provider, &self.settings) && self.mcp.computer_use_url.is_some();
        let control = self.control_offered(meta);
        if !(pull_requests || preview || computer_use || control) {
            return None;
        }
        let id = meta.id.as_str();
        let attached = session.mcp_servers_attached;
        let mut instructions = Vec::new();
        let channel = provider.caps().instruction_channel;
        if pull_requests && attached && channel != agent::InstructionChannel::None {
            instructions.push(InjectedInstructions {
                channel: InjectedChannel::Linking(channel),
                text: self.pull_request_instructions(id),
            });
        }
        if provider == ProviderKind::Codex {
            instructions.push(InjectedInstructions {
                channel: InjectedChannel::DeveloperInstructions,
                text: agent::codex::PREVIEW_BROWSER_INSTRUCTIONS.to_owned(),
            });
        }
        let servers = [
            (
                agent::McpRegistration::SERVER_NAME_PULL_REQUESTS,
                pull_request_mcp::INSTRUCTIONS,
                self.mcp.pull_request_registrations.contains_key(id),
            ),
            (
                agent::McpRegistration::SERVER_NAME_PREVIEW,
                preview_mcp::INSTRUCTIONS,
                self.mcp.preview_registrations.contains_key(id),
            ),
            (
                agent::McpRegistration::SERVER_NAME_COMPUTER_USE,
                computer_use_mcp::tools::INSTRUCTIONS,
                self.mcp.computer_use_registrations.contains_key(id),
            ),
        ];
        let control_instructions = self
            .mcp
            .control_registrations
            .contains_key(id)
            .then(|| control_mcp::instructions(id, meta.is_dispatched()));
        instructions.extend(
            servers
                .into_iter()
                .filter(|(_, _, registered)| attached && *registered)
                .map(|(name, text, _)| (name, text.to_owned()))
                .chain(
                    control_instructions
                        .filter(|_| attached)
                        .map(|text| (agent::McpRegistration::SERVER_NAME_CONTROL, text)),
                )
                .map(|(name, text)| InjectedInstructions {
                    channel: InjectedChannel::McpServer(name.to_owned()),
                    text,
                }),
        );
        let mut tools = Vec::new();
        let mut servers = Vec::new();
        let mut offer = |name: &str, listed: Vec<(String, String)>| {
            servers.push(tcode_protocol::InjectedServer {
                name: name.to_owned(),
                tools: listed.len(),
            });
            tools.extend(listed);
        };
        if pull_requests {
            offer(
                agent::McpRegistration::SERVER_NAME_PULL_REQUESTS,
                pull_request_mcp::tool_descriptions(&self.pull_request_tool_hosts(id)),
            );
        }
        if preview {
            offer(
                agent::McpRegistration::SERVER_NAME_PREVIEW,
                preview_mcp::tool_descriptions(),
            );
        }
        if computer_use {
            offer(
                agent::McpRegistration::SERVER_NAME_COMPUTER_USE,
                computer_use_mcp::tools::tool_descriptions(),
            );
        }
        if control {
            offer(
                agent::McpRegistration::SERVER_NAME_CONTROL,
                self.mcp
                    .control_source
                    .as_ref()
                    .and_then(|source| source.offered(id))
                    .unwrap_or_else(|| control_mcp::tool_descriptions(&self.control_access(meta))),
            );
        }
        Some(tcode_protocol::InjectedTools {
            tools,
            servers,
            attached,
            instructions,
        })
    }

    pub(super) fn computer_use_registration_for(
        &mut self,
        meta: &SessionMeta,
    ) -> Option<agent::McpRegistration> {
        if let Some(registration) = self.mcp.computer_use_registrations.get(&meta.id) {
            return Some(registration.clone());
        }
        let token = self.mcp.computer_use_tokens.as_ref()?.register(&meta.id);
        let registration = agent::McpRegistration {
            name: agent::McpRegistration::SERVER_NAME_COMPUTER_USE.into(),
            url: self.mcp.computer_use_url.clone()?,
            bearer_token: token,
        };
        self.mcp
            .computer_use_registrations
            .insert(meta.id.clone(), registration.clone());
        Some(registration)
    }

    pub(super) fn cancel_computer_use_feedback(&self, session_id: &str) {
        if let Some(tokens) = &self.mcp.computer_use_tokens {
            tokens.cancel(session_id);
        }
    }

    pub(super) fn revoke_computer_use_registration(&mut self, session_id: &str) {
        if let Some(registration) = self.mcp.computer_use_registrations.remove(session_id)
            && let Some(tokens) = &self.mcp.computer_use_tokens
        {
            tokens.revoke(session_id, &registration.bearer_token);
        }
    }

    pub(super) fn preview_registration_for(
        &mut self,
        meta: &SessionMeta,
    ) -> Option<agent::McpRegistration> {
        if let Some(registration) = self.mcp.preview_registrations.get(&meta.id) {
            return Some(registration.clone());
        }
        let token = self.mcp.preview_tokens.as_ref()?.register(&meta.id);
        let registration = agent::McpRegistration {
            name: agent::McpRegistration::SERVER_NAME_PREVIEW.into(),
            url: self.mcp.preview_url.clone()?,
            bearer_token: token,
        };
        self.mcp
            .preview_registrations
            .insert(meta.id.clone(), registration.clone());
        Some(registration)
    }

    /// Start a subagent, a thread whose `parent_session_id` names the thread that started it,
    /// owing that thread the report for its first message.
    pub(super) fn install_subagent(
        &mut self,
        mut meta: SessionMeta,
        message: String,
        attachments: Vec<Attachment>,
        cx: &mut HostCx,
    ) -> String {
        // The report contract rides with every first message: the subagent sees nothing of
        // the thread that started it, and the tool description alone yields missing or
        // one-line reports. Skipped for providers without MCP attachment (pi), where the
        // tool does not exist.
        let message = if meta.provider.caps().mcp_servers {
            format!("{message}{CHILD_REPORT_FOOTER}")
        } else {
            message
        };
        meta.expectation = Some(ChildExpectation::opened(now_secs()));
        self.install_background_thread(meta, message, attachments, cx)
    }

    /// Persist a new thread and start it in the background on `first_message`, written by an
    /// agent. Nobody's selection or draft is touched.
    pub(super) fn install_background_thread(
        &mut self,
        meta: SessionMeta,
        first_message: String,
        attachments: Vec<Attachment>,
        cx: &mut HostCx,
    ) -> String {
        self.thread_activity
            .insert(meta.id.clone(), Default::default());
        self.enqueue_store_write(
            StoreWrite::UpsertMeta {
                meta: Box::new(meta.clone()),
                initial: true,
            },
            cx,
        );
        self.upsert_session_in_memory(meta.clone());
        let id = meta.id.clone();
        let provider_commands = self.cached_provider_commands_for(&meta);
        let mut session = Self::build_draft_session(
            meta.project_id.clone().unwrap_or_default(),
            meta.cwd.clone(),
            meta.provider,
            meta.model.clone(),
            None,
            provider_commands,
        );
        session.meta = meta;
        session.draft = false;
        session.push_queued(first_message, attachments);
        session.queue.last_mut().unwrap().origin = MessageOrigin::Agent;
        self.residents.parked.insert(id.clone(), session);
        self.reactivate_session(&id, cx);
        self.ensure_session_started(&id, cx);
        id
    }

    /// Stop a dispatched child without delivering its result: it keeps its
    /// transcript and no longer holds its lead.
    pub(crate) fn cancel_child(&mut self, thread_id: &str, cx: &mut HostCx) {
        self.clear_approvals(thread_id);
        if self.residents.live.contains_key(thread_id) {
            if let Some(child) = self.resident_mut(thread_id) {
                child.queue.clear();
                child.timeline.mark_idle();
                child.shutdown_to_idle();
            }
        } else {
            self.drop_background(thread_id, cx);
        }
        let Some(mut meta) = self.find_meta(thread_id) else {
            return;
        };
        meta.cancelled_at = Some(now_secs());
        meta.expectation = None;
        meta.updated_at = now_secs();
        if let Some(child) = self.resident_mut(thread_id) {
            child.meta = meta.clone();
        }
        self.persist_meta(&meta, cx);
    }

    pub fn settle_children(
        &mut self,
        parent_id: &str,
        cx: &mut HostCx,
    ) -> tcode_protocol::SettleChildrenResult {
        let children: Vec<_> = self
            .sessions
            .iter()
            .filter(|meta| {
                meta.is_dispatched() && meta.parent_session_id.as_deref() == Some(parent_id)
            })
            .filter_map(|meta| {
                self.agent_status(meta)
                    .map(|status| (meta.id.clone(), status.delivery))
            })
            .collect();
        let mut result = tcode_protocol::SettleChildrenResult::default();
        for (thread_id, delivery) in children {
            if delivery == AgentDelivery::AwaitingSettle && !self.settle_thread_busy(&thread_id) {
                self.settle_session(&thread_id, cx);
                result.settled.push(tcode_protocol::ChildSettlement {
                    thread_id,
                    delivery: AgentDelivery::Settled,
                });
            } else {
                result.skipped.push(tcode_protocol::ChildSettlement {
                    thread_id,
                    delivery,
                });
            }
        }
        result
    }

    pub(super) fn ensure_child_loaded(
        &mut self,
        thread_id: &str,
        cx: &mut HostCx,
    ) -> Result<(), String> {
        if self.residents.parked.contains_key(thread_id) {
            return Ok(());
        }
        let meta = self
            .sessions
            .iter()
            .find(|meta| meta.id == thread_id)
            .cloned()
            .ok_or_else(|| "unknown thread".to_string())?;
        if self.residents.live.contains_key(thread_id) {
            return Err("child thread is currently open in the foreground".into());
        }
        self.load_background_session(meta, cx);
        Ok(())
    }

    pub(super) fn load_background_session(&mut self, meta: SessionMeta, cx: &mut HostCx) {
        let thread_id = meta.id.clone();
        let commands = self.cached_provider_commands_for(&meta);
        let mut child = Self::build_draft_session(
            meta.project_id.clone().unwrap_or_default(),
            meta.cwd.clone(),
            meta.provider,
            meta.model.clone(),
            meta.acp_agent_id.clone(),
            commands,
        );
        child.meta = meta;
        child.draft = false;
        self.residents.parked.insert(thread_id.clone(), child);
        self.schedule_timeline_load(thread_id, TimelineLoadTarget::Background, cx);
    }

    pub(super) fn loaded_child_timeline(&self, session_id: &str) -> Option<&Timeline> {
        self.resident(session_id).map(|child| &child.timeline)
    }

    /// Record what a child owes its lead, on the thread and in the store.
    pub(super) fn set_child_expectation(
        &mut self,
        thread_id: &str,
        expectation: Option<ChildExpectation>,
        cx: &mut HostCx,
    ) {
        let Some(mut meta) = self.find_meta(thread_id) else {
            return;
        };
        if meta.expectation == expectation {
            return;
        }
        meta.expectation = expectation;
        meta.updated_at = now_secs();
        if let Some(child) = self.resident_mut(thread_id) {
            child.meta = meta.clone();
        }
        self.persist_meta(&meta, cx);
    }

    /// Hand the lead's (or tcode's own) message to a child: steered into a
    /// live turn, otherwise queued as the child's next turn. Reports which.
    pub(super) fn deliver_to_child(
        &mut self,
        thread_id: &str,
        message: String,
        cx: &mut HostCx,
    ) -> Result<&'static str, String> {
        // A live turn accepts the message right away — same routing as
        // parent callbacks. Queueing a mid-turn correction until the
        // turn ends would deliver it after the work it was meant to
        // redirect (and never, if the turn hangs).
        let can_steer = self
            .resident(thread_id)
            .is_some_and(|child| child.turn_in_flight && child.can_steer());
        // Provider channel gone: fall through so the text survives in the
        // queue for the wake-up path.
        if can_steer && self.steer_host_message(thread_id, &message, Vec::new(), cx) {
            return Ok("steered");
        }
        if !can_steer {
            self.reactivate_session(thread_id, cx);
        }
        if self.residents.live.contains_key(thread_id) {
            let default_window = self.default_context_window(thread_id);
            let child = self.resident_mut(thread_id).unwrap();
            child.push_queued(message, Vec::new());
            child.stamp_context_window_change(default_window);
            child.queue.last_mut().unwrap().origin = MessageOrigin::Agent;
            let idle = matches!(child.runtime, Runtime::Idle);
            if self.dispatch_next_queued(thread_id, cx).is_err() {
                return Err("child provider is unavailable".into());
            }
            if idle {
                self.ensure_started(thread_id, cx);
            }
            return Ok("queued");
        }
        self.ensure_child_loaded(thread_id, cx)?;
        let child = self.resident_mut(thread_id).unwrap();
        child.push_queued(message, Vec::new());
        child.queue.last_mut().unwrap().origin = MessageOrigin::Agent;
        let idle = matches!(child.runtime, Runtime::Idle);
        if !idle && !child.turn_in_flight {
            self.on_background_turn_completed(thread_id, cx);
        }
        if idle {
            self.ensure_session_started(thread_id, cx);
        }
        Ok("queued")
    }

    /// Steer text the host or a child wrote into the running turn.
    pub(super) fn steer_host_message(
        &mut self,
        thread: &str,
        text: &str,
        reports: Vec<ReportHandover>,
        cx: &mut HostCx,
    ) -> bool {
        let request_id = self.record_steer_request(thread, text, &[], cx);
        let steer = PendingSteer {
            request_id,
            text: text.to_owned(),
            attachments: Vec::new(),
            author: cx.author.clone(),
            origin: cx.origin,
            reports,
        };
        self.hand_over_steer(thread, steer, text.to_owned(), cx)
    }

    /// Nothing is left inside the thread that could still produce a report
    /// on its own: no turn, nothing queued, no background task or wake-up,
    /// no request waiting on an answer.
    pub(super) fn thread_quiescent(&self, id: &str) -> bool {
        !self.settle_thread_busy(id)
            && self.resident(id).is_none_or(|session| {
                session.queue.is_empty()
                    && session.background_task_count == 0
                    && matches!(session.runtime, Runtime::Idle | Runtime::Live(_))
            })
    }

    /// The child meta and its lead, when the child still owes or has owed a
    /// report and both sides are current.
    fn expectant_child(&self, child_id: &str) -> Option<(SessionMeta, String, ChildExpectation)> {
        let child = self
            .sessions
            .iter()
            .find(|meta| meta.id == child_id && meta.parent_session_id.is_some())
            .filter(|meta| meta.archived_at.is_none() && meta.cancelled_at.is_none())?
            .clone();
        let expectation = child.expectation.clone()?;
        let parent_id = child.parent_session_id.clone()?;
        if !self
            .find_meta(&parent_id)
            .is_some_and(|meta| meta.archived_at.is_none())
        {
            return None;
        }
        Some((child, parent_id, expectation))
    }

    /// A child's turn ended (or its background work did): decide whether the
    /// lead is owed anything now. A report the child made is delivered; a
    /// child that owes one and has gone quiet is reminded once, then its
    /// final message is delivered in the report's place. Turns that end for
    /// the child's own reasons — wake-ups, monitors, background tasks —
    /// deliver nothing.
    pub(super) fn settle_child_expectation(
        &mut self,
        child_id: &str,
        status: TurnStatus,
        cx: &mut HostCx,
    ) {
        let Some((child, parent_id, mut expectation)) = self.expectant_child(child_id) else {
            return;
        };
        let Some(resident) = self.resident(child_id) else {
            return;
        };
        if resident.turn_in_flight {
            return;
        }
        let timeline = &resident.timeline;
        let usage = timeline.usage;
        // A report made before the lead's latest message answers an earlier one. The
        // expectation may have opened before the child's log was loaded, so its time decides
        // as well as the item delivered last.
        let opened_ms = expectation.opened_at.saturating_mul(1000);
        let report = timeline
            .last_report()
            .filter(|(item, _)| {
                timeline
                    .entries
                    .iter()
                    .rev()
                    .find(|entry| entry.id == *item)
                    .and_then(|entry| entry.finished_ts.or(entry.ts))
                    .is_none_or(|at| at >= opened_ms)
            })
            .map(|(item, text)| (item.to_owned(), text.to_owned()));
        // The child chose this text for its lead: deliver it whatever else the
        // child is doing, once per report.
        if let Some((item, text)) = report
            && expectation.delivered_item.as_deref() != Some(item.as_str())
        {
            let state = if expectation.delivered_item.is_some() {
                "updated"
            } else {
                "reported"
            };
            let text = assemble_report_callback(
                child_id,
                &child.title,
                state,
                CallbackBody::Report(&text),
                usage.as_ref(),
            );
            expectation.state = AgentReport::Reported;
            expectation.delivered_item = Some(item);
            self.deliver_report_to_parent(child_id, &parent_id, text, expectation, cx);
            return;
        }
        if !expectation.state.pending() {
            return;
        }
        match status {
            TurnStatus::Completed => {}
            // The user stopped the child: they have it now.
            TurnStatus::Interrupted => return,
            TurnStatus::Failed => {
                let error = trailing_error(timeline)
                    .unwrap_or("the turn failed")
                    .to_owned();
                self.fail_child_expectation(child_id, &error, cx);
                return;
            }
        }
        if !self.thread_quiescent(child_id) {
            return;
        }
        if !expectation.reminded && child.provider.caps().mcp_servers {
            expectation.reminded = true;
            self.set_child_expectation(child_id, Some(expectation), cx);
            if let Err(error) = self.deliver_to_child(child_id, CHILD_REPORT_REMINDER.into(), cx) {
                self.fail_child_expectation(child_id, &error, cx);
            }
            return;
        }
        let final_message = final_assistant_message(timeline);
        let text = assemble_report_callback(
            child_id,
            &child.title,
            "unreported",
            CallbackBody::Fallback(&final_message),
            usage.as_ref(),
        );
        expectation.state = AgentReport::Unreported;
        self.deliver_report_to_parent(child_id, &parent_id, text, expectation, cx);
    }

    /// The child can no longer deliver what it owes: its lead hears why, once.
    pub(super) fn fail_child_expectation(&mut self, child_id: &str, error: &str, cx: &mut HostCx) {
        let Some((child, parent_id, mut expectation)) = self.expectant_child(child_id) else {
            return;
        };
        if !expectation.state.pending() {
            return;
        }
        let usage = self
            .resident(child_id)
            .and_then(|child| child.timeline.usage);
        let text = assemble_report_callback(
            child_id,
            &child.title,
            "failed",
            CallbackBody::Failure(error),
            usage.as_ref(),
        );
        expectation.state = AgentReport::Failed;
        self.deliver_report_to_parent(child_id, &parent_id, text, expectation, cx);
    }

    pub(super) fn deliver_child_approval_callback(
        &mut self,
        child_id: &str,
        request_id: &str,
        cx: &mut HostCx,
    ) {
        let Some(child) = self
            .sessions
            .iter()
            .find(|meta| meta.id == child_id && meta.parent_session_id.is_some())
            .cloned()
        else {
            return;
        };
        let Some(request) = self
            .approval_requests(child_id)
            .iter()
            .find(|request| request.id == request_id)
            .cloned()
        else {
            return;
        };
        if self.settings.subagents.child_approval == ChildApprovalMode::AlwaysAllow
            && let Some(option) = native_allow_option(&request)
        {
            match self.respond_session_approval(
                child_id,
                request.id.clone(),
                ApprovalDecision::Option(option.id.clone()),
            ) {
                Ok(()) => return,
                Err(err) => log::warn!("failed to answer child {child_id} approval: {err}"),
            }
        }
        if !self
            .callback_approval_requests
            .insert((child_id.to_string(), request.id.clone()))
        {
            return;
        }
        let parent_id = child.parent_session_id.as_deref().unwrap();
        let options = request.options.iter().map(|option| {
            serde_json::json!({ "id": option.id, "label": option.label, "kind": option.kind })
        }).collect::<Vec<_>>();
        let instruction = if self.settings.subagents.child_approval == ChildApprovalMode::Manual {
            "The user answers in the child thread."
        } else {
            "You are the approver: answer with tcode_control respond_approval, passing this thread_id and approval_id with option: <exact id> or cancel: true; reject anything outside the brief's scope. To allow, prefer an allow_always option over allow_once when one is offered."
        };
        let text = format!(
            "{} {ORCHESTRATE_APPROVAL_PHRASE}{} (approval_id: {}). Native options: {}. {}",
            callback_header(child_id, &child.title),
            approval_request_summary(&request),
            request.id,
            serde_json::to_string(&options).expect("approval options serialize"),
            instruction,
        );
        self.deliver_orchestrate_callback_to_parent(parent_id, text, cx);
    }

    /// A child's question goes to its lead, as an approval does; in Manual
    /// mode the lead is only told that the user answers it. Delivered once per
    /// request, whether the child blocks on it or keeps working.
    pub(super) fn deliver_child_user_input_callback(
        &mut self,
        child_id: &str,
        request_id: &str,
        cx: &mut HostCx,
    ) {
        let Some(child) = self
            .sessions
            .iter()
            .find(|meta| meta.id == child_id && meta.parent_session_id.is_some())
            .cloned()
        else {
            return;
        };
        let Some(pending) = self
            .resident(child_id)
            .and_then(|child| child.timeline.pending_user_input.as_ref())
            .filter(|pending| pending.request_id == request_id)
            .cloned()
        else {
            return;
        };
        if !self
            .callback_user_input_requests
            .insert((child_id.to_string(), pending.request_id.clone()))
        {
            return;
        }
        let parent_id = child.parent_session_id.as_deref().unwrap();
        let delivery = if pending.delivery.is_blocking() {
            "the child is blocked until it is answered"
        } else {
            "the child keeps working and reads the answer when it arrives; the question lapses when its turn ends"
        };
        let instruction = if self.settings.subagents.child_approval == ChildApprovalMode::Manual {
            "The user answers in the child thread."
        } else {
            "You are the decider: answer with tcode_control respond_input, passing this thread_id and question_id with answers keyed by question id, a string (an option label or free text) for a single-choice question and an array of strings for a multi_select one, deciding from the brief and the user's instructions."
        };
        let text = format!(
            "{} {ORCHESTRATE_QUESTION_PHRASE}question_id: {}; {delivery}). Questions: {}. {instruction}",
            callback_header(child_id, &child.title),
            pending.request_id,
            serde_json::to_string(&pending.questions).expect("questions serialize"),
        );
        self.deliver_orchestrate_callback_to_parent(parent_id, text, cx);
    }

    /// Deliver a child result into the orchestrator's current reasoning turn.
    ///
    /// A foreground parent already used `steer`, but a parked parent used to put
    /// callbacks into its ordinary queue. Parallel children could therefore
    /// leave results stranded while the orchestrator planned from only the first
    /// completion. Steering is session lifecycle behavior, not UI focus behavior,
    /// so foreground and parked parents follow the same routing here.
    pub(super) fn deliver_orchestrate_callback_to_parent(
        &mut self,
        parent_id: &str,
        text: String,
        cx: &mut HostCx,
    ) {
        self.deliver_callback(parent_id, text, None, cx);
    }

    /// A child's report, outcome or failure for its lead, kept with the child
    /// until the lead's provider takes it.
    fn deliver_report_to_parent(
        &mut self,
        child_id: &str,
        parent_id: &str,
        text: String,
        mut expectation: ChildExpectation,
        cx: &mut HostCx,
    ) {
        expectation.undelivered = Some(text.clone());
        self.set_child_expectation(child_id, Some(expectation), cx);
        self.deliver_callback(parent_id, text, Some(child_id), cx);
    }

    /// A report the user dropped from the lead's queue: the next start owes
    /// it nothing.
    pub(super) fn reports_dropped(&mut self, reports: &[ReportHandover], cx: &mut HostCx) {
        for meta in self.settle_reports(reports) {
            self.persist_meta(&meta, cx);
        }
    }

    /// The metas of the children that owe `reports` no more, for the caller
    /// to save.
    pub(super) fn settle_reports(&self, reports: &[ReportHandover]) -> Vec<SessionMeta> {
        let mut settled: Vec<SessionMeta> = Vec::new();
        for report in reports {
            if settled.iter().any(|meta| meta.id == report.child) {
                continue;
            }
            let Some(mut meta) = self.find_meta(&report.child) else {
                continue;
            };
            let Some(expectation) = meta
                .expectation
                .as_mut()
                .filter(|expectation| expectation.undelivered.as_ref() == Some(&report.text))
            else {
                continue;
            };
            expectation.undelivered = None;
            meta.updated_at = now_secs();
            settled.push(meta);
        }
        settled
    }

    /// After a restart: the reports a lead's queue held when the host stopped.
    pub fn resume_undelivered_reports(&mut self, cx: &mut HostCx) {
        let undelivered: Vec<_> = self
            .sessions
            .iter()
            .filter_map(|meta| {
                let text = meta.expectation.as_ref()?.undelivered.clone()?;
                Some((meta.id.clone(), meta.parent_session_id.clone()?, text))
            })
            .collect();
        for (child, parent, text) in undelivered {
            self.deliver_callback(&parent, text, Some(&child), cx);
        }
    }

    fn deliver_callback(
        &mut self,
        parent_id: &str,
        text: String,
        report: Option<&str>,
        cx: &mut HostCx,
    ) {
        if !self
            .find_meta(parent_id)
            .is_some_and(|meta| meta.archived_at.is_none())
        {
            return;
        }
        let mut callback_cx = cx.clone();
        callback_cx.origin = Some(MessageOrigin::Agent);
        callback_cx.author = None;
        let cx = &mut callback_cx;
        let can_steer = self
            .resident(parent_id)
            .is_some_and(|parent| parent.turn_in_flight && parent.can_steer());
        // A steered callback is already part of this turn, so persist it just
        // like a user-triggered steer before handing it to the provider. With
        // the provider channel gone it waits for the next start instead.
        let reports: Vec<_> = report
            .map(|child| ReportHandover {
                child: child.to_owned(),
                text: text.clone(),
            })
            .into_iter()
            .collect();
        if can_steer && self.steer_host_message(parent_id, &text, reports.clone(), cx) {
            return;
        }

        self.reactivate_session(parent_id, cx);
        self.queue_automatic_turn(
            parent_id,
            |parent| {
                let id = parent.push_or_merge_orchestrate_callback(text);
                if let Some(message) = parent.queue.iter_mut().find(|message| message.id == id) {
                    message.reports.extend(reports);
                }
            },
            cx,
        );
    }
}

/// A native provider as a model names it: `claude`, `codex`, `opencode`, …
pub(super) fn native_provider_named(name: &str) -> Option<ProviderKind> {
    match name.trim().to_ascii_lowercase().as_str() {
        "claude" | "claude_code" | "claude-code" => Some(ProviderKind::ClaudeCode),
        "codex" => Some(ProviderKind::Codex),
        "pi" => Some(ProviderKind::Pi),
        "opencode" | "open_code" | "open-code" => Some(ProviderKind::OpenCode),
        "cursor" => Some(ProviderKind::Cursor),
        "grok" => Some(ProviderKind::Grok),
        _ => None,
    }
}

/// The permission value a subagent starts with: what Settings → Control child approval
/// chooses from the provider's native control, `None` for a provider without one.
pub(super) fn resolve_child_permission(
    descriptor: Option<OptionDescriptor>,
    mode: ChildApprovalMode,
) -> Option<OptionSelection> {
    let OptionDescriptor::Select {
        id,
        default_value,
        recommended,
        permissive,
        ..
    } = descriptor?
    else {
        return None;
    };
    let value = match mode {
        ChildApprovalMode::Auto => recommended.or(default_value),
        ChildApprovalMode::AlwaysAllow => permissive.or(default_value),
        ChildApprovalMode::Orchestrator | ChildApprovalMode::Manual => default_value,
    }?;
    Some(OptionSelection {
        id,
        value: serde_json::Value::String(value),
    })
}

fn native_allow_option(request: &agent::ApprovalRequest) -> Option<&agent::ApprovalOption> {
    use agent::ApprovalOptionKind::{AllowAlways, AllowOnce};
    request
        .options
        .iter()
        .find(|option| option.kind == AllowAlways)
        .or_else(|| {
            request
                .options
                .iter()
                .find(|option| option.kind == AllowOnce)
        })
}

pub(super) fn resolve_approval_decision(
    option: Option<&str>,
    cancel: bool,
    request: &agent::ApprovalRequest,
) -> Result<ApprovalDecision, String> {
    match (option, cancel) {
        (Some(id), false) if request.options.iter().any(|option| option.id == id) => {
            Ok(ApprovalDecision::Option(id.to_string()))
        }
        (Some(id), false) => Err(format!(
            "unknown option: {id}; valid option ids: {}",
            request
                .options
                .iter()
                .map(|option| option.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
        (None, true) => Ok(ApprovalDecision::Cancel),
        _ => Err("supply option or cancel: true, never both".into()),
    }
}

pub(super) fn resolve_child_worktree(
    cwd: PathBuf,
    child_id: &str,
    data_dir: &Path,
) -> (PathBuf, Option<WorktreeInfo>, Option<String>) {
    match provision(&cwd, child_id, data_dir) {
        Ok(created) => {
            let info = WorktreeInfo {
                root_project_path: cwd,
                base: created.base,
                branch: created.branch,
            };
            (created.path, Some(info), None)
        }
        Err(ProvisionError::NotRepositoryRoot { path }) => {
            let warning = format!(
                "worktree isolation unavailable: {} is not a Git repository root; using the plain cwd",
                path.display()
            );
            log::warn!("subagent worktree: {warning}");
            (cwd, None, Some(warning))
        }
        Err(error) => {
            let warning = format!("worktree isolation failed ({error}); using the plain cwd");
            log::warn!("subagent worktree: {warning}");
            (cwd, None, Some(warning))
        }
    }
}

pub(super) fn final_assistant_message(timeline: &Timeline) -> String {
    let Some((last_index, last)) = timeline
        .entries
        .iter()
        .enumerate()
        .rev()
        .find(|(_, entry)| {
            matches!(
                &entry.content,
                EntryContent::Item(ItemContent::AssistantMessage { .. })
            )
        })
    else {
        return String::new();
    };

    // One provider message may contain several adjacent text blocks. They are
    // separate timeline entries, but together form the final assistant output.
    // Stop at the first non-assistant item so tool preambles from earlier in the
    // turn are not mistaken for part of the final answer.
    let mut parts = Vec::new();
    for entry in timeline.entries[..=last_index].iter().rev() {
        if entry.turn != last.turn {
            break;
        }
        match &entry.content {
            EntryContent::Item(ItemContent::AssistantMessage { text, .. }) => {
                parts.push(text.as_str())
            }
            _ => break,
        }
    }
    parts.reverse();
    parts.concat()
}

/// The error entry a failed turn ended on, when it is the thread's last entry.
fn trailing_error(timeline: &Timeline) -> Option<&str> {
    match &timeline.entries.last()?.content {
        EntryContent::Error { message, .. } => Some(message),
        EntryContent::ProviderStartError { error } => Some(error),
        _ => None,
    }
}

pub(super) fn tail_chars(text: &str, max: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(max)).collect()
}

pub(super) fn approval_request_summary(request: &agent::ApprovalRequest) -> String {
    let detail = match &request.kind {
        agent::ApprovalKind::ExecCommand { command, .. } => format!("command `{command}`"),
        agent::ApprovalKind::FileRead { detail } => format!("file read `{detail}`"),
        agent::ApprovalKind::FileChange { changes, .. } => match changes.as_slice() {
            [change] => format!("file change `{}`", change.path),
            changes => format!("{} file changes", changes.len()),
        },
        agent::ApprovalKind::ToolUse { name, .. } => format!("tool `{name}`"),
        agent::ApprovalKind::Plan { markdown, .. } => format!("plan: {markdown}"),
    };
    let one_line = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    let truncated: String = one_line.chars().take(180).collect();
    if one_line.chars().count() > 180 {
        format!("{truncated}…")
    } else {
        truncated
    }
}

/// The longest final message a fallback delivers whole; a longer one arrives as its tail.
const FALLBACK_MAX_CHARS: usize = 1200;

pub(super) enum CallbackBody<'a> {
    /// The child's own report, delivered verbatim.
    Report(&'a str),
    /// The child never reported: its final message stands in, bounded by
    /// [`FALLBACK_MAX_CHARS`].
    Fallback(&'a str),
    Failure(&'a str),
}

pub(super) fn assemble_report_callback(
    child_id: &str,
    title: &str,
    state: &str,
    body: CallbackBody<'_>,
    usage: Option<&agent::TokenUsage>,
) -> String {
    let mut token_parts = Vec::new();
    if let Some(usage) = usage {
        if let Some(input) = usage.input_tokens {
            let cached = usage
                .cached_input_tokens
                .filter(|cached| *cached > 0)
                .map(|cached| format!(" (+{cached} cached)"))
                .unwrap_or_default();
            token_parts.push(format!("input {input}{cached}"));
        }
        if let Some(output) = usage.output_tokens {
            token_parts.push(format!("output {output}"));
        }
        if let Some(total) = usage.total_processed_tokens.or(usage.used_tokens) {
            token_parts.push(format!("total {total}"));
        }
    }
    let token_segment = if token_parts.is_empty() {
        String::new()
    } else {
        format!(" tokens: {}.", token_parts.join(", "))
    };
    let body = match body {
        CallbackBody::Report(report) => format!("{ORCHESTRATE_REPORT_LABEL}{report}"),
        CallbackBody::Fallback(final_message) => {
            let digest = if final_message.is_empty() {
                "(no assistant output)".to_string()
            } else {
                let count = final_message.chars().count();
                if count <= FALLBACK_MAX_CHARS {
                    final_message.to_string()
                } else {
                    format!(
                        "{ORCHESTRATE_OUTPUT_TAIL_LABEL}{count} chars total; outline and read thread {child_id} for the full text):\n{}",
                        tail_chars(final_message, 600)
                    )
                }
            };
            format!("{ORCHESTRATE_FALLBACK_LABEL}{digest}")
        }
        CallbackBody::Failure(error) => error.to_string(),
    };
    format!(
        "{} {state}.{token_segment}\n{body}",
        callback_header(child_id, title)
    )
}

#[cfg(test)]
mod permission_tests {
    use super::*;

    #[test]
    fn missing_permission_hints_fall_back_and_control_less_providers_get_no_selection() {
        for provider in [ProviderKind::ClaudeCode, ProviderKind::OpenCode] {
            let mut descriptor = permission_control(provider).unwrap();
            match &mut descriptor {
                OptionDescriptor::Select {
                    recommended,
                    permissive,
                    ..
                } => {
                    *recommended = None;
                    *permissive = None;
                }
                OptionDescriptor::Boolean {
                    recommended,
                    permissive,
                    ..
                } => {
                    *recommended = None;
                    *permissive = None;
                }
            }
            let expected = if provider == ProviderKind::ClaudeCode {
                serde_json::json!("default")
            } else {
                serde_json::json!("normal")
            };
            for mode in [ChildApprovalMode::Auto, ChildApprovalMode::AlwaysAllow] {
                assert_eq!(
                    resolve_child_permission(Some(descriptor.clone()), mode)
                        .unwrap()
                        .value,
                    expected
                );
            }
        }
        for provider in [ProviderKind::Pi, ProviderKind::Acp] {
            for mode in [
                ChildApprovalMode::Orchestrator,
                ChildApprovalMode::Auto,
                ChildApprovalMode::AlwaysAllow,
                ChildApprovalMode::Manual,
            ] {
                assert_eq!(
                    resolve_child_permission(permission_control(provider), mode),
                    None
                );
            }
        }
    }
}
