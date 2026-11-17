//! Native OpenCode provider for OpenCode 2.x: one private `opencode serve
//! --stdio` per Tcode session, the server's `/api` routes, and its
//! `/api/event` SSE stream.

use crate::{ForkPoint, LaterHistory, RewindFailure, RewindFailureReason, RewindFile, RewindFiles};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine as _;
use serde_json::{Map, Value, json};
use smol::channel::{Receiver, Sender};
use smol::future;

use crate::actor::{self, EventSenderExt as _, SessionActor, TransportOutcome};
use crate::{
    AgentError, AgentEvent, ApprovalDecision, ApprovalKind, ApprovalOption, ApprovalOptionKind,
    ApprovalRequest, AskedQuestion, Attachment, BackgroundKind, BackgroundTask, ChangeCompleteness,
    Compaction, DeltaKind, FileChange, FileChangeKind, ItemContent, ItemStatus, LaunchEnv,
    ModelSpec, OptionDescriptor, OptionSelection, PlanStep, PlanStepStatus, ProviderCommand,
    ProviderCommandKind, ProviderKind, ResumeCursor, RewindMode, SearchHit, SearchScope,
    SelectOption, SessionCommand, SessionHandle, SessionOptions, ThreadItem, TokenUsage,
    ToolFamily, ToolOutput, UserInputDelivery, UserInputOption, UserInputQuestion, WebLink,
};

/// The directory OpenCode launched with `launch_env` keeps its sessions,
/// database and sign-ins in: `opencode` under `XDG_DATA_HOME`, else under
/// `~/.local/share`, on every platform.
pub fn data_dir(launch_env: &LaunchEnv) -> Option<PathBuf> {
    launch_env
        .child_path(ProviderKind::OpenCode, "XDG_DATA_HOME")
        .or_else(|| {
            launch_env
                .child_home(ProviderKind::OpenCode)
                .map(|home| home.join(".local/share"))
        })
        .map(|data| data.join("opencode"))
}

const PERMISSION_MODE: &str = "permission_mode";
const EFFORT: &str = crate::EFFORT_OPTION_ID;
const AGENT: &str = "agent";
/// Tcode's entry among the session's API-managed instructions.
const INSTRUCTIONS_KEY: &str = "tcode";
/// How long a model listing, which no one can stop, may take: a first start
/// migrates OpenCode's database before the server listens.
const LISTING_TIMEOUT: Duration = Duration::from_secs(60);
/// The server streams no shell output; its own clients poll the shell's
/// output route while the command runs.
const SHELL_POLL_INTERVAL: Duration = Duration::from_millis(250);

pub async fn start(opts: SessionOptions) -> Result<SessionHandle, AgentError> {
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
    processes: &crate::SessionProcesses,
) -> Result<Vec<ModelSpec>, AgentError> {
    let listing = crate::process::unblock({
        let processes = processes.clone();
        move || {
            let cwd = std::env::current_dir()?;
            let mut server = Server::spawn(
                binary_path.as_deref(),
                &cwd,
                &launch_env,
                &[],
                &[],
                &processes,
            )?;
            let result = (|| {
                let models = server.http.get("/api/model")?;
                let default = server.http.get("/api/model/default")?;
                Ok(map_models(&models["data"], &default["data"]))
            })();
            server.stop();
            result
        }
    });
    future::or(listing, async {
        smol::Timer::after(LISTING_TIMEOUT).await;
        // Its server killed, the listing's wait ends with it.
        processes.stop();
        Err(AgentError::Provider(format!(
            "OpenCode did not list its models within {} s",
            LISTING_TIMEOUT.as_secs()
        )))
    })
    .await
}

async fn run_actor(
    opts: SessionOptions,
    commands: Receiver<SessionCommand>,
    events: Sender<AgentEvent>,
    ready: Sender<Result<(), AgentError>>,
) {
    let launch = {
        let opts = opts.clone();
        crate::process::unblock(move || {
            let mut server = Server::spawn(
                opts.binary_path.as_deref(),
                &opts.cwd,
                &opts.launch_env,
                &opts.extra_args,
                &opts.mcp_servers.iter().collect::<Vec<_>>(),
                &opts.processes,
            )?;
            match server.subscribe() {
                Ok(stream) => Ok((server, stream)),
                Err(err) => {
                    server.stop();
                    Err(err)
                }
            }
        })
        .await
    };
    let (mut server, (transport, transport_tx)) = match launch {
        Ok(launched) => launched,
        Err(err) => {
            let _ = ready.send(Err(err)).await;
            return;
        }
    };
    if let Err(err) = wait_for_server_connected(&transport).await {
        server.stop();
        let _ = ready.send(Err(err)).await;
        return;
    }
    let setup = {
        let http = server.http.clone();
        let opts = opts.clone();
        crate::process::unblock(move || open_session(&http, &opts)).await
    };
    let setup = match setup {
        Ok(setup) => setup,
        Err(err) => {
            server.stop();
            let _ = ready.send(Err(err)).await;
            return;
        }
    };

    let model_id = setup.model.as_ref().map(ModelRef::id);
    let reported_variant = setup.model.as_ref().and_then(|model| model.variant.clone());
    let mut mapper = Mapper::new(
        setup.session_id.clone(),
        setup.mcp_servers,
        setup.context_windows,
    );
    mapper.served_model = model_id.clone();
    let mut actor = OpenCodeActor {
        clock: opts.clock.clone(),
        server,
        http: setup.http,
        transport,
        transport_tx,
        events,
        mapper,
        model: setup.model,
        variant: crate::selection_str(&opts.option_selections, EFFORT),
        reported_variant,
        agent: setup.agent,
        agents: setup.agents,
        commands: setup.commands.clone(),
        auto_approve: crate::selection_str(&opts.option_selections, PERMISSION_MODE).as_deref()
            == Some("auto"),
        permissions: HashMap::new(),
        forms: HashMap::new(),
        steers: HashMap::new(),
        prompts: HashSet::new(),
        shell_polls: HashMap::new(),
        background: Vec::new(),
    };
    actor
        .events
        .emit(AgentEvent::SessionStarted {
            provider_session_id: setup.session_id.clone(),
            resume: ResumeCursor(json!({ "session_id": setup.session_id })),
            model: model_id,
        })
        .await;
    actor.emit_options().await;
    actor
        .events
        .emit(AgentEvent::ProviderCommands {
            commands: setup.commands,
        })
        .await;
    for message in setup.warnings {
        actor.events.emit(AgentEvent::Warning { message }).await;
    }
    if opts.launch_env.home.is_some() {
        actor
            .events
            .emit(AgentEvent::Warning {
                message: "OpenCode has no supported single-directory home override; custom \
                          environment variables still apply"
                    .into(),
            })
            .await;
    }
    if ready.send(Ok(())).await.is_err() {
        actor.shutdown();
        return;
    }
    actor::run(actor, &commands).await;
}

/// What the actor needs from a created, resumed or forked session.
struct SessionSetup {
    session_id: String,
    /// Requests addressed to the session's own location.
    http: Http,
    model: Option<ModelRef>,
    agent: Option<String>,
    agents: Vec<AgentInfo>,
    commands: Vec<ProviderCommand>,
    context_windows: HashMap<String, u64>,
    mcp_servers: Vec<String>,
    /// The reads that failed, for the session to report once it starts.
    warnings: Vec<String>,
}

fn open_session(http: &Http, opts: &SessionOptions) -> Result<SessionSetup, AgentError> {
    let requested_model = opts
        .model
        .as_deref()
        .and_then(ModelRef::parse)
        .map(|model| ModelRef {
            variant: crate::selection_str(&opts.option_selections, EFFORT),
            ..model
        });
    let requested_agent = crate::selection_str(&opts.option_selections, AGENT);
    let resumed = opts
        .resume
        .as_ref()
        .and_then(|cursor| cursor.str_field(&["session_id"]));
    let session = match (resumed, &opts.fork) {
        (Some(_), Some(ForkPoint::Branch { .. })) => {
            return Err(AgentError::Provider(
                "OpenCode keeps no branches to go on in".into(),
            ));
        }
        (Some(session_id), Some(point)) => {
            let body = match point {
                ForkPoint::Before { checkpoint_id } => json!({ "before": checkpoint_id }),
                // A staged revert only hides what it removes: a whole fork
                // would copy those messages back.
                _ => {
                    let source = http.get(&format!("/api/session/{session_id}"))?;
                    match source["data"]["revert"]["messageID"].as_str() {
                        Some(message_id) => json!({ "before": message_id }),
                        None => json!({}),
                    }
                }
            };
            http.post(&format!("/api/session/{session_id}/fork"), &body)?
        }
        (Some(session_id), None) => http.get(&format!("/api/session/{session_id}"))?,
        (None, _) => {
            let mut body = json!({});
            if let Some(model) = &requested_model {
                body["model"] = model.wire();
            }
            if let Some(agent) = &requested_agent {
                body["agent"] = json!(agent);
            }
            http.post("/api/session", &body)?
        }
    };
    let session = &session["data"];
    let session_id = session["id"]
        .as_str()
        .ok_or_else(|| AgentError::Protocol("OpenCode session response omitted id".into()))?
        .to_owned();
    // Routes such as a shell's output resolve against the location the session
    // was created in, which OpenCode canonicalizes.
    let http = match session
        .pointer("/location/directory")
        .and_then(Value::as_str)
    {
        Some(directory) => {
            let directory = Path::new(directory);
            // The thread moved while no server ran: the session goes there now, and
            // its `session.moved` takes the requests along.
            if resumed.is_some() && !same_directory(directory, &opts.cwd) {
                http.post_empty(
                    &format!("/api/session/{session_id}/move"),
                    &move_body(&opts.cwd),
                )?;
            }
            http.at(directory)
        }
        None => http.clone(),
    };
    let mut warnings = Vec::new();
    let mut model = ModelRef::from_wire(&session["model"]);
    if let Some(requested) = requested_model
        && model.as_ref() != Some(&requested)
    {
        http.post_empty(
            &format!("/api/session/{session_id}/model"),
            &json!({ "model": requested.wire() }),
        )?;
        model = Some(requested);
    }
    if model.is_none() {
        // A session without its own model runs on the server default; knowing
        // it lets a per-turn variant be applied to it.
        model = optional(
            http.get("/api/model/default")
                .map(|default| ModelRef::from_wire(&default["data"])),
            "default model",
            &mut warnings,
        );
    }
    let mut agent = session["agent"].as_str().map(str::to_owned);
    if let Some(requested) = requested_agent
        && agent.as_deref() != Some(requested.as_str())
    {
        http.post_empty(
            &format!("/api/session/{session_id}/agent"),
            &json!({ "agent": requested }),
        )?;
        agent = Some(requested);
    }
    let entry =
        format!("/api/experimental/session/{session_id}/instructions/entries/{INSTRUCTIONS_KEY}");
    match &opts.instructions {
        Some(instructions) => http.put(&entry, &json!({ "value": instructions }))?,
        // Removing an absent entry is a no-op; a stale one must not outlive
        // the setting that put it there.
        None => http.delete(&entry)?,
    };
    Ok(SessionSetup {
        commands: optional(discover_commands(&http), "commands", &mut warnings),
        agents: optional(
            http.get("/api/agent")
                .map(|agents| map_agents(&agents["data"])),
            "agents",
            &mut warnings,
        ),
        context_windows: optional(
            http.get("/api/model")
                .map(|models| context_windows(&models["data"])),
            "models",
            &mut warnings,
        ),
        mcp_servers: optional(mcp_servers(&http), "MCP servers", &mut warnings),
        warnings,
        session_id,
        http,
        model,
        agent,
    })
}

/// A read the session runs without: its failure is told, and the session
/// starts as if it had read nothing.
fn optional<T: Default>(read: Result<T, AgentError>, what: &str, warnings: &mut Vec<String>) -> T {
    read.unwrap_or_else(|err| {
        warnings.push(read_failure(what, &err));
        T::default()
    })
}

