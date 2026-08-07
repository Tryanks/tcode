//! Native OpenCode provider: one authenticated `opencode serve` child per
//! tcode session, REST commands, and an SSE event stream.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read};
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Value, json};
use smol::channel::{Receiver, Sender};
use smol::future;

use crate::{
    AgentError, AgentEvent, ApprovalDecision, ApprovalKind, ApprovalMode, ApprovalRequest,
    Attachment, ChangeCompleteness, DeltaKind, FileChange, FileChangeKind, InteractionMode,
    ItemContent, ItemStatus, LaunchEnv, ModelSpec, OptionDescriptor, OptionSelection,
    ProviderCommand, ProviderCommandKind, ProviderKind, ResumeCursor, SelectOption, SessionCommand,
    SessionHandle, SessionOptions, ThreadItem, TokenUsage, TurnStatus,
};

pub async fn start(opts: SessionOptions) -> Result<SessionHandle, AgentError> {
    if opts.fork {
        return Err(AgentError::Protocol(
            "session fork is not supported for this provider".into(),
        ));
    }
    crate::spawn_session(
        ProviderKind::OpenCode,
        opts,
        run_actor,
        "OpenCode actor exited before reporting startup status",
    )
    .await
}

pub async fn list_models(
    binary_path: Option<PathBuf>,
    launch_env: LaunchEnv,
) -> Result<Vec<ModelSpec>, AgentError> {
    smol::unblock(move || {
        let cwd = std::env::current_dir()?;
        let mut server = OpenCodeServer::spawn(
            binary_path.as_deref(),
            &cwd,
            &launch_env,
            ApprovalMode::FullAccess,
            &[],
            &[],
        )?;
        let result = (|| {
            server.wait_healthy()?;
            let provider_state = server.http.get_json("/provider")?;
            let mut catalog = server.http.get_json("/config/providers")?;
            reconcile_provider_catalog(&mut catalog, &provider_state);
            Ok(map_models(&catalog))
        })();
        server.stop();
        result
    })
    .await
}

async fn run_actor(
    opts: SessionOptions,
    commands: Receiver<SessionCommand>,
    events: Sender<AgentEvent>,
    ready: Sender<Result<(), AgentError>>,
) {
    let mut server = match OpenCodeServer::spawn(
        opts.binary_path.as_deref(),
        &opts.cwd,
        &opts.launch_env,
        opts.approval_mode,
        &opts.extra_args,
        &opts.mcp_servers.iter().collect::<Vec<_>>(),
    ) {
        Ok(server) => server,
        Err(err) => {
            let _ = ready.send(Err(err)).await;
            return;
        }
    };
    if let Err(err) = server.wait_healthy() {
        server.stop();
        let _ = ready.send(Err(err)).await;
        return;
    }
    let sse = match server.subscribe() {
        Ok(sse) => sse,
        Err(err) => {
            server.stop();
            let _ = ready.send(Err(err)).await;
            return;
        }
    };
    if let Err(err) = wait_for_server_connected(&sse).await {
        server.stop();
        let _ = ready.send(Err(err)).await;
        return;
    }
    let requested_model = opts.model.as_deref().and_then(split_model_id);
    let session = if let Some(session_id) = resume_session_id(&opts.resume) {
        server.http.get_json(&format!("/session/{session_id}"))
    } else {
        let mut body = json!({});
        if let Some((provider_id, model_id)) = requested_model {
            body["model"] = json!({"providerID":provider_id,"id":model_id});
        }
        server
            .http
            .post_json("/session", &body)
            .map(|(_, value)| value)
    };
    let session = match session {
        Ok(session) => session,
        Err(err) => {
            server.stop();
            let _ = ready.send(Err(err)).await;
            return;
        }
    };
    let Some(session_id) = session.get("id").and_then(Value::as_str).map(str::to_owned) else {
        server.stop();
        let _ = ready
            .send(Err(AgentError::Protocol(
                "OpenCode session response omitted id".into(),
            )))
            .await;
        return;
    };
    let resolved_model = session
        .get("model")
        .and_then(|model| {
            Some(format!(
                "{}/{}",
                model.get("providerID")?.as_str()?,
                model.get("id")?.as_str()?
            ))
        })
        .or_else(|| opts.model.clone());
    let provider_commands = discover_commands(&server.http);
    let model = resolved_model
        .as_deref()
        .and_then(split_model_id)
        .map(|(provider_id, model_id)| (provider_id.to_owned(), model_id.to_owned()));
    let mut actor = OpenCodeActor {
        server,
        sse,
        events,
        mapper: OpenCodeMapper::new(session_id.clone()),
        model,
        variant: selected_variant(&opts.option_selections).map(str::to_owned),
        interaction_mode: opts.interaction_mode,
        approval_mode: opts.approval_mode,
        pending_permissions: HashSet::new(),
    };
    actor
        .emit(AgentEvent::SessionStarted {
            provider_session_id: session_id.clone(),
            resume: ResumeCursor(json!({"session_id":session_id})),
            model: resolved_model,
        })
        .await;
    actor
        .emit(AgentEvent::ProviderCommands {
            commands: provider_commands,
        })
        .await;
    if opts.launch_env.home.is_some() {
        actor
            .emit(AgentEvent::Warning {
                message:
                "OpenCode has no supported single-directory home override; custom environment variables still apply"
                    .into(),
             })
            .await;
    }
    if ready.send(Ok(())).await.is_err() {
        actor.shutdown().await;
        return;
    }

    let close_reason = loop {
        enum Input {
            Command(Result<SessionCommand, smol::channel::RecvError>),
            Event(Result<SseOutput, smol::channel::RecvError>),
        }
        let input = future::race(async { Input::Command(commands.recv().await) }, async {
            Input::Event(actor.sse.recv().await)
        })
        .await;
        match input {
            Input::Command(Ok(SessionCommand::Shutdown)) | Input::Command(Err(_)) => break None,
            Input::Command(Ok(command)) => {
                if let Err(err) = actor.handle_command(command).await {
                    actor
                        .emit(AgentEvent::Error {
                            message: err,
                            fatal: true,
                        })
                        .await;
                    break Some("OpenCode REST command failed".into());
                }
            }
            Input::Event(Ok(SseOutput::Event(event))) => actor.handle_event(&event).await,
            Input::Event(Ok(SseOutput::Error(err))) => {
                actor
                    .emit(AgentEvent::Error {
                        message: err.clone(),
                        fatal: true,
                    })
                    .await;
                break Some(err);
            }
            Input::Event(Ok(SseOutput::Eof)) | Input::Event(Err(_)) => {
                break Some("OpenCode SSE stream closed".into());
            }
        }
    };
    actor.shutdown().await;
    let reason = close_reason.map(|reason| {
        actor
            .server
            .stderr_tail
            .append_to(reason, "\nserver output:\n")
    });
    let _ = actor
        .events
        .send(AgentEvent::SessionClosed { reason })
        .await;
}

struct OpenCodeActor {
    server: OpenCodeServer,
    sse: Receiver<SseOutput>,
    events: Sender<AgentEvent>,
    mapper: OpenCodeMapper,
    model: Option<(String, String)>,
    variant: Option<String>,
    interaction_mode: InteractionMode,
    approval_mode: ApprovalMode,
    pending_permissions: HashSet<String>,
}

impl OpenCodeActor {
    async fn handle_event(&mut self, event: &Value) {
        let mapped = self.mapper.on_event(event);
        for request_id in mapped.permission_ids {
            self.pending_permissions.insert(request_id);
        }
        for event in mapped.events {
            self.emit(event).await;
        }
        if mapped.fetch_diff {
            match self.fetch_diff() {
                Ok(event) => self.emit(event).await,
                Err(err) => {
                    self.emit(AgentEvent::Warning {
                        message: format!("failed to fetch OpenCode session diff: {err}"),
                    })
                    .await
                }
            }
        }
    }

    async fn handle_command(&mut self, command: SessionCommand) -> Result<(), String> {
        match command {
            SessionCommand::SendTurn {
                delivery_id,
                text,
                options,
                attachments,
            } => {
                let mut parts = vec![json!({"type":"text","text":text})];
                parts.extend(attachments.into_iter().map(attachment_part));
                let mut body = json!({"parts":parts});
                if let Some((provider_id, model_id)) = &self.model {
                    body["model"] = json!({"providerID":provider_id,"modelID":model_id});
                }
                let interaction_mode = options
                    .as_ref()
                    .and_then(|options| options.interaction_mode)
                    .unwrap_or(self.interaction_mode);
                body["agent"] = json!(match interaction_mode {
                    InteractionMode::Build => "build",
                    InteractionMode::Plan => "plan",
                });
                let variant = options
                    .and_then(|options| options.effort)
                    .or_else(|| self.variant.clone());
                if let Some(variant) = variant {
                    body["variant"] = json!(variant);
                }
                let (status, _) = self
                    .server
                    .http
                    .post_json(
                        &format!("/session/{}/prompt_async", self.mapper.session_id),
                        &body,
                    )
                    .map_err(|err| err.to_string())?;
                if status != 204 {
                    return Err(format!(
                        "OpenCode prompt_async returned unexpected HTTP {status}"
                    ));
                }
                self.emit(AgentEvent::TurnAccepted { delivery_id }).await;
                Ok(())
            }
            SessionCommand::Interrupt => {
                self.mapper.interrupted = true;
                self.server
                    .http
                    .post_json(
                        &format!("/session/{}/abort", self.mapper.session_id),
                        &json!({}),
                    )
                    .map_err(|err| err.to_string())?;
                Ok(())
            }
            SessionCommand::RespondApproval {
                request_id,
                decision,
            } => {
                if !self.pending_permissions.remove(&request_id) {
                    return Ok(());
                }
                let reply = match decision {
                    ApprovalDecision::Approve => "once",
                    ApprovalDecision::ApproveForSession => "always",
                    ApprovalDecision::Deny
                    | ApprovalDecision::Cancel
                    | ApprovalDecision::Option(_) => "reject",
                };
                self.server
                    .http
                    .post_json(
                        &format!("/permission/{request_id}/reply"),
                        &json!({"reply":reply}),
                    )
                    .map_err(|err| err.to_string())?;
                if decision == ApprovalDecision::Cancel {
                    self.mapper.interrupted = true;
                    let _ = self.server.http.post_json(
                        &format!("/session/{}/abort", self.mapper.session_id),
                        &json!({}),
                    );
                }
                self.emit(AgentEvent::ApprovalResolved {
                    request_id,
                    decision,
                })
                .await;
                Ok(())
            }
            SessionCommand::SetApprovalMode(mode) => {
                if mode != self.approval_mode {
                    self.emit(AgentEvent::Warning {
                        message:
                            "OpenCode permission changes require restarting the per-session server"
                                .into(),
                    })
                    .await;
                }
                Ok(())
            }
            SessionCommand::SetInteractionMode(mode) => {
                self.interaction_mode = mode;
                Ok(())
            }
            SessionCommand::SetOption { id, value } if id == "reasoningEffort" => {
                self.variant = value.as_str().map(str::to_owned);
                Ok(())
            }
            SessionCommand::Steer { .. } => {
                self.emit(AgentEvent::Warning {
                    message: "OpenCode's server API does not support steering an active turn"
                        .into(),
                })
                .await;
                Ok(())
            }
            SessionCommand::RespondUserInput { .. } => {
                self.emit(AgentEvent::Warning {
                    message: "OpenCode structured questions are not yet bridged by this adapter"
                        .into(),
                })
                .await;
                Ok(())
            }
            SessionCommand::Rewind { .. } => {
                self.emit(AgentEvent::Warning {
                    message: "OpenCode rewind is not exposed by tcode's native adapter".into(),
                })
                .await;
                Ok(())
            }
            SessionCommand::SetOption { .. } | SessionCommand::Shutdown => Ok(()),
        }
    }

    fn fetch_diff(&self) -> Result<AgentEvent, AgentError> {
        let diff = self
            .server
            .http
            .get_json(&format!("/session/{}/diff", self.mapper.session_id))?;
        Ok(AgentEvent::TurnChangesUpdated {
            turn_id: self.mapper.turn_id(),
            changes: map_snapshot_diffs(&diff),
            completeness: ChangeCompleteness::Exact,
        })
    }

    async fn shutdown(&mut self) {
        let pending: Vec<String> = self.pending_permissions.drain().collect();
        for request_id in pending {
            let _ = self.server.http.post_json(
                &format!("/permission/{request_id}/reply"),
                &json!({"reply":"reject","message":"tcode session closed"}),
            );
        }
        self.server.stop();
    }

    async fn emit(&self, event: AgentEvent) {
        let _ = self.events.send(event).await;
    }
}