fn read_failure(what: &str, err: &AgentError) -> String {
    format!("failed to read OpenCode {what}: {err}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ModelRef {
    provider: String,
    id: String,
    variant: Option<String>,
}

impl ModelRef {
    /// Tcode's model id, `provider/model`.
    fn parse(model: &str) -> Option<Self> {
        let (provider, id) = model
            .split_once('/')
            .filter(|(provider, id)| !provider.is_empty() && !id.is_empty())?;
        Some(Self {
            provider: provider.into(),
            id: id.into(),
            variant: None,
        })
    }

    fn from_wire(model: &Value) -> Option<Self> {
        Some(Self {
            provider: model["providerID"].as_str()?.into(),
            id: model["id"].as_str()?.into(),
            variant: model["variant"].as_str().map(str::to_owned),
        })
    }

    fn wire(&self) -> Value {
        let mut model = json!({ "providerID": self.provider, "id": self.id });
        if let Some(variant) = &self.variant {
            model["variant"] = json!(variant);
        }
        model
    }

    fn id(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }
}

#[derive(Debug, Clone, PartialEq)]
struct AgentInfo {
    id: String,
    name: String,
    description: Option<String>,
}

/// The primary agents a user can select for a session; subagents and the
/// hidden system agents (title, compaction, summary) are not choices.
fn map_agents(agents: &Value) -> Vec<AgentInfo> {
    agents
        .as_array()
        .into_iter()
        .flatten()
        .filter(|agent| agent["mode"] == "primary" && agent["hidden"] != true)
        .filter_map(|agent| {
            let id = agent["id"].as_str()?.to_owned();
            Some(AgentInfo {
                name: agent["name"].as_str().unwrap_or(&id).to_owned(),
                description: agent["description"].as_str().map(str::to_owned),
                id,
            })
        })
        .collect()
}

/// The MCP servers configured for the session's location, Tcode's included.
fn mcp_servers(http: &Http) -> Result<Vec<String>, AgentError> {
    Ok(http.get("/api/mcp")?["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|server| server["name"].as_str().map(str::to_owned))
        .collect())
}

fn discover_commands(http: &Http) -> Result<Vec<ProviderCommand>, AgentError> {
    let listed = |path: &str| -> Result<Vec<Value>, AgentError> {
        Ok(http.get(path)?["data"]
            .as_array()
            .cloned()
            .unwrap_or_default())
    };
    let mut commands: Vec<ProviderCommand> = listed("/api/command")?
        .iter()
        .filter_map(|command| {
            let name = command["name"].as_str()?.trim();
            (!name.is_empty()).then(|| ProviderCommand {
                name: name.to_owned(),
                description: command["description"].as_str().map(str::to_owned),
                kind: ProviderCommandKind::Command,
            })
        })
        .collect();
    // A skill is invoked by its id; its display name may contain spaces.
    commands.extend(listed("/api/skill")?.iter().filter_map(|skill| {
        let id = skill["id"].as_str()?.trim();
        (!id.is_empty()).then(|| ProviderCommand {
            name: id.to_owned(),
            description: skill["description"].as_str().map(str::to_owned),
            kind: ProviderCommandKind::Skill,
        })
    }));
    Ok(commands)
}

fn map_models(models: &Value, default: &Value) -> Vec<ModelSpec> {
    let default = ModelRef::from_wire(default);
    models
        .as_array()
        .into_iter()
        .flatten()
        .filter(|model| {
            model["enabled"] != false
                && model["status"] != "deprecated"
                // Image and decision models cannot run a session.
                && model["type"].as_str().is_none_or(|kind| kind == "chat")
        })
        .filter_map(|model| {
            let reference = ModelRef::from_wire(model)?;
            let variants: Vec<String> = model["variants"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|variant| variant["id"].as_str().map(str::to_owned))
                .collect();
            let options = (!variants.is_empty())
                .then(|| OptionDescriptor::Select {
                    id: EFFORT.into(),
                    label: "Thinking".into(),
                    options: variants
                        .into_iter()
                        .map(|variant| SelectOption {
                            label: title_case(&variant),
                            value: variant,
                            description: None,
                            unavailable: None,
                            speed: None,
                            summary: None,
                            approval: None,
                        })
                        .collect(),
                    default_value: None,
                    role: crate::OptionRole::Effort,
                    apply: crate::ApplyTiming::Live,
                    recommended: None,
                    permissive: None,
                    minimum: None,
                })
                .into_iter()
                .collect();
            Some(ModelSpec {
                id: reference.id(),
                display_name: model["name"].as_str().unwrap_or(&reference.id).to_owned(),
                is_default: default.as_ref().is_some_and(|default| {
                    default.provider == reference.provider && default.id == reference.id
                }),
                options,
            })
        })
        .collect()
}

fn context_windows(models: &Value) -> HashMap<String, u64> {
    models
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|model| {
            Some((
                ModelRef::from_wire(model)?.id(),
                model.pointer("/limit/context")?.as_u64()?,
            ))
        })
        .collect()
}

fn title_case(value: &str) -> String {
    let mut characters = value.chars();
    characters
        .next()
        .map(|first| first.to_uppercase().collect::<String>() + characters.as_str())
        .unwrap_or_default()
}

struct OpenCodeActor {
    clock: crate::Clock,
    server: Server,
    http: Http,
    transport: Receiver<Transport>,
    /// Shell output pollers feed the same queue as the event stream.
    transport_tx: Sender<Transport>,
    events: Sender<AgentEvent>,
    mapper: Mapper,
    model: Option<ModelRef>,
    variant: Option<String>,
    /// The variant the session runs, as OpenCode last reported it: its
    /// model's, which has none until one is chosen (2.2.7).
    reported_variant: Option<String>,
    agent: Option<String>,
    agents: Vec<AgentInfo>,
    commands: Vec<ProviderCommand>,
    auto_approve: bool,
    /// Pending permission request id → the session that asked.
    permissions: HashMap<String, String>,
    forms: HashMap<String, PendingForm>,
    /// Inbox id of a steer → the runtime's steer request id.
    steers: HashMap<String, String>,
    /// Inbox ids of the turns Tcode sent; each is a revert boundary.
    prompts: HashSet<String>,
    shell_polls: HashMap<String, Arc<ShellPoll>>,
    /// The background work as last listed.
    background: Vec<BackgroundTask>,
}

/// What the actor tells the thread polling a shell's output.
#[derive(Default)]
struct ShellPoll {
    /// The call ended with its output: nothing more is read.
    stop: AtomicBool,
    /// The shell exited: what is left of its output is read, then nothing.
    exited: AtomicBool,
}

struct PendingForm {
    session_id: String,
    fields: Vec<(String, String)>,
}

impl SessionActor for OpenCodeActor {
    type TransportItem = Transport;

    fn transport(&self) -> &Receiver<Transport> {
        &self.transport
    }

    fn events(&self) -> &Sender<AgentEvent> {
        &self.events
    }

    fn clock(&self) -> &crate::Clock {
        &self.clock
    }

    fn command_failure_reason(&self) -> &'static str {
        "OpenCode REST command failed"
    }

    async fn handle_command(&mut self, command: SessionCommand) -> Result<(), String> {
        match command {
            SessionCommand::SendTurn {
                delivery_id,
                text,
                options,
                attachments,
            } => {
                let effort = options.and_then(|options| options.effort);
                self.send_turn(text, effort, attachments)
                    .map_err(|err| err.to_string())?;
                self.events
                    .emit(AgentEvent::TurnAccepted { delivery_id })
                    .await;
            }
            SessionCommand::Steer {
                request_id,
                text,
                attachments,
            } => {
                if self.mapper.active_turn.is_none() {
                    // No running turn to fold it into; the runtime sends it as one.
                    self.events
                        .emit(AgentEvent::SteerRejected { request_id })
                        .await;
                    return Ok(());
                }
                let mut body = prompt_body(text, &attachments, "steer");
                self.attach_skill(&mut body);
                let inbox = self
                    .http
                    .post(&self.session_path("prompt"), &body)
                    .map_err(|err| err.to_string())?;
                if let Some(inbox_id) = inbox.pointer("/data/id").and_then(Value::as_str) {
                    self.steers.insert(inbox_id.to_owned(), request_id);
                }
            }
            SessionCommand::Interrupt => {
                self.http
                    .post(&self.session_path("interrupt"), &json!({}))
                    .map_err(|err| err.to_string())?;
            }
            SessionCommand::RespondApproval {
                request_id,
                decision,
            } => self.respond_approval(request_id, decision).await?,
            SessionCommand::RespondUserInput {
                request_id,
                answers,
            } => self.respond_user_input(request_id, answers).await?,
            SessionCommand::SetOption { id, value } => self.set_option(&id, value).await?,
            SessionCommand::Compact => {
                self.http
                    .post(&self.session_path("compact"), &json!({}))
                    .map_err(|err| err.to_string())?;
            }
            SessionCommand::Rewind {
                checkpoint_id,
                mode,
            } => {
                let event = match self.rewind(&checkpoint_id, mode).await {
                    Ok(event) => event,
                    Err(failure) => AgentEvent::RewindFailed {
                        checkpoint_id,
                        mode,
                        failure,
                    },
                };
                self.events.emit(event).await;
            }
            SessionCommand::UndoRewind { checkpoint_id, .. } => {
                let event = match self.undo_rewind(&checkpoint_id) {
                    Ok(()) => AgentEvent::RewindUndone { checkpoint_id },
                    Err(failure) => AgentEvent::RewindUndoFailed {
                        checkpoint_id,
                        failure,
                    },
                };
                self.events.emit(event).await;
            }
            command @ SessionCommand::PreviewRewind { .. } => {
                if let Some(event) = command
                    .rewind_refusal(RewindFailure::new(RewindFailureReason::Unsupported, None))
                {
                    self.events.emit(event).await;
                }
            }
            SessionCommand::ApplySettings {
                model,
                option_selections,
            } => {
                let applied = self
                    .apply_settings(model.as_deref(), &option_selections)
                    .await;
                self.events
                    .emit(AgentEvent::SettingsApplied { applied })
                    .await;
            }
            // The session follows once OpenCode reports `session.moved`.
            SessionCommand::MoveTo { cwd } => {
                self.http
                    .post_empty(&self.session_path("move"), &move_body(&cwd))
                    .map_err(|err| err.to_string())?;
            }
            SessionCommand::StopBackgroundTask { task_id } => {
                for action in self.mapper.stop_background(task_id.as_deref()) {
                    self.perform(action).await;
                }
            }
            SessionCommand::RetryTurn { .. }
            | SessionCommand::MoveToBackground { .. }
            | SessionCommand::RespondWorktree { .. }
            | SessionCommand::SetGoal { .. }
            | SessionCommand::ClearGoal
            | SessionCommand::ReadAccountUsage { .. }
            | SessionCommand::Shutdown => {}
        }
        Ok(())
    }

    async fn handle_transport(
        &mut self,
        item: Result<Transport, smol::channel::RecvError>,
    ) -> TransportOutcome {
        match item {
            Ok(Transport::Event(event)) => match self.handle_event(&event).await {
                Ok(()) => {
                    self.list_background().await;
                    TransportOutcome::Continue
                }
                Err(err) => TransportOutcome::Fatal(err),
            },
            Ok(Transport::ShellOutput { item_id, text }) => {
                let events = self.mapper.shell_output(&item_id, text);
                for event in events {
                    self.events.emit(event).await;
                }
                TransportOutcome::Continue
            }
            Ok(Transport::ShellPollEnded { item_id }) => {
                self.shell_polls.remove(&item_id);
                let events = self.mapper.shell_poll_ended(&item_id);
                for event in events {
                    self.events.emit(event).await;
                }
                self.list_background().await;
                TransportOutcome::Continue
            }
            Ok(Transport::Error(err)) => TransportOutcome::Fatal(err),
            Ok(Transport::Eof) | Err(_) => {
                TransportOutcome::Closed("OpenCode event stream closed".into())
            }
        }
    }

    async fn teardown(mut self, reason: Option<String>) -> Option<String> {
        self.shutdown();
        reason.map(|reason| self.server.tail.append_to(reason, "\nserver output:\n"))
    }
}

impl OpenCodeActor {
    fn session_path(&self, route: &str) -> String {
        format!("/api/session/{}/{route}", self.mapper.root)
    }

    /// Lists the background work again when it changed; a piece started when
    /// it was first listed.
    async fn list_background(&mut self) {
        let now = crate::now_unix_millis();
        let tasks: Vec<BackgroundTask> = self
            .mapper
            .background()
            .into_iter()
            .map(|work| BackgroundTask {
                started_at: self
                    .background
                    .iter()
                    .find(|known| known.id == work.id)
                    .map_or(now, |known| known.started_at),
                id: work.id,
                name: work.name,
                kind: work.kind,
                item_id: Some(work.item_id),
                started_reported: false,
            })
            .collect();
        if tasks != self.background {
            self.background.clone_from(&tasks);
            self.events
                .emit(AgentEvent::BackgroundTaskList { tasks })
                .await;
        }
    }

    fn send_turn(
        &mut self,
        text: String,
        effort: Option<String>,
        attachments: Vec<Attachment>,
    ) -> Result<(), AgentError> {
        if let Some(variant) = effort.or_else(|| self.variant.clone())
            && let Some(model) = &self.model
            && model.variant.as_deref() != Some(variant.as_str())
        {
            let model = ModelRef {
                variant: Some(variant),
                ..model.clone()
            };
            self.http.post_empty(
                &self.session_path("model"),
                &json!({ "model": model.wire() }),
            )?;
            self.model = Some(model);
        }
        if let Some((name, arguments)) = self.command_invocation(&text) {
            let mut body = prompt_body(arguments, &attachments, "queue");
            body["name"] = json!(name);
            self.http.post_empty(&self.session_path("command"), &body)?;
            return Ok(());
        }
        let mut body = prompt_body(text, &attachments, "queue");
        self.attach_skill(&mut body);
        let inbox = self.http.post(&self.session_path("prompt"), &body)?;
        if let Some(inbox_id) = inbox.pointer("/data/id").and_then(Value::as_str) {
            self.prompts.insert(inbox_id.to_owned());
        }
        Ok(())
    }

    /// `/name arguments` for a listed OpenCode command, which 2.x runs through
    /// its command route rather than as prompt text.
    fn command_invocation(&self, text: &str) -> Option<(String, String)> {
        let rest = text.strip_prefix('/')?;
        let (name, arguments) = rest
            .split_once(char::is_whitespace)
            .map_or((rest, ""), |(name, arguments)| {
                (name, arguments.trim_start())
            });
        self.commands
            .iter()
            .any(|command| command.kind == ProviderCommandKind::Command && command.name == name)
            .then(|| (name.to_owned(), arguments.to_owned()))
    }