#[derive(Default)]
struct MappedEvent {
    events: Vec<AgentEvent>,
    permission_ids: Vec<String>,
    fetch_diff: bool,
}

struct OpenCodeMapper {
    session_id: String,
    turn_counter: u64,
    active_turn: Option<String>,
    last_turn: Option<String>,
    part_kinds: HashMap<String, DeltaKind>,
    user_messages: HashSet<String>,
    /// The same assistant usage is reported by both its step-finish part and
    /// message.updated reconciliation. Keying by message id prevents counting
    /// that full snapshot twice while still summing multi-message turns.
    turn_usages: HashMap<String, TokenUsage>,
    turn_usage: Option<TokenUsage>,
    cumulative_processed: u64,
    turn_failed: bool,
    interrupted: bool,
    last_served_model: Option<String>,
}

impl OpenCodeMapper {
    fn new(session_id: String) -> Self {
        Self {
            session_id,
            turn_counter: 0,
            active_turn: None,
            last_turn: None,
            part_kinds: HashMap::new(),
            user_messages: HashSet::new(),
            turn_usages: HashMap::new(),
            turn_usage: None,
            cumulative_processed: 0,
            turn_failed: false,
            interrupted: false,
            last_served_model: None,
        }
    }

    fn on_event(&mut self, event: &Value) -> MappedEvent {
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        let properties = event.get("properties").unwrap_or(&Value::Null);
        if let Some(session_id) = properties.get("sessionID").and_then(Value::as_str)
            && session_id != self.session_id
        {
            return MappedEvent::default();
        }
        let mut mapped = MappedEvent::default();
        match kind {
            "session.status" => match properties.pointer("/status/type").and_then(Value::as_str) {
                Some("busy") => mapped.events.extend(self.start_turn()),
                Some("retry") => mapped.events.push(AgentEvent::Warning {
                    message: format!(
                        "OpenCode retry {}: {}",
                        properties
                            .pointer("/status/attempt")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                        properties
                            .pointer("/status/message")
                            .and_then(Value::as_str)
                            .unwrap_or("provider error")
                    ),
                }),
                Some("idle") => mapped.events.extend(self.complete_turn()),
                _ => {}
            },
            "session.idle" => mapped.events.extend(self.complete_turn()),
            "message.part.delta" => {
                if properties
                    .get("messageID")
                    .and_then(Value::as_str)
                    .is_some_and(|id| self.user_messages.contains(id))
                {
                    return mapped;
                }
                let part_id = properties
                    .get("partID")
                    .and_then(Value::as_str)
                    .unwrap_or("opencode-part");
                let delta = properties
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !delta.is_empty() {
                    mapped.events.push(AgentEvent::Delta {
                        item_id: part_id.to_owned(),
                        kind: self
                            .part_kinds
                            .get(part_id)
                            .copied()
                            .unwrap_or(DeltaKind::AssistantText),
                        text: delta.to_owned(),
                    });
                }
            }
            "message.part.updated" => {
                let part = properties.get("part").unwrap_or(&Value::Null);
                if part
                    .get("messageID")
                    .and_then(Value::as_str)
                    .is_some_and(|id| self.user_messages.contains(id))
                {
                    return mapped;
                }
                mapped
                    .events
                    .extend(self.part_updated(part, properties.get("delta")));
                if part.get("type").and_then(Value::as_str) == Some("patch") {
                    mapped.fetch_diff = true;
                }
            }
            "message.updated" => {
                let info = properties.get("info").unwrap_or(&Value::Null);
                if info.get("role").and_then(Value::as_str) == Some("user") {
                    if let Some(id) = info.get("id").and_then(Value::as_str) {
                        self.user_messages.insert(id.to_owned());
                    }
                    return mapped;
                }
                if info.get("role").and_then(Value::as_str) == Some("assistant") {
                    if let Some(model) = info
                        .get("modelID")
                        .or_else(|| info.get("model_id"))
                        .and_then(Value::as_str)
                        && self.last_served_model.as_deref() != Some(model)
                    {
                        self.last_served_model = Some(model.to_owned());
                        mapped.events.push(AgentEvent::ServedModel {
                            model: model.to_owned(),
                            reason: None,
                        });
                    }
                    if let Some(error) = info.get("error") {
                        self.turn_failed = true;
                        mapped.events.push(AgentEvent::Error {
                            message: error_message(error),
                            fatal: false,
                        });
                    }
                    if info.pointer("/time/completed").is_some()
                        && let Some(mut usage) = usage_from_tokens(info.get("tokens"))
                    {
                        usage.cost_usd = info.get("cost").and_then(Value::as_f64);
                        let key = info
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("opencode-message");
                        self.set_usage(key, usage, &mut mapped.events);
                    }
                }
            }
            "session.diff" => {
                mapped.events.push(AgentEvent::TurnChangesUpdated {
                    turn_id: self.turn_id(),
                    changes: map_snapshot_diffs(properties.get("diff").unwrap_or(&Value::Null)),
                    completeness: ChangeCompleteness::Exact,
                });
            }
            "session.error" => {
                self.turn_failed = true;
                mapped.events.push(AgentEvent::Error {
                    message: properties
                        .get("error")
                        .map(error_message)
                        .unwrap_or_else(|| "OpenCode reported a session error".into()),
                    fatal: false,
                });
            }
            "permission.asked" | "permission.updated" | "permission.v2.asked" => {
                if let Some(request) = map_permission(properties) {
                    let request = ApprovalRequest {
                        turn_id: self.active_turn.clone(),
                        ..request
                    };
                    mapped.permission_ids.push(request.id.clone());
                    mapped.events.push(AgentEvent::ApprovalRequested(request));
                }
            }
            "session.compacted" => mapped.events.push(AgentEvent::ContextCompacted),
            _ => {}
        }
        mapped
    }

    fn start_turn(&mut self) -> Vec<AgentEvent> {
        if self.active_turn.is_some() {
            return Vec::new();
        }
        self.turn_counter += 1;
        let turn_id = format!("opencode-turn-{}", self.turn_counter);
        self.active_turn = Some(turn_id.clone());
        self.turn_usages.clear();
        self.turn_usage = None;
        self.turn_failed = false;
        vec![AgentEvent::TurnStarted { turn_id }]
    }

    fn turn_id(&self) -> String {
        self.active_turn
            .clone()
            .or_else(|| self.last_turn.clone())
            .unwrap_or_else(|| "opencode-turn".into())
    }