    /// A leading `$id` mention of a listed skill attaches that skill, as
    /// OpenCode's own composer does for a skill mention.
    fn attach_skill(&self, body: &mut Value) {
        let Some(rest) = body["text"]
            .as_str()
            .and_then(|text| text.strip_prefix('$'))
        else {
            return;
        };
        let id = &rest[..rest.find(char::is_whitespace).unwrap_or(rest.len())];
        if self
            .commands
            .iter()
            .any(|command| command.kind == ProviderCommandKind::Skill && command.name == id)
        {
            body["skills"] = json!([{
                "id": id,
                "mention": { "start": 0, "end": id.len() + 1, "text": format!("${id}") },
            }]);
        }
    }

    /// The session's model is `POST /session/{id}/model`, the route that also
    /// sets its variant before a turn (2.2.5); the variant and agent are the
    /// live options they always were. A session cannot be put back on the
    /// server's default model.
    async fn apply_settings(
        &mut self,
        model: Option<&str>,
        selections: &[OptionSelection],
    ) -> bool {
        let Some(mut target) = model.and_then(ModelRef::parse) else {
            return model.is_none() && self.model.is_none();
        };
        for selection in selections {
            if matches!(selection.id.as_str(), EFFORT | AGENT)
                && self
                    .set_option(&selection.id, selection.value.clone())
                    .await
                    .is_err()
            {
                return false;
            }
        }
        target.variant = self.variant.clone();
        if self
            .model
            .as_ref()
            .is_some_and(|current| current.provider == target.provider && current.id == target.id)
        {
            return true;
        }
        if let Err(error) = self.http.post_empty(
            &self.session_path("model"),
            &json!({ "model": target.wire() }),
        ) {
            log::warn!("opencode: model change refused: {error}");
            return false;
        }
        self.model = Some(target);
        true
    }

    async fn set_option(&mut self, id: &str, value: Value) -> Result<(), String> {
        match id {
            PERMISSION_MODE => {
                let Some(mode @ ("normal" | "auto")) = value.as_str() else {
                    return Ok(());
                };
                self.auto_approve = mode == "auto";
                self.emit_options().await;
                if self.auto_approve {
                    let pending: Vec<String> = self.permissions.keys().cloned().collect();
                    for request_id in pending {
                        self.reply_permission(&request_id, "once", None).await;
                    }
                }
            }
            EFFORT => self.variant = value.as_str().map(str::to_owned),
            AGENT => {
                let Some(agent) = value.as_str() else {
                    return Ok(());
                };
                if self.agent.as_deref() != Some(agent) {
                    self.http
                        .post_empty(&self.session_path("agent"), &json!({ "agent": agent }))
                        .map_err(|err| err.to_string())?;
                    self.agent = Some(agent.to_owned());
                }
                self.emit_options().await;
            }
            _ => {}
        }
        Ok(())
    }

    async fn emit_options(&self) {
        let mut descriptors: Vec<_> = crate::permission_control(ProviderKind::OpenCode)
            .into_iter()
            .collect();
        let mut selections = vec![OptionSelection {
            id: PERMISSION_MODE.into(),
            value: json!(if self.auto_approve { "auto" } else { "normal" }),
        }];
        // OpenCode names no variant for a model run at its own settings, and
        // reports no effort for those.
        selections.push(OptionSelection {
            id: EFFORT.into(),
            value: self
                .reported_variant
                .as_ref()
                .map_or(Value::Null, |variant| json!(variant)),
        });
        if self.agents.len() > 1 {
            descriptors.push(OptionDescriptor::Select {
                id: AGENT.into(),
                label: "Agent".into(),
                options: self
                    .agents
                    .iter()
                    .map(|agent| SelectOption {
                        value: agent.id.clone(),
                        label: agent.name.clone(),
                        description: agent.description.clone(),
                        unavailable: None,
                        speed: None,
                        summary: None,
                        approval: None,
                    })
                    .collect(),
                default_value: None,
                role: crate::OptionRole::Model,
                apply: crate::ApplyTiming::Live,
                recommended: None,
                permissive: None,
                minimum: None,
            });
            if let Some(agent) = &self.agent {
                selections.push(OptionSelection {
                    id: AGENT.into(),
                    value: json!(agent),
                });
            }
        }
        self.events
            .emit(AgentEvent::ProviderOptions {
                descriptors,
                selections,
            })
            .await;
    }

    async fn respond_approval(
        &mut self,
        request_id: String,
        decision: ApprovalDecision,
    ) -> Result<(), String> {
        let (reply, message) = match &decision {
            ApprovalDecision::Option(id) => (id.as_str(), None),
            // A rejection with a message is OpenCode's correction: the model
            // continues from the user's words instead of stopping.
            ApprovalDecision::Feedback(text) => ("reject", Some(text.as_str())),
            ApprovalDecision::Cancel => ("reject", None),
        };
        let Some(session_id) = self.permissions.remove(&request_id) else {
            return Ok(());
        };
        let mut body = json!({ "decision": reply });
        if let Some(message) = message {
            body["message"] = json!(message);
        }
        self.http
            .post_empty(
                &format!("/api/session/{session_id}/permission/{request_id}/reply"),
                &body,
            )
            .map_err(|err| err.to_string())?;
        self.events
            .emit(AgentEvent::ApprovalResolved {
                request_id,
                decision,
            })
            .await;
        Ok(())
    }

    async fn reply_permission(&mut self, request_id: &str, reply: &str, message: Option<&str>) {
        let Some(session_id) = self.permissions.get(request_id).cloned() else {
            return;
        };
        let mut body = json!({ "decision": reply });
        if let Some(message) = message {
            body["message"] = json!(message);
        }
        match self.http.post_empty(
            &format!("/api/session/{session_id}/permission/{request_id}/reply"),
            &body,
        ) {
            Ok(()) => {
                self.permissions.remove(request_id);
                self.events
                    .emit(AgentEvent::ApprovalResolved {
                        request_id: request_id.to_owned(),
                        decision: ApprovalDecision::Option(reply.to_owned()),
                    })
                    .await;
            }
            Err(err) => {
                self.events
                    .emit(AgentEvent::Warning {
                        message: format!("failed to answer OpenCode permission request: {err}"),
                    })
                    .await;
            }
        }
    }

    async fn respond_user_input(
        &mut self,
        request_id: String,
        answers: Map<String, Value>,
    ) -> Result<(), String> {
        let Some(form) = self.forms.remove(&request_id) else {
            return Ok(());
        };
        let path = format!("/api/session/{}/form/{request_id}", form.session_id);
        if answers.is_empty() {
            self.http.delete(&path).map_err(|err| err.to_string())?;
        } else {
            self.http
                .post_empty(
                    &format!("{path}/reply"),
                    &json!({ "answer": form_answer(&form.fields, &answers) }),
                )
                .map_err(|err| err.to_string())?;
        }
        self.events
            .emit(AgentEvent::UserInputResolved {
                request_id,
                answers,
            })
            .await;
        Ok(())
    }

    /// OpenCode reverts the conversation to a user message, restoring the
    /// files its steps changed when asked; it has no files-only restore. The
    /// revert stays staged, as OpenCode's own /undo leaves it: hidden and
    /// undoable (`DELETE …/revert`) until the next prompt or compaction
    /// commits it, which OpenCode does itself.
    async fn rewind(
        &mut self,
        checkpoint_id: &str,
        mode: RewindMode,
    ) -> Result<AgentEvent, RewindFailure> {
        if !mode.includes_conversation() {
            return Err(RewindFailure::new(
                RewindFailureReason::Unsupported,
                "OpenCode restores files only together with the conversation that changed them"
                    .to_owned(),
            ));
        }
        // A staging refuses a busy session; OpenCode's /undo interrupts the
        // running turn and waits for it to end first.
        self.finish_running_turn().await?;
        let message = self
            .http
            .get(&self.session_path(&format!("message/{checkpoint_id}")))
            .map_err(rewind_failure)?;
        // A staging over a staged revert moves its boundary, and clearing it
        // would bring back both cuts, which no undo of the later one shows.
        let staged_before = self.staged_revert()?.is_some();
        let revert = self
            .http
            .post(
                &self.session_path("revert/stage"),
                &json!({ "messageID": checkpoint_id, "files": mode.includes_files() }),
            )
            .map_err(rewind_failure)?;
        Ok(AgentEvent::RewindCompleted {
            checkpoint_id: checkpoint_id.to_owned(),
            mode,
            prefill: message
                .pointer("/data/text")
                .and_then(Value::as_str)
                .map(str::to_owned),
            prefill_attachments: local_attachments(&message["data"]["files"]),
            later_history: LaterHistory::HiddenUntilNextSend,
            files: mode
                .includes_files()
                .then(|| reverted_files(&revert["data"]["files"])),
            incomplete: None,
            undo_token: (!staged_before).then(|| checkpoint_id.to_owned()),
        })
    }

    /// The message the session's staged revert hides from, if one is staged.
    fn staged_revert(&self) -> Result<Option<String>, RewindFailure> {
        let session = self
            .http
            .get(&format!("/api/session/{}", self.mapper.root))
            .map_err(rewind_failure)?;
        Ok(session
            .pointer("/data/revert/messageID")
            .and_then(Value::as_str)
            .map(str::to_owned))
    }

    /// Interrupt the turn that runs and take its events up to its end, so the
    /// rewind's answer follows the turn's.
    async fn finish_running_turn(&mut self) -> Result<(), RewindFailure> {
        if self.mapper.active_turn.is_none() {
            return Ok(());
        }
        self.http
            .post(&self.session_path("interrupt"), &json!({}))
            .map_err(rewind_failure)?;
        while self.mapper.active_turn.is_some() {
            let item = self.transport.recv().await;
            if !matches!(
                self.handle_transport(item).await,
                TransportOutcome::Continue
            ) {
                return Err(RewindFailure::new(RewindFailureReason::ProcessGone, None));
            }
        }
        Ok(())
    }

    /// Clear the staged revert the rewind to `checkpoint_id` left, which puts
    /// back what it hid and the files it reverted.
    fn undo_rewind(&mut self, checkpoint_id: &str) -> Result<(), RewindFailure> {
        if self.staged_revert()?.as_deref() != Some(checkpoint_id) {
            // A prompt or a compaction committed it.
            return Err(RewindFailure::new(
                RewindFailureReason::Expired,
                "OpenCode has no staged revert at this message".to_owned(),
            ));
        }
        self.http
            .delete(&self.session_path("revert"))
            .map_err(rewind_failure)
    }

    async fn handle_event(&mut self, event: &Value) -> Result<(), String> {
        let data = &event["data"];
        match event["type"].as_str().unwrap_or_default() {
            "permission.asked" => self.permission_asked(data).await,
            "permission.replied" => {
                if let Some(request_id) = data["requestID"].as_str()
                    && self.permissions.remove(request_id).is_some()
                {
                    self.events
                        .emit(AgentEvent::ApprovalResolved {
                            request_id: request_id.to_owned(),
                            decision: ApprovalDecision::Option(
                                data["reply"].as_str().unwrap_or("reject").to_owned(),
                            ),
                        })
                        .await;
                }
            }
            "form.created" => self.form_created(&data["form"]).await,
            "form.replied" | "form.cancelled" => {
                if let Some(form_id) = data["id"].as_str()
                    && self.forms.remove(form_id).is_some()
                {
                    let answers = match data["answer"].as_object() {
                        Some(answer) => answer.clone(),
                        None => Map::new(),
                    };
                    self.events
                        .emit(AgentEvent::UserInputResolved {
                            request_id: form_id.to_owned(),
                            answers,
                        })
                        .await;
                }
            }
            "session.inbox.delivered" if data["sessionID"] == self.mapper.root.as_str() => {
                let inbox_id = data["inboxID"].as_str().unwrap_or_default();
                if let Some(started) = self.mapper.delivered(inbox_id) {
                    self.events.emit(started).await;
                }
                if let Some(request_id) = self.steers.remove(inbox_id) {
                    self.events
                        .emit(AgentEvent::SteerAccepted { request_id })
                        .await;
                } else if self.prompts.contains(inbox_id)
                    && let Some(turn_id) = self.mapper.active_turn.clone()
                {
                    self.events
                        .emit(AgentEvent::TurnCheckpoint {
                            turn_id,
                            checkpoint_id: inbox_id.to_owned(),
                        })
                        .await;
                }
            }
            // Plugins register commands, skills and agents after the server
            // is ready; these announce each change.
            "command.updated" | "skill.updated" => self.refresh_commands().await,
            "mcp.status.changed" => self.refresh_mcp_servers().await,
            "agent.updated" => self.refresh_agents().await,
            // Moved by `MoveTo` or by the model's own `session_move`: requests
            // follow the session, and so does what OpenCode scopes to a location.
            "session.moved" if data["sessionID"] == self.mapper.root.as_str() => {
                if let Some(to) = data.pointer("/location/directory").and_then(Value::as_str) {
                    let moved = self.http.at(Path::new(to));
                    let from = std::mem::replace(&mut self.http, moved);
                    self.events
                        .emit(AgentEvent::SessionMoved {
                            from: from.directory,
                            to: to.into(),
                            parent_item_id: None,
                        })
                        .await;
                    self.refresh_commands().await;
                    self.refresh_agents().await;
                    self.refresh_mcp_servers().await;
                }
            }
            "session.step.started" if data["sessionID"] == self.mapper.root.as_str() => {
                // A session without an agent of its own runs the configured default.
                let agent = data["agent"].as_str().map(str::to_owned);
                let variant = ModelRef::from_wire(&data["model"]).and_then(|model| model.variant);
                if (agent.is_some() && agent != self.agent) || variant != self.reported_variant {
                    if agent.is_some() {
                        self.agent = agent;
                    }
                    self.reported_variant = variant;
                    self.emit_options().await;
                }
            }
            // What a switch recorded for the session's next steps (2.2.7).
            "session.model.selected" if data["sessionID"] == self.mapper.root.as_str() => {
                if let Some(model) = ModelRef::from_wire(&data["model"]) {
                    self.reported_variant.clone_from(&model.variant);
                    self.events
                        .emit(AgentEvent::SessionModel { model: model.id() })
                        .await;
                    self.emit_options().await;
                }
            }
            "session.agent.selected" if data["sessionID"] == self.mapper.root.as_str() => {
                if let Some(agent) = data["agent"].as_str() {
                    self.agent = Some(agent.to_owned());
                    self.emit_options().await;
                }
            }
            "session.inbox.cancelled" => {
                if let Some(request_id) = data["inboxID"]
                    .as_str()
                    .and_then(|inbox_id| self.steers.remove(inbox_id))
                {
                    self.events
                        .emit(AgentEvent::SteerRejected { request_id })
                        .await;
                }
            }
            _ => {}
        }
        let mapped = self.mapper.on_event(event)?;
        for event in mapped.events {
            self.events.emit(event).await;
        }
        for action in mapped.actions {
            self.perform(action).await;
        }
        Ok(())
    }