    fn complete_turn(&mut self) -> Vec<AgentEvent> {
        let Some(turn_id) = self.active_turn.take() else {
            return Vec::new();
        };
        self.last_turn = Some(turn_id.clone());
        let status = if self.interrupted {
            TurnStatus::Interrupted
        } else if self.turn_failed {
            TurnStatus::Failed
        } else {
            TurnStatus::Completed
        };
        self.interrupted = false;
        vec![AgentEvent::TurnCompleted {
            turn_id,
            status,
            usage: self.turn_usage,
        }]
    }

    fn part_updated(&mut self, part: &Value, explicit_delta: Option<&Value>) -> Vec<AgentEvent> {
        let part_id = part
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("opencode-part")
            .to_owned();
        match part.get("type").and_then(Value::as_str) {
            Some("text") | Some("reasoning") => {
                let reasoning = part.get("type").and_then(Value::as_str) == Some("reasoning");
                let delta_kind = if reasoning {
                    DeltaKind::ReasoningText
                } else {
                    DeltaKind::AssistantText
                };
                self.part_kinds.insert(part_id.clone(), delta_kind);
                let text = part
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let mut events = Vec::new();
                if let Some(delta) = explicit_delta.and_then(Value::as_str)
                    && !delta.is_empty()
                {
                    events.push(AgentEvent::Delta {
                        item_id: part_id.clone(),
                        kind: delta_kind,
                        text: delta.to_owned(),
                    });
                }
                let item = ThreadItem {
                    id: part_id,
                    parent_item_id: None,
                    content: if reasoning {
                        ItemContent::Reasoning { text }
                    } else {
                        ItemContent::AssistantMessage { text }
                    },
                };
                events.push(if part.pointer("/time/end").is_some() {
                    AgentEvent::ItemCompleted(item)
                } else {
                    AgentEvent::ItemUpdated(item)
                });
                events
            }
            Some("tool") => self.tool_updated(part),
            Some("step-finish") => {
                if let Some(mut usage) = usage_from_tokens(part.get("tokens")) {
                    // The step-finish snapshot and completed message carry the
                    // same cumulative per-message cost. Recording it here lets
                    // the later reconciliation coalesce exactly.
                    usage.cost_usd = part.get("cost").and_then(Value::as_f64);
                    let mut events = Vec::new();
                    let key = part
                        .get("messageID")
                        .and_then(Value::as_str)
                        .unwrap_or(&part_id);
                    self.set_usage(key, usage, &mut events);
                    events
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    fn tool_updated(&mut self, part: &Value) -> Vec<AgentEvent> {
        let call_id = part
            .get("callID")
            .and_then(Value::as_str)
            .unwrap_or("opencode-tool")
            .to_owned();
        let name = part
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or("tool")
            .to_owned();
        let state = part.get("state").unwrap_or(&Value::Null);
        let input = state.get("input").cloned().unwrap_or(Value::Null);
        let (status, output) = match state.get("status").and_then(Value::as_str) {
            Some("completed") => (
                ItemStatus::Completed,
                state
                    .get("output")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            ),
            Some("error") => {
                self.turn_failed = true;
                (
                    ItemStatus::Failed,
                    state
                        .get("error")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                )
            }
            _ => (ItemStatus::InProgress, None),
        };
        let exit_code = state
            .pointer("/metadata/exit")
            .and_then(Value::as_i64)
            .and_then(|number| i32::try_from(number).ok());
        let item = open_code_tool_item(&call_id, &name, input, output, exit_code, status);
        vec![match state.get("status").and_then(Value::as_str) {
            Some("pending") => AgentEvent::ItemStarted(item),
            Some("completed" | "error") => AgentEvent::ItemCompleted(item),
            _ => AgentEvent::ItemUpdated(item),
        }]
    }

    fn set_usage(&mut self, key: &str, usage: TokenUsage, events: &mut Vec<AgentEvent>) {
        if self
            .turn_usages
            .get(key)
            .is_some_and(|previous| *previous == usage)
        {
            return;
        }
        let previous = self
            .turn_usages
            .insert(key.to_owned(), usage)
            .map(processed_tokens)
            .unwrap_or(0);
        let current = processed_tokens(usage);
        if current >= previous {
            self.cumulative_processed =
                self.cumulative_processed.saturating_add(current - previous);
        } else {
            self.cumulative_processed =
                self.cumulative_processed.saturating_sub(previous - current);
        }
        let mut aggregate = TokenUsage::default();
        for usage in self.turn_usages.values().copied() {
            aggregate.merge(usage);
        }
        let aggregate = TokenUsage {
            total_processed_tokens: Some(self.cumulative_processed),
            ..aggregate
        };
        self.turn_usage = Some(aggregate);
        events.push(AgentEvent::TokenUsage(aggregate));
    }
}

fn processed_tokens(usage: TokenUsage) -> u64 {
    usage.used_tokens.unwrap_or_else(|| {
        usage
            .input_tokens
            .unwrap_or(0)
            .saturating_add(usage.output_tokens.unwrap_or(0))
            .saturating_add(usage.cached_input_tokens.unwrap_or(0))
    })
}

fn open_code_tool_item(
    id: &str,
    name: &str,
    input: Value,
    output: Option<String>,
    exit_code: Option<i32>,
    status: ItemStatus,
) -> ThreadItem {
    let content = if name == "bash" {
        ItemContent::CommandExecution {
            command: input
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            output: output.unwrap_or_default(),
            exit_code,
            status,
        }
    } else {
        ItemContent::ToolCall {
            name: name.to_owned(),
            input,
            output,
            status,
        }
    };
    ThreadItem {
        id: id.to_owned(),
        parent_item_id: None,
        content,
    }
}

fn map_permission(properties: &Value) -> Option<ApprovalRequest> {
    let id = properties.get("id")?.as_str()?.to_owned();
    let name = properties
        .get("permission")
        .or_else(|| properties.get("action"))
        .and_then(Value::as_str)
        .unwrap_or("tool")
        .to_owned();
    let metadata = properties.get("metadata").cloned().unwrap_or(Value::Null);
    let resources: Vec<String> = properties
        .get("patterns")
        .or_else(|| properties.get("resources"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    let kind = match name.as_str() {
        "bash" => ApprovalKind::ExecCommand {
            command: metadata
                .get("command")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| resources.first().cloned())
                .unwrap_or_default(),
            cwd: metadata
                .get("cwd")
                .and_then(Value::as_str)
                .map(str::to_owned),
            reason: metadata
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        "edit" | "write" => ApprovalKind::FileChange {
            changes: resources
                .iter()
                .map(|path| FileChange {
                    path: path.clone(),
                    kind: FileChangeKind::Modify,
                    diff: None,
                })
                .collect(),
            reason: metadata
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        "read" => ApprovalKind::FileRead {
            detail: resources.join(", "),
        },
        _ => ApprovalKind::ToolUse {
            name: name.clone(),
            input: metadata.clone(),
            detail: if resources.is_empty() {
                metadata.to_string()
            } else {
                resources.join(", ")
            },
        },
    };
    Some(ApprovalRequest {
        id,
        turn_id: None,
        kind,
        options: Vec::new(),
    })
}

fn usage_from_tokens(tokens: Option<&Value>) -> Option<TokenUsage> {
    let tokens = tokens?;
    let input = crate::json_u64(tokens.get("input"));
    let reasoning = crate::json_u64(tokens.get("reasoning")).unwrap_or(0);
    let output =
        crate::json_u64(tokens.get("output")).map(|output| output.saturating_add(reasoning));
    let cache_read = crate::json_u64(tokens.pointer("/cache/read"));
    let cache_write = crate::json_u64(tokens.pointer("/cache/write")).unwrap_or(0);
    (input.is_some() || output.is_some() || cache_read.is_some()).then_some(TokenUsage {
        input_tokens: input,
        cached_input_tokens: cache_read,
        output_tokens: output,
        used_tokens: input
            .unwrap_or(0)
            .checked_add(output.unwrap_or(0))
            .and_then(|total| total.checked_add(cache_read.unwrap_or(0)))
            .and_then(|total| total.checked_add(cache_write)),
        ..Default::default()
    })
}

fn map_snapshot_diffs(value: &Value) -> Vec<FileChange> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|diff| {
            let path = diff.get("file")?.as_str()?.to_owned();
            let kind = match diff.get("status").and_then(Value::as_str) {
                Some("added") => FileChangeKind::Create,
                Some("deleted") => FileChangeKind::Delete,
                _ => FileChangeKind::Modify,
            };
            Some(FileChange {
                path,
                kind,
                diff: diff.get("patch").and_then(Value::as_str).map(str::to_owned),
            })
        })
        .collect()
}

fn error_message(error: &Value) -> String {
    error
        .get("data")
        .and_then(|data| data.get("message"))
        .or_else(|| error.get("message"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| error.to_string())
}

fn map_models(catalog: &Value) -> Vec<ModelSpec> {
    let defaults = catalog.get("default").and_then(Value::as_object);
    let mut models = Vec::new();
    for provider in catalog
        .get("providers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(provider_id) = provider.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(entries) = provider.get("models").and_then(Value::as_object) else {
            continue;
        };
        for (model_id, model) in entries {
            if model.get("status").and_then(Value::as_str) == Some("deprecated") {
                continue;
            }
            let mut options = Vec::new();
            if let Some(variants) = model.get("variants").and_then(Value::as_object)
                && !variants.is_empty()
            {
                let mut variants: Vec<_> = variants.keys().cloned().collect();
                const EFFORT_ORDER: &[&str] =
                    &["none", "minimal", "low", "medium", "high", "xhigh", "max"];
                variants.sort_by(|left, right| {
                    let left_order = EFFORT_ORDER
                        .iter()
                        .position(|effort| effort == left)
                        .unwrap_or(usize::MAX);
                    let right_order = EFFORT_ORDER
                        .iter()
                        .position(|effort| effort == right)
                        .unwrap_or(usize::MAX);
                    left_order.cmp(&right_order).then_with(|| left.cmp(right))
                });
                options.push(OptionDescriptor::Select {
                    id: "reasoningEffort".into(),
                    label: "Thinking".into(),
                    options: variants
                        .into_iter()
                        .map(|variant| SelectOption {
                            label: title_case(&variant),
                            value: variant,
                            description: None,
                        })
                        .collect(),
                    default_value: None,
                });
            }
            models.push(ModelSpec {
                id: format!("{provider_id}/{model_id}"),
                display_name: model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(model_id)
                    .to_owned(),
                is_default: defaults
                    .and_then(|defaults| defaults.get(provider_id))
                    .and_then(Value::as_str)
                    == Some(model_id),
                options,
            });
        }
    }
    models
}

/// `/config/providers` is the project-scoped configured catalog, while
/// `/provider` carries connection state and the server's resolved defaults.
/// Prefer the configured list, filling missing defaults (and an empty catalog)
/// from the provider endpoint so both authoritative views participate.
fn reconcile_provider_catalog(catalog: &mut Value, provider_state: &Value) {
    let configured_is_empty = catalog
        .get("providers")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty);
    if configured_is_empty {
        let connected: HashSet<&str> = provider_state
            .get("connected")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let providers = provider_state
            .get("all")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|provider| {
                provider
                    .get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| connected.contains(id))
            })
            .cloned()
            .collect();
        catalog["providers"] = Value::Array(providers);
    }

    let mut defaults = catalog
        .get("default")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(provider_defaults) = provider_state.get("default").and_then(Value::as_object) {
        for (provider, model) in provider_defaults {
            defaults
                .entry(provider.clone())
                .or_insert_with(|| model.clone());
        }
    }
    catalog["default"] = Value::Object(defaults);
}

fn title_case(value: &str) -> String {
    let mut characters = value.chars();
    characters
        .next()
        .map(|first| first.to_uppercase().collect::<String>() + characters.as_str())
        .unwrap_or_default()
}

fn discover_commands(http: &HttpClient) -> Vec<ProviderCommand> {
    let mut commands = http
        .get_json("/command")
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|command| {
            let name = command.get("name")?.as_str()?.trim();
            (!name.is_empty()).then(|| ProviderCommand {
                name: name.to_owned(),
                description: command
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                kind: ProviderCommandKind::Command,
            })
        })
        .collect::<Vec<_>>();
    // Current OpenCode includes skill-backed entries in `/command` as well as
    // `/skill`. Prefer the command representation instead of showing the same
    // slash entry twice.
    let mut seen: HashSet<String> = commands
        .iter()
        .map(|command| command.name.clone())
        .collect();
    commands.extend(
        http.get_json("/skill")
            .ok()
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|skill| {
                let name = skill.get("name")?.as_str()?.trim();
                (!name.is_empty() && seen.insert(name.to_owned())).then(|| ProviderCommand {
                    name: name.to_owned(),
                    description: skill
                        .get("description")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    kind: ProviderCommandKind::Skill,
                })
            }),
    );
    commands
}

fn attachment_part(attachment: Attachment) -> Value {
    json!({
        "type":"file",
        "mime":attachment.media_type,
        "url":format!("data:{};base64,{}", attachment.media_type, attachment.data_base64)
    })
}

fn split_model_id(model: &str) -> Option<(&str, &str)> {
    model
        .split_once('/')
        .filter(|(provider, id)| !provider.is_empty() && !id.is_empty())
}

fn selected_variant(selections: &[OptionSelection]) -> Option<&str> {
    selections
        .iter()
        .find(|selection| selection.id == "reasoningEffort")
        .and_then(|selection| selection.value.as_str())
}

fn resume_session_id(resume: &Option<ResumeCursor>) -> Option<&str> {
    resume.as_ref()?.0.get("session_id").and_then(Value::as_str)
}

struct OpenCodeServer {
    child: Child,
    http: HttpClient,
    stderr_tail: crate::process::StderrTail,
}

impl OpenCodeServer {
    fn spawn(
        binary_path: Option<&Path>,
        cwd: &Path,
        launch_env: &LaunchEnv,
        approval_mode: ApprovalMode,
        extra_args: &[String],
        registrations: &[&crate::McpRegistration],
    ) -> Result<Self, AgentError> {
        let binary = crate::resolve_binary(binary_path, "opencode")?;
        let port = reserve_loopback_port()?;
        let password = uuid::Uuid::new_v4().simple().to_string();
        let mut command = crate::process::command(&binary);
        command
            .arg("serve")
            .args(extra_args)
            .arg("--hostname")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(port.to_string())
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in launch_env.pairs(ProviderKind::OpenCode) {
            command.env(key, value);
        }
        // These values define the transport and permission boundary. Apply
        // them after profile env so they cannot be replaced by a stale custom
        // variable from Settings.
        command
            .env("OPENCODE_SERVER_PASSWORD", &password)
            .env("OPENCODE_PERMISSION", permission_policy(approval_mode));
        if let Some(config) = opencode_config_content(launch_env, registrations)? {
            command.env("OPENCODE_CONFIG_CONTENT", config);
        }
        let mut child = command
            .spawn()
            .map_err(|err| AgentError::Spawn(format!("spawning `{}`: {err}", binary.display())))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AgentError::Spawn("OpenCode child stdout missing".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| AgentError::Spawn("OpenCode child stderr missing".into()))?;
        let tail = crate::process::StderrTail::default();
        let _ = tail.spawn(stdout, "opencode-stdout", "OpenCode");
        let _ = tail.spawn(stderr, "opencode-stderr", "OpenCode");
        Ok(Self {
            child,
            http: HttpClient::new(format!("http://127.0.0.1:{port}"), password),
            stderr_tail: tail,
        })
    }