    async fn perform(&mut self, action: Action) {
        match action {
            Action::PollShell {
                item_id,
                shell_id,
                directory,
            } => {
                let poll = Arc::new(ShellPoll::default());
                self.shell_polls.insert(item_id.clone(), poll.clone());
                let http = match directory {
                    Some(directory) => self.http.at(Path::new(&directory)),
                    None => self.http.clone(),
                };
                let sender = self.transport_tx.clone();
                let _ = std::thread::Builder::new()
                    .name("opencode-shell-output".into())
                    .spawn(move || poll_shell_output(&http, &shell_id, &item_id, &poll, &sender));
            }
            Action::StopShell { item_id } => {
                if let Some(poll) = self.shell_polls.get(&item_id) {
                    poll.stop.store(true, Ordering::Relaxed);
                }
            }
            Action::DrainShell { item_id } => {
                if let Some(poll) = self.shell_polls.get(&item_id) {
                    poll.exited.store(true, Ordering::Relaxed);
                }
            }
            // Removing a running shell ends it as `killed` (`shell.ts`).
            Action::RemoveShell {
                shell_id,
                directory,
            } => {
                let http = match directory {
                    Some(directory) => self.http.at(Path::new(&directory)),
                    None => self.http.clone(),
                };
                if let Err(err) = http.delete(&format!("/api/shell/{shell_id}")) {
                    self.events
                        .emit(AgentEvent::Warning {
                            message: format!("OpenCode did not stop shell {shell_id}: {err}"),
                        })
                        .await;
                }
            }
            // An interrupted subagent reports itself `cancelled`
            // (`execution.ts`).
            Action::InterruptSession { session_id } => {
                let message = match self
                    .http
                    .post(&format!("/api/session/{session_id}/interrupt"), &json!({}))
                {
                    Ok(answer) if answer["interrupted"] == false => Some(format!(
                        "OpenCode had nothing running to interrupt in subagent session {session_id}"
                    )),
                    Ok(_) => None,
                    Err(err) => Some(format!(
                        "OpenCode did not interrupt subagent session {session_id}: {err}"
                    )),
                };
                if let Some(message) = message {
                    self.events.emit(AgentEvent::Warning { message }).await;
                }
            }
            Action::FetchDiff => {
                // A step ends only inside an execution, which names a turn.
                let Some(turn_id) = self.mapper.turn_id() else {
                    return;
                };
                match self.http.get(&self.session_path("diff")) {
                    Ok(diff) => {
                        self.events
                            .emit(AgentEvent::TurnChangesUpdated {
                                turn_id,
                                changes: file_diffs(&diff["data"]),
                                completeness: ChangeCompleteness::Exact,
                            })
                            .await;
                    }
                    Err(err) => {
                        self.events
                            .emit(AgentEvent::Warning {
                                message: format!("failed to fetch OpenCode turn diff: {err}"),
                            })
                            .await;
                    }
                }
            }
        }
    }

    async fn refresh_commands(&mut self) {
        match discover_commands(&self.http) {
            Ok(commands) if commands != self.commands => {
                self.commands = commands.clone();
                self.events
                    .emit(AgentEvent::ProviderCommands { commands })
                    .await;
            }
            Ok(_) => {}
            Err(err) => self.read_failed("commands", &err).await,
        }
    }

    async fn refresh_agents(&mut self) {
        match self.http.get("/api/agent") {
            Ok(agents) => {
                let agents = map_agents(&agents["data"]);
                if agents != self.agents {
                    self.agents = agents;
                    self.emit_options().await;
                }
            }
            Err(err) => self.read_failed("agents", &err).await,
        }
    }

    async fn refresh_mcp_servers(&mut self) {
        match mcp_servers(&self.http) {
            Ok(servers) => self.mapper.mcp_servers = servers,
            Err(err) => self.read_failed("MCP servers", &err).await,
        }
    }

    async fn read_failed(&self, what: &str, err: &AgentError) {
        self.events
            .emit(AgentEvent::Warning {
                message: read_failure(what, err),
            })
            .await;
    }

    async fn permission_asked(&mut self, data: &Value) {
        let (Some(request_id), Some(session_id)) =
            (data["id"].as_str(), data["sessionID"].as_str())
        else {
            return;
        };
        let Some(parent_item_id) = self.mapper.scope(session_id) else {
            return;
        };
        self.permissions
            .insert(request_id.to_owned(), session_id.to_owned());
        if self.auto_approve {
            self.reply_permission(request_id, "once", None).await;
            return;
        }
        let kind = self.mapper.approval_kind(data);
        let request = ApprovalRequest {
            id: request_id.to_owned(),
            turn_id: self.mapper.active_turn.clone(),
            parent_item_id,
            // A tool's message is already its detail.
            reason: (!matches!(kind, ApprovalKind::ToolUse { .. }))
                .then(|| data["message"].as_str())
                .flatten()
                .map(str::to_owned),
            blocked_path: None,
            kind,
            options: vec![
                ApprovalOption {
                    id: "once".into(),
                    label: "Allow once".into(),
                    kind: ApprovalOptionKind::AllowOnce,
                },
                ApprovalOption {
                    id: "always".into(),
                    label: "Always allow".into(),
                    kind: ApprovalOptionKind::AllowAlways,
                },
                ApprovalOption {
                    id: "reject".into(),
                    label: "Reject".into(),
                    kind: ApprovalOptionKind::RejectOnce,
                },
            ],
        };
        self.events
            .emit(AgentEvent::ApprovalRequested(request))
            .await;
    }

    async fn form_created(&mut self, form: &Value) {
        let (Some(form_id), Some(session_id)) = (form["id"].as_str(), form["sessionID"].as_str())
        else {
            return;
        };
        if self.mapper.scope(session_id).is_none() {
            return;
        }
        let fields: Vec<&Value> = form["fields"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|field| field["hidden"] != true)
            .collect();
        let questions = fields
            .iter()
            .filter_map(|field| form_question(field))
            .collect();
        self.forms.insert(
            form_id.to_owned(),
            PendingForm {
                session_id: session_id.to_owned(),
                fields: fields
                    .iter()
                    .filter_map(|field| {
                        Some((
                            field["key"].as_str()?.to_owned(),
                            field["type"].as_str().unwrap_or("string").to_owned(),
                        ))
                    })
                    .collect(),
            },
        );
        self.events
            .emit(AgentEvent::UserInputRequested {
                request_id: form_id.to_owned(),
                questions,
                delivery: UserInputDelivery::Blocking,
            })
            .await;
    }

    fn shutdown(&mut self) {
        for poll in self.shell_polls.values() {
            poll.stop.store(true, Ordering::Relaxed);
        }
        self.server.stop();
    }
}

/// `POST /session/{id}/move` to `directory` with `queue` delivery: a running
/// execution finishes where it is, and the prompt sent next waits behind the
/// move in the inbox. `steer` would move the session in the middle of a turn.
fn move_body(directory: &Path) -> Value {
    json!({ "directory": directory.to_string_lossy(), "delivery": "queue" })
}

/// OpenCode reports a created session's location canonicalized and a moved
/// one's as the move named it.
fn same_directory(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(a), Ok(b)) if a == b
        )
}

fn prompt_body(text: String, attachments: &[Attachment], delivery: &str) -> Value {
    let mut body = json!({ "text": text, "delivery": delivery });
    if !attachments.is_empty() {
        body["files"] = attachments
            .iter()
            .map(|attachment| {
                let mut file = json!({
                    "uri": format!(
                        "data:{};base64,{}",
                        attachment.media_type, attachment.data_base64
                    ),
                });
                if let Some(name) = attachment
                    .source_path
                    .as_deref()
                    .and_then(|path| Path::new(path).file_name())
                {
                    file["name"] = json!(name.to_string_lossy());
                }
                file
            })
            .collect();
    }
    body
}

/// What a refused request means for a rewind, with OpenCode's words.
fn rewind_failure(error: AgentError) -> RewindFailure {
    let detail = error.to_string();
    let status = detail
        .split("OpenCode HTTP ")
        .nth(1)
        .and_then(|rest| rest.get(..3))
        .and_then(|status| status.parse::<u16>().ok());
    let reason = match (&error, status) {
        (_, Some(409)) => RewindFailureReason::Busy,
        (_, Some(404)) => RewindFailureReason::TargetNotFound,
        (AgentError::Provider(_), _) => RewindFailureReason::Other,
        _ => RewindFailureReason::ProcessGone,
    };
    RewindFailure::new(reason, detail)
}

/// The files a staged revert restored (`Session.Revert.files`).
fn reverted_files(files: &Value) -> RewindFiles {
    RewindFiles {
        files: files
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|file| {
                Some(RewindFile {
                    path: file["file"].as_str()?.to_owned(),
                    insertions: file["additions"].as_u64(),
                    deletions: file["deletions"].as_u64(),
                })
            })
            .collect(),
        ..RewindFiles::default()
    }
}

/// The attachments of a restored prompt that are files on this machine. An
/// inline attachment (Tcode sends its images as `data:` URLs) has no path to
/// put back in a composer, so the prompt comes back without it.
/// The prompt's images that are local files: the composer takes images
/// alone, and an inline (`data:`) one has no path to hand it.
fn local_attachments(files: &Value) -> Vec<String> {
    files
        .as_array()
        .into_iter()
        .flatten()
        .filter(|file| {
            file.get("mime")
                .and_then(Value::as_str)
                .is_some_and(|mime| mime.starts_with("image/"))
        })
        .filter_map(|file| {
            let path = file
                .pointer("/source/uri")?
                .as_str()?
                .strip_prefix("file://")?;
            Some(percent_decode(path))
        })
        .collect()
}

/// `decodeURIComponent` of a `file://` URL's path.
fn percent_decode(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(byte) = path
                .get(index + 1..index + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            decoded.push(byte);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn form_question(field: &Value) -> Option<UserInputQuestion> {
    let key = field["key"].as_str()?;
    let title = field["title"].as_str();
    let description = field["description"].as_str();
    let question = description.or(title).unwrap_or(key).to_owned();
    let options = match field["type"].as_str() {
        Some("boolean") => ["true", "false"]
            .into_iter()
            .map(|label| UserInputOption {
                label: label.into(),
                description: String::new(),
            })
            .collect(),
        _ => field["options"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|option| UserInputOption {
                label: option["label"].as_str().unwrap_or_default().to_owned(),
                description: option["description"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            })
            .collect(),
    };
    Some(UserInputQuestion {
        id: key.to_owned(),
        header: title.unwrap_or(key).to_owned(),
        question,
        options,
        multi_select: field["type"] == "multiselect",
        optional: field["required"] != true,
        prefill: match &field["default"] {
            Value::String(text) => Some(text.clone()),
            Value::Number(number) => Some(number.to_string()),
            _ => None,
        },
    })
}

/// Converts the composer's answers, keyed by field key, to each field's type.
fn form_answer(fields: &[(String, String)], answers: &Map<String, Value>) -> Map<String, Value> {
    fields
        .iter()
        .filter_map(|(key, kind)| {
            let answer = answers.get(key)?;
            let texts: Vec<&str> = match answer {
                Value::String(text) => vec![text.as_str()],
                Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
                _ => Vec::new(),
            };
            // A field left empty is left out of the answer, not sent empty.
            if texts.iter().all(|text| text.is_empty()) {
                return None;
            }
            let value = match kind.as_str() {
                "multiselect" => json!(texts),
                "boolean" => json!(texts.first().is_some_and(|text| *text == "true")),
                "number" | "integer" => {
                    serde_json::from_str::<serde_json::Number>(texts.first()?.trim())
                        .ok()
                        .map(Value::Number)?
                }
                _ => json!(texts.join(", ")),
            };
            Some((key.clone(), value))
        })
        .collect()
}

/// Reads a shell's output as it grows, and says when it has stopped reading:
/// a command in the background settles only after all it wrote.
fn poll_shell_output(
    http: &Http,
    shell_id: &str,
    item_id: &str,
    poll: &ShellPoll,
    sender: &Sender<Transport>,
) {
    let mut cursor = 0;
    while !poll.stop.load(Ordering::Relaxed) {
        // Read before asking: once the shell has exited, its output is whole.
        let exited = poll.exited.load(Ordering::Relaxed);
        let Ok(page) = http.get(&format!("/api/shell/{shell_id}/output?cursor={cursor}")) else {
            // The server drops a shell once its command is settled.
            break;
        };
        let page = &page["data"];
        if let Some(text) = page["output"].as_str().filter(|text| !text.is_empty())
            && sender
                .send_blocking(Transport::ShellOutput {
                    item_id: item_id.to_owned(),
                    text: text.to_owned(),
                })
                .is_err()
        {
            return;
        }
        cursor = page["cursor"].as_u64().unwrap_or(cursor);
        if exited {
            // A page holds at most 64 KiB; `size` is all there is to read.
            if page["size"].as_u64().is_none_or(|size| cursor >= size) {
                break;
            }
            continue;
        }
        std::thread::sleep(SHELL_POLL_INTERVAL);
    }
    let _ = sender.send_blocking(Transport::ShellPollEnded {
        item_id: item_id.to_owned(),
    });
}

/// Side effects the actor performs for the mapper.
#[derive(Debug, PartialEq)]
enum Action {
    PollShell {
        item_id: String,
        shell_id: String,
        directory: Option<String>,
    },
    StopShell {
        item_id: String,
    },
    /// The shell exited: its poll reads the rest and ends.
    DrainShell {
        item_id: String,
    },
    /// Stop a shell in the background, in the location it runs in.
    RemoveShell {
        shell_id: String,
        directory: Option<String>,
    },
    /// Stop a subagent in the background.
    InterruptSession {
        session_id: String,
    },
    FetchDiff,
}

/// A piece of the work OpenCode goes on with in the background.
struct BackgroundWork {
    /// OpenCode's own id for it: the shell's, or the subagent's child
    /// session.
    id: String,
    item_id: String,
    name: String,
    kind: BackgroundKind,
}

#[derive(Default)]
struct Mapped {
    events: Vec<AgentEvent>,
    actions: Vec<Action>,
}

/// One tool call, keyed by its thread item id.
struct ToolCall {
    name: String,
    input: Value,
    parent_item_id: Option<String>,
    live_output: String,
    /// The child session a `subagent` call runs, once it reports one.
    child_session: Option<String>,
    child_model: Option<String>,
    /// The call returned while its work goes on: a subagent settles with its
    /// report, a command with its shell's exit.
    background: bool,
    /// The shell a `shell` call runs, once it reports one.
    shell_id: Option<String>,
    /// The location the shell runs in, as the call reported it.
    shell_directory: Option<String>,
    /// The shell's output is being polled.
    polling: bool,
    /// How the shell ended, once OpenCode says it exited.
    shell_end: Option<ShellEnd>,
}

#[derive(Clone, Copy)]
struct ShellEnd {
    status: ItemStatus,
    exit: Option<i32>,
}

struct Mapper {
    root: String,
    /// The session's MCP servers, whose tools OpenCode names `<server>_<tool>`.
    mcp_servers: Vec<String>,
    /// Child session → the subagent call that runs it, once known.
    children: HashMap<String, Option<String>>,
    tools: HashMap<String, ToolCall>,
    context_windows: HashMap<String, u64>,
    served_model: Option<String>,
    context_window: Option<u64>,
    active_turn: Option<String>,
    /// The root's inbox items not yet delivered → whether the item is a move.
    undelivered: HashMap<String, bool>,
    /// Inbox id → a message OpenCode wrote for the model itself, shown once
    /// it is delivered.
    synthetic: HashMap<String, ThreadItem>,
    /// An execution started for moves alone, which is no turn: its turn id,
    /// for the turn it starts if it delivers anything else.
    held_turn: Option<String>,
    last_turn: Option<String>,
    turn_usage: Option<TokenUsage>,
    turn_processed: u64,
    turn_cost: Option<f64>,
    turn_failed: bool,
    interrupted: bool,
    last_error: Option<String>,
}

impl Mapper {
    fn new(root: String, mcp_servers: Vec<String>, context_windows: HashMap<String, u64>) -> Self {
        Self {
            root,
            mcp_servers,
            children: HashMap::new(),
            tools: HashMap::new(),
            context_windows,
            served_model: None,
            context_window: None,
            active_turn: None,
            undelivered: HashMap::new(),
            synthetic: HashMap::new(),
            held_turn: None,
            last_turn: None,
            turn_usage: None,
            turn_processed: 0,
            turn_cost: None,
            turn_failed: false,
            interrupted: false,
            last_error: None,
        }
    }

    /// `Some(parent_item_id)` for this session's own events and those of the
    /// subagents it spawned (directly or not); `None` for any other session.
    fn scope(&self, session_id: &str) -> Option<Option<String>> {
        if session_id == self.root {
            Some(None)
        } else {
            self.children.get(session_id).cloned()
        }
    }

    /// The root's inbox item `inbox_id` was delivered: the turn an execution
    /// held for moves starts once it delivers something else.
    fn delivered(&mut self, inbox_id: &str) -> Option<AgentEvent> {
        if self.undelivered.remove(inbox_id) == Some(true) {
            return None;
        }
        let turn_id = self.held_turn.take()?;
        self.active_turn = Some(turn_id.clone());
        Some(AgentEvent::TurnStarted { turn_id })
    }

    fn turn_id(&self) -> Option<String> {
        self.active_turn.clone().or_else(|| self.last_turn.clone())
    }

    fn on_event(&mut self, event: &Value) -> Result<Mapped, String> {
        let kind = event["type"].as_str().unwrap_or_default();
        let data = &event["data"];
        let mut mapped = Mapped::default();
        if kind == "session.created" {
            if let (Some(session_id), Some(parent)) =
                (data["sessionID"].as_str(), data["parentID"].as_str())
                && self.scope(parent).is_some()
            {
                self.children.insert(session_id.to_owned(), None);
            }
            return Ok(mapped);
        }
        // A shell's events name the shell, not its session.
        if kind == "shell.exited" {
            if let Some(item_id) = data["id"].as_str().and_then(|shell_id| {
                self.tools
                    .iter()
                    .find(|(_, tool)| tool.shell_id.as_deref() == Some(shell_id))
                    .map(|(item_id, _)| item_id.clone())
            }) && let Some(tool) = self.tools.get_mut(&item_id)
            {
                tool.shell_end = Some(ShellEnd {
                    status: if data["status"] == "killed" {
                        ItemStatus::Interrupted
                    } else {
                        ItemStatus::Completed
                    },
                    exit: data["exit"]
                        .as_i64()
                        .and_then(|exit| i32::try_from(exit).ok()),
                });
                self.settle_background_shell(&item_id, &mut mapped);
            }
            return Ok(mapped);
        }
        let Some(session_id) = data["sessionID"].as_str() else {
            return Ok(mapped);
        };
        let Some(parent_item_id) = self.scope(session_id) else {
            return Ok(mapped);
        };
        let root = session_id == self.root;
        let message_id = data["assistantMessageID"].as_str().unwrap_or_default();
        match kind {
            "session.inbox.enqueued" => {
                let Some(inbox_id) = data["inboxID"].as_str() else {
                    return Ok(mapped);
                };
                let item = &data["item"];
                if root {
                    self.undelivered
                        .insert(inbox_id.to_owned(), item["type"] == "move");
                }
                if item["type"] == "synthetic" {
                    let payload = &item["payload"];
                    self.synthetic.insert(
                        inbox_id.to_owned(),
                        synthetic_item(inbox_id, payload, parent_item_id),
                    );
                    mapped.events.extend(self.subagent_reported(payload));
                }
            }
            "session.inbox.delivered" => {
                let delivered = data["inboxID"]
                    .as_str()
                    .and_then(|inbox_id| self.synthetic.remove(inbox_id));
                mapped
                    .events
                    .extend(delivered.map(AgentEvent::ItemCompleted));
            }
            "session.inbox.cancelled" => {
                if let Some(inbox_id) = data["inboxID"].as_str() {
                    self.undelivered.remove(inbox_id);
                    self.synthetic.remove(inbox_id);
                }
            }
            "session.synthetic" => {
                let Some(id) = event["id"].as_str() else {
                    return Err("OpenCode wrote the model a message without an event id".into());
                };
                mapped.events.push(AgentEvent::ItemCompleted(synthetic_item(
                    id,
                    data,
                    parent_item_id,
                )));
            }
            "session.execution.started" if root => {
                self.turn_processed = 0;
                self.turn_cost = None;
                self.last_error = None;
                if self.active_turn.is_none() && self.held_turn.is_none() {
                    // The event's id, unique across the session's servers, names
                    // the turn: a counter restarts with every server.
                    let Some(turn_id) = event["id"].as_str().map(str::to_owned) else {
                        return Err("OpenCode started an execution without an event id".into());
                    };
                    self.turn_usage = None;
                    self.turn_failed = false;
                    // A move delivered on its own runs an execution with no
                    // prompt and no step.
                    if !self.undelivered.is_empty() && self.undelivered.values().all(|&moves| moves)
                    {
                        self.held_turn = Some(turn_id);
                    } else {
                        self.active_turn = Some(turn_id.clone());
                        mapped.events.push(AgentEvent::TurnStarted { turn_id });
                    }
                }
            }
            "session.execution.succeeded"
            | "session.execution.failed"
            | "session.execution.interrupted"
                if root =>
            {
                self.held_turn = None;
                if kind == "session.execution.failed" {
                    self.turn_failed = true;
                    let message = error_message(&data["error"]);
                    if self.last_error.as_deref() != Some(message.as_str()) {
                        mapped.events.push(AgentEvent::Error {
                            message,
                            fatal: false,
                        });
                    }
                } else if kind == "session.execution.interrupted" {
                    self.interrupted = true;
                }
                if let Some(turn_id) = &self.active_turn {
                    self.last_turn = Some(turn_id.clone());
                }
                let usage = self.turn_usage.map(|usage| TokenUsage {
                    turn_processed_tokens: Some(self.turn_processed),
                    cost_usd: self.turn_cost,
                    ..usage
                });
                mapped.events.extend(crate::complete_mapped_turn(
                    &mut self.active_turn,
                    &mut self.interrupted,
                    self.turn_failed,
                    usage,
                ));
            }
            "session.step.started" => {
                let model = ModelRef::from_wire(&data["model"]).map(|model| model.id());
                if root {
                    if let Some(model) = &model {
                        self.context_window = self.context_windows.get(model).copied();
                        if self.served_model.as_deref() != Some(model.as_str()) {
                            let from = self.served_model.replace(model.clone());
                            mapped.events.push(AgentEvent::ServedModel {
                                model: model.clone(),
                                from,
                                reason: None,
                            });
                        }
                    }
                } else if let Some(item_id) = parent_item_id
                    && let Some(tool) = self.tools.get_mut(&item_id)
                    && tool.child_model != model
                {
                    tool.child_model = model;
                    let item = self.tool_item(&item_id, ItemStatus::InProgress, None, None);
                    mapped.events.extend(item.map(AgentEvent::ItemUpdated));
                }
            }
            "session.step.ended" if root => {
                if let Some(usage) = step_usage(&data["tokens"]) {
                    let usage = TokenUsage {
                        context_window: self.context_window,
                        ..usage
                    };
                    self.turn_processed = self
                        .turn_processed
                        .saturating_add(crate::processed_tokens(usage));
                    if let Some(cost) = data["cost"].as_f64() {
                        self.turn_cost = Some(self.turn_cost.unwrap_or(0.0) + cost);
                    }
                    self.turn_usage = Some(usage);
                    mapped.events.push(AgentEvent::TokenUsage(usage));
                }
                if data["files"]
                    .as_array()
                    .is_some_and(|files| !files.is_empty())
                {
                    mapped.actions.push(Action::FetchDiff);
                }
            }
            "session.step.failed" if root => {
                if data.pointer("/error/type") != Some(&json!("aborted")) {
                    self.turn_failed = true;
                    let message = error_message(&data["error"]);
                    self.last_error = Some(message.clone());
                    mapped.events.push(AgentEvent::Error {
                        message,
                        fatal: false,
                    });
                }
            }
            "session.retry.scheduled" if root => mapped.events.push(AgentEvent::RequestRetry {
                message: format!(
                    "OpenCode retry {}: {}",
                    data["attempt"].as_u64().unwrap_or(0),
                    error_message(&data["error"])
                ),
            }),
            "session.text.started" | "session.reasoning.started" => {
                let reasoning = kind == "session.reasoning.started";
                mapped.events.push(AgentEvent::ItemStarted(text_item(
                    message_id,
                    data,
                    reasoning,
                    String::new(),
                    parent_item_id,
                )));
            }
            "session.text.delta" | "session.reasoning.delta" => {
                let reasoning = kind == "session.reasoning.delta";
                if let Some(delta) = data["delta"].as_str().filter(|delta| !delta.is_empty()) {
                    mapped.events.push(AgentEvent::Delta {
                        item_id: text_item_id(message_id, data, reasoning),
                        kind: if reasoning {
                            DeltaKind::ReasoningText
                        } else {
                            DeltaKind::AssistantText
                        },
                        text: delta.to_owned(),
                    });
                }
            }
            "session.text.ended" | "session.reasoning.ended" => {
                let reasoning = kind == "session.reasoning.ended";
                mapped.events.push(AgentEvent::ItemCompleted(text_item(
                    message_id,
                    data,
                    reasoning,
                    data["text"].as_str().unwrap_or_default().to_owned(),
                    parent_item_id,
                )));
            }
            "session.tool.input.started" => {
                let item_id = tool_item_id(message_id, data);
                let name = self.tool_name(data["name"].as_str().unwrap_or("tool"));
                self.tools.insert(
                    item_id.clone(),
                    ToolCall {
                        name,
                        input: json!({}),
                        parent_item_id,
                        live_output: String::new(),
                        child_session: None,
                        child_model: None,
                        background: false,
                        shell_id: None,
                        shell_directory: None,
                        polling: false,
                        shell_end: None,
                    },
                );
                let item = self.tool_item(&item_id, ItemStatus::InProgress, None, None);
                mapped.events.extend(item.map(AgentEvent::ItemStarted));
            }
            "session.tool.called" => {
                let item_id = tool_item_id(message_id, data);
                let started = !self.tools.contains_key(&item_id);
                let tool = self
                    .tools
                    .entry(item_id.clone())
                    .or_insert_with(|| ToolCall {
                        name: data["name"].as_str().unwrap_or("tool").to_owned(),
                        input: Value::Null,
                        parent_item_id,
                        live_output: String::new(),
                        child_session: None,
                        child_model: None,
                        background: false,
                        shell_id: None,
                        shell_directory: None,
                        polling: false,
                        shell_end: None,
                    });
                tool.input = data["input"].clone();
                let item = self.tool_item(&item_id, ItemStatus::InProgress, None, None);
                mapped.events.extend(item.map(if started {
                    AgentEvent::ItemStarted
                } else {
                    AgentEvent::ItemUpdated
                }));
            }
            "session.tool.progress" => {
                let item_id = tool_item_id(message_id, data);
                let metadata = &data["metadata"];
                let Some(tool) = self.tools.get_mut(&item_id) else {
                    return Ok(mapped);
                };
                if tool.name == "shell"
                    && let Some(shell_id) = metadata["shellID"].as_str()
                {
                    tool.shell_id = Some(shell_id.to_owned());
                    tool.shell_directory = event
                        .pointer("/location/directory")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    tool.polling = true;
                    mapped.actions.push(Action::PollShell {
                        item_id: item_id.clone(),
                        shell_id: shell_id.to_owned(),
                        directory: tool.shell_directory.clone(),
                    });
                }
                if tool.name == "subagent"
                    && let Some(child) = metadata["sessionID"].as_str()
                {
                    tool.child_session = Some(child.to_owned());
                    self.children.insert(child.to_owned(), Some(item_id));
                }
            }
            "session.tool.success" | "session.tool.failed" => {
                let item_id = tool_item_id(message_id, data);
                let Some(tool) = self.tools.get_mut(&item_id) else {
                    return Ok(mapped);
                };
                let success = kind == "session.tool.success";
                // The call returned and its work goes on: a subagent (every
                // one, from 2.3.0) or a command moved to the background.
                if success
                    && matches!(tool.name.as_str(), "subagent" | "shell")
                    && data.pointer("/metadata/status") == Some(&json!("running"))
                {
                    tool.background = true;
                    let item = self.tool_item(&item_id, ItemStatus::InProgress, None, None);
                    if let Some(item) = item {
                        let backgrounded = AgentEvent::ItemBackgrounded {
                            item_id: item.id.clone(),
                            parent_item_id: item.parent_item_id.clone(),
                        };
                        mapped.events.push(AgentEvent::ItemUpdated(item));
                        mapped.events.push(backgrounded);
                    }
                    self.settle_background_shell(&item_id, &mut mapped);
                    return Ok(mapped);
                }
                mapped.actions.push(Action::StopShell {
                    item_id: item_id.clone(),
                });
                if tool.name == "todowrite" {
                    let todos = data
                        .pointer("/metadata/todos")
                        .unwrap_or(&tool.input["todos"]);
                    if success && let Some(steps) = plan_steps(todos) {
                        mapped.events.push(AgentEvent::PlanUpdated {
                            turn_id: self.active_turn.clone(),
                            explanation: None,
                            steps,
                        });
                    }
                    self.tools.remove(&item_id);
                    return Ok(mapped);
                }
                // A shell the user stopped fails with `metadata.interrupted`
                // (2.3.0's `shell.ts`).
                let status = if success {
                    ItemStatus::Completed
                } else if data.pointer("/error/type") == Some(&json!("aborted"))
                    || data.pointer("/metadata/interrupted") == Some(&json!(true))
                {
                    ItemStatus::Interrupted
                } else {
                    ItemStatus::Failed
                };
                let error = (!success).then(|| error_message(&data["error"]));
                if let Some(item) = self.tool_item(&item_id, status, Some(data), error) {
                    mapped.events.push(AgentEvent::ItemCompleted(item));
                }
                self.tools.remove(&item_id);
            }
            "session.compaction.started" if root => {
                mapped.events.push(AgentEvent::ContextCompacted(Compaction {
                    in_progress: true,
                    trigger: data["reason"].as_str().map(str::to_owned),
                    ..Compaction::default()
                }));
            }
            "session.compaction.ended" if root => {
                mapped.events.push(AgentEvent::ContextCompacted(Compaction {
                    in_progress: false,
                    trigger: data["reason"].as_str().map(str::to_owned),
                    ..Compaction::default()
                }));
            }
            "session.compaction.failed" if root => mapped.events.push(AgentEvent::Warning {
                message: format!(
                    "OpenCode context compaction failed: {}",
                    error_message(&data["error"])
                ),
            }),
            _ => {}
        }
        Ok(mapped)
    }

    fn shell_output(&mut self, item_id: &str, text: String) -> Vec<AgentEvent> {
        let Some(tool) = self.tools.get_mut(item_id) else {
            return Vec::new();
        };
        tool.live_output.push_str(&text);
        vec![AgentEvent::Delta {
            item_id: item_id.to_owned(),
            kind: DeltaKind::CommandOutput,
            text,
        }]
    }

    /// The calls OpenCode returned while their work goes on and has not
    /// ended, by when they were made: one piece for each shell that has not
    /// exited and each subagent that has not reported, however many calls
    /// joined it.
    fn background(&self) -> Vec<BackgroundWork> {
        let mut calls: Vec<(&String, &ToolCall)> = self
            .tools
            .iter()
            .filter(|(_, tool)| tool.background)
            .collect();
        calls.sort_by_key(|(item_id, _)| *item_id);
        let mut seen = HashSet::new();
        calls
            .into_iter()
            .filter_map(|(item_id, tool)| {
                let input = &tool.input;
                let (id, kind, name) = match tool.name.as_str() {
                    "shell" if tool.shell_end.is_none() => (
                        tool.shell_id.clone()?,
                        BackgroundKind::Command,
                        input["command"].as_str().unwrap_or_default(),
                    ),
                    "subagent" => (
                        tool.child_session.clone()?,
                        BackgroundKind::Agent,
                        input["description"]
                            .as_str()
                            .filter(|description| !description.trim().is_empty())
                            .or_else(|| input["agent"].as_str())
                            .unwrap_or_default(),
                    ),
                    _ => return None,
                };
                seen.insert(id.clone()).then(|| BackgroundWork {
                    id,
                    item_id: item_id.clone(),
                    name: name.to_owned(),
                    kind,
                })
            })
            .collect()
    }

    /// What stops the piece `task_id` of the background work, or all of it:
    /// what OpenCode's own app does for each.
    fn stop_background(&self, task_id: Option<&str>) -> Vec<Action> {
        self.background()
            .into_iter()
            .filter(|work| task_id.is_none_or(|id| id == work.id))
            .map(|work| match work.kind {
                BackgroundKind::Command => Action::RemoveShell {
                    directory: self
                        .tools
                        .get(&work.item_id)
                        .and_then(|tool| tool.shell_directory.clone()),
                    shell_id: work.id,
                },
                _ => Action::InterruptSession {
                    session_id: work.id,
                },
            })
            .collect()
    }

    /// The shell's output poll stopped reading.
    fn shell_poll_ended(&mut self, item_id: &str) -> Vec<AgentEvent> {
        let Some(tool) = self.tools.get_mut(item_id) else {
            return Vec::new();
        };
        tool.polling = false;
        let mut mapped = Mapped::default();
        self.settle_background_shell(item_id, &mut mapped);
        mapped.events
    }

    /// A command in the background ends when its shell has exited, whichever
    /// of that and the call's return OpenCode reports first, and its output
    /// has been read to the end.
    fn settle_background_shell(&mut self, item_id: &str, mapped: &mut Mapped) {
        let Some(tool) = self.tools.get(item_id) else {
            return;
        };
        let (true, Some(end)) = (tool.background, tool.shell_end) else {
            return;
        };
        if tool.polling {
            mapped.actions.push(Action::DrainShell {
                item_id: item_id.to_owned(),
            });
            return;
        }
        let item = self.tool_item(item_id, end.status, None, None);
        self.tools.remove(item_id);
        mapped.events.extend(item.map(AgentEvent::ItemCompleted));
    }

    /// A subagent in the background ends with the report OpenCode writes its
    /// parent, a synthetic message whose metadata names the child session and
    /// how its job ended (`subagent-completion.ts`). A call naming a child
    /// that is still running joins that run (`subagent.ts`), so the report
    /// ends every call waiting on the child.
    fn subagent_reported(&mut self, payload: &Value) -> Vec<AgentEvent> {
        let metadata = &payload["metadata"];
        if metadata["source"] != "subagent" {
            return Vec::new();
        }
        let status = match metadata["state"].as_str() {
            Some("completed") => ItemStatus::Completed,
            Some("error") => ItemStatus::Failed,
            Some("cancelled") => ItemStatus::Interrupted,
            _ => return Vec::new(),
        };
        let Some(child) = metadata["childID"].as_str() else {
            return Vec::new();
        };
        let mut calls: Vec<String> = self
            .tools
            .iter()
            .filter(|(_, tool)| tool.background && tool.child_session.as_deref() == Some(child))
            .map(|(item_id, _)| item_id.clone())
            .collect();
        calls.sort();
        calls
            .into_iter()
            .filter_map(|item_id| {
                let mut item = self.tool_item(&item_id, status, None, None)?;
                self.tools.remove(&item_id);
                if let ItemContent::Subagent { summary, .. } = &mut item.content {
                    *summary = payload["text"].as_str().map(subagent_output);
                }
                Some(AgentEvent::ItemCompleted(item))
            })
            .collect()
    }

    /// The canonical item for a tool call. `result` is the terminal
    /// success/failure payload (`content`, `metadata`); `error` its message.
    fn tool_item(
        &self,
        item_id: &str,
        status: ItemStatus,
        result: Option<&Value>,
        error: Option<String>,
    ) -> Option<ThreadItem> {
        let tool = self.tools.get(item_id)?;
        if tool.name == "todowrite" {
            // The todo list is the session's plan, not a timeline row.
            return None;
        }
        let metadata = result.map(|result| &result["metadata"]);
        let ToolOutput { text, images } = result
            .map(|result| ToolOutput::from_content(&result["content"]))
            .unwrap_or_default();
        let text = (!text.is_empty()).then_some(text);
        let input = &tool.input;
        let content = match tool.name.as_str() {
            "shell" => ItemContent::CommandExecution {
                command: input["command"].as_str().unwrap_or_default().to_owned(),
                input: Vec::new(),
                output: text
                    .or_else(|| (!tool.live_output.is_empty()).then(|| tool.live_output.clone()))
                    .or(error.filter(|_| status == ItemStatus::Failed))
                    .unwrap_or_default(),
                exit_code: metadata
                    .and_then(|metadata| metadata["exit"].as_i64())
                    .and_then(|exit| i32::try_from(exit).ok())
                    // A command in the background has no result but its exit.
                    .or_else(|| tool.shell_end.and_then(|end| end.exit)),
                status,
            },
            "edit" | "write" | "patch" => ItemContent::FileChange {
                changes: file_changes(&tool.name, input, text.as_deref(), metadata),
                status,
                output: if status == ItemStatus::Completed {
                    text
                } else {
                    error.or(text)
                },
                command: None,
            },
            "subagent" => ItemContent::Subagent {
                agent_type: input["agent"].as_str().unwrap_or_default().to_owned(),
                description: input["description"].as_str().unwrap_or_default().to_owned(),
                status,
                summary: match status {
                    ItemStatus::Completed => text.as_deref().map(subagent_output),
                    ItemStatus::InProgress => None,
                    _ => error.or(text),
                },
                model: input["model"]
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| tool.child_model.clone()),
                effort: None,
            },
            name => {
                let output = if status == ItemStatus::Completed {
                    text
                } else {
                    error.or(text)
                };
                let (family, output) = match status {
                    ItemStatus::Completed => tool_family(name, input, output, metadata),
                    _ => (tool_family(name, input, None, None).0, output),
                };
                ItemContent::ToolCall {
                    image_reads: images,
                    name: name.to_owned(),
                    input: input.clone(),
                    output,
                    status,
                    family: family.map(Box::new),
                }
            }
        };
        Some(ThreadItem {
            id: item_id.to_owned(),
            parent_item_id: tool.parent_item_id.clone(),
            content,
        })
    }

    /// OpenCode names an MCP tool `<server>_<tool>`, which cannot be split
    /// again (server names contain underscores); the session's servers are
    /// known here, so the call is named `server/tool`, the spelling
    /// [`crate::ToolName`] reads, and Tcode's own tools stay recognizable.
    fn tool_name(&self, wire: &str) -> String {
        self.mcp_servers
            .iter()
            .filter_map(|server| {
                let namespace: String = server
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || c == '-' {
                            c
                        } else {
                            '_'
                        }
                    })
                    .collect();
                let tool = wire.strip_prefix(&namespace)?.strip_prefix('_')?;
                (!tool.is_empty()).then_some((namespace.len(), server, tool))
            })
            .max_by_key(|(length, _, _)| *length)
            .map_or_else(
                || wire.to_owned(),
                |(_, server, tool)| format!("{server}/{tool}"),
            )
    }

    fn approval_kind(&self, data: &Value) -> ApprovalKind {
        let action = data["action"].as_str().unwrap_or("tool");
        let resources: Vec<String> = data["resources"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        let reason = data["message"].as_str().map(str::to_owned);
        let tool = match (
            data.pointer("/source/messageID").and_then(Value::as_str),
            data.pointer("/source/id").and_then(Value::as_str),
        ) {
            (Some(message_id), Some(id)) => self.tools.get(&format!("{message_id}:{id}")),
            _ => None,
        };
        match action {
            "shell" => ApprovalKind::ExecCommand {
                command: tool
                    .and_then(|tool| tool.input["command"].as_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| resources.join("\n")),
                cwd: tool
                    .and_then(|tool| tool.input["workdir"].as_str())
                    .map(str::to_owned),
                description: None,
            },
            "edit" => ApprovalKind::FileChange {
                changes: match data.pointer("/metadata/files") {
                    Some(files) => file_diffs(files),
                    None => resources
                        .iter()
                        .map(|path| FileChange {
                            path: path.clone(),
                            kind: FileChangeKind::Modify,
                            diff: None,
                        })
                        .collect(),
                },
            },
            "read" | "grep" | "glob" => ApprovalKind::FileRead {
                detail: resources.join(", "),
            },
            _ => ApprovalKind::ToolUse {
                name: tool.map_or(action, |tool| tool.name.as_str()).to_owned(),
                input: tool
                    .map(|tool| tool.input.clone())
                    .unwrap_or_else(|| data["metadata"].clone()),
                detail: if resources.is_empty() {
                    reason.unwrap_or_default()
                } else {
                    resources.join(", ")
                },
            },
        }
    }
}

fn text_item_id(message_id: &str, data: &Value, reasoning: bool) -> String {
    let kind = if reasoning { "reasoning" } else { "text" };
    format!(
        "{message_id}:{kind}:{}",
        data["ordinal"].as_u64().unwrap_or(0)
    )
}

fn text_item(
    message_id: &str,
    data: &Value,
    reasoning: bool,
    text: String,
    parent_item_id: Option<String>,
) -> ThreadItem {
    ThreadItem {
        id: text_item_id(message_id, data, reasoning),
        parent_item_id,
        content: if reasoning {
            ItemContent::Reasoning { text }
        } else {
            ItemContent::AssistantMessage {
                text,
                summarized: false,
            }
        },
    }
}

/// A tool call id is unique only within its assistant message.
fn tool_item_id(message_id: &str, data: &Value) -> String {
    format!("{message_id}:{}", data["id"].as_str().unwrap_or_default())
}

/// A message OpenCode wrote for the model itself (a subagent's report, how a
/// background command ended, a continuation it asks for), as it wrote it:
/// the `payload` of a `synthetic` inbox item, or a `session.synthetic` event's
/// data.
fn synthetic_item(id: &str, payload: &Value, parent_item_id: Option<String>) -> ThreadItem {
    ThreadItem {
        id: id.to_owned(),
        parent_item_id,
        content: ItemContent::Other {
            provider_kind: "synthetic".into(),
            summary: payload["text"].as_str().unwrap_or_default().to_owned(),
        },
    }
}

/// The subagent's reply inside the `<subagent …>` envelope OpenCode hands the
/// parent model.
fn subagent_output(text: &str) -> String {
    text.strip_prefix("<subagent ")
        .and_then(|rest| rest.split_once('\n'))
        .and_then(|(_, body)| body.trim_end().strip_suffix("</subagent>"))
        .map_or(text, str::trim_end)
        .to_owned()
}

/// `metadata.files` of edit, write and patch results and edit permission
/// requests: OpenCode's per-file diffs.
fn file_diffs(files: &Value) -> Vec<FileChange> {
    files
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|file| {
            Some(FileChange {
                path: file["file"].as_str()?.to_owned(),
                kind: match file["status"].as_str() {
                    Some("added") => FileChangeKind::Create,
                    Some("deleted") => FileChangeKind::Delete,
                    _ => FileChangeKind::Modify,
                },
                diff: file["patch"].as_str().map(str::to_owned),
            })
        })
        .collect()
}