    fn wait_healthy(&mut self) -> Result<(), AgentError> {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut last_error = None;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait()? {
                return Err(AgentError::Provider(self.stderr_tail.append_to(
                    format!("OpenCode server exited during startup ({status})"),
                    "\nserver output:\n",
                )));
            }
            match self
                .http
                .get_json_with(&self.http.health_agent, "/global/health")
            {
                Ok(health)
                    if health.get("healthy").and_then(Value::as_bool) == Some(true)
                        && health.get("version").and_then(Value::as_str).is_some() =>
                {
                    return Ok(());
                }
                Ok(health) => last_error = Some(format!("unexpected health response: {health}")),
                Err(err) => last_error = Some(err.to_string()),
            }
            std::thread::sleep(Duration::from_millis(75));
        }
        Err(AgentError::Protocol(self.stderr_tail.append_to(
            format!(
                "timed out waiting for OpenCode health: {}",
                last_error.unwrap_or_else(|| "no response".into())
            ),
            "\nserver output:\n",
        )))
    }

    fn subscribe(&self) -> Result<Receiver<SseOutput>, AgentError> {
        // REST calls have a finite read timeout. The event stream is meant to
        // stay idle indefinitely, so give its dedicated reader a long socket
        // timeout rather than inheriting the REST agent's 30-second limit.
        let sse_agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(1))
            .timeout_read(Duration::from_secs(24 * 60 * 60))
            .build();
        let response = sse_agent
            .get(&format!("{}/event", self.http.base))
            .set("Authorization", &self.http.authorization)
            .call()
            .map_err(http_error)?;
        let (sender, receiver) = smol::channel::unbounded();
        std::thread::Builder::new()
            .name("opencode-sse".into())
            .spawn(move || read_sse(response.into_reader(), sender))
            .map_err(|err| AgentError::Spawn(format!("spawning OpenCode SSE reader: {err}")))?;
        Ok(receiver)
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct HttpClient {
    base: String,
    authorization: String,
    agent: ureq::Agent,
    health_agent: ureq::Agent,
}

impl HttpClient {
    fn new(base: String, password: String) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(1))
            .timeout_read(Duration::from_secs(30))
            .timeout_write(Duration::from_secs(5))
            .build();
        // A freshly-bound OpenCode server can accept a connection before its
        // health handler is ready. Keep these probes short so wait_healthy's
        // 15-second deadline is not swallowed by one general REST timeout.
        let health_agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_millis(500))
            .timeout_read(Duration::from_secs(1))
            .build();
        Self {
            base,
            authorization: format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("opencode:{password}"))
            ),
            agent,
            health_agent,
        }
    }

    fn get_json(&self, path: &str) -> Result<Value, AgentError> {
        self.get_json_with(&self.agent, path)
    }

    fn get_json_with(&self, agent: &ureq::Agent, path: &str) -> Result<Value, AgentError> {
        let response = agent
            .get(&format!("{}{path}", self.base))
            .set("Authorization", &self.authorization)
            .call()
            .map_err(http_error)?;
        parse_response(response)
    }

    fn post_json(&self, path: &str, body: &Value) -> Result<(u16, Value), AgentError> {
        let response = self
            .agent
            .post(&format!("{}{path}", self.base))
            .set("Authorization", &self.authorization)
            .set("Content-Type", "application/json")
            .send_string(&body.to_string())
            .map_err(http_error)?;
        let status = response.status();
        let value = parse_response(response)?;
        Ok((status, value))
    }
}

fn parse_response(response: ureq::Response) -> Result<Value, AgentError> {
    if response.status() == 204 {
        return Ok(Value::Null);
    }
    let mut text = String::new();
    response.into_reader().read_to_string(&mut text)?;
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text)
        .map_err(|err| AgentError::Protocol(format!("invalid OpenCode JSON response: {err}")))
}

fn http_error(error: ureq::Error) -> AgentError {
    match error {
        ureq::Error::Status(status, response) => {
            let mut body = String::new();
            let _ = response
                .into_reader()
                .take(64 * 1024)
                .read_to_string(&mut body);
            AgentError::Provider(format!(
                "OpenCode HTTP {status}: {}",
                if body.trim().is_empty() {
                    "empty response".into()
                } else {
                    body
                }
            ))
        }
        ureq::Error::Transport(error) => AgentError::Protocol(format!("OpenCode HTTP: {error}")),
    }
}

enum SseOutput {
    Event(Value),
    Eof,
    Error(String),
}

fn read_sse(reader: impl Read, sender: Sender<SseOutput>) {
    let mut data = Vec::new();
    for line in BufReader::new(reader).lines() {
        match line {
            Ok(line) => {
                if line.is_empty() {
                    if !data.is_empty() {
                        let payload = data.join("\n");
                        data.clear();
                        match serde_json::from_str(&payload) {
                            Ok(value) => {
                                if sender.send_blocking(SseOutput::Event(value)).is_err() {
                                    return;
                                }
                            }
                            Err(err) => {
                                let _ = sender.send_blocking(SseOutput::Error(format!(
                                    "invalid OpenCode SSE JSON: {err}"
                                )));
                                return;
                            }
                        }
                    }
                } else if let Some(value) = line.strip_prefix("data:") {
                    data.push(value.strip_prefix(' ').unwrap_or(value).to_owned());
                }
            }
            Err(err) => {
                let _ = sender.send_blocking(SseOutput::Error(format!(
                    "failed reading OpenCode SSE: {err}"
                )));
                return;
            }
        }
    }
    let _ = sender.send_blocking(SseOutput::Eof);
}

async fn wait_for_server_connected(events: &Receiver<SseOutput>) -> Result<(), AgentError> {
    future::race(
        async {
            loop {
                match events.recv().await {
                    Ok(SseOutput::Event(event))
                        if event.get("type").and_then(Value::as_str)
                            == Some("server.connected") =>
                    {
                        return Ok(());
                    }
                    Ok(SseOutput::Event(_)) => {}
                    Ok(SseOutput::Error(error)) => {
                        return Err(AgentError::Protocol(error));
                    }
                    Ok(SseOutput::Eof) | Err(_) => {
                        return Err(AgentError::Protocol(
                            "OpenCode SSE closed before server.connected".into(),
                        ));
                    }
                }
            }
        },
        async {
            smol::Timer::after(Duration::from_secs(10)).await;
            Err(AgentError::Protocol(
                "timed out waiting for OpenCode server.connected".into(),
            ))
        },
    )
    .await
}

fn reserve_loopback_port() -> Result<u16, AgentError> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    Ok(listener.local_addr()?.port())
}

fn permission_policy(mode: ApprovalMode) -> String {
    match mode {
        ApprovalMode::Supervised => json!({
            "*":"allow",
            "edit":"ask",
            "bash":"ask",
            "external_directory":"ask"
        }),
        ApprovalMode::ReadOnly => json!({
            "*":"deny",
            "read":"allow",
            "glob":"allow",
            "grep":"allow",
            "list":"allow",
            "webfetch":"allow",
            "websearch":"allow"
        }),
        ApprovalMode::AutoAcceptEdits => json!({
            "*":"allow",
            "bash":"ask",
            "external_directory":"ask"
        }),
        ApprovalMode::FullAccess => json!({"*":"allow"}),
    }
    .to_string()
}