/// What OpenCode's own tool `name` is, from its input and, once it
/// succeeded, the text it gave the model. Returns the family and the output
/// the call shows: the text, or what is left of it once the family's fields
/// are read out. Formats are v2.2.7's (`packages/core/src/tool/plugin/`).
fn tool_family(
    name: &str,
    input: &Value,
    output: Option<String>,
    metadata: Option<&Value>,
) -> (Option<ToolFamily>, Option<String>) {
    let field = |key: &str| input.get(key).and_then(Value::as_str).map(str::to_owned);
    match name {
        "read" => {
            let Some(path) = field("path") else {
                return (None, output);
            };
            match output.as_deref().map(read_page) {
                Some(Some(Page::File {
                    first_line,
                    lines,
                    note,
                })) => (
                    Some(ToolFamily::Read {
                        path,
                        first_line: Some(first_line),
                        note: note.map(str::to_owned),
                    }),
                    Some(lines),
                ),
                // A directory's page is what it holds, not a file's lines.
                Some(Some(Page::Directory(entries))) => (
                    Some(ToolFamily::Search {
                        scope: SearchScope::Entries,
                        pattern: None,
                        hits: Some(
                            entries
                                .iter()
                                .map(|entry| hit(Path::new(&path).join(entry)))
                                .collect(),
                        ),
                        path: Some(path),
                        glob: None,
                    }),
                    output,
                ),
                _ => (
                    Some(ToolFamily::Read {
                        path,
                        first_line: None,
                        note: None,
                    }),
                    output,
                ),
            }
        }
        // `grep.ts`: `Found N matches`, then each file's absolute path and
        // its matches as `  Line N: text`.
        "grep" => (
            Some(ToolFamily::Search {
                scope: SearchScope::Content,
                pattern: field("pattern"),
                path: field("path"),
                glob: field("include"),
                hits: output.as_deref().map(grep_matches),
            }),
            output,
        ),
        // `question.ts`: the questions as asked, and `metadata.answers`, the
        // labels or words each got, in the questions' order.
        "question" => {
            let Some(asked) = input.get("questions").and_then(Value::as_array) else {
                return (None, output);
            };
            let answers = metadata
                .and_then(|metadata| metadata.get("answers"))
                .and_then(Value::as_array);
            let questions = asked
                .iter()
                .enumerate()
                .filter_map(|(index, question)| {
                    Some(AskedQuestion {
                        question: question.get("question")?.as_str()?.to_owned(),
                        options: question
                            .get("options")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(|option| {
                                Some(UserInputOption {
                                    label: option.get("label")?.as_str()?.to_owned(),
                                    description: option
                                        .get("description")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_owned(),
                                })
                            })
                            .collect(),
                        answers: answers
                            .and_then(|answers| answers.get(index))
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect(),
                    })
                })
                .collect();
            (Some(ToolFamily::Question { questions }), output)
        }
        // `skill.ts`: the skill's `id`, and once loaded its `name` in the
        // metadata.
        "skill" => match metadata
            .and_then(|metadata| metadata.get("name"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| field("id"))
        {
            Some(name) => (Some(ToolFamily::Skill { name }), output),
            None => (None, output),
        },
        "webfetch" => match field("url") {
            Some(url) => (Some(ToolFamily::Fetch { url, prompt: None }), output),
            None => (None, output),
        },
        // `websearch.ts`: each page as `## [title](url)`, then when it was
        // published and what it says.
        "websearch" => match field("query") {
            Some(query) => (
                Some(ToolFamily::WebSearch {
                    query,
                    links: output.as_deref().map(|text| {
                        text.lines()
                            .filter_map(|line| {
                                let link = line.strip_prefix("## [")?.strip_suffix(')')?;
                                let (title, url) = link.rsplit_once("](")?;
                                Some(WebLink {
                                    title: title.to_owned(),
                                    url: url.to_owned(),
                                })
                            })
                            .collect()
                    }),
                }),
                output,
            ),
            None => (None, output),
        },
        // `glob.ts`: the absolute path of each file it found.
        "glob" => (
            Some(ToolFamily::Search {
                scope: SearchScope::Files,
                pattern: field("pattern"),
                path: field("path"),
                glob: None,
                hits: output.as_deref().map(|text| {
                    text.lines()
                        .filter(|line| Path::new(line).is_absolute())
                        .map(hit)
                        .collect()
                }),
            }),
            output,
        ),
        _ => (None, output),
    }
}

fn hit(path: impl AsRef<Path>) -> SearchHit {
    SearchHit {
        path: path.as_ref().to_string_lossy().into_owned(),
        line: None,
        text: None,
    }
}

/// The matches in grep's text: a file's `<path>:` line, then its
/// `  Line N: text` lines; the count before and the truncation note after
/// are not matches.
fn grep_matches(text: &str) -> Vec<SearchHit> {
    let mut file = None;
    let mut hits = Vec::new();
    for row in text.lines() {
        if let Some(rest) = row.strip_prefix("  Line ")
            && let Some(path) = file
            && let Some((number, text)) = rest.split_once(": ")
            && let Ok(number) = number.parse()
        {
            hits.push(SearchHit {
                path: String::from(path),
                line: Some(number),
                text: Some(text.to_owned()),
            });
        } else if let Some(path) = row.strip_suffix(':')
            && Path::new(path).is_absolute()
        {
            file = Some(path);
        }
    }
    hits
}

/// A page `read` returned (`toModelContent` in `read.ts`): a file's
/// `Read file <path>, lines S-E` header, then each line as `N: text`, then
/// `[Output truncated. …]` when the file goes on; or a directory's
/// `Read directory <path>, entries S-E` header, then its entries by name and
/// the same notice. Only a page laid out exactly so is read: the header then
/// says nothing the page's own lines do not, and any other text stays the
/// call's output as the harness wrote it.
enum Page<'a> {
    File {
        first_line: u64,
        lines: String,
        note: Option<&'a str>,
    },
    Directory(Vec<&'a str>),
}

fn read_page(text: &str) -> Option<Page<'_>> {
    let mut rows: Vec<&str> = text.split('\n').collect();
    let header = rows.remove(0);
    let note = rows
        .last()
        .copied()
        .filter(|row| row.starts_with("[Output truncated. Continue reading with offset: "));
    if note.is_some() {
        rows.pop();
    }
    // `S-E`, the numbers of the first and last row the page holds.
    let span = |range: &str| -> Option<u64> {
        let (first, last) = range.split_once('-')?;
        let (first, last) = (first.parse::<u64>().ok()?, last.parse::<u64>().ok()?);
        (last.checked_sub(first)? + 1 == rows.len() as u64).then_some(first)
    };
    if let Some(rest) = header.strip_prefix("Read directory ") {
        let (_, range) = rest.rsplit_once(", entries ")?;
        span(range)?;
        return Some(Page::Directory(rows));
    }
    let (_, range) = header.strip_prefix("Read file ")?.rsplit_once(", lines ")?;
    let first_line = span(range)?;
    let lines = rows
        .iter()
        .zip(first_line..)
        .map(|(row, number)| row.strip_prefix(&format!("{number}: ")))
        .collect::<Option<Vec<_>>>()?;
    Some(Page::File {
        first_line,
        lines: lines.join("\n"),
        note,
    })
}