fn opencode_config_content(
    launch_env: &LaunchEnv,
    registrations: &[&crate::McpRegistration],
) -> Result<Option<String>, AgentError> {
    if registrations.is_empty() {
        return Ok(None);
    }
    let inline = launch_env
        .env
        .iter()
        .rev()
        .find(|(key, _)| key == "OPENCODE_CONFIG_CONTENT")
        .map(|(_, value)| value.clone())
        .or_else(|| std::env::var("OPENCODE_CONFIG_CONTENT").ok());
    let mut config = match inline {
        Some(inline) => {
            let config: Value = serde_json::from_str(&inline).map_err(|err| {
                AgentError::Protocol(format!(
                    "cannot merge tcode MCP servers into OPENCODE_CONFIG_CONTENT: {err}"
                ))
            })?;
            if !config.is_object() {
                return Err(AgentError::Protocol(
                    "cannot merge tcode MCP servers into non-object OPENCODE_CONFIG_CONTENT".into(),
                ));
            }
            config
        }
        None => json!({}),
    };
    let mut mcp = config
        .get("mcp")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for registration in registrations {
        mcp.insert(
            registration.name.clone(),
            json!({
                "type":"remote",
                "url":registration.url,
                "headers":{"Authorization":format!("Bearer {}", registration.bearer_token)},
                "enabled":true
            }),
        );
    }
    config["mcp"] = Value::Object(mcp);
    Ok(Some(config.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_recorded_sse_fixture_and_filters_other_sessions() {
        let mut mapper = OpenCodeMapper::new("ses_target".into());
        let mut events = Vec::new();
        let mut permission_ids = Vec::new();
        let mut fetched_diff = false;
        for line in include_str!("../tests/fixtures/opencode/sse_events.jsonl").lines() {
            let event: Value = serde_json::from_str(line).unwrap();
            let mapped = mapper.on_event(&event);
            events.extend(mapped.events);
            permission_ids.extend(mapped.permission_ids);
            fetched_diff |= mapped.fetch_diff;
        }
        assert!(matches!(events[0], AgentEvent::TurnStarted { .. }));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::Delta { kind: DeltaKind::AssistantText, text, .. } if text == "PONG"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::Delta { kind: DeltaKind::ReasoningText, text, .. } if text == "Thinking"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::ToolCall {
                    status: ItemStatus::Completed,
                    ..
                },
                ..
            })
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::TurnChangesUpdated { completeness: ChangeCompleteness::Exact, changes, .. } if changes.len() == 1
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::TokenUsage(TokenUsage {
                input_tokens: Some(10),
                output_tokens: Some(5),
                ..
            })
        )));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AgentEvent::TokenUsage(_)))
                .count(),
            1,
            "identical step-finish and message reconciliation snapshots are coalesced"
        );
        assert_eq!(permission_ids, vec!["per_1"]);
        assert!(fetched_diff);
        assert!(matches!(
            events.last(),
            Some(AgentEvent::TurnCompleted {
                status: TurnStatus::Completed,
                usage: Some(TokenUsage {
                    input_tokens: Some(10),
                    output_tokens: Some(5),
                    total_processed_tokens: Some(19),
                    cost_usd: Some(0.1),
                    ..
                }),
                ..
            })
        ));
        assert!(!events.iter().any(|event| matches!(
            event,
            AgentEvent::Delta { text, .. } if text == "WRONG SESSION"
        )));
        assert!(!events.iter().any(|event| matches!(
            event,
            AgentEvent::ItemUpdated(ThreadItem {
                content: ItemContent::AssistantMessage { text },
                ..
            }) if text == "DO NOT ECHO"
        )));
    }

    #[test]
    fn late_diff_stays_attached_to_the_completed_turn() {
        let mut mapper = OpenCodeMapper::new("ses_target".into());
        let started = mapper.on_event(&json!({
            "type":"session.status",
            "properties":{"sessionID":"ses_target","status":{"type":"busy"}}
        }));
        let turn_id = match started.events.as_slice() {
            [AgentEvent::TurnStarted { turn_id }] => turn_id.clone(),
            events => panic!("unexpected start events: {events:?}"),
        };
        mapper.on_event(&json!({
            "type":"session.idle",
            "properties":{"sessionID":"ses_target"}
        }));
        let late = mapper.on_event(&json!({
            "type":"session.diff",
            "properties":{"sessionID":"ses_target","diff":[]}
        }));
        assert!(matches!(
            late.events.as_slice(),
            [AgentEvent::TurnChangesUpdated { turn_id: id, .. }] if id == &turn_id
        ));
    }

    #[test]
    fn approval_modes_generate_fail_safe_policies() {
        let read_only: Value =
            serde_json::from_str(&permission_policy(ApprovalMode::ReadOnly)).unwrap();
        assert_eq!(read_only["*"], "deny");
        assert_eq!(read_only["read"], "allow");
        let supervised: Value =
            serde_json::from_str(&permission_policy(ApprovalMode::Supervised)).unwrap();
        assert_eq!(supervised["edit"], "ask");
        assert_eq!(supervised["bash"], "ask");
    }

    #[test]
    fn maps_current_v2_and_legacy_permission_event_names() {
        for (event_type, properties) in [
            (
                "permission.v2.asked",
                json!({
                    "id":"per_v2","sessionID":"ses_target","action":"edit",
                    "resources":["src/lib.rs"],"metadata":{}
                }),
            ),
            (
                "permission.updated",
                json!({
                    "id":"per_legacy","sessionID":"ses_target","permission":"bash",
                    "patterns":["cargo test"],"metadata":{},"always":[]
                }),
            ),
        ] {
            let mut mapper = OpenCodeMapper::new("ses_target".into());
            let mapped = mapper.on_event(&json!({"type":event_type,"properties":properties}));
            assert_eq!(mapped.permission_ids.len(), 1, "{event_type}");
            assert!(matches!(
                mapped.events.as_slice(),
                [AgentEvent::ApprovalRequested(_)]
            ));
        }
    }

    #[test]
    fn maps_only_configured_provider_models() {
        let models = map_models(&json!({
            "default":{"openai":"gpt-test"},
            "providers":[{"id":"openai","models":{"gpt-test":{
                "name":"GPT Test","status":"active","variants":{
                    "alpha":{},"high":{},"low":{},"max":{},"medium":{},
                    "minimal":{},"none":{},"xhigh":{},"zeta":{}
                }
            }}}]
        }));
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "openai/gpt-test");
        assert!(models[0].is_default);
        let [
            OptionDescriptor::Select {
                id, label, options, ..
            },
        ] = models[0].options.as_slice()
        else {
            panic!("expected one select descriptor");
        };
        assert_eq!(id, "reasoningEffort");
        assert_eq!(label, "Thinking");
        assert_eq!(
            options
                .iter()
                .map(|option| option.value.as_str())
                .collect::<Vec<_>>(),
            [
                "none", "minimal", "low", "medium", "high", "xhigh", "max", "alpha", "zeta"
            ]
        );
    }

    #[test]
    fn configured_catalog_reconciles_provider_defaults_and_fallback() {
        let provider = json!({
            "all":[{"id":"openai","models":{"gpt-test":{"name":"GPT Test"}}}],
            "default":{"openai":"gpt-test"},
            "connected":["openai"]
        });
        let mut empty = json!({"providers":[],"default":{}});
        reconcile_provider_catalog(&mut empty, &provider);
        let models = map_models(&empty);
        assert_eq!(models.len(), 1);
        assert!(models[0].is_default);

        let mut configured = json!({
            "providers":[{"id":"openai","models":{"custom":{"name":"Custom"}}}],
            "default":{}
        });
        reconcile_provider_catalog(&mut configured, &provider);
        assert_eq!(
            configured["providers"][0]["models"]["custom"]["name"],
            "Custom"
        );
        assert_eq!(configured["default"]["openai"], "gpt-test");
    }

    #[test]
    fn assistant_model_change_is_emitted_once() {
        let mut mapper = OpenCodeMapper::new("ses".into());
        let event = json!({
            "type": "message.updated",
            "properties": {
                "sessionID": "ses",
                "info": {"role": "assistant", "id": "msg", "modelID": "gpt-5"}
            }
        });
        let first = mapper.on_event(&event).events;
        let repeat = mapper.on_event(&event).events;
        assert!(matches!(
            first.as_slice(),
            [AgentEvent::ServedModel { model, reason: None }] if model == "gpt-5"
        ));
        assert!(repeat.is_empty());
    }

    #[test]
    fn bash_metadata_exit_populates_exit_code() {
        let mut mapper = OpenCodeMapper::new("ses".into());
        let event = json!({
            "type": "message.part.updated",
            "properties": {
                "sessionID": "ses",
                "part": {
                    "type": "tool",
                    "callID": "call",
                    "tool": "bash",
                    "state": {
                        "status": "completed",
                        "input": {"command": "false"},
                        "output": "",
                        "metadata": {"exit": 7}
                    }
                }
            }
        });
        assert!(matches!(
            mapper.on_event(&event).events.as_slice(),
            [AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::CommandExecution {
                    exit_code: Some(7),
                    ..
                },
                ..
            })]
        ));
    }
}