fn file_changes(
    name: &str,
    input: &Value,
    text: Option<&str>,
    metadata: Option<&Value>,
) -> Vec<FileChange> {
    if let Some(files) = metadata
        .map(|metadata| &metadata["files"])
        .filter(|files| files.is_array())
    {
        return file_diffs(files);
    }
    if name == "patch" {
        return patch_targets(input["patchText"].as_str().unwrap_or_default());
    }
    let Some(path) = input["path"].as_str().map(str::to_owned) else {
        // The model is still writing the call.
        return Vec::new();
    };
    // A write reports no diff; when it created the file, its content is the
    // whole change.
    if name == "write" && text.is_some_and(|text| text.starts_with("Created file")) {
        let content = input["content"].as_str().unwrap_or_default();
        return vec![FileChange {
            path,
            kind: FileChangeKind::Create,
            diff: (!content.is_empty()).then(|| {
                content
                    .lines()
                    .map(|line| format!("+{line}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            }),
        }];
    }
    vec![FileChange {
        path,
        kind: FileChangeKind::Modify,
        diff: None,
    }]
}

/// The files a patch names before it has run, from its own file headers.
fn patch_targets(patch: &str) -> Vec<FileChange> {
    let mut changes: Vec<FileChange> = Vec::new();
    for line in patch.lines() {
        let (kind, path) = if let Some(path) = line.strip_prefix("*** Add File: ") {
            (FileChangeKind::Create, path)
        } else if let Some(path) = line.strip_prefix("*** Update File: ") {
            (FileChangeKind::Modify, path)
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            (FileChangeKind::Delete, path)
        } else if let Some(path) = line.strip_prefix("*** Move to: ")
            && let Some(last) = changes.last_mut()
        {
            last.kind = FileChangeKind::Rename;
            last.path = path.trim().to_owned();
            continue;
        } else {
            continue;
        };
        changes.push(FileChange {
            path: path.trim().to_owned(),
            kind,
            diff: None,
        });
    }
    changes
}

fn plan_steps(todos: &Value) -> Option<Vec<PlanStep>> {
    let steps: Vec<PlanStep> = todos
        .as_array()?
        .iter()
        .filter_map(|todo| {
            Some(PlanStep {
                step: todo["content"].as_str()?.trim().to_owned(),
                status: match todo["status"].as_str() {
                    Some("completed") => PlanStepStatus::Completed,
                    Some("in_progress") => PlanStepStatus::InProgress,
                    _ => PlanStepStatus::Pending,
                },
            })
        })
        .collect();
    Some(steps)
}

/// One step's usage: OpenCode counts input apart from cache reads and writes,
/// and reasoning apart from output.
fn step_usage(tokens: &Value) -> Option<TokenUsage> {
    let input = crate::json_u64(tokens.get("input"))?;
    let output = crate::json_u64(tokens.get("output"))
        .unwrap_or(0)
        .saturating_add(crate::json_u64(tokens.get("reasoning")).unwrap_or(0));
    let cache_read = crate::json_u64(tokens.pointer("/cache/read")).unwrap_or(0);
    let cache_write = crate::json_u64(tokens.pointer("/cache/write")).unwrap_or(0);
    Some(TokenUsage {
        freshness: crate::ContextFreshness::Current,
        input_tokens: Some(input),
        cached_input_tokens: Some(cache_read),
        output_tokens: Some(output),
        used_tokens: Some(
            input
                .saturating_add(output)
                .saturating_add(cache_read)
                .saturating_add(cache_write),
        ),
        ..TokenUsage::default()
    })
}

fn error_message(error: &Value) -> String {
    match (error["message"].as_str(), error["type"].as_str()) {
        (Some(message), Some(kind)) if kind != "unknown" => format!("{message} ({kind})"),
        (Some(message), _) => message.to_owned(),
        _ => error.to_string(),
    }
}

/// One `opencode serve --stdio` child. Its stdin is held open: the server
/// exits when it closes, so it cannot outlive Tcode.
struct Server {
    child: crate::process::ProcessTree<Child>,
    _stdin: ChildStdin,
    http: Http,
    tail: crate::process::StderrTail,
}

impl Server {
    fn spawn(
        binary_path: Option<&Path>,
        cwd: &Path,
        launch_env: &LaunchEnv,
        extra_args: &[String],
        registrations: &[&crate::McpRegistration],
        processes: &crate::SessionProcesses,
    ) -> Result<Self, AgentError> {
        let binary = crate::resolve_binary(binary_path, "opencode")?;
        let password = uuid::Uuid::new_v4().simple().to_string();
        let mut command = crate::process::harness_command(&binary);
        command
            .arg("serve")
            .args(extra_args)
            .args(["--stdio", "--hostname", "127.0.0.1"])
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in launch_env.pairs(ProviderKind::OpenCode) {
            command.env(key, value);
        }
        // The server prefers this name over the legacy OPENCODE_SERVER_PASSWORD.
        command.env("OPENCODE_PASSWORD", &password);
        if let Some(config) = config_content(launch_env, registrations)? {
            command.env("OPENCODE_CONFIG_CONTENT", config);
        }
        crate::process::refuse_batch_line_breaks(command.get_program(), command.get_args())?;
        let mut child = processes.hold(crate::process::ProcessTree::new(command.spawn().map_err(
            |err| AgentError::Spawn(format!("spawning `{}`: {err}", binary.display())),
        )?));
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(AgentError::Spawn("OpenCode child stdio missing".into()));
        };
        let tail = crate::process::StderrTail::default();
        let _ = tail.spawn(stderr, "opencode-stderr", "OpenCode");
        // `--stdio` announces `{"url": …}` as its first stdout line.
        let (sender, receiver) = std::sync::mpsc::channel();
        let reader = std::thread::Builder::new()
            .name("opencode-startup".into())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut line = String::new();
                let read = reader.read_line(&mut line).map(|_| line);
                let _ = sender.send((read, reader));
            });
        let mut server = Self {
            child,
            _stdin: stdin,
            http: Http::new(String::new(), &password, cwd),
            tail,
        };
        if let Err(err) = reader {
            server.stop();
            return Err(AgentError::Spawn(format!(
                "spawning OpenCode stdout reader: {err}"
            )));
        }
        let announced = match receiver.recv() {
            Ok((Ok(line), stdout)) => {
                let _ = server.tail.spawn(stdout, "opencode-stdout", "OpenCode");
                line
            }
            Ok((Err(err), _)) => {
                return Err(server.startup_error(format!("reading OpenCode startup: {err}")));
            }
            Err(_) => {
                return Err(server.startup_error("the OpenCode startup reader ended".into()));
            }
        };
        let Some(url) = serde_json::from_str::<Value>(announced.trim())
            .ok()
            .and_then(|value| value["url"].as_str().map(str::to_owned))
        else {
            return Err(server.startup_error(if announced.is_empty() {
                "OpenCode server exited during startup".into()
            } else {
                format!("unexpected OpenCode startup output: {}", announced.trim())
            }));
        };
        server.http.base = url;
        server.wait_ready()?;
        Ok(server)
    }

    /// The server answers `/api/info` while it is still starting; every other
    /// route waits for it to be ready.
    fn wait_ready(&mut self) -> Result<(), AgentError> {
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Err(
                    self.startup_error(format!("OpenCode server exited during startup ({status})"))
                );
            }
            match self.http.status("/api/info") {
                Ok(200) => return Ok(()),
                Ok(500) => {
                    return Err(self.startup_error("OpenCode server failed to start".into()));
                }
                Ok(_) | Err(_) => {}
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn startup_error(&mut self, message: String) -> AgentError {
        // The output readers reach EOF only once the child is gone.
        self.stop();
        AgentError::Provider(self.tail.append_to(message, "\nserver output:\n"))
    }

    fn subscribe(&self) -> Result<(Receiver<Transport>, Sender<Transport>), AgentError> {
        let response = accepted(
            self.http.answer(
                ureq::http::Request::get(format!("{}/api/event", self.http.base))
                    .header("Authorization", &self.http.authorization),
                None,
            )?,
        )?;
        let (sender, receiver) = smol::channel::unbounded();
        let reader_sender = sender.clone();
        std::thread::Builder::new()
            .name("opencode-sse".into())
            .spawn(move || read_sse(response.into_body().into_reader(), reader_sender))
            .map_err(|err| AgentError::Spawn(format!("spawning OpenCode SSE reader: {err}")))?;
        Ok((receiver, sender))
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Merges Tcode's MCP servers into the inline config OpenCode reads, keeping
/// any inline config the user set themselves.
fn config_content(
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
                    "cannot merge Tcode MCP servers into OPENCODE_CONFIG_CONTENT: {err}"
                ))
            })?;
            if !config.is_object() {
                return Err(AgentError::Protocol(
                    "cannot merge Tcode MCP servers into non-object OPENCODE_CONFIG_CONTENT".into(),
                ));
            }
            config
        }
        None => json!({}),
    };
    if !config["mcp"].is_object() {
        config["mcp"] = json!({});
    }
    if !config["mcp"]["servers"].is_object() {
        config["mcp"]["servers"] = json!({});
    }
    for registration in registrations {
        config["mcp"]["servers"][&registration.name] = json!({
            "type": "remote",
            "url": registration.url,
            "headers": { "Authorization": format!("Bearer {}", registration.bearer_token) },
            // Code Mode would reach these tools only through scripts the model
            // writes; Tcode's own tools must stay calls of their own, which the
            // timeline and orchestration recognize by name.
            "codemode": false,
        });
    }
    Ok(Some(config.to_string()))
}

#[derive(Clone)]
struct Http {
    base: String,
    authorization: String,
    /// `x-opencode-directory`: the location a request resolves against.
    directory: PathBuf,
    agent: ureq::Agent,
}

impl Http {
    fn new(base: String, password: &str, directory: &Path) -> Self {
        Self {
            base,
            authorization: format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("opencode:{password}"))
            ),
            directory: directory.to_path_buf(),
            // No deadline: stopping the session kills the server, which ends
            // every request it has not answered. The server is on this machine, so no
            // proxy the environment names stands between.
            agent: ureq::Agent::config_builder()
                .http_status_as_error(false)
                .proxy(None)
                .build()
                .new_agent(),
        }
    }

    fn at(&self, directory: &Path) -> Self {
        Self {
            directory: directory.to_path_buf(),
            ..self.clone()
        }
    }

    fn request(&self, method: &str, path: &str) -> ureq::http::request::Builder {
        ureq::http::Request::builder()
            .method(method)
            .uri(format!("{}{path}", self.base))
            .header("Authorization", &self.authorization)
            .header(
                "x-opencode-directory",
                uri_component(&self.directory.to_string_lossy()),
            )
    }

    /// The server's answer to `request`, whatever its status.
    fn answer(
        &self,
        request: ureq::http::request::Builder,
        body: Option<&Value>,
    ) -> Result<Response, AgentError> {
        let url = request.uri_ref().map(ToString::to_string);
        match body {
            Some(body) => request
                .header("Content-Type", "application/json")
                .body(body.to_string())
                .map_err(ureq::Error::from)
                .and_then(|request| self.agent.run(request)),
            None => request
                .body(())
                .map_err(ureq::Error::from)
                .and_then(|request| self.agent.run(request)),
        }
        .map_err(|error| {
            AgentError::Protocol(format!(
                "OpenCode HTTP: {}: {error}",
                url.unwrap_or_default()
            ))
        })
    }

    fn get(&self, path: &str) -> Result<Value, AgentError> {
        parse_response(self.answer(self.request("GET", path), None)?)
    }

    fn status(&self, path: &str) -> Result<u16, AgentError> {
        Ok(self
            .answer(self.request("GET", path), None)?
            .status()
            .as_u16())
    }

    fn send(&self, method: &str, path: &str, body: &Value) -> Result<Value, AgentError> {
        parse_response(self.answer(self.request(method, path), Some(body))?)
    }

    fn post(&self, path: &str, body: &Value) -> Result<Value, AgentError> {
        self.send("POST", path, body)
    }

    fn post_empty(&self, path: &str, body: &Value) -> Result<(), AgentError> {
        self.send("POST", path, body).map(drop)
    }

    fn put(&self, path: &str, body: &Value) -> Result<(), AgentError> {
        self.send("PUT", path, body).map(drop)
    }

    fn delete(&self, path: &str) -> Result<(), AgentError> {
        parse_response(self.answer(self.request("DELETE", path), None)?).map(drop)
    }
}

/// `encodeURIComponent`, which OpenCode reverses on the directory header.
fn uri_component(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => char::from(byte).to_string(),
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

type Response = ureq::http::Response<ureq::Body>;

fn parse_response(response: Response) -> Result<Value, AgentError> {
    let response = accepted(response)?;
    if response.status() == 204 {
        return Ok(Value::Null);
    }
    let mut text = String::new();
    response
        .into_body()
        .into_reader()
        .read_to_string(&mut text)?;
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text)
        .map_err(|err| AgentError::Protocol(format!("invalid OpenCode JSON response: {err}")))
}

/// An answer the server did not refuse; a refusal as its status and the server's words.
fn accepted(response: Response) -> Result<Response, AgentError> {
    let status = response.status().as_u16();
    if status < 400 {
        return Ok(response);
    }
    let mut body = String::new();
    let _ = response
        .into_body()
        .into_reader()
        .take(64 * 1024)
        .read_to_string(&mut body);
    let detail = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|body| body["message"].as_str().map(str::to_owned))
        .unwrap_or(body);
    Err(AgentError::Provider(format!(
        "OpenCode HTTP {status}: {}",
        if detail.trim().is_empty() {
            "empty response"
        } else {
            detail.trim()
        }
    )))
}

enum Transport {
    Event(Value),
    ShellOutput { item_id: String, text: String },
    ShellPollEnded { item_id: String },
    Eof,
    Error(String),
}

fn read_sse(reader: impl Read, sender: Sender<Transport>) {
    let mut data = Vec::new();
    for line in BufReader::new(reader).lines() {
        let line = match line {
            Ok(line) => line,
            Err(err) => {
                let _ = sender.send_blocking(Transport::Error(format!(
                    "failed reading OpenCode events: {err}"
                )));
                return;
            }
        };
        if let Some(value) = line.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value).to_owned());
            continue;
        }
        if !line.is_empty() || data.is_empty() {
            continue;
        }
        let payload = data.join("\n");
        data.clear();
        let event = match serde_json::from_str(&payload) {
            Ok(event) => Transport::Event(event),
            Err(err) => Transport::Error(format!("invalid OpenCode event JSON: {err}")),
        };
        let fatal = matches!(event, Transport::Error(_));
        if sender.send_blocking(event).is_err() || fatal {
            return;
        }
    }
    let _ = sender.send_blocking(Transport::Eof);
}

async fn wait_for_server_connected(events: &Receiver<Transport>) -> Result<(), AgentError> {
    loop {
        match events.recv().await {
            Ok(Transport::Event(event)) if event["type"] == "server.connected" => {
                return Ok(());
            }
            Ok(
                Transport::Event(_)
                | Transport::ShellOutput { .. }
                | Transport::ShellPollEnded { .. },
            ) => {}
            Ok(Transport::Error(error)) => return Err(AgentError::Protocol(error)),
            Ok(Transport::Eof) | Err(_) => {
                return Err(AgentError::Protocol(
                    "OpenCode event stream closed before server.connected".into(),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests;
